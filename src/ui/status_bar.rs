use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::theme::Theme;
use crate::app::{StatusLevel, StatusMessage};
use crate::session::{Action, KeyBindings};

fn brand_style() -> Style {
    Style::default()
        .fg(Theme::accent())
        .add_modifier(Modifier::BOLD)
}

const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub fn render_header(frame: &mut Frame, area: Rect, badge: Option<HeaderBadge<'_>>) {
    if area.height == 0 {
        return;
    }
    let mut spans = vec![
        Span::styled(" friring", brand_style()),
        Span::styled(
            "  Multi-Session Agent Orchestrator",
            Style::default().fg(Theme::text_secondary()),
        ),
        Span::styled(
            concat!("  v", env!("FRIRING_VERSION")),
            Style::default().fg(Theme::text_muted()),
        ),
    ];
    if let Some(latest) = badge.as_ref().and_then(|b| b.update_latest) {
        spans.push(Span::styled(
            format!("  ⬆ v{latest} available"),
            Style::default()
                .fg(Theme::accent())
                .add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    if let Some(badge) = badge {
        let mut spans: Vec<Span<'_>> = Vec::new();
        if let Some(name) = badge.active_session {
            spans.push(Span::styled(
                name.to_string(),
                Style::default().fg(Theme::text_primary()),
            ));
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("◐ {} ", badge.theme_label),
            Style::default().fg(Theme::accent()),
        ));
        let right = Line::from(spans).alignment(ratatui::layout::Alignment::Right);
        frame.render_widget(Paragraph::new(right), area);
    }
}

/// Right-aligned overlay rendered on top of the header, plus the optional
/// left-aligned "update available" version badge.
pub struct HeaderBadge<'a> {
    pub active_session: Option<&'a str>,
    pub theme_label: &'a str,
    /// When `Some`, the latest available release version (no leading `v`),
    /// rendered as a left-aligned "⬆ vX.Y.Z available" badge after the version.
    pub update_latest: Option<&'a str>,
}

/// State needed to render the footer bar.
pub struct FooterState<'a> {
    pub session_count: usize,
    /// Sessions currently `Blocked` (needing attention). Rendered as a badge
    /// next to the session count — with the live `NextBlockedSession`
    /// shortcut as its hint — so attention is visible even when the sidebar
    /// is hidden (narrow terminals) or its dots are clipped.
    pub blocked_count: usize,
    pub status: Option<&'a StatusMessage>,
    pub focus_label: &'a str,
    pub sync_in_progress: bool,
    pub tick_count: u64,
    pub automation_count: usize,
    pub file_viewer_open: bool,
    /// Feature flags gating the panel buttons (hidden when off).
    pub tasks_enabled: bool,
    pub file_viewer_enabled: bool,
    pub info_panel_enabled: bool,
    /// Live keybindings, so each footer pill can show its (rebindable) shortcut.
    pub keybindings: &'a KeyBindings,
    /// The armed leader chord, if the leader is pending a second key. Shown as
    /// a badge so the armed state is visible even when the which-key overlay
    /// is delayed (`prefix.hint_delay_ms`) — an armed leader with no feedback
    /// anywhere reads as a frozen app.
    pub prefix_armed: Option<String>,
}

/// The clickable footer buttons, in render order, paired with the `Action` each
/// dispatches. `render_footer` filters this by feature flags and returns the
/// surviving `(ButtonHit, Action)` pairs so the click map can't drift from the
/// render.
///
/// Ordered by F-key (`Help · F1`, `Info · F2`, … `Settings · F6`) with `Quit`
/// last, so the row reads in the same order as the function-key alternates.
const FOOTER_BUTTONS: &[(&str, Action)] = &[
    ("Help", Action::ToggleHelp),
    ("Info", Action::ToggleInfoPanel),
    ("Files", Action::ToggleFileViewer),
    ("Theme", Action::OpenThemePicker),
    ("Tasks", Action::FocusTasks),
    ("Settings", Action::OpenSettings),
    ("Quit", Action::QuitApp),
];

/// The footer buttons that survive the current feature flags, in render order.
/// The panel toggles (Info/Files/Tasks) are dropped when their feature is off
/// (clicking a button that just toasts "disabled" would be noise).
fn footer_entries(flags: &FooterState<'_>) -> Vec<(&'static str, Action)> {
    FOOTER_BUTTONS
        .iter()
        .copied()
        .filter(|(_, action)| match action {
            Action::FocusTasks => flags.tasks_enabled,
            Action::ToggleFileViewer => flags.file_viewer_enabled,
            Action::ToggleInfoPanel => flags.info_panel_enabled,
            _ => true,
        })
        .collect()
}

/// The panel-toggle pills. They are the *optional* group: the trim drops them
/// together rather than shedding a random one, so the row never reads as a
/// half-collapsed set.
fn is_optional(action: &Action) -> bool {
    matches!(
        action,
        Action::ToggleInfoPanel | Action::ToggleFileViewer | Action::FocusTasks
    )
}

