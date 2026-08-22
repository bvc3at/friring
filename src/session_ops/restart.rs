//! Headless session restart — tears down the tmux window and re-launches
//! the agent CLI, resuming the existing conversation when the agent supports
//! it and a transcript exists, starting fresh otherwise.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::session::{SessionConfig, SessionId};
use crate::storage::Database;
use crate::sync::SharedSession;

/// The resolved inputs for re-spawning a session's tmux window: the agent
/// command + args, the process cwd, and the identity env. Extracted from the
/// side-effecting [`restart_session_headless`] so the resolution logic (env
/// injection, resume trigger, multi-repo workspace cwd) is unit-testable
/// without driving tmux.
#[derive(Debug)]
struct RestartPlan {
    window_name: String,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: HashMap<String, String>,
    /// What this relaunch did with the session's sandbox profile — see
    /// [`crate::session_ops::AgentInvocation::sandbox`]. Reported to the caller
    /// rather than swallowed: a restart is exactly when a boundary that could
    /// not be applied last time comes back, and when one that used to hold
    /// stops holding.
    sandbox: Option<crate::session::SandboxState>,
    /// The egress proxy this plan was composed against, if the profile needs
    /// one — bound already, and belonging to nobody. The agent this restart
    /// replaces is *still running* while the plan is built, and keeps the
    /// instance it was launched with until this one is committed; a restart
    /// that never reaches its spawn drops the plan and releases it.
    egress: crate::agent::sandboxing::PendingEgress,
}

/// The multiplexer calls a headless restart makes.
///
/// Injected so the two failure paths that matter — a kill that fails, leaving
/// the old agent alive, and a spawn that fails, leaving no agent at all — can
/// be exercised without a tmux server. Driving the real ones in a test would
/// either talk to the user's own friring socket or launch an agent.
struct WindowOps<'a> {
    kill: &'a dyn Fn(&RestartPlan) -> Result<(), String>,
    spawn: &'a dyn Fn(&RestartPlan) -> Result<(), String>,
}

/// The real ones, on the local tmux server.
fn tmux_windows() -> WindowOps<'static> {
    WindowOps {
        kill: &kill_agent_window,
        spawn: &spawn_agent_window,
    }
}

fn kill_agent_window(plan: &RestartPlan) -> Result<(), String> {
    crate::agent::tmux::kill_window(&plan.window_name)
        .map_err(|e| format!("Failed to kill tmux window: {e}"))
}

fn spawn_agent_window(plan: &RestartPlan) -> Result<(), String> {
    crate::agent::tmux::spawn_window(
        &plan.window_name,
        &plan.command,
        &plan.args,
        plan.cwd.as_deref(),
        &plan.env,
    )
    .map_err(|e| format!("Failed to re-spawn tmux window: {e}"))
}

