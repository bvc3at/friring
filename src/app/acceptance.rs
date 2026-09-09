//! In-process acceptance ("end-to-end") tests for the friring TUI.
//!
//! Where the focused unit tests in [`super::tests`] poke individual methods,
//! these drive a *real* [`App`] the way `main.rs`'s loop does — feeding
//! `update(AppMessage)` events, running the loop's deterministic tick half
//! ([`App::tick_core`] via [`Harness::tick`]; the excluded `tick_background`
//! spawns Tokio tasks that shell out), and rendering `view(Frame)` to a
//! headless ratatui [`TestBackend`]. No TTY, tmux server, or agent process is
//! involved:
//!
//! * sessions are inert [`Session::stub`]s on a no-op [`FakeBackend`],
//! * the database is `Database::open_in_memory()`,
//! * every config/data path is redirected to a throwaway tempdir via
//!   [`crate::paths::TestPathGuard`], so the suite never touches the
//!   developer's real `~/.config/friring`,
//! * agent output is injected per session with [`Harness::feed_output`]
//!   (through the same vt100 parser + `TermSignals` path the PTY reader uses),
//! * wall-clock-gated behavior (timeouts, debounces, the redraw floor) is
//!   fast-forwarded with [`Harness::advance`] (see [`clock`]) — never slept.
//!
//! Stable, deterministic screens (the empty welcome state, the keybindings
//! help overlay, the theme picker) are pinned with `insta` snapshots so a UI
//! change surfaces as a reviewable diff (`cargo insta review` /
//! `INSTA_UPDATE=always cargo test`). Flows whose output depends on live
//! metrics or wall-clock time are asserted on `App` *state* instead (modal
//! kind, selection index, panel visibility, quit flag) to stay robust.
//!
//! Finally, [`monkey_random_events_uphold_invariants`] fuzzes the whole
//! surface: thousands of seeded pseudo-random events (keys, chords, mouse,
//! ticks, clock jumps, resizes, injected output) with [`assert_invariants`]
//! checked after every step — the regression net for "weird TUI behavior"
//! that no directed test anticipated.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::*;
use crate::agent::AgentProvider;

/// Wide layout (≥120 cols) used by the behavioral tests — exercises the full
/// multi-panel TUI the way a real terminal would.
const STD_COLS: u16 = 120;
const STD_ROWS: u16 = 40;

/// Smaller, sessionless size for the pinned snapshot screens, kept compact so
/// the `.snap` files stay readable.
const SNAP_COLS: u16 = 100;
const SNAP_ROWS: u16 = 30;

/// Initialize a git repo at `dir` with one committed file, leaving an
/// uncommitted edit when `dirty`. Used by the hard-delete tests to give a
/// session a worktree whose state `git::worktree_stats` can read.
fn init_git_repo(dir: &Path, dirty: bool) {
    let git = |args: &[&str]| {
        // `git_program` scrubs inherited `GIT_*` vars so this stays hermetic even
        // when the suite runs under the project's pre-commit hook.
        let ok = crate::git::git_program()
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git")
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "friring-test"]);
    std::fs::write(dir.join("f.txt"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    if dirty {
        std::fs::write(dir.join("f.txt"), "changed\n").unwrap();
    }
}

/// Backend stand-in for the harness. Inert by default: `spawn`/`adopt` error,
/// so a test proves no accidental spawn while the session still has a real
/// vt100 parser (the session list draws). With `spawnable = true` they succeed,
/// returning an inert EOF reader + sink writer, so the spawn-dependent App flows
/// (restart, shell pane) run for real — those wire Tokio I/O tasks, so such
/// tests must be `#[tokio::test]`.
struct FakeBackend {
    spawnable: bool,
    /// Bytes every `spawn` returns as the pane's output stream (then EOF).
    /// The seam for exercising the real reader-loop wiring — bytes fed here
    /// travel the same `spawn_blocking` read → scanner → parser path a PTY's
    /// output does, which `feed_output_for_test` bypasses.
    spawn_output: Vec<u8>,
    /// Pushable remote-hook status events, drained by
    /// [`SessionBackend::take_hook_state_events`] — lets a test drive the
    /// remote-session status path without a control-mode connection.
    hook_events: std::sync::Mutex<Vec<(String, String)>>,
    /// What each spawned pane reports as its identity, keyed by pane id, plus
    /// the marker friring stamped on it.
    ///
    /// The seam every ADR-32 identity rule needs: `revalidate_identity` refuses
    /// to act on a pane whose recorded fields do not match, and a test can only
    /// exercise that against a backend that has fields to report.
    panes: std::sync::Mutex<HashMap<String, crate::session::MuxIdentity>>,
    /// How many panes have been spawned, so each gets its own ids.
    spawned: std::sync::atomic::AtomicU32,
    /// Refuse every `kill` — the `stop_failed` path.
    kill_refuses: bool,
    /// What this backend's multiplexer server says about itself (ADR-33).
    ///
    /// `None` by default, which is what every backend that is not a real
    /// multiplexer reports and what makes the socket check a no-op — so a test
    /// that does not care is unaffected, and the one that does can drive both
    /// sides of the comparison from the identity's own `host`.
    server: Option<crate::session::MuxServerIdentity>,
}

impl FakeBackend {
    /// Inert: spawning/adopting fails.
    fn stub() -> Self {
        Self {
            spawnable: false,
            spawn_output: Vec::new(),
            hook_events: std::sync::Mutex::new(Vec::new()),
            panes: std::sync::Mutex::new(HashMap::new()),
            spawned: std::sync::atomic::AtomicU32::new(0),
            kill_refuses: false,
            server: None,
        }
    }

    /// Spawnable: `spawn`/`adopt` succeed with no-op I/O.
    fn spawnable() -> Self {
        Self {
            spawnable: true,
            ..Self::stub()
        }
    }

    /// Spawnable, but every `kill` is refused — the pane friring cannot stop.
    fn unstoppable() -> Self {
        Self {
            kill_refuses: true,
            ..Self::spawnable()
        }
    }

    /// Spawnable, with each spawned pane emitting `output` before EOF.
    fn spawnable_with_output(output: &[u8]) -> Self {
        Self {
            spawn_output: output.to_vec(),
            ..Self::spawnable()
        }
    }

    /// Spawnable, but reporting a server whose socket is **not** the one the
    /// generated policy denies — the multiplexer a policy launch must refuse to
    /// land on (ADR-33).
    fn on_a_socket_the_policy_does_not_name() -> Self {
        Self {
            server: Some(crate::session::MuxServerIdentity {
                host: crate::session::HostMuxSockets {
                    own_socket: std::path::PathBuf::from("/tmp/tmux-501/friring"),
                    outer_socket: None,
                    uid: 501,
                },
                socket_path: Some("/tmp/tmux-501/somebody-else".to_string()),
                pid: Some(9),
                start_time: None,
                marker: Some("uuid".to_string()),
            }),
            ..Self::spawnable()
        }
    }

    /// Queue a `(pane_id, state)` event for the next drain.
    fn push_hook_event(&self, pane_id: &str, state: &str) {
        self.hook_events
            .lock()
            .unwrap()
            .push((pane_id.to_string(), state.to_string()));
    }
}

impl SessionBackend for FakeBackend {
    fn name(&self) -> &str {
        "fake"
    }
    fn check_available(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn ensure_ready(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn spawn(
        &self,
        _: &str,
        _: &str,
        _: &[String],
        _: Option<&Path>,
        _: &std::collections::HashMap<String, String>,
        _: u16,
        _: u16,
    ) -> anyhow::Result<crate::agent::backend::SpawnedSession> {
        anyhow::ensure!(self.spawnable, "inert fake backend does not spawn");
        let n = self
            .spawned
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let pane = format!("%{n}");
        let identity = crate::session::MuxIdentity {
            server: Some("fake".into()),
            window_id: Some(format!("@{n}")),
            pane_id: Some(pane.clone()),
            pane_pid: Some(40000 + n),
            launch_key: None,
        };
        self.panes
            .lock()
            .unwrap()
            .insert(pane.clone(), identity.clone());
        Ok(crate::agent::backend::SpawnedSession {
            identity,
            backend_id: pane,
            output: Box::new(std::io::Cursor::new(self.spawn_output.clone())),
            input: Box::new(std::io::sink()),
        })
    }

    fn pane_identity(&self, backend_id: &str) -> anyhow::Result<crate::session::MuxIdentity> {
        self.panes
            .lock()
            .unwrap()
            .get(backend_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such pane: {backend_id}"))
    }

    fn server_identity(&self) -> Option<crate::session::MuxServerIdentity> {
        self.server.clone()
    }

    fn set_pane_marker(&self, backend_id: &str, value: &str) -> anyhow::Result<()> {
        let mut panes = self.panes.lock().unwrap();
        let pane = panes
            .get_mut(backend_id)
            .ok_or_else(|| anyhow::anyhow!("no such pane: {backend_id}"))?;
        pane.launch_key = Some(value.to_string());
        Ok(())
    }
    fn adopt(
        &self,
        _: &str,
        _: u16,
        _: u16,
        _: Option<Vec<u8>>,
    ) -> anyhow::Result<crate::agent::backend::AdoptedSession> {
        anyhow::ensure!(self.spawnable, "inert fake backend does not adopt");
        Ok(crate::agent::backend::AdoptedSession {
            output: Box::new(std::io::empty()),
            input: Box::new(std::io::sink()),
        })
    }
    fn discover(&self) -> anyhow::Result<Vec<crate::agent::backend::DiscoveredSession>> {
        Ok(vec![])
    }
    fn resize(&self, _: &str, _: u16, _: u16) -> anyhow::Result<()> {
        Ok(())
    }
    fn is_dead(&self, _: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn kill(&self, backend_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!self.kill_refuses, "fake backend refuses to kill a pane");
        self.panes.lock().unwrap().remove(backend_id);
        Ok(())
    }
    fn detach(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn pane_pid(&self, _: &str) -> anyhow::Result<Option<u32>> {
        Ok(None)
    }
    fn take_hook_state_events(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.hook_events.lock().unwrap())
    }
}

/// A driveable TUI under test: a real [`App`] paired with a headless terminal,
/// plus the tempdir + path guard that keep it hermetic for the harness's life.
struct Harness {
    app: App,
    terminal: Terminal<TestBackend>,
    // Held for their `Drop` side effects (restore XDG paths / delete tempdir);
    // ordering matters — the guard resets path resolution before the dir goes.
    _guard: crate::paths::TestPathGuard,
    _tmp: tempfile::TempDir,
}

impl Harness {
    /// Build an `App` of `cols`×`rows` seeded with `session_count` stub
    /// sessions on the inert [`FakeBackend`].
    fn new(cols: u16, rows: u16, session_count: usize) -> Self {
        Self::with_backend(cols, rows, session_count, Arc::new(FakeBackend::stub()))
    }

    /// As [`Harness::new`], but on a caller-supplied backend — the seam that
    /// lets spawn-dependent flows run against a spawnable [`FakeBackend`].
    fn with_backend(
        cols: u16,
        rows: u16,
        session_count: usize,
        backend: Arc<dyn SessionBackend>,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let guard = crate::paths::TestPathGuard::new(tmp.path());

        let provider: Arc<dyn AgentProvider> = Arc::new(GenericProvider::new(
            crate::agent::agent_config::builtin_registry()
                .default_agent()
                .unwrap()
                .clone(),
        ));

        let mut app = App::new(
            rows,
            cols,
            BackendRegistry::new(Arc::clone(&backend)),
            crate::agent::agent_config::builtin_registry(),
            Database::open_in_memory().unwrap(),
        );
        // Acceptance tests must never hand copied pane/status text to the host
        // clipboard. Capturing here keeps every key and mouse flow hermetic.
        app.captured_clipboard = Some(Vec::new());
        for i in 0..session_count {
            app.sessions
                .push(Session::stub(&format!("session-{i}"), &backend, &provider));
        }
        if session_count > 0 {
            app.active_index = 0;
        }

        let terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        Self {
            app,
            terminal,
            _guard: guard,
            _tmp: tmp,
        }
    }

    /// Standard wide harness ([`STD_COLS`]×[`STD_ROWS`]) seeded with
    /// `session_count` stub sessions — the default for behavioral tests.
    fn standard(session_count: usize) -> Self {
        Self::new(STD_COLS, STD_ROWS, session_count)
    }

    /// Snapshot-sized, sessionless harness for the pinned-screen tests.
    /// Keybindings are pinned to the non-macOS defaults: snapshots are
    /// recorded once and checked on every platform, and the macOS-appended
    /// Cmd alternates would otherwise fork the rendered help overlay per-OS.
    fn snapshot() -> Self {
        let mut h = Self::new(SNAP_COLS, SNAP_ROWS, 0);
        h.app.keybindings = crate::session::KeyBindings::defaults_for(false);
        h
    }

    /// Wide harness on a spawnable [`FakeBackend`], with each session given a
    /// resumable `agent_session_id` so spawn-dependent flows (restart) aren't
    /// no-ops. Must be driven from a `#[tokio::test]`: the spawn path wires up
    /// Tokio I/O tasks and needs a runtime.
    fn spawnable(session_count: usize) -> Self {
        let mut h = Self::with_backend(
            STD_COLS,
            STD_ROWS,
            session_count,
            Arc::new(FakeBackend::spawnable()),
        );
        for (i, session) in h.app.sessions.iter_mut().enumerate() {
            session.info.agent_session_id = Some(format!("agent-{i}"));
        }
        h
    }

    /// Point the active session at a freshly-created git repo (clean, or
    /// `dirty` with one uncommitted change) so a `soft_delete`-off delete sees —
    /// or doesn't see — work at risk. Returns the backing `TempDir`, which the
    /// caller must keep alive for the repo to exist on disk.
    fn set_active_git_cwd(&mut self, dirty: bool) -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path(), dirty);
        let idx = self.app.active_index;
        self.app.sessions[idx].info.cwd = Some(repo.path().to_path_buf());
        repo
    }

    /// Feed one key event, exactly as the real event loop converts a crossterm
    /// `KeyPress` into an [`AppMessage`].
    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> &mut Self {
        self.app.update(AppMessage::KeyPress(code, mods));
        self
    }

    /// A `Ctrl+<c>` chord (the form most global friring bindings take).
    fn ctrl(&mut self, c: char) -> &mut Self {
        self.key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// Arm the leader (`Ctrl+F` by default) and press `code` after it.
    fn leader(&mut self, code: KeyCode) -> &mut Self {
        self.ctrl('f');
        self.key(code, KeyModifiers::NONE)
    }

    /// A bare function key (`F1`…`F5`).
    fn func(&mut self, n: u8) -> &mut Self {
        self.key(KeyCode::F(n), KeyModifiers::NONE)
    }

    /// An `Alt+<c>` chord (the narrow Alt exception: `Alt+A`, `Alt+U`, `Alt+L`).
    fn alt(&mut self, c: char) -> &mut Self {
        self.key(KeyCode::Char(c), KeyModifiers::ALT)
    }

    /// A `Shift+<letter>` chord (e.g. session reordering). Terminals deliver
    /// these as an uppercase char; `KeyChord::normalized` canonicalizes the
    /// encoding, so the uppercase-char + SHIFT form resolves the same binding.
    fn shift(&mut self, c: char) -> &mut Self {
        self.key(KeyCode::Char(c.to_ascii_uppercase()), KeyModifiers::SHIFT)
    }

    /// A bare `Shift` press, as kitty-protocol terminals report it — one tap
    /// of the double-`Shift` search gesture.
    fn shift_tap(&mut self) -> &mut Self {
        self.key(
            KeyCode::Modifier(crossterm::event::ModifierKeyCode::LeftShift),
            KeyModifiers::SHIFT,
        )
    }

    /// Draw the current state to the headless backend and return the visible
    /// glyphs as newline-separated rows (one string per terminal line), the
    /// shape both `insta` snapshots and substring assertions read.
    fn render(&mut self) -> String {
        let app = &mut self.app;
        self.terminal.draw(|f| app.view(f)).unwrap();
        let buffer = self.terminal.backend().buffer();
        let area = *buffer.area();
        let mut out = String::new();
        for y in 0..area.height {
            let mut line = String::new();
            for x in 0..area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            // Drop trailing blanks so snapshots aren't a wall of spaces.
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// Click the open Settings panel's row for `field`. Renders first so this
    /// frame's `ModalField` hitboxes exist, locates the one carrying `field`'s
    /// `ORDER` index, and dispatches a click at its left edge.
    fn click_settings_field(&mut self, field: modals::SettingsField) -> &mut Self {
        self.render();
        let index = modals::SettingsField::ORDER
            .iter()
            .position(|f| *f == field)
            .expect("field in ORDER");
        let rect = self
            .app
            .click_targets
            .iter()
            .find_map(|t| match t.action {
                ClickAction::ModalField(i) if i == index => Some(t.rect),
                _ => None,
            })
            .expect("settings field hitbox recorded");
        self.app.update(AppMessage::MouseClick {
            x: rect.x + 1,
            y: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        self
    }

    /// Run one deterministic tick — the third step of `main.rs`'s loop, minus
    /// its background half ([`App::tick_core`]; the excluded
    /// `tick_background` spawns Tokio tasks that shell out / hit the network).
    /// This drives everything tick-dependent hermetically: status derivation,
    /// timer expiry, the global-search debounce, automation firing, and the
    /// external-change poll.
    fn tick(&mut self) -> &mut Self {
        self.app.tick_core();
        self
    }

    /// Tick until the bridge's housekeeping pass has run once more.
    ///
    /// Driven off the pass's own counter rather than off the tick arithmetic,
    /// so the two cadence constants stay free to change.
    fn tick_until_bridge_housekeeping(&mut self) -> &mut Self {
        let before = self.app.perf_counters().bridge_response_sweeps;
        for _ in 0..200 {
            self.tick();
            if self.app.perf_counters().bridge_response_sweeps != before {
                return self;
            }
        }
        panic!("no bridge housekeeping pass inside 200 ticks");
    }

    /// Fast-forward the app's clock by `d` (see [`clock`]). Timers, debounces
    /// and retry windows age deterministically — the next [`Self::tick`] (or
    /// `should_redraw` check) observes the elapsed time without real waiting.
    fn advance(&mut self, d: std::time::Duration) -> &mut Self {
        clock::advance(d);
        self
    }

    /// Feed raw agent-output bytes to session `idx`, exactly as its PTY reader
    /// loop would — the seam for testing everything downstream of output:
    /// terminal rendering, the output-change redraw detector, OSC
    /// title/bell/notification signals, and buffer-content search.
    fn feed_output(&mut self, idx: usize, bytes: &[u8]) -> &mut Self {
        self.app.sessions[idx].feed_output_for_test(bytes);
        self
    }

    /// Resize both the app (as the real loop would on a terminal resize) and
    /// the headless backend, so subsequent renders draw at the new size.
    fn resize(&mut self, cols: u16, rows: u16) -> &mut Self {
        self.app.update(AppMessage::Resize(cols, rows));
        self.terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap();
        self
    }

    /// Render, then click the central-pane tab strip's cell for `tab` (returns
    /// false when no such tab was rendered, e.g. its feature is off).
    fn click_central_tab(&mut self, tab: CentralTab) -> bool {
        self.render();
        let rect = self.app.click_targets.iter().find_map(|t| match t.action {
            ClickAction::CentralTab(found) if found == tab => Some(t.rect),
            _ => None,
        });
        let Some(rect) = rect else {
            return false;
        };
        self.app.update(AppMessage::MouseClick {
            x: rect.x + 1,
            y: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        true
    }
}

// ── Snapshot tests: stable, deterministic screens ────────────────────────────

#[test]
fn empty_welcome_screen_renders() {
    let mut h = Harness::snapshot();
    insta::assert_snapshot!(h.render());
}

#[test]
fn help_overlay_lists_keybindings() {
    let mut h = Harness::snapshot();
    h.func(1); // F1 → ToggleHelp
    assert!(
        matches!(h.app.modal, modals::Modal::Help(_)),
        "F1 should open the help modal"
    );
    insta::assert_snapshot!(h.render());
}

#[test]
fn theme_picker_lists_palettes() {
    let mut h = Harness::snapshot();
    h.ctrl('y'); // Ctrl+Y → OpenThemePicker
    assert!(
        matches!(h.app.modal, modals::Modal::ThemePicker(_)),
        "Ctrl+Y should open the theme picker"
    );
    insta::assert_snapshot!(h.render());
}

#[test]
fn theme_picker_filtered_shows_both_sections() {
    // A query spanning dark and light themes: pins the `Dark`/`Light` section
    // headers, the narrowed match count, and the echoed query.
    let mut h = Harness::snapshot();
    h.ctrl('y');
    h.key(KeyCode::Char('/'), KeyModifiers::NONE); // open the filter sub-mode
    for c in "light".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    insta::assert_snapshot!(h.render());
}

// ── Behavioral tests: drive keys, assert on App state ────────────────────────

#[test]
fn ctrl_n_opens_repo_picker() {
    let mut h = Harness::standard(0);
    h.render();
    h.ctrl('n'); // Ctrl+N → NewSession
    assert!(
        matches!(h.app.modal, modals::Modal::RepoPicker(_)),
        "Ctrl+N should open the repo picker (no hosts configured)"
    );
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(!h.app.modal.is_open(), "Esc should dismiss the modal");
}

#[test]
fn session_name_modal_ctrl_o_workspace_dir_field() {
    let mut h = Harness::standard(0);
    // Name step of a *single-repo* worktree flow: the workspace-dir field is
    // not offered, so Ctrl+O is inert.
    h.app.new_session.base_branch = Some("main".into());
    h.app.new_session.repo_path = Some(std::path::PathBuf::from("/r/a"));
    let mut modal = modals::SessionNameModal::default();
    modal.name.set("demo");
    h.app.modal = modals::Modal::SessionName(modal);
    h.ctrl('o');
    let modals::Modal::SessionName(sn) = &h.app.modal else {
        panic!("name modal gone");
    };
    assert!(
        sn.workspace_dir.is_none(),
        "single-repo must not offer field"
    );

    // Two repos picked → multi-repo: Ctrl+O reveals + focuses the field.
    h.app.new_session.all_repos = Some(vec![
        std::path::PathBuf::from("/r/a"),
        std::path::PathBuf::from("/r/b"),
    ]);
    h.ctrl('o');
    let modals::Modal::SessionName(sn) = &h.app.modal else {
        panic!("name modal gone");
    };
    assert!(sn.workspace_dir.is_some() && sn.workspace_focused);

    // A bare name confirms into `<workspaces root>/<name>` and the flow
    // advances to the branch-name step.
    for c in "acme".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(h.app.modal, modals::Modal::WorktreeName(_)));
    let ws = h.app.new_session.workspace_dir.clone().expect("dir stored");
    assert_eq!(
        ws,
        crate::paths::workspaces_directory().unwrap().join("acme")
    );

    // Esc back to the name step re-opens the field prefilled with the choice.
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    let modals::Modal::SessionName(sn) = &h.app.modal else {
        panic!("name modal gone");
    };
    let field = sn.workspace_dir.as_ref().expect("field prefilled");
    assert!(field.value().ends_with("acme"), "got {}", field.value());

    // Ctrl+O again hides the field and reverts to the default workspace.
    h.ctrl('o');
    let modals::Modal::SessionName(sn) = &h.app.modal else {
        panic!("name modal gone");
    };
    assert!(sn.workspace_dir.is_none());
    assert!(h.app.new_session.workspace_dir.is_none());
}

#[test]
fn session_name_modal_rejects_occupied_workspace_dir() {
    let mut h = Harness::standard(0);
    h.app.new_session.base_branch = Some("main".into());
    h.app.new_session.repo_path = Some(std::path::PathBuf::from("/r/a"));
    h.app.new_session.all_repos = Some(vec![
        std::path::PathBuf::from("/r/a"),
        std::path::PathBuf::from("/r/b"),
    ]);
    let mut modal = modals::SessionNameModal::default();
    modal.name.set("demo");
    h.app.modal = modals::Modal::SessionName(modal);

    // A target holding real (non-symlink) content is refused: the modal stays
    // open with an error toast, and nothing is recorded.
    let busy = crate::paths::workspaces_directory().unwrap().join("busy");
    std::fs::create_dir_all(busy.join("real-content")).unwrap();
    h.ctrl('o');
    for c in "busy".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(h.app.modal, modals::Modal::SessionName(_)));
    assert_eq!(
        h.app.status_message.as_ref().map(|m| m.level),
        Some(StatusLevel::Error)
    );
    assert!(h.app.new_session.workspace_dir.is_none());
}

#[test]
fn ctrl_j_and_k_cycle_session_selection() {
    let mut h = Harness::standard(3);
    assert_eq!(h.app.active_index, 0);

    h.ctrl('j'); // NextSession
    assert_eq!(h.app.active_index, 1, "Ctrl+J moves to the next session");
    h.ctrl('j');
    assert_eq!(h.app.active_index, 2);

    h.ctrl('k'); // PreviousSession
    assert_eq!(h.app.active_index, 1, "Ctrl+K moves back up");
}

#[test]
fn ctrl_w_toggles_tasks_panel() {
    let mut h = Harness::standard(0);
    assert!(!h.app.show_tasks_panel);

    h.ctrl('w'); // FocusTasks
    assert!(h.app.show_tasks_panel, "Ctrl+W reveals the tasks panel");
    h.ctrl('w');
    assert!(!h.app.show_tasks_panel, "Ctrl+W again hides it");
}

#[test]
fn f5_toggles_tasks_panel_like_ctrl_w() {
    // F5 is the documented alternate chord for FocusTasks (Ctrl+W); both must
    // drive the same toggle.
    let mut h = Harness::standard(0);
    assert!(!h.app.show_tasks_panel);

    h.func(5);
    assert!(h.app.show_tasks_panel, "F5 reveals the tasks panel");
    h.func(5);
    assert!(!h.app.show_tasks_panel, "F5 again hides it");
}

#[test]
fn alt_l_collapses_the_session_list_and_widens_the_terminal() {
    use ratatui::layout::Rect;
    let screen = Rect::new(0, 0, STD_COLS, STD_ROWS);
    let mut h = Harness::standard(1);
    assert!(h.app.show_session_list, "the list is shown by default");
    assert!(h.app.layout_for(screen).left_panel.is_some());
    let shown_width = h.app.layout_for(screen).terminal.width;

    h.alt('l');
    assert!(!h.app.show_session_list, "Alt+L collapses the list");
    let hidden = h.app.layout_for(screen);
    assert!(
        hidden.left_panel.is_none(),
        "no left column while collapsed"
    );
    assert!(
        hidden.automations_panel.is_none(),
        "the automations pane shares the column"
    );
    assert!(
        hidden.terminal.width > shown_width,
        "the terminal reclaims the column's width"
    );

    // The rendered screen loses the ` Sessions ` panel border.
    let shown_screen = Harness::standard(1).render();
    let hidden_screen = h.render();
    let border = shown_screen
        .lines()
        .find(|row| row.contains("Sessions"))
        .expect("the Sessions panel renders when shown");
    assert!(!hidden_screen.contains(border.trim()));

    h.alt('l');
    assert!(h.app.show_session_list, "Alt+L again restores it");
    assert!(h.app.layout_for(screen).left_panel.is_some());
}

#[test]
fn leader_shift_l_toggles_the_session_list() {
    // The leader route is what makes the toggle reachable under
    // `mode = "prefix-only"` (where direct global chords are disabled) and on
    // a terminal with no option-as-alt.
    let mut h = Harness::standard(1);
    h.app.prefix_settings.mode = crate::session::PrefixMode::PrefixOnly;
    h.ctrl('f'); // arm the leader
    h.shift('l');
    assert!(!h.app.show_session_list);
    h.ctrl('f');
    h.shift('l');
    assert!(h.app.show_session_list);
}

#[test]
fn collapsing_moves_focus_off_the_left_column() {
    let mut h = Harness::standard(1);
    h.app.focus = InputFocus::SessionList;

    h.alt('l');
    assert_eq!(
        h.app.focus,
        InputFocus::Terminal,
        "focus retreats off the unrendered column"
    );

    // Restoring is purely a visibility toggle — it does not steal focus back.
    h.alt('l');
    assert_eq!(h.app.focus, InputFocus::Terminal);

    // The automations pane shares the column, so it retreats with it.
    h.app.focus = InputFocus::Automations;
    h.alt('l');
    assert_eq!(h.app.focus, InputFocus::Terminal);
}

#[test]
fn closing_a_pane_while_collapsed_falls_back_to_the_terminal() {
    // Every focus-drop site routes through `focus_fallback`, which must not
    // hand focus to the session list while it is collapsed.
    let mut h = Harness::standard(1);
    h.alt('l');

    h.func(5); // FocusTasks — showing the panel focuses it
    assert_eq!(h.app.focus, InputFocus::TaskList);
    h.func(5); // and hiding it drops that focus
    assert_eq!(
        h.app.focus,
        InputFocus::Terminal,
        "never onto the collapsed list"
    );
}

#[test]
fn searching_to_an_automation_brings_the_collapsed_column_back() {
    // The automations pane lives in the left column, and a global-search jump
    // is the only route into it that does not start from a rendered row — so
    // it has to restore the column rather than focus an invisible pane.
    let mut h = Harness::standard(1);
    let aid = h
        .app
        .db
        .create_automation(&crate::storage::automations::NewAutomation {
            name: "widget-nightly".into(),
            enabled: true,
            schedule: crate::session::AutomationSchedule::Once { at: 0 },
            timezone: None,
            action: crate::session::AutomationAction::send_to(SessionId::default()),
            prompt: "go".into(),
            next_run_at: None,
            prompt_steps: Vec::new(),
        })
        .unwrap();
    h.app.refresh_automations();

    h.alt('l');
    h.ctrl('/'); // GlobalSearch
    h.key(KeyCode::Tab, KeyModifiers::NONE); // widen past the session switcher
    for c in "widget-nightly".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    let selected = &h.app.global_search.results[h.app.global_search.selected];
    assert_eq!(
        selected.target,
        search::SearchTarget::Automation { id: aid }
    );

    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        h.app.show_session_list,
        "the jump restores the column it needs"
    );
    assert_eq!(h.app.focus, InputFocus::Automations);
}

#[test]
fn collapsed_list_is_skipped_by_the_focus_ring() {
    let mut h = Harness::standard(1);
    h.alt('l');
    h.app.focus = InputFocus::Terminal;

    h.ctrl('l'); // FocusForward
    assert_eq!(
        h.app.focus,
        InputFocus::Terminal,
        "the ring has no session-list stop while collapsed"
    );
    h.ctrl('h'); // FocusBackward
    assert_eq!(h.app.focus, InputFocus::Terminal);
}

#[test]
fn alt_l_toggles_from_a_focused_terminal() {
    // Not a bare Ctrl+<letter>, so it is not deferred to the PTY: the toggle
    // works from the pane it exists to widen.
    let mut h = Harness::standard(1);
    h.app.focus = InputFocus::Terminal;

    h.alt('l');
    assert!(!h.app.show_session_list);
    assert_eq!(h.app.focus, InputFocus::Terminal);
}

#[test]
fn alt_l_escapes_the_central_pane_capture_views() {
    // The review and activity views capture nearly every key; both list
    // `ToggleSessionList` among the chords that pass through, so the toggle
    // works from them without closing the view.
    let mut h = Harness::standard(1);
    open_review(&mut h, 3);

    h.alt('l');
    assert!(!h.app.show_session_list, "Alt+L reaches the column");
    assert_eq!(
        h.app.focus,
        InputFocus::CodeReview,
        "the review keeps focus"
    );

    let mut h = Harness::standard(1);
    h.app.sessions[0].info.cc_activity = Some(crate::session::CcActivity {
        workflows: Vec::new(),
        subagents: vec![crate::session::CcAgent {
            agent_id: "s1".into(),
            transcript_path: std::path::PathBuf::from("agent-s1.jsonl"),
            agent_type: "Explore".into(),
            description: None,
            label: None,
            phase_title: None,
            state: crate::session::CcAgentState::Done,
            mtime_ns: 0,
            size: 0,
            tokens: None,
            tool_calls: None,
            last_tool: None,
            model: None,
        }],
    });
    h.func(9); // ToggleCcActivity
    assert_eq!(h.app.focus, InputFocus::CcActivityTree);

    h.alt('l');
    assert!(!h.app.show_session_list, "Alt+L reaches the column");
    assert_eq!(
        h.app.focus,
        InputFocus::CcActivityTree,
        "the activity view keeps focus"
    );
    assert!(h.app.active_cc_activity().is_some(), "and stays open");
}

#[test]
fn collapsing_docks_an_inline_info_pane_in_its_own_column() {
    // Fork-specific: `auto`/`inline` put the info pane in the left column, so
    // collapsing it must fall the pane back to its dedicated column rather than
    // making F2 a dead key.
    use ratatui::layout::Rect;
    let screen = Rect::new(0, 0, STD_COLS, STD_ROWS);
    let mut h = Harness::standard(1);
    h.app.info_panel_position = crate::session::settings::InfoPanelPosition::Inline;
    h.func(2); // ToggleInfoPanel
    assert!(h.app.show_info_panel);
    let inline = h.app.layout_for(screen);
    assert_eq!(
        inline.info_panel.expect("inlined").x,
        inline.left_panel.expect("left column").x,
        "docked in the left column"
    );

    h.alt('l');
    assert!(h.app.show_info_panel, "the panel survives the collapse");
    let collapsed = h.app.layout_for(screen);
    let info = collapsed.info_panel.expect("moved to its own column");
    assert!(collapsed.left_panel.is_none());
    assert!(info.x < collapsed.terminal.x, "info column, then terminal");
}

#[test]
fn collapsing_hides_an_info_pane_with_nowhere_left_to_dock() {
    // Below `three_panel_min_cols` the dedicated column does not exist, so the
    // pane genuinely cannot render. It is turned off with a note rather than
    // left "shown" and invisible.
    use ratatui::layout::Rect;
    let screen = Rect::new(0, 0, 100, STD_ROWS);
    let mut h = Harness::new(100, STD_ROWS, 1);
    h.app.info_panel_position = crate::session::settings::InfoPanelPosition::Inline;
    h.func(2);
    assert!(h.app.layout_for(screen).info_panel.is_some());

    h.alt('l');
    assert!(!h.app.show_info_panel, "not left stranded");
    assert!(h.app.layout_for(screen).info_panel.is_none());
    let msg = h.app.status_message.as_ref().expect("a note was shown");
    assert!(msg.text.contains("Info panel"), "got: {}", msg.text);

    // And F2 says why instead of flipping a flag that changes nothing.
    h.func(2);
    assert!(!h.app.show_info_panel, "F2 is refused, not a silent no-op");
    let msg = h.app.status_message.as_ref().expect("a note was shown");
    assert!(msg.text.contains("Info panel"), "got: {}", msg.text);
}

#[test]
fn expand_chevron_shows_only_while_collapsed_and_restores_the_list() {
    let mut h = Harness::standard(1);
    h.render();
    let chevron = |h: &Harness| {
        h.app.click_targets.iter().find_map(|t| match t.action {
            ClickAction::Global(crate::session::Action::ToggleSessionList) => Some(t.rect),
            _ => None,
        })
    };
    assert!(
        chevron(&h).is_none(),
        "no chevron while the list is shown — the tab strip keeps those cells"
    );

    h.alt('l');
    let screen = h.render();
    assert!(screen.contains('▶'), "the expand chevron appears");
    let rect = chevron(&h).expect("chevron recorded as a click target");
    h.app.update(AppMessage::MouseClick {
        x: rect.x + 1,
        y: rect.y,
        modifiers: KeyModifiers::NONE,
    });
    assert!(h.app.show_session_list, "clicking it brings the list back");
    assert!(!h.render().contains('▶'));
}

#[test]
fn ctrl_slash_opens_global_search_popup() {
    let mut h = Harness::standard(2);
    assert!(!h.app.global_search.active);

    h.ctrl('/'); // GlobalSearch
    assert!(h.app.global_search.active, "Ctrl+/ opens the search popup");

    // The popup captures typing before global keybindings, so a plain letter
    // edits the query rather than triggering a binding.
    h.key(KeyCode::Char('s'), KeyModifiers::NONE);
    assert_eq!(h.app.global_search.query.value(), "s");

    // Esc restores the prior state.
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(!h.app.global_search.active, "Esc closes the search popup");
}

#[test]
fn double_shift_opens_global_search() {
    let mut h = Harness::standard(1);

    h.shift_tap();
    assert!(
        !h.app.global_search.active,
        "a single Shift tap only arms the gesture"
    );
    h.shift_tap();
    assert!(
        h.app.global_search.active,
        "the second tap within the window opens the search"
    );
}

#[test]
fn double_shift_times_out_and_rearms() {
    let mut h = Harness::standard(1);

    h.shift_tap();
    h.advance(std::time::Duration::from_millis(
        key_handlers::DOUBLE_SHIFT_WINDOW_MS + 10,
    ));
    h.shift_tap();
    assert!(
        !h.app.global_search.active,
        "a tap after the window expired must not trigger — it re-arms instead"
    );
    h.shift_tap();
    assert!(h.app.global_search.active, "…so the next quick tap opens");
}

#[test]
fn double_shift_is_broken_by_an_intervening_key() {
    let mut h = Harness::standard(1);

    // Shift → letter → Shift is ordinary typing (e.g. a capital, a pause,
    // another capital) — never a gesture.
    h.shift_tap();
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    h.shift_tap();
    assert!(!h.app.global_search.active);
}

#[test]
fn double_shift_ignored_while_search_or_modal_owns_input() {
    let mut h = Harness::standard(1);

    // While the popup is open, Shift presses are just capitals being typed.
    h.ctrl('/');
    h.shift_tap().shift_tap();
    assert!(h.app.global_search.active, "popup stays open");
    assert_eq!(
        h.app.global_search.query.value(),
        "",
        "bare modifier presses never reach the query"
    );

    // While a modal captures input, the gesture must not fire underneath it.
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    h.ctrl(','); // OpenSettings
    h.shift_tap().shift_tap();
    assert!(!h.app.global_search.active);
    assert!(h.app.modal.is_open(), "the modal is untouched");
}

#[test]
fn double_shift_respects_the_feature_flag() {
    let mut h = Harness::standard(1);
    h.app.features.double_shift_search = false;

    h.shift_tap().shift_tap();
    assert!(!h.app.global_search.active, "flag off ⇒ gesture inert");

    h.ctrl('/');
    assert!(h.app.global_search.active, "the chord keeps working");
}

#[test]
fn settings_panel_opens_and_closes() {
    let mut h = Harness::standard(1);
    h.ctrl(','); // OpenSettings
    assert!(
        matches!(h.app.modal, modals::Modal::Settings(_)),
        "Ctrl+, should open the settings panel"
    );

    // The panel shows section headers, the selected field's description, and
    // the restart marker on restart-required rows.
    let screen = h.render();
    assert!(screen.contains("FEATURES"), "section header renders");
    assert!(
        screen.contains("Tasks panel"),
        "selected field's description renders in the footer"
    );
    assert!(screen.contains('⟳'), "restart marker renders");

    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(!h.app.modal.is_open(), "Esc closes the settings panel");
}

#[test]
fn settings_panel_live_toggle_applies_on_save() {
    let mut h = Harness::standard(1);
    assert!(h.app.features.tasks, "tasks default on");

    h.ctrl(','); // OpenSettings — starts on the `tasks` field
    h.key(KeyCode::Char(' '), KeyModifiers::NONE); // toggle tasks off in the draft
    assert!(h.app.features.tasks, "draft edits don't apply until save");

    h.ctrl('s'); // Save
    assert!(!h.app.modal.is_open(), "save closes the panel");
    assert!(
        !h.app.features.tasks,
        "a live feature flag applies immediately on save"
    );
}

#[test]
fn settings_panel_click_toggles_boolean_field() {
    let mut h = Harness::standard(1);
    assert!(h.app.features.mouse, "mouse on by default");
    assert!(h.app.features.info_panel, "info_panel default on");

    h.ctrl(','); // OpenSettings — opens on the `tasks` field
                 // Click a *different* field than the one focused on open, so the click must
                 // both select the row and toggle its boolean.
    h.click_settings_field(modals::SettingsField::FeatInfoPanel);

    let modals::Modal::Settings(s) = &h.app.modal else {
        panic!("settings panel still open after the click");
    };
    assert_eq!(
        s.field,
        modals::SettingsField::FeatInfoPanel,
        "the click selected the clicked row"
    );
    assert!(
        !s.draft.features.info_panel,
        "the click also toggled the boolean off in the draft"
    );
    assert!(
        h.app.features.info_panel,
        "draft edits don't apply until save"
    );
}

#[test]
fn settings_panel_click_does_not_change_scalar() {
    let mut h = Harness::standard(1);
    h.ctrl(','); // OpenSettings
    h.render();
    let before = match &h.app.modal {
        modals::Modal::Settings(s) => s.draft.scrollback_lines,
        _ => unreachable!(),
    };

    h.click_settings_field(modals::SettingsField::ScrollbackLines);

    let modals::Modal::Settings(s) = &h.app.modal else {
        panic!("settings panel still open");
    };
    assert_eq!(
        s.field,
        modals::SettingsField::ScrollbackLines,
        "the click selected the scalar row"
    );
    assert_eq!(
        s.draft.scrollback_lines, before,
        "a click never steps a scalar value — only selects it"
    );
}

#[test]
fn settings_panel_esc_discards() {
    let mut h = Harness::standard(1);
    assert!(h.app.features.tasks);

    h.ctrl(','); // OpenSettings
    h.key(KeyCode::Char(' '), KeyModifiers::NONE); // toggle in the draft
    h.key(KeyCode::Esc, KeyModifiers::NONE); // discard

    assert!(
        h.app.features.tasks,
        "Esc discards the draft — no live preview applied"
    );
}

#[test]
fn ctrl_q_requests_quit() {
    let mut h = Harness::standard(1);
    assert!(!h.app.should_quit());
    h.ctrl('q'); // QuitApp
    assert!(h.app.should_quit(), "Ctrl+Q should request shutdown");
}

#[test]
fn session_list_renders_seeded_sessions() {
    // Not a snapshot (status dots/metrics drift); assert the names appear.
    let mut h = Harness::standard(2);
    let frame = h.render();
    assert!(
        frame.contains("session-0"),
        "first session name should render"
    );
    assert!(
        frame.contains("session-1"),
        "second session name should render"
    );
}

// ── Side panels: file viewer, info panel ─────────────────────────────────────

#[test]
fn file_viewer_toggles_via_f3_and_ctrl_e() {
    // F3 and Ctrl+E are the two default chords for ToggleFileViewer.
    let mut h = Harness::standard(1);
    assert!(!h.app.show_file_viewer);

    h.func(3);
    assert!(h.app.show_file_viewer, "F3 reveals the file viewer");
    h.func(3);
    assert!(!h.app.show_file_viewer, "F3 again hides it");

    h.ctrl('e');
    assert!(
        h.app.show_file_viewer,
        "Ctrl+E also reveals the file viewer"
    );
    h.ctrl('e');
    assert!(!h.app.show_file_viewer, "Ctrl+E again hides it");
}

#[test]
fn info_panel_toggles_via_f2_and_ctrl_b() {
    let mut h = Harness::standard(1);
    let initial = h.app.show_info_panel;

    h.func(2);
    assert_ne!(h.app.show_info_panel, initial, "F2 toggles the info panel");
    h.ctrl('b');
    assert_eq!(
        h.app.show_info_panel, initial,
        "Ctrl+B toggles it back (same action, alternate chord)"
    );
}

// ── Modals: automations list, restore deleted sessions ───────────────────────

#[test]
fn automations_list_modal_empty() {
    let mut h = Harness::snapshot();
    h.ctrl('p'); // Ctrl+P → OpenAutomations
    assert!(
        matches!(h.app.modal, modals::Modal::AutomationsList(_)),
        "Ctrl+P opens the automations list modal"
    );
    insta::assert_snapshot!(h.render());
}

#[test]
fn restore_sessions_modal_empty() {
    let mut h = Harness::snapshot();
    h.ctrl('u'); // Ctrl+U → OpenRestoreSessions
    assert!(
        matches!(h.app.modal, modals::Modal::RestoreSessions(_)),
        "Ctrl+U opens the restore-deleted-sessions modal"
    );
    insta::assert_snapshot!(h.render());
}

#[test]
fn force_deleted_restore_confirms_then_best_effort_restores() {
    let mut h = Harness::standard(1);

    // Persist the stub session, then force-delete it — the soft-deleted +
    // force-deleted DB row a best-effort recovery acts on (no worktrees → no
    // on-disk teardown needed).
    let id = h.app.sessions[0].info.id;
    let shared = h.app.session_to_shared(&h.app.sessions[0]);
    h.app.db.upsert_session(&shared).unwrap();
    crate::session_ops::delete_session_headless(&h.app.db, id, true).unwrap();
    assert!(
        h.app.db.get_deleted_session_by_id(id).unwrap().is_some(),
        "row is soft-deleted + force-deleted"
    );

    // Ctrl+U lists it; Enter on a force-deleted row opens the confirm prompt
    // rather than restoring immediately.
    h.ctrl('u');
    assert!(matches!(h.app.modal, modals::Modal::RestoreSessions(_)));
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        matches!(h.app.modal, modals::Modal::ConfirmRestore(_)),
        "Enter on a force-deleted row asks for confirmation"
    );
    assert!(
        h.app.db.get_deleted_session_by_id(id).unwrap().is_some(),
        "nothing restored before confirmation"
    );

    // Confirm → `restore_session` clears `deleted_at` + `force_deleted`, so the
    // row leaves the deleted list and is an active session again.
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(!h.app.modal.is_open(), "confirm closes the prompt");
    assert!(
        h.app.db.get_deleted_session_by_id(id).unwrap().is_none(),
        "the session is no longer in the deleted list"
    );
    assert!(
        h.app.db.get_session_by_id(id).unwrap().is_some(),
        "the row is an active session again"
    );
}

// ── Delete + undo ────────────────────────────────────────────────────────────

#[test]
fn ctrl_d_soft_deletes_and_ctrl_z_undoes() {
    let mut h = Harness::standard(2);
    assert_eq!(h.app.sessions.len(), 2);

    h.ctrl('d'); // DeleteSession (soft, with a 10s undo window)
    assert_eq!(
        h.app.sessions.len(),
        1,
        "delete removes the session from the list"
    );
    assert!(
        h.app.pending_delete.is_some(),
        "a pending delete is held for undo"
    );

    h.ctrl('z'); // UndoDelete
    assert_eq!(h.app.sessions.len(), 2, "undo restores the session");
    assert!(
        h.app.pending_delete.is_none(),
        "the undo consumes the pending delete"
    );
}

#[tokio::test]
async fn restore_picker_consumes_the_matching_pending_delete() {
    let mut h = Harness::spawnable(1);
    h.app.save_state();
    let id = h.app.sessions[0].info.id;

    h.alt('u'); // keep the deleted session as a ghost in the undo slot
    h.ctrl('d');
    assert!(h.app.pending_delete.is_some());
    assert!(h.app.sessions.is_empty());

    h.ctrl('u');
    assert!(matches!(h.app.modal, modals::Modal::RestoreSessions(_)));
    h.key(KeyCode::Enter, KeyModifiers::NONE);

    assert!(
        h.app.pending_delete.is_none(),
        "restoring the same row must consume its in-memory undo entry"
    );
    assert_eq!(
        h.app.sessions.iter().filter(|s| s.info.id == id).count(),
        1,
        "restore keeps exactly one instance of the session identity"
    );
    assert!(h.app.sessions[0].is_ghost(), "the pending ghost is reused");

    h.ctrl('z');
    assert_eq!(h.app.sessions.len(), 1, "undo is now a no-op");
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        !h.app.sessions[0].is_ghost(),
        "the restored ghost still loads"
    );
    assert_eq!(h.app.sessions.len(), 1);
}

#[test]
fn undo_does_not_duplicate_a_session_restored_by_another_instance() {
    let mut h = Harness::standard(1);
    h.app.save_state();
    let id = h.app.sessions[0].info.id;
    h.ctrl('d');

    let backend: Arc<dyn SessionBackend> = Arc::new(FakeBackend::stub());
    let provider: Arc<dyn AgentProvider> = Arc::new(GenericProvider::new(
        crate::agent::agent_config::builtin_registry()
            .default_agent()
            .unwrap()
            .clone(),
    ));
    let mut externally_restored = Session::stub("restored-elsewhere", &backend, &provider);
    externally_restored.info.id = id;
    h.app.sessions.push(externally_restored);

    h.ctrl('z');

    assert!(h.app.pending_delete.is_none());
    assert_eq!(
        h.app.sessions.iter().filter(|s| s.info.id == id).count(),
        1,
        "an external restore wins without duplicating SessionId"
    );
}

#[tokio::test]
async fn stale_restore_picker_does_not_duplicate_an_external_restore() {
    let mut h = Harness::spawnable(1);
    h.app.save_state();
    let id = h.app.sessions[0].info.id;
    h.ctrl('d');
    h.app.finalize_pending_delete();

    h.ctrl('u');
    assert!(matches!(h.app.modal, modals::Modal::RestoreSessions(_)));

    // Another instance restores the row after this modal captured its list,
    // and the normal state sync makes that session visible here.
    h.app.db.restore_session(id).unwrap();
    let backend: Arc<dyn SessionBackend> = Arc::new(FakeBackend::spawnable());
    let provider: Arc<dyn AgentProvider> = Arc::new(GenericProvider::new(
        crate::agent::agent_config::builtin_registry()
            .default_agent()
            .unwrap()
            .clone(),
    ));
    let mut externally_restored = Session::stub("restored-elsewhere", &backend, &provider);
    externally_restored.info.id = id;
    h.app.sessions.push(externally_restored);

    h.key(KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(
        h.app.sessions.iter().filter(|s| s.info.id == id).count(),
        1,
        "a stale restore selection must converge on the existing identity"
    );
}

#[test]
fn ctrl_d_hard_delete_confirms_when_soft_delete_disabled() {
    let mut h = Harness::standard(2);
    h.app.features.soft_delete = false;
    // The active session has uncommitted work, so a hard delete must confirm.
    let _repo = h.set_active_git_cwd(true);

    // Ctrl+D now opens a confirmation prompt instead of deleting immediately.
    h.ctrl('d');
    assert!(
        matches!(h.app.modal, modals::Modal::ConfirmDelete(_)),
        "Ctrl+D opens the hard-delete confirmation when soft_delete is off"
    );
    assert_eq!(
        h.app.sessions.len(),
        2,
        "nothing is deleted before confirmation"
    );

    // Esc cancels, leaving the session untouched.
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(!h.app.modal.is_open(), "Esc closes the confirmation");
    assert_eq!(h.app.sessions.len(), 2, "cancel leaves the session intact");

    // Re-open and confirm with Enter → the session is torn down, no undo.
    h.ctrl('d');
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(!h.app.modal.is_open(), "confirm closes the confirmation");
    assert_eq!(h.app.sessions.len(), 1, "confirm removes the session");
    assert!(
        h.app.pending_delete.is_none(),
        "a hard delete offers no Ctrl+Z undo"
    );
}

#[test]
fn hard_delete_confirmation_accepts_y_and_n_keys() {
    let mut h = Harness::standard(2);
    h.app.features.soft_delete = false;
    let _repo = h.set_active_git_cwd(true);

    // 'n' cancels, like Esc.
    h.ctrl('d');
    h.key(KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(!h.app.modal.is_open(), "'n' closes the confirmation");
    assert_eq!(h.app.sessions.len(), 2, "'n' cancels the delete");

    // 'y' confirms, like Enter.
    h.ctrl('d');
    h.key(KeyCode::Char('y'), KeyModifiers::NONE);
    assert!(!h.app.modal.is_open(), "'y' closes the confirmation");
    assert_eq!(h.app.sessions.len(), 1, "'y' confirms the delete");
}

#[test]
fn ctrl_d_hard_deletes_clean_session_without_confirmation() {
    let mut h = Harness::standard(2);
    h.app.features.soft_delete = false;
    // A clean git worktree has no work at risk → delete straight away.
    let _repo = h.set_active_git_cwd(false);

    h.ctrl('d');
    assert!(
        !h.app.modal.is_open(),
        "a clean session is hard-deleted without a confirmation prompt"
    );
    assert_eq!(h.app.sessions.len(), 1, "the clean session is removed");
    assert!(
        h.app.pending_delete.is_none(),
        "a hard delete offers no Ctrl+Z undo"
    );
}

#[test]
fn ctrl_d_confirms_dirty_session_and_lists_risk() {
    let mut h = Harness::standard(2);
    h.app.features.soft_delete = false;
    let _repo = h.set_active_git_cwd(true);

    h.ctrl('d');
    let modals::Modal::ConfirmDelete(ref cd) = h.app.modal else {
        panic!("a dirty session opens the hard-delete confirmation");
    };
    assert!(
        cd.risk.dirty && cd.risk.files_changed > 0,
        "the risk reflects the uncommitted change: {:?}",
        cd.risk
    );
    assert!(!cd.risk.unknown, "a local git worktree is inspectable");
}

// ── Pane focus cycling ───────────────────────────────────────────────────────

#[test]
fn focus_cycles_between_session_list_and_terminal() {
    // With no side panels shown, the session ring is [SessionList, Terminal].
    let mut h = Harness::standard(1);
    assert!(
        matches!(h.app.focus, InputFocus::SessionList),
        "focus starts on the session list"
    );

    h.ctrl('l'); // FocusForward
    assert!(
        matches!(h.app.focus, InputFocus::Terminal),
        "Ctrl+L moves to the terminal"
    );
    h.ctrl('l');
    assert!(
        matches!(h.app.focus, InputFocus::SessionList),
        "Ctrl+L wraps back to the session list"
    );
    h.ctrl('h'); // FocusBackward
    assert!(
        matches!(h.app.focus, InputFocus::Terminal),
        "Ctrl+H steps backward to the terminal"
    );
}

#[test]
fn focus_ring_includes_file_viewer_when_shown() {
    let mut h = Harness::standard(1);
    h.func(3); // show the file viewer
    assert!(h.app.show_file_viewer);

    // Cycling forward from the session list must reach the file viewer.
    let mut saw_file_viewer = false;
    for _ in 0..4 {
        h.ctrl('l');
        if matches!(h.app.focus, InputFocus::FileViewer) {
            saw_file_viewer = true;
            break;
        }
    }
    assert!(
        saw_file_viewer,
        "the focus ring visits the file viewer while it is shown"
    );
}

// ── Code review: focusable changed-files pane ────────────────────────────────

/// Open a synthetic review with `n` files on the active session and focus the
/// diff pane, without needing a real git worktree.
fn open_review(h: &mut Harness, n: usize) {
    let sid = h.app.active_session_id().unwrap();
    h.app
        .code_reviews
        .insert(sid, super::code_review::CodeReviewState::for_test(sid, n));
    h.app.focus = InputFocus::CodeReview;
}

#[test]
fn review_files_pane_joins_focus_ring_and_replaces_file_viewer() {
    let mut h = Harness::standard(1);
    h.func(3); // show the file viewer too — the review must still take the column
    open_review(&mut h, 3);

    // Cycling forward from the diff reaches the changed-files pane, never the
    // plain file viewer while a review owns the column.
    let mut saw_review_files = false;
    for _ in 0..4 {
        h.ctrl('l');
        assert!(
            !matches!(h.app.focus, InputFocus::FileViewer),
            "the file viewer is not a ring stop while a review is open"
        );
        if matches!(h.app.focus, InputFocus::ReviewFiles) {
            saw_review_files = true;
            break;
        }
    }
    assert!(
        saw_review_files,
        "the focus ring visits the changed-files pane"
    );
}

#[test]
fn review_files_pane_navigates_and_opens_into_diff() {
    let mut h = Harness::standard(1);
    open_review(&mut h, 3);
    h.app.focus = InputFocus::ReviewFiles;

    // The diff starts on the first file.
    assert_eq!(h.app.active_review().unwrap().current_file(), Some(0));

    // `j` walks to the next file (the diff follows).
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    assert_eq!(h.app.active_review().unwrap().current_file(), Some(1));
    h.key(KeyCode::Char('k'), KeyModifiers::NONE);
    assert_eq!(h.app.active_review().unwrap().current_file(), Some(0));

    // `G` jumps to the last file.
    h.key(KeyCode::Char('G'), KeyModifiers::SHIFT);
    assert_eq!(h.app.active_review().unwrap().current_file(), Some(2));

    // `r` marks the current file reviewed.
    h.key(KeyCode::Char('r'), KeyModifiers::NONE);
    assert!(!h.app.active_review().unwrap().reviewed_files.is_empty());

    // `Enter` drops focus into the diff at the selected file.
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(h.app.focus, InputFocus::CodeReview));
}

// ── CC activity view: workflow/subagent transcript ───────────────────────────

#[test]
fn cc_activity_view_opens_navigates_folds_and_closes() {
    use crate::session::{CcActivity, CcAgent, CcAgentState, CcRunStatus, CcWorkflow};
    let tmp = tempfile::tempdir().unwrap();
    // A real transcript file so selecting the agent loads blocks.
    let transcript = tmp.path().join("agent-a1.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"user\",\"message\":{\"content\":\"do the thing\"}}\n\
         {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"on it\"}]}}\n",
    )
    .unwrap();
    let agent = |id: &str, path: std::path::PathBuf, state| CcAgent {
        agent_id: id.into(),
        transcript_path: path,
        agent_type: "workflow-subagent".into(),
        description: None,
        label: None,
        phase_title: None,
        state,
        mtime_ns: 0,
        size: 0,
        tokens: None,
        tool_calls: None,
        last_tool: None,
        model: None,
    };
    let activity = CcActivity {
        workflows: vec![CcWorkflow {
            run_id: "wf_x".into(),
            name: Some("demo".into()),
            dir: tmp.path().to_path_buf(),
            status: CcRunStatus::Running,
            phases: Vec::new(),
            agents: vec![
                agent("a1", transcript.clone(), CcAgentState::Active),
                agent("a2", tmp.path().join("missing.jsonl"), CcAgentState::Done),
            ],
            summary: None,
            tempo: None,
            needs: None,
        }],
        subagents: vec![CcAgent {
            agent_type: "Explore".into(),
            ..agent("s1", tmp.path().join("agent-s1.jsonl"), CcAgentState::Done)
        }],
    };

    let mut h = Harness::spawnable(1);
    h.app.sessions[0].info.cc_activity = Some(activity);

    // F9 opens the view, focused on the navigator: 6 section rows, then the
    // agents subtree (workflow header + 2 agents + 1 standalone subagent).
    h.key(KeyCode::F(9), KeyModifiers::NONE);
    assert_eq!(h.app.focus, InputFocus::CcActivityTree);
    assert_eq!(h.app.active_cc_activity().unwrap().tree.len(), 10);
    // The Overview section auto-previews on open.
    assert_eq!(
        h.app.active_cc_activity().unwrap().open,
        Some(super::cc_activity::CcNodeRef::Section(
            super::activity::Section::Overview
        ))
    );

    // `6` jumps to the Agents section; the next two rows are the workflow
    // header and its first agent, whose transcript auto-previews; `Enter`
    // drops into it to read (prompt + assistant text = 2 rows).
    h.key(KeyCode::Char('6'), KeyModifiers::NONE);
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(h.app.focus, InputFocus::CcActivity);
    assert_eq!(h.app.active_cc_activity().unwrap().rows.len(), 2);

    // `/` opens find; typing filters to matching rows and jumps the selection.
    // The transcript is [Prompt("do the thing"), Text("on it")].
    h.key(KeyCode::Char('/'), KeyModifiers::NONE);
    assert!(
        h.app
            .active_cc_activity()
            .unwrap()
            .search
            .as_ref()
            .unwrap()
            .editing
    );
    for c in "thing".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    {
        let ca = h.app.active_cc_activity().unwrap();
        assert_eq!(ca.search.as_ref().unwrap().matches, vec![0]);
        assert_eq!(ca.selected, 0, "selection jumped to the first match");
    }
    // Tab commits (keeps the highlight bar); Esc then clears it without closing.
    h.key(KeyCode::Tab, KeyModifiers::NONE);
    assert!(
        !h.app
            .active_cc_activity()
            .unwrap()
            .search
            .as_ref()
            .unwrap()
            .editing
    );
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(h.app.active_cc_activity().unwrap().search.is_none());
    assert!(
        h.app.active_cc_activity().is_some(),
        "clearing the search keeps the view open"
    );

    // `h` steps back to the navigator; folding the workflow hides its agents.
    h.key(KeyCode::Char('h'), KeyModifiers::NONE);
    assert_eq!(h.app.focus, InputFocus::CcActivityTree);
    h.key(KeyCode::Char('6'), KeyModifiers::NONE);
    h.key(KeyCode::Char('j'), KeyModifiers::NONE); // onto the workflow header
    h.key(KeyCode::Char(' '), KeyModifiers::NONE);
    assert_eq!(
        h.app.active_cc_activity().unwrap().tree.len(),
        8,
        "a folded workflow hides its 2 agents (sections + header + standalone remain)"
    );
    // Space on the Agents section folds the whole subtree to sections only.
    h.key(KeyCode::Char('6'), KeyModifiers::NONE);
    h.key(KeyCode::Char(' '), KeyModifiers::NONE);
    assert_eq!(h.app.active_cc_activity().unwrap().tree.len(), 6);

    // Esc closes and returns focus to the terminal.
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(h.app.active_cc_activity().is_none());
    assert_eq!(h.app.focus, InputFocus::Terminal);
}

#[test]
fn disabling_activity_live_closes_the_view_and_rescues_focus() {
    let mut h = Harness::standard(1);
    h.func(9);
    assert!(h.app.active_cc_activity().is_some());
    assert!(matches!(h.app.focus, InputFocus::CcActivityTree));

    let mut settings = crate::session::settings::Settings::default();
    settings.features.cc_activity = false;
    h.app.apply_live_settings(&settings);

    assert!(h.app.cc_activities.is_empty());
    assert_eq!(h.app.focus, InputFocus::Terminal);
}

#[test]
fn activity_sections_render_seeded_events() {
    use super::activity::{ProviderKind, Section, SessionActivity};
    use crate::session::activity::{ActionKind, ActivityEvent};

    let ev = |kind, detail: &str, ok| ActivityEvent {
        ts_ms: Some(1_783_512_000_000),
        kind,
        detail: detail.into(),
        note: None,
        result_head: Some("output head".into()),
        ok,
        origin: None,
        minor: false,
        dur_ms: None,
    };
    let mut h = Harness::spawnable(1);
    let sid = h.app.sessions[0].info.id;
    h.app.activity.insert(
        sid,
        SessionActivity::seeded(
            ProviderKind::Claude,
            vec![
                ev(ActionKind::Command, "cargo test", Some(true)),
                ev(ActionKind::Command, "cargo bench", Some(false)),
                ev(ActionKind::Edit, "/repo/src/a.rs", Some(true)),
                ev(ActionKind::Read, "/repo/src/a.rs", Some(true)),
                ev(ActionKind::WebSearch, "ratatui table", Some(true)),
            ],
        ),
    );

    // F9 opens on Overview; counts land in the navigator state.
    h.key(KeyCode::F(9), KeyModifiers::NONE);
    {
        let ca = h.app.active_cc_activity().unwrap();
        assert_eq!(ca.counts.commands, 2);
        assert_eq!(ca.counts.total(), 5);
        assert_eq!(ca.files_count, 1, "edit+read of one path aggregate");
        // The dashboard's tile row carries the per-kind counts.
        assert!(ca.rows.iter().any(|r| matches!(
            r,
            super::cc_activity::CcRow::Tiles(t)
                if t.iter().any(|x| x.label == "cmds" && x.value == "2")
                    && t.iter().any(|x| x.label == "failed" && x.value == "1")
        )));
    }

    // Timeline shows every event; Commands filters to the two commands.
    h.key(KeyCode::Char('2'), KeyModifiers::NONE);
    {
        let ca = h.app.active_cc_activity().unwrap();
        assert_eq!(
            ca.open,
            Some(super::cc_activity::CcNodeRef::Section(Section::Timeline))
        );
        assert_eq!(ca.rows.len(), 5);
    }

    // Enter expands a Timeline event. Drop focus into the content pane, then
    // press Enter on the selected event row: for events, membership in
    // `collapsed_tools` reads as *expanded* (see ui/cc_activity.rs), so the
    // toggle reveals the result body.
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(h.app.focus, InputFocus::CcActivity);
    let ev_bi = {
        let ca = h.app.active_cc_activity().unwrap();
        match ca.rows[ca.selected] {
            super::cc_activity::CcRow::Block(bi) => bi,
            _ => panic!("Timeline rows are event blocks"),
        }
    };
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        h.app
            .active_cc_activity()
            .unwrap()
            .collapsed_tools
            .contains(&ev_bi),
        "Enter records the event block as expanded"
    );
    assert!(
        h.render().contains("output head"),
        "the expanded event renders its result body"
    );
    // Enter again collapses the event back to its compact one-line form.
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        !h.app
            .active_cc_activity()
            .unwrap()
            .collapsed_tools
            .contains(&ev_bi),
        "Enter again collapses the event"
    );

    h.key(KeyCode::Char('3'), KeyModifiers::NONE);
    assert_eq!(h.app.active_cc_activity().unwrap().rows.len(), 2);

    // Files section groups the touched path under "Edited (1)".
    h.key(KeyCode::Char('4'), KeyModifiers::NONE);
    {
        let ca = h.app.active_cc_activity().unwrap();
        assert!(ca.rows.iter().any(
            |r| matches!(r, super::cc_activity::CcRow::Header(s) if s.contains("Edited (1)"))
        ));
        assert!(ca.rows.iter().any(
            |r| matches!(r, super::cc_activity::CcRow::Text(s) if s.contains("/repo/src/a.rs"))
        ));
    }

    // Web section holds the single search; the frame renders without panic.
    h.key(KeyCode::Char('5'), KeyModifiers::NONE);
    assert_eq!(h.app.active_cc_activity().unwrap().rows.len(), 1);
    h.render();
}

#[test]
fn activity_timeline_groups_turns_folds_runs_and_badges_subagents() {
    use super::activity::{ProviderKind, Section, SessionActivity};
    use crate::session::activity::{ActionKind, ActivityEvent};

    let ev = |kind, detail: &str, origin: Option<&str>| ActivityEvent {
        ts_ms: Some(1_783_512_000_000),
        kind,
        detail: detail.into(),
        note: None,
        result_head: None,
        ok: Some(true),
        origin: origin.map(String::from),
        minor: false,
        dur_ms: Some(12_000),
    };
    let mut h = Harness::spawnable(1);
    let sid = h.app.sessions[0].info.id;
    h.app.activity.insert(
        sid,
        SessionActivity::seeded(
            ProviderKind::Claude,
            vec![
                ev(ActionKind::Prompt, "Fix the failing tests", None),
                ev(ActionKind::Command, "cargo nextest run", None),
                ev(ActionKind::Read, "/repo/src/a.rs", None),
                ev(ActionKind::Read, "/repo/src/a.rs", None),
                ev(ActionKind::Read, "/repo/src/a.rs", None),
                ev(ActionKind::Command, "cargo fmt", Some("fix-tests")),
            ],
        ),
    );

    h.key(KeyCode::F(9), KeyModifiers::NONE);
    h.key(KeyCode::Char('2'), KeyModifiers::NONE);
    {
        let ca = h.app.active_cc_activity().unwrap();
        assert_eq!(
            ca.open,
            Some(super::cc_activity::CcNodeRef::Section(Section::Timeline))
        );
        // Prompt + command + folded read run + subagent command = 4 rows.
        assert_eq!(ca.rows.len(), 4);
    }
    let frame = h.render();
    // The prompt renders as a dash-filled turn header…
    assert!(
        frame.contains("▶") && frame.contains("Fix the failing tests"),
        "turn header renders the prompt: {frame}"
    );
    // …events sit in the turn gutter, with the read run folded…
    assert!(frame.contains("│"), "timeline rows carry the turn gutter");
    assert!(
        frame.contains("×3"),
        "the repeated read folds to one ×3 row"
    );
    // …the subagent's work is nested and origin-badged, with its duration.
    assert!(
        frame.contains("└") && frame.contains("fix-tests"),
        "subagent-origin work is nested with its origin badge: {frame}"
    );
    assert!(frame.contains("12s"), "call→result duration renders");
}

#[test]
fn review_jump_to_file_anchors_header_to_top() {
    let mut h = Harness::standard(1);
    open_review(&mut h, 5);

    // Jumping to a file below the current window must scroll its header to the
    // top of the viewport, not leave it pinned to the bottom line (the renderer
    // only clamps the *upper* edge). Regression: clicking a changed-files row
    // landed the file at the last visible row.
    h.app.cr_jump_to_file(3);
    let cr = h.app.active_review().unwrap();
    assert_eq!(cr.current_file(), Some(3));
    assert_eq!(
        cr.scroll, cr.selected,
        "the jumped-to file header sits at the top of the viewport"
    );
}

#[test]
fn review_files_pane_demoted_to_terminal_when_review_closes() {
    let mut h = Harness::standard(1);
    open_review(&mut h, 2);
    h.app.focus = InputFocus::ReviewFiles;

    // Esc from the changed-files pane closes the review and drops focus back to
    // the terminal (no review owns the central pane anymore).
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(h.app.active_review().is_none());
    assert!(matches!(h.app.focus, InputFocus::Terminal));
}

// ── Manual session ordering ──────────────────────────────────────────────────

#[test]
fn shift_j_reorders_sessions() {
    let mut h = Harness::standard(2);
    let before = h.app.render_order_indices();
    assert_eq!(
        before,
        vec![0, 1],
        "initial render order is insertion order"
    );

    h.shift('j'); // SessionListMoveDown — move the selected (first) row down
    let after = h.app.render_order_indices();
    assert_eq!(
        after,
        vec![1, 0],
        "Shift+J swaps the first session past the second"
    );

    h.shift('k'); // SessionListMoveUp — move it back
    assert_eq!(
        h.app.render_order_indices(),
        vec![0, 1],
        "Shift+K restores the original order"
    );
}

// ── Tasks: panel focus + new-task editor ─────────────────────────────────────

#[test]
fn tasks_panel_new_task_opens_editor() {
    let mut h = Harness::standard(0);
    h.ctrl('w'); // FocusTasks → panel shown and focused
    assert!(h.app.show_tasks_panel);
    assert!(matches!(h.app.focus, InputFocus::TaskList));

    h.key(KeyCode::Char('n'), KeyModifiers::NONE); // new task
    assert!(
        matches!(h.app.focus, InputFocus::TaskEditor),
        "'n' opens the central-pane task editor"
    );
    assert!(
        h.app.task_ui.task_editor.is_some(),
        "a fresh task editor is in flight"
    );
}

#[test]
fn hiding_tasks_while_editing_rescues_focus() {
    let mut h = Harness::standard(1);
    h.ctrl('w');
    h.key(KeyCode::Char('n'), KeyModifiers::NONE);
    assert!(matches!(h.app.focus, InputFocus::TaskEditor));

    h.func(5);

    assert!(!h.app.show_tasks_panel);
    assert!(
        matches!(h.app.focus, InputFocus::SessionList),
        "focus must leave the editor when its panel is hidden"
    );
}

// ── Fork ─────────────────────────────────────────────────────────────────────

#[test]
fn leader_f_fork_opens_session_name_prompt() {
    // Fork pre-fills the session-name modal with "<name>-fork" before spawning,
    // so it is observable without a real backend. `Ctrl+F` itself is the leader
    // now, so fork is reached as `<leader> f` — the same letter, one key later.
    let mut h = Harness::standard(1);
    h.leader(KeyCode::Char('f'));
    assert!(
        matches!(h.app.modal, modals::Modal::SessionName(_)),
        "<leader> f opens the session-name prompt for the fork"
    );
}

// ── Help editor: capture mode ────────────────────────────────────────────────

#[test]
fn help_editor_enters_capture_mode() {
    let mut h = Harness::standard(0);
    h.func(1); // F1 → help
    match h.app.modal {
        modals::Modal::Help(ref help) => assert!(!help.capturing, "starts in navigation mode"),
        ref other => panic!("expected help modal, got {other:?}"),
    }

    h.key(KeyCode::Enter, KeyModifiers::NONE); // begin capturing a new chord
    match h.app.modal {
        modals::Modal::Help(ref help) => {
            assert!(
                help.capturing,
                "Enter starts capture mode for the selected action"
            )
        }
        ref other => panic!("expected help modal, got {other:?}"),
    }
}

// ── Behavioral effects: assert the action actually changed state ──────────────

#[test]
fn theme_picker_selection_applies_and_persists() {
    let mut h = Harness::standard(0);
    let entries = crate::ui::theme::all_theme_entries();
    let default_name = h.app.active_theme.name.clone();

    h.ctrl('y'); // open the picker (opens on the active theme, index 0)
    h.key(KeyCode::Char('j'), KeyModifiers::NONE); // move to the next palette
    h.key(KeyCode::Enter, KeyModifiers::NONE); // confirm

    assert!(!h.app.modal.is_open(), "confirming closes the picker");
    assert_eq!(
        h.app.active_theme.name, entries[1].name,
        "the second palette becomes active"
    );
    assert_ne!(
        h.app.active_theme.name, default_name,
        "the theme actually changed"
    );
    assert_eq!(
        h.app.db.get_active_theme().ok().flatten().as_deref(),
        Some(entries[1].name.as_str()),
        "the choice is persisted to the database"
    );
}

#[test]
fn theme_picker_cancel_restores_previewed_theme() {
    // The picker live-previews by mutating the global palette as the selection
    // moves; cancelling (`Esc`) must undo that preview, leaving the original
    // theme active and unpersisted.
    let mut h = Harness::standard(0);
    let entries = crate::ui::theme::all_theme_entries();
    let original_name = h.app.active_theme.name.clone();
    let original_palette = crate::ui::theme::current();

    h.ctrl('y'); // open the picker (opens on the active theme, index 0)
    h.key(KeyCode::Char('j'), KeyModifiers::NONE); // preview the next palette
    assert_eq!(
        crate::ui::theme::current(),
        entries[1].palette,
        "navigating previews the highlighted palette globally"
    );

    h.key(KeyCode::Esc, KeyModifiers::NONE); // cancel

    assert!(!h.app.modal.is_open(), "Esc closes the picker");
    assert_eq!(
        crate::ui::theme::current(),
        original_palette,
        "cancelling restores the palette active when the picker opened"
    );
    assert_eq!(
        h.app.active_theme.name, original_name,
        "the active theme is unchanged after cancel"
    );
    assert_eq!(
        h.app.db.get_active_theme().ok().flatten(),
        None,
        "cancelling persists nothing to the database"
    );
}

#[test]
fn help_editor_capture_rebinds_the_selected_action() {
    // The help editor opens with the first rebindable action selected; capturing
    // a fresh chord must reassign exactly that action.
    let action = crate::session::Action::rebindable_in_order()[0];
    let new_chord = crate::session::KeyChord::ctrl('x');

    let mut h = Harness::standard(0);
    h.func(1); // F1 → help
    h.key(KeyCode::Enter, KeyModifiers::NONE); // begin capture
    h.ctrl('x'); // the captured chord

    assert_eq!(
        h.app.keybindings.chord_for(action),
        Some(&new_chord),
        "the selected action is rebound to the captured chord"
    );
    match h.app.modal {
        modals::Modal::Help(ref help) => {
            assert!(!help.capturing, "capture ends after one chord")
        }
        ref other => panic!("expected help modal, got {other:?}"),
    }
}

#[test]
fn task_editor_creates_task_and_space_cycles_status() {
    let mut h = Harness::standard(0);
    h.ctrl('w'); // focus the tasks panel
    h.key(KeyCode::Char('n'), KeyModifiers::NONE); // new-task editor

    for ch in "Demo task".chars() {
        h.key(KeyCode::Char(ch), KeyModifiers::NONE);
    }
    h.ctrl('s'); // save from any field

    assert!(
        matches!(h.app.focus, InputFocus::TaskList),
        "saving returns to the panel"
    );
    assert_eq!(h.app.task_ui.cached_tasks.len(), 1, "the task is persisted");
    let task = &h.app.task_ui.cached_tasks[0];
    assert_eq!(task.title, "Demo task");
    assert_eq!(
        task.status,
        crate::session::TaskStatus::Todo,
        "new tasks start as Todo"
    );

    h.key(KeyCode::Char(' '), KeyModifiers::NONE); // cycle status
    assert_eq!(
        h.app.task_ui.cached_tasks[0].status,
        crate::session::TaskStatus::InProgress,
        "Space advances Todo → InProgress"
    );
}

#[test]
fn global_search_returns_results_for_a_session_query() {
    let mut h = Harness::standard(2); // session-0, session-1
    h.ctrl('/'); // open the search strip
    for ch in "session-1".chars() {
        h.key(KeyCode::Char(ch), KeyModifiers::NONE);
    }
    assert_eq!(h.app.global_search.query.value(), "session-1");
    assert!(
        !h.app.global_search.results.is_empty(),
        "a matching session name yields at least one result"
    );
}

#[test]
fn ctrl_c_in_a_focused_terminal_preserves_sigint_over_status_copy() {
    // Ctrl+C copies the status message only in non-PTY panes. In a focused
    // terminal it must still fall through to SIGINT — so the status-copy path is
    // skipped and the shown message is left untouched (deterministic: no
    // clipboard call). The status row stays reachable by mouse click there.
    let mut h = Harness::standard(1);
    h.app.focus = InputFocus::Terminal;
    h.app.set_error("boom");

    h.ctrl('c'); // Copy — no selection, terminal-focused → SIGINT, not a copy

    let msg = h.app.status_message.as_ref().expect("message still shown");
    assert_eq!(
        msg.text, "boom",
        "terminal Ctrl+C leaves the status message intact (SIGINT, no copy)"
    );
}

#[test]
fn ctrl_c_outside_a_terminal_copies_the_status_message() {
    // In a non-PTY pane Ctrl+C (no selection) runs the status-copy path. Every
    // branch of `copy_status_to_clipboard` overwrites the toast (copied /
    // unavailable / write-error), so the original message no longer stands —
    // deterministic regardless of whether a clipboard exists on the runner.
    let mut h = Harness::standard(1);
    h.app.focus = InputFocus::SessionList;
    h.app.set_error("boom");

    h.ctrl('c'); // Copy — no selection, non-terminal → copy the status message

    let msg = h
        .app
        .status_message
        .as_ref()
        .expect("a toast is still shown");
    assert_ne!(
        msg.text, "boom",
        "the copy path fired and replaced the original message with its result"
    );
}

#[test]
fn status_message_row_records_a_click_to_copy_target() {
    // The status row is click-to-copy: whenever a message is shown, its rect is
    // registered as a `CopyStatus` hitbox so a mouse click pulls the text out.
    let mut h = Harness::standard(1);
    h.app.set_info("something worth copying");
    h.render();

    let has_target = h
        .app
        .click_targets
        .iter()
        .any(|t| matches!(t.action, ClickAction::CopyStatus));
    assert!(
        has_target,
        "a shown status message registers a CopyStatus click target"
    );
}

// ── Spawn-dependent flows (fake backend, real Tokio I/O wiring) ───────────────

#[tokio::test]
async fn switcher_enter_loads_an_unloaded_session() {
    // `Enter` on a ghost row in the sidebar starts its agent; picking the same
    // ghost out of the switcher has to do the same, or the switcher reaches it
    // and leaves the user on a frozen frame with no way forward.
    let mut h = Harness::spawnable(2);
    h.app.set_active_index(1);
    h.alt('u'); // UnloadSession
    assert!(
        h.app.sessions[1].is_ghost(),
        "the row is a ghost to begin with"
    );
    h.app.set_active_index(0);

    h.ctrl('/'); // GlobalSearch — the switcher, listing everything but the active row
    h.key(KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(h.app.active_index, 1);
    assert!(
        !h.app.active_session_is_ghost(),
        "choosing a ghost loads it"
    );
    assert!(!h.app.global_search.active, "and the popup is gone");
}

#[tokio::test]
async fn loading_a_ghost_in_a_tiny_terminal_clamps_parser_size() {
    let mut h = Harness::spawnable(1);
    h.alt('u');
    assert!(h.app.sessions[0].is_ghost());

    h.resize(31, 4);
    h.key(KeyCode::Enter, KeyModifiers::NONE);

    assert!(!h.app.sessions[0].is_ghost());
    let mut size = None;
    h.app.with_active_parser(|p| size = Some(p.screen().size()));
    let (rows, cols) = size.expect("the loaded session has a parser");
    assert!(rows >= 1 && cols >= 1, "parser size was {rows}x{cols}");
}

#[tokio::test]
async fn ghost_load_refuses_a_sanitized_live_window_collision() {
    let mut h = Harness::spawnable(2);
    h.app.sessions[0].info.name = "alpha:beta".into();
    h.app.set_active_index(1);
    h.alt('u');
    h.app.sessions[1].info.name = "alpha.beta".into();

    h.key(KeyCode::Enter, KeyModifiers::NONE);

    assert!(
        h.app.sessions[1].is_ghost(),
        "the ghost stays unloaded when its tmux window is already owned"
    );
    assert_eq!(
        h.app
            .sessions
            .iter()
            .filter(|s| !s.is_placeholder())
            .count(),
        1,
        "no second live pane is spawned"
    );
    let msg = h
        .app
        .status_message
        .as_ref()
        .expect("collision is reported");
    assert_eq!(msg.level, StatusLevel::Error);
    assert!(msg.text.contains("tb-alpha_beta"), "got: {}", msg.text);
    assert!(msg.text.contains("alpha:beta"), "got: {}", msg.text);
}

#[tokio::test]
async fn ctrl_r_restarts_session_on_spawnable_backend() {
    // Restart kills + respawns through the backend and rewires I/O; the fake
    // backend makes that succeed without a real tmux/PTY.
    let mut h = Harness::spawnable(1);
    h.ctrl('r'); // RestartSession

    let msg = h
        .app
        .status_message
        .as_ref()
        .expect("restart reports a status toast");
    assert!(
        matches!(msg.level, StatusLevel::Info),
        "restart succeeds (not an error toast): {:?}",
        msg.text
    );
    assert!(
        msg.text.contains("restart"),
        "the toast names the restart: {:?}",
        msg.text
    );
}

#[tokio::test]
async fn ctrl_r_restart_drops_the_replaced_pane_s_memory_figure() {
    // A restart (and the load path a ghost's Enter takes) swaps in a new pane,
    // so the figure on the row was measured for a process that no longer
    // exists: on a load it is the ghost's `—`, which would keep claiming the
    // session is unloaded until the next scan. An in-flight pass measured the
    // old pane too, so it must not deliver either.
    let mut h = Harness::spawnable(1);
    h.app.sessions[0].info.memory = Some(crate::session::SessionMemory::Unloaded);
    let _tx = h.app.memory_refresh.start();

    h.ctrl('r'); // RestartSession

    assert_eq!(h.app.sessions[0].info.memory, None);
    assert!(!h.app.memory_refresh.in_progress());
}

#[tokio::test]
async fn ctrl_r_restart_preserves_friring_identity_env() {
    // `Session::restart` replaces the session env wholesale, so the restart path
    // must re-inject the `FRIRING_*` identity vars — otherwise the restarted
    // agent loses its identity and the metrics/status hooks break.
    let mut h = Harness::spawnable(1);
    let session_id = h.app.sessions[0].info.id;
    let agent_session_id = h.app.sessions[0]
        .info
        .agent_session_id
        .clone()
        .expect("spawnable sessions have an agent_session_id");

    h.ctrl('r'); // RestartSession

    let env = h.app.sessions[0].env();
    assert_eq!(
        env.get("FRIRING_SESSION"),
        Some(&session_id.to_string()),
        "the friring session key survives the restart"
    );
    assert_eq!(
        env.get("FRIRING_SESSION_ID"),
        Some(&agent_session_id),
        "the agent conversation id survives the restart"
    );
}

#[tokio::test]
async fn ctrl_r_restart_rewires_osc52_capture_to_the_new_pane() {
    // Restart swaps in a fresh reader loop with its own clipboard queue; the
    // session must adopt that queue (and reset its drain gate) or every
    // in-pane OSC 52 copy after a restart is silently lost. Driven through the
    // real reader-loop wiring — the `feed_output_for_test` seam pushes into
    // whatever queue the session already holds, so it cannot catch a restart
    // left pointing at the dead pane's queue.
    let mut h = Harness::with_backend(
        STD_COLS,
        STD_ROWS,
        1,
        Arc::new(FakeBackend::spawnable_with_output(
            b"\x1b]52;c;aGVsbG8=\x07", // OSC 52 copy of "hello"
        )),
    );
    h.app.sessions[0].info.agent_session_id = Some("agent-0".into());
    h.app.captured_clipboard = Some(Vec::new());

    h.ctrl('r'); // RestartSession — respawns through the fake backend

    // The new pane's output arrives via a `spawn_blocking` reader thread;
    // bounded-poll the deterministic tick until the copy lands (~2 s cap).
    for _ in 0..100 {
        if !h.app.captured_clipboard.as_ref().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        h.tick();
    }
    assert_eq!(
        h.app.captured_clipboard.as_deref(),
        Some(&["hello".to_string()][..]),
        "the restarted pane's OSC 52 copy reaches the clipboard exactly once"
    );
    assert_eq!(
        h.app.status_message.as_ref().map(|m| m.text.as_str()),
        Some("Copied from session-0"),
        "the toast names the originating session"
    );

    // Drained: another tick must not copy again.
    h.tick();
    assert_eq!(h.app.captured_clipboard.as_ref().unwrap().len(), 1);
}

#[tokio::test]
async fn ctrl_t_opens_shell_pane_on_spawnable_backend() {
    // Ctrl+T lazily spawns a shell pane via the backend and flips the session's
    // terminal view to the shell.
    let mut h = Harness::spawnable(1);
    let id = h.app.sessions[0].info.id;

    h.ctrl('t'); // ToggleShell

    assert!(
        h.app.status_message.is_none()
            || !matches!(
                h.app.status_message.as_ref().unwrap().level,
                StatusLevel::Error
            ),
        "opening the shell pane does not error"
    );
    assert!(
        h.app.sessions[0].shell_pane.is_some(),
        "a shell pane was spawned for the session"
    );
    assert_eq!(
        h.app.session_terminal_views.get(&id),
        Some(&TerminalView::Shell),
        "the active session now shows its shell view"
    );
}

#[tokio::test]
async fn cross_pane_osc52_copies_apply_newest_by_capture_order() {
    // A session drains its agent pane before its shell pane, so a shell copy
    // captured *earlier* than an agent copy would, without a capture sequence,
    // look like the newest and win the clipboard — an inversion. Capture the
    // shell copy first, the agent copy second: the newer (agent) copy must win,
    // and the older one must not be written at all (one clipboard write per
    // tick, not one per queued copy).
    let mut h = Harness::spawnable(1);
    h.ctrl('t'); // ToggleShell — spawn the shell pane
    assert!(h.app.sessions[0].shell_pane.is_some());
    h.app.captured_clipboard = Some(Vec::new());

    // Shell pane copies first (older), agent pane second (newer).
    h.app.sessions[0]
        .shell_pane
        .as_ref()
        .unwrap()
        .feed_output_for_test(b"\x1b]52;c;b2xkZXI=\x07"); // "older"
    h.app.sessions[0].feed_output_for_test(b"\x1b]52;c;bmV3ZXI=\x07"); // "newer"

    h.tick();

    assert_eq!(
        h.app.captured_clipboard.as_deref(),
        Some(&["newer".to_string()][..]),
        "the newest capture wins the clipboard; superseded copies are dropped"
    );

    // Both queues were still drained — a stale copy must not resurface later.
    h.tick();
    assert_eq!(h.app.captured_clipboard.as_ref().unwrap().len(), 1);
}

#[tokio::test]
async fn dragging_over_a_wrapped_url_copies_it_as_one_string() {
    // A URL longer than the pane occupies two visual rows. Reading the painted
    // cells would paste it with a newline at the seam — the one thing a URL
    // must survive. The vt100 grid knows the seam is a soft wrap, so the
    // selection rejoins it.
    let mut h = Harness::standard(1);
    h.render(); // lay the panes out so `screen_layout` is meaningful

    let area = h.app.screen_layout().terminal;
    let inner = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2);
    // Match the grid to the pane so the wrap lands exactly at its right edge.
    h.app
        .with_active_parser(|p| p.screen_mut().set_size(inner.height, inner.width));

    let tail = "/wrapped/across/two/rows";
    let url = format!(
        "https://example.com/{}{tail}",
        "x".repeat(inner.width as usize - "https://example.com/".len())
    );
    h.feed_output(0, url.as_bytes());
    h.render();

    // Drag from the URL's first cell to its last, across the wrap seam.
    h.app.update(AppMessage::MouseClick {
        x: inner.x,
        y: inner.y,
        modifiers: KeyModifiers::NONE,
    });
    h.app.update(AppMessage::MouseDrag {
        x: inner.x + tail.len() as u16 - 1,
        y: inner.y + 1,
    });
    h.app.update(AppMessage::MouseUp {
        x: inner.x + tail.len() as u16 - 1,
        y: inner.y + 1,
    });
    h.render(); // refreshes the selected-text cache

    assert_eq!(
        h.app.selected_text_cache.as_deref(),
        Some(url.as_str()),
        "a soft-wrapped URL copies as one unbroken string"
    );
}

#[tokio::test]
async fn dragging_in_the_session_list_copies_the_painted_row() {
    // The session list has no vt100 grid behind it, so it must keep reading
    // the painted cells — the grid path only claims the central terminal pane.
    let mut h = Harness::standard(1);
    h.render(); // lay the panes out so `screen_layout` is meaningful

    let panel = h
        .app
        .screen_layout()
        .left_panel
        .expect("the session list is laid out");
    let inner = Rect::new(panel.x + 1, panel.y + 1, panel.width - 2, panel.height - 2);

    let buffer = h.terminal.backend().buffer();
    let row = (inner.y..inner.y + inner.height)
        .find(|y| {
            (inner.x..inner.x + inner.width)
                .map(|x| buffer[(x, *y)].symbol())
                .collect::<String>()
                .contains("session-0")
        })
        .expect("the session is listed");

    h.app.update(AppMessage::MouseClick {
        x: inner.x,
        y: row,
        modifiers: KeyModifiers::NONE,
    });
    h.app.update(AppMessage::MouseDrag {
        x: inner.x + inner.width - 1,
        y: row,
    });
    h.app.update(AppMessage::MouseUp {
        x: inner.x + inner.width - 1,
        y: row,
    });
    h.render(); // refreshes the selected-text cache

    let text = h
        .app
        .selected_text_cache
        .as_deref()
        .expect("a pane with no grid behind it still copies");
    assert!(
        text.contains("session-0"),
        "the session row copies what is painted: {text:?}"
    );
}

#[tokio::test]
async fn central_tab_strip_switches_agent_and_shell_views() {
    // The top-border tab strip is mouse-driven: clicking Shell flips to the
    // shell view (spawning the pane), clicking Agent flips back.
    let mut h = Harness::spawnable(1);
    let id = h.app.sessions[0].info.id;
    assert_eq!(h.app.active_central_tab(), CentralTab::Agent);

    assert!(h.click_central_tab(CentralTab::Shell), "Shell tab rendered");
    assert!(
        h.app.sessions[0].shell_pane.is_some(),
        "clicking Shell spawned the shell pane"
    );
    assert_eq!(
        h.app.session_terminal_views.get(&id),
        Some(&TerminalView::Shell),
        "clicking Shell selects the shell view"
    );
    assert_eq!(h.app.active_central_tab(), CentralTab::Shell);

    assert!(h.click_central_tab(CentralTab::Agent), "Agent tab rendered");
    assert_eq!(
        h.app.session_terminal_views.get(&id),
        Some(&TerminalView::Claude),
        "clicking Agent returns to the agent view"
    );
    assert_eq!(h.app.active_central_tab(), CentralTab::Agent);
}

#[tokio::test]
async fn f8_leaves_open_review_for_the_shell() {
    // With a review overlaying the central pane, F8 (ToggleShell) must reach the
    // global binding (the review's key capture lets it fall through) and land on
    // the shell — not silently flip the hidden terminal view behind the review.
    let mut h = Harness::spawnable(1);
    let sid = h.app.active_session_id().unwrap();
    h.app
        .code_reviews
        .insert(sid, super::code_review::CodeReviewState::for_test(sid, 2));
    h.app.focus = InputFocus::CodeReview;
    assert_eq!(h.app.active_central_tab(), CentralTab::Review);

    h.func(8); // F8 = ToggleShell

    assert!(
        h.app.active_review().is_none(),
        "F8 closes the open review instead of being swallowed"
    );
    assert_eq!(
        h.app.active_central_tab(),
        CentralTab::Shell,
        "F8 lands on the shell view"
    );
    assert_eq!(
        h.app.focus,
        InputFocus::Terminal,
        "focus moves out of the (now closed) review to the terminal"
    );
    assert!(
        h.app.sessions[0].shell_pane.is_some(),
        "the shell pane was spawned"
    );
}

#[tokio::test]
async fn central_tab_strip_renders_labels_and_shortcuts() {
    // The strip paints Agent/Shell/Review with each toggle's shortcut hint in
    // the pane's top border (Agent has no dedicated key, so no hint).
    let mut h = Harness::spawnable(1);
    let screen = h.render();
    // The tab strip is the pane border row carrying the Review toggle hint `F7`
    // (anchored on it to avoid the "…Agent Orchestrator" header banner). The
    // F-key form is shown, not `^X`, since a focused terminal passes Ctrl chords
    // through to the agent.
    let top = screen.lines().find(|l| l.contains("F7")).unwrap_or("");
    for needle in ["Agent", "Shell", "F8", "Review", "F7"] {
        assert!(
            top.contains(needle),
            "central tab strip missing {needle:?}: {top:?}"
        );
    }
}

#[tokio::test]
async fn pane_title_never_runs_under_the_central_tab_strip() {
    // The tab pills and the session-info title share the pane's top border, and
    // the pills are painted last — so a title too long for what they leave used
    // to lose its head under them (worst on a worktree session, whose branch is
    // usually as long as its name). The title now budgets itself around the
    // strip: it must start at or after the last pill, at every width.
    let mut h = Harness::spawnable(1);
    h.app.sessions[0]
        .info
        .worktrees
        .push(crate::session::WorktreeInfo {
            repo_path: std::path::PathBuf::from("/repo"),
            worktree_path: std::path::PathBuf::from("/wt"),
            branch: "fix/displaying-top-status-in-the-central-pane".to_string(),
        });

    for cols in [60, 80, 100, STD_COLS, 160] {
        h.resize(cols, STD_ROWS);
        h.render();
        let pane = h
            .app
            .click_targets
            .iter()
            .find_map(|t| match t.action {
                ClickAction::FocusPane(InputFocus::Terminal) => Some(t.rect),
                _ => None,
            })
            .expect("terminal pane hitbox recorded");
        let tabs_end = h
            .app
            .click_targets
            .iter()
            .filter_map(|t| match t.action {
                ClickAction::CentralTab(_) => Some(t.rect.x + t.rect.width),
                _ => None,
            })
            .max()
            .expect("tab strip rendered");

        // The pills are painted *over* the title, so an overlap is invisible as
        // such — it shows up as a title missing its head. Read the border from
        // the strip's right edge to the pane corner (border fill first, then the
        // title): whatever survives the fit must be a whole field set, never the
        // tail of a longer one. Too narrow for even the status and the title
        // yields entirely, leaving the strip the whole border — which is why 60
        // and 80 columns are the intentional status-only/empty fallback. From
        // 100 up the branch must survive, as a truncated fragment at 100 and
        // whole once the pane is wide enough.
        let buffer = h.terminal.backend().buffer();
        let border = pane.x + pane.width - 1;
        let visible: String = (tabs_end..border)
            .map(|x| buffer[(x, pane.y)].symbol())
            .collect();
        let title = visible.trim_start_matches('─');
        assert!(
            title.is_empty() || (title.starts_with(' ') && title.ends_with("] ")),
            "at {cols} cols the title is a clipped remnant: {title:?}"
        );
        assert!(
            cols < 100 || title.contains("[fix/"),
            "at {cols} cols there is room for the branch field: {title:?}"
        );
        assert!(
            cols != 100 || title.contains('\u{2026}'),
            "at {cols} cols the branch is truncated, not dropped: {title:?}"
        );
    }
}

#[tokio::test]
async fn central_tab_strip_omits_feature_gated_tabs() {
    // Shell/Review tabs are gated by their feature flags. With both off, only
    // the Agent pill would remain — a tab strip you can't switch away from — so
    // the whole strip is suppressed rather than advertising a lone dead tab.
    let mut h = Harness::spawnable(1);
    let collect_tabs = |app: &super::App| -> Vec<CentralTab> {
        app.click_targets
            .iter()
            .filter_map(|t| match t.action {
                ClickAction::CentralTab(tab) => Some(tab),
                _ => None,
            })
            .collect()
    };

    // Only one alternate view enabled → the strip stays (Agent + the other).
    h.app.features.shell_pane = false;
    h.app.features.cc_activity = false;
    h.app.features.code_review = true;
    h.render();
    assert_eq!(
        collect_tabs(&h.app),
        vec![CentralTab::Agent, CentralTab::Review],
        "Review survives when the other alternate views are gated off"
    );

    // The activity view is likewise a gated central tab.
    h.app.features.shell_pane = false;
    h.app.features.code_review = false;
    h.app.features.cc_activity = true;
    h.render();
    assert_eq!(
        collect_tabs(&h.app),
        vec![CentralTab::Agent, CentralTab::CcActivity],
        "Activity survives when the other alternate views are gated off"
    );

    // All alternate views gated off → no tab strip at all.
    h.app.features.shell_pane = false;
    h.app.features.code_review = false;
    h.app.features.cc_activity = false;
    h.render();
    assert!(
        collect_tabs(&h.app).is_empty(),
        "the lone Agent tab is dropped when Shell, Review, and Activity are all off"
    );
}

/// The F2 info panel lists upcoming automation runs. When the `automations`
/// feature is off the TUI never fires those schedules (and the pane is hidden),
/// so the info panel must not surface them either — even though the cache is
/// still loaded from the DB.
#[tokio::test]
async fn info_panel_hides_automations_when_feature_off() {
    use crate::session::{Automation, AutomationAction, AutomationSchedule};
    let mut h = Harness::spawnable(1);
    h.app.show_info_panel = true;
    let far_future = crate::sync::current_time_millis() + 3_600_000;
    h.app.automation_ui.cached_automations = vec![Automation {
        id: 1,
        name: "infopanelnightly".into(),
        enabled: true,
        schedule: AutomationSchedule::Once { at: 0 },
        timezone: None,
        action: AutomationAction::send_to(SessionId::default()),
        prompt: "p".into(),
        created_at: 0,
        updated_at: 0,
        last_run_at: None,
        next_run_at: Some(far_future),
        prompt_steps: Vec::new(),
    }];

    // Feature on: the info panel's automations section lists it.
    h.app.features.automations = true;
    assert!(
        h.render().contains("infopanelnightly"),
        "info panel surfaces the upcoming automation when the feature is on"
    );

    // Feature off: the pane is hidden *and* the info-panel section is dropped,
    // so the automation appears nowhere.
    h.app.features.automations = false;
    assert!(
        !h.render().contains("infopanelnightly"),
        "info panel must not surface automations when the feature is off"
    );
}

// ── Performance counters: deterministic render-path proxies ───────────────────
//
// These assert on `App::perf_counters()` — wall-clock-free counts — so they
// gate the redraw-throttling and per-frame caching optimizations without timing
// flakiness. The acceptance harness drives `view()` directly (it skips
// `tick()`), so only the render-path counters are exercised here; the
// tick-driven counters (`status_refreshes`) and the redraw-skip accounting live
// in the `#[tokio::test]` units in `super::tests`.

// ── Leader key ──────────────────────────────────────────────────────────────

#[test]
fn leader_arms_and_a_bound_key_runs_the_action() {
    let mut h = Harness::standard(2);
    assert!(!h.app.prefix_state.is_armed());
    h.ctrl('f');
    assert!(h.app.prefix_state.is_armed(), "Ctrl+F arms the leader");
    h.render(); // the which-key overlay paints without disturbing the panes
    h.key(KeyCode::Char('b'), KeyModifiers::NONE);
    assert!(!h.app.prefix_state.is_armed(), "the key disarms");
    assert!(h.app.show_info_panel, "<leader> b toggles the info panel");
}

/// The which-key overlay paints as soon as the leader arms (`hint_delay_ms`
/// defaults to 0) and disappears once a key resolves it.
#[test]
fn which_key_overlay_paints_while_armed() {
    let mut h = Harness::standard(1);
    h.ctrl('f');
    let armed = h.render();
    // One label per section: at the standard 120 columns the groups don't all
    // fit side by side, so this is what catches a layout that silently drops
    // the ones that wrapped.
    for label in [
        "go to session N",
        "info panel",
        "new session",
        "code review",
        "quit",
        "perf HUD",
        "send key to agent",
    ] {
        assert!(
            armed.contains(label),
            "the armed overlay lists `{label}`:\n{armed}"
        );
    }
    assert!(
        armed.contains("ctrl+f"),
        "and titles itself with the leader"
    );

    h.key(KeyCode::Esc, KeyModifiers::NONE);
    let idle = h.render();
    assert!(
        !idle.contains("go to session N"),
        "and vanishes once disarmed:\n{idle}"
    );
}

/// A non-zero `hint_delay_ms` keeps the overlay hidden immediately after
/// arming — but the footer badge still shows the pending state, so the app
/// never looks frozen.
#[test]
fn hint_delay_hides_the_overlay_but_not_the_armed_badge() {
    let mut h = Harness::standard(1);
    h.app.prefix_settings.hint_delay_ms = 5_000;
    h.ctrl('f');
    assert!(h.app.prefix_state.is_armed());
    assert!(
        h.app.prefix_hint_chord().is_none(),
        "the overlay waits out the delay"
    );
    let painted = h.render();
    assert!(!painted.contains("go to session N"), "overlay is hidden");
    assert!(
        painted.contains("ctrl+f"),
        "the footer badge still shows it"
    );

    // Once the delay is out the overlay appears on its own — and the armed
    // state itself never expires, only a key press clears it.
    h.advance(std::time::Duration::from_millis(5_001));
    assert!(h.app.prefix_hint_chord().is_some());
    let painted = h.render();
    assert!(
        painted.contains("go to session N"),
        "the overlay appears once the delay elapses:\n{painted}"
    );
    assert!(
        h.app.prefix_state.is_armed(),
        "the delay does not time the leader out"
    );
}

/// `<leader> K` then a digit moves the active session that many places toward
/// the top, shifting the rows it passes rather than swapping with one.
#[test]
fn leader_shift_k_moves_the_session_up_by_the_digit() {
    let mut h = Harness::standard(4);
    h.app.set_active_index(3);
    let moved = h.app.sessions[3].info.id;

    h.leader(KeyCode::Char('K'));
    assert!(
        matches!(
            h.app.prefix_state,
            crate::app::PrefixState::AwaitingMove { up: true }
        ),
        "the gesture waits for its distance"
    );
    h.key(KeyCode::Char('2'), KeyModifiers::NONE);

    let order = h.app.render_order_indices();
    let pos = order
        .iter()
        .position(|&i| h.app.sessions[i].info.id == moved)
        .unwrap();
    assert_eq!(pos, 1, "moved two places up, from index 3 to 1");
    assert!(
        matches!(h.app.prefix_state, crate::app::PrefixState::Idle),
        "and the gesture ends"
    );
}

/// The distance clamps at the end of the list rather than erroring — a
/// generous digit means "as far as it goes".
#[test]
fn leader_move_clamps_at_the_end_of_the_list() {
    let mut h = Harness::standard(3);
    h.app.set_active_index(2);
    let moved = h.app.sessions[2].info.id;
    h.leader(KeyCode::Char('K'));
    h.key(KeyCode::Char('9'), KeyModifiers::NONE);
    let order = h.app.render_order_indices();
    let pos = order
        .iter()
        .position(|&i| h.app.sessions[i].info.id == moved)
        .unwrap();
    assert_eq!(pos, 0, "clamped to the top");
}

/// While the move is pending the list numbers rows by *distance* from the
/// active session, not by absolute position — otherwise the digit the user
/// reads would not be the digit they need.
#[test]
fn pending_move_numbers_rows_by_distance() {
    let mut h = Harness::standard(3);
    h.app.set_active_index(2);
    h.leader(KeyCode::Char('K'));
    assert_eq!(
        h.app.jump_numbering(),
        Some(crate::app::JumpNumbering::MoveDistance { from: 2, up: true })
    );
    assert_eq!(
        h.app.jump_overlay_attention_only(),
        None,
        "a move is neither the all-sessions nor the blocked numbering"
    );
}

/// A non-digit cancels the pending move without reordering anything.
#[test]
fn a_non_digit_cancels_a_pending_move() {
    let mut h = Harness::standard(3);
    h.app.set_active_index(2);
    let before = h.app.render_order_indices();
    h.leader(KeyCode::Char('J'));
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(matches!(h.app.prefix_state, crate::app::PrefixState::Idle));
    assert_eq!(h.app.render_order_indices(), before, "nothing moved");
}

/// GNU screen's convention: the leader table accepts its keys with `Ctrl`
/// still held, so a whole sequence can be typed without releasing the
/// modifier.
#[test]
fn leader_accepts_its_keys_with_ctrl_still_held() {
    let mut h = Harness::standard(1);
    h.ctrl('f');
    h.ctrl('b');
    assert!(
        h.app.show_info_panel,
        "<leader> Ctrl+B works like <leader> b"
    );
    assert!(!h.app.prefix_state.is_armed());
}

/// …but the leader pressed twice still means "send the literal byte", which
/// must win over the Ctrl-held resolution above.
#[test]
fn double_leader_still_sends_the_literal_byte() {
    let mut h = Harness::standard(1);
    h.app.focus = InputFocus::Terminal;
    h.ctrl('f');
    h.ctrl('f');
    assert!(!h.app.prefix_state.is_armed());
    assert!(
        !h.app.show_info_panel,
        "the second leader press is not a table lookup"
    );
}

/// Review finding D1: the capture panes consumed every Ctrl chord outside
/// their escape list, so the leader could not arm from the code-review or
/// activity views at all. It now runs ahead of them — a leader that only works
/// in some panes isn't a leader.
#[test]
fn leader_arms_from_the_code_review_pane() {
    let mut h = Harness::standard(1);
    h.app.focus = InputFocus::CodeReview;
    h.ctrl('f');
    assert!(
        h.app.prefix_state.is_armed(),
        "the leader arms even where the pane captures Ctrl chords"
    );
    h.key(KeyCode::Char('b'), KeyModifiers::NONE);
    assert!(h.app.show_info_panel, "and its table dispatches");
}

/// …but a text-entry submode still owns the chord: there the leader key is a
/// line-editing key in the field being typed into.
#[test]
fn leader_yields_to_a_text_entry_submode() {
    let mut h = Harness::standard(1);
    h.app.show_file_viewer = true;
    h.app.focus = InputFocus::FileViewer;
    h.app.file_viewer.search_active = true;
    h.ctrl('f');
    assert!(
        !h.app.prefix_state.is_armed(),
        "typing in a search field keeps Ctrl+A as beginning-of-line"
    );
}

/// Review finding D2: `prefix-only` promises every bare `Ctrl+<letter>`
/// reaches the agent, but the clipboard route ran ahead of the gate — so
/// `Ctrl+V` (image paste in Claude Code and Codex) never got there.
#[test]
fn prefix_only_lets_the_terminal_keep_its_clipboard_chords() {
    let mut h = Harness::standard(1);
    h.app.prefix_settings.mode = crate::session::PrefixMode::PrefixOnly;
    h.app.focus = InputFocus::Terminal;
    assert!(
        !h.app
            .handle_priority_key_for_test(KeyCode::Char('v'), KeyModifiers::CONTROL),
        "Ctrl+V falls through to the PTY in prefix-only"
    );
}

/// The gate is narrow: friring's own text inputs still receive paste, since
/// Copy/Paste deliberately have no leader route.
#[test]
fn prefix_only_still_pastes_into_friring_inputs() {
    let mut h = Harness::standard(1);
    h.app.prefix_settings.mode = crate::session::PrefixMode::PrefixOnly;
    h.app.focus = InputFocus::SessionList;
    assert!(
        h.app
            .handle_priority_key_for_test(KeyCode::Char('v'), KeyModifiers::CONTROL),
        "outside a terminal, paste is still friring's"
    );
}

/// `[prefix]` applies live like the other mirrored settings, and turning the
/// leader off clears any armed state rather than stranding the overlay.
#[test]
fn prefix_settings_apply_live_and_off_disarms() {
    let mut h = Harness::standard(1);
    h.ctrl('f');
    assert!(h.app.prefix_state.is_armed());

    let mut settings = crate::session::settings::Settings::default();
    settings.prefix.mode = crate::session::PrefixMode::Off;
    h.app.apply_live_settings(&settings);

    assert_eq!(h.app.prefix_settings.mode, crate::session::PrefixMode::Off);
    assert!(
        !h.app.prefix_state.is_armed(),
        "turning the leader off must clear the armed state, not strand it"
    );
    let painted = h.render();
    assert!(!painted.contains("go to session N"), "overlay is gone");

    // And a live rebind takes effect without a restart.
    let mut settings = crate::session::settings::Settings::default();
    settings.prefix.key = "ctrl+o".into();
    h.app.apply_live_settings(&settings);
    h.ctrl('o');
    assert!(h.app.prefix_state.is_armed(), "the new leader arms");
}

/// A rebind while the *old* leader is armed disarms too: still armed, the
/// new leader's first press would read as `<leader> <leader>` and go to the
/// agent instead of arming.
#[test]
fn prefix_rebind_while_armed_disarms_so_the_new_leader_arms() {
    let mut h = Harness::standard(1);
    h.ctrl('f');
    assert!(h.app.prefix_state.is_armed());

    let mut settings = crate::session::settings::Settings::default();
    settings.prefix.key = "ctrl+o".into();
    h.app.apply_live_settings(&settings);
    assert!(
        !h.app.prefix_state.is_armed(),
        "a rebind clears the state armed against the old leader"
    );

    h.ctrl('o');
    assert!(h.app.prefix_state.is_armed(), "the new leader arms");
}

#[test]
fn leader_digit_jumps_to_that_session_and_lands_in_the_terminal() {
    let mut h = Harness::standard(3);
    h.app.focus = InputFocus::SessionList;
    h.leader(KeyCode::Char('2'));
    assert_eq!(h.app.active_index, 1, "<leader> 2 selects the 2nd session");
    assert_eq!(
        h.app.focus,
        InputFocus::Terminal,
        "a jump lands in the terminal, like the Alt overlay"
    );

    // The digits number the list as *rendered*, so a manual reorder moves them.
    h.app.focus = InputFocus::SessionList;
    h.app.set_active_index(0);
    h.shift('j'); // move sessions[0] below sessions[1]
    assert_eq!(h.app.render_order_indices(), vec![1, 0, 2]);
    h.leader(KeyCode::Char('1'));
    assert_eq!(
        h.app.active_index, 1,
        "<leader> 1 is the top rendered row, not sessions[0]"
    );
}

/// The second-level route: `<leader> a` opens the blocked-only numbering and
/// a plain digit picks the Nth *blocked* session — a different numbering from
/// the all-session digits above.
#[test]
fn leader_a_then_digit_jumps_to_the_nth_blocked_session() {
    let mut h = Harness::standard(4);
    h.app.sessions[1].info.status = SessionStatus::Blocked;
    h.app.sessions[3].info.status = SessionStatus::Blocked;
    h.app.focus = InputFocus::SessionList;

    h.leader(KeyCode::Char('a'));
    h.key(KeyCode::Char('2'), KeyModifiers::NONE);
    assert_eq!(
        h.app.active_index, 3,
        "<leader> a 2 selects the 2nd blocked session, not the 2nd row"
    );
    assert_eq!(h.app.focus, InputFocus::Terminal);

    h.leader(KeyCode::Char('a'));
    h.key(KeyCode::Char('9'), KeyModifiers::NONE);
    assert_eq!(h.app.active_index, 3, "an out-of-range digit moves nothing");
    assert!(h
        .app
        .status_message
        .as_ref()
        .is_some_and(|m| m.text.contains("No session needing attention #9")));
}

#[test]
fn leader_out_of_range_digit_reports_instead_of_guessing() {
    let mut h = Harness::standard(2);
    h.leader(KeyCode::Char('9'));
    assert_eq!(h.app.active_index, 0, "no session moved");
    assert!(h
        .app
        .status_message
        .as_ref()
        .is_some_and(|m| m.text.contains("No session #9")));
}

#[test]
fn leader_esc_cancels_without_running_anything() {
    let mut h = Harness::standard(1);
    h.ctrl('f');
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(!h.app.prefix_state.is_armed());
    assert!(!h.app.show_info_panel, "nothing was dispatched");
}

/// Ctrl+C is the other cancel, and the one at risk: the priority Copy route
/// runs ahead of the leader, so it must not swallow the cancel — nor report
/// the sequence as a miss.
#[test]
fn leader_ctrl_c_cancels_without_running_anything() {
    let mut h = Harness::standard(1);
    h.ctrl('f');
    h.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!h.app.prefix_state.is_armed());
    assert!(!h.app.show_info_panel, "nothing was dispatched");
    assert!(
        !h.app
            .status_message
            .as_ref()
            .is_some_and(|m| m.text.contains("No leader binding")),
        "a cancel is not a missed binding"
    );
}

/// An unbound key after the leader must not reach the PTY: a leader press
/// plus a typo would otherwise inject a stray character into the agent.
#[test]
fn leader_unbound_key_reports_and_disarms() {
    let mut h = Harness::standard(1);
    h.leader(KeyCode::Char('§'));
    assert!(!h.app.prefix_state.is_armed());
    assert!(h
        .app
        .status_message
        .as_ref()
        .is_some_and(|m| m.text.contains("No leader binding")));
}

/// `F12` is the second leader, so it arms rather than toggling the perf HUD.
#[test]
fn second_leader_f12_arms_like_the_primary() {
    let mut h = Harness::standard(1);
    h.key(KeyCode::F(12), KeyModifiers::NONE);
    assert!(h.app.prefix_state.is_armed(), "F12 is prefix2");
    assert!(!h.app.show_perf_hud, "F12 no longer toggles the HUD");
    h.key(KeyCode::Char('b'), KeyModifiers::NONE);
    assert!(h.app.show_info_panel, "and its table is the same one");
}

/// `mode = "off"` restores the pre-leader behaviour exactly: `Ctrl+F` is inert
/// and `F12` goes back to the perf HUD.
#[test]
fn prefix_mode_off_disables_the_leader_and_returns_f12() {
    let mut h = Harness::standard(1);
    h.app.prefix_settings.mode = crate::session::PrefixMode::Off;
    h.ctrl('f');
    assert!(
        !h.app.prefix_state.is_armed(),
        "Ctrl+F does not arm when the leader is off"
    );
    assert!(
        matches!(h.app.modal, modals::Modal::SessionName(_)),
        "it is ForkSession's direct chord again"
    );
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    h.key(KeyCode::F(12), KeyModifiers::NONE);
    assert!(h.app.show_perf_hud, "F12 is the perf HUD again");
    h.ctrl('b');
    assert!(
        h.app.show_info_panel,
        "direct global chords still dispatch when the leader is off"
    );
}

/// The mode that pays for the feature: no global `Ctrl` chord dispatches, so
/// the whole namespace reaches the agent CLI. Pane-scoped keys still work.
#[test]
fn prefix_only_mode_blocks_direct_global_chords_but_keeps_scoped_ones() {
    let mut h = Harness::standard(2);
    h.app.prefix_settings.mode = crate::session::PrefixMode::PrefixOnly;

    h.ctrl('b');
    assert!(!h.app.show_info_panel, "Ctrl+B no longer toggles the panel");

    // …but the leader still reaches it.
    h.leader(KeyCode::Char('b'));
    assert!(h.app.show_info_panel, "<leader> b still works");

    // A pane-scoped single letter is untouched — it was never contested.
    h.app.focus = InputFocus::SessionList;
    h.app.set_active_index(0);
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    assert_eq!(h.app.active_index, 1, "session-list j still navigates");
}

/// The perf HUD moved off `F12` when `F12` became the second leader (see
/// `PrefixSettings::key2`), so it is reached as `<leader> m` — the only
/// route once the leader is on, which is the documented cost of `key2`.
#[test]
fn perf_hud_toggles_with_leader_m_and_activates_timing() {
    let mut h = Harness::standard(1);
    assert!(!h.app.perf_timing_active(), "timing is off by default");
    h.leader(KeyCode::Char('m'));
    assert!(h.app.show_perf_hud, "<leader> m opens the perf HUD");
    assert!(
        h.app.perf_timing_active(),
        "an open HUD switches timing collection on"
    );
    h.render(); // the overlay renders without disturbing the panes
    h.leader(KeyCode::Char('m'));
    assert!(!h.app.show_perf_hud, "<leader> m closes it again");
}

#[test]
fn perf_hud_feature_flag_disables_toggle_and_closes_overlay() {
    let mut h = Harness::standard(1);
    h.leader(KeyCode::Char('m'));
    assert!(h.app.show_perf_hud);
    // Disabling the live flag tears the overlay down and blocks the chord.
    let mut settings = crate::session::settings::Settings::default();
    settings.features.perf_hud = false;
    h.app.apply_live_settings(&settings);
    assert!(!h.app.show_perf_hud, "disabling the flag closes the HUD");
    h.leader(KeyCode::Char('m'));
    assert!(!h.app.show_perf_hud, "the chord toasts instead of toggling");
}

#[test]
fn perf_render_counter_tracks_painted_frames() {
    let mut h = Harness::standard(2);
    assert_eq!(h.app.perf_counters().frames_rendered, 0);
    h.render();
    h.render();
    h.render();
    assert_eq!(
        h.app.perf_counters().frames_rendered,
        3,
        "each view() paint bumps frames_rendered exactly once"
    );
}

#[test]
fn perf_terminal_render_locks_parser_once_per_frame() {
    // With an active session, the central pane locks its vt100 parser once per
    // painted frame (the O(1) scrollback read rides along, so it is not tracked
    // separately). Redraw throttling, not caching, bounds how often this runs.
    let mut h = Harness::standard(1);
    h.render();
    h.render();
    assert_eq!(
        h.app.perf_counters().parser_locks_render,
        2,
        "one parser lock per terminal frame"
    );
}

#[test]
fn perf_session_order_cached_across_idle_frames() {
    // The session-list ordering is status-independent, so once built it is
    // reused across frames whose grouping/nesting inputs didn't change. Three
    // paints with no session mutation must rebuild the order exactly once.
    let mut h = Harness::standard(3);
    h.render();
    h.render();
    h.render();
    assert_eq!(
        h.app.perf_counters().ordered_sessions_rebuilds,
        1,
        "the session order is cached: only the first frame rebuilds it"
    );
}

#[test]
fn perf_session_order_rebuilds_when_sessions_change() {
    // Adding a session changes the order signature, so the cache is invalidated
    // and the order rebuilt — exactly once for the change.
    let mut h = Harness::standard(2);
    h.render(); // builds the order (rebuild #1)
    h.render(); // cache hit, no rebuild
    assert_eq!(h.app.perf_counters().ordered_sessions_rebuilds, 1);

    // Mutate the session set, then repaint.
    let backend: Arc<dyn SessionBackend> = Arc::new(FakeBackend::stub());
    let provider: Arc<dyn AgentProvider> = Arc::new(GenericProvider::new(
        crate::agent::agent_config::builtin_registry()
            .default_agent()
            .unwrap()
            .clone(),
    ));
    h.app
        .sessions
        .push(Session::stub("session-new", &backend, &provider));
    h.render(); // signature changed → rebuild #2
    h.render(); // cache hit again
    assert_eq!(
        h.app.perf_counters().ordered_sessions_rebuilds,
        2,
        "a session-set change invalidates the cache exactly once"
    );
}

#[test]
fn perf_status_change_keeps_order_cache() {
    // The order is status-independent (ADR-P3): a session changing status must
    // NOT invalidate the cache — only grouping/ordering/nesting inputs do. This
    // pins the signature's field set; adding `status` to it would fail here.
    let mut h = Harness::standard(2);
    h.render(); // rebuild #1
    h.render(); // cache hit
    assert_eq!(h.app.perf_counters().ordered_sessions_rebuilds, 1);

    h.app.sessions[0].info.status = SessionStatus::Blocked;
    h.render(); // status changed, but order inputs did not → still a cache hit
    assert_eq!(
        h.app.perf_counters().ordered_sessions_rebuilds,
        1,
        "a status change must not rebuild the (status-independent) order"
    );
}

// ── Redraw throttling: the dirty-flag decision the render loop gates on ───────

#[test]
fn perf_first_frame_is_always_dirty() {
    // `needs_redraw` starts true so the very first loop iteration paints (the
    // smoke test and a real launch both rely on this).
    let h = Harness::standard(1);
    assert!(h.app.should_redraw(), "a freshly built App must paint once");
}

#[test]
fn perf_clean_state_skips_redraw() {
    // After a paint with nothing changed, the loop skips the (expensive) draw.
    let mut h = Harness::standard(1);
    h.app.mark_redrawn();
    assert!(
        !h.app.should_redraw(),
        "no input/output/forced-floor → no redraw"
    );
}

#[test]
fn perf_input_requests_redraw() {
    // Any key event re-dirties the UI so keypress-to-screen stays immediate.
    let mut h = Harness::standard(1);
    h.app.mark_redrawn();
    assert!(!h.app.should_redraw());
    h.ctrl('j'); // NextSession — goes through update()
    assert!(
        h.app.should_redraw(),
        "input must mark the UI dirty for the next frame"
    );
}

#[test]
fn perf_no_new_output_does_not_request_redraw() {
    // The lock-free output detector must not false-positive: with no reader
    // thread producing output, a second poll sees an unchanged signature and
    // leaves the UI clean.
    let mut h = Harness::standard(2);
    h.app.detect_output_redraw(); // prime the output-generation baseline
    h.app.mark_redrawn(); // clear any dirty from the first observation
    h.app.detect_output_redraw(); // no new output
    assert!(
        !h.app.should_redraw(),
        "unchanged output signature must not trigger a redraw"
    );
}

#[test]
fn perf_idle_iterations_skip_the_paint() {
    // Mimic the render loop's gate over several idle iterations (well within the
    // forced-redraw floor): the first paints, the rest are skipped.
    let mut h = Harness::standard(2);
    h.app.detect_output_redraw(); // prime output baseline
    let mut requested = 0u64;
    let mut skipped = 0u64;
    for _ in 0..5 {
        if h.app.should_redraw() {
            h.app.mark_redrawn();
            requested += 1;
        } else {
            h.app.note_redraw_skipped();
            skipped += 1;
        }
        h.app.detect_output_redraw(); // no new output between iterations
    }
    assert_eq!(requested, 1, "only the initial dirty frame paints");
    assert_eq!(skipped, 4, "idle iterations skip the expensive draw");
    assert_eq!(h.app.perf_counters().redraws_skipped, 4);
}

/// Disabling a live feature flag at runtime tears down whatever panel/view it
/// had left open (otherwise the panel keeps rendering with its tab/footer
/// affordance gone). Covers the `apply_live_settings` → `enforce_feature_visibility`
/// path the settings panel and config-reload both run.
#[tokio::test]
async fn disabling_a_live_feature_tears_down_its_open_surfaces() {
    let mut h = Harness::spawnable(1);
    let sid = h.app.sessions[0].info.id;

    // Open every live-gated surface, and park focus on the file viewer.
    h.app.show_info_panel = true;
    h.app.show_file_viewer = true;
    h.app.show_tasks_panel = true;
    h.app
        .session_terminal_views
        .insert(sid, TerminalView::Shell);
    open_minimal_review(&mut h);
    h.app.focus = InputFocus::FileViewer;

    // Flip the live UI feature flags off and re-apply (as the settings panel does).
    let mut settings = crate::session::settings::Settings::default();
    settings.features.info_panel = false;
    settings.features.file_viewer = false;
    settings.features.tasks = false;
    settings.features.shell_pane = false;
    settings.features.code_review = false;
    h.app.apply_live_settings(&settings);

    assert!(!h.app.show_info_panel, "info panel hidden");
    assert!(!h.app.show_file_viewer, "file viewer hidden");
    assert!(!h.app.show_tasks_panel, "tasks panel hidden");
    assert_eq!(
        h.app.session_terminal_views.get(&sid).copied(),
        Some(TerminalView::Claude),
        "shell view reverted to the agent view"
    );
    assert!(h.app.code_reviews.is_empty(), "open review closed");
    assert!(
        matches!(h.app.focus, InputFocus::SessionList),
        "focus moved off the now-hidden file viewer"
    );
}

/// `dispatch_action` partitions `Action` across several sub-dispatchers whose
/// final arm (`dispatch_scoped_pane_action`) is `unreachable!()`. A new `Action`
/// variant that isn't wired into any dispatcher would therefore panic at runtime
/// instead of failing to compile — this exercises every variant through the real
/// dispatch path so an unrouted action fails the suite loudly. A fresh harness
/// per action keeps the routing decision independent of accumulated side effects.
#[tokio::test]
async fn every_action_is_routed_by_dispatch_action() {
    for &action in crate::session::Action::all() {
        let mut h = Harness::standard(1);
        // The assertion is simply that this does not hit the `unreachable!()` in
        // `dispatch_scoped_pane_action` (or otherwise panic).
        let _ = h.app.dispatch_action(action);
    }
}

/// Install a minimal open+focused review on the harness (no git worktree
/// needed), for testing the view's key fall-through behavior.
fn open_minimal_review(h: &mut Harness) {
    use std::collections::HashSet;
    let sid = h.app.sessions[0].info.id;
    h.app.code_reviews.insert(
        sid,
        crate::app::code_review::CodeReviewState {
            session_id: sid,
            loading: false,
            repos: Vec::new(),
            multi: false,
            files: Vec::new(),
            comments: Vec::new(),
            reviewed_files: HashSet::new(),
            reviewed_hunks: HashSet::new(),
            fold_override: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            compose: None,
            side_by_side: false,
            click_side: None,
            h_scroll: 0,
            wrap: false,
            target: crate::app::code_review::ReviewTarget::Working,
            commits: Vec::new(),
            host: None,
            target_picker: None,
            search: None,
            filter: crate::app::code_review::ReviewFilter::default(),
            comment_picker: None,
            range: None,
            info_popup: None,
            context: crate::app::code_review::DEFAULT_CONTEXT,
        },
    );
    h.app.focus = InputFocus::CodeReview;
}

/// The review pane toggles shut on its own key, like every other pane: with a
/// review open and focused, pressing the bound chord (F7) again closes it and
/// moves focus away. Regression for the key being swallowed by the review's
/// own capture handler.
#[test]
fn review_toggle_key_closes_open_review() {
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    open_minimal_review(&mut h);

    h.key(KeyCode::F(7), KeyModifiers::NONE);

    assert!(
        h.app.active_review().is_none(),
        "pressing the review toggle again closes the open review"
    );
    assert_ne!(
        h.app.focus,
        InputFocus::CodeReview,
        "focus leaves the review when it closes"
    );
}

/// `/` opens find-in-diff (file-viewer pattern): typing jumps to the first
/// match, `Tab` commits, `n`/`N` step matches relative to the cursor, and `Esc`
/// clears the search before it closes the review.
#[test]
fn review_search_flow_finds_navigates_and_clears() {
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    let sid = h.app.sessions[0].info.id;
    // Two files: src/f0.rs, src/f1.rs (each one added line).
    h.app.code_reviews.insert(
        sid,
        crate::app::code_review::CodeReviewState::for_test(sid, 2),
    );
    h.app.focus = InputFocus::CodeReview;

    // `/` enters the search sub-mode (capturing keys).
    h.key(KeyCode::Char('/'), KeyModifiers::NONE);
    assert!(h
        .app
        .active_review()
        .and_then(|cr| cr.search.as_ref())
        .is_some_and(|s| s.editing));

    // Type ".rs" → matches both file headers; selection jumps to the first.
    for c in ['.', 'r', 's'] {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    let (first, second) = {
        let cr = h.app.active_review().unwrap();
        let s = cr.search.as_ref().unwrap();
        assert_eq!(s.matches.len(), 2, "both file headers match '.rs'");
        assert_eq!(cr.selected, s.matches[0], "jumps to the first match");
        (s.matches[0], s.matches[1])
    };

    // Enter (while typing) steps to the next match without leaving the input.
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(h.app.active_review().unwrap().selected, second);
    assert!(
        h.app
            .active_review()
            .unwrap()
            .search
            .as_ref()
            .unwrap()
            .editing
    );

    // Tab commits: still open, no longer editing.
    h.key(KeyCode::Tab, KeyModifiers::NONE);
    assert!(h
        .app
        .active_review()
        .and_then(|cr| cr.search.as_ref())
        .is_some_and(|s| !s.editing));

    // `n`/`N` step matches relative to the cursor (wrapping).
    h.key(KeyCode::Char('n'), KeyModifiers::NONE);
    assert_eq!(
        h.app.active_review().unwrap().selected,
        first,
        "n from the last match wraps to the first"
    );
    h.key(KeyCode::Char('N'), KeyModifiers::NONE);
    assert_eq!(
        h.app.active_review().unwrap().selected,
        second,
        "N from the first match wraps to the last"
    );

    // Esc clears the search but keeps the review open; a second Esc closes it.
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(h.app.active_review().is_some());
    assert!(h.app.active_review().unwrap().search.is_none());
    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(h.app.active_review().is_none());
}

/// The full annotate → send loop, key-driven end to end: a classified line
/// comment (`c`, Tab, Ctrl+S), a range comment (`V` + `j` + `c` — cycled to
/// `Question`), a review summary (`s`), the structured handoff compiled with
/// C-ids + quoted locators, then `e` closing the review, arming the re-review
/// nudge, and leaving the comments in SQLite for the reopen.
#[test]
fn review_annotate_send_loop_end_to_end() {
    use crate::session::review::{Classification, CommentAnchor, DiffLine, DiffLineKind, Side};
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    let sid = h.app.sessions[0].info.id;
    let mut cr = crate::app::code_review::CodeReviewState::for_test(sid, 1);
    // A second added line so a range can span (for_test files carry one).
    cr.files[0].hunks[0].lines.push(DiffLine {
        kind: DiffLineKind::Add,
        old_no: None,
        new_no: Some(2),
        text: "second".into(),
    });
    cr.rebuild_rows();
    h.app.code_reviews.insert(sid, cr);
    h.app.focus = InputFocus::CodeReview;

    // Rows: 0 FileHeader, 1 HunkHeader, 2 Line(new:1), 3 Line(new:2).
    // A classified line comment on new:1 — Tab cycles Note → Issue.
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    h.key(KeyCode::Char('c'), KeyModifiers::NONE);
    h.key(KeyCode::Tab, KeyModifiers::NONE);
    for ch in "needs a guard".chars() {
        h.key(KeyCode::Char(ch), KeyModifiers::NONE);
    }
    h.ctrl('s');

    // A range comment spanning both lines, cycled to Question.
    h.key(KeyCode::Char('V'), KeyModifiers::SHIFT);
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    h.key(KeyCode::Char('c'), KeyModifiers::NONE);
    for _ in 0..3 {
        h.key(KeyCode::Tab, KeyModifiers::NONE); // Note → Issue → Suggestion → Question
    }
    for ch in "why two?".chars() {
        h.key(KeyCode::Char(ch), KeyModifiers::NONE);
    }
    h.ctrl('s');

    // The review summary.
    h.key(KeyCode::Char('s'), KeyModifiers::NONE);
    for ch in "overall ok".chars() {
        h.key(KeyCode::Char(ch), KeyModifiers::NONE);
    }
    h.ctrl('s');

    // The compiled structured handoff carries the C-ids, the quoted locators
    // (for_test's line text is "x"), and the range span.
    let md = h.app.cr_review_markdown().expect("three comments compile");
    assert!(
        md.starts_with("Code review — 3 comments. Semantics:\n"),
        "preamble: {md}"
    );
    assert!(
        md.contains("### C1 [Issue] new:1\n> x\nneeds a guard\n"),
        "line record: {md}"
    );
    assert!(
        md.contains("### C2 [Question] new:1-2\n> x\n> second\nwhy two?\n"),
        "range record: {md}"
    );
    assert!(md.contains("\n## Review summary\n"), "summary: {md}");

    // `e` sends: the pane closes (the user watches the agent receive it) and
    // the re-review nudge is armed for this session.
    h.key(KeyCode::Char('e'), KeyModifiers::NONE);
    assert!(h.app.active_review().is_none(), "close-on-send");
    assert!(h.app.review_nudge_watch.contains_key(&sid));

    // Everything survives in SQLite for the reopen.
    let stored = h.app.db.list_review_comments(sid).unwrap();
    assert_eq!(stored.len(), 3);
    assert_eq!(stored[0].classification, Classification::Issue);
    assert_eq!(
        stored[1].anchor,
        CommentAnchor::Line {
            file: "src/f0.rs".into(),
            side: Side::New,
            line: 1,
            line_end: Some(2),
        }
    );
    assert_eq!(stored[2].anchor, CommentAnchor::Review);
}

/// Insert a review whose first diff line is `width` chars wide, so horizontal
/// scroll / wrap have something to act on. Returns the session id.
#[cfg(test)]
fn open_review_with_long_line(h: &mut Harness, width: usize) -> crate::session::SessionId {
    let sid = h.app.sessions[0].info.id;
    let mut cr = crate::app::code_review::CodeReviewState::for_test(sid, 1);
    cr.files[0].hunks[0].lines[0].text = "a".repeat(width);
    cr.rebuild_rows();
    h.app.code_reviews.insert(sid, cr);
    h.app.focus = InputFocus::CodeReview;
    sid
}

/// `Left`/`Right` (and `h`/`l`) scroll the diff body horizontally, clamped to
/// the longest line; `w` toggles wrap and resets the offset; scroll is a no-op
/// while wrapped.
#[test]
fn review_horizontal_scroll_and_wrap_toggle() {
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    open_review_with_long_line(&mut h, 300);

    // Right scrolls the body (step 8); Left scrolls back and clamps at 0.
    h.key(KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(h.app.active_review().unwrap().h_scroll, 8);
    h.key(KeyCode::Char('l'), KeyModifiers::NONE);
    assert_eq!(
        h.app.active_review().unwrap().h_scroll,
        16,
        "`l` also scrolls right"
    );
    for _ in 0..10 {
        h.key(KeyCode::Left, KeyModifiers::NONE);
    }
    assert_eq!(
        h.app.active_review().unwrap().h_scroll,
        0,
        "Left clamps at 0"
    );

    // A big jump clamps to the longest line (max_line_width - 1 = 299).
    for _ in 0..100 {
        h.key(KeyCode::Right, KeyModifiers::NONE);
    }
    assert_eq!(
        h.app.active_review().unwrap().h_scroll,
        299,
        "scroll clamps to the widest line"
    );

    // `w` turns on wrap and resets the horizontal offset; while wrapped, scroll
    // is a no-op.
    h.key(KeyCode::Char('w'), KeyModifiers::NONE);
    {
        let cr = h.app.active_review().unwrap();
        assert!(cr.wrap, "`w` enables wrap");
        assert_eq!(cr.h_scroll, 0, "enabling wrap resets h_scroll");
    }
    h.key(KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(
        h.app.active_review().unwrap().h_scroll,
        0,
        "horizontal scroll is a no-op while wrapped"
    );

    // `w` again turns wrap off.
    h.key(KeyCode::Char('w'), KeyModifiers::NONE);
    assert!(!h.app.active_review().unwrap().wrap);
}

/// Entering side-by-side pins the horizontal offset to 0 (h-scroll is
/// unified-only) and scroll stays a no-op there.
#[test]
fn review_side_by_side_disables_horizontal_scroll() {
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    open_review_with_long_line(&mut h, 300);

    h.key(KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(h.app.active_review().unwrap().h_scroll, 8);

    // `v` → side-by-side resets the offset.
    h.key(KeyCode::Char('v'), KeyModifiers::NONE);
    {
        let cr = h.app.active_review().unwrap();
        assert!(cr.side_by_side);
        assert_eq!(cr.h_scroll, 0, "side-by-side resets h_scroll");
    }
    h.key(KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(
        h.app.active_review().unwrap().h_scroll,
        0,
        "no horizontal scroll in side-by-side"
    );
}

/// A review is per-session like the shell view: switching to another session
/// hides it (and demotes the central focus), and switching back shows it again
/// — the state is preserved, not torn down.
#[test]
fn review_persists_per_session_across_switches() {
    let mut h = Harness::new(STD_COLS, STD_ROWS, 2);
    h.app.active_index = 0;
    open_minimal_review(&mut h); // review open + focused on session 0
    h.render();
    assert!(h.app.active_review().is_some());
    assert_eq!(h.app.focus, InputFocus::CodeReview);

    // Switch to session 1 (no review): it's hidden and focus drops off the
    // review (synced on render).
    h.app.active_index = 1;
    h.render();
    assert!(
        h.app.active_review().is_none(),
        "the other session has no review"
    );
    assert_ne!(
        h.app.focus,
        InputFocus::CodeReview,
        "focus leaves the review when its session isn't active"
    );

    // Switch back to session 0: the review is still there and re-focused.
    h.app.active_index = 0;
    h.render();
    assert!(
        h.app.active_review().is_some(),
        "session 0's review is preserved across the round-trip"
    );
    assert_eq!(
        h.app.focus,
        InputFocus::CodeReview,
        "returning to the review session re-focuses it"
    );
}

/// Hovering a code-review footer button brightens its fill to `accent_bright`,
/// exactly like the global footer and modal buttons. Regression: review footer
/// buttons (recorded as `ClickAction::ReviewButton`) were left out of the hover
/// highlight, so they never lit up under the pointer.
#[test]
fn hovering_review_footer_button_brightens_it() {
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    open_minimal_review(&mut h);
    h.render();
    let r = h
        .app
        .click_targets
        .iter()
        .find(|t| matches!(t.action, ClickAction::ReviewButton(_)))
        .map(|t| t.rect)
        .expect("review footer buttons recorded");
    h.app.update(AppMessage::MouseMove { x: r.x, y: r.y });
    h.render();
    let buf = h.terminal.backend().buffer();
    assert_eq!(
        buf[(r.x, r.y)].bg,
        crate::ui::theme::Theme::accent_bright(),
        "hovered review footer button should brighten to accent_bright"
    );
}

/// Clicking a diff row in the main review pane focuses it (regression: the
/// `ReviewRow` click selected the row but never set `InputFocus::CodeReview`,
/// so a click while another pane was focused left focus elsewhere — the
/// whole-pane `FocusPane` fallback is recorded after the row targets and never
/// wins on a row hit).
#[test]
fn clicking_review_row_focuses_the_pane() {
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    let sid = h.app.sessions[0].info.id;
    h.app.code_reviews.insert(
        sid,
        crate::app::code_review::CodeReviewState::for_test(sid, 2),
    );
    h.app.focus = InputFocus::CodeReview;
    h.render();
    // Move focus off the review, as if the session list were active.
    h.app.focus = InputFocus::SessionList;
    let r = h
        .app
        .click_targets
        .iter()
        .find(|t| matches!(t.action, ClickAction::ReviewRow(_)))
        .map(|t| t.rect)
        .expect("review diff rows recorded as click targets");
    h.app.update(AppMessage::MouseClick {
        x: r.x,
        y: r.y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(
        h.app.focus,
        InputFocus::CodeReview,
        "clicking a diff row focuses the review pane"
    );
}

/// A click in the paired side-by-side layout records which column (old | new)
/// it hit, so a follow-up comment attaches to that side; a later column-less
/// select (scrollbar drag) clears it.
#[test]
fn side_by_side_click_records_column_side() {
    use crate::session::review::Side;
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    let sid = h.app.sessions[0].info.id;
    let mut cr = crate::app::code_review::CodeReviewState::for_test(sid, 1);
    cr.side_by_side = true;
    cr.rebuild_rows();
    h.app.code_reviews.insert(sid, cr);
    h.app.focus = InputFocus::CodeReview;
    h.render();
    let r = h
        .app
        .click_targets
        .iter()
        .find(|t| matches!(t.action, ClickAction::ReviewRow(_)))
        .map(|t| t.rect)
        .expect("review diff rows recorded as click targets");

    // Right column → the New side is recorded for that row.
    h.app.update(AppMessage::MouseClick {
        x: r.x + r.width - 1,
        y: r.y,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        matches!(
            h.app.active_review().unwrap().click_side,
            Some((_, Side::New))
        ),
        "a right-column click records the New side"
    );

    // Left column → the Old side.
    h.app.update(AppMessage::MouseClick {
        x: r.x,
        y: r.y,
        modifiers: KeyModifiers::NONE,
    });
    assert!(
        matches!(
            h.app.active_review().unwrap().click_side,
            Some((_, Side::Old))
        ),
        "a left-column click records the Old side"
    );

    // A column-less select (scrollbar drag / cr_select_row) clears it.
    let sel = h.app.active_review().unwrap().selected;
    h.app.cr_select_row(sel);
    assert!(
        h.app.active_review().unwrap().click_side.is_none(),
        "a column-less select clears the recorded click side"
    );
}

/// The review-target picker is mouse-driven too: clicking one of its entries
/// dispatches the switch to that target, mirroring the keyboard Enter path.
/// The rebuild itself runs on a background worker (ADR-P8) — its application
/// is covered by `perf_review_build_result_applied_via_poll` — so this asserts
/// the dispatch side: picker closed, loading state on, build handed off.
#[tokio::test]
async fn clicking_review_target_entry_switches_target() {
    use crate::app::code_review::ReviewTarget;
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    let sid = h.app.sessions[0].info.id;
    h.app.code_reviews.insert(
        sid,
        crate::app::code_review::CodeReviewState::for_test(sid, 2),
    );
    h.app.focus = InputFocus::CodeReview;
    // The picker opens on Branch (the for_test default); it offers Working +
    // Branch entries.
    h.app.cr_open_target_picker();
    assert_eq!(h.app.active_review().unwrap().target, ReviewTarget::Branch);
    h.render();

    // Find the "Working" entry's hitbox (index 0) and click it.
    let r = h
        .app
        .click_targets
        .iter()
        .find(|t| matches!(t.action, ClickAction::ReviewTarget(0)))
        .map(|t| t.rect)
        .expect("target-picker entries recorded as click targets");
    h.app.update(AppMessage::MouseClick {
        x: r.x,
        y: r.y,
        modifiers: KeyModifiers::NONE,
    });

    let cr = h.app.active_review().unwrap();
    assert_eq!(
        cr.target,
        ReviewTarget::Branch,
        "the target only switches once the background build lands"
    );
    assert!(cr.loading, "the click enters the loading state");
    assert!(
        cr.target_picker.is_none(),
        "selecting a target closes the picker"
    );
    assert_eq!(
        h.app.perf_counters().review_builds_dispatched,
        1,
        "the rebuild was handed to the background worker"
    );
}

/// Global overlay/panel toggles fall through the review's key capture so they
/// stay reachable while a review is open (regression: the capture handler
/// swallowed them). The review itself stays open.
#[test]
fn info_panel_toggles_while_review_is_open() {
    let mut h = Harness::new(STD_COLS, STD_ROWS, 1);
    open_minimal_review(&mut h);
    assert!(!h.app.show_info_panel);

    h.key(KeyCode::F(2), KeyModifiers::NONE);

    assert!(
        h.app.show_info_panel,
        "F2 toggles the info panel even while the review is focused"
    );
    assert!(
        h.app.active_review().is_some(),
        "toggling the info panel leaves the review open"
    );
}

// ── Remote-hook status events (control-mode subscription → hook columns) ──────

/// The backend queued a remote-hook event for a session's pane: one refresh
/// drains it into the hook columns and the derived status reflects it — the
/// remote analogue of a local `friring-cli session signal`.
#[test]
fn remote_hook_event_drives_session_status() {
    let backend = Arc::new(FakeBackend::stub());
    let mut h = Harness::with_backend(STD_COLS, STD_ROWS, 2, backend.clone());
    h.app.sessions[0].info.backend_id = Some("%5".into());
    h.app.sessions[1].info.backend_id = Some("%9".into());
    h.app.save_state(); // hook columns update persisted rows

    backend.push_hook_event("%5", "working");
    h.app.refresh_session_statuses();

    // Stub sessions have fresh output, so `working` isn't quiescence-demoted.
    assert_eq!(h.app.sessions[0].info.status, SessionStatus::Working);
    assert_eq!(
        h.app.sessions[1].info.status,
        SessionStatus::Idle,
        "the other session is untouched"
    );
    let rows = h.app.db.load_hook_states().unwrap();
    assert_eq!(
        rows.get(&h.app.sessions[0].info.id)
            .and_then(|r| r.state.as_deref()),
        Some("working"),
        "the event is persisted through set_hook_state"
    );
}

/// Events that don't resolve to a session never touch the DB (an unknown pane
/// is *parked* for the adoption retry — see
/// `remote_hook_event_parked_until_session_adopted` — not applied), and a
/// non-allow-listed state (the value is remote-controlled free text) is
/// dropped outright.
#[test]
fn remote_hook_event_ignores_unmatched_and_invalid() {
    let backend = Arc::new(FakeBackend::stub());
    let mut h = Harness::with_backend(STD_COLS, STD_ROWS, 1, backend.clone());
    h.app.sessions[0].info.backend_id = Some("%5".into());
    h.app.save_state();

    backend.push_hook_event("%99", "working"); // no such pane
    backend.push_hook_event("%5", "rm -rf /"); // not an allowed state
    h.app.refresh_session_statuses();

    assert_eq!(h.app.sessions[0].info.status, SessionStatus::Idle);
    assert!(
        h.app
            .db
            .load_hook_states()
            .unwrap()
            .values()
            .all(|r| r.state.is_none()),
        "neither event may reach the hook columns"
    );
}

/// An event for a pane no session claims *yet* is parked and re-applied once
/// the session appears: the subscription's initial catch-up report lands while
/// the background restore is still adopting that host's windows, and dropping
/// it would lose e.g. a `done` set while the TUI was closed.
#[test]
fn remote_hook_event_parked_until_session_adopted() {
    let backend = Arc::new(FakeBackend::stub());
    let mut h = Harness::with_backend(STD_COLS, STD_ROWS, 1, backend.clone());
    let id = h.app.sessions[0].info.id;

    backend.push_hook_event("%5", "working");
    h.app.refresh_session_statuses(); // no session owns %5 yet → parked

    h.app.sessions[0].info.backend_id = Some("%5".into());
    h.app.save_state();
    h.app.refresh_session_statuses(); // nothing new pushed — the parked event applies

    let rows = h.app.db.load_hook_states().unwrap();
    assert_eq!(
        rows.get(&id).and_then(|r| r.state.as_deref()),
        Some("working"),
        "the pre-adoption event must survive to the adopting tick"
    );
}

/// Two transitions for one pane in a single drained batch (`working` then
/// `done`, e.g. queued while the main thread stalled) must both land: deduping
/// the second against the stale pre-batch cache would swallow the `done` and
/// leave the session spinning on `working`.
#[test]
fn remote_hook_batch_applies_both_transitions() {
    let backend = Arc::new(FakeBackend::stub());
    let mut h = Harness::with_backend(STD_COLS, STD_ROWS, 1, backend.clone());
    h.app.sessions[0].info.backend_id = Some("%5".into());
    h.app.save_state();
    let id = h.app.sessions[0].info.id;

    // A previous turn ended `done`, absorbed into the cache.
    backend.push_hook_event("%5", "done");
    h.app.refresh_session_statuses();

    backend.push_hook_event("%5", "working");
    backend.push_hook_event("%5", "done");
    h.app.refresh_session_statuses();

    let rows = h.app.db.load_hook_states().unwrap();
    assert_eq!(
        rows.get(&id).and_then(|r| r.state.as_deref()),
        Some("done"),
        "the batch's final transition must not be swallowed by the stale cache"
    );
}

/// A re-report of the current state (the subscription re-sends the pane
/// option's value on reconnect/TUI restart) must not re-stamp `state_at` —
/// otherwise an already-acknowledged `done` resurrects as unseen and re-fires
/// its OS notification on every restart.
#[test]
fn remote_hook_event_dedupes_repeated_state() {
    let backend = Arc::new(FakeBackend::stub());
    let mut h = Harness::with_backend(STD_COLS, STD_ROWS, 1, backend.clone());
    h.app.sessions[0].info.backend_id = Some("%5".into());
    h.app.save_state();
    let id = h.app.sessions[0].info.id;

    backend.push_hook_event("%5", "done");
    h.app.refresh_session_statuses();
    let first_at = h.app.db.load_hook_states().unwrap()[&id].state_at;
    assert!(first_at.is_some());

    std::thread::sleep(std::time::Duration::from_millis(5));
    backend.push_hook_event("%5", "done");
    h.app.refresh_session_statuses();
    let second_at = h.app.db.load_hook_states().unwrap()[&id].state_at;
    assert_eq!(
        first_at, second_at,
        "an identical re-report must not re-stamp state_at"
    );
}

// ── Tick-driven behavior: timers, debounce, redraw floor ─────────────────────
//
// These drive `App::tick_core` (the deterministic half of the event loop's
// tick) with the clock fast-forwarded via `Harness::advance`, so every
// wall-clock-gated behavior is asserted without sleeping.

#[test]
fn status_message_expires_after_timeout_via_tick() {
    let mut h = Harness::standard(1);
    h.app.set_status(StatusLevel::Info, "transient note");
    assert!(h.app.status_message.is_some());

    h.tick();
    assert!(
        h.app.status_message.is_some(),
        "a fresh message survives a tick"
    );

    h.advance(STATUS_MESSAGE_TIMEOUT).tick();
    assert!(
        h.app.status_message.is_none(),
        "the tick clears an expired status message"
    );
}

#[test]
fn pending_delete_finalizes_after_undo_window() {
    let mut h = Harness::standard(2);
    h.ctrl('d'); // DeleteSession (soft) — starts the undo window
    assert!(h.app.pending_delete.is_some());

    // Inside the window the delete stays pending (undoable).
    h.advance(UNDO_TIMEOUT - std::time::Duration::from_secs(1))
        .tick();
    assert!(
        h.app.pending_delete.is_some(),
        "still undoable inside the window"
    );

    h.advance(std::time::Duration::from_secs(2)).tick();
    assert!(
        h.app.pending_delete.is_none(),
        "the expired window finalizes the delete"
    );

    h.ctrl('z'); // UndoDelete — too late now
    assert_eq!(
        h.app.sessions.len(),
        1,
        "a finalized delete can no longer be undone"
    );
}

#[test]
fn global_search_content_scan_waits_for_debounce() {
    let mut h = Harness::standard(2);
    h.feed_output(1, b"a unique zebra-crossing appears\r\n");

    h.ctrl('/'); // GlobalSearch
    for c in "zebra".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    let content_hit = |app: &App| {
        app.global_search
            .results
            .iter()
            .any(|r| r.kind == search::SearchKind::Session && r.snippet.is_some())
    };

    h.tick();
    assert!(
        !content_hit(&h.app),
        "the expensive buffer scan is debounced — no content hit immediately"
    );
    assert!(h.app.global_search.content_dirty, "a scan is pending");

    h.advance(std::time::Duration::from_millis(
        search::CONTENT_DEBOUNCE_MS + 10,
    ))
    .tick();
    assert!(
        content_hit(&h.app),
        "once the query settles, the tick scans session buffers"
    );
    assert!(!h.app.global_search.content_dirty);
}

#[test]
fn global_search_files_match_from_the_cached_index() {
    // Files are matched against the index snapshotted at open — typing must
    // never walk the filesystem (the old per-keystroke walk was the strip's
    // dominant latency). Delivery through the task seam stands in for the
    // off-thread walk, keeping the test deterministic.
    let mut h = Harness::standard(1);
    h.ctrl('/');
    h.key(KeyCode::Tab, KeyModifiers::NONE); // widen past the session switcher
    for c in "zanzi".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    let file_hit = |app: &App| {
        app.global_search
            .results
            .iter()
            .any(|r| r.kind == search::SearchKind::File && r.label == "zanzibar.txt")
    };
    assert!(
        !file_hit(&h.app),
        "no index delivered yet ⇒ no file results"
    );

    let tx = h.app.global_search.file_index_task.start();
    tx.send(vec![search::FileIndexEntry {
        root: "/repo".into(),
        path: "/repo/zanzibar.txt".into(),
        name: "zanzibar.txt".into(),
        name_lc: "zanzibar.txt".into(),
    }])
    .unwrap();
    h.tick();
    assert!(
        file_hit(&h.app),
        "once the walk delivers, file matches fold into the open results"
    );
}

#[test]
fn global_search_matches_session_cwd_and_every_branch() {
    let mut h = Harness::standard(1);
    h.app.sessions[0].info.cwd = Some("/mnt/velociraptor-repo".into());
    h.app.sessions[0].info.worktrees = vec![
        crate::session::WorktreeInfo {
            repo_path: "/r".into(),
            worktree_path: "/w1".into(),
            branch: "main".into(),
        },
        crate::session::WorktreeInfo {
            repo_path: "/r".into(),
            worktree_path: "/w2".into(),
            branch: "feature/quokka-lift".into(),
        },
    ];

    let session_hit = |h: &mut Harness, query: &str| {
        h.ctrl('/');
        for c in query.chars() {
            h.key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        let hit = h
            .app
            .global_search
            .results
            .iter()
            .any(|r| r.kind == search::SearchKind::Session);
        h.key(KeyCode::Esc, KeyModifiers::NONE);
        hit
    };

    assert!(
        session_hit(&mut h, "velociraptor"),
        "the session's cwd is indexed (FEATURES.md promises all four fields)"
    );
    assert!(
        session_hit(&mut h, "quokka"),
        "every worktree branch is indexed, not just the first"
    );
}

#[test]
fn forced_redraw_floor_repaints_after_interval() {
    let mut h = Harness::standard(1);
    h.app.mark_redrawn();
    assert!(
        !h.app.should_redraw(),
        "clean state right after a paint — no redraw needed"
    );

    h.advance(FORCE_REDRAW_INTERVAL);
    assert!(
        h.app.should_redraw(),
        "the forced-redraw floor repaints time-driven UI"
    );
}

// ── Regressions the monkey test originally caught ────────────────────────────

#[test]
fn global_search_on_short_terminal_does_not_panic_session_resize() {
    // Historically the bottom-strip search shrank the content area to zero
    // rows on short terminals, and `Session::resize` had to clamp before
    // vt100's `set_size` (which underflows on 0). The popup floats now, but
    // this still guards opening + rendering the search on a tiny terminal.
    let mut h = Harness::new(30, 8, 1);
    h.render();
    h.ctrl('/'); // GlobalSearch
    h.render();
    assert!(h.app.global_search.active);
}

#[test]
fn narrow_resize_rescues_task_editor_focus() {
    // Shrinking below 120 cols hides the tasks panel; focus must leave the
    // *editor* too, or it keeps capturing every key for an invisible surface.
    let mut h = Harness::standard(1);
    h.ctrl('w'); // FocusTasks
    h.key(KeyCode::Char('n'), KeyModifiers::NONE); // new task → TaskEditor
    assert!(matches!(h.app.focus, InputFocus::TaskEditor));

    h.resize(100, 40);
    assert!(!h.app.show_tasks_panel, "narrow layout hides the panel");
    assert!(
        matches!(h.app.focus, InputFocus::SessionList),
        "focus is rescued off the hidden panel's editor"
    );
    h.render();
}

#[test]
fn stale_search_result_cannot_restore_focus_to_hidden_tasks() {
    let mut h = Harness::new(160, 40, 2);
    h.ctrl('w');
    h.ctrl('/');
    assert_eq!(h.app.global_search.results[0].label, "session-1");

    let stale_id = h.app.sessions[1].info.id;
    h.app.apply_removed_sessions(vec![stale_id]);
    h.resize(50, 40);
    h.key(KeyCode::Enter, KeyModifiers::NONE);

    assert!(!h.app.show_tasks_panel);
    assert!(
        !matches!(h.app.focus, InputFocus::TaskList | InputFocus::TaskEditor),
        "a stale result must fall back to a visible surface"
    );
}

#[test]
fn cancelling_search_after_a_narrow_resize_keeps_tasks_hidden() {
    let mut h = Harness::new(160, 40, 1);
    h.ctrl('w');
    h.ctrl('/');
    h.resize(50, 40);
    h.key(KeyCode::Esc, KeyModifiers::NONE);

    assert!(!h.app.show_tasks_panel);
    assert!(
        !matches!(h.app.focus, InputFocus::TaskList | InputFocus::TaskEditor),
        "cancel must not resurrect focus on a panel the new layout cannot show"
    );
}

// ── Injected agent output: the PTY seam ──────────────────────────────────────

#[test]
fn injected_output_marks_redraw_and_renders() {
    let mut h = Harness::standard(1);
    // Sync the output-change detector, then settle to a clean state.
    h.app.detect_output_redraw();
    h.app.mark_redrawn();
    h.app.detect_output_redraw();
    assert!(!h.app.should_redraw(), "no new output ⇒ no repaint");

    h.feed_output(0, b"MARKER-7f3a output line\r\n");
    h.app.detect_output_redraw();
    assert!(h.app.should_redraw(), "new output marks the UI dirty");

    let screen = h.render();
    assert!(
        screen.contains("MARKER-7f3a"),
        "injected output reaches the rendered terminal pane:\n{screen}"
    );
}

#[tokio::test]
async fn injected_shell_output_marks_redraw_and_renders() {
    // Regression: the output-change detector (ADR-P1) summed only the *agent*
    // panes' `last_output_at`, so a shell keystroke's echo never marked the UI
    // dirty — it painted on the next keypress or the 250 ms forced-redraw
    // floor. Measured on the real pipeline: ~280 ms per echoed character in
    // the shell tab vs ~40 ms on the agent tab.
    let mut h = Harness::spawnable(1);
    h.ctrl('t'); // ToggleShell — spawns the shell pane and shows the shell view
    assert!(h.app.sessions[0].shell_pane.is_some());

    // Sync the output-change detector, then settle to a clean state.
    h.app.detect_output_redraw();
    h.app.mark_redrawn();
    h.app.detect_output_redraw();
    assert!(!h.app.should_redraw(), "no new output ⇒ no repaint");

    h.app.sessions[0]
        .shell_pane
        .as_ref()
        .unwrap()
        .feed_output_for_test(b"SHELL-ECHO-42\r\n");
    h.app.detect_output_redraw();
    assert!(
        h.app.should_redraw(),
        "shell-pane output marks the UI dirty (echo repaints immediately)"
    );

    let screen = h.render();
    assert!(
        screen.contains("SHELL-ECHO-42"),
        "injected shell output reaches the rendered shell pane:\n{screen}"
    );
}

#[test]
fn osc_title_and_bell_reach_the_session() {
    let mut h = Harness::standard(1);

    h.feed_output(0, b"\x1b]0;Reticulating splines\x07");
    assert_eq!(
        h.app.sessions[0].agent_title().as_deref(),
        Some("Reticulating splines"),
        "an OSC 0 title lands in the session's activity text"
    );

    assert!(!h.app.sessions[0].needs_attention());
    h.feed_output(0, b"\x07");
    assert!(
        h.app.sessions[0].needs_attention(),
        "a BEL raises the attention flag"
    );
}

#[test]
fn split_utf8_output_chunks_render_intact() {
    // The reader loop protects vt100 from mid-codepoint chunks with a carry
    // buffer; `feed_output` bypasses the reader, so this documents that a test
    // feeding whole-codepoint chunks renders multi-byte text correctly (the
    // carry logic itself is unit-tested via `utf8_ready_prefix_len`).
    let mut h = Harness::standard(1);
    h.feed_output(0, "boîte — ünïcode ✓\r\n".as_bytes());
    let screen = h.render();
    assert!(
        screen.contains("boîte — ünïcode ✓"),
        "multi-byte output renders intact:\n{screen}"
    );
}

// ── Ctrl+O editor: terminal vs GUI routing ─────────────────────────────

#[test]
fn terminal_editor_stages_pending_run_for_main_loop() {
    // `ttt` is a known terminal editor, so Ctrl+O must NOT fire a null-stdio
    // spawn (which would die with no TTY). Instead it stages an invocation for
    // the main loop to run with a real TTY (popup/suspend).
    let mut h = Harness::standard(0);
    h.app.db.set_editor_command("ttt").unwrap();
    h.app.launch_editor(
        &[std::path::PathBuf::from("/tmp/repo")],
        Some("paths".to_string()),
    );
    let inv = h
        .app
        .take_pending_editor_run()
        .expect("ttt stages a terminal-editor run");
    assert_eq!(inv.program, "ttt");
    assert_eq!(inv.args, ["/tmp/repo".to_string()]);
    // One-shot: a second drain yields nothing.
    assert!(h.app.take_pending_editor_run().is_none());
}

#[test]
fn editor_mode_terminal_forces_tty_even_for_a_gui_editor() {
    // `code` is normally GUI (detached), but `editor mode terminal` overrides:
    // it must stage a terminal run too, keeping extra flags before the paths.
    let mut h = Harness::standard(0);
    h.app.db.set_editor_command("code --wait").unwrap();
    h.app
        .db
        .set_editor_mode(crate::session::settings::EditorMode::Terminal)
        .unwrap();
    h.app.launch_editor(
        &[
            std::path::PathBuf::from("/tmp/repo"),
            std::path::PathBuf::from("/other"),
        ],
        Some("paths".to_string()),
    );
    let inv = h
        .app
        .take_pending_editor_run()
        .expect("terminal mode forces the TTY path even for `code`");
    assert_eq!(inv.program, "code");
    assert_eq!(
        inv.args,
        [
            "--wait".to_string(),
            "/tmp/repo".to_string(),
            "/other".to_string()
        ]
    );
}

// ── Invariant tripwires + deterministic monkey test ──────────────────────────

/// Structural invariants that must hold after *any* event, in any order. The
/// monkey test checks these after every step; when a "weird TUI behavior" is
/// reduced to a rule ("focus never rests on a hidden pane"), add it here and
/// the monkey hunts for a sequence that breaks it.
fn assert_invariants(app: &App, ctx: &str) {
    assert!(
        app.sessions.is_empty() || app.active_index < app.sessions.len(),
        "[{ctx}] active_index {} out of bounds ({} sessions)",
        app.active_index,
        app.sessions.len()
    );
    let unique_ids: std::collections::HashSet<_> = app.sessions.iter().map(|s| s.info.id).collect();
    assert_eq!(
        unique_ids.len(),
        app.sessions.len(),
        "[{ctx}] live sessions contain a duplicate SessionId"
    );
    assert!(
        app.task_ui.filtered_task_indices.is_empty()
            || app.task_ui.task_panel_index < app.task_ui.filtered_task_indices.len(),
        "[{ctx}] task panel selection out of bounds"
    );
    assert!(
        app.automation_ui.cached_automations.is_empty()
            || app.automation_ui.automation_panel_index
                < app.automation_ui.cached_automations.len(),
        "[{ctx}] automation pane selection out of bounds"
    );

    // Panel visibility never outlives its feature flag.
    assert!(
        !app.show_tasks_panel || app.features.tasks,
        "[{ctx}] tasks panel shown with the feature disabled"
    );
    assert!(
        !app.show_file_viewer || app.features.file_viewer,
        "[{ctx}] file viewer shown with the feature disabled"
    );

    // Focus only ever rests on a surface that exists.
    match app.focus {
        InputFocus::TaskList | InputFocus::TaskEditor => assert!(
            app.features.tasks && app.show_tasks_panel,
            "[{ctx}] focus {:?} but the tasks panel is hidden",
            app.focus
        ),
        InputFocus::FileViewer => assert!(
            app.show_file_viewer,
            "[{ctx}] focus on a hidden file viewer"
        ),
        InputFocus::GlobalSearch => assert!(
            app.global_search.active,
            "[{ctx}] focus on a closed search strip"
        ),
        InputFocus::CodeReview | InputFocus::ReviewFiles => {
            assert!(
                app.features.code_review,
                "[{ctx}] review focus with the feature disabled"
            );
            assert!(
                app.active_review().is_some(),
                "[{ctx}] focus {:?} but the active session has no open review",
                app.focus
            );
        }
        InputFocus::Automations
        | InputFocus::AutomationEditor
        | InputFocus::AutomationRunHistory => {
            assert!(
                app.features.automations,
                "[{ctx}] automations focus with the feature disabled"
            );
            // The pane lives in the left column, and the editor / run history
            // exit back into it — so the whole context needs that column.
            assert!(
                app.show_session_list,
                "[{ctx}] focus {:?} but the left column is collapsed",
                app.focus
            );
        }
        InputFocus::CcActivity | InputFocus::CcActivityTree => {
            assert!(
                app.features.cc_activity,
                "[{ctx}] activity focus with the feature disabled"
            );
            assert!(
                app.active_cc_activity().is_some(),
                "[{ctx}] focus {:?} but the active session has no open activity view",
                app.focus
            );
        }
        InputFocus::SessionList => assert!(
            app.show_session_list,
            "[{ctx}] focus on a collapsed session list"
        ),
        InputFocus::Terminal => {}
    }

    if app.global_search.active {
        assert!(
            app.features.global_search,
            "[{ctx}] search strip active with the feature disabled"
        );
    }
}

/// Deterministic pseudo-random stream (an LCG — no dev-dependency, and a
/// failing seed reproduces exactly).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// `Ctrl` chords the monkey may press. Excluded on purpose:
/// `n` (repo picker `Enter` can reach real-git branch listing on the dev
/// machine), `o` (spawns `$EDITOR`), `v` (reads the system clipboard),
/// `q` (quit is a terminal state with nothing to fuzz behind it).
const MONKEY_CTRL: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'j', 'k', 'l', 'p', 'r', 's', 't', 'u', 'w', 'x', 'y',
    'z', '/', ',',
];

/// Plain (unmodified) keys the monkey may press: pane-scoped letters, digits,
/// and the structural navigation/editing keys.
const MONKEY_KEYS: &[KeyCode] = &[
    KeyCode::Char('j'),
    KeyCode::Char('k'),
    KeyCode::Char('h'),
    KeyCode::Char('l'),
    KeyCode::Char('g'),
    KeyCode::Char('r'),
    KeyCode::Char('d'),
    KeyCode::Char('e'),
    KeyCode::Char('n'),
    KeyCode::Char('s'),
    KeyCode::Char('w'),
    KeyCode::Char('y'),
    KeyCode::Char('/'),
    KeyCode::Char(' '),
    KeyCode::Char('1'),
    KeyCode::Char('9'),
    KeyCode::Esc,
    KeyCode::Enter,
    KeyCode::Tab,
    KeyCode::Backspace,
    KeyCode::Up,
    KeyCode::Down,
    KeyCode::Left,
    KeyCode::Right,
    KeyCode::PageUp,
    KeyCode::PageDown,
    KeyCode::Home,
    KeyCode::End,
];

/// Realistic agent-output chunks: plain text, SGR colour, OSC title, BEL,
/// OSC 9 notification, unicode, screen clears, and alt-screen flips.
const MONKEY_OUTPUT: &[&[u8]] = &[
    b"compiling foo v0.1.0\r\n",
    b"\x1b[31merror\x1b[0m: something\r\n",
    b"\x1b]0;Agent thinking\x07",
    b"\x07",
    b"\x1b]9;needs input\x07",
    "héllo wörld — \u{2714}\r\n".as_bytes(),
    b"\x1b[2J\x1b[H",
    b"\x1b[?1049h",
    b"\x1b[?1049l",
];

/// Monkey test: drive a real `App` with thousands of pseudo-random events —
/// keys, chords, ticks, clock jumps, resizes, mouse, and injected agent
/// output — rendering after every step and asserting [`assert_invariants`].
/// This is the net for "weird TUI behavior": any panic (in update *or* view)
/// or invariant violation fails with the seed + step for exact replay.
/// Tokio flavor: spawn-adjacent flows (fork/restart on the inert backend) may
/// touch the runtime before erroring.
#[tokio::test]
async fn monkey_random_events_uphold_invariants() {
    for seed in [0xDEADBEEFu64, 42, 20260707] {
        let mut rng = Rng(seed);
        let mut h = Harness::standard(3);
        h.render();

        for step in 0..2500 {
            let ctx = format!("seed {seed:#x} step {step}");
            match rng.below(100) {
                // Plain keys: letters, digits, and structural keys.
                0..=39 => {
                    let code = MONKEY_KEYS[rng.below(MONKEY_KEYS.len())];
                    h.key(code, KeyModifiers::NONE);
                }
                // Ctrl chords (friring's global namespace).
                40..=59 => {
                    let c = MONKEY_CTRL[rng.below(MONKEY_CTRL.len())];
                    h.ctrl(c);
                }
                // Shift chords (reorder/sort) and F-keys.
                60..=69 => {
                    if rng.below(2) == 0 {
                        let c = ['j', 'k', 's', 'd'][rng.below(4)];
                        h.shift(c);
                    } else {
                        h.func((rng.below(8) + 1) as u8);
                    }
                }
                // Deterministic tick, sometimes after a clock jump.
                70..=79 => {
                    if rng.below(2) == 0 {
                        let ms = [50, 200, 1_000, 5_000, 11_000][rng.below(5)];
                        h.advance(std::time::Duration::from_millis(ms));
                    }
                    h.tick();
                }
                // Mouse: click / scroll / move at a random point.
                80..=89 => {
                    let size = *h.terminal.backend().buffer().area();
                    let x = (rng.below(size.width.max(1) as usize)) as u16;
                    let y = (rng.below(size.height.max(1) as usize)) as u16;
                    let msg = match rng.below(4) {
                        0 => AppMessage::MouseClick {
                            x,
                            y,
                            modifiers: KeyModifiers::NONE,
                        },
                        1 => AppMessage::MouseScrollUp { x, y },
                        2 => AppMessage::MouseScrollDown { x, y },
                        _ => AppMessage::MouseMove { x, y },
                    };
                    h.app.update(msg);
                }
                // Agent output into a random session.
                90..=94 => {
                    if !h.app.sessions.is_empty() {
                        let idx = rng.below(h.app.sessions.len());
                        let chunk = MONKEY_OUTPUT[rng.below(MONKEY_OUTPUT.len())];
                        h.feed_output(idx, chunk);
                        h.app.detect_output_redraw();
                    }
                }
                // Resize, including below the 80/120 layout breakpoints.
                _ => {
                    let cols = (20 + rng.below(160)) as u16;
                    let rows = (8 + rng.below(43)) as u16;
                    h.resize(cols, rows);
                }
            }

            // Render every step: a draw panic (layout overflow, index OOB in a
            // widget) is as much a bug as an update panic.
            h.render();
            assert_invariants(&h.app, &ctx);
        }
    }
}

#[test]
fn theme_picker_filter_narrows_the_list_and_previews_a_match() {
    // Typing filters the list; the selection lands on the first match and is
    // live-previewed, and `Enter` commits *that* theme (not the entry that
    // happened to share the pre-filter index).
    let mut h = Harness::standard(0);
    h.ctrl('y');
    h.key(KeyCode::Char('/'), KeyModifiers::NONE); // open the filter sub-mode
    for c in "gruvbox".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    let entries = crate::ui::theme::all_theme_entries();
    let names: Vec<&str> = tp
        .matches
        .iter()
        .map(|&i| entries[i].name.as_str())
        .collect();
    assert_eq!(names, vec!["gruvbox-dark", "gruvbox-light"]);
    assert_eq!(tp.index, 0, "selection resets to the first match");
    assert_eq!(
        crate::ui::theme::current(),
        crate::ui::theme::find_theme_entry("gruvbox-dark")
            .unwrap()
            .palette,
        "the first match is previewed"
    );

    h.key(KeyCode::Down, KeyModifiers::NONE);
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        h.app.active_theme.name, "gruvbox-light",
        "Enter commits the selected *match*, not the same-numbered entry"
    );
}

#[test]
fn theme_picker_filter_matching_nothing_keeps_the_modal_usable() {
    let mut h = Harness::standard(0);
    h.ctrl('y');
    h.key(KeyCode::Char('/'), KeyModifiers::NONE); // open the filter sub-mode
    for c in "zzzz".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert!(tp.matches.is_empty());
    assert!(tp.selected_entry().is_none());
    // Rendering an empty match set must not panic, and Enter must be a no-op
    // rather than committing a stale index.
    h.render();
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(!h.app.modal.is_open(), "Enter closes the picker");
    assert_eq!(
        h.app.active_theme.name, "default",
        "no match means nothing is committed"
    );

    // Backspacing back to a real query restores the list.
    let mut h = Harness::standard(0);
    h.ctrl('y');
    h.key(KeyCode::Char('/'), KeyModifiers::NONE); // open the filter sub-mode
    for c in "nordx".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    h.key(KeyCode::Backspace, KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.matches.len(), 1, "'nord' matches exactly one theme");
}

#[test]
fn theme_picker_page_keys_move_by_a_screenful() {
    // PageDown steps by the rendered list height, so a 36-entry list is
    // traversable without holding Down.
    let mut h = Harness::standard(0);
    h.ctrl('y');
    h.render(); // establishes the page height
    let page = h.app.theme_picker_page;
    assert!(page > 1, "the list should render several rows, got {page}");

    h.key(KeyCode::PageDown, KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.index, page);

    h.key(KeyCode::End, KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(
        tp.index,
        crate::ui::theme::all_theme_entries().len() - 1,
        "End jumps to the last theme"
    );

    h.key(KeyCode::Home, KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.index, 0, "Home jumps back to the first");
}

#[test]
fn theme_picker_ctrl_n_p_navigate_and_other_ctrl_chords_dont_type() {
    // Parity with the global-search strip: Ctrl+N/Ctrl+P move the selection.
    // Any *other* Ctrl chord must be swallowed, never inserted as a letter —
    // a stray Ctrl+W would otherwise silently filter the list down to "w".
    let mut h = Harness::standard(0);
    h.ctrl('y');

    h.key(KeyCode::Char('n'), KeyModifiers::CONTROL);
    h.key(KeyCode::Char('n'), KeyModifiers::CONTROL);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.index, 2, "Ctrl+N moves down");
    assert!(tp.filter_query().is_empty(), "Ctrl+N must not type");

    h.key(KeyCode::Char('p'), KeyModifiers::CONTROL);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.index, 1, "Ctrl+P moves up");

    h.key(KeyCode::Char('w'), KeyModifiers::CONTROL);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert!(
        tp.filter_query().is_empty(),
        "an unhandled Ctrl chord must not leak into the filter"
    );
    assert_eq!(tp.index, 1, "and must not move the selection");
}

#[test]
fn theme_picker_jk_navigate_until_slash_opens_the_filter() {
    // The picker keeps the shared selector keys: `j`/`k` select, and only `/`
    // starts a query. Letters are literal navigation until then, so typing
    // `j` can never silently filter the list.
    let mut h = Harness::standard(0);
    h.ctrl('y');

    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.index, 2, "j moves down");
    assert!(tp.filter.is_none(), "j must not open the filter");

    h.key(KeyCode::Char('k'), KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.index, 1, "k moves up");

    // g/G jump to the ends, as in the other list surfaces.
    h.key(KeyCode::Char('G'), KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.index, crate::ui::theme::all_theme_entries().len() - 1);
    h.key(KeyCode::Char('g'), KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.index, 0);

    // `/` switches modes; only now do letters become query text.
    h.key(KeyCode::Char('/'), KeyModifiers::NONE);
    h.key(KeyCode::Char('j'), KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.filter_query(), "j", "after / a letter types");
    // No built-in name contains a `j`, so this also shows the letter really
    // reached the query rather than moving the cursor.
    assert!(tp.matches.is_empty(), "'j' matches no theme name");
}

#[test]
fn theme_picker_esc_closes_filter_first_then_the_modal() {
    // Two Esc levels, like the code-review find: the first leaves the filter
    // sub-mode (restoring the full list), the second cancels the picker.
    let mut h = Harness::standard(0);
    h.ctrl('y');
    h.key(KeyCode::Char('/'), KeyModifiers::NONE);
    for c in "nord".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.matches.len(), 1, "filtered down to Nord");

    h.key(KeyCode::Esc, KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("first Esc must keep the picker open");
    };
    assert!(tp.filter.is_none(), "first Esc closes the filter");
    assert_eq!(
        tp.matches.len(),
        crate::ui::theme::all_theme_entries().len(),
        "clearing the filter restores every theme"
    );
    // The cursor stayed on the theme the filter had selected, so leaving the
    // sub-mode doesn't jump the preview somewhere unrelated.
    assert_eq!(
        tp.selected_entry()
            .map(|i| crate::ui::theme::all_theme_entries()[i].name.clone()),
        Some("nord".to_string())
    );

    h.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(!h.app.modal.is_open(), "second Esc closes the picker");
}

#[test]
fn theme_picker_slash_is_not_query_text() {
    // `/` opens the sub-mode; pressing it again keeps the query rather than
    // inserting a literal slash (no theme name contains one).
    let mut h = Harness::standard(0);
    h.ctrl('y');
    h.key(KeyCode::Char('/'), KeyModifiers::NONE);
    for c in "nord".chars() {
        h.key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    h.key(KeyCode::Char('/'), KeyModifiers::NONE);
    let modals::Modal::ThemePicker(ref tp) = h.app.modal else {
        panic!("expected the theme picker");
    };
    assert_eq!(tp.filter_query(), "nord", "a second / must not type");
}

// ── The orchestration bridge's broker (ADR-30) ───────────────────────────

/// Give session `idx` a sandbox profile granting `grants`, so the broker serves
/// its queue.
///
/// A grant is a **profile** decision, so it is written as a stored profile
/// rather than poked onto the session: the broker asks the profile, and a test
/// that shortcut that would be testing something else.
fn grant_bridge(h: &mut Harness, idx: usize, grants: &[crate::session::BridgeCapability]) {
    let mut profile = crate::session::SandboxProfile::new(
        "orchestrator",
        vec![crate::session::SandboxPath::workspace("~/dev/app")],
    );
    profile.network_mode = crate::session::NetworkMode::None;
    profile.bridge_grants = grants.to_vec();
    h.app.db.upsert_sandbox_profile(&profile).unwrap();
    h.app.sessions[idx].info.sandbox_profile = Some(profile.name.clone());
    let key = h.app.sessions[idx].info.id.to_string();
    crate::paths::create_session_signal_dir(&key).unwrap();
    crate::paths::create_session_bridge_dirs(&key).unwrap();
}

/// Push a file's timestamps `age` into the past, so an age-based GC sees it as
/// old without the test waiting for a real clock.
fn backdate(path: &std::path::Path, age: std::time::Duration) {
    let when = std::time::SystemTime::now() - age;
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("the file to backdate");
    file.set_times(
        std::fs::FileTimes::new()
            .set_accessed(when)
            .set_modified(when),
    )
    .expect("backdating the file");
}

/// Write one request into session `idx`'s queue, as its agent's client would.
fn queue_request(h: &Harness, idx: usize, key: &str, request: &serde_json::Value) {
    let session = h.app.sessions[idx].info.id.to_string();
    let dir = crate::paths::bridge_request_dir(&session).unwrap();
    std::fs::write(
        dir.join(format!("{key}.req")),
        serde_json::to_string(request).unwrap(),
    )
    .unwrap();
}

/// The answer to `key`, once one exists.
fn bridge_answer(h: &Harness, idx: usize, key: &str) -> Option<serde_json::Value> {
    let session = h.app.sessions[idx].info.id.to_string();
    let path = crate::paths::bridge_response_dir(&session)
        .unwrap()
        .join(format!("{key}.res"));
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

/// A well-formed request envelope.
fn envelope(key: &str, verb: &str, body: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "protocol": crate::session::bridge::BRIDGE_PROTOCOL,
        "key": key,
        "verb": verb,
        "body": body,
    })
}

/// A queued request is served within two ticks — the broker polls on its own
/// cadence, so "next tick" is not the promise; "promptly, without the client
/// doing anything else" is.
#[test]
fn a_queued_bridge_request_is_served_within_two_ticks() {
    let mut h = Harness::standard(1);
    grant_bridge(&mut h, 0, &[crate::session::BridgeCapability::Mailbox]);
    queue_request(
        &h,
        0,
        "abc-1234",
        &envelope("abc-1234", "status", serde_json::json!({})),
    );

    for _ in 0..2 * 3 {
        h.tick();
        if bridge_answer(&h, 0, "abc-1234").is_some() {
            break;
        }
    }
    let answer = bridge_answer(&h, 0, "abc-1234").expect("the broker answered");
    assert_eq!(answer["ok"], true, "{answer}");
    assert!(answer["data"]["children"].is_array(), "{answer}");
}

/// The per-tick budget holds under a flood: a client writing faster than friring
/// answers costs latency, never frames. Asserted on the counter, because that is
/// the only thing a wall-clock-free test can prove about a budget.
#[test]
fn the_bridge_tick_budget_holds_under_a_flood() {
    let mut h = Harness::standard(1);
    grant_bridge(&mut h, 0, &[crate::session::BridgeCapability::Mailbox]);
    for n in 0..100 {
        queue_request(
            &h,
            0,
            &format!("flood-{n:04}"),
            &envelope(&format!("flood-{n:04}"), "status", serde_json::json!({})),
        );
    }

    let before = h.app.perf_counters().bridge_requests_served;
    // Enough ticks for exactly one polling pass.
    for _ in 0..3 {
        h.tick();
    }
    let served = h.app.perf_counters().bridge_requests_served - before;
    assert!(
        served <= crate::app::bridge::REQUESTS_PER_TICK as u64,
        "one pass served {served}, past the per-tick budget"
    );
    assert!(served > 0, "the pass must make progress");
}

/// ADR-P16. A **placeholder** bridge session's queue is never polled. It has no
/// agent process on this host, so by construction nothing is filling its `req/`
/// and a poll could only ever find the directory empty.
///
/// Asserted on the counter, because from the outside a pass over an empty queue
/// and a pass that never happened look identical — which is exactly what let
/// this cost 24% of a render thread unnoticed. The second half is the other
/// side of the same rule: nothing is remembered about the skip, so loading the
/// session serves the request that was waiting all along.
#[tokio::test]
async fn perf_a_ghost_bridge_session_is_never_polled_and_serving_resumes_on_load() {
    let mut h = Harness::spawnable(2);
    // Unloaded *before* the profile is attached: this test is about the poll,
    // and a sandboxed relaunch is a different path with its own tests.
    h.app.set_active_index(1);
    h.alt('u'); // UnloadSession
    assert!(
        h.app.sessions[1].is_ghost(),
        "the row is a ghost to begin with"
    );
    grant_bridge(&mut h, 0, &[crate::session::BridgeCapability::Mailbox]);
    grant_bridge(&mut h, 1, &[crate::session::BridgeCapability::Mailbox]);
    queue_request(
        &h,
        1,
        "ghost-001",
        &envelope("ghost-001", "status", serde_json::json!({})),
    );

    let before = h.app.perf_counters().bridge_queue_polls;
    // Three ticks is exactly one polling pass, whatever the tick count is at.
    for _ in 0..3 {
        h.tick();
    }
    assert_eq!(
        h.app.perf_counters().bridge_queue_polls - before,
        1,
        "a pass over two bridge sessions polled the unloaded one's queue too"
    );
    assert!(
        bridge_answer(&h, 1, "ghost-001").is_none(),
        "a ghost's queue was served"
    );

    h.app.set_active_index(1);
    h.key(KeyCode::Enter, KeyModifiers::NONE);
    // The load spawns a real window, so it completes on a tick rather than
    // inside the keypress. Waited for rather than assumed: how many passes it
    // takes is a property of the machine, and asserting straight after the key
    // made this test pass on the developer's and fail on a slower runner.
    for _ in 0..30 {
        if !h.app.sessions[1].is_ghost() {
            break;
        }
        h.tick();
    }
    assert!(!h.app.sessions[1].is_ghost(), "the ghost did not load");
    for _ in 0..6 {
        h.tick();
        if bridge_answer(&h, 1, "ghost-001").is_some() {
            break;
        }
    }
    let answer = bridge_answer(&h, 1, "ghost-001")
        .expect("the request waiting since before the unload was never served");
    assert_eq!(answer["ok"], true, "{answer}");
}

/// ADR-P16. The shared `.taking` directory is minted **once per broker pass**,
/// not once per session that pass serves.
///
/// Counted at [`crate::paths::mint_bridge_taking_dir`] itself rather than at the
/// broker's call to it: a gate on the caller would still pass if the mint moved
/// back inside the per-session take, which is the shape of the bug.
#[test]
fn perf_a_bridge_pass_mints_the_taking_directory_once() {
    const SESSIONS: usize = 4;
    const PASSES: u64 = 3;
    let mut h = Harness::standard(SESSIONS);
    for idx in 0..SESSIONS {
        grant_bridge(&mut h, idx, &[crate::session::BridgeCapability::Mailbox]);
    }

    let polls_before = h.app.perf_counters().bridge_queue_polls;
    crate::paths::BRIDGE_TAKING_MINTS.with(|mints| mints.set(0));
    for _ in 0..(PASSES * crate::app::bridge::POLL_TICKS) {
        h.tick();
    }
    let mints = crate::paths::BRIDGE_TAKING_MINTS.with(|mints| mints.get()) as u64;
    let polls = h.app.perf_counters().bridge_queue_polls - polls_before;

    assert_eq!(
        polls,
        PASSES * SESSIONS as u64,
        "every session is still polled"
    );
    assert_eq!(
        mints, PASSES,
        "the staging directory was minted {mints} times for {polls} queue polls"
    );
}

/// ADR-P16. The response GC reaches every bridge channel's `res/` directory —
/// nothing else removes an answer a client never acknowledged — while sweeping
/// only a bounded slice of them per pass.
///
/// The roster is the **disk**, and the two cases that shows are the ones a
/// session list cannot: a ghost, which is a session with no process, and a
/// channel with no session row in memory at all — which is what a cleanly
/// stopped child leaves behind, since the quiesce retires its runtime and keeps
/// everything else.
#[tokio::test]
async fn perf_the_response_gc_sweeps_every_channel_a_slice_at_a_time() {
    use crate::app::bridge::RESPONSE_GC_BATCH;
    // More channels than one slice, so "swept everything" and "swept a slice"
    // are distinguishable.
    let sessions = RESPONSE_GC_BATCH + 2;
    let mut h = Harness::spawnable(sessions);
    h.app.set_active_index(sessions - 1);
    h.alt('u'); // one of them is a ghost, and its directory must still shrink
    assert!(h.app.sessions[sessions - 1].is_ghost());
    for idx in 0..sessions {
        grant_bridge(&mut h, idx, &[crate::session::BridgeCapability::Mailbox]);
    }
    // The parked child: a channel friring minted whose session is no longer in
    // the list. Never swept while the roster was the session list.
    let retired = SessionId::default().to_string();
    crate::paths::create_session_signal_dir(&retired).unwrap();
    crate::paths::create_session_bridge_dirs(&retired).unwrap();

    let mut keys: Vec<String> = (0..sessions)
        .map(|idx| h.app.sessions[idx].info.id.to_string())
        .collect();
    keys.push(retired.clone());
    let stale: Vec<std::path::PathBuf> = keys
        .iter()
        .map(|key| {
            let path = crate::paths::bridge_response_dir(key)
                .unwrap()
                .join("aged-0001.res");
            std::fs::write(&path, "{}").unwrap();
            backdate(&path, crate::app::bridge::RESPONSE_MAX_AGE * 2);
            path
        })
        .collect();
    // A fresh answer in the same directory proves this is an age GC and not a
    // sweep of everything it finds.
    let fresh = crate::paths::bridge_response_dir(&keys[0])
        .unwrap()
        .join("fresh-0001.res");
    std::fs::write(&fresh, "{}").unwrap();

    // One housekeeping pass: a slice, not the fleet. At most the batch can have
    // gone, and the slice may also have landed on channels this test did not
    // put a stale file in — so the bound is what is asserted.
    h.tick_until_bridge_housekeeping();
    let survivors = stale.iter().filter(|p| p.exists()).count();
    assert!(
        survivors >= stale.len() - RESPONSE_GC_BATCH,
        "one pass swept {} of {} channels, past a slice of {RESPONSE_GC_BATCH}",
        stale.len() - survivors,
        stale.len()
    );

    // Keep going: the cursor moves on, so the whole roster comes round.
    for _ in 0..(2 * stale.len()) {
        h.tick_until_bridge_housekeeping();
    }
    let left: Vec<&std::path::PathBuf> = stale.iter().filter(|p| p.exists()).collect();
    assert!(
        left.is_empty(),
        "the sweep never reached these channels: {left:?}"
    );
    assert!(fresh.exists(), "the GC removed an answer that is not stale");
}

/// A session whose profile grants nothing has no bridge, whatever its agent
/// asks for. The refusal is a code the caller can act on, not prose.
#[test]
fn a_verb_without_its_grant_is_refused() {
    let mut h = Harness::standard(1);
    grant_bridge(&mut h, 0, &[]);
    queue_request(
        &h,
        0,
        "abc-1234",
        &envelope("abc-1234", "status", serde_json::json!({})),
    );
    for _ in 0..6 {
        h.tick();
    }
    let answer = bridge_answer(&h, 0, "abc-1234").expect("the broker answered");
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "grant_missing", "{answer}");
}

/// `inbox` reads the caller's own mail and there is no way to ask for another
/// session's: the recipient comes from the queue the request landed in.
#[test]
fn an_inbox_never_returns_another_sessions_mail() {
    let mut h = Harness::standard(2);
    grant_bridge(&mut h, 0, &[crate::session::BridgeCapability::Mailbox]);
    let caller = h.app.sessions[0].info.id;
    let other = h.app.sessions[1].info.id;
    for (to, body) in [(caller, "for the caller"), (other, "for somebody else")] {
        h.app
            .db
            .enqueue_message(&crate::storage::messages::NewMessage {
                to_session_id: to,
                from_session_id: None,
                from_task_id: None,
                kind: "task".to_string(),
                body: body.to_string(),
                in_reply_to: None,
            })
            .unwrap();
    }

    queue_request(
        &h,
        0,
        "abc-1234",
        &envelope("abc-1234", "inbox", serde_json::json!({ "claim": true })),
    );
    for _ in 0..6 {
        h.tick();
    }
    let answer = bridge_answer(&h, 0, "abc-1234").expect("the broker answered");
    assert_eq!(answer["ok"], true, "{answer}");
    let bodies: Vec<String> = answer["data"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(bodies, ["for the caller"], "{answer}");
    // And the other session's mail is untouched.
    assert_eq!(h.app.db.count_unread_messages(other).unwrap(), 1);
}

/// A `send` to a session the caller does not own is refused by the ownership
/// row, not by a name check: `to` is a name the broker resolves.
#[test]
fn a_send_to_a_session_the_caller_does_not_own_is_refused() {
    let mut h = Harness::standard(2);
    grant_bridge(&mut h, 0, &[crate::session::BridgeCapability::Mailbox]);
    let stranger = h.app.sessions[1].info.id.to_string();
    // The stranger is somebody *else's* child, so it exists as a child and is
    // still not this caller's.
    h.app
        .db
        .insert_bridge_child(&stranger, "some-other-owner", "k-1")
        .unwrap();

    queue_request(
        &h,
        0,
        "abc-1234",
        &envelope(
            "abc-1234",
            "send",
            serde_json::json!({ "to": stranger, "kind": "task", "body": "do this" }),
        ),
    );
    for _ in 0..6 {
        h.tick();
    }
    let answer = bridge_answer(&h, 0, "abc-1234").expect("the broker answered");
    assert_eq!(answer["error"], "not_owner", "{answer}");
    assert_eq!(
        h.app
            .db
            .count_unread_messages(h.app.sessions[1].info.id)
            .unwrap(),
        0,
        "nothing was delivered"
    );
}

/// A kind sent in the wrong direction is refused: a child that could send
/// `task` would be assigning work to its owner, and one that could send
/// `child.done` would be reporting a verdict only the host may reach.
#[test]
fn a_mail_kind_in_the_wrong_direction_is_refused() {
    let mut h = Harness::standard(2);
    grant_bridge(&mut h, 0, &[crate::session::BridgeCapability::Mailbox]);
    let child = h.app.sessions[1].info.id.to_string();
    let owner = h.app.sessions[0].info.id.to_string();
    h.app.db.insert_bridge_child(&child, &owner, "k-1").unwrap();
    h.app
        .db
        .set_bridge_child_state(&child, crate::session::ChildState::Ready)
        .unwrap();

    for (n, kind) in ["result", "child.done"].iter().enumerate() {
        let key = format!("dir-{n:05}");
        queue_request(
            &h,
            0,
            &key,
            &envelope(
                &key,
                "send",
                serde_json::json!({ "to": child, "kind": kind, "body": "{}" }),
            ),
        );
        for _ in 0..6 {
            h.tick();
        }
        let answer = bridge_answer(&h, 0, &key).expect("the broker answered");
        assert_eq!(answer["ok"], false, "{kind} must be refused: {answer}");
    }
    // …while the kind this direction *does* allow goes through.
    queue_request(
        &h,
        0,
        "task-0001",
        &envelope(
            "task-0001",
            "send",
            serde_json::json!({ "to": child, "kind": "task", "body": "do this" }),
        ),
    );
    for _ in 0..6 {
        h.tick();
    }
    assert_eq!(bridge_answer(&h, 0, "task-0001").unwrap()["ok"], true);
}

/// A `result` whose body is not a `ResultBody` is refused and starts no
/// quiesce: "the child asked to finish" is the one message that must not be
/// inferred from free text.
#[test]
fn a_result_that_is_not_a_result_body_is_refused() {
    let mut h = Harness::standard(2);
    grant_bridge(&mut h, 1, &[crate::session::BridgeCapability::Mailbox]);
    let child = h.app.sessions[1].info.id.to_string();
    let owner = h.app.sessions[0].info.id.to_string();
    h.app.db.insert_bridge_child(&child, &owner, "k-1").unwrap();
    h.app
        .db
        .set_bridge_child_state(&child, crate::session::ChildState::Working)
        .unwrap();

    queue_request(
        &h,
        1,
        "abc-1234",
        &envelope(
            "abc-1234",
            "send",
            serde_json::json!({ "to": "owner", "kind": "result", "body": "all done!" }),
        ),
    );
    for _ in 0..6 {
        h.tick();
    }
    let answer = bridge_answer(&h, 1, "abc-1234").expect("the broker answered");
    assert_eq!(answer["ok"], false, "{answer}");
    assert!(
        answer["message"].as_str().unwrap().contains("outcome"),
        "{answer}"
    );
    // The child is still working: nothing about its state moved.
    assert_eq!(
        h.app.db.bridge_child_state(&child).unwrap().unwrap().state,
        crate::session::ChildState::Working
    );
}

/// A child may not create children: orchestration is one level deep by
/// construction, so the refusal comes from the ownership row rather than from a
/// depth counter that could be miscounted.
#[test]
fn a_child_may_not_create_children() {
    let mut h = Harness::standard(2);
    grant_bridge(
        &mut h,
        1,
        &[
            crate::session::BridgeCapability::ChildLifecycle,
            crate::session::BridgeCapability::Mailbox,
        ],
    );
    let child = h.app.sessions[1].info.id.to_string();
    h.app
        .db
        .insert_bridge_child(&child, "some-owner", "k-1")
        .unwrap();

    queue_request(
        &h,
        1,
        "abc-1234",
        &envelope(
            "abc-1234",
            "create",
            serde_json::json!({
                "repo_root": "/repo",
                "branch": "feat/x",
                "agent": "worker",
                "task_kind": "task",
                "task_body": "go",
            }),
        ),
    );
    for _ in 0..6 {
        h.tick();
    }
    let answer = bridge_answer(&h, 1, "abc-1234").expect("the broker answered");
    assert_eq!(answer["error"], "depth_exceeded", "{answer}");
}

/// A replay returns the first answer's exact bytes; a key reused for a different
/// request is refused and does nothing.
#[test]
fn a_replayed_request_is_answered_from_the_journal() {
    let mut h = Harness::standard(1);
    grant_bridge(&mut h, 0, &[crate::session::BridgeCapability::Report]);
    let report = |summary: &str| {
        envelope(
            "abc-1234",
            "report",
            serde_json::json!({ "phase": "implementing", "summary": summary }),
        )
    };

    queue_request(&h, 0, "abc-1234", &report("first"));
    for _ in 0..6 {
        h.tick();
    }
    let first = bridge_answer(&h, 0, "abc-1234").expect("the broker answered");
    assert_eq!(first["ok"], true, "{first}");

    // The same key and the same body: the first answer, verbatim.
    queue_request(&h, 0, "abc-1234", &report("first"));
    for _ in 0..6 {
        h.tick();
    }
    assert_eq!(bridge_answer(&h, 0, "abc-1234").unwrap(), first);

    // The same key, different work: refused, and nothing recorded.
    let session = h.app.sessions[0].info.id.to_string();
    let before = h.app.db.bridge_reports(&session, 50).unwrap().len();
    queue_request(&h, 0, "abc-1234", &report("second"));
    for _ in 0..6 {
        h.tick();
    }
    let answer = bridge_answer(&h, 0, "abc-1234").unwrap();
    assert_eq!(answer["error"], "key_reused", "{answer}");
    assert_eq!(h.app.db.bridge_reports(&session, 50).unwrap().len(), before);
}

/// A request from a friring speaking another protocol version is refused rather
/// than interpreted.
#[test]
fn a_request_from_another_protocol_version_is_refused() {
    let mut h = Harness::standard(1);
    grant_bridge(&mut h, 0, &[crate::session::BridgeCapability::Mailbox]);
    let mut request = envelope("abc-1234", "status", serde_json::json!({}));
    request["protocol"] = serde_json::json!(999);
    queue_request(&h, 0, "abc-1234", &request);
    for _ in 0..6 {
        h.tick();
    }
    let answer = bridge_answer(&h, 0, "abc-1234").expect("the broker answered");
    assert_eq!(answer["ok"], false, "{answer}");
    assert!(
        answer["message"].as_str().unwrap().contains("protocol"),
        "{answer}"
    );
}

/// A field friring does not recognise is a request that does not mean here what
/// it meant where it was written, so it is refused rather than answered as
/// though it said something else.
#[test]
fn a_request_with_an_unknown_field_is_refused() {
    let mut h = Harness::standard(1);
    grant_bridge(&mut h, 0, &[crate::session::BridgeCapability::Report]);
    let request = envelope(
        "abc-1234",
        "report",
        serde_json::json!({ "phase": "implementing", "escalate_to_root": true }),
    );
    queue_request(&h, 0, "abc-1234", &request);
    for _ in 0..6 {
        h.tick();
    }
    let answer = bridge_answer(&h, 0, "abc-1234").expect("the broker answered");
    assert_eq!(answer["ok"], false, "{answer}");
}

// ── Stage F: the child lifecycle (ADR-32) ────────────────────────────────

/// A [`ChildEffects`](crate::app::bridge_spawn::ChildEffects) a test scripts.
///
/// The saga's whole contract is about what survives a failure at each step, so
/// every step it does outside SQLite has to be one a test can *make* fail. Real
/// `git` cannot be persuaded to refuse a checkout on demand, and a real egress
/// supervisor cannot be persuaded to go silent, so both are scripted here and
/// every call is recorded.
#[derive(Default)]
struct ScriptedEffects {
    calls: std::sync::Mutex<Vec<String>>,
    /// What `create_worktree` answers. `None` succeeds with a fabricated path.
    /// What S2 fails with, and whether that failure claimed the branch first —
    /// the two cases the unwind must tell apart.
    worktree_error: std::sync::Mutex<Option<crate::git::ClaimFailure>>,
    /// The directory a successful checkout reports.
    worktree_path: std::sync::Mutex<Option<std::path::PathBuf>>,
    base_head: std::sync::Mutex<Option<String>>,
    /// What the post-stop inspection finds.
    verdict: std::sync::Mutex<crate::git::WorktreeVerdict>,
    /// What the seeding answers.
    seed_error: std::sync::Mutex<Option<String>>,
    /// Whether the egress supervisor acknowledges the commit.
    acknowledges: std::sync::atomic::AtomicBool,
    /// How many commits a branch carries, for the reclaim rule.
    ahead: std::sync::Mutex<Option<u32>>,
    removed_worktrees: std::sync::Mutex<Vec<String>>,
    deleted_branches: std::sync::Mutex<Vec<String>>,
    /// What `worktree_is_on` answers. `Some(true)` — the default — is what a
    /// real successful claim leaves; `Some(false)` is the sanitized-name
    /// collision, where the recorded path is another launch's worktree, and
    /// `None` is a git that would not say.
    worktree_is_on: std::sync::Mutex<Option<bool>>,
    /// The multiplexer this harness's children were spawned on, so
    /// `verify_worktree` can record whether any pane was still alive **at the
    /// moment it looked**.
    ///
    /// The quiesce's whole promise is that the host reads the worktree only
    /// after the child's pane is dead. Asserting that the two both *happened*
    /// proves nothing about their order — the kill goes through the backend and
    /// the verification through this seam, two separate pieces of state — so the
    /// ordering has to be observed from inside one of them.
    panes: std::sync::Mutex<Option<Arc<FakeBackend>>>,
    /// How many panes were alive each time `verify_worktree` ran.
    panes_alive_at_verify: std::sync::Mutex<Vec<usize>>,
}

impl ScriptedEffects {
    fn new() -> Arc<Self> {
        let effects = Self::default();
        effects
            .acknowledges
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // `Default` is `None`, which means "git would not say" and would make
        // every unwind decline. The ordinary case is a directory this launch
        // really made.
        *effects.worktree_is_on.lock().unwrap() = Some(true);
        Arc::new(effects)
    }

    fn note(&self, what: &str) {
        self.calls.lock().unwrap().push(what.to_string());
    }

    fn called(&self, what: &str) -> bool {
        self.calls.lock().unwrap().iter().any(|c| c == what)
    }

    /// Watch `backend` so every verification records the live pane count.
    fn watching(&self, backend: &Arc<FakeBackend>) {
        *self.panes.lock().unwrap() = Some(Arc::clone(backend));
    }
}

impl crate::app::bridge_spawn::ChildEffects for ScriptedEffects {
    fn create_worktree(
        &self,
        _repo: &Path,
        _branch: &str,
        _base: &str,
    ) -> Result<std::path::PathBuf, crate::git::ClaimFailure> {
        self.note("create_worktree");
        if let Some(error) = self.worktree_error.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(self
            .worktree_path
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("/fabricated/worktree")))
    }

    fn head_commit(&self, _cwd: &Path) -> Option<String> {
        self.base_head.lock().unwrap().clone()
    }

    fn verify_worktree(
        &self,
        _worktree: &Path,
        _base_head: Option<&str>,
    ) -> crate::git::WorktreeVerdict {
        self.note("verify_worktree");
        // Read *now*, not afterwards: this is the instant the host looks at the
        // worktree, and the quiesce's promise is that nothing can be writing it.
        let alive = self
            .panes
            .lock()
            .unwrap()
            .as_ref()
            .map(|b| b.panes.lock().unwrap().len())
            .unwrap_or(0);
        self.panes_alive_at_verify.lock().unwrap().push(alive);
        self.verdict.lock().unwrap().clone()
    }

    fn worktree_is_on(&self, _repo: &Path, _worktree: &Path, _branch: &str) -> Option<bool> {
        self.note("worktree_is_on");
        // Defaults to yes, which is what a real successful claim leaves behind;
        // a test that wants the sanitized-name collision (or a git that will not
        // answer) sets it.
        *self.worktree_is_on.lock().unwrap()
    }

    fn remove_worktree(&self, _repo: &Path, worktree: &Path) -> Result<(), String> {
        self.note("remove_worktree");
        self.removed_worktrees
            .lock()
            .unwrap()
            .push(worktree.display().to_string());
        Ok(())
    }

    fn delete_branch(&self, _repo: &Path, branch: &str) -> Result<(), String> {
        self.note("delete_branch");
        self.deleted_branches
            .lock()
            .unwrap()
            .push(branch.to_string());
        Ok(())
    }

    fn commits_ahead(&self, _repo: &Path, _base: &str, _tip: &str) -> Option<u32> {
        *self.ahead.lock().unwrap()
    }

    fn egress_acknowledged(&self, _session_key: &str) -> bool {
        self.acknowledges.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn seed_child_state(
        &self,
        _plan: &crate::sandbox::child_state::SeedPlan,
    ) -> Result<(), String> {
        self.note("seed_child_state");
        match self.seed_error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// A harness whose one session can create bridge children.
///
/// Everything a `create` needs and nothing more: a spawnable backend that
/// reports pane identities (so every ADR-32 identity rule is exercised rather
/// than skipped), a profile granting `child-lifecycle`, a recorded repository,
/// a child agent friring can give private state to, and a fabricated sandbox
/// host so the composition resolves the same backend on every machine.
struct ChildHarness {
    h: Harness,
    effects: Arc<ScriptedEffects>,
    // Held for its `Drop`: the host override is thread-local.
    _sandbox: crate::agent::sandboxing::TestSandboxHost,
}

/// The agent a child runs, declaring exactly what private state needs.
fn worker_agent() -> crate::session::AgentDef {
    crate::session::AgentDef {
        name: "worker".into(),
        command: "worker".into(),
        args: vec![],
        resume_args: vec![],
        fork_args: vec![],
        new_session_args: vec![],
        resume_latest: false,
        hook_schema: None,
        transcript: None,
        sandbox: Some(crate::session::AgentSandboxDef {
            config_dir_env: Some("WORKER_HOME".into()),
            state_dir: Some("~/.worker".into()),
            ..Default::default()
        }),
    }
}

impl ChildHarness {
    fn new() -> Self {
        Self::with_watched_backend(Arc::new(FakeBackend::spawnable()))
    }

    /// A harness whose effects seam can see the multiplexer, so the quiesce's
    /// *ordering* is observable and not merely its two halves.
    fn with_watched_backend(backend: Arc<FakeBackend>) -> Self {
        let watched = Arc::clone(&backend);
        let harness = Self::with_backend(backend);
        harness.effects.watching(&watched);
        harness
    }

    fn with_backend(backend: Arc<dyn SessionBackend>) -> Self {
        let mut h = Harness::with_backend(STD_COLS, STD_ROWS, 1, backend);
        // Installed *after* the harness pinned its paths: the fabricated place
        // tree and the profile file both land under the tempdir.
        let sandbox = crate::agent::sandboxing::TestSandboxHost::seatbelt();

        let mut profile = crate::session::SandboxProfile::new(
            "orchestrator",
            vec![crate::session::SandboxPath::workspace("~/dev/app")],
        );
        profile.network_mode = crate::session::NetworkMode::None;
        profile.bridge_grants = vec![
            crate::session::BridgeCapability::ChildLifecycle,
            crate::session::BridgeCapability::Mailbox,
            crate::session::BridgeCapability::Report,
        ];
        profile.child_agents = vec!["worker".into()];
        profile.max_children = 2;
        h.app.db.upsert_sandbox_profile(&profile).unwrap();
        h.app.sessions[0].info.sandbox_profile = Some(profile.name.clone());
        h.app.agents.agents.push(worker_agent());

        let owner = h.app.sessions[0].info.id.to_string();
        crate::paths::create_session_signal_dir(&owner).unwrap();
        crate::paths::create_session_bridge_dirs(&owner).unwrap();
        h.app
            .db
            .upsert_session_repo(&owner, "/repo/app", "cwd", Some("/repo/app"), None)
            .unwrap();

        let effects = ScriptedEffects::new();
        h.app.child_lifecycle =
            crate::app::bridge_spawn::ChildLifecycle::with_effects(Arc::clone(&effects) as Arc<_>);
        Self {
            h,
            effects,
            _sandbox: sandbox,
        }
    }

    fn owner(&self) -> SessionId {
        self.h.app.sessions[0].info.id
    }

    /// Queue a `create` as the owner's own agent would.
    fn create(&mut self, key: &str) {
        self.create_on(key, "feat/one");
    }

    /// [`Self::create`] on a named branch, for a test that needs two children at
    /// once: two creates resolving to one worktree directory are refused, so a
    /// second live child needs a branch of its own.
    fn create_on(&mut self, key: &str, branch: &str) {
        queue_request(
            &self.h,
            0,
            key,
            &envelope(
                key,
                "create",
                serde_json::json!({
                    "repo_root": "/repo/app",
                    "branch": branch,
                    "agent": "worker",
                    "task_kind": "task",
                    "task_body": "do the thing",
                }),
            ),
        );
    }

    /// Run the tick pipeline and the saga driver `passes` times.
    ///
    /// Every wait in the saga is a state check or a pass counter rather than a
    /// wall-clock sleep, so this converges in a handful of passes. The short
    /// sleep is only to let a blocking task's worker thread finish.
    async fn drive(&mut self, passes: usize) {
        for _ in 0..passes {
            self.h.tick();
            self.h.app.tick_child_sagas();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Drive until `stop` says so, or the budget runs out.
    async fn drive_until(&mut self, passes: usize, stop: impl Fn(&Self) -> bool) {
        for _ in 0..passes {
            self.h.tick();
            self.h.app.tick_child_sagas();
            if stop(self) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// [`Self::drive_until`] with the child's hook reporting on every pass.
    ///
    /// S9 accepts nothing but a hook report stamped at or after the gate opened,
    /// so a test that lets a launch run to `ready` has to keep reporting rather
    /// than report once — the pane may not exist yet on the pass it chose.
    async fn drive_reporting(&mut self, passes: usize, stop: impl Fn(&Self) -> bool) {
        for _ in 0..passes {
            self.h.tick();
            self.h.app.tick_child_sagas();
            self.child_hook_reports();
            if stop(self) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Drive until `key` has an answer, and return it.
    ///
    /// The budget is generous on purpose. It bounds how long a *genuinely*
    /// unanswered request takes to fail and nothing else — the loop returns on
    /// the pass the answer lands — so the only thing a tight number buys is a
    /// flake on a slower machine, which is what 60 passes bought on CI.
    async fn answer(&mut self, key: &str) -> serde_json::Value {
        const PASSES: usize = 240;
        self.drive_until(PASSES, |h| bridge_answer(&h.h, 0, key).is_some())
            .await;
        bridge_answer(&self.h, 0, key)
            .unwrap_or_else(|| panic!("no answer to '{key}' after {PASSES} passes"))
    }

    /// Create one child and drive it all the way to `ready`.
    ///
    /// The hook is reported on every pass, because S9 accepts nothing else and a
    /// test that reported it once could report it before the pane existed.
    async fn ready_child(&mut self, key: &str) -> crate::session::BridgeChild {
        self.create(key);
        for _ in 0..60 {
            self.h.tick();
            self.h.app.tick_child_sagas();
            self.child_hook_reports();
            if self.child_state() == Some(crate::session::ChildState::Ready) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            self.child_state(),
            Some(crate::session::ChildState::Ready),
            "the saga did not reach a ready child"
        );
        self.child().expect("a ready child")
    }

    /// Accept a finish intent from the child and drive the quiesce to a
    /// terminal or held state.
    async fn quiesce(&mut self, child: &str, outcome: crate::session::Outcome) {
        let id: SessionId = child.parse().unwrap();
        self.h.app.accept_finish_intent(id, outcome, None);
        self.drive_until(60, |h| h.h.app.child_lifecycle.in_flight() == 0)
            .await;
    }

    /// The one child this harness's owner has, if it has one.
    fn child(&self) -> Option<crate::session::BridgeChild> {
        self.h
            .app
            .db
            .bridge_children_of(&self.owner().to_string())
            .unwrap()
            .into_iter()
            .next()
    }

    fn child_state(&self) -> Option<crate::session::ChildState> {
        let child = self.child()?;
        self.h
            .app
            .db
            .bridge_child_state(&child.child_id)
            .unwrap()
            .map(|row| row.state)
    }

    /// Report the child's hook state, which is S9's only accepted proof.
    ///
    /// Through the **hook row**, which is what the sandboxed status channel
    /// writes and what S9 reads. Setting `SessionInfo::status` would prove
    /// nothing: it is `Working` from the moment a session is constructed.
    fn child_hook_reports(&mut self) {
        let Some(child) = self.child() else { return };
        let Ok(id) = child.child_id.parse::<SessionId>() else {
            return;
        };
        let _ = self.h.app.db.set_hook_state(id, "idle");
        self.h.app.cached_hook_states = self.h.app.db.load_hook_states().unwrap_or_default();
    }

    /// Report the child's hook state through the **file channel** a sandboxed
    /// agent actually writes, rather than straight into the row.
    ///
    /// The difference is the dedupe: `apply_status_signals` drops a file that
    /// repeats the recorded state, so it does not re-stamp `state_at` — and
    /// `state_at` is the whole of S9's proof. A test that wrote the row directly
    /// would never take that path, which is exactly the path a relaunched
    /// agent's first report takes when it says the same word the previous one
    /// ended on.
    fn child_signals(&mut self, word: &str) {
        let Some(child) = self.child() else { return };
        let Ok(id) = child.child_id.parse::<SessionId>() else {
            return;
        };
        let Ok(channel) = crate::paths::create_session_signal_dir(&child.child_id) else {
            return;
        };
        std::fs::write(&channel.file, format!("{word}\n")).unwrap();
        super::status_signals::apply_status_signals(&self.h.app.db, &[id]);
        self.h.app.cached_hook_states = self.h.app.db.load_hook_states().unwrap_or_default();
    }
}

/// The whole of `create`: a request in the queue becomes a committed child, a
/// released gate and a `ready` state — and the request is answered only then.
#[tokio::test]
async fn a_create_runs_the_saga_to_a_ready_child() {
    let mut h = ChildHarness::new();
    h.create("create-0001");

    // Up to the point the gate is released, nothing is answered: the client is
    // still waiting, which is what a deferred verb means.
    h.drive_until(60, |h| {
        h.child()
            .and_then(|c| h.h.app.db.child_saga_of_child(&c.child_id).ok().flatten())
            .and_then(|saga| saga.step)
            == Some(crate::session::SagaStep::Released)
    })
    .await;

    let child = h.child().expect("the saga committed a child");
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Starting),
        "a released gate is not yet a ready child"
    );
    assert!(
        bridge_answer(&h.h, 0, "create-0001").is_none(),
        "a create is answered when the child is ready, not before"
    );
    // The gate really was opened, by the host and by nothing else.
    let gate = crate::sandbox::dirs::gate_release_file(&child.child_id).unwrap();
    assert!(gate.exists(), "S8 renames a release file into the gate");

    // The child's own hook reports — the only proof S9 accepts.
    h.child_hook_reports();
    h.drive_until(60, |h| {
        h.child_state() == Some(crate::session::ChildState::Ready)
    })
    .await;

    assert_eq!(h.child_state(), Some(crate::session::ChildState::Ready));
    let answer = bridge_answer(&h.h, 0, "create-0001").expect("the saga answered");
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(answer["data"]["child_id"], child.child_id, "{answer}");
    assert_eq!(answer["data"]["state"], "ready", "{answer}");

    // The child's first mail is the task, inserted in the same transaction as
    // its row.
    let id: SessionId = child.child_id.parse().unwrap();
    let mail = h.h.app.db.list_messages(id, true, None).unwrap();
    assert!(
        mail.iter().any(|m| m.kind == "task"),
        "the child's first mail is its task: {mail:?}"
    );
    // And the owner was told, in friring's own words.
    let owner_mail = h.h.app.db.list_messages(h.owner(), true, None).unwrap();
    assert!(
        owner_mail.iter().any(|m| m.kind == "child.ready"),
        "{owner_mail:?}"
    );
}

/// The gate is what makes every step before S8 able to fail without the agent
/// ever having run — so a silent supervisor never opens it.
#[tokio::test]
async fn a_silent_egress_supervisor_never_releases_the_gate() {
    let mut h = ChildHarness::new();
    // A supervisor that never says it holds the committed instance.
    h.effects
        .acknowledges
        .store(false, std::sync::atomic::Ordering::SeqCst);
    // The profile has to want a proxy for there to be anything to acknowledge.
    let mut profile =
        h.h.app
            .db
            .get_sandbox_profile("orchestrator")
            .unwrap()
            .unwrap()
            .profile;
    profile.network_mode = crate::session::NetworkMode::Allowlist;
    profile.network_allow = vec!["api.example.com".into()];
    h.h.app.db.upsert_sandbox_profile(&profile).unwrap();

    h.create("create-0002");
    h.drive(30).await;

    // Unconditional: the saga must reach S6 and stop there. Wrapped in an
    // `if let`, a launch that failed before the commit would assert nothing at
    // all and the test would still be green.
    let child = h
        .child()
        .expect("a child was committed before the egress gate");
    let gate = crate::sandbox::dirs::gate_release_file(&child.child_id).unwrap();
    let state =
        h.h.app
            .db
            .bridge_child_state(&child.child_id)
            .unwrap()
            .map(|row| row.state);
    assert_ne!(
        state,
        Some(crate::session::ChildState::Ready),
        "a child whose proxy never acknowledged is never ready"
    );
    assert!(
        !gate.exists(),
        "the gate must stay shut while the boundary is unproven"
    );
    // Stopped *at* the acknowledgement, not before it and not past it.
    let saga =
        h.h.app
            .db
            .child_saga(&h.owner().to_string(), "create-0002")
            .unwrap()
            .expect("the saga row survives an unacknowledged commit");
    assert_eq!(
        saga.step,
        Some(crate::session::SagaStep::Committed),
        "the saga did not stop where the acknowledgement is waited on"
    );
    // …and the caller is still waiting, rather than being told it succeeded.
    assert!(
        bridge_answer(&h.h, 0, "create-0002").is_none(),
        "a create was answered while its boundary was unproven: {:?}",
        bridge_answer(&h.h, 0, "create-0002")
    );
}

/// A `create` whose repository the owner does not work in is refused before
/// anything is minted, and the refusal names which rule it broke.
#[tokio::test]
async fn a_create_outside_the_owners_repositories_is_refused() {
    let mut h = ChildHarness::new();
    queue_request(
        &h.h,
        0,
        "create-0003",
        &envelope(
            "create-0003",
            "create",
            serde_json::json!({
                "repo_root": "/somebody/elses/repo",
                "branch": "feat/one",
                "agent": "worker",
                "task_kind": "task",
                "task_body": "do the thing",
            }),
        ),
    );
    let answer = h.answer("create-0003").await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "repo_not_owned", "{answer}");
    assert!(h.child().is_none(), "nothing was created");
    assert!(!h.effects.called("create_worktree"), "and nothing was made");
}

/// ADR-33's socket check has exactly one production caller — the policy launch
/// in `spawn_inner` — and every other test here runs against a backend that
/// reports no server identity, so removing that call site would fail nothing.
/// This is the launch that must not happen: a server whose `#{socket_path}` is
/// not the one the generated policy denies is a server the policy says nothing
/// about, and a child spawned onto it would be unconstrained in exactly the
/// dimension the deny set exists for.
#[tokio::test]
async fn a_child_is_never_spawned_onto_a_server_the_policy_does_not_deny() {
    let backend = Arc::new(FakeBackend::on_a_socket_the_policy_does_not_name());
    let mut h = ChildHarness::with_watched_backend(Arc::clone(&backend));

    h.create("create-0081");
    let answer = h.answer("create-0081").await;

    assert_eq!(answer["ok"], false, "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .unwrap_or_default()
            .contains("identity_mismatch"),
        "the refusal must name the check that made it: {answer}"
    );
    // Refused *before* the window was opened, which is the whole point: a pane
    // on the wrong server is one friring would then have to find and kill.
    assert!(
        backend.panes.lock().unwrap().is_empty(),
        "a pane was opened on a server the policy does not constrain"
    );
}

/// An agent the profile does not list is refused, whatever the request says.
#[tokio::test]
async fn a_create_naming_an_unlisted_agent_is_refused() {
    let mut h = ChildHarness::new();
    queue_request(
        &h.h,
        0,
        "create-0004",
        &envelope(
            "create-0004",
            "create",
            serde_json::json!({
                "repo_root": "/repo/app",
                "branch": "feat/one",
                "agent": "something-else",
                "task_kind": "task",
                "task_body": "do the thing",
            }),
        ),
    );
    let answer = h.answer("create-0004").await;
    assert_eq!(answer["error"], "agent_not_allowed", "{answer}");
}

/// A `git` that refuses the checkout fails the saga, and the failure is the
/// answer a replay of the same key returns.
#[tokio::test]
async fn a_worktree_that_cannot_be_created_fails_the_saga() {
    let mut h = ChildHarness::new();
    *h.effects.worktree_error.lock().unwrap() = Some(crate::git::ClaimFailure {
        detail: "fatal: branch already checked out".into(),
        owns_branch: false,
    });
    h.create("create-0005");
    let answer = h.answer("create-0005").await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already checked out"),
        "{answer}"
    );
    assert!(h.child().is_none(), "nothing was committed");

    // The journal is what makes a retry safe: the same key returns this answer
    // rather than starting a second saga.
    let journaled =
        h.h.app
            .db
            .bridge_request(&h.owner().to_string(), "create-0005")
            .unwrap()
            .expect("the request is journaled");
    assert_eq!(
        journaled.state,
        crate::storage::bridge::RequestState::Failed,
        "{journaled:?}"
    );
}

/// A seeding that cannot be carried out is `state_unrelocatable`, and the child
/// is never started: there is no shared-state mode and no fallback to one.
#[tokio::test]
async fn a_child_whose_private_state_cannot_be_seeded_is_refused() {
    let mut h = ChildHarness::new();
    *h.effects.seed_error.lock().unwrap() = Some("the hook file is not UTF-8".into());
    h.create("create-0006");
    let answer = h.answer("create-0006").await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "state_unrelocatable", "{answer}");
    assert!(h.child().is_none());
}

/// A child whose pane runs but whose hook never reports is never `ready`: a
/// live process does not show the agent is running from the private state
/// friring seeded.
#[tokio::test]
async fn a_child_whose_hook_never_reports_is_never_ready() {
    let mut h = ChildHarness::new();
    h.create("create-0007");
    h.drive_until(60, |h| {
        h.child()
            .and_then(|c| h.h.app.db.child_saga_of_child(&c.child_id).ok().flatten())
            .and_then(|saga| saga.step)
            == Some(crate::session::SagaStep::Released)
    })
    .await;
    let child = h.child().expect("the saga committed a child");

    // Driven well past the release, because the vacuous version of this rule
    // reads `SessionInfo::status` — which is `Working` from the moment a session
    // is constructed, so it says "ready" on the very first pass after S8.
    h.drive(20).await;
    assert_ne!(
        h.child_state(),
        Some(crate::session::ChildState::Ready),
        "a pane is not proof the agent runs from its private state"
    );
    assert!(bridge_answer(&h.h, 0, "create-0007").is_none());

    // Nor is a report from **before** the gate opened. A resume reuses the
    // child's session id, so a row left by its previous life is exactly the
    // stale proof S9 must not accept.
    let id = child.child_id.parse::<SessionId>().unwrap();
    h.h.app.db.set_hook_state_at(id, "idle", 1).unwrap();
    h.h.app.cached_hook_states = h.h.app.db.load_hook_states().unwrap_or_default();
    assert!(
        h.h.app.cached_hook_states.contains_key(&id),
        "the fixture wrote no hook row"
    );
    h.drive(10).await;
    assert_ne!(
        h.child_state(),
        Some(crate::session::ChildState::Ready),
        "a hook report older than the gate was accepted as this launch's"
    );
}

/// A `result` is a finish **intent**, not a verdict: over a dirty worktree the
/// child lands in `dirty` whatever the outcome claimed, and is never `done`.
#[tokio::test]
async fn a_dirty_worktree_is_never_integrated_whatever_the_child_claimed() {
    for outcome in [
        crate::session::Outcome::Completed,
        crate::session::Outcome::Failed,
    ] {
        let mut h = ChildHarness::new();
        *h.effects.verdict.lock().unwrap() = crate::git::WorktreeVerdict {
            branch: Some("feat/one".into()),
            head: Some("abc1234".into()),
            dirty: true,
            ahead_of_base: 1,
            unreadable: false,
        };
        let child = h.ready_child("create-0008").await;
        h.quiesce(&child.child_id, outcome).await;

        assert_eq!(
            h.child_state(),
            Some(crate::session::ChildState::Dirty),
            "'{outcome}' over a dirty worktree must not be integrated"
        );
        let verdict = h.h.app.db.bridge_result(&child.child_id).unwrap();
        assert!(verdict.is_some_and(|v| v.dirty), "the host recorded dirty");
        // And the slot is still held: the owner has to deal with the worktree.
        assert_eq!(
            h.h.app
                .db
                .live_bridge_children(&h.owner().to_string())
                .unwrap(),
            1
        );
        let owner_mail = h.h.app.db.list_messages(h.owner(), true, None).unwrap();
        assert!(
            owner_mail.iter().any(|m| m.kind == "child.dirty"),
            "{owner_mail:?}"
        );
    }
}

/// A clean worktree takes the intent's own verdict — `failed` stays `failed`.
#[tokio::test]
async fn a_clean_worktree_takes_the_intents_own_verdict() {
    let mut h = ChildHarness::new();
    *h.effects.verdict.lock().unwrap() = crate::git::WorktreeVerdict {
        branch: Some("feat/one".into()),
        head: Some("abc1234".into()),
        dirty: false,
        ahead_of_base: 2,
        unreadable: false,
    };
    let child = h.ready_child("create-0009").await;
    h.quiesce(&child.child_id, crate::session::Outcome::Failed)
        .await;

    assert_eq!(h.child_state(), Some(crate::session::ChildState::Failed));
    let verdict = h.h.app.db.bridge_result(&child.child_id).unwrap().unwrap();
    assert_eq!(verdict.outcome, crate::session::Outcome::Failed);
    assert_eq!(verdict.ahead_of_base, 2);
    assert!(!verdict.dirty);
    // The slot is released.
    assert_eq!(
        h.h.app
            .db
            .live_bridge_children(&h.owner().to_string())
            .unwrap(),
        0
    );
}

/// A completed intent over a clean worktree is the one path to `done`.
#[tokio::test]
async fn a_completed_intent_over_a_clean_worktree_is_done() {
    let mut h = ChildHarness::new();
    *h.effects.verdict.lock().unwrap() = crate::git::WorktreeVerdict {
        branch: Some("feat/one".into()),
        head: Some("abc1234".into()),
        dirty: false,
        ahead_of_base: 1,
        unreadable: false,
    };
    let child = h.ready_child("create-0021").await;
    h.quiesce(&child.child_id, crate::session::Outcome::Completed)
        .await;
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Done));
    let owner_mail = h.h.app.db.list_messages(h.owner(), true, None).unwrap();
    assert!(
        owner_mail.iter().any(|m| m.kind == "child.done"),
        "{owner_mail:?}"
    );
}

/// A worktree git could not read is **dirty**, never verified-complete.
#[tokio::test]
async fn an_unreadable_worktree_is_never_reported_complete() {
    let mut h = ChildHarness::new();
    *h.effects.verdict.lock().unwrap() = crate::git::WorktreeVerdict {
        dirty: true,
        unreadable: true,
        ..Default::default()
    };
    let child = h.ready_child("create-0022").await;
    h.quiesce(&child.child_id, crate::session::Outcome::Completed)
        .await;
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Dirty));
}

/// A pane friring cannot stop lands the child in `stop_failed` — never
/// integrated, never reused, and surfaced for an operator.
#[tokio::test]
async fn a_pane_that_will_not_die_lands_in_stop_failed() {
    let mut h = ChildHarness::with_backend(Arc::new(FakeBackend::unstoppable()));
    let child = h.ready_child("create-0010").await;
    h.quiesce(&child.child_id, crate::session::Outcome::Completed)
        .await;

    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::StopFailed)
    );
    assert!(
        !h.effects.called("verify_worktree"),
        "nothing is verified in a worktree whose agent may still be writing"
    );
    let owner_mail = h.h.app.db.list_messages(h.owner(), true, None).unwrap();
    assert!(
        owner_mail.iter().any(|m| m.kind == "child.stop_failed"),
        "{owner_mail:?}"
    );
}

/// A `result` whose body is not a `ResultBody` is refused and starts no
/// quiesce: "the child asked to finish" is the one message that must never be
/// inferred from free text.
#[tokio::test]
async fn a_malformed_result_starts_no_quiesce() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0011").await;

    // The child's own queue, which the launch minted.
    let dir = crate::paths::bridge_request_dir(&child.child_id).unwrap();
    std::fs::write(
        dir.join("bad-result-1.req"),
        serde_json::to_string(&envelope(
            "bad-result-1",
            "send",
            serde_json::json!({ "to": "owner", "kind": "result", "body": "I am done!" }),
        ))
        .unwrap(),
    )
    .unwrap();
    let response = crate::paths::bridge_response_dir(&child.child_id)
        .unwrap()
        .join("bad-result-1.res");
    h.drive_until(60, |_| response.exists()).await;

    let answer: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&response).expect("the broker answered"))
            .unwrap();
    assert_eq!(answer["ok"], false, "{answer}");
    assert_ne!(
        h.child_state(),
        Some(crate::session::ChildState::Finishing),
        "a free-text result must not be read as a finish intent"
    );
    assert_eq!(h.h.app.child_lifecycle.in_flight(), 0);
}

/// One `create` key makes one child however often it is replayed, and the
/// replay returns the first attempt's exact bytes.
#[tokio::test]
async fn a_replayed_create_returns_the_first_answer_and_makes_no_second_child() {
    let mut h = ChildHarness::new();
    h.ready_child("create-0012").await;
    let first = bridge_answer(&h.h, 0, "create-0012").expect("the saga answered");

    // The same key and the same body again.
    h.create("create-0012");
    h.drive(12).await;
    let second = bridge_answer(&h.h, 0, "create-0012").expect("a replay is answered");
    assert_eq!(first, second, "a replay returns the first attempt's answer");
    assert_eq!(
        h.h.app
            .db
            .bridge_children_of(&h.owner().to_string())
            .unwrap()
            .len(),
        1,
        "one key, one child"
    );
}

/// A recipient nudged repeatedly with no bridge call in return is marked
/// `stalled` **once** and told about once, rather than typed at forever.
///
/// What this cannot cover is the *delivery*: `send_prompt_now` talks to a real
/// tmux, and this harness's backend is not one — a nudge here never leaves. So
/// the give-up rule is asserted from the counter it actually runs on, and
/// delivery into a live pane is left to the harnesses that have a multiplexer
/// (`docs/E2E.md`).
#[tokio::test]
async fn a_recipient_that_never_answers_is_marked_once_and_not_nudged_forever() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0080").await;
    let id: SessionId = child.child_id.parse().unwrap();

    h.h.app.owe_bridge_nudge_for_test(id);
    for _ in 0..12 {
        h.h.app.force_bridge_nudge_unanswered_for_test(id);
        h.drive(1).await;
    }

    assert_eq!(
        h.h.app
            .db
            .bridge_child_state(&child.child_id)
            .unwrap()
            .map(|row| row.state),
        Some(crate::session::ChildState::Stalled),
        "a recipient that never answers was nudged forever"
    );
    // Said once, not every tick: `stalled` is attention, and repeating it would
    // bury the owner's mailbox under one child.
    let owner_mail =
        h.h.app
            .db
            .list_messages(h.owner(), false, Some(50))
            .unwrap();
    assert_eq!(
        owner_mail
            .iter()
            .filter(|m| m.kind == crate::session::bridge::MailKind::ChildStalled.as_str())
            .count(),
        1,
        "the owner was told more than once: {owner_mail:?}"
    );
}

/// **Any** bridge call resets the unanswered-nudge count — not only an
/// `inbox --claim`.
///
/// The rule the stall watch enforces is that the agent is still talking to
/// friring, so a `status` answers a nudge exactly as reading the mail does. A
/// child one nudge below the give-up threshold that calls `status` and is then
/// nudged again must not be marked `stalled`: `stalled` is sticky (nothing
/// moves it back but an operator), so a count that only mail could spend would
/// retire a child that was answering all along.
#[tokio::test]
async fn any_bridge_call_from_a_nudged_child_resets_its_unanswered_count() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0081").await;
    let id: SessionId = child.child_id.parse().unwrap();

    // One nudge below the threshold, with nothing back yet.
    h.h.app.owe_bridge_nudge_for_test(id);
    for _ in 0..(crate::app::bridge::MAX_UNANSWERED_NUDGES - 1) {
        h.h.app.force_bridge_nudge_unanswered_for_test(id);
        h.drive(1).await;
    }
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Ready),
        "the child was retired before the threshold"
    );

    // A `status` from the child — not an `inbox --claim`.
    let dir = crate::paths::bridge_request_dir(&child.child_id).unwrap();
    std::fs::write(
        dir.join("status-0081.req"),
        serde_json::to_string(&envelope("status-0081", "status", serde_json::json!({}))).unwrap(),
    )
    .unwrap();
    let response = crate::paths::bridge_response_dir(&child.child_id)
        .unwrap()
        .join("status-0081.res");
    h.drive_until(60, |_| response.exists()).await;
    assert!(response.exists(), "the child's status was never answered");

    // The count is spent, so the next nudge is the first of a new run.
    h.h.app.force_bridge_nudge_unanswered_for_test(id);
    h.drive(2).await;
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Ready),
        "a child that answered friring with 'status' was still marked stalled"
    );
}

/// A bridge call resets the unanswered **count** and does not discard the nudge
/// the recipient is still owed.
///
/// The record is the only trace that mail is waiting, and `owe_bridge_nudge`
/// recreates it only when *new* mail arrives. Dropped on any call, a recipient
/// that answers `status` promptly and never opens its inbox is never reminded
/// again — and the mail it was owed a reminder about sits unread for ever, with
/// the stall watch that would have surfaced it spent.
#[tokio::test]
async fn a_bridge_call_spends_the_nudge_count_and_not_the_nudge() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0092").await;
    let id: SessionId = child.child_id.parse().unwrap();

    // Mail the child something and leave it unread — the state the reminder
    // exists for.
    h.h.app.host_mail(
        id,
        crate::session::bridge::MailKind::Cancel,
        &child.child_id,
    );
    assert!(h.h.app.db.count_unread_messages(id).unwrap() > 0);
    h.h.app.force_bridge_nudge_unanswered_for_test(id);
    assert_eq!(h.h.app.bridge_nudge_owed_for_test(id), Some(1));

    // A `status` from the child. Not a claim: it never reads its mail.
    let dir = crate::paths::bridge_request_dir(&child.child_id).unwrap();
    std::fs::write(
        dir.join("status-0092.req"),
        serde_json::to_string(&envelope("status-0092", "status", serde_json::json!({}))).unwrap(),
    )
    .unwrap();
    let response = crate::paths::bridge_response_dir(&child.child_id)
        .unwrap()
        .join("status-0092.res");
    h.drive_until(60, |_| response.exists()).await;
    assert!(response.exists(), "the child's status was never answered");

    assert_eq!(
        h.h.app.bridge_nudge_owed_for_test(id),
        Some(0),
        "a bridge call must spend the count and keep the reminder"
    );

    // And when the mail is actually taken, the reminder is discharged — the
    // record means "this recipient has mail it has not taken delivery of", and
    // every drain path settles it rather than only `inbox --claim`.
    h.h.app.db.claim_messages(id, Some(20)).unwrap();
    h.drive(3).await;
    assert_eq!(
        h.h.app.bridge_nudge_owed_for_test(id),
        None,
        "a drained mailbox is still owed a nudge"
    );
}

/// One stalled child must not starve every other nudge in the process.
///
/// `tick_bridge_nudges` types at most one nudge per pass and picks the
/// longest-waiting recipient. The exhausted branch used to return *without*
/// restarting that recipient's interval, so its `at` never moved again: it won
/// `max_by_key(elapsed)` on every later pass and returned, and no other
/// recipient was ever reached. A stalled child is exactly the case that
/// persists — `stalled` is sticky and only an operator clears it — so one
/// unanswering worker silenced the nudge for its own **owner**, which is the
/// session that has to notice it.
#[tokio::test]
async fn a_stalled_child_does_not_starve_its_owners_nudge() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0140").await;
    let child_id: SessionId = child.child_id.parse().unwrap();
    let owner = h.owner();

    // Both records are built here rather than left to whatever the launch
    // happened to leave behind, so which one a pass picks is decided by this
    // test and not by an accident of ordering. `owe_bridge_nudge` back-dates a
    // new record a whole interval, so both are due at once — and the child's is
    // aged *first*, which makes it the longest-waiting and therefore the one
    // every pass selects while it holds its place.
    h.h.app.forget_bridge_nudges_for_test();
    h.h.app.owe_bridge_nudge_for_test(child_id);
    for _ in 0..crate::app::bridge::MAX_UNANSWERED_NUDGES {
        h.h.app.force_bridge_nudge_unanswered_for_test(child_id);
    }
    h.h.app.owe_bridge_nudge_for_test(owner);
    assert_eq!(h.h.app.bridge_nudge_due_for_test(owner), Some(true));
    assert!(
        h.h.app.db.count_unread_messages(child_id).unwrap() > 0
            && h.h.app.db.count_unread_messages(owner).unwrap() > 0,
        "both recipients need unread mail, or their records are discharged \
         rather than contended"
    );

    h.drive_until(30, |h| {
        h.child_state() == Some(crate::session::ChildState::Stalled)
    })
    .await;
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Stalled),
        "the child never reached the give-up threshold"
    );
    h.drive(6 * crate::app::bridge::POLL_TICKS as usize).await;

    // The give-up branch restarted the child's interval, so it stopped being
    // the longest-waiting…
    assert_eq!(
        h.h.app.bridge_nudge_due_for_test(child_id),
        Some(false),
        "a recipient past its allowance kept its place at the front of the queue"
    );
    // …and the owner, which had been waiting behind it, was reached. A record
    // is minted *due*, so one that is no longer due is one a pass got to —
    // which is the only thing this can observe, since the harness has no
    // multiplexer and no nudge ever actually leaves.
    assert_eq!(
        h.h.app.bridge_nudge_due_for_test(owner),
        Some(false),
        "a stalled child held the nudge slot and its owner was never reached"
    );
    // And the stalled child is still owed one — it is quietened, not forgotten,
    // so a resume finds the reminder for the mail it never read.
    assert!(
        h.h.app.bridge_nudge_owed_for_test(child_id).is_some(),
        "the stalled child's owed nudge was discarded"
    );
}

/// A recipient this instance cannot type into keeps its owed nudge.
///
/// The debt was dropped whenever the recipient was absent from `sessions`, and
/// `owe_bridge_nudge` recreates one only when **new** mail arrives — so mail
/// already queued for a parked child, for a session another friring on the same
/// database has loaded, or for one this instance has unloaded was never
/// announced by anybody, however long the recipient ran afterwards.
///
/// A parked child is the exact case: `stop` mails it `cancel` and *then* retires
/// its runtime, so the mail lands and the recipient leaves the session list in
/// one operation.
#[tokio::test]
async fn a_parked_recipient_keeps_the_nudge_it_is_owed() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0141").await;
    let child_id: SessionId = child.child_id.parse().unwrap();

    queue_request(
        &h.h,
        0,
        "stop-0141",
        &envelope(
            "stop-0141",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    let stopped = h.answer("stop-0141").await;
    assert_eq!(stopped["data"]["state"], "stopped", "{stopped}");
    assert!(
        !h.h.app
            .sessions
            .iter()
            .any(|session| session.info.id == child_id),
        "the parked child is still in the session list, so this proves nothing"
    );
    assert!(
        h.h.app.db.count_unread_messages(child_id).unwrap() > 0,
        "the parked child has no unread mail to be owed a nudge about"
    );

    // The parked child is made the only candidate, so a pass has to select it
    // — otherwise "the record survived" would also be true of a record no pass
    // ever looked at.
    h.h.app.forget_bridge_nudges_for_test();
    h.h.app.owe_bridge_nudge_for_test(child_id);
    h.drive(3 * crate::app::bridge::POLL_TICKS as usize).await;

    assert_eq!(
        h.h.app.bridge_nudge_due_for_test(child_id),
        Some(false),
        "no pass reached the parked child, so nothing here is about what a pass does with one"
    );
    assert!(
        h.h.app.bridge_nudge_owed_for_test(child_id).is_some(),
        "the reminder for mail a parked child has never read was thrown away"
    );
}

/// The owed-nudge set is re-derived from the mailbox, so it survives the things
/// that lose it.
///
/// `BridgeState` is per process: a restart, or a handover between two friring
/// instances on one database, starts with no record at all — and the mail those
/// records were about is still sitting unread. Nothing announced it, because
/// `owe_bridge_nudge` fires on arrival and the arrival already happened.
#[tokio::test]
async fn a_restart_re_derives_the_owed_nudges_from_the_mailbox() {
    let mut h = ChildHarness::new();
    let owner = h.owner();
    h.h.app.host_mail(
        owner,
        crate::session::bridge::MailKind::ChildStalled,
        "some-child",
    );
    assert!(h.h.app.db.count_unread_messages(owner).unwrap() > 0);

    // What a fresh process starts with.
    h.h.app.forget_bridge_nudges_for_test();
    assert_eq!(h.h.app.bridge_nudge_owed_for_test(owner), None);

    h.drive_until(400, |h| h.h.app.bridge_nudge_owed_for_test(owner).is_some())
        .await;

    assert!(
        h.h.app.bridge_nudge_owed_for_test(owner).is_some(),
        "unread mail that predates this instance is never announced to its recipient"
    );
}

/// A child's inbox is held to the plan's 50, not the generic 500.
///
/// Both sides of a child's mailbox are agent-chosen — its owner decides how much
/// to send and the child decides when to drain — and at the bridge's 64 KiB body
/// cap the generic ceiling is ~32 MiB of undrained mail per recipient.
#[tokio::test]
async fn a_childs_inbox_is_capped_tighter_than_an_ordinary_one() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0093").await;
    let id: SessionId = child.child_id.parse().unwrap();
    let cap = crate::session::bridge::MAX_UNREAD_PER_CHILD;

    // Fill it to the cap through the same path the owner's `send` takes.
    for _ in 0..cap {
        h.h.app.host_mail(
            id,
            crate::session::bridge::MailKind::Cancel,
            &child.child_id,
        );
    }
    assert_eq!(
        h.h.app.db.count_unread_messages(id).unwrap(),
        cap,
        "the child's mailbox did not fill to the bridge cap"
    );

    // The owner's own `send` is refused, and told why — backpressure it can act
    // on rather than a message quietly lost.
    queue_request(
        &h.h,
        0,
        "send-0093",
        &envelope(
            "send-0093",
            "send",
            serde_json::json!({ "to": child.child_id, "kind": "answer", "body": "one more" }),
        ),
    );
    let answer = h.answer("send-0093").await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "quota", "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .unwrap_or_default()
            .contains(&cap.to_string()),
        "the refusal did not name the cap it hit: {answer}"
    );
    // Well under the generic ceiling, which is the point.
    assert!(cap < crate::storage::messages::MAX_UNREAD_PER_RECIPIENT);
}

/// The other half of the pair: an **owner** is held to 200, not to the child's
/// 50 and not to the generic 500.
///
/// An owner is the recipient of every one of its children plus friring's own
/// host mail, so it legitimately accumulates a deeper backlog — but a child that
/// can fill its owner's inbox is a child that can starve a fan-out leader, so
/// the deeper cap is still a cap.
#[tokio::test]
async fn an_owners_inbox_is_capped_wider_than_a_childs_but_still_capped() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0094").await;
    let owner = h.owner();
    let cap = crate::session::bridge::MAX_UNREAD_PER_OWNER;
    assert!(cap > crate::session::bridge::MAX_UNREAD_PER_CHILD);

    // Fill it through friring's own host mail, which takes the same cap. Had
    // the owner been charged the child's cap, this would settle at 50.
    for _ in 0..cap {
        h.h.app.host_mail(
            owner,
            crate::session::bridge::MailKind::ChildDone,
            &child.child_id,
        );
    }
    assert_eq!(
        h.h.app.db.count_unread_messages(owner).unwrap(),
        cap,
        "the owner's mailbox did not fill to the owner cap"
    );

    // The child's own `send` upward is then refused, with the owner's cap named.
    let dir = crate::paths::bridge_request_dir(&child.child_id).unwrap();
    std::fs::write(
        dir.join("blocked-0094.req"),
        serde_json::to_string(&envelope(
            "blocked-0094",
            "send",
            serde_json::json!({ "to": "owner", "kind": "blocked", "body": "which branch?" }),
        ))
        .unwrap(),
    )
    .unwrap();
    let response = crate::paths::bridge_response_dir(&child.child_id)
        .unwrap()
        .join("blocked-0094.res");
    h.drive_until(60, |_| response.exists()).await;

    let answer: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&response).expect("the broker answered"))
            .unwrap();
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "quota", "{answer}");
    let message = answer["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&format!("({cap} unread)")),
        "the refusal named a cap that is not the owner's: {answer}"
    );
    assert!(cap < crate::storage::messages::MAX_UNREAD_PER_RECIPIENT);
}

/// The constraint two racing instances rest on: `(owner, request_key)` is
/// unique, so a second commit under one `create` key is refused by the table
/// rather than making a second child.
///
/// Deliberately named for the **constraint** and not for the race. Two real
/// brokers are two processes against one file, which this harness has no way to
/// arrange — the broker lease that keeps them off each other's queues is a
/// courtesy, and this row is the guard. A test claiming to exercise two
/// instances while inserting twice in sequence would be claiming the harder
/// thing and asserting the easier one.
#[test]
fn one_create_key_admits_exactly_one_ownership_row() {
    let db = crate::storage::Database::open_in_memory().unwrap();
    db.insert_bridge_child("child-a", "owner", "create-0013")
        .unwrap();
    assert!(
        db.insert_bridge_child("child-b", "owner", "create-0013")
            .is_err(),
        "a second child under one create key must be refused by the table"
    );
    assert_eq!(db.bridge_children_of("owner").unwrap().len(), 1);
}

/// A `resume` relaunches the **same** `child_id` and keeps its ownership row.
#[tokio::test]
async fn a_resume_relaunches_the_same_child_and_keeps_its_ownership() {
    let mut h = ChildHarness::new();
    *h.effects.verdict.lock().unwrap() = crate::git::WorktreeVerdict {
        dirty: true,
        ..Default::default()
    };
    let child = h.ready_child("create-0014").await;
    h.quiesce(&child.child_id, crate::session::Outcome::Completed)
        .await;
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Dirty));

    let creates_before = h
        .effects
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.as_str() == "create_worktree")
        .count();

    queue_request(
        &h.h,
        0,
        "resume-0001",
        &envelope(
            "resume-0001",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    // Driven to the **relaunched pane**, not to `starting`: `begin_resume`
    // writes that column before its job runs, so an implementation that only
    // flipped it and relaunched nothing would satisfy a `Starting` stop
    // condition.
    h.drive_until(60, |h| {
        h.h.app
            .db
            .child_saga_of_child(&child.child_id)
            .ok()
            .flatten()
            .and_then(|saga| saga.step)
            == Some(crate::session::SagaStep::Released)
    })
    .await;
    assert!(
        h.h.app
            .sessions
            .iter()
            .any(|s| s.info.id == child.child_id.parse().unwrap()),
        "a resume left no running session for the child"
    );
    assert_eq!(
        h.effects
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_str() == "create_worktree")
            .count(),
        creates_before,
        "a resume relaunches in the child's existing worktree and must never re-run S2: the real \
         effect cuts a new branch, so it would fail on the one the child already has"
    );

    // The same child, the same ownership row.
    let children =
        h.h.app
            .db
            .bridge_children_of(&h.owner().to_string())
            .unwrap();
    assert_eq!(children.len(), 1, "a resume makes no second child");
    assert_eq!(children[0].child_id, child.child_id);
    assert_eq!(children[0].request_key, "create-0014");
    assert_ne!(
        h.child_state(),
        Some(crate::session::ChildState::Dirty),
        "a resume moves the child out of dirty"
    );

    // The relaunched agent reports the **same word** its previous life ended on,
    // through the file channel it really uses. `apply_status_signals` drops a
    // file repeating the recorded state, so unless the gate release cleared the
    // child's hook row this report never re-stamps `state_at` and S9 waits out
    // its readiness timeout on a child that is running perfectly well.
    for _ in 0..60 {
        h.h.tick();
        h.h.app.tick_child_sagas();
        h.child_signals("idle");
        if h.child_state() == Some(crate::session::ChildState::Ready) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Ready),
        "a resumed child that reports the same state as before must still prove ready"
    );
    let answer = h.answer("resume-0001").await;
    assert_eq!(answer["ok"], true, "{answer}");
}

/// A clean owner stop is the bridge's slot-releasing parking operation: the
/// process goes away, while the same child, ownership and worktree can be
/// relaunched explicitly later.
#[tokio::test]
async fn a_stopped_child_releases_its_slot_and_resumes_as_the_same_child() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0104").await;
    let child_id: SessionId = child.child_id.parse().unwrap();
    let creates_before = h
        .effects
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.as_str() == "create_worktree")
        .count();

    queue_request(
        &h.h,
        0,
        "stop-0104",
        &envelope(
            "stop-0104",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    let stopped = h.answer("stop-0104").await;
    assert_eq!(stopped["data"]["state"], "stopped", "{stopped}");
    assert_eq!(
        h.h.app
            .db
            .bridge_child_state(&child.child_id)
            .unwrap()
            .map(|row| row.state),
        Some(crate::session::ChildState::Stopped)
    );
    assert_eq!(
        h.h.app
            .db
            .live_bridge_children(&h.owner().to_string())
            .unwrap(),
        0,
        "a clean stop kept its fan-out slot"
    );
    assert!(
        !h.h.app
            .sessions
            .iter()
            .any(|session| session.info.id == child_id),
        "a stopped child kept a live session runtime"
    );

    queue_request(
        &h.h,
        0,
        "resume-0104",
        &envelope(
            "resume-0104",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    h.drive_reporting(60, |h| {
        h.child_state() == Some(crate::session::ChildState::Ready)
    })
    .await;
    let resumed = h.answer("resume-0104").await;
    assert_eq!(resumed["ok"], true, "{resumed}");
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Ready));
    assert_eq!(
        h.h.app
            .db
            .bridge_children_of(&h.owner().to_string())
            .unwrap()
            .into_iter()
            .map(|row| row.child_id)
            .collect::<Vec<_>>(),
        vec![child.child_id.clone()],
        "resume replaced the stopped child instead of relaunching it"
    );
    assert_eq!(
        h.effects
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.as_str() == "create_worktree")
            .count(),
        creates_before,
        "resume recreated a stopped child's preserved worktree"
    );
}

/// A `resume` comes back to the child's **conversation**, not merely to its
/// files.
///
/// The worktree, the branch, the mailbox and the private state directory are
/// half of what an owner parked; the thread is the other half, and for an agent
/// that has one it is the half that decides whether the worker still knows what
/// it was doing. friring keeps the child's `agent_session_id` across the
/// relaunch and emits the agent's own resume group, so the launch reopens that
/// conversation rather than minting one.
///
/// Reverting `child_resume_identity` to the fresh `Uuid::new_v4()` every launch
/// used to get fails this at the id assertion: the resumed child comes back
/// under a conversation nothing has ever written to.
#[tokio::test]
async fn a_resume_keeps_the_childs_conversation_identity() {
    let mut h = ChildHarness::new();
    // A worker that can resume the way codex and opencode do: id-less flags,
    // resolved against the launch directory — which for a child is the worktree
    // friring kept for it.
    let resuming =
        h.h.app
            .agents
            .agents
            .iter_mut()
            .find(|def| def.name == "worker")
            .expect("the harness worker");
    resuming.resume_args = vec!["resume".into(), "--last".into()];
    resuming.resume_latest = true;
    // And where that agent's conversations live, which is what makes the resume
    // *provable* rather than hopeful — `resume --last` in an empty directory is
    // not a resume, and friring refuses one it cannot prove.
    resuming.transcript = Some(crate::session::TranscriptDef {
        dir: "sessions".into(),
        suffix: ".jsonl".into(),
        name_has_id: false,
    });

    let child = h.ready_child("create-0160").await;
    let child_id: SessionId = child.child_id.parse().unwrap();
    let created_conversation =
        h.h.app
            .db
            .get_session_by_id(child_id)
            .unwrap()
            .and_then(|row| row.agent_session_id.clone())
            .expect("a created child records the conversation it started");
    // What the agent wrote in its own private state directory (ADR-31) while it
    // was running. Nothing seeds this; it is the evidence the thread exists.
    let thread = crate::sandbox::dirs::child_state_dir(&child.child_id)
        .expect("the child's private state directory")
        .join("sessions/2026/09/rollout-0160.jsonl");
    std::fs::create_dir_all(thread.parent().unwrap()).unwrap();
    std::fs::write(&thread, "{\"thread\":\"0160\"}\n").unwrap();

    queue_request(
        &h.h,
        0,
        "stop-0160",
        &envelope(
            "stop-0160",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    assert_eq!(h.answer("stop-0160").await["data"]["state"], "stopped");

    queue_request(
        &h.h,
        0,
        "resume-0160",
        &envelope(
            "resume-0160",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    h.drive_reporting(60, |h| {
        h.child_state() == Some(crate::session::ChildState::Ready)
    })
    .await;
    let resumed = h.answer("resume-0160").await;
    assert_eq!(resumed["ok"], true, "{resumed}");

    assert_eq!(
        h.h.app
            .db
            .get_session_by_id(child_id)
            .unwrap()
            .and_then(|row| row.agent_session_id.clone())
            .as_deref(),
        Some(created_conversation.as_str()),
        "the resumed child came back under a different conversation"
    );
}

/// The **by-id** half of the same contract, against a transcript in the child's
/// own private state directory.
///
/// An agent that resumes by id (claude's shape) puts the id friring minted on
/// the command line, so the question is not "is there a conversation here" but
/// "is *this* one here". Both answers are exercised, in the order they happen to
/// a parked worker: the transcript is absent first — the state a child has
/// before its agent has written anything, and the state
/// `a_resume_reads_the_childs_own_state_directory` used to launch blank into —
/// and present second.
///
/// The directory searched is the child's, never the operator's: it comes from
/// `child_env`, which points the agent's own `config_dir_env` at the private
/// state directory ADR-31 gives the child. Pointing it at the default location
/// would ask about the operator's conversations, and answer `true` for a child
/// whose own thread does not exist.
#[tokio::test]
async fn a_by_id_resume_is_decided_by_the_transcript_in_the_childs_private_state() {
    let mut h = ChildHarness::new();
    let by_id =
        h.h.app
            .agents
            .agents
            .iter_mut()
            .find(|def| def.name == "worker")
            .expect("the harness worker");
    by_id.resume_args = vec!["--resume".into(), "{id}".into()];
    by_id.resume_latest = false;
    by_id.transcript = Some(crate::session::TranscriptDef {
        dir: "projects".into(),
        suffix: ".jsonl".into(),
        name_has_id: true,
    });

    let child = h.ready_child("create-0170").await;
    let child_id: SessionId = child.child_id.parse().unwrap();
    let conversation =
        h.h.app
            .db
            .get_session_by_id(child_id)
            .unwrap()
            .and_then(|row| row.agent_session_id.clone())
            .expect("a created child records the conversation it started");

    queue_request(
        &h.h,
        0,
        "stop-0170",
        &envelope(
            "stop-0170",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    assert_eq!(h.answer("stop-0170").await["data"]["state"], "stopped");

    // Nothing written yet: refused, and the child is left where it was.
    queue_request(
        &h.h,
        0,
        "resume-0170a",
        &envelope(
            "resume-0170a",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    let refused = h.answer("resume-0170a").await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Stopped));

    // The transcript the agent wrote for *this* conversation, under the private
    // state directory friring gave it.
    let state_dir = crate::sandbox::dirs::child_state_dir(&child.child_id)
        .expect("the child's private state directory");
    let transcript = state_dir
        .join("projects/-repo")
        .join(format!("{conversation}.jsonl"));
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{\"role\":\"user\"}\n").unwrap();

    // A neighbouring conversation must not answer for this one.
    std::fs::write(
        transcript.with_file_name("11111111-2222-3333-4444-555555555555.jsonl"),
        "{\"role\":\"user\"}\n",
    )
    .unwrap();

    let identity =
        h.h.app
            .child_resume_identity(child_id, "worker", &h.h.app.child_env(child_id))
            .expect("the conversation is on disk");
    assert_eq!(identity.agent_session_id, conversation);
    assert_eq!(
        identity.resume_trigger.as_deref(),
        Some(conversation.as_str()),
        "a by-id resume must put the child's own id on the command line"
    );

    queue_request(
        &h.h,
        0,
        "resume-0170b",
        &envelope(
            "resume-0170b",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    h.drive_reporting(60, |h| {
        h.child_state() == Some(crate::session::ChildState::Ready)
    })
    .await;
    let resumed = h.answer("resume-0170b").await;
    assert_eq!(resumed["ok"], true, "{resumed}");
    assert_eq!(
        h.h.app
            .db
            .get_session_by_id(child_id)
            .unwrap()
            .and_then(|row| row.agent_session_id.clone())
            .as_deref(),
        Some(conversation.as_str()),
        "the resumed child came back under a different conversation"
    );
}

/// An agent that **can** resume but never said where its conversations live is
/// refused, rather than resumed on the assumption that one is there.
///
/// The hole a directory-existence check leaves: `resume --last` resolves to
/// whatever the agent finds, and an agent nobody has declared a transcript for
/// gives friring nothing to check — so a "resume" that starts a brand-new
/// conversation under the parked child's id is indistinguishable from one that
/// came back. friring will not guess, and the refusal names the block to add.
///
/// Generic by construction: the check asks the registry what this agent
/// declared, never what it is. No agent name appears in the decision.
#[tokio::test]
async fn a_resume_is_refused_when_the_agent_declares_no_transcript() {
    let mut h = ChildHarness::new();
    let resuming =
        h.h.app
            .agents
            .agents
            .iter_mut()
            .find(|def| def.name == "worker")
            .expect("the harness worker");
    resuming.resume_args = vec!["resume".into(), "--last".into()];
    resuming.resume_latest = true;
    resuming.transcript = None;

    let child = h.ready_child("create-0171").await;
    queue_request(
        &h.h,
        0,
        "stop-0171",
        &envelope(
            "stop-0171",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    assert_eq!(h.answer("stop-0171").await["data"]["state"], "stopped");

    queue_request(
        &h.h,
        0,
        "resume-0171",
        &envelope(
            "resume-0171",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    let refused = h.answer("resume-0171").await;
    assert_eq!(refused["ok"], false, "{refused}");
    let message = refused["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("transcript"),
        "the refusal must name what is missing: {refused}"
    );
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Stopped),
        "a refused resume moved the child"
    );

    // And the same for an agent that resumes **by id**, which is the branch that
    // used to be waved through: `resume_trigger_for` would fall back to one
    // vendor's on-disk layout, so a stale file of *that* vendor's could
    // authorize a resume for an agent whose own conversation store friring never
    // looked at. The child here has a recorded id and no declaration.
    let by_id =
        h.h.app
            .agents
            .agents
            .iter_mut()
            .find(|def| def.name == "worker")
            .expect("the harness worker");
    by_id.resume_args = vec!["--resume".into(), "{id}".into()];
    by_id.resume_latest = false;
    by_id.transcript = None;

    queue_request(
        &h.h,
        0,
        "resume-0171b",
        &envelope(
            "resume-0171b",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    let refused = h.answer("resume-0171b").await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap_or_default()
            .contains("transcript"),
        "a by-id agent with no declaration must be refused too: {refused}"
    );
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Stopped),
        "a refused resume moved the child"
    );
}

/// A refused resume must leave a **live** child's pane running.
///
/// `stalled` is set from the nudge counter alone, with the child's agent still
/// running, and `begin_resume` stops that pane before relaunching. So the order
/// matters: a refusal decided *after* the stop leaves a child whose agent has
/// been killed and whose resume did not happen — the worst of both, and
/// unrecoverable without a second resume that would then be refused for the
/// same reason. Starting from `stopped`, as the other refusal tests do, cannot
/// see it: there is no pane left to kill.
#[tokio::test]
async fn a_refused_resume_leaves_a_stalled_childs_pane_running() {
    let backend = Arc::new(FakeBackend::spawnable());
    let mut h = ChildHarness::with_watched_backend(Arc::clone(&backend));
    let resuming =
        h.h.app
            .agents
            .agents
            .iter_mut()
            .find(|def| def.name == "worker")
            .expect("the harness worker");
    resuming.resume_args = vec!["resume".into(), "--last".into()];
    resuming.resume_latest = true;
    // Declared, and its directory deliberately left empty: the conversation
    // cannot be reached, so the resume must be refused.
    resuming.transcript = Some(crate::session::TranscriptDef {
        dir: "sessions".into(),
        suffix: ".jsonl".into(),
        name_has_id: false,
    });

    let child = h.ready_child("create-0172").await;
    let running_pane = {
        let panes = backend.panes.lock().unwrap();
        assert_eq!(panes.len(), 1, "a ready child is one pane: {panes:?}");
        panes.keys().next().cloned().unwrap()
    };
    h.h.app
        .db
        .set_bridge_child_state(&child.child_id, crate::session::ChildState::Stalled)
        .unwrap();

    queue_request(
        &h.h,
        0,
        "resume-0172",
        &envelope(
            "resume-0172",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    let refused = h.answer("resume-0172").await;
    assert_eq!(refused["ok"], false, "{refused}");

    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Stalled),
        "a refused resume moved a live child"
    );
    let panes = backend.panes.lock().unwrap();
    assert_eq!(
        panes.len(),
        1,
        "a refused resume spawned or lost a pane: {panes:?}"
    );
    assert!(
        panes.contains_key(&running_pane),
        "a refused resume killed the running agent it refused to relaunch"
    );
}

/// A `resume` that cannot reach the conversation is **refused**.
///
/// Never launched blank, because a blank one is indistinguishable from the
/// outside: the child comes up, answers its mail, has forgotten the task, and
/// nothing anywhere says so. An agent that resumes by id has a transcript
/// friring can look for, and its absence is the case this covers.
///
/// An agent that declares no resume contract at all is deliberately *not* one of
/// these — a `/bin/sh` worker has no thread to lose — which is what
/// `a_stopped_child_releases_its_slot_and_resumes_as_the_same_child` still
/// exercises on the default harness agent.
#[tokio::test]
async fn a_resume_is_refused_when_the_conversation_cannot_be_reached() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0161").await;

    queue_request(
        &h.h,
        0,
        "stop-0161",
        &envelope(
            "stop-0161",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    assert_eq!(h.answer("stop-0161").await["data"]["state"], "stopped");

    // Declared *after* the child was created, so the transcript this contract
    // implies was never written — the shape a claude worker has when its
    // transcript has been cleaned up under it.
    let by_id =
        h.h.app
            .agents
            .agents
            .iter_mut()
            .find(|def| def.name == "worker")
            .expect("the harness worker");
    by_id.resume_args = vec!["--resume".into(), "{id}".into()];
    by_id.resume_latest = false;
    by_id.transcript = Some(crate::session::TranscriptDef {
        dir: "projects".into(),
        suffix: ".jsonl".into(),
        name_has_id: true,
    });

    queue_request(
        &h.h,
        0,
        "resume-0161",
        &envelope(
            "resume-0161",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    let refused = h.answer("resume-0161").await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cannot reach this child's"),
        "the refusal must name the conversation it could not reach: {refused}"
    );
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Stopped),
        "a refused resume moved the child"
    );
}

/// Parking is only worth having if the parked worker is still there afterwards.
///
/// A clean stop retires the child's **runtime** and nothing else: its private
/// agent state (ADR-31) — where an interactive Codex worker's own thread lives —
/// its bridge channel and its ownership row all survive, and a resume re-seeds
/// the family files it is declared to seed without touching what the agent
/// wrote. Without that, "resume the same child" would relaunch an agent with no
/// memory of what it was doing, which is a replacement worker under an old id.
#[tokio::test]
async fn parking_a_child_preserves_its_private_agent_state() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0132").await;
    let state_dir = crate::sandbox::dirs::child_state_dir(&child.child_id)
        .expect("the child's private state directory");
    // What the agent itself writes there, which nothing seeds and nothing may
    // remove: a Codex rollout is exactly this shape.
    let thread = state_dir.join("sessions/2026/rollout-parked.jsonl");
    std::fs::create_dir_all(thread.parent().unwrap()).unwrap();
    std::fs::write(&thread, "{\"thread\":\"parked\"}\n").unwrap();
    let channel = crate::paths::bridge_request_dir(&child.child_id).unwrap();
    assert!(channel.is_dir(), "the child has no bridge channel to keep");

    queue_request(
        &h.h,
        0,
        "stop-0132",
        &envelope(
            "stop-0132",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    let stopped = h.answer("stop-0132").await;
    assert_eq!(stopped["data"]["state"], "stopped", "{stopped}");
    assert!(
        thread.exists(),
        "a clean stop destroyed the parked worker's own thread"
    );
    assert!(channel.is_dir(), "a clean stop removed the child's channel");
    assert!(
        h.h.app.db.bridge_child(&child.child_id).unwrap().is_some(),
        "a clean stop dropped the ownership row"
    );

    queue_request(
        &h.h,
        0,
        "resume-0132",
        &envelope(
            "resume-0132",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    h.drive_reporting(60, |h| {
        h.child_state() == Some(crate::session::ChildState::Ready)
    })
    .await;
    let resumed = h.answer("resume-0132").await;
    assert_eq!(resumed["ok"], true, "{resumed}");
    assert_eq!(
        std::fs::read_to_string(&thread).ok().as_deref(),
        Some("{\"thread\":\"parked\"}\n"),
        "the resume re-seeded over the worker's own thread"
    );
}

/// A stopped or unusable child no longer owns a slot. Resuming one must claim
/// capacity before changing its state or starting a pane, just like a create.
#[tokio::test]
async fn a_released_child_resume_refuses_when_the_fanout_is_full() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0105").await;
    queue_request(
        &h.h,
        0,
        "stop-0105",
        &envelope(
            "stop-0105",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    let stopped = h.answer("stop-0105").await;
    assert_eq!(stopped["data"]["state"], "stopped", "{stopped}");
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Stopped));

    let owner = h.owner().to_string();
    for n in 1..=2 {
        let id = format!("resume-cap-filler-{n}");
        h.h.app
            .db
            .insert_bridge_child(&id, &owner, &format!("resume-cap-key-{n}"))
            .unwrap();
        h.h.app
            .db
            .set_bridge_child_state(&id, crate::session::ChildState::Ready)
            .unwrap();
    }

    queue_request(
        &h.h,
        0,
        "resume-0105",
        &envelope(
            "resume-0105",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    let answer = h.answer("resume-0105").await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "fanout_exhausted", "{answer}");
    assert_eq!(
        h.h.app
            .db
            .bridge_child_state(&child.child_id)
            .unwrap()
            .map(|row| row.state),
        Some(crate::session::ChildState::Stopped),
        "a refused resume consumed the parked state"
    );
    assert_eq!(
        h.h.app.child_lifecycle.in_flight(),
        0,
        "a refused resume still started a relaunch"
    );
    assert!(
        h.h.app
            .db
            .child_saga(&owner, "resume-0105")
            .unwrap()
            .is_none(),
        "a capacity refusal recorded a relaunch saga"
    );
}

/// Make the owner's **aggregate** child-state read fail while every single-row
/// read still works.
///
/// `bridge_child_states_of` maps `updated_at` as an integer and SQLite is
/// dynamically typed, so one sibling row holding text is a genuine
/// deserialization failure over the set — and over nothing else. Dropping the
/// table would fail the ownership and state reads a request makes first, and
/// the refusal would then prove nothing about the capacity check.
fn break_the_live_child_count(h: &ChildHarness, owner: &str) {
    h.h.app
        .db
        .insert_bridge_child("unreadable-sibling", owner, "unreadable-sibling-key")
        .unwrap();
    h.h.app
        .db
        .set_bridge_child_state("unreadable-sibling", crate::session::ChildState::Ready)
        .unwrap();
    h.h.app
        .db
        .conn_ref()
        .execute(
            "UPDATE bridge_child_state SET updated_at = 'not-a-number' \
             WHERE child_id = 'unreadable-sibling'",
            [],
        )
        .unwrap();
    assert!(
        h.h.app.db.live_bridge_children(owner).is_err(),
        "the injection did not actually break the count"
    );
}

/// A launch another broker is still running counts against the cap here.
///
/// The bridge lease moves: it is renewed on a cadence and taken over when it
/// lapses, so an instance can pick up an owner's queue while a peer is part-way
/// through a `create` for the same owner. Capacity used to be the durable
/// children **plus this process's own in-flight jobs**, and the second half is
/// invisible across a handover — the new broker would see only the committed
/// children and admit its peer's launches all over again, one extra child per
/// launch in flight.
///
/// The claim is durable the whole time: `begin_create` writes the saga row
/// before it pushes the job, and S6 writes the child's state row and the
/// `committed` step together — so a pre-commit saga with no state row is
/// exactly "a slot claimed, no child yet", from any process.
///
/// Written as the row a peer would have left, because two real brokers are two
/// processes against one file and this harness is one.
#[tokio::test]
async fn a_peers_uncommitted_launch_still_holds_a_slot() {
    let mut h = ChildHarness::new();
    let owner = h.owner().to_string();
    // The profile allows two. One committed child, plus one launch a peer has
    // accepted and not committed, is the cap.
    let child = h.ready_child("create-0150").await;
    assert_eq!(h.h.app.db.live_bridge_children(&owner).unwrap(), 1);

    let peer_child = SessionId::default().to_string();
    h.h.app
        .db
        .upsert_child_saga(&crate::session::ChildSaga {
            owner_id: owner.clone(),
            key: "peer-create-0150".to_string(),
            child_id: Some(peer_child.clone()),
            step: Some(crate::session::SagaStep::Pane),
            ..crate::session::ChildSaga::default()
        })
        .unwrap();
    assert_eq!(
        h.h.app.db.pending_child_slot_claims(&owner).unwrap(),
        1,
        "a pre-commit saga with no child state is a slot claimed"
    );
    // The number the cap is actually read from, and it comes out of **one**
    // statement. The two halves are disjoint at any single instant, which is
    // exactly why reading them at two instants loses a child: a peer that
    // commits in between moves its child from the pending side to the live
    // side, and a count taken before that on one side and after it on the other
    // sees it on neither.
    assert_eq!(
        h.h.app.db.reserved_child_slots(&owner).unwrap(),
        2,
        "a live child and a peer's uncommitted launch are two slots"
    );

    // A branch of its own, so a refusal can only be about capacity: two creates
    // resolving to one worktree directory are refused for that instead.
    h.create_on("create-0151", "feat/two");
    let answer = h.answer("create-0151").await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "fanout_exhausted", "{answer}");
    assert!(
        h.h.app
            .db
            .bridge_children_of(&owner)
            .unwrap()
            .iter()
            .all(|row| row.child_id == child.child_id),
        "a create past the cap still made a child"
    );

    // And the claim is released the way a real one is — the peer's launch
    // reaching a final step — rather than by anything this instance does.
    h.h.app
        .db
        .upsert_child_saga(&crate::session::ChildSaga {
            owner_id: owner.clone(),
            key: "peer-create-0150".to_string(),
            child_id: Some(peer_child),
            step: Some(crate::session::SagaStep::Failed),
            ..crate::session::ChildSaga::default()
        })
        .unwrap();
    assert_eq!(h.h.app.db.pending_child_slot_claims(&owner).unwrap(), 0);
    assert_eq!(
        h.h.app.db.reserved_child_slots(&owner).unwrap(),
        1,
        "the released claim leaves only the live child"
    );
    h.create_on("create-0152", "feat/three");
    h.drive_until(60, |h| {
        h.h.app
            .db
            .bridge_children_of(&owner)
            .map(|rows| rows.len() >= 2)
            .unwrap_or(false)
    })
    .await;
    assert_eq!(
        h.h.app.db.bridge_children_of(&owner).unwrap().len(),
        2,
        "the slot the peer's launch held was never usable again: {:?}",
        bridge_answer(&h.h, 0, "create-0152")
    );
}

/// Capacity fails **closed** on a `create`.
///
/// `reserved_child_slots` read an unreadable child count as zero, so the one
/// moment friring could not see the children an owner already has was the moment
/// it would authorize a whole `max_children` worth more of them. The refusal is
/// `failed` and not `fanout_exhausted`: the cap was not reached, it could not be
/// evaluated, and a leader retrying on "not now" would retry forever.
#[tokio::test]
async fn a_create_is_refused_when_the_live_child_count_cannot_be_read() {
    let mut h = ChildHarness::new();
    let owner = h.owner().to_string();
    break_the_live_child_count(&h, &owner);

    h.create("create-0130");
    let answer = h.answer("create-0130").await;

    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "failed", "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .is_some_and(|m| m.contains("how many children")),
        "{answer}"
    );
    assert_eq!(
        h.h.app.child_lifecycle.in_flight(),
        0,
        "a create friring could not authorize still started a saga"
    );
    assert!(
        h.h.app
            .db
            .bridge_children_of(&owner)
            .unwrap()
            .iter()
            .all(|row| row.child_id == "unreadable-sibling"),
        "a create friring could not authorize still made a child"
    );
}

/// The same rule on the other side of the parking cycle, and the parked child is
/// left exactly as it was.
///
/// A `resume` of a released child is a fresh capacity claim, so it has the same
/// uncertainty to fail closed on — and the refusal must not spend the thing it
/// refuses: the child stays `stopped`, with no relaunch saga and no runtime.
#[tokio::test]
async fn a_released_child_resume_is_refused_when_the_live_child_count_cannot_be_read() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0131").await;
    let owner = h.owner().to_string();
    queue_request(
        &h.h,
        0,
        "stop-0131",
        &envelope(
            "stop-0131",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    let stopped = h.answer("stop-0131").await;
    assert_eq!(stopped["data"]["state"], "stopped", "{stopped}");

    break_the_live_child_count(&h, &owner);
    queue_request(
        &h.h,
        0,
        "resume-0131",
        &envelope(
            "resume-0131",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    let answer = h.answer("resume-0131").await;

    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "failed", "{answer}");
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Stopped),
        "a refused resume consumed the parked state"
    );
    assert_eq!(
        h.h.app.child_lifecycle.in_flight(),
        0,
        "a refused resume still started a relaunch"
    );
    assert!(
        h.h.app
            .db
            .child_saga(&owner, "resume-0131")
            .unwrap()
            .is_none(),
        "a refusal friring could not authorize recorded a relaunch saga"
    );
    assert!(
        !h.h.app
            .sessions
            .iter()
            .any(|session| session.info.id.to_string() == child.child_id),
        "a refused resume started the child's runtime"
    );
}

/// Two creates that resolve to one worktree directory make one child, and the
/// second is refused.
///
/// `worktree_segments` maps `/` to `-`, so `feat/one` and `feat-one` are two
/// git-legal branch names for one directory. The broker takes both in a single
/// pass and S2 cuts the directory off the tick, so at the moment the second is
/// validated the path still does not exist — only in-flight accounting can tell
/// them apart, and without it both children would share one writable workspace.
#[tokio::test]
async fn two_creates_resolving_to_one_worktree_make_one_child() {
    let mut h = ChildHarness::new();
    let keys = ["create-0031", "create-0032"];
    for (key, branch) in keys.iter().zip(["feat/one", "feat-one"]) {
        queue_request(
            &h.h,
            0,
            key,
            &envelope(
                key,
                "create",
                serde_json::json!({
                    "repo_root": "/repo/app",
                    "branch": branch,
                    "agent": "worker",
                    "task_kind": "task",
                    "task_body": "do the thing",
                }),
            ),
        );
    }
    // `read_dir` order is unspecified, so which of the two wins is not asserted
    // — only that exactly one does. The other is answered immediately, because a
    // refusal is not deferred the way an accepted create is.
    h.drive_until(60, |h| {
        keys.iter().any(|key| bridge_answer(&h.h, 0, key).is_some())
    })
    .await;
    let refusals: Vec<serde_json::Value> = keys
        .iter()
        .filter_map(|key| bridge_answer(&h.h, 0, key))
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "exactly one create is answered: {refusals:?}"
    );
    assert_eq!(refusals[0]["ok"], false, "{}", refusals[0]);
    // Named specifically: the profile allows two children, so a fan-out refusal
    // or the pre-existing-worktree refusal would be a different bug passing this
    // test.
    assert!(
        refusals[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("already in flight"),
        "{}",
        refusals[0]
    );

    // The survivor really becomes a child, and it is the only one.
    for _ in 0..60 {
        h.h.tick();
        h.h.app.tick_child_sagas();
        h.child_hook_reports();
        if h.child_state() == Some(crate::session::ChildState::Ready) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Ready),
        "the accepted create did not reach a ready child"
    );
    assert_eq!(
        h.h.app
            .db
            .bridge_children_of(&h.owner().to_string())
            .unwrap()
            .len(),
        1,
        "two creates on one worktree made two children"
    );
}

/// A child that is running is never resumable: relaunching one would kill work
/// in progress.
#[tokio::test]
async fn a_running_child_may_not_be_resumed() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0023").await;
    queue_request(
        &h.h,
        0,
        "resume-0002",
        &envelope(
            "resume-0002",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    let answer = h.answer("resume-0002").await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not resumable"),
        "{answer}"
    );
}

/// A claimed branch is not a claimed path, and the reclaim asks git which it is.
///
/// `worktree_segments` maps `/` to `-`, so `feat/one` and `feat-one` resolve to
/// one directory. Two creates issued in the same tick both pass validation (the
/// path does not exist yet), both win their own distinct ref, and only one wins
/// `git worktree add` — but the loser's failure reports `owns_branch: true`,
/// which is honest about the ref and says nothing about the directory. Trusting
/// it alone, the loser's unwind force-removes the winner's freshly created —
/// therefore clean — worktree. So the directory's ownership is asked of git, and
/// an answer of anything but "this launch's branch" leaves it alone.
///
/// The branch is still reclaimed: it really was this attempt's, and leaking one
/// ref per collision is the thing the unwind is for.
#[tokio::test]
async fn a_reclaim_leaves_a_worktree_that_git_says_is_another_launchs() {
    for (n, is_on, why) in [
        (0u8, Some(false), "another launch's worktree"),
        (1, None, "a directory git would not name"),
    ] {
        let mut h = ChildHarness::new();
        let owner = h.owner().to_string();
        let worktree = tempfile::tempdir().unwrap();
        let key = format!("create-005{n}");
        *h.effects.worktree_is_on.lock().unwrap() = is_on;
        *h.effects.ahead.lock().unwrap() = Some(0);
        h.h.app
            .db
            .take_bridge_request(&owner, &key, "create", "hash", None)
            .unwrap();
        h.h.app
            .db
            .upsert_child_saga(&crate::session::ChildSaga {
                owner_id: owner.clone(),
                key: key.clone(),
                child_id: Some(SessionId::default().to_string()),
                step: Some(crate::session::SagaStep::Worktree),
                worktree_path: Some(worktree.path().display().to_string()),
                // The ref really was this attempt's — that is exactly the case
                // that used to be treated as licence over the directory.
                branch_claimed: true,
                branch: Some("feat-one".into()),
                base_head: None,
                instance_id: Some("a-dead-instance".into()),
                lease_until: Some(1),
                ..Default::default()
            })
            .unwrap();

        h.h.app.tick_child_sagas();

        assert!(
            !h.effects.called("remove_worktree"),
            "{why} was removed by a saga that only owned the branch"
        );
        assert!(
            worktree.path().exists(),
            "{why} was destroyed on disk by another launch's unwind"
        );
        assert!(
            h.effects.called("delete_branch"),
            "the branch this attempt really did cut must still be reclaimed ({why})"
        );
        let banner = h.h.app.status_message.as_ref().expect("an operator banner");
        assert!(
            banner.text.contains(&worktree.path().display().to_string()),
            "the operator must be told which directory was left alone ({why}): {}",
            banner.text
        );
    }
}

/// Recovery reconciles a saga a previous run left below the committed line: the
/// worktree it recorded is reclaimed and the request is failed.
#[tokio::test]
async fn recovery_reclaims_an_interrupted_launch_by_its_recorded_identity() {
    let mut h = ChildHarness::new();
    let owner = h.owner().to_string();
    let worktree = tempfile::tempdir().unwrap();
    // What a previous run would have left: a saga past S2, with a lease that
    // has expired and an instance nobody is.
    h.h.app
        .db
        .take_bridge_request(&owner, "create-0015", "create", "hash", None)
        .unwrap();
    h.h.app
        .db
        .upsert_child_saga(&crate::session::ChildSaga {
            owner_id: owner.clone(),
            key: "create-0015".into(),
            child_id: Some(SessionId::default().to_string()),
            step: Some(crate::session::SagaStep::Worktree),
            worktree_path: Some(worktree.path().display().to_string()),
            // A saga past S2 recorded that its own `git branch` created the
            // branch; a reclaim acts on nothing it cannot prove it made.
            branch_claimed: true,
            branch: Some("feat/interrupted".into()),
            base_head: Some("abc1234".into()),
            instance_id: Some("a-dead-instance".into()),
            lease_until: Some(1),
            ..Default::default()
        })
        .unwrap();
    *h.effects.ahead.lock().unwrap() = Some(0);

    h.h.app.tick_child_sagas();

    assert!(
        h.effects.called("remove_worktree"),
        "the recorded worktree is reclaimed"
    );
    assert!(
        h.effects.called("delete_branch"),
        "an empty branch is deleted"
    );
    let saga =
        h.h.app
            .db
            .child_saga(&owner, "create-0015")
            .unwrap()
            .unwrap();
    assert_eq!(saga.step, Some(crate::session::SagaStep::Failed));
    let answer = bridge_answer(&h.h, 0, "create-0015").expect("the request is answered");
    assert_eq!(answer["ok"], false, "{answer}");
}

/// A finish intent held behind a launch survives the process that held it.
///
/// The agent starts at S8, so a small worker can report `completed` before S9
/// proves the child ready. The broker answers that `send` `ok` and journals the
/// answer, which a replay returns verbatim — so the intent reaches
/// `accept_finish_intent` exactly once, and the launch it lands in has not
/// written `finishing` yet. Kept only in the job, a crash between S8 and S9
/// loses it: recovery's `Finishing` sweep does not see this child, and the saga
/// sweep adopts it with no verdict, holding its owner's fan-out slot until
/// somebody stops it by hand. So it is written to the saga row instead, and
/// recovery carries it out.
#[tokio::test]
async fn a_held_finish_intent_is_carried_out_after_a_restart() {
    let mut h = ChildHarness::new();
    let owner = h.owner().to_string();
    h.create("create-0092");
    h.drive_until(30, |h| h.child().is_some()).await;
    let child = h.child().expect("the launch committed a child");
    let id: SessionId = child.child_id.parse().unwrap();

    // What the broker does the moment the `result` mail is durable, while the
    // launch is still running.
    h.h.app
        .accept_finish_intent(id, crate::session::Outcome::Completed, Some(13));

    let saga =
        h.h.app
            .db
            .child_saga(&owner, "create-0092")
            .unwrap()
            .expect("the launch has a saga row");
    assert_eq!(
        saga.finish_outcome.as_deref(),
        Some("completed"),
        "the held intent was never written down, so a crash here loses it"
    );
    assert_eq!(saga.finish_message_id, Some(13), "the mail row was lost");

    // The crash: every job this process was carrying is gone, and the saga row
    // is what a new instance starts from. Left at the committed line, so
    // recovery adopts rather than reconciles.
    h.h.app.child_lifecycle.jobs.clear();
    h.h.app
        .db
        .upsert_child_saga(&crate::session::ChildSaga {
            step: Some(crate::session::SagaStep::Released),
            instance_id: Some("a-dead-instance".into()),
            lease_until: Some(1),
            ..saga
        })
        .unwrap();

    h.h.app.recover_child_sagas();
    h.drive_reporting(120, |h| {
        h.h.app.child_lifecycle.in_flight() == 0
            && h.child_state()
                .is_some_and(crate::session::ChildState::is_terminal)
    })
    .await;

    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Done),
        "a child that had already reported was adopted with no verdict"
    );
    let verdict =
        h.h.app
            .db
            .bridge_result(&child.child_id)
            .unwrap()
            .expect("the recovered intent must still produce a host verdict");
    assert_eq!(verdict.outcome, crate::session::Outcome::Completed);
    assert_eq!(
        verdict.message_id,
        Some(13),
        "the recovered verdict lost the mail row the intent arrived on"
    );
}

/// Recovery is a rule about **every** step below the committed line, not only
/// the one a fixture happened to pick. At `Pane` a gated pane was recorded and
/// has to be killed by its exact identity; at `Committed` and `Released` the
/// child is a real session and nothing may be removed at all.
#[tokio::test]
async fn recovery_reconciles_each_step_by_what_it_positively_names() {
    for (n, step, removes) in [
        (0, crate::session::SagaStep::Dirs, true),
        (1, crate::session::SagaStep::Pane, true),
        (2, crate::session::SagaStep::Committed, false),
        (3, crate::session::SagaStep::Released, false),
    ] {
        let backend = Arc::new(FakeBackend::spawnable());
        let mut h = ChildHarness::with_watched_backend(Arc::clone(&backend));
        let owner = h.owner().to_string();
        let worktree = tempfile::tempdir().unwrap();
        let key = format!("create-006{n}");
        let child = SessionId::default().to_string();
        h.h.app
            .db
            .take_bridge_request(&owner, &key, "create", "hash", None)
            .unwrap();
        h.h.app
            .db
            .upsert_child_saga(&crate::session::ChildSaga {
                owner_id: owner.clone(),
                key: key.clone(),
                child_id: Some(child.clone()),
                step: Some(step),
                worktree_path: Some(worktree.path().display().to_string()),
                // A saga past S2 recorded that its own `git branch` created the
                // branch; a reclaim acts on nothing it cannot prove it made.
                branch_claimed: true,
                branch: Some("feat/interrupted".into()),
                base_head: Some("abc1234".into()),
                // A recorded pane, marker included — without which
                // `MuxIdentity::is_recorded` is false and the kill is skipped.
                mux_server: Some("server-1".into()),
                mux_window_id: Some("@7".into()),
                mux_pane_id: Some("%12".into()),
                mux_pane_pid: Some(4242),
                mux_launch_key: Some(crate::session::MuxIdentity::launch_marker(&child, "k")),
                instance_id: Some("a-dead-instance".into()),
                lease_until: Some(1),
                ..Default::default()
            })
            .unwrap();
        // A live pane matching the recorded identity exactly, plus a decoy that
        // matches nothing: the kill has to be aimed by identity, not by "the
        // pane that happens to be there". Without a pane to find,
        // `kill_recorded_pane` takes its already-gone branch and a kill that
        // never happened would look exactly like a kill that did.
        if step == crate::session::SagaStep::Pane {
            let mut panes = backend.panes.lock().unwrap();
            panes.insert(
                "%12".into(),
                crate::session::MuxIdentity {
                    server: Some("server-1".into()),
                    window_id: Some("@7".into()),
                    pane_id: Some("%12".into()),
                    pane_pid: Some(4242),
                    launch_key: Some(crate::session::MuxIdentity::launch_marker(&child, "k")),
                },
            );
            panes.insert(
                "%99".into(),
                crate::session::MuxIdentity {
                    server: Some("server-1".into()),
                    window_id: Some("@9".into()),
                    pane_id: Some("%99".into()),
                    pane_pid: Some(9999),
                    launch_key: Some(crate::session::MuxIdentity::launch_marker(
                        &SessionId::default().to_string(),
                        "other",
                    )),
                },
            );
        }
        // Past the committed line the child is a real session, so its rows exist.
        if !removes {
            h.h.app
                .db
                .insert_bridge_child(&child, &owner, &key)
                .unwrap();
            h.h.app
                .db
                .set_bridge_child_state(&child, crate::session::ChildState::Starting)
                .unwrap();
        }
        *h.effects.ahead.lock().unwrap() = Some(0);

        h.h.app.tick_child_sagas();

        assert_eq!(
            h.effects.called("remove_worktree"),
            removes,
            "{step:?}: reconciliation removed the wrong thing"
        );
        if step == crate::session::SagaStep::Pane {
            let panes = backend.panes.lock().unwrap();
            assert!(
                !panes.contains_key("%12"),
                "the pane the saga recorded survived reconciliation"
            );
            assert!(
                panes.contains_key("%99"),
                "reconciliation killed a pane the saga never named"
            );
        }
        if removes {
            let answer = bridge_answer(&h.h, 0, &key)
                .unwrap_or_else(|| panic!("{step:?}: the request was never answered"));
            assert_eq!(answer["ok"], false, "{step:?}: {answer}");
        } else {
            // A committed child is adopted, not unwound: its ownership row, its
            // worktree and its mailbox are all real.
            assert!(
                h.h.app.db.bridge_child(&child).unwrap().is_some(),
                "{step:?}: a committed child lost its ownership row"
            );
            // …and adoption *ran*, rather than leaving the seeded row alone.
            // This child is not in the session list, so it is not running: the
            // one resumable outcome, said in all three places it is said.
            assert_eq!(
                h.h.app
                    .db
                    .bridge_child_state(&child)
                    .unwrap()
                    .map(|row| row.state),
                Some(crate::session::ChildState::Stalled),
                "{step:?}: an unrunnable adopted child was not marked resumable"
            );
            let saga =
                h.h.app
                    .db
                    .child_saga(&owner, &key)
                    .unwrap()
                    .unwrap_or_else(|| panic!("{step:?}: the saga row is gone"));
            assert_eq!(
                saga.step,
                Some(crate::session::SagaStep::Failed),
                "{step:?}: the interrupted saga was left open"
            );
            let owner_mail =
                h.h.app
                    .db
                    .list_messages(h.owner(), false, Some(50))
                    .unwrap();
            assert_eq!(
                owner_mail
                    .iter()
                    .filter(|m| m.kind == crate::session::bridge::MailKind::ChildStalled.as_str())
                    .count(),
                1,
                "{step:?}: the owner was never told its child stalled: {owner_mail:?}"
            );
        }
    }
}

/// `stop` is a public verb with a grace period, a cancel, a deferred answer and
/// terminal-state idempotency, and none of it was covered: the only route to
/// `Stopped` any test took was the delete cascade, which enters the quiesce
/// already acknowledged.
#[tokio::test]
async fn the_stop_verb_cancels_waits_and_answers_once() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0070").await;

    queue_request(
        &h.h,
        0,
        "stop-0070",
        &envelope(
            "stop-0070",
            "stop",
            // A real grace, so the wait is the thing being observed: friring
            // asks the child to finish and gives it this long to answer before
            // stopping it anyway.
            serde_json::json!({ "child": child.child_id, "grace_secs": 120 }),
        ),
    );
    h.drive(3).await;

    // Accepted, not answered: a stop takes as long as the child's own finish
    // intent, so the journal row waits.
    assert!(
        bridge_answer(&h.h, 0, "stop-0070").is_none(),
        "a stop was answered before the child stopped: {:?}",
        bridge_answer(&h.h, 0, "stop-0070")
    );
    // Still running during the grace, and deliberately: the point of the window
    // is that the child may finish on its own terms. What is durable meanwhile
    // is the journal row, which recovery reads if this instance dies holding it.
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Ready));
    assert_eq!(
        h.h.app
            .db
            .bridge_request(&h.owner().to_string(), "stop-0070")
            .unwrap()
            .map(|row| row.state),
        Some(crate::storage::bridge::RequestState::Accepted)
    );
    // The child was told, in friring's own words, before anything was killed.
    let mail =
        h.h.app
            .db
            .list_messages(child.child_id.parse().unwrap(), false, Some(20))
            .unwrap();
    assert!(
        mail.iter().any(|m| m.kind == "cancel"),
        "the child was stopped without being asked to finish: {mail:?}"
    );

    // The child answers, and the quiesce runs to a verdict.
    h.h.app.accept_finish_intent(
        child.child_id.parse().unwrap(),
        crate::session::Outcome::Completed,
        None,
    );
    h.drive_until(60, |h| h.h.app.child_lifecycle.in_flight() == 0)
        .await;

    let answer = bridge_answer(&h.h, 0, "stop-0070").expect("the stop is answered when it is done");
    assert_eq!(answer["ok"], true, "{answer}");
    // `stopped`, not `done`, even though the child reported `completed`: an
    // owner-initiated stop is a stop. `done` is reachable only from a finish the
    // child started.
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Stopped));

    // A replay of the same key returns the same answer and stops nothing twice.
    queue_request(
        &h.h,
        0,
        "stop-0070",
        &envelope(
            "stop-0070",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 120 }),
        ),
    );
    h.drive(3).await;
    let replay = bridge_answer(&h.h, 0, "stop-0070").expect("a replay is answered");
    assert_eq!(replay, answer, "a replay did not return the first answer");
}

/// A **pre-effect** saga write that fails stops the launch before the effect.
///
/// The module header's promise is that every external effect is written down
/// first, so recovery has an exact identity to reconcile. A step that recorded
/// nothing and made the effect anyway would leave a worktree or a gated pane
/// that recovery cannot name and will not touch — the one failure the whole
/// design is arranged around, and the one nothing injected until now.
///
/// Injected for real rather than through a seam: a `BEFORE INSERT` trigger makes
/// `upsert_child_saga` fail exactly the way a broken database does, and S2's
/// pre-effect record is the first write after the job starts.
#[tokio::test]
async fn a_step_that_cannot_be_recorded_is_never_carried_out() {
    let mut h = ChildHarness::new();
    h.h.app
        .db
        .conn_ref()
        .execute(
            // BEFORE **UPDATE**, so S1's insert lands and S2's pre-effect
            // record — the first write of an existing row — is the one that
            // fails. An insert trigger would refuse at S1 and never reach the
            // step whose ordering is the property under test.
            "CREATE TRIGGER no_saga_step BEFORE UPDATE ON child_sagas \
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
            [],
        )
        .unwrap();

    h.create("create-0085");
    let answer = h.answer("create-0085").await;

    assert_eq!(answer["ok"], false, "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .unwrap_or_default()
            .contains("will not carry out what it cannot recover"),
        "{answer}"
    );
    // The effect never ran, which is the property: a worktree friring could not
    // write down is a worktree it must not make.
    assert!(
        !h.effects.called("create_worktree"),
        "the launch cut a worktree it had failed to record"
    );
    assert!(h.child().is_none(), "nothing was committed");
}

/// A launch that lost the branch reclaims **nothing**, and one that won it
/// reclaims what it made.
///
/// The planned worktree path is recorded before `git` runs, so a saga that lost
/// the ref to another instance is holding the *winner's* directory against its
/// own failure. Removing it is the one mistake here that destroys work nobody
/// can recover, and telling the two apart is not something the planned path can
/// do — it is what the two-phase claim reports.
#[tokio::test]
async fn only_a_launch_that_claimed_the_branch_reclaims_anything() {
    for (owns, expect_reclaim) in [(false, false), (true, true)] {
        let mut h = ChildHarness::new();
        *h.effects.worktree_error.lock().unwrap() = Some(crate::git::ClaimFailure {
            detail: "fatal: a branch named 'feat/one' already exists".into(),
            owns_branch: owns,
        });
        h.create("create-0083");
        let answer = h.answer("create-0083").await;
        assert_eq!(answer["ok"], false, "{answer}");

        let reclaimed = !h.effects.deleted_branches.lock().unwrap().is_empty();
        assert_eq!(
            reclaimed,
            expect_reclaim,
            "a launch that {} the branch {} reclaim",
            if owns { "won" } else { "lost" },
            if reclaimed { "did" } else { "did not" }
        );
        // The saga records which it was, because a *later* process's recovery
        // has only the row to decide from.
        let saga =
            h.h.app
                .db
                .child_saga(&h.owner().to_string(), "create-0083")
                .unwrap()
                .expect("the failed launch left its saga row");
        assert_eq!(saga.branch_claimed, owns);
    }
}

/// Recovery declines a worktree it cannot prove the interrupted saga made.
///
/// The narrow window the flag cannot cover is a crash *during* `git worktree
/// add`, after the branch was claimed and before the tick recorded it. A leaked
/// directory can be removed by hand; a wrongly deleted one cannot be brought
/// back, so recovery leaves it and says where it is.
#[tokio::test]
async fn recovery_leaves_a_worktree_it_cannot_prove_belongs_to_the_saga() {
    let mut h = ChildHarness::new();
    let dir = tempfile::tempdir().unwrap();
    let worktree = dir.path().join("someone-elses");
    std::fs::create_dir_all(&worktree).unwrap();
    let child = SessionId::default();
    h.h.app
        .db
        .upsert_child_saga(&crate::session::ChildSaga {
            owner_id: h.owner().to_string(),
            key: "create-0084".to_string(),
            child_id: Some(child.to_string()),
            step: Some(crate::session::SagaStep::Worktree),
            worktree_path: Some(worktree.display().to_string()),
            branch: Some("feat/interrupted".to_string()),
            // The whole point: unproven, so unreclaimable.
            branch_claimed: false,
            ..crate::session::ChildSaga::default()
        })
        .unwrap();

    h.h.app.recover_child_sagas();

    assert!(
        worktree.exists(),
        "recovery removed a worktree it could not prove was the saga's"
    );
    assert!(
        h.effects.deleted_branches.lock().unwrap().is_empty(),
        "recovery deleted a branch it could not prove was the saga's"
    );
    assert!(
        h.effects.removed_worktrees.lock().unwrap().is_empty(),
        "recovery removed a worktree it could not prove was the saga's"
    );
}

/// A `stop` that lands on a running launch is answered by the **stop**, not by
/// the create it was coalesced onto.
///
/// The waiter list attaches a key to whatever job holds the child, and every
/// waiter is answered from that job's own response. For a `stop` on a launch
/// that is `ok` with `state: "ready"` — the caller is told quiescence completed
/// while nothing was stopped, nothing was verified, and no verdict was written.
#[tokio::test]
async fn a_stop_during_a_create_stops_the_child_rather_than_reporting_it_ready() {
    let mut h = ChildHarness::new();
    h.create("create-0080");
    // Far enough in that the child exists and S9 has not been satisfied: no hook
    // has reported, so the launch is still waiting for its readiness proof.
    h.drive_until(30, |h| h.child().is_some()).await;
    let child = h.child().expect("the launch committed a child");
    assert_ne!(
        h.child_state(),
        Some(crate::session::ChildState::Ready),
        "this test needs a launch that has not finished"
    );

    queue_request(
        &h.h,
        0,
        "stop-0080",
        &envelope(
            "stop-0080",
            "stop",
            // No grace: this test is about which answer the caller gets, not
            // about the window, and waiting one out would be a wall-clock sleep.
            serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
        ),
    );
    h.drive(2).await;
    // Nothing yet: the launch is still running and the stop is held behind it.
    assert!(bridge_answer(&h.h, 0, "stop-0080").is_none());

    // The launch finishes normally, and the held stop then runs as a real
    // quiesce of its own.
    h.drive_reporting(90, |h| bridge_answer(&h.h, 0, "stop-0080").is_some())
        .await;

    let create = bridge_answer(&h.h, 0, "create-0080").expect("the create is answered");
    let stop = bridge_answer(&h.h, 0, "stop-0080").expect("the stop is answered");
    assert_eq!(create["data"]["state"], "ready", "{create}");
    // The two answers are about different things, and the stop's is about
    // stopping. Sharing the create's `ok` is the exact defect — but so is a
    // refusal, whose `data` is absent and would satisfy "not the create's".
    assert_eq!(stop["ok"], true, "the stop was refused: {stop}");
    assert_eq!(stop["data"]["child"], child.child_id, "{stop}");
    assert_eq!(
        stop["data"]["state"], "stopped",
        "the stop was answered with the create's own result: {stop}"
    );
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Stopped),
        "the child was reported stopped without being stopped"
    );
    // And friring really did look at the worktree afterwards, which is what the
    // create's answer would have claimed without doing.
    assert!(
        h.effects.called("verify_worktree"),
        "nothing verified the worktree of a child a caller was told was stopped"
    );
}

/// A `result` that arrives **before** the readiness proof is kept, and quiesced
/// once the launch ends.
///
/// The agent starts when the gate opens at S8, one step before S9, so a worker
/// small enough to finish inside that window is ordinary rather than exotic. Its
/// `send` is already answered `ok` with a `message_id`, so dropping the intent
/// strands the child `ready` for good: no ack, no quiesce, no verdict, and a
/// fan-out slot held for ever.
#[tokio::test]
async fn a_result_that_beats_the_readiness_proof_is_quiesced_and_not_dropped() {
    let mut h = ChildHarness::new();
    h.create("create-0091");
    h.drive_until(30, |h| h.child().is_some()).await;
    let child = h.child().expect("the launch committed a child");
    let id: SessionId = child.child_id.parse().unwrap();
    assert_ne!(
        h.child_state(),
        Some(crate::session::ChildState::Ready),
        "this test needs a child that has not yet proved ready"
    );

    // What the broker does the moment the `result` mail is durable.
    h.h.app
        .accept_finish_intent(id, crate::session::Outcome::Completed, Some(7));

    // The launch runs to its own end, and the held intent becomes the quiesce.
    h.drive_reporting(120, |h| {
        h.h.app.child_lifecycle.in_flight() == 0
            && h.child_state()
                .is_some_and(crate::session::ChildState::is_terminal)
    })
    .await;

    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Done),
        "a child that finished early never reached a terminal state"
    );
    let verdict =
        h.h.app
            .db
            .bridge_result(&child.child_id)
            .unwrap()
            .expect("a child that reported a result has a host verdict");
    assert_eq!(verdict.outcome, crate::session::Outcome::Completed);
    assert_eq!(
        verdict.message_id,
        Some(7),
        "the verdict lost the mail row the intent arrived on"
    );
    // And the child was told its intent was taken, which is the whole reason the
    // host sends an `ack` at all.
    let mail = h.h.app.db.list_messages(id, false, Some(20)).unwrap();
    assert!(
        mail.iter().any(|m| m.kind == "ack"),
        "the child was never acknowledged: {mail:?}"
    );
}

/// When both a `result` and a `stop` are held behind one launch, the one that
/// arrived first decides — as it would have on the live path.
///
/// Live, the pair resolves by arrival order: a `result` creates the quiesce and
/// a later `stop` joins it as a waiter, so the child's own `completed` is what
/// both callers are answered from; a `stop` first wins over a later result,
/// deliberately. A held pair carries no order unless it is recorded, and without
/// it the deferred path always behaved as if the stop came first — turning a
/// child that really did finish its work into `failed`/`stopped`, and losing the
/// verdict an integration step reads.
///
/// Both orders are exercised, because a rule that only ever produces one answer
/// is not a rule about order.
#[tokio::test]
async fn a_held_result_and_a_held_stop_are_resolved_by_which_arrived_first() {
    for (n, result_first, want_state, want_outcome) in [
        (
            0u8,
            true,
            crate::session::ChildState::Done,
            crate::session::Outcome::Completed,
        ),
        (
            1,
            false,
            crate::session::ChildState::Stopped,
            crate::session::Outcome::Failed,
        ),
    ] {
        let mut h = ChildHarness::new();
        let create = format!("create-009{n}");
        let stop = format!("stop-009{n}");
        h.create(&create);
        h.drive_until(30, |h| h.child().is_some()).await;
        let child = h.child().expect("the launch committed a child");
        let id: SessionId = child.child_id.parse().unwrap();
        assert_ne!(
            h.child_state(),
            Some(crate::session::ChildState::Ready),
            "this case needs a launch that has not finished"
        );

        // No grace: this is about which answer wins, not about the window.
        let queue_stop = |h: &ChildHarness| {
            queue_request(
                &h.h,
                0,
                &stop,
                &envelope(
                    &stop,
                    "stop",
                    serde_json::json!({ "child": child.child_id, "grace_secs": 0 }),
                ),
            );
        };
        if result_first {
            // Nothing is holding a stop yet, so this intent is unambiguously
            // first — no waiting needed to establish it.
            h.h.app
                .accept_finish_intent(id, crate::session::Outcome::Completed, Some(11));
            queue_stop(&h);
        } else {
            queue_stop(&h);
            // Driven until the broker has actually **taken** the stop, not for a
            // fixed number of passes: under load a fixed count leaves the stop
            // still in the queue, the intent lands first, and the case silently
            // becomes the other one.
            h.drive_until(60, |h| h.h.app.held_stop_keys_for_test(id) == 1)
                .await;
            assert_eq!(
                h.h.app.held_stop_keys_for_test(id),
                1,
                "case {n} needs the stop held before the result arrives"
            );
            h.h.app
                .accept_finish_intent(id, crate::session::Outcome::Completed, Some(11));
        }

        h.drive_reporting(120, |h| {
            h.h.app.child_lifecycle.in_flight() == 0
                && h.child_state()
                    .is_some_and(crate::session::ChildState::is_terminal)
        })
        .await;

        assert_eq!(
            h.child_state(),
            Some(want_state),
            "case {n}: the wrong arrival decided the child's state"
        );
        let verdict =
            h.h.app
                .db
                .bridge_result(&child.child_id)
                .unwrap()
                .expect("a quiesce that ran leaves a verdict");
        assert_eq!(
            verdict.outcome, want_outcome,
            "case {n}: the wrong arrival decided the verdict"
        );
        // Whichever won, the `stop` is still answered — it was journaled
        // `accepted`, and a replay of an accepted key waits rather than acting.
        let answer = bridge_answer(&h.h, 0, &stop).expect("the held stop is answered");
        assert_eq!(answer["ok"], true, "case {n}: {answer}");
        assert_eq!(
            answer["data"]["state"],
            want_state.as_str(),
            "case {n}: the stop's answer disagrees with the child's state: {answer}"
        );
    }
}

/// A quiesce that cannot record what it found refuses, and leaves the child
/// where a retry can pick it up.
///
/// The pane is already dead by then, so the alternative is a caller told `done`
/// over an empty `bridge_results` row — which an integration step reads as a
/// branch friring verified. `finishing` is live, so the slot is held, an
/// operator sees it, and `recover_child_sagas` runs the quiesce again next start.
#[tokio::test]
async fn a_quiesce_that_cannot_be_recorded_refuses_and_stays_recoverable() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0082").await;

    // Real failure injection rather than a seam: the verdict write is a plain
    // INSERT, and a BEFORE INSERT trigger makes it fail the way a broken
    // database does.
    h.h.app
        .db
        .conn_ref()
        .execute(
            "CREATE TRIGGER no_verdict BEFORE INSERT ON bridge_results \
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
            [],
        )
        .unwrap();

    queue_request(
        &h.h,
        0,
        "stop-0082",
        &envelope(
            "stop-0082",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 1 }),
        ),
    );
    h.h.app.accept_finish_intent(
        child.child_id.parse().unwrap(),
        crate::session::Outcome::Completed,
        None,
    );
    let answer = h.answer("stop-0082").await;

    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "quiesce_failed", "{answer}");
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Finishing),
        "a quiesce that recorded nothing left a terminal state behind"
    );
    assert!(
        h.h.app.db.bridge_result(&child.child_id).unwrap().is_none(),
        "a verdict was reported that was never written"
    );
    // Still live, so the fan-out slot is held rather than released for a child
    // nothing has a verdict for.
    assert!(h
        .child_state()
        .is_some_and(crate::session::ChildState::is_live));
}

/// The other half of that write: the verdict lands and the **state** write is
/// the one that fails.
///
/// The two are chained, not transactional, so this is a real partial write —
/// a `bridge_results` row for a child still marked `finishing`. It must refuse
/// exactly as the verdict failure does: nothing may be reported complete, and
/// the child stays live so its slot is held and the next start re-runs the
/// quiesce over the verdict it already wrote.
#[tokio::test]
async fn a_quiesce_whose_state_write_fails_refuses_and_keeps_the_verdict() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0083").await;

    queue_request(
        &h.h,
        0,
        "stop-0083",
        &envelope(
            "stop-0083",
            "stop",
            serde_json::json!({ "child": child.child_id, "grace_secs": 1 }),
        ),
    );
    h.h.app.accept_finish_intent(
        child.child_id.parse().unwrap(),
        crate::session::Outcome::Completed,
        None,
    );

    // Installed *after* the intent moved the child to `finishing`: the state
    // write is an upsert, so a trigger on the conflict branch would otherwise
    // fail that transition too and the quiesce would never get as far as the
    // verdict.
    h.h.app
        .db
        .conn_ref()
        .execute(
            "CREATE TRIGGER no_state BEFORE UPDATE ON bridge_child_state \
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
            [],
        )
        .unwrap();

    let answer = h.answer("stop-0083").await;

    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "quiesce_failed", "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .unwrap_or_default()
            .contains("the child's state"),
        "the refusal did not name the write that failed: {answer}"
    );
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Finishing),
        "a quiesce whose state write failed reported a state it never wrote"
    );
    assert!(
        h.child_state()
            .is_some_and(crate::session::ChildState::is_live),
        "the fan-out slot was released for a child with no recorded outcome"
    );
    assert!(
        h.h.app.db.bridge_result(&child.child_id).unwrap().is_some(),
        "the verdict that was written was lost with the state write that was not"
    );
}

/// A branch that carries commits is never deleted: a saga that failed is not
/// evidence that the work in it is worthless.
#[tokio::test]
async fn recovery_keeps_a_branch_that_carries_work() {
    let mut h = ChildHarness::new();
    let owner = h.owner().to_string();
    let worktree = tempfile::tempdir().unwrap();
    h.h.app
        .db
        .take_bridge_request(&owner, "create-0016", "create", "hash", None)
        .unwrap();
    h.h.app
        .db
        .upsert_child_saga(&crate::session::ChildSaga {
            owner_id: owner.clone(),
            key: "create-0016".into(),
            child_id: Some(SessionId::default().to_string()),
            step: Some(crate::session::SagaStep::Worktree),
            worktree_path: Some(worktree.path().display().to_string()),
            // A saga past S2 recorded that its own `git branch` created the
            // branch; a reclaim acts on nothing it cannot prove it made.
            branch_claimed: true,
            branch: Some("feat/has-work".into()),
            base_head: Some("abc1234".into()),
            instance_id: Some("a-dead-instance".into()),
            lease_until: Some(1),
            ..Default::default()
        })
        .unwrap();
    // Three commits nobody else has.
    *h.effects.ahead.lock().unwrap() = Some(3);

    h.h.app.tick_child_sagas();

    assert!(h.effects.called("remove_worktree"), "a clean worktree goes");
    assert!(
        !h.effects.called("delete_branch"),
        "a branch with commits stays"
    );
}

/// A dirty worktree an interrupted launch left is never removed, and the
/// operator is told rather than left to find out.
#[tokio::test]
async fn recovery_keeps_a_worktree_with_uncommitted_work() {
    let mut h = ChildHarness::new();
    let owner = h.owner().to_string();
    let worktree = tempfile::tempdir().unwrap();
    *h.effects.verdict.lock().unwrap() = crate::git::WorktreeVerdict {
        dirty: true,
        ..Default::default()
    };
    h.h.app
        .db
        .upsert_child_saga(&crate::session::ChildSaga {
            owner_id: owner.clone(),
            key: "create-0024".into(),
            child_id: Some(SessionId::default().to_string()),
            step: Some(crate::session::SagaStep::Worktree),
            worktree_path: Some(worktree.path().display().to_string()),
            // A saga past S2 recorded that its own `git branch` created the
            // branch; a reclaim acts on nothing it cannot prove it made.
            branch_claimed: true,
            branch: Some("feat/unfinished".into()),
            instance_id: Some("a-dead-instance".into()),
            lease_until: Some(1),
            ..Default::default()
        })
        .unwrap();

    h.h.app.tick_child_sagas();

    assert!(
        !h.effects.called("remove_worktree"),
        "uncommitted work is never removed by a reconciliation"
    );
    assert!(
        h.h.app
            .status_message
            .as_ref()
            .is_some_and(|m| m.text.contains("uncommitted work")),
        "the operator is told: {:?}",
        h.h.app.status_message
    );
}

/// A saga another instance is driving is left alone: two friring processes
/// reconciling one child would be two processes killing one pane.
#[tokio::test]
async fn recovery_leaves_a_live_lease_to_its_holder() {
    let mut h = ChildHarness::new();
    let owner = h.owner().to_string();
    h.h.app
        .db
        .upsert_child_saga(&crate::session::ChildSaga {
            owner_id: owner.clone(),
            key: "create-0017".into(),
            child_id: Some(SessionId::default().to_string()),
            step: Some(crate::session::SagaStep::Worktree),
            worktree_path: Some("/somewhere".into()),
            branch: Some("feat/theirs".into()),
            instance_id: Some("another-running-friring".into()),
            lease_until: Some(crate::sync::state::current_time_millis() + 600_000),
            ..Default::default()
        })
        .unwrap();

    h.h.app.tick_child_sagas();

    assert!(
        !h.effects.called("remove_worktree"),
        "another instance's saga is not this one's to reconcile"
    );
    let saga =
        h.h.app
            .db
            .child_saga(&owner, "create-0017")
            .unwrap()
            .unwrap();
    assert_eq!(saga.step, Some(crate::session::SagaStep::Worktree));
}

/// A hard delete of an owner stops its children first, and never deletes them:
/// removing a leader is not an instruction to throw away what its workers
/// wrote.
#[tokio::test]
async fn deleting_an_owner_stops_its_children_and_keeps_their_work() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0018").await;

    let owner = h.owner();
    h.h.app
        .cascade_bridge_delete(owner)
        .expect("the bridge is readable");
    assert_eq!(
        h.h.app.child_lifecycle.in_flight(),
        1,
        "the owner's live child is stopped first"
    );
    h.drive_until(60, |h| h.h.app.child_lifecycle.in_flight() == 0)
        .await;

    assert_eq!(h.child_state(), Some(crate::session::ChildState::Stopped));
    // The ownership row outlives everything.
    assert!(h.h.app.db.bridge_child(&child.child_id).unwrap().is_some());
}

/// A hard delete whose cascade cannot read the bridge is **refused**, not
/// carried out — the same fail-closed rule the headless force-delete follows.
///
/// An unreadable ownership table is not evidence that this session owns
/// nothing. Deleting it anyway would remove the one session that could stop
/// whatever it does own, and leave those agents running for an owner that is
/// gone.
#[tokio::test]
async fn a_hard_delete_is_refused_when_the_bridge_cannot_be_read() {
    let mut h = ChildHarness::new();
    h.ready_child("create-0095").await;
    let owner = h.owner();

    // Persisted so the force-delete stamp has a row it could land on.
    let shared = h.h.app.session_to_shared(&h.h.app.sessions[0]);
    h.h.app.db.upsert_session(&shared).unwrap();

    h.h.app
        .db
        .conn_ref()
        .execute("DROP TABLE bridge_children", [])
        .unwrap();

    h.h.app.confirm_hard_delete_session(owner);

    assert!(
        h.h.app.sessions.iter().any(|s| s.info.id == owner),
        "the owner left the session list on a delete that could not read its children"
    );
    assert!(
        h.h.app.db.get_session_by_id(owner).unwrap().is_some(),
        "a refused delete neither soft-deletes the row nor removes it"
    );
    let stamped: i64 =
        h.h.app
            .db
            .conn_ref()
            .query_row(
                "SELECT force_deleted FROM sessions WHERE id = ?1",
                [owner.to_string()],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(
        stamped, 0,
        "the owner was stamped force-deleted by a delete that was refused"
    );
    assert_eq!(
        h.h.app.child_lifecycle.in_flight(),
        0,
        "a stop was started for children the cascade could not enumerate"
    );
    assert!(
        matches!(
            h.h.app.status_message.as_ref().map(|m| m.level),
            Some(StatusLevel::Error)
        ),
        "the operator was not told the delete was refused"
    );
}

/// Deleting a **child** tells its owner, so a leader polling `status` does not
/// see a child that simply stopped existing.
#[tokio::test]
async fn deleting_a_child_tells_its_owner() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0019").await;
    let id: SessionId = child.child_id.parse().unwrap();

    h.h.app
        .cascade_bridge_delete(id)
        .expect("the bridge is readable");

    let owner_mail = h.h.app.db.list_messages(h.owner(), true, None).unwrap();
    assert!(
        owner_mail
            .iter()
            .any(|m| m.kind == "child.removed_by_operator"),
        "{owner_mail:?}"
    );
    assert!(h
        .h
        .app
        .db
        .bridge_child_state(&child.child_id)
        .unwrap()
        .unwrap()
        .force_deleted_at
        .is_some());
}

/// The fan-out cap counts **live** children, and a dirty one still holds its
/// slot.
#[tokio::test]
async fn the_fanout_cap_counts_live_children() {
    let mut h = ChildHarness::new();
    let owner = h.owner().to_string();
    for (n, state) in [
        (1, crate::session::ChildState::Ready),
        (2, crate::session::ChildState::Dirty),
    ] {
        let id = format!("filler-{n}");
        h.h.app
            .db
            .insert_bridge_child(&id, &owner, &format!("filler-key-{n}"))
            .unwrap();
        h.h.app.db.set_bridge_child_state(&id, state).unwrap();
    }
    h.create("create-0020");
    let answer = h.answer("create-0020").await;
    assert_eq!(answer["error"], "fanout_exhausted", "{answer}");
}

/// The cap counts committed rows **and** in-flight creates, because a tick takes
/// several requests at once and the rows only exist after S6. The window between
/// those two is where it can go wrong in the other direction: from `Committed`
/// until the child's own hook reports, a create has a row *and* a job, and
/// counting both charges one child two slots — halving the cap whenever anything
/// is starting up.
#[tokio::test]
async fn a_committed_child_is_charged_one_slot_not_two() {
    let mut h = ChildHarness::new();
    // `max_children = 2`, so a single child mid-startup must still leave room.
    h.create("create-0021");
    h.drive_until(60, |h| {
        h.child()
            .and_then(|c| h.h.app.db.child_saga_of_child(&c.child_id).ok().flatten())
            .and_then(|saga| saga.step)
            == Some(crate::session::SagaStep::Released)
    })
    .await;
    let first = h.child().expect("the first child committed");
    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Starting),
        "the window this test is about is committed-but-not-ready"
    );

    // A successful `create` is deferred until its child is ready, so the second
    // one is judged by what it commits rather than by an answer. A refusal is
    // what would arrive immediately, which is why its absence is the assertion.
    // Its own branch, because two live children never share a worktree — that is
    // a different refusal, and it would satisfy this assertion for the wrong
    // reason.
    h.create_on("create-0022", "feat/two");
    h.drive_until(60, |h| {
        h.h.app
            .db
            .bridge_children_of(&h.owner().to_string())
            .map(|c| c.len())
            .unwrap_or(0)
            == 2
    })
    .await;

    let refusal = bridge_answer(&h.h, 0, "create-0022");
    assert!(
        refusal.is_none(),
        "the second create was refused while one child was merely starting: {refusal:?}"
    );
    let children =
        h.h.app
            .db
            .bridge_children_of(&h.owner().to_string())
            .unwrap();
    assert_eq!(
        children.len(),
        2,
        "one starting child consumed both fan-out slots"
    );
    assert!(children.iter().any(|c| c.child_id == first.child_id));
}

/// The branch is chosen by the requesting agent and it decides a **path**:
/// `worktree_segments` maps only `/` to `-`, so `..` survives and resolves to
/// the parent of every friring worktree for that repository — which a create
/// would then hand the child as its workspace.
#[tokio::test]
async fn a_child_branch_that_would_escape_the_worktree_root_is_refused() {
    for branch in ["..", "../..", "-x", "a..b", "with space", "tail.lock", ""] {
        let mut h = ChildHarness::new();
        let key = "create-0050";
        queue_request(
            &h.h,
            0,
            key,
            &envelope(
                key,
                "create",
                serde_json::json!({
                    "repo_root": "/repo/app",
                    "branch": branch,
                    "agent": "worker",
                    "task_kind": "task",
                    "task_body": "do the thing",
                }),
            ),
        );
        let answer = h.answer(key).await;
        assert_eq!(answer["ok"], false, "branch '{branch}' was accepted");
        assert!(h.child().is_none(), "branch '{branch}' made a child anyway");
    }
}

/// `create_or_attach_worktree` returns an existing deterministic path unchecked,
/// so a create naming a branch the operator already has a worktree for would run
/// the child **in the operator's worktree** — and two children on one branch
/// would share one.
#[tokio::test]
async fn a_create_refuses_a_branch_whose_worktree_already_exists() {
    let mut h = ChildHarness::new();
    let planned =
        crate::git::planned_worktree_path(std::path::Path::new("/repo/app"), "feat/one").unwrap();
    std::fs::create_dir_all(&planned).unwrap();

    h.create("create-0051");
    let answer = h.answer("create-0051").await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .is_some_and(|m| m.contains("already a worktree")),
        "{answer}"
    );
    assert!(
        h.child().is_none(),
        "a child was made in an existing worktree"
    );
}

/// A child's boundary — the narrowing, the gate, the private state — is built
/// by the spawn saga and by nothing else, so every **generic** relaunch path has
/// to decline. `Ctrl+R` would otherwise put the agent back in the child's
/// worktree under its owner's un-narrowed profile.
#[tokio::test]
async fn a_bridge_child_is_never_relaunched_by_a_generic_path() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0040").await;
    let id: SessionId = child.child_id.parse().unwrap();

    assert!(
        h.h.app.is_bridge_child(id),
        "the ownership row is what marks a child"
    );
    let index =
        h.h.app
            .sessions
            .iter()
            .position(|s| s.info.id == id)
            .expect("the child is a session");
    h.h.app.active_index = index;
    h.h.app.restart_active_session();

    let refusal = h.h.app.status_message.as_ref().map(|m| m.text.clone());
    assert!(
        refusal
            .as_deref()
            .is_some_and(|m| m.contains("bridge child")),
        "the restart was not refused: {refusal:?}"
    );
    // Refused, not half-done: the child is still the session it was.
    assert!(h.h.app.sessions.iter().any(|s| s.info.id == id));

    // And the read fails **closed**: a row friring cannot read is a child, not
    // a non-child. An unreadable ownership table is exactly the state in which
    // relaunching would put the agent back under its owner's un-narrowed
    // profile, so the refusal has to survive it.
    h.h.app
        .db
        .conn_ref()
        .execute("DROP TABLE bridge_children", [])
        .unwrap();
    assert!(
        h.h.app.is_bridge_child(id),
        "an ownership row friring cannot read was read as 'not a child'"
    );
    h.h.app.status_message = None;
    h.h.app.restart_active_session();
    let refusal = h.h.app.status_message.as_ref().map(|m| m.text.clone());
    assert!(
        refusal
            .as_deref()
            .is_some_and(|m| m.contains("bridge child")),
        "the restart was not refused once ownership became unreadable: {refusal:?}"
    );
    assert!(h.h.app.sessions.iter().any(|s| s.info.id == id));
}

/// A child the quiesce has stopped must not stay an active session row with an
/// `agent_session_id` and no pane — that is precisely what startup restore
/// relaunches, restarting an agent inside a worktree friring already verified.
#[tokio::test]
async fn a_terminal_child_is_retired_from_the_session_list() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0041").await;
    let id: SessionId = child.child_id.parse().unwrap();
    assert!(h.h.app.sessions.iter().any(|s| s.info.id == id));

    h.quiesce(&child.child_id, crate::session::Outcome::Completed)
        .await;
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Done));

    assert!(
        !h.h.app.sessions.iter().any(|s| s.info.id == id),
        "a done child is still in the session list"
    );
    assert!(
        h.h.app.db.unloaded_session_ids().unwrap().contains(&id),
        "a done child's row is still loaded, so a restore would respawn it"
    );
    // The rows a later decision reads are untouched: ownership, the worktree and
    // the verdict are not runtime.
    assert!(h.h.app.db.bridge_child(&child.child_id).unwrap().is_some());
    assert!(h.h.app.db.bridge_result(&child.child_id).unwrap().is_some());
}

/// `stalled` is set from the nudge counter alone, with the child's agent still
/// running. A resume that simply relaunched would leave two panes under one
/// session id — and S8's identity check would find the *old* one, pass, and open
/// the *new* one's gate.
#[tokio::test]
async fn a_resume_stops_the_running_pane_and_leaves_one_session() {
    let backend = Arc::new(FakeBackend::spawnable());
    let mut h = ChildHarness::with_watched_backend(Arc::clone(&backend));
    let child = h.ready_child("create-0042").await;
    let id: SessionId = child.child_id.parse().unwrap();
    // The pane the child is running in *now*. Counting sessions cannot see the
    // regression this test is named for: S6 replaces the in-memory session
    // either way, so a resume that stopped nothing leaves the old pane alive
    // beside the new one and the session list still says one.
    let running_pane = {
        let panes = backend.panes.lock().unwrap();
        assert_eq!(panes.len(), 1, "a ready child is one pane: {panes:?}");
        panes.keys().next().cloned().unwrap()
    };
    h.h.app
        .db
        .set_bridge_child_state(&child.child_id, crate::session::ChildState::Stalled)
        .unwrap();

    queue_request(
        &h.h,
        0,
        "resume-0042",
        &envelope(
            "resume-0042",
            "resume",
            serde_json::json!({ "child": child.child_id }),
        ),
    );
    // Driven to the *relaunched* pane, not merely to `starting`: `begin_resume`
    // writes that column before the job runs, so stopping there would assert
    // nothing about what the saga did.
    h.drive_until(60, |h| {
        h.h.app
            .db
            .child_saga_of_child(&child.child_id)
            .ok()
            .flatten()
            .and_then(|saga| saga.step)
            == Some(crate::session::SagaStep::Released)
    })
    .await;

    assert_eq!(
        h.h.app.sessions.iter().filter(|s| s.info.id == id).count(),
        1,
        "one id must have exactly one session: S8 revalidates the first match"
    );
    assert_eq!(
        h.h.app
            .db
            .bridge_children_of(&h.owner().to_string())
            .unwrap()
            .len(),
        1,
        "a resume made a second child"
    );
    // One pane, and not the one that was already there: the old agent was
    // stopped and a new one relaunched, rather than two agents sharing one
    // worktree.
    let panes = backend.panes.lock().unwrap();
    assert_eq!(
        panes.len(),
        1,
        "the resumed child's old pane is still running beside its new one: {panes:?}"
    );
    assert!(
        !panes.contains_key(&running_pane),
        "the resume left the original pane in place, so nothing was relaunched"
    );
}

/// A request accepted against a job already carrying its child is journaled
/// `accepted` and answered by nothing of its own — and a replay of an accepted
/// key waits rather than acting. Without a waiter list that caller never hears
/// back.
#[tokio::test]
async fn a_second_request_against_a_running_job_is_answered_too() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0043").await;

    // Two stops for one child. The first starts the quiesce; the second finds a
    // job already carrying it.
    for key in ["stop-0043a", "stop-0043b"] {
        queue_request(
            &h.h,
            0,
            key,
            &envelope(key, "stop", serde_json::json!({ "child": child.child_id })),
        );
        h.drive(2).await;
    }
    h.h.app.accept_finish_intent(
        child.child_id.parse().unwrap(),
        crate::session::Outcome::Completed,
        None,
    );
    h.drive_until(60, |h| h.h.app.child_lifecycle.in_flight() == 0)
        .await;

    for key in ["stop-0043a", "stop-0043b"] {
        let answer = bridge_answer(&h.h, 0, key)
            .unwrap_or_else(|| panic!("'{key}' was accepted and never answered"));
        assert_eq!(answer["key"], key, "an answer carries the key it answers");
    }
}

/// A `stop` writes no saga row (the durable record of a quiesce is
/// `ChildState::Finishing` itself), so a crash mid-stop leaves its journal row
/// `accepted` for ever — and a replay of an accepted key waits by design.
/// Recovery closes those out with a typed refusal the caller can act on.
#[tokio::test]
async fn recovery_answers_a_request_its_predecessor_died_holding() {
    let mut h = ChildHarness::new();
    let owner = h.owner().to_string();
    // A row from *before* this instance came up: what a crash leaves behind.
    h.h.app
        .db
        .take_bridge_request(&owner, "stop-0044", "stop", "hash", None)
        .unwrap();
    h.h.app.child_lifecycle.started_at = u64::MAX;
    h.h.app.child_lifecycle.recovered = false;

    h.h.app.tick_child_sagas();

    let answer =
        bridge_answer(&h.h, 0, "stop-0044").expect("recovery left an accepted request unanswered");
    assert_eq!(answer["ok"], false);
    assert_eq!(answer["error"], "broker_absent", "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .is_some_and(|m| m.contains("restarted")),
        "{answer}"
    );

    // And a request this instance accepted itself is left alone: the broker runs
    // in `tick_core` and this driver in `tick_background`, so on the first tick
    // a fresh row is already in the journal.
    h.h.app.child_lifecycle.started_at = 0;
    h.h.app.child_lifecycle.recovered = false;
    h.h.app
        .db
        .take_bridge_request(&owner, "stop-0045", "stop", "hash", None)
        .unwrap();
    h.h.app.tick_child_sagas();
    assert!(
        bridge_answer(&h.h, 0, "stop-0045").is_none(),
        "recovery refused a request this instance had just accepted"
    );
}

// ── Stage H: host-verified results and escalation ────────────────────────

/// The verdict is the **host's**, read after the pane stopped. The child's
/// `result` carries an outcome and a summary and nothing else — there is no
/// field in it for a branch or a head, so a worker cannot report a commit it did
/// not make.
#[tokio::test]
async fn a_verdict_carries_only_what_the_host_read_after_the_stop() {
    let mut h = ChildHarness::new();
    *h.effects.verdict.lock().unwrap() = crate::git::WorktreeVerdict {
        branch: Some("feat/one".into()),
        head: Some("deadbeef".into()),
        dirty: false,
        ahead_of_base: 3,
        unreadable: false,
    };
    let child = h.ready_child("create-0030").await;
    h.quiesce(&child.child_id, crate::session::Outcome::Completed)
        .await;

    let verdict = h.h.app.db.bridge_result(&child.child_id).unwrap().unwrap();
    assert_eq!(verdict.head.as_deref(), Some("deadbeef"));
    assert_eq!(verdict.branch.as_deref(), Some("feat/one"));
    assert_eq!(verdict.ahead_of_base, 3);
    assert!(verdict.verified_at > 0, "the host stamped when it looked");
    // The verification happened **after** the kill, which is the property that
    // makes the verdict mean anything: friring read a worktree nothing could
    // still be writing. Observed from inside the verification rather than
    // inferred from both having happened.
    assert!(h.effects.called("verify_worktree"));
    let alive = h.effects.panes_alive_at_verify.lock().unwrap().clone();
    assert_eq!(
        alive,
        vec![0],
        "the host inspected the worktree while a pane was still alive"
    );

    // And `status` reports it, so an integration step reads the host's fields.
    queue_request(
        &h.h,
        0,
        "status-0030",
        &envelope("status-0030", "status", serde_json::json!({})),
    );
    let answer = h.answer("status-0030").await;
    let child_view = &answer["data"]["children"][0];
    assert_eq!(child_view["result"]["head"], "deadbeef", "{answer}");
    assert_eq!(child_view["result"]["dirty"], false, "{answer}");
    assert_eq!(child_view["result"]["ahead_of_base"], 3, "{answer}");
    assert_eq!(child_view["state"], "done", "{answer}");
}

/// A report that asks for an operator moves the child to `blocked` and mirrors
/// itself to the owner, labelled as the child's own words.
#[tokio::test]
async fn a_report_needing_an_operator_escalates_to_the_owner() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0031").await;

    let dir = crate::paths::bridge_request_dir(&child.child_id).unwrap();
    std::fs::write(
        dir.join("report-0031.req"),
        serde_json::to_string(&envelope(
            "report-0031",
            "report",
            serde_json::json!({
                "phase": "blocked",
                "progress": 40,
                "summary": "the migration needs a decision",
                "needs_operator": true,
            }),
        ))
        .unwrap(),
    )
    .unwrap();
    let response = crate::paths::bridge_response_dir(&child.child_id)
        .unwrap()
        .join("report-0031.res");
    h.drive_until(60, |_| response.exists()).await;

    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Blocked),
        "a child that says it is stuck is stuck in the owner's view too"
    );
    let owner_mail = h.h.app.db.list_messages(h.owner(), true, None).unwrap();
    let mirrored = owner_mail
        .iter()
        .find(|m| m.kind == "report")
        .expect("the report is mirrored to the owner");
    let body: serde_json::Value = serde_json::from_str(&mirrored.body).unwrap();
    assert_eq!(body["child_authored"], true, "{body}");
    assert_eq!(body["needs_operator"], true, "{body}");
    assert_eq!(body["summary"], "the migration needs a decision", "{body}");
    // The mail is attributed to the child, because it is the child's words.
    assert_eq!(
        mirrored.from_session_id.map(|id| id.to_string()),
        Some(child.child_id.clone())
    );
}

/// An escalation never puts a child that has already been judged back into the
/// fan-out.
#[tokio::test]
async fn an_escalation_never_revives_a_finished_child() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0032").await;
    h.quiesce(&child.child_id, crate::session::Outcome::Completed)
        .await;
    assert_eq!(h.child_state(), Some(crate::session::ChildState::Done));

    let dir = crate::paths::bridge_request_dir(&child.child_id).unwrap();
    std::fs::write(
        dir.join("report-0032.req"),
        serde_json::to_string(&envelope(
            "report-0032",
            "report",
            serde_json::json!({ "phase": "blocked", "needs_operator": true }),
        ))
        .unwrap(),
    )
    .unwrap();
    h.drive(15).await;

    assert_eq!(
        h.child_state(),
        Some(crate::session::ChildState::Done),
        "a verdict is not something a late report can undo"
    );
}

// ── Stage I: UI and CLI surfaces ─────────────────────────────────────────

/// The `Bridge:` row says what a session *is* in an orchestration, from either
/// side, and the owner's row marks a child that needs a person.
#[tokio::test]
async fn the_info_panel_says_where_a_session_sits_in_an_orchestration() {
    let mut h = ChildHarness::new();
    let child = h.ready_child("create-0040").await;

    let owner_row =
        h.h.app
            .bridge_row_for_test(h.owner())
            .expect("the owner has a bridge row");
    assert_eq!(owner_row.children, Some((1, 2)));
    assert!(!owner_row.needs_operator);
    assert!(
        owner_row.detail().contains("1/2 children"),
        "{}",
        owner_row.detail()
    );

    let id: SessionId = child.child_id.parse().unwrap();
    let child_row =
        h.h.app
            .bridge_row_for_test(id)
            .expect("the child has a bridge row");
    assert_eq!(child_row.child_state.as_deref(), Some("ready"));
    assert_eq!(child_row.owner.as_deref(), Some("session-0"));
    assert!(
        child_row.detail().starts_with("child of session-0"),
        "{}",
        child_row.detail()
    );

    // A dirty child is one an operator has to deal with, and the owner's row
    // has to say so — the whole point of the row.
    h.h.app
        .db
        .set_bridge_child_state(&child.child_id, crate::session::ChildState::Dirty)
        .unwrap();
    let owner_row = h.h.app.bridge_row_for_test(h.owner()).unwrap();
    assert!(owner_row.needs_operator);
    assert!(
        owner_row.detail().contains("needs you"),
        "{}",
        owner_row.detail()
    );
}

/// A session in no orchestration gets no row at all: the overwhelming majority
/// of sessions must pay nothing for a feature they do not use.
#[test]
fn an_ordinary_session_has_no_bridge_row() {
    let h = Harness::standard(1);
    let id = h.app.sessions[0].info.id;
    assert!(h.app.bridge_row_for_test(id).is_none());
}

/// `session get` carries the host's verdict and never the child's own words:
/// an integration step reads this document, and text a worker wrote must not be
/// part of what decides whether its branch is merged.
#[tokio::test]
async fn session_get_reports_the_bridge_and_never_a_childs_summary() {
    let mut h = ChildHarness::new();
    *h.effects.verdict.lock().unwrap() = crate::git::WorktreeVerdict {
        branch: Some("feat/one".into()),
        head: Some("cafebabe".into()),
        dirty: false,
        ahead_of_base: 4,
        unreadable: false,
    };
    let child = h.ready_child("create-0041").await;
    h.quiesce(&child.child_id, crate::session::Outcome::Completed)
        .await;

    // The owner is a harness stub, so it reaches the rows the way every
    // session does — through `save_state`, which is also what writes the
    // `session_repos` row a `create` is checked against.
    h.h.app.save_state();
    let owner = h.owner().to_string();
    let out = crate::cli::sessions::run(
        crate::cli::sessions::Action::Get {
            uuid: owner.clone(),
        },
        &h.h.app.db,
    )
    .expect("session get");
    let bridge = &out["bridge"];
    assert!(
        bridge["owner"].is_null(),
        "the owner has no owner: {bridge}"
    );
    assert_eq!(bridge["children"][0]["id"], child.child_id, "{bridge}");
    assert_eq!(bridge["children"][0]["state"], "done", "{bridge}");
    assert_eq!(
        bridge["children"][0]["result"]["head"], "cafebabe",
        "{bridge}"
    );
    assert_eq!(
        bridge["children"][0]["result"]["ahead_of_base"], 4,
        "{bridge}"
    );
    // No summary field anywhere: the document an integration step reads carries
    // host-known fields only.
    assert!(
        !out.to_string().contains("summary"),
        "a session document must not carry child-authored text: {out}"
    );
    // And the egress token is nowhere in it, as nothing that renders a session
    // may carry one.
    assert!(!out.to_string().contains("token"), "{out}");

    // The child's own document names its owner.
    let child_out = crate::cli::sessions::run(
        crate::cli::sessions::Action::Get {
            uuid: child.child_id.clone(),
        },
        &h.h.app.db,
    )
    .expect("session get for the child");
    assert_eq!(child_out["bridge"]["owner"], owner, "{child_out}");
    assert_eq!(child_out["bridge"]["state"], "done", "{child_out}");
}

/// An ordinary session's document carries `"bridge": null` — present, so a
/// consumer can key on it, and empty, so it says what it means.
#[test]
fn an_ordinary_session_document_carries_a_null_bridge() {
    let db = Database::open_in_memory().unwrap();
    let shared = sync::SharedSession {
        id: SessionId::default(),
        name: "plain".into(),
        agent: "claude".into(),
        backend_id: String::new(),
        backend_type: "local-tmux".into(),
        agent_session_id: None,
        cwd: None,
        additional_dirs: Vec::new(),
        workspace_dir: None,
        worktrees: Vec::new(),
        shell_backend_id: None,
        sandbox_profile: None,
        sandbox_enforcement: Default::default(),
        parent_session_id: None,
        display_order: None,
        tombstone: false,
        tombstone_at: None,
        mux: crate::session::MuxIdentity::default(),
        egress: crate::session::EgressRecord::default(),
        sandbox_overlay: None,
    };
    db.upsert_session(&shared).unwrap();
    let out = crate::cli::sessions::run(
        crate::cli::sessions::Action::Get {
            uuid: shared.id.to_string(),
        },
        &db,
    )
    .unwrap();
    assert!(out["bridge"].is_null(), "{out}");
}

/// The five bridge fields round-trip through the profile editor, and the four
/// that only mean anything once something is granted appear with the grant.
#[test]
fn the_profile_editor_round_trips_the_bridge_fields() {
    use crate::app::modals::{BridgePreset, SandboxField};

    let mut profile = crate::session::SandboxProfile::new(
        "orchestrator",
        vec![crate::session::SandboxPath::workspace("~/dev/app")],
    );
    profile.backend = crate::session::SandboxBackendKind::Seatbelt;
    profile.bridge_grants = vec![
        crate::session::BridgeCapability::ChildLifecycle,
        crate::session::BridgeCapability::Mailbox,
        crate::session::BridgeCapability::Report,
    ];
    profile.max_children = 5;
    profile.child_agents = vec!["codex".into(), "claude".into()];
    profile.child_shared_rw = vec!["~/.cargo/registry".into()];
    profile.child_seed_allow = vec![crate::session::ChildSeedAllow {
        path: "auth.json".into(),
        mode: crate::session::SeedMode::LinkRw,
    }];

    let editor = crate::app::modals::SandboxEditorModal::from_profile(&profile);
    assert_eq!(editor.bridge.preset, BridgePreset::Leader);
    let fields = editor.visible_fields();
    for field in [
        SandboxField::BridgeGrants,
        SandboxField::MaxChildren,
        SandboxField::ChildAgents,
        SandboxField::ChildSharedRw,
        SandboxField::ChildSeedAllow,
    ] {
        assert!(fields.contains(&field), "{field:?} is missing");
    }
    let rebuilt = editor.build_profile().expect("the form round-trips");
    assert_eq!(rebuilt.bridge_grants, profile.bridge_grants);
    assert_eq!(rebuilt.max_children, 5);
    assert_eq!(rebuilt.child_agents, profile.child_agents);
    assert_eq!(rebuilt.child_shared_rw, profile.child_shared_rw);
    assert_eq!(rebuilt.child_seed_allow, profile.child_seed_allow);
}

/// A profile that grants nothing shows only the grant row: a fan-out cap on a
/// profile with no bridge is a number nothing reads.
#[test]
fn the_dependent_bridge_rows_appear_with_the_grant() {
    use crate::app::modals::SandboxField;

    let mut profile = crate::session::SandboxProfile::new(
        "plain",
        vec![crate::session::SandboxPath::workspace("~/dev/app")],
    );
    profile.backend = crate::session::SandboxBackendKind::Seatbelt;
    let mut editor = crate::app::modals::SandboxEditorModal::from_profile(&profile);
    let fields = editor.visible_fields();
    assert!(fields.contains(&SandboxField::BridgeGrants));
    assert!(!fields.contains(&SandboxField::MaxChildren));

    editor.field = SandboxField::BridgeGrants;
    editor.adjust(1);
    assert!(editor.visible_fields().contains(&SandboxField::MaxChildren));
    assert!(!editor.build_profile().unwrap().bridge_grants.is_empty());
}

/// A seed authorization friring could not join onto a child's private state
/// directory is refused at save, not at launch.
#[test]
fn a_seed_authorization_that_would_escape_is_refused_at_save() {
    use crate::app::modals::{BridgePreset, SandboxField};

    let mut profile = crate::session::SandboxProfile::new(
        "orchestrator",
        vec![crate::session::SandboxPath::workspace("~/dev/app")],
    );
    profile.backend = crate::session::SandboxBackendKind::Seatbelt;
    let mut editor = crate::app::modals::SandboxEditorModal::from_profile(&profile);
    editor.bridge.preset = BridgePreset::Leader;
    editor.field = SandboxField::ChildSeedAllow;

    for hostile in [
        "../../etc/passwd:copy",
        "/etc/passwd:copy",
        "auth.json:teleport",
    ] {
        editor.bridge.child_seed_allow.set(hostile);
        assert!(
            editor.build_profile().is_err(),
            "'{hostile}' must be refused at save"
        );
    }
    editor.bridge.child_seed_allow.set("auth.json:link-rw");
    assert!(editor.build_profile().is_ok());
}

/// A profile that resolves to a **place** cannot grant the bridge: the bridge
/// is a directory friring mints on the host, and a place has no such path.
#[test]
fn a_place_backed_profile_cannot_grant_the_bridge() {
    use crate::app::modals::{sandbox_field_available, BridgePreset, SandboxField};

    for field in [
        SandboxField::BridgeGrants,
        SandboxField::MaxChildren,
        SandboxField::ChildAgents,
        SandboxField::ChildSharedRw,
        SandboxField::ChildSeedAllow,
    ] {
        assert!(
            !sandbox_field_available(
                field,
                crate::session::SandboxBackendKind::Docker,
                crate::session::NetworkMode::Allowlist
            ),
            "{field:?} must be unavailable for a place"
        );
        assert!(sandbox_field_available(
            field,
            crate::session::SandboxBackendKind::Seatbelt,
            crate::session::NetworkMode::Allowlist
        ));
    }
    // And a grant typed against a policy backend is dropped rather than saved
    // when the backend is changed to a place.
    let mut profile = crate::session::SandboxProfile::new(
        "moved",
        vec![crate::session::SandboxPath::workspace("~/dev/app")],
    );
    profile.backend = crate::session::SandboxBackendKind::Seatbelt;
    let mut editor = crate::app::modals::SandboxEditorModal::from_profile(&profile);
    editor.bridge.preset = BridgePreset::Leader;
    assert!(!editor.build_profile().unwrap().bridge_grants.is_empty());
    editor.backend = crate::session::SandboxBackendKind::Docker;
    assert!(
        editor.build_profile().unwrap().bridge_grants.is_empty(),
        "a grant the launch would refuse must not be saved"
    );
}

// ── Stage L: egress restoration across a restart ─────────────────────────

/// A filtered session's proxy listener lives in the **friring process**, so a
/// restart has to rebind one at the *same* endpoint with the *same* token — an
/// agent that is still running holds proxy URLs naming both.
///
/// This drops the `App` and rebuilds from the same database, which is what a
/// restart is from the row's point of view.
#[tokio::test]
async fn a_restart_rebinds_a_filtered_session_at_its_persisted_endpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::paths::TestPathGuard::new(tmp.path());
    let _host = crate::agent::sandboxing::TestSandboxHost::seatbelt();
    let db_path = tmp.path().join("restore.db");

    let mut profile = crate::session::SandboxProfile::new(
        "filtered",
        vec![crate::session::SandboxPath::workspace("~/dev/app")],
    );
    profile.network_mode = crate::session::NetworkMode::Allowlist;
    profile.network_allow = vec!["api.example.com:443".into()];

    // What a launch persisted: the endpoint the agent's environment names, and
    // the credential it demands.
    let session_id = SessionId::default();
    let endpoint = {
        let db = Database::open(&db_path).unwrap();
        db.upsert_sandbox_profile(&profile).unwrap();
        let shared = sync::SharedSession {
            id: session_id,
            name: "filtered".into(),
            agent: "claude".into(),
            backend_id: "fake:0".into(),
            backend_type: "local-tmux".into(),
            agent_session_id: None,
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: Vec::new(),
            shell_backend_id: None,
            sandbox_profile: Some("filtered".into()),
            sandbox_enforcement: Default::default(),
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
            mux: crate::session::MuxIdentity::default(),
            egress: crate::session::EgressRecord {
                endpoint: Some("tcp:0".into()),
                token: Some("a-persisted-token".into()),
                state: crate::session::EgressState::Active,
            },
            sandbox_overlay: None,
        };
        db.upsert_session(&shared).unwrap();
        // Bind once so the port is a real one, then record what it became: the
        // row has to name an endpoint a restart can actually rebind.
        let home = crate::paths::home_dir().unwrap().display().to_string();
        // The host this test installed, which is the one the restore resolves
        // too — so the transport both sides bind in is seatbelt's loopback
        // whatever the machine underneath offers.
        let backend = crate::agent::sandboxing::with_host(|host| {
            host.select(profile.backend).backend().unwrap()
        });
        let policy = profile.resolve(backend, &home).unwrap();
        let bound = crate::sandbox::egress::establish_at(
            &session_id.to_string(),
            &policy,
            crate::sandbox::ProxyTransport::Loopback,
            "tcp:0",
            "a-persisted-token",
        )
        .expect("the first bind");
        let endpoint = crate::sandbox::egress::PersistedEndpoint::of(&bound.endpoint).to_string();
        crate::sandbox::egress::stop(&session_id.to_string());
        db.set_session_egress(
            session_id,
            &crate::session::EgressRecord {
                endpoint: Some(endpoint.clone()),
                token: Some("a-persisted-token".into()),
                state: crate::session::EgressState::Active,
            },
        )
        .unwrap();
        endpoint
    };

    // The restart: a fresh App over the same rows.
    let backend: Arc<dyn SessionBackend> = Arc::new(FakeBackend::stub());
    let provider: Arc<dyn AgentProvider> = Arc::new(GenericProvider::new(
        crate::agent::agent_config::builtin_registry()
            .default_agent()
            .unwrap()
            .clone(),
    ));
    let mut app = App::new(
        STD_ROWS,
        STD_COLS,
        BackendRegistry::new(Arc::clone(&backend)),
        crate::agent::agent_config::builtin_registry(),
        Database::open(&db_path).unwrap(),
    );
    let mut session = Session::stub("filtered", &backend, &provider);
    session.info.id = session_id;
    session.info.sandbox_profile = Some("filtered".into());
    app.sessions.push(session);

    app.restore_session_egress_for_test(session_id);

    let state = app.sessions[0].info.egress_state.clone();
    assert_eq!(
        state,
        crate::session::EgressState::Active,
        "the proxy was rebound at the persisted endpoint: {endpoint}"
    );
    // And the row says so too, so a second restart reads the same thing.
    let persisted = app.db.session_egress(session_id).unwrap().unwrap();
    assert_eq!(persisted.state, crate::session::EgressState::Active);
    assert_eq!(persisted.endpoint.as_deref(), Some(endpoint.as_str()));
    // The token is untouched: rotating it would invalidate the URLs a running
    // agent already holds.
    assert_eq!(persisted.token.as_deref(), Some("a-persisted-token"));
    crate::sandbox::egress::stop(&session_id.to_string());
}

/// A port the persisted endpoint names that is no longer available leaves the
/// session `Unrestorable` — never `sandbox_unenforced`.
///
/// The distinction is the whole reason `EgressState` exists: an enforced
/// boundary with a dead proxy is still enforced. The agent has *no* network
/// rather than an unfiltered one, and painting `⚠ NOT APPLIED` over it would
/// say the agent is running on the host.
#[tokio::test]
async fn a_port_that_cannot_be_rebound_is_unrestorable_and_never_unenforced() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::paths::TestPathGuard::new(tmp.path());
    let _host = crate::agent::sandboxing::TestSandboxHost::seatbelt();

    let mut profile = crate::session::SandboxProfile::new(
        "filtered",
        vec![crate::session::SandboxPath::workspace("~/dev/app")],
    );
    profile.network_mode = crate::session::NetworkMode::Allowlist;
    profile.network_allow = vec!["api.example.com:443".into()];

    let backend: Arc<dyn SessionBackend> = Arc::new(FakeBackend::stub());
    let provider: Arc<dyn AgentProvider> = Arc::new(GenericProvider::new(
        crate::agent::agent_config::builtin_registry()
            .default_agent()
            .unwrap()
            .clone(),
    ));
    let mut app = App::new(
        STD_ROWS,
        STD_COLS,
        BackendRegistry::new(Arc::clone(&backend)),
        crate::agent::agent_config::builtin_registry(),
        Database::open_in_memory().unwrap(),
    );
    app.db.upsert_sandbox_profile(&profile).unwrap();
    let session_id = SessionId::default();
    let mut session = Session::stub("filtered", &backend, &provider);
    session.info.id = session_id;
    session.info.sandbox_profile = Some("filtered".into());
    app.sessions.push(session);
    app.save_state();

    // Somebody else is holding the port the row names.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    app.db
        .set_session_egress(
            session_id,
            &crate::session::EgressRecord {
                endpoint: Some(format!("tcp:{port}")),
                token: Some("t".into()),
                state: crate::session::EgressState::Active,
            },
        )
        .unwrap();

    app.restore_session_egress_for_test(session_id);

    let state = app.sessions[0].info.egress_state.clone();
    assert!(
        matches!(state, crate::session::EgressState::Unrestorable(_)),
        "a port friring cannot take back is unrestorable, got {state:?}"
    );
    // The boundary is still enforced: nothing wrote the fallback marker.
    assert!(
        !matches!(
            app.sessions[0].info.sandbox_state,
            Some(crate::session::SandboxState::Unenforced(_))
        ),
        "an enforced boundary with a dead proxy is still enforced"
    );
    // And the token is exactly as it was.
    assert_eq!(
        app.db
            .session_egress(session_id)
            .unwrap()
            .unwrap()
            .token
            .as_deref(),
        Some("t")
    );
    drop(taken);
}

// ── Stage L: the refusals, in every shape ────────────────────────────────

/// Every way a bridge-required agent must **not** start, asserted through the
/// launch path rather than against the refusal function alone.
///
/// The failure this closes is subtle: a bridge-required agent that starts
/// without a bridge has no boundary *and* a channel nobody is serving, which is
/// strictly worse than not starting. So every one of these is an integrity
/// refusal that `allow_unsandboxed_fallback` may not answer.
#[test]
fn a_bridge_required_agent_never_starts_where_it_cannot_be_served() {
    let tmp = tempfile::tempdir().unwrap();
    let _guard = crate::paths::TestPathGuard::new(tmp.path());
    let _host = crate::agent::sandboxing::TestSandboxHost::seatbelt();

    let mut def = worker_agent();
    def.sandbox.as_mut().unwrap().bridge_requires = vec![crate::session::BridgeCapability::Mailbox];

    let mut granting = crate::session::SandboxProfile::new(
        "granting",
        vec![crate::session::SandboxPath::workspace("~/dev/app")],
    );
    granting.network_mode = crate::session::NetworkMode::None;
    granting.bridge_grants = vec![crate::session::BridgeCapability::Mailbox];
    // Even with the escape hatch on, none of these may fall back to the host.
    granting.allow_unsandboxed_fallback = true;

    let config = |profile: Option<crate::session::SandboxProfile>| crate::session::SessionConfig {
        session_id: Some(SessionId::default()),
        agent: "worker".into(),
        sandbox: profile,
        ..Default::default()
    };

    // No profile at all: the bridge is granted by a profile.
    let err = crate::agent::sandboxing::apply(Some(&def), &config(None), "worker", &[])
        .expect_err("a bridge agent with no profile is refused");
    assert!(err.contains("never started as a plain session"), "{err}");

    // A profile that grants nothing of what the agent needs.
    let mut ungranting = granting.clone();
    ungranting.bridge_grants = Vec::new();
    let err = crate::agent::sandboxing::apply(Some(&def), &config(Some(ungranting)), "worker", &[])
        .expect_err("a profile that grants nothing is refused");
    assert!(err.contains("grant_missing"), "{err}");

    // A remote session: friring mints the bridge directory on the machine it
    // runs on.
    let mut remote = config(Some(granting.clone()));
    remote.backend = Some("ssh:devbox".into());
    let err = crate::agent::sandboxing::apply(Some(&def), &remote, "worker", &[])
        .expect_err("a remote session cannot carry the bridge");
    assert!(err.contains("runs on a remote host"), "{err}");

    // The headless path, which exits after spawning: nothing would own the
    // session's proxy or answer its requests.
    let err = crate::agent::sandboxing::apply_for(
        Some(&def),
        &config(Some(granting.clone())),
        "worker",
        &[],
        None,
        false,
    )
    .expect_err("a headless create cannot serve the bridge");
    assert!(err.contains("bridge_requires_tui"), "{err}");

    // And the one that must work, so the four above are refusals rather than a
    // feature that never starts anything.
    assert!(crate::agent::sandboxing::apply(
        Some(&def),
        &config(Some(granting.clone())),
        "worker",
        &[]
    )
    .is_ok());

    // Last, because installing another host clears the one above: a machine
    // offering no backend at all. `bridge_refusal` cannot see this — the probe
    // fails, so there are no capabilities to inspect — and composition refuses
    // with an ordinary, non-integrity `Refusal` that `allow_unsandboxed_fallback`
    // would otherwise turn into an unsandboxed launch of a bridge agent.
    let _bare = crate::agent::sandboxing::TestSandboxHost::new(crate::sandbox::SandboxHost::new(
        std::sync::Arc::new(
            crate::sandbox::probe::StubHost::new()
                .with_home("/fabricated/home")
                .with_command(
                    "uname -s",
                    crate::sandbox::probe::ProbeOutput::success("Linux\n"),
                )
                .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n"),
        ),
    ));
    let refused =
        crate::agent::sandboxing::apply(Some(&def), &config(Some(granting)), "worker", &[]);
    assert!(
        refused.is_err(),
        "a bridge agent must not fall back onto the host: {refused:?}"
    );
}

/// A child whose profile authorizes no seed the agent requires never launches
/// — and never launches sharing its family's state instead.
#[tokio::test]
async fn a_child_whose_seed_is_unauthorized_is_refused_and_never_shared() {
    let mut h = ChildHarness::new();
    // The agent now requires a seed the profile does not authorize.
    let agent =
        h.h.app
            .agents
            .agents
            .iter_mut()
            .find(|a| a.name == "worker")
            .unwrap();
    agent.sandbox.as_mut().unwrap().child_state_seed = vec![crate::session::ChildStateSeed {
        src: "auth.json".into(),
        mode: crate::session::SeedMode::LinkRw,
        required: true,
    }];

    h.create("create-0050");
    let answer = h.answer("create-0050").await;
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"], "state_unrelocatable", "{answer}");
    assert!(h.child().is_none(), "nothing was launched");
    // There is no shared-state mode and no fallback to one, so nothing was
    // seeded either.
    assert!(!h.effects.called("seed_child_state"), "the plan never ran");
}
