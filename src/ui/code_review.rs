//! Native renderer for the code-review view (the diff + interleaved comments +
//! summary + an in-view compose box), modeled on the file viewer. Pure
//! rendering: it returns click/scroll hitboxes for the app layer to record.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::app::code_review::{CodeReviewState, ComposeState, ReviewButton, ReviewRow};
use crate::session::review::{
    pair_hunk, Classification, CommentAnchor, DiffFile, DiffHunk, DiffLine, DiffLineKind, SidePair,
};
use crate::ui::scrollbar::{self, ScrollbarGeom};
use crate::ui::theme::Theme;
use crate::ui::{focus_block, render_button_bar, ButtonSpec, FocusLevel, RowHitbox};

/// What the renderer hands back for the app layer to record as click/scroll
/// targets this frame.
pub(crate) struct CodeReviewHits {
    /// One hitbox per visible diff/comment row (index = row in `state.rows`).
    pub rows: Vec<RowHitbox>,
    /// One hitbox per visible target-picker entry (index = entry in the
    /// picker); empty unless the target picker is open.
    pub targets: Vec<RowHitbox>,
    /// Footer buttons paired with the action each triggers.
    pub buttons: Vec<(crate::ui::ButtonHit, ReviewButton)>,
    pub scrollbar: Option<ScrollbarGeom>,
}

/// Theme color for a classification badge. Each class gets a distinct palette
/// color so the badge reads at a glance (`Question` borrows the "working"
/// yellow — attention-seeking without the alarm of `Issue`'s danger red).
fn class_color(c: Classification) -> Color {
    match c {
        Classification::Issue => Theme::danger(),
        Classification::Suggestion => Theme::accent(),
        Classification::Question => Theme::status_working(),
        Classification::Note => Theme::text_secondary(),
        Classification::Praise => Theme::status_done(),
    }
}

/// Theme color for a file's status glyph (M/A/D/R): added green, deleted red,
/// modified yellow, renamed accent — so the status reads at a glance.
fn status_color(s: crate::session::review::FileStatus) -> Color {
    use crate::session::review::FileStatus::*;
    match s {
        Added => Theme::diff_added(),
        Deleted => Theme::diff_removed(),
        Modified => Theme::status_working(),
        Renamed => Theme::accent(),
    }
}

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    state: &mut CodeReviewState,
    level: FocusLevel,
) -> CodeReviewHits {
    let (add, del) = state.totals();
    let target = state.target.label(&state.repos, &state.commits);
    // Surface a non-default context width (`=` cycle) so a widened diff can't
    // be mistaken for the default view.
    let ctx = if state.context == crate::app::code_review::DEFAULT_CONTEXT {
        String::new()
    } else {
        format!(" · U{}", state.context)
    };
    let title = format!(" Code review · {target}{ctx}  +{add} -{del} ");
    // Right-aligned so the app-layer central-pane tab strip (Agent/Shell/Review)
    // overlaid on the left of this top border has room.
    let block = focus_block("", level)
        .title_top(Line::from(Span::styled(title, crate::ui::title_style(level))).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return CodeReviewHits {
            rows: Vec::new(),
            targets: Vec::new(),
            buttons: Vec::new(),
            scrollbar: None,
        };
    }

    // Footer (buttons) is always the last row; the diff uses the rest. A search
    // bar, when open, takes the top row of what's left.
    let footer = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    let mut diff_area = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
    if state.search.is_some() && diff_area.height > 1 {
        let search_row = Rect::new(diff_area.x, diff_area.y, diff_area.width, 1);
        diff_area = Rect::new(
            diff_area.x,
            diff_area.y + 1,
            diff_area.width,
            diff_area.height - 1,
        );
        render_search_bar(frame, search_row, state);
    }

    let composing = state.compose.is_some();
    // The target picker, when open, replaces the diff body. Its entries are
    // clickable (`targets`), on top of the keyboard-driven ↑/↓/Enter path.
    let mut targets = Vec::new();
    let hits = if state.loading {
        // A background build is in flight (ADR-P8): the pane opened instantly
        // and fills in when the worker delivers. No rows = nothing clickable.
        let msg = Line::from(Span::styled(
            "Building diff…",
            Style::default().fg(Theme::text_muted()),
        ))
        .centered();
        let y = diff_area.y + diff_area.height / 2;
        frame.render_widget(
            Paragraph::new(msg),
            Rect::new(diff_area.x, y, diff_area.width, 1),
        );
        (Vec::new(), None)
    } else if state.target_picker.is_some() {
        targets = render_target_picker(frame, diff_area, state);
        (Vec::new(), None)
    } else if state.comment_picker.is_some() {
        render_comment_picker(frame, diff_area, state);
        (Vec::new(), None)
    } else {
        render_rows(frame, diff_area, state)
    };
    // The compose box floats inline at the selected line (its screen row from
    // the hitboxes), not pinned to the bottom — GitHub-style line comments.
    if composing {
        if let Some(comp) = state.compose.as_ref() {
            let anchor_y = hits
                .0
                .iter()
                .find(|h| h.index == state.selected)
                .map(|h| h.rect.y);
            render_compose_inline(frame, diff_area, anchor_y, comp);
        }
    }
    let buttons = render_footer(frame, footer, composing, state.side_by_side, state.wrap);

    CodeReviewHits {
        rows: hits.0,
        targets,
        buttons,
        scrollbar: hits.1,
    }
}

