//! The automation dry-run overlay: what an automation *would* do on its next
//! fire, without firing it.
//!
//! The plan itself is built by [`crate::session::automation::dry_run_plan`], so
//! this overlay and `friring-cli automation dry-run` describe the same thing.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::theme::Theme;
use super::{centered_fixed_height_rect, render_modal_frame};

/// Width of the label column, matching the editor's field labels.
const LABEL_WIDTH: usize = 16;

pub struct AutomationDryRunState<'a> {
    /// The automation's name, for the modal title.
    pub name: &'a str,
    /// Ordered `(label, value)` rows from `dry_run_plan`.
    pub rows: &'a [(String, String)],
}

/// Render the dry-run plan as a centered overlay.
pub fn render_automation_dry_run_modal(frame: &mut Frame, state: &AutomationDryRunState<'_>) {
    let frame_area = frame.area();
    // One row per plan entry, plus the border, the hint line, and its spacer.
    let height = (state.rows.len() as u16).saturating_add(4);
    let area = centered_fixed_height_rect(70, height.min(frame_area.height), frame_area);
    let inner = render_modal_frame(frame, area, &format!("Dry run — {}", state.name));
    frame.render_widget(Paragraph::new(plan_lines(state, inner)), inner);
}

/// The plan rows plus the pinned hint line, windowed to `inner`'s height so a
/// long multi-step plan scrolls off the bottom rather than overflowing.
fn plan_lines<'a>(state: &AutomationDryRunState<'a>, inner: Rect) -> Vec<Line<'a>> {
    let hint = super::key_hint_line(&[("Esc", " close")]);
    let visible = (inner.height as usize).saturating_sub(2);
    let mut lines: Vec<Line> = state
        .rows
        .iter()
        .take(visible)
        .map(|(label, value)| plan_line(label, value))
        .collect();
    lines.push(Line::from(""));
    lines.push(hint);
    lines
}

/// One `label   value` row. An indented label (the per-step wait rows come
/// pre-indented) keeps the step sequence visually nested under its step.
fn plan_line<'a>(label: &str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {label:<LABEL_WIDTH$}"), Theme::label()),
        Span::styled(
            value.to_string(),
            Style::default().fg(Theme::text_primary()),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<(String, String)> {
        vec![
            ("action".to_string(), "spawn".to_string()),
            ("host".to_string(), "devbox".to_string()),
            ("step 1/2".to_string(), "/model opus".to_string()),
        ]
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn plan_lines_render_every_row_plus_the_hint() {
        let rows = rows();
        let state = AutomationDryRunState {
            name: "inbox",
            rows: &rows,
        };
        let lines = plan_lines(&state, Rect::new(0, 0, 60, 20));
        // 3 plan rows + a spacer + the hint.
        assert_eq!(lines.len(), 5);
        assert!(text(&lines[0]).contains("spawn"));
        assert!(text(&lines[2]).contains("/model opus"));
        assert!(text(&lines[4]).contains("close"));
    }

    #[test]
    fn plan_lines_clip_to_the_available_height() {
        let rows = rows();
        let state = AutomationDryRunState {
            name: "inbox",
            rows: &rows,
        };
        // Height 4 leaves room for 2 plan rows; the hint stays pinned.
        let lines = plan_lines(&state, Rect::new(0, 0, 60, 4));
        assert_eq!(lines.len(), 4);
        assert!(text(&lines[3]).contains("close"));
    }
}