/// Build the [`RestartPlan`] for a persisted session: keep its identity stable,
/// inject the standard `FRIRING_*` env, decide the resume trigger from the
/// agent definition, and resolve the process cwd (the symlink workspace for a
/// multi-repo session, else the primary repo — mirroring the TUI's
/// `App::resolve_process_cwd`).
///
/// `sandbox` is the session's profile, re-read from storage by the caller: a
/// restart must rebuild the same boundary the spawn built, and a profile that
/// has since been edited takes effect here.
fn build_restart_plan(
    session: &SharedSession,
    sandbox: Option<crate::session::SandboxProfile>,
) -> Result<RestartPlan, String> {
    let agent_session_id = session.agent_session_id.clone().ok_or_else(|| {
        format!(
            "Cannot restart session {} without agent_session_id",
            session.id
        )
    })?;

    let mut config = SessionConfig {
        // Keep the same identity across a restart so `FRIRING_SESSION` is stable.
        session_id: Some(session.id),
        agent_session_id: Some(agent_session_id.clone()),
        cwd: session.cwd.clone(),
        agent: session.agent.clone(),
        // Only reaches the args when the restart falls back to a *fresh*
        // conversation (no transcript → new_session_args); a resume never
        // renames — the resume template carries no {name}.
        session_name: Some(session.name.clone()),
        sandbox,
        ..SessionConfig::default()
    };
    super::inject_friring_env(&mut config, &agent_session_id, None);
    let def = super::resolve_agent_def(Some(&config.agent));
    config.resume_session_id = super::resume_trigger_for(&def, &agent_session_id, &config.env);

    // A multi-repo session (≥2 members) launches in its per-session symlink
    // workspace, gathering every member dir; a single-repo session keeps the
    // primary repo. Only resolve when there is a primary cwd to anchor on.
    if let Some(primary) = session.cwd.clone() {
        // The headless restart path is local-only (`spawn_window`; remote
        // sessions are refused up front in `restart_session_headless`), so
        // the workspace is always built locally.
        config.cwd = Some(super::spawn::resolve_launch_cwd(
            &agent_session_id,
            &primary,
            &session.worktrees,
            &session.additional_dirs,
            None,
            session.workspace_dir.as_deref(),
        ));
    }

    let invocation = super::build_agent_invocation(&def, &mut config)?;
    // Composing a filtered profile binds a fresh proxy; claiming it here is
    // what makes it this *plan's*, so it lives exactly as long as the plan does
    // and the running agent's own boundary is left where it is. Claimed before
    // the refusal below, so refusing releases it too.
    let egress = crate::agent::sandboxing::pending_egress(&config);
    // This path re-spawns through the local one-shot `tmux -e KEY=VALUE`, whose
    // argv is readable by other local users — so a launch carrying a credential
    // is refused rather than either exposed there or silently started without
    // it (`docs/SANDBOX.md` §Failure modes). Off-host sessions, which is where
    // an injected credential actually arises, never reach here: they are refused
    // by `restart_session_with` before the plan is built.
    if !invocation.secret_env.is_empty() {
        return Err(format!(
            "Session '{}' has a sandbox profile that injects a credential, which friring will \
             not pass on a tmux client's command line; restart it from the TUI instead",
            session.name
        ));
    }

    Ok(RestartPlan {
        window_name: session.name.clone(),
        command: invocation.command,
        args: invocation.args,
        cwd: config.cwd,
        env: config.env,
        sandbox: invocation.sandbox,
        egress,
    })
}

/// Restart an existing session in-place — kills its tmux window and
/// re-spawns the agent CLI.
///
/// For the `claude` agent, uses its resume group when a transcript for the
/// session id exists on disk, otherwise pins the same id for a fresh start.
/// For `resume_latest` agents (codex, opencode, antigravity, aider, copilot) it
/// resumes the latest session in the (unchanged) launch directory. Other agents
/// degrade to "start fresh" (the live tmux process is what carries state across
/// restarts).
///
/// Returns what the relaunch did with the session's sandbox profile, so the
/// caller can say so: `None` for a session with no profile, and an
/// [`Unenforced`](crate::session::SandboxState::Unenforced) state when the
/// profile could not be applied and its escape hatch let the agent start on the
/// host anyway. The session's stored profile is untouched either way — the next
/// restart tries the boundary again.
pub fn restart_session_headless(
    db: &Database,
    session_id: SessionId,
) -> Result<Option<crate::session::SandboxState>, String> {
    restart_session_with(db, session_id, &tmux_windows())
}

