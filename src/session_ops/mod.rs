//! Headless session operations — spawn and restart sessions without the TUI.
//!
//! Callers (MCP, CLI) use these helpers to drive the same local-tmux-backed
//! sessions the TUI manages, without requiring the TUI event loop. All
//! operations are synchronous against the SQLite database and the `tmux -L
//! friring` server.

pub mod builtin_hooks;
pub mod delete;
pub mod extensions;
pub mod restart;
pub mod spawn;

pub use builtin_hooks::{ensure_builtin_hooks_extension, HOOKS_EXTENSION_NAME};
pub use delete::{delete_session_headless, ForceDeleteReport};
pub use extensions::{
    activate_extension, deactivate_extension, ensure_extension, extension_health,
    heal_active_extensions, install_extension, reinstall_extension, uninstall_extension,
    update_all_extensions, update_extension, DeactivateReport, EnsureReport, ExtensionHealth,
    InstallReport, ReinstallReport, UninstallReport, UpdateReport,
};
pub use restart::restart_session_headless;
pub use spawn::{spawn_session_headless, SpawnRequest, SpawnResult};

use std::collections::HashMap;

use crate::session::{AutomationRunStatus, SessionConfig};

/// Reject a `Spawn` automation that cannot fire as authored.
///
/// Takes the finished action rather than loose fields so every authoring path —
/// the TUI editor, `automation create`/`edit`, `automation import`, and
/// extension activation — enforces exactly the same rule by making one call. A
/// per-site field check is how the paths drifted apart in the first place.
///
/// Non-`Spawn` actions are a no-op.
///
/// (`crate::agent::…` is reached by fully-qualified path only — `session_ops`
/// may not `use` it; see `tests/architecture_rules.rs`.)
pub fn validate_spawn_action(action: &crate::session::AutomationAction) -> Result<(), String> {
    let crate::session::AutomationAction::Spawn {
        repo_path,
        worktree_branch,
        agent,
        extra_repos,
        host,
        ..
    } = action
    else {
        return Ok(());
    };
    validate_spawn_selectors(agent.as_deref(), host.as_deref())?;

    let Some(host) = host.as_deref().filter(|h| !h.is_empty()) else {
        return Ok(());
    };
    // Worktree provisioning for a remote spawn is not supported. The TUI fires
    // through the local `git::create_or_attach_worktree`, so it would create the
    // checkout on the wrong machine and hand the remote session a path that does
    // not exist there. Rejecting at save keeps every firing path agreeing on
    // what an automation does, rather than having it work headlessly and quietly
    // misbehave from the TUI.
    if worktree_branch.is_some() {
        return Err(format!(
            "a spawn on host '{host}' cannot use a worktree branch — remote worktree \
             provisioning is unsupported; drop the worktree to run in the repo root"
        ));
    }
    if extra_repos.iter().any(|e| e.worktree) {
        return Err(format!(
            "a spawn on host '{host}' cannot use worktree extra-repos — remote worktree \
             provisioning is unsupported; attach them as plain directories instead"
        ));
    }
    // `~` is expanded against the *local* home before the row is stored, which
    // silently points a remote spawn at a path belonging to this machine's user.
    let home_relative = |p: &std::path::Path| {
        let s = p.to_string_lossy();
        s == "~" || s.starts_with("~/")
    };
    if home_relative(repo_path) || extra_repos.iter().any(|e| home_relative(&e.repo_path)) {
        return Err(format!(
            "a spawn on host '{host}' needs absolute paths — `~` resolves to this \
             machine's home, not the host's"
        ));
    }
    Ok(())
}

/// Reject a spawn naming an agent or host that isn't configured.
///
/// Prefer [`validate_spawn_action`], which applies this plus the remote-spawn
/// rules to a finished action.
///
/// Neither is checked downstream: agent resolution falls back to the
/// registry default for an unknown name, so a typo would silently launch the
/// wrong agent on every fire, and an unknown host only surfaces when the fire
/// tries to reach it. Lives here rather than in `cli` so the CLI authoring
/// paths and extension activation ([`ensure_extension`]) enforce the same rule
/// — a manifest is as much an authoring path as a flag.
///
/// (`crate::agent::…` is reached by fully-qualified path only — `session_ops`
/// may not `use` it; see `tests/architecture_rules.rs`.)
pub fn validate_spawn_selectors(agent: Option<&str>, host: Option<&str>) -> Result<(), String> {
    if let Some(name) = agent.map(str::trim).filter(|a| !a.is_empty()) {
        let registry = crate::agent::agent_config::load_or_seed();
        if !registry.names().contains(&name) {
            return Err(format!(
                "Unknown agent '{name}'. Configure it in agents.toml. Available: [{}]",
                registry.names().join(", ")
            ));
        }
    }
    if let Some(name) = host.map(str::trim).filter(|h| !h.is_empty()) {
        let registry = crate::agent::host_config::load_all();
        if registry.get(name).is_none() {
            return Err(format!(
                "Unknown host '{name}'. Configure it in hosts.toml. Available: [{}]",
                registry.names().join(", ")
            ));
        }
    }
    Ok(())
}

