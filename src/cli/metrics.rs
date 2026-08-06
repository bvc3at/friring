//! Agent metrics subcommands — `session metrics` / `resources` / `activity`
//! and the account-level `usage`.
//!
//! Every command reads the **source** the TUI reads (a statusline JSON file,
//! the machine's process table, the agent's own transcripts, the vendor usage
//! API) rather than a value a running TUI published. Two reasons, in order:
//!
//! - **No TUI is required.** Sessions outlive the TUI (tmux keeps them alive),
//!   so a metrics command that only worked while a TUI was attached would be
//!   useless from cron or a script — the very place these numbers are wanted.
//! - **Nothing is persisted.** Caching them into SQLite on the TUI's tick
//!   cadence would bump every *other* friring connection's `data_version` and
//!   force a full shared-state reload on each poll; see the note on
//!   `App::publish_perf_snapshot`, which is gated behind a debug flag for
//!   exactly that reason. The sources are cheap and never stale.
//!
//! Coverage therefore matches the TUI's exactly, including its gaps: the
//! statusline dir is local-only (`session_ops::inject_friring_env` skips it for
//! ssh/wsl sessions), and the process table and transcripts are local too. A
//! remote session reports `null` with a note, never a zero.

use clap::Args;
use serde_json::{json, Value};

use crate::cli::output::{self, CommandOutput};
use crate::session::{AgentMetrics, SessionId};
use crate::storage::Database;
use crate::sync::SharedSession;

/// Which sessions a per-session metrics command runs over: one resolved UUID,
/// or every active session.
#[derive(Args, Debug)]
pub struct TargetArgs {
    /// Session UUID. Omit with --all to cover every active session.
    pub uuid: Option<String>,
    /// Report every active session instead of one.
    #[arg(long)]
    pub all: bool,
}

impl TargetArgs {
    /// Resolve to the sessions to report on, preserving `session list` order.
    fn resolve(&self, db: &Database) -> Result<Vec<SharedSession>, String> {
        match (&self.uuid, self.all) {
            (Some(uuid), false) => {
                let id: SessionId = uuid
                    .parse()
                    .map_err(|_| format!("Invalid session UUID: {uuid}"))?;
                let session = db
                    .get_session_by_id(id)
                    .map_err(|e| format!("get_session_by_id: {e}"))?
                    .ok_or_else(|| format!("Session not found: {uuid}"))?;
                Ok(vec![session])
            }
            (None, true) => db
                .list_active_sessions()
                .map_err(|e| format!("list_active_sessions: {e}")),
            (Some(_), true) => Err("Pass a session UUID or --all, not both".into()),
            (None, false) => Err("Pass a session UUID or --all".into()),
        }
    }

    /// Whether the caller asked for a collection. Drives the JSON shape: a
    /// single target returns the object itself (like `session get`), `--all`
    /// returns an array (like `session list`).
    fn is_collection(&self) -> bool {
        self.all
    }
}

/// Wrap per-session rows in the shape the target implies, so `--all` is an
/// array and a single UUID is the bare object.
fn shape(target: &TargetArgs, mut rows: Vec<Value>) -> Value {
    if target.is_collection() {
        Value::Array(rows)
    } else {
        // `resolve` guarantees exactly one row for a single target.
        rows.pop().unwrap_or(Value::Null)
    }
}

/// The identity fields every per-session metrics row carries, so a `--all`
/// array is self-describing without a second `session list` call.
fn row_identity(s: &SharedSession) -> Value {
    json!({
        "session_id": s.id.to_string(),
        "name": s.name,
        "agent": s.agent,
    })
}

/// Merge `extra` into an identity object (both must be JSON objects).
fn with_identity(s: &SharedSession, extra: Value) -> Value {
    let mut row = row_identity(s);
    if let (Some(obj), Value::Object(extra)) = (row.as_object_mut(), extra) {
        obj.extend(extra);
    }
    row
}

/// Whether this session runs on a remote host, whose local-only sources
/// friring never writes or reads (see the module docs).
fn is_remote(s: &SharedSession) -> bool {
    crate::session::is_remote_backend(&s.backend_type)
}

// --- session metrics (statusline) -------------------------------------------

