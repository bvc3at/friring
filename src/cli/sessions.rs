//! Session CRUD and orchestration subcommands.

use std::collections::HashMap;
use std::path::PathBuf;

use clap::Subcommand;
use serde_json::{json, Value};

use crate::cli::output::{self, CommandOutput};
use crate::session::bridge::BridgeChild;
use crate::session::{SandboxEnforcement, SessionId};
use crate::storage::{Database, HookRow};
use crate::sync::SharedSession;

#[derive(Subcommand, Debug)]
pub enum Action {
    /// List all active sessions.
    List {
        /// Only list children of this parent session UUID.
        #[arg(long)]
        parent: Option<String>,
    },
    /// Get a session by UUID.
    Get {
        /// Session UUID.
        uuid: String,
    },
    /// Create a new session (runs synchronously — tmux window live on return).
    Create {
        /// Session name (1-64 chars, no slashes or leading '.').
        #[arg(long)]
        name: String,
        /// Absolute path to the repository or working directory.
        #[arg(long)]
        repo_path: PathBuf,
        /// Coding agent to launch (e.g. "claude", "codex"). Falls back to the
        /// default agent from `agents.toml`.
        #[arg(long)]
        agent: Option<String>,
        /// If set, create a git worktree on this branch off --base-branch.
        #[arg(long)]
        worktree_branch: Option<String>,
        /// Base branch for the worktree (default: main).
        #[arg(long)]
        base_branch: Option<String>,
        /// Remote host to run the session on (name from `hosts.toml`). The
        /// worktree and tmux window are created on that host over SSH.
        #[arg(long)]
        host: Option<String>,
        /// Parent session UUID (lead/worker relationship for orchestration).
        /// Must reference an existing active session.
        #[arg(long)]
        parent: Option<String>,
        /// Additional repo to span (repeatable). `PATH` or `PATH@BASE` — each
        /// gets its own isolated worktree on `--worktree-branch` off `BASE`
        /// (default: the primary's `--base-branch`). Makes a multi-repo session.
        #[arg(long = "add-repo")]
        add_repo: Vec<String>,
        /// Additional directory to span (repeatable), attached as-is (no
        /// worktree / branch). Makes a multi-repo session.
        #[arg(long = "add-dir")]
        add_dir: Vec<String>,
        /// Sandbox profile to run the agent under (name from the profile list;
        /// see docs/SANDBOX.md). Unset = unsandboxed. An unknown name fails.
        #[arg(long)]
        sandbox: Option<String>,
    },
    /// Soft-delete a session.
    ///
    /// By default only the DB row is soft-deleted (the TUI cleans up the
    /// tmux window and worktree on next sync). Pass `--force` to also
    /// kill the tmux window, remove worktrees, and cancel pending
    /// scheduled commands — useful for headless cleanup when the TUI
    /// isn't running.
    Delete {
        /// Session UUID.
        uuid: String,
        /// Also kill the tmux window, remove worktrees, and cancel
        /// pending scheduled commands for this session.
        #[arg(long)]
        force: bool,
    },
    /// Restore a soft-deleted session.
    Restore {
        /// Session UUID.
        uuid: String,
        /// Recover a force-deleted session best-effort: only committed branch
        /// state comes back (uncommitted/untracked work was lost on delete).
        #[arg(long)]
        best_effort: bool,
    },
    /// Restart a session in-place (kills the window, re-spawns with --resume).
    Restart {
        /// Session UUID.
        uuid: String,
    },
    /// Type text into a session's terminal, followed by Enter.
    Send {
        /// Session UUID.
        uuid: String,
        /// Text to send.
        text: String,
        /// Type even when the session is showing a dialog. Without it the send
        /// is refused there, because the trailing Enter would answer the dialog
        /// rather than submit the text.
        #[arg(long)]
        force: bool,
    },
    /// Capture rendered pane contents as text.
    Capture {
        /// Session UUID.
        uuid: String,
        /// Scrollback lines to include (default 200, max 10000).
        #[arg(long, default_value_t = 200)]
        lines: u32,
    },
    /// Mark a session as the pending focus target for the running TUI.
    ///
    /// Writes the session id into the SQLite `metadata` row the TUI polls;
    /// the next external-state tick reads + clears it and switches the
    /// active terminal. Used by the macOS click-to-focus path
    /// (`terminal-notifier -execute` shells back into this), and works as
    /// a generic "switch the TUI to `<session>`" hook from any external
    /// trigger. A no-op when the TUI isn't running (the request just
    /// sits in the DB until either it is or the row is overwritten).
    Focus {
        /// Session UUID.
        uuid: String,
    },
    /// Report a session's agent metrics (cost, tokens, context, code churn).
    ///
    /// Read from the agent's statusline JSON under `FRIRING_METRICS_DIR` —
    /// Claude-only today, and local sessions only (friring never injects that
    /// dir into a remote agent). Works with no TUI running.
    Metrics(crate::cli::metrics::TargetArgs),
    /// Report a session's agent process-tree memory (summed RSS + process count).
    ///
    /// Local sessions only: a remote session's tree lives on its host. Add
    /// `--cpu` to also sample process CPU, which costs a short sampling delay.
    Resources(crate::cli::metrics::ResourceArgs),
    /// Report what a session's agent did (commands, edits, reads, tokens).
    ///
    /// Reconstructed from the agent CLI's own on-disk transcripts, the same
    /// source the F9 activity view reads. Local sessions only.
    Activity(crate::cli::metrics::TargetArgs),
    /// Report an agent lifecycle transition (called from an agent hook).
    ///
    /// Records the session's state so the TUI can render it (working/blocked/
    /// done/idle) — works headless; the TUI picks it up via its data_version
    /// poll. Identity defaults to the calling session ($FRIRING_SESSION,
    /// injected at spawn), so an agent hook passes no id.
    Signal {
        /// The reported state. `idle` = agent ready/at-rest (e.g. a fresh
        /// session boot); `done` = a turn just finished (shows until you look).
        #[arg(long, value_parser = ["working", "blocked", "done", "idle"])]
        state: String,
        /// Override the calling session (UUID). Defaults to $FRIRING_SESSION,
        /// then a lookup by the agent conversation id ($FRIRING_SESSION_ID).
        #[arg(long)]
        session: Option<String>,
    },
}

