//! Renderer for the sandbox-profile list (`crate::app::modals::SandboxListModal`)
//! — `docs/SANDBOX.md` §UI. Modeled on [`super::automations_list_modal`]: `n`
//! new, `Enter` edit, `d` delete, and an empty state that says how to start.
//!
//! Each row names the profile, the backend it will actually run (an `auto`
//! profile shows what the host's ladder resolved to, so the choice is never
//! invisible), how many paths it exposes, how much network it allows, and —
//! for a place — that every session on it lands in one shared place, plus the
//! places it is actually running right now.
//!
//! It is also the manager view: `s` stops a profile's places, `r` rebuilds
//! them and `p` reclaims the ones nothing needs. Each of the first two takes a
//! container away from whatever is running in it, so it is confirmed first —
//! the question lands in the footer, `y` carries it out and anything else
//! cancels ([`PendingPlaceAction`]).

use ratatui::{
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::session::{NetworkMode, SandboxBackendKind, SandboxShape};

use super::render_list_modal_frame;
use super::theme::Theme;

/// What a row says about a profile whose backend is a **place**.
///
/// A place is created once per profile and shared by every session on it
/// (ADR-26), and the sessions in one are not isolated from each other: one uid,
/// one pid namespace, one filesystem, and therefore each other's egress
/// credential. This row is where a profile is picked for a session — the list,
/// and the new-session wizard's step, which renders the same summary — so
/// "shared" belongs on it rather than only in the editor that spells out what
/// it costs.
const SHARED_PLACE: &str = "shared place";

/// One live place of a profile, as the list shows it.
///
/// A profile may own several at once — a profile edit asks for a *new*
/// container while the sessions already launched keep running in the old one —
/// so this is a list rather than a state word, and the manager's actions act on
/// all of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaceRow {
    /// Which engine holds it.
    pub engine: SandboxBackendKind,
    /// The engine's own handle, as `sandbox_instances` recorded it.
    pub id: String,
    /// Backend-defined lifecycle state, free text (the container backend writes
    /// `running` and nothing else).
    pub state: String,
}

/// What a destructive place action is waiting to be told.
///
/// Lives on the row rather than in a modal of its own because the list holds
/// nothing but its rows, and because a question that belongs to a row must go
/// away with it: rebuilding the list drops the pending action, which fails
/// towards *not* removing a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPlaceAction {
    pub action: PlaceAction,
    /// The profile the answer acts on, carried rather than re-read from the
    /// selection: what is confirmed and what is done have to be the same
    /// profile even if the list is rebuilt between the two.
    pub profile: String,
    /// The question, composed where the counts are known
    /// (`crate::app::sandbox`) so the renderer states no policy of its own.
    pub question: String,
}

/// A manager action against a profile's places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceAction {
    /// Remove every place this profile owns. Whatever is running in them stops.
    Stop,
    /// Remove them and start a fresh one from the profile as it is now.
    Rebuild,
}

impl PlaceAction {
    /// The word the confirmation and the status line use.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::Rebuild => "Rebuild",
        }
    }
}

/// One profile as the list shows it. Owned by the modal state
/// (`crate::app::modals::SandboxListModal`), which holds nothing else: a
/// second app-side copy of these columns could only drift from this one.
#[derive(Debug, Clone, Default)]
pub struct SandboxProfileRow {
    pub name: String,
    /// The profile's own choice, `auto` included.
    pub backend: SandboxBackendKind,
    /// What [`SandboxBackendKind::Auto`] resolved to on this host, or `None`
    /// for an explicit backend and for an `auto` that has not been probed.
    pub resolved: Option<SandboxBackendKind>,
    pub paths: usize,
    pub network: NetworkMode,
    /// Columns of the stored row friring could not decode
    /// (`crate::storage::sandboxes::StoredSandboxProfile`). Empty for a healthy
    /// profile; anything in it means the summary below would be describing
    /// substituted values, so the row reports the damage instead.
    pub undecoded: Vec<String>,
    /// The places this profile owns right now, most recently used first. Policy
    /// backends never create one, so this stays empty for them.
    pub places: Vec<PlaceRow>,
    /// Why the backend this profile would run on is **not available here**, from
    /// the probe. `None` when it is available, and when an `auto` ladder has not
    /// resolved — an unresolved `auto` rules nothing out.
    ///
    /// Rendered wherever a profile is offered, because the alternative is a user
    /// picking `docker` on a machine with no engine and finding out at launch.
    pub unavailable: Option<String>,
    /// A destructive place action this row is waiting to be confirmed. Only the
    /// selected row can carry one, and only until the next keystroke.
    pub pending: Option<PendingPlaceAction>,
}

