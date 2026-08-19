//! Headless session spawn — creates a local-tmux session without requiring
//! the TUI event loop.

use std::path::PathBuf;

use crate::session::{ExtraRepo, HostDef, SessionConfig, SessionId};
use crate::storage::Database;
use crate::sync::{SharedSession, SharedWorktree};

/// Default base branch for `--worktree-branch` when none is given.
const DEFAULT_BASE_BRANCH: &str = "main";

/// Backend identifier for the local-tmux backend (matches `LocalTmuxBackend`).
const LOCAL_TMUX_BACKEND_TYPE: &str = "local-tmux";

/// Request to create a new headless session.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Session name (used for the tmux window `tb-<name>`).
    pub name: String,
    /// Directory the agent process should `cd` into.
    pub repo_path: PathBuf,
    /// Optional branch name — when set, a git worktree is created at
    /// `repo_path/worktrees/<name>` and used as the cwd instead of
    /// `repo_path` itself.
    pub worktree_branch: Option<String>,
    /// Base branch to create the worktree from (default `main`).
    pub base_branch: Option<String>,
    /// Optional agent name — falls back to the registry default agent.
    pub agent: Option<String>,
    /// Optional pre-generated agent session UUID. When unset one is generated
    /// so callers can return it to the user immediately.
    pub agent_session_id: Option<String>,
    /// Optional remote host name (from `hosts.toml`). When set, the session is
    /// created on that host over SSH (worktree + tmux window live remotely).
    pub host: Option<String>,
    /// Optional parent session (lead/worker relationship for orchestration).
    /// Must reference an existing active session.
    pub parent_session_id: Option<SessionId>,
    /// Optional originating task id. When set it is injected as `FRIRING_TASK`
    /// so the session's outgoing messages auto-tag `from_task_id` without the
    /// agent passing any id by hand.
    pub task_id: Option<i64>,
    /// Additional repositories this session spans (empty = single-repo, the
    /// unchanged common case). Each either gets its own isolated worktree on
    /// the shared `worktree_branch` (off its own `base_branch`) or is attached
    /// as-is as an additional directory. When any extra is non-empty the agent
    /// launches in a per-session symlink workspace gathering every member.
    pub extra_repos: Vec<ExtraRepo>,
    /// Name of the sandbox profile to run the agent under (`docs/SANDBOX.md`).
    /// `None` = unsandboxed, the default. An unknown name fails the spawn
    /// rather than quietly running on the host.
    pub sandbox_profile: Option<String>,
}

/// Result returned on successful headless spawn.
#[derive(Debug, Clone)]
pub struct SpawnResult {
    pub session_id: SessionId,
    pub name: String,
    pub agent: String,
    pub agent_session_id: String,
    pub cwd: PathBuf,
    pub worktrees: Vec<SharedWorktree>,
    pub parent_session_id: Option<SessionId>,
    /// What the launch did with [`SpawnRequest::sandbox_profile`]. `None` = the
    /// session asked for no boundary. A
    /// [`SandboxState::Unenforced`](crate::session::SandboxState::Unenforced)
    /// here means the agent is running **on the host**: the caller reports it,
    /// because a session the user believes is sandboxed and is not is the worst
    /// outcome this feature has.
    pub sandbox: Option<crate::session::SandboxState>,
    /// What the user has to type in the session's pane to sign the agent in,
    /// when the boundary started it signed out. `None` when there is nothing to
    /// do — every policy-backed launch, and every place that already holds a
    /// credential. Reported for the reason the fallback is: an agent parked at
    /// a sign-in prompt with nothing said about it reads as a broken session.
    pub sandbox_login: Option<String>,
}

/// How a headless spawn creates the session's window, and what it learns: the
/// remote pane id, or an empty string for a local spawn (whose pane the TUI
/// resolves by name).
///
/// Injected so the failure path — the one that must leave behind no egress
/// proxy, no scratch directory and no generated policy file — is exercised
/// without a tmux server. Driving the real one in a test would either talk to
/// the user's own friring socket or launch an agent.
type WindowSpawner<'a> = &'a dyn Fn(
    LaunchTarget<'_>,
    &str,
    &str,
    &[String],
    &std::path::Path,
    &std::collections::HashMap<String, String>,
) -> Result<String, String>;

