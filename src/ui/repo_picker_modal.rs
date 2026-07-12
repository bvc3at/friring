use std::collections::HashSet;
use std::path::PathBuf;

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use super::render_modal_frame;
use super::theme::Theme;
use super::{centered_fixed_height_rect, render_text_field_with_suggestion};
use crate::app::modals::{PathCandidate, RepoInputMode, RepoRow, RepoRowKind};

pub struct RepoPickerState<'a> {
    /// Bookmark rows (headers, children, standalone repos) followed by the
    /// pinned helper rows, in display order.
    pub rows: &'a [RepoRow],
    /// Checked repos, keyed by path.
    pub selected: &'a HashSet<PathBuf>,
    /// Repos flagged for worktree mode, keyed by path.
    pub worktree: &'a HashSet<PathBuf>,
    /// Parent folders whose child tree is collapsed (drives the ▸/▾ glyph).
    pub collapsed: &'a HashSet<PathBuf>,
    pub list_index: usize,
    pub filtered_indices: &'a [usize],
    /// The single always-focused palette input.
    pub input: &'a str,
    pub input_cursor: usize,
    /// Fish-style ghost completion (path mode, local only).
    pub suggestion: Option<&'a str>,
    pub mode: RepoInputMode,
    /// Path mode: live directory candidates + the highlighted one (`None` =
    /// the typed path itself is the Enter target).
    pub candidates: &'a [PathCandidate],
    pub candidate_index: Option<usize>,
    /// Checked-repo count (shown in the list title and the Enter hint).
    pub picked: usize,
    /// The target host's name for an off-local session (`None` = local).
    /// Shown in the list title so it's unambiguous whose filesystem the
    /// repos (and the typed path) belong to.
    pub host: Option<&'a str>,
}

pub fn render_repo_picker_modal(
    frame: &mut Frame,
    state: &RepoPickerState<'_>,
) -> super::ModalRender {
    let row_count = match state.mode {
        RepoInputMode::Filter => state.filtered_indices.len(),
        RepoInputMode::Path => state.candidates.len(),
    };
    let visible_count = row_count.clamp(1, 10);
    let list_height = visible_count as u16 + 2; // +2 for borders

    // Layout: list + palette input(3) + footer(1) + outer border(2)
    let total_height = list_height + 3 + 1 + 2;

    let area = centered_fixed_height_rect(60, total_height, frame.area());

    let inner = render_modal_frame(frame, area, "New Session — Repo");

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(list_height),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(inner);
    let (list_area, input_area, footer_area) = (chunks[0], chunks[1], chunks[2]);

    let hitboxes = match state.mode {
        RepoInputMode::Filter => render_bookmark_list(frame, list_area, state),
        RepoInputMode::Path => render_candidate_list(frame, list_area, state),
    };

    render_text_field_with_suggestion(
        frame,
        input_area,
        "Filter or path",
        state.input,
        state.input_cursor,
        true,
        state.suggestion,
    );

    // Footer: mode-dependent key hints on the left, clickable `[ Open ]`
    // (Enter) / `[ Cancel ]` (Esc) buttons on the right. The hint is clipped to
    // the space left of the pills so a wide hint row can't render underneath
    // them (see `render_hint_action_footer`).
    let buttons = super::render_hint_action_footer(
        frame,
        footer_area,
        footer_line(state),
        (
            "Open",
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
        "Cancel",
    );
    (hitboxes, buttons)
}

/// Render the bookmark list with checkboxes, fuzzy highlighting, and scrolling.
fn render_bookmark_list(
    frame: &mut Frame,
    list_area: ratatui::layout::Rect,
    state: &RepoPickerState<'_>,
) -> super::SelectorHits {
    let repo_count = state.rows.iter().filter(|r| r.is_repo()).count();
    let mut title = match state.host {
        Some(host) => format!(" Repos on {host} ({repo_count})"),
        None => format!(" Repos ({repo_count})"),
    };
    if state.picked > 0 {
        title.push_str(&format!(" — {} picked", state.picked));
    }
    title.push(' ');

    let list_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::border_unfocused()));

    let list_inner_area = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);

    if state.filtered_indices.is_empty() {
        let placeholder = Paragraph::new(Line::from(Span::styled(
            "  No matches",
            Style::default().fg(Theme::text_muted()),
        )));
        frame.render_widget(placeholder, list_inner_area);
        return (Vec::new(), None);
    }

    let total = state.filtered_indices.len();
    let visible_count = list_inner_area.height as usize;
    let scroll_offset = if state.list_index >= visible_count {
        state.list_index - visible_count + 1
    } else {
        0
    };

    // Reserve the rightmost column for a scrollbar when the list overflows.
    let (rows_area, track) = super::scrollbar::reserve_track(list_inner_area, total, visible_count);

    let items: Vec<ListItem<'_>> = state
        .filtered_indices
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_count)
        .map(|(vi, &real_idx)| bookmark_item(state, vi, real_idx))
        .collect();

    frame.render_widget(List::new(items), rows_area);

    // One hitbox per visible row, indexed by position in `filtered_indices`
    // (the same space as `list_index`).
    let hitboxes = (scroll_offset..total.min(scroll_offset + visible_count))
        .enumerate()
        .map(|(line, vi)| super::RowHitbox {
            rect: ratatui::layout::Rect::new(
                rows_area.x,
                rows_area.y + line as u16,
                rows_area.width,
                1,
            ),
            index: vi,
        })
        .collect();

    let geom = track.and_then(|t| {
        super::scrollbar::render_into(frame, t, total, visible_count, state.list_index)
    });
    (hitboxes, geom)
}

