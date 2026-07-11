//! The conversation-import picker (`i` in the session list): a fuzzy-searchable
//! list of on-disk Claude Code conversations plus the working-directory input
//! of the second step. Mirrors the repo picker's shape (search bar → list →
//! text field → hint/button footer); state lives in
//! `crate::app::cc_import::ConversationPickerModal` (crate-private, so no
//! intra-doc link from this public module).

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use super::render_modal_frame;
use super::theme::Theme;
use super::{centered_fixed_height_rect, render_text_field, render_text_field_with_suggestion};
use crate::app::cc_import::{ConversationPickerFocus, ConversationPickerModal};

/// The clickable sub-areas that focus an editable field: the optional search
/// bar and the always-present directory input.
pub struct ConvoFocusAreas {
    pub search: Option<Rect>,
    pub dir: Rect,
}

pub fn render_conversation_picker_modal(
    frame: &mut Frame,
    cp: &ConversationPickerModal,
    now_ms: u64,
) -> (super::ModalRender, ConvoFocusAreas) {
    let search_active =
        cp.focus == ConversationPickerFocus::Search || !cp.search_input.value().is_empty();
    let visible_count = cp.filtered_indices.len().clamp(1, 10);
    let list_height = visible_count as u16 + 2; // +2 for borders

    let search_height: u16 = if search_active { 3 } else { 0 };
    // Layout: search(optional 3) + list + dir input(3) + footer(1) + border(2)
    let total_height = search_height + list_height + 3 + 1 + 2;

    let area = centered_fixed_height_rect(70, total_height, frame.area());
    let inner = render_modal_frame(frame, area, "Import Claude Code Conversation");

    let mut constraints = Vec::new();
    if search_active {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Length(list_height));
    constraints.push(Constraint::Length(3));
    constraints.push(Constraint::Min(1));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let (search_area, list_area, dir_area, footer_area) = if search_active {
        (Some(chunks[0]), chunks[1], chunks[2], chunks[3])
    } else {
        (None, chunks[0], chunks[1], chunks[2])
    };

    if let Some(area) = search_area {
        render_text_field(
            frame,
            area,
            &format!(
                "Search ({}/{})",
                cp.filtered_indices.len(),
                cp.entries.len()
            ),
            cp.search_input.value(),
            cp.search_input.cursor_pos(),
            cp.focus == ConversationPickerFocus::Search,
        );
    }

    let hitboxes = render_conversation_list(frame, list_area, cp, now_ms);

    render_text_field_with_suggestion(
        frame,
        dir_area,
        "Working directory",
        cp.dir_input.value(),
        cp.dir_input.cursor_pos(),
        cp.focus == ConversationPickerFocus::Dir,
        cp.dir_suggestion.as_deref(),
    );

    let buttons = super::render_hint_action_footer(
        frame,
        footer_area,
        footer_line(cp.focus),
        (
            "Resume",
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
        "Cancel",
    );
    (
        (hitboxes, buttons),
        ConvoFocusAreas {
            search: search_area,
            dir: dir_area,
        },
    )
}

/// Render the conversation list (windowed, with fuzzy highlighting and a
/// scrollbar) or its loading/empty placeholder.
fn render_conversation_list(
    frame: &mut Frame,
    list_area: Rect,
    cp: &ConversationPickerModal,
    now_ms: u64,
) -> super::SelectorHits {
    // Keep the cursor row visible through the directory step too — it is the
    // chosen conversation the typed directory applies to.
    let list_focused = cp.focus != ConversationPickerFocus::Search;
    let border_color = if cp.focus == ConversationPickerFocus::List {
        Theme::border_focused()
    } else {
        Theme::border_unfocused()
    };

    let list_block = Block::default()
        .title(format!(" Conversations ({}) ", cp.entries.len()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let list_inner_area = list_block.inner(list_area);
    frame.render_widget(list_block, list_area);

    if cp.filtered_indices.is_empty() {
        let msg = if cp.loading {
            // Not "~/.claude/projects" — the scan honors $CLAUDE_CONFIG_DIR, so
            // a fixed path here would mislead anyone who has overridden it.
            "  Scanning Claude Code conversations…"
        } else if cp.search_input.value().is_empty() {
            "  No importable conversations found"
        } else {
            "  No matches"
        };
        let placeholder = Paragraph::new(Line::from(Span::styled(
            msg,
            Style::default().fg(Theme::text_muted()),
        )));
        frame.render_widget(placeholder, list_inner_area);
        return (Vec::new(), None);
    }

    let total = cp.filtered_indices.len();
    let visible_count = list_inner_area.height as usize;
    let scroll_offset = if cp.list_index >= visible_count {
        cp.list_index - visible_count + 1
    } else {
        0
    };

    let (rows_area, track) = super::scrollbar::reserve_track(list_inner_area, total, visible_count);

    let items: Vec<ListItem<'_>> = cp
        .filtered_indices
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_count)
        .map(|(vi, &real_idx)| conversation_item(cp, vi, real_idx, list_focused, now_ms))
        .collect();
    frame.render_widget(List::new(items), rows_area);

    // One hitbox per visible row, indexed by position in `filtered_indices`
    // (the same space as `list_index`).
    let hitboxes = (scroll_offset..total.min(scroll_offset + visible_count))
        .enumerate()
        .map(|(line, vi)| super::RowHitbox {
            rect: Rect::new(rows_area.x, rows_area.y + line as u16, rows_area.width, 1),
            index: vi,
        })
        .collect();

    let geom = track
        .and_then(|t| super::scrollbar::render_into(frame, t, total, visible_count, cp.list_index));
    (hitboxes, geom)
}

/// One conversation row: the filter/display text (title — directory) with
/// fuzzy-match highlighting, plus a muted last-active age suffix.
fn conversation_item<'a>(
    cp: &ConversationPickerModal,
    visible_index: usize,
    real_idx: usize,
    list_focused: bool,
    now_ms: u64,
) -> ListItem<'a> {
    let entry = &cp.entries[real_idx];
    let is_cursor = visible_index == cp.list_index && list_focused;
    let style = if is_cursor {
        Theme::selected_item()
    } else {
        Theme::normal_item()
    };

    let display = entry.display_text();
    let query = cp.search_input.value();
    let mut spans = if query.is_empty() {
        vec![Span::styled(display, style)]
    } else {
        super::fuzzy_highlighted_spans(query, &display, style)
    };
    spans.push(Span::styled(
        format!("  {}", age_label(now_ms, entry.mtime_ms)),
        Style::default().fg(Theme::text_muted()),
    ));
    ListItem::new(Line::from(spans))
}

