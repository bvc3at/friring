use ratatui::{
    layout::{Constraint, Direction, Layout},
    Frame,
};

use super::centered_fixed_height_rect;
use super::render_modal_frame;
use super::{render_selector_footer, render_selector_rows, selector_line};

pub struct SyncBasePickerState<'a> {
    /// Repo the choice applies to (rendered in the title).
    pub repo_name: &'a str,
    pub remotes: &'a [String],
    pub selected_index: usize,
}

pub fn render_sync_base_picker_modal(
    frame: &mut Frame,
    state: &SyncBasePickerState<'_>,
) -> super::ModalRender {
    let height = (state.remotes.len().clamp(1, 15) + 4) as u16;
    let area = centered_fixed_height_rect(50, height, frame.area());

    let inner = render_modal_frame(frame, area, &format!("Sync Base — {}", state.repo_name));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let buttons = render_selector_footer(frame, chunks[1]);

    let lines = state
        .remotes
        .iter()
        .enumerate()
        .map(|(i, remote)| selector_line(remote, i == state.selected_index))
        .collect();

    let hits = render_selector_rows(frame, chunks[0], lines, state.selected_index);
    (hits, buttons)
}
