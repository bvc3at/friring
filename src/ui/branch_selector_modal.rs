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
use super::{render_selector_footer, render_selector_rows, selector_line};

pub struct BranchSelectorState<'a> {
    pub branches: &'a [String],
    pub selected_index: usize,
    /// The list is still being read off-thread (ADR-P12): render a muted
    /// placeholder row with no clickable hitboxes.
    pub loading: bool,
}

pub fn render_branch_selector_modal(
    frame: &mut Frame,
    state: &BranchSelectorState<'_>,
) -> super::ModalRender {
    // One row minimum so the loading placeholder has somewhere to draw.
    let height = (state.branches.len().clamp(1, 15) + 4) as u16;
    let area = centered_fixed_height_rect(50, height, frame.area());

    let inner = render_modal_frame(frame, area, "Base Branch");

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let buttons = render_selector_footer(frame, chunks[1]);

    if state.loading {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "Loading branches…",
                Style::default().fg(Theme::text_muted()),
            )),
            chunks[0],
        );
        return ((Vec::new(), None), buttons);
    }

    // render_selector_rows windows these around the selection (with a scrollbar)
    // when there are more than the 15-line cap fit.
    let lines: Vec<Line<'_>> = state
        .branches
        .iter()
        .enumerate()
        .map(|(i, branch)| selector_line(branch, i == state.selected_index))
        .collect();

    let hits = render_selector_rows(frame, chunks[0], lines, state.selected_index);
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
        let state = BranchSelectorState {
            branches: &branches,
            selected_index: 2,
            loading: false,
        };
        assert_eq!(state.selected_index, 2);
        assert_eq!(state.branches.len(), 3);
    }
}
