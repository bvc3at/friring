//! Headless session deletion — soft-delete by default, `force` also tears
//! down the tmux window, worktrees, and pending scheduled commands so the
//! filesystem and tmux server don't leak orphans when the TUI isn't
//! running to observe the deletion.

use crate::session::SessionId;
use crate::storage::Database;

/// Outcome of a force-delete, reported to callers for their JSON payload.
#[derive(Debug, Clone, Default)]
pub struct ForceDeleteReport {
    pub killed_window: bool,
    pub removed_worktrees: Vec<String>,
    pub worktree_errors: Vec<String>,
    pub disabled_automations: usize,
    /// Set when the session lived on a remote host (SSH/WSL) and its window
    /// could not be torn down there: the host is unreachable, has no
    /// `hosts.toml` entry, or the session carries no pane id. Best-effort — an
    /// unreachable host is expected (that's often *why* someone force-deletes),
    /// so this is recorded rather than aborting the delete.
    pub remote_teardown_error: Option<String>,
    /// The bridge children this delete stopped first (ADR-32).
    pub stopped_children: Vec<String>,
}

/// Stop every live bridge child of a session being force-deleted.
///
/// The owner is going away, so there is nobody left to receive a `result`, and
/// the grace period a `stop` verb grants exists precisely so a child can send
/// one. Each child's runtime is torn down and its state recorded.
///
/// **The recorded state is what the teardown achieved**, never what it
/// attempted. A pane that refused to die is `stop_failed` — which
/// [`ChildState::is_live`](crate::session::ChildState::is_live) still counts as
/// live, so the child keeps its fan-out slot and an operator is told to look —
/// and only a teardown that reported nothing wrong is `stopped`. Recording a
/// terminal state friring did not verify is the one outcome that corrupts every
/// later decision: integration reads it, `status` reports it, and the slot is
/// released for a child that may still be writing its worktree. A child whose
/// session row cannot be read is `stop_failed` for the same reason: nothing was
/// torn down, so nothing was verified dead.
///
/// **Not deleted.** The child's worktree, branch and rows survive: a
/// force-deleted owner is a reason to stop its workers, not a licence to throw
/// away what they wrote.
///
/// **An unreadable row aborts the delete.** The cascade is the only thing that
/// stops this owner's workers, and a read that failed is not the same answer as
/// "there are none": `unwrap_or_default()` here would delete the owner with its
/// children still running, holding worktrees nothing can now find. So the whole
/// force-delete is refused and the operator is told which read failed — the
/// escape hatch is narrower than it was, and it is still there, because a
/// child's own row can be force-deleted directly.
fn stop_owned_children(db: &Database, owner: SessionId) -> Result<Vec<String>, String> {
    let mut stopped = Vec::new();
    let children = db.bridge_children_of(&owner.to_string()).map_err(|e| {
        format!(
            "Session {owner} owns bridge children and friring could not read them ({e}), so it \
             will not delete the one session that could stop them. Repair the database, or \
             force-delete each child by id first"
        )
    })?;
    for child in children {
        // Fail closed on a read error, exactly as the cascade above does: an
        // unreadable state is not evidence that a child is finished, and
        // skipping it would leave an agent running for an owner that is gone.
        let live = match db.bridge_child_state(&child.child_id) {
            Ok(row) => row.is_some_and(|row| row.state.is_live()),
            Err(e) => {
                return Err(format!(
                    "friring could not read the state of child '{}' ({e}), so it cannot tell \
                     whether stopping it is still needed and will not delete its owner",
                    child.child_id
                ))
            }
        };
        if !live {
            continue;
        }
        let Ok(id) = child.child_id.parse::<SessionId>() else {
            continue;
        };
        // `false` until a teardown says otherwise: a session row that cannot be
        // read, or is not there, is a child friring never tried to stop and
        // certainly did not verify dead. It keeps its slot and an operator is
        // told to look, which is the whole rule above.
        let mut torn_down = false;
        if let Ok(Some(row)) = db.get_session_by_id(id) {
            let recorded = recorded_places(db, &row.backend_type);
            let mut child_report = ForceDeleteReport::default();
            teardown_runtime_resources(&row, &recorded, &mut child_report);
            // A window friring could not kill, or a remote host it could not
            // reach: either way the child's agent may still be running.
            torn_down = child_report.remote_teardown_error.is_none()
                && (child_report.killed_window || row.backend_id.is_empty());
        }
        let state = if torn_down {
            crate::session::ChildState::Stopped
        } else {
            crate::session::ChildState::StopFailed
        };
        let _ = db.set_bridge_child_state(&child.child_id, state);
        if torn_down {
            stopped.push(child.child_id);
        }
    }
    Ok(stopped)
}

