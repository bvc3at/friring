//! Renderer for the sandbox-profile editor
//! (`crate::app::modals::SandboxEditorModal`) — `docs/SANDBOX.md` §UI. Modeled
//! on [`super::automation_editor_modal`]: one `visible_fields` projection drives
//! both the rows drawn here and the modal's Tab order, so the two cannot drift.
//!
//! Two things this editor adds to that shape:
//!
//! - **Two add/remove sub-lists** (paths, allowed domains). Each renders its
//!   entries under an anchor row that carries the list chords, with the
//!   selected entry marked; a path entry also carries its `‹ ro | rw ›` intent.
//! - **Unavailable capabilities are shown, not hidden.** A knob the chosen
//!   backend cannot honour (memory on a policy backend, a read scope in a
//!   place) renders the reason in place of its value and refuses input, which
//!   is the documented treatment for a capability a backend lacks.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::app::modals::{sandbox_field_available, SandboxField};
use crate::session::{NetworkMode, PathMode, ReadScope, SandboxBackendKind, SandboxShape};

use super::theme::Theme;
use super::{centered_fixed_height_rect, editor_field_line_with_cursor, render_modal_frame};

/// Indent of a sub-list entry row, before its `▸`/blank selection marker.
const ENTRY_INDENT: &str = "    ";

/// Display columns a path row's `‹ rw ›` intent cell occupies.
const MODE_CELL_WIDTH: usize = 6;

/// Borrowed view of a `crate::app::modals::SandboxEditorModal`.
pub struct SandboxEditorState<'a> {
    pub editing: bool,
    pub field: SandboxField,
    /// The fields shown, in display and navigation order — projected from the
    /// modal's own `visible_fields`, so render order can't drift from the
    /// cursor's.
    pub visible_fields: Vec<SandboxField>,
    /// Caret within the focused text field. Only the focused field draws one,
    /// so one position covers every text row.
    pub cursor: usize,
    pub name: &'a str,
    /// The profile's own backend choice, `auto` included.
    pub backend: SandboxBackendKind,
    /// What `auto` resolved to on this host, if it has been probed.
    pub resolved: Option<SandboxBackendKind>,
    /// The backend that will actually run — `backend`, or what `auto` resolved
    /// to. Its [`SandboxShape`] decides which capabilities are available.
    pub effective_backend: SandboxBackendKind,
    /// Each path as typed, with its read/write intent.
    pub paths: Vec<(&'a str, PathMode)>,
    pub path_index: usize,
    pub network: NetworkMode,
    /// Allowed `host[:port]` entries, as typed.
    pub domains: Vec<&'a str>,
    pub domain_index: usize,
    pub prompt_new_domains: bool,
    pub read_scope: ReadScope,
    pub memory: &'a str,
    pub cpus: &'a str,
    pub image: &'a str,
    pub containerfile: &'a str,
    pub allow_unsandboxed_fallback: bool,
}

impl<'a> SandboxEditorState<'a> {
    /// Borrow view data from an editor modal.
    pub fn from_modal(m: &'a crate::app::modals::SandboxEditorModal) -> Self {
        Self {
            editing: m.editing.is_some(),
            field: m.field,
            visible_fields: m.visible_fields(),
            cursor: m.active_cursor(),
            name: m.name.value(),
            backend: m.backend,
            resolved: m.resolved,
            effective_backend: m.effective_backend(),
            paths: m.paths.iter().map(|p| (p.text.value(), p.mode)).collect(),
            path_index: m.path_index,
            network: m.network_mode,
            domains: m.domains.iter().map(|d| d.value()).collect(),
            domain_index: m.domain_index,
            prompt_new_domains: m.prompt_new_domains,
            read_scope: m.read_scope,
            memory: m.memory.value(),
            cpus: m.cpus.value(),
            image: m.image.value(),
            containerfile: m.containerfile.value(),
            allow_unsandboxed_fallback: m.allow_unsandboxed_fallback,
        }
    }

    /// The shape of the backend that will run, or `None` while an `auto`
    /// backend is unresolved — in which case nothing is ruled out, exactly as
    /// the profile validator exempts `auto`.
    fn shape(&self) -> Option<SandboxShape> {
        self.effective_backend.shape()
    }
}

