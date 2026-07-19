use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::centered_fixed_height_rect;
use super::render_modal_frame;
use super::theme::Theme;

pub struct SessionNameState<'a> {
    pub name: &'a str,
    pub cursor: usize,
    /// Flow-specific title ("New Session — Name", "Fork — Name", …).
    pub title: &'a str,
    /// One muted line of accumulated wizard choices ("devbox · friring · wt
    /// from main"), so a bare name prompt doesn't appear out of nowhere.
    pub breadcrumb: Option<&'a str>,
    /// The optional workspace-dir field as `(value, cursor)` when shown
    /// (`Ctrl+O` on a multi-repo local flow), `None` when hidden.
    pub workspace_dir: Option<(&'a str, usize)>,
    /// Whether the shown workspace-dir field has focus (else the name field).
    pub workspace_focused: bool,
    /// Whether to advertise the `Ctrl+O` toggle in the footer (multi-repo
    /// local flows only — the field would be inert anywhere else).
    pub offer_workspace_dir: bool,
}

pub fn render_session_name_modal(
    frame: &mut Frame,
    state: &SessionNameState<'_>,
) -> super::ModalButtons {
    let crumb_height = u16::from(state.breadcrumb.is_some());
    let ws_height = if state.workspace_dir.is_some() { 3 } else { 0 };
    let area = centered_fixed_height_rect(50, 8 + crumb_height + ws_height, frame.area());

    let inner = render_modal_frame(frame, area, state.title);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(crumb_height),
            Constraint::Length(3),
            Constraint::Length(ws_height),
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
        "Name",
        state.name,
        state.cursor,
        !state.workspace_focused,
    );

    if let Some((value, cursor)) = state.workspace_dir {
        super::render_text_field(
            frame,
            chunks[2],
            "Workspace dir (name or ~/path)",
            value,
            cursor,
            state.workspace_focused,
        );
    }

    let confirm = (
        "Confirm",
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    );
    if !state.offer_workspace_dir {
        return super::render_action_footer(frame, chunks[3], confirm, "Cancel");
    }
    super::render_hint_action_footer(frame, chunks[3], workspace_hint(state), confirm, "Cancel")
}

/// The `Ctrl+O` workspace-dir hint for the footer: how to reveal the field, or
/// — once shown — how to move between fields and revert to the default.
fn workspace_hint(state: &SessionNameState<'_>) -> Line<'static> {
    if state.workspace_dir.is_some() {
        Line::from(vec![
            Span::styled("Tab", Theme::keybind()),
            Span::styled(" field  ", Theme::keybind_desc()),
            Span::styled("^O", Theme::keybind()),
            Span::styled(" default dir", Theme::keybind_desc()),
        ])
    } else {
        Line::from(vec![
            Span::styled("^O", Theme::keybind()),
            Span::styled(" workspace dir", Theme::keybind_desc()),
        ])
    }
}