impl SandboxProfileRow {
    /// The backend this profile would actually run — its own choice, or what
    /// `auto` resolved to on this host. `None` for an `auto` nothing has probed,
    /// which rules nothing in or out.
    fn effective_backend(&self) -> Option<SandboxBackendKind> {
        match self.backend {
            SandboxBackendKind::Auto => self.resolved,
            explicit => Some(explicit),
        }
    }

    /// The row's right-hand summary: `auto → seatbelt · 3 paths · allowlist`.
    ///
    /// A row that did not decode says so instead: its backend, path count and
    /// network mode are partly friring's own substitutions, and printing them
    /// as if they were the profile would be the same lie the refusal to launch
    /// exists to prevent.
    pub fn summary(&self) -> String {
        if !self.undecoded.is_empty() {
            return format!(
                "unreadable {} — will not launch until repaired",
                self.undecoded.join(", ")
            );
        }
        let mut out = self.backend.to_string();
        let auto = matches!(self.backend, SandboxBackendKind::Auto);
        if let Some(resolved) = self.resolved.filter(|_| auto) {
            out.push_str(&format!(" → {resolved}"));
        }
        let unit = if self.paths == 1 { "path" } else { "paths" };
        out.push_str(&format!(" · {} {unit} · {}", self.paths, self.network));
        // Before the live state, because it is a property of the profile rather
        // than of whatever instance happens to be up.
        if self.effective_backend().and_then(SandboxBackendKind::shape) == Some(SandboxShape::Place)
        {
            out.push_str(&format!(" · {SHARED_PLACE}"));
        }
        if let Some(places) = self.places_summary() {
            out.push_str(&format!(" · {places}"));
        }
        // Last, and phrased as the probe phrased it: a profile that cannot run
        // here is still worth offering — the user may be about to install the
        // engine, or may be editing it for another machine — but never worth
        // offering silently.
        if let Some(reason) = &self.unavailable {
            out.push_str(&format!(" · unavailable — {reason}"));
        }
        out
    }

    /// Whether the stored row decoded completely. Drives the row's colour: a
    /// broken profile is a launch failure waiting to happen, not a neutral
    /// entry.
    pub fn is_intact(&self) -> bool {
        self.undecoded.is_empty()
    }

