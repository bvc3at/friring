//! Sandbox-profile cluster for the Friring TUI application.
//!
//! The `impl App` methods behind `docs/SANDBOX.md` §UI: the profile list modal,
//! the profile editor, and the create/edit/delete operations against
//! [`crate::storage::sandboxes`]. Mirrors [`super::automation`], which is the
//! collection-editing pattern this screen clones.
//!
//! Rendering lives in [`crate::ui::sandbox_list_modal`] and
//! [`crate::ui::sandbox_editor_modal`]; the form state itself is
//! [`modals::SandboxEditorModal`].

use crossterm::event::{KeyCode, KeyModifiers};
use tracing::error;

use super::modals;
use super::{App, StatusLevel};
use crate::sandbox::SandboxHost;
use crate::session::{SandboxBackendKind, SandboxProfile};
use crate::ui::sandbox_list_modal::SandboxProfileRow;
use crate::ui::sandbox_picker_modal::SandboxChoice;

impl App {
    /// Load the profile a session row names, for a launch that rebuilds the
    /// boundary from persisted state.
    ///
    /// # Errors
    ///
    /// The name is set but no such profile is stored. Deleting a profile
    /// deliberately leaves referencing sessions dangling, so that a relaunch
    /// fails here rather than quietly running the agent on the host.
    pub(crate) fn load_session_sandbox(
        &self,
        name: Option<&str>,
    ) -> Result<Option<SandboxProfile>, String> {
        let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
            return Ok(None);
        };
        match self.db.get_sandbox_profile(name) {
            Ok(Some(profile)) => Ok(Some(profile)),
            Ok(None) => Err(format!(
                "Sandbox profile '{name}' no longer exists — recreate it, or clear the \
                 session's profile, before relaunching"
            )),
            Err(e) => {
                error!("Failed to load sandbox profile '{name}': {e}");
                Err(format!("Failed to load sandbox profile '{name}'"))
            }
        }
    }

    /// Every stored profile as a list row, with `auto` resolved against this
    /// host so the row shows what would actually run.
    pub(crate) fn sandbox_profile_rows(&self) -> Vec<SandboxProfileRow> {
        let profiles = match self.db.list_sandbox_profiles() {
            Ok(profiles) => profiles,
            Err(e) => {
                error!("Failed to list sandbox profiles: {e}");
                Vec::new()
            }
        };
        let host = SandboxHost::local_shared();
        profiles
            .into_iter()
            .map(|p| SandboxProfileRow {
                resolved: resolve_backend(host, p.backend),
                name: p.name,
                backend: p.backend,
                paths: p.paths.len(),
                network: p.network_mode,
                // Place instances land with the sandbox transport (P3); until
                // then no profile has one and the column stays empty.
                instance: None,
            })
            .collect()
    }

    /// Open the sandbox-profile list modal.
    pub(crate) fn open_sandbox_list(&mut self) {
        let entries = self.sandbox_profile_rows();
        self.modal = modals::Modal::SandboxList(modals::SandboxListModal { index: 0, entries });
    }

    /// Rebuild the list rows after a mutation, keeping the cursor where it was.
    /// Unlike the automations list this stays open when it empties — the empty
    /// state carries the `n` hint, which is the only way back in.
    fn refresh_sandbox_list(&mut self) {
        let index = match self.modal {
            modals::Modal::SandboxList(ref sl) => sl.index,
            _ => return,
        };
        self.open_sandbox_list();
        if let modals::Modal::SandboxList(ref mut sl) = self.modal {
            sl.index = index.min(sl.entries.len().saturating_sub(1));
        }
    }

    /// Open the editor on a blank profile.
    pub(crate) fn open_sandbox_editor(&mut self) {
        let mut m = modals::SandboxEditorModal::default();
        self.resolve_sandbox_editor_backend(&mut m);
        self.modal = modals::Modal::SandboxEditor(Box::new(m));
    }

    /// Open the editor on a stored profile.
    fn open_edit_sandbox_profile(&mut self, name: &str) {
        match self.db.get_sandbox_profile(name) {
            Ok(Some(profile)) => {
                let mut m = modals::SandboxEditorModal::from_profile(&profile);
                self.resolve_sandbox_editor_backend(&mut m);
                self.modal = modals::Modal::SandboxEditor(Box::new(m));
            }
            Ok(None) => self.set_error(format!("Sandbox profile '{name}' no longer exists")),
            Err(e) => {
                error!("Failed to load sandbox profile '{name}': {e}");
                self.set_error("Failed to load sandbox profile");
            }
        }
    }

    /// Tell the editor what `auto` resolves to here, so the header shows
    /// `auto → seatbelt` and the place-only knobs grey out. Left `None` when
    /// nothing is available: an unresolved `auto` rules nothing out, which is
    /// the same exemption the profile validator makes.
    fn resolve_sandbox_editor_backend(&self, m: &mut modals::SandboxEditorModal) {
        m.resolved = resolve_backend(SandboxHost::local_shared(), m.backend);
    }

    /// Persist the editor's profile. Returns whether the modal may close: a
    /// validation failure keeps the form open with the message in the toast,
    /// because there is no inline form-error widget.
    fn save_sandbox_profile(&mut self, m: &modals::SandboxEditorModal) -> bool {
        let existing = match self.db.list_sandbox_profile_names() {
            Ok(names) => names,
            Err(e) => {
                error!("Failed to list sandbox profiles: {e}");
                self.set_error("Failed to read sandbox profiles");
                return false;
            }
        };
        let profile = match m.validated_profile(&existing) {
            Ok(profile) => profile,
            Err(message) => {
                self.set_error(message);
                return false;
            }
        };

        // The name *is* the storage key, so a rename is its own operation: it
        // rewrites the profile row, its instances and every session pointing at
        // it in one transaction. An upsert under the new name would leave the
        // old profile behind and strand those sessions on it.
        if let Some(old) = m.editing.as_deref().filter(|old| *old != profile.name) {
            match self.db.rename_sandbox_profile(old, &profile.name) {
                Ok(true) => {}
                Ok(false) => {
                    self.set_error(format!("Sandbox profile '{old}' no longer exists"));
                    return false;
                }
                Err(e) => {
                    error!("Failed to rename sandbox profile '{old}': {e}");
                    self.set_error("Failed to rename sandbox profile");
                    return false;
                }
            }
            self.rename_sandbox_profile_in_sessions(old, &profile.name);
        }

        if let Err(e) = self.db.upsert_sandbox_profile(&profile) {
            error!("Failed to save sandbox profile '{}': {e}", profile.name);
            self.set_error("Failed to save sandbox profile");
            return false;
        }
        self.set_status(
            StatusLevel::Success,
            format!("Sandbox profile '{}' saved", profile.name),
        );
        true
    }

    /// Follow a rename through the in-memory sessions. Storage rewrote the
    /// rows; the live [`SessionInfo`](crate::session::SessionInfo)s are a
    /// separate copy, and the next full-row write-back would put the old name
    /// straight back.
    fn rename_sandbox_profile_in_sessions(&mut self, from: &str, to: &str) {
        for session in &mut self.sessions {
            if session
                .info
                .sandbox_profile
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(from))
            {
                session.info.sandbox_profile = Some(to.to_string());
            }
        }
    }

    /// Delete a profile.
    ///
    /// Sessions referencing it keep the reference: clearing it would silently
    /// relaunch those agents on the host, so the count goes in the toast and
    /// the next relaunch fails loudly instead.
    fn delete_sandbox_profile_by_name(&mut self, name: &str) {
        let in_use = self
            .db
            .count_sessions_using_sandbox_profile(name)
            .unwrap_or(0);
        match self.db.delete_sandbox_profile(name) {
            Ok(true) => {
                let message = if in_use > 0 {
                    format!(
                        "Sandbox profile '{name}' deleted — {in_use} session(s) still \
                         reference it and will refuse to relaunch"
                    )
                } else {
                    format!("Sandbox profile '{name}' deleted")
                };
                // An orphaned reference is a launch failure waiting to
                // happen, so it reads as an error rather than a success.
                let level = if in_use > 0 {
                    StatusLevel::Error
                } else {
                    StatusLevel::Success
                };
                self.set_status(level, message);
            }
            Ok(false) => self.set_error(format!("Sandbox profile '{name}' not found")),
            Err(e) => {
                error!("Failed to delete sandbox profile '{name}': {e}");
                self.set_error("Failed to delete sandbox profile");
            }
        }
    }

    // ---- The new-session wizard's sandbox step ---------------------------

    /// Open the wizard's sandbox step for a session spanning `dirs`, or return
    /// `false` when there is nothing to ask about (no profiles stored).
    ///
    /// Runs after directory selection so it can rank the profiles that cover
    /// the chosen directories first and label the ones that do not
    /// (`docs/SANDBOX.md` §UI). Every profile stays selectable: the wizard
    /// knows the launch cwd, not what the user intends to reach from it.
    pub(crate) fn open_sandbox_picker(&mut self, dirs: &[std::path::PathBuf]) -> bool {
        let profiles = match self.db.list_sandbox_profiles() {
            Ok(profiles) if !profiles.is_empty() => profiles,
            Ok(_) => return false,
            Err(e) => {
                error!("Failed to list sandbox profiles: {e}");
                return false;
            }
        };
        let home = crate::paths::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let host = SandboxHost::local_shared();

        let mut choices = vec![SandboxChoice {
            label: "none".to_string(),
            profile: String::new(),
            detail: "run on the host, no boundary".to_string(),
            covers: true,
        }];
        let mut rows: Vec<SandboxChoice> = profiles
            .into_iter()
            .map(|p| {
                let covers = dirs
                    .iter()
                    .all(|dir| p.covers(&dir.to_string_lossy(), &home));
                let row = SandboxProfileRow {
                    resolved: resolve_backend(host, p.backend),
                    name: p.name.clone(),
                    backend: p.backend,
                    paths: p.paths.len(),
                    network: p.network_mode,
                    instance: None,
                };
                SandboxChoice {
                    label: p.name,
                    profile: row.name.clone(),
                    detail: row.summary(),
                    covers,
                }
            })
            .collect();
        // Covering profiles first, each group keeping storage's name order.
        rows.sort_by_key(|c| !c.covers);
        choices.extend(rows);

        let previous = self.new_session.sandbox_profile.as_deref().unwrap_or("");
        let selected_index = choices
            .iter()
            .position(|c| c.profile == previous)
            .unwrap_or(0);
        self.new_session.sandbox_step_shown = true;
        self.modal =
            modals::Modal::SandboxPicker(crate::ui::sandbox_picker_modal::SandboxPickerState {
                choices,
                selected_index,
                filter: Default::default(),
            });
        true
    }

    /// Keys for the wizard's sandbox step — the host picker's keymap: type to
    /// filter, ↑/↓ (or Ctrl+P/Ctrl+N) to move, Enter to choose, Esc to step
    /// back.
    pub(crate) fn handle_sandbox_picker_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let modals::Modal::SandboxPicker(ref mut sp) = self.modal else {
            return;
        };
        let count = sp.choices.len();
        let visible = sp.filter.len(count);
        match code {
            KeyCode::Esc if sp.filter.is_active() => sp.filter.clear(&mut sp.selected_index),
            KeyCode::Esc => {
                self.new_session.sandbox_step_shown = false;
                self.new_session.sandbox_profile = None;
                self.new_session_step_back();
            }
            KeyCode::Down if sp.selected_index + 1 < visible => sp.selected_index += 1,
            KeyCode::Up => sp.selected_index = sp.selected_index.saturating_sub(1),
            // Inert on a query with no matches (backspace to widen it).
            KeyCode::Enter => {
                let Some(real) = sp.filter.real_index(sp.selected_index, count) else {
                    return;
                };
                let profile = sp
                    .choices
                    .get(real)
                    .map(|c| c.profile.clone())
                    .unwrap_or_default();
                self.new_session.sandbox_profile = (!profile.is_empty()).then_some(profile);
                self.open_pending_session_name_modal();
            }
            KeyCode::Backspace => {
                let labels = sp.choices.iter().map(|c| c.label.as_str());
                sp.filter.pop(labels, &mut sp.selected_index);
            }
            KeyCode::Char('n') if mods.contains(KeyModifiers::CONTROL) => {
                if sp.selected_index + 1 < visible {
                    sp.selected_index += 1;
                }
            }
            KeyCode::Char('p') if mods.contains(KeyModifiers::CONTROL) => {
                sp.selected_index = sp.selected_index.saturating_sub(1);
            }
            KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                let labels = sp.choices.iter().map(|c| c.label.as_str());
                sp.filter.push(c, labels, &mut sp.selected_index);
            }
            _ => {}
        }
    }

    // ---- Key handling ----------------------------------------------------

    /// Keys for the profile list: `Esc` closes, `j`/`k` (and the arrows) move,
    /// `n` creates, `e`/`Enter` edits, `d` deletes.
    pub(crate) fn handle_sandbox_list_key(&mut self, code: KeyCode) {
        let modals::Modal::SandboxList(ref mut sl) = self.modal else {
            return;
        };
        match code {
            KeyCode::Esc => self.modal.close(),
            KeyCode::Char('j') | KeyCode::Down => {
                if sl.index + 1 < sl.entries.len() {
                    sl.index += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => sl.index = sl.index.saturating_sub(1),
            KeyCode::Char('n') => self.open_sandbox_editor(),
            KeyCode::Char('e') | KeyCode::Enter => {
                if let Some(name) = sl.selected_name().map(str::to_string) {
                    self.open_edit_sandbox_profile(&name);
                }
            }
            KeyCode::Char('d') => {
                if let Some(name) = sl.selected_name().map(str::to_string) {
                    self.delete_sandbox_profile_by_name(&name);
                    self.refresh_sandbox_list();
                }
            }
            _ => {}
        }
    }

    /// Keys for the profile editor. `Save` persists and returns to the list;
    /// `Cancel` returns to it without saving, so `n`/`e` are a round trip
    /// rather than a dead end.
    pub(crate) fn handle_sandbox_editor_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let modals::Modal::SandboxEditor(ref mut m) = self.modal else {
            return;
        };
        match m.handle_key(code, mods) {
            modals::EditorOutcome::Continue => {
                // The chosen backend can change with a keystroke, and every
                // capability row's availability follows what `auto` resolves to.
                self.refresh_sandbox_editor_resolution();
            }
            modals::EditorOutcome::Cancel => self.open_sandbox_list(),
            modals::EditorOutcome::Save => {
                let form = m.clone();
                if self.save_sandbox_profile(&form) {
                    self.open_sandbox_list();
                }
            }
        }
    }

    /// Re-resolve the open editor's `auto` after a backend change.
    fn refresh_sandbox_editor_resolution(&mut self) {
        let resolved = match self.modal {
            modals::Modal::SandboxEditor(ref m) => {
                resolve_backend(SandboxHost::local_shared(), m.backend)
            }
            _ => return,
        };
        if let modals::Modal::SandboxEditor(ref mut m) = self.modal {
            m.resolved = resolved;
        }
    }
}