/// Compact "last active" age: `now`, `12m`, `3h`, `5d`.
fn age_label(now_ms: u64, mtime_ms: u64) -> String {
    let secs = now_ms.saturating_sub(mtime_ms) / 1000;
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Build the footer hint line for the current focus.
fn footer_line(focus: ConversationPickerFocus) -> Line<'static> {
    match focus {
        ConversationPickerFocus::List => Line::from(vec![
            Span::styled("j/k", Theme::keybind()),
            Span::styled(" nav  ", Theme::keybind_desc()),
            Span::styled("/", Theme::keybind()),
            Span::styled(" search  ", Theme::keybind_desc()),
            Span::styled("Enter", Theme::keybind()),
            Span::styled(" choose dir  ", Theme::keybind_desc()),
            Span::styled("Esc", Theme::keybind()),
            Span::styled(" cancel", Theme::keybind_desc()),
        ]),
        ConversationPickerFocus::Search => Line::from(vec![
            Span::styled("Enter", Theme::keybind()),
            Span::styled(" keep filter  ", Theme::keybind_desc()),
            Span::styled("Esc", Theme::keybind()),
            Span::styled(" clear", Theme::keybind_desc()),
        ]),
        ConversationPickerFocus::Dir => Line::from(vec![
            Span::styled("Tab", Theme::keybind()),
            Span::styled(" complete  ", Theme::keybind_desc()),
            Span::styled("Enter", Theme::keybind()),
            Span::styled(" resume here  ", Theme::keybind_desc()),
            Span::styled("Esc", Theme::keybind()),
            Span::styled(" back to list", Theme::keybind_desc()),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_label_buckets_by_magnitude() {
        let now = 10 * 86_400_000;
        assert_eq!(age_label(now, now - 30_000), "now");
        assert_eq!(age_label(now, now - 12 * 60_000), "12m");
        assert_eq!(age_label(now, now - 3 * 3_600_000), "3h");
        assert_eq!(age_label(now, now - 5 * 86_400_000), "5d");
        // A skewed future mtime saturates to "now" instead of underflowing.
        assert_eq!(age_label(0, 1_000), "now");
    }

    fn span_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn footer_line_matches_focus() {
        let list = span_text(&footer_line(ConversationPickerFocus::List).spans);
        assert!(list.contains("choose dir"));
        let dir = span_text(&footer_line(ConversationPickerFocus::Dir).spans);
        assert!(dir.contains("resume here"));
        assert!(dir.contains("back to list"));
        let search = span_text(&footer_line(ConversationPickerFocus::Search).spans);
        assert!(search.contains("keep filter"));
    }

    #[test]
    fn highlighted_spans_accents_fuzzy_matches() {
        let spans =
            crate::ui::fuzzy_highlighted_spans("fix", "fix the docs — ~/repo", Style::default());
        assert_eq!(span_text(&spans), "fix the docs — ~/repo");
        let accented: String = spans
            .iter()
            .filter(|s| s.style.fg == Some(Theme::accent()))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(accented, "fix");
    }
}