/// Where a headless launch puts its window: this machine's tmux, a host's over
/// SSH/WSL, or the tmux **inside a sandbox place**. The three multiplexers are
/// reached differently and nothing else about the launch changes, which is
/// exactly the transport seam ADR-26 leans on.
#[derive(Clone, Copy)]
pub(crate) enum LaunchTarget<'a> {
    Local,
    Host(&'a HostDef),
    Place(&'a crate::agent::transport::Place),
}

/// The real one: a window on the local tmux server, on the host's over SSH, or
/// on the one running inside a place.
fn spawn_launch_window(
    target: LaunchTarget<'_>,
    name: &str,
    command: &str,
    args: &[String],
    cwd: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
) -> Result<String, String> {
    // Off-host spawns drive control mode to learn the real pane id; local spawns
    // leave `backend_id` empty for the TUI to resolve by name.
    match target {
        LaunchTarget::Host(h) => {
            crate::agent::tmux::spawn_window_remote(h, name, command, args, Some(cwd), env)
                .map_err(|e| format!("Failed to spawn remote tmux window: {e:#}"))
        }
        LaunchTarget::Place(place) => {
            crate::agent::tmux::spawn_window_place(place, name, command, args, Some(cwd), env)
                .map_err(|e| format!("Failed to spawn tmux window in the sandbox place: {e:#}"))
        }
        LaunchTarget::Local => {
            crate::agent::tmux::spawn_window(name, command, args, Some(cwd), env)
                .map(|()| String::new())
                .map_err(|e| format!("Failed to spawn tmux window: {e}"))
        }
    }
}

/// Refuse a launch that would put a credential on a command line.
///
/// A window's environment reaches an off-host target — an SSH host, a sandbox
/// place — inside a control-mode command sent over the tmux socket, which never
/// touches a process table. The **local** one-shot spawn has no control
/// connection and passes each variable as a `tmux -e KEY=VALUE` argument
/// instead, readable by any local user through `/proc/<pid>/cmdline`
/// (`docs/SANDBOX.md` §Failure modes). Refusing is the point: the alternative is
/// exposing the token in order to satisfy the launch, and the quieter
/// alternative — dropping it — starts an agent that will fail to authenticate
/// with nothing on screen saying why.
///
/// Unreachable today by construction (only a place injects a credential, and a
/// place is never the local target), which is exactly why it is a check rather
/// than a comment: the next credential strategy must not be able to make it
/// reachable silently.
fn credential_channel(
    target: LaunchTarget<'_>,
    secret_env: &[(String, String)],
) -> Result<(), String> {
    if secret_env.is_empty() || !matches!(target, LaunchTarget::Local) {
        return Ok(());
    }
    Err(
        "This sandbox profile injects a credential, which friring will not pass on a tmux \
         client's command line where another local user could read it; start the session from \
         the TUI instead"
            .to_string(),
    )
}

/// Spawn a new session inside `tmux -L friring`, persisting its state to the
/// shared SQLite database.
pub fn spawn_session_headless(db: &Database, req: SpawnRequest) -> Result<SpawnResult, String> {
    spawn_session_with(db, req, &spawn_launch_window)
}

/// [`spawn_session_headless`] against `spawn_window`. See [`WindowSpawner`].
fn spawn_session_with(
    db: &Database,
    req: SpawnRequest,
    spawn_window: WindowSpawner<'_>,
) -> Result<SpawnResult, String> {
    crate::paths::validate_safe_name(&req.name)?;
    validate_parent_session(db, req.parent_session_id)?;

    // Resolve the agent definition once; `agent_name` is derived from it so the
    // persisted name always matches the def that's actually launched.
    let mut agent_def = super::resolve_agent_def(req.agent.as_deref());
    let agent_name = agent_def.name.clone();

    // Resolve the optional remote host. `backend_type` is `local-tmux` or
    // `ssh:<host>`; `host` is the matching HostDef for remote git/tmux ops.
    // Resolved *before* the window-name check because that check is per-server.
    let (backend_type, host) = resolve_host(req.host.as_deref())?;
    reject_window_name_conflict(db, &req.name, &backend_type)?;

    // The def's `args` may reference friring-managed config files by their
    // *local* absolute path (e.g. claude's hooks `--settings <config>/hooks/
    // claude.json`), which the agent errors on when the path doesn't exist on
    // the host ("Settings file not found" → the pane dies instantly). Rewrite
    // them for the host: materialize the file remotely (translating a
    // home-anchored path to the remote home) or, when that's impossible, strip
    // the flag so the agent at least launches.
    if let Some(h) = host.as_ref() {
        agent_def.args = adapt_agent_args_for_remote(h, agent_def.args);
    }
    let (primary_cwd, worktrees, additional_dirs) = resolve_dirs(&req, host.as_ref())?;

    let agent_session_id = req
        .agent_session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Local claude: point the hooks `--settings` at a per-session symlink so a
    // backgrounded (daemon) workflow is attributed to this exact session in the
    // activity view (mirrors the TUI's `launch_provider_for`). Remote spawns
    // already had their `--settings` remote-adapted above and aren't scanned.
    if host.is_none() {
        agent_def.args =
            super::builtin_hooks::rewrite_settings_for_session(&agent_session_id, agent_def.args);
    }

    // For a multi-repo session, launch the agent in a per-session symlink
    // workspace gathering every member dir (so each repo is a visible subdir,
    // agent-neutral). `info.cwd` keeps the *primary* repo. Single-repo sessions
    // launch directly in the primary cwd, unchanged. Mirrors the TUI's
    // `App::resolve_process_cwd`.
    let launch_cwd = resolve_launch_cwd(
        &agent_session_id,
        &primary_cwd,
        &worktrees,
        &additional_dirs,
        host.as_ref(),
        None,
    );

    // Mint the friring SessionId up front so it can be injected into the
    // process env (`FRIRING_SESSION`) before the agent launches.
    let session_id = SessionId::default();

    let mut config = SessionConfig {
        session_id: Some(session_id),
        agent_session_id: Some(agent_session_id.clone()),
        cwd: Some(launch_cwd.clone()),
        agent: agent_name.clone(),
        backend: (backend_type != LOCAL_TMUX_BACKEND_TYPE).then(|| backend_type.clone()),
        session_name: Some(req.name.clone()),
        sandbox: super::load_sandbox_profile(db, req.sandbox_profile.as_deref())?,
        ..SessionConfig::default()
    };
    super::inject_friring_env(&mut config, &agent_session_id, req.task_id);

    let invocation = super::build_agent_invocation(&agent_def, &mut config)?;
    let (command, args) = (invocation.command, invocation.args);
    // A place is where this session lives from now on, so it is what
    // `backend_type` records — mirroring `ssh:<host>`, and for the same reason:
    // restore and reattach re-derive the transport from it.
    let backend_type = match &invocation.place {
        Some(place) => place.backend_name(),
        None => backend_type,
    };
    let target = match (&invocation.place, host.as_ref()) {
        (Some(place), _) => LaunchTarget::Place(place),
        (None, Some(h)) => LaunchTarget::Host(h),
        (None, None) => LaunchTarget::Local,
    };
    // A filtered profile bound an egress proxy to compose that invocation
    // against, and it is nobody's until this session exists. Held from here to
    // the upsert so every failure in between releases it, rather than leaving a
    // live credential for a session that never happened — which is why it is
    // claimed *before* the refusal below rather than after it.
    let egress = crate::agent::sandboxing::pending_egress(&config);
    credential_channel(target, &invocation.secret_env)?;
    config.env.extend(invocation.secret_env.iter().cloned());

    let backend_id =
        match spawn_window(target, &req.name, &command, &args, &launch_cwd, &config.env) {
            Ok(backend_id) => backend_id,
            Err(e) => {
                // Nothing will adopt what this launch minted. The proxy goes with
                // `egress`; the scratch directory the agent would have written and
                // the policy file generated for it have to be said out loud.
                crate::agent::sandboxing::cleanup_by_session_id(session_id);
                return Err(e);
            }
        };

    let shared = SharedSession {
        id: session_id,
        name: req.name.clone(),
        agent: agent_name.clone(),
        backend_id: backend_id.clone(),
        backend_type,
        agent_session_id: Some(agent_session_id.clone()),
        // `cwd` is the *primary* repo (for display / git context); the workspace
        // is a spawn-time launch detail, re-derived idempotently on every launch.
        cwd: Some(primary_cwd.clone()),
        additional_dirs: additional_dirs.clone(),
        workspace_dir: None,
        worktrees: worktrees.clone(),
        shell_backend_id: None,
        // The profile the session **asked for**, recorded even when the launch
        // fell back to the host: clearing it would strand the session outside
        // its boundary for good, where keeping it makes the next relaunch
        // sandboxed again as soon as the backend is available.
        sandbox_profile: config.sandbox.as_ref().map(|p| p.name.clone()),
        // …and what this launch managed to apply. A headless spawn writes the
        // row once and never comes back to it, so a fallback that went
        // unrecorded here would render as a boundary that holds.
        sandbox_enforcement: crate::session::SandboxEnforcement::from_launch(
            invocation.sandbox.as_ref(),
        ),
        parent_session_id: req.parent_session_id,
        display_order: None,
        tombstone: false,
        tombstone_at: None,
    };
    // The tmux window is already live. If the DB upsert fails now, no row exists
    // for the TUI to adopt and the window would be orphaned — untrackable and
    // unkillable from the UI. Best-effort tear it down before surfacing the
    // error so we don't leak a window.
    if let Err(e) = db.upsert_session(&shared) {
        tracing::error!(
            "spawn race: DB upsert failed after the tmux window for '{}' spawned; \
             tearing down the orphaned window: {e}",
            req.name
        );
        let cleanup = match target {
            LaunchTarget::Host(h) => crate::agent::tmux::kill_pane_remote(h, &backend_id),
            LaunchTarget::Place(place) => crate::agent::tmux::kill_pane_place(place, &backend_id),
            LaunchTarget::Local => crate::agent::tmux::kill_window(&req.name),
        };
        if let Err(kill_err) = cleanup {
            tracing::error!(
                "failed to tear down orphaned window for '{}': {kill_err}",
                req.name
            );
        }
        // The window is gone with the row that would have tracked it, so the
        // boundary minted for it goes too: `egress` releases the proxy as this
        // returns, and the id-keyed cleanup takes the rest.
        crate::agent::sandboxing::cleanup_by_session_id(session_id);
        return Err(format!("Failed to persist session: {e}"));
    }
    // The window is live and the row that owns it is committed: this session
    // exists, so its boundary is the session's now.
    egress.commit();
    if let Some(instance) = &invocation.instance {
        super::record_sandbox_instance(db, instance);
    }

    // Record the worktree's fork point so the code-review view can scope its
    // diff to `<base>..HEAD`. Only meaningful for worktree sessions; a bare-repo
    // session leaves it NULL (review falls back to the repo's default branch).
    if req.worktree_branch.is_some() {
        let base = req.base_branch.as_deref().unwrap_or(DEFAULT_BASE_BRANCH);
        if let Err(e) = db.set_session_base_branch(session_id, base) {
            tracing::warn!("Failed to record session base branch: {e}");
        }
    }

    // No spawn-time status seed: a fresh session is `Idle` (the hooks-driven
    // default) until the agent's hooks report otherwise — e.g. claude's
    // SessionStart → idle on boot, then working/blocked/done through the turn.
    // Seeding `working` here made an idle, just-booted agent look stuck working.

    Ok(SpawnResult {
        session_id,
        name: req.name,
        agent: agent_name,
        agent_session_id,
        cwd: primary_cwd,
        worktrees,
        parent_session_id: req.parent_session_id,
        sandbox: invocation.sandbox,
        sandbox_login: invocation.login,
    })
}

/// Refuse a name that would resolve to the tmux window of an existing session
/// **on the same backend**. Runs before any side effects (worktree creation,
/// tmux spawn).
///
/// The window name is the sanitized session name, and sanitizing is
/// many-to-one (`foo bar` and `foo.bar` both become `tb-foo_bar`), so distinct
/// names can still collide. tmux allows the duplicate and then resolves the
/// window ambiguously: reads land on whichever window came first and
/// `send-keys` fails, leaving two sessions cross-wired to one agent.
///
/// Scoped to `backend_type` because a tmux window namespace is per *server*:
/// the local server and every `ssh:<host>` have independent window names, so a
/// local `tb-foo_bar` is no reason to refuse the same name on a remote host.
fn reject_window_name_conflict(
    db: &Database,
    name: &str,
    backend_type: &str,
) -> Result<(), String> {
    let window = crate::agent::tmux::agent_window_name(name);
    let sessions = db
        .list_active_sessions()
        .map_err(|e| format!("list_active_sessions: {e}"))?;
    match sessions.iter().find(|s| {
        s.backend_type == backend_type && crate::agent::tmux::agent_window_name(&s.name) == window
    }) {
        Some(other) => Err(format!(
            "Session '{}' already uses tmux window {window} on {backend_type}; \
             pick another name",
            other.name
        )),
        None => Ok(()),
    }
}

/// Validate that the requested parent session, if any, exists and is active.
/// Runs before any side effects (worktree creation, tmux spawn).
fn validate_parent_session(db: &Database, parent: Option<SessionId>) -> Result<(), String> {
    let Some(parent) = parent else {
        return Ok(());
    };
    match db.get_session_by_id(parent) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!("Parent session not found: {parent}")),
        Err(e) => Err(format!("get_session_by_id: {e}")),
    }
}