/// Order the *essential* pills give way in once even key-only chips overflow —
/// the lowest rank goes first. Cosmetics (Theme) lead; `Help` is last out
/// because it is the one chip that documents every key the row can no longer
/// show.
fn pill_drop_rank(action: Action) -> u8 {
    match action {
        Action::ToggleHelp => 3,
        Action::QuitApp => 2,
        Action::OpenSettings => 1,
        _ => 0,
    }
}

/// How much of a pill's `label · shortcut` text survives the responsive trim.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PillText {
    /// ` Help · F1 ` — the resting form.
    Full,
    /// ` Help F1 ` — the ` · ` separator dropped, buying two columns per pill
    /// back for the left-hand text.
    Tight,
    /// ` F1 ` — the last resort: the key alone (a button with no bound key
    /// keeps its label, since a blank chip would be unclickable noise).
    KeyOnly,
}

/// Label each pill with its live (rebindable) shortcut at the given trim level.
/// Index-aligned with `entries` so the click→action map survives every step of
/// the ladder.
fn pill_labels(
    state: &FooterState<'_>,
    entries: &[(&'static str, Action)],
    text: PillText,
) -> Vec<String> {
    entries
        .iter()
        .map(|(label, action)| {
            let shortcut = crate::session::compact_shortcut(state.keybindings.chords_for(*action));
            match (shortcut, text) {
                (Some(sc), PillText::Full) => format!("{label} · {sc}"),
                (Some(sc), PillText::Tight) => format!("{label} {sc}"),
                (Some(sc), PillText::KeyOnly) => sc,
                (None, _) => (*label).to_string(),
            }
        })
        .collect()
}

/// Pick the pill row for a footer `width` columns wide, degrading in the order
/// [`render_footer`] documents. `left_width` is what the left-hand text wants at
/// full length — it only decides whether the ` · ` separators are affordable,
/// since past that point the left text is what yields. `pinned` is the width
/// the never-dropped left segments reserve.
///
/// Returns the surviving entries with their labels, index-aligned.
fn fit_pills(
    state: &FooterState<'_>,
    width: u16,
    left_width: u16,
    pinned: u16,
) -> (Vec<(&'static str, Action)>, Vec<String>) {
    let mut entries = footer_entries(state);

    // 1. Resting form — only while the whole row (pills *and* the full left
    //    text) fits, so the separators are never paid for with dropped text.
    let full = pill_labels(state, &entries, PillText::Full);
    if pill_block_width(&full).saturating_add(left_width) <= width {
        return (entries, full);
    }

    // Everything below sacrifices left-hand text, which the caller trims — but
    // never past the pinned segments, so the pills make room for those.
    let room = width.saturating_sub(pinned);

    // 2. Drop the ` · ` separators.
    let tight = pill_labels(state, &entries, PillText::Tight);
    if pill_block_width(&tight) <= room {
        return (entries, tight);
    }

    // 3. The pills no longer fit on their own: drop the optional panel toggles.
    entries.retain(|(_, action)| !is_optional(action));
    let tight = pill_labels(state, &entries, PillText::Tight);
    if pill_block_width(&tight) <= room {
        return (entries, tight);
    }

    // 4. The compact state: chips shrink to the bare key, handing every freed
    //    column back to the left-hand text.
    let mut keys = pill_labels(state, &entries, PillText::KeyOnly);

    // 5. Still overflowing — shed whole chips, least useful first.
    while pill_block_width(&keys) > room && !entries.is_empty() {
        let Some(idx) = entries
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, action))| pill_drop_rank(*action))
            .map(|(idx, _)| idx)
        else {
            break;
        };
        entries.remove(idx);
        keys.remove(idx);
    }
    (entries, keys)
}

/// Total width of the footer pill block: each pill is ` label ` (the label plus
/// the two padding spaces) and pills are joined by a single-space separator.
/// Mirrors `render_button_bar`'s packing so the responsive trim below agrees
/// with what actually renders.
fn pill_block_width(labels: &[String]) -> u16 {
    if labels.is_empty() {
        return 0;
    }
    let pills: u16 = labels.iter().map(|l| l.chars().count() as u16 + 2).sum();
    pills + labels.len() as u16 - 1
}