/// Render the path-mode directory candidates. No row hitboxes: the palette is
/// keyboard-first here (the wheel still steps the highlight via Up/Down), and
/// a candidate click would need its own index space vs `list_index`.
fn render_candidate_list(
    frame: &mut Frame,
    list_area: ratatui::layout::Rect,
    state: &RepoPickerState<'_>,
) -> super::SelectorHits {
    let title = match state.host {
        Some(host) => format!(" Directories on {host} ({}) ", state.candidates.len()),
        None => format!(" Directories ({}) ", state.candidates.len()),
    };
    let list_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Theme::border_unfocused()));
    let list_inner_area = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);

    if state.candidates.is_empty() {
        let msg = match state.host {
            Some(_) => "  Tab lists remote directories (one ssh call)",
            None => "  No matching directories",
        };
        let placeholder = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(Theme::text_muted()),
        )));
        frame.render_widget(placeholder, list_inner_area);
        return (Vec::new(), None);
    }

    let visible_count = list_inner_area.height as usize;
    let anchor = state.candidate_index.unwrap_or(0);
    let scroll_offset = if anchor >= visible_count {
        anchor - visible_count + 1
    } else {
        0
    };

    let items: Vec<ListItem<'_>> = state
        .candidates
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_count)
        .map(|(i, c)| {
            let style = if Some(i) == state.candidate_index {
                Theme::selected_item()
            } else {
                Theme::normal_item()
            };
            let mut spans = vec![Span::styled(format!("{}/", c.name), style)];
            if c.is_repo {
                spans.push(Span::styled(
                    " (repo)",
                    Style::default().fg(Theme::accent()),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    frame.render_widget(List::new(items), list_inner_area);

    (Vec::new(), None)
}

/// Build a single list item for whatever kind of row this is.
fn bookmark_item<'a>(
    state: &RepoPickerState<'a>,
    visible_index: usize,
    real_idx: usize,
) -> ListItem<'a> {
    let row = &state.rows[real_idx];
    let is_cursor = visible_index == state.list_index;

    let style = if is_cursor {
        Theme::selected_item()
    } else {
        Theme::normal_item()
    };

    match row.kind {
        RepoRowKind::Header => header_item(state, &row.path, style),
        RepoRowKind::Repo { .. } => child_item(state, row, style),
        RepoRowKind::ImportSuggestion => ListItem::new(Line::from(vec![
            Span::styled("⊕ ", Style::default().fg(Theme::accent())),
            Span::styled(
                format!(
                    "import repos from {}",
                    crate::paths::display_path_tilde(&row.path)
                ),
                style,
            ),
        ])),
        RepoRowKind::StartHere => {
            let label = match state.host {
                Some(_) => "start in the host's default dir (no repo)",
                None => "start in ~ (no repo)",
            };
            ListItem::new(Line::from(vec![
                Span::styled("~ ", Style::default().fg(Theme::text_muted())),
                Span::styled(label, style),
            ]))
        }
    }
}

/// Build a parent header row: a collapse glyph + basename + dim `(parent)` marker.
fn header_item<'a>(
    state: &RepoPickerState<'a>,
    path: &std::path::Path,
    style: Style,
) -> ListItem<'a> {
    let glyph = if state.collapsed.contains(path) {
        "▸ "
    } else {
        "▾ "
    };
    ListItem::new(Line::from(vec![
        Span::styled(glyph, style),
        Span::styled(crate::paths::display_path(path), style),
        Span::styled(" (parent)", Style::default().fg(Theme::text_muted())),
    ]))
}