/// [`restart_session_headless`] against `windows`. See [`WindowOps`].
fn restart_session_with(
    db: &Database,
    session_id: SessionId,
    windows: &WindowOps<'_>,
) -> Result<Option<crate::session::SandboxState>, String> {
    let session = db
        .get_session_by_id(session_id)
        .map_err(|e| format!("Failed to load session: {e}"))?
        .ok_or_else(|| format!("Session not found: {session_id}"))?;

    // The kill/spawn below drive the *local* tmux only. Silently "restarting" an
    // off-host session would leave its real window running where it is and spawn
    // a stray local one, so refuse. A **sandbox place** is off-host in exactly
    // the way that matters here: its tmux is inside the container, so the local
    // kill would find nothing and the local spawn would put an unsandboxed agent
    // on the host under a profile that says otherwise.
    if crate::session::is_offhost_backend(&session.backend_type) {
        return Err(format!(
            "Session '{}' runs on backend '{}'; headless restart is \
             local-only — restart it from the TUI instead",
            session.name, session.backend_type
        ));
    }

    let sandbox = super::load_sandbox_profile(db, session.sandbox_profile.as_deref())?;
    let plan = build_restart_plan(&session, sandbox)?;

    // Neither of these may commit the plan's boundary. A kill that fails leaves
    // the old agent running on the instance it was launched with, and a spawn
    // that fails leaves nothing to hand a new one to; in both cases dropping
    // `plan` releases what was prepared and changes nothing else.
    (windows.kill)(&plan)?;
    (windows.spawn)(&plan)?;
    // The window exists, so the boundary composed for it is now the session's,
    // and the one the retired agent was using is shut down.
    plan.egress.commit();

    // The agent was re-spawned fresh; clear any stale hook-driven status so it
    // doesn't show a leftover Blocked/Working/Done until the agent re-reports
    // (a resumed agent may not re-fire its boot hook). Best-effort.
    let _ = db.clear_hook_state(session_id);

    Ok(plan.sandbox)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(agent_session_id: Option<&str>, cwd: Option<PathBuf>) -> SharedSession {
        SharedSession {
            id: SessionId::default(),
            name: "demo".into(),
            agent: "claude".into(),
            backend_id: String::new(),
            backend_type: "local-tmux".into(),
            agent_session_id: agent_session_id.map(String::from),
            cwd,
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
        }
    }

    /// A place-backed session's tmux is inside the container, so the local
    /// kill/spawn pair would find nothing to kill and would put an
    /// **unsandboxed** agent on the host under a profile that says otherwise.
    #[test]
    fn a_headless_restart_refuses_an_offhost_session_of_either_shape() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let db = Database::open_in_memory().unwrap();
        for backend_type in ["sandbox:dev", "ssh:devbox"] {
            let mut sess = session(Some("agent-conv-uuid"), Some(PathBuf::from("/tmp/repo")));
            sess.backend_type = backend_type.to_string();
            db.upsert_session(&sess).unwrap();
            let err = restart_session_with(&db, sess.id, &tmux_windows()).unwrap_err();
            assert!(err.contains(backend_type), "{err}");
            assert!(err.contains("local-only"), "{err}");
        }
    }

    #[test]
    fn restart_plan_requires_agent_session_id() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let err = build_restart_plan(&session(None, None), None).unwrap_err();
        assert!(err.contains("agent_session_id"), "got: {err}");
    }

    #[test]
    fn restart_plan_injects_identity_env() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let sess = session(Some("agent-conv-uuid"), Some(PathBuf::from("/tmp/repo")));
        let plan = build_restart_plan(&sess, None).unwrap();

        // The friring session key and the agent conversation id are both present
        // and distinct, exactly as a fresh spawn would inject them.
        assert_eq!(plan.env.get("FRIRING_SESSION"), Some(&sess.id.to_string()));
        assert_eq!(
            plan.env.get("FRIRING_SESSION_ID"),
            Some(&"agent-conv-uuid".to_string())
        );
    }

    #[test]
    fn restart_plan_fresh_fallback_passes_session_name_to_new_session_args() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        // No transcript on disk for this id, so claude's restart falls back to a
        // fresh conversation: new_session_args run and the friring session name
        // ("demo") reaches argv via the seeded `-n {name}` pair.
        let plan = build_restart_plan(&session(Some("agent-conv-uuid"), Some(cwd)), None).unwrap();
        assert_eq!(
            plan.args,
            vec!["--session-id", "agent-conv-uuid", "-n", "demo"]
        );
    }

    #[test]
    fn restart_plan_single_repo_launches_in_primary() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let primary = temp.path().join("primary");
        std::fs::create_dir_all(&primary).unwrap();
        let plan = build_restart_plan(&session(Some("sid"), Some(primary.clone())), None).unwrap();
        assert_eq!(plan.cwd, Some(primary));
    }

    #[test]
    fn restart_plan_multi_repo_launches_in_workspace() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let primary = temp.path().join("primary");
        std::fs::create_dir_all(&primary).unwrap();
        let extra = temp.path().join("extra");
        std::fs::create_dir_all(&extra).unwrap();

        let mut sess = session(Some("sid-multi"), Some(primary.clone()));
        sess.additional_dirs = vec![extra];

        let plan = build_restart_plan(&sess, None).unwrap();
        // ≥2 members → the symlink workspace, not the primary repo itself.
        assert_ne!(plan.cwd.as_deref(), Some(primary.as_path()));
        assert!(plan.cwd.is_some());
    }

    #[test]
    fn restart_plan_honors_custom_workspace_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let primary = temp.path().join("primary");
        std::fs::create_dir_all(&primary).unwrap();
        let extra = temp.path().join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        let custom = temp.path().join("named-ws");

        let mut sess = session(Some("sid-ws"), Some(primary));
        sess.additional_dirs = vec![extra];
        sess.workspace_dir = Some(custom.clone());

        let plan = build_restart_plan(&sess, None).unwrap();
        // The relaunch happens in the user-chosen dir, not workspaces/<id>.
        assert_eq!(plan.cwd, Some(custom));
    }

    /// A restart re-derives its boundary from the database, so a profile that
    /// was deleted while the session ran has to stop the relaunch. Clearing the
    /// reference instead would put the agent on the host with no boundary and
    /// no message — the one failure this feature must never have.
    #[test]
    fn a_restart_refuses_when_the_sessions_profile_was_deleted() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let db = Database::open_in_memory().unwrap();

        let mut sess = session(Some("sid-sandbox"), Some(temp.path().join("repo")));
        sess.sandbox_profile = Some("deleted".into());
        db.upsert_session(&sess).unwrap();

        let err = restart_session_headless(&db, sess.id).unwrap_err();
        assert!(err.contains("deleted"), "got: {err}");
        assert!(err.contains("no longer exists"), "got: {err}");
    }

    /// A restart whose boundary cannot be applied, on a profile that allows the
    /// escape hatch: the agent starts on the host, the caller is told why, and
    /// — the part that used to be wrong — the session's stored profile is left
    /// exactly where it was, so the next restart tries the boundary again.
    #[test]
    fn a_headless_restart_that_falls_back_reports_it_and_keeps_the_profile() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let db = Database::open_in_memory().unwrap();

        // `wsl-distro` is a place backend: unavailable in this build on every
        // host, so the decision is the same wherever the suite runs.
        let mut profile = crate::session::SandboxProfile::new(
            "dev",
            vec![crate::session::SandboxPath::workspace("~/dev/app")],
        );
        profile.backend = crate::session::SandboxBackendKind::WslDistro;
        profile.allow_unsandboxed_fallback = true;
        db.upsert_sandbox_profile(&profile).unwrap();

        let mut sess = session(Some("sid-fallback"), Some(temp.path().join("repo")));
        sess.sandbox_profile = Some("dev".into());
        db.upsert_session(&sess).unwrap();

        let loaded = super::super::load_sandbox_profile(&db, Some("dev")).unwrap();
        let plan = build_restart_plan(&sess, loaded).unwrap();

        let Some(crate::session::SandboxState::Unenforced(reason)) = plan.sandbox.as_ref() else {
            panic!("expected the escape hatch to fire, got {:?}", plan.sandbox);
        };
        assert!(reason.contains("wsl-distro"), "{reason}");
        // Nothing wrapped the agent — this really is a host process.
        assert_eq!(plan.command, super::super::resolve_agent_def(None).command);
        // And the link storage holds is untouched, which is what makes the
        // next restart rebuild the boundary instead of staying on the host.
        assert_eq!(
            db.get_session_by_id(sess.id)
                .unwrap()
                .unwrap()
                .sandbox_profile
                .as_deref(),
            Some("dev")
        );
    }

    /// Without the escape hatch the same restart refuses outright rather than
    /// relaunching the agent outside its boundary.
    #[test]
    fn a_headless_restart_refuses_when_the_boundary_cannot_be_applied() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());

        let mut profile = crate::session::SandboxProfile::new(
            "dev",
            vec![crate::session::SandboxPath::workspace("~/dev/app")],
        );
        profile.backend = crate::session::SandboxBackendKind::WslDistro;

        let sess = session(Some("sid-strict"), Some(temp.path().join("repo")));
        let err = build_restart_plan(&sess, Some(profile)).unwrap_err();
        assert!(err.contains("wsl-distro"), "got: {err}");
    }

    /// A filtered session, stored, with the boundary composable wherever the
    /// suite runs — the shape the egress tests below need.
    fn filtered_session(db: &Database, allow: &str) -> SharedSession {
        let mut profile = crate::session::SandboxProfile::new(
            "dev",
            vec![crate::session::SandboxPath::workspace(
                "/fabricated/dev/app",
            )],
        );
        profile.network_allow = vec![allow.to_string()];
        db.upsert_sandbox_profile(&profile).unwrap();

        let mut sess = session(Some("sid-egress"), Some(PathBuf::from("/fabricated/repo")));
        sess.sandbox_profile = Some("dev".into());
        db.upsert_session(&sess).unwrap();
        sess
    }

    /// The loopback port a composed relaunch would dial, out of the
    /// environment it hands the window.
    fn composed_proxy_port(env: &HashMap<String, String>) -> u16 {
        let url = env
            .get("HTTP_PROXY")
            .unwrap_or_else(|| panic!("no HTTP_PROXY in the relaunch environment: {env:?}"));
        url.rsplit(':')
            .next()
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("no port in {url}"))
    }

    fn listening(port: u16) -> bool {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    }

    /// A released instance is shut down on the supervisor's own thread, so the
    /// assertion is that it happens, not that it has already happened.
    fn closes(port: u16) -> bool {
        (0..200).any(|_| {
            if !listening(port) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            false
        })
    }

    /// Composing happens before the kill on purpose — a healthy session must
    /// survive a composition failure — so a kill that fails leaves the old
    /// agent running. Its boundary is the one that was there before, and the
    /// one this relaunch bound goes away with the relaunch.
    #[test]
    fn a_headless_restart_whose_kill_fails_leaves_the_running_boundary_alone() {
        let temp = tempfile::TempDir::new().unwrap();
        let _paths = crate::paths::TestPathGuard::new(temp.path());
        let _host = crate::agent::sandboxing::TestSandboxHost::seatbelt();
        let db = Database::open_in_memory().unwrap();
        let sess = filtered_session(&db, "api.anthropic.com");

        // The instance the running agent was launched with: the same
        // composition a previous restart made, committed as that one would
        // have committed it.
        let previous = build_restart_plan(
            &sess,
            super::super::load_sandbox_profile(&db, Some("dev")).unwrap(),
        )
        .expect("the boundary composes");
        let running_port = composed_proxy_port(&previous.env);
        previous.egress.commit();
        assert!(listening(running_port));

        let relaunch_env = std::sync::Mutex::new(HashMap::new());
        let err = restart_session_with(
            &db,
            sess.id,
            &WindowOps {
                kill: &|plan| {
                    relaunch_env.lock().unwrap().clone_from(&plan.env);
                    Err("Failed to kill tmux window: no such window".to_string())
                },
                spawn: &|_| panic!("a restart must not spawn after a failed kill"),
            },
        )
        .expect_err("the kill fails");
        assert!(err.contains("kill"), "{err}");

        let relaunch_port = composed_proxy_port(&relaunch_env.lock().unwrap());
        assert_ne!(
            running_port, relaunch_port,
            "a second instance, not the same"
        );
        assert!(
            closes(relaunch_port),
            "the relaunch that never happened left its listener behind"
        );
        assert!(
            listening(running_port),
            "the still-running agent lost the way out it was launched with"
        );
    }

    /// The same for a spawn that fails: the session keeps the instance it had
    /// — the next restart replaces it — rather than being left pointing at a
    /// boundary composed for a pane that does not exist.
    #[test]
    fn a_headless_restart_whose_spawn_fails_keeps_the_boundary_it_had() {
        let temp = tempfile::TempDir::new().unwrap();
        let _paths = crate::paths::TestPathGuard::new(temp.path());
        let _host = crate::agent::sandboxing::TestSandboxHost::seatbelt();
        let db = Database::open_in_memory().unwrap();
        let sess = filtered_session(&db, "api.anthropic.com");

        let previous = build_restart_plan(
            &sess,
            super::super::load_sandbox_profile(&db, Some("dev")).unwrap(),
        )
        .expect("the boundary composes");
        let running_port = composed_proxy_port(&previous.env);
        previous.egress.commit();

        let relaunch_env = std::sync::Mutex::new(HashMap::new());
        restart_session_with(
            &db,
            sess.id,
            &WindowOps {
                kill: &|_| Ok(()),
                spawn: &|plan| {
                    relaunch_env.lock().unwrap().clone_from(&plan.env);
                    Err("Failed to re-spawn tmux window: no server".to_string())
                },
            },
        )
        .expect_err("the spawn fails");

        assert!(
            closes(composed_proxy_port(&relaunch_env.lock().unwrap())),
            "the relaunch that never happened left its listener behind"
        );
        assert!(
            listening(running_port),
            "the session's boundary was retired for one nothing uses"
        );
    }

    /// And the success path, which is what keeps the two above from being
    /// satisfied by never committing at all: a relaunch that reaches its
    /// window takes over, and the instance it replaced is shut down.
    #[test]
    fn a_headless_restart_that_succeeds_replaces_the_boundary() {
        let temp = tempfile::TempDir::new().unwrap();
        let _paths = crate::paths::TestPathGuard::new(temp.path());
        let _host = crate::agent::sandboxing::TestSandboxHost::seatbelt();
        let db = Database::open_in_memory().unwrap();
        let sess = filtered_session(&db, "api.anthropic.com");

        let previous = build_restart_plan(
            &sess,
            super::super::load_sandbox_profile(&db, Some("dev")).unwrap(),
        )
        .expect("the boundary composes");
        let running_port = composed_proxy_port(&previous.env);
        previous.egress.commit();

        let relaunch_env = std::sync::Mutex::new(HashMap::new());
        restart_session_with(
            &db,
            sess.id,
            &WindowOps {
                kill: &|_| Ok(()),
                spawn: &|plan| {
                    relaunch_env.lock().unwrap().clone_from(&plan.env);
                    Ok(())
                },
            },
        )
        .expect("the relaunch spawns");

        let relaunch_port = composed_proxy_port(&relaunch_env.lock().unwrap());
        assert!(listening(relaunch_port), "the relaunch has no way out");
        assert!(
            closes(running_port),
            "the instance the retired agent was using kept running"
        );
    }

    /// The other half of the same wiring: a session's profile survives the
    /// round trip through storage, so the restart path has something to
    /// rebuild from.
    #[test]
    fn a_sessions_profile_is_persisted_and_reloaded_for_the_relaunch() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let db = Database::open_in_memory().unwrap();

        let profile = crate::session::SandboxProfile::new(
            "dev",
            vec![crate::session::SandboxPath::workspace("~/dev/app")],
        );
        db.upsert_sandbox_profile(&profile).unwrap();

        let mut sess = session(Some("sid-live"), Some(temp.path().join("repo")));
        sess.sandbox_profile = Some("dev".into());
        db.upsert_session(&sess).unwrap();

        let stored = db.get_session_by_id(sess.id).unwrap().unwrap();
        assert_eq!(stored.sandbox_profile.as_deref(), Some("dev"));
        let loaded = super::super::load_sandbox_profile(&db, stored.sandbox_profile.as_deref())
            .unwrap()
            .expect("the profile the session names is still stored");
        assert_eq!(loaded.name, "dev");
        assert_eq!(loaded.paths.len(), 1);
    }
}
