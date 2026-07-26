//! Key event handlers for the Friring TUI application.
//!
//! This module contains all keyboard input handling logic organized by context:
//! - Global keybindings (always active)
//! - Focus-based handlers (ProjectList, SessionList, Terminal)
//! - Modal handlers (RepoPicker, BranchSelector, AgentPicker, etc.)

use crate::session::SessionConfig;

use super::{clock, App, InputFocus, TerminalView};
use crate::agent::input;
use crate::paths;
use crossterm::event::{KeyCode, KeyModifiers, ModifierKeyCode};
use tracing::{error, warn};

/// Two bare `Shift` taps at most this far apart — with no other key between —
/// open the global search (the JetBrains "Search Everywhere" gesture).
pub(crate) const DOUBLE_SHIFT_WINDOW_MS: u64 = 400;

/// Convert a session name into a git-branch-friendly name.
///
/// Lowercases, collapses each run of spaces/underscores/hyphens to a single
/// `-`, drops other non-alphanumeric chars, and trims leading/trailing `-`.
/// `/` is kept so hierarchical names (`fix/branch-naming`) survive as the
/// branch pre-fill: a `/` absorbs any `-` that would sit next to it (so a
/// separator adjacent to a `/` collapses into the `/` rather than becoming a
/// hyphen), consecutive `/` collapse, and leading/trailing `/` are trimmed —
/// keeping the result valid per `git check-ref-format`.
fn session_name_to_branch(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() {
            result.push(c.to_ascii_lowercase());
        } else if c == '/' {
            while result.ends_with('-') || result.ends_with('/') {
                result.pop();
            }
            if !result.is_empty() {
                result.push('/');
            }
        } else if (c == ' ' || c == '-' || c == '_')
            && !(result.ends_with('-') || result.ends_with('/'))
        {
            result.push('-');
        }
    }
    result.trim_matches(['-', '/']).to_string()
}

/// Whether a pressed chord is a bare `Ctrl+<letter>` — the namespace friring
/// shares with readline / shell line-editing chords. Used to gate
/// [`crate::session::Action::terminal_passthrough`] so the PTY-deferral only
/// fires for the conflicting chords; a non-`Ctrl+letter` rebind of a
/// passthrough action keeps working in the terminal.
fn is_ctrl_letter_chord(code: KeyCode, mods: KeyModifiers) -> bool {
    mods == KeyModifiers::CONTROL && matches!(code, KeyCode::Char(c) if c.is_ascii_alphabetic())
}

/// The directory-completion suffix to append after `prefix`, given the candidate
/// directory `names` in the parent (as returned by `git::list_dir_on`). Mirrors
/// `paths::complete_directory_path` for the remote case:
/// - hidden (`.`-prefixed) names are offered only to a `.`-prefix (like the
///   local completer's `matching_dir_names`);
/// - no match, or an ambiguous prefix with nothing more shared → `None`;
/// - a single match → the remaining chars **plus a trailing `/`** (so the next
///   `Tab` descends into it);
/// - several matches → their longest common prefix beyond `prefix`.
///
/// Pure (no I/O) so the completion logic is unit-tested without a remote host.
fn dir_completion_suffix(names: &[String], prefix: &str) -> Option<String> {
    let show_hidden = prefix.starts_with('.');
    let matches: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|n| show_hidden || !n.starts_with('.'))
        .filter(|n| n.starts_with(prefix))
        .collect();
    let first = matches.first()?;
    // Longest common prefix of the matches, floored to a char boundary of
    // `first` (a byte-wise LCP can land mid-char when names diverge inside a
    // multibyte char). It always covers at least `prefix` — itself a valid
    // boundary — so slicing back into `first` is safe.
    let mut common_len = matches[1..].iter().fold(first.len(), |end, m| {
        first
            .bytes()
            .zip(m.bytes())
            .take(end)
            .take_while(|(a, b)| a == b)
            .count()
    });
    while !first.is_char_boundary(common_len) {
        common_len -= 1;
    }
    let beyond = first.get(prefix.len()..common_len)?;
    if beyond.is_empty() && matches.len() > 1 {
        return None; // ambiguous with nothing new to add
    }
    let suffix = if matches.len() == 1 {
        format!("{beyond}/")
    } else {
        beyond.to_string()
    };
    (!suffix.is_empty()).then_some(suffix)
}

impl App {
    /// Main key handler dispatcher.
    ///
    /// Routes key events to the appropriate handler based on:
    /// 1. Modal state (highest priority)
    /// 2. Global keybindings (Ctrl+Q, Ctrl+N, etc.)
    /// 3. Focus-based handlers (ProjectList, SessionList, Terminal)
    pub(crate) fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        // Bare modifier presses only arrive on kitty-protocol terminals (we
        // push REPORT_ALL_KEYS_AS_ESCAPE_CODES). They are inert for every
        // handler below and must never reach a text input or the PTY, so they
        // are consumed here — where a double-tap of `Shift` opens the global
        // search. Any other key breaks a pending double-tap. (Alt press/release
        // never lands here — it arrives as `AppMessage::AltHeld`; see
        // `key_to_message`.)
        if let KeyCode::Modifier(m) = code {
            self.handle_modifier_press(m);
            return;
        }
        self.pending_double_shift = None;

        // Self-heal a missed Alt release (a lost kitty release event, e.g.
        // terminal focus stolen mid-hold): while Alt is really held every key
        // event carries the ALT bit, so one arriving without it means the
        // release never reached us.
        if self.alt_held && !mods.contains(KeyModifiers::ALT) {
            self.set_alt_held(false);
        }

        // Help overlay + clipboard chords are routed before any modal handler.
        if self.handle_priority_key(code, mods) {
            return;
        }

        // Cmd/Super chords are commands, never text: only the keybinding
        // lookup may consume them (see `handle_super_chord`).
        if mods.contains(KeyModifiers::SUPER) {
            self.handle_super_chord(code, mods);
            return;
        }

        // An open modal captures all input.
        if self.handle_modal_key_if_open(code, mods) {
            return;
        }

        // The global-search popup captures all input while open (typed chars
        // edit the query; arrows/Enter/Esc navigate/activate/close).
        if self.global_search.active {
            self.handle_global_search_key(code, mods);
            return;
        }

        // Any key press clears text selection (but the key still performs its action)
        self.text_selection = None;

        // The leader key, routed **ahead of every capture pane**. Those panes
        // consume all Ctrl chords outside their small escape lists, so with the
        // leader behind them it could not arm at all from the code-review or
        // activity views — the leader has to be the one key that always works,
        // or it isn't a leader. The exception is a text-entry submode
        // (`text_entry_owns_keys`), where the leader chord is a line-editing
        // key in the field being typed into.
        if !self.text_entry_owns_keys() && self.handle_prefix_key(code, mods) {
            return;
        }

        // The in-pane automation editor / run-history capture input like the
        // overlay modal (see `handle_automation_pane_capture`).
        if self.handle_automation_pane_capture(code, mods) {
            return;
        }

        // The native code-review view captures keys (nav / comment / compose)
        // before the global lookup; focus/quit chords fall through so the user
        // can always leave.
        if self.handle_code_review_key(code, mods) {
            return;
        }

        // The review's changed-files list (file-viewer column) likewise captures
        // its navigation keys before the global lookup, with the same fall-through
        // for focus/quit chords.
        if self.handle_review_files_key(code, mods) {
            return;
        }

        // The Claude Code activity view (transcript pane + its tree) captures
        // navigation keys before the global lookup too — so plain `j`/`k`/`g`
        // navigate rather than hitting the keybinding lookup or the PTY. Focus/
        // quit chords fall through (see `cc_escape_chord`).
        if self.handle_cc_activity_key(code, mods) {
            return;
        }
        if self.handle_cc_activity_tree_key(code, mods) {
            return;
        }

        // Session jump digits (`Alt+1…9`, and plain digits / `Esc` while the
        // blocked-only overlay is open) are fixed keys, routed here — after
        // the modal/capture gates so typed digits still reach text inputs,
        // before the lookup + pane handlers so they can't leak into the PTY.
        if self.handle_session_jump_key(code, mods) {
            return;
        }

        // Keybinding lookup, scoped to the focused pane: global actions plus
        // any scoped to the current context (file viewer, session list,
        // terminal). Some readline/shell chords (Ctrl+A/E/W/U/R/D/…) defer to
        // the PTY when the terminal is focused so the inner agent CLI's
        // line editing keeps working — see `Action::terminal_passthrough`.
        // The deferral is gated on the bound chord still being a bare
        // `Ctrl+<letter>`, so a rebind to a non-conflicting key keeps the
        // friring command working even in the terminal.
        let context = self.focus_key_context();
        if let Some(action) = self.keybindings.lookup_in(context, code, mods) {
            // In `prefix-only` the leader is the sole route to a **global**
            // command — that is what hands the whole `Ctrl+<letter>` namespace
            // back to the agent CLI. Pane-scoped actions (session-list `j`/`k`,
            // file-viewer nav) are untouched: they are single letters inside a
            // focused pane and were never part of the contested space.
            let direct_blocked = !self.prefix_settings.mode.direct_enabled()
                && action.context() == crate::session::KeyContext::Global;
            let defer_to_pty = self.focus == InputFocus::Terminal
                && action.terminal_passthrough()
                && is_ctrl_letter_chord(code, mods);
            if !direct_blocked && !defer_to_pty && self.dispatch_action(action) {
                return;
            }
        }