pub fn run(action: Action, db: &Database) -> Result<CommandOutput, String> {
    match action {
        Action::List { parent } => {
            let parent_id = parent.as_deref().map(parse_session_id).transpose()?;
            let sessions: Vec<SharedSession> = db
                .list_active_sessions()
                .map_err(|e| format!("list_active_sessions: {e}"))?
                .into_iter()
                .filter(|s| parent_id.is_none() || s.parent_session_id == parent_id)
                .collect();
            let hooks = db
                .load_hook_states()
                .map_err(|e| format!("load_hook_states: {e}"))?;
            let index = BridgeIndex::from_rows(
                db.all_bridge_children()
                    .map_err(|e| format!("all_bridge_children: {e}"))?,
            );
            let mut rendered = Vec::with_capacity(sessions.len());
            for s in &sessions {
                let bridge = bridge_json(db, &index, s)?;
                rendered.push(shared_session_to_json(s, hooks.get(&s.id), bridge));
            }
            Ok(CommandOutput::new(
                Value::Array(rendered),
                render_session_list(&sessions),
            ))
        }
        Action::Get { uuid } => {
            let session = resolve(db, &uuid)?;
            let hooks = db
                .load_hook_states()
                .map_err(|e| format!("load_hook_states: {e}"))?;
            // One session, so the index is built from the two statements this
            // path always ran rather than from a whole-table read.
            let key = session.id.to_string();
            let mut rows = db
                .bridge_children_of(&key)
                .map_err(|e| format!("bridge_children_of({key}): {e}"))?;
            rows.extend(
                db.bridge_child(&key)
                    .map_err(|e| format!("bridge_child({key}): {e}"))?,
            );
            let index = BridgeIndex::from_rows(rows);
            let bridge = bridge_json(db, &index, &session)?;
            Ok(CommandOutput::new(
                shared_session_to_json(&session, hooks.get(&session.id), bridge),
                render_session_detail(&session),
            ))
        }
        Action::Create {
            name,
            repo_path,
            agent,
            worktree_branch,
            base_branch,
            host,
            parent,
            add_repo,
            add_dir,
            sandbox,
        } => {
            let parent_session_id = parent.as_deref().map(parse_session_id).transpose()?;
            let extra_repos = super::parse_extra_repos(&add_repo, &add_dir);
            let req = crate::session_ops::SpawnRequest {
                name,
                repo_path,
                worktree_branch,
                base_branch,
                agent,
                agent_session_id: None,
                host,
                parent_session_id,
                task_id: None,
                extra_repos,
                sandbox_profile: sandbox,
            };
            let res = crate::session_ops::spawn_session_headless(db, req)?;
            let mut human = format!(
                "Created session '{}' ({}) — {}\ncwd: {}",
                res.name,
                res.agent,
                res.session_id,
                res.cwd.display()
            );
            // A `--sandbox` the launch could not honour is the one thing about
            // this session the caller must not have to read a log to learn.
            let unenforced = unenforced_sandbox_reason(res.sandbox.as_ref());
            if let Some(reason) = unenforced.as_deref() {
                human.push_str(&format!("\nNOT sandboxed — {reason}"));
            }
            // A boundary that holds and an agent with no credential in it: the
            // session works, and nothing will happen in that pane until somebody
            // signs in there.
            if let Some(how) = res.sandbox_login.as_deref() {
                human.push_str(&format!("\nSigned out inside the sandbox — {how}"));
            }
            Ok(CommandOutput::new(
                json!({
                    "id": res.session_id.to_string(),
                    "name": res.name,
                    "agent": res.agent,
                    "agent_session_id": res.agent_session_id,
                    "cwd": res.cwd.display().to_string(),
                    "parent_session_id": res.parent_session_id.map(|id| id.to_string()),
                    "sandbox_unenforced": unenforced,
                    "sandbox_login": res.sandbox_login,
                }),
                human,
            ))
        }
        Action::Delete { uuid, force } => {
            let session = resolve(db, &uuid)?;
            let report = crate::session_ops::delete_session_headless(db, session.id, force)?;
            let mut human = format!("Deleted session '{}' ({})", session.name, session.id);
            if force {
                let mut detail: Vec<(&str, String)> = vec![
                    ("killed window", report.killed_window.to_string()),
                    (
                        "removed worktrees",
                        report.removed_worktrees.len().to_string(),
                    ),
                    // An operator who force-deleted an orchestrator stopped N
                    // other agents by doing so, and nothing else says which.
                    (
                        "stopped children",
                        report.stopped_children.len().to_string(),
                    ),
                    (
                        "disabled automations",
                        report.disabled_automations.to_string(),
                    ),
                ];
                if !report.worktree_errors.is_empty() {
                    detail.push(("worktree errors", report.worktree_errors.join("; ")));
                }
                if let Some(err) = &report.remote_teardown_error {
                    detail.push(("remote teardown error", err.clone()));
                }
                for line in output::kv(&detail).lines() {
                    human.push_str(&format!("\n  {line}"));
                }
            }
            Ok(CommandOutput::new(
                json!({
                    "deleted": true,
                    "id": session.id.to_string(),
                    "name": session.name,
                    "forced": force,
                    "killed_window": report.killed_window,
                    "removed_worktrees": report.removed_worktrees,
                    "worktree_errors": report.worktree_errors,
                    "stopped_children": report.stopped_children,
                    "disabled_automations": report.disabled_automations,
                    "remote_teardown_error": report.remote_teardown_error,
                }),
                human,
            ))
        }
        Action::Restore { uuid, best_effort } => {
            let id: SessionId = uuid
                .parse()
                .map_err(|_| format!("Invalid session UUID: {uuid}"))?;
            let deleted = db
                .get_deleted_session_by_id(id)
                .map_err(|e| format!("get_deleted_session_by_id: {e}"))?
                .ok_or_else(|| format!("Deleted session not found: {uuid}"))?;
            // A force-deleted session lost its uncommitted work; restoring it only
            // recovers committed branch state, so require an explicit opt-in.
            if deleted.force_deleted && !best_effort {
                return Err(format!(
                    "Session '{}' was force-deleted; pass --best-effort to recover committed work (uncommitted/untracked changes are gone)",
                    deleted.name
                ));
            }
            // `restore_session` clears `deleted_at` and `force_deleted`; a running
            // TUI re-creates worktrees + the tmux window on its next sync.
            db.restore_session(deleted.id)
                .map_err(|e| format!("restore_session: {e}"))?;
            let best_effort_recovery = deleted.force_deleted;
            let human = if best_effort_recovery {
                format!(
                    "Restored session '{}' ({}) — best-effort: uncommitted work was not recovered",
                    deleted.name, deleted.id
                )
            } else {
                format!("Restored session '{}' ({})", deleted.name, deleted.id)
            };
            Ok(CommandOutput::new(
                json!({
                    "restored": true,
                    "id": deleted.id.to_string(),
                    "name": deleted.name,
                    "best_effort": best_effort_recovery,
                }),
                human,
            ))
        }
        Action::Restart { uuid } => {
            let session = resolve(db, &uuid)?;
            let sandbox = crate::session_ops::restart_session_headless(db, session.id)?;
            // A relaunch re-decides the boundary, and the restart re-spawns the
            // window without rewriting the row — so record the verdict here or
            // the TUI adopting this pane later would render the *previous*
            // launch's answer: the shield over an agent that just landed on the
            // host, or a warning about one that is back inside its sandbox.
            if let Err(e) = db.set_session_sandbox_enforcement(
                session.id,
                &SandboxEnforcement::from_launch(sandbox.as_ref()),
            ) {
                // The window is already back up, so say what really happened:
                // the restart worked and the record of its boundary did not.
                return Err(format!(
                    "Restarted '{}' but failed to record what the relaunch did with its \
                     sandbox ({e}) — the stored state is the previous launch's until the \
                     next relaunch",
                    session.name
                ));
            }
            let unenforced = unenforced_sandbox_reason(sandbox.as_ref());
            let mut human = format!("Restarted session '{}' ({})", session.name, session.id);
            if let Some(reason) = unenforced.as_deref() {
                human.push_str(&format!("\nNOT sandboxed — {reason}"));
            }
            Ok(CommandOutput::new(
                json!({
                    "restarted": true,
                    "session_id": session.id.to_string(),
                    "session_name": session.name,
                    "sandbox_unenforced": unenforced,
                }),
                human,
            ))
        }
        Action::Send { uuid, text, force } => {
            let session = resolve(db, &uuid)?;
            if text.trim().is_empty() {
                return Err("text must not be empty".into());
            }
            // Typing into a pane is this command's whole purpose, so a refusal
            // is a loud error rather than the silent skip the mailbox wake
            // takes: the caller asked for exactly this and deserves to know it
            // didn't happen. `--force` is the way through for an operator who
            // is looking at the dialog and means to answer it.
            if !force {
                if let Some(reason) = refuse_send(db, &session) {
                    return Err(format!(
                        "refusing to type into '{}': {reason}. The trailing Enter would answer it \
                         — re-run with --force to type anyway.",
                        session.name
                    ));
                }
            }
            crate::agent::tmux::send_prompt_unguarded_on(
                &crate::agent::tmux::MuxTarget::local(),
                &session.name,
                &text,
            )
            .map_err(|e| format!("send_prompt_unguarded_on: {e}"))?;
            Ok(CommandOutput::new(
                json!({
                    "sent": true,
                    "session_id": session.id.to_string(),
                    "session_name": session.name,
                    "forced": force,
                }),
                format!("Sent to '{}'.", session.name),
            ))
        }
        Action::Capture { uuid, lines } => {
            let session = resolve(db, &uuid)?;
            let output = crate::agent::tmux::capture_pane_text(&session.name, lines)
                .map_err(|e| format!("capture_pane_text: {e}"))?;
            let human = output.clone();
            Ok(CommandOutput::new(
                json!({
                    "session_id": session.id.to_string(),
                    "session_name": session.name,
                    "lines": lines,
                    "output": output,
                }),
                human,
            ))
        }
        Action::Focus { uuid } => {
            let session = resolve(db, &uuid)?;
            db.set_pending_focus_session_id(session.id)
                .map_err(|e| format!("set_pending_focus_session_id: {e}"))?;
            Ok(CommandOutput::new(
                json!({
                    "focused": true,
                    "session_id": session.id.to_string(),
                    "session_name": session.name,
                }),
                format!("Focus requested for '{}'.", session.name),
            ))
        }
        Action::Metrics(target) => crate::cli::metrics::run_metrics(target, db),
        Action::Resources(args) => crate::cli::metrics::run_resources(args, db),
        Action::Activity(target) => crate::cli::metrics::run_activity(target, db),
        Action::Signal { state, session } => {
            let target = resolve_signal_target(db, session.as_deref())?;
            db.set_hook_state(target.id, &state)
                .map_err(|e| format!("set_hook_state: {e}"))?;
            Ok(CommandOutput::new(
                json!({
                    "signaled": true,
                    "session_id": target.id.to_string(),
                    "session_name": target.name,
                    "state": state,
                }),
                format!("Signaled {state} for '{}'.", target.name),
            ))
        }
    }
}