/// Resolve the primary working directory, all worktree records, and the
/// non-worktree additional directories for a (possibly multi-repo) spawn.
///
/// Single-repo (no `extra_repos`): returns the bare repo path when no worktree
/// branch is given, otherwise the primary worktree path plus one
/// [`SharedWorktree`] — byte-identical to the pre-multi-repo behavior.
///
/// Multi-repo: the primary is resolved as above; each [`ExtraRepo`] either gets
/// its own worktree on the **shared** `worktree_branch` (off its own
/// `base_branch`, falling back to the primary's base) appended to `worktrees`,
/// or — when `worktree == false` — is attached as-is in `additional_dirs`. The
/// member set (worktrees + additional dirs) is what the symlink workspace
/// gathers; see [`crate::workspace::ensure_workspace`].
fn resolve_dirs(
    req: &SpawnRequest,
    host: Option<&HostDef>,
) -> Result<(PathBuf, Vec<SharedWorktree>, Vec<PathBuf>), String> {
    let mut worktrees: Vec<SharedWorktree> = Vec::new();
    let mut additional_dirs: Vec<PathBuf> = Vec::new();

    // Primary repo: worktree when a branch is set, otherwise the repo root.
    let primary_cwd = match req.worktree_branch.as_deref() {
        None => req.repo_path.clone(),
        Some(branch) => {
            let base = req.base_branch.as_deref().unwrap_or(DEFAULT_BASE_BRANCH);
            let path = create_worktree(host, &req.repo_path, branch, base)?;
            worktrees.push(SharedWorktree {
                repo_path: req.repo_path.clone(),
                worktree_path: path.clone(),
                branch: branch.to_string(),
            });
            path
        }
    };

    // Extra repos: each its own isolated worktree on the shared branch, or a
    // plain additional directory.
    for extra in &req.extra_repos {
        if extra.worktree {
            let branch = req.worktree_branch.as_deref().ok_or_else(|| {
                "a worktree extra-repo requires --worktree-branch (the shared branch)".to_string()
            })?;
            let base = extra
                .base_branch
                .as_deref()
                .or(req.base_branch.as_deref())
                .unwrap_or(DEFAULT_BASE_BRANCH);
            let path = create_worktree(host, &extra.repo_path, branch, base)?;
            worktrees.push(SharedWorktree {
                repo_path: extra.repo_path.clone(),
                worktree_path: path.clone(),
                branch: branch.to_string(),
            });
        } else {
            additional_dirs.push(extra.repo_path.clone());
        }
    }

    Ok((primary_cwd, worktrees, additional_dirs))
}