/// Render the footer bar and return each clickable button's hitbox paired with
/// the `Action` it dispatches, packed against the right edge. Info/Files/Tasks
/// are gated by their feature flags.
///
/// The row is two blocks that must never touch: the left-hand text (focus,
/// counts, key hints) and the right-packed pills. Both are laid out against the
/// same column budget and painted into *disjoint* rects, so nothing can bleed
/// under a pill or through the one-column gaps between them. What gives way as
/// the terminal narrows, in order:
///
/// 1. the ` · ` separators inside the pills (` Help · F1 ` → ` Help F1 `),
///    bought back as columns for the text;
/// 2. the left-hand text, segment by segment, least useful first (the priority
///    order on `left_segments`) — text yields before any button does;
/// 3. the optional panel-toggle pills, dropped together, once the pills alone
///    no longer fit;
/// 4. the pill labels, leaving key-only chips (` F1 `) and handing every freed
///    column back to the text;
/// 5. whole chips, least useful first (`pill_drop_rank`).
pub fn render_footer(
    frame: &mut Frame,
    area: Rect,
    state: &FooterState<'_>,
) -> Vec<(super::ButtonHit, Action)> {
    if area.height == 0 || area.width == 0 {
        return Vec::new();
    }
    // The footer always carries the idle session/automation counts + the focus
    // hint; the transient status/error message (and sync spinner) live on their
    // own dedicated row above the footer (`render_status_message_row`), so
    // nothing here can be overwritten by the right-aligned pills.
    let mut segments = left_segments(state);
    let left_width = segments_width(&segments);
    let pinned = segments
        .iter()
        .filter(|s| s.priority == PRIO_PINNED)
        .map(LeftSegment::width)
        .sum();

    let (entries, labels) = fit_pills(state, area.width, left_width, pinned);

    // The text gets its own rect, ending where the pills begin: whatever the
    // trim can't shed is clipped there rather than painted under them.
    let budget = area.width.saturating_sub(pill_block_width(&labels));
    trim_segments(&mut segments, budget);
    let spans: Vec<Span<'_>> = segments.into_iter().flat_map(|s| s.spans).collect();
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect {
            width: budget,
            ..area
        },
    );

    let specs: Vec<super::ButtonSpec<'_>> = labels
        .iter()
        .map(|l| super::ButtonSpec::secondary(l))
        .collect();
    let hits = super::render_button_bar(frame, area, &specs, true);

    // Each placed hit keeps its index into `entries`, so the click→action map
    // follows the same feature-filtered, responsively trimmed list that was
    // rendered.
    hits.into_iter()
        .map(|hit| {
            let action = entries[hit.index].1;
            (hit, action)
        })
        .collect()
}

/// Render the active status/error message (or the live sync spinner) into its
/// own full-width row directly above the footer. The badge + message text get
/// the whole line, so a long message is never clipped by the footer pills —
/// this is the fix for messages being hidden under the right-aligned buttons.
///
/// The caller only carves this row (`PanelAreas::status_message`) when there is
/// something to show, so the `else` branch below is a defensive no-op.
pub fn render_status_message_row(frame: &mut Frame, area: Rect, state: &FooterState<'_>) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut spans: Vec<Span<'_>> = Vec::new();
    if state.sync_in_progress {
        push_spinner_badge(&mut spans, state.tick_count, "SYNC");
        let text = state
            .status
            .map_or("Syncing...".to_string(), |s| s.text.clone());
        spans.push(Span::styled(
            format!(" {text} "),
            Style::default().fg(Theme::accent()),
        ));
    } else if let Some(msg) = state.status {
        push_status_message(&mut spans, msg);
    } else {
        return; // nothing to show — row shouldn't have been carved
    }
    // A status row is a single line; ratatui clips a longer message to width.
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn push_status_message<'a>(spans: &mut Vec<Span<'a>>, msg: &'a StatusMessage) {
    let (badge_text, badge_bg, text_color) = match msg.level {
        StatusLevel::Info => (" INFO ", Theme::accent(), Theme::text_secondary()),
        StatusLevel::Success => (" ✓ SYNC ", Theme::tool_allowed(), Theme::tool_allowed()),
        StatusLevel::Error => (" ERROR ", Theme::status_error(), Theme::status_error()),
    };
    spans.push(Span::styled(
        badge_text,
        Style::default().fg(Theme::text_primary()).bg(badge_bg),
    ));
    spans.push(Span::styled(
        format!(" {} ", msg.text),
        Style::default().fg(text_color),
    ));
}

/// Drop order for the footer's left-hand text — the responsive trim sheds the
/// *highest* number first, so what survives a narrow footer is the most useful
/// text rather than a half-word cut off under the pills.
///
/// `PRIO_PINNED` is never dropped: an armed leader with no feedback anywhere
/// reads as a frozen app, so the pills reserve room for its badge instead (see
/// [`fit_pills`]).
const PRIO_PINNED: u8 = 0;
const PRIO_BLOCKED: u8 = 1;
const PRIO_FOCUS: u8 = 2;
/// The file viewer's hints **outrank the counts below** — they are contextual
/// state, not ambient: while the viewer is open they are the live guidance for
/// the pane the user is driving, and nothing else on screen carries them, where
/// the session count is also in the sidebar. So a narrow footer with the viewer
/// open keeps `j/k Move` and drops ` N session(s) `. They trim from their own
/// tail (`+ index`), so `j/k Move` outlives `n/N Next/Prev`.
const PRIO_FILE_HINTS: u8 = 10;
const PRIO_SESSIONS: u8 = 20;
const PRIO_AUTOMATIONS: u8 = 21;
/// The global hints go first of everything: they duplicate the help overlay and
/// apply whatever is focused. Tail-first within the pair, so `^O Open` goes
/// before `^H/^L Focus`.
const PRIO_GLOBAL_HINTS: u8 = 30;

