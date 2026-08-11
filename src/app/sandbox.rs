//! Sandbox-profile cluster for the Friring TUI application.
//!
//! The `impl App` methods behind `docs/SANDBOX.md` §UI: the profile list modal,
//! the profile editor, and the create/edit/delete operations against
//! [`crate::storage::sandboxes`]. Mirrors [`super::automation`], which is the
//! collection-editing pattern this screen clones.
//!
//! It is also where the egress firewall reaches the user: the tick drains
//! [`crate::sandbox::egress`]'s refusals here, turns them into status lines and
//! first-use questions, and writes an answer back to both the running proxy and
//! the stored profile.
//!
//! Rendering lives in [`crate::ui::sandbox_list_modal`],
//! [`crate::ui::sandbox_editor_modal`] and
//! [`crate::ui::sandbox_domain_modal`]; the form state itself is
//! [`modals::SandboxEditorModal`] and the question is
//! [`DomainPrompt`](super::egress_prompts::DomainPrompt).

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyModifiers};
use tracing::error;

use super::egress_prompts::{DomainPrompt, Observed};
use super::modals;
use super::{App, StatusLevel};
use crate::proxy::DenyReason;
use crate::sandbox::SandboxHost;
use crate::session::{DomainRule, SandboxBackendKind, SandboxProfile};
use crate::ui::sandbox_list_modal::{PendingPlaceAction, PlaceAction, PlaceRow, SandboxProfileRow};
use crate::ui::sandbox_picker_modal::SandboxChoice;

impl App {
    /// Load the profile a session row names, for a launch that rebuilds the
    /// boundary from persisted state.
    ///
    /// # Errors
    ///
    /// Either the name is set but no such profile is stored — deleting a
    /// profile deliberately leaves referencing sessions dangling, so that a
    /// relaunch fails here rather than quietly running the agent on the host —
    /// or the stored row did not decode, in which case part of its policy is a
    /// substituted guess and the profile is repairable rather than runnable
    /// ([`StoredSandboxProfile`](crate::storage::sandboxes::StoredSandboxProfile)).
    pub(crate) fn load_session_sandbox(
        &self,
        name: Option<&str>,
    ) -> Result<Option<SandboxProfile>, String> {
        let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
            return Ok(None);
        };
        match self.db.get_sandbox_profile(name) {
            Ok(Some(stored)) => match stored.launch_refusal() {
                Some(refusal) => Err(refusal),
                None => Ok(Some(stored.profile)),
            },
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
            .map(|p| {
                let places = self.place_rows(&p.profile.name);
                profile_row(host, &p, places)
            })
            .collect()
    }