/// Resolve the session a `signal` targets: an explicit `--session` UUID, else
/// the calling session from `$FRIRING_SESSION`, else a lookup by the agent
/// conversation id from `$FRIRING_SESSION_ID` (the env fallback for agents whose
/// hooks don't inherit `$FRIRING_SESSION`). Errors when none resolves.
fn resolve_signal_target(db: &Database, session: Option<&str>) -> Result<SharedSession, String> {
    if let Some(uuid) = session {
        return resolve(db, uuid);
    }
    crate::cli::identity::calling_session_or_by_agent_id(db)?
        .ok_or_else(|| "not inside a friring session; pass --session <uuid>".into())
}

/// Render the session list as an aligned table (or a friendly empty line).
fn render_session_list(sessions: &[SharedSession]) -> String {
    if sessions.is_empty() {
        return "No active sessions.".to_string();
    }
    let rows: Vec<Vec<String>> = sessions
        .iter()
        .map(|s| {
            // `dash` already maps an empty branch (no worktree) to "-".
            let branch = s.worktrees.first().map(|w| w.branch.as_str());
            vec![
                s.name.clone(),
                s.agent.clone(),
                s.backend_type.clone(),
                output::dash(branch),
                output::dash(s.cwd.as_ref().map(|p| p.display().to_string()).as_deref()),
                s.id.to_string(),
            ]
        })
        .collect();
    output::table(&["NAME", "AGENT", "BACKEND", "BRANCH", "CWD", "ID"], &rows)
}