/// One self-contained chunk of the footer's left-hand text, with the priority
/// that orders the responsive trim. Segments carry their own padding, so the
/// row re-flows cleanly whichever ones are dropped.
struct LeftSegment {
    spans: Vec<Span<'static>>,
    priority: u8,
    /// A hint: it fills the tail of a row that already shows everything ranked
    /// above it, and is never kept in place of something *more important* that
    /// didn't fit. Without this a stray `^O Open` survives alone on a footer too
    /// narrow for the focus label — which reads as a leftover, not as a degraded
    /// row. Note this is relative to `priority`, not to some hints-lose-to-text
    /// rule: the file viewer's hints outrank the counts and do displace them.
    trailing: bool,
}

impl LeftSegment {
    fn new(priority: u8, spans: Vec<Span<'static>>) -> Self {
        Self {
            spans,
            priority,
            trailing: false,
        }
    }

    fn trailing(mut self) -> Self {
        self.trailing = true;
        self
    }

    fn width(&self) -> u16 {
        self.spans
            .iter()
            .map(|s| s.content.chars().count() as u16)
            .sum()
    }
}

fn segments_width(segments: &[LeftSegment]) -> u16 {
    segments.iter().map(LeftSegment::width).sum()
}

/// A `key description` hint pair, styled as one trailing segment.
fn hint_segment(priority: u8, key: &'static str, desc: &'static str) -> LeftSegment {
    LeftSegment::new(
        priority,
        vec![
            Span::styled(key, Theme::keybind().add_modifier(Modifier::BOLD)),
            Span::styled(desc, Theme::keybind_desc()),
        ],
    )
    .trailing()
}

/// The file viewer's navigation hints, shown while it is open. One segment each
/// so a narrow footer keeps `j/k Move` long after it has lost `n/N Next/Prev`.
const FILE_VIEWER_HINTS: &[(&str, &str)] = &[
    ("j/k", " Move  "),
    ("h/l", " Collapse/Expand  "),
    ("\u{23CE}", " Open  "),
    ("/", " Search  "),
    ("n/N", " Next/Prev "),
];

/// The footer's left-hand text in render order, split into individually
/// droppable segments.
///
/// Render order is the *reading* order — state first (leader badge, focus,
/// counts), key hints last — and is deliberately independent of `priority`,
/// which is what drives the trim: the blocked badge renders after the session
/// count but outlives it, and the file-viewer hints render after both counts
/// yet outrank them. So dropping a segment mid-row re-flows everything after
/// it; only the tail (the global hints) shortens the row in place.
fn left_segments(state: &FooterState<'_>) -> Vec<LeftSegment> {
    let mut segments = Vec::new();
    // An armed leader replaces nothing — it prepends, so the badge sits where
    // the eye already is and the pending state is unmissable.
    if let Some(chord) = &state.prefix_armed {
        segments.push(LeftSegment::new(
            PRIO_PINNED,
            vec![Span::styled(
                format!(" {chord} "),
                Style::default()
                    .bg(Theme::accent())
                    .fg(Theme::modal_bg())
                    .add_modifier(Modifier::BOLD),
            )],
        ));
    }
    segments.push(LeftSegment::new(
        PRIO_FOCUS,
        vec![Span::styled(
            format!(" {} ", state.focus_label),
            Theme::focused_title(),
        )],
    ));
    segments.push(LeftSegment::new(
        PRIO_SESSIONS,
        vec![Span::styled(
            format!(" {} session(s) ", state.session_count),
            Style::default().fg(Theme::text_secondary()),
        )],
    ));
    if state.blocked_count > 0 {
        let shortcut = crate::session::compact_shortcut(
            state.keybindings.chords_for(Action::NextBlockedSession),
        );
        let label = match shortcut {
            Some(sc) => format!(" \u{25c6} {} blocked · {sc} ", state.blocked_count),
            None => format!(" \u{25c6} {} blocked ", state.blocked_count),
        };
        segments.push(LeftSegment::new(
            PRIO_BLOCKED,
            vec![Span::styled(
                label,
                Style::default()
                    .fg(Theme::text_primary())
                    .bg(super::status_color(crate::session::SessionStatus::Blocked)),
            )],
        ));
    }
    if state.automation_count > 0 {
        segments.push(LeftSegment::new(
            PRIO_AUTOMATIONS,
            vec![Span::styled(
                format!(" {} automation(s) ", state.automation_count),
                Style::default()
                    .fg(Theme::text_primary())
                    .bg(Theme::accent()),
            )],
        ));
    }
    if state.file_viewer_open {
        for (idx, (key, desc)) in FILE_VIEWER_HINTS.iter().enumerate() {
            segments.push(hint_segment(PRIO_FILE_HINTS + idx as u8, key, desc));
        }
    }
    // Focus + Open stay informational hints (no single click target); the footer
    // buttons (Help / Info / Files / Theme / Tasks / Settings / Quit) are
    // rendered as right-aligned clickable pills.
    let bold_key = Theme::keybind().add_modifier(Modifier::BOLD);
    let desc = Theme::keybind_desc();
    segments.push(
        LeftSegment::new(
            PRIO_GLOBAL_HINTS,
            vec![
                Span::styled(" ^H", bold_key),
                Span::styled("/", desc),
                Span::styled("^L", bold_key),
                Span::styled(" Focus ", desc),
            ],
        )
        .trailing(),
    );
    segments.push(hint_segment(PRIO_GLOBAL_HINTS + 1, "^O", " Open "));
    segments
}