        self.handle_focused_pane_key(code, mods);
    }

    /// A bare modifier key press (kitty-protocol terminals only). Two `Shift`
    /// taps within [`DOUBLE_SHIFT_WINDOW_MS`] open the global search —
    /// mirroring the JetBrains "Search Everywhere" gesture. Taps never
    /// accumulate while a modal or the search itself owns input (there,
    /// `Shift` presses are just capitals being typed), and any non-`Shift`
    /// modifier breaks a pending tap like a regular key would.
    fn handle_modifier_press(&mut self, m: ModifierKeyCode) {
        let is_shift = matches!(m, ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift);
        if !is_shift
            || self.modal.is_open()
            || self.global_search.active
            || !self.features.double_shift_search
        {
            self.pending_double_shift = None;
            return;
        }
        let within_window = self.pending_double_shift.take().is_some_and(|t| {
            clock::elapsed_since(t) <= std::time::Duration::from_millis(DOUBLE_SHIFT_WINDOW_MS)
        });
        if within_window {
            // Routed through the action dispatch so the `features.global_search`
            // gate (and its toast) behave exactly like the Ctrl+/ chord.
            self.dispatch_action(crate::session::Action::GlobalSearch);
        } else {
            self.pending_double_shift = Some(clock::now());
        }
    }

    /// Cmd/Super chords are commands, never text: only the keybinding lookup
    /// may consume them. The focus-based handlers (modal inputs, the search
    /// query, in-pane editors, list hotkeys) predate the kitty keyboard
    /// protocol and match `Char` without checking SUPER, so a chord like Cmd+J
    /// would otherwise type a bare `j`. While a modal or the search popup owns
    /// input the chord is swallowed outright, mirroring how Ctrl chords are
    /// unavailable there. (The terminal pass-through swallows SUPER on its own —
    /// `agent::input::key_to_bytes`.)
    fn handle_super_chord(&mut self, code: KeyCode, mods: KeyModifiers) {
        if self.modal.is_open() || self.global_search.active {
            return;
        }
        self.text_selection = None;
        let context = self.focus_key_context();
        if let Some(action) = self.keybindings.lookup_in(context, code, mods) {
            self.dispatch_action(action);
        }
    }

    /// Route a key (already cleared of priority/modal/global handling) to the
    /// handler for the currently focused pane.
    fn handle_focused_pane_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.focus {
            InputFocus::SessionList => self.handle_session_list_key(code),
            InputFocus::Automations => self.handle_automations_pane_key(code),
            InputFocus::AutomationEditor => self.handle_automation_editor_pane_key(code, mods),
            InputFocus::AutomationRunHistory => self.handle_automation_run_history_key(code),
            InputFocus::TaskList => self.handle_task_list_key(code),
            InputFocus::TaskEditor => self.handle_task_editor_pane_key(code, mods),
            // The global-search popup captures input earlier (before the global
            // keybinding lookup), so this arm is effectively unreachable.
            InputFocus::GlobalSearch => self.handle_global_search_key(code, mods),
            InputFocus::Terminal => self.handle_terminal_key(code, mods),
            InputFocus::FileViewer => self.handle_file_viewer_key(code, mods),
            // The code-review view, its changed-files list, and the activity
            // view (transcript + tree) capture input earlier (before the global
            // keybinding lookup), so these arms are effectively unreachable.
            InputFocus::CodeReview
            | InputFocus::ReviewFiles
            | InputFocus::CcActivity
            | InputFocus::CcActivityTree => {}
        }
    }

    /// The keybinding [`KeyContext`](crate::session::KeyContext) for the
    /// current focus, used to scope the lookup. Single-letter scoped actions
    /// (file viewer / session list) only resolve here, so the terminal keeps
    /// forwarding those keys to the PTY. While the file-viewer search field is
    /// active we fall back to `Global` so typed letters edit the query instead
    /// of navigating.
    pub(crate) fn focus_key_context(&self) -> crate::session::KeyContext {
        use crate::session::KeyContext;
        match self.focus {
            InputFocus::SessionList => KeyContext::SessionList,
            InputFocus::Automations => KeyContext::Automations,
            InputFocus::TaskList => KeyContext::Tasks,
            InputFocus::FileViewer if !self.file_viewer.search_active => KeyContext::FileViewer,
            InputFocus::Terminal => KeyContext::Terminal,
            // The editor / run-history focuses are capture sub-modes handled
            // before the lookup, so they stay on Global here.
            _ => KeyContext::Global,
        }
    }

    /// Whether a text-entry submode currently owns every keystroke, so the
    /// leader must not steal from it: in a field being typed into, the leader
    /// chord is a line-editing key (`Ctrl+A` is beginning-of-line) and the
    /// user is composing text, not issuing commands.
    ///
    /// Modals and the global-search popup are already handled before the
    /// leader runs, so this only needs to cover the in-pane editors and the
    /// search/compose sub-modes of the capture panes.
    fn text_entry_owns_keys(&self) -> bool {
        if matches!(
            self.focus,
            InputFocus::AutomationEditor | InputFocus::TaskEditor
        ) {
            return true;
        }
        if self.focus == InputFocus::FileViewer && self.file_viewer.search_active {
            return true;
        }
        let review_typing = self.active_review().is_some_and(|cr| {
            cr.compose.is_some() || cr.search.as_ref().is_some_and(|s| s.editing)
        });
        if review_typing {
            return true;
        }
        self.active_cc_activity()
            .is_some_and(|cc| cc.search.as_ref().is_some_and(|s| s.editing))
    }

    /// The tmux-style leader key. Returns `true` if the key was consumed.
    ///
    /// Two jobs, split by [`PrefixState`](super::PrefixState):
    /// - **Idle** — arm on the leader chord (or the second leader), consuming
    ///   it. Any other key is left alone for the handlers below.
    /// - **Armed** — resolve this key against the leader table. Every path
    ///   disarms first, so no branch can leave the leader stuck armed.
    ///
    /// Order matters inside the armed branch: the leader itself sends its own
    /// byte (so the inner agent can still receive it), then cancel, then
    /// digits (session selection is the feature the leader exists for), then
    /// the action table. An unrecognised key reports rather than falling
    /// through to the PTY — a leader press followed by a typo would otherwise
    /// inject a stray character into the agent's prompt.
    fn handle_prefix_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        let leaders = self.prefix_settings.chords();
        if leaders.is_empty() {
            return false;
        }
        let pressed = crate::session::KeyChord::normalized(mods, code);
        let is_leader = leaders.contains(&pressed);

        // Waiting for a move distance: a digit performs the move, anything
        // else cancels. Checked before the arm/dispatch logic so the digit is
        // never mistaken for a session jump.
        if let super::PrefixState::AwaitingMove { up } = self.prefix_state {
            self.prefix_state = super::PrefixState::Idle;
            self.prefix_hint_redraw_requested = false;
            self.request_redraw();
            if let KeyCode::Char(c) = code {
                if let Some(d) = c.to_digit(10).filter(|d| *d > 0) {
                    self.move_active_session_by(d as usize, up);
                    return true;
                }
            }
            return true;
        }

        if !self.prefix_state.is_armed() {
            if is_leader {
                self.prefix_state = super::PrefixState::Armed {
                    since: clock::now(),
                    chord: pressed,
                };
                self.prefix_hint_redraw_requested = false;
                self.request_redraw();
                return true;
            }
            return false;
        }

        self.prefix_state = super::PrefixState::Idle;
        self.prefix_hint_redraw_requested = false;
        self.request_redraw();

        // `<leader> <leader>` → send the leader's own byte to the agent. The
        // tmux `send-prefix` convention; without it the leader chord would be
        // permanently unreachable by the inner CLI (and friring unusable
        // inside itself). Only meaningful with a terminal focused — elsewhere
        // there is nothing to send to, so it just disarms.
        if is_leader {
            if self.focus == InputFocus::Terminal {
                self.handle_terminal_key(code, mods);
            }
            return true;
        }

        // Esc / Ctrl+C back out without running anything.
        if code == KeyCode::Esc || (mods == KeyModifiers::CONTROL && code == KeyCode::Char('c')) {
            return true;
        }

        // `<leader> 1`–`9` — jump to the Nth session in rendered order. Same
        // numbering the Alt-held overlay paints, so the two routes agree.
        if let KeyCode::Char(c) = code {
            if c.is_ascii_digit() {
                self.jump_to_digit(c, false);
                return true;
            }
        }

        // `<leader> K` / `<leader> J` — arm the move gesture and wait for its
        // distance digit, re-numbering the list as distances from the active
        // session.
        for up in [true, false] {
            if pressed == crate::session::keybindings::move_session_key(up) {
                self.prefix_state = super::PrefixState::AwaitingMove { up };
                self.request_redraw();
                return true;
            }
        }

        // Resolve the leader table, accepting the key with Ctrl still held as
        // the same entry. GNU screen ships exactly this: "all commands that are
        // bound to lower-case letters are also bound to their control character
        // counterparts", so `C-f C-b` works as well as `C-f b`. For a key
        // pressed dozens of times an hour, not having to release the modifier
        // mid-sequence is free ergonomics.
        let resolved = crate::session::keybindings::action_for_prefix_key(pressed).or_else(|| {
            let bare = crate::session::KeyChord::normalized(mods & !KeyModifiers::CONTROL, code);
            (bare != pressed)
                .then(|| crate::session::keybindings::action_for_prefix_key(bare))
                .flatten()
        });
        if let Some(action) = resolved {
            self.dispatch_action(action);
            return true;
        }

        self.set_status(
            super::StatusLevel::Info,
            format!("No leader binding for `{}`", pressed.display()),
        );
        true
    }

    /// Help-overlay dismissal and clipboard chords, routed ahead of modal
    /// handlers. Returns `true` if the key was consumed.
    /// Test hook for [`Self::handle_priority_key`]'s consume/fall-through
    /// decision, which is otherwise only observable through a real clipboard.
    #[cfg(test)]
    pub(crate) fn handle_priority_key_for_test(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> bool {
        self.handle_priority_key(code, mods)
    }

    fn handle_priority_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        // The interactive help/keybinding editor captures all input — routed
        // ahead of the global keybinding lookup so a chord being captured
        // (e.g. ctrl+q) rebinds rather than triggering its action (quit).
        if matches!(self.modal, super::modals::Modal::Help(_)) {
            return self.handle_help_key(code, mods);
        }

        // In `prefix-only` a focused terminal keeps its whole `Ctrl` namespace,
        // clipboard chords included: `Ctrl+V` is image-paste in Claude Code and
        // Codex and `Ctrl+C` is their interrupt, so intercepting them here
        // would break the very thing that mode exists to fix. The gate is
        // narrow on purpose — only a focused terminal, so paste still reaches
        // friring's own modals, search field and in-pane editors, which have no
        // other way to receive it (`Copy`/`Paste` deliberately have no leader
        // key). Reaching friring's clipboard *in* the terminal is then the
        // terminal emulator's job (`Cmd+V`/`Ctrl+Shift+V`), as it is for any
        // full-screen TUI.
        let terminal_owns_clipboard =
            !self.prefix_settings.mode.direct_enabled() && self.focus == InputFocus::Terminal;

        // Clipboard chords (Copy/Paste) are user-rebindable global actions but
        // routed here, ahead of modal handlers, so Paste reaches modal/terminal
        // text inputs and Copy works from inside any modal. Resolved via the
        // (global) keybindings so a user's rebind takes effect.
        match self.keybindings.lookup(code, mods) {
            _ if terminal_owns_clipboard => return false,
            // Paste always consumes — `paste_from_clipboard` knows whether a
            // modal text input is open and routes the text accordingly.
            Some(crate::session::Action::Paste) => {
                self.paste_from_clipboard();
                return true;
            }
            // Copy consumes only with an active selection; otherwise it falls
            // through to the normal handlers (e.g. SIGINT when the terminal is
            // focused — see `dispatch_action`'s `Copy` arm).
            Some(crate::session::Action::Copy) if self.text_selection.is_some() => {
                self.copy_selection_to_clipboard();
                return true;
            }
            _ => {}
        }

        false
    }

    /// Interactive F1 help / keybinding editor. Always consumes input while
    /// the help modal is open (returns `true`).
    ///
    /// Navigation mode: `j`/`k` (or arrows) move the selection, `Enter`/`r`
    /// begins capturing a new chord for the selected action, `d` resets the
    /// selected action to its default(s), `Shift+D` resets *all* actions, and
    /// `F1`/`Esc` close the overlay.
    ///
    /// Capture mode: the next keypress (any chord, including `ctrl+q` or `f1`)
    /// becomes the action's sole binding; `Esc` cancels without rebinding.
    pub(super) fn handle_help_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        use crate::session::{Action, KeyChord};

        let actions = Action::rebindable_in_order();
        let super::modals::Modal::Help(ref mut help) = self.modal else {
            return true;
        };

        if help.capturing {
            if code == KeyCode::Esc {
                help.capturing = false;
                return true;
            }
            // Normalize so the toast below shows the stored chord (masked
            // modifiers, canonical Shift+letter) — `rebind` normalizes again
            // for storage.
            let chord = KeyChord::normalized(mods, code);
            let selected = help.selected.min(actions.len().saturating_sub(1));
            help.capturing = false;
            let action = actions[selected];
            let stolen = self.keybindings.rebind(action, chord);
            self.persist_keybindings();
            if let Some(other) = stolen {
                self.set_info(format!(
                    "{} reassigned from '{}'",
                    chord.display(),
                    other.label()
                ));
            }
            return true;
        }

        match code {
            KeyCode::Esc | KeyCode::F(1) => self.modal.close(),
            KeyCode::Char('j') | KeyCode::Down => {
                help.selected = (help.selected + 1).min(actions.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                help.selected = help.selected.saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('r') => help.capturing = true,
            KeyCode::Char('d') => {
                let selected = help.selected.min(actions.len().saturating_sub(1));
                let action = actions[selected];
                self.keybindings.reset(action);
                self.persist_keybindings();
            }
            // Shift+D resets every action to its built-in default.
            KeyCode::Char('D') => self.reset_all_keybindings(),
            _ => {}
        }
        true
    }

    /// Restore every keybinding to its compiled-in default and remove the
    /// user override file so defaults remain authoritative. Surfaces failures
    /// via the status bar; the in-memory map is reset regardless.
    fn reset_all_keybindings(&mut self) {
        self.keybindings = crate::session::KeyBindings::default();
        if let Err(e) = crate::storage::keybindings::delete_keybindings_json() {
            self.set_error(format!("Failed to reset keybindings: {e}"));
        } else {
            self.set_info("All keybindings reset to defaults");
        }
        self.mark_keybindings_saved();
    }

    /// Serialize the current keybindings and write them to
    /// `~/.config/friring/keybindings.json`. Surfaces failures via the status
    /// bar rather than aborting — the in-memory map is already updated.
    fn persist_keybindings(&mut self) {
        match self.keybindings.to_json() {
            Ok(json) => {
                if let Err(e) = crate::storage::keybindings::save_keybindings_json(&json) {
                    self.set_error(format!("Failed to save keybindings: {e}"));
                }
            }
            Err(e) => self.set_error(format!("Failed to serialize keybindings: {e}")),
        }
        self.mark_keybindings_saved();
    }

    /// Route the key to the open modal's handler, if any. Returns `true` if a
    /// modal was open and consumed the key.
    pub(super) fn handle_modal_key_if_open(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        use super::modals::Modal;
        // Pressing a modal's own opener chord again dismisses it (toggle),
        // mirroring the help overlay's F1-to-close. Routed as an `Esc` so each
        // modal keeps its own dismissal semantics — the theme picker restores
        // its live-preview original, the Settings panel discards its draft.
        if self.modal_opener_pressed(code, mods) {
            return self.handle_modal_key_if_open(KeyCode::Esc, KeyModifiers::NONE);
        }
        match self.modal {
            Modal::RestoreSessions(_) => self.handle_restore_sessions_key(code),
            Modal::BranchSelector(_) => self.handle_branch_selector_key(code, mods),
            Modal::SyncBasePicker(_) => self.handle_sync_base_picker_key(code),
            Modal::WorktreeName(_) => self.handle_worktree_name_key(code, mods),
            Modal::SessionName(_) => self.handle_session_name_key(code, mods),
            Modal::AutomationEditor(_) => self.handle_automation_editor_key(code, mods),
            Modal::AutomationsList(_) => self.handle_automations_list_key(code),
            Modal::AgentPicker(_) => self.handle_agent_picker_key(code, mods),
            Modal::HostPicker(_) => self.handle_host_picker_key(code, mods),
            Modal::ThemePicker(_) => self.handle_theme_picker_key(code),
            Modal::RepoPicker(_) => self.handle_repo_picker_key(code, mods),
            Modal::ConversationPicker(_) => self.handle_conversation_picker_key(code, mods),
            Modal::TaskActionPicker(_) => self.handle_task_action_picker_key(code),
            Modal::ConfirmDelete(_) => self.handle_confirm_delete_key(code),
            Modal::ConfirmRestore(_) => self.handle_confirm_restore_key(code),
            Modal::Settings(_) => self.handle_settings_key(code, mods),
            _ => return false,
        }
        true
    }

    /// Whether `code`/`mods` resolves to the action that *opens* the currently
    /// open modal — i.e. the user re-pressed its toggle (e.g. `F4`/`Ctrl+Y`
    /// with the theme picker open, `F6`/`Ctrl+,` with Settings open). Only the
    /// self-toggling overlays opt in; the rest are dismissed only via `Esc`.
    fn modal_opener_pressed(&self, code: KeyCode, mods: KeyModifiers) -> bool {
        use super::modals::Modal;
        let Some(action) = self.keybindings.lookup(code, mods) else {
            return false;
        };
        match self.modal {
            Modal::ThemePicker(_) => action == crate::session::Action::OpenThemePicker,
            Modal::Settings(_) => action == crate::session::Action::OpenSettings,
            _ => false,
        }
    }

    /// Drive the Settings panel: edits a working copy, persists on `Ctrl+S`,
    /// discards on `Esc` (no live preview to revert).
    fn handle_settings_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let super::modals::Modal::Settings(ref mut m) = self.modal else {
            return;
        };
        match m.handle_key(code, mods) {
            super::modals::EditorOutcome::Continue => {}
            super::modals::EditorOutcome::Save => self.submit_settings_panel(),
            super::modals::EditorOutcome::Cancel => self.modal.close(),
        }
    }

    /// Drive the hard-delete confirmation prompt: `Enter`/`y` tears the session
    /// down, `Esc`/`n` cancels.
    fn handle_confirm_delete_key(&mut self, code: KeyCode) {
        let super::modals::Modal::ConfirmDelete(ref cd) = self.modal else {
            return;
        };
        match code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                let session_id = cd.session_id;
                self.modal.close();
                self.confirm_hard_delete_session(session_id);
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.modal.close();
            }
            _ => {}
        }
    }

    /// Drive the best-effort restore confirmation for a force-deleted session:
    /// `Enter`/`y` restores (committed branch state only), `Esc`/`n` cancels.
    fn handle_confirm_restore_key(&mut self, code: KeyCode) {
        let super::modals::Modal::ConfirmRestore(ref cr) = self.modal else {
            return;
        };
        match code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                let deleted = cr.deleted.clone();
                self.modal.close();
                self.restore_deleted_session(deleted);
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.modal.close();
            }
            _ => {}
        }
    }

    /// Drive the trigger-time task action picker: `j`/`k` (or arrows) select,
    /// `Enter` runs the chosen action, `Esc` closes.
    fn handle_task_action_picker_key(&mut self, code: KeyCode) {
        let super::modals::Modal::TaskActionPicker(ref mut p) = self.modal else {
            return;
        };
        match code {
            KeyCode::Esc => self.modal.close(),
            KeyCode::Char('j') | KeyCode::Down if p.selected + 1 < p.choices.len() => {
                p.selected += 1;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                p.selected = p.selected.saturating_sub(1);
            }
            KeyCode::Enter => self.confirm_task_action_picker(),
            _ => {}
        }
    }

    /// `Enter` on the task action picker: run the selected Send/Spawn choice for
    /// the task being acted on, then close the picker.
    fn confirm_task_action_picker(&mut self) {
        use super::modals::TaskActionChoice;
        let super::modals::Modal::TaskActionPicker(ref p) = self.modal else {
            return;
        };
        let Some(choice) = p.choices.get(p.selected).cloned() else {
            self.modal.close();
            return;
        };
        let task_id = p.task_id;
        let title = p.title.clone();
        let status = self
            .task_ui
            .cached_tasks
            .iter()
            .find(|t| t.id == task_id)
            .map(|t| t.status)
            .unwrap_or_default();
        self.modal.close();
        match choice {
            TaskActionChoice::Send(session_id, _) => {
                self.send_task_to_session(task_id, &title, status, session_id);
            }
            TaskActionChoice::SpawnNew => {
                // Reuse the normal new-session flow (host picker included);
                // the prompt is delivered + the task advanced when the spawn
                // lands. Going through the wizard entry also clears a backend
                // left over from a previously cancelled remote flow, which
                // would otherwise silently make this picker remote.
                self.task_ui.pending_task_prompt = Some((task_id, title));
                self.start_new_session();
            }
        }
    }

    /// The ordered focus ring for the **current context**. `Ctrl+L`/`Ctrl+H`
    /// cycle *within* this ring; switching between the session and automation
    /// contexts is done with `j`/`k` in the left column (not the focus cycle).
    ///
    /// - Session context: `SessionList → Terminal` (+ `TaskList` then
    ///   `FileViewer` when those panels are shown — each is a cycle stop while
    ///   visible, exactly like the file viewer).
    /// - Automation context: `Automations → editor` (+ `run history` for an
    ///   existing automation).
    ///
    /// So cycling out of the automation editor/history wraps back to the
    /// **Automations** pane (returning to the selected automation, like `Esc`),
    /// never off to a session.
    fn focus_ring(&self) -> Vec<InputFocus> {
        use InputFocus::*;
        match self.focus {
            Automations | AutomationEditor | AutomationRunHistory => {
                let mut ring = vec![Automations, AutomationEditor];
                // The run-history panel exists only for an existing automation.
                if self.scoped_automation_id().is_some() {
                    ring.push(AutomationRunHistory);
                }
                ring
            }
            // The tasks panel joins the session ring so `Ctrl+L`/`Ctrl+H` move
            // in and out of it like any other pane (it lives in the right column,
            // not the left-column circular list). `Esc` still drops straight back
            // to the session list.
            SessionList | Terminal | FileViewer | TaskList | CodeReview | ReviewFiles
            | CcActivity | CcActivityTree => {
                // Order mirrors the on-screen columns: central → tasks → files.
                // The central pane is a mutually-exclusive overlay when the active
                // session has one open (persisted per session, like the shell
                // view) — the code review or the activity view — else the
                // terminal. So `Ctrl+L`/`Ctrl+H` move in and out of the overlay
                // like the terminal, and `Ctrl+H` to the session list keeps it
                // open.
                let review = self.active_review().is_some();
                let cc = self.active_cc_activity().is_some();
                let central = if review {
                    CodeReview
                } else if cc {
                    CcActivity
                } else {
                    Terminal
                };
                let mut ring = vec![SessionList, central];
                if self.show_tasks_panel {
                    ring.push(TaskList);
                }
                // While an overlay is open the file-viewer column shows its nav
                // list (forced visible, see `layout_for`): the review's
                // changed-files list, or the activity view's tree; otherwise it's
                // the file viewer when that panel is toggled on.
                if review {
                    ring.push(ReviewFiles);
                } else if cc {
                    ring.push(CcActivityTree);
                } else if self.show_file_viewer {
                    ring.push(FileViewer);
                }
                ring
            }
            // While editing a task in the central pane, the ring is
            // `TaskList → editor` (like the automation editor): cycling out of
            // the editor returns to the tasks panel, never off to a session.
            TaskEditor => vec![TaskList, TaskEditor],
            // The global-search popup is entered/left only via its keybinding
            // (`Ctrl+/` by default) / `Esc`, so `Ctrl+L`/`Ctrl+H` are no-ops
            // while it's open.
            GlobalSearch => vec![GlobalSearch],
        }
    }

    /// Cycle focus forward (Ctrl+L) within the current context's ring.
    pub(crate) fn cycle_focus_forward(&self) -> InputFocus {
        let ring = self.focus_ring();
        let pos = ring.iter().position(|f| *f == self.focus).unwrap_or(0);
        ring[(pos + 1) % ring.len()]
    }

    /// Cycle focus backward (Ctrl+H) within the current context's ring.
    pub(crate) fn cycle_focus_backward(&self) -> InputFocus {
        let ring = self.focus_ring();
        let pos = ring.iter().position(|f| *f == self.focus).unwrap_or(0);
        ring[(pos + ring.len() - 1) % ring.len()]
    }

    /// Shared bookkeeping after a focus change via the `Ctrl+L`/`Ctrl+H` cycle:
    /// keep the in-pane editor + run history in sync, and start the run-history
    /// selection at the top (newest run) when entering that panel.
    pub(super) fn on_focus_changed(&mut self) {
        if self.focus == InputFocus::AutomationRunHistory {
            self.automation_ui.automation_run_index = 0;
        }
        // Entering the tasks panel via the cycle: refresh the task list and the
        // in-pane editor preview.
        if self.focus == InputFocus::TaskList {
            self.refresh_tasks();
        }
        if matches!(self.focus, InputFocus::TaskList | InputFocus::TaskEditor) {
            self.refresh_task_view();
        }
        self.refresh_automation_view();
    }

    /// Handle keys while editing the scoped task in the central pane. `Enter`
    /// saves and returns to the tasks panel; `Esc` discards and returns. Field
    /// navigation is the `TaskEditorModal`'s own. Mirrors
    /// `handle_automation_editor_pane_key`.
    pub(crate) fn handle_task_editor_pane_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(editor) = self.task_ui.task_editor.as_mut() else {
            match code {
                KeyCode::Char('n') => self.new_task_in_pane(),
                KeyCode::Esc => self.focus = InputFocus::TaskList,
                _ => {}
            }
            return;
        };
        match editor.handle_key(code, mods) {
            super::modals::EditorOutcome::Continue => {}
            super::modals::EditorOutcome::Save => {
                let Some(editor) = self.task_ui.task_editor.clone() else {
                    return;
                };
                if self.save_task(&editor) {
                    // A brand-new task lands at the top of the list.
                    if editor.editing_id.is_none() {
                        self.task_ui.task_panel_index = 0;
                    }
                    self.focus = InputFocus::TaskList;
                    self.refresh_task_view();
                }
            }
            super::modals::EditorOutcome::Cancel => {
                // Discard edits and restore the preview for the selection.
                self.focus = InputFocus::TaskList;
                self.refresh_task_view();
            }
        }
    }

    fn handle_file_viewer_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if self.file_viewer.search_active {
            self.handle_file_viewer_search_key(code, mods);
        } else {
            self.handle_file_viewer_nav_key(code);
        }
    }

    fn handle_file_viewer_search_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Esc => self.file_viewer.end_search(),
            // Enter/Down cycle to next match and stay in search mode.
            // Tab commits and exits search mode.
            KeyCode::Enter | KeyCode::Down => self.file_viewer.next_match(),
            KeyCode::Up => self.file_viewer.prev_match(),
            KeyCode::Tab => self.file_viewer.search_active = false,
            KeyCode::Char('n') if ctrl => self.file_viewer.next_match(),
            KeyCode::Char('p') if ctrl => self.file_viewer.prev_match(),
            KeyCode::Backspace => self.file_viewer.search_pop(),
            KeyCode::Char(c) if !ctrl => self.file_viewer.search_push(c),
            _ => {}
        }
    }

    fn handle_file_viewer_nav_key(&mut self, code: KeyCode) {
        // Navigation/search/expand keys are rebindable `FileViewer`-scoped
        // actions, resolved by the context lookup in `handle_key` before this
        // runs. Only the fixed "Esc clears an active query" shortcut remains.
        if matches!(code, KeyCode::Esc) && !self.file_viewer.search_query.is_empty() {
            self.file_viewer.end_search();
        }
    }

    fn open_file_in_editor(&mut self, root: std::path::PathBuf, file: std::path::PathBuf) {
        let Some(editor) = super::helpers::resolve_editor_command(&self.db) else {
            self.set_error(super::EDITOR_NOT_CONFIGURED);
            return;
        };
        if let Err(e) = super::helpers::open_in_editor(&[root, file], &editor) {
            warn!("file viewer: failed to open editor: {e}");
        }
    }

    /// The session jump-overlay keys (see `App::jump_overlay_blocked_only`).
    /// Returns `true` when the key was consumed.
    ///
    /// - While the blocked-only overlay is open: a digit jumps to that
    ///   blocked session, `Esc` dismisses, the toggle chord falls through to
    ///   the lookup (which flips the overlay off), and any other key
    ///   dismisses a *sticky* overlay without being consumed — stray typing
    ///   acts normally instead of being hijacked.
    /// - Otherwise `Alt+<digit>` jumps by the all-session numbering (works
    ///   blind on legacy terminals that can't show the hold overlay).
    fn handle_session_jump_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        use super::BlockedJumpMode;
        let plain_or_alt = mods.is_empty() || mods == KeyModifiers::ALT;
        if let Some(mode) = self.blocked_jump {
            match code {
                KeyCode::Char(c @ '1'..='9') if plain_or_alt => {
                    self.blocked_jump = None;
                    self.jump_to_digit(c, true);
                    return true;
                }
                KeyCode::Esc => {
                    self.blocked_jump = None;
                    return true;
                }
                _ => {
                    let is_toggle = self.keybindings.lookup(code, mods)
                        == Some(crate::session::Action::JumpToBlocked);
                    if !is_toggle && mode == BlockedJumpMode::Sticky {
                        self.blocked_jump = None;
                    }
                    return false;
                }
            }
        }
        if mods == KeyModifiers::ALT {
            if let KeyCode::Char(c @ '1'..='9') = code {
                self.jump_to_digit(c, false);
                return true;
            }
        }
        false
    }

    /// Session-list keys are all rebindable `SessionList`-scoped actions
    /// (`SessionListNext`/`Prev`/`Open`), resolved by the context lookup in
    /// `handle_key` before this runs. Only the fixed `Esc` escape hatch lives
    /// here (literal, like the other panes' Esc): the list is a transient
    /// "manage" surface, so backing out of it must never cost more than one
    /// keystroke. No-op with no sessions — the terminal would be a dead end.
    pub(crate) fn handle_session_list_key(&mut self, code: KeyCode) {
        if code == KeyCode::Esc && !self.sessions.is_empty() {
            self.focus = InputFocus::Terminal;
        }
    }

    fn handle_terminal_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        // Terminal scroll is handled by the rebindable `TerminalScroll*`
        // actions in `handle_key` before this runs; everything else snaps to
        // the bottom and is forwarded to the PTY.

        self.with_active_parser(|parser| {
            if parser.screen().scrollback() > 0 {
                parser.screen_mut().set_scrollback(0);
            }
        });

        // A placeholder (unreachable remote) has no live pane; swallow the
        // keystroke and hint how to recover instead of silently dropping it.
        if self
            .sessions
            .get(self.active_index)
            .is_some_and(|s| s.is_placeholder())
        {
            let host = self
                .sessions
                .get(self.active_index)
                .and_then(|s| s.info.remote_host.clone())
                .unwrap_or_else(|| "?".into());
            self.set_status(
                super::StatusLevel::Info,
                format!("Session unreachable — host '{host}' is offline (restart to retry)"),
            );
            return;
        }

        if let Some(session) = self.sessions.get(self.active_index) {
            if let Some(bytes) = input::key_to_bytes(code, mods) {
                let result = if let (TerminalView::Shell, Some(shell)) =
                    (self.active_terminal_view(), &session.shell_pane)
                {
                    shell.send_input(bytes)
                } else {
                    session.send_input(bytes)
                };
                if let Err(e) = result {
                    error!("Failed to send input: {e}");
                }
            }
        }
    }

    fn handle_restore_sessions_key(&mut self, code: KeyCode) {
        let super::modals::Modal::RestoreSessions(ref mut rs) = self.modal else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.modal.close();
            }
            KeyCode::Char('j') | KeyCode::Down
                if !rs.list.is_empty() && rs.index + 1 < rs.list.len() =>
            {
                rs.index += 1;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                rs.index = rs.index.saturating_sub(1);
            }
            KeyCode::Enter => {
                if rs.list.is_empty() {
                    return;
                }
                let deleted = rs.list.remove(rs.index);
                if rs.index >= rs.list.len() && rs.index > 0 {
                    rs.index -= 1;
                }
                // A force-deleted session lost its uncommitted work; confirm the
                // best-effort recovery first. Plain soft-deletes restore directly.
                if deleted.force_deleted {
                    self.modal =
                        super::modals::Modal::ConfirmRestore(super::modals::ConfirmRestoreModal {
                            deleted,
                        });
                } else {
                    self.modal.close();
                    self.restore_deleted_session(deleted);
                }
            }
            _ => {}
        }
    }

    /// Type-to-filter selector: printable keys edit the fuzzy query (so `j`/`k`
    /// type, they don't navigate — arrows and Ctrl+N/P move the cursor), and
    /// Esc clears an active query before it closes the modal.
    fn handle_branch_selector_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let super::modals::Modal::BranchSelector(ref mut bs) = self.modal else {
            return;
        };
        let visible = bs.filter.len(bs.branches.len());
        match code {
            KeyCode::Esc if bs.filter.is_active() => bs.filter.clear(&mut bs.index),
            KeyCode::Esc => {
                self.modal.close();
                // These are re-derived when the palette re-submits; dropping
                // the parked origin-fetch signal here keeps the ADR-P12
                // abort-site rule (no worktree create may consume a stale one —
                // re-submitting re-arms it with a fresh channel).
                self.new_session.repo_path = None;
                self.new_session.all_repos = None;
                self.new_session.normal_repos.clear();
                self.new_session.base_branch = None;
                self.new_session.workspace_dir = None;
                self.new_session.fetch_done = None;
                // Back one step: the palette as the user left it.
                self.restore_repo_picker();
            }
            KeyCode::Down if bs.index + 1 < visible => bs.index += 1,
            KeyCode::Up => bs.index = bs.index.saturating_sub(1),
            // Inert until the background load delivers (ADR-P12) — there is no
            // branch to select yet. A query with no matches is likewise inert.
            KeyCode::Enter if !bs.loading => {
                let Some(real) = bs.filter.real_index(bs.index, bs.branches.len()) else {
                    return;
                };
                let base_branch = bs.branches[real].clone();
                // Records the base branch AND prefills the name step from the
                // repo basename (the worktree branch name derives from it).
                self.confirm_branch_selection(base_branch);
            }
            KeyCode::Backspace => bs.filter.pop(&bs.branches, &mut bs.index),
            KeyCode::Char('n') if mods.contains(KeyModifiers::CONTROL) => {
                if bs.index + 1 < visible {
                    bs.index += 1;
                }
            }
            KeyCode::Char('p') if mods.contains(KeyModifiers::CONTROL) => {
                bs.index = bs.index.saturating_sub(1);
            }
            KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                bs.filter.push(c, &bs.branches, &mut bs.index);
            }
            _ => {}
        }
    }

    /// Drive the sync base picker (`Ctrl+S` with a multi-remote repo): Enter
    /// picks the highlighted remote (persisted as the repo's default) and the
    /// parked sync run advances; Esc drops the whole run.
    fn handle_sync_base_picker_key(&mut self, code: KeyCode) {
        let super::modals::Modal::SyncBasePicker(ref mut sb) = self.modal else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.modal.close();
                self.cancel_sync_base();
            }
            KeyCode::Char('j') | KeyCode::Down if sb.index + 1 < sb.remotes.len() => {
                sb.index += 1;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                sb.index = sb.index.saturating_sub(1);
            }
            KeyCode::Enter if !sb.remotes.is_empty() => {
                let remote = sb.remotes[sb.index].clone();
                self.modal.close();
                self.confirm_sync_base(remote);
            }
            _ => {}
        }
    }

    /// `Enter` on the branch selector: record the base branch and advance to
    /// the name step, prefilled from the repo (worktree flow — the name also
    /// seeds the branch name).
    fn confirm_branch_selection(&mut self, base_branch: String) {
        self.new_session.base_branch = Some(base_branch);
        let mut modal = super::modals::SessionNameModal::default();
        let cwd = self.new_session.repo_path.clone();
        modal.name.set(&self.suggested_session_name(cwd.as_deref()));
        self.prefill_workspace_dir_field(&mut modal);
        self.modal = super::modals::Modal::SessionName(modal);
    }

    fn handle_worktree_name_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let super::modals::Modal::WorktreeName(ref mut wn) = self.modal else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.modal.close();
                self.worktree_name_back();
            }
            KeyCode::Enter => {
                let new_branch = wn.name.value().trim().to_string();
                if new_branch.is_empty() {
                    self.set_error("Branch name cannot be empty");
                    return;
                }
                self.modal.close();
                self.confirm_worktree_name(&new_branch);
            }
            other => {
                super::modals::apply_text_input_key(Some(&mut wn.name), other, mods);
            }
        }
    }

    /// `Esc` on the branch-name modal: back to the session-name step (the name
    /// seeds the branch, so it's the natural place to edit). All worktree-flow
    /// pending state (base branch, repos, fetch signal) stays armed for the
    /// re-confirm.
    fn worktree_name_back(&mut self) {
        let mut modal = super::modals::SessionNameModal::default();
        if let Some(name) = self.new_session.session_name.take() {
            modal.name.set(&name);
        }
        self.prefill_workspace_dir_field(&mut modal);
        self.modal = super::modals::Modal::SessionName(modal);
    }

    /// Spawn the worktree session for the confirmed branch name.
    fn confirm_worktree_name(&mut self, new_branch: &str) {
        let Some(base_branch) = self.new_session.base_branch.take() else {
            return;
        };
        // Use all repos for multi-repo projects, single repo otherwise
        let repo_paths = if let Some(all_repos) = self.new_session.all_repos.take() {
            self.new_session.repo_path = None;
            all_repos
        } else if let Some(repo_path) = self.new_session.repo_path.take() {
            vec![repo_path]
        } else {
            return;
        };
        let session_name = self.new_session.session_name.take();
        self.spawn_worktree_session(&repo_paths, new_branch, &base_branch, session_name);
    }

    fn handle_session_name_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        // `Ctrl+O` toggles the optional workspace-dir field. Matched before the
        // text-input fallthrough, which swallows every Ctrl+letter chord.
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('o') | KeyCode::Char('O'))
        {
            self.toggle_workspace_dir_field();
            return;
        }
        let super::modals::Modal::SessionName(ref mut sn) = self.modal else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.modal.close();
                self.session_name_back();
            }
            KeyCode::Tab | KeyCode::BackTab if sn.workspace_dir.is_some() => {
                sn.workspace_focused = !sn.workspace_focused;
            }
            KeyCode::Enter => {
                let name = sn.name.value().trim().to_string();
                let ws_raw = sn.workspace_dir.as_ref().map(|f| f.value().to_string());
                if name.is_empty() {
                    self.set_error("Session name cannot be empty");
                    return;
                }
                // Resolve + validate the workspace dir before committing, so a
                // bad value keeps the modal open with everything editable.
                if let Some(raw) = ws_raw {
                    let dir = match crate::workspace::resolve_custom_workspace_dir(&raw) {
                        Ok(dir) => dir,
                        Err(e) => {
                            self.set_error(e);
                            return;
                        }
                    };
                    if let Some(dir) = &dir {
                        if let Err(e) = crate::workspace::validate_custom_workspace_dir(dir) {
                            self.set_error(e);
                            return;
                        }
                    }
                    self.new_session.workspace_dir = dir;
                }
                self.modal.close();
                self.confirm_session_name(name);
            }
            other => {
                let field = match sn.workspace_dir.as_mut() {
                    Some(ws) if sn.workspace_focused => Some(ws),
                    _ => Some(&mut sn.name),
                };
                super::modals::apply_text_input_key(field, other, mods);
            }
        }
    }

    /// `Ctrl+O` on the name modal: show + focus the optional workspace-dir
    /// field, or hide it again (reverting to the default id-derived
    /// workspace). A no-op when the pending spawn doesn't offer the field
    /// (single-repo, or a remote host).
    fn toggle_workspace_dir_field(&mut self) {
        let offered = self.pending_spawn_offers_workspace_dir();
        let prefill = self
            .new_session
            .workspace_dir
            .as_ref()
            .map(|dir| crate::paths::display_path_tilde(dir));
        let super::modals::Modal::SessionName(ref mut sn) = self.modal else {
            return;
        };
        if sn.workspace_dir.is_some() {
            sn.workspace_dir = None;
            sn.workspace_focused = false;
            self.new_session.workspace_dir = None;
            return;
        }
        if !offered {
            return;
        }
        let mut field = super::modals::TextInput::new();
        if let Some(prefill) = prefill {
            field.set(&prefill);
        }
        sn.workspace_dir = Some(field);
        sn.workspace_focused = true;
    }

    /// `Esc` on the name modal: step back to wherever this flow came from —
    /// or cancel when there is no prior step to return to.
    fn session_name_back(&mut self) {
        // Fork has no prior step — Esc cancels as before. Checked first: a
        // fork of a worktree session pre-seeds `spawn_worktrees` with the
        // *source's* worktrees, which must not read as "created" below.
        if self.new_session.fork {
            self.new_session.spawn_config = None;
            self.new_session.spawn_worktrees.clear();
            self.new_session.fork = false;
            self.new_session.parent_session_id = None;
            self.new_session.workspace_dir = None;
            return;
        }
        // Worktrees already created (the agent picker stepped back here, or a
        // create completed earlier): stepping further back can't un-create
        // them, so this stays a cancel — re-running the create would collide
        // on `git worktree add -b`.
        if !self.new_session.spawn_worktrees.is_empty() {
            self.new_session.spawn_config = None;
            self.new_session.spawn_worktrees.clear();
            self.new_session.import = false;
            self.new_session.parent_session_id = None;
            self.new_session.additional_dirs.clear();
            self.new_session.workspace_dir = None;
            self.new_session.saved_repo_picker = None;
            self.set_info("Cancelled — created worktree(s) kept on disk");
            return;
        }
        // Worktree flow → back to the branch selector. A fresh dispatch
        // re-arms the branch load and the origin fetch (ADR-P12: the previous
        // `fetch_done` was already consumed or is safely overwritten); the
        // previously chosen base stays highlighted via `poll_branch_load`.
        if self.new_session.base_branch.is_some() {
            self.start_branch_selection();
            return;
        }
        // Import flow → back to the conversation picker's directory step
        // (re-confirming re-stages the transcript, which is idempotent).
        if self.new_session.import {
            self.new_session.import = false;
            self.new_session.spawn_config = None;
            if let Some(cp) = self.new_session.saved_conversation_picker.take() {
                self.modal = super::modals::Modal::ConversationPicker(*cp);
            }
            return;
        }
        // Normal flow → back to the repo palette. `spawn_session_with_config`
        // consumed the backend; restore it so bookmarks stay host-scoped, and
        // drop the derived `additional_dirs` (the re-submit recomputes them —
        // leaving them would leak stale dirs into the next spawn).
        if let Some(config) = self.new_session.spawn_config.take() {
            self.new_session.backend = config.backend;
        }
        self.new_session.additional_dirs.clear();
        self.new_session.workspace_dir = None;
        self.new_session.parent_session_id = None;
        self.restore_repo_picker();
    }

    /// Advance from the confirmed session name to the next step of the flow.
    fn confirm_session_name(&mut self, name: String) {
        if self.new_session.base_branch.is_some() {
            // Worktree flow — proceed to branch name input.
            let branch = session_name_to_branch(&name);
            self.new_session.session_name = Some(name);
            let mut modal = super::modals::WorktreeNameModal::default();
            modal.name.set(&branch);
            self.modal = super::modals::Modal::WorktreeName(modal);
        } else if let Some(config) = self.new_session.spawn_config.take() {
            let worktrees = std::mem::take(&mut self.new_session.spawn_worktrees);
            if self.new_session.fork || self.new_session.import {
                // Fork / conversation-import flow — agent already set on the
                // config, spawn directly.
                self.new_session.fork = false;
                self.new_session.import = false;
                self.do_spawn_session_async(name, &config, worktrees);
            } else {
                // Normal flow — proceed to role selection / spawn.
                self.finish_prepare_spawn(name, config, worktrees);
            }
        }
    }

    /// Type-to-filter selector — same keymap as
    /// [`Self::handle_branch_selector_key`].
    fn handle_host_picker_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let super::modals::Modal::HostPicker(ref mut hp) = self.modal else {
            return;
        };
        let choice_count = hp.choices.len();
        let visible = hp.filter.len(choice_count);
        match code {
            KeyCode::Esc if hp.filter.is_active() => hp.filter.clear(&mut hp.selected_index),
            KeyCode::Esc => {
                self.modal.close();
                self.new_session.backend = None;
            }
            KeyCode::Down if hp.selected_index + 1 < visible => hp.selected_index += 1,
            KeyCode::Up => hp.selected_index = hp.selected_index.saturating_sub(1),
            // Inert on a query with no matches (backspace to widen it).
            KeyCode::Enter => {
                let Some(real) = hp.filter.real_index(hp.selected_index, choice_count) else {
                    return;
                };
                let backend = hp
                    .choices
                    .get(real)
                    .map(|c| c.backend.clone())
                    .unwrap_or_default();
                self.modal.close();
                self.confirm_host_picker(backend);
            }
            KeyCode::Backspace => {
                let labels = hp.choices.iter().map(|c| c.label.as_str());
                hp.filter.pop(labels, &mut hp.selected_index);
            }
            KeyCode::Char('n') if mods.contains(KeyModifiers::CONTROL) => {
                if hp.selected_index + 1 < visible {
                    hp.selected_index += 1;
                }
            }
            KeyCode::Char('p') if mods.contains(KeyModifiers::CONTROL) => {
                hp.selected_index = hp.selected_index.saturating_sub(1);
            }
            KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                let labels = hp.choices.iter().map(|c| c.label.as_str());
                hp.filter.push(c, labels, &mut hp.selected_index);
            }
            _ => {}
        }
    }

    /// `Enter` on the host picker: record the chosen backend (empty == local
    /// default) and advance to the repo picker. The backend is passed in to
    /// avoid re-borrowing the modal after it's closed.
    fn confirm_host_picker(&mut self, backend: String) {
        // Empty backend == local default.
        self.new_session.backend = if backend.is_empty() {
            None
        } else {
            Some(backend)
        };
        self.open_repo_picker();
    }

    /// Type-to-filter selector — same keymap as
    /// [`Self::handle_branch_selector_key`].
    fn handle_agent_picker_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let super::modals::Modal::AgentPicker(ref mut ap) = self.modal else {
            return;
        };
        let choice_count = ap.choices.len();
        let visible = ap.filter.len(choice_count);
        match code {
            KeyCode::Esc if ap.filter.is_active() => ap.filter.clear(&mut ap.selected_index),
            KeyCode::Esc => {
                self.modal.close();
                self.agent_picker_back();
            }
            KeyCode::Down if ap.selected_index + 1 < visible => ap.selected_index += 1,
            KeyCode::Up => ap.selected_index = ap.selected_index.saturating_sub(1),
            // Inert on a query with no matches (backspace to widen it).
            KeyCode::Enter => {
                let Some(real) = ap.filter.real_index(ap.selected_index, choice_count) else {
                    return;
                };
                let chosen = ap.choices.get(real).map(|c| c.name.clone());
                self.modal.close();
                self.confirm_agent_picker(chosen);
            }
            KeyCode::Backspace => {
                let labels = ap.choices.iter().map(|c| c.label());
                ap.filter.pop(labels, &mut ap.selected_index);
            }
            KeyCode::Char('n') if mods.contains(KeyModifiers::CONTROL) => {
                if ap.selected_index + 1 < visible {
                    ap.selected_index += 1;
                }
            }
            KeyCode::Char('p') if mods.contains(KeyModifiers::CONTROL) => {
                ap.selected_index = ap.selected_index.saturating_sub(1);
            }
            KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                let labels = ap.choices.iter().map(|c| c.label());
                ap.filter.push(c, labels, &mut ap.selected_index);
            }
            _ => {}
        }
    }

    /// `Esc` on the agent picker. With a worktree creation in flight (ADR-P12:
    /// the picker opens *over* the create) this stays a full cancel — stepping
    /// back and re-confirming would re-run `git worktree add -b` against the
    /// branch the in-flight create is already making, a guaranteed collision —
    /// and the pending create is marked cancelled so its result is dropped.
    /// Otherwise it steps back to the name modal; re-confirming there just
    /// re-runs the side-effect-free `finish_prepare_spawn`.
    fn agent_picker_back(&mut self) {
        if self.pending_worktree_create.is_some() {
            self.new_session.spawn_config = None;
            self.new_session.spawn_worktrees.clear();
            self.new_session.spawn_name = None;
            self.new_session.saved_repo_picker = None;
            self.new_session.workspace_dir = None;
            if let Some(pending) = self.pending_worktree_create.as_mut() {
                if matches!(pending.agent_pick, super::AgentPick::Open) {
                    pending.agent_pick = super::AgentPick::Cancelled;
                }
            }
            return;
        }
        let mut modal = super::modals::SessionNameModal::default();
        if let Some(name) = self.new_session.spawn_name.take() {
            modal.name.set(&name);
        }
        self.prefill_workspace_dir_field(&mut modal);
        self.modal = super::modals::Modal::SessionName(modal);
    }

    /// `Enter` on the agent picker: stamp the chosen agent onto the pending
    /// spawn config and launch the session (a no-op if any pending state is
    /// missing). The chosen agent name is passed in to avoid re-borrowing the
    /// modal after it's closed.
    ///
    /// In the worktree flow the picker opens *while* the worktrees are still
    /// being created (ADR-P12), so the spawn config may not exist yet: the
    /// choice is parked on the pending create and `continue_worktree_spawn`
    /// completes the spawn when the worker delivers.
    fn confirm_agent_picker(&mut self, chosen: Option<String>) {
        if let (Some(mut config), Some(name), Some(agent)) = (
            self.new_session.spawn_config.take(),
            self.new_session.spawn_name.take(),
            chosen.clone(),
        ) {
            config.agent = agent;
            let worktrees = std::mem::take(&mut self.new_session.spawn_worktrees);
            self.do_spawn_session_async(name, &config, worktrees);
            return;
        }
        if let (Some(pending), Some(agent)) = (self.pending_worktree_create.as_mut(), chosen) {
            if matches!(pending.agent_pick, super::AgentPick::Open) {
                pending.agent_pick = super::AgentPick::Chosen(agent);
                self.set_info("Creating worktree(s)…");
            }
        }
    }

    /// Gate a feature-flagged action: returns whether the feature is enabled,
    /// surfacing a toast naming the switch when it isn't. Callers still
    /// consume the key either way (a disabled chord must not reach the PTY).
    fn feature_gate(&mut self, enabled: bool, what: &str) -> bool {
        if !enabled {
            self.set_info(format!("{what} is disabled ([features] in settings.toml)"));
        }
        enabled
    }

    /// Run a feature-gated action when its switch is enabled (toasting the
    /// switch name otherwise), and always consume the key (`true`) so a
    /// disabled chord never falls through to the PTY.
    fn gated(&mut self, enabled: bool, what: &str, act: impl FnOnce(&mut Self)) -> bool {
        if self.feature_gate(enabled, what) {
            act(self);
        }
        true
    }

    pub(super) fn dispatch_action(&mut self, action: crate::session::Action) -> bool {
        if let Some(consumed) = self.dispatch_app_action(action) {
            return consumed;
        }
        if let Some(consumed) = self.dispatch_focus_action(action) {
            return consumed;
        }
        if let Some(consumed) = self.dispatch_panel_action(action) {
            return consumed;
        }
        if let Some(consumed) = self.dispatch_clipboard_action(action) {
            return consumed;
        }
        if let Some(consumed) = self.dispatch_session_list_action(action) {
            return consumed;
        }
        self.dispatch_scoped_pane_action(action)
    }

    /// Global app-control actions (quit, new/fork/restart/delete session, sync,
    /// open editor, undo/restore, theme/settings/help). Returns `Some(consumed)`
    /// when `action` is one of these, else `None`.
    fn dispatch_app_action(&mut self, action: crate::session::Action) -> Option<bool> {
        use crate::session::Action;
        let consumed = match action {
            Action::QuitApp => {
                self.should_quit = true;
                true
            }
            Action::ReloadApp => {
                // A normal quit plus the flag: `main` re-execs the on-disk
                // binary after `shutdown()`, and the new image re-adopts the
                // detached sessions on startup.
                self.reload_requested = true;
                self.should_quit = true;
                true
            }
            Action::NewSession => {
                self.act_new_session();
                true
            }
            Action::DeleteSession => self.act_delete_session(),
            Action::OpenInEditor => {
                self.open_active_in_editor();
                true
            }
            Action::StartSync => {
                self.start_sync();
                true
            }
            Action::ForkSession => {
                self.fork_active_session();
                true
            }
            Action::RestartSession => {
                self.restart_active_session();
                true
            }
            Action::UndoDelete => {
                if self.pending_delete.is_some() {
                    self.undo_delete();
                }
                true
            }
            Action::OpenRestoreSessions => {
                self.open_restore_sessions_modal();
                true
            }
            Action::OpenThemePicker => {
                self.open_theme_picker();
                true
            }
            Action::ToggleHelp => {
                self.modal = super::modals::Modal::Help(super::modals::HelpModal::default());
                true
            }
            Action::OpenSettings => {
                self.open_settings_panel();
                true
            }
            _ => return None,
        };
        Some(consumed)
    }

    /// Focus-cycle and inter-session navigation actions. Returns
    /// `Some(consumed)` when `action` is one of these, else `None`.
    fn dispatch_focus_action(&mut self, action: crate::session::Action) -> Option<bool> {
        use crate::session::Action;
        match action {
            Action::FocusBackward => {
                self.focus = self.cycle_focus_backward();
                self.on_focus_changed();
            }
            Action::FocusForward => {
                self.focus = self.cycle_focus_forward();
                self.on_focus_changed();
            }
            Action::NextSession => self.switch_session_forward(),
            Action::PreviousSession => self.switch_session_backward(),
            Action::NextBlockedSession => self.focus_next_blocked(),
            Action::LastSession => self.toggle_last_session(),
            Action::JumpToBlocked => self.toggle_blocked_jump(),
            _ => return None,
        }
        Some(true)
    }

    /// Feature-gated panel/pane toggles (shell, info panel, tasks, file viewer,
    /// global search, automations). Returns `Some(consumed)` when `action` is
    /// one of these, else `None`.
    fn dispatch_panel_action(&mut self, action: crate::session::Action) -> Option<bool> {
        use crate::session::Action;
        let consumed = match action {
            Action::ToggleShell => self.gated(
                self.features.shell_pane,
                "Shell pane",
                Self::toggle_shell_view,
            ),
            Action::ToggleReview => self.gated(
                self.features.code_review,
                "Code review",
                Self::toggle_code_review,
            ),
            Action::ToggleCcActivity => self.gated(
                self.features.cc_activity,
                "Agent activity",
                Self::toggle_cc_activity,
            ),
            Action::OpenAutomations => self.gated(
                self.features.automations,
                "Automations",
                Self::open_automations_list,
            ),
            Action::ToggleInfoPanel => self.gated(self.features.info_panel, "Info panel", |s| {
                s.show_info_panel = !s.show_info_panel;
                s.resize_sessions_to_content_area();
            }),
            Action::FocusTasks => {
                self.gated(self.features.tasks, "Tasks panel", Self::act_toggle_tasks)
            }
            Action::ToggleFileViewer => self.gated(
                self.features.file_viewer,
                "File viewer",
                Self::act_toggle_file_viewer,
            ),
            Action::GlobalSearch => self.gated(
                self.features.global_search,
                "Global search",
                Self::open_global_search,
            ),
            Action::TogglePerfHud => self.gated(self.features.perf_hud, "Perf HUD", |s| {
                s.show_perf_hud = !s.show_perf_hud;
                s.request_redraw();
            }),
            _ => return None,
        };
        Some(consumed)
    }

    /// Clipboard actions (Copy/Paste). Copy prefers the active selection; with no
    /// selection it copies the current status-bar message — except in a focused
    /// terminal (agent or shell), where it yields (false) so the PTY still gets
    /// SIGINT and the status row stays click-to-copy. Paste is normally
    /// intercepted earlier (so it reaches modal text inputs); this path covers
    /// the plain-terminal case. Returns `Some(consumed)` when `action` is one of
    /// these, else `None`.
    fn dispatch_clipboard_action(&mut self, action: crate::session::Action) -> Option<bool> {
        use crate::session::Action;
        let consumed = match action {
            Action::Copy => {
                if self.text_selection.is_some() {
                    self.copy_selection_to_clipboard();
                    true
                } else if !matches!(self.focus, InputFocus::Terminal)
                    && self.status_message.is_some()
                {
                    // A non-PTY pane: Ctrl+C has no other meaning, so copy the
                    // status message. In a focused terminal we fall through to
                    // SIGINT (below) instead.
                    self.copy_status_to_clipboard();
                    true
                } else {
                    false // terminal → SIGINT; elsewhere with no status → no-op
                }
            }
            Action::Paste => {
                self.paste_from_clipboard();
                true
            }
            _ => return None,
        };
        Some(consumed)
    }

    /// Session-list-scoped actions (navigation, move, open, sort). Returns
    /// `Some(consumed)` when `action` is one of these, else `None`.
    fn dispatch_session_list_action(&mut self, action: crate::session::Action) -> Option<bool> {
        use crate::session::Action;
        match action {
            Action::SessionListNext => self.act_session_list_next(),
            Action::SessionListPrev => self.act_session_list_prev(),
            Action::SessionListOpen => self.focus = InputFocus::Terminal,
            Action::SessionListMoveDown => self.move_active_session(true),
            Action::SessionListMoveUp => self.move_active_session(false),
            Action::SessionListSortAlphabetically => self.sort_sessions_alphabetically(),
            // Same switch as the F9 view: both features read Claude Code's
            // undocumented on-disk layout, so one flag governs both.
            Action::SessionListImport => {
                self.gated(
                    self.features.cc_activity,
                    "Agent activity",
                    Self::start_conversation_import,
                );
            }
            _ => return None,
        }
        Some(true)
    }

    /// File-viewer and terminal-scroll scoped actions, delegated to their
    /// sub-dispatchers. This is the final fall-through arm of `dispatch_action`.
    fn dispatch_scoped_pane_action(&mut self, action: crate::session::Action) -> bool {
        use crate::session::Action;
        match action {
            Action::AutomationsNew
            | Action::AutomationsNext
            | Action::AutomationsPrev
            | Action::AutomationsOpen
            | Action::AutomationsToggle
            | Action::AutomationsRun
            | Action::AutomationsDelete => self.dispatch_automations_pane_action(action),
            Action::TasksNew
            | Action::TasksNext
            | Action::TasksPrev
            | Action::TasksOpen
            | Action::TasksCycleStatus
            | Action::TasksRun
            | Action::TasksOpenRelated
            | Action::TasksDelete
            | Action::TasksPreviewDown
            | Action::TasksPreviewUp => self.dispatch_tasks_pane_action(action),
            Action::FileViewerDown
            | Action::FileViewerUp
            | Action::FileViewerCollapse
            | Action::FileViewerExpand
            | Action::FileViewerSearch
            | Action::FileViewerNextMatch
            | Action::FileViewerPrevMatch => self.dispatch_file_viewer_action(action),
            Action::TerminalScrollUp
            | Action::TerminalScrollDown
            | Action::TerminalPageUp
            | Action::TerminalPageDown => self.dispatch_terminal_scroll_action(action),
            // Every other action is handled by an earlier dispatcher in
            // `dispatch_action`, so this fall-through is never reached.
            _ => unreachable!("action handled by an earlier dispatcher"),
        }
    }

    /// Run a `FileViewer`-scoped action. Always consumes the key (`true`).
    fn dispatch_file_viewer_action(&mut self, action: crate::session::Action) -> bool {
        use crate::session::Action;
        match action {
            Action::FileViewerDown => self.file_viewer.move_selection(1),
            Action::FileViewerUp => self.file_viewer.move_selection(-1),
            Action::FileViewerCollapse => self.file_viewer.collapse(),
            Action::FileViewerExpand => self.file_viewer_expand(),
            Action::FileViewerSearch => self.file_viewer.start_search(),
            Action::FileViewerNextMatch => self.file_viewer.next_match(),
            Action::FileViewerPrevMatch => self.file_viewer.prev_match(),
            _ => {}
        }
        true
    }

    /// Run a terminal-scroll action. Always consumes the key (`true`).
    fn dispatch_terminal_scroll_action(&mut self, action: crate::session::Action) -> bool {
        use crate::session::Action;
        match action {
            Action::TerminalScrollUp => self.scroll_terminal_up(1),
            Action::TerminalScrollDown => self.scroll_terminal_down(1),
            Action::TerminalPageUp => {
                let amount = self.page_scroll_amount();
                self.scroll_terminal_up(amount);
            }
            Action::TerminalPageDown => {
                let amount = self.page_scroll_amount();
                self.scroll_terminal_down(amount);
            }
            _ => {}
        }
        true
    }

    /// `Ctrl+N`: in the automations context create an automation (mirrors `n`),
    /// else start the new-session wizard (clearing any leftover task prompt).
    fn act_new_session(&mut self) {
        if matches!(
            self.focus,
            InputFocus::Automations | InputFocus::AutomationEditor
        ) {
            self.new_automation_in_pane();
        } else {
            // A manual new-session must not inherit a task prompt or fork
            // parenthood left over from a cancelled task-spawn / fork.
            self.task_ui.pending_task_prompt = None;
            self.new_session.parent_session_id = None;
            self.start_new_session();
        }
    }

    /// `Ctrl+D`: delete the focused entity (session / automation / task). Editors
    /// and search capture their own keys earlier, so they yield here.
    fn act_delete_session(&mut self) -> bool {
        match self.focus {
            InputFocus::SessionList | InputFocus::FileViewer => {
                self.close_active_session();
                true
            }
            InputFocus::Automations => {
                self.dispatch_automations_pane_action(crate::session::Action::AutomationsDelete)
            }
            InputFocus::TaskList => {
                self.dispatch_tasks_pane_action(crate::session::Action::TasksDelete)
            }
            InputFocus::AutomationEditor
            | InputFocus::AutomationRunHistory
            | InputFocus::TaskEditor
            | InputFocus::CodeReview
            | InputFocus::ReviewFiles
            // The activity panes capture Ctrl+D as half-page paging before the
            // global lookup, so this is effectively unreachable for them.
            | InputFocus::CcActivity
            | InputFocus::CcActivityTree
            | InputFocus::GlobalSearch => false,
            InputFocus::Terminal => false, // forward to PTY
        }
    }

    /// Toggle the tasks panel column (`F5`/`Ctrl+W`), mirroring the file viewer:
    /// showing it also focuses it; hiding it drops focus back to the list.
    fn act_toggle_tasks(&mut self) {
        self.show_tasks_panel = !self.show_tasks_panel;
        if self.show_tasks_panel {
            self.refresh_tasks();
            self.task_ui.task_panel_index = 0;
            self.focus = InputFocus::TaskList;
            // Populate the central-pane preview for the selected task (without
            // this the workspace shows the empty hint).
            self.sync_task_editor();
        } else if self.focus == InputFocus::TaskList {
            self.focus = InputFocus::SessionList;
        }
        self.resize_sessions_to_content_area();
    }

    /// Toggle the file viewer column; showing it rebuilds it for the active
    /// session, hiding it returns focus to the session list.
    fn act_toggle_file_viewer(&mut self) {
        self.show_file_viewer = !self.show_file_viewer;
        if self.show_file_viewer {
            self.rebuild_file_viewer_for_active();
        } else if self.focus == InputFocus::FileViewer {
            self.focus = InputFocus::SessionList;
        }
        self.resize_sessions_to_content_area();
    }

    /// Session-list `Ctrl+J`: step to the next session, or flow into the
    /// automations pane past the last so the left column reads as one list.
    /// With automations disabled there is no pane to flow into, so the list
    /// wraps onto itself.
    fn act_session_list_next(&mut self) {
        if self.features.automations && self.active_is_last_in_order() {
            self.focus = InputFocus::Automations;
            self.automation_ui.automation_panel_index = 0;
            self.refresh_automation_view();
        } else {
            self.switch_session_forward();
        }
    }

    /// Session-list `Ctrl+K`: step to the previous session, or flow into the
    /// automations pane (last row) above the first.
    fn act_session_list_prev(&mut self) {
        if self.features.automations && self.active_is_first_in_order() {
            self.focus = InputFocus::Automations;
            self.automation_ui.automation_panel_index = self
                .automation_ui
                .cached_automations
                .len()
                .saturating_sub(1);
            self.refresh_automation_view();
        } else {
            self.switch_session_backward();
        }
    }

    /// Expand the selected file-viewer node, opening it in the editor when it's
    /// a file (dirs just toggle). Shared by the `FileViewerExpand` action.
    pub(super) fn file_viewer_expand(&mut self) {
        use crate::ui::file_viewer::Activation;
        // Capture root+file before activate() (activate only opens files, not dirs).
        let file_with_root = self.file_viewer.selected_file_with_root();
        if matches!(self.file_viewer.activate(), Activation::Open(_)) {
            if let Some((file, root)) = file_with_root {
                self.open_file_in_editor(root, file);
            }
        }
    }

    fn handle_theme_picker_key(&mut self, code: KeyCode) {
        let entries = crate::ui::theme::all_theme_entries();
        let entry_count = entries.len();
        let super::modals::Modal::ThemePicker(ref mut tp) = self.modal else {
            return;
        };
        match code {
            KeyCode::Esc => {
                // Cancel: undo the live preview by restoring the palette that
                // was active when the picker opened (nothing is persisted).
                crate::ui::theme::set_active(tp.original.clone());
                self.modal.close();
            }
            KeyCode::Char('j') | KeyCode::Down if tp.index + 1 < entry_count => {
                tp.index += 1;
                crate::ui::theme::set_active(entries[tp.index].palette.clone());
            }
            KeyCode::Char('k') | KeyCode::Up if tp.index > 0 => {
                tp.index -= 1;
                crate::ui::theme::set_active(entries[tp.index].palette.clone());
            }
            KeyCode::Enter => {
                let idx = tp.index;
                self.modal.close();
                self.commit_theme_selection(entries, idx);
            }
            _ => {}
        }
    }

    /// `Enter` on the theme picker: activate the selected entry and persist it
    /// as the active theme.
    fn commit_theme_selection(
        &mut self,
        entries: Vec<crate::session::theme_config::ThemeEntry>,
        idx: usize,
    ) {
        let Some(entry) = entries.into_iter().nth(idx) else {
            return;
        };
        crate::ui::theme::set_active(entry.palette.clone());
        if let Err(e) = self.db.set_active_theme(&entry.name) {
            tracing::error!("Failed to persist active theme: {e}");
            self.set_error(format!("Failed to persist theme: {e}"));
        }
        self.active_theme = entry;
    }

    /// Open the base-branch selector for the worktree flow **without blocking
    /// the UI** (ADR-P12): the modal opens instantly in a loading state, the
    /// branch listing runs on a `spawn_blocking` worker (applied by
    /// `poll_branch_load`), and the `git fetch origin` — a network round-trip
    /// that used to freeze the key handler for seconds — runs concurrently on
    /// its own worker. The fetch never changes the *list* (`git branch` shows
    /// local refs only); it matters at `git worktree add` time, so its
    /// completion signal is parked in `new_session.fetch_done` for the
    /// worktree-create worker to wait on.
    pub(crate) fn start_branch_selection(&mut self) {
        // Re-dispatching over an in-flight load is fine (back-then-forward
        // navigation): `BackgroundTask::start` hands out a fresh channel, the
        // orphaned worker's send fails silently, and `poll_branch_load` only
        // ever reads the newest receiver.

        // Resolve the remote host (if any) so branch listing targets the
        // session's machine. Cloned so we don't hold a borrow on `self`.
        let host = self
            .host_for_backend(self.new_session.backend.as_deref())
            .cloned();

        let Some(repo_path) = self.new_session.repo_path.clone() else {
            return;
        };

        self.modal = super::modals::Modal::BranchSelector(super::modals::BranchSelectorModal {
            index: 0,
            branches: Vec::new(),
            filter: Default::default(),
            loading: true,
        });

        let tx = self.branch_load.start();
        self.metrics.bump(|p| &mut p.branch_loads_dispatched);
        let list_host = host.clone();
        let list_repo = repo_path.clone();
        tokio::task::spawn_blocking(move || {
            let result = match crate::git::list_branches_on(list_host.as_ref(), &list_repo) {
                Ok(branches) if branches.is_empty() => {
                    Err("No branches found in repository".to_string())
                }
                Ok(branches) => Ok(Self::ordered_branch_list(
                    list_host.as_ref(),
                    &list_repo,
                    branches,
                )),
                Err(e) => Err(format!("Failed to list branches: {e:#}")),
            };
            let _ = tx.send(result);
        });

        let (fetch_tx, fetch_rx) = std::sync::mpsc::channel();
        self.new_session.fetch_done = Some(fetch_rx);
        let all_repos = self.new_session.all_repos.clone();
        tokio::task::spawn_blocking(move || {
            Self::fetch_pending_repos(host.as_ref(), &repo_path, all_repos.as_ref());
            let _ = fetch_tx.send(());
        });
    }

    /// Fetch origin for the primary repo and any extra worktree repos so the
    /// worktrees fork from fresh refs. Failures are non-fatal (logged only).
    /// Runs on a background worker — never on the UI thread (ADR-P12).
    fn fetch_pending_repos(
        host: Option<&crate::session::HostDef>,
        repo_path: &std::path::Path,
        all_repos: Option<&Vec<std::path::PathBuf>>,
    ) {
        if let Err(e) = crate::git::git_fetch_on(host, repo_path) {
            warn!("git fetch origin failed (continuing): {e:#}");
        }
        let Some(all_repos) = all_repos else {
            return;
        };
        for extra_repo in all_repos.iter().skip(1) {
            if let Err(e) = crate::git::git_fetch_on(host, extra_repo) {
                warn!(
                    "git fetch origin failed for {} (continuing): {e:#}",
                    extra_repo.display()
                );
            }
        }
    }

    /// Order a branch list for the selector: the local default branch first,
    /// then `origin/<default>` (remote-based branching) pinned at the very top.
    fn ordered_branch_list(
        host: Option<&crate::session::HostDef>,
        repo_path: &std::path::Path,
        mut branches: Vec<String>,
    ) -> Vec<String> {
        // One `symbolic-ref` subprocess serves both the local-default pick and
        // the `origin/<default>` pin below (it used to run twice).
        let remote_default = crate::git::default_branch_from_remote_on(host, repo_path);

        // Move the default branch to front so it's pre-selected: the remote's
        // default when it exists locally, else a local `main`/`master`.
        let default = remote_default
            .as_ref()
            .filter(|name| branches.contains(*name))
            .cloned()
            .or_else(|| {
                ["main", "master"]
                    .into_iter()
                    .find(|c| branches.iter().any(|b| b == c))
                    .map(str::to_string)
            });
        if let Some(default) = default {
            if let Some(pos) = branches.iter().position(|b| b == &default) {
                let branch = branches.remove(pos);
                branches.insert(0, branch);
            }
        }

        // Insert origin/<default> at position 0 for remote-based branching.
        let remote_ref = remote_default
            .map(|name| format!("origin/{name}"))
            .or_else(|| {
                for candidate in ["origin/main", "origin/master"] {
                    if crate::git::branch_exists_on(host, repo_path, candidate) {
                        return Some(candidate.to_string());
                    }
                }
                None
            });
        if let Some(ref remote) = remote_ref {
            if !branches.contains(remote) {
                branches.insert(0, remote.clone());
            }
        }

        branches
    }

    // ── Repo Picker Modal ────────────────────────────────────────────────

    /// The repo palette: one always-focused input, no internal focus zones.
    /// Typing edits the input (filter or path); everything acting on the
    /// highlighted row lives on chords/arrows that can never collide with text
    /// (plain `Space`/`Delete` act on rows only while the input is empty).
    fn handle_repo_picker_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if !matches!(self.modal, super::modals::Modal::RepoPicker(_)) {
            return;
        }
        // Row-action chords win over text editing. Ctrl+P is not `Ctrl+I` —
        // that is `Tab`. Ctrl+Space arrives as `Char(' ')` on modern
        // keyboard-protocol terminals and as NUL on legacy ones.
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('p') | KeyCode::Char('P') => {
                    self.repo_picker_import_parent();
                    return;
                }
                KeyCode::Char('t') | KeyCode::Char('T') => {
                    self.repo_picker_toggle_worktree();
                    return;
                }
                KeyCode::Char(' ') | KeyCode::Null => {
                    self.repo_picker_row_action();
                    return;
                }
                _ => {}
            }
        }
        match code {
            KeyCode::Esc => self.repo_picker_back(),
            KeyCode::Enter => self.repo_picker_enter(),
            KeyCode::Up => self.repo_picker_move(-1),
            KeyCode::Down => self.repo_picker_move(1),
            KeyCode::PageUp => self.repo_picker_move(-10),
            KeyCode::PageDown => self.repo_picker_move(10),
            // Tab ONLY completes — it never moves focus (there is none to
            // move) and never toggles anything.
            KeyCode::Tab => self.repo_picker_complete(),
            KeyCode::Char(' ') if self.repo_picker_input_empty() => self.repo_picker_row_action(),
            KeyCode::Delete if self.repo_picker_input_empty() => self.repo_picker_delete_bookmark(),
            other => {
                let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
                    return;
                };
                let before = rp.input.value().to_string();
                if !super::modals::apply_text_input_key(Some(&mut rp.input), other, mods) {
                    return;
                }
                // An edit re-filters and snaps the highlight to the best (first)
                // match; a bare cursor move does neither.
                if rp.input.value() != before {
                    rp.list_index = 0;
                    self.recompute_repo_filter();
                }
                self.refresh_repo_picker_candidates();
            }
        }
    }

    /// `Esc` on the repo palette: back to the host picker when that step was
    /// shown, else — this is the first step — cancel the flow.
    fn repo_picker_back(&mut self) {
        self.modal.close();
        self.new_session.saved_repo_picker = None;
        if self.hosts.is_empty() {
            self.new_session.backend = None;
            return;
        }
        self.open_host_picker();
    }

    /// Whether the palette input is empty (plain `Space`/`Delete` act on the
    /// highlighted row only then — once the user types, keys edit text).
    fn repo_picker_input_empty(&self) -> bool {
        match &self.modal {
            super::modals::Modal::RepoPicker(rp) => rp.input.value().is_empty(),
            _ => false,
        }
    }

    /// Move the highlight by `delta`. In filter mode this walks the bookmark
    /// rows; in path mode it walks the directory candidates, where stepping up
    /// past the first candidate returns to `None` — "act on the typed path
    /// itself" — so the literal input always stays reachable.
    fn repo_picker_move(&mut self, delta: i32) {
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        if rp.input_mode() == super::modals::RepoInputMode::Path {
            let len = rp.candidates.len() as i32;
            if len == 0 {
                return;
            }
            rp.candidate_index = match rp.candidate_index {
                None if delta > 0 => Some(((delta - 1).min(len - 1)) as usize),
                None => None,
                Some(i) => {
                    let next = i as i32 + delta;
                    if next < 0 {
                        None
                    } else {
                        Some(next.min(len - 1) as usize)
                    }
                }
            };
            return;
        }
        let len = rp.filtered_indices.len();
        if len == 0 {
            return;
        }
        rp.list_index = (rp.list_index as i32 + delta).clamp(0, len as i32 - 1) as usize;
    }

    /// `Space` (input empty) / `Ctrl+Space` / row click: act on the highlighted
    /// row — toggle a repo's checkbox, fold a parent header, import a suggested
    /// folder, or (for the pinned "start here" row) start the session.
    fn repo_picker_row_action(&mut self) {
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        let Some(&real_idx) = rp.filtered_indices.get(rp.list_index) else {
            return;
        };
        let Some(row) = rp.rows.get(real_idx) else {
            return;
        };
        let (kind, path) = (row.kind, row.path.clone());
        match kind {
            super::modals::RepoRowKind::Header => rp.toggle_collapsed(real_idx),
            super::modals::RepoRowKind::Repo { .. } => rp.toggle_selected(&path),
            super::modals::RepoRowKind::ImportSuggestion => self.repo_picker_import_folder(&path),
            super::modals::RepoRowKind::StartHere => self.repo_picker_start_here(),
        }
    }

    /// Toggle the worktree flag of the repo under the cursor, auto-selecting it
    /// when worktree mode is turned on.
    fn repo_picker_toggle_worktree(&mut self) {
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        let Some(&real_idx) = rp.filtered_indices.get(rp.list_index) else {
            return;
        };
        let Some(row) = rp.rows.get(real_idx) else {
            return;
        };
        if !row.is_repo() {
            return;
        }
        let path = row.path.clone();
        rp.toggle_worktree(&path);
    }

    /// Delete the bookmark under the cursor. A standalone repo is removed in
    /// place; a parent header drops the parent bookmark and its (ephemeral)
    /// child rows via a full re-scan; a child row has no persistent identity, so
    /// deleting it is a no-op (delete its parent header instead).
    fn repo_picker_delete_bookmark(&mut self) {
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        let Some(&real_idx) = rp.filtered_indices.get(rp.list_index) else {
            return;
        };
        let Some(row) = rp.rows.get(real_idx) else {
            return;
        };
        // Pinned helper rows have no bookmark to forget.
        if matches!(
            row.kind,
            super::modals::RepoRowKind::ImportSuggestion | super::modals::RepoRowKind::StartHere
        ) {
            return;
        }
        let path = row.path.clone();
        let is_header = rp.is_header_row(real_idx);
        let is_child = rp.is_child_row(real_idx);

        if is_child {
            self.set_status(
                super::StatusLevel::Info,
                "Child of a parent bookmark — delete the parent header instead",
            );
            return;
        }

        if let Err(e) = self
            .db
            .delete_repo_bookmark(self.bookmark_host_key(), &path)
        {
            error!("Failed to delete repo bookmark: {e}");
        }

        if is_header {
            // Rebuild so the header and all its child rows disappear together.
            self.refresh_repo_picker_rows();
            return;
        }

        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        rp.rows.remove(real_idx);
        // Forgetting a bookmark also drops its picks — a hidden selection that
        // resurfaces on re-add would be surprising.
        rp.selected.remove(&path);
        rp.worktree.remove(&path);
        self.recompute_repo_filter();
    }

    /// `Tab`: complete the typed path — and nothing else. A Tab with nothing
    /// to complete is a no-op. Remote targets have no per-keystroke candidates
    /// (that would fire an ssh/wsl round-trip on every character); list the
    /// remote directory on demand here instead. Completion only applies with
    /// the cursor at the end (inserting mid-string would garble the path).
    fn repo_picker_complete(&mut self) {
        if self.new_session.backend.is_some() {
            let super::modals::Modal::RepoPicker(ref rp) = self.modal else {
                return;
            };
            let value = rp.input.value().to_string();
            let at_end = rp.input.cursor_pos() == value.chars().count();
            let sug = at_end
                .then(|| self.remote_path_candidates(&value))
                .flatten();
            if let super::modals::Modal::RepoPicker(ref mut rp) = self.modal {
                if let Some(sug) = sug {
                    for c in sug.chars() {
                        rp.input.insert(c);
                    }
                    // A descent invalidates the listed candidates (they were
                    // the parent's); a partial completion just narrows them.
                    if sug.ends_with('/') {
                        rp.candidates.clear();
                        rp.candidate_index = None;
                    } else if let Some((_, prefix)) = rp
                        .input
                        .value()
                        .rsplit_once('/')
                        .map(|(a, b)| (a, b.to_string()))
                    {
                        rp.candidates.retain(|c| c.name.starts_with(&prefix));
                        rp.candidate_index = None;
                    }
                }
            }
            return;
        }
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        if let Some(suggestion) = rp.path_suggestion.take() {
            for c in suggestion.chars() {
                rp.input.insert(c);
            }
            self.recompute_repo_filter();
        }
        self.refresh_repo_picker_candidates();
    }

    /// `Enter` — the palette's primary action, in priority order: a typed path
    /// is committed and the flow advances with it; checked repos submit;
    /// otherwise the highlighted row acts (open repo / fold header / import
    /// suggestion / start without a repo).
    fn repo_picker_enter(&mut self) {
        let super::modals::Modal::RepoPicker(ref rp) = self.modal else {
            return;
        };
        if rp.input_mode() == super::modals::RepoInputMode::Path {
            // A highlighted candidate acts directly: a git repo opens (the
            // browse fast path), anything else drills in. The typed path
            // itself stays reachable at `candidate_index == None`.
            if let Some(ci) = rp.candidate_index {
                let Some(c) = rp.candidates.get(ci) else {
                    return;
                };
                let (is_repo, name, full) = (c.is_repo, c.name.clone(), c.full.clone());
                if is_repo {
                    self.repo_picker_open_candidate(&full);
                } else {
                    self.repo_picker_drill_into(&name);
                }
                return;
            }
            // No highlight: commit the typed path (any existing dir — also
            // the way to open a non-repo directory) and advance.
            if self.repo_picker_commit_path_input() {
                self.submit_repo_picker();
            }
            return;
        }
        if rp.picked_count() > 0 {
            self.submit_repo_picker();
            return;
        }
        let Some(&real_idx) = rp.filtered_indices.get(rp.list_index) else {
            return;
        };
        let Some(row) = rp.rows.get(real_idx) else {
            return;
        };
        let (kind, path) = (row.kind, row.path.clone());
        match kind {
            super::modals::RepoRowKind::Header => {
                let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
                    return;
                };
                rp.toggle_collapsed(real_idx);
            }
            // Single-repo fast path: nothing is checked, so Enter means "this
            // one" — check it and go.
            super::modals::RepoRowKind::Repo { .. } => {
                let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
                    return;
                };
                rp.selected.insert(path);
                self.submit_repo_picker();
            }
            super::modals::RepoRowKind::ImportSuggestion => self.repo_picker_import_folder(&path),
            super::modals::RepoRowKind::StartHere => self.repo_picker_start_here(),
        }
    }

    /// The pinned "start here" row: an explicit no-repo session (local `$HOME`,
    /// remote default directory).
    fn repo_picker_start_here(&mut self) {
        // Park the palette so Esc from the name step restores it as-was.
        if let super::modals::Modal::RepoPicker(ref rp) = self.modal {
            self.new_session.saved_repo_picker = Some(Box::new(rp.clone()));
        }
        self.modal.close();
        self.spawn_repo_picker_no_repos();
    }

    /// Enter on a candidate that is a git repo: bookmark it, select it, and
    /// advance the flow with it. The path input is cleared first so the
    /// palette parked for Esc-back shows the picked bookmark, not a stale
    /// path-mode candidate view.
    fn repo_picker_open_candidate(&mut self, full: &std::path::Path) {
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        let persist = Self::repo_picker_select_or_add_row(rp, full);
        rp.input.clear();
        if persist {
            if let Err(e) = self.db.upsert_repo_bookmark(self.bookmark_host_key(), full) {
                error!("Failed to save repo bookmark: {e}");
                self.set_error(format!("Failed to save repo bookmark: {e}"));
            }
        }
        self.recompute_repo_filter();
        self.refresh_repo_picker_candidates();
        self.submit_repo_picker();
    }

    /// Enter on a candidate that is a plain directory: descend into it,
    /// keeping the user's typed form (a `~/…` input stays tilde-style).
    fn repo_picker_drill_into(&mut self, name: &str) {
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        let value = rp.input.value().to_string();
        // Replace the name-prefix — everything after the last path separator
        // (`/`, and `\` on Windows; `std::path::is_separator`, matching
        // `paths::split_path_input`) — with the chosen candidate, re-appending
        // that same separator so the next refresh lists its children while the
        // input keeps the user's typed prefix (and thus its path-mode `~`/`/`
        // lead). Splitting on a hardcoded `/` would miss a `~\…` input and drop
        // into the absolute fallback below, flipping `input_mode` to Filter.
        let new = match value.rfind(std::path::is_separator) {
            Some(i) => {
                let sep = &value[i..=i];
                format!("{}{name}{sep}", &value[..=i])
            }
            // A separator-free path-mode input is only a bare `~` (`input_mode`
            // requires a `~`/`/`/`./`/`../` lead). Rebuild from the candidate's
            // full path, re-tildified so it keeps a `~` lead — an absolute
            // `C:\…` would flip `input_mode` back to Filter on Windows.
            None => {
                let Some(c) = rp.candidates.iter().find(|c| c.name == name) else {
                    return;
                };
                format!(
                    "{}{}",
                    crate::paths::display_path_tilde(&c.full),
                    std::path::MAIN_SEPARATOR
                )
            }
        };
        rp.input.set(&new);
        self.recompute_repo_filter();
        self.refresh_repo_picker_candidates();
    }

    /// Commit the typed path in the palette input: add or re-select the
    /// bookmark, persist it (scoped to the target host), and clear the input.
    /// Returns whether a repo row ended up selected (the caller then advances
    /// the flow) — `false` on an empty input or a validation error.
    fn repo_picker_commit_path_input(&mut self) -> bool {
        // A remote path expands `~` against the *remote* home (never the local
        // one) and is verified to exist on the host before it's accepted —
        // catching a typo here beats failing minutes later at branch listing
        // or worktree creation. One ssh/wsl round-trip, on explicit Enter only.
        let remote_host = self
            .host_for_backend(self.new_session.backend.as_deref())
            .cloned();
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return false;
        };
        let mut path = rp.input.value().trim().to_string();
        if path.is_empty() {
            self.recompute_repo_filter();
            return false;
        }
        // Normalize a browse-style trailing separator ("~/code/" means ~/code,
        // and on Windows "~\code\") so the bookmark never carries one and
        // dedupes against the bare form. `is_separator` matches `/` everywhere
        // and `\` on Windows, mirroring `paths::split_path_input`.
        while path.len() > 1 && path.ends_with(std::path::is_separator) {
            path.pop();
        }
        let expanded = match &remote_host {
            Some(host) => {
                let expanded = match crate::git::expand_remote_tilde(host, &path) {
                    Ok(p) => p,
                    Err(e) => {
                        self.set_error(format!("Cannot resolve ~ on '{}': {e:#}", host.name));
                        return false;
                    }
                };
                if crate::git::list_dir_on(host, &expanded).is_err() {
                    self.set_error(format!("Path not found on '{}': {expanded}", host.name));
                    return false;
                }
                std::path::PathBuf::from(expanded)
            }
            None => {
                // Mirror the remote check locally: a typo'd path must not
                // become a bookmark that spawns a session in a dead cwd.
                let expanded = paths::expand_tilde(&path);
                if !expanded.is_dir() {
                    self.set_error(format!("Path not found: {}", expanded.display()));
                    return false;
                }
                expanded
            }
        };
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return false;
        };
        let persist = Self::repo_picker_select_or_add_row(rp, &expanded);
        let selected = rp.selected.contains(&expanded);
        if persist {
            if let Err(e) = self
                .db
                .upsert_repo_bookmark(self.bookmark_host_key(), &expanded)
            {
                error!("Failed to save repo bookmark: {e}");
                self.set_error(format!("Failed to save repo bookmark: {e}"));
            }
        }
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return false;
        };
        rp.input.clear();
        self.recompute_repo_filter();
        self.refresh_repo_picker_candidates();
        selected
    }

    /// Select an already-represented bookmark row for `expanded`, or push a new
    /// auto-selected row. Returns whether the path should be persisted as a
    /// standalone bookmark: a path already shown as a parent's child (or the
    /// parent header itself) is already covered, so it is not re-persisted.
    fn repo_picker_select_or_add_row(
        rp: &mut super::modals::RepoPickerModal,
        expanded: &std::path::Path,
    ) -> bool {
        // If already represented, just select it (no duplicate row or DB entry).
        let Some(idx) = rp.rows.iter().position(|r| r.path == *expanded) else {
            // New rows land before the pinned helper rows, not after them.
            let insert_at = rp
                .rows
                .iter()
                .position(|r| {
                    matches!(
                        r.kind,
                        super::modals::RepoRowKind::ImportSuggestion
                            | super::modals::RepoRowKind::StartHere
                    )
                })
                .unwrap_or(rp.rows.len());
            rp.rows.insert(
                insert_at,
                super::modals::RepoRow {
                    path: expanded.to_path_buf(),
                    kind: super::modals::RepoRowKind::Repo { child: false },
                },
            );
            rp.selected.insert(expanded.to_path_buf());
            return true;
        };
        let is_child = rp.is_child_row(idx);
        let is_header = rp.is_header_row(idx);
        if !is_header {
            rp.selected.insert(expanded.to_path_buf());
        }
        !is_child && !is_header
    }

    /// Import the typed path as a *parent* folder: persist it as a parent
    /// bookmark, then rebuild the list (re-scanning its git sub-directories).
    /// The parent itself is not added as a selectable repo — its children are.
    /// Local targets only: the child scan walks the local filesystem, so a
    /// remote parent would import the wrong machine's repos.
    fn repo_picker_import_parent(&mut self) {
        if self.new_session.backend.is_some() {
            self.set_status(
                super::StatusLevel::Info,
                "Parent import scans the local filesystem — add remote repos by path instead",
            );
            return;
        }
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        let path = rp.input.value().trim().to_string();
        if path.is_empty() {
            self.set_status(
                super::StatusLevel::Info,
                "Type a folder path, then Ctrl+P to import its repos as a parent",
            );
            return;
        }
        let expanded = paths::expand_tilde(&path);
        if let super::modals::Modal::RepoPicker(ref mut rp) = self.modal {
            rp.input.clear();
            rp.path_suggestion = None;
        }
        self.repo_picker_import_folder(&expanded);
    }

    /// Persist `dir` as a parent bookmark and re-scan the picker rows. Shared
    /// by the typed-path `Ctrl+P` and the first-run import-suggestion rows.
    fn repo_picker_import_folder(&mut self, dir: &std::path::Path) {
        // The wizard is local at both call sites, so this is equivalent to a
        // literal "" — but the `"" = local` encoding stays owned by
        // `bookmark_host_key` alone.
        let host = self.bookmark_host_key().to_string();
        if let Err(e) = self.db.upsert_repo_bookmark_kind(&host, dir, true) {
            error!("Failed to save parent bookmark: {e}");
            self.set_error(format!("Failed to save parent bookmark: {e}"));
        }
        self.refresh_repo_picker_rows();
    }

    /// One explicit ssh/wsl listing for the typed remote path: fills the
    /// candidate list (`is_repo` unknowable without one round-trip each, so
    /// always `false` — Enter on a remote candidate drills in) and returns the
    /// completion suffix (mirroring `paths::complete_directory_path`). Only
    /// called on an explicit `Tab` so it doesn't run per keystroke.
    fn remote_path_candidates(&mut self, input: &str) -> Option<String> {
        let host = self
            .host_for_backend(self.new_session.backend.as_deref())?
            .clone();
        // Need at least one `/` to know which remote dir to list.
        let (parent, prefix) = input.rsplit_once('/')?;
        let parent = if parent.is_empty() { "/" } else { parent };
        let entries = crate::git::list_dir_on(&host, parent).ok()?;
        let sug = dir_completion_suffix(&entries, prefix);
        let show_hidden = prefix.starts_with('.');
        if let super::modals::Modal::RepoPicker(ref mut rp) = self.modal {
            let mut names: Vec<String> = entries
                .into_iter()
                .filter(|n| (show_hidden || !n.starts_with('.')) && n.starts_with(prefix))
                .collect();
            names.sort();
            rp.candidates = names
                .into_iter()
                .map(|name| super::modals::PathCandidate {
                    full: std::path::PathBuf::from(format!(
                        "{}/{name}",
                        parent.trim_end_matches('/')
                    )),
                    name,
                    is_repo: false,
                })
                .collect();
            rp.candidate_index = None;
        }
        sug
    }

    /// Recompute the path-mode candidate list and the ghost completion derived
    /// from it. For a remote target (SSH host or WSL distro) the path lives on
    /// the *remote* filesystem, so listing the local one would suggest the
    /// wrong directories entirely — remote candidates are only filled by an
    /// explicit `Tab` (see [`Self::remote_path_candidates`]).
    pub(super) fn refresh_repo_picker_candidates(&mut self) {
        let remote = self.new_session.backend.is_some();
        let super::modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        rp.candidates.clear();
        rp.candidate_index = None;
        rp.path_suggestion = None;
        // Filter text is not a path — completing it would be noise.
        if remote || rp.input_mode() != super::modals::RepoInputMode::Path {
            return;
        }
        let value = rp.input.value().to_string();
        // Completion only applies with the cursor at the end (inserting
        // mid-string would garble the path).
        if rp.input.cursor_pos() != value.chars().count() {
            return;
        }
        let Some((parent, prefix)) = paths::split_path_input(&value) else {
            return;
        };
        let mut names = paths::matching_dir_names(&parent, &prefix);
        names.sort();
        rp.path_suggestion = dir_completion_suffix(&names, &prefix);
        rp.candidates = names
            .into_iter()
            .map(|name| {
                let full = parent.join(&name);
                let is_repo = crate::git::is_git_repo(&full);
                super::modals::PathCandidate {
                    name,
                    full,
                    is_repo,
                }
            })
            .collect();
    }

    fn recompute_repo_filter(&mut self) {
        if let super::modals::Modal::RepoPicker(ref mut rp) = self.modal {
            rp.recompute_filter();
        }
    }

    fn submit_repo_picker(&mut self) {
        let super::modals::Modal::RepoPicker(ref rp) = self.modal else {
            return;
        };

        let (worktree_repos, normal_repos) = Self::partition_selected_repos(rp);

        // Touch all selected bookmarks so they stay sorted by recency.
        for repo in worktree_repos.iter().chain(normal_repos.iter()) {
            if let Err(e) = self.db.upsert_repo_bookmark(self.bookmark_host_key(), repo) {
                error!("Failed to touch repo bookmark: {e}");
            }
        }

        // Nothing checked: Enter acts on the highlighted row instead (see
        // `repo_picker_enter`); the old silent $HOME fallthrough is gone.
        if worktree_repos.is_empty() && normal_repos.is_empty() {
            return;
        }

        // Park the palette so Esc from a later step restores it as-was.
        if let super::modals::Modal::RepoPicker(ref rp) = self.modal {
            self.new_session.saved_repo_picker = Some(Box::new(rp.clone()));
        }
        self.modal.close();

        if !worktree_repos.is_empty() {
            self.spawn_repo_picker_worktrees(worktree_repos, normal_repos);
        } else {
            self.spawn_repo_picker_normal(normal_repos);
        }
    }

    /// No repos selected — spawn with HOME as cwd. For a remote target,
    /// leave cwd unset so the remote session starts in its own default
    /// directory (local $HOME is meaningless there).
    fn spawn_repo_picker_no_repos(&mut self) {
        let mut config = SessionConfig::default();
        if self.new_session.backend.is_none() {
            if let Some(home) = crate::paths::home_dir() {
                config.cwd = Some(home);
            }
        }
        self.spawn_session_with_config(&config);
    }

    /// Has worktree repos — go to branch selection.
    /// Store normal repos for inclusion after worktree creation.
    fn spawn_repo_picker_worktrees(
        &mut self,
        worktree_repos: Vec<std::path::PathBuf>,
        normal_repos: Vec<std::path::PathBuf>,
    ) {
        self.new_session.repo_path = Some(worktree_repos[0].clone());
        self.new_session.all_repos = if worktree_repos.len() > 1 {
            Some(worktree_repos)
        } else {
            None
        };
        self.new_session.normal_repos = normal_repos;
        self.start_branch_selection();
    }

    /// All normal repos — spawn directly (local-tmux), going straight
    /// to the agent picker chain.
    fn spawn_repo_picker_normal(&mut self, normal_repos: Vec<std::path::PathBuf>) {
        self.new_session.additional_dirs = normal_repos[1..].to_vec();
        let config = SessionConfig {
            cwd: Some(normal_repos[0].clone()),
            ..SessionConfig::default()
        };
        self.spawn_session_with_config(&config);
    }

    /// Split the selected bookmarks into (worktree repos, normal repos).
    fn partition_selected_repos(
        rp: &super::modals::RepoPickerModal,
    ) -> (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) {
        let mut worktree_repos: Vec<std::path::PathBuf> = Vec::new();
        let mut normal_repos: Vec<std::path::PathBuf> = Vec::new();
        // Iterate rows (not the selection set) so the result keeps the list's
        // recency order — the first selected repo becomes the session cwd.
        for row in &rp.rows {
            if row.is_header() || !rp.selected.contains(&row.path) {
                continue;
            }
            if rp.worktree.contains(&row.path) {
                worktree_repos.push(row.path.clone());
            } else {
                normal_repos.push(row.path.clone());
            }
        }
        (worktree_repos, normal_repos)
    }
}