/// Run an `Exec` automation's shell command headlessly (`sh -c`, or `cmd /C` on
/// Windows) and report its outcome for the run history. No session/agent is
/// involved — this is the deterministic-scheduled-job path shared by the TUI and
/// the headless `automation tick`. stdout+stderr are tail-truncated so a chatty
/// command can't bloat the history.
///
/// The command is killed after `timeout_secs`
/// ([`DEFAULT_EXEC_TIMEOUT_SECS`](crate::session::automation::DEFAULT_EXEC_TIMEOUT_SECS)
/// when `None`), so a hung job releases its run row instead of sitting
/// `Running` forever.
pub fn run_exec_command_with_timeout(
    command: &str,
    timeout_secs: Option<u64>,
) -> (AutomationRunStatus, String) {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (AutomationRunStatus::Error, format!("spawn failed: {e}")),
    };

    // Drain both pipes on their own threads: polling `try_wait` while the child
    // fills a pipe buffer would deadlock (the child blocks on write, we block on
    // the deadline), and `wait_with_output` consumes the child so there'd be no
    // handle left to kill.
    let stdout = child.stdout.take().map(spawn_reader);
    let stderr = child.stderr.take().map(spawn_reader);

    let timeout = std::time::Duration::from_secs(
        timeout_secs.unwrap_or(crate::session::automation::DEFAULT_EXEC_TIMEOUT_SECS),
    );
    let (status, timed_out) = wait_with_deadline(&mut child, timeout);
    let out = stdout.map(join_reader).unwrap_or_default();
    let err = stderr.map(join_reader).unwrap_or_default();

    let mut detail = tail_chars(&out);
    let stderr_tail = tail_chars(&err);
    if !stderr_tail.is_empty() {
        if !detail.is_empty() {
            detail.push('\n');
        }
        detail.push_str(&stderr_tail);
    }

    if timed_out {
        let secs = timeout.as_secs();
        let msg = if detail.is_empty() {
            format!("timed out after {secs}s (killed)")
        } else {
            format!("timed out after {secs}s (killed): {detail}")
        };
        return (AutomationRunStatus::Error, msg);
    }
    let Some(status) = status else {
        return (AutomationRunStatus::Error, "wait failed".to_string());
    };
    if status.success() {
        let msg = if detail.is_empty() {
            "ok".into()
        } else {
            detail
        };
        (AutomationRunStatus::Success, msg)
    } else {
        let code = status.code().map_or("signal".into(), |c| c.to_string());
        let msg = if detail.is_empty() {
            format!("exit {code}")
        } else {
            format!("exit {code}: {detail}")
        };
        (AutomationRunStatus::Error, msg)
    }
}

/// [`run_exec_command_with_timeout`] on the default deadline.
pub fn run_exec_command(command: &str) -> (AutomationRunStatus, String) {
    run_exec_command_with_timeout(command, None)
}

/// Run an `Exec` automation on a detached worker thread, closing out the
/// already-recorded `Running` run (`run_id`) when the command exits.
///
/// This is what keeps a long command off the TUI's tick thread. The worker owns
/// its own database connection (`App::db` isn't shareable, and the run outlives
/// the tick that started it); if it can't open one the run row is left for
/// `Database::reap_orphaned_automation_runs` to close on the next startup.
pub fn run_exec_command_detached(run_id: i64, command: String, timeout_secs: Option<u64>) {
    std::thread::spawn(move || {
        let (status, detail) = run_exec_command_with_timeout(&command, timeout_secs);
        let Some(path) = crate::paths::database_file() else {
            tracing::error!("exec run {run_id} finished but the database path is unresolvable");
            return;
        };
        let db = match crate::storage::Database::open(&path) {
            Ok(db) => db,
            Err(e) => {
                tracing::error!("exec run {run_id} finished but its result is unrecorded: {e}");
                return;
            }
        };
        if let Err(e) = db.finish_automation_run(run_id, status, &detail) {
            tracing::error!("Failed to finish automation run {run_id}: {e}");
        }
    });
}

