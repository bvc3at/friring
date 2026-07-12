use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::Style,
    text::Line,
    widgets::Paragraph,
    Frame,
};

use super::theme::Theme;
use super::{
    centered_fixed_height_rect, render_filter_row, render_filter_selector_footer,
    render_modal_frame, render_selector_rows, selector_line_filtered,
};

/// One selectable host row: display label + the backend name it maps to
/// (`local-tmux` or `ssh:<host>`).
#[derive(Debug, Clone, Default)]
pub struct HostChoice {
    /// Display label (e.g. `local` or `devbox  (me@devbox)`).
    pub label: String,
    /// Backend name spawned on. Empty string means the local default backend.
    pub backend: String,
}

#[derive(Debug, Clone, Default)]
pub struct HostPickerState {
    pub choices: Vec<HostChoice>,
    /// Selection cursor in the fuzzy filter's *filtered* row space (the two
    /// coincide while no query is typed).
    pub selected_index: usize,
    pub(crate) filter: crate::fuzzy::FuzzyFilter,
}

pub fn render_host_picker_modal(frame: &mut Frame, state: &HostPickerState) -> super::ModalRender {
    let visible = state.filter.visible_indices(state.choices.len());
    let filter_active = state.filter.is_active();
    let height = (visible.len().clamp(1, 15) as u16) + 3 + u16::from(filter_active);
    let area = centered_fixed_height_rect(50, height, frame.area());

    let inner = render_modal_frame(frame, area, "New Session — Host");

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
            state.choices.len(),
        );
    }
    let buttons = render_filter_selector_footer(frame, footer_area, filter_active);

    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "No matching hosts",
                Style::default().fg(Theme::text_muted()),
            )),
            rows_area,
        );
        return ((Vec::new(), None), buttons);
    }

    let lines: Vec<Line<'_>> = visible
        .iter()
        .enumerate()
        .map(|(pos, &i)| {
            selector_line_filtered(
                &state.choices[i].label,
                state.filter.query(),
                pos == state.selected_index,
            )
        })
        .collect();

    let hits = render_selector_rows(frame, rows_area, lines, state.selected_index);
    (hits, buttons)
}