/// Read each target's statusline metrics file and report it.
pub fn run_metrics(target: TargetArgs, db: &Database) -> Result<CommandOutput, String> {
    let sessions = target.resolve(db)?;
    let rows: Vec<Value> = sessions.iter().map(metrics_row).collect();
    let human = render_metrics(&sessions, &rows);
    Ok(CommandOutput::new(shape(&target, rows), human))
}

/// One session's statusline metrics, or a note explaining the absence.
///
/// The three absences are deliberately distinct: a remote session can never
/// have a file, a session with no conversation id has nothing to key one by,
/// and a missing file is the ordinary "the agent hasn't rendered its statusline
/// into friring's metrics dir" case (it needs a `statusLine` writing
/// `$FRIRING_METRICS_DIR/$FRIRING_SESSION_ID.json` — see docs/CLI.md).
fn metrics_row(s: &SharedSession) -> Value {
    let (metrics, note) = read_statusline(s);
    with_identity(
        s,
        json!({
            "metrics": metrics.map(|m| serde_json::to_value(m).unwrap_or(Value::Null)),
            "note": note,
        }),
    )
}

fn read_statusline(s: &SharedSession) -> (Option<AgentMetrics>, Option<&'static str>) {
    if is_remote(s) {
        return (
            None,
            Some("remote session: metrics are written on the host"),
        );
    }
    let Some(agent_session_id) = s.agent_session_id.as_deref() else {
        return (None, Some("session has no agent conversation id yet"));
    };
    let Some(path) = crate::paths::session_metrics_file(agent_session_id) else {
        return (None, Some("no metrics directory could be resolved"));
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return (None, Some("no statusline metrics file written yet"));
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return (None, Some("statusline metrics file is not valid JSON"));
    };
    let metrics = AgentMetrics::from_statusline_json(&value);
    if metrics.is_empty() {
        return (
            None,
            Some("statusline metrics file has no recognized fields"),
        );
    }
    (Some(metrics), None)
}

fn render_metrics(sessions: &[SharedSession], rows: &[Value]) -> String {
    if sessions.is_empty() {
        return "No sessions.".to_string();
    }
    let table_rows: Vec<Vec<String>> = sessions
        .iter()
        .zip(rows)
        .map(|(s, row)| {
            let m = &row["metrics"];
            vec![
                s.name.clone(),
                output::dash(m["model_display_name"].as_str()),
                m["total_cost_usd"]
                    .as_f64()
                    .map_or_else(|| "-".into(), |c| format!("${c:.4}")),
                fmt_tokens(
                    m["total_input_tokens"].as_u64(),
                    m["total_output_tokens"].as_u64(),
                ),
                m["used_percentage"]
                    .as_u64()
                    .map_or_else(|| "-".into(), |p| format!("{p}%")),
                fmt_churn(
                    m["total_lines_added"].as_u64(),
                    m["total_lines_removed"].as_u64(),
                ),
                output::dash(row["note"].as_str()),
            ]
        })
        .collect();
    output::table(
        &[
            "SESSION",
            "MODEL",
            "COST",
            "TOKENS I/O",
            "CTX",
            "LINES +/-",
            "NOTE",
        ],
        &table_rows,
    )
}

/// `in/out` token pair, dashed when neither side was reported.
fn fmt_tokens(input: Option<u64>, output_tokens: Option<u64>) -> String {
    match (input, output_tokens) {
        (None, None) => "-".to_string(),
        (i, o) => format!("{}/{}", compact(i.unwrap_or(0)), compact(o.unwrap_or(0))),
    }
}

/// `+added/-removed` pair, dashed when neither side was reported.
fn fmt_churn(added: Option<u64>, removed: Option<u64>) -> String {
    match (added, removed) {
        (None, None) => "-".to_string(),
        (a, r) => format!("+{}/-{}", a.unwrap_or(0), r.unwrap_or(0)),
    }
}

/// Human-scale count: `1.2M` / `15.2k` / `900`.
fn compact(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1_000.0),
        n => n.to_string(),
    }
}

// --- session activity (agent transcripts) -----------------------------------

/// Passes `scan_once` may spend draining a large transcript's backlog before
/// reporting partial history. Each pass ingests up to 8 MB, so this covers a
/// ~800 MB transcript — far past any real one, while still bounded.
const ACTIVITY_MAX_PASSES: usize = 100;

