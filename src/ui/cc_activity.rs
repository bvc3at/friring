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
use crate::app::cc_activity::{
    CcActivityState, CcNodeRef, CcRow, CcTreeRow, SparkRow, StatTile, Tone,
};
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

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Accent => accent(),
        Tone::Working => Style::default().fg(Theme::status_working()),
        Tone::Done => Style::default().fg(Theme::status_done()),
        Tone::Danger => danger(),
        Tone::Normal => normal(),
    }
}

/// Overview stat tiles: `⟨glyph⟩ ⟨value⟩ ⟨label⟩`, triple-spaced, wrapped by
/// whole tiles so nothing ever clips at the pane edge. A lone tile wider than
/// the pane is emitted with its label dropped (glyph + value alone), so even a
/// very narrow pane stays within `width`.
fn tiles_lines(tiles: &[StatTile], width: usize) -> Vec<Line<'static>> {
    let tile_width = |t: &StatTile| 2 + t.value.chars().count() + 1 + t.label.len();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    let mut used = 1usize;
    for t in tiles {
        let w = tile_width(t);
        let sep = usize::from(spans.len() > 1) * 3;
        if used + sep + w > width && spans.len() > 1 {
            out.push(Line::from(std::mem::take(&mut spans)));
            spans.push(Span::raw(" "));
            used = 1;
        } else if sep > 0 {
            spans.push(Span::raw("   "));
            used += 3;
        }
        spans.push(Span::styled(
            format!("{} ", t.glyph),
            tone_style(t.tone).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            t.value.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        // Drop the label if this lone tile would otherwise overrun the pane.
        if used + w <= width {
            spans.push(Span::styled(format!(" {}", t.label), dim()));
            used += w;
        } else {
            used += 2 + t.value.chars().count();
        }
    }
    if spans.len() > 1 {
        out.push(Line::from(spans));
    }
    out
}

/// The Overview sparkline: `HH:MM ▁▂▅█… HH:MM  caption`. The bucket run is
/// max-pooled down to the width left beside the time labels, and the caption
/// drops to its own line when it doesn't fit — the row always stays inside
/// the pane.
fn spark_lines(spark: &SparkRow, width: usize) -> Vec<Line<'static>> {
    const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let labels = 1 + spark.start.chars().count() + 1 + 1 + spark.end.chars().count();
    // Size the glyph run strictly from the space left beside the labels (no
    // minimum) so a narrow pane downsamples instead of overrunning. When the
    // labels alone don't fit, drop the whole spark to a wrapped caption line.
    let avail = width.saturating_sub(labels).min(spark.buckets.len());
    if avail < 2 {
        return caption_only(&spark.caption, width);
    }
    let buckets = downsample_max(&spark.buckets, avail);
    let max = buckets.iter().copied().max().unwrap_or(0).max(1);
    let glyph_run: String = buckets
        .iter()
        .map(|&v| {
            if v == 0 {
                ' '
            } else {
                // Scale 1..=max onto the 8 glyph levels, non-zero floor.
                GLYPHS[((v * 8).div_ceil(max) as usize).clamp(1, 8) - 1]
            }
        })
        .collect();
    let mut line = vec![
        Span::styled(format!(" {} ", spark.start), dim()),
        Span::styled(glyph_run, accent()),
        Span::styled(format!(" {}", spark.end), dim()),
    ];
    let caption = format!("  {}", spark.caption);
    if labels + avail + caption.chars().count() <= width {
        line.push(Span::styled(caption, dim()));
        vec![Line::from(line)]
    } else {
        let mut out = vec![Line::from(line)];
        out.extend(caption_only(&spark.caption, width));
        out
    }
}

/// The sparkline caption on its own line(s), wrapped to `width`.
fn caption_only(caption: &str, width: usize) -> Vec<Line<'static>> {
    body_lines(&format!(" {caption}"), dim(), true, 0, width, None)
}

/// Max-pool `buckets` into `cells` slots (peaks survive a squeeze).
fn downsample_max(buckets: &[u64], cells: usize) -> Vec<u64> {
    if cells == 0 || buckets.len() <= cells {
        return buckets.to_vec();
    }
    (0..cells)
        .map(|i| {
            let lo = i * buckets.len() / cells;
            let hi = ((i + 1) * buckets.len() / cells).max(lo + 1);
            buckets[lo..hi.min(buckets.len())]
                .iter()
                .copied()
                .max()
                .unwrap_or(0)
        })
        .collect()
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
        CcRow::Header(s) => vec![styled(s, dim().add_modifier(Modifier::BOLD), width)],
        CcRow::Text(s) => body_lines(s, normal(), state.wrap, state.h_scroll, width, query),
        CcRow::Tiles(tiles) => tiles_lines(tiles, width),
        CcRow::Spark(spark) => spark_lines(spark, width),
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
            let timeline = matches!(&state.open, Some(CcNodeRef::Section(Section::Timeline)));
            event_lines(e, expanded, timeline, wrap, h, width, query)
        }
    }
}