/// Render the review-target picker (branch / working / per-commit) into `area`.
///
/// Returns one [`RowHitbox`] per rendered entry (`index` = entry position in
/// `picker.entries`) so a click selects that target, matching the keyboard
/// ↑/↓/Enter path.
fn render_target_picker(frame: &mut Frame, area: Rect, state: &CodeReviewState) -> Vec<RowHitbox> {
    let Some(picker) = state.target_picker.as_ref() else {
        return Vec::new();
    };
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        " Review target  (↑/↓ select · Enter · Esc · or click)",
        Style::default().fg(Theme::text_muted()),
    ))];
    let mut hits = Vec::new();
    let height = area.height as usize;
    for (i, target) in picker
        .entries
        .iter()
        .enumerate()
        .take(height.saturating_sub(1))
    {
        let selected = i == picker.selected;
        let marker = if selected { "▸ " } else { "  " };
        let label = target.label(&state.repos, &state.commits);
        let style = if selected {
            Style::default()
                .fg(Theme::selection_fg())
                .bg(Theme::selection_bg())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Theme::text_primary())
        };
        // Row 0 is the header line; entry `i` renders on the next row down.
        hits.push(RowHitbox {
            rect: Rect::new(area.x, area.y + 1 + i as u16, area.width, 1),
            index: i,
        });
        lines.push(Line::from(Span::styled(
            truncate(&format!("{marker}{label}"), area.width as usize),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
    hits
}

/// Render the all-comments popup (`@`) in place of the diff body, mirroring
/// [`render_target_picker`]: one row per comment — `C<id> [Class]
/// <file>:<line> — <body head>` — with the selection windowed into view.
fn render_comment_picker(frame: &mut Frame, area: Rect, state: &CodeReviewState) {
    let Some(picker) = state.comment_picker.as_ref() else {
        return;
    };
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        " Comments  (↑/↓ select · Enter jump · Esc)",
        Style::default().fg(Theme::text_muted()),
    ))];
    let height = (area.height as usize).saturating_sub(1);
    // Window the entries so the selection stays visible in a long list.
    let start = picker
        .selected
        .saturating_sub(height.saturating_sub(1))
        .min(picker.entries.len().saturating_sub(height.max(1)));
    for (i, id) in picker.entries.iter().enumerate().skip(start).take(height) {
        let selected = i == picker.selected;
        let marker = if selected { "▸ " } else { "  " };
        let (loc, class, head) = match state.comment(*id) {
            Some(c) => {
                let loc = match &c.anchor {
                    CommentAnchor::Line { file, side, line } => {
                        format!("{file}:{}:{line}", side.as_str())
                    }
                    CommentAnchor::File { file } => format!("{file} (file)"),
                    CommentAnchor::Review => "summary".to_string(),
                };
                let head: String = c
                    .body
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(60)
                    .collect();
                (loc, c.classification, head)
            }
            None => ("?".to_string(), Classification::default(), String::new()),
        };
        let style = if selected {
            Style::default()
                .fg(Theme::selection_fg())
                .bg(Theme::selection_bg())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Theme::text_primary())
        };
        let badge_style = if selected {
            style
        } else {
            Style::default()
                .fg(class_color(class))
                .add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}C{id} "), style),
            Span::styled(format!("[{}] ", class.label()), badge_style),
            Span::styled(
                truncate(
                    &format!("{loc} — {head}"),
                    (area.width as usize).saturating_sub(12),
                ),
                style,
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the windowed diff/comment rows + scrollbar. Returns row hitboxes
/// (index in `state.rows`) and the scrollbar geometry.
///
/// Selection, hitboxes, and the scrollbar are all defined over **logical** rows
/// (`state.rows`): the scrollbar thumb tracks `state.selected` over
/// `total = state.rows.len()`, and its drag mapping (`position_for_y` →
/// `cr_select_row`) feeds a logical index. When `wrap` is on, a logical row
/// expands into several **visual** rows; only the vertical windowing here
/// becomes visual — every visual sub-row carries its parent's logical index, so
/// clicks and the scrollbar stay correct.
fn render_rows(
    frame: &mut Frame,
    area: Rect,
    state: &mut CodeReviewState,
) -> (Vec<RowHitbox>, Option<ScrollbarGeom>) {
    let total = state.rows.len();
    let height = area.height as usize;
    if height == 0 {
        return (Vec::new(), None);
    }

    let (content, track) = scrollbar::reserve_track(area, total, height);
    let width = content.width as usize;

    // Line-number column width from the largest number on screen.
    let num_w = line_number_width(state);

    // Wrap applies to both layouts: a unified line soft-wraps its body, a paired
    // side-by-side row soft-wraps each half independently (the taller half drives
    // the row count). Horizontal scroll stays unified-only — side-by-side always
    // pins `h_scroll` to 0 (below), so the scroll math treats an unwrapped paired
    // row exactly like an unwrapped unified one.
    let wrap = state.wrap;

    // Final horizontal clamp: the app layer bounds `h_scroll` by the longest
    // line, but the exact body width is only known now — never scroll past what
    // the widest line can reveal. Wrapped/side-by-side layouts pin it to 0.
    // Gated on an active scroll: `max_line_width` is an O(total chars) scan,
    // so the default `h_scroll == 0` frame must not pay it on every repaint.
    if state.wrap || state.side_by_side {
        state.h_scroll = 0;
    } else if state.h_scroll > 0 {
        let avail = width.saturating_sub(gutter_width(num_w)).max(1);
        let max_h = state.max_line_width().saturating_sub(avail);
        state.h_scroll = state.h_scroll.min(max_h);
    }

    // Clamp scroll so the selection stays visible (the nav layer set a lower
    // bound; here we enforce the upper edge given the known height). With wrap
    // off, one logical row = one visual row, so the original math holds.
    if !wrap {
        if state.selected >= state.scroll + height {
            state.scroll = state.selected + 1 - height;
        }
        if state.scroll + height > total {
            state.scroll = total.saturating_sub(height);
        }
    }
    if state.selected < state.scroll {
        state.scroll = state.selected;
    }

    // The active search query drives the in-row match highlight — trimmed +
    // lowercased exactly like `search_matches`, so a row the matcher counted
    // never renders without its highlight (e.g. a query with a trailing space).
    let query = state
        .search
        .as_ref()
        .map(|s| s.query.trim().to_lowercase())
        .filter(|q| !q.is_empty());

    if wrap {
        converge_wrap_scroll(state, width, num_w, height);
    }

    // Build the windowed visual lines. When wrapping, the selected logical row's
    // first visual line must stay on screen: expand from `scroll` and, if the
    // selection would fall past `height`, advance `scroll` and retry (bounded by
    // `total`). Each visual line remembers its logical row for the hitboxes.
    let (visual, logical_of): (Vec<Line>, Vec<usize>) = loop {
        let mut lines: Vec<Line> = Vec::with_capacity(height);
        let mut logical: Vec<usize> = Vec::with_capacity(height);
        let mut selected_first: Option<usize> = None;
        for i in state.scroll..total {
            if lines.len() >= height {
                break;
            }
            if i == state.selected {
                selected_first = Some(lines.len());
            }
            for line in row_visual_lines(
                state,
                i,
                width,
                num_w,
                query.as_deref(),
                state.h_scroll,
                wrap,
            ) {
                lines.push(line);
                logical.push(i);
            }
        }
        // Selection off the bottom (only possible when wrapping inflates rows):
        // scroll down one logical row and rebuild.
        let overflows =
            wrap && selected_first.map_or(true, |f| f >= height) && state.scroll < state.selected;
        if overflows {
            state.scroll += 1;
            continue;
        }
        lines.truncate(height);
        logical.truncate(height);
        break (lines, logical);
    };

    frame.render_widget(Paragraph::new(visual), content);

    // The view is selection-primary: the thumb tracks `selected` (which reaches
    // the last row, `total - 1`), not `scroll` (which caps at `total - height`,
    // so a thumb driven by it could never reach the bottom of the track). This
    // also matches the drag mapping — `position_for_y` returns a `0..total`
    // index that `apply_scrollbar_position` feeds straight to `cr_select_row`.
    let geom = track.and_then(|t| scrollbar::render_into(frame, t, total, height, state.selected));

    let hitboxes = logical_of
        .into_iter()
        .enumerate()
        .map(|(row, index)| RowHitbox {
            rect: Rect::new(content.x, content.y + row as u16, content.width, 1),
            index,
        })
        .collect();
    (hitboxes, geom)
}

/// Width for the diff line-number gutter (two numbers, old+new).
fn line_number_width(state: &CodeReviewState) -> usize {
    let max = state
        .files
        .iter()
        .flat_map(|f| f.hunks.iter())
        .map(|h| h.new_start as usize + h.lines.len())
        .max()
        .unwrap_or(0);
    max.to_string().len().max(2)
}

/// Build the visual sub-rows for logical row `i`. `query` (lowercased,
/// non-empty) is the active find-in-diff search. `h_scroll` slides the body
/// horizontally (unified, non-wrap). With `wrap` on, a long diff line expands
/// into several `Line`s — a unified line wraps its body (continuation rows carry
/// a blank gutter), a paired side-by-side row wraps each half independently
/// (the taller half drives the row count); every other row kind yields one line.
fn row_visual_lines<'a>(
    state: &CodeReviewState,
    i: usize,
    width: usize,
    num_w: usize,
    query: Option<&str>,
    h_scroll: usize,
    wrap: bool,
) -> Vec<Line<'a>> {
    let selected = i == state.selected;
    let sel_style = |base: Style| {
        if selected {
            base.bg(Theme::selection_bg()).fg(Theme::selection_fg())
        } else {
            base
        }
    };

    match &state.rows[i] {
        ReviewRow::FileHeader(fi) => {
            vec![file_header_line(state, *fi, width, query, &sel_style)]
        }
        ReviewRow::HunkHeader(fi, hi) => {
            vec![hunk_header_line(state, *fi, *hi, width, query, &sel_style)]
        }
        ReviewRow::Line(fi, hi, li) => {
            let f = &state.files[*fi];
            let hunk = &f.hunks[*hi];
            if state.side_by_side {
                paired_diff_line(
                    f, hunk, *li, width, num_w, wrap, selected, query, &sel_style,
                )
            } else {
                let l = &hunk.lines[*li];
                // Word-level highlight of the changed tokens vs the aligned
                // counterpart line; empty for context / unrelated pairs. The
                // selection bg owns a selected row, so no word bg there.
                let word = word_ranges_for(hunk, *li);
                let word_bg = (!selected && !word.is_empty()).then(|| match l.kind {
                    DiffLineKind::Add => Theme::diff_added_word_bg(),
                    _ => Theme::diff_removed_word_bg(),
                });
                if wrap {
                    unified_diff_line_wrapped(
                        f, l, width, num_w, selected, query, &word, word_bg, &sel_style,
                    )
                } else {
                    vec![unified_diff_line(
                        f, l, width, num_w, selected, h_scroll, query, &word, word_bg, &sel_style,
                    )]
                }
            }
        }
        ReviewRow::Comment(id) | ReviewRow::Summary(id) => {
            vec![comment_line(state, *id, width, query, sel_style)]
        }
        ReviewRow::SummaryHeader => vec![Line::from(Span::styled(
            truncate("── Review summary (s to add) ──", width),
            sel_style(
                Style::default()
                    .fg(Theme::accent())
                    .add_modifier(Modifier::BOLD),
            ),
        ))],
        ReviewRow::Info(text) => vec![Line::from(Span::styled(
            truncate(text, width),
            Style::default().fg(Theme::text_muted()),
        ))],
    }
}

/// A file header: a full-width rule with the path embedded (tuicr-style file
/// separator), a fold chevron, and the status glyph + add/remove counts tinted
/// with the diff colors so the status reads at a glance.
fn file_header_line<'a>(
    state: &CodeReviewState,
    fi: usize,
    width: usize,
    query: Option<&str>,
    sel_style: &impl Fn(Style) -> Style,
) -> Line<'a> {
    let f = &state.files[fi];
    let chevron = if state.is_file_folded(&f.path) {
        "▸"
    } else {
        "▾"
    };
    let header = Style::default()
        .fg(Theme::accent_bright())
        .add_modifier(Modifier::BOLD);
    let lead = format!("{chevron} ");
    let glyph = f.glyph().to_string();
    let mid = format!(" {}  ", f.path);
    let adds = format!("+{}", f.added_count());
    let dels = format!(" -{}", f.deleted_count());
    let mark = if state.reviewed_files.contains(&f.path) {
        " ✓"
    } else {
        ""
    };
    let used = lead.chars().count()
        + glyph.chars().count()
        + mid.chars().count()
        + adds.chars().count()
        + dels.chars().count()
        + mark.chars().count()
        + 1; // trailing space before the rule
    let pad = if used < width {
        "─".repeat(width - used)
    } else {
        String::new()
    };
    let mut spans = vec![
        Span::styled(lead, sel_style(header)),
        Span::styled(
            glyph,
            sel_style(
                Style::default()
                    .fg(status_color(f.status))
                    .add_modifier(Modifier::BOLD),
            ),
        ),
    ];
    spans.extend(highlight_text(mid, sel_style(header), query));
    spans.extend([
        Span::styled(adds, sel_style(Style::default().fg(Theme::diff_added()))),
        Span::styled(dels, sel_style(Style::default().fg(Theme::diff_removed()))),
    ]);
    if !mark.is_empty() {
        spans.push(Span::styled(
            mark.to_string(),
            sel_style(Style::default().fg(Theme::status_done())),
        ));
    }
    spans.push(Span::styled(format!(" {pad}"), sel_style(header)));
    Line::from(spans)
}