/// What `requested` resolves to on `host`, or `None` when nothing on the ladder
/// is available. Only `auto` needs resolving — an explicitly chosen backend is
/// already the answer, and a pin never falls back.
fn resolve_backend(
    host: &SandboxHost,
    requested: SandboxBackendKind,
) -> Option<SandboxBackendKind> {
    match requested {
        SandboxBackendKind::Auto => host.select(requested).chosen,
        explicit => Some(explicit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{NetworkMode, SandboxPath};

    #[test]
    fn an_explicit_backend_resolves_to_itself_without_probing() {
        let host = SandboxHost::new(std::sync::Arc::new(
            crate::sandbox::probe::StubHost::default(),
        ));
        assert_eq!(
            resolve_backend(&host, SandboxBackendKind::Seatbelt),
            Some(SandboxBackendKind::Seatbelt)
        );
        // An unknown host offers nothing, so `auto` has nothing to resolve to —
        // which the editor reads as "rule nothing out", not as a failure.
        assert_eq!(resolve_backend(&host, SandboxBackendKind::Auto), None);
    }

    #[test]
    fn auto_resolves_down_the_hosts_ladder() {
        let host = SandboxHost::new(std::sync::Arc::new(
            crate::sandbox::probe::StubHost::linux_with_bwrap("0.11.0"),
        ));
        assert_eq!(
            resolve_backend(&host, SandboxBackendKind::Auto),
            Some(SandboxBackendKind::Bwrap)
        );
    }

    /// The list row is what the user reads before launching a session under a
    /// profile, so it has to agree with what storage holds.
    #[test]
    fn list_rows_report_each_profiles_own_shape() {
        let db = crate::storage::Database::open_in_memory().unwrap();
        let mut profile = SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("~/dev/lib"),
            ],
        );
        profile.backend = SandboxBackendKind::Bwrap;
        profile.network_mode = NetworkMode::Full;
        db.upsert_sandbox_profile(&profile).unwrap();

        let profiles = db.list_sandbox_profiles().unwrap();
        let host = SandboxHost::new(std::sync::Arc::new(
            crate::sandbox::probe::StubHost::linux_with_bwrap("0.11.0"),
        ));
        let rows: Vec<SandboxProfileRow> = profiles
            .into_iter()
            .map(|p| SandboxProfileRow {
                resolved: resolve_backend(&host, p.backend),
                name: p.name,
                backend: p.backend,
                paths: p.paths.len(),
                network: p.network_mode,
                instance: None,
            })
            .collect();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "dev");
        assert_eq!(rows[0].paths, 2);
        assert_eq!(rows[0].network, NetworkMode::Full);
        // An explicit backend renders without an arrow; only `auto` gets one.
        assert!(!rows[0].summary().contains('→'), "{}", rows[0].summary());
    }
}