/// Why `field` is inert, phrased for the row that replaces its value — or
/// `None` when it is editable.
///
/// Derived from `sandbox_field_available` rather than re-deciding: this module
/// owns the wording, `crate::app::modals` owns the rule, and a field that stops
/// accepting input can never end up without an explanation.
pub fn unavailable_reason(
    field: SandboxField,
    backend: SandboxBackendKind,
    shape: Option<SandboxShape>,
    network: NetworkMode,
) -> Option<String> {
    if sandbox_field_available(field, shape, network) {
        return None;
    }
    Some(match field {
        SandboxField::Memory | SandboxField::Cpus => {
            format!("unavailable — {backend} applies a policy to a host process")
        }
        SandboxField::Image | SandboxField::Containerfile => {
            format!("unavailable — {backend} runs the host toolchain")
        }
        SandboxField::ReadScope => {
            format!("unavailable — {backend} has no host filesystem to read past the workspace")
        }
        SandboxField::PromptDomains => match network {
            NetworkMode::None => "unavailable — network 'none' lets nothing out".to_string(),
            _ => "unavailable — network 'full' leaves no domain unlisted".to_string(),
        },
        _ => format!("unavailable with {backend}"),
    })
}

/// Render the editor as a centered modal overlay. Returns the per-field click
/// hitboxes plus the `[ Save ]` / `[ Cancel ]` footer buttons.
pub fn render_sandbox_editor_modal(
    frame: &mut Frame,
    state: &SandboxEditorState<'_>,
) -> (Vec<super::RowHitbox>, super::ModalButtons) {
    let frame_area = frame.area();
    // The body's height depends on the sub-list lengths, so measure it at the
    // overlay's inner width before choosing the modal's height.
    let estimated_inner = (frame_area.width * 60 / 100).saturating_sub(2);
    let (body, _, _) = editor_body_lines(state, estimated_inner);
    let height = (body.len() + editor_footer_lines(state).len() + 2) as u16;
    let area = centered_fixed_height_rect(60, height.min(frame_area.height), frame_area);

    let inner = render_modal_frame(frame, area, editor_title(state));
    let (lines, field_rows) = windowed_editor_lines(state, inner.width, inner.height);
    frame.render_widget(Paragraph::new(lines), inner);
    let field_hits = field_hitboxes(inner, &field_rows);

    let footer_row = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    let buttons = super::render_action_footer(
        frame,
        footer_row,
        (
            "Save",
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
        "Cancel",
    );
    (field_hits, buttons)
}

fn editor_title(state: &SandboxEditorState<'_>) -> &'static str {
    if state.editing {
        "Edit Sandbox Profile"
    } else {
        "New Sandbox Profile"
    }
}

/// One click hitbox per visible field, from each field's `(index, display_row,
/// row_count)` within `inner` — a sub-list spans its anchor plus its entries.
fn field_hitboxes(inner: Rect, field_rows: &[(usize, u16, u16)]) -> Vec<super::RowHitbox> {
    field_rows
        .iter()
        .map(|&(index, row, count)| super::RowHitbox {
            rect: Rect::new(inner.x, inner.y + row, inner.width, count.max(1)),
            index,
        })
        .collect()
}

/// The editor body scroll-windowed to `height` rows around the active field,
/// with the footer pinned at the bottom — the automation editor's arrangement,
/// which a profile with many paths needs just as much. Also returns each
/// visible field's `(index, display_row, row_count)` for click hit-testing.
fn windowed_editor_lines<'a>(
    state: &SandboxEditorState<'a>,
    width: u16,
    height: u16,
) -> (Vec<Line<'a>>, Vec<(usize, u16, u16)>) {
    let (body, active_row, field_spans) = editor_body_lines(state, width);
    let footer = editor_footer_lines(state);

    let visible = (height as usize).saturating_sub(footer.len());
    let body_len = body.len();
    let (mut lines, start) = if body_len <= visible || visible == 0 {
        (body, 0usize)
    } else {
        let active = active_row.unwrap_or(0);
        let start = active.saturating_sub(visible / 2).min(body_len - visible);
        (
            body.into_iter()
                .skip(start)
                .take(visible)
                .collect::<Vec<_>>(),
            start,
        )
    };

    // Clip each field's rows to the window; rows scrolled off aren't clickable.
    let mut field_rows = Vec::new();
    if visible > 0 {
        let win_end = start + visible;
        for (i, (fstart, fcount)) in field_spans.into_iter().enumerate() {
            let vstart = fstart.max(start);
            let vend = (fstart + fcount).min(win_end);
            if vstart < vend {
                field_rows.push((i, (vstart - start) as u16, (vend - vstart) as u16));
            }
        }
    }

    lines.extend(footer);
    (lines, field_rows)
}

/// The body rows, the display index of the active field's first row (what the
/// caller windows around), and each field's `(start_row, row_count)` span.
fn editor_body_lines<'a>(
    state: &SandboxEditorState<'a>,
    width: u16,
) -> (Vec<Line<'a>>, Option<usize>, Vec<(usize, usize)>) {
    let mut lines: Vec<Line> = Vec::new();
    let mut active_row = None;
    let mut field_spans = Vec::with_capacity(state.visible_fields.len());
    for field in &state.visible_fields {
        let start = lines.len();
        let active = *field == state.field;
        if active {
            active_row = Some(start);
        }
        match field {
            SandboxField::Paths => lines.extend(paths_block(state, active, width)),
            SandboxField::Domains => lines.extend(domains_block(state, active, width)),
            other => lines.push(field_line(*other, state, active)),
        }
        field_spans.push((start, lines.len() - start));
    }
    (lines, active_row, field_spans)
}