/// Render a single session as an aligned key/value block, with any worktrees
/// listed one per line beneath it.
fn render_session_detail(s: &SharedSession) -> String {
    let pairs: Vec<(&str, String)> = vec![
        ("name", s.name.clone()),
        ("id", s.id.to_string()),
        ("agent", s.agent.clone()),
        ("backend", s.backend_type.clone()),
        (
            "agent_session_id",
            output::dash(s.agent_session_id.as_deref()),
        ),
        (
            "cwd",
            output::dash(s.cwd.as_ref().map(|p| p.display().to_string()).as_deref()),
        ),
        (
            "workspace_dir",
            output::dash(
                s.workspace_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .as_deref(),
            ),
        ),
        (
            "parent",
            output::dash(s.parent_session_id.map(|id| id.to_string()).as_deref()),
        ),
        ("sandbox", render_sandbox(s)),
    ];
    let mut block = output::kv(&pairs);
    for w in &s.worktrees {
        block.push_str(&format!(
            "\nworktree:  {} @ {}",
            w.branch,
            w.worktree_path.display()
        ));
    }
    block
}

/// The `sandbox` line of `session get`: the profile the session asked for, and
/// — when the last launch could not deliver it — that it is **not** in force,
/// with the reason. A human reading this row must not have to know that a
/// profile name alone says nothing about whether the boundary went on.
fn render_sandbox(s: &SharedSession) -> String {
    let Some(profile) = s.sandbox_profile.as_deref() else {
        return output::dash(None);
    };
    match s.sandbox_enforcement.unenforced_reason() {
        Some(reason) => format!("{profile} — NOT enforced: {reason}"),
        None => profile.to_string(),
    }
}

fn parse_session_id(uuid: &str) -> Result<SessionId, String> {
    uuid.parse()
        .map_err(|_| format!("Invalid session UUID: {uuid}"))
}

fn resolve(db: &Database, uuid: &str) -> Result<SharedSession, String> {
    let id = parse_session_id(uuid)?;
    db.get_session_by_id(id)
        .map_err(|e| format!("get_session_by_id: {e}"))?
        .ok_or_else(|| format!("Session not found: {uuid}"))
}

/// Why a launch ran **outside** the sandbox profile it asked for, or `None`
/// when the boundary went on (and for a session that asked for none).
///
/// The session keeps its profile through a fallback so the next relaunch tries
/// again, which is exactly why the fallback itself has to be reported rather
/// than inferred from the row. Goes through [`SandboxEnforcement`] rather than
/// matching the state itself so what a command *reports* and what the row
/// *records* can never be two different answers to the same question.
fn unenforced_sandbox_reason(state: Option<&crate::session::SandboxState>) -> Option<String> {
    SandboxEnforcement::from_launch(state)
        .unenforced_reason()
        .map(str::to_string)
}