/// Create a worktree, wrapping the error with the branch/base for context.
fn create_worktree(
    host: Option<&HostDef>,
    repo: &std::path::Path,
    branch: &str,
    base: &str,
) -> Result<PathBuf, String> {
    crate::git::create_worktree_on(host, repo, branch, base)
        .map_err(|e| format!("Failed to create worktree {branch} off {base} in {repo:?}: {e}"))
}

/// The directory the agent process should launch in: a per-session symlink
/// workspace for a multi-repo session (≥2 members), else the primary cwd.
///
/// Mirrors the TUI's `App::resolve_process_cwd`: members are the worktree repos
/// (labeled by their original repo name) followed by the non-worktree
/// additional dirs; a workspace build failure falls back to the primary cwd.
///
/// Shared with [`super::restart`] so the headless restart launches a multi-repo
/// session in its symlink workspace, not the primary repo.
pub(crate) fn resolve_launch_cwd(
    agent_session_id: &str,
    primary_cwd: &std::path::Path,
    worktrees: &[SharedWorktree],
    additional_dirs: &[PathBuf],
    host: Option<&HostDef>,
    workspace_dir: Option<&std::path::Path>,
) -> PathBuf {
    let mut members: Vec<(String, PathBuf)> = Vec::new();
    if worktrees.is_empty() {
        members.push((dir_label(primary_cwd), primary_cwd.to_path_buf()));
    } else {
        for wt in worktrees {
            members.push((dir_label(&wt.repo_path), wt.worktree_path.clone()));
        }
    }
    for dir in additional_dirs {
        members.push((dir_label(dir), dir.clone()));
    }

    if members.len() < 2 {
        return primary_cwd.to_path_buf();
    }
    build_multi_repo_workspace(host, agent_session_id, &members, workspace_dir)
        .unwrap_or_else(|| primary_cwd.to_path_buf())
}