/// The pinned footer: which shape the profile will run as (and what that costs
/// it), the honest scope of the boundary, and the key hints. Save and cancel
/// are rendered as clickable buttons over the last row, so the hints carry only
/// the navigation chords.
fn editor_footer_lines<'a>(state: &SandboxEditorState<'a>) -> Vec<Line<'a>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("  shape    ", Theme::label()),
            Span::styled(shape_summary(state), Style::default().fg(Theme::accent())),
        ]),
        // Non-goal stated where the profile is authored: policy backends share
        // the host kernel and a domain allowlist is bypassable.
        Line::from(Span::styled(
            "  a sandbox reduces blast radius; it does not prove containment",
            Style::default().fg(Theme::text_muted()),
        )),
    ];

    lines.push(super::key_hint_line(&[
        ("Tab/↑↓", " move  "),
        ("←→", " adjust  "),
        ("Space", " toggle"),
    ]));
    lines
}

/// What the profile will run as, and the consequence that follows from it
/// (ADR-26): a policy sandbox dies one pane at a time, a place dies whole.
fn shape_summary(state: &SandboxEditorState<'_>) -> String {
    match state.shape() {
        Some(SandboxShape::Policy) => format!(
            "policy — {} wraps the agent; tmux stays outside",
            state.effective_backend
        ),
        Some(SandboxShape::Place) => format!(
            "place — every session on this profile shares one {}",
            state.effective_backend
        ),
        None => "auto — resolved per host when a session starts".to_string(),
    }
}

/// One single-row field. An unavailable capability renders its reason in place
/// of a value.
fn field_line<'a>(field: SandboxField, state: &SandboxEditorState<'a>, active: bool) -> Line<'a> {
    let label = field_label(field);
    if let Some(reason) =
        unavailable_reason(field, state.effective_backend, state.shape(), state.network)
    {
        return unavailable_line(label, &reason, active);
    }
    match field {
        SandboxField::Backend => backend_line(state, active),
        SandboxField::PromptDomains => toggle_line(label, state.prompt_new_domains, active),
        SandboxField::Fallback => toggle_line(label, state.allow_unsandboxed_fallback, active),
        _ => {
            let (value, selector) = field_value(field, state, active);
            let cursor = (!selector).then_some(state.cursor);
            editor_field_line_with_cursor(label, value, selector, active, cursor)
        }
    }
}