/// Why typing into `session` is refused right now, if it is.
///
/// Both halves of the guard, checked here rather than inside the send, because
/// `--force` has to be able to step past them: the agent's own reported state
/// (`session signal --state blocked`) and a scrape of its visible pane. tmux is
/// reached by fully-qualified path (never `use crate::agent`) — see
/// tests/architecture_rules.rs::cli_module_isolation.
fn refuse_send(db: &Database, session: &SharedSession) -> Option<String> {
    if crate::cli::pane_guard::blocked_on_prompt(db, session.id) {
        return Some(crate::cli::pane_guard::BLOCKED_REASON.to_string());
    }
    crate::agent::tmux::pane_modal_on(&crate::agent::tmux::MuxTarget::local(), &session.name)
        .map(crate::cli::pane_guard::modal_reason)
}

// `hook_state`/`hook_state_at` are the raw persisted hook columns (schema
// v34), not the TUI's derived status: an external observer (automation, the
// agent-e2e harness) must see exactly what `session signal` wrote, without
// the TUI's quiescence downgrade. Null until the first hook fires.
//
// `sandbox_profile` and `sandbox_unenforced` are the two halves of the boundary
// and answer different questions: the profile is what the session asked for
// (it survives a fallback, because the next relaunch tries again), the reason
// is why the last launch did not deliver it. `null` there means no launch
// recorded a complaint — `create`/`restart` report their own launch's verdict
// under the same key.
//
// `egress_state` is a third question and deliberately not folded into either:
// a session whose proxy could not be rebound after a restart is still
// *sandboxed*, and reporting that as `sandbox_unenforced` would say the agent
// is running on the host. The endpoint is reported for the same reason a port
// number is ever reported — it is what an operator checks — and the token it
// demands is not, because nothing that renders a session may carry it.
fn shared_session_to_json(
    s: &SharedSession,
    hook: Option<&HookRow>,
    bridge: Option<Value>,
) -> Value {
    json!({
        "id": s.id.to_string(),
        "name": s.name,
        "agent": s.agent,
        "backend_type": s.backend_type,
        "agent_session_id": s.agent_session_id,
        "cwd": s.cwd.as_ref().map(|p| p.display().to_string()),
        "additional_dirs": s.additional_dirs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "workspace_dir": s.workspace_dir.as_ref().map(|p| p.display().to_string()),
        "parent_session_id": s.parent_session_id.map(|id| id.to_string()),
        "sandbox_profile": s.sandbox_profile,
        "sandbox_unenforced": s.sandbox_enforcement.unenforced_reason(),
        "egress_state": s.egress.state.label(),
        "egress_unrestorable_reason": s.egress.state.reason(),
        "egress_endpoint": s.egress.endpoint,
        "display_order": s.display_order,
        "hook_state": hook.and_then(|h| h.state.as_deref()),
        "hook_state_at": hook.and_then(|h| h.state_at),
        "worktrees": s.worktrees.iter().map(|w| json!({
            "repo_path": w.repo_path.display().to_string(),
            "worktree_path": w.worktree_path.display().to_string(),
            "branch": w.branch,
        })).collect::<Vec<_>>(),
        "bridge": bridge,
    })
}

/// Every ownership row, indexed both ways, so `list` asks for them once.
///
/// `session list` renders one document per session and the bridge block is
/// `None` for nearly all of them; discovering that per session cost two
/// statements each. `session get` builds this for the one session it was asked
/// about, which is the same two statements it always did.
struct BridgeIndex {
    owner_of: HashMap<String, String>,
    children_of: HashMap<String, Vec<BridgeChild>>,
}

impl BridgeIndex {
    fn from_rows(rows: Vec<BridgeChild>) -> Self {
        let mut owner_of = HashMap::new();
        let mut children_of: HashMap<String, Vec<BridgeChild>> = HashMap::new();
        for row in rows {
            owner_of.insert(row.child_id.clone(), row.owner_id.clone());
            children_of
                .entry(row.owner_id.clone())
                .or_default()
                .push(row);
        }
        Self {
            owner_of,
            children_of,
        }
    }
}