/// Fit the left-hand text into `budget` columns by keeping segments
/// most-important-first while they still fit, then rendering the survivors in
/// render order.
///
/// Keeping rather than dropping is what stops a *wide* segment from starving the
/// row: the 19-column blocked badge skips when it doesn't fit and the columns go
/// to the focus label instead, where a drop-until-it-fits loop would have shed
/// the label first and then the badge too, leaving the row blank. Trailing
/// segments are the exception — a hint never fills in for state that didn't fit.
///
/// Pinned segments are kept whatever the budget: when even they overflow, the
/// rect clips them, which is the compact last resort (show as much as there is
/// room for), not a badge painted over a pill.
fn trim_segments(segments: &mut Vec<LeftSegment>, budget: u16) {
    if segments_width(segments) <= budget {
        return;
    }
    let mut by_priority: Vec<usize> = (0..segments.len()).collect();
    by_priority.sort_by_key(|&idx| segments[idx].priority); // stable: ties keep render order
    let mut keep = vec![false; segments.len()];
    let mut spent = 0u16;
    let mut skipped = false;
    for idx in by_priority {
        let segment = &segments[idx];
        let fits = spent.saturating_add(segment.width()) <= budget;
        if segment.priority == PRIO_PINNED || (fits && !(segment.trailing && skipped)) {
            spent = spent.saturating_add(segment.width());
            keep[idx] = true;
        } else {
            skipped = true;
        }
    }
    *segments = std::mem::take(segments)
        .into_iter()
        .zip(keep)
        .filter(|(_, keep)| *keep)
        .map(|(segment, _)| segment)
        .collect();
}