/// The label column for every field. The row builders pad it to a fixed width,
/// so every value starts in the same column.
fn field_label(field: SandboxField) -> &'static str {
    match field {
        SandboxField::Name => "name",
        SandboxField::Backend => "backend",
        SandboxField::Paths => "paths",
        SandboxField::PathText => "path",
        SandboxField::PathMode => "mode",
        SandboxField::Network => "network",
        SandboxField::Domains => "allow",
        SandboxField::DomainText => "domain",
        SandboxField::PromptDomains => "prompt",
        SandboxField::ReadScope => "reads",
        SandboxField::Memory => "memory",
        SandboxField::Cpus => "cpus",
        SandboxField::Image => "image",
        SandboxField::Containerfile => "build",
        SandboxField::Fallback => "escape",
    }
}

/// `(value, is_selector)` for the fields that render as a plain value row.
fn field_value(
    field: SandboxField,
    state: &SandboxEditorState<'_>,
    active: bool,
) -> (String, bool) {
    match field {
        SandboxField::Name => (placeholder(state.name, "(required)", active), false),
        SandboxField::PathText => (
            placeholder(selected_path_text(state), "(e.g. ~/dev/app)", active),
            false,
        ),
        SandboxField::PathMode => (
            state
                .paths
                .get(state.path_index)
                .map_or_else(|| PathMode::default().to_string(), |(_, m)| m.to_string()),
            true,
        ),
        SandboxField::Network => (state.network.to_string(), true),
        SandboxField::DomainText => (
            placeholder(
                state.domains.get(state.domain_index).copied().unwrap_or(""),
                "(e.g. github.com:443)",
                active,
            ),
            false,
        ),
        SandboxField::ReadScope => (state.read_scope.to_string(), true),
        SandboxField::Memory => (placeholder(state.memory, "(uncapped)", active), false),
        SandboxField::Cpus => (placeholder(state.cpus, "(uncapped)", active), false),
        SandboxField::Image => (
            placeholder(state.image, "(the default image)", active),
            false,
        ),
        SandboxField::Containerfile => (placeholder(state.containerfile, "(none)", active), false),
        // Rendered by their own builders; `field_line` never routes them here.
        SandboxField::Backend
        | SandboxField::Paths
        | SandboxField::Domains
        | SandboxField::PromptDomains
        | SandboxField::Fallback => (String::new(), false),
    }
}

/// The selected path's text, or `""` when the list is empty (in which case the
/// `path` row is not shown at all).
fn selected_path_text<'a>(state: &SandboxEditorState<'a>) -> &'a str {
    state
        .paths
        .get(state.path_index)
        .map_or("", |(text, _)| *text)
}

/// `value`, or `hint` while the field is empty and unfocused — a focused empty
/// field shows its caret instead, so the hint can't be mistaken for text the
/// cursor is sitting in.
fn placeholder(value: &str, hint: &str, active: bool) -> String {
    if value.is_empty() && !active {
        hint.to_string()
    } else {
        value.to_string()
    }
}

/// The backend selector, with what `auto` resolved to appended so the choice
/// the ladder made is never invisible.
fn backend_line<'a>(state: &SandboxEditorState<'a>, active: bool) -> Line<'a> {
    let mut line = editor_field_line_with_cursor(
        field_label(SandboxField::Backend),
        state.backend.to_string(),
        true,
        active,
        None,
    );
    if let Some(resolved) = state
        .resolved
        .filter(|_| matches!(state.backend, SandboxBackendKind::Auto))
    {
        line.spans.push(Span::styled(
            format!("  → {resolved}"),
            Style::default().fg(Theme::text_muted()),
        ));
    }
    line
}