/// Build the per-session multi-repo symlink workspace — on the *remote* host
/// when one is given (a local symlink dir wouldn't exist there), else with the
/// local builder. A user-chosen `workspace_dir` overrides the default
/// id-derived path on local builds only (the wizard never offers it for a
/// remote spawn; a stale persisted value degrades to the default rather than
/// guessing a remote path). `None` (error already logged) tells the caller to
/// fall back to its primary cwd. Single home for the host branching + fallback
/// policy, shared by [`resolve_launch_cwd`] and the TUI's
/// `App::resolve_process_cwd`.
pub(crate) fn build_multi_repo_workspace(
    host: Option<&HostDef>,
    agent_session_id: &str,
    members: &[(String, PathBuf)],
    workspace_dir: Option<&std::path::Path>,
) -> Option<PathBuf> {
    let built = match host {
        Some(h) => {
            if workspace_dir.is_some() {
                tracing::warn!("custom workspace dir is local-only; using the default remote path");
            }
            crate::git::ensure_remote_workspace(h, agent_session_id, members)
        }
        None => match workspace_dir {
            Some(dir) => {
                crate::workspace::ensure_workspace_at(dir, members).map_err(anyhow::Error::from)
            }
            None => crate::workspace::ensure_workspace(agent_session_id, members)
                .map_err(anyhow::Error::from),
        },
    };
    match built {
        Ok(ws) => Some(ws),
        Err(e) => {
            tracing::error!("Failed to build multi-repo workspace: {e}");
            None
        }
    }
}

/// Adapt agent `args` that reference friring-managed config files (by their
/// *local* absolute path) for a spawn on the remote `host`, returning the args
/// to actually launch with. An agent handed a path that doesn't exist on the
/// host errors out and the pane dies instantly (claude: "Settings file not
/// found"), so an unresolvable path must never reach the remote launch:
///
/// - **Translate + materialize** (POSIX remotes): rewrite a home-anchored
///   config path onto the *remote* home (identity when the homes agree), copy
///   the local file to that remote path, and substitute the rewritten arg.
/// - **Strip as fallback**: on a `psmux` host (native Windows — no POSIX side
///   to copy to and hook commands are `sh` syntax anyway), a non-POSIX local
///   config root, a config path outside the local home that a home-translation
///   can't map, or a failed remote copy/home lookup, drop the path **and its
///   preceding flag** (e.g. the whole `--settings <path>` pair) with a warning
///   so the agent launches clean instead of dead.
///
/// Scope is deliberately narrow: only paths under the **friring config dir**
/// are touched (and only existing local files are copied), so an arbitrary
/// path in the agent's own args — a repo path, a user file — is never
/// rewritten or shipped. Each shipped file also has its friring-managed hook
/// commands rewritten for the host
/// ([`super::builtin_hooks::rewrite_hook_signals_for_remote`]): the local
/// `friring-cli session signal` can't work there, but a tmux pane user option
/// can — the local TUI receives it over its control-mode subscription, so
/// remote sessions get live hooks-driven status.
///
/// Shared by the headless spawn and the TUI (`App::build_spawn_inputs`) so
/// both paths launch a remote session with the same args.
pub(crate) fn adapt_agent_args_for_remote(host: &HostDef, args: Vec<String>) -> Vec<String> {
    let Some(config_root) = crate::paths::config_file()
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
    else {
        return args;
    };
    // Resolve the translation target lazily (one ssh round-trip) and at most
    // once; `None` = strip mode.
    let mut remote_root: Option<Option<String>> = None;
    crate::agent::config_args::rewrite_config_path_args(args, &config_root, |local_path| {
        let root = remote_root
            .get_or_insert_with(|| remote_config_root(host, &config_root))
            .clone()?;
        let remote_path = format!("{root}{}", &local_path[config_root.len()..]);
        // Read failure (missing/unreadable/non-file) → strip, as before.
        let contents = std::fs::read_to_string(local_path).ok()?;
        let contents = super::builtin_hooks::rewrite_hook_signals_for_remote(&contents);
        match crate::git::copy_bytes_to_remote(host, contents.as_bytes(), &remote_path) {
            Ok(()) => Some(remote_path),
            Err(e) => {
                tracing::warn!(
                    "failed to materialize agent config {local_path} on host '{}': {e:#}",
                    host.name
                );
                None
            }
        }
    })
}

/// Where the local friring config root lands on `host`, or `None` when no
/// remote location can hold it (→ strip the args instead):
/// - a non-POSIX (Windows `C:\…`) local root or a `psmux` host can't take a
///   POSIX copy at all;
/// - a home-anchored root translates onto the **remote** home (identity when
///   local and remote `$HOME` agree — the common same-user WSL/devbox case);
/// - a root outside the local home is mirrored at the same absolute path.
fn remote_config_root(host: &HostDef, config_root: &str) -> Option<String> {
    if !config_root.starts_with('/') || host.mux() == "psmux" {
        tracing::warn!(
            "stripping local agent-config args for host '{}': no POSIX path for the \
             friring config dir there",
            host.name
        );
        return None;
    }
    let Some(local_home) = crate::paths::home_dir() else {
        return Some(config_root.to_string());
    };
    let local_home = local_home.to_string_lossy().into_owned();
    // Boundary-checked: home `/home/a` must not claim `/home/abc/…` (the
    // stripped suffix would splice a garbage sibling path onto the remote
    // home). Such a root is mirrored at its absolute path instead.
    let Some(suffix) = config_root
        .strip_prefix(&local_home)
        .filter(|s| s.starts_with('/'))
    else {
        return Some(config_root.to_string());
    };
    match crate::git::remote_home(host) {
        Ok(remote_home) => Some(format!("{remote_home}{suffix}")),
        Err(e) => {
            tracing::warn!(
                "stripping local agent-config args for host '{}': cannot resolve remote \
                 home: {e:#}",
                host.name
            );
            None
        }
    }
}

/// A human-friendly label for a member directory in the symlink workspace:
/// the git repo display name, falling back to the final path component.
fn dir_label(path: &std::path::Path) -> String {
    crate::git::repo_display_name(path)
        .or_else(|| path.file_name().and_then(|s| s.to_str()).map(String::from))
        .unwrap_or_else(|| "repo".to_string())
}

