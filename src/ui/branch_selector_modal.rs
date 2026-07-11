use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::Style,
    text::Line,
    widgets::Paragraph,
    Frame,
};

use super::centered_fixed_height_rect;
use super::render_modal_frame;
use super::theme::Theme;
use super::{render_filter_row, render_filter_selector_footer, render_selector_rows};

pub struct BranchSelectorState<'a> {
    pub branches: &'a [String],
    /// Selection cursor in the fuzzy filter's *filtered* row space (the two
    /// coincide while no query is typed).
    pub selected_index: usize,
    pub(crate) filter: &'a crate::fuzzy::FuzzyFilter,
    /// The list is still being read off-thread (ADR-P12): render a muted
    /// placeholder row with no clickable hitboxes.
    pub loading: bool,
}

pub fn render_branch_selector_modal(
    frame: &mut Frame,
    state: &BranchSelectorState<'_>,
) -> super::ModalRender {
    let visible = state.filter.visible_indices(state.branches.len());
    let filter_active = state.filter.is_active();
    // One row minimum so the loading/no-match placeholder has somewhere to
    // draw; +1 for the query row while a filter is typed.
    let height = (visible.len().clamp(1, 15) + 4) as u16 + u16::from(filter_active);
    let area = centered_fixed_height_rect(50, height, frame.area());

    let inner = render_modal_frame(frame, area, "Base Branch");

    let mut constraints = Vec::new();
    if filter_active {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Min(1));
    constraints.push(Constraint::Length(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    let (filter_area, rows_area, footer_area) = if filter_active {
        (Some(chunks[0]), chunks[1], chunks[2])
    } else {
        (None, chunks[0], chunks[1])
    };

    if let Some(fa) = filter_area {
        render_filter_row(
            frame,
            fa,
            state.filter.query(),
            visible.len(),
            state.branches.len(),
        );
    }
    let buttons = render_filter_selector_footer(frame, footer_area, filter_active);

    let placeholder = if state.loading {
        Some("Loading branches…")
    } else if visible.is_empty() {
        Some("No matching branches")
    } else {
        None
    };
    if let Some(msg) = placeholder {
        frame.render_widget(
            Paragraph::new(Line::styled(msg, Style::default().fg(Theme::text_muted()))),
            rows_area,
        );
        return ((Vec::new(), None), buttons);
    }

    // render_selector_rows windows these around the selection (with a scrollbar)
    // when there are more than the 15-line cap fit.
    let lines: Vec<Line<'_>> = visible
        .iter()
        .enumerate()
        .map(|(pos, &i)| {
            super::selector_line_filtered(
                &state.branches[i],
                state.filter.query(),
                pos == state.selected_index,
            )
        })
        .collect();

    let hits = render_selector_rows(frame, rows_area, lines, state.selected_index);
    (hits, buttons)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The modal caps at 15 visible branches plus 4 lines of chrome
    /// (2 borders + 1 title line padding + 1 footer); an empty list still
    /// reserves one row for the loading placeholder.
    #[test]
    fn modal_height_caps_at_15_branches() {
        let height = |n: usize| (n.clamp(1, 15) + 4) as u16;
        assert_eq!(height(1), 5);
        assert_eq!(height(15), 19);
        assert_eq!(height(30), 19);
        assert_eq!(height(0), 5);
    }

    #[test]
    fn branch_selector_state_holds_index() {
        let branches = vec!["main".to_string(), "dev".to_string(), "feature".to_string()];
        let filter = crate::fuzzy::FuzzyFilter::default();
        let state = BranchSelectorState {
            branches: &branches,
            selected_index: 2,
            filter: &filter,
            loading: false,
        };
        assert_eq!(state.selected_index, 2);
        assert_eq!(state.branches.len(), 3);
    }
}