/// A boolean row in the settings modal's idiom: a calm `on`, a receding `off`.
fn toggle_line<'a>(label: &str, on: bool, active: bool) -> Line<'a> {
    let prefix = if active { "▸ " } else { "  " };
    let (text, style) = if on {
        (
            "on",
            Style::default()
                .fg(Theme::tool_allowed())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ("off", Style::default().fg(Theme::text_muted()))
    };
    Line::from(vec![
        Span::styled(format!("{prefix}{label:<9}"), Theme::label()),
        Span::styled(text, style),
    ])
}

/// A capability the chosen backend cannot honour: the reason replaces the
/// value, so the row is visibly present and visibly inert.
fn unavailable_line<'a>(label: &str, reason: &str, active: bool) -> Line<'a> {
    let prefix = if active { "▸ " } else { "  " };
    Line::from(vec![
        Span::styled(format!("{prefix}{label:<9}"), Theme::label()),
        Span::styled(
            format!("({reason})"),
            Style::default()
                .fg(Theme::text_muted())
                .add_modifier(Modifier::DIM),
        ),
    ])
}

/// The path sub-list: an anchor row carrying the list chords, then one row per
/// path showing its `‹ ro | rw ›` intent.
fn paths_block<'a>(state: &SandboxEditorState<'a>, active: bool, width: u16) -> Vec<Line<'a>> {
    let mut lines = vec![editor_field_line_with_cursor(
        field_label(SandboxField::Paths),
        sublist_summary(state.path_index, state.paths.len(), active, true),
        true,
        active,
        None,
    )];
    for (i, (path, mode)) in state.paths.iter().enumerate() {
        lines.push(path_row(path, *mode, i == state.path_index, active, width));
    }
    lines
}

/// The allowed-domain sub-list. Entries stay editable in every network mode —
/// they are the profile's data — but the anchor says when the list is inert.
fn domains_block<'a>(state: &SandboxEditorState<'a>, active: bool, width: u16) -> Vec<Line<'a>> {
    let mut anchor = editor_field_line_with_cursor(
        field_label(SandboxField::Domains),
        sublist_summary(state.domain_index, state.domains.len(), active, false),
        true,
        active,
        None,
    );
    if !matches!(state.network, NetworkMode::Allowlist) {
        anchor.spans.push(Span::styled(
            format!("  (inactive — network is {})", state.network),
            Style::default().fg(Theme::text_muted()),
        ));
    }
    let mut lines = vec![anchor];
    for (i, domain) in state.domains.iter().enumerate() {
        lines.push(entry_row(
            domain,
            None,
            i == state.domain_index,
            active,
            width,
        ));
    }
    lines
}

/// The anchor row's value: the position in the list, plus the chords that edit
/// it while the anchor is focused (where letters are safe — nothing types into
/// a selector).
fn sublist_summary(index: usize, len: usize, active: bool, reorderable: bool) -> String {
    if len == 0 {
        return if active {
            "none — n adds one".to_string()
        } else {
            "none".to_string()
        };
    }
    let position = format!("{}/{len}", index.min(len - 1) + 1);
    if !active {
        return position;
    }
    if reorderable {
        format!("{position}   n add · d remove · [ ] reorder")
    } else {
        format!("{position}   n add · d remove")
    }
}

/// One path entry row: the path, then its intent right-aligned in a fixed cell
/// so every mode lines up.
fn path_row<'a>(
    path: &str,
    mode: PathMode,
    selected: bool,
    list_focused: bool,
    width: u16,
) -> Line<'a> {
    entry_row(path, Some(mode), selected, list_focused, width)
}