/// Soft-delete a session and (when `force`) also tear down its runtime
/// resources: the tmux window, on-disk worktrees, and any pending
/// scheduled commands queued against it.
///
/// Worktree and tmux cleanup are best-effort — individual failures are
/// captured in the report but do not abort the delete. The DB row is
/// always soft-deleted last so `Ctrl+U` / `restore_session` can still
/// revive the metadata (the TUI will re-spawn a fresh window on restore).
pub fn delete_session_headless(
    db: &Database,
    session_id: SessionId,
    force: bool,
) -> Result<ForceDeleteReport, String> {
    let session = db
        .get_session_by_id(session_id)
        .map_err(|e| format!("get_session_by_id: {e}"))?
        .ok_or_else(|| format!("Session not found: {session_id}"))?;

    let mut report = ForceDeleteReport::default();

    // A **live** bridge child is not soft-deletable. Without `force` there is no
    // runtime teardown at all, so the row would be stamped force-deleted and its
    // owner told the child is gone while its agent kept writing the worktree a
    // later verdict reads. The owner's own `stop` verb is the path that stops a
    // child and has the host verify it; `--force` is the operator's override,
    // and it does tear the runtime down.
    if !force {
        let live = db
            .bridge_child_state(&session_id.to_string())
            .ok()
            .flatten()
            .is_some_and(|row| row.state.is_live());
        if live {
            return Err(format!(
                "Session {session_id} is a running bridge child. Its owner's 'stop' is what \
                 stops one, so friring can verify the pane died before anything reads its \
                 worktree — or pass --force to take it away anyway"
            ));
        }
    }

    if force {
        // The owner's children first (ADR-32). A force-delete takes away the
        // one session that could ever answer a child's `blocked`, read its
        // `result` or integrate its branch, so leaving them running would leave
        // agents working for nobody — and holding worktrees an operator has no
        // way left to find.
        // Before anything of the owner's own runtime is touched: a refusal here
        // must leave the session exactly as it was, not half torn down.
        report.stopped_children = stop_owned_children(db, session_id)?;
        let recorded = recorded_places(db, &session.backend_type);
        teardown_runtime_resources(&session, &recorded, &mut report);
        report.disabled_automations = db
            .disable_send_automations_for_session(session_id)
            .map_err(|e| format!("disable_send_automations_for_session: {e}"))?;
    }
    // An operator took a child away. Its owner is told in friring's own words,
    // because a leader polling `status` would otherwise see a child that simply
    // stopped existing.
    if let Ok(Some(row)) = db.bridge_child(&session_id.to_string()) {
        let _ = db.mark_bridge_child_force_deleted(&session_id.to_string());
        if let Ok(owner) = row.owner_id.parse::<SessionId>() {
            let _ = db.enqueue_message_capped(
                &crate::storage::messages::NewMessage {
                    to_session_id: owner,
                    from_session_id: None,
                    from_task_id: None,
                    kind: crate::session::bridge::MailKind::ChildRemovedByOperator
                        .as_str()
                        .to_string(),
                    body: serde_json::json!({ "child": session_id.to_string() }).to_string(),
                    in_reply_to: None,
                },
                crate::session::bridge::MAX_UNREAD_PER_OWNER,
            );
        }
    }

    db.soft_delete_session(session_id)
        .map_err(|e| format!("soft_delete_session: {e}"))?;

    // Record that this delete tore down the worktrees/tmux so the restore list
    // can tag + block it (a force-delete is not restorable).
    if force {
        db.mark_session_force_deleted(session_id)
            .map_err(|e| format!("mark_session_force_deleted: {e}"))?;
    }

    Ok(report)
}