/// Build a (possibly indented) repo row: checkbox + path + optional `[wt]`
/// marker, with the filter query highlighted when one is active.
fn child_item<'a>(state: &RepoPickerState<'a>, row: &RepoRow, style: Style) -> ListItem<'a> {
    let path = &row.path;
    let checked = state.selected.contains(path);
    let is_wt = state.worktree.contains(path);

    let indent = if row.is_child() { "  " } else { "" };
    let check = if checked { "[x] " } else { "[ ] " };
    let display = crate::paths::display_path(path);
    let query = match state.mode {
        RepoInputMode::Filter => state.input,
        RepoInputMode::Path => "",
    };

    let mut spans = vec![Span::styled(indent, style)];
    if query.is_empty() {
        spans.push(Span::styled(format!("{check}{display}"), style));
    } else {
        spans.extend(highlighted_spans(query, check, &display, style));
    }

    if checked && is_wt {
        spans.push(Span::styled(" [wt]", Style::default().fg(Theme::accent())));
    }
    ListItem::new(Line::from(spans))
}

/// Build spans for a bookmark with fuzzy-match positions highlighted in the
/// accent color: the checkbox prefix in the base style, then the shared
/// highlighter over the displayed path.
fn highlighted_spans(query: &str, check: &str, display: &str, style: Style) -> Vec<Span<'static>> {
    let mut result = vec![Span::styled(check.to_string(), style)];
    result.extend(super::fuzzy_highlighted_spans(query, display, style));
    result
}

/// One `key desc` hint pair, styled for the footer.
fn hint(key: &'static str, desc: &'static str) -> [Span<'static>; 2] {
    [
        Span::styled(key, Theme::keybind()),
        Span::styled(desc, Theme::keybind_desc()),
    ]
}

