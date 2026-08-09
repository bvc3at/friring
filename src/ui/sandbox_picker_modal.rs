//! The new-session wizard's sandbox step (`docs/SANDBOX.md` §UI).
//!
//! A filtered selector over the stored profiles, shaped like the host picker
//! because it answers the same kind of question one step later: the host picker
//! chooses *where* the agent runs, this chooses *how much of it* the agent can
//! reach. Runs after directory selection so a profile that does not cover the
//! chosen directories can say so rather than being silently offered.

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::theme::Theme;
use super::{
    centered_fixed_height_rect, render_filter_row, render_filter_selector_footer,
    render_modal_frame, render_selector_rows, selector_line_filtered,
};

/// One selectable row: a stored profile, or the "no sandbox" row the step
/// always offers first (the step is skippable, and skipping is the default).
#[derive(Debug, Clone, Default)]
pub struct SandboxChoice {
    /// Display label — the profile name, or `none` for the unsandboxed row.
    pub label: String,
    /// The profile's name; empty for the unsandboxed row.
    pub profile: String,
    /// Backend and scope, e.g. `auto → seatbelt · 2 paths · allowlist`.
    pub detail: String,
    /// Whether the profile's paths cover every directory the session spans.
    /// A profile that does not is still offered — the user may know better
    /// than the wizard — but it is labelled, because launching an agent whose
    /// workspace it cannot read looks like a broken agent, not a narrow
    /// profile.
    pub covers: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SandboxPickerState {
    pub choices: Vec<SandboxChoice>,
    /// Selection cursor in the fuzzy filter's *filtered* row space (the two
    /// coincide while no query is typed).
    pub selected_index: usize,
    pub(crate) filter: crate::fuzzy::FuzzyFilter,
}

pub fn render_sandbox_picker_modal(
    frame: &mut Frame,
    state: &SandboxPickerState,
) -> super::ModalRender {
    let visible = state.filter.visible_indices(state.choices.len());
    let filter_active = state.filter.is_active();
    let height = (visible.len().clamp(1, 15) as u16) + 3 + u16::from(filter_active);
    let area = centered_fixed_height_rect(64, height, frame.area());

    let inner = render_modal_frame(frame, area, "New Session — Sandbox");

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
                "No matching sandbox profiles",
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
            choice_line(
                &state.choices[i],
                state.filter.query(),
                pos == state.selected_index,
            )
        })
        .collect();

    let hits = render_selector_rows(frame, rows_area, lines, state.selected_index);
    (hits, buttons)
}

/// The label with its fuzzy-match highlighting, followed by the muted detail
/// and — for a profile that does not cover the chosen directories — the reason
/// it is worth a second look.
fn choice_line<'a>(choice: &'a SandboxChoice, query: &str, selected: bool) -> Line<'a> {
    let mut line = selector_line_filtered(&choice.label, query, selected);
    if !choice.detail.is_empty() {
        line.spans.push(Span::styled(
            format!("  {}", choice.detail),
            Style::default().fg(Theme::text_muted()),
        ));
    }
    if !choice.covers {
        line.spans.push(Span::styled(
            "  does not cover the selected directories",
            Style::default().fg(Theme::tool_disallowed()),
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn choices() -> Vec<SandboxChoice> {
        vec![
            SandboxChoice {
                label: "none".into(),
                profile: String::new(),
                detail: "run on the host, no boundary".into(),
                covers: true,
            },
            SandboxChoice {
                label: "dev".into(),
                profile: "dev".into(),
                detail: "auto → seatbelt · 2 paths · allowlist".into(),
                covers: true,
            },
            SandboxChoice {
                label: "docs".into(),
                profile: "docs".into(),
                detail: "bwrap · 1 path · none".into(),
                covers: false,
            },
        ]
    }

    fn rendered(width: u16, height: u16, state: &SandboxPickerState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                render_sandbox_picker_modal(f, state);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_profile_is_offered_and_a_mismatch_says_so() {
        let state = SandboxPickerState {
            choices: choices(),
            selected_index: 0,
            filter: Default::default(),
        };
        let text = rendered(100, 12, &state);
        assert!(text.contains("none"), "{text}");
        assert!(text.contains("dev"), "{text}");
        // A non-covering profile stays selectable, with the warning attached.
        assert!(text.contains("does not cover"), "{text}");
        // The resolved backend is visible without opening the editor.
        assert!(text.contains("seatbelt"), "{text}");
    }

    #[test]
    fn an_empty_filter_result_says_so_instead_of_rendering_nothing() {
        let mut state = SandboxPickerState {
            choices: choices(),
            selected_index: 0,
            filter: Default::default(),
        };
        for c in "zzzz".chars() {
            let labels: Vec<&str> = state.choices.iter().map(|c| c.label.as_str()).collect();
            state.filter.push(c, labels, &mut state.selected_index);
        }
        let text = rendered(100, 12, &state);
        assert!(text.contains("No matching sandbox profiles"), "{text}");
    }

    /// Every modal has to survive a terminal too small to hold it.
    #[test]
    fn renders_at_extreme_sizes() {
        let state = SandboxPickerState {
            choices: choices(),
            selected_index: 2,
            filter: Default::default(),
        };
        for (w, h) in [(1, 1), (10, 3), (40, 8), (120, 40)] {
            let _ = rendered(w, h, &state);
        }
    }
}