/// A hunk header: the `@@ -a,b +c,d @@` ranges (recomputed from the lines) + the
/// section heading, with a `✓` when the hunk is marked reviewed.
fn hunk_header_line<'a>(
    state: &CodeReviewState,
    fi: usize,
    hi: usize,
    width: usize,
    query: Option<&str>,
    sel_style: &impl Fn(Style) -> Style,
) -> Line<'a> {
    let f = &state.files[fi];
    let h = &f.hunks[hi];
    let mark = if state.reviewed_hunks.contains(&(f.path.clone(), hi)) {
        " ✓"
    } else {
        ""
    };
    // Old-side span = lines present on the old side (context + deletions);
    // new-side span = lines present on the new side (context + additions).
    let old_span = h.lines.iter().filter(|l| l.old_no.is_some()).count();
    let new_span = h.lines.iter().filter(|l| l.new_no.is_some()).count();
    let text = format!(
        "  @@ -{},{} +{},{} @@ {}{}",
        h.old_start, old_span, h.new_start, new_span, h.header, mark
    );
    let base = sel_style(
        Style::default()
            .fg(Theme::accent())
            .add_modifier(Modifier::DIM),
    );
    Line::from(highlight_text(truncate(&text, width), base, query))
}

/// A unified-diff line: a `old new ±` gutter plus the syntax-highlighted body,
/// with the add/remove row tint (the gutter sign + tint carry the +/-, leaving
/// the text free for syntax colour). `word`/`word_bg` paint the word-level
/// intra-line diff over changed tokens. The gutter stays pinned; the body is
/// windowed to `[h_scroll, h_scroll + avail)` (horizontal scroll) and padded to
/// `width`.
#[allow(clippy::too_many_arguments)]
fn unified_diff_line<'a>(
    f: &DiffFile,
    l: &DiffLine,
    width: usize,
    num_w: usize,
    selected: bool,
    h_scroll: usize,
    query: Option<&str>,
    word: &[(usize, usize)],
    word_bg: Option<Color>,
    sel_style: &impl Fn(Style) -> Style,
) -> Line<'a> {
    let (sign, row_bg) = diff_row_bg(l.kind);
    let bg = row_bg_fn(row_bg, selected);
    let gutter = diff_gutter(l, num_w, sign);
    let avail = width.saturating_sub(gutter_width(num_w));

    let mut spans = vec![Span::styled(
        gutter,
        sel_style(bg(Style::default().fg(Theme::text_muted()))),
    )];
    spans.extend(diff_body_spans(
        f, &l.text, h_scroll, avail, query, word, word_bg, sel_style, &bg,
    ));
    Line::from(spans)
}

/// A unified-diff line soft-wrapped onto as many rows as its body needs. The
/// first row carries the real gutter; continuation rows carry a blank
/// gutter-width prefix so the body stays left-aligned. The row tint + selection
/// highlight cover every wrapped row (so a selected wrapped line reads as one
/// block). Empty bodies still emit one row (matching the non-wrap path).
#[allow(clippy::too_many_arguments)]
fn unified_diff_line_wrapped<'a>(
    f: &DiffFile,
    l: &DiffLine,
    width: usize,
    num_w: usize,
    selected: bool,
    query: Option<&str>,
    word: &[(usize, usize)],
    word_bg: Option<Color>,
    sel_style: &impl Fn(Style) -> Style,
) -> Vec<Line<'a>> {
    let (sign, row_bg) = diff_row_bg(l.kind);
    let bg = row_bg_fn(row_bg, selected);
    let gutter = diff_gutter(l, num_w, sign);
    let gutter_w = gutter_width(num_w);
    let avail = width.saturating_sub(gutter_w).max(1);

    let body_len = l.text.chars().count();
    let chunks = body_len.div_ceil(avail).max(1);
    let mut lines = Vec::with_capacity(chunks);
    for c in 0..chunks {
        let prefix = if c == 0 {
            Span::styled(
                gutter.clone(),
                sel_style(bg(Style::default().fg(Theme::text_muted()))),
            )
        } else {
            // Blank gutter so continuation text lines up under the first row.
            Span::styled(" ".repeat(gutter_w), sel_style(bg(Style::default())))
        };
        let mut spans = vec![prefix];
        spans.extend(diff_body_spans(
            f,
            &l.text,
            c * avail,
            avail,
            query,
            word,
            word_bg,
            sel_style,
            &bg,
        ));
        lines.push(Line::from(spans));
    }
    lines
}

/// The `('sign', row-tint)` for a diff line kind.
fn diff_row_bg(kind: DiffLineKind) -> (char, Option<Color>) {
    match kind {
        DiffLineKind::Add => ('+', Some(Theme::diff_added_bg())),
        DiffLineKind::Del => ('-', Some(Theme::diff_removed_bg())),
        DiffLineKind::Context => (' ', None),
    }
}

/// A closure that applies the row tint under a style — unless the row is
/// selected, where the selection background wins.
fn row_bg_fn(row_bg: Option<Color>, selected: bool) -> impl Fn(Style) -> Style {
    move |s: Style| match row_bg {
        Some(c) if !selected => s.bg(c),
        _ => s,
    }
}

/// The `{old} {new} {sign} ` line-number gutter for a unified diff line.
fn diff_gutter(l: &DiffLine, num_w: usize, sign: char) -> String {
    let old = l.old_no.map(|n| n.to_string()).unwrap_or_default();
    let new = l.new_no.map(|n| n.to_string()).unwrap_or_default();
    format!("{old:>num_w$} {new:>num_w$} {sign} ")
}

/// Cell width of [`diff_gutter`]'s output (two right-aligned numbers + three
/// single-space/sign separators). The single encoding of the gutter layout —
/// every consumer (h-scroll clamp, wrap chunking, continuation padding) derives
/// from it, so a format change can't desync them from what is painted.
fn gutter_width(num_w: usize) -> usize {
    num_w * 2 + 4
}

/// With wrap on, a far jump (`G`, a scrollbar drag, a search step) can leave
/// `scroll` many rows above the selection. Converge directly with a backward
/// walk over visual-row counts — O(height) count-only passes — instead of
/// letting `render_rows`' build loop advance one row per full-window rebuild
/// (quadratic in the jump distance, a visible stall on a large diff). Lands
/// on the smallest scroll ≥ the current one that keeps the selection's first
/// visual row within `height`.
fn converge_wrap_scroll(state: &mut CodeReviewState, width: usize, num_w: usize, height: usize) {
    if !state.wrap || state.selected <= state.scroll {
        return;
    }
    // Rows above the selection may fill at most height-1 visual rows, leaving
    // one for the selection's first line (saturating: height 0 → selection).
    let budget = height.saturating_sub(1);
    let mut acc = 0usize;
    let mut first = state.selected;
    while first > state.scroll {
        let c = visual_line_count(state, first - 1, width, num_w);
        if acc + c > budget {
            break;
        }
        acc += c;
        first -= 1;
    }
    state.scroll = first;
}

/// How many visual rows logical row `i` occupies — the count-only mirror of
/// [`row_visual_lines`] (wrap inflates only diff lines; every other row kind is
/// exactly one). Must stay in lockstep with [`unified_diff_line_wrapped`]'s and
/// [`paired_diff_line`]'s chunking so the wrap scroll walk in `render_rows`
/// lands exactly where the build does.
fn visual_line_count(state: &CodeReviewState, i: usize, width: usize, num_w: usize) -> usize {
    if !state.wrap {
        return 1;
    }
    match &state.rows[i] {
        ReviewRow::Line(fi, hi, li) => {
            let hunk = &state.files[*fi].hunks[*hi];
            if state.side_by_side {
                paired_visual_count(hunk, *li, width, num_w)
            } else {
                let l = &hunk.lines[*li];
                let avail = width.saturating_sub(gutter_width(num_w)).max(1);
                l.text.chars().count().div_ceil(avail).max(1)
            }
        }
        _ => 1,
    }
}

/// Visual-row count of a wrapped paired side-by-side row — the count-only mirror
/// of [`paired_diff_line`]'s chunking (each half wraps independently; the taller
/// half drives the row count). Kept next to it so the two can't drift.
fn paired_visual_count(hunk: &DiffHunk, li: usize, width: usize, num_w: usize) -> usize {
    let pair = paired_row(hunk, li);
    let body_w = paired_body_width(width, num_w);
    let lc = pair.old.map_or(0, |i| {
        hunk.lines[i].text.chars().count().div_ceil(body_w).max(1)
    });
    let rc = pair.new.map_or(0, |i| {
        hunk.lines[i].text.chars().count().div_ceil(body_w).max(1)
    });
    lc.max(rc).max(1)
}

/// Styled spans for a diff body windowed to `[start, start + avail)` chars,
/// padded to `avail`. When the active search query hits the visible window the
/// literal matches are highlighted over plain text (search clarity wins — over
/// syntax *and* the word-level diff); otherwise the syntax-highlighted token
/// stream is sliced to the window (the full line is tokenized for correctness,
/// then windowed) and the word-level diff ranges (`word`, absolute char
/// ranges) paint `word_bg` under their tokens. Shared by the horizontal-scroll
/// and wrap paths.
#[allow(clippy::too_many_arguments)]
fn diff_body_spans<'a>(
    f: &DiffFile,
    text: &str,
    start: usize,
    avail: usize,
    query: Option<&str>,
    word: &[(usize, usize)],
    word_bg: Option<Color>,
    sel_style: &impl Fn(Style) -> Style,
    bg: &impl Fn(Style) -> Style,
) -> Vec<Span<'a>> {
    let windowed: String = text.chars().skip(start).take(avail).collect();
    let positions = query
        .map(|q| match_positions(&windowed, q))
        .unwrap_or_default();

    let mut spans = Vec::new();
    let mut used;
    if !positions.is_empty() {
        used = windowed.chars().count();
        let base = sel_style(bg(Style::default().fg(Theme::text_primary())));
        spans.extend(crate::ui::highlight::highlighted_spans_owned(
            &windowed, &positions, base,
        ));
    } else {
        let lang = crate::ui::syntax::lang_for(&f.path);
        // Walk the full token stream, tracking the running char position so each
        // token can be sliced to the visible window `[start, start + avail)`.
        used = 0;
        let mut pos = 0usize;
        for (tok, tcolor) in crate::ui::syntax::highlight(text, &lang) {
            let tok_len = tok.chars().count();
            let tok_start = pos;
            pos += tok_len;
            if used >= avail {
                break;
            }
            // Intersect this token's char span with the visible window.
            if pos <= start {
                continue; // entirely left of the window
            }
            let skip = start.saturating_sub(tok_start);
            let piece: String = tok.chars().skip(skip).take(avail - used).collect();
            if piece.is_empty() {
                continue;
            }
            let piece_start = tok_start + skip;
            used += piece.chars().count();
            // Word-diff bg over the syntax fg: split the piece at word-range
            // boundaries so exactly the changed tokens carry the stronger bg.
            let style = sel_style(bg(Style::default().fg(tcolor)));
            spans.extend(word_split_spans(piece, piece_start, word, word_bg, style));
        }
    }
    // Pad so the row tint fills the available width.
    if used < avail {
        spans.push(Span::styled(
            " ".repeat(avail - used),
            sel_style(bg(Style::default())),
        ));
    }
    spans
}