#[cfg(test)]
mod tests {
    use super::{dir_completion_suffix, is_ctrl_letter_chord, session_name_to_branch};
    use crossterm::event::{KeyCode, KeyModifiers};

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn dir_completion_single_match_appends_trailing_slash() {
        // One match → complete the rest and descend on the next Tab.
        let dirs = names(&["repositories", "downloads"]);
        assert_eq!(
            dir_completion_suffix(&dirs, "rep"),
            Some("ositories/".to_string())
        );
    }

    #[test]
    fn dir_completion_multi_match_uses_common_prefix() {
        // Several matches → their longest common prefix beyond the typed prefix,
        // no trailing slash (still ambiguous).
        let dirs = names(&["adaptfy-landing", "adaptfy-landing-infra", "ai-ml"]);
        assert_eq!(
            dir_completion_suffix(&dirs, "adaptfy"),
            Some("-landing".to_string())
        );
    }

    #[test]
    fn dir_completion_ambiguous_with_no_shared_extension_is_none() {
        // Two matches that share nothing past the prefix → nothing to add.
        let dirs = names(&["ai-ml", "ai-infra"]);
        assert_eq!(dir_completion_suffix(&dirs, "ai-"), None);
    }

    #[test]
    fn dir_completion_no_match_is_none() {
        let dirs = names(&["repositories", "downloads"]);
        assert_eq!(dir_completion_suffix(&dirs, "zzz"), None);
        assert_eq!(dir_completion_suffix(&[], "any"), None);
    }