/// Resolve `--host` to `(backend_type, host)`.
///
/// `None`/empty → the local backend. A named host must exist in `hosts.toml`
/// **or** be an auto-discovered local WSL distro, otherwise an error is
/// returned listing the available hosts.
fn resolve_host(host_name: Option<&str>) -> Result<(String, Option<HostDef>), String> {
    let Some(name) = host_name.filter(|n| !n.is_empty()) else {
        return Ok((LOCAL_TMUX_BACKEND_TYPE.to_string(), None));
    };
    let registry = crate::agent::host_config::load_all();
    match registry.get(name) {
        Some(h) => Ok((h.backend_name(), Some(h.clone()))),
        None => {
            let available = registry.names().join(", ");
            Err(format!(
                "Unknown host '{name}'. Configure it in hosts.toml (or check the \
                 WSL distro name). Available: [{available}]"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::DEFAULT_AGENT_NAME;
    use crate::storage::Database;

    fn empty_db() -> Database {
        Database::open_in_memory().expect("open in-memory db")
    }

    fn req(name: &str) -> SpawnRequest {
        SpawnRequest {
            name: name.into(),
            repo_path: PathBuf::from("/tmp"),
            worktree_branch: None,
            base_branch: None,
            agent: None,
            agent_session_id: None,
            host: None,
            parent_session_id: None,
            task_id: None,
            extra_repos: Vec::new(),
            sandbox_profile: None,
        }
    }

    /// A credential goes over a control connection or not at all. The local
    /// one-shot spawner puts its whole environment in a `tmux` client's argv,
    /// so a launch carrying one is refused there and injected everywhere else.
    #[test]
    fn a_credential_never_rides_a_tmux_clients_command_line() {
        let secret = vec![("ANTHROPIC_API_KEY".to_string(), "sk-fabricated".to_string())];
        let host = HostDef {
            name: "devbox".into(),
            destination: "devbox".into(),
            ..HostDef::default()
        };
        let place = crate::agent::transport::Place::new("/usr/bin/podman", "abc123", "dev")
            .expect("a spellable place");

        let err = credential_channel(LaunchTarget::Local, &secret).unwrap_err();
        assert!(err.contains("command line"), "{err}");
        assert!(err.contains("from the TUI"), "{err}");

        assert!(credential_channel(LaunchTarget::Place(&place), &secret).is_ok());
        assert!(credential_channel(LaunchTarget::Host(&host), &secret).is_ok());
        // Nothing to protect, nothing to refuse.
        assert!(credential_channel(LaunchTarget::Local, &[]).is_ok());
    }

    #[test]
    fn empty_name_is_rejected() {
        let db = empty_db();
        let err = spawn_session_headless(&db, req("")).unwrap_err();
        assert!(err.to_lowercase().contains("name"), "got {err}");
    }

    #[test]
    fn unsafe_names_are_rejected() {
        let db = empty_db();
        for bad in [".hidden", "foo/bar", "foo..bar", "foo\\bar"] {
            assert!(
                spawn_session_headless(&db, req(bad)).is_err(),
                "should reject {bad}"
            );
        }
    }

    /// A headless spawn binds its egress proxy while composing — argv has to
    /// name the port — so a window that never spawns must leave nothing at all
    /// behind: no listener, no writable scratch directory, and no generated
    /// policy file, for a session that does not exist.
    #[test]
    fn a_spawn_that_fails_leaves_no_boundary_behind() {
        let temp = tempfile::TempDir::new().unwrap();
        let _paths = crate::paths::TestPathGuard::new(temp.path());
        let _host = crate::agent::sandboxing::TestSandboxHost::seatbelt();
        let db = empty_db();

        let mut profile = crate::session::SandboxProfile::new(
            "dev",
            vec![crate::session::SandboxPath::workspace(
                "/fabricated/dev/app",
            )],
        );
        profile.network_allow = vec!["api.anthropic.com".into()];
        db.upsert_sandbox_profile(&profile).unwrap();

        let mut request = req("boxed");
        // Not the host temp root the default carries: a read-write grant over
        // it reaches friring's own tmux socket, which the launch refuses.
        request.repo_path = PathBuf::from("/fabricated/repo");
        request.sandbox_profile = Some("dev".into());

        let composed = std::sync::Mutex::new(std::collections::HashMap::new());
        let err = spawn_session_with(&db, request, &|_, _, _, _, _, env| {
            composed.lock().unwrap().clone_from(env);
            Err("Failed to spawn tmux window: no server".to_string())
        })
        .expect_err("the window cannot be spawned");
        assert!(err.contains("tmux window"), "{err}");

        // The listener is closed on the egress supervisor's own thread, so the
        // assertion is that it happens, not that it already has.
        let url = composed
            .lock()
            .unwrap()
            .get("HTTP_PROXY")
            .cloned()
            .expect("the launch was composed against a proxy");
        let port: u16 = url
            .rsplit(':')
            .next()
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("no port in {url}"));
        let closed = (0..200).any(|_| {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            false
        });
        assert!(
            closed,
            "the proxy for a session that never existed is still listening"
        );

        let data = crate::paths::log_directory().expect("the fabricated data directory");
        for (what, dir) in [
            ("scratch directory", data.join("sandbox").join("tmp")),
            ("policy file", data.join("sandbox").join("profiles")),
        ] {
            let left: Vec<_> = std::fs::read_dir(&dir)
                .map(|entries| entries.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            assert!(left.is_empty(), "a failed spawn left a {what}: {left:?}");
        }
    }

    #[test]
    fn unknown_parent_session_is_rejected_before_spawn() {
        let db = empty_db();
        let mut r = req("worker");
        r.parent_session_id = Some(SessionId::default());
        let err = spawn_session_headless(&db, r).unwrap_err();
        assert!(err.contains("Parent session not found"), "got {err}");
    }

    #[test]
    fn validate_parent_session_accepts_existing_and_none() {
        let db = empty_db();
        assert!(validate_parent_session(&db, None).is_ok());

        let parent = crate::sync::SharedSession {
            id: SessionId::default(),
            name: "lead".into(),
            agent: "dev".into(),
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
        };
        db.upsert_session(&parent).unwrap();
        assert!(validate_parent_session(&db, Some(parent.id)).is_ok());
    }

    /// `foo bar` and `foo.bar` are distinct session names that sanitize onto
    /// one tmux window, which tmux then resolves ambiguously.
    #[test]
    fn window_name_conflict_is_rejected_before_spawn() {
        let db = empty_db();
        let existing = crate::sync::SharedSession {
            id: SessionId::default(),
            name: "foo bar".into(),
            agent: DEFAULT_AGENT_NAME.into(),
            backend_id: "%1".into(),
            backend_type: "local-tmux".into(),
            agent_session_id: None,
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: Vec::new(),
            shell_backend_id: None,
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
            sandbox_profile: None,
            sandbox_enforcement: crate::session::SandboxEnforcement::default(),
        };
        db.upsert_session(&existing).unwrap();

        let local = LOCAL_TMUX_BACKEND_TYPE;
        let err = reject_window_name_conflict(&db, "foo.bar", local).unwrap_err();
        assert!(err.contains("tb-foo_bar"), "got {err}");
        assert!(reject_window_name_conflict(&db, "foo-bar", local).is_ok());

        // A tmux window namespace is per server, so the same name on a *remote*
        // host is not a conflict — rejecting it would block a legitimate spawn.
        assert!(
            reject_window_name_conflict(&db, "foo.bar", "ssh:builder").is_ok(),
            "a local window must not reserve the name on another host"
        );

        // The guard runs inside the spawn itself, before anything is created:
        // the request fails and no second row (nor tmux window) appears.
        let err = spawn_session_headless(&db, req("foo.bar")).unwrap_err();
        assert!(err.contains("tb-foo_bar"), "got {err}");
        assert_eq!(db.list_active_sessions().unwrap().len(), 1);
    }

    #[test]
    fn adapt_agent_args_is_identity_without_config_paths() {
        // No arg references the friring config dir → args pass through
        // untouched and, because the remote root is resolved lazily, no ssh
        // round-trip is attempted (the host here doesn't exist).
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let host = HostDef {
            name: "nonexistent-host".into(),
            destination: "user@nonexistent-host".into(),
            ..Default::default()
        };
        let args: Vec<String> = ["--session-id", "abc", "--model", "opus"]
            .map(String::from)
            .into();
        assert_eq!(adapt_agent_args_for_remote(&host, args.clone()), args);
    }

    #[test]
    fn rewrite_config_args_substitutes_translated_path() {
        let args: Vec<String> = ["--settings", "/home/a/.config/friring/hooks/claude.json"]
            .map(String::from)
            .into();
        let out = crate::agent::config_args::rewrite_config_path_args(
            args,
            "/home/a/.config/friring",
            |p| Some(p.replace("/home/a/", "/home/b/")),
        );
        assert_eq!(
            out,
            ["--settings", "/home/b/.config/friring/hooks/claude.json"].map(String::from)
        );
    }

    #[test]
    fn rewrite_config_args_strips_flag_and_path_pair() {
        // When no remote path can work, the path AND its `--settings` flag must
        // both vanish — leaving a dangling flag would eat the next arg.
        let args: Vec<String> = [
            "--verbose",
            "--settings",
            "/home/a/.config/friring/hooks/claude.json",
            "--session-id",
            "x",
        ]
        .map(String::from)
        .into();
        let out = crate::agent::config_args::rewrite_config_path_args(
            args,
            "/home/a/.config/friring",
            |_| None,
        );
        assert_eq!(out, ["--verbose", "--session-id", "x"].map(String::from));
    }

    #[test]
    fn rewrite_config_args_handles_equals_form_and_positional() {
        // `--flag=<path>` rewrites in place / drops as one token; a positional
        // config path (no preceding flag) drops alone.
        let args: Vec<String> = ["--settings=/cfg/hooks/x.json", "/cfg/seed.toml"]
            .map(String::from)
            .into();
        let rewritten =
            crate::agent::config_args::rewrite_config_path_args(args.clone(), "/cfg", |p| {
                Some(format!("/rem{p}"))
            });
        assert_eq!(
            rewritten,
            ["--settings=/rem/cfg/hooks/x.json", "/rem/cfg/seed.toml"].map(String::from)
        );
        let stripped = crate::agent::config_args::rewrite_config_path_args(args, "/cfg", |_| None);
        assert!(stripped.is_empty());
    }

    #[test]
    fn rewrite_config_args_never_pops_a_self_contained_equals_token() {
        // A `--flag=value` token before a stripped positional path is complete
        // on its own — even when it was itself just rewritten — and must
        // survive the pop that removes a dangling value-taking flag.
        let args: Vec<String> = ["--settings=/cfg/a.json", "/cfg/b.json"]
            .map(String::from)
            .into();
        let out = crate::agent::config_args::rewrite_config_path_args(args, "/cfg", |p| {
            (p == "/cfg/a.json").then(|| format!("/rem{p}"))
        });
        assert_eq!(out, ["--settings=/rem/cfg/a.json"].map(String::from));
    }

    #[test]
    fn rewrite_config_args_ignores_sibling_prefixed_paths() {
        // `/cfg-backup` shares the `/cfg` string prefix but is not under the
        // config root — it must pass through untouched, not be shipped/stripped.
        let args: Vec<String> = ["--settings", "/cfg-backup/notes.md"]
            .map(String::from)
            .into();
        let out = crate::agent::config_args::rewrite_config_path_args(args.clone(), "/cfg", |_| {
            panic!("map must not be called for a sibling-prefixed path")
        });
        assert_eq!(out, args);
    }

    #[test]
    fn rewrite_config_args_leaves_unrelated_args_untouched() {
        let args: Vec<String> = ["--model", "opus", "--add-dir", "/home/a/repo"]
            .map(String::from)
            .into();
        let out = crate::agent::config_args::rewrite_config_path_args(
            args.clone(),
            "/home/a/.config/friring",
            |_| panic!("map must not be called for non-config args"),
        );
        assert_eq!(out, args);
    }

    #[test]
    fn resolve_agent_def_derives_name_with_fallback() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        // An explicit, seeded agent wins and its name round-trips.
        assert_eq!(super::super::resolve_agent_def(Some("codex")).name, "codex");
        // Empty/None/unknown fall back to the registry default.
        assert_eq!(
            super::super::resolve_agent_def(Some("")).name,
            DEFAULT_AGENT_NAME
        );
        assert_eq!(
            super::super::resolve_agent_def(None).name,
            DEFAULT_AGENT_NAME
        );
        assert_eq!(
            super::super::resolve_agent_def(Some("no-such-agent")).name,
            DEFAULT_AGENT_NAME
        );
    }

    #[test]
    fn resolve_host_none_is_local() {
        let (backend_type, host) = resolve_host(None).unwrap();
        assert_eq!(backend_type, LOCAL_TMUX_BACKEND_TYPE);
        assert!(host.is_none());
        // Empty string is treated the same as None.
        let (backend_type, host) = resolve_host(Some("")).unwrap();
        assert_eq!(backend_type, LOCAL_TMUX_BACKEND_TYPE);
        assert!(host.is_none());
    }

    #[test]
    fn resolve_host_unknown_errors_with_guidance() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let err = resolve_host(Some("nope")).unwrap_err();
        assert!(err.contains("Unknown host 'nope'"), "got: {err}");
        assert!(err.contains("hosts.toml"), "got: {err}");
    }

    #[test]
    fn resolve_host_reads_configured_host() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let path = crate::agent::host_config::hosts_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "[[hosts]]\nname = \"devbox\"\ndestination = \"me@devbox\"\n",
        )
        .unwrap();

        let (backend_type, host) = resolve_host(Some("devbox")).unwrap();
        assert_eq!(backend_type, "ssh:devbox");
        assert_eq!(host.unwrap().destination, "me@devbox");
    }

    #[test]
    fn resolve_dirs_single_repo_no_worktree_is_unchanged() {
        let mut r = req("s");
        r.repo_path = PathBuf::from("/tmp/primary");
        let (cwd, worktrees, additional) = resolve_dirs(&r, None).unwrap();
        assert_eq!(cwd, PathBuf::from("/tmp/primary"));
        assert!(worktrees.is_empty());
        assert!(additional.is_empty());
    }

    #[test]
    fn resolve_dirs_attaches_dir_extras_without_git() {
        // dir-only extras (worktree == false) never touch git, so this is
        // hermetic. The primary has no worktree branch either.
        let mut r = req("s");
        r.repo_path = PathBuf::from("/tmp/primary");
        r.extra_repos = vec![
            ExtraRepo {
                repo_path: PathBuf::from("/tmp/extra-a"),
                worktree: false,
                base_branch: None,
            },
            ExtraRepo {
                repo_path: PathBuf::from("/tmp/extra-b"),
                worktree: false,
                base_branch: None,
            },
        ];
        let (cwd, worktrees, additional) = resolve_dirs(&r, None).unwrap();
        assert_eq!(cwd, PathBuf::from("/tmp/primary"));
        assert!(worktrees.is_empty());
        assert_eq!(
            additional,
            vec![PathBuf::from("/tmp/extra-a"), PathBuf::from("/tmp/extra-b")]
        );
    }

    #[test]
    fn resolve_dirs_worktree_extra_without_shared_branch_errors() {
        let mut r = req("s");
        r.repo_path = PathBuf::from("/tmp/primary");
        // No worktree_branch on the primary, but an extra wants a worktree.
        r.extra_repos = vec![ExtraRepo {
            repo_path: PathBuf::from("/tmp/extra"),
            worktree: true,
            base_branch: None,
        }];
        let err = resolve_dirs(&r, None).unwrap_err();
        assert!(err.contains("worktree-branch"), "got: {err}");
    }

    #[test]
    fn resolve_launch_cwd_single_member_is_primary() {
        let primary = PathBuf::from("/tmp/primary");
        // No worktrees, no extra dirs → 1 member → primary cwd, no workspace.
        let got = resolve_launch_cwd("sid-1", &primary, &[], &[], None, None);
        assert_eq!(got, primary);
    }

    #[test]
    fn resolve_launch_cwd_multi_member_builds_workspace() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let primary = temp.path().join("primary");
        std::fs::create_dir_all(&primary).unwrap();
        let extra = temp.path().join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        let got = resolve_launch_cwd("sid-multi", &primary, &[], &[extra], None, None);
        // Two members → a symlink workspace, not the primary itself.
        assert_ne!(got, primary);
        assert!(got.join("primary").exists() || got.exists());
    }

    #[test]
    fn resolve_launch_cwd_honors_custom_workspace_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let primary = temp.path().join("primary");
        std::fs::create_dir_all(&primary).unwrap();
        let extra = temp.path().join("extra");
        std::fs::create_dir_all(&extra).unwrap();
        let custom = temp.path().join("my-ws");

        let got = resolve_launch_cwd(
            "sid-custom",
            &primary,
            &[],
            &[extra],
            None,
            Some(custom.as_path()),
        );
        assert_eq!(got, custom);
        assert!(std::fs::read_link(custom.join("primary")).is_ok());
    }
}
