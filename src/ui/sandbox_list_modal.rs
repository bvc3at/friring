//! Renderer for the sandbox-profile list (`crate::app::modals::SandboxListModal`)
//! — `docs/SANDBOX.md` §UI. Modeled on [`super::automations_list_modal`]: `n`
//! new, `Enter` edit, `d` delete, and an empty state that says how to start.
//!
//! Each row names the profile, the backend it will actually run (an `auto`
//! profile shows what the host's ladder resolved to, so the choice is never
//! invisible), how many paths it exposes and how much network it allows.

use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::session::{NetworkMode, SandboxBackendKind};

use super::render_list_modal_frame;
use super::theme::Theme;

/// One profile as the list shows it. Owned by the modal state
/// (`crate::app::modals::SandboxListModal`), which holds nothing else: a
/// second app-side copy of these columns could only drift from this one.
#[derive(Debug, Clone)]
pub struct SandboxProfileRow {
    pub name: String,
    /// The profile's own choice, `auto` included.
    pub backend: SandboxBackendKind,
    /// What [`SandboxBackendKind::Auto`] resolved to on this host, or `None`
    /// for an explicit backend and for an `auto` that has not been probed.
    pub resolved: Option<SandboxBackendKind>,
    pub paths: usize,
    pub network: NetworkMode,
    /// Live place state for a place backend (`running`, `stopped`), rendered
    /// when present. Policy backends never create an instance, so this stays
    /// `None` for them — as it does everywhere until instance tracking lands.
    pub instance: Option<String>,
}

impl SandboxProfileRow {
    /// The row's right-hand summary: `auto → seatbelt · 3 paths · allowlist`.
    pub fn summary(&self) -> String {
        let mut out = self.backend.to_string();
        let auto = matches!(self.backend, SandboxBackendKind::Auto);
        if let Some(resolved) = self.resolved.filter(|_| auto) {
            out.push_str(&format!(" → {resolved}"));
        }
        let unit = if self.paths == 1 { "path" } else { "paths" };
        out.push_str(&format!(" · {} {unit} · {}", self.paths, self.network));
        if let Some(state) = &self.instance {
            out.push_str(&format!(" · {state}"));
        }
        out
    }
}

pub struct SandboxListState<'a> {
    pub entries: &'a [SandboxProfileRow],
    pub selected_index: usize,
}

/// Render the list. Returns the row hitboxes (plus scrollbar geometry) and the
/// footer buttons, so a click edits the row it landed on.
pub fn render_sandbox_list_modal(
    frame: &mut Frame,
    state: &SandboxListState<'_>,
) -> super::ModalRender {
    let empty_footer = Line::from(vec![
        Span::styled("n", Theme::keybind()),
        Span::styled(" new  ", Theme::keybind_desc()),
        Span::styled("Esc", Theme::keybind()),
        Span::styled(" close", Theme::keybind_desc()),
    ]);

    let Some([list_area, footer_area]) = render_list_modal_frame(
        frame,
        70,
        "Sandbox Profiles",
        state.entries.len(),
        Some("No sandbox profiles — press n to create one"),
        Some(empty_footer),
    ) else {
        return ((Vec::new(), None), Vec::new());
    };

    let inner_width = list_area.width as usize;
    let lines: Vec<Line<'_>> = state
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| row_line(entry, i == state.selected_index, inner_width))
        .collect();

    let hits = super::render_selector_rows(frame, list_area, lines, state.selected_index);

    let help = Line::from(vec![
        Span::styled("n", Theme::keybind()),
        Span::styled(" new  ", Theme::keybind_desc()),
        Span::styled("d", Theme::keybind()),
        Span::styled(" delete", Theme::keybind_desc()),
    ]);
    frame.render_widget(Paragraph::new(help), footer_area);
    let buttons = super::render_action_footer(
        frame,
        footer_area,
        (
            "Edit",
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
        "Close",
    );
    (hits, buttons)
}

/// One list row, fitted to `width` display columns.
fn row_line<'a>(entry: &SandboxProfileRow, selected: bool, width: usize) -> Line<'a> {
    let text = super::truncate_ellipsis(&format!(" {} — {} ", entry.name, entry.summary()), width);
    let style = if selected {
        Theme::selected_item()
    } else {
        Style::default().fg(Theme::text_secondary())
    };
    Line::from(Span::styled(text, style))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn row() -> SandboxProfileRow {
        SandboxProfileRow {
            name: "dev".to_string(),
            backend: SandboxBackendKind::Auto,
            resolved: None,
            paths: 2,
            network: NetworkMode::Allowlist,
            instance: None,
        }
    }

    #[test]
    fn summary_names_backend_paths_and_network() {
        assert_eq!(row().summary(), "auto · 2 paths · allowlist");
    }

    #[test]
    fn summary_shows_what_auto_resolved_to() {
        let mut r = row();
        r.resolved = Some(SandboxBackendKind::Seatbelt);
        assert_eq!(r.summary(), "auto → seatbelt · 2 paths · allowlist");
        // An explicit backend has nothing to resolve, so a stale probe result
        // must not turn into a contradictory arrow.
        r.backend = SandboxBackendKind::Bwrap;
        assert_eq!(r.summary(), "bwrap · 2 paths · allowlist");
    }

    #[test]
    fn summary_counts_one_path_in_the_singular() {
        let mut r = row();
        r.paths = 1;
        assert!(r.summary().contains("1 path ·"), "{}", r.summary());
        r.paths = 0;
        assert!(r.summary().contains("0 paths ·"), "{}", r.summary());
    }

    #[test]
    fn summary_appends_instance_state_when_present() {
        let mut r = row();
        r.backend = SandboxBackendKind::Docker;
        r.instance = Some("running".to_string());
        assert_eq!(r.summary(), "docker · 2 paths · allowlist · running");
    }

    #[test]
    fn row_line_fits_the_available_width() {
        let mut r = row();
        r.name = "a-very-long-profile-name".to_string();
        let line = row_line(&r, true, 20);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.chars().count() <= 20, "{text:?}");
        assert!(text.ends_with('…'));
    }

    fn draw(width: u16, height: u16, entries: &[SandboxProfileRow], selected: usize) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let render = render_sandbox_list_modal(
                    f,
                    &SandboxListState {
                        entries,
                        selected_index: selected,
                    },
                );
                let ((hits, _), _) = &render;
                for h in hits {
                    assert!(h.rect.right() <= Rect::new(0, 0, width, height).right());
                }
            })
            .unwrap();
    }

    #[test]
    fn renders_without_panicking_at_small_sizes() {
        let entries = vec![row(), row(), row()];
        for (w, h) in [(1, 1), (4, 3), (20, 5), (40, 8), (120, 40)] {
            draw(w, h, &entries, 2);
            // The empty state takes a different path through the frame.
            draw(w, h, &[], 0);
        }
    }
}