    #[test]
    fn dir_completion_hidden_dirs_only_offered_to_dot_prefix() {
        // Mirrors the local completer: hidden entries never match a plain
        // prefix (including the empty one), but a `.`-prefix reaches them.
        let dirs = names(&[".config", ".cache", "repos"]);
        assert_eq!(dir_completion_suffix(&dirs, ""), Some("repos/".to_string()));
        assert_eq!(
            dir_completion_suffix(&dirs, ".co"),
            Some("nfig/".to_string())
        );
        assert_eq!(dir_completion_suffix(&dirs, "."), Some("c".to_string()));
    }

    #[test]
    fn dir_completion_multibyte_divergence_floors_to_char_boundary() {
        // "répo-a" vs "rêpo-b" diverge inside the 2-byte é/ê — the byte-wise
        // LCP lands mid-char and must be floored, not sliced (panic) or
        // dropped (no completion for a valid shared prefix).
        let dirs = names(&["répo-a", "rêpo-b"]);
        assert_eq!(dir_completion_suffix(&dirs, "r"), None);
        // Diverging *after* a multibyte char keeps the full shared run.
        let dirs = names(&["été-x", "été-y"]);
        assert_eq!(dir_completion_suffix(&dirs, "é"), Some("té-".to_string()));
    }

    #[test]
    fn dir_completion_exact_match_alone_still_descends() {
        // Prefix already equals the only entry → append just the slash.
        let dirs = names(&["repositories"]);
        assert_eq!(
            dir_completion_suffix(&dirs, "repositories"),
            Some("/".to_string())
        );
    }

