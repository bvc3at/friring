//! Native renderer for the activity view (F9): a side navigator of sections
//! (overview / timeline / commands / files / web / agents — the agents tree
//! nested under its section) and a central pane showing the selected
//! section's normalized event stream, an agent transcript, or a workflow
//! overview. Pure rendering — it returns click/scroll hitboxes for the app
//! layer to record. Mirrors the code-review renderer's shape (`ui/code_review`)
//! without the diff machinery.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::activity::{fmt_time, Section};
use crate::app::cc_activity::{CcActivityState, CcNodeRef, CcRow, CcTreeRow};
use crate::session::activity::{ActionKind, ActivityEvent};
use crate::session::{CcAgent, CcAgentState, CcRunStatus, TranscriptBlock};
use crate::ui::scrollbar::{self, ScrollbarGeom};
use crate::ui::theme::Theme;
use crate::ui::{focus_block, title_style, FocusLevel, RowHitbox};

/// What the central-pane renderer hands back for the app layer to record.
pub(crate) struct CcActivityHits {
    /// One hitbox per visible transcript row (index = row in `state.rows`).
    pub rows: Vec<RowHitbox>,
    pub scrollbar: Option<ScrollbarGeom>,
}

fn normal() -> Style {
    Style::default()
}
fn dim() -> Style {
    Style::default().fg(Theme::text_secondary())
}
fn accent() -> Style {
    Style::default()
        .fg(Theme::accent())
        .add_modifier(Modifier::BOLD)
}
fn danger() -> Style {
    Style::default().fg(Theme::danger())
}

fn agent_label(a: &CcAgent) -> String {
    a.label.clone().unwrap_or_else(|| a.agent_type.clone())
}

fn state_style(s: CcAgentState) -> Style {
    match s {
        CcAgentState::Active => Style::default().fg(Theme::status_working()),
        CcAgentState::Done => Style::default().fg(Theme::status_done()),
        CcAgentState::Error => danger(),
    }
}

fn state_glyph(s: CcAgentState) -> char {
    match s {
        CcAgentState::Active => '◐',
        CcAgentState::Done => '●',
        CcAgentState::Error => '✗',
    }
}

// ── Central transcript pane ──────────────────────────────────────────────

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    state: &mut CcActivityState,
    level: FocusLevel,
) -> CcActivityHits {
    let title = format!(" Activity · {} ", open_label(state));
    let block = focus_block("", level)
        .title_top(Line::from(Span::styled(title, title_style(level))).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return CcActivityHits {
            rows: Vec::new(),
            scrollbar: None,
        };
    }

    // A one-row hint footer; the transcript uses the rest.
    let footer = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " j/k scroll · / find · w wrap · Enter fold · 1-6 section · h nav · Esc close",
            dim(),
        ))),
        footer,
    );
    let mut body = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(1),
    );
    if body.height == 0 {
        return CcActivityHits {
            rows: Vec::new(),
            scrollbar: None,
        };
    }
    // A search, when open, takes the top row of the body (like the file viewer /
    // code review); the transcript shrinks below it.
    if state.search.is_some() && body.height > 1 {
        let search_row = Rect::new(body.x, body.y, body.width, 1);
        render_search_bar(frame, search_row, state);
        body = Rect::new(body.x, body.y + 1, body.width, body.height - 1);
    }

    // Reserve the rightmost column for a scrollbar, so long transcripts show
    // their position.
    let (text_area, track) = scrollbar::reserve_track(body, state.rows.len(), body.height as usize);
    let width = text_area.width as usize;
    let show_cursor = level == FocusLevel::Focused;
    // The active query drives the per-row match highlight (trimmed + lowercased
    // exactly like the matcher, so a counted row never renders unhighlighted).
    let query = state.active_query();

    clamp_scroll(state, width, text_area.height as usize);

    // Window rows from `scroll`, drawing each row's visual lines until the area
    // fills. Record one hitbox spanning each logical row.
    let mut hitboxes = Vec::new();
    let mut y = text_area.y;
    let bottom = text_area.y + text_area.height;
    let mut i = state.scroll;
    while i < state.rows.len() && y < bottom {
        let lines = row_visual_lines(state, i, width, show_cursor, query.as_deref());
        let start_y = y;
        for line in &lines {
            if y >= bottom {
                break;
            }
            let row = Rect::new(text_area.x, y, text_area.width, 1);
            frame.render_widget(Paragraph::new(line.clone()), row);
            y += 1;
        }
        let drawn = y.saturating_sub(start_y);
        if drawn > 0 {
            hitboxes.push(RowHitbox {
                rect: Rect::new(text_area.x, start_y, text_area.width, drawn),
                index: i,
            });
        }
        i += 1;
    }

    let geom = track.and_then(|t| {
        scrollbar::render_into(
            frame,
            t,
            state.rows.len(),
            text_area.height as usize,
            state.selected,
        )
    });
    CcActivityHits {
        rows: hitboxes,
        scrollbar: geom,
    }
}