/// How much of each pipe is kept while the command runs.
///
/// Only [`tail_chars`]'s last 500 characters ever reach the run row, but the
/// pipe still has to be drained or the child blocks. Buffering it all would let
/// a chatty command (a runaway build log) grow a `Vec` inside friring's own
/// process for the whole timeout window — up to 15 minutes by default — for
/// output that is then discarded. 64 KiB is far more than the tail needs, even
/// for multi-byte characters.
const EXEC_TAIL_BYTES: usize = 64 * 1024;

/// Drain a child pipe on its own thread (see [`run_exec_command_with_timeout`]
/// for why the pipes can't be polled inline), keeping only the trailing
/// [`EXEC_TAIL_BYTES`].
fn spawn_reader<R: std::io::Read + Send + 'static>(
    mut pipe: R,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut tail: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        let mut chunk = [0u8; 8192];
        loop {
            match std::io::Read::read(&mut pipe, &mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend(&chunk[..n]);
                    let excess = tail.len().saturating_sub(EXEC_TAIL_BYTES);
                    tail.drain(..excess);
                }
            }
        }
        Vec::from(tail)
    })
}

/// Collect a reader thread's bytes; a panicked reader yields no output rather
/// than taking the run down with it.
fn join_reader(handle: std::thread::JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
}

/// Wait for `child` up to `timeout`, killing it if the deadline passes.
/// Returns `(exit status, timed_out)`.
fn wait_with_deadline(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> (Option<std::process::ExitStatus>, bool) {
    /// Poll cadence — fine enough that a quick command isn't noticeably
    /// delayed, coarse enough to cost nothing over a 15-minute job.
    const POLL: std::time::Duration = std::time::Duration::from_millis(50);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (Some(status), false),
            Ok(None) => {}
            Err(_) => return (None, false),
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            // Reap the killed child so it doesn't linger as a zombie; the pipe
            // readers unblock once its stdio handles close.
            let _ = child.wait();
            return (None, true);
        }
        std::thread::sleep(POLL);
    }
}

/// Keep the last 500 chars of a captured stream. Single pass via a capped ring
/// so a huge stream isn't walked twice (count + skip) or materialized in full.
fn tail_chars(raw: &[u8]) -> String {
    const TAIL_CHARS: usize = 500;
    let t = String::from_utf8_lossy(raw);
    let t = t.trim();
    let mut ring: std::collections::VecDeque<char> =
        std::collections::VecDeque::with_capacity(TAIL_CHARS + 1);
    for c in t.chars() {
        if ring.len() == TAIL_CHARS {
            ring.pop_front();
        }
        ring.push_back(c);
    }
    ring.into_iter().collect()
}