    #[test]
    fn ctrl_letter_chord_detects_readline_namespace() {
        // Bare Ctrl+letter — the readline-conflicting namespace.
        assert!(is_ctrl_letter_chord(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL
        ));
        assert!(is_ctrl_letter_chord(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL
        ));
        // Plain letters, F-keys, and Ctrl+<non-letter> are not in the namespace,
        // so a passthrough action bound to them keeps working in the terminal.
        assert!(!is_ctrl_letter_chord(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        ));
        assert!(!is_ctrl_letter_chord(KeyCode::F(3), KeyModifiers::NONE));
        assert!(!is_ctrl_letter_chord(
            KeyCode::Char('1'),
            KeyModifiers::CONTROL
        ));
        // Extra modifiers take it out of the bare-Ctrl namespace.
        assert!(!is_ctrl_letter_chord(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ));
    }

    #[test]
    fn basic_conversion() {
        assert_eq!(session_name_to_branch("My Feature"), "my-feature");
    }

    #[test]
    fn multiple_spaces() {
        assert_eq!(session_name_to_branch("hello   world"), "hello-world");
    }

    #[test]
    fn uppercase() {
        assert_eq!(session_name_to_branch("FOO"), "foo");
    }

    #[test]
    fn consecutive_hyphens() {
        assert_eq!(session_name_to_branch("a--b"), "a-b");
    }