/// Split `piece` (whose first char sits at absolute char index `piece_start`
/// of the full line) at the boundaries of the word-diff `ranges`, styling
/// covered sub-slices with `word_bg` over `base`. With no ranges (or no bg)
/// the piece stays one span.
fn word_split_spans<'a>(
    piece: String,
    piece_start: usize,
    ranges: &[(usize, usize)],
    word_bg: Option<Color>,
    base: Style,
) -> Vec<Span<'a>> {
    let Some(wbg) = word_bg else {
        return vec![Span::styled(piece, base)];
    };
    let piece_end = piece_start + piece.chars().count();
    let mut cuts = vec![piece_start, piece_end];
    for &(s, e) in ranges {
        if s > piece_start && s < piece_end {
            cuts.push(s);
        }
        if e > piece_start && e < piece_end {
            cuts.push(e);
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    let chars: Vec<char> = piece.chars().collect();
    cuts.windows(2)
        .map(|w| {
            let sub: String = chars[w[0] - piece_start..w[1] - piece_start]
                .iter()
                .collect();
            let covered = ranges.iter().any(|&(s, e)| w[0] >= s && w[1] <= e);
            let style = if covered { base.bg(wbg) } else { base };
            Span::styled(sub, style)
        })
        .collect()
}

/// Char ranges of row `li`'s changed tokens vs its positionally aligned
/// counterpart (`del[k] ↔ add[k]`, the same pairing the side-by-side layout
/// draws — a `-` run followed by a `+` run pairs positionally in unified
/// rendering too). Empty for context lines, unpaired halves, and unrelated
/// pairs (see [`crate::session::review::word_diff`]'s 30% gate).
fn word_ranges_for(hunk: &DiffHunk, li: usize) -> Vec<(usize, usize)> {
    let line = &hunk.lines[li];
    if line.kind == DiffLineKind::Context {
        return Vec::new();
    }
    let pair = paired_row(hunk, li);
    let (Some(o), Some(n)) = (pair.old, pair.new) else {
        return Vec::new();
    };
    let Some((old_r, new_r)) =
        crate::session::review::word_diff(&hunk.lines[o].text, &hunk.lines[n].text)
    else {
        return Vec::new();
    };
    if line.kind == DiffLineKind::Del {
        old_r
    } else {
        new_r
    }
}

fn comment_line<'a>(
    state: &CodeReviewState,
    id: i64,
    width: usize,
    query: Option<&str>,
    sel_style: impl Fn(Style) -> Style,
) -> Line<'a> {
    let Some(c) = state.comment(id) else {
        return Line::from("");
    };
    let badge = format!("  ▸ [{}] ", c.classification.label());
    let first = c.body.lines().next().unwrap_or("");
    let more = if c.body.lines().count() > 1 {
        " …"
    } else {
        ""
    };
    let body = truncate(
        &format!("{first}{more}"),
        width.saturating_sub(badge.chars().count()),
    );
    let mut spans = vec![Span::styled(
        badge,
        sel_style(
            Style::default()
                .fg(class_color(c.classification))
                .add_modifier(Modifier::BOLD),
        ),
    )];
    spans.extend(highlight_text(
        body,
        sel_style(Style::default().fg(Theme::text_secondary())),
        query,
    ));
    Line::from(spans)
}

/// Re-derive the [`SidePair`] a paired row stands for from the same pure pairing
/// the row builder used, so renderer and builder never disagree on which lines
/// share the row. `li` is the row's representative line index.
fn paired_row(hunk: &DiffHunk, li: usize) -> SidePair {
    pair_hunk(hunk)
        .into_iter()
        .find(|p| p.old == Some(li) || p.new == Some(li))
        .unwrap_or(SidePair {
            old: Some(li),
            new: None,
        })
}

/// Body (text) width of one side-by-side half cell: the half width minus the
/// right-aligned line-number column and its trailing space (mirrors
/// [`half_cell_chunk`]'s framing).
fn paired_body_width(width: usize, num_w: usize) -> usize {
    let half = width.saturating_sub(1) / 2;
    half.saturating_sub(num_w + 1).max(1)
}