/// Decide whether to pass the agent's resume group vs starting fresh when
/// (re)spawning. Returns `Some(id.clone())` only when a Claude transcript for
/// that id already exists on disk under `CLAUDE_CONFIG_DIR`/`~/.claude`.
///
/// Claude-specific: only the `claude` agent persists resumable transcripts.
/// For agents without an on-disk transcript this returns `None`, which makes
/// restart start a fresh conversation (the live tmux process is what provides
/// cross-restart persistence in the general case).
///
/// Shared by [`restart::restart_session_headless`] and
/// `App::restart_active_session` so the headless and TUI paths agree.
pub(crate) fn resume_id_if_transcript_exists(
    agent_session_id: &str,
    env: &HashMap<String, String>,
) -> Option<String> {
    let config_dir_override = env.get("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from);
    crate::paths::claude_transcript_exists(agent_session_id, config_dir_override.as_deref())
        .then(|| agent_session_id.to_string())
}

/// Decide the `resume_session_id` to use when restarting a session, given the
/// agent's definition.
///
/// - Agents that resume "the latest session in the launch directory"
///   ([`AgentDef::resumes_latest`]) get the session id back as a non-`None`
///   *trigger*: their `resume_args` are id-less (no `{id}` token), so the value
///   itself is ignored — its presence is what makes [`AgentDef::build_args`]
///   emit the resume group. Restart always reuses the session's directory, so
///   the agent's own "last in cwd" resolution targets the right conversation.
/// - Everyone else (claude) falls back to the transcript check, which returns
///   the pinned id only when a resumable transcript exists on disk.
///
/// Shared by the headless restart path and `App`'s restart/restore paths so
/// they agree on when to resume vs. start fresh.
pub(crate) fn resume_trigger_for(
    def: &crate::session::AgentDef,
    agent_session_id: &str,
    env: &HashMap<String, String>,
) -> Option<String> {
    if def.resumes_latest() {
        return Some(agent_session_id.to_string());
    }
    resume_id_if_transcript_exists(agent_session_id, env)
}

/// Resolve the [`AgentDef`](crate::session::AgentDef) for a requested agent name
/// from the on-disk registry. The single source of truth for the agent fallback
/// chain (requested name → registry default → built-in default), so spawn and
/// restart agree on both the launched def *and* its `.name` without re-running
/// the seed. `None`/empty falls straight through to the registry default.
pub(crate) fn resolve_agent_def(requested: Option<&str>) -> crate::session::AgentDef {
    let registry = crate::agent::agent_config::load_or_seed();
    requested
        .filter(|n| !n.is_empty())
        .and_then(|n| registry.get(n))
        .or_else(|| registry.default_agent())
        .cloned()
        .unwrap_or_else(|| {
            crate::agent::agent_config::builtin_registry()
                .default_agent()
                .cloned()
                .expect("built-in registry always has a default agent")
        })
}

/// Build the `(command, args)` invocation for an already-resolved [`AgentDef`].
///
/// Centralised here so headless spawn and restart agree on the args, and so the
/// `AgentDef` is resolved exactly once per operation (callers pass the def they
/// already resolved rather than re-running [`resolve_agent_def`]).
fn build_agent_invocation(
    def: &crate::session::AgentDef,
    config: &SessionConfig,
) -> (String, Vec<String>) {
    let provider = crate::agent::GenericProvider::new(def.clone());
    // Reach the provider trait methods via fully-qualified call syntax so this
    // module imports nothing from the agent module (architecture rule:
    // session_ops must stay free of agent-layer imports).
    let command =
        <crate::agent::GenericProvider as crate::agent::AgentProvider>::command(&provider)
            .to_string();
    let args = <crate::agent::GenericProvider as crate::agent::AgentProvider>::build_args(
        &provider, config,
    );
    (command, args)
}

/// Inject the standard friring env hints into a session config so a
/// `friring-cli` call running *inside* the session can prove its own identity
/// without scraping panes or names:
///
/// - `FRIRING_SESSION` — the friring [`SessionId`] (the registry key). Read by
///   the mailbox CLI to auto-stamp provenance and default the inbox to "me".
///   Requires `config.session_id` to be set before calling.
/// - `FRIRING_SESSION_ID` — the *agent's* conversation id (`agent_session_id`),
///   consumed by the metrics statusline. Distinct from `FRIRING_SESSION`.
/// - `FRIRING_TASK` — the originating task id, when this session was spawned for
///   a task (so messages auto-tag `from_task_id`). Headless `task run` only; the
///   TUI task-spawn path tracks the link in-memory instead.
/// - `FRIRING_METRICS_DIR` — metrics output dir.
/// - `FRIRING_CONFIG_DIR` / `FRIRING_DATA_DIR` — the resolved config/data dirs,
///   so the agent's `friring-cli` (its status hook) targets the same DB the TUI
///   reads regardless of XDG / PATH / a stale tmux-server env.
///
/// The three *path* vars are **local-only**: a remote (SSH/WSL) session skips
/// them — the local dirs don't exist on the host, and a remote `friring-cli`
/// pinned to them would resolve garbage instead of its own defaults. The
/// identity vars (`FRIRING_SESSION`/`FRIRING_SESSION_ID`/`FRIRING_TASK`) are
/// opaque and travel everywhere.
///
/// Kept in sync with `App::build_spawn_inputs` so headless and TUI sessions look
/// identical to the spawned process (modulo `FRIRING_TASK` as noted above).
///
/// Shared by the headless spawn/restart paths and the TUI `Ctrl+R` restart
/// (`App::restart_active_session`), so a restarted session keeps the same
/// identity env a fresh spawn would have had.
pub(crate) fn inject_friring_env(
    config: &mut SessionConfig,
    agent_session_id: &str,
    task_id: Option<i64>,
) {
    config
        .env
        .insert("FRIRING_SESSION_ID".into(), agent_session_id.into());
    if let Some(id) = config.session_id {
        config.env.insert("FRIRING_SESSION".into(), id.to_string());
    }
    if let Some(task_id) = task_id {
        config
            .env
            .insert("FRIRING_TASK".into(), task_id.to_string());
    }
    if config
        .backend
        .as_deref()
        .is_some_and(crate::session::is_remote_backend)
    {
        return;
    }
    if let Some(dir) = crate::paths::metrics_directory() {
        config
            .env
            .insert("FRIRING_METRICS_DIR".into(), dir.to_string_lossy().into());
    }
    // Pin the agent's `friring-cli` (its status hook) to the *same* config/data
    // dirs this friring resolved, so a status `signal` always lands in the DB
    // the TUI reads — independent of XDG, which `friring-cli` is on PATH, or a
    // stale tmux-server env. Derived from the resolved file paths' parents.
    if let Some(dir) = crate::paths::config_file().and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        config.env.insert(
            crate::paths::CONFIG_DIR_OVERRIDE_ENV.into(),
            dir.to_string_lossy().into(),
        );
    }
    if let Some(dir) =
        crate::paths::database_file().and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        config.env.insert(
            crate::paths::DATA_DIR_OVERRIDE_ENV.into(),
            dir.to_string_lossy().into(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::AutomationAction;
    use crate::session::SessionId;

    #[cfg(unix)]
    #[test]
    fn run_exec_command_kills_a_hung_command_at_the_deadline() {
        let started = std::time::Instant::now();
        let (status, detail) = run_exec_command_with_timeout("sleep 30", Some(1));
        assert_eq!(status, AutomationRunStatus::Error);
        assert!(detail.contains("timed out"), "got {detail}");
        // The point of the deadline: it returns instead of hanging the caller.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_exec_command_captures_output_larger_than_a_pipe_buffer() {
        // Regression guard for the reader threads: polling the deadline while
        // the child fills a 64 KiB pipe would deadlock instead of finishing.
        let (status, detail) = run_exec_command_with_timeout(
            "for i in $(seq 1 20000); do echo aaaaaaaaaaaaaaaaaaaa; done",
            Some(30),
        );
        assert_eq!(status, AutomationRunStatus::Success);
        // Only the tail is kept, so the history stays bounded.
        assert!(detail.len() <= 500, "detail was {} chars", detail.len());
    }

    #[cfg(unix)]
    #[test]
    fn run_exec_command_keeps_the_tail_of_a_huge_stream() {
        // The reader drops everything but the trailing `EXEC_TAIL_BYTES` as it
        // drains, so the last lines still have to survive intact.
        let (status, detail) = run_exec_command_with_timeout(
            "for i in $(seq 1 60000); do echo filler-line-$i; done; echo THE-END",
            Some(60),
        );
        assert_eq!(status, AutomationRunStatus::Success);
        assert!(detail.ends_with("THE-END"), "got {detail}");
        assert!(detail.len() <= 500, "detail was {} chars", detail.len());
    }

    #[cfg(unix)]
    #[test]
    fn run_exec_command_reports_success_and_failure() {
        let (status, detail) = run_exec_command("printf hello");
        assert_eq!(status, AutomationRunStatus::Success);
        assert!(detail.contains("hello"), "got {detail}");

        let (status, detail) = run_exec_command("exit 3");
        assert_eq!(status, AutomationRunStatus::Error);
        assert!(detail.contains('3'), "got {detail}");

        // No output on success collapses to a friendly "ok".
        let (status, detail) = run_exec_command("true");
        assert_eq!(status, AutomationRunStatus::Success);
        assert_eq!(detail, "ok");
    }

    #[test]
    fn inject_env_sets_identity_and_task() {
        let sid = SessionId::default();
        let mut config = SessionConfig {
            session_id: Some(sid),
            ..SessionConfig::default()
        };
        inject_friring_env(&mut config, "agent-conv-uuid", Some(42));
        // The friring session key and the agent conversation id are distinct.
        assert_eq!(config.env.get("FRIRING_SESSION"), Some(&sid.to_string()));
        assert_eq!(
            config.env.get("FRIRING_SESSION_ID"),
            Some(&"agent-conv-uuid".to_string())
        );
        assert_eq!(config.env.get("FRIRING_TASK"), Some(&"42".to_string()));
    }

    #[test]
    fn inject_env_pins_config_and_data_dirs() {
        // The agent's status hook must target the same DB the TUI reads, so the
        // resolved config/data dirs are injected for `friring-cli` to honour.
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut config = SessionConfig {
            session_id: Some(SessionId::default()),
            ..SessionConfig::default()
        };
        inject_friring_env(&mut config, "agent-conv-uuid", None);

        let cfg_dir = config
            .env
            .get(crate::paths::CONFIG_DIR_OVERRIDE_ENV)
            .expect("config dir injected");
        let data_dir = config
            .env
            .get(crate::paths::DATA_DIR_OVERRIDE_ENV)
            .expect("data dir injected");
        assert_eq!(
            Some(std::path::Path::new(cfg_dir)),
            crate::paths::config_file()
                .as_deref()
                .and_then(|p| p.parent())
        );
        assert_eq!(
            Some(std::path::Path::new(data_dir)),
            crate::paths::database_file()
                .as_deref()
                .and_then(|p| p.parent())
        );
    }

    #[test]
    fn inject_env_skips_local_path_dirs_for_remote_backend() {
        // The metrics/config/data dirs are *local* paths — meaningless on an
        // SSH/WSL host, and a remote `friring-cli` pinned to them would resolve
        // garbage. Identity vars still travel.
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut config = SessionConfig {
            session_id: Some(SessionId::default()),
            backend: Some("ssh:devbox".into()),
            ..SessionConfig::default()
        };
        inject_friring_env(&mut config, "agent-conv-uuid", None);
        assert!(config.env.contains_key("FRIRING_SESSION"));
        assert!(config.env.contains_key("FRIRING_SESSION_ID"));
        assert!(!config.env.contains_key("FRIRING_METRICS_DIR"));
        assert!(!config
            .env
            .contains_key(crate::paths::CONFIG_DIR_OVERRIDE_ENV));
        assert!(!config.env.contains_key(crate::paths::DATA_DIR_OVERRIDE_ENV));
    }

    #[test]
    fn inject_env_omits_task_when_absent() {
        let mut config = SessionConfig {
            session_id: Some(SessionId::default()),
            ..SessionConfig::default()
        };
        inject_friring_env(&mut config, "agent-conv-uuid", None);
        assert!(config.env.contains_key("FRIRING_SESSION"));
        assert!(!config.env.contains_key("FRIRING_TASK"));
    }

    #[test]
    fn resume_id_is_none_when_no_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("CLAUDE_CONFIG_DIR".into(), tmp.path().display().to_string());
        assert_eq!(resume_id_if_transcript_exists("some-uuid", &env), None);
    }

    #[test]
    fn resume_id_is_some_when_transcript_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = "77777777-8888-9999-aaaa-bbbbbbbbbbbb";
        let proj = tmp.path().join("projects").join("-slug");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join(format!("{sid}.jsonl")), b"").unwrap();

        let mut env = HashMap::new();
        env.insert("CLAUDE_CONFIG_DIR".into(), tmp.path().display().to_string());
        assert_eq!(
            resume_id_if_transcript_exists(sid, &env),
            Some(sid.to_string())
        );
    }

    #[test]
    fn resume_trigger_latest_agent_always_triggers() {
        // A resume_latest agent (codex) triggers resume regardless of any
        // on-disk claude transcript; the returned id is just the trigger.
        let codex = crate::agent::agent_config::builtin_registry()
            .get("codex")
            .unwrap()
            .clone();
        assert!(codex.resumes_latest());
        let env = HashMap::new();
        assert_eq!(
            resume_trigger_for(&codex, "friring-uuid", &env),
            Some("friring-uuid".to_string())
        );
    }

    #[test]
    fn resume_trigger_claude_defers_to_transcript() {
        // claude is not resume_latest, so it only resumes when a transcript
        // exists — same behaviour as resume_id_if_transcript_exists.
        let claude = crate::agent::agent_config::builtin_registry()
            .get("claude")
            .unwrap()
            .clone();
        assert!(!claude.resumes_latest());

        let tmp = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("CLAUDE_CONFIG_DIR".into(), tmp.path().display().to_string());
        assert_eq!(resume_trigger_for(&claude, "missing", &env), None);

        let sid = "11111111-2222-3333-4444-555555555555";
        let proj = tmp.path().join("projects").join("-slug");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join(format!("{sid}.jsonl")), b"").unwrap();
        assert_eq!(
            resume_trigger_for(&claude, sid, &env),
            Some(sid.to_string())
        );
    }
}