/// Where a session sits in an orchestration, for `session get` and `list`
/// (ADR-32).
///
/// `null` for the overwhelming majority of sessions, which are in none. What is
/// reported is **host-known** throughout: the ownership row, the states friring
/// itself set, and the verdict it read from git after the pane stopped. A
/// child's own summary is not here — an integration step reads this document,
/// and text a worker wrote must not be part of what decides whether its branch
/// is merged.
///
/// Every read is propagated rather than defaulted. A database error and "this
/// session is in no orchestration" are the same JSON — `null` — and the
/// consumers are scripts, so answering the second when the first is true would
/// tell an integration step that a verified child has no verdict. The sibling
/// `load_hook_states` read in the same command already fails the command; this
/// one now does too.
///
/// The egress token is never reported, here or anywhere else that renders a
/// session.
fn bridge_json(
    db: &Database,
    index: &BridgeIndex,
    s: &SharedSession,
) -> Result<Option<Value>, String> {
    let key = s.id.to_string();
    let owner = index.owner_of.get(&key);
    let empty = Vec::new();
    let children = index.children_of.get(&key).unwrap_or(&empty);
    if owner.is_none() && children.is_empty() {
        return Ok(None);
    }
    let own_state = db
        .bridge_child_state(&key)
        .map_err(|e| format!("bridge_child_state({key}): {e}"))?
        .map(|row| row.state.to_string());
    // One statement for the owner's whole set, rather than one per child.
    let states: HashMap<String, String> = if children.is_empty() {
        HashMap::new()
    } else {
        db.bridge_child_states_of(&key)
            .map_err(|e| format!("bridge_child_states_of({key}): {e}"))?
            .into_iter()
            .map(|row| (row.child_id, row.state.to_string()))
            .collect()
    };
    let mut rendered = Vec::with_capacity(children.len());
    for child in children {
        let result = db
            .bridge_result(&child.child_id)
            .map_err(|e| format!("bridge_result({}): {e}", child.child_id))?;
        rendered.push(json!({
            "id": child.child_id,
            "created_at": child.created_at,
            "state": states.get(&child.child_id),
            "result": result.map(|r| json!({
                "outcome": r.outcome.as_str(),
                "branch": r.branch,
                "head": r.head,
                "dirty": r.dirty,
                "ahead_of_base": r.ahead_of_base,
                "verified_at": r.verified_at,
            })),
        }));
    }
    Ok(Some(json!({
        "owner": owner,
        "state": own_state,
        "children": rendered,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[test]
    fn list_empty_returns_array() {
        let db = db();
        let v = run(Action::List { parent: None }, &db).unwrap();
        assert!(v.is_array(), "got {v}");
        assert_eq!(v.as_array().unwrap().len(), 0);
        assert_eq!(v.human, "No active sessions.");
    }

    /// A leader's own children, and the verdict friring reached for each.
    #[test]
    fn list_and_get_report_the_orchestration_a_session_is_in() {
        let db = db();
        let leader = make_test_session("leader");
        let child = make_test_session("child");
        db.upsert_session(&leader).unwrap();
        db.upsert_session(&child).unwrap();
        let (owner_id, child_id) = (leader.id.to_string(), child.id.to_string());
        db.insert_bridge_child(&child_id, &owner_id, "k1").unwrap();
        db.set_bridge_child_state(&child_id, crate::session::bridge::ChildState::Ready)
            .unwrap();

        let listed = run(Action::List { parent: None }, &db).unwrap();
        let rows = listed.as_array().unwrap();
        let of = |name: &str| {
            rows.iter()
                .find(|r| r["name"] == name)
                .unwrap_or_else(|| panic!("no row for {name}"))
                .clone()
        };
        assert_eq!(of("leader")["bridge"]["children"][0]["id"], child_id);
        assert_eq!(of("leader")["bridge"]["children"][0]["state"], "ready");
        assert_eq!(of("child")["bridge"]["owner"], owner_id);

        // `get` composes the same block from the one session's own rows.
        let got = run(
            Action::Get {
                uuid: child_id.clone(),
            },
            &db,
        )
        .unwrap();
        assert_eq!(got["bridge"]["owner"], owner_id);
    }

    /// A database that cannot be read must not render as "in no orchestration".
    ///
    /// The two are the same JSON — `null` — and the consumers are scripts, so a
    /// swallowed error would tell an integration step that a verified child has
    /// no verdict. Fails without the propagation: the reads were
    /// `.ok().flatten()` and `unwrap_or_default()`.
    #[test]
    fn a_bridge_read_that_fails_fails_the_command() {
        let db = db();
        let shared = make_test_session("leader");
        let id = shared.id;
        db.upsert_session(&shared).unwrap();
        // A read that genuinely fails, rather than a seam: there is no table.
        db.conn_ref()
            .execute("DROP TABLE bridge_children", [])
            .unwrap();

        let err = run(Action::List { parent: None }, &db).unwrap_err();
        assert!(err.contains("all_bridge_children"), "got {err}");
        let err = run(
            Action::Get {
                uuid: id.to_string(),
            },
            &db,
        )
        .unwrap_err();
        assert!(err.contains("bridge_children_of"), "got {err}");
    }

    #[test]
    fn get_and_list_expose_raw_hook_state() {
        let db = db();
        let shared = make_test_session("hooked");
        let id = shared.id;
        db.upsert_session(&shared).unwrap();

        // Before any signal: fields present, null.
        let v = run(
            Action::Get {
                uuid: id.to_string(),
            },
            &db,
        )
        .unwrap();
        assert!(v["hook_state"].is_null(), "got {v}");
        assert!(v["hook_state_at"].is_null(), "got {v}");

        db.set_hook_state(id, "working").unwrap();
        let v = run(
            Action::Get {
                uuid: id.to_string(),
            },
            &db,
        )
        .unwrap();
        assert_eq!(v["hook_state"], "working");
        assert!(v["hook_state_at"].is_i64(), "got {v}");

        let v = run(Action::List { parent: None }, &db).unwrap();
        assert_eq!(v.as_array().unwrap()[0]["hook_state"], "working");
    }

    #[test]
    fn get_exposes_workspace_dir_and_additional_dirs() {
        let db = db();
        let mut shared = make_test_session("multi");
        shared.additional_dirs = vec![std::path::PathBuf::from("/repos/b")];
        shared.workspace_dir = Some(std::path::PathBuf::from("/home/dev/named-ws"));
        let id = shared.id;
        db.upsert_session(&shared).unwrap();

        let v = run(
            Action::Get {
                uuid: id.to_string(),
            },
            &db,
        )
        .unwrap();
        assert_eq!(v["workspace_dir"], "/home/dev/named-ws");
        assert_eq!(v["additional_dirs"][0], "/repos/b");

        // A default-workspace session reports null, not a phantom path.
        let plain = make_test_session("plain");
        let pid = plain.id;
        db.upsert_session(&plain).unwrap();
        let v = run(
            Action::Get {
                uuid: pid.to_string(),
            },
            &db,
        )
        .unwrap();
        assert!(v["workspace_dir"].is_null(), "got {v}");
    }

    /// The two halves of the boundary are separate keys because they answer
    /// separate questions. A consumer that only saw `sandbox_profile` would
    /// report a session as sandboxed while its agent runs on the host.
    #[test]
    fn get_and_list_report_the_profile_and_whether_it_is_enforced() {
        let db = db();
        let mut shared = make_test_session("boxed");
        shared.sandbox_profile = Some("dev".to_string());
        let id = shared.id;
        db.upsert_session(&shared).unwrap();

        // Nothing recorded: the profile is reported, with no complaint.
        let v = run(
            Action::Get {
                uuid: id.to_string(),
            },
            &db,
        )
        .unwrap();
        assert_eq!(v["sandbox_profile"], "dev");
        assert!(v["sandbox_unenforced"].is_null(), "got {v}");
        assert!(v.human.contains("sandbox"), "{}", v.human);
        assert!(!v.human.contains("NOT enforced"), "{}", v.human);

        // A launch that fell back to the host says so, in both shapes.
        db.set_session_sandbox_enforcement(
            id,
            &SandboxEnforcement::Unenforced("bwrap is not installed".to_string()),
        )
        .unwrap();
        let v = run(
            Action::Get {
                uuid: id.to_string(),
            },
            &db,
        )
        .unwrap();
        assert_eq!(v["sandbox_profile"], "dev");
        assert_eq!(v["sandbox_unenforced"], "bwrap is not installed");
        assert!(
            v.human.contains("NOT enforced: bwrap is not installed"),
            "{}",
            v.human
        );

        let v = run(Action::List { parent: None }, &db).unwrap();
        let row = &v.as_array().unwrap()[0];
        assert_eq!(row["sandbox_unenforced"], "bwrap is not installed");

        // An unsandboxed session carries the key, empty — a stable key set for
        // a consumer that greps for it.
        let plain = make_test_session("plain");
        let plain_id = plain.id;
        db.upsert_session(&plain).unwrap();
        let v = run(
            Action::Get {
                uuid: plain_id.to_string(),
            },
            &db,
        )
        .unwrap();
        assert!(v["sandbox_profile"].is_null(), "got {v}");
        assert!(v["sandbox_unenforced"].is_null(), "got {v}");
    }

    #[test]
    fn signal_explicit_session_sets_hook_state() {
        let db = db();
        let shared = make_test_session("worker");
        let id = shared.id;
        db.upsert_session(&shared).unwrap();

        let out = run(
            Action::Signal {
                state: "blocked".into(),
                session: Some(id.to_string()),
            },
            &db,
        )
        .unwrap();
        assert_eq!(out["state"], "blocked");
        assert_eq!(out["signaled"], true);

        let states = db.load_hook_states().unwrap();
        assert_eq!(states.get(&id).unwrap().state.as_deref(), Some("blocked"));
    }

    #[test]
    fn signal_without_identity_errors() {
        let db = db();
        // No --session and (in test) no FRIRING_SESSION env → clear error.
        std::env::remove_var("FRIRING_SESSION");
        std::env::remove_var("FRIRING_SESSION_ID");
        let err = run(
            Action::Signal {
                state: "done".into(),
                session: None,
            },
            &db,
        )
        .unwrap_err();
        assert!(err.contains("not inside a friring session"), "got {err}");
    }

    fn make_test_session(name: &str) -> SharedSession {
        SharedSession {
            id: SessionId::default(),
            name: name.into(),
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
        }
    }

    #[test]
    fn render_session_list_tabulates_rows() {
        let s = SharedSession {
            id: SessionId::default(),
            name: "demo".into(),
            agent: "dev".into(),
            backend_id: String::new(),
            backend_type: "local-tmux".into(),
            agent_session_id: None,
            cwd: Some(std::path::PathBuf::from("/tmp/repo")),
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
        let rendered = render_session_list(std::slice::from_ref(&s));
        assert!(rendered.contains("NAME"));
        assert!(rendered.contains("demo"));
        assert!(rendered.contains("local-tmux"));
        // No worktree → branch column shows a dash.
        assert!(rendered.contains('-'));
    }

    #[test]
    fn list_returns_session_with_expected_shape() {
        let db = db();
        let id = SessionId::default();
        let shared = SharedSession {
            id,
            name: "demo".into(),
            agent: "dev".into(),
            backend_id: "tb-demo".into(),
            backend_type: "local-tmux".into(),
            agent_session_id: Some("agent-1".into()),
            cwd: Some(std::path::PathBuf::from("/tmp/repo")),
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

        let v = run(Action::List { parent: None }, &db).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let s = &arr[0];
        assert_eq!(s["id"].as_str(), Some(id.to_string().as_str()));
        assert_eq!(s["name"].as_str(), Some("demo"));
        assert_eq!(s["agent"].as_str(), Some("dev"));
        assert_eq!(s["backend_type"].as_str(), Some("local-tmux"));
        assert_eq!(s["agent_session_id"].as_str(), Some("agent-1"));
        assert_eq!(s["cwd"].as_str(), Some("/tmp/repo"));
        assert!(s["parent_session_id"].is_null());
        assert!(s["display_order"].is_null());
        assert!(s["worktrees"].is_array());
    }

    #[test]
    fn list_emits_parent_session_id_and_filters_by_parent() {
        let db = db();
        let parent_id = SessionId::default();
        let parent = SharedSession {
            id: parent_id,
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
            mux: crate::session::MuxIdentity::default(),
            egress: crate::session::EgressRecord::default(),
            sandbox_overlay: None,
        };
        db.upsert_session(&parent).unwrap();
        let mut child = parent.clone();
        child.id = SessionId::default();
        child.name = "worker".into();
        child.parent_session_id = Some(parent_id);
        db.upsert_session(&child).unwrap();

        // Unfiltered list carries the field on both rows.
        let v = run(Action::List { parent: None }, &db).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let worker = arr.iter().find(|s| s["name"] == "worker").unwrap();
        assert_eq!(
            worker["parent_session_id"].as_str(),
            Some(parent_id.to_string().as_str())
        );

        // --parent filters to direct children only.
        let v = run(
            Action::List {
                parent: Some(parent_id.to_string()),
            },
            &db,
        )
        .unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"].as_str(), Some("worker"));

        // Malformed --parent uuid errors.
        let err = run(
            Action::List {
                parent: Some("not-a-uuid".into()),
            },
            &db,
        )
        .unwrap_err();
        assert!(err.contains("Invalid session UUID"), "got {err}");
    }

    #[test]
    fn get_unknown_uuid_errors() {
        let db = db();
        let err = run(
            Action::Get {
                uuid: "not-a-uuid".into(),
            },
            &db,
        )
        .unwrap_err();
        assert!(err.contains("Invalid session UUID"), "got {err}");
    }

    #[test]
    fn send_rejects_empty_text() {
        let db = db();
        let id = SessionId::default();
        let shared = SharedSession {
            id,
            name: "demo".into(),
            agent: "dev".into(),
            backend_id: "tb-demo".into(),
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
        // Both empty and whitespace-only text are rejected (trimmed check).
        for text in ["", "   \t\n"] {
            let err = run(
                Action::Send {
                    uuid: id.to_string(),
                    text: text.to_string(),
                    force: false,
                },
                &db,
            )
            .unwrap_err();
            assert!(err.contains("text"), "got {err}");
        }
    }

    #[test]
    fn soft_delete_leaves_session_recoverable() {
        // `session delete` without --force only soft-deletes the DB row,
        // leaving its automations enabled and the session restorable.
        let db = db();
        let id = SessionId::default();
        let shared = SharedSession {
            id,
            name: "Foo Bar".into(),
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
            mux: crate::session::MuxIdentity::default(),
            egress: crate::session::EgressRecord::default(),
            sandbox_overlay: None,
        };
        db.upsert_session(&shared).unwrap();
        let auto = db
            .create_automation(&crate::storage::automations::NewAutomation {
                name: "noop".into(),
                enabled: true,
                schedule: crate::session::AutomationSchedule::Once { at: u64::MAX },
                timezone: None,
                action: crate::session::AutomationAction::send_to(id),
                prompt: "noop".into(),
                next_run_at: Some(u64::MAX),
                prompt_steps: Vec::new(),
            })
            .unwrap();

        let v = run(
            Action::Delete {
                uuid: id.to_string(),
                force: false,
            },
            &db,
        )
        .unwrap();
        assert_eq!(v["deleted"], true);
        assert_eq!(v["forced"], false);
        assert_eq!(v["disabled_automations"], 0);

        // Row is soft-deleted but recoverable; the automation is untouched.
        assert!(db.get_session_by_id(id).unwrap().is_none());
        assert!(db.get_automation(auto).unwrap().unwrap().enabled);
        let restored = run(
            Action::Restore {
                uuid: id.to_string(),
                best_effort: false,
            },
            &db,
        )
        .unwrap();
        assert_eq!(restored["restored"], true);
        assert_eq!(restored["best_effort"], false);
        assert!(db.get_session_by_id(id).unwrap().is_some());
    }

    #[test]
    fn restore_force_deleted_requires_best_effort_flag() {
        let db = db();
        let shared = make_test_session("forced");
        let id = shared.id;
        db.upsert_session(&shared).unwrap();

        // Force-delete marks the row force_deleted; restore must then opt in.
        run(
            Action::Delete {
                uuid: id.to_string(),
                force: true,
            },
            &db,
        )
        .unwrap();

        let err = run(
            Action::Restore {
                uuid: id.to_string(),
                best_effort: false,
            },
            &db,
        )
        .unwrap_err();
        assert!(err.contains("--best-effort"), "{err}");
        // Still soft-deleted (refused).
        assert!(db.get_deleted_session_by_id(id).unwrap().is_some());

        let restored = run(
            Action::Restore {
                uuid: id.to_string(),
                best_effort: true,
            },
            &db,
        )
        .unwrap();
        assert_eq!(restored["restored"], true);
        assert_eq!(restored["best_effort"], true);
        assert!(db.get_session_by_id(id).unwrap().is_some());
    }
}
