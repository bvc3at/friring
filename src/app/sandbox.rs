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
use crate::ui::sandbox_list_modal::SandboxProfileRow;
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
                let instance = self.place_state(&p.profile.name);
                profile_row(host, &p, instance)
            })
            .collect()
    }

    /// What the profile list says about a profile's live place: the state of
    /// its most recently used instance, or `None` when it has none.
    ///
    /// A profile may own several rows — a rebuild adds one rather than
    /// overwriting the previous id, which is what keeps the superseded container
    /// findable — so the most recent is the one that describes the place
    /// sessions are actually in.
    fn place_state(&self, profile: &str) -> Option<String> {
        self.db
            .list_sandbox_instances_for_profile(profile)
            .ok()?
            .into_iter()
            .max_by_key(|row| row.last_used_at)
            .map(|row| row.state)
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
                // The profile's own tree — its synthetic home, the credential
                // it was signed into and every session directory inside it —
                // belongs to the profile, so it goes with it. The *place* does
                // not: nothing here stops a running container, and the
                // reclaiming pass is what removes one whose profile is gone.
                crate::sandbox::dirs::cleanup_place(name);
                // …and with the copy gone, so is friring's record that this
                // profile held the one permitted seed of a credential family.
                // Leaving it would refuse every future profile a `seed-file`
                // on behalf of a boundary that no longer exists (ADR-28).
                crate::sandbox::auth::release_seeds(name);
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
        let Some(sweep) = self.sandbox_gc_input() else {
            return;
        };
        let tx = self.sandbox_gc.start();
        std::thread::spawn(move || {
            let _ = tx.send(sweep.run());
        });
    }

    /// Everything the pass needs from this instance, gathered on the UI thread:
    /// the profiles that could own a place, the recorded rows, and the places
    /// that must survive whatever else is true of them.
    ///
    /// `None` when there is nothing a pass could act on, so an installation with
    /// no place profiles never spawns a worker.
    fn sandbox_gc_input(&self) -> Option<GcSweep> {
        let profiles: Vec<SandboxProfile> = self
            .db
            .list_sandbox_profiles()
            .unwrap_or_default()
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
        )> = [SandboxBackendKind::Docker, SandboxBackendKind::Podman]
            .into_iter()
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
            records,
            in_use: self.places_in_use(),
        })
    }

    /// What a pass may not touch: the places live sessions are in.
    ///
    /// Two sources, and the second is the careful one. This instance knows
    /// exactly which container each of *its* place-backed sessions is in. It
    /// knows nothing about a session another friring is driving — only that a
    /// row for it exists — so a profile with a live session row this instance is
    /// not driving protects **every** container of that profile by name. The
    /// cost is a superseded container surviving until that session ends; the
    /// alternative is pulling a place out from under somebody else's agent.
    fn places_in_use(&self) -> PlacesInUse {
        let mut ids: HashSet<String> = HashSet::new();
        let mut profiles: HashSet<String> = HashSet::new();
        for shared in self.db.list_active_sessions().unwrap_or_default() {
            let Some(profile) = crate::session::sandbox_backend_profile(&shared.backend_type)
            else {
                continue;
            };
            match self.place_containers.get(&shared.backend_type) {
                Some(id) => {
                    ids.insert(id.clone());
                }
                None => {
                    profiles.insert(profile.to_string());
                }
            }
        }
        PlacesInUse { ids, profiles }
    }

    /// Apply a finished pass: forget the rows it reconciled away, re-record the
    /// containers it adopted, and report only what went wrong.
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
        for failure in outcome.failures {
            // A place that will not go is the next pass's problem, but the user
            // is the one paying for the disk, so it is said once rather than
            // only logged.
            self.set_error(format!("Could not reclaim a sandbox place: {failure}"));
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
                let row = profile_row(host, &stored, None);
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
    records: Vec<(
        SandboxBackendKind,
        crate::sandbox::container::InstanceRecord,
    )>,
    in_use: PlacesInUse,
}

/// What a pass must leave alone — see [`App::places_in_use`].
struct PlacesInUse {
    ids: HashSet<String>,
    profiles: HashSet<String>,
}

/// What a finished pass leaves for the UI thread to write down.
pub(crate) struct GcOutcome {
    forget: Vec<(SandboxBackendKind, String)>,
    adopt: Vec<crate::storage::sandboxes::SandboxInstance>,
    failures: Vec<String>,
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
        let mut outcome = GcOutcome {
            forget: Vec::new(),
            adopt: Vec::new(),
            failures: Vec::new(),
        };
        let host = SandboxHost::local_shared();
        for engine in [SandboxBackendKind::Docker, SandboxBackendKind::Podman] {
            let Some(backend) = host.container(engine) else {
                continue;
            };
            let Ok(live) = backend.live_places() else {
                continue;
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
                        unplannable.insert(profile.name.clone());
                    }
                }
            }
            let in_use: Vec<String> = live
                .iter()
                .filter(|container| {
                    self.in_use.ids.contains(&container.id)
                        || container.profile.as_ref().is_some_and(|profile| {
                            self.in_use.profiles.contains(profile) || unplannable.contains(profile)
                        })
                })
                .map(|container| container.id.clone())
                .collect();

            let plan = crate::sandbox::container::gc_plan(crate::sandbox::container::GcInput {
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
            outcome.failures.extend(backend.reap(&plan));
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
        outcome
    }
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
    instance: Option<String>,
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
        instance,
        unavailable: backend_unavailable(host, p.backend, resolved),
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

        let shared = crate::sync::SharedSession {
            id: crate::session::SessionId::default(),
            name: "demo".into(),
            agent: "claude".into(),
            backend_id: String::new(),
            backend_type: "sandbox:dev".into(),
            agent_session_id: Some("conv".into()),
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: Vec::new(),
            shell_backend_id: None,
            sandbox_profile: Some("dev".into()),
            sandbox_enforcement: Default::default(),
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
        };
        app.db.upsert_session(&shared).unwrap();

        // Nothing opened this place here, so the profile is protected whole.
        let in_use = app.places_in_use();
        assert!(in_use.ids.is_empty());
        assert!(in_use.profiles.contains("dev"));

        // Once this instance knows which container the session is in, the
        // protection narrows to that one — which is what lets a superseded
        // container be reclaimed while its replacement is in use.
        app.place_containers
            .insert("sandbox:dev".into(), "ctr1".into());
        let in_use = app.places_in_use();
        assert!(in_use.ids.contains("ctr1"));
        assert!(in_use.profiles.is_empty());
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
        assert!(app.sandbox_gc_input().is_none());

        let mut place = SandboxProfile::new("box", vec![SandboxPath::workspace("~/dev/app")]);
        place.backend = SandboxBackendKind::Podman;
        app.db.upsert_sandbox_profile(&place).unwrap();
        assert!(app.sandbox_gc_input().is_some());
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
            .map(|p| profile_row(&host, p, None))
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
        let rows: Vec<SandboxProfileRow> =
            stored.iter().map(|p| profile_row(&host, p, None)).collect();

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