    #[test]
    fn trims_hyphens() {
        assert_eq!(session_name_to_branch(" -trim- "), "trim");
    }

    #[test]
    fn underscores_become_hyphens() {
        assert_eq!(session_name_to_branch("foo_bar"), "foo-bar");
    }

    #[test]
    fn strips_special_chars() {
        assert_eq!(session_name_to_branch("foo@bar!baz"), "foobarbaz");
    }

    #[test]
    fn empty_string() {
        assert_eq!(session_name_to_branch(""), "");
    }

    #[test]
    fn mixed_separators() {
        assert_eq!(session_name_to_branch("a - b _ c"), "a-b-c");
    }

    #[test]
    fn only_special_chars() {
        assert_eq!(session_name_to_branch("@#$%"), "");
    }

    #[test]
    fn unicode_alphanumeric() {
        assert_eq!(session_name_to_branch("café"), "café");
    }

    #[test]
    fn preserves_slash_hierarchy() {
        assert_eq!(
            session_name_to_branch("fix/branch-naming"),
            "fix/branch-naming"
        );
    }

    #[test]
    fn slash_absorbs_adjacent_separators() {
        assert_eq!(
            session_name_to_branch("Fix / Branch Naming"),
            "fix/branch-naming"
        );
    }

    #[test]
    fn collapses_and_trims_slashes() {
        assert_eq!(session_name_to_branch("//a//b//"), "a/b");
    }

    #[test]
    fn only_slashes() {
        assert_eq!(session_name_to_branch("///"), "");
    }
}