/// The places friring has **recorded** for the profile a session is sandboxed
/// under, or nothing when it is not sandboxed into one.
///
/// Read where a `Database` is in hand and handed to
/// [`teardown_runtime_resources`], which deliberately touches no SQLite of its
/// own. It is what keeps a place findable across a profile rename: the running
/// container still carries the label it was created with, and only these rows
/// were rewritten — see
/// [`running_places`](crate::agent::sandboxing::running_places).
pub fn recorded_places(db: &Database, backend_type: &str) -> Vec<String> {
    let Some(profile) = crate::session::sandbox_backend_profile(backend_type) else {
        return Vec::new();
    };
    db.list_sandbox_instances_for_profile(profile)
        .map(|rows| rows.into_iter().map(|row| row.external_id).collect())
        .unwrap_or_default()
}

/// Tear down a session's slow runtime resources: kill the tmux window, remove
/// worktrees + the symlink workspace. Touches no SQLite — safe to call from a
/// background thread after the row has been soft-deleted on the UI thread, so
/// the TUI's hard-delete confirmation can close without blocking on a remote
/// `kill-window` or a `git worktree remove`. Best-effort: failures are logged
/// into `report` (or `tracing::warn`), never abort.
///
/// `recorded` is [`recorded_places`]' answer, read by the caller for exactly
/// that reason: a place-backed session's container is found by profile name,
/// and after a rename the rows are the only half that still says the new one.
///
/// **Backend-aware.** The window kill and each worktree removal run on the
/// server the session actually lives on, resolved from `session.backend_type`:
/// a local backend uses the local tmux socket + local `git`; an `ssh:`/`wsl:`
/// backend kills the pane and removes the worktrees over that host's launcher.
/// The symlink workspace is always local (a spawn-time process-cwd detail under
/// the local data dir), so it is torn down regardless of backend.
pub fn teardown_runtime_resources(
    session: &crate::sync::SharedSession,
    recorded: &[String],
    report: &mut ForceDeleteReport,
) {
    if let Some(profile) = crate::session::sandbox_backend_profile(&session.backend_type) {
        // A **place**-backed session: its pane is inside the container, so the
        // local kill would find nothing and leave the agent running. Worktree
        // removal stays local — a place mounts every path at exactly its host
        // path, so the checkout the container sees *is* the host's.
        let places = crate::agent::sandboxing::running_places(profile, recorded);
        if places.is_empty() {
            // Not an error: a place that is not running took every pane in it
            // with it, which is the outcome this call wanted.
            tracing::info!(
                "sandbox place '{profile}' is not running; session '{}' had no pane to kill",
                session.name
            );
        } else {
            kill_place_window(&places, session, report);
        }
        for wt in &session.worktrees {
            remove_worktree_into(None, wt, report);
        }
    } else if crate::session::is_remote_backend(&session.backend_type) {
        // Off-local session: kill the pane + remove worktrees on the host. An
        // unresolvable/unreachable host is expected — record it, never abort.
        let registry = crate::agent::host_config::load_all();
        match registry.get_by_backend(&session.backend_type) {
            Some(host) => {
                kill_remote_window(host, session, report);
                for wt in &session.worktrees {
                    remove_worktree_into(Some(host), wt, report);
                }
            }
            None => {
                let msg = format!(
                    "remote host '{}' not found in hosts.toml; \
                     left its window + worktrees in place",
                    session.backend_type
                );
                tracing::warn!("{msg}");
                report.remote_teardown_error = Some(msg);
            }
        }
    } else {
        kill_local_window(session, report);
        for wt in &session.worktrees {
            remove_worktree_into(None, wt, report);
        }
    }

    // Tear down the multi-repo symlink workspace (if any). Only the symlinks
    // are removed — the underlying repos are untouched. Always local: the
    // workspace lives under the local data dir even for a remote session.
    if let Some(asid) = &session.agent_session_id {
        if let Err(e) = crate::workspace::remove_workspace(asid) {
            tracing::warn!("remove_workspace({asid}) failed: {e}");
        }
    }
    // A user-chosen workspace dir is not derivable from the id — remove it via
    // its persisted path. Guarded: only a symlink-only dir is ever deleted.
    if let Some(ws) = &session.workspace_dir {
        if let Err(e) = crate::workspace::remove_workspace_at(ws) {
            tracing::warn!("remove_workspace_at({}) failed: {e}", ws.display());
        }
    }
    // The scratch directory a sandboxed launch minted, and the policy file
    // generated for it, both under the data directory (`docs/SANDBOX.md`
    // §Launch integration). Keyed on the session id, so it is the *desired*
    // profile that says whether there is anything to drop — a launch that fell
    // back to the host keeps its profile and may have minted the scratch before
    // failing. Always local: friring writes them beside its own database, on
    // whichever machine composed the launch.
    if session.sandbox_profile.is_some() {
        crate::agent::sandboxing::cleanup_by_session_id(session.id);
    }
}