/// One sub-list entry row, with an optional right-aligned mode cell.
///
/// The row is fitted to `width`: the entry text elides first, and the intent
/// cell itself is dropped once there is no room for both — an overflowing row
/// would push the modal's own border off the screen.
fn entry_row<'a>(
    text: &str,
    mode: Option<PathMode>,
    selected: bool,
    list_focused: bool,
    width: u16,
) -> Line<'a> {
    let marker = if selected { "▸ " } else { "  " };
    let style = if selected && list_focused {
        Style::default()
            .fg(Theme::border_focused())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Theme::text_secondary())
    };

    let width = width as usize;
    // Room for the indent, the marker, the intent cell, a separating column and
    // at least a glyph of the entry itself.
    let show_mode = mode.is_some() && width >= ENTRY_INDENT.len() + MODE_CELL_WIDTH + 4;
    let head_budget = width.saturating_sub(if show_mode { MODE_CELL_WIDTH + 1 } else { 0 });
    let shown = if text.trim().is_empty() {
        "(empty)"
    } else {
        text
    };
    let head = super::truncate_ellipsis(&format!("{ENTRY_INDENT}{marker}{shown}"), head_budget);

    let mut spans = Vec::with_capacity(3);
    let head_width = head.width();
    spans.push(Span::styled(head, style));
    if let Some(mode) = mode.filter(|_| show_mode) {
        // `head_budget` left the cell its columns, so this gap is at least one.
        let pad = width.saturating_sub(head_width + MODE_CELL_WIDTH);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(format!("‹ {mode} ›"), style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::modals::SandboxEditorModal;
    use SandboxField as F;

    fn state() -> SandboxEditorState<'static> {
        SandboxEditorState {
            editing: false,
            field: F::Name,
            visible_fields: vec![
                F::Name,
                F::Backend,
                F::Paths,
                F::Network,
                F::Domains,
                F::PromptDomains,
                F::ReadScope,
                F::Memory,
                F::Cpus,
                F::Image,
                F::Containerfile,
                F::Fallback,
            ],
            cursor: 0,
            name: "",
            backend: SandboxBackendKind::Auto,
            resolved: None,
            effective_backend: SandboxBackendKind::Auto,
            paths: Vec::new(),
            path_index: 0,
            network: NetworkMode::Allowlist,
            domains: Vec::new(),
            domain_index: 0,
            prompt_new_domains: true,
            read_scope: ReadScope::HostMinusSecrets,
            memory: "",
            cpus: "",
            image: "",
            containerfile: "",
            allow_unsandboxed_fallback: false,
        }
    }

    /// A modal whose two sub-lists each hold one row, so `visible_fields`
    /// includes the per-entry text and mode rows.
    fn modal_with_one_entry_each() -> SandboxEditorModal {
        let mut m = SandboxEditorModal::default();
        m.paths.push(Default::default());
        m.domains.push(Default::default());
        m
    }

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn body_text(state: &SandboxEditorState<'_>) -> Vec<String> {
        editor_body_lines(state, 60).0.iter().map(text).collect()
    }

    #[test]
    fn body_rows_follow_the_visible_field_projection() {
        let mut s = state();
        s.paths = vec![("~/dev/app", PathMode::ReadWrite)];
        s.visible_fields = SandboxEditorModal::default().visible_fields();
        let (lines, _, spans) = editor_body_lines(&s, 60);
        // One span per field, and the spans tile the body without gaps.
        assert_eq!(spans.len(), s.visible_fields.len());
        let mut next = 0;
        for (start, count) in &spans {
            assert_eq!(*start, next);
            next += count;
        }
        assert_eq!(next, lines.len());
    }

    #[test]
    fn active_row_points_at_the_focused_field() {
        let mut s = state();
        s.field = F::Network;
        let (_, active, spans) = editor_body_lines(&s, 60);
        let idx = s
            .visible_fields
            .iter()
            .position(|f| *f == F::Network)
            .unwrap();
        assert_eq!(active, Some(spans[idx].0));
    }

    #[test]
    fn path_rows_render_under_the_anchor_with_their_mode() {
        let mut s = state();
        s.field = F::Paths;
        s.paths = vec![
            ("~/dev/app", PathMode::ReadWrite),
            ("/srv/shared", PathMode::ReadOnly),
        ];
        s.path_index = 1;
        let rows = paths_block(&s, true, 60);
        assert_eq!(rows.len(), 3, "anchor + one row per path");
        assert!(text(&rows[0]).contains("2/2"));
        assert!(text(&rows[0]).contains("n add · d remove · [ ] reorder"));
        assert!(text(&rows[1]).contains("~/dev/app"));
        assert!(text(&rows[1]).contains("‹ rw ›"));
        // The selection marker is on the second entry.
        assert!(!text(&rows[1]).contains('▸'));
        assert!(text(&rows[2]).contains('▸'));
        assert!(text(&rows[2]).contains("‹ ro ›"));
    }

    #[test]
    fn empty_sublists_offer_the_add_chord_only_while_focused() {
        assert_eq!(sublist_summary(0, 0, true, true), "none — n adds one");
        assert_eq!(sublist_summary(0, 0, false, true), "none");
        // An unfocused list shows its position without the chord noise.
        assert_eq!(sublist_summary(1, 3, false, true), "2/3");
        assert_eq!(sublist_summary(0, 2, true, false), "1/2   n add · d remove");
        // A stale index past the end still names a row that exists.
        assert_eq!(sublist_summary(9, 2, false, false), "2/2");
    }

    #[test]
    fn domain_anchor_says_when_the_list_is_inert() {
        let mut s = state();
        s.domains = vec!["github.com"];
        assert!(!text(&domains_block(&s, false, 60)[0]).contains("inactive"));
        s.network = NetworkMode::Full;
        assert!(text(&domains_block(&s, false, 60)[0]).contains("inactive — network is full"));
    }

    #[test]
    fn place_only_fields_state_their_reason_on_a_policy_backend() {
        let mut s = state();
        s.backend = SandboxBackendKind::Seatbelt;
        s.effective_backend = SandboxBackendKind::Seatbelt;
        let rows = body_text(&s);
        let joined = rows.join("\n");
        assert!(joined.contains("memory"));
        assert!(
            joined.contains("unavailable — seatbelt applies a policy to a host process"),
            "{joined}"
        );
        assert!(joined.contains("unavailable — seatbelt runs the host toolchain"));
        // A policy backend keeps its read scope; a place is what loses it.
        assert!(!joined.contains("no host filesystem"));
    }

    #[test]
    fn read_scope_states_its_reason_on_a_place_backend() {
        let mut s = state();
        s.backend = SandboxBackendKind::Docker;
        s.effective_backend = SandboxBackendKind::Docker;
        let joined = body_text(&s).join("\n");
        assert!(joined.contains("no host filesystem to read past the workspace"));
        // Limits and images are exactly what a place backend *can* do.
        assert!(!joined.contains("applies a policy"));
    }

    #[test]
    fn unresolved_auto_rules_nothing_out() {
        let joined = body_text(&state()).join("\n");
        assert!(!joined.contains("unavailable"), "{joined}");
        // …and a probed `auto` narrows to the resolved backend's shape.
        let mut s = state();
        s.resolved = Some(SandboxBackendKind::Bwrap);
        s.effective_backend = SandboxBackendKind::Bwrap;
        assert!(body_text(&s).join("\n").contains("unavailable"));
    }

    #[test]
    fn prompt_toggle_is_inert_outside_the_allowlist() {
        let mut s = state();
        assert_eq!(
            text(&field_line(F::PromptDomains, &s, false)).trim_end(),
            "  prompt   on"
        );
        s.network = NetworkMode::Full;
        assert!(text(&field_line(F::PromptDomains, &s, false)).contains("no domain unlisted"));
        s.network = NetworkMode::None;
        assert!(text(&field_line(F::PromptDomains, &s, false)).contains("lets nothing out"));
    }

    #[test]
    fn wording_exists_exactly_when_the_rule_refuses_input() {
        let fields = SandboxEditorModal::default().visible_fields();
        for backend in SandboxBackendKind::ALL {
            for network in NetworkMode::ALL {
                for field in &fields {
                    let shape = backend.shape();
                    let reason = unavailable_reason(*field, *backend, shape, *network);
                    assert_eq!(
                        reason.is_none(),
                        sandbox_field_available(*field, shape, *network),
                        "{field:?} on {backend} with {network}"
                    );
                    // Every reason is specific, never the generic fallback.
                    if let Some(r) = reason {
                        assert!(r.contains('—'), "{r}");
                    }
                }
            }
        }
    }

    #[test]
    fn toggles_read_on_and_off() {
        assert_eq!(
            text(&toggle_line("escape", true, false)).trim_end(),
            "  escape   on"
        );
        assert_eq!(
            text(&toggle_line("escape", false, true)).trim_end(),
            "▸ escape   off"
        );
    }

    #[test]
    fn backend_row_shows_what_auto_resolved_to() {
        let mut s = state();
        assert_eq!(
            text(&backend_line(&s, false)).trim_end(),
            "  backend  ‹ auto ›"
        );
        s.resolved = Some(SandboxBackendKind::Seatbelt);
        assert!(text(&backend_line(&s, false)).contains("‹ auto ›  → seatbelt"));
        // An explicit choice has nothing to resolve.
        s.backend = SandboxBackendKind::Bwrap;
        assert_eq!(
            text(&backend_line(&s, false)).trim_end(),
            "  backend  ‹ bwrap ›"
        );
    }

    #[test]
    fn placeholders_yield_to_the_caret_once_focused() {
        assert_eq!(placeholder("", "(required)", false), "(required)");
        assert_eq!(placeholder("", "(required)", true), "");
        assert_eq!(placeholder("dev", "(required)", false), "dev");
    }

    #[test]
    fn entry_rows_fit_their_width() {
        let long = "~/a-very-long-path-that-will-not-fit-in-this-row/at-all";
        for width in [0u16, 1, 8, 14, 20, 40, 80] {
            let rendered = text(&entry_row(
                long,
                Some(PathMode::ReadWrite),
                true,
                true,
                width,
            ));
            assert!(
                rendered.width() <= width as usize,
                "width {width}: {rendered:?}"
            );
            // The intent cell is dropped only where it cannot fit beside the
            // path; wherever it is shown, it lands on the row's right edge.
            if width >= 14 {
                assert!(rendered.ends_with("‹ rw ›"), "width {width}: {rendered:?}");
                assert_eq!(rendered.width(), width as usize, "width {width}");
            } else {
                assert!(!rendered.contains('‹'), "width {width}: {rendered:?}");
            }
        }
        // A blank row (just added with `n`) still reads as a row.
        assert!(text(&entry_row("  ", None, false, false, 40)).contains("(empty)"));
    }

    #[test]
    fn footer_names_the_shape_and_pins_the_hints() {
        let mut s = state();
        assert!(shape_summary(&s).starts_with("auto"));
        s.effective_backend = SandboxBackendKind::Seatbelt;
        assert!(shape_summary(&s).starts_with("policy"));
        s.effective_backend = SandboxBackendKind::Podman;
        assert!(shape_summary(&s).starts_with("place"));
        let footer = editor_footer_lines(&s);
        assert_eq!(footer.len(), 3);
        assert!(text(&footer[1]).contains("does not prove containment"));
        assert!(text(&footer[2]).contains("adjust"));
    }

    #[test]
    fn windowing_keeps_the_active_field_visible_and_the_footer_pinned() {
        let mut s = state();
        s.paths = (0..30).map(|_| ("~/dev/app", PathMode::ReadOnly)).collect();
        s.visible_fields = modal_with_one_entry_each().visible_fields();
        s.field = F::Fallback;
        let height = 12;
        let (lines, rows) = windowed_editor_lines(&s, 60, height);
        assert_eq!(lines.len(), height as usize);
        // The last body row before the pinned footer belongs to the focused
        // field, and the hint line is the very last row.
        assert!(rows
            .iter()
            .any(|(i, _, _)| s.visible_fields[*i] == F::Fallback));
        assert!(text(&lines[lines.len() - 1]).contains("move"));
    }

    fn draw(width: u16, height: u16, s: &SandboxEditorState<'_>) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let (hits, _) = render_sandbox_editor_modal(f, s);
                for h in hits {
                    assert!(h.rect.right() <= width, "hitbox escapes the terminal");
                }
            })
            .unwrap();
    }

    #[test]
    fn renders_without_panicking_at_small_sizes() {
        let mut s = state();
        s.visible_fields = SandboxEditorModal::default().visible_fields();
        for (w, h) in [(1, 1), (3, 2), (12, 4), (30, 10), (80, 24), (200, 60)] {
            draw(w, h, &s);
        }
        // …and with both sub-lists populated, where the body is tallest.
        let mut filled = state();
        filled.paths = vec![("~/dev/app", PathMode::ReadWrite); 6];
        filled.domains = vec!["github.com:443"; 6];
        filled.field = F::Paths;
        filled.visible_fields = modal_with_one_entry_each().visible_fields();
        for (w, h) in [(1, 1), (20, 6), (80, 24)] {
            draw(w, h, &filled);
        }
    }
}