/// The find bar atop the transcript while a search is open: the query, the
/// match position (derived from the selection), and mode-appropriate key hints.
fn render_search_bar(frame: &mut Frame, area: Rect, state: &CcActivityState) {
    let Some(s) = &state.search else {
        return;
    };
    let total = s.matches.len();
    // "current" is derived from the selection (like code review / the file
    // viewer's `current_match_index`): the 1-based rank of the selected row
    // among the matches, or 0 when the cursor isn't on a match. Blank on an
    // empty query, "no matches" only once something's been typed.
    let current = s
        .matches
        .iter()
        .position(|&i| i == state.selected)
        .map(|p| p + 1)
        .unwrap_or(0);
    let pos = if s.query.trim().is_empty() {
        String::new()
    } else if total == 0 {
        "no matches".to_string()
    } else {
        format!("{current}/{total}")
    };
    let hint = if s.editing {
        "Enter/↓ next · ↑ prev · Tab done · Esc cancel"
    } else {
        "n/N next/prev · Esc clear"
    };
    let mut spans = vec![Span::styled(format!("/{}", s.query), accent())];
    if !pos.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(pos, dim()));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(hint, dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn open_label(state: &CcActivityState) -> String {
    match &state.open {
        Some(CcNodeRef::Section(s)) => s.label().to_string(),
        Some(CcNodeRef::WorkflowOverview(run)) => format!("workflow {run}"),
        Some(CcNodeRef::WorkflowAgent(run, aid)) => state
            .activity
            .workflow_agent(run, aid)
            .map(agent_label)
            .unwrap_or_else(|| aid.clone()),
        Some(CcNodeRef::Subagent(aid)) => state
            .activity
            .subagent(aid)
            .map(agent_label)
            .unwrap_or_else(|| aid.clone()),
        None => "—".to_string(),
    }
}

/// Raise `scroll` minimally so the selected row is fully visible (its lower
/// bound; the upper bound `scroll <= selected` is kept by `ensure_visible`).
fn clamp_scroll(state: &mut CcActivityState, width: usize, height: usize) {
    if height == 0 {
        return;
    }
    while state.scroll < state.selected {
        let used: usize = (state.scroll..=state.selected)
            .map(|i| row_height(state, i, width))
            .sum();
        if used <= height {
            break;
        }
        state.scroll += 1;
    }
}

fn row_height(state: &CcActivityState, i: usize, width: usize) -> usize {
    // Highlighting never changes the line count, so height math ignores the query.
    row_visual_lines(state, i, width, false, None).len()
}

/// The visual lines for one logical transcript row (wrap / h-scroll applied to
/// bodies). `query` (lowercased, non-empty) highlights matched substrings; the
/// selected row's lines are reverse-highlighted when `cursor`.
fn row_visual_lines(
    state: &CcActivityState,
    i: usize,
    width: usize,
    cursor: bool,
    query: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines = match &state.rows[i] {
        CcRow::Info(s) => vec![styled(s, dim(), width)],
        CcRow::Text(s) => body_lines(s, normal(), state.wrap, state.h_scroll, width, query),
        CcRow::Block(bi) => block_lines(state, *bi, width, query),
    };
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    if cursor && state.selected == i {
        for line in &mut lines {
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
    }
    lines
}

fn block_lines(
    state: &CcActivityState,
    bi: usize,
    width: usize,
    query: Option<&str>,
) -> Vec<Line<'static>> {
    let Some(block) = state.blocks.get(bi) else {
        return vec![Line::from("")];
    };
    let collapsed = state.collapsed_tools.contains(&bi);
    let (wrap, h) = (state.wrap, state.h_scroll);
    match block {
        TranscriptBlock::Prompt(s) => {
            let mut out = vec![Line::from(Span::styled("▶ task prompt", accent()))];
            out.extend(body_lines(s, dim(), wrap, h, width, query));
            out
        }
        TranscriptBlock::Thinking(s) => {
            let mut out = vec![Line::from(Span::styled(
                "✱ thinking",
                dim().add_modifier(Modifier::ITALIC),
            ))];
            out.extend(body_lines(s, dim(), wrap, h, width, query));
            out
        }
        TranscriptBlock::Text(s) => {
            let mut out = body_lines(s, normal(), wrap, h, width, query);
            // Mark the assistant text with a leading bullet on its first line.
            if let Some(first) = out.first_mut() {
                first.spans.insert(0, Span::styled("⏺ ", accent()));
            }
            out
        }
        TranscriptBlock::ToolUse { name, input } => {
            let summary = first_line(input);
            let header = format!("⏺ {name}({})", truncate(&summary, 60));
            if collapsed {
                vec![Line::from(Span::styled(format!("{header} ▸"), accent()))]
            } else {
                let mut out = vec![Line::from(Span::styled(header, accent()))];
                out.extend(body_lines(input, dim(), wrap, h, width, query));
                out
            }
        }
        TranscriptBlock::ToolResult { content, is_error } => {
            let style = if *is_error { danger() } else { dim() };
            if collapsed {
                let n = content.lines().count().max(1);
                vec![Line::from(Span::styled(format!("  ⎿ {n} lines ▸"), dim()))]
            } else {
                let mut out = vec![Line::from(Span::styled("  ⎿ result", dim()))];
                out.extend(body_lines(content, style, wrap, h, width, query));
                out
            }
        }
        // Events are compact by default (one line each — a retrospective list,
        // not a transcript); Enter *expands* to note/result. The fold set is
        // therefore read inverted for this variant.
        TranscriptBlock::Event(e) => {
            let expanded = collapsed;
            event_lines(e, expanded, wrap, h, width, query)
        }
    }
}

/// The one-line header (+ optional expanded body) of a normalized activity
/// event: `HH:MM:SS  tag  detail`, error-marked when the action failed.
fn event_lines(
    e: &ActivityEvent,
    expanded: bool,
    wrap: bool,
    h: usize,
    width: usize,
    query: Option<&str>,
) -> Vec<Line<'static>> {
    let mut header: Vec<Span<'static>> = Vec::new();
    if let Some(ts) = e.ts_ms {
        header.push(Span::styled(format!("{} ", fmt_time(ts)), dim()));
    }
    header.push(Span::styled(
        format!("{:<5} ", event_tag(e.kind)),
        event_style(e.kind),
    ));
    header.push(Span::styled(
        truncate(&first_line(&e.detail), width.saturating_sub(16).max(20)),
        normal(),
    ));
    if e.ok == Some(false) {
        header.push(Span::styled(" ✗", danger()));
    }
    let has_body = e.note.is_some() || e.result_head.is_some() || e.origin.is_some();
    if has_body && !expanded {
        header.push(Span::styled(" ▸", dim()));
    }
    let mut out = vec![Line::from(header)];
    if expanded {
        if let Some(o) = &e.origin {
            out.push(Line::from(Span::styled(format!("  · in {o}"), dim())));
        }
        if let Some(n) = &e.note {
            out.extend(body_lines(n, dim(), wrap, h, width, query));
        }
        if let Some(r) = &e.result_head {
            let style = if e.ok == Some(false) { danger() } else { dim() };
            out.push(Line::from(Span::styled("  ⎿ result", dim())));
            out.extend(body_lines(r, style, wrap, h, width, query));
        }
    }
    out
}