/// Render one paired side-by-side row: the old-side cell `│` the new-side cell.
/// True GitHub-style pairing — a deletion (left) and its aligned addition
/// (right) sit on the *same* screen row (see
/// [`crate::session::review::pair_hunk`]). `li` is the row's representative line
/// index; the [`SidePair`] it belongs to supplies both sides (a blank half-cell
/// where a side is absent). The selection + comment anchor stay 1 row = 1
/// selectable unit; which side a comment attaches to is resolved at compose
/// time. Each half renders through [`diff_body_spans`], so syntax
/// highlighting, the word-level diff bg, and search highlighting compose here
/// exactly as in the unified body (add/remove stays on the tint + numbers,
/// matching the gutter-sign convention).
///
/// With `wrap` on, each half soft-wraps independently onto as many chunks as its
/// text needs; the taller half drives the visual-row count (mirrored by
/// [`paired_visual_count`]), and the shorter half pads with blank cells past its
/// last chunk. Off, each half truncates to one row (the historical behavior).
#[allow(clippy::too_many_arguments)]
fn paired_diff_line<'a>(
    f: &DiffFile,
    hunk: &DiffHunk,
    li: usize,
    width: usize,
    num_w: usize,
    wrap: bool,
    selected: bool,
    query: Option<&str>,
    sel_style: &impl Fn(Style) -> Style,
) -> Vec<Line<'a>> {
    let pair = paired_row(hunk, li);
    let half = width.saturating_sub(1) / 2;
    let body_w = paired_body_width(width, num_w);
    let prim = || Style::default().fg(Theme::text_primary());

    let left = pair.old.map(|i| &hunk.lines[i]);
    let right = pair.new.map(|i| &hunk.lines[i]);
    // Each cell tints only when it carries a change (a context line pairs with
    // itself and stays plain on both sides); sel_style overrides bg on select.
    let ltint = matches!(left, Some(l) if l.kind == DiffLineKind::Del).then(Theme::diff_removed_bg);
    let rtint = matches!(right, Some(l) if l.kind == DiffLineKind::Add).then(Theme::diff_added_bg);

    // Word-level diff across the aligned pair (only a real del↔add pair has
    // one); the selection bg owns a selected row, so no word bg there.
    let (word_old, word_new) = match (left, right) {
        (Some(l), Some(r))
            if l.kind == DiffLineKind::Del && r.kind == DiffLineKind::Add && !selected =>
        {
            crate::session::review::word_diff(&l.text, &r.text).unwrap_or_default()
        }
        _ => Default::default(),
    };

    // How many chunks each present half needs (an absent half contributes none);
    // the taller drives the row count. Must match `paired_visual_count`.
    let chunks = |line: Option<&DiffLine>| -> usize {
        match line {
            Some(l) if wrap => l.text.chars().count().div_ceil(body_w).max(1),
            Some(_) => 1,
            None => 0,
        }
    };
    let lchunks = chunks(left);
    let rchunks = chunks(right);
    let rows = lchunks.max(rchunks).max(1);

    (0..rows)
        .map(|c| {
            // A half renders its `c`-th chunk while it still has one; past that
            // (the shorter side, or an absent side) it pads blank + plain.
            let mut spans: Vec<Span> = Vec::new();
            if c < lchunks {
                let l = left.expect("chunk count > 0 implies present");
                spans.extend(half_cell_spans(
                    f,
                    l.old_no,
                    &l.text,
                    c,
                    num_w,
                    half,
                    ltint,
                    query,
                    &word_old,
                    Theme::diff_removed_word_bg(),
                    selected,
                    sel_style,
                ));
            } else {
                spans.push(Span::styled(
                    half_cell_chunk(None, "", 0, num_w, half),
                    sel_style(prim()),
                ));
            }
            spans.push(Span::styled(
                "│",
                sel_style(Style::default().fg(Theme::text_muted())),
            ));
            if c < rchunks {
                let r = right.expect("chunk count > 0 implies present");
                spans.extend(half_cell_spans(
                    f,
                    r.new_no,
                    &r.text,
                    c,
                    num_w,
                    half,
                    rtint,
                    query,
                    &word_new,
                    Theme::diff_added_word_bg(),
                    selected,
                    sel_style,
                ));
            } else {
                spans.push(Span::styled(
                    half_cell_chunk(None, "", 0, num_w, half),
                    sel_style(prim()),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

/// One present side-by-side half cell as styled spans: the right-aligned line
/// number column (blank on continuation chunks), then the `c`-th `body_w`-wide
/// slice of the body through [`diff_body_spans`] — so syntax highlighting, the
/// word-level diff bg, and search-match highlighting compose in the paired
/// layout exactly as in unified. Total width is exactly `cell_w`.
#[allow(clippy::too_many_arguments)]
fn half_cell_spans<'a>(
    f: &DiffFile,
    num: Option<u32>,
    text: &str,
    c: usize,
    num_w: usize,
    cell_w: usize,
    tint: Option<Color>,
    query: Option<&str>,
    word: &[(usize, usize)],
    word_bg_color: Color,
    selected: bool,
    sel_style: &impl Fn(Style) -> Style,
) -> Vec<Span<'a>> {
    let body_w = cell_w.saturating_sub(num_w + 1).max(1);
    let n = if c == 0 {
        num.map(|n| n.to_string()).unwrap_or_default()
    } else {
        String::new()
    };
    let bg = row_bg_fn(tint, selected);
    let word_bg = (!word.is_empty()).then_some(word_bg_color);
    let mut spans = vec![Span::styled(
        format!("{n:>num_w$} "),
        sel_style(bg(Style::default().fg(Theme::text_muted()))),
    )];
    spans.extend(diff_body_spans(
        f,
        text,
        c * body_w,
        body_w,
        query,
        word,
        word_bg,
        sel_style,
        &bg,
    ));
    spans
}

/// A fixed-width side-by-side half cell for wrap chunk `c`: the right-aligned
/// line number (only on `c == 0`; blank on continuation rows) then the `c`-th
/// `body_w`-wide slice of `text`, padded or truncated to exactly `cell_w`
/// columns so the center separator stays aligned. With `c == 0` and a
/// single-chunk text this reproduces the unwrapped cell exactly.
fn half_cell_chunk(num: Option<u32>, text: &str, c: usize, num_w: usize, cell_w: usize) -> String {
    let body_w = cell_w.saturating_sub(num_w + 1).max(1);
    let n = if c == 0 {
        num.map(|n| n.to_string()).unwrap_or_default()
    } else {
        String::new()
    };
    let slice: String = text.chars().skip(c * body_w).take(body_w).collect();
    let raw = format!("{n:>num_w$} {slice}");
    let len = raw.chars().count();
    if len > cell_w {
        raw.chars().take(cell_w).collect()
    } else {
        let mut s = raw;
        s.push_str(&" ".repeat(cell_w - len));
        s
    }
}

/// Render the changed-files list shown in the file-viewer column during a review
/// (the navigation aid; clicking a row jumps the diff to that file). Returns one
/// [`RowHitbox`] per visible file (index = diff-file index).
pub(crate) fn render_files_list(
    frame: &mut Frame,
    area: Rect,
    state: &CodeReviewState,
    level: FocusLevel,
) -> Vec<RowHitbox> {
    // The active filter is part of the header so the narrowed list can't be
    // mistaken for "these are all the changes".
    let title = match state.filter.label() {
        Some(l) => format!(" Changed files · {l} "),
        None => " Changed files ".to_string(),
    };
    let block = focus_block(&title, level);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 || state.files.is_empty() {
        return Vec::new();
    }
    // Reserve the last row for a compact nav-key legend (the keys that aren't
    // footer buttons), so all shortcuts stay discoverable.
    let hint_row = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(
                " ↑↓ move · ↵ open · r seen · o filter · / find",
                inner.width as usize,
            ),
            Style::default().fg(Theme::text_muted()),
        ))),
        hint_row,
    );
    let list = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
    let height = list.height as usize;
    if height == 0 {
        return Vec::new();
    }

    // An empty *filtered* list explains itself (the unfiltered list can't be
    // empty here) — never a bare pane with no hint of why.
    let visible = state.visible_file_indices();
    if visible.is_empty() {
        let what = state.filter.label().unwrap_or("matching");
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(
                    &format!(" No {what} files — o cycles the filter"),
                    list.width as usize,
                ),
                Style::default().fg(Theme::text_muted()),
            ))),
            Rect::new(list.x, list.y, list.width, 1),
        );
        return Vec::new();
    }

    // Render the files as a folder tree (directories as headers, files indented
    // beneath). `current_file()` is `None` on the summary section.
    let tree = build_file_tree(&state.files, &visible);
    let total = tree.len();
    let current_opt = state.current_file();
    let anchor = current_opt
        .and_then(|ci| {
            tree.iter()
                .position(|r| matches!(r, TreeRow::File { index, .. } if *index == ci))
        })
        .unwrap_or(0);
    let start = anchor
        .saturating_sub(height.saturating_sub(1))
        .min(total.saturating_sub(height));
    let end = (start + height).min(total);
    let w = list.width as usize;

    let mut lines: Vec<Line> = Vec::with_capacity(end - start);
    let mut hitboxes: Vec<RowHitbox> = Vec::new();
    for (row, ti) in (start..end).enumerate() {
        let line = match &tree[ti] {
            TreeRow::Folder { depth, name } => folder_row_line(*depth, name, w),
            TreeRow::File { depth, index } => {
                hitboxes.push(RowHitbox {
                    rect: Rect::new(list.x, list.y + row as u16, list.width, 1),
                    index: *index,
                });
                file_row_line(state, *index, *depth, current_opt == Some(*index))
            }
        };
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), list);
    hitboxes
}

/// A directory header row in the changed-files tree.
fn folder_row_line<'a>(depth: usize, name: &str, width: usize) -> Line<'a> {
    let text = format!("{}{name}/", "  ".repeat(depth));
    Line::from(Span::styled(
        truncate(&text, width),
        Style::default()
            .fg(Theme::text_muted())
            .add_modifier(Modifier::BOLD),
    ))
}

/// A file row in the changed-files tree: indented name + colored status glyph
/// and `+`/`-` counts (dimmed under the selection highlight on the current row).
fn file_row_line<'a>(
    state: &CodeReviewState,
    index: usize,
    depth: usize,
    current: bool,
) -> Line<'a> {
    let f = &state.files[index];
    let mark = if state.reviewed_files.contains(&f.path) {
        "✓ "
    } else {
        "  "
    };
    let name = f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
    let base = if current {
        Style::default()
            .fg(Theme::selection_fg())
            .bg(Theme::selection_bg())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Theme::text_primary())
    };
    // Tint the glyph + counts with diff colors, except on the selected row where
    // the highlight bg owns the line.
    let tint = |c: Color| if current { base } else { base.fg(c) };
    Line::from(vec![
        Span::styled(format!("{}{mark}", "  ".repeat(depth)), base),
        Span::styled(f.glyph().to_string(), tint(status_color(f.status))),
        Span::styled(format!(" {name}  "), base),
        Span::styled(format!("+{}", f.added_count()), tint(Theme::diff_added())),
        Span::styled(
            format!(" -{}", f.deleted_count()),
            tint(Theme::diff_removed()),
        ),
    ])
}

/// A row in the changed-files folder tree: a directory header or a file leaf
/// (carrying its diff-file index for click→jump).
enum TreeRow {
    Folder { depth: usize, name: String },
    File { depth: usize, index: usize },
}

/// Build a folder tree from the diff files listed in `indices` (the
/// filter-visible subset, in diff order): group by directory (so files in the
/// same folder sit together under one header), preserving each file's original
/// diff-file index for hit-testing. Multi-repo paths (`<repo>/<path>`) nest the
/// repo as the top-level folder automatically.
fn build_file_tree(files: &[crate::session::review::DiffFile], indices: &[usize]) -> Vec<TreeRow> {
    let mut entries: Vec<(usize, Vec<&str>)> = indices
        .iter()
        .map(|&i| (i, files[i].path.split('/').collect()))
        .collect();
    // Sort by path segments so sibling files group under a shared directory.
    entries.sort_by(|a, b| a.1.cmp(&b.1));

    let mut rows = Vec::new();
    let mut prev_dirs: Vec<&str> = Vec::new();
    for (idx, segs) in &entries {
        let dirs = &segs[..segs.len().saturating_sub(1)];
        // Emit a folder header for each directory segment that differs from the
        // previous file's path (the standard sorted-paths → tree fold).
        let common = dirs
            .iter()
            .zip(prev_dirs.iter())
            .take_while(|(a, b)| a == b)
            .count();
        for (d, seg) in dirs.iter().enumerate().skip(common) {
            rows.push(TreeRow::Folder {
                depth: d,
                name: seg.to_string(),
            });
        }
        rows.push(TreeRow::File {
            depth: dirs.len(),
            index: *idx,
        });
        prev_dirs = dirs.to_vec();
    }
    rows
}

/// Place the compose box inline at the selected line: just below it when there's
/// room, else just above it, else pinned to the bottom (selection off-screen).
/// Clears the area first so the diff underneath doesn't bleed through.
fn render_compose_inline(
    frame: &mut Frame,
    area: Rect,
    anchor_y: Option<u16>,
    comp: &ComposeState,
) {
    let h = area.height.clamp(3, 6);
    let bottom = area.y + area.height;
    let top = match anchor_y {
        Some(ay) if ay + 1 + h <= bottom => ay + 1,
        Some(ay) if ay >= area.y + h => ay - h,
        _ => bottom.saturating_sub(h),
    };
    // Indent one column so the box reads as attached to the line above it.
    let rect = Rect::new(area.x + 1, top, area.width.saturating_sub(2).max(1), h);
    frame.render_widget(Clear, rect);
    render_compose(frame, rect, comp);
}