/// Reconstruct what each target's agent did, from its own on-disk transcripts.
///
/// This is the one metrics command whose cost scales with history: it parses
/// the session's transcript from scratch (the TUI tails it incrementally
/// instead, because it rescans every second).
pub fn run_activity(target: TargetArgs, db: &Database) -> Result<CommandOutput, String> {
    let sessions = target.resolve(db)?;
    let agents = crate::agent::agent_config::load_or_seed();
    let rows: Vec<Value> = sessions.iter().map(|s| activity_row(s, &agents)).collect();
    let human = render_activity(&sessions, &rows);
    Ok(CommandOutput::new(shape(&target, rows), human))
}

fn activity_row(s: &SharedSession, agents: &crate::session::AgentRegistry) -> Value {
    use crate::session::activity::ActivityCounts;

    // The provider keys off the registry entry's *command* basename, so a
    // custom agent name wrapping a known CLI still resolves.
    let command = agents
        .get(&s.agent)
        .map(|a| a.command.clone())
        .unwrap_or_else(|| s.agent.clone());

    // An unmeasured row still carries every key, explicitly null: absent is not
    // zero, and a jq pipeline sees one stable shape across both outcomes.
    if is_remote(s) {
        return unmeasured_activity(s, "remote session: its transcripts live on the host".into());
    }
    let Some(provider) = crate::activity::ProviderKind::for_command(&command) else {
        let note = crate::activity::unsupported_reason(&command)
            .map(str::to_string)
            .unwrap_or_else(|| format!("no activity provider for '{command}'"));
        return unmeasured_activity(s, note);
    };

    let (state, complete) = crate::activity::scan_once(
        provider,
        s.agent_session_id.clone(),
        candidate_dirs(s),
        ACTIVITY_MAX_PASSES,
    );
    let events = state.events();
    let counts = ActivityCounts::tally(events);
    let meta = state.meta();
    let note = if events.is_empty() {
        Some("no transcript found for this session yet".to_string())
    } else if !complete {
        Some("history still incomplete: the transcript exceeded the read budget".to_string())
    } else if state.truncated() {
        Some("oldest history was clipped by the source's own cap".to_string())
    } else {
        None
    };

    with_identity(
        s,
        json!({
            "provider": provider.id(),
            "title": meta.title,
            "model": meta.model,
            "counts": {
                "total": counts.total(),
                "prompts": counts.prompts,
                "commands": counts.commands,
                "edits": counts.edits,
                "reads": counts.reads,
                "searches": counts.searches,
                "web": counts.web,
                "subagents": counts.subagents,
                "other": counts.other,
                "failed": counts.failed,
            },
            "tokens": {
                "input": meta.input_tokens,
                "output": meta.output_tokens,
                "cache_read": meta.cache_read_tokens,
                "cache_write": meta.cache_write_tokens,
            },
            "files": files_summary(events),
            "note": note,
        }),
    )
}

/// A row for a session whose activity could not be measured at all: the same
/// keys a measured row carries, each explicitly `null`, plus the reason. A zero
/// here would read as "measured, and it did nothing".
fn unmeasured_activity(s: &SharedSession, note: String) -> Value {
    with_identity(
        s,
        json!({
            "provider": Value::Null,
            "counts": Value::Null,
            "tokens": Value::Null,
            "files": Value::Null,
            "note": note,
        }),
    )
}

