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

pub struct WorktreeNameState<'a> {
    pub name: &'a str,
    pub cursor: usize,
    pub base_branch: &'a str,
    /// One muted line of accumulated wizard choices (see the session-name
    /// modal's breadcrumb).
    pub breadcrumb: Option<&'a str>,
}

pub fn render_worktree_name_modal(
    frame: &mut Frame,
    state: &WorktreeNameState<'_>,
) -> super::ModalButtons {
    let crumb_height = u16::from(state.breadcrumb.is_some());
    let area = centered_fixed_height_rect(50, 8 + crumb_height, frame.area());

    let title = format!("New Session — Branch (from {})", state.base_branch);
    let inner = render_modal_frame(frame, area, &title);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(crumb_height),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(inner);

    if let Some(crumb) = state.breadcrumb {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" {crumb}"),
                Style::default().fg(Theme::text_muted()),
            )),
            chunks[0],
        );
    }

    super::render_text_field(
        frame,
        chunks[1],
        "Branch Name",
        state.name,
        state.cursor,
        true,
    );

    super::render_action_footer(
        frame,
        chunks[2],
        (
            "Confirm",
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
        "Cancel",
    )
}