/// Kill the session's window on the local tmux server, reaping the pane's child
/// process on Windows (where a live process's cwd blocks the later rmdir).
fn kill_local_window(session: &crate::sync::SharedSession, report: &mut ForceDeleteReport) {
    // Capture the pane's OS pid *before* the kill so we can reap the pane's child
    // process below. Windows refuses to remove a directory that is a live
    // process's cwd, and a session's agent runs with cwd = its worktree /
    // extension home; Unix has no such restriction, so this is Windows-only.
    #[cfg(windows)]
    let pane_pid = crate::agent::tmux::window_pane_pid(&session.name)
        .ok()
        .flatten();

    match crate::agent::tmux::kill_window(&session.name) {
        Ok(()) => report.killed_window = true,
        Err(e) => tracing::warn!("kill_window({}) failed: {e}", session.name),
    }

    // `kill-window` returns before the OS reaps the pane's child process; wait
    // for it (force-terminating as a backstop) before the rmdir steps below.
    // NOTE: this only handles a handle held by the *pane child*. psmux ALSO holds
    // a server-level handle to each pane's `-c` cwd that only `kill-server`
    // releases (verified in the Windows VM) — which we can't do per-session on
    // the shared server. So removing a just-deleted session's own working dir can
    // still fail on Windows; that is a documented psmux limitation, not covered
    // here.
    #[cfg(windows)]
    if let Some(pid) = pane_pid {
        reap_pane_process(pid);
    }
}