/// Short fixed-width kind tag prefixing an event's header line.
fn event_tag(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Command => "$",
        ActionKind::Edit => "edit",
        ActionKind::Read => "read",
        ActionKind::Search => "grep",
        ActionKind::WebSearch => "web",
        ActionKind::WebFetch => "fetch",
        ActionKind::Subagent => "agent",
        ActionKind::Mcp => "mcp",
        ActionKind::Other => "tool",
    }
}

fn event_style(kind: ActionKind) -> Style {
    match kind {
        ActionKind::Command => accent(),
        ActionKind::Edit => Style::default().fg(Theme::status_working()),
        ActionKind::WebSearch | ActionKind::WebFetch => Style::default().fg(Theme::status_done()),
        _ => dim().add_modifier(Modifier::BOLD),
    }
}

/// Split `text` into styled visual lines, wrapping or horizontally scrolling
/// each logical line to `width`. When `query` (lowercased, non-empty) is
/// present, its matched substrings in each visual line are highlighted.
fn body_lines(
    text: &str,
    style: Style,
    wrap: bool,
    h_scroll: usize,
    width: usize,
    query: Option<&str>,
) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::from("")];
    }
    let mut out = Vec::new();
    for logical in text.split('\n') {
        if wrap {
            let chars: Vec<char> = logical.chars().collect();
            if chars.is_empty() {
                out.push(Line::from(""));
            } else {
                for chunk in chars.chunks(width) {
                    out.push(highlighted_line(chunk.iter().collect(), style, query));
                }
            }
        } else {
            let s: String = logical.chars().skip(h_scroll).take(width).collect();
            out.push(highlighted_line(s, style, query));
        }
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

/// A single styled line, with `query`'s matches highlighted when present.
fn highlighted_line(s: String, style: Style, query: Option<&str>) -> Line<'static> {
    match query {
        Some(q) if !q.is_empty() => {
            let positions = match_byte_positions(&s, q);
            if positions.is_empty() {
                Line::from(Span::styled(s, style))
            } else {
                Line::from(crate::ui::highlight::highlighted_spans_owned(
                    &s, &positions, style,
                ))
            }
        }
        _ => Line::from(Span::styled(s, style)),
    }
}