    /// What this profile's places are doing, or `None` when it has none.
    ///
    /// One place reports its state; several report how many there are as well,
    /// because that is the thing worth noticing — a profile edit left a
    /// superseded container behind, and the manager's actions cover all of
    /// them.
    pub fn places_summary(&self) -> Option<String> {
        match self.places.as_slice() {
            [] => None,
            [one] => Some(one.state.clone()),
            many => Some(format!("{} places · {}", many.len(), many[0].state)),
        }
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

    // A pending place action owns the footer, and takes the buttons with it: a
    // click that replayed `Enter` while a container removal was waiting would
    // be a mouse answering a question about somebody's running agent.
    if let Some(pending) = state
        .entries
        .get(state.selected_index)
        .and_then(|entry| entry.pending.as_ref())
    {
        frame.render_widget(
            Paragraph::new(confirm_line(&pending.question, inner_width)),
            footer_area,
        );
        return (hits, Vec::new());
    }

    let help = Line::from(vec![
        Span::styled("n", Theme::keybind()),
        Span::styled(" new  ", Theme::keybind_desc()),
        Span::styled("d", Theme::keybind()),
        Span::styled(" delete  ", Theme::keybind_desc()),
        Span::styled("s", Theme::keybind()),
        Span::styled(" stop  ", Theme::keybind_desc()),
        Span::styled("r", Theme::keybind()),
        Span::styled(" rebuild  ", Theme::keybind_desc()),
        Span::styled("p", Theme::keybind()),
        Span::styled(" prune", Theme::keybind_desc()),
    ]);
    // The hint-aware footer, because the manager's five keys are wide enough to
    // reach the buttons on a narrow terminal.
    let buttons = super::render_hint_action_footer(
        frame,
        footer_area,
        help,
        (
            "Edit",
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
        "Close",
    );
    (hits, buttons)
}

/// The confirmation footer: the question, then the two answers.
///
/// `y` and nothing else confirms. The list's own keys are `Enter` (edit) and
/// `d` (delete), so letting either double as "yes" would carry out a container
/// removal with a keystroke that means something else everywhere in friring —
/// the same reason the firewall's question does not take `Enter`.
fn confirm_line<'a>(question: &str, width: usize) -> Line<'a> {
    const ANSWERS: &str = "  y confirm · any other key cancels";
    let room = width.saturating_sub(ANSWERS.chars().count());
    Line::from(vec![
        Span::styled(
            super::truncate_ellipsis(question, room),
            Style::default().fg(Theme::danger()),
        ),
        Span::styled(ANSWERS, Theme::keybind_desc()),
    ])
}

/// One list row, fitted to `width` display columns.
fn row_line<'a>(entry: &SandboxProfileRow, selected: bool, width: usize) -> Line<'a> {
    let text = super::truncate_ellipsis(&format!(" {} — {} ", entry.name, entry.summary()), width);
    let style = match (selected, entry.is_intact()) {
        (true, _) => Theme::selected_item(),
        (false, true) => Style::default().fg(Theme::text_secondary()),
        // The selection style already owns the whole row's colours, so an
        // unreadable profile can only be coloured while it is *not* selected —
        // its summary says so in words either way.
        (false, false) => Style::default().fg(Theme::danger()),
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
            paths: 2,
            network: NetworkMode::Allowlist,
            ..Default::default()
        }
    }

    fn place(id: &str, state: &str) -> PlaceRow {
        PlaceRow {
            engine: SandboxBackendKind::Docker,
            id: id.to_string(),
            state: state.to_string(),
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
        r.places = vec![place("ctr-1", "running")];
        assert_eq!(
            r.summary(),
            "docker · 2 paths · allowlist · shared place · running"
        );
    }

    /// A profile edit builds a *new* container while the sessions already
    /// launched keep running in the old one, so a profile can own several at
    /// once — and the count is the thing worth noticing, since the manager's
    /// actions cover all of them.
    #[test]
    fn summary_counts_a_profiles_places_when_it_has_more_than_one() {
        let mut r = row();
        r.backend = SandboxBackendKind::Podman;
        r.places = vec![place("ctr-2", "running"), place("ctr-1", "running")];
        assert!(
            r.summary().ends_with("· 2 places · running"),
            "{}",
            r.summary()
        );
        // A policy backend never creates one, and says nothing about places.
        let mut policy = row();
        policy.backend = SandboxBackendKind::Seatbelt;
        assert!(policy.places_summary().is_none());
        assert!(!policy.summary().contains("place"), "{}", policy.summary());
    }

    /// A place is created once per profile and shared by every session that
    /// picks it, and the sessions in one are not isolated from each other. This
    /// row is where a profile is picked — in the list, and in the new-session
    /// wizard, which renders the same summary — so it says which profiles put
    /// two agents in the same box. A policy backend wraps one process per
    /// session and shares nothing, so it says nothing.
    #[test]
    fn summary_marks_a_place_as_shared_and_a_policy_backend_as_neither() {
        let mut r = row();
        for backend in SandboxBackendKind::ALL {
            r.backend = *backend;
            r.resolved = None;
            let place = backend.shape() == Some(SandboxShape::Place);
            assert_eq!(r.summary().contains("shared place"), place, "{backend}");
            // Never the opposite claim, whatever the backend.
            assert!(!r.summary().contains("isolated"), "{backend}");
        }

        // An `auto` profile is marked by what it *resolved* to, because that is
        // what will run — and claims nothing until a host has been probed.
        r.backend = SandboxBackendKind::Auto;
        assert!(!r.summary().contains("shared place"));
        r.resolved = Some(SandboxBackendKind::Seatbelt);
        assert!(!r.summary().contains("shared place"));
        r.resolved = Some(SandboxBackendKind::Podman);
        assert_eq!(
            r.summary(),
            "auto → podman · 2 paths · allowlist · shared place"
        );
    }

    /// A profile whose stored policy did not decode must not be summarised as
    /// if friring had read it: the numbers in that summary would be its own
    /// substitutions.
    #[test]
    fn summary_reports_an_unreadable_row_instead_of_its_values() {
        let mut r = row();
        r.undecoded = vec!["read_scope".to_string(), "network_deny".to_string()];
        let summary = r.summary();
        assert_eq!(
            summary,
            "unreadable read_scope, network_deny — will not launch until repaired"
        );
        assert!(!r.is_intact());
        assert!(!summary.contains("allowlist"), "{summary}");
        assert!(!summary.contains("2 paths"), "{summary}");
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

    fn draw(width: u16, height: u16, entries: &[SandboxProfileRow], selected: usize) -> usize {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut buttons = 0;
        terminal
            .draw(|f| {
                let render = render_sandbox_list_modal(
                    f,
                    &SandboxListState {
                        entries,
                        selected_index: selected,
                    },
                );
                let ((hits, _), footer) = &render;
                buttons = footer.len();
                for h in hits {
                    assert!(h.rect.right() <= Rect::new(0, 0, width, height).right());
                }
            })
            .unwrap();
        buttons
    }

    #[test]
    fn renders_without_panicking_at_small_sizes() {
        let mut pending = row();
        pending.pending = Some(PendingPlaceAction {
            action: PlaceAction::Stop,
            profile: "dev".to_string(),
            question: "Stop 2 place(s) of 'dev'?".to_string(),
        });
        let entries = vec![row(), row(), pending];
        for (w, h) in [(1, 1), (4, 3), (20, 5), (40, 8), (120, 40)] {
            draw(w, h, &entries, 2);
            // The empty state takes a different path through the frame.
            draw(w, h, &[], 0);
        }
    }

    /// A pending removal takes the footer buttons away: a click that replayed
    /// `Enter` would be a mouse answering a question about a running agent's
    /// container.
    #[test]
    fn a_pending_action_replaces_the_footer_and_its_buttons() {
        let entries = vec![row()];
        assert!(
            draw(80, 10, &entries, 0) > 0,
            "the ordinary footer has buttons"
        );

        let mut pending = row();
        pending.pending = Some(PendingPlaceAction {
            action: PlaceAction::Rebuild,
            profile: "dev".to_string(),
            question: "Rebuild 'dev'?".to_string(),
        });
        assert_eq!(draw(80, 10, &[pending], 0), 0);
    }

    /// The question is what the app composed, and the answer keys are spelled
    /// out beside it — `y`, never `Enter`.
    #[test]
    fn the_confirmation_states_the_question_and_the_key_that_answers_it() {
        let line = confirm_line("Stop 2 place(s) of 'dev'?", 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("Stop 2 place(s) of 'dev'?"), "{text}");
        assert!(text.contains("y confirm"), "{text}");
        assert!(!text.contains("Enter"), "{text}");
        // The answers survive a width the question does not.
        let narrow: String = confirm_line("Stop 2 place(s) of 'dev'?", 40)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(narrow.contains("y confirm"), "{narrow}");
    }
}