/// Render the in-view comment compose box.
fn render_compose(frame: &mut Frame, area: Rect, comp: &ComposeState) {
    if area.height == 0 {
        return;
    }
    let target = match &comp.anchor {
        CommentAnchor::Line { side, line, .. } => format!("line {}:{}", side.as_str(), line),
        CommentAnchor::File { file } => format!("file {file}"),
        CommentAnchor::Review => "review summary".to_string(),
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!(" Comment on {target}  "),
            Style::default().fg(Theme::text_secondary()),
        ),
        Span::styled(
            format!("[{}]", comp.classification.label()),
            Style::default()
                .fg(class_color(comp.classification))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  Tab: cycle type · Ctrl+S: save · Esc: cancel",
            Style::default().fg(Theme::text_muted()),
        ),
    ]));
    // Body with a visible cursor.
    let (cur_line, cur_col) = comp.body.cursor_line_col();
    let body = comp.body.value();
    let body_lines: Vec<&str> = if body.is_empty() {
        vec![""]
    } else {
        body.split('\n').collect()
    };
    for (li, text) in body_lines.iter().enumerate() {
        if li == cur_line {
            lines.push(cursor_line(text, cur_col));
        } else {
            lines.push(Line::from(Span::styled(
                text.to_string(),
                Style::default().fg(Theme::text_primary()),
            )));
        }
    }
    let block = crate::ui::focus_block(" Compose ", FocusLevel::Focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// A compose body line with a reversed cell at the cursor column.
fn cursor_line<'a>(text: &str, col: usize) -> Line<'a> {
    let chars: Vec<char> = text.chars().collect();
    let before: String = chars.iter().take(col).collect();
    let at = chars.get(col).copied().unwrap_or(' ');
    let after: String = chars.iter().skip(col + 1).collect();
    Line::from(vec![
        Span::styled(before, Style::default().fg(Theme::text_primary())),
        Span::styled(
            at.to_string(),
            Style::default().add_modifier(Modifier::REVERSED),
        ),
        Span::styled(after, Style::default().fg(Theme::text_primary())),
    ])
}

/// Render the context-dependent footer button bar and pair each button with its
/// [`ReviewButton`] action.
fn render_footer(
    frame: &mut Frame,
    area: Rect,
    composing: bool,
    side_by_side: bool,
    wrap: bool,
) -> Vec<(crate::ui::ButtonHit, ReviewButton)> {
    // Each button carries its keyboard shortcut as a dimmed hint (`label·key`)
    // so it stays discoverable. The non-composing bar leads with the essential
    // actions (Comment, Send→Agent, Close) so they survive a narrow footer —
    // `render_button_bar` drops overflow from the right and marks it with `…`.
    let view_label = if side_by_side { "Unified" } else { "Side" };
    let wrap_label = if wrap { "NoWrap" } else { "Wrap" };
    let (specs, actions): (Vec<ButtonSpec>, Vec<ReviewButton>) = if composing {
        (
            vec![
                ButtonSpec::primary("Save").with_hint("·^S"),
                ButtonSpec::secondary("Cancel").with_hint("·esc"),
                ButtonSpec::secondary("Class").with_hint("·tab"),
            ],
            vec![
                ReviewButton::Save,
                ReviewButton::Cancel,
                ReviewButton::CycleClass,
            ],
        )
    } else {
        (
            vec![
                ButtonSpec::primary("Comment").with_hint("·c"),
                ButtonSpec::primary("Send→Agent").with_hint("·e"),
                ButtonSpec::secondary("Close").with_hint("·esc"),
                ButtonSpec::secondary("Find").with_hint("·/"),
                ButtonSpec::secondary("File").with_hint("·f"),
                ButtonSpec::secondary("Summary").with_hint("·s"),
                ButtonSpec::secondary("Reviewed").with_hint("·r"),
                ButtonSpec::secondary("Reload").with_hint("·F5"),
                ButtonSpec::secondary("Target").with_hint("·t"),
                ButtonSpec::secondary(view_label).with_hint("·v"),
                ButtonSpec::secondary(wrap_label).with_hint("·w"),
                ButtonSpec::secondary("Copy").with_hint("·y"),
            ],
            vec![
                ReviewButton::Comment,
                ReviewButton::SendToAgent,
                ReviewButton::Close,
                ReviewButton::Find,
                ReviewButton::FileComment,
                ReviewButton::Summary,
                ReviewButton::MarkReviewed,
                ReviewButton::Reload,
                ReviewButton::Target,
                ReviewButton::ToggleView,
                ReviewButton::ToggleWrap,
                ReviewButton::Copy,
            ],
        )
    };
    let hits = render_button_bar(frame, area, &specs, false);
    hits.into_iter().map(|h| (h, actions[h.index])).collect()
}

/// Byte offsets of every source char inside a case-insensitive occurrence of
/// `query` (already trimmed + lowercased) within `text`. Non-overlapping,
/// left-to-right. The text is lowered char-by-char *including multi-char
/// expansions* (`İ` → `i̇`), matching `search_matches`' `str::to_lowercase`
/// row filter — a per-char equality would leave such matcher hits rendering
/// with no highlight. Feeds
/// [`crate::ui::highlight::highlighted_spans_owned`] so search hits get the
/// same accent+bold+underline emphasis as the fuzzy lists elsewhere in the app.
fn match_positions(text: &str, query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    // (source byte offset, lowered char) — an expansion repeats its offset, so
    // a window landing anywhere inside it still highlights the source char.
    let lowered: Vec<(usize, char)> = text
        .char_indices()
        .flat_map(|(o, c)| c.to_lowercase().map(move |lc| (o, lc)))
        .collect();
    let q: Vec<char> = query.chars().collect();
    let (n, m) = (lowered.len(), q.len());
    let mut positions = Vec::new();
    let mut i = 0;
    while i + m <= n {
        if (0..m).all(|k| lowered[i + k].1 == q[k]) {
            positions.extend((0..m).map(|k| lowered[i + k].0));
            i += m;
        } else {
            i += 1;
        }
    }
    // Offsets are non-decreasing; expansions produce adjacent duplicates.
    positions.dedup();
    positions
}

/// Spans for `text` styled with `base`, with literal `query` hits highlighted
/// when `query` (lowercased, non-empty) is set and matches; otherwise a single
/// plain span. Owned so the spans can outlive a per-row `String`.
fn highlight_text(text: String, base: Style, query: Option<&str>) -> Vec<Span<'static>> {
    if let Some(q) = query {
        let positions = match_positions(&text, q);
        if !positions.is_empty() {
            return crate::ui::highlight::highlighted_spans_owned(&text, &positions, base);
        }
    }
    vec![Span::styled(text, base)]
}

/// Render the find-in-diff search bar (the top row of the diff area while a
/// search is open): the `/`-prefixed query (with a caret while typing) plus the
/// match position/count and key hints, so the shortcut stays discoverable.
fn render_search_bar(frame: &mut Frame, area: Rect, state: &CodeReviewState) {
    let Some(s) = state.search.as_ref() else {
        return;
    };
    let style = Style::default().fg(Theme::search_bar());
    let total = s.matches.len();
    // The "current" position is derived from the selection (like the file
    // viewer's `current_match_index`), so it stays correct after `n`/`N` or a
    // plain `j`/`k` move; 0 when the cursor isn't on a match.
    let current = s
        .matches
        .iter()
        .position(|&i| i == state.selected)
        .map(|p| p + 1)
        .unwrap_or(0);
    let count = if s.query.trim().is_empty() {
        String::new()
    } else if total == 0 {
        "  no matches".to_string()
    } else {
        format!("  {current}/{total}")
    };
    let caret = if s.editing { "█" } else { "" };
    let hint = if s.editing {
        "   ↵/↓ next · ↑ prev · tab done · esc cancel"
    } else {
        "   n next · N prev · esc clear"
    };
    let text = format!("/{}{caret}{count}{hint}", s.query);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            truncate(&text, area.width as usize),
            style,
        ))),
        area,
    );
}