/// The launch dirs a cwd-keyed provider matches a session by.
///
/// Mirrors the TUI's `session_candidate_dirs`: the persisted member dirs plus
/// the launch cwd `App::session_process_cwd_existing` would derive — a
/// deterministic path, so no running app (and no tmux round-trip) is needed.
fn candidate_dirs(s: &SharedSession) -> Vec<String> {
    use crate::session::activity::normalize_dir;

    let mut out: Vec<String> = Vec::new();
    let paths = s
        .worktrees
        .iter()
        .map(|w| w.worktree_path.clone())
        .chain(s.cwd.clone())
        .chain(s.workspace_dir.clone())
        .chain(default_workspace_dir(s))
        .chain(s.additional_dirs.iter().cloned());
    for p in paths {
        let n = normalize_dir(&p.to_string_lossy());
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// The id-derived symlink workspace a multi-repo session launches in, when it
/// has no user-chosen `workspace_dir`.
///
/// Without it a default multi-repo session is invisible to every cwd-keyed
/// provider: the agent's transcript records the *workspace* as its cwd, which
/// is none of the member dirs. Mirrors `App::session_process_cwd_existing`;
/// remote sessions never reach here (`activity_row` returns before this).
fn default_workspace_dir(s: &SharedSession) -> Option<std::path::PathBuf> {
    if s.workspace_dir.is_some() || member_dir_count(s) < 2 {
        return None;
    }
    crate::paths::session_workspace_dir(s.agent_session_id.as_deref()?)
}

/// How many directories this session spans, counted as `session_member_dirs`
/// does: worktrees *replace* `cwd` as members, and an `additional_dir` that is
/// already a worktree is not a second member.
fn member_dir_count(s: &SharedSession) -> usize {
    let base = if s.worktrees.is_empty() {
        usize::from(s.cwd.is_some())
    } else {
        s.worktrees.len()
    };
    let extra = s
        .additional_dirs
        .iter()
        .filter(|d| !s.worktrees.iter().any(|w| w.worktree_path == **d))
        .count();
    base + extra
}

/// The most-touched files, capped — the full stream is the F9 view's job.
fn files_summary(events: &[crate::session::activity::ActivityEvent]) -> Value {
    const TOP_FILES: usize = 10;

    let mut files = crate::session::activity::aggregate_files(events);
    files.truncate(TOP_FILES);
    Value::Array(
        files
            .into_iter()
            .map(|f| {
                json!({
                    "path": f.path,
                    "edits": f.edits,
                    "reads": f.reads,
                    "last_ts_ms": f.last_ts_ms,
                })
            })
            .collect(),
    )
}

fn render_activity(sessions: &[SharedSession], rows: &[Value]) -> String {
    if sessions.is_empty() {
        return "No sessions.".to_string();
    }
    let table_rows: Vec<Vec<String>> = sessions
        .iter()
        .zip(rows)
        .map(|(s, row)| {
            let c = &row["counts"];
            // Dashed, not zeroed: an unmeasured session reports no count at all.
            let n = |key: &str| {
                c[key]
                    .as_u64()
                    .map_or_else(|| "-".to_string(), |v| v.to_string())
            };
            vec![
                s.name.clone(),
                output::dash(row["provider"].as_str()),
                n("prompts"),
                n("commands"),
                n("edits"),
                n("reads"),
                n("subagents"),
                n("failed"),
                fmt_tokens(
                    row["tokens"]["input"].as_u64(),
                    row["tokens"]["output"].as_u64(),
                ),
                output::dash(row["note"].as_str()),
            ]
        })
        .collect();
    output::table(
        &[
            "SESSION",
            "PROVIDER",
            "TURNS",
            "CMDS",
            "EDITS",
            "READS",
            "AGENTS",
            "FAIL",
            "TOKENS I/O",
            "NOTE",
        ],
        &table_rows,
    )
}

// --- usage (account rate limits) --------------------------------------------

/// `usage` args. Account-level, so it is *not* a per-session command: the
/// account is scoped to `(agent, host)` — wherever that agent's credentials
/// live — not to any one session.
#[derive(Args, Debug)]
pub struct UsageArgs {
    /// Agent to query (claude, codex, antigravity). Defaults to all supported
    /// agents; unsupported names report a note rather than failing.
    #[arg(long)]
    pub agent: Vec<String>,
    /// Host from `hosts.toml` whose credentials to read. Defaults to this
    /// machine — a remote agent is logged in on its own host, so its usage is
    /// only readable there.
    #[arg(long)]
    pub host: Option<String>,
    /// Give up on a fetch after this many seconds. Each fetch already has its
    /// own internal bound; this caps the whole command when a host hangs.
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,
}

/// Fetch account usage windows, one row per agent.
///
/// Unlike the per-session commands this reaches the network (and, for codex,
/// spawns `codex app-server`), so it is the one metrics command with real
/// latency. Fetches run concurrently and the whole command is bounded by
/// `--timeout`.
pub fn run_usage(args: UsageArgs, _db: &Database) -> Result<CommandOutput, String> {
    let hosts = crate::agent::host_config::load_all();
    let host = match args.host.as_deref() {
        Some(name) => Some(
            hosts
                .get(name)
                .ok_or_else(|| {
                    format!(
                        "Unknown host '{name}'. Configured: {}",
                        if hosts.is_empty() {
                            "none".to_string()
                        } else {
                            hosts.names().join(", ")
                        }
                    )
                })?
                .clone(),
        ),
        None => None,
    };

    let agents: Vec<String> = if args.agent.is_empty() {
        crate::usage::supported_agents()
            .iter()
            .map(|a| (*a).to_string())
            .collect()
    } else {
        args.agent.clone()
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to start the async runtime: {e}"))?;
    let deadline = std::time::Duration::from_secs(args.timeout);
    let fetched = runtime.block_on(async {
        // Spawned (so each fetch owns its inputs and the set runs
        // concurrently), then awaited in order — three agents must not cost
        // three timeouts back to back.
        let handles: Vec<_> = agents
            .iter()
            .map(|agent| {
                let (agent, host) = (agent.clone(), host.clone());
                tokio::spawn(async move {
                    match tokio::time::timeout(deadline, crate::usage::fetch(&agent, host.as_ref()))
                        .await
                    {
                        Ok(usage) => usage,
                        Err(_) => crate::session::AgentUsage {
                            note: Some(format!("timed out after {}s", deadline.as_secs())),
                            ..Default::default()
                        },
                    }
                })
            })
            .collect();
        let mut fetched = Vec::with_capacity(handles.len());
        for handle in handles {
            fetched.push(handle.await.unwrap_or_else(|e| crate::session::AgentUsage {
                note: Some(format!("usage fetch failed: {e}")),
                ..Default::default()
            }));
        }
        fetched
    });

    let host_name = host.as_ref().map(|h| h.name.clone());
    let rows: Vec<Value> = agents
        .iter()
        .zip(&fetched)
        .map(|(agent, usage)| {
            json!({
                "agent": agent,
                "host": host_name,
                "plan": usage.plan,
                "windows": usage.windows,
                "note": usage.note,
            })
        })
        .collect();

    let human = render_usage(&rows);
    Ok(CommandOutput::new(Value::Array(rows), human))
}

fn render_usage(rows: &[Value]) -> String {
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    for row in rows {
        let agent = row["agent"].as_str().unwrap_or("?");
        let plan = output::dash(row["plan"].as_str());
        let windows = row["windows"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        if windows.is_empty() {
            table_rows.push(vec![
                agent.to_string(),
                plan,
                "-".into(),
                "-".into(),
                "-".into(),
                output::dash(row["note"].as_str()),
            ]);
            continue;
        }
        for (i, w) in windows.iter().enumerate() {
            table_rows.push(vec![
                // The agent/plan columns head their group only, so several
                // windows read as one agent's block rather than a repeat.
                if i == 0 {
                    agent.to_string()
                } else {
                    String::new()
                },
                if i == 0 { plan.clone() } else { String::new() },
                w["label"].as_str().unwrap_or("?").to_string(),
                w["used_percent"]
                    .as_f64()
                    .map_or_else(|| "-".into(), |p| format!("{p:.0}%")),
                w["resets_at"]
                    .as_u64()
                    .map_or_else(|| "-".into(), fmt_reset),
                output::dash(row["note"].as_str()),
            ]);
        }
    }
    if table_rows.is_empty() {
        return "No agents queried.".to_string();
    }
    output::table(
        &["AGENT", "PLAN", "WINDOW", "USED", "RESETS", "NOTE"],
        &table_rows,
    )
}

/// Render a reset time as a relative "in 2h41m" — an absolute epoch is
/// useless at a glance, and the JSON keeps the exact value.
fn fmt_reset(epoch_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let Some(remaining) = epoch_secs.checked_sub(now).filter(|r| *r > 0) else {
        return "now".to_string();
    };
    let (h, m) = (remaining / 3600, (remaining % 3600) / 60);
    if h > 0 {
        format!("in {h}h{m:02}m")
    } else {
        format!("in {m}m")
    }
}

// --- session resources (process tree) ---------------------------------------

/// `session resources` args: the shared target plus the opt-in CPU sample.
#[derive(Args, Debug)]
pub struct ResourceArgs {
    #[command(flatten)]
    pub target: TargetArgs,
    /// Also report per-session CPU usage. Off by default because CPU is a
    /// *rate*: it needs two samples, so this delays the command by
    /// `--cpu-sample-ms`. Memory is instantaneous and always reported.
    #[arg(long)]
    pub cpu: bool,
    /// Interval between the two CPU samples, in milliseconds. Shorter is
    /// faster but noisier.
    #[arg(long, default_value_t = 200, requires = "cpu")]
    pub cpu_sample_ms: u64,
}

/// Sum each target's agent process tree, and optionally sample its CPU.
///
/// The process table is read **once** for the whole run and indexed, so `--all`
/// costs the same single sweep as one session.
pub fn run_resources(args: ResourceArgs, db: &Database) -> Result<CommandOutput, String> {
    use crate::session::memory::{ProcTable, SessionMemory};

    let sessions = args.target.resolve(db)?;

    // Resolve every root pid first: an all-remote (or all-dead) set needs no
    // process-table read at all. One tmux call covers the whole run.
    let panes = crate::agent::tmux::agent_window_pane_pids().unwrap_or_default();
    let roots: Vec<Option<u32>> = sessions.iter().map(|s| root_pid(s, &panes)).collect();
    let table = if roots.iter().any(Option::is_some) {
        ProcTable::new(crate::proctable::read())
    } else {
        ProcTable::default()
    };
    let cpu = if args.cpu {
        sample_cpu(&roots, args.cpu_sample_ms)
    } else {
        vec![None; roots.len()]
    };

    let rows: Vec<Value> = sessions
        .iter()
        .zip(&roots)
        .zip(&cpu)
        .map(|((s, root), cpu)| {
            // An empty table means the read failed or the platform has no
            // implementation — unknown, not free. `> 1` rejects init/launchd:
            // every process descends from it, so a garbled pane pid would
            // otherwise price the whole machine as one session.
            let memory = match root {
                Some(pid) if *pid > 1 && !table.is_empty() => Some(table.subtree(*pid)),
                _ => None,
            };
            let note = resource_note(s, *root, memory.as_ref(), table.is_empty());
            let (state, rss, procs) = match memory {
                Some(SessionMemory::Live { rss_bytes, procs }) => {
                    ("live", Some(rss_bytes), Some(procs))
                }
                Some(SessionMemory::Unloaded) => ("unloaded", Some(0), Some(0)),
                None => ("unknown", None, None),
            };
            with_identity(
                s,
                json!({
                    "state": state,
                    "pid": root,
                    "rss_bytes": rss,
                    "procs": procs,
                    "cpu_percent": cpu,
                    "note": note,
                }),
            )
        })
        .collect();

    let human = render_resources(&sessions, &rows, args.cpu);
    Ok(CommandOutput::new(shape(&args.target, rows), human))
}

/// The pane's root pid, or `None` when this session can't have one locally —
/// a remote session, or a window that isn't live (an unloaded ghost).
fn root_pid(s: &SharedSession, panes: &std::collections::HashMap<String, u32>) -> Option<u32> {
    if is_remote(s) {
        return None;
    }
    panes
        .get(&crate::agent::tmux::agent_window_name(&s.name))
        .copied()
}

/// Sample CPU for the resolved roots by refreshing twice around a delay —
/// sysinfo reports usage as a delta between refreshes, so a single refresh
/// would report 0 for every process.
///
/// Only the root process is sampled, matching the TUI's `session_cpu_percent`
/// (its memory counterpart sums the tree; its CPU does not).
fn sample_cpu(roots: &[Option<u32>], sample_ms: u64) -> Vec<Option<f32>> {
    let pids: Vec<sysinfo::Pid> = roots
        .iter()
        .flatten()
        .map(|p| sysinfo::Pid::from_u32(*p))
        .collect();
    if pids.is_empty() {
        return vec![None; roots.len()];
    }
    let kind = sysinfo::ProcessRefreshKind::nothing().with_cpu();
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&pids), false, kind);
    std::thread::sleep(std::time::Duration::from_millis(sample_ms));
    sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&pids), false, kind);
    roots
        .iter()
        .map(|root| {
            let pid = sysinfo::Pid::from_u32((*root)?);
            sys.process(pid).map(sysinfo::Process::cpu_usage)
        })
        .collect()
}