fn push_spinner_badge<'a>(spans: &mut Vec<Span<'a>>, tick_count: u64, label: &'a str) {
    let idx = (tick_count as usize / 10) % SPINNER_CHARS.len();
    let spinner = SPINNER_CHARS[idx];
    spans.push(Span::styled(
        format!(" {spinner} {label} "),
        Style::default()
            .fg(Theme::text_primary())
            .bg(Theme::accent()),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    /// Render the header into a 120×1 buffer (a realistic three-panel width,
    /// wide enough that the right-aligned theme overlay doesn't paint over the
    /// left badge) and return its single line.
    fn header_line(update_latest: Option<&str>) -> String {
        let backend = TestBackend::new(120, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_header(
                    f,
                    Rect::new(0, 0, 120, 1),
                    Some(HeaderBadge {
                        active_session: None,
                        theme_label: "Default",
                        update_latest,
                    }),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect()
    }

    #[test]
    fn update_badge_renders_when_a_newer_release_is_available() {
        let line = header_line(Some("0.114.0"));
        assert!(
            line.contains("⬆ v0.114.0 available"),
            "badge missing from header: {line:?}"
        );
    }

    #[test]
    fn no_update_badge_without_a_newer_release() {
        let line = header_line(None);
        assert!(
            !line.contains("available"),
            "unexpected badge in header: {line:?}"
        );
    }

    fn test_keybindings() -> &'static KeyBindings {
        static KB: std::sync::OnceLock<KeyBindings> = std::sync::OnceLock::new();
        KB.get_or_init(KeyBindings::default)
    }

    fn footer_state(file_viewer_open: bool) -> FooterState<'static> {
        FooterState {
            prefix_armed: None,
            session_count: 1,
            blocked_count: 0,
            status: None,
            focus_label: "Files",
            sync_in_progress: false,
            tick_count: 0,
            automation_count: 0,
            file_viewer_open,
            tasks_enabled: true,
            file_viewer_enabled: true,
            info_panel_enabled: true,
            keybindings: test_keybindings(),
        }
    }

    /// Render the given state into a 120×1 buffer and return (button+action
    /// hits, line text).
    fn render_footer_state(
        state: &FooterState<'_>,
    ) -> (Vec<(super::super::ButtonHit, Action)>, String) {
        footer_at(120, state)
    }

    /// Render `state` into a `width`×1 buffer and return (button+action hits,
    /// line text).
    fn footer_at(
        width: u16,
        state: &FooterState<'_>,
    ) -> (Vec<(super::super::ButtonHit, Action)>, String) {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|f| hits = render_footer(f, Rect::new(0, 0, width, 1), state))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let line: String = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
        (hits, line)
    }

    /// Render a default footer (all features on) with the given file-viewer
    /// visibility.
    fn footer_render(file_viewer_open: bool) -> (Vec<(super::super::ButtonHit, Action)>, String) {
        render_footer_state(&footer_state(file_viewer_open))
    }

    fn hit_actions(hits: &[(super::super::ButtonHit, Action)]) -> Vec<Action> {
        hits.iter().map(|(_, a)| *a).collect()
    }

    #[test]
    fn footer_renders_all_buttons() {
        let (hits, line) = footer_render(false);
        for label in [
            "Help", "Info", "Files", "Theme", "Tasks", "Settings", "Quit",
        ] {
            assert!(line.contains(label), "missing {label} in footer: {line:?}");
        }
        // Each pill carries its live shortcut: an F-key alternate where one
        // exists (Help · F1, Info · F2), else the caret-ctrl chord (Quit · ^Q).
        for shortcut in ["F1", "F2", "^Q"] {
            assert!(
                line.contains(shortcut),
                "missing shortcut {shortcut} in footer: {line:?}"
            );
        }
        // Every button maps back to its Action, in render order (F-key order,
        // Quit last) — guards the index→action remapping in `render_footer`.
        assert_eq!(
            hit_actions(&hits),
            vec![
                Action::ToggleHelp,
                Action::ToggleInfoPanel,
                Action::ToggleFileViewer,
                Action::OpenThemePicker,
                Action::FocusTasks,
                Action::OpenSettings,
                Action::QuitApp,
            ]
        );
    }

    /// The panel/pane-toggle buttons are dropped when their feature is off.
    #[test]
    fn footer_hides_panel_buttons_when_features_off() {
        let mut state = footer_state(false);
        state.tasks_enabled = false;
        state.file_viewer_enabled = false;
        state.info_panel_enabled = false;
        let (hits, _) = render_footer_state(&state);
        assert_eq!(
            hit_actions(&hits),
            vec![
                Action::ToggleHelp,
                Action::OpenThemePicker,
                Action::OpenSettings,
                Action::QuitApp,
            ],
            "only the non-panel buttons remain"
        );
    }

    /// On a narrow footer the optional panel-toggle pills (Info/Files/Tasks)
    /// drop together, but the essential Help/Theme/Settings/Quit pills always
    /// survive.
    #[test]
    fn footer_drops_view_toggles_when_narrow() {
        let state = footer_state(false);
        let (hits, _) = footer_at(70, &state);
        let actions = hit_actions(&hits);
        assert!(
            !actions.contains(&Action::ToggleInfoPanel)
                && !actions.contains(&Action::ToggleFileViewer)
                && !actions.contains(&Action::FocusTasks),
            "panel-toggle pills drop together when the footer is too narrow: {actions:?}"
        );
        for essential in [
            Action::ToggleHelp,
            Action::OpenSettings,
            Action::OpenThemePicker,
            Action::QuitApp,
        ] {
            assert!(
                actions.contains(&essential),
                "essential pill {essential:?} must survive a narrow footer: {actions:?}"
            );
        }
    }

    /// The two panel buttons gate independently — a swapped flag in
    /// `footer_entries` would surface here.
    #[test]
    fn footer_panel_buttons_gate_independently() {
        let mut state = footer_state(false);
        state.tasks_enabled = false;
        state.file_viewer_enabled = true;
        let actions = hit_actions(&render_footer_state(&state).0);
        assert!(!actions.contains(&Action::FocusTasks), "Tasks gated off");
        assert!(
            actions.contains(&Action::ToggleFileViewer),
            "Files still shown"
        );
    }

    /// The buttons stay visible with the file viewer open; its hints share the
    /// row to the left of them.
    #[test]
    fn footer_keeps_buttons_with_file_viewer_open() {
        let (hits, line) = footer_render(true);
        assert_eq!(
            hits.len(),
            7,
            "buttons must remain when the file viewer is open"
        );
        assert!(line.contains("Quit"), "buttons still rendered: {line:?}");
        assert!(
            line.contains("Open") || line.contains("Move"),
            "file-viewer hints share the row: {line:?}"
        );
        // The rightmost button ends at the footer's right edge.
        let last = hits.iter().max_by_key(|(h, _)| h.rect.x).unwrap().0.rect;
        assert_eq!(last.x + last.width, 120);
    }

    fn long_error() -> StatusMessage {
        StatusMessage {
            text: "a long error that would previously be hidden under the footer pills".into(),
            level: StatusLevel::Error,
            created_at: std::time::Instant::now(),
        }
    }

    /// A long status message renders in full on its dedicated row (no pills to
    /// clip it) — the fix for messages hidden under the footer buttons.
    #[test]
    fn status_row_shows_full_message() {
        let msg = long_error();
        let mut state = footer_state(false);
        state.status = Some(&msg);

        let backend = TestBackend::new(120, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_status_message_row(f, Rect::new(0, 0, 120, 1), &state))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let line: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect();

        assert!(line.contains("ERROR"), "error badge present: {line:?}");
        assert!(
            line.contains("would previously be hidden"),
            "message body visible on its own row: {line:?}"
        );
        // The status row carries no footer pills — they live on the footer row.
        assert!(
            !line.contains("Quit"),
            "status row must not carry footer pills: {line:?}"
        );
    }

    /// Regression guard for the original bug: with a message active, the footer
    /// row still renders every pill intact (the message no longer flows across
    /// it — it lives on the row above).
    #[test]
    fn footer_pills_survive_with_status_message() {
        let msg = long_error();
        let mut state = footer_state(false);
        state.status = Some(&msg);
        let (hits, line) = render_footer_state(&state);

        assert!(
            line.contains("Quit"),
            "pills intact with a message: {line:?}"
        );
        // The footer row shows the idle counts, not the message.
        assert!(
            !line.contains("would previously be hidden"),
            "message must not render on the footer row: {line:?}"
        );
        let last = hits.iter().max_by_key(|(h, _)| h.rect.x).unwrap().0.rect;
        assert_eq!(last.x + last.width, 120);
    }

    /// The left-hand text at full length, before any responsive trim — what a
    /// wide-enough footer shows.
    fn left_text(state: &FooterState<'_>) -> String {
        left_segments(state)
            .iter()
            .flat_map(|s| &s.spans)
            .map(|s| s.content.as_ref())
            .collect()
    }

    /// Blocked sessions surface as a footer badge carrying the live
    /// `NextBlockedSession` shortcut hint; without any it stays hidden.
    #[test]
    fn footer_blocked_badge_shows_count_and_shortcut() {
        let idle_counts = |blocked: usize| -> String {
            let mut state = footer_state(false);
            state.blocked_count = blocked;
            left_text(&state)
        };

        let text = idle_counts(2);
        assert!(
            text.contains("◆ 2 blocked · F10"),
            "blocked badge with live shortcut hint: {text:?}"
        );

        let text = idle_counts(0);
        assert!(
            !text.contains("blocked"),
            "badge hidden with nothing blocked: {text:?}"
        );
    }

    /// The left informational hint cluster advertises both Focus (^H/^L) and
    /// Open (^O).
    #[test]
    fn shortcut_hints_advertise_focus_and_open() {
        let text = left_text(&footer_state(false));
        assert!(text.contains("^H"), "Focus-previous key present: {text:?}");
        assert!(text.contains("^L"), "Focus-next key present: {text:?}");
        assert!(text.contains("Focus"), "Focus label present: {text:?}");
        assert!(text.contains("^O"), "Open key present: {text:?}");
        assert!(text.contains("Open"), "Open label present: {text:?}");
    }

    /// The bug this whole ladder exists for: at *every* width, the left-hand
    /// text stops where the pills start. Any column of the pill block that
    /// isn't inside a pill must be blank — a stray glyph there is left text
    /// bleeding through the one-column gaps between the chips.
    #[test]
    fn footer_text_never_bleeds_into_the_pill_block() {
        for width in 1..=200u16 {
            for (blocked, automations, viewer) in
                [(0, 0, false), (2, 3, false), (1, 0, true), (0, 2, true)]
            {
                let mut state = footer_state(viewer);
                state.blocked_count = blocked;
                state.automation_count = automations;
                state.session_count = 7;
                let (hits, line) = footer_at(width, &state);
                let Some(first) = hits.iter().map(|(h, _)| h.rect.x).min() else {
                    continue;
                };
                let cells: Vec<char> = line.chars().collect();
                for x in first..width {
                    let inside = hits
                        .iter()
                        .any(|(h, _)| x >= h.rect.x && x < h.rect.x + h.rect.width);
                    if !inside {
                        assert_eq!(
                            cells[x as usize], ' ',
                            "text bled into the pill gap at column {x} \
                             (width {width}, blocked {blocked}, viewer {viewer}): {line:?}"
                        );
                    }
                }
            }
        }
    }

    /// The degenerate end of the range: a zero-width area draws nothing, and a
    /// footer only a few columns wide still lays out without panicking and
    /// without placing a chip past the right edge.
    #[test]
    fn footer_survives_degenerate_widths() {
        let state = footer_state(false);
        let backend = TestBackend::new(10, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut hits = Vec::new();
        terminal
            .draw(|f| hits = render_footer(f, Rect::new(0, 0, 0, 1), &state))
            .unwrap();
        assert!(hits.is_empty(), "no clickable chips in a zero-width footer");

        let mut armed = footer_state(false);
        armed.prefix_armed = Some("^A".to_string());
        for state in [&state, &armed] {
            for width in 1..=7u16 {
                let (hits, line) = footer_at(width, state);
                for (hit, _) in &hits {
                    assert!(
                        hit.rect.x + hit.rect.width <= width,
                        "chip overruns the {width}-column footer: {line:?}"
                    );
                }
            }
        }
    }

    /// Step 1 of the ladder: the ` · ` separators are the first thing sold for
    /// columns — the left-hand text is still whole at that point.
    #[test]
    fn footer_drops_pill_separators_before_any_text() {
        let state = footer_state(false);
        let (_, wide) = footer_at(160, &state);
        assert!(
            wide.contains("Help · F1"),
            "resting form when wide: {wide:?}"
        );

        let (_, line) = footer_at(120, &state);
        assert!(
            line.contains("Help F1") && !line.contains("Help · F1"),
            "separators dropped first: {line:?}"
        );
        assert!(
            line.contains("1 session(s)") && line.contains("Files"),
            "the left-hand text survives that step intact: {line:?}"
        );
    }

    /// Step 2: text goes segment by segment, least useful first — the generic
    /// key hints before the counts, the focus label last.
    #[test]
    fn footer_trims_left_text_by_priority() {
        let state = footer_state(false);
        let (_, line) = footer_at(105, &state);
        assert!(
            !line.contains("Focus") && line.contains("1 session(s)"),
            "the key hints go before the session count: {line:?}"
        );

        let (_, line) = footer_at(88, &state);
        assert!(
            !line.contains("session(s)") && line.contains("Files"),
            "the count goes before the focus label: {line:?}"
        );
    }

    /// A hint is decoration, never a stand-in: at a width where `^O Open` would
    /// fit but the session count no longer does, the columns stay empty rather
    /// than showing a hint floating where the state should be.
    #[test]
    fn footer_hints_never_replace_state() {
        let state = footer_state(false);
        let (_, line) = footer_at(92, &state);
        assert!(
            line.contains("Files") && !line.contains("session(s)"),
            "the count is what didn't fit here: {line:?}"
        );
        assert!(
            !line.contains("Open") && !line.contains("Focus"),
            "no hint fills in for it: {line:?}"
        );
    }

    /// Steps 4–5: with no room for labelled pills the chips shrink to their
    /// bare key, and the columns that frees go back to the left-hand text.
    #[test]
    fn footer_falls_back_to_key_only_pills() {
        let state = footer_state(false);
        let (hits, line) = footer_at(40, &state);
        assert!(
            !line.contains("Help") && !line.contains("Settings"),
            "pill labels gone in the compact state: {line:?}"
        );
        assert!(
            line.contains("F1") && line.contains("^Q"),
            "chips keep their key: {line:?}"
        );
        assert!(
            line.contains("Files"),
            "the freed columns go back to the text: {line:?}"
        );
        assert_eq!(
            hit_actions(&hits),
            vec![
                Action::ToggleHelp,
                Action::OpenThemePicker,
                Action::OpenSettings,
                Action::QuitApp
            ],
            "key-only chips still dispatch, in order: {line:?}"
        );

        // Squeezed further, whole chips go, cosmetics first…
        let (hits, line) = footer_at(12, &state);
        assert_eq!(
            hit_actions(&hits),
            vec![Action::ToggleHelp, Action::QuitApp],
            "Theme and Settings go before Help/Quit: {line:?}"
        );
        // …until only Help is left: the chip that documents every key the row
        // can no longer show.
        let (hits, line) = footer_at(5, &state);
        assert_eq!(
            hit_actions(&hits),
            vec![Action::ToggleHelp],
            "Help outlives the other chips: {line:?}"
        );
    }

    /// The armed-leader badge outranks the pills: it is the one segment the
    /// trim can't shed, because an armed leader with no feedback anywhere reads
    /// as a frozen app.
    #[test]
    fn footer_keeps_the_armed_leader_badge_when_narrow() {
        let mut state = footer_state(false);
        state.prefix_armed = Some("^A".to_string());
        for width in [120u16, 80, 60, 40, 30, 8, 4] {
            let (_, line) = footer_at(width, &state);
            assert!(
                line.contains("^A"),
                "armed leader badge survives width {width}: {line:?}"
            );
        }
    }

    /// The file viewer's hints trim from the tail: `j/k Move` outlives
    /// `n/N Next/Prev`, and both live in the same flow as the rest of the text.
    #[test]
    fn footer_trims_file_viewer_hints_from_the_tail() {
        let state = footer_state(true);
        let (_, wide) = footer_at(200, &state);
        assert!(
            wide.contains("Next/Prev") && wide.contains("Move"),
            "every hint when there is room: {wide:?}"
        );

        let (_, line) = footer_at(120, &state);
        assert!(
            line.contains("Move") && !line.contains("Next/Prev"),
            "the tail hints go first: {line:?}"
        );
    }
}