/// Truncate `s` to `width` display columns (char-based; good enough for the
/// diff text we render).
fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        s.chars().take(width.saturating_sub(1)).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::code_review::CodeReviewState;
    use crate::session::review::{DiffFile, DiffHunk, DiffLine, DiffLineKind, FileStatus};
    use crate::session::SessionId;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::HashSet;

    fn demo_state() -> CodeReviewState {
        let file = DiffFile {
            path: "src/a/very/deep/foo.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            untracked: false,
            note: None,
            hunks: vec![DiffHunk {
                old_start: 1,
                new_start: 1,
                header: "fn x".into(),
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Context,
                        old_no: Some(1),
                        new_no: Some(1),
                        text: "ctx".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Del,
                        old_no: Some(2),
                        new_no: None,
                        text: "old".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Add,
                        old_no: None,
                        new_no: Some(2),
                        text: "new".into(),
                    },
                ],
            }],
        };
        let mut s = CodeReviewState {
            session_id: SessionId::default(),
            loading: false,
            repos: vec![crate::app::code_review::ReviewRepo {
                label: String::new(),
                dir: std::path::PathBuf::from("/tmp"),
                base: Some("main".into()),
            }],
            multi: false,
            files: vec![file],
            comments: Vec::new(),
            reviewed_files: HashSet::new(),
            reviewed_hunks: HashSet::new(),
            fold_override: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            compose: None,
            side_by_side: false,
            click_side: None,
            h_scroll: 0,
            wrap: false,
            target: crate::app::code_review::ReviewTarget::Branch,
            commits: Vec::new(),
            host: None,
            target_picker: None,
            search: None,
            filter: crate::app::code_review::ReviewFilter::default(),
            comment_picker: None,
            context: crate::app::code_review::DEFAULT_CONTEXT,
        };
        s.rebuild_rows();
        s
    }

    /// Like `demo_state` but the single hunk's first line is very long, so
    /// horizontal-scroll and wrap have something to act on.
    fn demo_state_long() -> CodeReviewState {
        let mut s = demo_state();
        s.files[0].hunks[0].lines[0].text = "z".repeat(300);
        s.rebuild_rows();
        s
    }

    #[test]
    fn renders_unified_and_side_by_side_without_panic() {
        for sxs in [false, true] {
            for wrap in [false, true] {
                let mut state = demo_state();
                state.side_by_side = sxs;
                state.wrap = wrap;
                let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
                term.draw(|f| {
                    let area = Rect::new(0, 0, 100, 20);
                    let hits = render(f, area, &mut state, FocusLevel::Focused);
                    assert!(!hits.rows.is_empty(), "diff rows are clickable");
                    assert!(!hits.buttons.is_empty(), "footer buttons render");
                })
                .unwrap();
            }
        }
    }

    /// A state whose one change pair shares most tokens (`let x = old;` →
    /// `let x = new;`), so the word-level diff highlights exactly `old`/`new`.
    fn word_diff_state() -> CodeReviewState {
        let mut s = demo_state();
        s.files[0].hunks[0].lines[1].text = "let x = old;".into();
        s.files[0].hunks[0].lines[2].text = "let x = new;".into();
        // Park the selection on the file header so no diff row is
        // selection-styled (the selection bg would override the word bg).
        s.selected = 0;
        s.rebuild_rows();
        s
    }

    /// Cells painted with `bg` on row `y`, as a string of their symbols.
    fn cells_with_bg(buf: &ratatui::buffer::Buffer, y: u16, w: u16, bg: Color) -> String {
        (0..w)
            .filter(|&x| buf[(x, y)].style().bg == Some(bg))
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    /// Unified: the changed token carries the stronger word bg; the shared
    /// tokens keep the plain row tint.
    #[test]
    fn word_diff_highlights_changed_token_in_unified() {
        let mut state = word_diff_state();
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| {
            let _ = render(f, Rect::new(0, 0, 60, 20), &mut state, FocusLevel::Focused);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let (mut removed_hl, mut added_hl) = (String::new(), String::new());
        for y in 0..20 {
            removed_hl.push_str(&cells_with_bg(buf, y, 60, Theme::diff_removed_word_bg()));
            added_hl.push_str(&cells_with_bg(buf, y, 60, Theme::diff_added_word_bg()));
        }
        assert_eq!(removed_hl, "old", "only the removed token gets the word bg");
        assert_eq!(added_hl, "new", "only the added token gets the word bg");
    }

    /// Side-by-side: the same word-level highlight lands in each half cell —
    /// and the halves now carry syntax colour (`let` as a keyword), proving
    /// the highlighter runs there too.
    #[test]
    fn word_diff_and_syntax_compose_in_side_by_side() {
        let mut state = word_diff_state();
        state.side_by_side = true;
        state.rebuild_rows();
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| {
            let _ = render(f, Rect::new(0, 0, 80, 20), &mut state, FocusLevel::Focused);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let (mut removed_hl, mut added_hl) = (String::new(), String::new());
        let mut fg_colors: std::collections::HashSet<String> = std::collections::HashSet::new();
        for y in 0..20 {
            removed_hl.push_str(&cells_with_bg(buf, y, 80, Theme::diff_removed_word_bg()));
            added_hl.push_str(&cells_with_bg(buf, y, 80, Theme::diff_added_word_bg()));
            let row: String = (0..80).map(|x| buf[(x, y)].symbol()).collect();
            if row.contains("let x = old;") {
                // Collect the distinct fg colours across the paired row's text:
                // syntax highlighting must produce more than one.
                for x in 0..80 {
                    if buf[(x, y)].symbol() != " " {
                        fg_colors.insert(format!("{:?}", buf[(x, y)].style().fg));
                    }
                }
            }
        }
        assert_eq!(removed_hl, "old");
        assert_eq!(added_hl, "new");
        assert!(
            fg_colors.len() > 1,
            "syntax highlighting colours the paired halves: {fg_colors:?}"
        );
    }

    /// Search-match highlighting wins over the word-diff bg (the match must
    /// stay legible), per the search-first rule in `diff_body_spans`.
    #[test]
    fn search_highlight_wins_over_word_diff() {
        let mut state = word_diff_state();
        state.search = Some(crate::app::code_review::ReviewSearch {
            query: "new".into(),
            editing: false,
            matches: Vec::new(),
        });
        state.refresh_search_matches();
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| {
            let _ = render(f, Rect::new(0, 0, 60, 20), &mut state, FocusLevel::Focused);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let mut added_hl = String::new();
        for y in 0..20 {
            added_hl.push_str(&cells_with_bg(buf, y, 60, Theme::diff_added_word_bg()));
        }
        assert!(
            added_hl.is_empty(),
            "the matched line renders search emphasis, not word bg: {added_hl:?}"
        );
    }

    /// True paired side-by-side draws a deletion and its aligned addition on the
    /// SAME screen row: one line shows the old body left, the new body right,
    /// split by the `│` separator — the density win over v1's split-column.
    #[test]
    fn paired_side_by_side_shows_both_sides_on_one_row() {
        let mut state = demo_state();
        state.side_by_side = true;
        state.rebuild_rows();
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| {
            let _ = render(f, Rect::new(0, 0, 60, 20), &mut state, FocusLevel::Focused);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let mut paired_row = false;
        for y in 0..20 {
            let row: String = (0..60).map(|x| buf[(x, y)].symbol()).collect();
            if row.contains("old") && row.contains("new") && row.contains('│') {
                paired_row = true;
            }
        }
        assert!(
            paired_row,
            "the deletion (old) and addition (new) render on one paired row"
        );
    }

    /// A far jump under wrap (`G`-style: selection moved, scroll untouched)
    /// converges in one pass: `converge_wrap_scroll` walks visual-row counts
    /// backward from the selection instead of the build loop's
    /// one-row-per-rebuild retry, and lands on the same invariant — the
    /// selection's first visual row fits within the viewport height.
    #[test]
    fn converge_wrap_scroll_jumps_directly_to_the_selection() {
        let mut state = demo_state_long();
        state.wrap = true;
        // Rows: FileHeader, HunkHeader, the 300-char line (row 2), then two
        // short lines. At width 40 / num_w 2 the long line wraps to 10 visual
        // rows, so with height 5 a selection on the last row (4) must scroll
        // past it: rows 3+4 fill 2 of the 4 non-selection slots, row 2's 10
        // don't fit → scroll = 3.
        state.selected = state.rows.len() - 1;
        state.scroll = 0;
        converge_wrap_scroll(&mut state, 40, 2, 5);
        assert_eq!(state.scroll, 3);

        // A selection already in reach leaves scroll alone.
        let mut near = demo_state_long();
        near.wrap = true;
        near.selected = 1;
        converge_wrap_scroll(&mut near, 40, 2, 5);
        assert_eq!(near.scroll, 0);
    }

    /// Wrap expands a long line onto extra visual rows: several hitboxes share
    /// the same logical `index` (so a click on any wrapped row selects the whole
    /// line), and the tail of the line appears on a lower row.
    #[test]
    fn wrap_expands_long_line_into_multiple_visual_rows() {
        let mut state = demo_state_long();
        state.wrap = true;
        // Select the long line (row 2: FileHeader, HunkHeader, then first line).
        state.selected = 2;
        let mut term = Terminal::new(TestBackend::new(40, 20)).unwrap();
        let mut hits: Vec<RowHitbox> = Vec::new();
        term.draw(|f| {
            hits = render(f, Rect::new(0, 0, 40, 20), &mut state, FocusLevel::Focused).rows;
        })
        .unwrap();
        let dup = hits.iter().filter(|h| h.index == 2).count();
        assert!(
            dup >= 2,
            "the long line wraps onto ≥2 visual rows sharing its logical index (got {dup})"
        );
        // The wrapped continuation (the second visual row of logical line 2)
        // still shows body text — with a blank gutter under the first row.
        let cont_y = hits.iter().filter(|h| h.index == 2).nth(1).unwrap().rect.y;
        let buf = term.backend().buffer();
        let cont: String = (0..40).map(|x| buf[(x, cont_y)].symbol()).collect();
        assert!(
            cont.contains('z'),
            "wrapped continuation shows body text: {cont:?}"
        );
    }

    /// Wrap in side-by-side: a long paired row expands onto several visual rows
    /// sharing its logical index (so selection/anchor stay 1 row = 1 unit), each
    /// keeps the `│` separator, and the wrapped tail shows body text on a lower
    /// row — the split-mode mirror of [`wrap_expands_long_line_into_multiple_visual_rows`].
    #[test]
    fn wrap_expands_paired_row_in_side_by_side() {
        let mut state = demo_state_long();
        state.side_by_side = true;
        state.wrap = true;
        state.rebuild_rows();
        // Side-by-side rows: FileHeader, HunkHeader, then one paired row per
        // SidePair. The 300-char line is the context pair (logical row 2).
        state.selected = 2;
        let mut term = Terminal::new(TestBackend::new(40, 20)).unwrap();
        let mut hits: Vec<RowHitbox> = Vec::new();
        term.draw(|f| {
            hits = render(f, Rect::new(0, 0, 40, 20), &mut state, FocusLevel::Focused).rows;
        })
        .unwrap();
        let dup = hits.iter().filter(|h| h.index == 2).count();
        assert!(
            dup >= 2,
            "the long paired row wraps onto ≥2 visual rows sharing its logical index (got {dup})"
        );
        let cont_y = hits.iter().filter(|h| h.index == 2).nth(1).unwrap().rect.y;
        let buf = term.backend().buffer();
        let cont: String = (0..40).map(|x| buf[(x, cont_y)].symbol()).collect();
        assert!(
            cont.contains('z'),
            "wrapped continuation shows body text: {cont:?}"
        );
        assert!(
            cont.contains('│'),
            "the paired separator persists on continuation rows: {cont:?}"
        );
    }

    /// Asymmetric paired wrap: a long deletion paired with a short addition. The
    /// taller (left) half drives the visual-row count — and the builder emits
    /// exactly as many rows as `paired_visual_count` predicts (the counter/render
    /// mirror can't drift) — while the shorter half renders only on the first row
    /// and pads blank on the continuations.
    #[test]
    fn wrap_paired_row_taller_half_drives_rows_shorter_pads_blank() {
        let mut state = demo_state();
        // demo_state's hunk is [ctx, Del "old", Add "new"]; make the deletion
        // long so its half wraps while the addition stays a single row.
        state.files[0].hunks[0].lines[1].text = "x".repeat(300);
        state.side_by_side = true;
        state.wrap = true;
        state.rebuild_rows();
        // Side-by-side rows: FileHeader, HunkHeader, context pair (2), the
        // del/add pair (3, keyed by the old line index 1).
        state.selected = 3;
        // `render` wraps the diff in a bordered block, so the row builder sees a
        // content width of (area 40 − 1-col border each side) = 38 — feed the
        // counter the same so `expected` matches what the builder emits.
        let num_w = line_number_width(&state);
        let expected = paired_visual_count(&state.files[0].hunks[0], 1, 38, num_w);
        assert!(expected >= 2, "the long deletion should wrap to ≥2 rows");

        // Tall enough that the whole wrapped pair (plus the header/context rows
        // and the footer) fits the viewport, so the windowed render isn't cut
        // short and can be compared against the full `paired_visual_count`.
        let mut term = Terminal::new(TestBackend::new(40, 40)).unwrap();
        let mut hits: Vec<RowHitbox> = Vec::new();
        term.draw(|f| {
            hits = render(f, Rect::new(0, 0, 40, 40), &mut state, FocusLevel::Focused).rows;
        })
        .unwrap();

        let pair_rows: Vec<u16> = hits
            .iter()
            .filter(|h| h.index == 3)
            .map(|h| h.rect.y)
            .collect();
        assert_eq!(
            pair_rows.len(),
            expected,
            "the builder emits exactly paired_visual_count rows"
        );

        let buf = term.backend().buffer();
        let row_text = |y: u16| -> String { (0..40).map(|x| buf[(x, y)].symbol()).collect() };
        // The short addition ("new") shows on exactly one visual row of the pair.
        let with_new = pair_rows
            .iter()
            .filter(|&&y| row_text(y).contains("new"))
            .count();
        assert_eq!(
            with_new, 1,
            "shorter half renders once, blank on continuations"
        );
        // Every row of the pair carries the long deletion's wrapping tail.
        assert!(
            pair_rows.iter().all(|&y| row_text(y).contains('x')),
            "the taller half's wrapped body spans all pair rows"
        );
    }

    /// With wrap off, a long line stays one visual row per logical row (the
    /// selection/anchor invariant), and horizontal scroll reveals its tail with
    /// the line-number gutter still pinned at column 0.
    #[test]
    fn h_scroll_reveals_tail_with_pinned_gutter() {
        let mut state = demo_state_long();
        state.wrap = false;
        state.h_scroll = 100;
        let mut term = Terminal::new(TestBackend::new(40, 20)).unwrap();
        let mut hits: Vec<RowHitbox> = Vec::new();
        term.draw(|f| {
            hits = render(f, Rect::new(0, 0, 40, 20), &mut state, FocusLevel::Focused).rows;
        })
        .unwrap();
        // One hitbox per logical row — no visual expansion when wrap is off.
        assert_eq!(
            hits.iter().filter(|h| h.index == 2).count(),
            1,
            "wrap off → one visual row per logical line"
        );
        // Locate the diff-line row (logical index 2) on screen via its hitbox;
        // read from the hitbox's x (past the block border) so we see the gutter.
        let hb = hits.iter().find(|h| h.index == 2).unwrap();
        let (x0, y) = (hb.rect.x, hb.rect.y);
        let buf = term.backend().buffer();
        let row: String = (x0..x0 + hb.rect.width)
            .map(|x| buf[(x, y)].symbol())
            .collect();
        // The gutter (old/new line numbers "1 1") is still pinned at the left.
        assert!(
            row.trim_start().starts_with('1'),
            "line-number gutter stays pinned at the body's left edge: {row:?}"
        );
        assert!(
            row.contains('z'),
            "the scrolled-in body is visible: {row:?}"
        );
    }

    #[test]
    fn footer_leads_with_essentials_and_marks_overflow_when_narrow() {
        // 40 cols fits exactly Comment·c (11) + Send→Agent·e (14) + Close·esc
        // (11) with separators (11+1+14+1+11 = 38); the rest overflow.
        let mut term = Terminal::new(TestBackend::new(40, 1)).unwrap();
        let mut actions = Vec::new();
        term.draw(|f| {
            actions = render_footer(f, Rect::new(0, 0, 40, 1), false, false, false)
                .into_iter()
                .map(|(_, a)| a)
                .collect();
        })
        .unwrap();
        assert_eq!(
            actions,
            vec![
                ReviewButton::Comment,
                ReviewButton::SendToAgent,
                ReviewButton::Close,
            ],
            "the essential actions lead and survive a narrow footer"
        );
        let buf = term.backend().buffer();
        let row: String = (0..40).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(
            row.contains('…'),
            "dropped buttons are marked with an ellipsis"
        );
    }

    #[test]
    fn renders_changed_files_list_without_panic() {
        let state = demo_state();
        let mut term = Terminal::new(TestBackend::new(30, 10)).unwrap();
        term.draw(|f| {
            let area = Rect::new(0, 0, 30, 10);
            let rows = render_files_list(f, area, &state, FocusLevel::Active);
            assert_eq!(rows.len(), 1, "one changed file → one clickable row");
        })
        .unwrap();
    }

    #[test]
    fn file_tree_groups_dirs_and_keeps_indices() {
        use crate::session::review::{DiffFile, FileStatus};
        let mk = |p: &str| DiffFile {
            path: p.into(),
            old_path: None,
            status: FileStatus::Modified,
            untracked: false,
            note: None,
            hunks: Vec::new(),
        };
        // Out of path order on purpose — the tree sorts + groups by directory.
        let files = vec![mk("src/b.rs"), mk("top.rs"), mk("src/ui/a.rs")];
        let tree = build_file_tree(&files, &[0, 1, 2]);
        // Folder headers appear for `src` and `src/ui`; the top-level file has no
        // folder. Each file row carries its ORIGINAL index for click→jump.
        let folders: Vec<(usize, &str)> = tree
            .iter()
            .filter_map(|r| match r {
                TreeRow::Folder { depth, name } => Some((*depth, name.as_str())),
                _ => None,
            })
            .collect();
        assert!(folders.contains(&(0, "src")));
        assert!(folders.contains(&(1, "ui")));
        let file_indices: Vec<usize> = tree
            .iter()
            .filter_map(|r| match r {
                TreeRow::File { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        // All three original indices present (order is by sorted path).
        assert_eq!(file_indices.len(), 3);
        assert!(
            file_indices.contains(&0) && file_indices.contains(&1) && file_indices.contains(&2)
        );
    }

    #[test]
    fn match_positions_finds_case_insensitive_runs() {
        // "Foo bar foo" with query "foo" → two non-overlapping matches, each
        // contributing one byte offset per char (3 chars → 3 offsets).
        let pos = match_positions("Foo bar foo", "foo");
        assert_eq!(pos, vec![0, 1, 2, 8, 9, 10]);
        // No match → empty; empty query → empty.
        assert!(match_positions("abc", "z").is_empty());
        assert!(match_positions("abc", "").is_empty());
    }

    #[test]
    fn match_positions_handles_multichar_lowercase_expansion() {
        // `İ` (U+0130) lowers to two chars (`i` + U+0307). `search_matches`
        // lowers rows with `str::to_lowercase`, so it counts this row as a hit
        // for the lowered query — the highlighter must agree and mark the
        // source char (one deduped offset, since both lowered chars share it).
        let lowered = "\u{130}".to_lowercase(); // "i\u{307}"
        let pos = match_positions("a\u{130}b", &lowered);
        assert_eq!(pos, vec![1], "the single source char is highlighted once");
    }

    #[test]
    fn renders_search_bar_and_highlights_match() {
        let mut state = demo_state();
        state.search = Some(crate::app::code_review::ReviewSearch {
            query: "ctx".to_string(),
            editing: true,
            matches: state.search_matches("ctx"),
        });
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| {
            let area = Rect::new(0, 0, 80, 20);
            let _ = render(f, area, &mut state, FocusLevel::Focused);
        })
        .unwrap();
        // The search bar prints the `/`-prefixed query somewhere on screen.
        let buf = term.backend().buffer();
        let mut screen = String::new();
        for y in 0..20 {
            for x in 0..80 {
                screen.push_str(buf[(x, y)].symbol());
            }
            screen.push('\n');
        }
        assert!(
            screen.contains("/ctx"),
            "search bar shows the query: {screen}"
        );
    }

    #[test]
    fn renders_inline_compose_without_panic() {
        use crate::session::review::{Classification, CommentAnchor, Side};
        let mut state = demo_state();
        state.selected = 2; // a Line row
        state.compose = Some(crate::app::code_review::ComposeState {
            anchor: CommentAnchor::Line {
                file: "src/foo.rs".into(),
                side: Side::New,
                line: 2,
            },
            classification: Classification::Issue,
            body: crate::app::modals::TextArea::new(),
            editing_id: None,
        });
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| {
            let area = Rect::new(0, 0, 100, 20);
            let hits = render(f, area, &mut state, FocusLevel::Focused);
            // Footer still renders its buttons while composing.
            assert!(!hits.buttons.is_empty());
        })
        .unwrap();
    }
}