fn resource_note(
    s: &SharedSession,
    root: Option<u32>,
    memory: Option<&crate::session::memory::SessionMemory>,
    table_empty: bool,
) -> Option<&'static str> {
    if is_remote(s) {
        return Some("remote session: its process tree lives on the host");
    }
    if root.is_none() {
        return Some("no live tmux pane (unloaded, or the window is gone)");
    }
    if memory.is_none() && table_empty {
        return Some("process table unavailable on this platform");
    }
    match memory {
        Some(crate::session::memory::SessionMemory::Unloaded) => Some("pane process has exited"),
        _ => None,
    }
}

fn render_resources(sessions: &[SharedSession], rows: &[Value], with_cpu: bool) -> String {
    if sessions.is_empty() {
        return "No sessions.".to_string();
    }
    let mut headers: Vec<&str> = vec!["SESSION", "STATE", "PID", "RSS", "PROCS"];
    if with_cpu {
        headers.push("CPU");
    }
    headers.push("NOTE");
    let table_rows: Vec<Vec<String>> = sessions
        .iter()
        .zip(rows)
        .map(|(s, row)| {
            let mut cells = vec![
                s.name.clone(),
                row["state"].as_str().unwrap_or("unknown").to_string(),
                row["pid"]
                    .as_u64()
                    .map_or_else(|| "-".into(), |p| p.to_string()),
                row["rss_bytes"].as_u64().map_or_else(|| "-".into(), bytes),
                row["procs"]
                    .as_u64()
                    .map_or_else(|| "-".into(), |p| p.to_string()),
            ];
            if with_cpu {
                cells.push(
                    row["cpu_percent"]
                        .as_f64()
                        .map_or_else(|| "-".into(), |c| format!("{c:.1}%")),
                );
            }
            cells.push(output::dash(row["note"].as_str()));
            cells
        })
        .collect();
    output::table(&headers, &table_rows)
}