    /// A profile's live places, most recently used first.
    ///
    /// A profile may own several — a rebuild adds a row rather than overwriting
    /// the previous id, which is what keeps the superseded container findable —
    /// so the manager view lists them all and the first is the one describing
    /// the place new sessions land in.
    fn place_rows(&self, profile: &str) -> Vec<PlaceRow> {
        let mut rows = self
            .db
            .list_sandbox_instances_for_profile(profile)
            .unwrap_or_default();
        rows.sort_by_key(|row| std::cmp::Reverse(row.last_used_at));
        rows.into_iter()
            .map(|row| PlaceRow {
                engine: row.engine,
                id: row.external_id,
                state: row.state,
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
    ///
    /// A row that did not decode opens too — the editor is the only repair
    /// there is — pre-filled with the narrow values storage substituted and
    /// carrying the list of columns it substituted them for, so a save is an
    /// informed repair rather than a silent one.
    fn open_edit_sandbox_profile(&mut self, name: &str) {
        match self.db.get_sandbox_profile(name) {
            Ok(Some(stored)) => {
                let mut m = modals::SandboxEditorModal::from_profile(&stored.profile);
                m.undecoded = stored.undecoded.iter().map(ToString::to_string).collect();
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
        let host = SandboxHost::local_shared();
        m.resolved = resolve_backend(host, m.backend);
        m.backend_unavailable = backend_unavailable(host, m.backend, m.resolved);
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
    ///
    /// The opened-container map is keyed on the same `sandbox:<profile>` name
    /// `sessions.backend_type` holds, so it moves with the rename too: left
    /// behind, its ids would be filed under a backend name no session has any
    /// more, and the reclaiming pass would fall back to protecting the profile
    /// whole ([`places_in_use`](Self::places_in_use)) rather than the containers
    /// this instance actually opened.
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
        let old = format!("{}{from}", crate::session::SANDBOX_BACKEND_PREFIX);
        if let Some(opened) = self.place_containers.remove(&old) {
            self.place_containers
                .entry(format!("{}{to}", crate::session::SANDBOX_BACKEND_PREFIX))
                .or_default()
                .extend(opened);
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
                // The profile's own tree — its synthetic home, the credential
                // it was signed into and every session directory inside it —
                // belongs to the profile, so it goes with it. The *place* does
                // not: nothing here stops a running container, and the
                // reclaiming pass is what removes one whose profile is gone.
                //
                // Which is exactly why a profile with sessions still on it keeps
                // its tree. That tree is bind-mounted into a container this
                // delete does not stop: removing it takes `$HOME` out from under
                // an agent mid-turn, unlinks the egress sockets its siblings are
                // talking through, and destroys the login. The reclaiming pass
                // removes the container first and the tree goes with the next
                // one, so the outcome is the same a moment later — without
                // reaching inside a live boundary to get it.
                let reclaimed = in_use == 0;
                if reclaimed {
                    crate::sandbox::dirs::cleanup_place(name);
                    // …and with the copy gone, so is friring's record that this
                    // profile held the one permitted seed of a credential
                    // family. Leaving it would refuse every future profile a
                    // `seed-file` on behalf of a boundary that no longer exists
                    // (ADR-28). Kept while the tree is: the copy is still there,
                    // and releasing the family would let a second profile take a
                    // second copy of one credential.
                    crate::sandbox::auth::release_seeds(name);
                }
                let message = if in_use > 0 {
                    format!(
                        "Sandbox profile '{name}' deleted — {in_use} session(s) still \
                         reference it and will refuse to relaunch; its sandbox home and login \
                         are kept until they stop"
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

    // ---- Reclaiming places -------------------------------------------------

    /// Reclaim the places nothing needs any more, on a slow background pass.
    ///
    /// A place outlives the launch that made it and the friring that made it, so
    /// something has to reconcile the `sandbox_instances` table against what the
    /// engines actually hold: rows whose container is gone, containers whose
    /// profile is gone, and containers a profile edit superseded — an edited
    /// profile builds a *new* container by design (the spec digest is in the
    /// name), which is precisely what leaves the old one behind.
    ///
    /// Every step is a container-engine command, so the whole pass runs on a
    /// worker; the decision itself is pure
    /// ([`gc_plan`](crate::sandbox::container::gc_plan)) and the worker only
    /// carries it out.
    pub(crate) fn tick_sandbox_gc(&mut self) {
        if self.metrics.tick_count % SANDBOX_GC_INTERVAL_TICKS != SANDBOX_GC_OFFSET_TICKS {
            return;
        }
        if self.sandbox_gc.in_progress() {
            return;
        }
        let Some(sweep) = self.sandbox_gc_input(None) else {
            return;
        };
        self.start_sandbox_job(move || sweep.run());
    }

    /// Put a place job on the single background slot the reclaiming pass uses,
    /// answering whether it started.
    ///
    /// One slot, because every one of these drives a container engine: a pass
    /// and a manual rebuild running at once would have two workers deciding
    /// what to remove from one engine's list of containers.
    fn start_sandbox_job(&mut self, job: impl FnOnce() -> GcOutcome + Send + 'static) -> bool {
        if self.sandbox_gc.in_progress() {
            self.set_error("A sandbox place job is already running — wait for it to finish");
            return false;
        }
        let tx = self.sandbox_gc.start();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
        true
    }

    /// Everything the pass needs from this instance, gathered on the UI thread:
    /// the profiles that could own a place, the recorded rows, and the places
    /// that must survive whatever else is true of them.
    ///
    /// `None` when there is nothing a pass could act on, so an installation with
    /// no place profiles never spawns a worker.
    ///
    /// `only` narrows the pass to one profile — the manager view's per-row
    /// prune. The whole-installation decisions (collecting the place *trees* of
    /// profiles that no longer exist) are skipped there: they are about every
    /// profile, and answering them from one row's key would reclaim things the
    /// user was not looking at.
    fn sandbox_gc_input(&self, only: Option<String>) -> Option<GcSweep> {
        let stored = self.db.list_sandbox_profiles().unwrap_or_default();
        // Every name, before either filter below narrows it: the pass reclaims a
        // place *tree* by elimination, and a profile that is merely unreadable
        // or has been edited onto a policy backend still owns the login in its
        // own tree.
        let known: Vec<String> = stored.iter().map(|row| row.profile.name.clone()).collect();
        let profiles: Vec<SandboxProfile> = stored
            .into_iter()
            // A row friring could not decode is repairable, not runnable — and
            // guessing at its spec here would compare a place against a policy
            // nobody wrote. Left alone, which keeps its place alive.
            .filter(super::super::storage::sandboxes::StoredSandboxProfile::is_intact)
            .map(|stored| stored.profile)
            // A profile pinned to a policy backend can never own a place, so it
            // is not a reason to ask an engine anything. `auto` stays in: on a
            // host with no policy backend its ladder ends at one.
            .filter(|profile| profile.backend.shape() != Some(crate::session::SandboxShape::Policy))
            .collect();
        let records: Vec<(
            SandboxBackendKind,
            crate::sandbox::container::InstanceRecord,
        )> = crate::sandbox::PLACE_KINDS
            .iter()
            .copied()
            .flat_map(|engine| {
                self.db
                    .list_sandbox_instances_for_engine(engine)
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |row| {
                        (
                            engine,
                            crate::sandbox::container::InstanceRecord {
                                profile: row.profile,
                                external_id: row.external_id,
                                last_used_at: row.last_used_at,
                            },
                        )
                    })
            })
            .collect();
        if profiles.is_empty() && records.is_empty() {
            return None;
        }
        Some(GcSweep {
            profiles,
            known,
            records,
            in_use: self.places_in_use(),
            only,
        })
    }

    /// What a pass may not touch: the places live sessions are in.
    ///
    /// Two sources, and the second is the careful one. This instance knows every
    /// container it has opened for a profile, so a live session of that profile
    /// protects **all** of them: a session row names only the profile, and an
    /// edited profile leaves earlier sessions running in the container they were
    /// launched into. It knows nothing about a session another friring is
    /// driving — only that a row for it exists — so a profile with a live
    /// session row this instance never opened a place for protects **every**
    /// container of that profile by name. Either way the cost is a superseded
    /// container surviving until those sessions end; the alternative is pulling
    /// a place out from under a running agent.
    ///
    /// The names are lowercased, because the three spellings this has to line up
    /// need not agree: a session records the spelling it was created with, a
    /// container carries the one its label was baked with, and the profile row
    /// carries the one it is stored under.
    fn places_in_use(&self) -> PlacesInUse {
        let mut ids: HashSet<String> = HashSet::new();
        let mut profiles: HashSet<String> = HashSet::new();
        for shared in self.db.list_active_sessions().unwrap_or_default() {
            let Some(profile) = crate::session::sandbox_backend_profile(&shared.backend_type)
            else {
                continue;
            };
            match self.place_containers.get(&shared.backend_type) {
                Some(opened) => ids.extend(opened.iter().cloned()),
                None => {
                    profiles.insert(profile.to_ascii_lowercase());
                }
            }
        }
        PlacesInUse { ids, profiles }
    }

    /// Apply a finished pass: forget the rows it reconciled away, re-record the
    /// containers it adopted, and report only what went wrong.
    ///
    /// A pass the *user* asked for also reports what it did — a background
    /// sweep that reclaimed nothing should stay silent, but a keystroke that
    /// appears to do nothing reads as a broken key.
    pub(crate) fn poll_sandbox_gc(&mut self) {
        let super::background::TaskPoll::Done(outcome) = self.sandbox_gc.poll() else {
            return;
        };
        for (engine, external_id) in outcome.forget {
            if let Err(e) = self.db.delete_sandbox_instance(engine, &external_id) {
                error!("Failed to forget sandbox instance: {e}");
            }
        }
        for row in outcome.adopt {
            if let Err(e) = self.db.upsert_sandbox_instance(&row) {
                error!("Failed to adopt sandbox instance: {e}");
            }
        }
        // The list is showing places that have just changed underneath it.
        if matches!(self.modal, modals::Modal::SandboxList(_)) {
            self.refresh_sandbox_list();
        }
        for failure in &outcome.failures {
            // A place that will not go is the next pass's problem, but the user
            // is the one paying for the disk, so it is said once rather than
            // only logged.
            self.set_error(format!("Could not reclaim a sandbox place: {failure}"));
        }
        if let Some(report) = outcome.report.filter(|_| outcome.failures.is_empty()) {
            self.set_status(StatusLevel::Success, report);
        }
    }

    // ---- The manager view's place actions ---------------------------------

    /// The confirmation the selected row is waiting on, if any.
    fn pending_place_action(&self) -> Option<PendingPlaceAction> {
        match self.modal {
            modals::Modal::SandboxList(ref sl) => sl.selected().and_then(|row| row.pending.clone()),
            _ => None,
        }
    }

    /// Arm a destructive place action on the selected row, with the question it
    /// has to answer.
    ///
    /// The counts are read here rather than in the renderer, so the sentence the
    /// user reads is the one friring is actually about to act on.
    ///
    /// The place count comes from the **engines**, not from `sandbox_instances`:
    /// a row whose container is already gone would inflate it and an adopted
    /// container no row describes would hide from it, and the job this question
    /// authorises acts on what the engines hold either way. Asked here, on the
    /// keystroke, rather than on the background slot the job itself takes: a
    /// destructive confirmation that armed itself asynchronously could land on
    /// whichever row the selection had reached by then. It costs one container
    /// listing per **installed** engine — [`live_places_here`] asks the cached
    /// probe first, so a host with no engine spawns nothing.
    ///
    /// The recorded ids go with the profile name for the reason the job's own
    /// filter takes them: an engine cannot relabel a running container, so
    /// after a rename the row is the only half that says the new name.
    ///
    /// [`live_places_here`]: crate::sandbox::live_places_here
    fn request_place_action(&mut self, action: PlaceAction) {
        let Some(name) = (match self.modal {
            modals::Modal::SandboxList(ref sl) => sl.selected().map(|row| row.name.clone()),
            _ => None,
        }) else {
            return;
        };
        let recorded = self.recorded_place_ids(&name);
        let places = crate::agent::sandboxing::running_places(&name, &recorded).len();
        if action == PlaceAction::Stop && places == 0 {
            self.set_status(
                StatusLevel::Info,
                format!("Sandbox profile '{name}' has no live place to stop"),
            );
            return;
        }
        let sessions = self
            .db
            .count_sessions_using_sandbox_profile(&name)
            .unwrap_or(0);
        // What "sessions" means here is what the count means: sessions that
        // *reference the profile*. A place-backed one is in one of these
        // containers; a policy-backed one never was, and over-stating it is the
        // safe direction for a question about removing them.
        let cost = match sessions {
            0 => "no session is using it".to_string(),
            n => format!("{n} session(s) using it stop with them"),
        };
        let question = match action {
            PlaceAction::Stop => {
                format!("Stop and remove the {places} place(s) '{name}' is running? {cost}.")
            }
            PlaceAction::Rebuild => format!(
                "Rebuild '{name}'? the {places} place(s) it is running go and a fresh one starts \
                 from the profile as it is now — {cost}."
            ),
        };
        self.clear_place_confirmations();
        if let modals::Modal::SandboxList(ref mut sl) = self.modal {
            let index = sl.index;
            if let Some(row) = sl.entries.get_mut(index) {
                row.pending = Some(PendingPlaceAction {
                    action,
                    profile: name,
                    question,
                });
            }
        }
        self.request_redraw();
    }

    /// Answer an armed action. `y` alone carries it out — see
    /// [`handle_sandbox_list_key`](Self::handle_sandbox_list_key).
    fn answer_place_action(&mut self, pending: &PendingPlaceAction, code: KeyCode) {
        self.clear_place_confirmations();
        self.request_redraw();
        if !matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.set_status(
                StatusLevel::Info,
                format!(
                    "{} of '{}' cancelled",
                    pending.action.verb(),
                    pending.profile
                ),
            );
            return;
        }
        self.start_place_action(&pending.profile, pending.action);
    }

    fn clear_place_confirmations(&mut self) {
        if let modals::Modal::SandboxList(ref mut sl) = self.modal {
            for row in &mut sl.entries {
                row.pending = None;
            }
        }
    }

    /// Drop a question the selection has moved away from — see
    /// [`handle_sandbox_list_key`](Self::handle_sandbox_list_key).
    fn drop_unselected_place_confirmations(&mut self) {
        if let modals::Modal::SandboxList(ref mut sl) = self.modal {
            let selected = sl.index;
            for (index, row) in sl.entries.iter_mut().enumerate() {
                if index != selected {
                    row.pending = None;
                }
            }
        }
    }

    /// The `sandbox_instances` ids friring holds for `name`, without their
    /// engines — what a question and a teardown need to find a container whose
    /// label a rename left behind.
    fn recorded_place_ids(&self, name: &str) -> Vec<String> {
        self.db
            .list_sandbox_instances_for_profile(name)
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.external_id)
            .collect()
    }

    /// Remove a profile's places, and for a rebuild start a fresh one.
    ///
    /// The profile is **re-read** rather than taken from the list row: it may
    /// have been edited since the list was built, and a rebuild has to start the
    /// place the profile describes now. A profile that no longer exists or no
    /// longer decodes refuses here, in the words every other launch path uses.
    fn start_place_action(&mut self, name: &str, action: PlaceAction) {
        let profile = match self.load_session_sandbox(Some(name)) {
            Ok(Some(profile)) => profile,
            Ok(None) => return,
            Err(message) => {
                self.set_error(message);
                return;
            }
        };
        let records: Vec<(SandboxBackendKind, String)> = self
            .db
            .list_sandbox_instances_for_profile(name)
            .unwrap_or_default()
            .into_iter()
            .map(|row| (row.engine, row.external_id))
            .collect();
        let job = PlaceJob {
            profile,
            records,
            restart: action == PlaceAction::Rebuild,
        };
        if self.start_sandbox_job(move || job.run()) {
            self.set_status(
                StatusLevel::Info,
                format!("{} of sandbox profile '{name}' started…", action.verb()),
            );
        }
    }

    /// Reclaim what nothing needs, now — the background pass on demand, scoped
    /// to one profile when the manager view asked for it.
    fn start_place_reclaim(&mut self, only: Option<String>) {
        let label = only
            .as_deref()
            .map(|name| format!("sandbox profile '{name}'"))
            .unwrap_or_else(|| "every sandbox profile".to_string());
        let Some(sweep) = self.sandbox_gc_input(only) else {
            self.set_status(StatusLevel::Info, "There are no sandbox places to reclaim");
            return;
        };
        if self.start_sandbox_job(move || sweep.run()) {
            self.set_status(StatusLevel::Info, format!("Reclaiming places for {label}…"));
        }
    }

    // ---- The egress firewall's first-use prompt ---------------------------

    /// Drain the egress proxy's refusals and act on them, then raise the next
    /// queued question if the single modal slot is free.
    ///
    /// Polled from the tick rather than pushed, because the proxies run on
    /// their own runtime and the TUI owns no async context here — the same
    /// shape every other background signal arrives in.
    ///
    /// Every newly refused host reaches the status bar, whatever the reason and
    /// whatever the profile says: an agent that cannot reach the network is
    /// failing, and the user needs the reason more than the silence. Only
    /// [`DenyReason::NotAllowlisted`] on a profile that asked to be asked also
    /// becomes a question.
    pub(crate) fn tick_sandbox_egress(&mut self) {
        let denials = crate::sandbox::egress::take_denials();
        if !denials.is_empty() {
            self.report_sandbox_denials(denials);
        }
        self.open_next_domain_prompt();
    }

    /// Report one drain's refusals: one status line, and a queued question for
    /// each host a profile asked about.
    fn report_sandbox_denials(&mut self, denials: Vec<crate::sandbox::SessionDenial>) {
        // This is the only path that grows the history, so it is where a
        // session that has gone away stops being remembered.
        let live: HashSet<String> = self
            .sessions
            .iter()
            .map(|s| s.info.id.to_string())
            .collect();
        self.egress_prompts.retain_sessions(&live);

        let mut reported: Vec<String> = Vec::new();
        for denial in denials {
            match self
                .egress_prompts
                .observe(&denial.session_key, &prompt_key(&denial.event.host))
            {
                // Said once already. An agent retrying a blocked host in a loop
                // must not own the status bar.
                Observed::Repeat => continue,
                Observed::Saturated => {
                    reported.push(format!(
                        "{} was refused too many different hosts — friring has stopped \
                         reporting them",
                        self.sandbox_session_label(&denial.session_key)
                    ));
                    continue;
                }
                Observed::First => {}
            }
            self.queue_domain_prompt(&denial);
            reported.push(format!(
                "{} was blocked reaching {} — {}",
                self.sandbox_session_label(&denial.session_key),
                describe_destination(&denial.event.host, denial.event.port),
                denial.event.reason
            ));
        }

        let Some(first) = reported.first() else {
            return;
        };
        // One line per drain, not per refusal: the status bar holds one message,
        // and a burst that overwrote itself would leave whichever arrived last.
        let message = match reported.len() {
            1 => first.clone(),
            n => format!("{first} (+{} more blocked)", n - 1),
        };
        self.set_status(StatusLevel::Error, message);
    }

    /// Queue a first-use question for this refusal, if it is one.
    ///
    /// Four things have to hold, and each of them fails towards *not* asking:
    /// the reason is "not in the allowlist" (a deny rule, `network = none` or a
    /// rejected token are answers the user already gave, and
    /// [`DenyReason::UnsupportedHost`] must never be asked about — its answer
    /// would be a rule the profile validator refuses to store); friring still
    /// holds the session, so there is a profile to write to; that profile
    /// decodes and carries `prompt_new_domains`; and the host can be spelled as
    /// a rule at all.
    fn queue_domain_prompt(&mut self, denial: &crate::sandbox::SessionDenial) {
        if !matches!(denial.event.reason, DenyReason::NotAllowlisted) {
            return;
        }
        let Some(session) = self
            .sessions
            .iter()
            .find(|s| s.info.id.to_string() == denial.session_key)
        else {
            return;
        };
        let session_name = session.info.name.clone();
        let Some(profile_name) = session.info.sandbox_profile.clone() else {
            return;
        };
        let Some(rule) = allow_rule_for(&denial.event.host, denial.event.port) else {
            return;
        };
        // Re-read rather than trusting the launch: this is the profile the
        // answer is written back to, and a deleted or undecodable one has
        // nowhere to put it.
        match self.load_session_sandbox(Some(&profile_name)) {
            Ok(Some(profile)) if profile.prompt_new_domains => {}
            _ => return,
        }
        self.egress_prompts.enqueue(DomainPrompt {
            session_key: denial.session_key.clone(),
            session_name,
            profile: profile_name,
            host: denial.event.host.clone(),
            port: denial.event.port,
            rule,
            shown_at: None,
        });
    }

    /// Drop everything friring is holding about one session's refusals,
    /// including a question already on screen.
    ///
    /// Called where the boundary is rebuilt. [`EgressPromptState::forget`]
    /// clears the history and the queue, but a question that has been *popped*
    /// into the modal is in neither — and the session key is stable across a
    /// relaunch by design, so answering it would apply a grant to the instance
    /// that replaced the one the question was about, and write it into whatever
    /// profile the question named rather than the one now in force. The
    /// question is withdrawn instead: the new boundary will ask again if the
    /// agent asks again.
    pub(crate) fn forget_egress_prompts(&mut self, session_key: &str) {
        self.egress_prompts.forget(session_key);
        if matches!(
            self.modal,
            modals::Modal::SandboxDomainPrompt(ref prompt) if prompt.session_key == session_key
        ) {
            self.modal.close();
            self.request_redraw();
        }
    }

    /// Put the next queued question on screen once nothing else is.
    ///
    /// Only one modal is ever open, so a refusal arriving while the user is in
    /// the profile editor (or answering the previous question) waits its turn
    /// instead of stealing the screen mid-edit.
    fn open_next_domain_prompt(&mut self) {
        if self.modal.is_open() {
            return;
        }
        if let Some(mut prompt) = self.egress_prompts.next_prompt() {
            // Stamped here rather than where it was queued: the arming window
            // is time on screen, and a question can wait behind another modal
            // for minutes.
            prompt.shown_at = Some(std::time::Instant::now());
            self.modal = modals::Modal::SandboxDomainPrompt(prompt);
            self.request_redraw();
        }
    }

    /// How a refusal names its session: its display name, or the launch key for
    /// one friring no longer holds (a session deleted between the request and
    /// the drain).
    fn sandbox_session_label(&self, session_key: &str) -> String {
        self.sessions
            .iter()
            .find(|s| s.info.id.to_string() == session_key)
            .map(|s| format!("Sandboxed session '{}'", s.info.name))
            .unwrap_or_else(|| format!("Sandboxed session {session_key}"))
    }

    /// Act on the answer to a first-use question.
    ///
    /// "Allow" reaches the running proxy **first**. That order is what makes the
    /// agent's next attempt succeed with no restart, and it is also the check:
    /// a session torn down since the question was raised, or a rule the proxy
    /// will not load, must not leave the stored profile permanently wider on the
    /// strength of an answer that changed nothing. The profile write follows,
    /// and if *it* fails the live grant stands — the user allowed it, this run
    /// has it, and the message says the profile did not keep it.
    fn answer_domain_prompt(&mut self, prompt: DomainPrompt, allow: bool) {
        if !allow {
            self.set_status(
                StatusLevel::Info,
                format!(
                    "{} stays blocked for '{}'",
                    describe_destination(&prompt.host, prompt.port),
                    prompt.session_name
                ),
            );
            return;
        }
        if let Err(message) =
            crate::sandbox::egress::allow_domain(&prompt.session_key, &prompt.rule)
        {
            self.set_error(format!(
                "Could not allow {} for '{}': {message}",
                prompt.rule, prompt.session_name
            ));
            return;
        }
        match self.add_domain_to_profile(&prompt.profile, &prompt.rule) {
            Ok(()) => self.set_status(
                StatusLevel::Success,
                format!(
                    "Allowed {} for '{}' and added it to sandbox profile '{}'",
                    prompt.rule, prompt.session_name, prompt.profile
                ),
            ),
            Err(message) => self.set_error(format!(
                "Allowed {} for '{}' for this run only — {message}",
                prompt.rule, prompt.session_name
            )),
        }
    }

    /// Add one allow rule to a stored profile, so the grant survives the next
    /// launch.
    ///
    /// The profile is re-read here rather than carried on the prompt: it may
    /// have been edited (or deleted) while the question waited, and writing a
    /// stale copy back would silently undo those edits. A profile that no longer
    /// decodes is refused for the same reason — the full-row write would replace
    /// the user's unreadable values with the narrow ones storage substituted for
    /// them.
    fn add_domain_to_profile(&self, name: &str, rule: &str) -> Result<(), String> {
        let mut profile = match self.db.get_sandbox_profile(name) {
            Ok(Some(stored)) if stored.launch_refusal().is_none() => stored.profile,
            Ok(Some(_)) => {
                return Err(format!(
                    "sandbox profile '{name}' no longer reads back, so it was left alone"
                ))
            }
            Ok(None) => return Err(format!("sandbox profile '{name}' no longer exists")),
            Err(e) => {
                error!("Failed to load sandbox profile '{name}': {e}");
                return Err(format!("sandbox profile '{name}' could not be read"));
            }
        };
        // An entry that already means this is left alone: two spellings of one
        // rule read as two grants in the editor and enforce exactly one.
        let covered = profile
            .network_allow
            .iter()
            .any(|entry| DomainRule::parse(entry).is_ok_and(|parsed| parsed.to_string() == rule));
        if covered {
            return Ok(());
        }
        profile.network_allow.push(rule.to_string());
        self.db.upsert_sandbox_profile(&profile).map_err(|e| {
            error!("Failed to save sandbox profile '{name}': {e}");
            format!("sandbox profile '{name}' could not be saved")
        })
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
            .map(|stored| {
                let covers = dirs
                    .iter()
                    .all(|dir| stored.profile.covers(&dir.to_string_lossy(), &home));
                let row = profile_row(host, &stored, Vec::new());
                SandboxChoice {
                    label: stored.profile.name,
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
    /// `n` creates, `e`/`Enter` edits, `d` deletes — and the manager's own
    /// `s` stop, `r` rebuild, `p` prune.
    ///
    /// A stop or a rebuild is confirmed first, and **`y` is the only answer
    /// that carries it out**: `Enter` and `d` already mean something else on
    /// this list, so letting either double as "yes" would take a container away
    /// from a running agent with a keystroke that means edit or delete.
    /// Anything else cancels, and so does rebuilding the list.
    pub(crate) fn handle_sandbox_list_key(&mut self, code: KeyCode) {
        // A click moves the selection without passing through here, so a
        // question armed on the row that *was* selected would be left armed and
        // invisible — the footer only shows the selected row's. Dropped, which
        // is the direction that does not remove a container.
        self.drop_unselected_place_confirmations();
        if let Some(pending) = self.pending_place_action() {
            self.answer_place_action(&pending, code);
            return;
        }
        let modals::Modal::SandboxList(ref mut sl) = self.modal else {
            return;
        };
        match code {
            KeyCode::Char('s') => self.request_place_action(PlaceAction::Stop),
            KeyCode::Char('r') => self.request_place_action(PlaceAction::Rebuild),
            KeyCode::Char('p') => {
                if let Some(name) = sl.selected_name().map(str::to_string) {
                    self.start_place_reclaim(Some(name));
                }
            }
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
        // Answered here rather than in the form's own key table, because the
        // answer is not the form's: it needs the agent registry, friring's own
        // configuration directory and the host's home. `Ctrl+L` and not a bare
        // letter, because every other key in this editor is text input.
        if mods.contains(KeyModifiers::CONTROL)
            && matches!(code, KeyCode::Char('l') | KeyCode::Char('L'))
        {
            self.refresh_sandbox_editor_lint();
            return;
        }
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

    /// Keys for the first-use domain question: `y` allows the host and writes
    /// it into the profile, `Esc`/`n`/`Enter` leave it blocked. Either answer
    /// closes the question for good — the host is already settled, so a retry
    /// will not raise it again.
    ///
    /// Two departures from every other confirmation in friring, both because
    /// this is the only one an *agent* can cause to appear (see
    /// [`PROMPT_ARMING`]):
    ///
    /// - **`Enter` does not grant.** It is the most-pressed key in an agent
    ///   pane, which is where the user's hands are when this arrives, so it
    ///   means the safe answer instead of the standing-default one.
    /// - **Nothing is answered until the question has been on screen.** Before
    ///   that every key is swallowed rather than acted on: a keystroke already
    ///   in flight when the modal appeared belongs to the pane behind it, not
    ///   to a security grant. Swallowed rather than passed through, so it does
    ///   not reach the agent either.
    pub(crate) fn handle_sandbox_domain_prompt_key(&mut self, code: KeyCode) {
        let modals::Modal::SandboxDomainPrompt(ref prompt) = self.modal else {
            return;
        };
        if !prompt.is_armed() {
            return;
        }
        let allow = match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => true,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n') | KeyCode::Char('N') => false,
            _ => return,
        };
        let prompt = prompt.clone();
        self.modal.close();
        self.answer_domain_prompt(prompt, allow);
    }

    /// Run the config-projection lint over the form as it reads now, and hang
    /// the verdicts on the editor.
    ///
    /// The deferred half of `docs/SANDBOX.md` §Config projection: `plan` reads
    /// the user's configuration off disk and classifies every entry, which is
    /// the one derived value in this editor that cannot be recomputed per
    /// keystroke. So it is asked for, and what comes back says which form it
    /// answered about.
    ///
    /// Only for a **place**: a policy sandbox is on the host's own filesystem,
    /// so the agent reads its configuration where it always did and nothing is
    /// projected at all. An unresolved `auto` has no answer yet either.
    fn refresh_sandbox_editor_lint(&mut self) {
        let (profile, backend) = match self.modal {
            modals::Modal::SandboxEditor(ref m) => match (m.build_profile(), m.effective_shape()) {
                (Ok(profile), Some(crate::session::SandboxShape::Place)) => {
                    (profile, m.effective_backend())
                }
                (Ok(_), _) => {
                    self.set_status(
                        StatusLevel::Info,
                        "Config projection is a place's — a policy sandbox reads the host's own \
                         configuration",
                    );
                    return;
                }
                // A form that cannot be turned into a profile cannot be linted
                // either, and the message is the one a save would give.
                (Err(e), _) => {
                    self.set_error(e);
                    return;
                }
            },
            _ => return,
        };
        let home = crate::paths::home_dir()
            .map(|home| home.display().to_string())
            .unwrap_or_default();
        let reports = sandbox_lint_reports(&profile, backend, &self.agents, &home);
        let asked = reports.len();
        if let modals::Modal::SandboxEditor(ref mut m) = self.modal {
            m.lint = Some(reports);
        }
        if asked == 0 {
            self.set_status(
                StatusLevel::Info,
                "No agent in the registry declares configuration to project",
            );
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

/// What the config-projection lint says about `profile`, one report per agent
/// that declares configuration.
///
/// A free function taking every input rather than an `App` method reading them,
/// for the reason the projection itself takes them: the answer must be
/// reproducible from its arguments, and a test must be able to point it at a
/// fabricated home rather than at the machine it runs on.
///
/// The reports cover **the user's** configuration. friring's own hook payload
/// crosses on top of it and is what makes a place report status, but which file
/// that is comes from the launch's own argv, so it is a launch fact rather than
/// a profile one — the session's `Sandbox:` row is where it is answered.
fn sandbox_lint_reports(
    profile: &SandboxProfile,
    backend: SandboxBackendKind,
    agents: &crate::session::AgentRegistry,
    home: &str,
) -> Vec<modals::SandboxLintReport> {
    let Ok(policy) = profile.resolve(backend, home) else {
        return Vec::new();
    };
    // What the place *mounts*: exactly the profile's own paths, which is what
    // decides whether a reference inside a projected document still resolves in
    // there.
    let granted: Vec<String> = policy
        .rw_paths
        .iter()
        .chain(policy.ro_paths.iter())
        .cloned()
        .collect();
    let database = crate::paths::database_file().map(|p| p.display().to_string());
    let managed_root = crate::agent::config_args::managed_root().unwrap_or_default();
    let platform = crate::sandbox::SecretPlatform::of(SandboxHost::local_shared().platform());

    let mut reports = Vec::new();
    for name in agents.names() {
        let Some(def) = agents.get(name) else {
            continue;
        };
        let Some(declaration) = def.sandbox.as_ref() else {
            continue;
        };
        if declaration.copy_in.is_empty() && declaration.enforced.is_empty() {
            continue;
        }
        let plan = crate::sandbox::plan_projection(&crate::sandbox::ProjectionInput {
            profile: &profile.name,
            agent: Some(declaration),
            granted: &granted,
            home,
            inside_home: crate::sandbox::container::CONTAINER_HOME,
            platform,
            friring_db: database.as_deref(),
            managed_root: &managed_root,
            // A profile names no launch, so friring's own payload is not part
            // of this question — see the note above.
            managed: &[],
        });
        reports.push(modals::SandboxLintReport {
            agent: name.to_string(),
            summary: plan.summary(),
            actionable: plan
                .actionable()
                .map(|finding| (finding.entry.clone(), finding.reason.clone()))
                .collect(),
        });
    }
    reports
}

/// The toast for a session whose launch fell back to the host, or `None` when
/// the boundary went on (and for a session that never asked for one).
///
/// The state is on the session and in the info panel, but a toast is what gets
/// noticed at the moment of the launch, and the escape hatch firing is exactly
/// the event the design wants visible rather than ambient (`docs/SANDBOX.md`
/// How often the place-reclaiming pass runs, in ticks (~10 ms each), and where
/// in the cycle it starts.
///
/// Slow on purpose: a place is created once per profile and superseded only by a
/// profile edit, so nothing accumulates between passes. The offset keeps it off
/// the tick that already does the periodic session-list refresh.
const SANDBOX_GC_INTERVAL_TICKS: u64 = 30_000;
const SANDBOX_GC_OFFSET_TICKS: u64 = 1_500;

/// One pass's inputs, moved to the worker whole so the UI thread does no engine
/// work and the worker does no database work.
pub(crate) struct GcSweep {
    profiles: Vec<SandboxProfile>,
    /// The names of **every** stored profile, including the ones `profiles`
    /// filters out (policy-pinned, or a row friring could not decode). A place
    /// tree is reclaimed by elimination, so a name missing from here reads as a
    /// deleted profile — and a profile edited from a place backend to a policy
    /// one still owns the login in its tree.
    known: Vec<String>,
    records: Vec<(
        SandboxBackendKind,
        crate::sandbox::container::InstanceRecord,
    )>,
    in_use: PlacesInUse,
    /// One profile's places only — the manager view's per-row prune. `None` is
    /// the periodic pass, which is also the only one that collects place trees.
    only: Option<String>,
}

/// What a pass must leave alone — see [`App::places_in_use`].
///
/// Both name sets are lowercased;
/// [`profile_names_of`](crate::sandbox::container::gc::profile_names_of)
/// lowercases what it compares against them.
struct PlacesInUse {
    ids: HashSet<String>,
    profiles: HashSet<String>,
}

/// The live containers a pass must keep, whatever else is true of them.
///
/// Split out of [`GcSweep::run`] because it is the whole of the "would this
/// reap a place an agent is working in?" question and the rest of `run` needs
/// an engine: this way the answer is a unit test rather than a container.
///
/// A container is tested against **both** names it could answer to
/// ([`profile_names_of`](crate::sandbox::container::gc::profile_names_of)), the
/// row's and the label's, because a rename keeps only the first current.
fn protected_ids(
    live: &[crate::sandbox::container::LiveContainer],
    records: &[crate::sandbox::container::InstanceRecord],
    in_use: &PlacesInUse,
    unplannable: &HashSet<String>,
) -> Vec<String> {
    live.iter()
        .filter(|container| {
            in_use.ids.contains(&container.id)
                || crate::sandbox::container::gc::profile_names_of(container, records)
                    .iter()
                    .any(|name| in_use.profiles.contains(name) || unplannable.contains(name))
        })
        .map(|container| container.id.clone())
        .collect()
}

/// What a finished pass leaves for the UI thread to write down.
#[derive(Default)]
pub(crate) struct GcOutcome {
    forget: Vec<(SandboxBackendKind, String)>,
    adopt: Vec<crate::storage::sandboxes::SandboxInstance>,
    failures: Vec<String>,
    /// What to tell the user, for a job they asked for by keystroke. `None` for
    /// the background pass: a sweep that reclaimed nothing has nothing to say,
    /// and saying it every few minutes would train the status bar to be ignored.
    report: Option<String>,
}

impl GcSweep {
    /// Run the pass: ask each engine what it holds, decide, and carry it out.
    ///
    /// Per engine, because that is the only unit that can be reconciled: what a
    /// `docker` reports says nothing about a `podman` container, and a row whose
    /// engine is not installed must not be read as a place that has vanished —
    /// which is why an engine that will not answer is skipped whole rather than
    /// treated as holding nothing.
    fn run(self) -> GcOutcome {
        let mut outcome = GcOutcome::default();
        // The profiles still labelling a place this pass did *not* remove. A
        // tree may only be collected once nothing is running out of it, so an
        // engine that would not answer — or a removal that failed — has to read
        // as "something may still be running in there" and stops every tree
        // being collected this pass. An engine friring cannot drive at all is
        // not that: it is holding nothing, because it created nothing.
        let mut held: Vec<String> = Vec::new();
        let mut settled = true;
        let mut removed = 0usize;
        let host = SandboxHost::local_shared();
        for engine in crate::sandbox::PLACE_KINDS.iter().copied() {
            let Some(backend) = host.place(engine) else {
                continue;
            };
            // An engine friring cannot drive at all created nothing, so it holds
            // nothing and is not a reason to spare every tree; one that *is*
            // installed and would not answer may be holding anything.
            let live = match crate::sandbox::live_places_here(backend) {
                Ok(Some(live)) => live,
                Ok(None) => continue,
                Err(_) => {
                    settled = false;
                    continue;
                }
            };
            let records: Vec<crate::sandbox::container::InstanceRecord> = self
                .records
                .iter()
                .filter(|(kind, _)| *kind == engine)
                .map(|(_, record)| record.clone())
                .collect();
            // A profile with no opinion (its engine could not plan it) is left
            // out of `current`, which would read as "the profile is gone" — so
            // its containers are protected by name instead.
            let mut current = std::collections::BTreeMap::new();
            let mut unplannable: HashSet<String> = HashSet::new();
            for profile in &self.profiles {
                match backend.current_spec(profile) {
                    Some(spec) => {
                        current.insert(profile.name.clone(), spec);
                    }
                    None => {
                        unplannable.insert(profile.name.to_ascii_lowercase());
                    }
                }
            }
            let in_use = protected_ids(&live, &records, &self.in_use, &unplannable);

            let mut plan = crate::sandbox::container::gc_plan(crate::sandbox::container::GcInput {
                records: &records,
                live: &live,
                current: &current,
                in_use: &in_use,
                now: crate::sync::current_time_millis(),
                // Idleness never reclaims a place. A place is the environment a
                // session lives in, not a cache, and "unused for a while" is
                // indistinguishable from "the user is on holiday". Only a
                // deleted profile or a rebuild takes one.
                idle_after_ms: None,
            });
            // A pass the manager view asked for acts on the row it was pressed
            // on and nothing else: the decision above is the same one, narrowed
            // afterwards rather than re-derived, so a scoped prune can never
            // reclaim something the whole-installation pass would have spared.
            if let Some(only) = &self.only {
                narrow_to_profile(&mut plan, only, &live, &records);
            }
            held.extend(
                live.iter()
                    .filter(|container| !plan.remove.contains(&container.id))
                    .filter_map(|container| container.profile.clone()),
            );
            let failures = backend.reap(&plan);
            settled &= failures.is_empty();
            removed += plan.remove.len().saturating_sub(failures.len());
            outcome.failures.extend(failures);
            outcome
                .forget
                .extend(plan.forget.into_iter().map(|id| (engine, id)));
            outcome
                .adopt
                .extend(plan.adopt.into_iter().filter_map(|container| {
                    Some(crate::storage::sandboxes::SandboxInstance::new(
                        container.profile?,
                        engine,
                        container.id,
                        crate::sandbox::container::INSTANCE_STATE_RUNNING,
                    ))
                }));
        }
        // The place *trees*, after the containers and only when every engine
        // agreed about what it holds. Deleting a profile deliberately leaves its
        // tree behind while sessions are still running in it — the tree is that
        // container's `$HOME` — so this is the only thing that collects one.
        //
        // Never from a scoped pass: a tree is collected by *elimination* from
        // the list of profiles that still exist, and one row's keystroke is not
        // an answer about every other profile's tree.
        if settled && self.only.is_none() {
            crate::sandbox::dirs::reclaim_orphan_places(&self.known, &held);
        }
        // A pass the user asked for says what it did, including when that is
        // nothing: a keystroke with no visible effect reads as a broken key.
        if self.only.is_some() {
            let forgotten = outcome.forget.len();
            outcome.report = Some(format!(
                "Reclaimed {removed} sandbox place(s) and forgot {forgotten} record(s)"
            ));
        }
        outcome
    }
}

/// Which of one engine's live containers a [`PlaceJob`] is about to remove.
///
/// Split out of [`PlaceJob::run`] for the reason [`protected_ids`] is split out
/// of [`GcSweep::run`]: the rest of `run` needs an engine, and this is the whole
/// of "does the job act on exactly what the question promised?".
///
/// Both names a container can answer to, and that is what keeps it in step with
/// the confirmation the user gave — `request_place_action` counts through
/// [`running_places`](crate::agent::sandboxing::running_places), which asks the
/// same pair. An engine cannot relabel a running container, so after a rename
/// the label carries the old name and only the recorded rows carry the new one:
/// matched on the label alone this job would report a renamed profile's places
/// stopped while leaving every one of them running.
///
/// The record match is scoped to `engine`, because an id is only unique within
/// one of them.
fn job_targets(
    live: &[crate::sandbox::container::LiveContainer],
    profile: &str,
    records: &[(SandboxBackendKind, String)],
    engine: SandboxBackendKind,
) -> Vec<String> {
    live.iter()
        .filter(|container| container.owned)
        .filter(|container| {
            container
                .profile
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(profile))
                || records
                    .iter()
                    .any(|(kind, id)| *kind == engine && *id == container.id)
        })
        .map(|container| container.id.clone())
        .collect()
}

/// The manager view's stop and rebuild, on the same background slot as the
/// reclaiming pass and answering with the same outcome.
///
/// Unlike a pass, this one is **told** what to remove: the user asked for it by
/// name, and the confirmation said what it costs. What it still will not do is
/// touch anything friring did not create — `live_places` filters on friring's
/// own label and [`PlaceBackend::reap`](crate::sandbox::PlaceBackend::reap)
/// re-checks it before every removal.
pub(crate) struct PlaceJob {
    /// The profile as it reads **now**, re-loaded when the action was
    /// confirmed: a rebuild starts the place this describes, not the one the
    /// list row was built from.
    profile: SandboxProfile,
    /// Its recorded `(engine, id)` pairs, so the rows of containers that are
    /// gone afterwards can be forgotten without asking the UI thread again.
    records: Vec<(SandboxBackendKind, String)>,
    /// Start a fresh place once the old ones are gone (rebuild), or leave the
    /// profile with none until its next launch (stop).
    restart: bool,
}

impl PlaceJob {
    fn run(self) -> GcOutcome {
        let mut outcome = GcOutcome::default();
        let host = SandboxHost::local_shared();
        let mut removed = 0usize;
        // A plain list, because a backend kind is not `Hash` — and there are
        // at most a handful of containers per profile.
        let mut gone: Vec<(SandboxBackendKind, String)> = Vec::new();
        for engine in crate::sandbox::PLACE_KINDS.iter().copied() {
            let Some(backend) = host.place(engine) else {
                continue;
            };
            let live = match crate::sandbox::live_places_here(backend) {
                Ok(Some(live)) => live,
                // Not installed here, so it created none of this profile's
                // places and has nothing to say about them.
                Ok(None) => continue,
                // Installed and would not answer: it holds nothing this job can
                // name. Reported, because the user asked for something and part
                // of it did not happen.
                Err(e) => {
                    outcome.failures.push(format!("{engine}: {e}"));
                    continue;
                }
            };
            let mine = job_targets(&live, &self.profile.name, &self.records, engine);
            if mine.is_empty() {
                continue;
            }
            let plan = crate::sandbox::container::GcPlan {
                remove: mine.clone(),
                forget: Vec::new(),
                adopt: Vec::new(),
            };
            outcome.failures.extend(backend.reap(&plan));
            // Which rows to forget is decided by asking the engine again rather
            // than by assuming the removals worked: a row forgotten for a
            // container that is still there loses the only id anything has for
            // it, which is the leak `sandbox_instances` exists to prevent.
            match backend.live_places() {
                Ok(after) => {
                    for id in mine {
                        if !after.iter().any(|container| container.id == id) {
                            removed += 1;
                            gone.push((engine, id));
                        }
                    }
                }
                Err(e) => outcome.failures.push(format!("{engine}: {e}")),
            }
        }
        outcome.forget.extend(
            self.records
                .into_iter()
                .filter(|record| gone.contains(record)),
        );

        if !self.restart {
            outcome.report = Some(format!(
                "Stopped {removed} place(s) of sandbox profile '{}'",
                self.profile.name
            ));
            return outcome;
        }
        // The fresh place, from the profile as it reads now. Composed through
        // the same call a launch makes, so an image that has to be built, an
        // engine that is not there and a mount friring will not grant all fail
        // here in the words a launch would have used.
        match crate::agent::sandboxing::open_place(&self.profile) {
            Ok((_, instance)) => {
                outcome
                    .adopt
                    .push(crate::storage::sandboxes::SandboxInstance::new(
                        instance.profile,
                        instance.engine,
                        instance.external_id,
                        instance.state,
                    ));
                outcome.report = Some(format!(
                    "Rebuilt sandbox profile '{}' — {removed} place(s) removed, a fresh one is up",
                    self.profile.name
                ));
            }
            Err(message) => outcome.failures.push(message),
        }
        outcome
    }
}

/// Narrow a reclaiming plan to one profile's places.
///
/// A container's profile comes from friring's own label, falling back to the
/// row that records it — an adopted container carries the label, and a row for
/// a container the engine no longer reports has only the row.
fn narrow_to_profile(
    plan: &mut crate::sandbox::container::GcPlan,
    profile: &str,
    live: &[crate::sandbox::container::LiveContainer],
    records: &[crate::sandbox::container::InstanceRecord],
) {
    let owner = |id: &str| -> Option<String> {
        live.iter()
            .find(|container| container.id == id)
            .and_then(|container| container.profile.clone())
            .or_else(|| {
                records
                    .iter()
                    .find(|record| record.external_id == id)
                    .map(|record| record.profile.clone())
            })
    };
    let mine = |id: &String| owner(id).is_some_and(|name| name.eq_ignore_ascii_case(profile));
    plan.remove.retain(&mine);
    plan.forget.retain(&mine);
    plan.adopt.retain(|container| {
        container
            .profile
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(profile))
    });
}

/// §Failure modes). A free function rather than a method so a caller that has
/// already moved the session into `self.sessions` can compose the message first
/// and set it afterwards.
pub(crate) fn unenforced_sandbox_message(info: &crate::session::SessionInfo) -> Option<String> {
    // Both halves, in the same order the indicators read them: a state with no
    // profile beside it would be claiming something about a session that never
    // asked for a boundary.
    let profile = info.sandbox_profile.as_deref()?;
    let crate::session::SandboxState::Unenforced(reason) = info.sandbox_state.as_ref()? else {
        return None;
    };
    Some(format!(
        "'{}' is NOT sandboxed — profile '{profile}' could not be applied: {reason}",
        info.name
    ))
}

/// What a launch has to say about the session's boundary, if anything, and how
/// loudly.
///
/// Two different facts, in priority order, because only one status line exists
/// and one of them is worse than the other:
///
/// - the boundary was **not applied** and the agent is on the host — an error,
///   and the more serious fact whenever both are true;
/// - the boundary holds and the agent has **no credential in it** — an
///   ordinary notice, because the fix is a command typed in the pane. Said at
///   the launch as well as on the info panel: a place is a fresh home, and an
///   agent sitting at a sign-in prompt with no explanation reads as a broken
///   session.
pub(crate) fn sandbox_launch_notice(
    info: &crate::session::SessionInfo,
) -> Option<(super::StatusLevel, String)> {
    if let Some(message) = unenforced_sandbox_message(info) {
        return Some((super::StatusLevel::Error, message));
    }
    let how = info.sandbox_login.as_deref()?;
    let profile = info.sandbox_profile.as_deref()?;
    Some((
        super::StatusLevel::Info,
        format!(
            "'{}' started signed out inside sandbox '{profile}' — {how}",
            info.name
        ),
    ))
}

/// The rule an "allow" would store for `host` on `port`, or `None` when friring
/// could never store one — a name that does not canonicalise, or a shape the
/// profile's own grammar rejects.
///
/// **Exactly what the modal promises: that host, on that port.** Both halves are
/// narrowings the user did not have to ask for, and both are load-bearing. The
/// port is the one that was refused, because that is what the question named.
/// The host is exact — [`DomainRule::exact`], never [`DomainRule::parse`] —
/// because the *sandbox* chose the spelling it was refused under, and
/// `.github.com` read as a rule would mean the whole subtree; a client would
/// then be picking how wide its own grant is.
///
/// A SOCKS5 client names an IPv6 destination unbracketed — the address is the
/// address, not a URL authority — while an HTTP `CONNECT` line brackets it. Both
/// spellings canonicalise to the address, and the rule renders in the single
/// spelling a profile stores.
///
/// A destination local to the *host* is never offered. `NO_PROXY` is set so the
/// agent's own local traffic bypasses the filter entirely, so a question about
/// `127.0.0.1:2375` reads like the agent asking for a dev server it started —
/// while the address the proxy would dial is on the machine outside the
/// boundary. The proxy refuses those with a reason of its own
/// ([`DenyReason::HostLocal`]) and never raises this prompt for one; declining
/// to build the rule is the second lock, so no path can mint the grant by
/// accident. Reaching one deliberately is an edit to the profile.
fn allow_rule_for(host: &str, port: u16) -> Option<String> {
    // Port 0 has no meaning as a destination; the proxy only reports one
    // alongside an empty host, which is never an allowlist question.
    if port == 0 || crate::proxy::host_is_local(host) {
        return None;
    }
    Some(DomainRule::exact(host, Some(port)).ok()?.to_string())
}

/// The key one refused host is remembered under, so that the same destination
/// spelled two ways is one question rather than two.
///
/// The proxy reports the host as the client wrote it, and a client picks the
/// spelling: `github.com.`, `GitHub.com`, `127.1` and `2130706433` all reach one
/// place. Canonicalising with the same constructor [`allow_rule_for`] uses keeps
/// the dedup key and the rule the answer stores in step. A host that does not
/// canonicalise (a U-label, the empty host) has no canonical form to key on, so
/// it falls back to its lowercased spelling — still deduplicated, just only
/// against itself.
///
/// Reading the client's spelling as a *rule* would break the dedup as well as
/// the grant: `github.com` and `.github.com` are one destination but two
/// patterns, so a client alternating them would raise two questions about the
/// same place. [`DomainRule::exact`] never yields a pattern, and takes the host
/// alone — so an unbracketed IPv6 literal needs no brackets added first.
fn prompt_key(host: &str) -> String {
    DomainRule::exact(host, None)
        .map(|parsed| parsed.host)
        .unwrap_or_else(|_| host.to_ascii_lowercase())
}

/// A refused destination, as a line of prose. The host is empty when a request
/// was refused before one was named — a SOCKS5 client that fails
/// authentication, which the protocol checks before it asks where to go.
fn describe_destination(host: &str, port: u16) -> String {
    if host.is_empty() {
        return "an unnamed destination".to_string();
    }
    format!("{host}:{port}")
}

/// One stored profile as a list row, with `auto` resolved against `host`.
///
/// The undecoded columns come across too: a row whose policy friring could not
/// read still lists (that is where it gets repaired or deleted) but must say so
/// rather than render a summary of values it substituted.
fn profile_row(
    host: &SandboxHost,
    stored: &crate::storage::sandboxes::StoredSandboxProfile,
    places: Vec<PlaceRow>,
) -> SandboxProfileRow {
    let p = &stored.profile;
    let resolved = resolve_backend(host, p.backend);
    SandboxProfileRow {
        resolved,
        name: p.name.clone(),
        backend: p.backend,
        paths: p.paths.len(),
        network: p.network_mode,
        undecoded: stored
            .undecoded_columns()
            .into_iter()
            .map(str::to_string)
            .collect(),
        places,
        unavailable: backend_unavailable(host, p.backend, resolved),
        pending: None,
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

/// Why the backend a profile would run on cannot be used here, or `None` when
/// it can.
///
/// Two shapes of "no", and they read differently: a **pinned** backend that is
/// not installed reports the probe's own actionable sentence, because the user
/// chose it and a pin never falls back; an **`auto`** whose whole ladder came up
/// empty reports every rung's reason, because "nothing here can sandbox" is the
/// only useful summary of it. An `auto` that resolved is available by
/// construction — the ladder only ever picks an available rung.
pub(crate) fn backend_unavailable(
    host: &SandboxHost,
    requested: SandboxBackendKind,
    resolved: Option<SandboxBackendKind>,
) -> Option<String> {
    match (requested, resolved) {
        (SandboxBackendKind::Auto, Some(_)) => None,
        (SandboxBackendKind::Auto, None) => Some(
            host.select(requested)
                .rejection_summary()
                .replace('\n', "; "),
        ),
        (explicit, _) => {
            let availability = host.probe(explicit);
            (!availability.is_available()).then(|| availability.message())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::{DenialEvent, Protocol};
    use crate::sandbox::SessionDenial;
    use crate::session::{NetworkMode, SandboxPath};

    /// A registry entry with nothing in it but a name and what it declares
    /// about a sandbox — `AgentDef` has no `Default`, deliberately, so every
    /// field is spelled once here rather than in each test.
    fn agent_def(
        name: &str,
        sandbox: Option<crate::session::AgentSandboxDef>,
    ) -> crate::session::AgentDef {
        crate::session::AgentDef {
            name: name.to_string(),
            command: name.to_string(),
            args: Vec::new(),
            resume_args: Vec::new(),
            fork_args: Vec::new(),
            new_session_args: Vec::new(),
            resume_latest: false,
            hook_schema: None,
            sandbox,
        }
    }

    /// The profile editor's config lint, over a **fabricated** home: an agent
    /// that declares a directory friring can carry, one it cannot, and one that
    /// only crosses if the profile mounts it.
    ///
    /// The verdicts are the projection's; what this pins is that the editor asks
    /// it the right question — the profile's own granted paths, the agent's own
    /// declaration, one report per agent that has one.
    #[test]
    fn the_editor_lints_a_places_config_against_the_form_in_front_of_it() {
        use crate::session::{AgentRegistry, AgentSandboxDef};

        let home = crate::sandbox::dirs::test_temp_base("editor-lint").join("home");
        let skills = home.join(".fabricated-agent/skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(skills.join("a.md"), "# a skill\n").unwrap();
        let outside = home.join(".fabricated-agent/notes");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("n.md"), "notes\n").unwrap();
        // A settings document pointing at a host directory the profile does not
        // grant: the verdict a user has a decision to make about, and the one
        // this test carries all the way into the editor's report.
        let plugins = home.join(".fabricated-agent/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let plugins = plugins.display().to_string();
        std::fs::write(
            home.join(".fabricated-agent/settings.json"),
            serde_json::json!({ "plugins": { "repositories": [&plugins] } }).to_string(),
        )
        .unwrap();
        let home = home.display().to_string();

        let agent = agent_def(
            "fabricated",
            Some(AgentSandboxDef {
                copy_in: vec![
                    "~/.fabricated-agent/skills".into(),
                    "~/.fabricated-agent/notes".into(),
                    "~/.fabricated-agent/settings.json".into(),
                ],
                ..Default::default()
            }),
        );
        // A second agent with no declaration is not a report: there is nothing
        // of its configuration to say anything about.
        let quiet = agent_def("quiet", None);
        let agents = AgentRegistry {
            config_version: Some(1),
            default: "fabricated".to_string(),
            agents: vec![agent, quiet],
        };

        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("/srv/work")]);
        let reports = sandbox_lint_reports(&profile, SandboxBackendKind::Docker, &agents, &home);
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].agent, "fabricated");
        assert!(
            reports[0].summary.contains("config projected"),
            "{reports:?}"
        );
        // The verdict itself, not just the count: the entry the user has to
        // decide about is named with the reason, which is the whole content of
        // the editor's lint pane.
        assert_eq!(reports[0].actionable.len(), 1, "{reports:?}");
        let (entry, reason) = &reports[0].actionable[0];
        assert!(entry.contains("settings.json"), "{entry}");
        assert!(entry.contains("plugins.repositories"), "{entry}");
        assert!(reason.contains(&plugins), "{reason}");
        assert!(reason.contains("read-only"), "{reason}");

        // …and granting exactly that path answers it: the reference resolves in
        // the place, so there is nothing left to decide.
        let granted = SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("/srv/work"),
                SandboxPath::read_only(&plugins),
            ],
        );
        let reports = sandbox_lint_reports(&granted, SandboxBackendKind::Docker, &agents, &home);
        assert!(reports[0].actionable.is_empty(), "{reports:?}");
    }

    /// A policy backend has nothing to project, so the lint is not offered for
    /// one — the agent reads the host's own configuration where it always did.
    #[test]
    fn a_policy_profile_is_not_linted_because_nothing_is_projected() {
        use crate::session::{AgentRegistry, AgentSandboxDef};

        let (mut app, _guard, _tmp) = crate::app::state::tests::app_with_sessions(0);
        app.agents = AgentRegistry {
            config_version: Some(1),
            default: "fabricated".to_string(),
            agents: vec![agent_def(
                "fabricated",
                Some(AgentSandboxDef {
                    copy_in: vec!["~/.fabricated-agent/skills".into()],
                    ..Default::default()
                }),
            )],
        };
        app.modal = modals::Modal::SandboxEditor(Box::new({
            let mut m = modals::SandboxEditorModal::from_profile(&SandboxProfile::new(
                "dev",
                vec![SandboxPath::workspace("/srv/work")],
            ));
            m.backend = SandboxBackendKind::Seatbelt;
            m
        }));
        app.handle_sandbox_editor_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        let modals::Modal::SandboxEditor(ref m) = app.modal else {
            panic!("the editor stays open");
        };
        assert!(m.lint.is_none(), "a policy profile has nothing to lint");
    }

    /// The pass may not take a place out from under a running agent, and the
    /// careful half is what it does *not* know: a session another friring is
    /// driving is a row and nothing more, so every container of that profile is
    /// protected by name rather than by id.
    #[test]
    fn a_place_a_live_session_is_in_is_never_reclaimed() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let mut app = crate::app::tests::app_with_sessions(0);
        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        app.db.upsert_sandbox_profile(&profile).unwrap();

        app.db
            .upsert_session(&sandboxed_session("demo", "dev"))
            .unwrap();

        // Nothing opened this place here, so the profile is protected whole.
        let in_use = app.places_in_use();
        assert!(in_use.ids.is_empty());
        assert!(in_use.profiles.contains("dev"));

        // Once this instance knows which container the session is in, the
        // protection narrows to the places it opened — which is what lets a
        // container no session of this instance is in be reclaimed.
        app.place_containers
            .entry("sandbox:dev".into())
            .or_default()
            .insert("ctr1".into());
        let in_use = app.places_in_use();
        assert!(in_use.ids.contains("ctr1"));
        assert!(in_use.profiles.is_empty());

        // Editing the profile builds a new container for the next session, but
        // the session above is still running in the old one, so both stay.
        app.place_containers
            .entry("sandbox:dev".into())
            .or_default()
            .insert("ctr2".into());
        let in_use = app.places_in_use();
        assert!(in_use.ids.contains("ctr1"));
        assert!(in_use.ids.contains("ctr2"));
        assert!(in_use.profiles.is_empty());
    }

    /// A live session's `SharedSession` row, sandboxed under `profile`.
    fn sandboxed_session(name: &str, profile: &str) -> crate::sync::SharedSession {
        crate::sync::SharedSession {
            id: crate::session::SessionId::default(),
            name: name.into(),
            agent: "claude".into(),
            backend_id: String::new(),
            backend_type: format!("sandbox:{profile}"),
            agent_session_id: None,
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: Vec::new(),
            shell_backend_id: None,
            sandbox_profile: Some(profile.into()),
            sandbox_enforcement: Default::default(),
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
        }
    }

    /// Renaming a profile must not hand its running place to the pass.
    ///
    /// A container's profile label is baked in at create and cannot be
    /// relabelled, while the rename rewrites the profile row, the instance rows
    /// and the session's `sandbox:<profile>` in one transaction — so the
    /// protection set holds the new name and the container still answers to the
    /// old one. Matched on the label alone, the place an agent is working in
    /// reads as a place whose profile is gone, and rule 3 stops and removes it.
    ///
    /// Driven through the manager's `p`, which is
    /// [`sandbox_gc_input`](App::sandbox_gc_input) plus the pure decision:
    /// running the sweep itself would ask a real engine, and this is the whole
    /// of what it would decide.
    #[test]
    fn a_renamed_profiles_live_place_is_not_reclaimed_out_from_under_it() {
        use crate::sandbox::container::{InstanceRecord, LiveContainer};
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let mut app = crate::app::tests::app_with_sessions(0);
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.backend = SandboxBackendKind::Podman;
        app.db.upsert_sandbox_profile(&profile).unwrap();
        app.db
            .upsert_sandbox_instance(&crate::storage::sandboxes::SandboxInstance::new(
                "dev",
                SandboxBackendKind::Podman,
                "ctr",
                "running",
            ))
            .unwrap();
        // A session this instance did not open the place for — another
        // friring's, or one restored across a restart — which is the case that
        // protects by name rather than by id.
        app.db
            .upsert_session(&sandboxed_session("boxed", "dev"))
            .unwrap();

        // What the editor does when the name changes.
        assert!(app.db.rename_sandbox_profile("dev", "dev2").unwrap());
        app.rename_sandbox_profile_in_sessions("dev", "dev2");

        let sweep = app
            .sandbox_gc_input(Some("dev2".to_string()))
            .expect("a sweep with one place profile");
        let records: Vec<InstanceRecord> = sweep
            .records
            .iter()
            .map(|(_, record)| record.clone())
            .collect();
        assert_eq!(records[0].profile, "dev2", "the row moved with the rename");
        // The container cannot be relabelled, so it still says `dev`.
        let live = [LiveContainer {
            id: "ctr".to_string(),
            profile: Some("dev".to_string()),
            spec: Some("a-spec".to_string()),
            owned: true,
        }];

        let in_use = protected_ids(&live, &records, &sweep.in_use, &HashSet::new());
        assert_eq!(in_use, ["ctr"], "the live place is protected");

        // …and that is the only thing standing between it and removal: to every
        // other rule this is a container whose profile no longer exists.
        let current =
            std::collections::BTreeMap::from([("dev2".to_string(), "another-spec".to_string())]);
        let plan = crate::sandbox::container::gc_plan(crate::sandbox::container::GcInput {
            records: &records,
            live: &live,
            current: &current,
            in_use: &in_use,
            now: 10_000,
            idle_after_ms: None,
        });
        assert!(plan.remove.is_empty(), "{plan:?}");
        assert!(plan.forget.is_empty(), "{plan:?}");
    }

    /// The manager view's stop and rebuild act on **exactly** what the
    /// confirmation counted, across a rename.
    ///
    /// The question is asked through `running_places`, which matches a container
    /// on the recorded row as well as on the label. So this must too, or a
    /// renamed profile's `s` reports "stopped 0 place(s)" while every one of them
    /// keeps running with an agent inside — a job that says it did something it
    /// did not.
    #[test]
    fn a_place_job_removes_the_containers_the_question_counted_after_a_rename() {
        use crate::sandbox::container::LiveContainer;
        let owned = |id: &str, label: Option<&str>| LiveContainer {
            id: id.to_string(),
            profile: label.map(str::to_string),
            spec: Some("a-spec".to_string()),
            owned: true,
        };
        // `ours` still carries the label it was created with; the rename moved
        // its row to `dev2`. `adopted` carries the label and no row — a place
        // this friring found after a crash. `stranger` is somebody else's.
        let live = [
            owned("ours", Some("dev")),
            owned("adopted", Some("dev2")),
            LiveContainer {
                owned: false,
                ..owned("stranger", Some("dev2"))
            },
            owned("elsewhere", Some("other")),
        ];
        let records = vec![(SandboxBackendKind::Podman, "ours".to_string())];

        assert_eq!(
            job_targets(&live, "dev2", &records, SandboxBackendKind::Podman),
            ["ours", "adopted"]
        );
        // An id is unique within one engine only, so a row recorded against
        // another engine names nothing here.
        assert_eq!(
            job_targets(&live, "dev2", &records, SandboxBackendKind::Docker),
            ["adopted"]
        );
        // And the label half still stands on its own, for a place with no row.
        assert_eq!(
            job_targets(&live, "DEV", &[], SandboxBackendKind::Podman),
            ["ours"],
            "the label is compared case-insensitively, like every other name here"
        );
    }

    /// The narrow protection has to survive a rename too: the ids this instance
    /// opened are filed under the `sandbox:<profile>` name the session carries,
    /// and a map left behind at the old name protects nothing.
    #[test]
    fn a_rename_carries_the_opened_containers_to_the_new_backend_name() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let mut app = crate::app::tests::app_with_sessions(0);
        app.db
            .upsert_session(&sandboxed_session("boxed", "dev"))
            .unwrap();
        app.place_containers
            .entry("sandbox:dev".into())
            .or_default()
            .insert("ctr".into());
        app.db
            .upsert_sandbox_profile(&SandboxProfile::new(
                "dev",
                vec![SandboxPath::workspace("~/dev/app")],
            ))
            .unwrap();

        assert!(app.db.rename_sandbox_profile("dev", "dev2").unwrap());
        app.rename_sandbox_profile_in_sessions("dev", "dev2");

        let in_use = app.places_in_use();
        assert!(
            in_use.ids.contains("ctr"),
            "the opened id followed the name"
        );
        assert!(
            in_use.profiles.is_empty(),
            "and the protection stayed narrow rather than widening to the profile"
        );
    }

    /// A pass costs an engine command, so an installation whose profiles could
    /// never own a place does not start one.
    #[test]
    fn a_policy_only_installation_never_asks_an_engine_anything() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let app = crate::app::tests::app_with_sessions(0);
        let mut policy = SandboxProfile::new("mac", vec![SandboxPath::workspace("~/dev/app")]);
        policy.backend = SandboxBackendKind::Seatbelt;
        app.db.upsert_sandbox_profile(&policy).unwrap();
        assert!(app.sandbox_gc_input(None).is_none());

        let mut place = SandboxProfile::new("box", vec![SandboxPath::workspace("~/dev/app")]);
        place.backend = SandboxBackendKind::Podman;
        app.db.upsert_sandbox_profile(&place).unwrap();
        assert!(app.sandbox_gc_input(None).is_some());
    }

    // ---- The manager view -------------------------------------------------

    const PODMAN: &str = "/usr/bin/podman";

    /// A podman holding `ids`, every one of them labelled for one profile.
    ///
    /// [`StubHost`](crate::sandbox::probe::StubHost) matches a whole command
    /// line and an `inspect` differs only in the id it ends with, so the two
    /// commands a listing makes are answered here and everything else is
    /// delegated. Nothing is started, pulled or built: this *is* the engine, and
    /// it is a table.
    struct FakePlaces {
        base: crate::sandbox::probe::StubHost,
        ids: Vec<String>,
        label: String,
    }

    impl FakePlaces {
        fn new(label: &str, ids: Vec<String>) -> Self {
            use crate::sandbox::probe::ProbeOutput;
            Self {
                base: crate::sandbox::probe::StubHost::new()
                    .with_home("/fabricated/home")
                    .with_command("uname -s", ProbeOutput::success("Linux\n"))
                    .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
                    .with_binary("podman")
                    .with_command("id -u", ProbeOutput::success("1000\n"))
                    .with_command("id -g", ProbeOutput::success("1000\n"))
                    .with_command(
                        &format!(
                            "{PODMAN} info --format {}",
                            "{{.Version.Version}}|{{.Host.Security.Rootless}}"
                        ),
                        ProbeOutput::success("5.2.2|true\n"),
                    ),
                ids,
                label: label.to_string(),
            }
        }
    }

    impl crate::sandbox::probe::ProbeHost for FakePlaces {
        fn which(&self, program: &str) -> Option<String> {
            crate::sandbox::probe::ProbeHost::which(&self.base, program)
        }

        fn home(&self) -> Option<String> {
            crate::sandbox::probe::ProbeHost::home(&self.base)
        }

        fn path_exists(&self, path: &str) -> bool {
            crate::sandbox::probe::ProbeHost::path_exists(&self.base, path)
        }

        fn read_file(&self, path: &str) -> Option<String> {
            crate::sandbox::probe::ProbeHost::read_file(&self.base, path)
        }

        fn run(
            &self,
            program: &str,
            args: &[&str],
        ) -> Result<crate::sandbox::probe::ProbeOutput, String> {
            type ProbeOutput = crate::sandbox::probe::ProbeOutput;
            let last = args.last().copied().unwrap_or_default();
            match (program, args.first().copied()) {
                (PODMAN, Some("ps")) => Ok(ProbeOutput::success(self.ids.join("\n"))),
                (PODMAN, Some("inspect")) => Ok(match self.ids.iter().any(|id| id == last) {
                    true => {
                        ProbeOutput::success(format!("{last}|running|1|{}|a-spec\n", self.label))
                    }
                    false => ProbeOutput::failure(125, "no such container\n"),
                }),
                _ => crate::sandbox::probe::ProbeHost::run(&self.base, program, args),
            }
        }
    }

    /// The manager's fixture, with the recorded rows and the running containers
    /// in step — which is the case a test about anything *else* wants.
    fn manager(
        places: usize,
    ) -> (
        App,
        crate::paths::TestPathGuard,
        tempfile::TempDir,
        crate::agent::sandboxing::TestSandboxHost,
    ) {
        manager_with(places, places)
    }

    /// An [`App`] holding one place profile with `recorded` instance rows, on an
    /// engine holding `live` containers of it, with the profile list open.
    ///
    /// The two counts are separate because they genuinely disagree in the field:
    /// a row outlives the container a rebuild replaced until a pass reconciles
    /// it away, and a container friring adopted after a crash has no row at all.
    fn manager_with(
        recorded: usize,
        live: usize,
    ) -> (
        App,
        crate::paths::TestPathGuard,
        tempfile::TempDir,
        crate::agent::sandboxing::TestSandboxHost,
    ) {
        let (mut app, guard, tmp) = crate::app::state::tests::app_with_sessions(0);
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.backend = SandboxBackendKind::Podman;
        app.db.upsert_sandbox_profile(&profile).unwrap();
        for n in 0..recorded {
            app.db
                .upsert_sandbox_instance(&crate::storage::sandboxes::SandboxInstance::new(
                    "dev",
                    SandboxBackendKind::Podman,
                    format!("ctr-{n}"),
                    "running",
                ))
                .unwrap();
        }
        // Every engine question this fixture answers is the stub's, so no test
        // here depends on what the machine running it happens to have installed.
        let host =
            crate::agent::sandboxing::TestSandboxHost::new(SandboxHost::new(std::sync::Arc::new(
                FakePlaces::new("dev", (0..live).map(|n| format!("ctr-{n}")).collect()),
            )));
        app.open_sandbox_list();
        (app, guard, tmp, host)
    }

    fn selected_row(app: &App) -> &SandboxProfileRow {
        match app.modal {
            modals::Modal::SandboxList(ref sl) => sl.selected().expect("a selected row"),
            ref other => panic!("expected the profile list, got {other:?}"),
        }
    }

    /// The list is the manager view: it shows every place a profile owns, in
    /// most-recently-used order, because a profile edit leaves the superseded
    /// container running beside the new one.
    #[test]
    fn the_list_shows_every_place_a_profile_owns() {
        let (app, _g, _t, _h) = manager(2);
        let row = selected_row(&app);
        assert_eq!(row.places.len(), 2);
        assert_eq!(row.places[0].engine, SandboxBackendKind::Podman);
        assert!(
            row.summary().contains("2 places · running"),
            "{}",
            row.summary()
        );
    }

    /// Stopping a place takes it away from whatever is running in it, so it is
    /// confirmed first — and the question names both counts, because those are
    /// what the answer costs.
    #[test]
    fn stopping_a_place_asks_first_and_names_what_it_costs() {
        let (mut app, _g, _t, _h) = manager(2);
        app.sessions.clear();
        app.db
            .upsert_session(&sandboxed_session("boxed", "dev"))
            .unwrap();

        app.handle_sandbox_list_key(KeyCode::Char('s'));

        let pending = selected_row(&app)
            .pending
            .clone()
            .expect("stopping asks first");
        assert_eq!(pending.action, PlaceAction::Stop);
        assert_eq!(pending.profile, "dev");
        assert!(
            pending.question.contains("2 place(s)"),
            "{}",
            pending.question
        );
        assert!(
            pending.question.contains("1 session(s)"),
            "{}",
            pending.question
        );
    }

    /// The question counts what the **engines** hold, not what
    /// `sandbox_instances` records.
    ///
    /// The two disagree in both directions, and the job this question authorises
    /// acts on the engines either way — so a count taken from the rows can
    /// promise to remove places that are already gone, or refuse to offer a
    /// container that is right there.
    #[test]
    fn the_confirmation_counts_the_places_that_are_actually_running() {
        // Three rows, one container: two rebuilds nothing has reconciled away.
        let (mut app, _g, _t, _h) = manager_with(3, 1);
        assert_eq!(selected_row(&app).places.len(), 3, "the rows say three");

        app.handle_sandbox_list_key(KeyCode::Char('s'));

        let pending = selected_row(&app)
            .pending
            .clone()
            .expect("stopping asks first");
        assert!(
            pending.question.contains("1 place(s)"),
            "{}",
            pending.question
        );
        assert!(
            !pending.question.contains("3 place(s)"),
            "{}",
            pending.question
        );
    }

    /// The other direction: a container friring adopted after a crash has no row
    /// at all, and it is still a place the stop would remove — so the question
    /// is asked rather than answered with "there is nothing to stop".
    #[test]
    fn a_running_place_with_no_row_is_still_offered_for_stopping() {
        let (mut app, _g, _t, _h) = manager_with(0, 1);
        assert!(selected_row(&app).places.is_empty(), "no row describes it");

        app.handle_sandbox_list_key(KeyCode::Char('s'));

        let pending = selected_row(&app)
            .pending
            .clone()
            .expect("a running place is stoppable whether or not a row names it");
        assert!(
            pending.question.contains("1 place(s)"),
            "{}",
            pending.question
        );
    }

    /// `y` is the only answer that carries it out. `Enter` and `d` already mean
    /// edit and delete on this list, so neither may double as "yes" — and while
    /// a question is armed they do not do their own job either.
    #[test]
    fn only_y_confirms_and_every_other_key_cancels() {
        for code in [
            KeyCode::Enter,
            KeyCode::Char('d'),
            KeyCode::Char('n'),
            KeyCode::Esc,
            KeyCode::Char('j'),
        ] {
            let (mut app, _g, _t, _h) = manager(1);
            app.handle_sandbox_list_key(KeyCode::Char('s'));
            assert!(selected_row(&app).pending.is_some());

            app.handle_sandbox_list_key(code);

            assert!(
                selected_row(&app).pending.is_none(),
                "{code:?} left the question armed"
            );
            // The list is still open on the same profile: the cancelled key did
            // not also delete it, edit it or close the modal.
            assert!(app.db.get_sandbox_profile("dev").unwrap().is_some());
            assert!(
                matches!(app.modal, modals::Modal::SandboxList(_)),
                "{code:?} did its own job as well as cancelling"
            );
            assert!(app
                .status_message
                .as_ref()
                .is_some_and(|s| s.text.contains("cancelled")));
        }
    }

    /// A click moves the selection without going through the key handler, so a
    /// question the cursor has left behind is dropped rather than left armed on
    /// a row whose footer nobody can see.
    #[test]
    fn a_question_the_selection_moved_away_from_is_dropped() {
        let (mut app, _g, _t, _h) = manager(1);
        let mut second = SandboxProfile::new("other", vec![SandboxPath::workspace("~/dev/lib")]);
        second.backend = SandboxBackendKind::Docker;
        app.db.upsert_sandbox_profile(&second).unwrap();
        app.open_sandbox_list();

        app.handle_sandbox_list_key(KeyCode::Char('s'));
        assert!(selected_row(&app).pending.is_some());

        // What a mouse click does: the cursor moves with no keystroke.
        if let modals::Modal::SandboxList(ref mut sl) = app.modal {
            sl.index = 1;
        }
        app.handle_sandbox_list_key(KeyCode::Char('k'));

        assert!(match app.modal {
            modals::Modal::SandboxList(ref sl) =>
                sl.entries.iter().all(|row| row.pending.is_none()),
            _ => false,
        });
        // …and the keystroke did its ordinary job rather than being eaten as an
        // answer to a question nobody could see.
        assert_eq!(selected_row(&app).name, "dev");
    }

    /// A profile with no place has nothing to stop, and says so rather than
    /// arming a question whose answer would do nothing.
    #[test]
    fn stopping_a_profile_with_no_place_is_a_message_not_a_question() {
        let (mut app, _g, _t, _h) = manager(0);
        app.handle_sandbox_list_key(KeyCode::Char('s'));
        assert!(selected_row(&app).pending.is_none());
        assert!(app
            .status_message
            .as_ref()
            .is_some_and(|s| s.text.contains("no live place")));
    }

    /// The profile is re-read when the answer comes in, so a rebuild starts the
    /// place the profile describes *now* — and one that has gone in the meantime
    /// refuses in the words every other launch path uses instead of acting on a
    /// stale copy.
    #[test]
    fn confirming_against_a_deleted_profile_refuses_rather_than_acting() {
        let (mut app, _g, _t, _h) = manager(1);
        app.handle_sandbox_list_key(KeyCode::Char('r'));
        assert!(selected_row(&app).pending.is_some());
        app.db.delete_sandbox_profile("dev").unwrap();

        app.handle_sandbox_list_key(KeyCode::Char('y'));

        assert!(!app.sandbox_gc.in_progress(), "nothing was started");
        assert!(app
            .status_message
            .as_ref()
            .is_some_and(|s| s.text.contains("no longer exists")));
    }

    /// One background slot, because every one of these drives a container
    /// engine: two workers reconciling one engine's containers would be two
    /// opinions about what to remove.
    #[test]
    fn a_second_place_job_waits_for_the_first() {
        let (mut app, _g, _t, _h) = manager(1);
        let _busy = app.sandbox_gc.start();
        app.handle_sandbox_list_key(KeyCode::Char('s'));
        app.handle_sandbox_list_key(KeyCode::Char('y'));
        assert!(app
            .status_message
            .as_ref()
            .is_some_and(|s| s.text.contains("already running")));
    }

    /// A prune with nothing to reclaim says so rather than spawning a worker to
    /// ask an engine about profiles that could never own a place.
    #[test]
    fn pruning_a_policy_only_installation_asks_no_engine() {
        let (mut app, _g, _t) = crate::app::state::tests::app_with_sessions(0);
        let mut policy = SandboxProfile::new("mac", vec![SandboxPath::workspace("~/dev/app")]);
        policy.backend = SandboxBackendKind::Seatbelt;
        app.db.upsert_sandbox_profile(&policy).unwrap();
        app.open_sandbox_list();

        app.handle_sandbox_list_key(KeyCode::Char('p'));

        assert!(!app.sandbox_gc.in_progress());
        assert!(app
            .status_message
            .as_ref()
            .is_some_and(|s| s.text.contains("no sandbox places to reclaim")));
    }

    /// A scoped prune acts on the row it was pressed on and nothing else. The
    /// decision is the whole-installation one, narrowed afterwards, so it can
    /// only ever reclaim *less* than the background pass would.
    #[test]
    fn a_scoped_prune_narrows_the_plan_to_one_profile() {
        use crate::sandbox::container::{GcPlan, InstanceRecord, LiveContainer};
        let live = vec![
            LiveContainer {
                id: "mine".into(),
                profile: Some("dev".into()),
                spec: Some("old".into()),
                owned: true,
            },
            LiveContainer {
                id: "theirs".into(),
                profile: Some("other".into()),
                spec: Some("old".into()),
                owned: true,
            },
        ];
        // A row whose container the engine no longer reports: its profile can
        // only come from the record.
        let records = vec![InstanceRecord {
            profile: "dev".into(),
            external_id: "vanished".into(),
            last_used_at: 0,
        }];
        let mut plan = GcPlan {
            remove: vec!["mine".into(), "theirs".into()],
            forget: vec!["mine".into(), "theirs".into(), "vanished".into()],
            adopt: live.clone(),
        };

        narrow_to_profile(&mut plan, "DEV", &live, &records);

        assert_eq!(plan.remove, ["mine"]);
        assert_eq!(plan.forget, ["mine", "vanished"]);
        assert_eq!(plan.adopt.len(), 1);
        assert_eq!(plan.adopt[0].id, "mine");
    }

    /// A profile whose whole point is the allowlist: nothing permitted, and the
    /// firewall told to ask before it refuses something new.
    fn allowlist_profile(name: &str, ask: bool) -> SandboxProfile {
        let mut profile = SandboxProfile::new(name, vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_mode = NetworkMode::Allowlist;
        profile.prompt_new_domains = ask;
        profile
    }

    /// An [`App`] holding one sandboxed session under `profile`, plus that
    /// session's launch key — which is what the proxy is registered under and
    /// what a denial is tagged with.
    fn boxed_app(
        profile: &SandboxProfile,
    ) -> (App, String, crate::paths::TestPathGuard, tempfile::TempDir) {
        let (mut app, guard, tmp) = crate::app::state::tests::app_with_sessions(1);
        app.db.upsert_sandbox_profile(profile).unwrap();
        app.sessions[0].info.sandbox_profile = Some(profile.name.clone());
        let key = app.sessions[0].info.id.to_string();
        (app, key, guard, tmp)
    }

    fn denial(key: &str, host: &str, port: u16, reason: DenyReason) -> SessionDenial {
        SessionDenial {
            session_key: key.to_string(),
            event: DenialEvent {
                protocol: Protocol::Http,
                host: host.to_string(),
                port,
                reason,
            },
        }
    }

    fn open_prompt(app: &App) -> &DomainPrompt {
        match app.modal {
            modals::Modal::SandboxDomainPrompt(ref prompt) => prompt,
            ref other => panic!("expected the domain prompt, got {other:?}"),
        }
    }

    /// Answer the open question the way a user who has read it would: the
    /// arming window is wound back rather than waited out, so a test measures
    /// the answer instead of the clock. A test about the window itself presses
    /// the key without this.
    fn answer_prompt(app: &mut App, code: KeyCode) {
        if let modals::Modal::SandboxDomainPrompt(ref mut prompt) = app.modal {
            prompt.shown_at =
                std::time::Instant::now().checked_sub(crate::app::egress_prompts::PROMPT_ARMING);
        }
        app.handle_sandbox_domain_prompt_key(code);
    }

    fn stored_allow(app: &App, name: &str) -> Vec<String> {
        app.db
            .get_sandbox_profile(name)
            .unwrap()
            .expect("the profile is stored")
            .profile
            .network_allow
    }

    /// The question names everything the answer depends on, and the refusal
    /// reaches the status bar whether or not it becomes a question.
    #[test]
    fn an_unlisted_domain_raises_a_question_and_a_status_line() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, _t) = boxed_app(&profile);

        app.report_sandbox_denials(vec![denial(
            &key,
            "api.github.com",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();

        let prompt = open_prompt(&app);
        assert_eq!(prompt.session_key, key);
        assert_eq!(prompt.session_name, "session-0");
        assert_eq!(prompt.profile, "dev");
        assert_eq!(prompt.host, "api.github.com");
        assert_eq!(prompt.port, 443);
        // Scoped to the port that was refused: the question said `:443`, so the
        // grant is `:443` and not every port on that host.
        assert_eq!(prompt.rule, "api.github.com:443");

        let status = app.status_message.as_ref().expect("a status line");
        assert_eq!(status.level, StatusLevel::Error);
        assert!(status.text.contains("session-0"), "{}", status.text);
        assert!(
            status.text.contains("api.github.com:443"),
            "{}",
            status.text
        );
    }

    /// Allowing has to do both halves: apply to the boundary that is running
    /// (so the agent's retry succeeds with nothing restarted) and write the rule
    /// into the profile (so the next launch still has it).
    #[test]
    fn allowing_applies_to_the_running_proxy_and_the_stored_profile() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, tmp) = boxed_app(&profile);
        // A real instance for this session, bound on an ephemeral loopback port
        // exactly as a seatbelt launch would leave it. Nothing connects through
        // it; the wire-level half is `proxy::tests`.
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/home/u")
            .expect("a valid profile");
        crate::sandbox::egress::establish(
            &key,
            &policy,
            crate::sandbox::ProxyTransport::Loopback,
            tmp.path(),
        )
        .expect("the proxy binds");

        app.report_sandbox_denials(vec![denial(
            &key,
            "api.github.com",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();
        answer_prompt(&mut app, KeyCode::Char('y'));

        assert!(!app.modal.is_open(), "answering closes the question");
        assert_eq!(
            crate::sandbox::egress::running_allow_rules(&key),
            Some(vec!["api.github.com:443".to_string()]),
            "the running boundary learned the rule"
        );
        assert_eq!(stored_allow(&app, "dev"), ["api.github.com:443"]);
        let status = app.status_message.as_ref().expect("a status line");
        assert_eq!(status.level, StatusLevel::Success);

        crate::sandbox::egress::stop(&key);
    }

    /// The keystroke already in flight when the question appeared belongs to
    /// the pane behind it.
    ///
    /// This is the one modal in friring an *agent* can raise: the user's focus
    /// is a terminal pane, they are typing into it, and the tick puts a
    /// security grant under their hands between two keystrokes. Typing "yes, go
    /// ahead" into a Claude pane must not be what widens a sandbox — and an
    /// agent that can provoke a refusal can choose the moment. So nothing is
    /// answered until the question has been on screen, and the key that grants
    /// is not the one an agent pane is full of.
    #[test]
    fn a_question_that_just_appeared_is_not_answered_by_the_next_keystroke() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, _t) = boxed_app(&profile);

        app.report_sandbox_denials(vec![denial(
            &key,
            "evil.example",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();

        // Mid-word in the pane behind: every one of these arrives before the
        // question could have been read, and none of them answers it.
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('e'),
            KeyCode::Char('s'),
            KeyCode::Enter,
        ] {
            app.handle_sandbox_domain_prompt_key(code);
            assert!(app.modal.is_open(), "{code:?} answered an unread question");
        }
        assert!(stored_allow(&app, "dev").is_empty());

        // And once it has been on screen, `Enter` — the key that used to grant,
        // and the one an agent pane is full of — is the safe answer.
        answer_prompt(&mut app, KeyCode::Enter);
        assert!(!app.modal.is_open());
        assert!(
            stored_allow(&app, "dev").is_empty(),
            "Enter widened the profile"
        );
    }

    /// Refusing writes nothing, anywhere — and the same host never asks again,
    /// however hard the agent retries.
    #[test]
    fn refusing_writes_nothing_and_is_not_asked_twice() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, _t) = boxed_app(&profile);

        app.report_sandbox_denials(vec![denial(
            &key,
            "tracker.example",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();
        answer_prompt(&mut app, KeyCode::Esc);

        assert!(!app.modal.is_open());
        assert!(stored_allow(&app, "dev").is_empty());

        // The agent keeps trying; the user is not asked again and the status
        // bar is not overwritten by the retries.
        app.set_info("something else");
        for _ in 0..20 {
            app.report_sandbox_denials(vec![denial(
                &key,
                "tracker.example",
                443,
                DenyReason::NotAllowlisted,
            )]);
            app.open_next_domain_prompt();
        }
        assert!(!app.modal.is_open(), "a refused host does not re-prompt");
        assert_eq!(app.status_message.as_ref().unwrap().text, "something else");
    }

    /// A burst faster than anyone can answer is one question, and the questions
    /// for other hosts wait their turn behind whatever is already on screen.
    #[test]
    fn a_burst_becomes_one_question_and_the_rest_queue_behind_the_open_modal() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, _t) = boxed_app(&profile);

        let mut burst: Vec<SessionDenial> = (0..25)
            .map(|_| denial(&key, "api.github.com", 443, DenyReason::NotAllowlisted))
            .collect();
        burst.push(denial(&key, "pypi.org", 443, DenyReason::NotAllowlisted));
        app.report_sandbox_denials(burst);

        app.open_next_domain_prompt();
        assert_eq!(open_prompt(&app).host, "api.github.com");
        // A second question does not replace the one being answered.
        app.open_next_domain_prompt();
        assert_eq!(open_prompt(&app).host, "api.github.com");

        answer_prompt(&mut app, KeyCode::Esc);
        app.open_next_domain_prompt();
        assert_eq!(open_prompt(&app).host, "pypi.org");
    }

    /// The user is mid-edit in another modal. A refusal must not throw the
    /// editor away — it waits.
    #[test]
    fn a_refusal_never_steals_an_open_modal() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, _t) = boxed_app(&profile);
        app.open_sandbox_editor();

        app.report_sandbox_denials(vec![denial(
            &key,
            "api.github.com",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();
        assert!(
            matches!(app.modal, modals::Modal::SandboxEditor(_)),
            "the editor kept the screen"
        );

        app.modal.close();
        app.open_next_domain_prompt();
        assert_eq!(open_prompt(&app).host, "api.github.com");
    }

    /// Every other reason is an answer the user already gave, so it is reported
    /// and never asked about. `UnsupportedHost` above all: its answer would be a
    /// rule the profile validator refuses to store.
    #[test]
    fn only_an_unlisted_host_becomes_a_question() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, _t) = boxed_app(&profile);

        app.report_sandbox_denials(vec![
            denial(
                &key,
                "b\u{fffd}cher.example",
                443,
                DenyReason::UnsupportedHost("is not ASCII; write it in punycode"),
            ),
            denial(
                &key,
                "gist.github.com",
                443,
                DenyReason::DeniedByRule("gist.github.com".into()),
            ),
            denial(&key, "anywhere.example", 443, DenyReason::NetworkDisabled),
            denial(&key, "", 0, DenyReason::Unauthorized),
            // The machine friring runs on. Offering this one would be asking
            // the user to hand a sandbox the host's own services, in words
            // ("'session-0' asked for 127.0.0.1:2375") that read like the
            // agent's own dev server.
            denial(&key, "127.0.0.1", 2375, DenyReason::HostLocal),
        ]);
        app.open_next_domain_prompt();

        assert!(!app.modal.is_open(), "none of those is a question");
        let status = app
            .status_message
            .as_ref()
            .expect("they are still reported");
        assert_eq!(status.level, StatusLevel::Error);
        assert!(status.text.contains("+4 more blocked"), "{}", status.text);
    }

    /// `prompt_new_domains` off means "refuse quietly, do not ask me" — the
    /// refusal is still visible, because an agent that cannot reach the network
    /// is failing and the reason is the only way to know why.
    #[test]
    fn a_profile_that_asked_not_to_be_asked_is_only_reported() {
        let profile = allowlist_profile("quiet", false);
        let (mut app, key, _g, _t) = boxed_app(&profile);

        app.report_sandbox_denials(vec![denial(
            &key,
            "api.github.com",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();

        assert!(!app.modal.is_open());
        assert!(app
            .status_message
            .as_ref()
            .is_some_and(|s| s.text.contains("api.github.com:443")));
    }

    /// A session friring no longer holds cannot be asked about — there is no
    /// profile to write an answer to — but the refusal is still reported.
    #[test]
    fn a_refusal_from_an_unknown_session_is_reported_without_a_question() {
        let profile = allowlist_profile("dev", true);
        let (mut app, _key, _g, _t) = boxed_app(&profile);

        app.report_sandbox_denials(vec![denial(
            "a-session-that-is-gone",
            "api.github.com",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();

        assert!(!app.modal.is_open());
        assert!(app
            .status_message
            .as_ref()
            .is_some_and(|s| s.text.contains("a-session-that-is-gone")));
    }

    /// A profile deleted while the question waited has nowhere to keep the
    /// answer, so nothing is written and the message says so rather than
    /// reporting a grant that did not happen.
    #[test]
    fn allowing_against_a_deleted_profile_reports_the_failure() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, tmp) = boxed_app(&profile);
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/home/u")
            .expect("a valid profile");
        crate::sandbox::egress::establish(
            &key,
            &policy,
            crate::sandbox::ProxyTransport::Loopback,
            tmp.path(),
        )
        .expect("the proxy binds");

        app.report_sandbox_denials(vec![denial(
            &key,
            "api.github.com",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();
        app.db.delete_sandbox_profile("dev").unwrap();
        answer_prompt(&mut app, KeyCode::Char('y'));

        let status = app.status_message.as_ref().expect("a status line");
        assert_eq!(status.level, StatusLevel::Error);
        assert!(status.text.contains("no longer exists"), "{}", status.text);
        // The running boundary still learned it: the user allowed it, and this
        // run has it — only the persistence failed.
        assert_eq!(
            crate::sandbox::egress::running_allow_rules(&key),
            Some(vec!["api.github.com:443".to_string()])
        );

        crate::sandbox::egress::stop(&key);
    }

    /// A stale question — the session was torn down while it waited — must not
    /// widen the stored profile on the strength of an answer that reached no
    /// boundary at all.
    #[test]
    fn a_stale_answer_widens_nothing() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, _t) = boxed_app(&profile);

        app.report_sandbox_denials(vec![denial(
            &key,
            "api.github.com",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();
        // No proxy was ever started for this key, which is what a torn-down
        // session looks like from here.
        answer_prompt(&mut app, KeyCode::Char('y'));

        assert!(stored_allow(&app, "dev").is_empty());
        let status = app.status_message.as_ref().expect("a status line");
        assert_eq!(status.level, StatusLevel::Error);
        assert!(status.text.contains("Could not allow"), "{}", status.text);
    }

    /// The other stale answer, and the one that is *not* harmless: the session
    /// is relaunched while its question is on screen.
    ///
    /// The key is stable across a relaunch by design, so an answer given then
    /// would reach the boundary that replaced the one the question was about —
    /// a question about instance N applied to instance N+1, and written into
    /// whatever profile the question named rather than the one now in force.
    /// Clearing the history is not enough, because a question already on screen
    /// is in neither the history nor the queue.
    #[test]
    fn relaunching_a_session_withdraws_the_question_on_screen() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, tmp) = boxed_app(&profile);
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/home/u")
            .expect("a valid profile");
        crate::sandbox::egress::establish(
            &key,
            &policy,
            crate::sandbox::ProxyTransport::Loopback,
            tmp.path(),
        )
        .expect("the proxy binds");

        app.report_sandbox_denials(vec![denial(
            &key,
            "evil.example",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();
        assert_eq!(open_prompt(&app).host, "evil.example");

        // The relaunch: a fresh instance under the same key, from a re-read
        // profile the user may have narrowed in the meantime.
        app.forget_egress_prompts(&key);
        assert!(!app.modal.is_open(), "the question outlived its boundary");

        // A keypress now lands on nothing, and neither boundary is widened.
        app.handle_sandbox_domain_prompt_key(KeyCode::Char('y'));
        assert_eq!(
            crate::sandbox::egress::running_allow_rules(&key),
            Some(vec![])
        );
        assert!(stored_allow(&app, "dev").is_empty());

        // The new boundary asks for itself when the agent asks again.
        app.report_sandbox_denials(vec![denial(
            &key,
            "evil.example",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();
        assert_eq!(open_prompt(&app).host, "evil.example");

        crate::sandbox::egress::stop(&key);
    }

    /// The rule is stored canonically, so the profile holds one spelling of an
    /// address however the sandbox asked for it — and a host with no canonical
    /// spelling is never asked about, because the answer could not be stored.
    #[test]
    fn the_stored_rule_is_canonical_and_port_scoped() {
        assert_eq!(
            allow_rule_for("API.GitHub.COM", 443).as_deref(),
            Some("api.github.com:443")
        );
        assert_eq!(
            allow_rule_for("github.com.", 80).as_deref(),
            Some("github.com:80")
        );
        // A client that spells its destination `.github.com` is asking for the
        // apex; the grant must not be its subtree.
        assert_eq!(
            allow_rule_for(".github.com", 443).as_deref(),
            Some("github.com:443")
        );
        // Every legacy spelling of one address reduces to the address, so the
        // rule the profile keeps is the one every request canonicalises to.
        assert_eq!(
            allow_rule_for("192.0.2.10", 8080).as_deref(),
            Some("192.0.2.10:8080")
        );
        assert_eq!(
            allow_rule_for("3221225994", 8080).as_deref(),
            Some("192.0.2.10:8080")
        );
        // A SOCKS client names IPv6 bare, a CONNECT line brackets it; both end
        // up as the single spelling `DomainRule` parses back.
        assert_eq!(
            allow_rule_for("2001:db8::1", 443).as_deref(),
            Some("[2001:db8::1]:443")
        );
        assert_eq!(
            allow_rule_for("[2001:db8::1]", 443).as_deref(),
            Some("[2001:db8::1]:443")
        );
        // Whatever a rule renders as has to parse back, or the profile stores
        // an entry its own validator would refuse.
        for rendered in ["api.github.com:443", "192.0.2.10:8080", "[2001:db8::1]:443"] {
            assert_eq!(DomainRule::parse(rendered).unwrap().to_string(), rendered);
        }
        // Nothing storable: a U-label, and a refusal with no destination.
        assert_eq!(allow_rule_for("b\u{fc}cher.example", 443), None);
        assert_eq!(allow_rule_for("", 0), None);
    }

    /// The prompt cannot mint a grant on the machine friring runs on. The proxy
    /// refuses these with a reason that never reaches this path, so this is the
    /// second lock rather than the first — and the one that matters, because the
    /// modal would ask about `127.0.0.1:2375` in words the user would read as
    /// the agent's own dev server while granting the host's container daemon.
    #[test]
    fn a_destination_on_the_host_itself_is_never_offered() {
        for host in [
            "127.0.0.1",
            "127.1",
            "2130706433",
            "::1",
            "[::1]",
            "::ffff:127.0.0.1",
            "0.0.0.0",
            "169.254.169.254",
            "fe80::1",
        ] {
            assert_eq!(allow_rule_for(host, 2375), None, "{host} was offered");
        }
        // A name is not one of these, whatever it resolves to: the resolver
        // settles that at connect time, where the proxy checks it.
        assert_eq!(
            allow_rule_for("localhost", 2375).as_deref(),
            Some("localhost:2375")
        );
    }

    /// One destination is one question, however the sandbox spelled it: the
    /// dedup key is canonicalised the same way the stored rule is, so a client
    /// cannot turn a single refused host into a stream of questions by varying
    /// case, the root dot, or an address's notation.
    #[test]
    fn one_destination_in_two_spellings_is_one_question() {
        let mut state = crate::app::egress_prompts::EgressPromptState::default();
        for (first, again) in [
            ("github.com", "GitHub.com."),
            ("127.1", "2130706433"),
            ("2001:db8::1", "[2001:DB8:0:0:0:0:0:1]"),
        ] {
            assert_eq!(state.observe("s1", &prompt_key(first)), Observed::First);
            assert_eq!(state.observe("s1", &prompt_key(again)), Observed::Repeat);
        }
        // A host with no canonical form still deduplicates against itself.
        let u_label = "b\u{fc}cher.example";
        assert_eq!(state.observe("s1", &prompt_key(u_label)), Observed::First);
        assert_eq!(state.observe("s1", &prompt_key(u_label)), Observed::Repeat);
    }

    /// The wiring itself: a refusal arriving on the proxy's own channel reaches
    /// the user through the ordinary tick, with nothing hand-fed.
    ///
    /// Runs one process per test under the repo's runner, which is what makes a
    /// process-wide buffer safe to assert on.
    #[test]
    fn the_tick_drains_the_proxys_own_denial_channel() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, _t) = boxed_app(&profile);
        let _ = crate::sandbox::egress::take_denials();

        crate::sandbox::egress::record_denial_for_test(denial(
            &key,
            "api.github.com",
            443,
            DenyReason::NotAllowlisted,
        ));
        app.tick_core();

        assert_eq!(open_prompt(&app).rule, "api.github.com:443");
        assert!(app
            .status_message
            .as_ref()
            .is_some_and(|s| s.text.contains("api.github.com:443")));
    }

    /// Quitting takes every boundary's way out with it, rather than leaving a
    /// listener and a socket behind for a sandbox that outlives friring under
    /// tmux.
    #[test]
    fn quitting_stops_the_proxies_it_started() {
        let profile = allowlist_profile("dev", true);
        let (app, key, _g, tmp) = boxed_app(&profile);
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/home/u")
            .expect("a valid profile");
        crate::sandbox::egress::establish(
            &key,
            &policy,
            crate::sandbox::ProxyTransport::Loopback,
            tmp.path(),
        )
        .expect("the proxy binds");
        assert!(crate::sandbox::egress::running_allow_rules(&key).is_some());

        app.shutdown();

        assert!(
            crate::sandbox::egress::running_allow_rules(&key).is_none(),
            "the session's proxy outlived the friring that started it"
        );
    }

    #[test]
    fn an_unstorable_host_is_reported_but_never_asked_about() {
        let profile = allowlist_profile("dev", true);
        let (mut app, key, _g, _t) = boxed_app(&profile);

        app.report_sandbox_denials(vec![denial(
            &key,
            "b\u{fc}cher.example",
            443,
            DenyReason::NotAllowlisted,
        )]);
        app.open_next_domain_prompt();

        assert!(!app.modal.is_open(), "its answer could not be stored");
        assert!(app.status_message.is_some());
    }

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
            .iter()
            .map(|p| profile_row(&host, p, Vec::new()))
            .collect();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "dev");
        assert_eq!(rows[0].paths, 2);
        assert_eq!(rows[0].network, NetworkMode::Full);
        // An explicit backend renders without an arrow; only `auto` gets one.
        assert!(!rows[0].summary().contains('→'), "{}", rows[0].summary());
        assert!(rows[0].is_intact());
    }

    /// A corrupt row still lists — it is repairable, and the list is the only
    /// way in — but it is marked, because its backend, path count and network
    /// mode would otherwise be reported as the profile's when they are
    /// storage's substitutions.
    #[test]
    fn a_row_that_did_not_decode_lists_as_invalid() {
        let db = crate::storage::Database::open_in_memory().unwrap();
        db.insert_undecodable_sandbox_profile("broken").unwrap();
        let host = SandboxHost::new(std::sync::Arc::new(
            crate::sandbox::probe::StubHost::linux_with_bwrap("0.11.0"),
        ));

        let stored = db.list_sandbox_profiles().unwrap();
        let rows: Vec<SandboxProfileRow> = stored
            .iter()
            .map(|p| profile_row(&host, p, Vec::new()))
            .collect();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "broken");
        assert!(!rows[0].is_intact());
        assert!(rows[0].undecoded.contains(&"read_scope".to_string()));
        assert!(rows[0].undecoded.contains(&"network_deny".to_string()));
        assert!(
            rows[0].summary().contains("unreadable"),
            "{}",
            rows[0].summary()
        );
    }

    /// The toast a launch that landed on the host raises. Only that outcome
    /// raises one: a boundary that went on, and a session that never asked for
    /// one, are both silent.
    #[test]
    fn only_an_unenforced_boundary_produces_a_message() {
        let mut info = crate::session::SessionInfo::new("api".to_string());
        assert!(unenforced_sandbox_message(&info).is_none());

        info.sandbox_profile = Some("dev".to_string());
        info.sandbox_state = Some(crate::session::SandboxState::Applied(
            "seatbelt · inner agent sandbox: off".to_string(),
        ));
        assert!(unenforced_sandbox_message(&info).is_none());

        info.sandbox_state = Some(crate::session::SandboxState::Unenforced(
            "bwrap is not installed".to_string(),
        ));
        let message = unenforced_sandbox_message(&info).expect("a fallback is reported");
        assert!(message.contains("'api'"), "{message}");
        assert!(message.contains("NOT sandboxed"), "{message}");
        assert!(message.contains("'dev'"), "{message}");
        assert!(message.contains("bwrap is not installed"), "{message}");
    }

    /// A boundary that holds and an agent with no credential in it is a notice,
    /// not an error — and an agent on the host outranks it, because only one
    /// status line exists and that is the more serious fact.
    #[test]
    fn a_missing_login_is_reported_but_never_over_a_missing_boundary() {
        let mut info = crate::session::SessionInfo::new("api".to_string());
        info.sandbox_profile = Some("dev".to_string());
        info.sandbox_state = Some(crate::session::SandboxState::Applied(
            "podman · inner agent sandbox: off".to_string(),
        ));
        // A boundary that holds and nothing to do is silent.
        assert!(sandbox_launch_notice(&info).is_none());

        info.sandbox_login = Some("sign in inside this pane: /login".to_string());
        let (level, message) = sandbox_launch_notice(&info).expect("a login is reported");
        assert_eq!(level, super::super::StatusLevel::Info);
        assert!(message.contains("'api'"), "{message}");
        assert!(message.contains("'dev'"), "{message}");
        assert!(message.contains("/login"), "{message}");

        // The fallback wins while both are true: an agent outside its boundary
        // is worse news than one that has to log in.
        info.sandbox_state = Some(crate::session::SandboxState::Unenforced(
            "podman is not installed".to_string(),
        ));
        let (level, message) = sandbox_launch_notice(&info).expect("the fallback is reported");
        assert_eq!(level, super::super::StatusLevel::Error);
        assert!(message.contains("NOT sandboxed"), "{message}");
    }
}