/// The one-line header (+ optional expanded body) of a normalized activity
/// event: `HH:MM:SS  tag  detail`, error-marked when the action failed. On
/// the Timeline, a [`ActionKind::Prompt`] event renders as a turn header and
/// every other row sits in a turn gutter (`│`, deepened to `└` for
/// subagent-origin work); minor (bookkeeping) rows render dim.
fn event_lines(
    e: &ActivityEvent,
    expanded: bool,
    timeline: bool,
    wrap: bool,
    h: usize,
    width: usize,
    query: Option<&str>,
) -> Vec<Line<'static>> {
    if e.kind == ActionKind::Prompt {
        return vec![turn_header_line(e, width)];
    }
    let base = if e.minor { dim() } else { normal() };
    let mut used = 0usize;
    let mut header: Vec<Span<'static>> = Vec::new();
    if timeline {
        let gutter = if e.origin.is_some() {
            "│   └ "
        } else {
            "│ "
        };
        used += gutter.chars().count();
        header.push(Span::styled(gutter, dim()));
    }
    if let Some(ts) = e.ts_ms {
        used += 9;
        header.push(Span::styled(format!("{} ", fmt_time(ts)), dim()));
    }
    used += 6;
    header.push(Span::styled(
        format!("{:<5} ", e.kind.tag()),
        if e.minor { dim() } else { event_style(e.kind) },
    ));
    // Suffixes in DROP order (lowest priority first): a tight row sheds the
    // duration then the origin before the failure/fold markers, so ✗ / ▸ — the
    // load-bearing signals — stay on the row and it never runs past the edge.
    let has_body = e.note.is_some() || e.result_head.is_some() || e.origin.is_some();
    let mut suffixes: Vec<Span<'static>> = Vec::new();
    if let Some(d) = e.dur_ms.filter(|&d| d >= 1000) {
        suffixes.push(Span::styled(format!(" · {}", fmt_dur(d)), dim()));
    }
    if let Some(o) = &e.origin {
        suffixes.push(Span::styled(format!(" · {}", truncate(o, 24)), dim()));
    }
    if e.ok == Some(false) {
        suffixes.push(Span::styled(" ✗", danger()));
    }
    if has_body && !expanded {
        suffixes.push(Span::styled(" ▸", dim()));
    }
    let span_w = |ss: &[Span]| ss.iter().map(|s| s.content.chars().count()).sum::<usize>();
    // Drop from the low-priority front until the fixed prefix plus the kept
    // suffixes fit; the detail then yields the remaining columns (or is omitted).
    let mut start = 0;
    while start < suffixes.len() && used + span_w(&suffixes[start..]) > width {
        start += 1;
    }
    let kept = suffixes.split_off(start);
    let detail_budget = width.saturating_sub(used + span_w(&kept));
    if detail_budget > 0 {
        header.push(Span::styled(
            truncate(&first_line(&e.detail), detail_budget),
            base,
        ));
    }
    header.extend(kept);
    let mut out = vec![Line::from(header)];
    if expanded {
        if let Some(o) = &e.origin {
            // Wrapped like any body text so a long workflow label can't clip.
            out.extend(body_lines(
                &format!("· in {o}"),
                dim(),
                wrap,
                h,
                width,
                query,
            ));
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

/// A Timeline turn header: `▶ HH:MM:SS "prompt…" ───`, dash-filled to the
/// pane edge so turns read as visual breaks in the stream.
fn turn_header_line(e: &ActivityEvent, width: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::styled("▶ ", accent())];
    let mut used = 2usize;
    if let Some(ts) = e.ts_ms {
        used += 9;
        spans.push(Span::styled(format!("{} ", fmt_time(ts)), dim()));
    }
    // No minimum floor — the prompt yields to the pane width so the header
    // never runs past the edge; the dash fill only draws in leftover space.
    let text = truncate(&first_line(&e.detail), width.saturating_sub(used));
    used += text.chars().count();
    if !text.is_empty() {
        spans.push(Span::styled(text, accent().add_modifier(Modifier::BOLD)));
    }
    if width > used + 2 {
        spans.push(Span::styled(
            format!(" {}", "─".repeat(width - used - 2)),
            dim(),
        ));
    }
    Line::from(spans)
}

/// Compact call→result duration: `12s`, `1m03s`.
fn fmt_dur(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

fn event_style(kind: ActionKind) -> Style {
    match kind {
        ActionKind::Prompt | ActionKind::Command => accent(),
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
    use super::{
        downsample_max, event_lines, match_byte_positions, spark_lines, tiles_lines,
        turn_header_line,
    };
    use crate::app::cc_activity::{SparkRow, StatTile, Tone};
    use crate::session::activity::{ActionKind, ActivityEvent};
    use ratatui::text::Line;

    fn tile(value: &str, label: &'static str) -> StatTile {
        StatTile {
            glyph: "$",
            value: value.into(),
            label,
            tone: Tone::Accent,
        }
    }

    /// Terminal columns a rendered line occupies (char count, matching the
    /// budgeting the renderer itself uses).
    fn line_width(line: &Line) -> usize {
        line.spans.iter().map(|s| s.content.chars().count()).sum()
    }

    #[test]
    fn spark_lines_never_exceed_width() {
        let spark = SparkRow {
            start: "14:00".into(),
            end: "14:42".into(),
            buckets: (0..32).map(|i| (i % 7) as u64).collect(),
            caption: "47 events · 42m".into(),
        };
        for width in 1..=60usize {
            for line in spark_lines(&spark, width) {
                assert!(
                    line_width(&line) <= width,
                    "spark line {:?} exceeds width {width}",
                    line
                );
            }
        }
    }

    #[test]
    fn event_and_turn_rows_never_exceed_width_and_keep_markers() {
        // A maximal-suffix timeline row: gutter, timestamp, duration, a long
        // origin, a failure marker, and a fold marker.
        let ev = ActivityEvent {
            ts_ms: Some(1_783_512_000_000),
            kind: ActionKind::Command,
            detail: "cargo nextest run --all --workspace --no-fail-fast".into(),
            note: Some("n".into()),
            result_head: Some("boom".into()),
            ok: Some(false),
            origin: Some("a-rather-long-workflow-agent-label".into()),
            minor: false,
            dur_ms: Some(63_000),
        };
        // Fit is guaranteed once the fixed prefix (gutter+timestamp+tag ≈ 21)
        // fits; below that even the prefix can't, which is outside any real pane.
        for width in 21..=80usize {
            for line in event_lines(&ev, false, true, true, 0, width, None) {
                assert!(
                    line_width(&line) <= width,
                    "event line exceeds width {width}: {line:?}"
                );
            }
        }
        // At any realistic pane width the failure + fold markers survive the
        // budgeting (that is the whole point of the priority drop).
        for width in 40..=80usize {
            let lines = event_lines(&ev, false, true, true, 0, width, None);
            let flat: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                flat.contains('✗'),
                "failure marker dropped at width {width}"
            );
            assert!(flat.contains('▸'), "fold marker dropped at width {width}");
        }

        let prompt = ActivityEvent {
            ts_ms: Some(1_783_512_000_000),
            kind: ActionKind::Prompt,
            detail: "Refactor the activity view and make every widget fit the pane".into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor: false,
            dur_ms: None,
        };
        for width in 12..=80usize {
            let line = turn_header_line(&prompt, width);
            assert!(
                line_width(&line) <= width,
                "turn header exceeds width {width}: {line:?}"
            );
        }
    }

    #[test]
    fn tiles_wrap_by_whole_tiles_never_clipping() {
        let tiles = vec![tile("23", "cmds"), tile("11", "edits"), tile("47", "reads")];
        // Wide pane: one line. Each tile is 2+2+1+len(label) wide, +3 gaps.
        assert_eq!(tiles_lines(&tiles, 80).len(), 1);
        // Narrow pane: tiles flow onto following lines, whole.
        let narrow = tiles_lines(&tiles, 14);
        assert!(narrow.len() > 1, "tiles must wrap, not clip");
        for line in &narrow {
            let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 14, "no wrapped tile line may exceed the width: {w}");
        }
    }

    #[test]
    fn downsample_max_keeps_peaks() {
        assert_eq!(downsample_max(&[1, 9, 1, 1, 5, 1], 3), vec![9, 1, 5]);
        // Fewer buckets than cells → untouched.
        assert_eq!(downsample_max(&[3, 4], 8), vec![3, 4]);
    }

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