/// Kill the session's window **inside a sandbox place**, by name, in every place
/// the profile currently has.
///
/// The place twin of [`kill_remote_window`] and best-effort like it, but
/// addressed by `tb-<session>` rather than by the persisted pane id — because
/// unlike a remote host, a profile can have **several live containers at once**
/// (an edited profile builds a new one while the sessions already launched stay
/// in the old), the session row records no container, and a pane id is per tmux
/// server. Killing `%1` in the wrong container of the right profile kills a
/// different session's agent; killing `tb-<session>` there kills nothing,
/// because the name is unique to the session wherever it lives.
///
/// So every place is asked, and a window found in none of them is not a failure:
/// a place that has gone away since the row was written took its panes with it,
/// which is the outcome this call wanted.
fn kill_place_window(
    places: &[crate::agent::transport::Place],
    session: &crate::sync::SharedSession,
    report: &mut ForceDeleteReport,
) {
    let mut failures: Vec<String> = Vec::new();
    for place in places {
        let target = crate::agent::tmux::MuxTarget::for_place(place);
        match crate::agent::tmux::kill_window_on(&target, &session.name) {
            Ok(true) => report.killed_window = true,
            Ok(false) => {}
            Err(e) => failures.push(format!("{}: {e}", place.container())),
        }
    }
    if !report.killed_window && !failures.is_empty() {
        let msg = format!(
            "could not kill session '{}' in sandbox place '{}': {}",
            session.name,
            places[0].profile(),
            failures.join("; ")
        );
        tracing::warn!("{msg}");
        report.remote_teardown_error = Some(msg);
    }
}

/// Kill the session's pane on a remote host by its persisted pane id (`%N`) —
/// the addressable unit remotely (there's no cheap "window by friring name"
/// lookup over the wire). Best-effort: a blank pane id or an unreachable host
/// is recorded in `report.remote_teardown_error`, never aborts.
fn kill_remote_window(
    host: &crate::session::HostDef,
    session: &crate::sync::SharedSession,
    report: &mut ForceDeleteReport,
) {
    let pane = session.backend_id.trim();
    if pane.is_empty() {
        let msg = format!(
            "session '{}' on {} has no pane id; could not kill its remote window",
            session.name,
            host.backend_name()
        );
        tracing::warn!("{msg}");
        report.remote_teardown_error = Some(msg);
        return;
    }
    match crate::agent::tmux::kill_pane_remote(host, pane) {
        Ok(()) => report.killed_window = true,
        Err(e) => {
            let msg = format!(
                "kill_pane_remote({}, {pane}) failed: {e}",
                host.backend_name()
            );
            tracing::warn!("{msg}");
            report.remote_teardown_error = Some(msg);
        }
    }
}