/// Build the footer hint line for the current palette mode.
fn footer_line(state: &RepoPickerState<'_>) -> Line<'static> {
    // Plain Space/Delete act on rows only while the input is empty; once the
    // user types, the chorded variants stay available.
    let pick_key = if state.input.is_empty() {
        "Space"
    } else {
        "^Space"
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    match state.mode {
        RepoInputMode::Path => {
            spans.extend(hint("Tab", " complete  "));
            spans.extend(hint("↑↓", " browse  "));
            spans.extend(hint("Enter", " open / drill in  "));
            spans.extend(hint("Esc", " cancel"));
        }
        RepoInputMode::Filter => {
            if state.picked > 0 {
                spans.push(Span::styled("Enter", Theme::keybind()));
                spans.push(Span::styled(
                    format!(" open {} picked  ", state.picked),
                    Theme::keybind_desc(),
                ));
            } else {
                spans.extend(hint("Enter", " open  "));
            }
            spans.push(Span::styled(pick_key, Theme::keybind()));
            spans.extend([
                Span::styled(" pick  ", Theme::keybind_desc()),
                Span::styled("^T", Theme::keybind()),
                Span::styled(" worktree  ", Theme::keybind_desc()),
            ]);
            if state.input.is_empty() {
                spans.extend(hint("Del", " forget  "));
                spans.extend(hint("^P", " import"));
            } else {
                spans.extend(hint("Esc", " cancel"));
            }
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn accent_chars(spans: &[Span<'_>]) -> String {
        spans
            .iter()
            .filter(|s| s.style.fg == Some(Theme::accent()))
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn highlighted_spans_no_match_keeps_display_in_base_style() {
        let base = Style::default();
        // "zzz" can't be a subsequence of "hello" → no highlights.
        let spans = highlighted_spans("zzz", "[x] ", "hello", base);
        assert_eq!(span_text(&spans), "[x] hello");
        assert!(
            accent_chars(&spans).is_empty(),
            "no character should be accented on a miss"
        );
    }

    #[test]
    fn highlighted_spans_empty_query_highlights_nothing() {
        let base = Style::default();
        let spans = highlighted_spans("", "", "hello", base);
        assert_eq!(span_text(&spans), "hello");
        assert!(accent_chars(&spans).is_empty());
    }

    #[test]
    fn highlighted_spans_accents_matched_characters() {
        let base = Style::default();
        let spans = highlighted_spans("ll", "", "hello", base);
        assert_eq!(span_text(&spans), "hello");
        assert_eq!(accent_chars(&spans), "ll");
    }

    #[test]
    fn highlighted_spans_slices_multibyte_chars_correctly() {
        let base = Style::default();
        // 'é' is two bytes; the position slicing must use char width, not +1,
        // or this would panic / corrupt the text (regression target).
        let spans = highlighted_spans("é", "", "héllo", base);
        assert_eq!(span_text(&spans), "héllo");
        assert_eq!(accent_chars(&spans), "é");
    }

    fn picker_state(
        input: &'static str,
        mode: RepoInputMode,
        picked: usize,
    ) -> RepoPickerState<'static> {
        static EMPTY_ROWS: &[RepoRow] = &[];
        static EMPTY_IDX: &[usize] = &[];
        // Leaked once so the borrows are 'static — fine for a test fixture.
        let selected: &'static HashSet<PathBuf> = Box::leak(Box::new(HashSet::new()));
        let worktree: &'static HashSet<PathBuf> = Box::leak(Box::new(HashSet::new()));
        let collapsed: &'static HashSet<PathBuf> = Box::leak(Box::new(HashSet::new()));
        RepoPickerState {
            rows: EMPTY_ROWS,
            selected,
            worktree,
            collapsed,
            list_index: 0,
            filtered_indices: EMPTY_IDX,
            input,
            input_cursor: input.len(),
            suggestion: None,
            mode,
            candidates: &[],
            candidate_index: None,
            picked,
            host: None,
        }
    }

    #[test]
    fn footer_line_path_mode_shows_tab_completes_only() {
        let s = picker_state("~/co", RepoInputMode::Path, 0);
        let text = span_text(&footer_line(&s).spans);
        assert!(text.contains("Tab complete"));
        assert!(text.contains("open / drill in"));
        assert!(!text.contains("worktree"));
    }

    #[test]
    fn footer_line_empty_input_offers_plain_space_and_del() {
        let s = picker_state("", RepoInputMode::Filter, 0);
        let text = span_text(&footer_line(&s).spans);
        assert!(text.contains("Space pick"));
        assert!(!text.contains("^Space pick"));
        assert!(text.contains("Del forget"));
        assert!(text.contains("^P import"));
    }

    #[test]
    fn footer_line_while_typing_switches_to_chorded_pick() {
        let s = picker_state("fri", RepoInputMode::Filter, 0);
        let text = span_text(&footer_line(&s).spans);
        assert!(text.contains("^Space pick"));
        assert!(!text.contains("Del forget"), "Del edits text while typing");
    }

    #[test]
    fn footer_line_with_picks_counts_them_on_enter() {
        let s = picker_state("", RepoInputMode::Filter, 2);
        let text = span_text(&footer_line(&s).spans);
        assert!(text.contains("open 2 picked"));
    }
}