/// Byte offsets in `s` covered by any occurrence of the lowercased
/// `query_lower` (case-insensitive). Skips highlighting when lowercasing
/// changes byte length (rare non-ASCII) so offsets can't desync — the row is
/// still navigable.
fn match_byte_positions(s: &str, query_lower: &str) -> Vec<usize> {
    if query_lower.is_empty() {
        return Vec::new();
    }
    let hay = s.to_lowercase();
    if hay.len() != s.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(pos) = hay[start..].find(query_lower) {
        let at = start + pos;
        out.extend(at..at + query_lower.len());
        start = at + query_lower.len();
    }
    out
}

fn styled(s: &str, style: Style, width: usize) -> Line<'static> {
    let t: String = s.chars().take(width.max(1)).collect();
    Line::from(Span::styled(t, style))
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

// ── Side tree column ─────────────────────────────────────────────────────

/// Render the workflow/subagent tree into the file-viewer column. Returns one
/// [`RowHitbox`] per visible row (index = row in `state.tree`) so a click jumps
/// the selection there.
pub(crate) fn render_tree(
    frame: &mut Frame,
    area: Rect,
    state: &CcActivityState,
    level: FocusLevel,
) -> Vec<RowHitbox> {
    let block = focus_block(" Activity ", level);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return Vec::new();
    }

    let (list_area, track) =
        scrollbar::reserve_track(inner, state.tree.len(), inner.height as usize);
    let height = list_area.height as usize;
    // Window the list around the selection.
    let (start, _end) =
        crate::ui::file_viewer::visible_window(state.tree.len(), state.tree_selected, height);

    let mut hitboxes = Vec::new();
    for ((offset, row), y) in state
        .tree
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .zip(list_area.y..)
    {
        let selected = offset == state.tree_selected;
        let line = tree_row_line(state, row, selected);
        let rect = Rect::new(list_area.x, y, list_area.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        hitboxes.push(RowHitbox {
            rect,
            index: offset,
        });
    }
    if let Some(t) = track {
        scrollbar::render_into(frame, t, state.tree.len(), height, state.tree_selected);
    }
    hitboxes
}