/// Human byte size, matching the info panel's scale.
fn bytes(n: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let mb = n as f64 / MB;
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(uuid: Option<&str>, all: bool) -> TargetArgs {
        TargetArgs {
            uuid: uuid.map(str::to_string),
            all,
        }
    }

    #[test]
    fn target_rejects_both_and_neither() {
        let db = Database::open_in_memory().unwrap();
        assert!(target(Some("x"), true).resolve(&db).is_err());
        assert!(target(None, false).resolve(&db).is_err());
    }

    #[test]
    fn target_rejects_a_malformed_uuid() {
        let db = Database::open_in_memory().unwrap();
        let err = target(Some("not-a-uuid"), false).resolve(&db).unwrap_err();
        assert!(err.contains("Invalid session UUID"), "got {err}");
    }

    #[test]
    fn shape_is_an_array_for_all_and_an_object_for_one() {
        let rows = vec![json!({ "a": 1 })];
        assert!(shape(&target(None, true), rows.clone()).is_array());
        assert_eq!(shape(&target(Some("x"), false), rows)["a"], 1);
    }

    #[test]
    fn bytes_scales_to_gb() {
        assert_eq!(bytes(333 * 1024 * 1024), "333 MB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn cpu_sampling_is_skipped_when_no_root_resolves() {
        // No pids to sample: the delay must not be paid, and every slot is
        // still accounted for.
        let started = std::time::Instant::now();
        let sampled = sample_cpu(&[None, None], 5_000);
        assert_eq!(sampled.len(), 2);
        assert!(sampled.iter().all(Option::is_none));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "sample_cpu slept with nothing to sample"
        );
    }

    #[test]
    fn compact_and_pair_formatting() {
        assert_eq!(compact(900), "900");
        assert_eq!(compact(15_200), "15.2k");
        assert_eq!(compact(1_500_000), "1.5M");
        assert_eq!(fmt_tokens(None, None), "-");
        // One side reported is still a pair — the other is a real zero.
        assert_eq!(fmt_tokens(Some(1_200), None), "1.2k/0");
        assert_eq!(fmt_churn(None, None), "-");
        assert_eq!(fmt_churn(Some(156), Some(23)), "+156/-23");
    }
}