/// Wait (≈5s) for a killed pane's child process to exit, force-terminating it as
/// a backstop if it outlives the grace period. Windows-only: a live process's
/// cwd is unremovable on Windows, and a session's agent runs in its worktree.
/// This reaps the *pane child* only — psmux's own server-level handle on the
/// pane's `-c` cwd is a separate, un-fixable-per-session issue (see callsite).
#[cfg(windows)]
fn reap_pane_process(pid: u32) {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    let kind = sysinfo::ProcessRefreshKind::nothing();
    let mut terminated = false;
    for tick in 0..50 {
        // `remove_dead_processes = true` drops exited pids, so `process(pid)`
        // going `None` means the process is truly gone.
        sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), true, kind);
        match sys.process(pid) {
            None => return,
            Some(proc) => {
                // Give it ~2s to exit on its own, then force the kill.
                if !terminated && tick >= 20 {
                    proc.kill();
                    terminated = true;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Best-effort worktree removal on `host` (local when `None`), recording
/// success/failure into `report`. Removes the worktree *directory* only — the
/// git branch is deliberately left behind (local and remote alike), matching
/// force-delete's contract.
fn remove_worktree_into(
    host: Option<&crate::session::HostDef>,
    wt: &crate::sync::SharedWorktree,
    report: &mut ForceDeleteReport,
) {
    match crate::git::remove_worktree_on(host, &wt.repo_path, &wt.worktree_path) {
        Ok(()) => report
            .removed_worktrees
            .push(wt.worktree_path.display().to_string()),
        Err(e) => report
            .worktree_errors
            .push(format!("{}: {e}", wt.worktree_path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;
    use crate::sync::SharedSession;

    fn insert_session(db: &Database, name: &str) -> SessionId {
        insert_session_on(db, name, "local-tmux", "")
    }

    /// Insert a session with an explicit backend type + pane id, so the remote
    /// teardown paths can be exercised.
    fn insert_session_on(
        db: &Database,
        name: &str,
        backend_type: &str,
        backend_id: &str,
    ) -> SessionId {
        let id = SessionId::default();
        let shared = SharedSession {
            id,
            name: name.into(),
            agent: "dev".into(),
            backend_id: backend_id.into(),
            backend_type: backend_type.into(),
            agent_session_id: Some(uuid::Uuid::new_v4().to_string()),
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
        id
    }

    fn send_automation(db: &Database, session_id: SessionId, name: &str) -> i64 {
        use crate::session::{AutomationAction, AutomationSchedule};
        use crate::storage::automations::NewAutomation;
        db.create_automation(&NewAutomation {
            name: name.into(),
            enabled: true,
            schedule: AutomationSchedule::Once { at: u64::MAX },
            timezone: None,
            action: AutomationAction::send_to(session_id),
            prompt: "noop".into(),
            next_run_at: Some(u64::MAX),
            prompt_steps: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn soft_delete_without_force_leaves_no_side_effects() {
        let db = Database::open_in_memory().unwrap();
        let id = insert_session(&db, "demo");

        // Send automation targeting the session — should survive a soft delete.
        let auto = send_automation(&db, id, "noop");

        let report = delete_session_headless(&db, id, false).unwrap();
        assert!(!report.killed_window);
        assert!(report.removed_worktrees.is_empty());
        assert_eq!(report.disabled_automations, 0);

        assert!(db.get_session_by_id(id).unwrap().is_none());
        assert!(db.get_automation(auto).unwrap().unwrap().enabled);
    }

    #[test]
    fn force_delete_disables_send_automations() {
        let db = Database::open_in_memory().unwrap();
        let id = insert_session(&db, "demo");

        let a = send_automation(&db, id, "a");
        let b = send_automation(&db, id, "b");

        let report = delete_session_headless(&db, id, true).unwrap();
        assert_eq!(report.disabled_automations, 2);
        assert!(!db.get_automation(a).unwrap().unwrap().enabled);
        assert!(!db.get_automation(b).unwrap().unwrap().enabled);
    }

    #[test]
    fn force_delete_marks_force_deleted_but_soft_does_not() {
        let db = Database::open_in_memory().unwrap();

        let soft = insert_session(&db, "soft");
        delete_session_headless(&db, soft, false).unwrap();
        assert!(
            !db.get_deleted_session_by_id(soft)
                .unwrap()
                .unwrap()
                .force_deleted,
            "a soft delete stays restorable"
        );

        let hard = insert_session(&db, "hard");
        delete_session_headless(&db, hard, true).unwrap();
        assert!(
            db.get_deleted_session_by_id(hard)
                .unwrap()
                .unwrap()
                .force_deleted,
            "a force delete is flagged as not restorable"
        );
    }

    // The resolved-remote-host kill/worktree path (a configured, reachable
    // host) is not unit-tested here: it needs a live SSH/WSL host and
    // `kill_pane_remote` would issue a real connection. The routing is thin —
    // `remove_worktree_on` / `kill_pane_remote` are exercised where they live —
    // so these tests cover the two host-resolution failure modes instead.
    // (cfg(test) sandboxes the config dir, so `load_all` sees an empty
    // `hosts.toml` and never touches the real network.)

    #[test]
    fn force_delete_remote_session_with_no_configured_host_records_error() {
        let db = Database::open_in_memory().unwrap();
        let id = insert_session_on(&db, "remote", "ssh:devbox", "%3");

        let report = delete_session_headless(&db, id, true).unwrap();

        // No matching host in (the empty test) hosts.toml → recorded, not killed.
        assert!(!report.killed_window);
        let err = report
            .remote_teardown_error
            .expect("remote teardown error recorded");
        assert!(err.contains("ssh:devbox"), "got {err}");

        // The row is still soft- + force-deleted (best-effort teardown).
        assert!(db.get_session_by_id(id).unwrap().is_none());
        assert!(
            db.get_deleted_session_by_id(id)
                .unwrap()
                .unwrap()
                .force_deleted
        );
    }

    #[test]
    fn force_delete_local_session_records_no_remote_error() {
        let db = Database::open_in_memory().unwrap();
        let id = insert_session(&db, "local");

        let report = delete_session_headless(&db, id, true).unwrap();
        assert!(report.remote_teardown_error.is_none());
    }

    #[test]
    fn remote_teardown_with_unresolved_host_leaves_worktrees_untouched() {
        // A remote session whose host isn't configured: its worktree dirs live
        // on that (now unreachable) host, so they must be left alone — NOT
        // attempted against the local `git`, which would either error out or,
        // worse, act on a same-path local directory. Exercises the routing
        // directly (no DB round-trip needed for the worktree list).
        let session = SharedSession {
            id: SessionId::default(),
            name: "remote".into(),
            agent: "dev".into(),
            backend_id: "%3".into(),
            backend_type: "wsl:Ubuntu".into(),
            // `None` so no local symlink-workspace cleanup is attempted either.
            agent_session_id: None,
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: vec![crate::sync::SharedWorktree {
                repo_path: "/nonexistent/repo".into(),
                worktree_path: "/nonexistent/repo/wt".into(),
                branch: "feat/x".into(),
            }],
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

        let mut report = ForceDeleteReport::default();
        teardown_runtime_resources(&session, &[], &mut report);

        assert!(
            report.remote_teardown_error.is_some(),
            "unreachable host recorded"
        );
        assert!(
            report.removed_worktrees.is_empty() && report.worktree_errors.is_empty(),
            "no local git worktree removal attempted for a remote session"
        );
        assert!(!report.killed_window);
    }

    /// A sandboxed session's scratch directory is the agent's own writable
    /// space; it must not outlive the session. The layout is
    /// `<data>/sandbox/tmp/<session id>` (`crate::sandbox::dirs`), spelled out
    /// here because `session_ops` reaches the sandbox layer only through
    /// `agent::sandboxing`.
    #[test]
    fn tearing_down_a_sandboxed_session_drops_the_scratch_it_minted() {
        let scratch_root = crate::paths::log_directory()
            .expect("a test build pins the data directory under a temp dir")
            .join("sandbox")
            .join("tmp");

        let sandboxed = SessionId::default();
        let plain = SessionId::default();
        for id in [sandboxed, plain] {
            let dir = scratch_root.join(id.to_string());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("agent-scratch"), "x").unwrap();
        }

        let session = |id, profile: Option<&str>| SharedSession {
            id,
            name: "remote".into(),
            agent: "dev".into(),
            backend_id: "%3".into(),
            // An unresolvable remote host: teardown records the failure and
            // performs no tmux or git work, leaving the sandbox half isolated.
            backend_type: "wsl:Ubuntu".into(),
            agent_session_id: None,
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: Vec::new(),
            shell_backend_id: None,
            sandbox_profile: profile.map(str::to_string),
            sandbox_enforcement: Default::default(),
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
            mux: crate::session::MuxIdentity::default(),
            egress: crate::session::EgressRecord::default(),
            sandbox_overlay: None,
        };

        let mut report = ForceDeleteReport::default();
        teardown_runtime_resources(&session(sandboxed, Some("dev")), &[], &mut report);
        teardown_runtime_resources(&session(plain, None), &[], &mut report);

        assert!(
            !scratch_root.join(sandboxed.to_string()).exists(),
            "the sandboxed session's scratch directory outlived it"
        );
        // The desired profile is what says there is anything to drop, so a
        // session that never asked for a boundary is not touched.
        assert!(scratch_root.join(plain.to_string()).exists());
        let _ = std::fs::remove_dir_all(scratch_root.join(plain.to_string()));
    }

    /// A child whose session row is gone was never torn down, so it is not
    /// `stopped`.
    ///
    /// `stopped` releases the owner's fan-out slot and tells `status` and
    /// integration the pane is dead. Recording it for a child friring never
    /// looked at — no window killed, no host reached, nothing verified — is the
    /// one outcome that corrupts every later decision, and a row that will not
    /// read is exactly that case.
    #[test]
    fn a_child_whose_session_row_is_missing_is_recorded_stop_failed() {
        let db = Database::open_in_memory().unwrap();
        let owner = insert_session(&db, "owner");
        // Owned and live, but with no session row of its own: the arm that would
        // have torn something down cannot run.
        let child = SessionId::default().to_string();
        db.insert_bridge_child(&child, &owner.to_string(), "create-0001")
            .unwrap();
        db.set_bridge_child_state(&child, crate::session::ChildState::Working)
            .unwrap();

        let report = delete_session_headless(&db, owner, true).unwrap();

        assert!(
            report.stopped_children.is_empty(),
            "a child friring never touched was reported stopped: {:?}",
            report.stopped_children
        );
        assert_eq!(
            db.bridge_child_state(&child).unwrap().map(|row| row.state),
            Some(crate::session::ChildState::StopFailed),
            "an unverified child must keep its slot and be flagged for a person"
        );
    }

    /// A force-delete whose ownership cascade cannot be read is refused, and
    /// leaves the owner exactly as it was.
    ///
    /// `unwrap_or_default()` on that read is an empty cascade, which is
    /// indistinguishable from "this session owns nothing" — so the one session
    /// that could ever stop these children is deleted while they keep running,
    /// holding worktrees nothing can now find. The escape hatch survives: a
    /// child's own row can still be force-deleted by id.
    #[test]
    fn an_owner_whose_children_cannot_be_read_is_not_force_deleted() {
        let db = Database::open_in_memory().unwrap();
        let owner = insert_session(&db, "owner");
        // A read that genuinely fails, rather than a seam: the cascade's own
        // SELECT has no table to run against.
        db.conn_ref()
            .execute("DROP TABLE bridge_children", [])
            .unwrap();

        let err = delete_session_headless(&db, owner, true).unwrap_err();
        assert!(err.contains("bridge children"), "got {err}");
        assert!(
            db.get_session_by_id(owner).unwrap().is_some(),
            "the owner was deleted after the cascade was refused"
        );
        // Untouched, not merely still present: a refusal that had already
        // stamped or soft-deleted the row would be a half-done delete.
        let (deleted, forced): (Option<i64>, i64) = db
            .conn_ref()
            .query_row(
                "SELECT deleted_at, force_deleted FROM sessions WHERE id = ?1",
                rusqlite::params![owner.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(deleted, None, "the owner was soft-deleted anyway");
        assert_eq!(forced, 0, "the owner was stamped force-deleted anyway");
    }

    /// The same rule one level down: a child whose *state* cannot be read is not
    /// evidence that stopping it is unnecessary.
    #[test]
    fn an_owner_whose_child_state_cannot_be_read_is_not_force_deleted() {
        let db = Database::open_in_memory().unwrap();
        let owner = insert_session(&db, "owner");
        let child = SessionId::default().to_string();
        db.insert_bridge_child(&child, &owner.to_string(), "create-0001")
            .unwrap();
        db.conn_ref()
            .execute("DROP TABLE bridge_child_state", [])
            .unwrap();

        let err = delete_session_headless(&db, owner, true).unwrap_err();
        assert!(err.contains(&child), "got {err}");
        assert!(db.get_session_by_id(owner).unwrap().is_some());
    }

    #[test]
    fn missing_session_errors() {
        let db = Database::open_in_memory().unwrap();
        let err = delete_session_headless(&db, SessionId::default(), false).unwrap_err();
        assert!(err.contains("Session not found"), "got {err}");
    }
}