/// A section's navigator label with its live count, e.g. `Commands (42)`.
fn section_label(state: &CcActivityState, section: Section) -> String {
    let c = &state.counts;
    let count = match section {
        Section::Overview => None,
        Section::Timeline => Some(c.total()),
        Section::Commands => Some(c.commands),
        Section::Files => Some(state.files_count),
        Section::Web => Some(c.web),
        Section::Agents => Some(state.activity.agent_count()),
    };
    match count {
        Some(n) if n > 0 => format!("{} ({n})", section.label()),
        _ => section.label().to_string(),
    }
}

fn tree_row_line(state: &CcActivityState, row: &CcTreeRow, selected: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = match row {
        CcTreeRow::Section(section) => {
            let digit = crate::app::activity::SECTIONS
                .iter()
                .position(|s| s == section)
                .map(|i| i + 1)
                .unwrap_or_default();
            let mut label = format!("{digit} {}", section_label(state, *section));
            if *section == Section::Agents {
                label = format!("{} {}", if state.agents_folded { "▸" } else { "▾" }, label);
            }
            vec![Span::styled(label, accent())]
        }
        CcTreeRow::Workflow(wi) => {
            let w = state.activity.workflows.get(*wi);
            let folded = w
                .map(|w| state.collapsed_workflows.contains(&w.run_id))
                .unwrap_or(false);
            let chevron = if folded { "▸" } else { "▾" };
            let name = w
                .map(|w| w.name.clone().unwrap_or_else(|| w.run_id.clone()))
                .unwrap_or_default();
            let count = w.map(|w| w.agents.len()).unwrap_or(0);
            let running = w
                .map(|w| matches!(w.status, CcRunStatus::Running))
                .unwrap_or(false);
            let tag = if running { "running" } else { "done" };
            // Nested one level under the Agents section row.
            vec![Span::styled(
                format!("  {chevron} {name}  ({count} agents, {tag})"),
                accent(),
            )]
        }
        CcTreeRow::WorkflowAgent(wi, ai) => {
            match state
                .activity
                .workflows
                .get(*wi)
                .and_then(|w| w.agents.get(*ai))
            {
                Some(a) => vec![
                    Span::styled(
                        format!("    {} ", state_glyph(a.state)),
                        state_style(a.state),
                    ),
                    Span::styled(agent_label(a), normal()),
                ],
                None => vec![Span::styled("    ?".to_string(), dim())],
            }
        }
        CcTreeRow::Subagent(si) => match state.activity.subagents.get(*si) {
            Some(a) => {
                let mut label = a.agent_type.clone();
                if let Some(d) = &a.description {
                    label.push_str(&format!(": {}", truncate(d, 40)));
                }
                vec![
                    Span::styled(format!("  {} ", state_glyph(a.state)), state_style(a.state)),
                    Span::styled(label, normal()),
                ]
            }
            None => vec![Span::styled("  ?".to_string(), dim())],
        },
        CcTreeRow::Info(s) => vec![Span::styled(format!("  {s}"), dim())],
    };
    if selected {
        for span in &mut spans {
            span.style = span.style.add_modifier(Modifier::REVERSED);
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::match_byte_positions;

    #[test]
    fn match_byte_positions_finds_all_case_insensitive() {
        // Query is pre-lowercased by the caller; matching is case-insensitive.
        assert_eq!(match_byte_positions("Foo foo FOO", "foo"), {
            let mut v = Vec::new();
            v.extend(0..3); // "Foo"
            v.extend(4..7); // "foo"
            v.extend(8..11); // "FOO"
            v
        });
        assert!(match_byte_positions("nothing", "xyz").is_empty());
        assert!(match_byte_positions("anything", "").is_empty());
    }

    #[test]
    fn match_byte_positions_skips_when_lowercasing_changes_len() {
        // A char whose lowercase differs in byte length (İ → i̇) would desync
        // offsets — highlighting is skipped rather than risk a panic/misalign.
        assert!(match_byte_positions("İstanbul", "i").is_empty());
    }
}
