pub(crate) mod activity;
mod automation;
mod automation_state;
mod background;
pub(crate) mod cc_activity;
pub(crate) mod cc_import;
mod clipboard;
pub(crate) mod clock;
pub(crate) mod code_review;
mod config_reload;
mod helpers;
mod key_handlers;
mod memory;
pub(crate) mod metrics_state;
pub(crate) mod modals;
mod new_session_state;
mod notify_state;
pub(crate) mod search;
mod state;
mod sync_state;
mod task_state;
mod tasks;
mod view;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::{Position, Rect},
    widgets::{Block, Borders},
};
use tracing::{debug, error, info, warn};

use crate::agent::{AgentProvider, BackendRegistry, GenericProvider, Session, SessionBackend};
use crate::git;
use crate::session::{
    AgentDef, AgentRegistry, SessionConfig, SessionId, SessionInfo, SessionStatus, WorktreeInfo,
    DEFAULT_AGENT_NAME,
};

use crate::storage::Database;
use crate::storage::DeletedSessionInfo;
use crate::sync::{self, SharedWorktree, StateDelta, SyncState};
use crate::ui::layout;
use crate::ui::scrollbar::ScrollbarGeom;
use crate::ui::selection::{PaneBounds, Selection, TermPos};
use notify_state::NotificationState;

const MOUSE_SCROLL_LINES: usize = 3;

/// How long the user has to press Ctrl+Z to undo a session delete.
const UNDO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a status-bar message is shown before reverting to default counts.
const STATUS_MESSAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Ticks per `Working`-spinner frame. The loop ticks ~every 10 ms, so 12 ticks
/// ≈ 125 ms/frame ≈ 8 fps — a smooth spinner without thrashing the renderer.
const SPINNER_TICKS_PER_FRAME: u64 = 12;

/// Upper bound between forced repaints when nothing else marked the UI dirty.
/// The render loop only paints when state changed (a key, agent output, a
/// background poll landing) — this floor guarantees time-driven UI that nothing
/// explicitly flags (the live clock/metrics, cursor blink, a session going
/// quiet `Busy → Waiting`, an expiring status toast) still refreshes promptly.
/// 250 ms ≈ 4 fps when idle, vs. the old unconditional ~100 fps. See
/// `docs/PERFORMANCE.md`.
const FORCE_REDRAW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Slow-op thresholds (ms): a named synchronous UI-thread operation at or
/// above `RECORD` lands in the slow-op ring (HUD / perf-window line); at or
/// above `WARN` it also gets a `tracing::warn!` — a visible stall (multiple
/// dropped frames) worth a log entry even when nobody is watching the HUD.
const SLOW_OP_RECORD_MS: u64 = 5;
const SLOW_OP_WARN_MS: u64 = 100;

/// Ticks (~10 ms each) per steady-state perf report window (~10 s): under
/// `FRIRING_PERF_LOG` each window emits one `perf_window` log line (counter
/// deltas + timing percentiles) and refreshes the published snapshot.
/// Tick-based so tests can drive windows without a wall clock.
const PERF_WINDOW_TICKS: u64 = 1_000;

/// Ticks between perf-snapshot publishes while the perf HUD is open (~5 s).
/// Snapshot writes bump other connections' `data_version`, so this stays
/// coarse and only runs while someone is actually looking at perf data.
const PERF_SNAPSHOT_TICKS: u64 = 500;

/// Ticks (~10 ms each) between the status refresh's `PRAGMA data_version`
/// reads (~100 ms). Bounds an external `session signal`'s worst-case display
/// latency while cutting the per-tick rusqlite round-trip 10× (ADR-P10);
/// own-connection writes bypass the throttle via cache invalidation.
const HOOK_VERSION_CHECK_TICKS: u64 = 10;

/// Tick delay before sending Enter after pasting text into a session.
/// At ~10ms per tick, 10 ticks ≈ 100ms — enough for the app to process the pasted text.
const DEFERRED_INPUT_DELAY_TICKS: u64 = 10;

/// Cadence of the crash-safety ghost-frame debounce (~60 s at the 10 ms tick):
/// frequent enough that a reboot's ghosts look current, rare enough that the
/// in-memory serialization + small DB writes never register.
const FRAME_PERSIST_INTERVAL_TICKS: u64 = 6_000;

/// Nominal milliseconds per tick, for converting a wall-clock delay (a prompt
/// step's settle time) into the tick offsets `deferred_inputs` schedules on.
const TICK_MS: u64 = 10;

/// How often to refresh system metrics (in ticks). At ~10ms per tick, 100 ≈ 1 second.
const METRICS_REFRESH_TICKS: u64 = 100;

/// How often to scan each local session's Claude Code `subagents/` tree for the
/// activity view (in ticks, ~1 s). The scan is stat-gated: an unchanged tree
/// skips the JSONL parse, so idle sessions stay cheap.
const CC_REFRESH_TICKS: u64 = 100;

/// How often to tail each local session's agent activity sources (in ticks,
/// ~1 s), offset half a cadence from [`CC_REFRESH_TICKS`] so the two scans
/// never land on the same tick. Also stat-gated.
const ACTIVITY_REFRESH_TICKS: u64 = 100;

/// How often to price each local session's agent process tree (in ticks,
/// ~3 s). Deliberately slower than the ~1 s scans: the pass reads the whole
/// process table (a procfs walk, or a `ps` fork on macOS) and a session's
/// footprint moves on the scale of seconds, not frames. Offset half a cadence
/// so it never lands on the tick the 1 s scans share.
const MEMORY_REFRESH_TICKS: u64 = 300;

/// How often to refresh git stats for the active session (in ticks). Git stats
/// shell out to `git`, so they run on a slower cadence than other metrics
/// (~5 s) and only for the visible session.
const GIT_REFRESH_TICKS: u64 = 500;

/// Ticks (~10 ms each) between config-file mtime polls (~1 s). Cheap: two
/// `stat` calls per poll.
const CONFIG_RELOAD_TICKS: u64 = 100;

/// How often to refresh account usage / rate-limits (in ticks). At ~10ms per
/// tick, 30000 ≈ 5 minutes. Usage windows are coarse and fetching hits the
/// network, so this is deliberately slow; fires once early then every 5 min.
const USAGE_REFRESH_TICKS: u64 = 30_000;

/// Cache key for account usage: `(agent name, host name)` (`None` = local).
/// The credential source — and therefore the account — is the machine the
/// agent process runs on, so two sessions with the same agent on different
/// hosts get separate entries while same-host sessions share one fetch.
pub(crate) type UsageKey = (String, Option<String>);

/// One planned account-usage refresh from [`App::plan_usage_fetches`]: either
/// a real background fetch, or a static note for a scope that can't be
/// fetched right now (host unreachable / missing from `hosts.toml`).
#[derive(Debug)]
enum UsageFetchPlan {
    /// Spawn [`crate::usage::fetch`] against these credentials
    /// (`None` host = local).
    Fetch(Option<crate::session::HostDef>),
    /// Don't spawn anything; show this note **unless** an earlier fetch
    /// already cached real data (last-known usage beats a transient outage).
    Unavailable(&'static str),
}

/// Prepared inputs for a `Session::spawn`, produced on the UI thread by
/// [`App::build_spawn_inputs`] and consumed either inline (synchronous spawn)
/// or moved into a blocking task (interactive spawn).
struct SpawnInputs {
    /// Process-launch config (its `cwd` is the symlink workspace for multi-repo
    /// sessions; the primary repo is carried separately).
    config: SessionConfig,
    /// The primary repo path restored onto `SessionInfo.cwd` after spawn.
    primary_cwd: Option<PathBuf>,
    /// The user-chosen workspace dir, only when it actually became the launch
    /// cwd — persisted onto `SessionInfo.workspace_dir` so restart / shell pane
    /// / delete resolve the same directory.
    workspace_dir: Option<PathBuf>,
    backend: Arc<dyn SessionBackend>,
    provider: Arc<dyn crate::agent::AgentProvider>,
    rows: u16,
    cols: u16,
}

/// Continuation for a backgrounded interactive `Session::spawn`: the metadata
/// and follow-up actions applied once the session is live (in
/// [`App::poll_session_spawn`]).
struct PendingSessionSpawn {
    primary_cwd: Option<PathBuf>,
    worktrees: Vec<WorktreeInfo>,
    additional_dirs: Vec<PathBuf>,
    /// User-chosen workspace dir that became the launch cwd (see
    /// [`SpawnInputs::workspace_dir`]).
    workspace_dir: Option<PathBuf>,
    /// Parent session (lead/worker linkage), captured at kickoff like the
    /// other wizard state so an overlapping flow can't steal it.
    parent_session_id: Option<SessionId>,
    /// A task-initiated spawn's `(task_id, title)`, captured at kickoff so the
    /// prompt is delivered + the task advanced when the session comes up.
    task_prompt: Option<(i64, String)>,
    /// Agent name for the spawn, captured so a failure toast names the real
    /// agent (codex/aider/…) rather than hardcoding "claude".
    agent: String,
    /// Base branch the worktree was forked from (worktree spawns only),
    /// persisted once the session is live so the code-review view can scope its
    /// diff to `<base>..HEAD`. `None` for bare-repo / fork spawns.
    base_branch: Option<String>,
}

/// One remote backend's discovery result: its `backend_type`, whether the host
/// was **reachable** (its `ensure_ready` succeeded — distinguishes "host down"
/// from "host up but no windows"), plus the windows it reported. Sent once per
/// discovery attempt by the restore threads.
type RemoteDiscovery = (String, bool, Vec<crate::agent::backend::DiscoveredSession>);

/// How long to wait between retry sweeps for a still-unreachable remote backend.
const REMOTE_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(20);

/// Total budget for tearing down every backend's control-mode connection at
/// quit (see [`App::shutdown_backends`]). Shared across the concurrent
/// teardowns, not per backend. Generous relative to the ~50 ms a healthy
/// connection needs — it is a backstop against a wedged transport hanging the
/// process, not the expected cost.
const BACKEND_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Background restore + reconnect loop for remote-backed sessions. Startup
/// readies + discovers only *local* backends synchronously; each remote
/// (`ssh:`/`wsl:`) backend is readied on its own thread, because a single ssh
/// connect can take seconds (fail-fast is bounded by
/// [`crate::shell::SSH_HARDENING_OPTS`]) and must never block the first frame.
///
/// Every remote session is inserted as a **placeholder** row up front (see
/// [`crate::agent::backend::Session::placeholder`]) so it always shows in the
/// list, tagged `SessionStatus::Unreachable`. Each discovery thread sends one
/// [`RemoteDiscovery`]; on success the placeholder is replaced in place by the
/// real adopted session, and on failure it stays and the backend is retried
/// every [`REMOTE_RETRY_INTERVAL`]. This struct lives as long as any backend
/// still has placeholder rows awaiting adoption.
struct RemoteRestore {
    rx: mpsc::Receiver<RemoteDiscovery>,
    /// Kept so retry sweeps can re-spawn discovery threads on the same channel.
    tx: mpsc::Sender<RemoteDiscovery>,
    /// Sessions still awaiting adoption, keyed by `backend_type`.
    pending: HashMap<String, Vec<sync::SharedSession>>,
    /// Backends with a discovery thread currently running (don't double-spawn).
    inflight: std::collections::HashSet<String>,
    /// When the next retry sweep for still-pending backends is due.
    next_retry_at: std::time::Instant,
    /// Backends already surfaced as unreachable via a toast (dedup so the retry
    /// loop doesn't re-toast every sweep).
    notified_unreachable: std::collections::HashSet<String>,
    perf_log: bool,
}

impl RemoteRestore {
    /// An empty restore with its own discovery channel and the first retry due
    /// at `next_retry_at`. Callers fill `pending`/`inflight` and spawn discovery
    /// threads on `tx`.
    fn new(perf_log: bool, next_retry_at: std::time::Instant) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rx,
            tx,
            pending: HashMap::new(),
            inflight: std::collections::HashSet::new(),
            next_retry_at,
            notified_unreachable: std::collections::HashSet::new(),
            perf_log,
        }
    }
}

/// Continuation for a backgrounded worktree-creation: the wizard inputs needed
/// to resume the spawn flow once the worktrees exist (in
/// [`App::poll_worktree_create`]).
struct PendingWorktreeCreate {
    /// Chosen backend (`ssh:<host>` or `None` for local).
    backend: Option<String>,
    /// Plain (non-worktree) repos to attach alongside the worktree repos.
    normal_repos: Vec<PathBuf>,
    /// Session name when already known (worktree flow); `None` routes through
    /// the name modal.
    session_name: Option<String>,
    /// Base branch the worktrees were forked from, carried through to the spawn
    /// so it can be persisted for the code-review view.
    base_branch: String,
    /// Progress of the agent picker that overlaps the creation (ADR-P12).
    agent_pick: AgentPick,
}

/// Progress of the agent picker opened *while* the worktrees are still being
/// created (ADR-P12) — the two run concurrently, and whichever finishes last
/// triggers the spawn.
enum AgentPick {
    /// No overlapping picker (≤1 agent, or a flow without a session name):
    /// `continue_worktree_spawn` routes through the classic picker-after-create
    /// path.
    NotOpened,
    /// The picker is open; the user hasn't chosen yet. A finished create
    /// stashes its config for `confirm_agent_picker` to consume.
    Open,
    /// The user chose this agent before the create finished; spawn immediately
    /// on delivery.
    Chosen(String),
    /// The user cancelled the picker mid-create: drop the delivered worktrees
    /// (they stay on disk, matching a cancel after creation).
    Cancelled,
}

/// Create one worktree per repo off the UI thread, rolling back any already
/// created if a later one fails. Returns the worktree infos in `repo_paths`
/// order, or a formatted error after rollback.
fn create_worktrees(
    host: Option<&crate::session::HostDef>,
    repo_paths: &[PathBuf],
    new_branch: &str,
    base_branch: &str,
) -> Result<Vec<WorktreeInfo>, String> {
    let mut worktree_infos: Vec<WorktreeInfo> = Vec::new();
    for repo_path in repo_paths {
        // Multi-repo spawn: the chosen base comes from the *primary* repo's
        // branch list and may not exist in an extra repo. Fall back to that
        // repo's own default branch — mirroring the headless `--add-repo
        // PATH[@BASE]` model where each repo resolves its own base — instead
        // of failing (and rolling back) the whole spawn.
        let repo_base = if git::branch_exists_on(host, repo_path, base_branch) {
            base_branch.to_string()
        } else {
            let branches = git::list_branches_on(host, repo_path).unwrap_or_default();
            match git::default_branch_on(host, repo_path, &branches) {
                Some(fallback) => {
                    tracing::info!(
                        "base '{base_branch}' not found in {}; forking its worktree \
                         from the repo's default branch '{fallback}'",
                        repo_path.display()
                    );
                    fallback
                }
                // No resolvable default: keep the original base so the error
                // below names the branch the user actually picked.
                None => base_branch.to_string(),
            }
        };
        match git::create_worktree_on(host, repo_path, new_branch, &repo_base) {
            Ok(worktree_path) => worktree_infos.push(WorktreeInfo {
                repo_path: repo_path.clone(),
                worktree_path,
                branch: new_branch.to_string(),
            }),
            Err(e) => {
                // Roll back already-created worktrees before bailing.
                for info in &worktree_infos {
                    if let Err(re) =
                        git::remove_worktree_on(host, &info.repo_path, &info.worktree_path)
                    {
                        error!("Failed to roll back worktree: {re}");
                    }
                }
                error!("Failed to create worktree in {}: {e}", repo_path.display());
                return Err(format!("{e:#}"));
            }
        }
    }
    Ok(worktree_infos)
}

/// Result of a background system-metrics refresh, delivered via `App::metrics_refresh`.
struct MetricsRefresh {
    /// The sysinfo collector, returned so it retains CPU-delta state across
    /// refreshes (it is moved into the worker for the duration).
    sys: sysinfo::System,
    /// Aggregate machine + active-session metrics for the info panel.
    metrics: crate::ui::info_panel::SystemMetrics,
    /// Per-session agent metrics parsed from statusline JSON files.
    agent_metrics: Vec<(SessionId, crate::session::AgentMetrics)>,
}

/// Collect machine + active-session + per-agent metrics off the UI thread.
///
/// Owns `sys` for the duration (CPU deltas need a persistent collector) and
/// returns it so the caller can move it back. `active` is the active session's
/// `(backend, backend_id)` for the PID lookup (a control-mode round-trip);
/// `metrics_files` pairs each session id with its statusline JSON path.
fn collect_system_metrics(
    mut sys: sysinfo::System,
    active: Option<(Arc<dyn SessionBackend>, String)>,
    metrics_files: Vec<(SessionId, PathBuf)>,
) -> MetricsRefresh {
    sys.refresh_cpu_all();
    sys.refresh_memory();

    let cpu_percent = sys.global_cpu_usage();
    let memory_used = sys.used_memory();
    let memory_total = sys.total_memory();

    // Resolve the active session's root PID and sample its CPU. Its *memory*
    // comes from the per-session scan instead (`app::memory`), which sums the
    // whole agent process tree rather than this one process.
    let session_cpu_percent = active
        .and_then(|(backend, id)| backend.pane_pid(&id).ok().flatten())
        .map(|pid| {
            let pid = sysinfo::Pid::from_u32(pid);
            let kind = sysinfo::ProcessRefreshKind::nothing().with_cpu();
            sys.refresh_processes_specifics(sysinfo::ProcessesToUpdate::Some(&[pid]), false, kind);
            sys.process(pid).map(|p| p.cpu_usage()).unwrap_or(0.0)
        })
        .unwrap_or(0.0);

    let metrics = crate::ui::info_panel::SystemMetrics {
        cpu_percent,
        memory_used,
        memory_total,
        session_cpu_percent,
    };

    // Poll agent metrics files written by the statusline script.
    let mut agent_metrics = Vec::new();
    for (session_id, path) in metrics_files {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&content) {
                agent_metrics.push((
                    session_id,
                    crate::session::AgentMetrics::from_statusline_json(&raw),
                ));
            }
        }
    }

    MetricsRefresh {
        sys,
        metrics,
        agent_metrics,
    }
}

/// Aggregate git stats (diff + dirty + ahead/behind) across a session's
/// worktree paths. Shells out to `git` per path, so it runs off the UI thread.
fn aggregate_git_stats(paths: &[PathBuf]) -> Option<crate::session::GitStats> {
    let mut agg: Option<crate::session::GitStats> = None;
    for path in paths {
        if let Some(stats) = crate::git::worktree_stats(path) {
            let acc = agg.get_or_insert_with(Default::default);
            acc.files_changed += stats.files_changed;
            acc.insertions += stats.insertions;
            acc.deletions += stats.deletions;
            acc.dirty |= stats.dirty;
            acc.ahead += stats.ahead;
            acc.behind += stats.behind;
        }
    }
    agg
}

pub use modals::{AutomationActionKind, AutomationField, TaskField, TriggerKind};

/// Ticks (~10 ms each) to wait after spawning a session before pasting its
/// automation prompt, giving the agent CLI time to come up (~3 s).
const AGENT_BOOT_DELAY_TICKS: u64 = 300;

/// Everything [`App::spawn_and_prompt`] needs to spawn (or reuse) a named
/// session and deliver its prompt steps.
pub(crate) struct SpawnPromptRequest<'a> {
    /// Session name; an existing session of this name is reused.
    pub name: String,
    pub repo_path: &'a std::path::Path,
    /// `None` = run in the repo root; `Some` = create/attach a worktree branch.
    pub worktree_branch: Option<&'a str>,
    /// Base branch for a new worktree (default `main`).
    pub base_branch: Option<&'a str>,
    /// Agent name; `None` = registry default.
    pub agent: Option<&'a str>,
    /// `hosts.toml` host to spawn on; `None` = local.
    pub host: Option<&'a str>,
    pub extra_repos: &'a [crate::session::ExtraRepo],
    /// Ordered prompt steps, each delivered as its own paste + Enter.
    pub steps: &'a [crate::session::PromptStep],
}

pub enum AppMessage {
    KeyPress(KeyCode, KeyModifiers),
    /// Alt pressed (`true`) / released (`false`) — kitty-protocol modifier
    /// key events (legacy terminals never produce this). Drives the
    /// session-jump number overlay; see `App::set_alt_held`.
    AltHeld(bool),
    /// Text pasted via the terminal's bracketed paste mode.
    Paste(String),
    /// Mouse wheel up/down, carrying the cursor position so the scroll can be
    /// routed to whichever pane is under the cursor.
    MouseScrollUp {
        x: u16,
        y: u16,
    },
    MouseScrollDown {
        x: u16,
        y: u16,
    },
    MouseClick {
        x: u16,
        y: u16,
        modifiers: KeyModifiers,
    },
    MouseDrag {
        x: u16,
        y: u16,
    },
    MouseUp {
        x: u16,
        y: u16,
    },
    /// Pointer moved with no button held — tracked for hover highlighting.
    MouseMove {
        x: u16,
        y: u16,
    },
    Resize(u16, u16),
    ExternalStateChange(StateDelta),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    Info,
    Success,
    Error,
}

/// Which mechanism served a clipboard copy (see [`App::set_clipboard_text`]).
/// The raw OSC 52 path is fire-and-forget — the terminal never acknowledges
/// it — so its toasts carry a marker; the native and tmux paths report a real
/// success (a returned `Ok`/exit status), so they read as a plain "copied".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardVia {
    Native,
    Tmux,
    Osc52,
}

impl ClipboardVia {
    /// The success toast for a copy served this way.
    pub(crate) fn toast(self, msg: &str) -> String {
        match self {
            ClipboardVia::Native | ClipboardVia::Tmux => msg.into(),
            ClipboardVia::Osc52 => format!("{msg} (OSC 52)"),
        }
    }
}

/// What became of the native clipboard write before a fallback ran — recorded
/// so [`App::clipboard_error`] can explain a fallback failure honestly rather
/// than always implying native was tried (see [`App::set_clipboard_text`]).
enum NativeCopy {
    /// Deliberately not attempted: over SSH the native clipboard wouldn't reach
    /// the user — it's the *host's* on macOS, or absent on a display-less Linux
    /// host (`clipboard::native_clipboard_is_remote`).
    Skipped,
    /// Attempted, but the display-server write errored.
    Failed(String),
    /// No native handle at all — no reachable display server.
    Unavailable,
}

/// Which scroll state a rendered scrollbar drives. Recorded per-frame in
/// [`App::scrollbar_hits`] so mouse clicks/drags on a track can be routed back
/// to the right pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollTarget {
    Terminal,
    TaskPreview,
    FileViewer,
    RunHistory,
    /// The code-review view's diff scrollbar.
    CodeReview,
    /// The activity view's transcript scrollbar — position is the selection.
    CcActivity,
    /// The active modal's list scrollbar — position is the selection index.
    Modal,
}

/// One scrollbar rendered this frame: its geometry plus the scroll state it
/// drives. Built in [`App::view`], hit-tested by the mouse handlers.
pub(crate) struct ScrollbarHit {
    pub(crate) geom: ScrollbarGeom,
    pub(crate) target: ScrollTarget,
}

/// What a left click on a recorded screen region does. Recorded per-frame in
/// [`App::click_targets`] (mirroring [`App::scrollbar_hits`]) so the mouse
/// handler can route clicks to rows and panes without re-deriving the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClickAction {
    /// Select the session at this display-order index (resolved through
    /// `render_order_indices()` at click time, like `Ctrl+J`/`Ctrl+K`).
    SelectSession(usize),
    /// Select the task at this index in the filtered tasks panel.
    SelectTask(usize),
    /// Select the automation at this index in the automations pane.
    SelectAutomation(usize),
    /// Select + activate the file-viewer row at this flattened tree index
    /// (expand/collapse a directory, open a file).
    SelectFileRow(usize),
    /// Focus the pane — the whole-rect fallback recorded after row targets.
    FocusPane(InputFocus),
    /// Select + activate the row in the active modal's list.
    ModalRow(usize),
    /// A global footer button — dispatches `Action` (Help/Settings/Theme/Quit)
    /// exactly as if its bound key were pressed. Only live when no modal is
    /// open (a modal swallows every click).
    Global(crate::session::Action),
    /// A modal footer button — replays a synthesized key through the open
    /// modal's own handler (Save→Enter/^S, Cancel→Esc, …) so the side effects
    /// match the keyboard path. Dispatched by `handle_modal_click`.
    ModalButton { code: KeyCode, mods: KeyModifiers },
    /// Select the index-th field of the active **editor modal** (Settings /
    /// Automation editor) — `index` is its position in that modal's visible
    /// field order. Dispatched by `handle_modal_click` → `select_modal_field`.
    ModalField(usize),
    /// Focus the conversation picker's `Search`/`Dir` sub-area (its editable
    /// fields).
    ConvoFocus(cc_import::ConversationPickerFocus),
    /// Focus an **in-pane editor** (automation / task) and select its index-th
    /// visible field. Dispatched by `activate_click_target`.
    PaneField { focus: InputFocus, index: usize },
    /// Select the code-review row at this index in `code_review.rows`.
    ReviewRow(usize),
    /// A code-review footer button.
    ReviewButton(code_review::ReviewButton),
    /// Jump the diff to the changed-file at this diff-file index (clicked in the
    /// changed-files list).
    ReviewFile(usize),
    /// Select the review-target-picker entry at this index (clicked while the
    /// picker is open).
    ReviewTarget(usize),
    /// Jump the activity tree to the node at this `state.tree` index (clicked in
    /// the tree column).
    CcActivityNode(usize),
    /// Select the activity transcript row at this `state.rows` index.
    CcActivityRow(usize),
    /// Select a central-pane view from the tab strip in the pane's top border
    /// (Agent / Shell / Review). Dispatched by `activate_click_target`.
    CentralTab(CentralTab),
    /// Copy the current status-bar message to the clipboard (click the status
    /// row). Dispatched by `activate_click_target`.
    CopyStatus,
}

/// One clickable region rendered this frame: its rect plus what a click on it
/// does. First recorded match wins, so rows are pushed before their pane's
/// whole-rect `FocusPane` fallback.
pub(crate) struct ClickTarget {
    pub(crate) rect: Rect,
    pub(crate) action: ClickAction,
}

/// A scrollable pane identified by hit-testing the cursor against the layout,
/// used to route a mouse-wheel tick to the pane under the cursor. Broader than
/// [`ScrollTarget`] because the wheel also scrolls the selection-driven list
/// panes (which have no draggable scrollbar of their own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollPane {
    Terminal,
    TaskPreview,
    FileViewer,
    RunHistory,
    SessionList,
    TasksList,
    Automations,
    CodeReview,
    /// The changed-files list shown in the file-viewer column during a review.
    ReviewFiles,
    /// The activity view's transcript (central pane).
    CcActivity,
    /// The activity view's tree shown in the file-viewer column.
    CcActivityTree,
}

#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub text: String,
    pub level: StatusLevel,
    pub created_at: std::time::Instant,
}

/// A pending `$VISUAL`/`$EDITOR` round-trip (the review's `E`). The app can't
/// run the editor itself — the main loop owns the terminal — so it queues this
/// request; the loop takes it ([`App::take_pending_editor`]), tears the TUI
/// down, runs the editor to completion, rebuilds the terminal, and reports
/// back via [`App::editor_closed`].
pub struct EditorRequest {
    pub program: String,
    pub args: Vec<String>,
    /// Reload the review diff after the editor exits — set for the Working
    /// target only, where the edit changes what the diff shows.
    pub reload_review: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFocus {
    SessionList,
    /// The automations pane beneath the session list (selecting an automation).
    Automations,
    /// Editing the scoped automation in the central pane (like a session's
    /// terminal — reached with `Enter`/`Ctrl+L` from the automations pane).
    AutomationEditor,
    /// Browsing the scoped automation's run history (beneath the editor),
    /// reached with `Ctrl+L` from the editor. `j`/`k` select a run; `r` triggers
    /// a fresh run.
    AutomationRunHistory,
    /// The tasks panel on the right (selecting/acting on a task).
    TaskList,
    /// Editing the scoped task in the central pane (like a session's terminal —
    /// reached with `Enter`/`e` from the tasks panel; `Esc` returns to it).
    TaskEditor,
    /// The centered global-search popup (`Ctrl+/` or double-`Shift`).
    /// Captures all input while active; entered/left only via its keybinding /
    /// `Esc`.
    GlobalSearch,
    Terminal,
    FileViewer,
    /// The native code-review view occupying the central pane (toggled like the
    /// shell). Captures keys for its own navigation / commenting.
    CodeReview,
    /// The review's **changed-files list** in the file-viewer column (the
    /// navigation aid shown while a review is open). Focusable like the file
    /// viewer: `j`/`k` walk the files (the diff follows), `Enter` drops into the
    /// diff at the selected file, `r`/`R` toggle reviewed.
    ReviewFiles,
    /// The agent activity view's **content pane** (the selected section's
    /// event list, an agent transcript, or a workflow overview) in the central
    /// pane (toggled like the review). Captures keys for scrolling + folding.
    /// The `Cc` prefix is historical — the view is agent-neutral.
    CcActivity,
    /// The activity view's **navigator** in the file-viewer column: the six
    /// sections (Overview/Timeline/Commands/Files/Web/Agents) with the Claude
    /// workflow/subagent tree nested under Agents. Focusable like
    /// `ReviewFiles`: `j`/`k` browse (the content follows), `Enter`/`l` drops
    /// into the content pane.
    CcActivityTree,
}

/// Which pane the terminal view is showing for a given session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalView {
    Claude,
    Shell,
}

/// How the blocked-only jump overlay (`Alt+A`) was entered, which decides how
/// it is dismissed. `Held` = entered while Alt was down (kitty-protocol
/// terminals): the Alt release dismisses it, like letting go of a modifier.
/// `Sticky` = entered by a tap (legacy terminals, where modifier state is
/// invisible): it stays until a digit jump, `Esc`, the toggle chord, or any
/// other key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttentionJumpMode {
    Held,
    Sticky,
}

/// The three mutually-exclusive central-pane views, surfaced as a clickable tab
/// strip in the pane's top border. `Agent`/`Shell` map to [`TerminalView`];
/// `Review` is the native code-review overlay. A tab click *selects* the view
/// (see `App::select_central_tab`), unlike the keyboard toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CentralTab {
    Agent,
    Shell,
    Review,
    /// The agent activity view (per-session retrospective across agent CLIs;
    /// `Cc` prefix historical).
    CcActivity,
}

/// Holds a recently deleted session for undo (Ctrl+Z) support.
struct PendingDelete {
    session: Session,
    session_id: SessionId,
    created_at: std::time::Instant,
}

/// The TEA model: owns all session/UI state and coordinates side effects.
/// A terminal editor invocation staged for the main loop to run with a real
/// TTY (a `tmux display-popup` inside tmux, or a TUI suspend-and-resume
/// outside). Built by `helpers::classify_editor` when `Ctrl+O` resolves to a
/// terminal editor (vim, nano, `ttt`, …) and drained from `App` by `run_loop`.
/// GUI editors never go through this — they stay detached.
#[derive(Debug, Clone)]
pub struct EditorInvocation {
    /// Editor binary (first whitespace token of the configured command).
    pub program: String,
    /// Extra editor args followed by the paths to open, in order.
    pub args: Vec<String>,
}

pub struct App {
    pub(crate) sessions: Vec<Session>,
    pub(crate) active_index: usize,
    /// The session that was active before the last deliberate switch, for the
    /// `LastSession` toggle (tmux `last-window`). By id, not index — deletes
    /// shift indices; a stale id is dropped lazily by the toggle. Recorded by
    /// [`Self::set_active_index`]; bookkeeping moves (restore reshuffles,
    /// delete clamps, search previews) bypass it on purpose.
    last_active_session: Option<SessionId>,
    /// Every session ever activated, most-recently-active first (front = the
    /// session showing now). Powers the switcher's no-query list and its
    /// recency tiebreak. Fed by the same *deliberate*-switch rule as
    /// [`Self::last_active_session`], so a live search preview arrowing past a
    /// session never promotes it. In-memory: a restart starts from render order.
    session_mru: Vec<SessionId>,
    /// Whether the Alt key is currently held (kitty-protocol modifier events;
    /// always `false` on legacy terminals). See [`Self::set_alt_held`].
    alt_held: bool,
    /// When Alt went down — the jump numbers appear only after
    /// [`JUMP_OVERLAY_DELAY_MS`] so pass-through Alt chords (readline
    /// `M-b`/`M-f`) don't flash them.
    alt_held_since: Option<std::time::Instant>,
    /// Whether the redraw for the delay elapsing was already requested
    /// (the overlay appears on a timer, not an input event — see
    /// [`Self::tick_jump_overlay`]).
    alt_overlay_redraw_requested: bool,
    /// The attention-only jump overlay (`Alt+A`), when open.
    attention_jump: Option<AttentionJumpMode>,
    /// The label-jump overlay (`Alt+G` / `<leader> A`), when open.
    pub(crate) label_jump: Option<LabelJump>,
    /// Repo groups collapsed in the session list, by
    /// [`group_key`](crate::ui::project_list::group_key). Persisted (DB
    /// metadata) — folding is how a long list gets curated, which is worth
    /// setting up once rather than every launch.
    pub(crate) folded_groups: std::collections::HashSet<String>,
    /// Whether unloaded sessions are folded out of the list into a title-bar
    /// count (the ghost shelf). In-memory like the other view toggles; its
    /// startup value comes from `[navigation] ghost_shelf`.
    pub(crate) ghost_shelf: bool,
    backends: BackendRegistry,
    /// Registry of declarative agent definitions, used to build providers per
    /// session at spawn/restart time.
    pub(crate) agents: AgentRegistry,
    /// Configured remote SSH hosts (from `hosts.toml`), used to resolve the
    /// `HostDef` for a session's `ssh:<host>` backend when running git over SSH.
    pub(crate) hosts: crate::session::HostRegistry,
    pub(crate) db: Database,
    pub(crate) focus: InputFocus,
    pub(crate) should_quit: bool,
    /// Quit-and-re-exec (`Action::ReloadApp`): read by `main` after
    /// `shutdown()` to exec the on-disk binary, which re-adopts the freshly
    /// detached sessions on startup.
    pub(crate) reload_requested: bool,
    pub(crate) status_message: Option<StatusMessage>,
    terminal_rows: u16,
    pub(crate) terminal_cols: u16,
    session_counter: usize,
    /// Whole-feature switches (`[features]` in settings.toml), copied out of
    /// the process-wide settings at construction so tests can flip flags
    /// without touching the first-writer-wins global.
    pub(crate) features: crate::session::settings::FeatureFlags,
    /// Where the info panel docks (`info_panel_position`) — copied out of the
    /// global like [`Self::features`] so the settings panel / live reload can
    /// re-apply it without a restart.
    pub(crate) info_panel_position: crate::session::settings::InfoPanelPosition,
    /// Code-review knobs (`[review]` in settings.toml) — copied out of the
    /// global like [`Self::features`] so they apply live and tests can flip
    /// them without touching the first-writer-wins global.
    pub(crate) review_settings: crate::session::settings::ReviewSettings,
    /// Leader-key settings (`[prefix]` in settings.toml) — copied out of the
    /// global like [`Self::features`] so the settings panel applies them live
    /// and tests can switch modes without touching the process-wide global.
    pub(crate) prefix_settings: crate::session::settings::PrefixSettings,
    /// Session-navigation knobs (`[navigation]` in settings.toml) — copied out
    /// of the global like [`Self::features`] so the settings panel / live
    /// reload re-apply them without a restart.
    pub(crate) navigation: crate::session::settings::NavigationSettings,
    /// Whether the leader is armed (see [`PrefixState`]).
    pub(crate) prefix_state: PrefixState,
    /// Whether the redraw for `prefix.hint_delay_ms` elapsing was already
    /// requested. The armed state never times out, so without this latch the
    /// tick would re-request a frame forever (see [`Self::tick_prefix_hint`]).
    prefix_hint_redraw_requested: bool,
    pub(crate) show_info_panel: bool,
    /// Last content-area size pushed to the session PTYs. The `auto` info-pane
    /// dock can move between the left column and its own column when content
    /// changes (no resize event involved), so the tick compares against this
    /// and re-pushes on drift.
    last_content_size: Option<(u16, u16)>,
    /// Whether the tasks panel column is shown (toggled like the file viewer).
    pub(crate) show_tasks_panel: bool,
    pub(crate) show_file_viewer: bool,
    /// Whether the left column — session list, automations pane, and the info
    /// pane when it docks inline — is shown. The inverse of the other `show_*`
    /// flags: it defaults to `true`, since the column is the app's resting
    /// state rather than an opt-in panel. In-memory only (a restart brings it
    /// back), like the other view toggles. Toggled by
    /// [`Action::ToggleSessionList`](crate::session::Action::ToggleSessionList).
    pub(crate) show_session_list: bool,
    pub(crate) file_viewer: crate::ui::file_viewer::FileViewerState,
    /// Open native code-review views, keyed by session — persisted per session
    /// like [`Self::session_terminal_views`] (the shell view), so switching
    /// sessions and returning keeps the review open. The active session's entry
    /// (if any) is reached via [`Self::active_review`] / [`Self::active_review_mut`].
    pub(crate) code_reviews: std::collections::HashMap<SessionId, code_review::CodeReviewState>,
    /// Committed review-search queries per session, newest last — recalled
    /// with `↑`/`↓` in the find bar. In-memory only (not persisted) and kept
    /// outside [`CodeReviewState`](code_review::CodeReviewState) deliberately:
    /// the review closes on every Send→Agent, and the history must survive
    /// the reopen.
    pub(crate) review_search_history: std::collections::HashMap<SessionId, Vec<String>>,
    /// Sessions a review was sent to (`e`) that are being watched for the
    /// agent finishing, mapped to the last status observed — when a watched
    /// session leaves `Working`, a re-review nudge toast fires
    /// (`[review] nudge_on_idle`) and the watch is consumed (one nudge per
    /// send). Outside [`CodeReviewState`](code_review::CodeReviewState)
    /// because Send→Agent closes the review view.
    pub(crate) review_nudge_watch: std::collections::HashMap<SessionId, SessionStatus>,
    /// A queued editor round-trip (see [`EditorRequest`]), drained by the main
    /// loop each tick.
    pub(crate) pending_editor: Option<EditorRequest>,
    /// Open agent-activity views (section navigator + content state), keyed by
    /// session — persisted per session like [`Self::code_reviews`], so switching
    /// sessions and returning keeps the view open. Reached via
    /// [`Self::active_cc_activity`] / `_mut` (`cc_` prefix historical).
    pub(crate) cc_activities: std::collections::HashMap<SessionId, cc_activity::CcActivityState>,
    pub(crate) modal: modals::Modal,
    /// Height (rows) of the theme picker's list as last rendered, so
    /// `PageUp`/`PageDown` step by exactly one visible screenful. Written by
    /// the view each frame; `0` before the first render, hence the `max(1)` at
    /// the use site.
    pub(crate) theme_picker_page: usize,
    /// A terminal-editor run staged for the main loop to execute with a real
    /// TTY (set by `Ctrl+O` when the editor is a terminal one). Drained each
    /// iteration by `run_loop` via [`Self::take_pending_editor_run`]; GUI
    /// editors spawn detached and never populate this.
    pub(crate) pending_editor_run: Option<EditorInvocation>,
    /// In-progress new-session wizard (also drives fork/restart re-spawns).
    pub(crate) new_session: new_session_state::NewSessionWizardState,
    /// Inter-instance DB sync (polls for changes from other friring instances).
    sync_state: SyncState,
    /// Worktree-to-main git sync (Ctrl+S).
    worktree_sync: sync_state::WorktreeSyncState,
    /// System/process metrics + the tick counter pacing periodic refreshes.
    metrics: metrics_state::MetricsState,
    /// Background system-metrics refresh (also guards `sys` ownership so
    /// refreshes never overlap), polled each tick.
    metrics_refresh: background::BackgroundTask<MetricsRefresh>,
    /// Background scan of each local session's Claude Code `subagents/` tree,
    /// indexing workflows/subagents onto `SessionInfo.cc_activity`. Polled each
    /// tick; gated on `[features] cc_activity`.
    cc_refresh: background::BackgroundTask<cc_activity::CcRefresh>,
    /// Per-session directory signature of the last CC-activity scan, so an
    /// unchanged `subagents/` tree skips re-parsing. `None`/absent = never
    /// scanned. Pure in-memory (the index is file-derived, never persisted).
    cached_cc_signatures: std::collections::HashMap<SessionId, u64>,
    /// Per-session agent-neutral activity accumulators (normalized command /
    /// edit / read / web event streams tailed from each agent's on-disk
    /// records). Entries are *moved* into the in-flight scan and re-inserted
    /// by `poll_activity_refresh`. File-derived, never persisted.
    pub(crate) activity: std::collections::HashMap<SessionId, activity::SessionActivity>,
    /// Background per-session activity-event tail, polled each tick; gated on
    /// `[features] cc_activity` like the tree scan above.
    activity_refresh: background::BackgroundTask<activity::ActivityRefresh>,
    /// Background active-session git-stats refresh, polled each tick.
    git_stats: background::BackgroundTask<(SessionId, Option<crate::session::GitStats>)>,
    /// Background scan pricing each local session's agent process tree onto
    /// `SessionInfo.memory`. Polled each tick; gated on `[features]
    /// session_memory`.
    memory_refresh: background::BackgroundTask<memory::MemoryRefresh>,
    /// Cached update-check result, rendered as the header "update available"
    /// badge. `Some` only when `[features] version_check` is on and a newer
    /// release is known (from the on-disk cache). Read off the network — see
    /// [`crate::agent::version_check`].
    update_status: Option<crate::agent::version_check::UpdateStatus>,
    /// One-shot background GitHub update check (network), polled each tick. Fires
    /// once on startup when the cache is stale; on success the cache is rewritten
    /// and `update_status` re-read from it.
    version_check_task: background::BackgroundTask<Result<(), String>>,
    /// Background worktree-creation (`git worktree add`) for the new-session
    /// wizard, polled each tick; in-flight state guards against re-entry and
    /// clobbering the pending continuation.
    worktree_create: background::BackgroundTask<Result<Vec<WorktreeInfo>, String>>,
    /// Background branch listing for the new-session worktree flow's base
    /// branch selector, polled each tick. The selector opens instantly in a
    /// loading state and is filled by [`Self::poll_branch_load`] (ADR-P12).
    branch_load: background::BackgroundTask<Result<Vec<String>, String>>,
    /// Continuation for a completed worktree-creation: the wizard inputs needed
    /// to resume the spawn flow once the worktrees exist.
    pending_worktree_create: Option<PendingWorktreeCreate>,
    /// Background `Session::spawn` (PTY/tmux window creation) for the
    /// interactive new-session flow, polled each tick. Programmatic spawns
    /// stay synchronous.
    session_spawn: background::BackgroundTask<Result<Session, String>>,
    /// One-shot background scan of `~/.claude/projects` for the
    /// conversation-import picker (`i` in the session list), polled each tick.
    conversation_scan: background::BackgroundTask<Vec<cc_import::CcConversation>>,
    /// Off-thread code-review diff build (open/retarget), applied by
    /// [`Self::poll_review_build`]. See ADR-P8 in `docs/PERFORMANCE.md`.
    review_build: background::BackgroundTask<code_review::ReviewBuildResult>,
    /// Continuation for a completed background spawn: the metadata + follow-up
    /// (task prompt) to apply once the session is live.
    pending_session_spawn: Option<PendingSessionSpawn>,
    /// Remote-backed sessions still being restored in the background (one
    /// discovery thread per host), drained each tick by
    /// [`Self::poll_remote_restore`]. `None` once every remote backend has
    /// reported (or when there was nothing remote to restore).
    remote_restore: Option<RemoteRestore>,
    /// Deferred inputs: `(session_id, data, tick_at_which_to_send)`.
    /// Used to introduce a small delay between pasting text and pressing Enter.
    deferred_inputs: Vec<(SessionId, Vec<u8>, u64)>,
    /// `now_millis()` of each session's last debounced ghost-frame save,
    /// compared against `last_output_at` so [`Self::persist_dirty_frames`]
    /// skips sessions with no new output.
    frame_saved_at: HashMap<SessionId, u64>,
    /// Per-session terminal view state (Claude vs Shell). Defaults to Claude.
    session_terminal_views: HashMap<SessionId, TerminalView>,
    /// Recently deleted session awaiting finalization or undo (Ctrl+Z).
    pending_delete: Option<PendingDelete>,
    /// Active text selection (click+drag), uses screen-absolute coordinates.
    pub(crate) text_selection: Option<Selection>,
    /// Scrollbars rendered this frame, with the scroll state each drives.
    /// Cleared and rebuilt every [`App::view`]; hit-tested by the mouse handlers
    /// so a click/drag on a track scrolls the owning pane.
    pub(crate) scrollbar_hits: Vec<ScrollbarHit>,
    /// The scrollbar currently being dragged, if any. Set when a click lands on
    /// a track, cleared on mouse-up, so drags keep driving the same pane.
    pub(crate) dragging_scrollbar: Option<ScrollTarget>,
    /// Clickable regions rendered this frame (list rows, pane focus areas,
    /// modal rows). Cleared and rebuilt every [`App::view`]; hit-tested by
    /// [`App::handle_mouse_click`]. First match wins.
    pub(crate) click_targets: Vec<ClickTarget>,
    /// Last pointer position from a motion event, used to highlight the
    /// hovered row in list panes and selector modals.
    pub(crate) mouse_hover: Option<(u16, u16)>,
    /// Cached text extracted for the current selection, refreshed every frame
    /// by [`App::apply_selection_highlight`].
    selected_text_cache: Option<String>,
    /// Selection text read from the terminal pane's vt100 grid during this
    /// frame's central-pane render, where the parser is already locked (so a
    /// live drag costs no extra lock). `Some` only while the selection is
    /// inside a pane showing a terminal; every other pane falls back to the
    /// painted cells. See `ui::selection::extract_text_from_screen`.
    terminal_selection_text: Option<String>,
    /// Persistent clipboard handle to avoid "dropped too quickly" warnings on
    /// Linux. `None` when no display server is reachable (SSH/tmux/WSL) —
    /// copies then fall back to OSC 52 (see [`Self::set_clipboard_text`]).
    clipboard: Option<arboard::Clipboard>,
    /// Test-only capture: when `Some`, [`Self::set_clipboard_text`] records
    /// the text here and reports a native success instead of writing anywhere
    /// real — a test copy must never reach the developer's actual clipboard
    /// (or spawn `tmux load-buffer` against their real server).
    #[cfg(test)]
    pub(crate) captured_clipboard: Option<Vec<String>>,
    /// Persistent list state for the session section (preserves scroll offset).
    pub(crate) session_list_state: ratatui::widgets::ListState,
    /// Automations-pane UI state (cached list, selection, run history, editor).
    pub(crate) automation_ui: automation_state::AutomationUiState,
    /// Tasks-panel UI state (cached list, selection, editor, links).
    pub(crate) task_ui: task_state::TaskUiState,
    /// Global search popup (`Ctrl+/` or double-`Shift`): centered cross-scope
    /// search, Search-Everywhere-style.
    pub(crate) global_search: search::GlobalSearchState,
    /// When a bare `Shift` press last arrived (kitty-protocol terminals only)
    /// with no other key since — the pending first tap of the double-`Shift`
    /// search opener. See `App::handle_modifier_press`.
    pub(crate) pending_double_shift: Option<std::time::Instant>,
    /// Currently active theme (built-in preset or custom from themes.toml),
    /// cached so the header doesn't hit SQLite every render. Kept in sync with
    /// `db.set_active_theme` writes.
    pub(crate) active_theme: crate::session::theme_config::ThemeEntry,
    /// User-customizable global keybindings. Loaded from
    /// `~/.config/friring/keybindings.json` on startup, falling back to defaults
    /// when the file is missing or malformed.
    pub(crate) keybindings: crate::session::KeyBindings,
    /// Account-level usage/rate-limit info (the `/usage` equivalent), fetched
    /// in the background and shown for the active session. Keyed per
    /// [`UsageKey`] — agent **and** host — because the account is whatever
    /// credentials live on the machine the agent runs on: a `claude` session
    /// on `ssh:devbox` may be a different account than a local one.
    pub(crate) usage: HashMap<UsageKey, crate::session::AgentUsage>,
    /// Sends background usage-fetch results back to the app loop.
    usage_tx: mpsc::Sender<(UsageKey, crate::session::AgentUsage)>,
    /// Receives background usage-fetch results, drained each tick.
    usage_rx: mpsc::Receiver<(UsageKey, crate::session::AgentUsage)>,
    /// Config-load problems collected at startup (keybindings.json here,
    /// agents.toml/hosts.toml reported by main), shown joined in one status
    /// toast via [`Self::report_config_warnings`].
    config_warnings: Vec<String>,
    /// Receives the result of the silent startup auto-update, which runs on a
    /// background thread so a slow download never blocks the TUI from starting
    /// (`[features] auto_update`; see `main::spawn_auto_update`). `None` when the
    /// feature is off / this is a dev build (no thread spawned). Drained each
    /// tick by [`Self::poll_auto_update`]; sends one "Updated …" message only
    /// when binaries were actually replaced.
    auto_update_rx: Option<mpsc::Receiver<String>>,
    /// Last-seen mtimes of the live-reloadable config files (see
    /// [`Self::poll_config_reload`]).
    config_reload: config_reload::ConfigReloadState,
    /// OS notification dispatcher — `None` when the feature is disabled
    /// (`[features] notifications = false`) so the background thread never
    /// starts. The wrapper tracks per-session prior status + last-fired-at so
    /// dedup and "only on transition" logic live next to the sender.
    notification_state: Option<NotificationState>,
    /// Redraw-throttling dirty flag. The render loop paints only when this is
    /// set (or `FORCE_REDRAW_INTERVAL` elapsed). Starts `true` so the first
    /// frame always paints. Set by [`Self::request_redraw`] from `update`,
    /// state-changing tick steps, and the agent-output detector.
    needs_redraw: bool,
    /// Current frame of the `Working` status spinner (index into
    /// [`crate::ui::SPINNER_FRAMES`]). Advanced from `tick_count` in
    /// [`Self::refresh_session_statuses`]; only forces a repaint while a session
    /// is actually working, so an idle TUI still paints ~4 fps.
    spinner_frame: usize,
    /// The session that was focused on the previous status refresh. When focus
    /// moves off a `done` session, that session is marked "seen" (→ `Idle`), so
    /// the blue `Done` state stays visible until you actually switch away.
    last_active_session_id: Option<crate::session::SessionId>,
    /// Cached persisted hook-status rows (`session signal` state). Reloaded only
    /// when the DB's `data_version` moves (an *external* signal), not on every
    /// tick — the per-tick `data_version` read is far cheaper than the
    /// sessions-table scan it replaces. This process's own `seen_at` writes
    /// don't bump our `data_version`, so they're applied write-through in
    /// [`Self::refresh_session_statuses`]. See `docs/PERFORMANCE.md`.
    cached_hook_states: HashMap<crate::session::SessionId, crate::storage::HookRow>,
    /// `data_version` observed at the last [`Self::cached_hook_states`] reload
    /// (`None` = never loaded, which forces the first load).
    hook_states_version: Option<i64>,
    /// Remote hook events whose pane didn't match a session yet, kept for
    /// retry: the subscription's initial catch-up report arrives while the
    /// background restore is still discovering/adopting that host's windows,
    /// so dropping unmatched events would lose e.g. a `done` set while the TUI
    /// was closed. Entries carry their arrival time and expire (another
    /// instance's panes never match). See [`Self::drain_remote_hook_events`].
    pending_remote_hook_events: Vec<(String, String, String, std::time::Instant)>,
    /// When the last frame was painted, for the forced-redraw floor.
    last_draw_at: std::time::Instant,
    /// Cheap rolling signature of agent output across all sessions (sum of each
    /// session's monotonic `last_output_at`). A change means new output arrived
    /// — detected without locking any vt100 parser. See
    /// [`Self::detect_output_redraw`].
    last_output_gen: u64,
    /// Cached session-list ordering (`(content-signature, order)`). The order
    /// depends only on the session set's grouping/nesting inputs, so it is
    /// reused across frames until [`Self::session_order_signature`] changes,
    /// skipping the per-frame grouping/sort/nest work. See `render_left_panel`.
    cached_session_order: Option<(u64, crate::ui::project_list::SessionOrder)>,
    /// `FRIRING_PERF_LOG` presence, read once at construction so the hot loop's
    /// timing gate ([`Self::perf_timing_active`]) is a bool check, not an env
    /// lookup per iteration.
    perf_log_env: bool,
    /// Whether the perf HUD overlay is visible (toggled by
    /// `Action::TogglePerfHud`); also enables timing collection.
    show_perf_hud: bool,
    /// Counter values at the last `perf_window` report, so each window logs
    /// deltas (the counters themselves stay cumulative for the tests/HUD).
    perf_window_base: metrics_state::PerfCounters,
    /// Startup phase breakdown handed over by `main` (the `startup` log line's
    /// fields), included in the published perf snapshot so `friring-cli perf`
    /// shows boot cost too.
    startup_phases: Option<serde_json::Value>,
}

const EDITOR_NOT_CONFIGURED: &str =
    "No editor configured — run `friring-cli editor set <cmd>` or export $EDITOR/$VISUAL";

/// Output-quiescence threshold that breaks a *stuck* `working` hook state.
///
/// TUI agents continuously animate their in-progress line while a turn runs
/// (Claude's `… (Xs · esc to interrupt)` ticks the elapsed seconds at least
/// once a second), so a genuinely-working session is never quiet for long. But
/// when a turn is **interrupted** (Esc / Ctrl+C) Claude Code fires *no* hook —
/// it has no interrupt/idle-prompt event (verified against the hooks docs) — so
/// the persisted state stays `working` forever and the dot spins indefinitely.
/// If a `working` session has produced no terminal output for this long, the
/// agent is actually idle at its prompt, so we fall back to `Idle`. Hooks stay
/// the primary signal (this only rescues a missed `done`/`idle` edge); the
/// threshold is generous so a slow-but-live turn never trips it.
const WORKING_OUTPUT_STALE_MS: u64 = 10_000;

/// How long Alt must be held before the session-jump numbers appear: long
/// enough that pass-through Alt chords (readline `M-b`/`M-f` in the shell
/// pane) don't flash the overlay, short enough that a deliberate hold feels
/// immediate. See [`App::jump_overlay_attention_only`] / [`App::set_alt_held`].
const JUMP_OVERLAY_DELAY_MS: u64 = 150;

/// Whether the leader key is armed, and since when.
///
/// Deliberately **has no timeout**. tmux waits indefinitely after its prefix;
/// WezTerm expires its LEADER after 1s and opencode after 2s, which over SSH
/// (friring's normal deployment) turns "I pressed the leader then paused to
/// read the overlay" into "my keystroke went to the agent". The armed state
/// ends only on a key press — a bound key, a cancel, or an unbound key that
/// reports and disarms. See `App::handle_prefix_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrefixState {
    /// No leader pending; keys dispatch normally.
    Idle,
    /// `<leader> K` / `<leader> J` was pressed and we are waiting for the
    /// digit that says *how far* to move the active session. A second-level
    /// pending state rather than an action, because the digit is an argument.
    AwaitingMove {
        /// Toward the top of the list (`K`) rather than the bottom (`J`).
        up: bool,
    },
    /// The leader was pressed; the next key resolves against the leader table.
    /// Carries the arm time so the which-key overlay can honour
    /// `prefix.hint_delay_ms` (0 = show immediately, the default), and the
    /// chord that armed it so the overlay titles itself with the key the user
    /// actually pressed rather than always the primary.
    Armed {
        since: std::time::Instant,
        chord: crate::session::KeyChord,
    },
}

impl PrefixState {
    pub(crate) fn is_armed(self) -> bool {
        matches!(self, PrefixState::Armed { .. })
    }
}

/// How the session list numbers its rows this frame.
///
/// The digits are an *argument* to whatever gesture is pending, so the
/// numbering has to follow the gesture rather than being one fixed scheme:
/// after `<leader>` a digit picks a session, after `<leader> K` the same digit
/// means "this many rows up". Painting the wrong scheme would be worse than
/// painting none — the user would move a session they meant to jump to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JumpNumbering {
    /// Every session, numbered from the top (Alt held, or an armed leader).
    All,
    /// Only the sessions needing attention (`Alt+A` / `<leader> a`) — see
    /// [`App::attention_status`] for which those are.
    Attention,
    /// Rows in one direction from `from`, numbered by *distance* — so the row
    /// labelled `3` is where `<leader> K 3` lands the session.
    MoveDistance { from: usize, up: bool },
    /// Every session, wearing a home-row letter label (`Alt+G` /
    /// `<leader> A`). Unlike [`Self::All`] this is not capped at nine, which is
    /// the whole point of it.
    Labels,
}

/// The label-jump overlay (`Alt+G` / `<leader> A`), while open.
///
/// A sticky mode rather than a held one: the Alt-hold overlay needs the kitty
/// protocol to see the key going down, which an outer tmux strips — and this
/// fork is normally driven through one. Typed letters accumulate so a two-key
/// label can be entered; any key that can't continue a label ends the mode.
#[derive(Debug, Clone, Default)]
pub(crate) struct LabelJump {
    /// Label characters typed so far. Rows whose label starts with this show
    /// only the remainder, so the overlay narrows as the user commits.
    pub(crate) typed: String,
}

/// Map a session's persisted hook state to its rendered [`SessionStatus`]. Pure
/// so it's unit-testable without an `App`/DB. `exited` forces `Idle` (a crashed/
/// finished process); `just_seen` is `true` when the user just moved focus off a
/// `done` session this tick (acknowledged → `Idle`); `quiet_for_ms` is the time
/// since the session's last terminal output, used to rescue a stuck `working`
/// state (see [`WORKING_OUTPUT_STALE_MS`]). A `done` session is `Done` (blue)
/// until seen; `idle`/missing/unknown states are `Idle`.
fn derive_session_status(
    hook: Option<&crate::storage::HookRow>,
    exited: bool,
    just_seen: bool,
    quiet_for_ms: u64,
) -> SessionStatus {
    if exited {
        return SessionStatus::Idle;
    }
    match hook.and_then(|h| h.state.as_deref()) {
        // A live `working` turn keeps emitting output; a stuck one (interrupt /
        // crash / an agent that missed its done edge) goes quiet → fall to Idle.
        Some("working") if quiet_for_ms <= WORKING_OUTPUT_STALE_MS => SessionStatus::Working,
        Some("working") => SessionStatus::Idle,
        Some("blocked") => SessionStatus::Blocked,
        Some("done") => {
            let state_at = hook.and_then(|h| h.state_at).unwrap_or(0);
            let seen_at = hook.and_then(|h| h.seen_at).unwrap_or(0);
            if just_seen || seen_at >= state_at {
                SessionStatus::Idle
            } else {
                SessionStatus::Done
            }
        }
        _ => SessionStatus::Idle,
    }
}

/// Spin up the OS notification dispatcher when the feature is enabled,
/// returning `None` otherwise so the background thread never starts.
/// Reads the process-wide settings directly — they're already published by
/// `main` before `App::new` runs.
fn build_notification_state() -> Option<NotificationState> {
    let settings = crate::session::settings::global();
    if !settings.features.notifications {
        return None;
    }
    // Resolve the delivery backend (dbus / Windows-toast / macOS / none) from
    // the configured preference plus host probing, then start the dispatcher
    // for it. A `none` backend (e.g. WSL without powershell, or backend="off")
    // still starts the thread but drops every notification — the reason is
    // recorded for the `friring-cli notify` diagnostic rather than silently
    // lost as before.
    let backend = crate::notifications::detect_backend(settings.notifications.backend);
    if !backend.is_deliverable() {
        debug!(
            "notifications enabled but no deliverable backend: {}",
            backend.label()
        );
    }
    let sender = crate::notifications::start(backend);
    Some(NotificationState::new(sender, settings.notifications))
}

impl App {
    pub fn new(
        rows: u16,
        cols: u16,
        backends: BackendRegistry,
        agents: AgentRegistry,
        db: Database,
    ) -> Self {
        // Resolve the persisted active theme — built-in or custom — defaulting
        // to the Default preset when unset/unknown.
        let active_theme = db
            .get_active_theme()
            .ok()
            .flatten()
            .as_deref()
            .and_then(crate::ui::theme::find_theme_entry)
            .unwrap_or_else(|| {
                crate::session::theme_config::ThemeEntry::from_preset(
                    crate::session::ThemePreset::Default,
                )
            });

        // Load keybindings from JSON config or fall back to defaults. Problems
        // are collected into `config_warnings` so the first frame can surface
        // them in the status bar (a log-only warning is invisible in a TUI).
        let mut config_warnings: Vec<String> = Vec::new();
        let keybindings = match crate::storage::keybindings::load_keybindings_json() {
            Ok(Some(json)) => match crate::session::KeyBindings::from_json_with_warnings(&json) {
                Ok((bindings, warnings)) => {
                    config_warnings
                        .extend(warnings.iter().map(|w| format!("keybindings.json: {w}")));
                    bindings
                }
                Err(e) => {
                    config_warnings
                        .push(format!("keybindings.json: {e}; using default keybindings"));
                    crate::session::KeyBindings::default()
                }
            },
            Ok(None) => crate::session::KeyBindings::default(),
            Err(e) => {
                config_warnings.push(format!("keybindings.json: {e}; using default keybindings"));
                crate::session::KeyBindings::default()
            }
        };
        for w in &config_warnings {
            tracing::warn!("{w}");
        }

        let session_counter = db.get_session_counter().unwrap_or(0);

        let mut sync_state = SyncState::new();

        // Initialize the sync snapshot from the current DB state so the first
        // poll doesn't produce a false delta treating everything as "added".
        if let Ok(initial_state) = db.load_shared_state() {
            sync_state.set_initial_snapshot(initial_state);
        }

        let (usage_tx, usage_rx) = mpsc::channel();

        let mut app = Self {
            sessions: Vec::new(),
            active_index: 0,
            last_active_session: None,
            session_mru: Vec::new(),
            alt_held: false,
            alt_held_since: None,
            alt_overlay_redraw_requested: false,
            attention_jump: None,
            label_jump: None,
            folded_groups: std::collections::HashSet::new(),
            ghost_shelf: crate::session::settings::global().navigation.ghost_shelf,
            backends,
            agents,
            hosts: crate::session::HostRegistry::default(),
            db,
            focus: InputFocus::SessionList,
            should_quit: false,
            reload_requested: false,
            status_message: None,
            terminal_rows: rows,
            terminal_cols: cols,
            session_counter,
            features: crate::session::settings::global().features,
            info_panel_position: crate::session::settings::global().info_panel_position,
            review_settings: crate::session::settings::global().review,
            prefix_settings: crate::session::settings::global().prefix.clone(),
            navigation: crate::session::settings::global().navigation,
            prefix_state: PrefixState::Idle,
            prefix_hint_redraw_requested: false,
            show_info_panel: false,
            last_content_size: None,
            show_tasks_panel: false,
            show_file_viewer: false,
            show_session_list: true,
            file_viewer: crate::ui::file_viewer::FileViewerState::new(),
            code_reviews: std::collections::HashMap::new(),
            review_search_history: std::collections::HashMap::new(),
            review_nudge_watch: std::collections::HashMap::new(),
            pending_editor: None,
            cc_activities: std::collections::HashMap::new(),
            modal: modals::Modal::None,
            theme_picker_page: 0,
            pending_editor_run: None,
            new_session: new_session_state::NewSessionWizardState::default(),
            sync_state,
            worktree_sync: sync_state::WorktreeSyncState::default(),
            metrics: metrics_state::MetricsState::new(),
            metrics_refresh: background::BackgroundTask::default(),
            cc_refresh: background::BackgroundTask::default(),
            cached_cc_signatures: std::collections::HashMap::new(),
            activity: std::collections::HashMap::new(),
            activity_refresh: background::BackgroundTask::default(),
            git_stats: background::BackgroundTask::default(),
            memory_refresh: background::BackgroundTask::default(),
            // Seed the badge from the cache (no network); refreshed on first
            // tick if the flag is on and the cache is stale.
            update_status: if crate::session::settings::global().features.version_check {
                crate::agent::version_check::read_cached_status()
            } else {
                None
            },
            version_check_task: background::BackgroundTask::default(),
            worktree_create: background::BackgroundTask::default(),
            branch_load: background::BackgroundTask::default(),
            pending_worktree_create: None,
            session_spawn: background::BackgroundTask::default(),
            conversation_scan: background::BackgroundTask::default(),
            review_build: background::BackgroundTask::default(),
            pending_session_spawn: None,
            remote_restore: None,
            deferred_inputs: Vec::new(),
            frame_saved_at: HashMap::new(),
            session_terminal_views: HashMap::new(),
            pending_delete: None,
            text_selection: None,
            scrollbar_hits: Vec::new(),
            dragging_scrollbar: None,
            click_targets: Vec::new(),
            mouse_hover: None,
            selected_text_cache: None,
            terminal_selection_text: None,
            clipboard: arboard::Clipboard::new().ok(),
            #[cfg(test)]
            captured_clipboard: None,
            session_list_state: ratatui::widgets::ListState::default(),
            automation_ui: automation_state::AutomationUiState::default(),
            task_ui: task_state::TaskUiState::default(),
            global_search: search::GlobalSearchState::default(),
            pending_double_shift: None,
            active_theme,
            keybindings,
            usage: HashMap::new(),
            usage_tx,
            usage_rx,
            config_warnings: Vec::new(),
            auto_update_rx: None,
            config_reload: config_reload::ConfigReloadState {
                agents_mtime: config_reload::agents_mtime(),
                keybindings_mtime: config_reload::keybindings_mtime(),
                settings_mtime: config_reload::settings_mtime(),
            },
            notification_state: build_notification_state(),
            needs_redraw: true,
            spinner_frame: 0,
            last_active_session_id: None,
            cached_hook_states: HashMap::new(),
            pending_remote_hook_events: Vec::new(),
            hook_states_version: None,
            last_draw_at: clock::now(),
            last_output_gen: 0,
            cached_session_order: None,
            perf_log_env: std::env::var_os("FRIRING_PERF_LOG").is_some(),
            show_perf_hud: false,
            perf_window_base: metrics_state::PerfCounters::default(),
            startup_phases: None,
        };
        app.report_config_warnings(config_warnings);
        app
    }

    /// Surface config-load warnings in the status bar (they are otherwise only
    /// visible in the log file, which nobody watches while the TUI owns the
    /// screen). Accumulates across calls — main reports agents.toml/hosts.toml
    /// problems after construction — and shows them joined in one toast.
    pub fn report_config_warnings(&mut self, warnings: Vec<String>) {
        if warnings.is_empty() {
            return;
        }
        self.config_warnings.extend(warnings);
        let text = format!("Config: {}", self.config_warnings.join(" · "));
        self.set_status(StatusLevel::Error, text);
    }

    /// Attach the receiver for the background startup auto-update (see
    /// `main::spawn_auto_update`). The update runs off-thread so its download
    /// never delays the first frame; the result is drained in [`Self::tick`].
    pub fn set_auto_update_receiver(&mut self, rx: mpsc::Receiver<String>) {
        self.auto_update_rx = Some(rx);
    }

    /// Drain the background auto-update result. The thread sends at most one
    /// message — only when binaries were actually replaced — so we surface it as
    /// an info toast and drop the receiver. A disconnected channel (the thread
    /// finished with nothing to report, or failed) also drops the receiver so we
    /// stop polling.
    fn poll_auto_update(&mut self) {
        let Some(rx) = &self.auto_update_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(msg) => {
                self.set_status(StatusLevel::Info, msg);
                self.auto_update_rx = None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.auto_update_rx = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Reload `agents.toml` / `keybindings.json` in place when their mtime
    /// changes — editing either takes effect without a restart. Self-writes
    /// (the F1 editor persisting a rebind) refresh the stored mtime at save
    /// time, so they don't re-toast here.
    fn poll_config_reload(&mut self) {
        if config_reload::agents_mtime() != self.config_reload.agents_mtime {
            self.reload_agents_config();
        }

        let kb_mtime = config_reload::keybindings_mtime();
        if kb_mtime != self.config_reload.keybindings_mtime {
            self.config_reload.keybindings_mtime = kb_mtime;
            self.reload_keybindings_config();
        }

        if config_reload::settings_mtime() != self.config_reload.settings_mtime {
            self.reload_settings_config();
        }
    }

    /// Reload `agents.toml` and toast the result. Caller has already detected an
    /// mtime change; this re-stats afterwards so a re-seeded file is recorded.
    fn reload_agents_config(&mut self) {
        let (registry, warnings) = crate::agent::agent_config::load_or_seed_with_warnings();
        self.agents = registry;
        // Re-stat after the load: a missing file gets re-seeded by it.
        self.config_reload.agents_mtime = config_reload::agents_mtime();
        self.toast_config_reload("agents.toml reloaded", &warnings);
    }

    /// Reload `keybindings.json` and toast the result. Caller has already
    /// recorded the new mtime.
    fn reload_keybindings_config(&mut self) {
        let (bindings, warnings) = Self::load_keybindings_with_warnings();
        self.keybindings = bindings;
        self.toast_config_reload("keybindings.json reloaded", &warnings);
    }

    /// Reload `settings.toml` when it changes on disk (a hand-edit, the in-TUI
    /// panel, or another instance). Re-applies the live feature flags in place;
    /// restart-only values stay frozen in the global, so the toast flags when a
    /// restart is needed. Caller has already detected the mtime change; this
    /// re-stats afterwards so a re-seeded file is recorded.
    fn reload_settings_config(&mut self) {
        let (settings, mut warnings) = crate::agent::settings_config::load_or_seed_with_warnings();
        if settings.restart_only_differs(crate::session::settings::global()) {
            warnings.push("restart to apply some changes".into());
        }
        self.apply_live_settings(&settings);
        self.config_reload.settings_mtime = config_reload::settings_mtime();
        self.toast_config_reload("settings.toml reloaded", &warnings);
    }

    /// Apply the **live** portion of `settings` (the UI-panel feature flags and
    /// the info-pane position, both read from `App` state each frame) and
    /// resize panes to match. The restart-only values are intentionally left to
    /// the next launch. Shared by the settings panel's save path and the
    /// live-reload poll.
    pub(crate) fn apply_live_settings(&mut self, settings: &crate::session::settings::Settings) {
        self.features = settings.features;
        self.info_panel_position = settings.info_panel_position;
        self.review_settings = settings.review;
        let old_leaders = self.prefix_settings.chords();
        self.prefix_settings = settings.prefix.clone();
        self.navigation = settings.navigation;
        // A leader set that changed — rebound, or emptied by `mode = off` —
        // must not leave the old armed state behind. Armed against the *new*
        // leader, `handle_prefix_key` would read its first press as
        // `<leader> <leader>` and send its bytes to the agent; emptied, no key
        // could clear the overlay and the footer badge at all.
        if old_leaders != self.prefix_settings.chords() {
            self.prefix_state = PrefixState::Idle;
            self.prefix_hint_redraw_requested = false;
        }
        self.enforce_feature_visibility();
        self.resize_sessions_to_content_area();
    }

    /// Where focus retreats when the pane holding it is closed or hidden. The
    /// session list is the natural home, but it is itself hideable
    /// ([`Self::show_session_list`]) — so while it is collapsed, focus falls to
    /// the terminal instead of resting on a surface that isn't rendered. Every
    /// site that drops a pane's focus goes through here.
    pub(crate) fn focus_fallback(&self) -> InputFocus {
        if self.show_session_list {
            InputFocus::SessionList
        } else {
            InputFocus::Terminal
        }
    }

    /// Tear down any panel/view/focus that a now-disabled live feature flag
    /// leaves stranded. The open-state booleans (`show_*`), the per-session
    /// shell views, and the open code reviews are all opt-in toggles that
    /// survive a flag flip, so without this a feature disabled at runtime would
    /// keep rendering its panel even though its tab/footer affordance is gone.
    /// Each branch only forces the *hidden* state, so it's idempotent and never
    /// re-opens anything when a flag is turned back on.
    fn enforce_feature_visibility(&mut self) {
        if !self.features.info_panel {
            self.show_info_panel = false;
        }
        if !self.features.file_viewer {
            self.show_file_viewer = false;
            if self.focus == InputFocus::FileViewer {
                self.focus = self.focus_fallback();
            }
        }
        if !self.features.tasks {
            self.show_tasks_panel = false;
            if matches!(self.focus, InputFocus::TaskList | InputFocus::TaskEditor) {
                self.focus = self.focus_fallback();
            }
        }
        if !self.features.automations
            && matches!(
                self.focus,
                InputFocus::Automations
                    | InputFocus::AutomationEditor
                    | InputFocus::AutomationRunHistory
            )
        {
            self.focus = self.focus_fallback();
        }
        if !self.features.global_search && self.global_search.active {
            self.close_global_search();
        }
        if !self.features.shell_pane {
            // Flip every session showing its shell back to the agent view (the
            // Shell tab/toggle is gone, so there's no way back otherwise).
            for view in self.session_terminal_views.values_mut() {
                if *view == TerminalView::Shell {
                    *view = TerminalView::Claude;
                }
            }
        }
        if !self.features.code_review && !self.code_reviews.is_empty() {
            self.code_reviews.clear();
            if matches!(self.focus, InputFocus::CodeReview | InputFocus::ReviewFiles) {
                self.focus = InputFocus::Terminal;
            }
        }
        if !self.features.perf_hud {
            self.show_perf_hud = false;
        }
        if !self.features.session_memory {
            // The badges render straight off `info.memory`, so the last scan's
            // figures would stay on screen after the flag went off — and a scan
            // already in flight would repopulate them. Drop its receiver so its
            // result can never be polled, then clear what it already wrote.
            self.memory_refresh.cancel();
            for session in &mut self.sessions {
                session.info.memory = None;
            }
        }
    }

    /// Record the current `settings.toml` mtime so the next reload poll doesn't
    /// treat the settings panel's own write as an external edit.
    pub(crate) fn mark_settings_saved(&mut self) {
        self.config_reload.settings_mtime = config_reload::settings_mtime();
    }

    /// Load the on-disk keybindings, falling back to defaults (with a warning)
    /// on any read/parse error.
    fn load_keybindings_with_warnings() -> (crate::session::KeyBindings, Vec<String>) {
        match crate::storage::keybindings::load_keybindings_json() {
            Ok(Some(json)) => match crate::session::KeyBindings::from_json_with_warnings(&json) {
                Ok((bindings, warnings)) => (
                    bindings,
                    warnings
                        .into_iter()
                        .map(|w| format!("keybindings.json: {w}"))
                        .collect(),
                ),
                Err(e) => (
                    crate::session::KeyBindings::default(),
                    vec![format!("keybindings.json: {e}; using default keybindings")],
                ),
            },
            Ok(None) => (crate::session::KeyBindings::default(), Vec::new()),
            Err(e) => (
                crate::session::KeyBindings::default(),
                vec![format!("keybindings.json: {e}; using default keybindings")],
            ),
        }
    }

    /// Toast the outcome of a live config reload: an info `ok` line when clean,
    /// otherwise the joined warnings (also logged).
    fn toast_config_reload(&mut self, ok: &str, warnings: &[String]) {
        if warnings.is_empty() {
            self.set_status(StatusLevel::Info, ok);
        } else {
            self.set_status(
                StatusLevel::Error,
                format!("Config: {}", warnings.join(" · ")),
            );
        }
        for w in warnings {
            warn!("{w}");
        }
    }

    /// Record the current `keybindings.json` mtime so the next reload poll
    /// doesn't treat our own write as an external edit.
    pub(crate) fn mark_keybindings_saved(&mut self) {
        self.config_reload.keybindings_mtime = config_reload::keybindings_mtime();
    }

    /// Build an [`AgentProvider`] for a session
    /// config by looking its agent up in the registry. Falls back to the
    /// registry default, then to the built-in default, so a stale/unknown agent
    /// name never breaks spawning.
    ///
    /// For **adoption** (attaching to an already-running window). Paths that
    /// launch a new process use `launch_provider_for`, which also adapts the
    /// def's args for a remote host.
    pub fn provider_for(&self, config: &SessionConfig) -> Arc<dyn crate::agent::AgentProvider> {
        Arc::new(GenericProvider::new(self.agent_def_for(&config.agent)))
    }

    /// [`Self::provider_for`], plus remote arg adaptation: when `config` targets
    /// a remote (SSH/WSL) backend, the def's args that reference friring-managed
    /// config files by *local* path (claude's hooks `--settings …`) are rewritten
    /// for the host — materialized at a home-translated remote path, or stripped
    /// when no remote path can work — because an unresolvable path kills the
    /// agent on launch ("Settings file not found"). Shares the headless spawn's
    /// implementation; used by every path that launches a new agent process
    /// (spawn, restore, respawn-on-restore).
    fn launch_provider_for(&self, config: &SessionConfig) -> Arc<dyn crate::agent::AgentProvider> {
        let mut def = self.agent_def_for(&config.agent);
        if let Some(h) = self.host_for_backend(config.backend.as_deref()) {
            def.args = crate::session_ops::spawn::adapt_agent_args_for_remote(h, def.args);
        } else if let Some(sid) = config.agent_session_id.as_deref() {
            // Local claude: point the hooks `--settings` at a per-session symlink
            // so a backgrounded (daemon) workflow is attributed to this exact
            // session in the activity view — not disambiguated by cwd. Shares the
            // headless spawn's rewrite; a no-op for agents without the hook.
            def.args =
                crate::session_ops::builtin_hooks::rewrite_settings_for_session(sid, def.args);
        }
        Arc::new(GenericProvider::new(def))
    }

    /// Resolve the [`AgentDef`] for an agent name via the same fallback chain as
    /// [`Self::provider_for`] (named agent → registry default → built-in
    /// default). Used to decide resume/fork behaviour at restart time.
    fn agent_def_for(&self, agent: &str) -> AgentDef {
        self.agents
            .get(agent)
            .or_else(|| self.agents.default_agent())
            .cloned()
            .unwrap_or_else(|| {
                crate::agent::agent_config::builtin_registry()
                    .default_agent()
                    .cloned()
                    .expect("built-in registry always has a default agent")
            })
    }

    /// Entry point for the new-session wizard.
    ///
    /// When any off-local host is available — a configured SSH/WSL host
    /// (`hosts.toml`) or an auto-discovered WSL distro — first shows the host
    /// picker so the user can choose where the session runs; otherwise goes
    /// straight to the repo picker (preserving the local-only UX).
    pub(crate) fn start_new_session(&mut self) {
        // Clear any choice left over from a previously cancelled flow.
        self.new_session.backend = None;
        self.new_session.workspace_dir = None;
        self.new_session.saved_repo_picker = None;
        self.new_session.saved_conversation_picker = None;

        if self.hosts.is_empty() {
            self.open_repo_picker();
            return;
        }
        self.open_host_picker();
    }

    /// Open the host picker (`local` + every configured host), preselecting
    /// the wizard's current backend so Esc-back from the repo palette lands on
    /// the choice that was made.
    pub(crate) fn open_host_picker(&mut self) {
        let mut choices = vec![crate::ui::host_picker_modal::HostChoice {
            label: "local".to_string(),
            backend: String::new(),
        }];
        for host in &self.hosts.hosts {
            choices.push(crate::ui::host_picker_modal::HostChoice {
                label: format!("{}  ({})", host.name, host.picker_detail()),
                backend: host.backend_name(),
            });
        }
        let current = self.new_session.backend.as_deref().unwrap_or_default();
        let selected_index = choices
            .iter()
            .position(|c| c.backend == current)
            .unwrap_or(0);
        self.modal = modals::Modal::HostPicker(crate::ui::host_picker_modal::HostPickerState {
            choices,
            selected_index,
            filter: Default::default(),
        });
    }

    /// Open the repo-picker palette for creating a new session.
    ///
    /// Loads the target host's bookmarks from the database (bookmarks are
    /// host-scoped — a remote target shows the repos previously used *on that
    /// host*, never local paths) and shows the palette with its single input
    /// ready for typing.
    pub(crate) fn open_repo_picker(&mut self) {
        let bookmarks = self.load_repo_bookmarks();
        let mut rp = modals::RepoPickerModal {
            remote: self.new_session.backend.is_some(),
            ..Default::default()
        };
        Self::rebuild_repo_picker_rows(&mut rp, bookmarks, Self::import_suggestion_dirs());
        self.modal = modals::Modal::RepoPicker(rp);
    }

    /// The host-scope key for repo bookmarks: the new-session wizard's target
    /// backend name (`ssh:<name>` / `wsl:<name>`), or `""` for local — the
    /// `repo_bookmarks.host` column (schema v39).
    pub(super) fn bookmark_host_key(&self) -> &str {
        self.new_session.backend.as_deref().unwrap_or_default()
    }

    /// Load persisted repo bookmarks for the new-session wizard's current
    /// target host, logging (and swallowing) any DB error.
    fn load_repo_bookmarks(&self) -> Vec<crate::storage::repo_bookmarks::RepoBookmark> {
        match self.db.list_repo_bookmarks(self.bookmark_host_key()) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("Failed to load repo bookmarks: {e}");
                Vec::new()
            }
        }
    }

    /// Reopen the repo palette parked by a forward step (Esc-back), falling
    /// back to a fresh open for flows that entered the wizard mid-way.
    pub(crate) fn restore_repo_picker(&mut self) {
        match self.new_session.saved_repo_picker.take() {
            Some(rp) => self.modal = modals::Modal::RepoPicker(*rp),
            None => self.open_repo_picker(),
        }
    }

    /// Re-read bookmarks and rebuild the open repo picker's rows in place
    /// (re-scanning parent folders). Used after importing/deleting a bookmark.
    pub(crate) fn refresh_repo_picker_rows(&mut self) {
        let bookmarks = self.load_repo_bookmarks();
        let suggestions = Self::import_suggestion_dirs();
        let modals::Modal::RepoPicker(ref mut rp) = self.modal else {
            return;
        };
        Self::rebuild_repo_picker_rows(rp, bookmarks, suggestions);
    }

    /// First-run helper: common local project folders that actually exist,
    /// offered as one-key parent imports while the picker has no bookmarks.
    fn import_suggestion_dirs() -> Vec<PathBuf> {
        let Some(home) = crate::paths::home_dir() else {
            return Vec::new();
        };
        ["code", "src", "projects", "dev", "work"]
            .iter()
            .map(|d| home.join(d))
            .filter(|p| p.is_dir())
            .collect()
    }

    /// (Re)build the repo picker rows from persisted bookmarks, **re-scanning**
    /// parent bookmarks for their current git sub-directories. Standalone repos
    /// become one row; a parent becomes a header row followed by an indented
    /// child row per discovered repo (children are ephemeral — never persisted).
    /// Preserves the existing search input by recomputing the filter.
    fn rebuild_repo_picker_rows(
        rp: &mut modals::RepoPickerModal,
        bookmarks: Vec<crate::storage::repo_bookmarks::RepoBookmark>,
        import_suggestions: Vec<PathBuf>,
    ) {
        use std::collections::HashSet;

        // Only the rows are rebuilt. `selected`/`worktree`/`collapsed` are
        // path-keyed and deliberately kept: a mid-flow refresh (parent import,
        // bookmark delete) must not drop the user's in-flight picks.
        rp.rows.clear();

        // Scan each parent once; a path that appears as a child of any parent
        // takes precedence over a standalone bookmark of the same path, so the
        // repo is shown only once (grouped under its parent).
        let scans: HashMap<PathBuf, Vec<PathBuf>> = bookmarks
            .iter()
            .filter(|b| b.is_parent)
            .map(|b| {
                (
                    b.repo_path.clone(),
                    crate::git::scan_child_repos(&b.repo_path),
                )
            })
            .collect();
        let child_paths: HashSet<&PathBuf> = scans.values().flatten().collect();

        // `emitted` guards against any path being rendered twice (duplicate
        // bookmarks, a child shared by two parents, a parent nested in another).
        let mut emitted: HashSet<PathBuf> = HashSet::new();
        for bm in &bookmarks {
            Self::emit_bookmark_row(rp, bm, &scans, &child_paths, &mut emitted);
        }

        // Pinned helper rows come last. Import suggestions only make sense for
        // a first run (no bookmark rows at all) on the local filesystem; the
        // "start here" escape hatch is always available.
        if !rp.remote && rp.rows.is_empty() {
            for dir in import_suggestions {
                rp.push_row(dir, modals::RepoRowKind::ImportSuggestion);
            }
        }
        rp.push_row(PathBuf::new(), modals::RepoRowKind::StartHere);

        rp.list_index = 0;
        rp.recompute_filter();
    }

    /// Emit the row(s) for a single bookmark into the repo picker: a parent
    /// header followed by its scanned children, or a standalone repo.
    /// `emitted` dedupes paths across the whole list; `child_paths` lets a
    /// standalone bookmark be dropped when a parent already covers it.
    fn emit_bookmark_row(
        rp: &mut modals::RepoPickerModal,
        bm: &crate::storage::repo_bookmarks::RepoBookmark,
        scans: &HashMap<PathBuf, Vec<PathBuf>>,
        child_paths: &std::collections::HashSet<&PathBuf>,
        emitted: &mut std::collections::HashSet<PathBuf>,
    ) {
        if !bm.is_parent {
            // Drop a standalone bookmark that is already covered by a parent.
            if child_paths.contains(&bm.repo_path) {
                return;
            }
            if emitted.insert(bm.repo_path.clone()) {
                rp.push_row(
                    bm.repo_path.clone(),
                    modals::RepoRowKind::Repo { child: false },
                );
            }
            return;
        }
        if !emitted.insert(bm.repo_path.clone()) {
            return;
        }
        rp.push_row(bm.repo_path.clone(), modals::RepoRowKind::Header);
        for child in scans.get(&bm.repo_path).into_iter().flatten() {
            if emitted.insert(child.clone()) {
                rp.push_row(child.clone(), modals::RepoRowKind::Repo { child: true });
            }
        }
    }

    #[cfg(test)]
    fn next_session_name(&mut self) -> String {
        self.session_counter += 1;
        self.session_counter.to_string()
    }

    pub(crate) fn spawn_session_with_config(&mut self, config: &SessionConfig) {
        let mut config = config.clone();
        // Apply the host chosen in the new-session wizard (None = local).
        if config.backend.is_none() {
            config.backend = self.new_session.backend.take();
        }
        self.prepare_spawn(config, Vec::new());
    }

    /// Route session creation through the name modal, then agent selection.
    ///
    /// The name modal opens prefilled with a suggestion derived from the
    /// working directory, so the common case is Enter-through; the user edits
    /// or clears it freely.
    pub(crate) fn prepare_spawn(&mut self, config: SessionConfig, worktrees: Vec<WorktreeInfo>) {
        let mut modal = modals::SessionNameModal::default();
        modal
            .name
            .set(&self.suggested_session_name(config.cwd.as_deref()));
        self.prefill_workspace_dir_field(&mut modal);
        self.new_session.spawn_config = Some(config);
        self.new_session.spawn_worktrees = worktrees;
        self.modal = modals::Modal::SessionName(modal);
    }

    /// Whether the name step should offer the optional workspace-dir field
    /// (`Ctrl+O`): only for a **local** pending spawn (a custom workspace dir
    /// is local-only) spanning ≥2 member dirs (single-repo sessions launch in
    /// the repo itself — there is no workspace to place).
    pub(crate) fn pending_spawn_offers_workspace_dir(&self) -> bool {
        // The backend is consumed by `spawn_session_with_config` in the normal
        // flow — fall back to the pending config's copy (mirrors
        // `wizard_breadcrumb`).
        let backend = self.new_session.backend.as_deref().or_else(|| {
            self.new_session
                .spawn_config
                .as_ref()
                .and_then(|c| c.backend.as_deref())
        });
        if self.host_for_backend(backend).is_some() {
            return false;
        }
        // Worktree flow: the name step precedes worktree creation, so count
        // the picked repos rather than the not-yet-existing member dirs
        // (`all_repos` is `Some` only for >1 worktree repos).
        if self.new_session.base_branch.is_some() {
            return self.new_session.all_repos.is_some()
                || !self.new_session.normal_repos.is_empty();
        }
        let worktrees = self.new_session.spawn_worktrees.len();
        worktrees.max(1) + self.new_session.additional_dirs.len() >= 2
    }

    /// Re-arm the name modal's optional workspace-dir field from wizard state,
    /// so stepping back to the name step doesn't silently drop the choice.
    pub(crate) fn prefill_workspace_dir_field(&self, modal: &mut modals::SessionNameModal) {
        if let Some(dir) = &self.new_session.workspace_dir {
            let mut field = modals::TextInput::new();
            field.set(&crate::paths::display_path_tilde(dir));
            modal.workspace_dir = Some(field);
        }
    }

    /// A prefilled session name: the working directory's basename, deduped
    /// against existing session names with a numeric suffix — duplicate names
    /// make the tmux window lookup ambiguous.
    pub(crate) fn suggested_session_name(&self, cwd: Option<&std::path::Path>) -> String {
        let base = cwd.map(crate::paths::display_path).unwrap_or_default();
        if base.is_empty() {
            return base;
        }
        let taken = |name: &str| self.sessions.iter().any(|s| s.info.name == name);
        if !taken(&base) {
            return base;
        }
        (2..100)
            .map(|i| format!("{base}-{i}"))
            .find(|c| !taken(c))
            .unwrap_or(base)
    }

    /// Continue spawn after the user has chosen a session name: open the agent
    /// picker populated from the registry. With zero or one agent the picker is
    /// skipped and the session spawns immediately.
    fn finish_prepare_spawn(
        &mut self,
        name: String,
        config: SessionConfig,
        worktrees: Vec<WorktreeInfo>,
    ) {
        let names = self.agents.names();
        if names.len() <= 1 {
            let mut config = config;
            config.agent = names
                .first()
                .map(|s| s.to_string())
                .unwrap_or_else(|| DEFAULT_AGENT_NAME.to_string());
            self.do_spawn_session_async(name, &config, worktrees);
            return;
        }

        self.new_session.spawn_name = Some(name);
        self.new_session.spawn_config = Some(config);
        self.new_session.spawn_worktrees = worktrees;
        self.open_agent_picker();
    }

    fn restart_active_session(&mut self) {
        let Some(session) = self.sessions.get(self.active_index) else {
            return;
        };
        // A ghost's "restart" IS its load: fall through to the normal path,
        // where `Session::restart` spawns the agent (no pane to kill) and
        // clears the ghost flags. Only the *remote-unreachable* placeholder
        // has no process to launch — a manual restart there means "reconnect
        // now", so kick an immediate retry sweep for its backend instead.
        let is_ghost_load = session.is_ghost();
        if session.is_placeholder() && !is_ghost_load {
            let backend_type = session.backend_name().to_string();
            self.retry_remote_backend_now(&backend_type);
            self.set_status(StatusLevel::Info, "Retrying remote host…");
            return;
        }
        let Some(agent_session_id) = session.info.agent_session_id.clone() else {
            return;
        };

        let agent = session.info.agent.clone();
        let session_name = session.info.name.clone();
        // Keep the same friring identity across a restart so injected env stays
        // stable (`FRIRING_SESSION`).
        let session_id = session.info.id;
        // Preserve a remote backend on the config — set *before* env injection
        // (which skips the local-path dir vars for remote sessions) and used to
        // adapt the relaunch args for the host.
        let backend_type = session.backend_name().to_string();
        // Rebuild the process cwd: the primary repo for a single-repo session,
        // or the (idempotently rebuilt) symlink workspace for a multi-repo one.
        let cwd = self.session_process_cwd(&session.info);

        let mut config = SessionConfig {
            resume_session_id: None,
            session_id: Some(session_id),
            agent_session_id: Some(agent_session_id.clone()),
            cwd,
            agent,
            fork_session_id: None,
            backend: crate::session::is_remote_backend(&backend_type).then_some(backend_type),
            // Only reaches the args when the restart falls back to a fresh
            // conversation (new_session_args); a resume never renames.
            session_name: Some(session_name),
            ..SessionConfig::default()
        };
        // `Session::restart` replaces the session env wholesale, so re-inject the
        // standard `FRIRING_*` identity vars (the same set a fresh spawn gets via
        // `build_spawn_inputs`); otherwise the restarted agent loses its identity
        // and the metrics/status hooks break.
        crate::session_ops::inject_friring_env(&mut config, &agent_session_id, None);
        let def = self.agent_def_for(&config.agent);
        config.resume_session_id =
            crate::session_ops::resume_trigger_for(&def, &agent_session_id, &config.env);

        self.do_restart(
            config,
            if is_ghost_load {
                "Session loaded"
            } else {
                "Session restarted"
            },
        );
    }

    /// Execute the actual restart with the finalized config. `success_msg` is
    /// the status-bar text on success ("restarted" vs a ghost's "loaded").
    fn do_restart(&mut self, config: SessionConfig, success_msg: &str) {
        let (rows, cols) = self.content_area_size();
        // Resolve the relaunch provider from the *current* registry (and adapt
        // its args for a remote backend) before restarting — the provider the
        // session stored at spawn/adopt time may predate both.
        let provider = self.launch_provider_for(&config);
        let Some(session) = self.active_session_mut() else {
            // The active session vanished (e.g. deleted by a concurrent CLI
            // command) before the restart fired — degrade to a no-op.
            self.new_session.restart = false;
            return;
        };
        session.set_provider(provider);
        let session_id = session.info.id;
        match session.restart(&config, rows, cols) {
            Ok(()) => {
                // The measured tree belonged to the pane just replaced — on a
                // load it was the ghost's `—`. Back to unknown until the next
                // scan prices the new pane.
                session.info.memory = None;
                // Re-spawned fresh: clear stale hook-driven status so it doesn't
                // linger as Blocked/Working/Done until the agent re-reports (a
                // resumed agent may not re-fire its boot hook). Mirrors the
                // headless `restart_session_headless` path.
                let _ = self.db.clear_hook_state(session_id);
                // The agent process is running (again) — the row is no longer
                // unloaded. A no-op for plain restarts (flag already clear).
                let _ = self.db.set_session_unloaded(session_id, false);
                // A scan started before the relaunch measured the dead pane's
                // tree; drop it rather than let it overwrite the fresh one.
                self.memory_refresh.cancel();
                // Our own write doesn't move this connection's `data_version`,
                // so force the status cache to reload and pick up the cleared row.
                self.invalidate_hook_state_cache();
                self.save_state();
                self.set_status(StatusLevel::Info, success_msg.to_string());
            }
            Err(e) => {
                error!("Failed to restart session: {e}");
                self.set_error(format!("Failed to restart session: {e:#}"));
            }
        }
        self.new_session.restart = false;
    }

    /// Unload the active session: save its ghost frame (the visible screen),
    /// kill the agent window (and shell
    /// pane), and swap in a greyed ghost in place. This is what actually frees
    /// memory — the agent *process* (hundreds of MB) dies; the frozen frame
    /// costs a few KB. Enter / restart loads the session again, resuming the
    /// conversation where the agent supports it.
    pub(crate) fn unload_active_session(&mut self) {
        let already = match self.sessions.get(self.active_index) {
            None => return,
            Some(s) => s.is_placeholder(),
        };
        if already {
            self.set_status(StatusLevel::Info, "Session is not loaded");
            return;
        }
        // Persist the full row first: the frame/flag writes below UPDATE the
        // row, and the ghost swap makes save_state skip this session afterwards.
        self.save_state();

        let (id, name, shared, frame) = {
            let session = &self.sessions[self.active_index];
            (
                session.info.id,
                session.info.name.clone(),
                self.session_to_shared(session),
                session.capture_unload_frame(),
            )
        };
        if let Some((rows, cols, bytes)) = &frame {
            if let Err(e) = self.db.save_session_frame(id, *rows, *cols, bytes) {
                error!("Failed to save ghost frame for '{name}': {e}");
            }
        }

        // Tear the agent down BEFORE any state that claims it is gone — the
        // `unloaded` flag and the ghost swap. A ghost asserts "this session's
        // process is not running": reaching that state after a failed kill
        // would strand a live agent still holding the memory the unload exists
        // to reclaim, with no row left pointing at it. On failure the session
        // is untouched and stays live (the backend leaves the pane registered
        // when its kill fails), so the user can retry or investigate.
        // Saving the frame above is safe either way — the crash-safety
        // debounce writes frames for live sessions too.
        if let Err(e) = self.sessions[self.active_index].kill_checked() {
            error!("Failed to unload session '{name}': {e}");
            self.set_error(format!("Failed to unload '{name}': {e:#}"));
            return;
        }
        if let Err(e) = self.db.set_session_unloaded(id, true) {
            error!("Failed to flag session '{name}' unloaded: {e}");
        }

        // Ghost from the just-captured bytes (not a DB re-read) so the swap
        // works even if the frame write failed.
        let (info, backend, provider) = self.persisted_session_parts(&shared);
        let (rows, cols) = self.content_area_size();
        let ghost = Session::ghost(
            info,
            rows,
            cols,
            &backend,
            &provider,
            HashMap::new(),
            frame.as_ref().map(|(_, _, bytes)| bytes.as_slice()),
        );
        // Already killed above; dropping `old` retires the (now EOF'd) reader.
        let _old = std::mem::replace(&mut self.sessions[self.active_index], ghost);
        // The kill succeeded, so the absence is measured, not guessed: flip the
        // badge to `—` in this frame instead of up to a cadence later. A scan
        // in flight still holds the live figure, so drop it first.
        self.memory_refresh.cancel();
        self.sessions[self.active_index].info.memory =
            Some(crate::session::SessionMemory::Unloaded);
        // Back to the agent view: the companion shell pane died with the
        // session and is deliberately not restored on load, so a remembered
        // Shell tab would label the ghost's frozen frame "Shell" and route the
        // load-time keystrokes to a pane that no longer exists.
        self.session_terminal_views.remove(&id);
        // Leave the pane the way `FocusBackward` would: a ghost has no live
        // PTY, so keeping terminal focus would point the keyboard at a surface
        // that only answers "press Enter to load". The list is where the next
        // action (pick another session, or Enter to load this one back) lives
        // — unless it is collapsed, in which case the frozen pane stays focused.
        self.focus = self.focus_fallback();
        self.on_focus_changed();
        self.request_redraw();
        self.set_status(
            StatusLevel::Info,
            format!("Unloaded '{name}' — press Enter to load it again"),
        );
    }

    /// Whether the active session is a ghost (unloaded, Enter loads it).
    pub(crate) fn active_session_is_ghost(&self) -> bool {
        self.sessions
            .get(self.active_index)
            .is_some_and(|s| s.is_ghost())
    }

    /// Cycle the active session among **loaded** sessions only (skipping
    /// ghosts and unreachable placeholders), in rendered order, wrapping.
    /// From a ghost it keeps going in the requested direction to the nearest
    /// loaded session — the ghost holds its place in the order, it just can't
    /// be landed on.
    pub(crate) fn switch_loaded_session(&mut self, forward: bool) {
        // The *unfiltered* order, so a placeholder still occupies its slot and
        // the scan below leaves it from the right side.
        let order = self.render_order_indices();
        let len = order.len();
        let next = match order.iter().position(|&i| i == self.active_index) {
            Some(pos) => (1..=len)
                .map(|step| {
                    if forward {
                        (pos + step) % len
                    } else {
                        (pos + len - step) % len
                    }
                })
                .find(|&p| !self.sessions[order[p]].is_placeholder()),
            // Active session isn't rendered at all (nothing to scan from):
            // land on the first loaded row.
            None => (0..len).find(|&p| !self.sessions[order[p]].is_placeholder()),
        };
        match next {
            Some(p) => self.set_active_index(order[p]),
            None => self.set_status(StatusLevel::Info, "No loaded sessions"),
        }
    }

    /// Open the active session's worktree (or cwd) in the configured editor.
    fn open_active_in_editor(&mut self) {
        if self.try_open_selected_file() {
            return;
        }
        let paths = match self.collect_active_session_paths() {
            Some(p) => p,
            None => return,
        };
        self.launch_editor(&paths, Some(format!("{} path(s)", paths.len())));
    }

    /// If the file viewer is focused on a file, open `[root, file]` so editors
    /// open the workspace and highlight the file. Returns true if handled.
    fn try_open_selected_file(&mut self) -> bool {
        if self.focus != InputFocus::FileViewer {
            return false;
        }
        let Some((file, root)) = self.file_viewer.selected_file_with_root() else {
            return false;
        };
        let subject = file
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.launch_editor(&[root, file.clone()], Some(subject));
        true
    }

    fn collect_active_session_paths(&mut self) -> Option<Vec<std::path::PathBuf>> {
        let Some(session) = self.sessions.get(self.active_index) else {
            self.set_error("No active session");
            return None;
        };
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for wt in &session.info.worktrees {
            if !paths.contains(&wt.worktree_path) {
                paths.push(wt.worktree_path.clone());
            }
        }
        if paths.is_empty() {
            if let Some(cwd) = session.info.cwd.clone() {
                paths.push(cwd);
            }
        }
        for dir in &session.info.additional_dirs {
            if !paths.contains(dir) {
                paths.push(dir.clone());
            }
        }
        if paths.is_empty() {
            self.set_error("Active session has no worktree or cwd to open");
            return None;
        }
        Some(paths)
    }

    /// Launch the configured editor over `paths`. Terminal editors (vim, nano,
    /// `ttt`, …) get a real TTY: this stages an [`EditorInvocation`] for the
    /// main loop (popup inside tmux, suspend outside) instead of spawning.
    /// GUI editors spawn detached (the classic path). `subject` flavors the
    /// status toast (e.g. the file name or "N path(s)").
    fn launch_editor(&mut self, paths: &[std::path::PathBuf], subject: Option<String>) {
        let Some(editor) = helpers::resolve_editor_command(&self.db) else {
            self.set_error(EDITOR_NOT_CONFIGURED);
            return;
        };
        let mode = helpers::resolve_editor_mode(&self.db);
        match helpers::classify_editor(paths, &editor, mode) {
            Ok(helpers::EditorLaunch::Detached) => match helpers::open_in_editor(paths, &editor) {
                Ok(()) => self.set_info(format!(
                    "Opening {} in {editor}",
                    subject.as_deref().unwrap_or("paths")
                )),
                Err(e) => self.set_error(format!("Failed to launch editor `{editor}`: {e}")),
            },
            Ok(helpers::EditorLaunch::Terminal(inv)) => {
                // GUI-style toast, but defer the actual run to the main loop so
                // the editor gets a real TTY (popup/suspend), not a null-stdio
                // spawn. Cleared by `take_pending_editor_run` after it runs.
                self.pending_editor_run = Some(inv);
                self.set_info(format!(
                    "Opening {} in {editor} (terminal)",
                    subject.as_deref().unwrap_or("paths")
                ));
            }
            Err(e) => self.set_error(format!("Failed to launch editor `{editor}`: {e}")),
        }
    }

    /// Pop a terminal-editor invocation staged by `launch_editor`, if any.
    /// Drained by `run_loop` after `update` so the render loop can hand the
    /// TTY to the editor (popup/suspend) and repaint on return.
    pub fn take_pending_editor_run(&mut self) -> Option<EditorInvocation> {
        self.pending_editor_run.take()
    }

    fn fork_active_session(&mut self) {
        let Some(session) = self.sessions.get(self.active_index) else {
            return;
        };

        let agent = session.info.agent.clone();
        let cwd = session.info.cwd.clone();
        let worktrees = session.info.worktrees.clone();
        let source_name = session.info.name.clone();
        let fork_session_id = session.info.agent_session_id.clone();

        let config = SessionConfig {
            resume_session_id: None,
            agent_session_id: None,
            cwd,
            agent,
            fork_session_id,
            ..SessionConfig::default()
        };

        self.new_session.spawn_config = Some(config);
        self.new_session.spawn_worktrees = worktrees;
        self.new_session.fork = true;
        self.new_session.parent_session_id = Some(session.info.id);

        let mut sn = modals::SessionNameModal::default();
        sn.name.set(&format!("{source_name}-fork"));
        self.modal = modals::Modal::SessionName(sn);
    }

    fn close_active_session(&mut self) {
        if self.sessions.is_empty() {
            return;
        }

        let Some(session) = self.sessions.get(self.active_index) else {
            return;
        };

        let session_id = session.info.id;

        // An *unreachable remote* placeholder has no live pane / local resource
        // to tear down — deleting it just removes the row (a hard delete would
        // run blocking remote git/worktree ops against the down host). Always
        // soft-delete it, regardless of the `soft_delete` feature flag. A ghost
        // is a placeholder too, but it owns real local resources (worktrees), so
        // it follows the configured delete policy like any loaded session.
        if session.is_placeholder() && !session.is_ghost() {
            if let Err(e) = self.db.soft_delete_session(session_id) {
                error!("Failed to soft-delete session in DB: {e}");
            }
            let removed = self.sessions.remove(self.active_index);
            let name = removed.info.name.clone();
            self.session_terminal_views.remove(&session_id);
            self.code_reviews.remove(&session_id);
            self.sync_active_session_to_project();
            self.finalize_pending_delete();
            self.pending_delete = Some(PendingDelete {
                session: removed,
                session_id,
                created_at: clock::now(),
            });
            self.set_status(
                StatusLevel::Info,
                format!("Deleted '{name}'. Ctrl+Z to undo"),
            );
            self.save_state();
            return;
        }

        // When soft-delete is disabled, a TUI delete is a destructive hard
        // delete (kills the tmux window, removes worktrees) with no Ctrl+Z
        // undo. Confirm before tearing anything down only when the session has
        // work at risk (uncommitted changes / unmerged commits, or a state we
        // can't verify); a known-clean session is deleted straight away.
        if !self.features.soft_delete {
            match self.assess_delete_risk(session) {
                Some(risk) => {
                    self.modal = modals::Modal::ConfirmDelete(modals::ConfirmDeleteModal {
                        session_id,
                        session_name: session.info.name.clone(),
                        risk,
                    });
                }
                None => self.confirm_hard_delete_session(session_id),
            }
            return;
        }

        if let Err(e) = self.db.soft_delete_session(session_id) {
            error!("Failed to soft-delete session in DB: {e}");
        }

        // Remove from the list only — do NOT kill the backend or remove
        // worktrees yet (Ctrl+Z undo / Ctrl+U restore reuse them).
        let removed_session = self.sessions.remove(self.active_index);
        let session_name = removed_session.info.name.clone();

        self.session_terminal_views.remove(&session_id);
        self.code_reviews.remove(&session_id);

        self.sync_active_session_to_project();

        // Finalize any existing pending delete before storing the new one.
        self.finalize_pending_delete();

        self.pending_delete = Some(PendingDelete {
            session: removed_session,
            session_id,
            created_at: clock::now(),
        });

        self.set_status(
            StatusLevel::Info,
            format!("Deleted '{session_name}'. Ctrl+Z to undo"),
        );

        // Sync to shared state for other instances
        self.save_state();
    }

    /// Assess what a hard delete of `session` would destroy, so a clean session
    /// can skip the confirmation prompt. Returns `None` when the session is
    /// known-clean (delete silently) or `Some(risk)` describing the uncommitted
    /// changes / unmerged commits to confirm. Remote-host sessions can't be
    /// inspected cheaply, so they always confirm (`DeleteRisk::unknown`).
    fn assess_delete_risk(&self, session: &Session) -> Option<modals::DeleteRisk> {
        if session.info.remote_host.is_some() {
            return Some(modals::DeleteRisk::unknown());
        }

        // Inspect each worktree friring would tear down; for a non-worktree
        // session fall back to its cwd (the live agent's working dir).
        let paths: Vec<std::path::PathBuf> = if session.info.worktrees.is_empty() {
            session.info.cwd.iter().cloned().collect()
        } else {
            session
                .info
                .worktrees
                .iter()
                .map(|w| w.worktree_path.clone())
                .collect()
        };

        let stats: Vec<_> = paths
            .iter()
            .map(|p| crate::git::worktree_stats(p))
            .collect();
        modals::DeleteRisk::from_stats(&stats)
    }

    /// Hard-delete a session after the confirmation prompt (soft_delete off):
    /// soft-delete the row + disable pending sends on the UI thread (fast
    /// SQLite writes) so the modal closes and the row vanishes immediately,
    /// then defer the slow tmux `kill-window` + `git worktree remove` +
    /// symlink-workspace cleanup to a background task. There is no Ctrl+Z
    /// undo (the row stays restorable via Ctrl+U, which re-spawns fresh) —
    /// the confirmation modal is the safety net instead.
    fn confirm_hard_delete_session(&mut self, session_id: SessionId) {
        let Some(idx) = self.sessions.iter().position(|s| s.info.id == session_id) else {
            return;
        };

        // Snapshot the shared row before the soft-delete so the background
        // teardown still has the window name + worktrees + agent_session_id.
        let shared = match self.db.get_session_by_id(session_id) {
            Ok(opt) => opt,
            Err(e) => {
                error!("Hard-delete lookup for session {session_id} failed: {e}");
                None
            }
        };

        if let Err(e) = self.db.disable_send_automations_for_session(session_id) {
            error!("Failed to disable pending sends for session {session_id}: {e}");
        }
        if let Err(e) = self.db.soft_delete_session(session_id) {
            error!("Failed to soft-delete session {session_id}: {e}");
        }
        // Flag as force-deleted so the Ctrl+U restore list tags + blocks it —
        // its worktrees (and any uncommitted work) are gone with the teardown
        // below, so it can't be coherently restored.
        if let Err(e) = self.db.mark_session_force_deleted(session_id) {
            error!("Failed to mark session {session_id} force-deleted: {e}");
        }

        let removed_session = self.sessions.remove(idx);
        let session_name = removed_session.info.name.clone();
        self.session_terminal_views.remove(&session_id);
        self.code_reviews.remove(&session_id);

        if self.active_index >= self.sessions.len() {
            self.active_index = self.sessions.len().saturating_sub(1);
        }
        self.sync_active_session_to_project();

        // Drop the live PTY connection; the tmux window itself is killed by
        // the background teardown below.
        removed_session.kill();

        if let Some(shared) = shared {
            tokio::task::spawn_blocking(move || {
                let mut report = crate::session_ops::delete::ForceDeleteReport::default();
                crate::session_ops::delete::teardown_runtime_resources(&shared, &mut report);
            });
        }

        self.set_status(
            StatusLevel::Info,
            format!("Permanently deleted '{session_name}'"),
        );

        self.save_state();
    }

    /// Recreate git worktrees from shared worktree metadata.
    ///
    /// For each worktree whose branch still exists, runs `git worktree add`
    /// to restore it. Returns the successfully recreated worktrees.
    fn recreate_worktrees(worktrees: &[SharedWorktree]) -> Vec<WorktreeInfo> {
        let mut infos = Vec::new();
        for wt in worktrees {
            if git::branch_exists(&wt.repo_path, &wt.branch) {
                match git::add_existing_worktree(&wt.repo_path, &wt.branch) {
                    Ok(wt_path) => {
                        infos.push(WorktreeInfo {
                            repo_path: wt.repo_path.clone(),
                            worktree_path: wt_path,
                            branch: wt.branch.clone(),
                        });
                    }
                    Err(e) => {
                        error!("Failed to recreate worktree for {}: {e}", wt.branch);
                    }
                }
            }
        }
        infos
    }

    /// Finalize a pending delete — kill the backend session.
    ///
    /// Worktrees, containers, and VMs are intentionally preserved on disk
    /// so that restored sessions (Ctrl+U) can reuse them without re-cloning.
    fn finalize_pending_delete(&mut self) {
        if let Some(pending) = self.pending_delete.take() {
            // Clean up derived per-session artifacts (rebuilt on restore): the
            // agent metrics file and the multi-repo symlink workspace. Worktrees
            // are intentionally left on disk for Ctrl+U restore.
            if let Some(ref sid) = pending.session.info.agent_session_id {
                if let Some(metrics_dir) = crate::paths::metrics_directory() {
                    let _ = std::fs::remove_file(metrics_dir.join(format!("{sid}.json")));
                }
                let _ = crate::workspace::remove_workspace(sid);
                // A user-chosen workspace dir lives outside the workspaces
                // root — remove it via its persisted path (guarded: only a
                // symlink-only dir is ever deleted).
                if let Some(ws) = &pending.session.info.workspace_dir {
                    if let Err(e) = crate::workspace::remove_workspace_at(ws) {
                        warn!("failed to remove workspace dir {}: {e}", ws.display());
                    }
                }
                // A remote session's workspace lives on its host (see
                // `git::ensure_remote_workspace`) — tear it down there too, or
                // it leaks forever. Gated on multi-repo so a single-repo delete
                // never pays an ssh/wsl round-trip (no workspace exists).
                let info = &pending.session.info;
                let multi = session_member_dirs(
                    info.cwd.as_deref(),
                    &info.worktrees,
                    &info.additional_dirs,
                )
                .len()
                    >= 2;
                if multi {
                    if let Some(host) = info.remote_host.as_deref().and_then(|n| self.hosts.get(n))
                    {
                        if let Err(e) = crate::git::remove_remote_workspace(host, sid) {
                            warn!("failed to remove remote workspace for {sid}: {e:#}");
                        }
                    }
                }
            }
            pending.session.kill();
        }
    }

    /// Undo the most recent session delete (Ctrl+Z).
    fn undo_delete(&mut self) {
        let Some(pending) = self.pending_delete.take() else {
            return;
        };

        if let Err(e) = self.db.restore_session(pending.session_id) {
            error!("Failed to restore session in DB: {e}");
            self.set_error("Failed to undo delete");
            return;
        }

        let session_name = pending.session.info.name.clone();
        self.sessions.push(pending.session);
        self.set_active_index(self.sessions.len() - 1);
        self.save_state();

        self.set_status(StatusLevel::Success, format!("Restored '{session_name}'"));
    }

    /// Open the theme picker, pre-selecting the currently active theme
    /// (built-in preset or custom from themes.toml).
    fn open_theme_picker(&mut self) {
        let active = self.db.get_active_theme().ok().flatten();
        let entries = crate::ui::theme::all_theme_entries();
        let index = active
            .as_deref()
            .and_then(|name| entries.iter().position(|e| e.name == name))
            .unwrap_or(0);
        let original = crate::ui::theme::current();
        // Opens in navigation mode (no filter), so the match list is every
        // entry and the filtered index equals the full-list index.
        self.modal = modals::Modal::ThemePicker(modals::ThemePickerModal {
            index,
            original,
            filter: None,
            matches: (0..entries.len()).collect(),
        });
    }

    /// Open the Settings panel. The draft reflects the live source of truth:
    /// `self.features` / `self.info_panel_position` for the live-applied values
    /// (so in-session changes show), and `settings::global()` for the scalars +
    /// notifications (read once at startup, never mutated in-process).
    pub(crate) fn open_settings_panel(&mut self) {
        let draft = crate::session::settings::Settings {
            features: self.features,
            info_panel_position: self.info_panel_position,
            review: self.review_settings,
            prefix: self.prefix_settings.clone(),
            ..crate::session::settings::global().clone()
        };
        self.modal = modals::Modal::Settings(modals::SettingsModal::new(draft));
    }

    /// Persist the Settings panel draft to `settings.toml`, apply the live
    /// feature flags immediately, and toast the result. Keeps the modal open on
    /// a write error so edits aren't lost.
    pub(crate) fn submit_settings_panel(&mut self) {
        let (draft, restart) = match self.modal {
            modals::Modal::Settings(ref m) => (m.draft.clone(), m.restart_required_changed()),
            _ => return,
        };
        if let Err(e) = crate::agent::settings_config::save_settings(&draft) {
            self.set_error(format!("Failed to save settings: {e}"));
            return;
        }
        // Live-apply the feature flags that gate UI panels; restart-required
        // settings only take effect from the on-disk file on next launch.
        self.apply_live_settings(&draft);
        // Record our own write so the live-reload poll doesn't re-toast it.
        self.mark_settings_saved();
        self.modal.close();
        if restart {
            self.set_status(
                StatusLevel::Info,
                "Settings saved — some changes apply after restart",
            );
        } else {
            self.set_status(StatusLevel::Success, "Settings saved");
        }
    }

    fn open_restore_sessions_modal(&mut self) {
        match self.db.list_deleted_sessions() {
            Ok(list) => {
                self.modal =
                    modals::Modal::RestoreSessions(modals::RestoreSessionsModal { list, index: 0 });
            }
            Err(e) => {
                error!("Failed to list deleted sessions: {e}");
                self.set_error("Failed to list deleted sessions");
            }
        }
    }

    /// Restore a soft-deleted session: un-delete in DB, recreate worktrees, and spawn.
    ///
    /// Works for force-deleted sessions too, on a best-effort basis: force-delete
    /// removed the worktree directory but not the git branch, so
    /// [`Self::recreate_worktrees`] reattaches each branch that still exists
    /// (uncommitted work was lost on delete). `restore_session` also clears the
    /// `force_deleted` flag. The TUI gates this behind a confirm modal.
    fn restore_deleted_session(&mut self, deleted: DeletedSessionInfo) {
        let was_force_deleted = deleted.force_deleted;
        let wanted_worktrees = deleted.worktrees.len();

        if let Err(e) = self.db.restore_session(deleted.id) {
            error!("Failed to restore session in DB: {e}");
            self.set_error("Failed to restore session");
            return;
        }

        let worktree_infos = Self::recreate_worktrees(&deleted.worktrees);
        let recovered_worktrees = worktree_infos.len();
        let cwd = worktree_infos
            .first()
            .map(|wt| wt.worktree_path.clone())
            .or(deleted.cwd.clone());

        // Re-spawn on the session's *persisted* backend, not the local default:
        // a remote (`ssh:<host>`) session must land on its own host, or its
        // `backend_type` is corrupted (and a remote pane-id could collide with a
        // local one). Skip restore when this instance can't manage that backend.
        let Some(backend) = self.resolve_persisted_backend(&deleted.backend_type) else {
            self.set_error(format!(
                "Cannot restore '{}': backend '{}' is not available on this instance",
                deleted.name, deleted.backend_type
            ));
            return;
        };

        // Reuse the existing SessionId + inject identity/dir env so the restored
        // session's status hooks can attribute their `session signal` (otherwise
        // it renders Idle forever).
        let mut config = Self::restored_session_config(
            deleted.id,
            deleted.agent_session_id.clone(),
            deleted.agent,
            deleted.name.clone(),
            cwd,
            &deleted.backend_type,
        );
        config.resume_session_id = deleted.agent_session_id;

        let session_name = deleted.name.clone();
        let (rows, cols) = self.content_area_size();
        let provider = self.launch_provider_for(&config);

        match Session::spawn(
            session_name.clone(),
            rows,
            cols,
            &config,
            &backend,
            &provider,
        ) {
            Ok(mut session) => {
                session.info.id = deleted.id;
                session.info.worktrees = worktree_infos;
                session.info.parent_session_id = deleted.parent_session_id;
                // `DeletedSessionInfo` doesn't carry display_order: a restored
                // session simply re-appends at the end of its repo group.
                resolve_repo_display_names(&mut session.info);
                self.sessions.push(session);
                self.set_active_index(self.sessions.len() - 1);
                self.focus = InputFocus::Terminal;

                self.save_state();

                if was_force_deleted {
                    // Recovery is lossy: note it, and flag any worktree whose
                    // branch was gone (so couldn't be reattached).
                    let mut msg =
                        format!("Restored '{session_name}' (best-effort: uncommitted work lost");
                    if recovered_worktrees < wanted_worktrees {
                        msg.push_str(&format!(
                            ", {recovered_worktrees} of {wanted_worktrees} worktrees recovered"
                        ));
                    }
                    msg.push(')');
                    self.set_status(StatusLevel::Info, msg);
                } else {
                    self.set_status(StatusLevel::Success, format!("Restored '{session_name}'"));
                }
            }
            Err(e) => {
                error!("Failed to spawn restored session: {e}");
                self.set_error(format!("Failed to restore session: {e:#}"));
            }
        }
    }

    /// Apply shared session metadata to a local session info.
    /// Used when updating or adopting sessions from shared state.
    fn apply_shared_session_metadata(session: &mut Session, shared: &sync::SharedSession) {
        session.info.name = shared.name.clone();
        session.info.agent = shared.agent.clone();
        session.info.cwd = shared.cwd.clone();
        session.info.additional_dirs = shared.additional_dirs.clone();
        session.info.workspace_dir = shared.workspace_dir.clone();
        session.info.agent_session_id = shared.agent_session_id.clone();
        session.info.worktrees = shared.worktrees.iter().cloned().map(Into::into).collect();
        session.info.parent_session_id = shared.parent_session_id;
        session.info.display_order = shared.display_order;
        resolve_repo_display_names(&mut session.info);
    }

    pub fn update(&mut self, msg: AppMessage) {
        // Any input event is a potential visual change (a key, a mouse move
        // that re-highlights a row, a paste). Mark the UI dirty so the render
        // loop paints this iteration — keypress-to-screen stays immediate.
        self.request_redraw();

        // `[features] mouse = false`: capture is never enabled, so mouse
        // events shouldn't arrive — drop any that do (defense in depth, and
        // it makes the flag authoritative in tests).
        if !self.features.mouse
            && matches!(
                msg,
                AppMessage::MouseScrollUp { .. }
                    | AppMessage::MouseScrollDown { .. }
                    | AppMessage::MouseClick { .. }
                    | AppMessage::MouseDrag { .. }
                    | AppMessage::MouseUp { .. }
                    | AppMessage::MouseMove { .. }
            )
        {
            return;
        }
        match msg {
            AppMessage::KeyPress(code, mods) => self.handle_key(code, mods),
            AppMessage::AltHeld(held) => self.set_alt_held(held),
            AppMessage::Paste(text) => self.handle_paste(text),
            AppMessage::MouseScrollUp { x, y } => self.handle_mouse_scroll(x, y, true),
            AppMessage::MouseScrollDown { x, y } => self.handle_mouse_scroll(x, y, false),
            AppMessage::MouseClick { x, y, modifiers } => self.handle_mouse_click(x, y, modifiers),
            AppMessage::MouseDrag { x, y } => self.handle_mouse_drag(x, y),
            AppMessage::MouseUp { x, y } => self.handle_mouse_up(x, y),
            AppMessage::MouseMove { x, y } => self.mouse_hover = Some((x, y)),
            AppMessage::Resize(cols, rows) => self.handle_resize(cols, rows),
            AppMessage::ExternalStateChange(delta) => self.handle_external_state_change(delta),
        }
    }

    /// Get the current terminal view for the active session.
    pub(crate) fn active_terminal_view(&self) -> TerminalView {
        self.sessions
            .get(self.active_index)
            .and_then(|s| self.session_terminal_views.get(&s.info.id))
            .copied()
            .unwrap_or(TerminalView::Claude)
    }

    /// The id of the currently selected session, if any.
    pub(crate) fn active_session_id(&self) -> Option<SessionId> {
        self.sessions.get(self.active_index).map(|s| s.info.id)
    }

    /// The active session's open code-review view, if any. The review is stored
    /// per session in [`Self::code_reviews`] so it persists across switches.
    pub(crate) fn active_review(&self) -> Option<&code_review::CodeReviewState> {
        self.code_reviews.get(&self.active_session_id()?)
    }

    /// Mutable [`Self::active_review`].
    pub(crate) fn active_review_mut(&mut self) -> Option<&mut code_review::CodeReviewState> {
        let sid = self.active_session_id()?;
        self.code_reviews.get_mut(&sid)
    }

    /// Keep the central-pane focus consistent with the active session's review:
    /// promote `Terminal`→`CodeReview` when that session has a review open (the
    /// review owns the central pane, so terminal focus is meaningless there), and
    /// demote `CodeReview`→`Terminal` when it doesn't (after switching to a
    /// non-review session). Mirrors how the shell view follows the session; other
    /// focuses (session list, file viewer, …) are left untouched.
    pub(crate) fn sync_review_focus(&mut self) {
        let has_review = self.active_review().is_some();
        match self.focus {
            InputFocus::CodeReview | InputFocus::ReviewFiles if !has_review => {
                self.focus = InputFocus::Terminal
            }
            InputFocus::Terminal if has_review => self.focus = InputFocus::CodeReview,
            _ => {}
        }
    }

    pub(crate) fn with_active_parser(&self, f: impl FnOnce(&mut crate::agent::SessionParser)) {
        if let Some(session) = self.sessions.get(self.active_index) {
            let parser_arc = if self.active_terminal_view() == TerminalView::Shell {
                session.shell_pane.as_ref().map(|sp| &sp.parser)
            } else {
                None
            }
            .unwrap_or(&session.parser);
            if let Ok(mut parser) = parser_arc.lock() {
                f(&mut parser);
            }
        }
    }

    /// Toggle the terminal view between Claude and Shell for the active session.
    /// Lazily spawns the shell pane on first toggle.
    pub(crate) fn toggle_shell_view(&mut self) {
        // A review overlays the central pane; F8/Ctrl+T leaves it straight to
        // the shell (the user's "back to shell" expectation, same as the Shell
        // tab) rather than silently flipping the hidden terminal view behind the
        // review.
        if self.active_review().is_some() {
            self.select_central_tab(CentralTab::Shell);
            return;
        }
        match self.active_terminal_view() {
            TerminalView::Claude => self.show_shell_view(),
            TerminalView::Shell => self.show_agent_view(),
        }
    }

    /// Switch the active session's terminal view back to the agent CLI.
    fn show_agent_view(&mut self) {
        if let Some(sid) = self.active_session_id() {
            self.session_terminal_views
                .insert(sid, TerminalView::Claude);
        }
    }

    /// Switch the active session's terminal view to its shell, creating the
    /// shell pane on first use. Started in the same launch cwd as the agent (the
    /// multi-repo workspace when there is one), so switching lands you there.
    fn show_shell_view(&mut self) {
        let Some(session) = self.sessions.get(self.active_index) else {
            return;
        };
        let session_id = session.info.id;
        // A placeholder owns no pane, and placeholder teardown (`kill`/`detach`)
        // skips shell panes — spawning one here would leak a window nothing ever
        // closes.
        if session.is_placeholder() {
            self.set_status(StatusLevel::Info, "Session is not loaded");
            return;
        }
        if session.shell_pane.is_none() {
            let (rows, cols) = self.content_area_size();
            // Resolve the launch cwd (host-aware for remote workspaces) before
            // taking the mutable session borrow; the immutable borrow of
            // `self.sessions` ends with this block. The *non-building* variant:
            // the agent is running in the workspace, and the ensure-style
            // rebuild would rm -rf its cwd out from under it.
            let shell_cwd = {
                let idx = self.active_index;
                match self.sessions.get(idx) {
                    Some(s) => self.session_process_cwd_existing(&s.info),
                    None => None,
                }
            };
            let Some(session) = self.active_session_mut() else {
                // Active session removed concurrently — nothing to switch to.
                return;
            };
            if let Err(e) = session.ensure_shell_pane(rows, cols, shell_cwd.as_deref()) {
                error!("Failed to create shell pane: {e}");
                self.set_error(format!("Failed to create shell: {e:#}"));
                return;
            }
            self.save_state();
        }
        self.session_terminal_views
            .insert(session_id, TerminalView::Shell);
    }

    /// Select a central-pane view from the top-border tab strip. Unlike the
    /// keyboard toggles this *selects* `tab` unambiguously: switching to
    /// Agent/Shell closes any open review first, and Review opens the review if
    /// it isn't already. Feature-gated tabs aren't rendered, so a click on a
    /// disabled view can't arrive here.
    pub(crate) fn select_central_tab(&mut self, tab: CentralTab) {
        match tab {
            CentralTab::Agent => {
                self.close_central_overlays();
                self.show_agent_view();
                self.focus_central_terminal();
            }
            CentralTab::Shell => {
                self.close_central_overlays();
                self.show_shell_view();
                self.focus_central_terminal();
            }
            CentralTab::Review => {
                if self.active_cc_activity().is_some() {
                    self.close_cc_activity();
                }
                if self.active_review().is_none() {
                    self.toggle_code_review();
                }
                // `toggle_code_review` can fail to open (e.g. no worktree); only
                // grab focus when a review is actually present.
                if self.active_review().is_some() && self.focus != InputFocus::CodeReview {
                    self.focus = InputFocus::CodeReview;
                    self.on_focus_changed();
                }
            }
            CentralTab::CcActivity => {
                if self.active_cc_activity().is_none() {
                    // `toggle_cc_activity` closes any open review first.
                    self.toggle_cc_activity();
                }
                if self.active_cc_activity().is_some()
                    && !matches!(
                        self.focus,
                        InputFocus::CcActivity | InputFocus::CcActivityTree
                    )
                {
                    self.focus = InputFocus::CcActivityTree;
                    self.on_focus_changed();
                }
            }
        }
    }

    /// Close whichever central-pane overlay is open (review or activity view),
    /// so switching to the Agent/Shell tab leaves a clean terminal.
    fn close_central_overlays(&mut self) {
        if self.active_review().is_some() {
            self.close_code_review();
        }
        if self.active_cc_activity().is_some() {
            self.close_cc_activity();
        }
    }

    /// Focus the central terminal pane (shared by the Agent/Shell tab clicks).
    fn focus_central_terminal(&mut self) {
        if self.focus != InputFocus::Terminal {
            self.focus = InputFocus::Terminal;
            self.on_focus_changed();
        }
    }

    /// The central-pane tab currently shown: Review wins (it overlays the pane),
    /// else the per-session terminal view.
    pub(crate) fn active_central_tab(&self) -> CentralTab {
        if self.active_review().is_some() {
            CentralTab::Review
        } else if self.active_cc_activity().is_some() {
            CentralTab::CcActivity
        } else if self.active_terminal_view() == TerminalView::Shell {
            CentralTab::Shell
        } else {
            CentralTab::Agent
        }
    }

    pub(crate) fn scroll_terminal_up(&mut self, lines: usize) {
        self.text_selection = None;
        self.with_active_parser(|parser| {
            let current = parser.screen().scrollback();
            parser.screen_mut().set_scrollback(current + lines);
        });
    }

    pub(crate) fn scroll_terminal_down(&mut self, lines: usize) {
        self.text_selection = None;
        self.with_active_parser(|parser| {
            let current = parser.screen().scrollback();
            parser
                .screen_mut()
                .set_scrollback(current.saturating_sub(lines));
        });
    }

    pub(crate) fn page_scroll_amount(&self) -> usize {
        let (rows, _) = self.content_area_size();
        (rows as usize) / 2
    }

    fn handle_mouse_click(&mut self, x: u16, y: u16, modifiers: KeyModifiers) {
        // A modal captures every click: a hit on one of its rows acts on it,
        // anything else is swallowed — clicks never reach the scrollbars,
        // panes, or selection beneath an overlay.
        if !matches!(self.modal, modals::Modal::None) {
            self.handle_modal_click(x, y);
            return;
        }

        let areas = self.screen_layout();
        let border_block = Block::default().borders(Borders::ALL);

        // Ctrl+Click: URL opening (terminal-relative, existing behavior)
        if modifiers.contains(KeyModifiers::CONTROL) {
            self.text_selection = None;
            self.open_ctrl_clicked_url(border_block.inner(areas.terminal), x, y);
            return;
        }

        // Grab a scrollbar thumb: a click on any rendered track starts a drag of
        // that pane's scroll state (and never starts a text selection).
        if self.try_grab_scrollbar(x, y, None) {
            return;
        }

        // While the global-search popup is open it owns all input (it is
        // entered/left only via its keybinding / Esc / Enter), so plain
        // clicks are swallowed rather than stealing focus from it.
        if self.global_search.active {
            return;
        }

        // Row / pane targets recorded by the last view() (first match wins:
        // rows are recorded before their pane's whole-rect fallback). A
        // consumed click stops here; session-list and terminal clicks fall
        // through so the same press still arms text selection.
        let pos = Position::new(x, y);
        if let Some((action, rect)) = self
            .click_targets
            .iter()
            .find(|t| t.rect.contains(pos))
            .map(|t| (t.action, t.rect))
        {
            // A diff-row click carries its column so a paired side-by-side row
            // can steer a follow-up comment to the old/new side it hit.
            if let ClickAction::ReviewRow(i) = action {
                self.focus = InputFocus::CodeReview;
                self.cr_click_row(i, x.saturating_sub(rect.x), rect.width);
                return;
            }
            if self.activate_click_target(action) {
                return;
            }
        }

        // Find which pane was clicked; use inner area (excluding borders).
        let pane_rects = [Some(areas.terminal), areas.left_panel, areas.info_panel];
        let pane_inner = pane_rects
            .into_iter()
            .flatten()
            .find(|r| r.contains(pos))
            .map(|r| border_block.inner(r));

        let Some(inner) = pane_inner else {
            self.text_selection = None;
            return;
        };

        let pane = PaneBounds::from_rect(inner);
        let anchor = TermPos {
            row: y as usize,
            col: x as usize,
        };
        self.text_selection = Some(Selection::new(anchor, pane));
    }

    /// Open the URL under a Ctrl+Click inside the terminal pane, if any.
    /// `inner` is the terminal's content area (borders excluded); a click
    /// outside it (or with no URL at that cell) is a no-op.
    fn open_ctrl_clicked_url(&mut self, inner: Rect, x: u16, y: u16) {
        use crate::ui::links;

        if !inner.contains(Position::new(x, y)) {
            return;
        }
        let screen_col = (x - inner.x) as usize;
        let screen_row = (y - inner.y) as usize;
        self.with_active_parser(|parser| {
            let rows = links::extract_screen_rows(parser.screen());
            let detected = links::detect_urls(&rows);
            if let Some(url) = links::url_at_position(&detected, screen_row, screen_col) {
                helpers::open_url(url);
            }
        });
    }

    /// Route a click while a modal is open: a hit on a recorded row selects
    /// it and immediately activates it with the row's primary key (replayed
    /// through the modal's own key handler so side effects match the keyboard
    /// path). Every other click — inside or outside the overlay — is
    /// swallowed, so a stray click can never discard typed input or fall
    /// through to the panes beneath.
    fn handle_modal_click(&mut self, x: u16, y: u16) {
        // The F1 editor consumes the next *keypress* while capturing; clicks
        // are ignored so they can't be mistaken for a chord.
        if self.help_is_capturing() {
            return;
        }
        // The modal's own scrollbar is grabbable (recorded under
        // `ScrollTarget::Modal`); the pane scrollbars beneath the overlay are
        // not.
        if self.try_grab_scrollbar(x, y, Some(ScrollTarget::Modal)) {
            return;
        }

        // Each `try_*` block filters the click registry to its own action type
        // (the registry also holds pane targets beneath the overlay; a plain
        // first-match would hit those and swallow the click). Their rects never
        // overlap, so the order is priority-for-clarity. First one to consume the
        // click wins.
        let pos = Position::new(x, y);
        if self.try_modal_button_click(pos) {
            return;
        }
        if self.try_modal_field_click(pos) {
            return;
        }
        if self.try_convo_focus_click(pos) {
            return;
        }
        self.try_modal_row_click(pos);
    }

    /// Footer buttons (`[ Save ]` / `[ Cancel ]` / …) replay their key through
    /// the modal's own handler, so a click is identical to the keypress.
    fn try_modal_button_click(&mut self, pos: Position) -> bool {
        let Some((code, mods)) = self.click_targets.iter().find_map(|t| match t.action {
            ClickAction::ModalButton { code, mods } if t.rect.contains(pos) => Some((code, mods)),
            _ => None,
        }) else {
            return false;
        };
        if matches!(self.modal, modals::Modal::Help(_)) {
            self.handle_help_key(code, mods);
        } else {
            self.handle_modal_key_if_open(code, mods);
        }
        true
    }

    /// Editor-field clicks select that field (no key replay — the user then
    /// adjusts/types with the keyboard, exactly as after Tab/↑↓). A Settings
    /// boolean row also toggles on click (its whole point is the on/off switch;
    /// scalar rows only select, so a stray click can't change a number).
    fn try_modal_field_click(&mut self, pos: Position) -> bool {
        let Some(index) = self.click_targets.iter().find_map(|t| match t.action {
            ClickAction::ModalField(i) if t.rect.contains(pos) => Some(i),
            _ => None,
        }) else {
            return false;
        };
        self.select_modal_field(index);
        if let modals::Modal::Settings(s) = &mut self.modal {
            if !s.field.is_scalar() {
                s.toggle();
            }
        }
        true
    }

    /// Conversation picker: clicking the search / directory field focuses it.
    fn try_convo_focus_click(&mut self, pos: Position) -> bool {
        let Some(focus) = self.click_targets.iter().find_map(|t| match t.action {
            ClickAction::ConvoFocus(focus) if t.rect.contains(pos) => Some(focus),
            _ => None,
        }) else {
            return false;
        };
        if let modals::Modal::ConversationPicker(ref mut cp) = self.modal {
            cp.focus = focus;
        }
        true
    }

    /// A list-row click selects the row and replays its activation chord.
    fn try_modal_row_click(&mut self, pos: Position) {
        let Some(row) = self.click_targets.iter().find_map(|t| match t.action {
            ClickAction::ModalRow(row) if t.rect.contains(pos) => Some(row),
            _ => None,
        }) else {
            return;
        };
        let Some((code, mods)) = self.select_modal_row(row) else {
            return;
        };
        if matches!(self.modal, modals::Modal::Help(_)) {
            self.handle_help_key(code, mods);
        } else {
            self.handle_modal_key_if_open(code, mods);
        }
    }

    /// Move the open modal's selection to `row` (a row index recorded by this
    /// frame's renderer, so it is always in bounds) and return the key chord
    /// that activates a row there (see [`modals::Modal::list_selection`]).
    fn select_modal_row(&mut self, row: usize) -> Option<(KeyCode, KeyModifiers)> {
        // The conversation picker routes keys by its internal focus; a row
        // click always means the list (mirrors the keyboard path), so force it
        // before moving.
        if let modals::Modal::ConversationPicker(ref mut cp) = self.modal {
            cp.focus = cc_import::ConversationPickerFocus::List;
        }
        let (index, code, mods) = self.modal.list_selection()?;
        *index = row;
        Some((code, mods))
    }

    /// Select the index-th field of the active editor modal (its position in
    /// that modal's visible field order), so a click focuses a field exactly
    /// like Tab/↑↓ would. No-op for modals without a field list.
    fn select_modal_field(&mut self, index: usize) {
        match &mut self.modal {
            modals::Modal::Settings(s) => {
                if let Some(&field) = modals::SettingsField::ORDER.get(index) {
                    s.field = field;
                }
            }
            modals::Modal::AutomationEditor(a) => {
                if let Some(&field) = a.visible_fields().get(index) {
                    a.field = field;
                }
            }
            _ => {}
        }
    }

    /// Act on a clicked target. Returns `true` when the click is fully
    /// consumed; `false` lets the caller continue to text-selection arming
    /// (terminal / session-list / info panes keep their drag-select).
    fn activate_click_target(&mut self, action: ClickAction) -> bool {
        match action {
            ClickAction::SelectSession(display_idx) => {
                if let Some(&idx) = self.visible_order_indices().get(display_idx) {
                    self.set_active_index(idx);
                }
                // Clicking a row is *activation*, not list management: land in
                // the terminal (like Enter / a notification click) so typing
                // reaches the agent instead of the list's single-letter
                // hotkeys. The list itself stays reachable via Ctrl+H or a
                // click on its empty area (the whole-rect FocusPane fallback).
                self.focus = InputFocus::Terminal;
                self.on_focus_changed();
                false
            }
            ClickAction::SelectTask(i) => {
                self.focus = InputFocus::TaskList;
                let len = self.task_ui.filtered_task_indices.len();
                if len > 0 {
                    self.task_ui.task_panel_index = i.min(len - 1);
                }
                // Same bookkeeping as entering the panel via the focus cycle
                // (refresh list + in-pane preview). Leaving an in-pane editor
                // this way discards unsaved edits, exactly like Esc/Ctrl+H.
                self.on_focus_changed();
                true
            }
            ClickAction::SelectAutomation(i) => {
                self.focus = InputFocus::Automations;
                let len = self.automation_ui.cached_automations.len();
                if len > 0 {
                    self.automation_ui.automation_panel_index = i.min(len - 1);
                }
                self.refresh_automation_view();
                true
            }
            ClickAction::SelectFileRow(i) => {
                self.focus = InputFocus::FileViewer;
                self.file_viewer.select_index(i);
                // Single click activates, like Enter: toggle a directory,
                // open a file in the editor.
                self.file_viewer_expand();
                true
            }
            ClickAction::FocusPane(focus) => {
                let changed = self.focus != focus;
                self.focus = focus;
                if changed {
                    self.on_focus_changed();
                }
                // Terminal and session-list clicks keep arming drag-select.
                !matches!(focus, InputFocus::Terminal | InputFocus::SessionList)
            }
            // Modal rows/buttons/fields are dispatched by `handle_modal_click`
            // before pane targets are even considered.
            ClickAction::ModalRow(_)
            | ClickAction::ModalButton { .. }
            | ClickAction::ModalField(_)
            | ClickAction::ConvoFocus(_) => true,
            ClickAction::Global(action) => {
                self.dispatch_action(action);
                true
            }
            ClickAction::PaneField { focus, index } => {
                // Enter the editor if not already in it (a fresh sync resets the
                // field), then select the clicked field. When already focused we
                // only move the field — never re-sync — so unsaved edits survive.
                if self.focus != focus {
                    match focus {
                        InputFocus::AutomationEditor => self.enter_automation_editor(),
                        InputFocus::TaskEditor => self.enter_task_editor(),
                        _ => self.focus = focus,
                    }
                }
                self.select_pane_field(focus, index);
                true
            }
            ClickAction::ReviewRow(i) => {
                // A click in the diff body focuses the review pane (the
                // whole-pane `FocusPane` fallback is recorded after the row
                // targets, so it never wins on a row hit).
                self.focus = InputFocus::CodeReview;
                self.cr_select_row(i);
                true
            }
            ClickAction::ReviewButton(button) => {
                self.focus = InputFocus::CodeReview;
                self.cr_button(button);
                true
            }
            ClickAction::ReviewFile(fi) => {
                self.focus = InputFocus::ReviewFiles;
                self.cr_jump_to_file(fi);
                true
            }
            ClickAction::ReviewTarget(i) => {
                self.focus = InputFocus::CodeReview;
                self.cr_select_target(i);
                true
            }
            ClickAction::CcActivityNode(i) => {
                // A click in the tree focuses it and jumps the selection (which
                // previews that node's transcript in the central pane).
                self.focus = InputFocus::CcActivityTree;
                self.ca_jump_to_tree_row(i);
                true
            }
            ClickAction::CcActivityRow(i) => {
                self.focus = InputFocus::CcActivity;
                self.ca_select_row(i);
                true
            }
            ClickAction::CentralTab(tab) => {
                self.select_central_tab(tab);
                true
            }
            ClickAction::CopyStatus => {
                self.copy_status_to_clipboard();
                true
            }
        }
    }

    /// Set the active field of the focused in-pane editor (automation / task) by
    /// its position in the editor's visible field order.
    fn select_pane_field(&mut self, focus: InputFocus, index: usize) {
        match focus {
            InputFocus::AutomationEditor => {
                if let Some(m) = self.automation_ui.automation_editor.as_mut() {
                    if let Some(&field) = m.visible_fields().get(index) {
                        m.field = field;
                    }
                }
            }
            InputFocus::TaskEditor => {
                if let Some(m) = self.task_ui.task_editor.as_mut() {
                    if let Some(&field) = m.visible_fields().get(index) {
                        m.field = field;
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether the F1 editor is mid chord-capture — any key (including a
    /// synthesized one) would become the new binding, so mouse handlers must
    /// stay silent.
    fn help_is_capturing(&self) -> bool {
        matches!(self.modal, modals::Modal::Help(ref h) if h.capturing)
    }

    /// Grab the scrollbar track under the cursor and start dragging it.
    /// `only` restricts which target may be grabbed: modals grab only their
    /// own bar, panes grab any. Returns whether a track was hit.
    fn try_grab_scrollbar(&mut self, x: u16, y: u16, only: Option<ScrollTarget>) -> bool {
        let Some(hit) = self
            .scrollbar_hits
            .iter()
            .find(|h| only.map_or(true, |t| h.target == t) && h.geom.contains(x, y))
        else {
            return false;
        };
        let target = hit.target;
        let pos = hit.geom.position_for_y(y);
        let content_len = hit.geom.content_len;
        self.text_selection = None;
        self.dragging_scrollbar = Some(target);
        self.apply_scrollbar_position(target, pos, content_len);
        true
    }

    fn handle_mouse_drag(&mut self, x: u16, y: u16) {
        // A scrollbar drag takes precedence: keep driving the grabbed pane's
        // scroll state (y can leave the track — `position_for_y` clamps it).
        if let Some(target) = self.dragging_scrollbar {
            if let Some(hit) = self.scrollbar_hits.iter().find(|h| h.target == target) {
                let pos = hit.geom.position_for_y(y);
                let content_len = hit.geom.content_len;
                self.apply_scrollbar_position(target, pos, content_len);
            }
            return;
        }

        if let Some(ref mut sel) = self.text_selection {
            let (cx, cy) = sel.pane.clamp(x, y);
            sel.cursor = TermPos {
                row: cy as usize,
                col: cx as usize,
            };
        }
    }

    fn handle_mouse_up(&mut self, x: u16, y: u16) {
        // End an in-progress scrollbar drag without touching the text selection.
        if self.dragging_scrollbar.take().is_some() {
            return;
        }

        self.handle_mouse_drag(x, y);

        if let Some(ref mut sel) = self.text_selection {
            sel.dragging = false;

            // If anchor == cursor, it was just a click (no drag) — clear selection
            if sel.anchor == sel.cursor {
                self.text_selection = None;
            }
        }
    }

    /// Apply a scrollbar position (in `0..content_len`) to the scroll state it
    /// drives. `content_len` is passed in (read from the hit) so the terminal
    /// arm can invert without re-borrowing `scrollbar_hits` across
    /// `with_active_parser`.
    fn apply_scrollbar_position(&mut self, target: ScrollTarget, pos: usize, content_len: usize) {
        match target {
            ScrollTarget::Terminal => {
                // The scrollbar position is inverted vs. scrollback (0 = bottom):
                // render uses `position = total - scrollback`, so invert back.
                let scrollback = content_len.saturating_sub(pos);
                self.text_selection = None;
                self.with_active_parser(|parser| {
                    parser.screen_mut().set_scrollback(scrollback);
                });
            }
            ScrollTarget::TaskPreview => {
                let max = self.task_preview_max_scroll();
                self.task_ui.task_preview_scroll = (pos as u16).min(max);
            }
            ScrollTarget::FileViewer => {
                self.file_viewer.select_index(pos);
            }
            ScrollTarget::RunHistory => {
                let max = self
                    .automation_ui
                    .cached_automation_runs
                    .len()
                    .saturating_sub(1);
                self.automation_ui.automation_run_index = pos.min(max);
            }
            ScrollTarget::CodeReview => {
                // The review is selection-primary: `render_rows` derives `scroll`
                // from `selected` every frame, so setting `scroll` directly here
                // would snap back. Move the selection instead (matching the wheel
                // + keyboard paths); the scroll offset follows on render.
                self.cr_select_row(pos);
            }
            // Also selection-primary (see `CodeReview`) — move the selection.
            ScrollTarget::CcActivity => self.ca_select_row(pos),
            ScrollTarget::Modal => self.step_modal_selection_to(pos),
        }
    }

    /// Move the open modal's selection to `target` by replaying Up/Down
    /// through its own key handler — keeps each modal's clamping and side
    /// effects (e.g. the theme picker's live preview) identical to keyboard
    /// navigation. Stops as soon as a step no longer makes progress.
    fn step_modal_selection_to(&mut self, target: usize) {
        // While the F1 editor is capturing, any key would be taken as the new
        // chord — never synthesize navigation there.
        if self.help_is_capturing() {
            return;
        }
        loop {
            let Some(current) = self.modal_selected_index() else {
                return;
            };
            if current == target {
                return;
            }
            let key = if target > current {
                KeyCode::Down
            } else {
                KeyCode::Up
            };
            self.synthesize_modal_nav(key);
            if self.modal_selected_index() == Some(current) {
                return; // clamped — can't get closer
            }
        }
    }

    /// Replay a navigation key through the open modal's key handler.
    fn synthesize_modal_nav(&mut self, code: KeyCode) {
        if matches!(self.modal, modals::Modal::Help(_)) {
            self.handle_help_key(code, KeyModifiers::NONE);
        } else {
            self.handle_modal_key_if_open(code, KeyModifiers::NONE);
        }
    }

    /// The open modal's current selection index, when it has a selectable list.
    /// Shares [`modals::Modal::list_selection`] with [`Self::select_modal_row`]
    /// (hence `&mut self`) so the two can't drift onto different modal sets.
    fn modal_selected_index(&mut self) -> Option<usize> {
        self.modal.list_selection().map(|(index, _, _)| *index)
    }

    /// Route a mouse-wheel tick to whichever pane is under the cursor, so the
    /// wheel scrolls the hovered pane (terminal, task preview, file viewer, run
    /// history, or a list pane) rather than always the terminal.
    fn handle_mouse_scroll(&mut self, x: u16, y: u16, up: bool) {
        // An open modal owns the wheel: one selection step per tick (like
        // j/k), never the panes beneath. Capture mode would treat the
        // synthesized key as the new chord, so it stays untouched.
        if !matches!(self.modal, modals::Modal::None) {
            if !self.help_is_capturing() {
                self.synthesize_modal_nav(if up { KeyCode::Up } else { KeyCode::Down });
            }
            return;
        }

        self.scroll_pane(self.pane_at(x, y), up, x, y);
    }

    /// Apply a wheel tick (`up`) to a specific scrollable pane (the terminal
    /// when `pane` is `None`/`Terminal`). `(x, y)` is the cursor position in
    /// screen cells, used to forward mouse coordinates to the inner PTY when
    /// the agent has mouse tracking enabled (Claude Code, vim, htop, …).
    fn scroll_pane(&mut self, pane: Option<ScrollPane>, up: bool, x: u16, y: u16) {
        let step: i32 = if up { -1 } else { 1 };
        match pane {
            Some(ScrollPane::Terminal) | None => {
                // Modern TUIs on the alternate screen (Claude Code, vim, htop,
                // …) enable mouse tracking and handle wheel scrolling
                // themselves; vt100's scrollback is empty on the alt screen so
                // the local fallback would be a silent no-op. Forward instead.
                if self.try_forward_wheel_to_pty(x, y, up) {
                    return;
                }
                if up {
                    self.scroll_terminal_up(MOUSE_SCROLL_LINES);
                } else {
                    self.scroll_terminal_down(MOUSE_SCROLL_LINES);
                }
            }
            Some(ScrollPane::TaskPreview) => {
                self.scroll_task_preview(step * MOUSE_SCROLL_LINES as i32)
            }
            Some(ScrollPane::FileViewer) => self
                .file_viewer
                .move_selection(step * MOUSE_SCROLL_LINES as i32),
            Some(ScrollPane::RunHistory) => self.move_run_history_selection(step),
            Some(ScrollPane::SessionList) => {
                if up {
                    self.switch_session_backward();
                } else {
                    self.switch_session_forward();
                }
            }
            Some(ScrollPane::TasksList) => self.move_task_selection(step),
            Some(ScrollPane::Automations) => self.move_automation_selection(step),
            Some(ScrollPane::CodeReview) => self.cr_move(step as isize),
            Some(ScrollPane::ReviewFiles) => self.cr_jump_file(!up),
            Some(ScrollPane::CcActivity) => self.ca_move(step as isize),
            Some(ScrollPane::CcActivityTree) => self.ca_tree_move(step as isize),
        }
    }

    /// Step the tasks-panel selection by `delta`, clamped, refreshing the preview.
    fn move_task_selection(&mut self, delta: i32) {
        let len = self.task_ui.filtered_task_indices.len();
        if len == 0 {
            return;
        }
        let next = (self.task_ui.task_panel_index as i32 + delta).clamp(0, len as i32 - 1);
        let next = next as usize;
        if next != self.task_ui.task_panel_index {
            self.task_ui.task_panel_index = next;
            self.refresh_task_view();
        }
    }

    /// Hit-test `(x, y)` against the current layout to find the scrollable pane
    /// under the cursor (used for pane-aware wheel scrolling).
    fn pane_at(&self, x: u16, y: u16) -> Option<ScrollPane> {
        let areas = self.screen_layout();
        let pos = Position::new(x, y);
        let hit = |r: Option<Rect>| r.map(|r| r.contains(pos)).unwrap_or(false);

        if hit(areas.file_viewer) {
            // During a review this column hosts the changed-files list; during an
            // activity view it hosts the workflow/subagent tree.
            if self.active_review().is_some() {
                return Some(ScrollPane::ReviewFiles);
            }
            if self.active_cc_activity().is_some() {
                return Some(ScrollPane::CcActivityTree);
            }
            return Some(ScrollPane::FileViewer);
        }
        if hit(areas.tasks_panel) {
            return Some(ScrollPane::TasksList);
        }
        if hit(areas.automations_panel) {
            return Some(ScrollPane::Automations);
        }
        if hit(areas.left_panel) {
            return Some(ScrollPane::SessionList);
        }
        if areas.terminal.contains(pos) {
            // The central pane hosts the terminal, the task preview, or the
            // automation run-history depending on focus.
            return Some(match self.focus {
                InputFocus::TaskList | InputFocus::TaskEditor => ScrollPane::TaskPreview,
                InputFocus::AutomationRunHistory => ScrollPane::RunHistory,
                InputFocus::CodeReview => ScrollPane::CodeReview,
                InputFocus::CcActivity => ScrollPane::CcActivity,
                _ => ScrollPane::Terminal,
            });
        }
        None
    }

    /// Forward a wheel tick to the active session's PTY when the inner agent
    /// has enabled xterm mouse tracking — the convention modern TUIs (Claude
    /// Code, vim, htop, btop, …) use to subscribe to wheel events. Returns
    /// `true` when the event was forwarded so the caller skips the local
    /// scrollback fallback (which is a no-op on the alternate screen anyway).
    ///
    /// Only the SGR encoding (DECSET 1006) is supported: the legacy 1005/utf8
    /// and default encodings cap row/col at 223 and aren't used by anything
    /// that ships in 2024+. Falling back to vt100 scrollback for them is fine.
    fn try_forward_wheel_to_pty(&self, x: u16, y: u16, up: bool) -> bool {
        let Some(session) = self.sessions.get(self.active_index) else {
            return false;
        };

        let view = self.active_terminal_view();
        let parser_arc = if view == TerminalView::Shell {
            session.shell_pane.as_ref().map(|sp| &sp.parser)
        } else {
            None
        }
        .unwrap_or(&session.parser);

        let (mode, encoding) = {
            let Ok(parser) = parser_arc.lock() else {
                return false;
            };
            let screen = parser.screen();
            (
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            )
        };

        if mode == vt100::MouseProtocolMode::None || encoding != vt100::MouseProtocolEncoding::Sgr {
            return false;
        }

        // Map the screen-cell click to 1-based PTY cell coordinates. A wheel
        // tick outside the terminal pane (the cursor is hovering another panel)
        // is left to the local fallback.
        let inner = Block::default()
            .borders(Borders::ALL)
            .inner(self.screen_layout().terminal);
        if !inner.contains(Position::new(x, y)) {
            return false;
        }
        let col = u32::from(x - inner.x) + 1;
        let row = u32::from(y - inner.y) + 1;

        // Xterm wheel buttons: 64 = wheel up, 65 = wheel down. SGR encoding:
        // CSI < Cb ; Cx ; Cy M (press; release would be `m`).
        let button: u32 = if up { 64 } else { 65 };
        let bytes = format!("\x1b[<{button};{col};{row}M").into_bytes();

        let result = if view == TerminalView::Shell {
            // The branch above only set `view = Shell` when the pane exists;
            // unwrap is fine, but keep it defensive.
            session
                .shell_pane
                .as_ref()
                .map(|sp| sp.send_input(bytes))
                .unwrap_or(Ok(()))
        } else {
            session.send_input(bytes)
        };
        if let Err(e) = result {
            tracing::warn!("Failed to forward wheel event to PTY: {e}");
            return false;
        }
        true
    }

    /// Copy `text` to the system clipboard, preferring the native handle and
    /// falling back (see [`clipboard`]) when no display server is reachable,
    /// the native write fails, or the native clipboard belongs to the SSH host
    /// rather than the machine in front of the user. Returns how the copy was
    /// served so the caller's toast can flag the fire-and-forget path.
    ///
    /// Fallback order (see the [`clipboard`] module docs for why): inside tmux,
    /// `tmux load-buffer -w` — the raw OSC 52 an app writes to its own stdout
    /// is dropped by tmux's default `set-clipboard external`, so the escape
    /// must come from tmux itself; outside tmux, raw OSC 52 to stdout.
    pub(crate) fn set_clipboard_text(&mut self, text: &str) -> Result<ClipboardVia, String> {
        // Test capture: recorded, never written anywhere real (see the field).
        #[cfg(test)]
        if let Some(captured) = &mut self.captured_clipboard {
            captured.push(text.to_string());
            return Ok(ClipboardVia::Native);
        }

        // 1. Native display-server clipboard — unless it is the SSH host's:
        //    on macOS, NSPasteboard accepts writes from an SSH login, so the
        //    copy would "succeed" onto a machine the user isn't looking at
        //    while the terminal-routed fallbacks below never run.
        let native = if clipboard::native_clipboard_is_remote() {
            NativeCopy::Skipped
        } else {
            match &mut self.clipboard {
                Some(cb) => match cb.set_text(text) {
                    Ok(()) => return Ok(ClipboardVia::Native),
                    Err(e) => NativeCopy::Failed(e.to_string()),
                },
                None => NativeCopy::Unavailable,
            }
        };

        // 2. Inside tmux: authoritative (real exit status), works under the
        //    default clipboard policy where a raw app OSC 52 would be dropped.
        if std::env::var_os("TMUX").is_some() {
            return clipboard::tmux_copy(text)
                .map(|()| ClipboardVia::Tmux)
                .map_err(|e| Self::clipboard_error(&native, "tmux load-buffer", &e));
        }

        // 3. No display server and no tmux: raw OSC 52 to a direct terminal.
        clipboard::osc52_copy(text)
            .map(|()| ClipboardVia::Osc52)
            .map_err(|e| match e {
                // A size refusal is about the text, not the transport: report
                // it bare rather than as "the OSC 52 stage failed".
                clipboard::Osc52Error::TooLarge { .. } => e.to_string(),
                clipboard::Osc52Error::Write(err) => Self::clipboard_error(&native, "OSC 52", &err),
            })
    }

    /// Compose a clipboard-failure message, prefixing the fallback's own error
    /// with *why* the native path didn't serve the copy — honestly
    /// distinguishing a native write that was tried and failed from one that
    /// was deliberately skipped (so a message never claims native "failed" when
    /// it was never attempted).
    fn clipboard_error(native: &NativeCopy, stage: &str, err: &impl std::fmt::Display) -> String {
        let prefix = match native {
            NativeCopy::Skipped => "Native clipboard skipped (wouldn't reach you over SSH)".into(),
            NativeCopy::Failed(e) => format!("Clipboard write failed: {e}"),
            NativeCopy::Unavailable => "Clipboard not available".to_string(),
        };
        format!("{prefix}; {stage} failed: {err}")
    }

    fn copy_selection_to_clipboard(&mut self) {
        let text = match &self.selected_text_cache {
            Some(t) if !t.is_empty() => t.clone(),
            _ => return,
        };

        match self.set_clipboard_text(&text) {
            Ok(via) => {
                self.text_selection = None;
                self.selected_text_cache = None;
                self.set_status(StatusLevel::Info, via.toast("Copied to clipboard"));
            }
            Err(e) => self.set_error(e),
        }
    }

    /// Route clipboard writes captured from pane output streams (OSC 52 —
    /// Claude Code's `/copy`, nvim's OSC 52 provider, anything in an agent or
    /// shell pane; see `agent::osc52`) through the same clipboard stack as
    /// every other copy surface. In a normal terminal the emulator would honor
    /// the pane's escape itself; here friring *is* that pane's terminal, so it
    /// forwards the copy to wherever the user's clipboard actually is (native,
    /// or the tmux/OSC 52 route over SSH). Any session's panes may copy —
    /// standard OSC 52 semantics, background panes included — so the toast
    /// names the originating session.
    ///
    /// Every pane's queue is drained, but only the **newest** copy is written:
    /// each copy carries a global capture sequence (`agent::backend`'s
    /// `OSC52_SEQ`), so the winner is the one a real terminal would have left
    /// on the clipboard — the older ones are overwritten before anyone can
    /// paste them, and their toasts before anyone can read them. Writing them
    /// all would put up to `PaneClipboard::CAP` blocking `tmux load-buffer`
    /// spawns *per pane* on the event-loop tick (ADR-P: the UI thread does not
    /// block) to reach the same end state.
    fn drain_pane_clipboard_copies(&mut self) {
        let mut copies: Vec<(u64, String, String)> = Vec::new();
        for session in self.sessions.iter_mut() {
            let name = session.info.name.clone();
            for (seq, text) in session.drain_osc52_copies() {
                copies.push((seq, name.clone(), text));
            }
        }
        let Some((_, name, text)) = copies.into_iter().max_by_key(|(seq, _, _)| *seq) else {
            return;
        };
        match self.set_clipboard_text(&text) {
            Ok(via) => {
                self.set_status(StatusLevel::Info, via.toast(&format!("Copied from {name}")));
            }
            Err(e) => self.set_error(e),
        }
    }

    /// Copy the current status-bar message (info / error / …) to the clipboard.
    /// Reachable via `Copy` (Ctrl+C, outside a focused terminal) or by clicking
    /// the status row — so a stray error/path can be pulled out of the TUI to
    /// paste elsewhere. A no-op (no toast) when nothing is shown.
    fn copy_status_to_clipboard(&mut self) {
        let Some(text) = self
            .status_message
            .as_ref()
            .map(|m| m.text.clone())
            .filter(|t| !t.is_empty())
        else {
            return; // nothing shown → no-op (no "copied" toast to overwrite it)
        };

        match self.set_clipboard_text(&text) {
            Ok(via) => {
                self.set_status(StatusLevel::Info, via.toast("Status message copied"));
            }
            Err(e) => self.set_error(e),
        }
    }

    /// Wrap text in bracketed paste escape sequences and send it to the
    /// active session (or shell pane, if focused).
    fn send_paste_to_session(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(session) = self.sessions.get(self.active_index) {
            let mut paste = b"\x1b[200~".to_vec();
            paste.extend_from_slice(text.as_bytes());
            paste.extend_from_slice(b"\x1b[201~");
            let result = if let (TerminalView::Shell, Some(shell)) =
                (self.active_terminal_view(), &session.shell_pane)
            {
                shell.send_input(paste)
            } else {
                session.send_input(paste)
            };
            if let Err(e) = result {
                error!("Failed to send pasted input: {e}");
            }
        }
    }

    /// Handle a native paste event from crossterm's bracketed paste capture.
    fn handle_paste(&mut self, text: String) {
        self.text_selection = None;
        self.selected_text_cache = None;
        if self.try_paste_into_modal_input(&text) {
            return;
        }
        self.send_paste_to_session(&text);
    }

    pub(crate) fn paste_from_clipboard(&mut self) {
        self.text_selection = None;
        self.selected_text_cache = None;

        // No OSC 52 fallback here: terminals block clipboard *reads* for
        // security. The terminal's own paste keystroke still works — it
        // arrives as a bracketed paste (`handle_paste`), not through us.
        // Over SSH the native clipboard isn't the user's to read (see
        // `clipboard::native_clipboard_is_remote`): on macOS it is the *host's*
        // (a read would paste whatever that machine last copied), on a
        // display-less Linux host there is none — either way, refuse rather
        // than paste the wrong text or error obscurely.
        //
        // Info, not Error, in both branches: running over SSH or without a
        // display server is the expected steady state for a whole class of
        // setups, and a red banner every time the user presses paste would
        // report a correctly-working refusal as a fault. The read that *fails*
        // below still errors — that one is a fault.
        if clipboard::native_clipboard_is_remote() {
            self.set_status(
                StatusLevel::Info,
                format!(
                    "Clipboard read unavailable over SSH — {}",
                    clipboard::PASTE_UNAVAILABLE_HINT
                ),
            );
            return;
        }
        let Some(clipboard) = &mut self.clipboard else {
            self.set_status(
                StatusLevel::Info,
                format!(
                    "No clipboard to read here — {}",
                    clipboard::PASTE_UNAVAILABLE_HINT
                ),
            );
            return;
        };

        let text = match clipboard.get_text() {
            Ok(t) => t,
            Err(e) => {
                self.set_error(format!("Clipboard read failed: {e}"));
                return;
            }
        };

        if self.try_paste_into_modal_input(&text) {
            return;
        }
        self.send_paste_to_session(&text);
    }

    /// Route pasted text into the focused text input when one is open — a modal
    /// field or an in-pane editor. Returns `true` when consumed, signalling the
    /// caller to skip the default "send to session" behaviour.
    ///
    /// While *any* modal is open the paste is consumed regardless of whether a
    /// text field has focus, so it can never leak through to the terminal in the
    /// main pane behind the overlay. New modals with text inputs should add
    /// their target here so paste lands in them.
    fn try_paste_into_modal_input(&mut self, text: &str) -> bool {
        use modals::Modal;

        match &mut self.modal {
            Modal::WorktreeName(wn) => wn.name.insert_str(text),
            Modal::SessionName(sn) => match sn.workspace_dir.as_mut() {
                Some(ws) if sn.workspace_focused => ws.insert_str(text),
                _ => sn.name.insert_str(text),
            },
            Modal::RepoPicker(rp) => {
                rp.input.insert_str(text);
                rp.recompute_filter();
                // Refresh the path candidates for a pasted path. Done after
                // the `rp` borrow ends.
                self.refresh_repo_picker_candidates();
            }
            Modal::AutomationEditor(m) => {
                if let Some(field) = m.active_field_mut() {
                    field.insert_str(text);
                }
            }
            // No modal: route to a focused in-pane editor if any.
            Modal::None => return self.try_paste_into_pane_editor(text),
            // Selector-only modals (agent/host/theme/branch pickers, lists, …)
            // have no text field, but still swallow the paste so it can't fall
            // through to the terminal beneath them.
            _ => {}
        }
        true
    }

    /// Route pasted text into a focused in-pane editor (the task or automation
    /// editor, which are panes rather than modals). Returns `true` when the
    /// editor pane is focused — inserting into its text field if one is focused,
    /// otherwise swallowing the paste so it can't leak into the terminal. Called
    /// only when no modal is open.
    fn try_paste_into_pane_editor(&mut self, text: &str) -> bool {
        // Resolve the focused editor's text field (or `None` for a
        // selector/multi-line field handled inline), then insert once below so
        // both editor arms share the tail.
        let field = match self.focus {
            InputFocus::TaskEditor => {
                let Some(editor) = self.task_ui.task_editor.as_mut() else {
                    return true;
                };
                // The description is a multi-line `TextArea`, handled here.
                if editor.field == modals::TaskField::Description {
                    editor.description.insert_str(text);
                    return true;
                }
                editor.active_field_mut()
            }
            InputFocus::AutomationEditor => match self.automation_ui.automation_editor.as_mut() {
                Some(editor) => editor.active_field_mut(),
                None => return true,
            },
            _ => return false,
        };
        if let Some(field) = field {
            field.insert_str(text);
        }
        true
    }

    pub(crate) fn spawn_worktree_session(
        &mut self,
        repo_paths: &[PathBuf],
        new_branch: &str,
        base_branch: &str,
        session_name: Option<String>,
    ) {
        if self.worktree_create.in_progress() {
            self.set_status(StatusLevel::Info, "Worktree creation already in progress…");
            return;
        }

        // Resolve the remote host (if any) so worktrees are created on the
        // session's target machine over SSH. Consume the wizard's choice.
        let backend = self.new_session.backend.take();
        let host = self.host_for_backend(backend.as_deref()).cloned();
        let normal_repos = std::mem::take(&mut self.new_session.normal_repos);
        let fetch_done = self.new_session.fetch_done.take();

        let repo_paths = repo_paths.to_vec();
        let new_branch = new_branch.to_string();
        let base_branch = base_branch.to_string();

        // Overlap the agent picker with the creation (ADR-P12): with the name
        // known and >1 agents to choose from, the user picks the agent while
        // the worker runs; `continue_worktree_spawn` joins the two.
        let open_picker = session_name.is_some() && self.agents.names().len() > 1;
        let agent_pick = if open_picker {
            AgentPick::Open
        } else {
            AgentPick::NotOpened
        };

        // Shell out to `git worktree add` off the UI thread (one per repo, with
        // rollback on failure); the spawn flow resumes in `poll_worktree_create`.
        let tx = self.worktree_create.start();
        self.pending_worktree_create = Some(PendingWorktreeCreate {
            backend,
            normal_repos,
            session_name,
            base_branch: base_branch.clone(),
            agent_pick,
        });
        self.set_status(StatusLevel::Info, "Creating worktree(s)…");
        tokio::task::spawn_blocking(move || {
            // The branch-selection fetch runs concurrently (ADR-P12); wait for
            // it (bounded) so the worktrees fork from fresh origin refs. A
            // timeout falls through — fetch failures were always non-fatal.
            if let Some(rx) = fetch_done {
                if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                    rx.recv_timeout(std::time::Duration::from_secs(30))
                {
                    tracing::warn!("origin fetch still running after 30s; creating worktrees now");
                }
            }
            let result = create_worktrees(host.as_ref(), &repo_paths, &new_branch, &base_branch);
            let _ = tx.send(result);
        });

        if open_picker {
            self.open_agent_picker();
        }
    }

    /// Open the agent picker populated from the registry, pre-selecting the
    /// default agent. Callers must ensure the registry has >1 agent.
    fn open_agent_picker(&mut self) {
        let names = self.agents.names();
        let default = self.agents.default_name();
        let selected_index = names.iter().position(|n| *n == default).unwrap_or(0);
        let choices = self
            .agents
            .agents
            .iter()
            .map(|a| crate::ui::agent_picker_modal::AgentChoice {
                name: a.name.clone(),
                command: a.command.clone(),
            })
            .collect();
        self.modal = modals::Modal::AgentPicker(crate::ui::agent_picker_modal::AgentPickerState {
            choices,
            selected_index,
            filter: Default::default(),
        });
    }

    /// Apply a completed background branch listing (ADR-P12), if one has
    /// finished, into the still-open branch selector. A result whose selector
    /// was cancelled (Esc) — or replaced by a later flow — is dropped.
    fn poll_branch_load(&mut self) {
        let result = match self.branch_load.poll() {
            background::TaskPoll::Pending => return,
            background::TaskPoll::Died => Err("Branch listing failed (worker died)".to_string()),
            background::TaskPoll::Done(result) => result,
        };
        let loading_selector = matches!(
            self.modal,
            modals::Modal::BranchSelector(ref bs) if bs.loading
        );
        if !loading_selector {
            return;
        }
        match result {
            Ok(branches) => {
                if let modals::Modal::BranchSelector(ref mut bs) = self.modal {
                    // Back-then-forward navigation: keep the previously chosen
                    // base branch highlighted instead of snapping to the top.
                    if let Some(prev) = self.new_session.base_branch.as_deref() {
                        if let Some(pos) = branches.iter().position(|b| b == prev) {
                            bs.index = pos;
                        }
                    }
                    bs.branches = branches;
                    bs.loading = false;
                    // A query typed while the list was loading applies now.
                    bs.filter.refilter(&bs.branches, &mut bs.index);
                }
                self.metrics.bump(|p| &mut p.branch_loads_applied);
                self.request_redraw();
            }
            Err(e) => {
                error!("{e}");
                self.modal.close();
                self.set_error(e);
                // Mirror the selector's Esc: abort the pending worktree flow.
                self.new_session.repo_path = None;
                self.new_session.all_repos = None;
                self.new_session.normal_repos.clear();
                self.new_session.fetch_done = None;
            }
        }
    }

    /// Apply a completed background worktree-creation, if one has finished, and
    /// resume the spawn flow.
    fn poll_worktree_create(&mut self) {
        let result = match self.worktree_create.poll() {
            background::TaskPoll::Pending => return,
            background::TaskPoll::Died => {
                self.pending_worktree_create = None;
                self.set_error("Worktree creation failed (worker died)");
                return;
            }
            background::TaskPoll::Done(result) => result,
        };
        let Some(pending) = self.pending_worktree_create.take() else {
            return;
        };

        match result {
            Ok(worktree_infos) => self.continue_worktree_spawn(worktree_infos, pending),
            Err(e) => {
                // Close an agent picker overlapping this create (ADR-P12) —
                // there is nothing left to pick for.
                if matches!(pending.agent_pick, AgentPick::Open)
                    && matches!(self.modal, modals::Modal::AgentPicker(_))
                {
                    self.modal.close();
                    self.new_session.spawn_name = None;
                }
                self.set_error(format!("Failed to create worktree: {e}"));
            }
        }
    }

    /// Build the session config from freshly-created worktrees and continue into
    /// the name/agent modal (or spawn directly when the name is known).
    fn continue_worktree_spawn(
        &mut self,
        worktree_infos: Vec<WorktreeInfo>,
        pending: PendingWorktreeCreate,
    ) {
        let Some(primary) = worktree_infos.first() else {
            self.set_error("Worktree creation produced no worktrees");
            return;
        };
        let primary_path = primary.worktree_path.clone();

        // Combine remaining worktree paths + normal repos as additional dirs.
        let mut additional_dirs: Vec<PathBuf> = worktree_infos[1..]
            .iter()
            .map(|w| w.worktree_path.clone())
            .collect();
        additional_dirs.extend(pending.normal_repos);
        self.new_session.additional_dirs = additional_dirs;
        // Carry the fork point to the spawn so it can be persisted for the
        // code-review view (scopes the diff to `<base>..HEAD`).
        self.new_session.spawn_base_branch = Some(pending.base_branch);

        let config = SessionConfig {
            cwd: Some(primary_path),
            backend: pending.backend,
            ..SessionConfig::default()
        };

        let Some(name) = pending.session_name else {
            self.prepare_spawn(config, worktree_infos);
            return;
        };

        // Session name already known (worktree flow). The agent picker ran
        // concurrently with the creation (ADR-P12) — join on its progress.
        match pending.agent_pick {
            // The user already picked: spawn right away.
            AgentPick::Chosen(agent) => {
                let config = SessionConfig { agent, ..config };
                self.do_spawn_session_async(name, &config, worktree_infos);
            }
            // Still picking: park the inputs for `confirm_agent_picker` and
            // retire the now-stale "Creating worktree(s)…" status.
            AgentPick::Open if matches!(self.modal, modals::Modal::AgentPicker(_)) => {
                self.new_session.spawn_name = Some(name);
                self.new_session.spawn_config = Some(config);
                self.new_session.spawn_worktrees = worktree_infos;
                self.status_message = None;
            }
            // Cancelled (or the picker vanished some other way): drop the
            // result. The worktrees stay on disk, matching a cancel after
            // creation.
            AgentPick::Open | AgentPick::Cancelled => {
                self.set_info("Session creation cancelled (worktrees kept on disk)");
            }
            // No overlapping picker — classic picker-after-create path.
            AgentPick::NotOpened => self.finish_prepare_spawn(name, config, worktree_infos),
        }
    }

    /// Install the configured remote-host registry (from `hosts.toml`). Called
    /// once at startup after the SSH backends are registered.
    pub fn set_hosts(&mut self, hosts: crate::session::HostRegistry) {
        self.hosts = hosts;
    }

    /// Resolve the [`HostDef`] for a backend name, or `None` for the local
    /// backend. Used to run git operations (worktree create/remove, branch
    /// listing) on the correct host.
    ///
    /// [`HostDef`]: crate::session::HostDef
    pub(crate) fn host_for_backend(
        &self,
        backend: Option<&str>,
    ) -> Option<&crate::session::HostDef> {
        // `get_by_backend` returns `None` for local (`non-ssh:`/`non-wsl:`)
        // names.
        self.hosts.get_by_backend(backend?)
    }

    /// The launch cwd for an *existing* session, derived from its persisted
    /// `SessionInfo`: the multi-repo symlink workspace ((re)built on the
    /// session's host, remote or local), or the primary repo when single-repo.
    /// Used by the restart path — the agent is about to relaunch, so the
    /// destructive workspace rebuild is safe there.
    fn session_process_cwd(&self, info: &SessionInfo) -> Option<PathBuf> {
        // `remote_host` is the bare host name (`None` = local).
        let host = info.remote_host.as_deref().and_then(|n| self.hosts.get(n));
        resolve_process_cwd(
            info.agent_session_id.as_deref(),
            info.cwd.clone(),
            &info.worktrees,
            &info.additional_dirs,
            host,
            info.workspace_dir.as_deref(),
        )
    }

    /// Like [`session_process_cwd`](Self::session_process_cwd) but **never
    /// (re)builds** the workspace — it derives the same deterministic path
    /// without touching the filesystem. For callers that resolve the cwd of a
    /// session whose agent is *still running* there (the shell pane): the
    /// ensure-style rebuild is `rm -rf` + recreate, which would delete the
    /// running agent's cwd inode out from under it.
    fn session_process_cwd_existing(&self, info: &SessionInfo) -> Option<PathBuf> {
        let members =
            session_member_dirs(info.cwd.as_deref(), &info.worktrees, &info.additional_dirs);
        if members.len() < 2 {
            return info.cwd.clone();
        }
        // A user-chosen workspace dir is recorded only when it became the
        // launch cwd (always local), so it *is* the deterministic answer.
        if let Some(ws) = &info.workspace_dir {
            return Some(ws.clone());
        }
        let Some(id) = info.agent_session_id.as_deref() else {
            return info.cwd.clone();
        };
        let host = info.remote_host.as_deref().and_then(|n| self.hosts.get(n));
        let path = match host {
            // Only network cost is the (cached) remote `$HOME` lookup.
            Some(h) => crate::git::remote_workspace_dir(h, id)
                .map(PathBuf::from)
                .map_err(|e| error!("Failed to resolve remote workspace path: {e:#}")),
            None => crate::workspace::workspace_path(id)
                .map_err(|e| error!("Failed to resolve workspace path: {e}")),
        };
        path.ok().or_else(|| info.cwd.clone())
    }

    /// Resolve the backend for a *persisted* session by its `backend_type`, or
    /// `None` when this instance cannot manage it.
    ///
    /// An unknown off-local backend (`ssh:<host>` / `wsl:<distro>` — e.g. a host
    /// this instance hasn't loaded from `hosts.toml`, a distro not present here,
    /// or one another instance configured) is **skipped** rather than falling
    /// back to local — adopting an off-local session on the local backend would
    /// corrupt its `backend_type` and risk a pane-id collision (tmux numbers
    /// panes `%N` per server, so a remote `%1` can match an unrelated local
    /// `%1`). Legacy/local values (empty, `tmux`, `local-tmux`) still fall back
    /// to the default local backend.
    pub(crate) fn resolve_persisted_backend(
        &self,
        backend_type: &str,
    ) -> Option<Arc<dyn SessionBackend>> {
        if let Some(b) = self.backends.get(backend_type) {
            return Some(b.clone());
        }
        if crate::session::is_remote_backend(backend_type) {
            return None;
        }
        Some(self.backends.default_backend().clone())
    }

    /// Resolve the backend a session should spawn on, ensuring it is ready —
    /// [`Self::backend_lookup`] + [`ensure_backend_ready`] in one blocking
    /// call. Kept for tests exercising the combined behavior; the spawn paths
    /// call the halves separately so readiness can leave the UI thread.
    #[cfg(test)]
    pub(crate) fn backend_for(
        &self,
        config: &SessionConfig,
    ) -> Result<Arc<dyn SessionBackend>, String> {
        let backend = self.backend_lookup(config)?;
        ensure_backend_ready(&backend)?;
        Ok(backend)
    }

    /// Look up `config.backend` in the registry (falling back to the default
    /// local backend), **without** the readiness round-trip — the async spawn
    /// path runs [`ensure_backend_ready`] on its worker instead (the
    /// control-mode attach / SSH connect is a measurable UI stall, ADR-P12).
    /// Returns a status-line-friendly error if the backend is unknown.
    fn backend_lookup(&self, config: &SessionConfig) -> Result<Arc<dyn SessionBackend>, String> {
        match config.backend.as_deref() {
            Some(name) if !name.is_empty() => self
                .backends
                .get(name)
                .cloned()
                .ok_or_else(|| format!("Unknown backend '{name}'")),
            _ => Ok(self.backends.default_backend().clone()),
        }
    }

    /// Common spawn preparation shared by the sync and async paths: fill in
    /// defaults (agent, `agent_session_id`), inject statusline env vars, resolve
    /// the process cwd (a symlink workspace for multi-repo sessions), and select
    /// the backend + provider. Returns `None` after setting a status error when
    /// the backend is unknown or unreachable.
    fn build_spawn_inputs(
        &mut self,
        name: &str,
        config: &SessionConfig,
        worktrees: &[WorktreeInfo],
        additional_dirs: &[PathBuf],
        workspace_dir: Option<PathBuf>,
    ) -> Option<SpawnInputs> {
        let (rows, cols) = self.content_area_size();

        let mut config = config.clone();
        if config.agent.is_empty() {
            config.agent = self.agents.default_name();
        }
        // Fill `{name}` in the agent's launch templates (e.g. claude's `-n`) so
        // a conversation this spawn *creates* carries the friring session name.
        config.session_name = Some(name.to_string());
        let agent_session_id = config
            .agent_session_id
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        // Mint the friring SessionId up front (unless a respawn supplied one) so
        // it can be injected as `FRIRING_SESSION` before launch and `Session::spawn`
        // reuses it. Stable across restarts.
        if config.session_id.is_none() {
            config.session_id = Some(SessionId::default());
        }

        // Inject identity + statusline env vars. `FRIRING_TASK` is left unset: TUI
        // task spawns track the task↔session link in-memory (`task_session_links`),
        // so only the headless `task run` path auto-tags messages with it.
        crate::session_ops::inject_friring_env(&mut config, &agent_session_id, None);

        // For a multi-repo session, launch the agent in a symlink workspace that
        // gathers every member dir; `info.cwd` keeps the primary repo (restored
        // after spawn). Single-repo sessions are unchanged.
        let primary_cwd = config.cwd.clone();
        let spawn_host = self.host_for_backend(config.backend.as_deref()).cloned();
        config.cwd = resolve_process_cwd(
            config.agent_session_id.as_deref(),
            primary_cwd.clone(),
            worktrees,
            additional_dirs,
            spawn_host.as_ref(),
            workspace_dir.as_deref(),
        );
        // Persist the custom dir only when it really became the launch cwd
        // (single-member / build-failure spawns fall back — recording the
        // unused path would point restart and delete at a dir the agent never
        // ran in).
        let workspace_dir =
            workspace_dir.filter(|dir| config.cwd.as_deref() == Some(dir.as_path()));

        // Lookup only — readiness is the caller's job: the sync path blocks on
        // it inline, the async path readies on its worker (ADR-P12).
        let backend = match self.backend_lookup(&config) {
            Ok(b) => b,
            Err(e) => {
                error!("Failed to select backend: {e}");
                self.set_error(e);
                return None;
            }
        };

        let provider = self.launch_provider_for(&config);

        Some(SpawnInputs {
            config,
            primary_cwd,
            workspace_dir,
            backend,
            provider,
            rows,
            cols,
        })
    }

    /// Adopt a freshly spawned [`Session`] into the app: attach its metadata,
    /// select + focus it, persist, and run any task-initiated follow-up. Shared
    /// by the synchronous and backgrounded spawn paths.
    #[allow(clippy::too_many_arguments)]
    fn finalize_spawned_session(
        &mut self,
        mut session: Session,
        primary_cwd: Option<PathBuf>,
        worktrees: Vec<WorktreeInfo>,
        additional_dirs: Vec<PathBuf>,
        workspace_dir: Option<PathBuf>,
        parent_session_id: Option<SessionId>,
        task_prompt: Option<(i64, String)>,
        base_branch: Option<String>,
    ) {
        session.info.cwd = primary_cwd;
        session.info.worktrees = worktrees;
        session.info.additional_dirs = additional_dirs;
        session.info.workspace_dir = workspace_dir;
        session.info.parent_session_id = parent_session_id;

        // The async spawn worker pre-resolves the display names off-thread
        // from the same member set (ADR-P12); only the synchronous path still
        // resolves here (`git remote get-url` per member repo).
        if session.info.repo_display_names.is_empty() {
            resolve_repo_display_names(&mut session.info);
        }
        let session_id = session.info.id;
        self.sessions.push(session);
        self.set_active_index(self.sessions.len() - 1);
        self.focus = InputFocus::Terminal;
        self.status_message = None;

        self.save_state();

        // Persist the worktree's fork point (write-once, like the hook columns)
        // so the code-review view can scope its diff to `<base>..HEAD`. Runs
        // after `save_state` so the row exists; `upsert_session` never lists it.
        if let Some(base) = base_branch {
            if let Err(e) = self.db.set_session_base_branch(session_id, &base) {
                tracing::warn!("Failed to record session base branch: {e}");
            }
        }
        // No spawn-time status seed: a fresh session is `Idle` until the agent's
        // hooks report otherwise (claude's SessionStart → idle on boot, then
        // working/blocked/done). Seeding `working` made an idle session look
        // stuck working.

        // A task-initiated spawn (the trigger-time picker's "Spawn new session")
        // delivers the task title once the agent has booted, then advances the
        // task to in progress.
        if let Some((task_id, title)) = task_prompt {
            let new_id = self.sessions[self.active_index].info.id;
            // Record the link now — the session was named by the user, so the
            // `<title> · #<id>` convention can't recover it later.
            self.task_ui.task_session_links.insert(task_id, new_id);
            let prompt = self.task_agent_prompt(task_id, &title);
            self.send_prompt_to_session(new_id, &prompt, AGENT_BOOT_DELAY_TICKS);
            let status = self
                .task_ui
                .cached_tasks
                .iter()
                .find(|t| t.id == task_id)
                .map(|t| t.status)
                .unwrap_or_default();
            self.advance_task_to_in_progress(task_id, status);
            self.refresh_tasks();
        }
    }

    /// Spawn a session **synchronously** (blocks the caller on PTY/tmux
    /// creation). Used by programmatic callers that need the session present
    /// immediately — automations/tasks read the new id right back, and restore
    /// runs inside `tick()`. The interactive new-session flow uses
    /// [`Self::do_spawn_session_async`] instead so `Ctrl+N` doesn't freeze.
    pub(crate) fn do_spawn_session(
        &mut self,
        name: String,
        config: &SessionConfig,
        worktrees: Vec<WorktreeInfo>,
    ) {
        let additional_dirs = std::mem::take(&mut self.new_session.additional_dirs);
        let workspace_dir = self.new_session.workspace_dir.take();
        let parent_session_id = self.new_session.parent_session_id.take();
        let base_branch = self.new_session.spawn_base_branch.take();
        let Some(inputs) =
            self.build_spawn_inputs(&name, config, &worktrees, &additional_dirs, workspace_dir)
        else {
            return;
        };
        // Synchronous path: blocking on backend readiness here is the point.
        if let Err(e) = ensure_backend_ready(&inputs.backend) {
            error!("Failed to select backend: {e}");
            self.set_error(e);
            return;
        }

        match Session::spawn(
            name,
            inputs.rows,
            inputs.cols,
            &inputs.config,
            &inputs.backend,
            &inputs.provider,
        ) {
            Ok(session) => {
                let task_prompt = self.task_ui.pending_task_prompt.take();
                self.finalize_spawned_session(
                    session,
                    inputs.primary_cwd,
                    worktrees,
                    additional_dirs,
                    inputs.workspace_dir,
                    parent_session_id,
                    task_prompt,
                    base_branch,
                );
            }
            Err(e) => {
                error!("Failed to spawn session: {e}");
                self.set_error(format!("Failed to start {}: {e:#}", inputs.config.agent));
            }
        }
    }

    /// Spawn a session for the **interactive** new-session flow without blocking
    /// the UI: `Session::spawn` (PTY/tmux window creation, 500ms+) runs on a
    /// blocking task and the session is adopted in [`Self::poll_session_spawn`].
    /// Falls back to the synchronous path if a spawn is already in flight (so a
    /// double-trigger is never silently dropped).
    pub(crate) fn do_spawn_session_async(
        &mut self,
        name: String,
        config: &SessionConfig,
        worktrees: Vec<WorktreeInfo>,
    ) {
        // The wizard is committed — the parked back-navigation states have
        // nothing to return to.
        self.new_session.saved_repo_picker = None;
        self.new_session.saved_conversation_picker = None;

        if self.session_spawn.in_progress() {
            self.do_spawn_session(name, config, worktrees);
            return;
        }

        let additional_dirs = std::mem::take(&mut self.new_session.additional_dirs);
        let workspace_dir = self.new_session.workspace_dir.take();
        let parent_session_id = self.new_session.parent_session_id.take();
        let base_branch = self.new_session.spawn_base_branch.take();
        let Some(inputs) =
            self.build_spawn_inputs(&name, config, &worktrees, &additional_dirs, workspace_dir)
        else {
            return;
        };
        let task_prompt = self.task_ui.pending_task_prompt.take();

        let SpawnInputs {
            config,
            primary_cwd,
            workspace_dir,
            backend,
            provider,
            rows,
            cols,
        } = inputs;

        let agent = config.agent.clone();
        let tx = self.session_spawn.start();
        // Clones for the worker's display-name resolution (`git remote
        // get-url` per member repo — a subprocess that must not run on the UI
        // thread, ADR-P12); the originals ride in the pending continuation.
        let worker_cwd = primary_cwd.clone();
        let worker_worktrees = worktrees.clone();
        let worker_dirs = additional_dirs.clone();
        self.pending_session_spawn = Some(PendingSessionSpawn {
            primary_cwd,
            worktrees,
            additional_dirs,
            workspace_dir,
            parent_session_id,
            task_prompt,
            agent,
            base_branch,
        });
        self.set_status(StatusLevel::Info, format!("Spawning {name}…"));

        tokio::task::spawn_blocking(move || {
            // Backend readiness (control-mode attach / SSH connect) belongs on
            // the worker too — it stalled the agent-picker Enter (ADR-P12).
            let result = ensure_backend_ready(&backend)
                .and_then(|()| {
                    Session::spawn(name, rows, cols, &config, &backend, &provider)
                        .map_err(|e| format!("{e:#}"))
                })
                .map(|mut session| {
                    session.info.repo_display_names =
                        session_member_dirs(worker_cwd.as_deref(), &worker_worktrees, &worker_dirs)
                            .into_iter()
                            .filter_map(|(name, _)| name)
                            .collect();
                    session
                });
            let _ = tx.send(result);
        });
    }

    /// Adopt a completed background `Session::spawn`, if one has finished.
    fn poll_session_spawn(&mut self) {
        let result = match self.session_spawn.poll() {
            background::TaskPoll::Pending => return,
            background::TaskPoll::Died => {
                self.pending_session_spawn = None;
                self.set_error("Session spawn failed (worker died)");
                return;
            }
            background::TaskPoll::Done(result) => result,
        };
        let Some(pending) = self.pending_session_spawn.take() else {
            return;
        };

        match result {
            Ok(session) => self.finalize_spawned_session(
                session,
                pending.primary_cwd,
                pending.worktrees,
                pending.additional_dirs,
                pending.workspace_dir,
                pending.parent_session_id,
                pending.task_prompt,
                pending.base_branch,
            ),
            Err(e) => {
                error!("Failed to spawn session: {e}");
                self.set_error(format!("Failed to start {}: {e}", pending.agent));
            }
        }
    }

    /// Clamp the active session index to the valid range after a session is removed.
    pub(crate) fn sync_active_session_to_project(&mut self) {
        if self.sessions.is_empty() {
            self.active_index = 0;
        } else if self.active_index >= self.sessions.len() {
            self.active_index = self.sessions.len() - 1;
        }
    }

    /// The rendered order of `self.sessions`, from the same
    /// `ui::project_list::compute_session_order` the rendering widget uses, so
    /// navigation and reordering operate on the exact order the user sees.
    fn session_order(&self) -> crate::ui::project_list::SessionOrder {
        let infos: Vec<&crate::session::SessionInfo> =
            self.sessions.iter().map(|s| &s.info).collect();
        crate::ui::project_list::compute_session_order(&infos)
    }

    /// Indices into `self.sessions` in the **full** rendered order, hidden rows
    /// included. This is the order the reordering operations work in: they
    /// renumber every session's `display_order` along it, so a filtered order
    /// would silently renumber the collapsed rows into each other.
    ///
    /// Navigation wants [`Self::visible_order_indices`] instead.
    fn render_order_indices(&self) -> Vec<usize> {
        self.session_order().order
    }

    /// What the session list hides this frame: collapsed repo groups and, with
    /// the ghost shelf on, unloaded sessions — never the active session, whose
    /// row has to stay under the cursor.
    pub(crate) fn visibility_filter(&self) -> crate::ui::project_list::VisibilityFilter<'_> {
        crate::ui::project_list::VisibilityFilter {
            folded_groups: &self.folded_groups,
            ghost_shelf: self.ghost_shelf,
            keep: self.active_session_id(),
        }
    }

    /// Indices into `self.sessions` for the rows actually **on screen**, in
    /// render order — what `Ctrl+J`/`Ctrl+K`, the jump digits and the labels
    /// step through. Stepping onto a row the user can't see would look like the
    /// key was swallowed, so navigation follows the list rather than the data.
    pub(crate) fn visible_order_indices(&self) -> Vec<usize> {
        let infos: Vec<&crate::session::SessionInfo> =
            self.sessions.iter().map(|s| &s.info).collect();
        let order = crate::ui::project_list::compute_session_order(&infos);
        let visible =
            crate::ui::project_list::visible_rows(&infos, &order, &self.visibility_filter());
        order
            .order
            .into_iter()
            .zip(visible)
            .filter_map(|(i, shown)| shown.then_some(i))
            .collect()
    }

    /// The group key of the session at `idx`, for the fold toggles.
    fn group_key_at(&self, idx: usize) -> Option<String> {
        self.sessions
            .get(idx)
            .map(|s| crate::ui::project_list::group_key(&s.info))
    }

    /// Collapse or expand the active session's repo group and persist the set.
    ///
    /// Collapsing moves the selection to the group's first row: otherwise the
    /// active session would be force-kept visible under its own collapsed
    /// header (see [`Self::visibility_filter`]) and the key would look like it
    /// did nothing.
    pub(crate) fn set_active_group_folded(&mut self, folded: bool) {
        let Some(key) = self.group_key_at(self.active_index) else {
            return;
        };
        let changed = if folded {
            self.folded_groups.insert(key.clone())
        } else {
            self.folded_groups.remove(&key)
        };
        if !changed {
            return;
        }
        if folded {
            if let Some(first) = self
                .render_order_indices()
                .into_iter()
                .find(|&i| self.group_key_at(i).as_deref() == Some(key.as_str()))
            {
                self.set_active_index(first);
            }
        }
        self.persist_folded_groups();
    }

    /// Write the collapsed-group set back to the DB. A failure here costs the
    /// arrangement on the next launch, not the fold itself, so it is logged
    /// rather than surfaced.
    fn persist_folded_groups(&self) {
        let mut keys: Vec<String> = self.folded_groups.iter().cloned().collect();
        keys.sort_unstable();
        if let Err(e) = self.db.set_folded_session_groups(&keys) {
            tracing::warn!("failed to persist collapsed session groups: {e}");
        }
    }

    /// Restore the collapsed-group set at startup.
    pub fn load_folded_groups(&mut self) {
        match self.db.get_folded_session_groups() {
            Ok(keys) => self.folded_groups = keys.into_iter().collect(),
            Err(e) => tracing::warn!("failed to load collapsed session groups: {e}"),
        }
    }

    /// Show or hide the unloaded sessions (the ghost shelf). Reports the new
    /// state, because with no ghosts around the toggle has nothing visible to
    /// change and would otherwise look broken.
    pub(crate) fn toggle_ghost_shelf(&mut self) {
        self.ghost_shelf = !self.ghost_shelf;
        let ghosts = self
            .sessions
            .iter()
            .filter(|s| s.info.status == SessionStatus::Unloaded)
            .count();
        let msg = match (self.ghost_shelf, ghosts) {
            (_, 0) => "No unloaded sessions to shelve".to_string(),
            (true, n) => format!("Shelved {n} unloaded session(s)"),
            (false, n) => format!("Showing {n} unloaded session(s)"),
        };
        self.set_status(StatusLevel::Info, msg);
    }

    /// Move the selection to the first visible session of the next (or
    /// previous) repo group, wrapping — the coarse step that makes a list of
    /// twenty sessions across five repos five keystrokes wide instead of twenty.
    pub(crate) fn jump_to_adjacent_group(&mut self, forward: bool) {
        let visible = self.visible_order_indices();
        if visible.is_empty() {
            return;
        }
        // First visible row of each group, in render order.
        let mut heads: Vec<usize> = Vec::new();
        let mut last: Option<String> = None;
        for &i in &visible {
            let key = self.group_key_at(i);
            if key != last {
                heads.push(i);
                last = key;
            }
        }
        if heads.len() < 2 {
            self.set_status(StatusLevel::Info, "Only one repo group");
            return;
        }
        // The group the cursor is in, by its head's position.
        let active_key = self.group_key_at(self.active_index);
        let here = heads
            .iter()
            .position(|&i| self.group_key_at(i) == active_key)
            .unwrap_or(0);
        let next = if forward {
            (here + 1) % heads.len()
        } else {
            (here + heads.len() - 1) % heads.len()
        };
        self.set_active_index(heads[next]);
    }

    /// Move the active session one step up or down in the rendered order
    /// (`Shift+J`/`Shift+K` in the session list): root blocks swap within their
    /// repo group, whole groups swap past the group edge, nested children move
    /// among their siblings (see `ui::project_list::move_in_order`).
    ///
    /// On success every session is renumbered densely along the new order and
    /// persisted, so the order survives restarts and reaches other instances
    /// via the DB poll. The selection follows the moved row automatically
    /// (`active_index` is an input index, which the move never changes).
    pub(crate) fn move_active_session(&mut self, down: bool) {
        if self.sessions.is_empty() {
            return;
        }
        let ord = self.session_order();
        let Some(new_order) = crate::ui::project_list::move_in_order(&ord, self.active_index, down)
        else {
            return;
        };
        for (pos, &idx) in new_order.iter().enumerate() {
            self.sessions[idx].info.display_order = Some(pos as i64);
        }
        self.save_state();
    }

    /// Sort sessions alphabetically by name within each repo group
    /// (`Shift+S` in the session list). Group order is unchanged; parent/child
    /// nesting is preserved (children sort among their siblings). Renumbers
    /// every session's `display_order` densely along the new order so the
    /// arrangement survives restarts and reaches other instances via the DB
    /// poll. No-op on an empty list.
    pub(crate) fn sort_sessions_alphabetically(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let infos: Vec<&crate::session::SessionInfo> =
            self.sessions.iter().map(|s| &s.info).collect();
        let new_order = crate::ui::project_list::sort_alphabetically_within_groups(&infos);
        for (pos, &idx) in new_order.iter().enumerate() {
            self.sessions[idx].info.display_order = Some(pos as i64);
        }
        self.save_state();
    }

    /// Whether the active session is the first row in render order (top of the
    /// left column). Treats an empty list as "first" so `k` is a no-op there.
    pub(crate) fn active_is_first_in_order(&self) -> bool {
        match self.visible_order_indices().first() {
            Some(&first) => first == self.active_index,
            None => true,
        }
    }

    /// Whether the active session is the last row in render order (the bottom of
    /// the session list, directly above the automations pane). Treats an empty
    /// list as "last" so `j` falls straight through into the automations pane.
    pub(crate) fn active_is_last_in_order(&self) -> bool {
        match self.visible_order_indices().last() {
            Some(&last) => last == self.active_index,
            None => true,
        }
    }

    /// Select the last session in render order — used when navigating up out of
    /// the automations pane back into the session list.
    pub(crate) fn select_last_session(&mut self) {
        if let Some(&last) = self.visible_order_indices().last() {
            self.set_active_index(last);
        }
    }

    /// Select the first session in render order — used when looping down out of
    /// the automations pane back to the top of the session list.
    pub(crate) fn select_first_session(&mut self) {
        if let Some(&first) = self.visible_order_indices().first() {
            self.set_active_index(first);
        }
    }

    /// Change the active session, remembering the one it replaces for the
    /// `LastSession` toggle. Every *deliberate* switch funnels through here
    /// (Ctrl+J/K, list j/k, clicks, jumps, search commit, spawn/undelete);
    /// bookkeeping moves that merely keep the selection valid (restore
    /// reshuffles, delete clamps, search live-previews) assign `active_index`
    /// directly so they never pollute the toggle history.
    pub(crate) fn set_active_index(&mut self, idx: usize) {
        if idx != self.active_index {
            self.last_active_session = self.active_session_id();
        }
        self.active_index = idx;
        self.note_session_use();
    }

    /// Promote the active session to the front of the MRU list. Called by every
    /// deliberate switch — including the search commit, which sets
    /// `active_index` directly to keep live previews out of the toggle history
    /// but still means "I chose this one".
    pub(crate) fn note_session_use(&mut self) {
        let Some(id) = self.active_session_id() else {
            return;
        };
        self.session_mru.retain(|&seen| seen != id);
        self.session_mru.insert(0, id);
    }

    /// Indices into `self.sessions`, most-recently-active first. Sessions never
    /// deliberately switched to (a fresh start, or ones only ever passed over)
    /// trail in render order, so the list is total and stable rather than
    /// half-empty on the first switch of a session.
    pub(crate) fn mru_order_indices(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::with_capacity(self.sessions.len());
        for id in &self.session_mru {
            if let Some(i) = self.sessions.iter().position(|s| s.info.id == *id) {
                out.push(i);
            }
        }
        for i in self.render_order_indices() {
            if !out.contains(&i) {
                out.push(i);
            }
        }
        out
    }

    /// A session's position in the MRU list — the switcher's recency tiebreak
    /// (lower is more recent). Never-activated sessions sort last.
    fn mru_rank(&self, idx: usize) -> usize {
        self.sessions
            .get(idx)
            .and_then(|s| self.session_mru.iter().position(|id| *id == s.info.id))
            .unwrap_or(usize::MAX)
    }

    /// Toggle between the two most recent sessions (tmux `last-window`, vim's
    /// alternate buffer — hence the `Ctrl+^`/`Ctrl+6` default). Focus lands in
    /// the terminal like the other jumps, and the toggle re-records the
    /// session it left, so pressing it again bounces back.
    pub(crate) fn toggle_last_session(&mut self) {
        let Some(id) = self.last_active_session else {
            self.set_status(StatusLevel::Info, "No previous session");
            return;
        };
        let Some(idx) = self.sessions.iter().position(|s| s.info.id == id) else {
            // The remembered session was deleted since; drop the stale id.
            self.last_active_session = None;
            self.set_status(StatusLevel::Info, "Previous session is gone");
            return;
        };
        if idx == self.active_index {
            return;
        }
        self.set_active_index(idx);
        self.focus = InputFocus::Terminal;
        self.on_focus_changed();
    }

    /// Switch to the next session in the **rendered** order (wraps around).
    pub(crate) fn switch_session_forward(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let order = self.visible_order_indices();
        let pos = order
            .iter()
            .position(|&i| i == self.active_index)
            .unwrap_or(0);
        let next = (pos + 1) % order.len();
        self.set_active_index(order[next]);
    }

    /// Track the Alt key's held state (kitty-protocol modifier events).
    /// While held, the session list numbers its rows for `Alt+<digit>` jumps
    /// (after [`JUMP_OVERLAY_DELAY_MS`]); releasing Alt also dismisses a
    /// held-mode blocked overlay. Repeats / duplicate events are no-ops.
    pub(crate) fn set_alt_held(&mut self, held: bool) {
        if held == self.alt_held {
            return;
        }
        self.alt_held = held;
        self.alt_overlay_redraw_requested = false;
        self.alt_held_since = held.then(std::time::Instant::now);
        if !held && self.attention_jump == Some(AttentionJumpMode::Held) {
            self.attention_jump = None;
        }
    }

    /// Which jump overlay the session list should paint this frame:
    /// `Some(true)` = blocked-only numbering (`Alt+A`, or `<leader> a`),
    /// `Some(false)` = all sessions (Alt held past the delay, or an armed
    /// leader), `None` = no overlay.
    ///
    /// The armed leader paints the same numbers as the Alt-hold: `<leader> 1`
    /// is otherwise a documentation-only route, and a which-key row reading
    /// "go to session N" is useless without knowing which N is which.
    pub(crate) fn jump_overlay_attention_only(&self) -> Option<bool> {
        match self.jump_numbering()? {
            JumpNumbering::Attention => Some(true),
            JumpNumbering::All => Some(false),
            JumpNumbering::MoveDistance { .. } | JumpNumbering::Labels => None,
        }
    }

    /// The numbering scheme the session list should paint this frame, if any.
    /// See [`JumpNumbering`] for why this follows the pending gesture.
    pub(crate) fn jump_numbering(&self) -> Option<JumpNumbering> {
        if let PrefixState::AwaitingMove { up } = self.prefix_state {
            return Some(JumpNumbering::MoveDistance {
                from: self.active_index,
                up,
            });
        }
        if self.label_jump.is_some() {
            return Some(JumpNumbering::Labels);
        }
        if self.attention_jump.is_some() {
            return Some(JumpNumbering::Attention);
        }
        if self.prefix_state.is_armed() {
            return Some(JumpNumbering::All);
        }
        let delay_elapsed = self
            .alt_held_since
            .is_some_and(|t| t.elapsed().as_millis() as u64 >= JUMP_OVERLAY_DELAY_MS);
        if self.alt_held && delay_elapsed {
            return Some(JumpNumbering::All);
        }
        None
    }

    /// Move the active session `distance` places toward the top (`up`) or
    /// bottom of the rendered order, shifting the sessions it passes rather
    /// than swapping with one. Clamps at the ends: asking to move further than
    /// the list allows lands it first/last rather than reporting an error,
    /// which is what a user typing a generous digit means.
    pub(crate) fn move_active_session_by(&mut self, distance: usize, up: bool) {
        if distance == 0 || self.sessions.is_empty() {
            return;
        }
        let order = self.render_order_indices();
        let Some(pos) = order.iter().position(|&i| i == self.active_index) else {
            return;
        };
        let target = if up {
            pos.saturating_sub(distance)
        } else {
            (pos + distance).min(order.len() - 1)
        };
        if target == pos {
            self.set_status(
                StatusLevel::Info,
                format!("Already at the {}", if up { "top" } else { "bottom" }),
            );
            return;
        }
        // Reuse the single-step reorder so grouping/nesting rules stay in one
        // place: repeating it is O(distance) on a list capped at 9 moves.
        let steps = pos.abs_diff(target);
        for _ in 0..steps {
            self.move_active_session(!up);
        }
    }

    /// The leader chord to title the which-key overlay with, or `None` when
    /// the overlay should stay hidden — either the leader isn't armed, or
    /// `prefix.hint_delay_ms` hasn't elapsed yet (it defaults to 0, so the
    /// overlay is normally immediate).
    pub(crate) fn prefix_hint_chord(&self) -> Option<crate::session::KeyChord> {
        let PrefixState::Armed { since, chord } = self.prefix_state else {
            return None;
        };
        let delay = self.prefix_settings.hint_delay_ms;
        if delay > 0 && clock::elapsed_since(since) < std::time::Duration::from_millis(delay) {
            return None;
        }
        Some(chord)
    }

    /// Tick hook for a *non-zero* `hint_delay_ms`: like the Alt-hold overlay,
    /// the which-key box then appears on a timer rather than an input event,
    /// so nothing else would mark the frame dirty while the user waits. A zero
    /// delay (the default) paints on the arming keypress and never reaches here.
    fn tick_prefix_hint(&mut self) {
        if self.prefix_state.is_armed()
            && self.prefix_settings.hint_delay_ms > 0
            && !self.prefix_hint_redraw_requested
            && self.prefix_hint_chord().is_some()
        {
            self.prefix_hint_redraw_requested = true;
            self.request_redraw();
        }
    }

    /// Tick hook: the Alt-hold overlay appears on a *timer*, not an input
    /// event, so the frame where the delay elapses must be requested here —
    /// nothing else marks the UI dirty while the user just holds Alt.
    fn tick_jump_overlay(&mut self) {
        if self.alt_held
            && !self.alt_overlay_redraw_requested
            && self.jump_overlay_attention_only().is_some()
        {
            self.alt_overlay_redraw_requested = true;
            self.request_redraw();
        }
    }

    /// Which status the attention queue is walking right now, or `None` when
    /// nothing needs the user.
    ///
    /// `Blocked` (an agent waiting on an answer) always wins: those sessions
    /// are *stopped* until you act. Only when none is blocked does the queue
    /// fall through to `Done` — a run that finished and hasn't been looked at
    /// (`derive_session_status` drops a session back to `Idle` the moment it
    /// is seen, so `Done` already means "unseen"). Following both at once
    /// would bury the blocking prompts among finished runs.
    ///
    /// The `Done` half is opt-out via `[navigation] attention_includes_done`.
    pub(crate) fn attention_status(&self) -> Option<SessionStatus> {
        let any = |status| self.sessions.iter().any(|s| s.info.status == status);
        if any(SessionStatus::Blocked) {
            return Some(SessionStatus::Blocked);
        }
        if self.navigation.attention_includes_done && any(SessionStatus::Done) {
            return Some(SessionStatus::Done);
        }
        None
    }

    /// The sessions digits `1`–`9` jump to, in the order the overlay numbers
    /// them: rendered order, optionally filtered to the attention queue,
    /// capped at 9. Must stay consistent with the numbering `App::view` paints
    /// (same order, same predicate — see `render_left_panel`).
    pub(crate) fn session_jump_targets(&self, attention_only: bool) -> Vec<usize> {
        // `Some(status) == None` is false, so an empty queue numbers nothing —
        // treating "no filter status" as "no filter" would silently turn the
        // attention overlay into the all-sessions one.
        let wanted = self.attention_status();
        self.visible_order_indices()
            .into_iter()
            .filter(|&i| !attention_only || wanted == Some(self.sessions[i].info.status))
            .take(9)
            .collect()
    }

    /// Activate the `digit`-numbered session of the jump overlay (all
    /// sessions or blocked-only, matching what the overlay renders) and land
    /// in the terminal. An out-of-range digit reports instead of guessing.
    pub(crate) fn jump_to_digit(&mut self, digit: char, attention_only: bool) {
        let n = digit.to_digit(10).unwrap_or(0) as usize;
        let targets = self.session_jump_targets(attention_only);
        match n.checked_sub(1).and_then(|i| targets.get(i)) {
            Some(&idx) => {
                self.set_active_index(idx);
                self.focus = InputFocus::Terminal;
                self.on_focus_changed();
            }
            None => {
                let what = if attention_only {
                    "session needing attention"
                } else {
                    "session"
                };
                self.set_status(StatusLevel::Info, format!("No {what} #{n}"));
            }
        }
    }

    /// The sessions the label-jump overlay labels, in rendered order — every
    /// one of them, which is the difference from [`Self::session_jump_targets`]
    /// and its nine-digit ceiling.
    pub(crate) fn label_jump_targets(&self) -> Vec<usize> {
        self.visible_order_indices()
    }

    /// The label each row wears this frame, parallel to `label_jump_targets`.
    /// While a two-key label is half-typed, rows that can't still match drop
    /// their label and the rest show only the remainder — so the overlay
    /// narrows to the reachable set as the user commits.
    pub(crate) fn label_jump_chips(&self) -> Vec<Option<String>> {
        let Some(state) = self.label_jump.as_ref() else {
            return Vec::new();
        };
        let targets = self.label_jump_targets();
        crate::ui::project_list::session_labels(targets.len())
            .into_iter()
            .map(|label| {
                label
                    .strip_prefix(state.typed.as_str())
                    .filter(|rest| !rest.is_empty())
                    .map(str::to_string)
            })
            .collect()
    }

    /// Open the label-jump overlay (`Alt+G` / `<leader> A`), or close it if it
    /// is already open. Reports rather than opening an empty overlay.
    pub(crate) fn toggle_label_jump(&mut self) {
        if self.label_jump.take().is_some() {
            return;
        }
        if self.sessions.is_empty() {
            self.set_status(StatusLevel::Info, "No sessions to jump to");
            return;
        }
        self.label_jump = Some(LabelJump::default());
    }

    /// Feed a character to the open label-jump overlay. Returns `false` when
    /// the character can't continue any label, which the caller reports as a
    /// miss — the mode always ends on the keystroke either way, so a typo can
    /// never leave the user trapped in an overlay.
    pub(crate) fn push_label_jump_char(&mut self, c: char) -> bool {
        let Some(mut state) = self.label_jump.take() else {
            return false;
        };
        state.typed.push(c.to_ascii_lowercase());
        let targets = self.label_jump_targets();
        let labels = crate::ui::project_list::session_labels(targets.len());
        if let Some(pos) = labels.iter().position(|l| *l == state.typed) {
            let idx = targets[pos];
            self.set_active_index(idx);
            self.focus = InputFocus::Terminal;
            self.on_focus_changed();
            return true;
        }
        // Not a whole label yet: stay open only while something can still
        // complete it.
        if labels.iter().any(|l| l.starts_with(&state.typed)) {
            self.label_jump = Some(state);
            return true;
        }
        false
    }

    /// Toggle the attention-only jump overlay (`Alt+A`): the sessions needing
    /// the user (see [`Self::attention_status`]) get numbers `1`–`9` and a
    /// digit jumps to that one — fewer, lower digits than the all-session
    /// numbering when the list is long. Entered while Alt is held it lives
    /// until the Alt release; entered by a tap (legacy terminals) it is
    /// sticky — see [`AttentionJumpMode`].
    pub(crate) fn toggle_attention_jump(&mut self) {
        if self.attention_jump.is_some() {
            self.attention_jump = None;
            return;
        }
        if self.session_jump_targets(true).is_empty() {
            self.set_status(StatusLevel::Info, "Nothing needs attention");
            return;
        }
        self.attention_jump = Some(if self.alt_held {
            AttentionJumpMode::Held
        } else {
            AttentionJumpMode::Sticky
        });
    }

    /// Jump to the next session needing attention, scanning forward from the
    /// active session in **rendered** order (wraps) and landing focus in the
    /// terminal — so pressing the key repeatedly walks the attention queue
    /// top-to-bottom, answering each prompt in turn and then reviewing each
    /// finished run. Rendered order (not waiting-since time) so the walk
    /// matches the sidebar the user is looking at; status never reorders rows,
    /// so the walk is stable. No-ops with a status hint when nothing is
    /// waiting. See [`Self::attention_status`] for what counts.
    pub(crate) fn focus_next_attention(&mut self) {
        let Some(wanted) = self.attention_status() else {
            self.set_status(StatusLevel::Info, "Nothing needs attention");
            return;
        };
        let order = self.visible_order_indices();
        if order.is_empty() {
            return;
        }
        let pos = order
            .iter()
            .position(|&i| i == self.active_index)
            .unwrap_or(0);
        // Steps 1..len visit every *other* session once, so the active one
        // never counts as its own jump target.
        let target = (1..order.len())
            .map(|step| order[(pos + step) % order.len()])
            .find(|&idx| self.sessions[idx].info.status == wanted);
        match target {
            Some(idx) => {
                self.set_active_index(idx);
                self.focus = InputFocus::Terminal;
                self.on_focus_changed();
            }
            // The only remaining member of the queue is the session already
            // on screen.
            None => self.set_status(StatusLevel::Info, "Nothing else needs attention"),
        }
    }

    /// Switch to the previous session in the **rendered** order (wraps around).
    pub(crate) fn switch_session_backward(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let order = self.visible_order_indices();
        let pos = order
            .iter()
            .position(|&i| i == self.active_index)
            .unwrap_or(0);
        let prev = if pos == 0 { order.len() - 1 } else { pos - 1 };
        self.set_active_index(order[prev]);
    }

    fn handle_resize(&mut self, cols: u16, rows: u16) {
        self.terminal_cols = cols;
        self.terminal_rows = rows;

        // Collapse the optional right-side panels if the terminal gets too
        // narrow (they only render at width >= 120 anyway). The info panel is
        // exempt unless pinned to its column: with `auto`/`inline` it docks in
        // the left column, which narrow terminals still show — but not while
        // that column is collapsed, which leaves every position column-only.
        if cols < 120 {
            if self.info_panel_position == crate::session::settings::InfoPanelPosition::Column
                || !self.show_session_list
            {
                self.show_info_panel = false;
            }
            self.show_tasks_panel = false;
            // Rescue the editor too, not just the list — otherwise focus stays
            // on the hidden panel's editor, which keeps capturing every key.
            if matches!(self.focus, InputFocus::TaskList | InputFocus::TaskEditor) {
                self.focus = self.focus_fallback();
            }
        }

        self.resize_sessions_to_content_area();
    }

    /// Push the current content-area `(rows, cols)` to every session — call after any layout change.
    pub(crate) fn resize_sessions_to_content_area(&mut self) {
        let (rows, cols) = self.content_area_size();
        self.last_content_size = Some((rows, cols));
        for session in &self.sessions {
            session.resize(rows, cols);
        }
    }

    /// Re-push PTY sizes when the computed content area drifted without a
    /// resize event: the `auto` info-pane dock moves between the left column
    /// and its own column as its inputs (info content, session/automation
    /// counts) change, which shifts the terminal width mid-session.
    ///
    /// Runs on every (unthrottled ~100 Hz) tick, so it must stay cheap. Only
    /// the `auto` dock with the panel open can resize the terminal from
    /// content — `column`/`inline` never do, and every explicit panel toggle
    /// already re-pushes sizes — so gate on that before the (line-building)
    /// `content_area_size` measure and keep the idle tick allocation-free.
    /// A no-op (no backend traffic) while the size is stable.
    fn sync_content_size(&mut self) {
        use crate::session::settings::InfoPanelPosition;
        if !(self.show_info_panel && self.info_panel_position == InfoPanelPosition::Auto) {
            return;
        }
        if self.last_content_size != Some(self.content_area_size()) {
            self.resize_sessions_to_content_area();
        }
    }

    pub fn tick(&mut self) {
        self.tick_core();
        self.tick_background();
    }

    /// Drain the queued editor round-trip (the review's `E`), if any. Called
    /// by the main loop, which owns the terminal teardown/rebuild around
    /// running the editor.
    pub fn take_pending_editor(&mut self) -> Option<EditorRequest> {
        self.pending_editor.take()
    }

    /// Called by the main loop after an editor round-trip: the editor owned
    /// the whole screen, so force a repaint; surface a spawn failure; and for
    /// a Working-target trip reload the review so the edit shows.
    pub fn editor_closed(&mut self, reload_review: bool, error: Option<String>) {
        self.request_redraw();
        if let Some(e) = error {
            self.set_error(e);
            return;
        }
        if reload_review && self.active_review().is_some() {
            self.cr_reload();
        }
    }

    /// The deterministic half of [`Self::tick`]: everything that only reads
    /// state, polls already-running work, or writes through the in-process DB —
    /// no Tokio task is ever spawned here. Split out so the acceptance harness
    /// can drive the tick pipeline (status derivation, timer expiry, search
    /// debounce, automation firing, external-change polling) hermetically and
    /// without a runtime; `main`'s loop always runs both halves via `tick()`.
    pub(crate) fn tick_core(&mut self) {
        self.metrics.tick_count = self.metrics.tick_count.wrapping_add(1);

        self.tick_jump_overlay();
        self.tick_prefix_hint();

        self.tick_global_search_content();
        self.poll_global_search_file_index();

        self.refresh_session_statuses();

        // Forward clipboard writes panes made via OSC 52 (e.g. an agent's
        // `/copy`) to the user's clipboard.
        self.drain_pane_clipboard_copies();

        // Catch layout drift the event loop can't see (the auto info-pane dock
        // moving as content changes) and re-push PTY sizes.
        self.sync_content_size();

        // Convert any live remote session whose host connection just dropped into
        // an unreachable placeholder + queue it for reconnect (see the method
        // doc for why `has_exited()` is the reliable host-loss signal here).
        self.detect_lost_remote_sessions();

        // Poll for sync results from background worktree sync threads
        self.poll_sync_remotes();
        self.poll_sync_results();

        // Poll for backgrounded interactive spawn work (branch listing +
        // worktree creation + `Session::spawn`) so `Ctrl+N` never freezes
        // the UI.
        self.poll_branch_load();
        self.poll_worktree_create();
        self.poll_session_spawn();

        // Fill the conversation-import picker once its disk scan completes.
        self.poll_conversation_import();

        // Apply a finished off-thread code-review diff build (ADR-P8).
        self.poll_review_build();

        // Adopt remote-backed sessions whose host discovery (started at
        // restore) has since completed.
        self.poll_remote_restore();

        // Send deferred inputs whose delay has elapsed
        self.drain_deferred_inputs();

        self.tick_expire_timers();

        self.poll_external_changes();

        // Fire due automations. The first tick forces an immediate catch-up
        // pass so automations missed while the TUI was down run right away;
        // afterwards it runs on the regular ~1 s cadence.
        self.process_automations(self.metrics.tick_count == 1);

        // Refresh cached automations + tasks for the UI (same cadence). The
        // first tick primes the caches so the panels aren't empty on open.
        if self.metrics.tick_count == 1 || self.metrics.tick_count % 100 == 0 {
            self.refresh_automations();
            self.refresh_tasks();
        }

        // Crash-safety ghost frames: persist changed visible screens so a hard
        // crash / reboot restores ghosts at most one interval stale.
        if self.metrics.tick_count % FRAME_PERSIST_INTERVAL_TICKS == 0 {
            self.persist_dirty_frames();
        }
    }

    /// The spawning half of [`Self::tick`]: kicks off background refreshes
    /// (sysinfo/git/usage shell-outs), the opt-in update check, and the
    /// auto-updater — each lands on a Tokio task. Kept out of
    /// [`Self::tick_core`] so tests driving the tick pipeline never touch the
    /// network, the filesystem outside the harness tempdir, or a runtime.
    fn tick_background(&mut self) {
        self.tick_background_refreshes();

        self.tick_version_check();

        self.poll_auto_update();

        self.tick_perf_window();
    }

    /// Steady-state perf reporting: once per window (under `FRIRING_PERF_LOG`)
    /// log counter deltas + timing percentiles + the window's slow ops, then
    /// reset the per-window timing state so each report stands alone. The
    /// startup line at first paint is separate and unaffected. Both the window
    /// report and an open HUD also refresh the published snapshot
    /// (`friring-cli perf`); a default run publishes nothing.
    fn tick_perf_window(&mut self) {
        let tick = self.metrics.tick_count;
        let window_due = self.perf_log_env && tick % PERF_WINDOW_TICKS == 0;
        let snapshot_due = self.show_perf_hud && tick % PERF_SNAPSHOT_TICKS == 0;
        if !window_due && !snapshot_due {
            return;
        }
        // Publish before the window reset below so the snapshot carries this
        // window's timing percentiles rather than an empty histogram.
        self.publish_perf_snapshot();
        if !window_due {
            return;
        }
        let now = self.perf_counters();
        let d = now.delta(&self.perf_window_base);
        let timings = &self.metrics.timings;
        let slow_ops = timings
            .slow_ops
            .iter_recent()
            .map(|op| format!("{}={}ms", op.name, op.ms))
            .collect::<Vec<_>>()
            .join(",");
        tracing::info!(
            frames = d.frames_rendered,
            redraws_requested = d.redraws_requested,
            redraws_skipped = d.redraws_skipped,
            status_refreshes = d.status_refreshes,
            order_rebuilds = d.ordered_sessions_rebuilds,
            hook_state_loads = d.hook_state_loads,
            external_poll_checks = d.external_poll_checks,
            external_poll_reloads = d.external_poll_reloads,
            frame_p50_us = timings.frame.percentile_us(50),
            frame_p95_us = timings.frame.percentile_us(95),
            frame_max_us = timings.frame.max_us(),
            tick_p50_us = timings.tick.percentile_us(50),
            tick_p95_us = timings.tick.percentile_us(95),
            tick_max_us = timings.tick.max_us(),
            slow_ops = %slow_ops,
            sessions = self.sessions.len(),
            "perf_window"
        );
        self.perf_window_base = now;
        self.metrics.timings.reset_window();
    }

    /// Startup phase durations from `main`, for the published snapshot.
    pub fn set_startup_phases(&mut self, phases: serde_json::Value) {
        self.startup_phases = phases.into();
    }

    /// Write the current counters + timing stats as a JSON blob into the
    /// `metadata` table for `friring-cli perf`. Only called while perf timing
    /// is active (see [`Self::tick_perf_window`]) — the write bumps other
    /// friring connections' `data_version`, so it must never run on a
    /// default-config idle instance. Best-effort: a failed write only warns.
    fn publish_perf_snapshot(&self) {
        let p = self.perf_counters();
        let t = &self.metrics.timings;
        let histo = |h: &metrics_state::DurationHistogram| {
            serde_json::json!({
                "p50_us": h.percentile_us(50),
                "p95_us": h.percentile_us(95),
                "max_us": h.max_us(),
            })
        };
        let slow_ops: Vec<serde_json::Value> = t
            .slow_ops
            .iter_recent()
            .map(|op| serde_json::json!({ "op": op.name, "ms": op.ms }))
            .collect();
        let captured_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let snapshot = serde_json::json!({
            "pid": std::process::id(),
            "captured_at": captured_at,
            "session_count": self.sessions.len(),
            "tick_count": self.metrics.tick_count,
            "counters": {
                "frames_rendered": p.frames_rendered,
                "redraws_requested": p.redraws_requested,
                "redraws_skipped": p.redraws_skipped,
                "status_refreshes": p.status_refreshes,
                "ordered_sessions_rebuilds": p.ordered_sessions_rebuilds,
                "parser_locks_render": p.parser_locks_render,
                "automation_entries_built": p.automation_entries_built,
                "hook_state_loads": p.hook_state_loads,
                "external_poll_checks": p.external_poll_checks,
                "external_poll_reloads": p.external_poll_reloads,
            },
            "frame": histo(&t.frame),
            "tick": histo(&t.tick),
            "slow_ops": slow_ops,
            "startup": self.startup_phases,
        });
        if let Err(e) = self.db.set_perf_snapshot(&snapshot.to_string()) {
            warn!("failed to publish perf snapshot: {e}");
        }
    }

    /// Snapshot of the deterministic render/tick performance counters. Read by
    /// the perf regression tests, the perf HUD, the `perf_window` log line, and
    /// the published snapshot. See [`metrics_state::PerfCounters`].
    pub(crate) fn perf_counters(&self) -> metrics_state::PerfCounters {
        self.metrics.perf
    }

    /// Whether wall-clock perf timing should be collected this iteration:
    /// opted in via `FRIRING_PERF_LOG` or by opening the perf HUD. A cached
    /// bool so the hot loop pays nothing when observability is off.
    pub fn perf_timing_active(&self) -> bool {
        self.perf_log_env || self.show_perf_hud
    }

    /// Record one `terminal.draw` duration (called from the render loop, only
    /// while [`Self::perf_timing_active`]).
    pub fn record_frame_time(&mut self, d: std::time::Duration) {
        self.metrics.timings.frame.record(d);
    }

    /// Record one `App::tick` duration (called from the render loop, only
    /// while [`Self::perf_timing_active`]).
    pub fn record_tick_time(&mut self, d: std::time::Duration) {
        self.metrics.timings.tick.record(d);
    }

    /// Record one `App::update` (input dispatch) duration; outliers land in
    /// the slow-op ring so a stalling key handler is attributable.
    pub fn record_update_time(&mut self, d: std::time::Duration) {
        self.note_slow_op("input_dispatch", d.as_millis() as u64);
    }

    /// Record an already-measured operation duration: at or above
    /// `SLOW_OP_RECORD_MS` it lands in the slow-op ring (HUD / perf-window
    /// line); at or above `SLOW_OP_WARN_MS` it also gets a `warn!` in the log.
    /// The single sink behind [`Self::time_op`] and the workers that time
    /// themselves (e.g. the code-review build).
    pub(crate) fn note_slow_op(&mut self, name: &'static str, ms: u64) {
        if ms >= SLOW_OP_WARN_MS {
            tracing::warn!(op = name, ms, "slow op");
        }
        if ms >= SLOW_OP_RECORD_MS {
            self.metrics
                .timings
                .slow_ops
                .push(metrics_state::SlowOp { name, ms });
        }
    }

    /// Measure a named synchronous UI-thread operation. Call sites are rare,
    /// user-triggered ops (never the per-tick hot path), so this measures
    /// unconditionally: notable durations land in the slow-op ring and
    /// stall-grade ones also get a `warn!` in the log.
    pub(crate) fn time_op<T>(&mut self, name: &'static str, f: impl FnOnce(&mut Self) -> T) -> T {
        let start = std::time::Instant::now();
        let out = f(self);
        self.note_slow_op(name, start.elapsed().as_millis() as u64);
        out
    }

    /// Mark the UI dirty so the render loop paints on its next iteration.
    /// Cheap and idempotent; over-marking only costs an extra (correct) frame.
    /// Also driven by the binary's `main` after a terminal-editor run (popup
    /// cover / suspend teardown) to force a full repaint on return.
    pub fn request_redraw(&mut self) {
        self.needs_redraw = true;
    }

    /// Whether the render loop should paint a frame this iteration: either state
    /// changed since the last paint, or the `FORCE_REDRAW_INTERVAL` floor
    /// elapsed (so time-driven UI — clock, metrics, cursor blink, quiet-session
    /// status transitions — still refreshes without an explicit dirty flag).
    pub fn should_redraw(&self) -> bool {
        self.needs_redraw || clock::elapsed_since(self.last_draw_at) >= FORCE_REDRAW_INTERVAL
    }

    /// Record that a frame was just painted: clear the dirty flag, reset the
    /// forced-redraw timer, and count the requested redraw.
    pub fn mark_redrawn(&mut self) {
        self.needs_redraw = false;
        self.last_draw_at = clock::now();
        self.metrics.bump(|p| &mut p.redraws_requested);
    }

    /// Record that a loop iteration skipped the paint because nothing changed.
    pub fn note_redraw_skipped(&mut self) {
        self.metrics.bump(|p| &mut p.redraws_skipped);
    }

    /// Detect new agent/shell output since the last check and mark the UI dirty
    /// if so. Reads each session's monotonic `last_output_at` atomic — and its
    /// shell pane's, when one is open (a shell keystroke's echo must repaint
    /// immediately, not wait out the forced-redraw floor) — summing them into a
    /// rolling signature (no parser lock); a change means at least one pane
    /// produced output, so the terminal needs repainting.
    pub fn detect_output_redraw(&mut self) {
        let output_gen = self.sessions.iter().fold(0u64, |acc, s| {
            let shell = s.shell_pane.as_ref().map_or(0, |sp| sp.last_output_at());
            acc.wrapping_add(s.last_output_at()).wrapping_add(shell)
        });
        if output_gen != self.last_output_gen {
            self.last_output_gen = output_gen;
            self.needs_redraw = true;
        }
    }

    /// Content signature of the inputs that determine the session-list ordering.
    /// [`crate::ui::project_list::compute_session_order`] is a pure function of
    /// exactly these per-session fields (grouping by `repo_display_names`,
    /// sorting by `display_order`, nesting by `id`/`parent_session_id`) plus the
    /// session count/order — never status — so an unchanged signature means the
    /// cached order is still valid. Cheaper than recomputing the order
    /// (no grouping HashMap, sorts, nest recursion, or label allocations).
    fn session_order_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.sessions.len().hash(&mut h);
        for s in &self.sessions {
            s.info.id.hash(&mut h);
            s.info.display_order.hash(&mut h);
            s.info.parent_session_id.hash(&mut h);
            s.info.repo_display_names.hash(&mut h);
        }
        h.finish()
    }

    /// Drive the opt-in GitHub update check. Off the render path: on the first
    /// tick (when the flag is on and the on-disk cache is stale) it fires a
    /// single background network refresh; the result only ever lands by
    /// re-reading the cache, so rendering never makes a network call.
    fn tick_version_check(&mut self) {
        if !self.features.version_check {
            return;
        }

        // One attempt per launch: fire on the first tick if the cache is stale.
        if self.metrics.tick_count == 1
            && !self.version_check_task.in_progress()
            && crate::agent::version_check::cache_is_stale()
        {
            let tx = self.version_check_task.start();
            tokio::task::spawn_blocking(move || {
                let _ = tx.send(crate::agent::version_check::refresh_cache().map(|_| ()));
            });
        }

        // Apply a completed refresh by re-reading the cache (single source of
        // truth). A failed/dead refresh leaves the prior badge untouched.
        if let background::TaskPoll::Done(Ok(())) = self.version_check_task.poll() {
            self.update_status = crate::agent::version_check::read_cached_status();
        }
    }

    /// Expire the undo window for a pending delete and auto-clear stale status
    /// messages so default project/session counts reappear.
    fn tick_expire_timers(&mut self) {
        // Finalize pending delete after undo timeout
        if let Some(ref pending) = self.pending_delete {
            if clock::elapsed_since(pending.created_at) >= UNDO_TIMEOUT {
                self.finalize_pending_delete();
            }
        }

        // Auto-expire status messages so default project/session counts reappear
        if let Some(ref msg) = self.status_message {
            if clock::elapsed_since(msg.created_at) >= STATUS_MESSAGE_TIMEOUT {
                self.status_message = None;
            }
        }
    }

    /// Apply completed background metric/git-stat/memory/usage refreshes and
    /// kick off the next ones on their cadences. All run off the UI thread
    /// (sysinfo + statusline file reads / `git` shell-outs / the process-table
    /// read) so a slow read never stalls rendering — mirrors the worktree-sync
    /// poll.
    fn tick_background_refreshes(&mut self) {
        self.poll_metrics_refresh();
        self.poll_git_stats();
        self.poll_cc_refresh();
        self.poll_activity_refresh();
        self.poll_memory_refresh();

        if self.metrics.tick_count % METRICS_REFRESH_TICKS == 0 {
            self.start_metrics_refresh();
        }
        if self.features.cc_activity && self.metrics.tick_count % CC_REFRESH_TICKS == 0 {
            self.start_cc_refresh();
        }
        // Offset half a cadence from the tree scan so the two never share a tick.
        if self.features.cc_activity
            && self.metrics.tick_count % ACTIVITY_REFRESH_TICKS == ACTIVITY_REFRESH_TICKS / 2
        {
            self.start_activity_refresh();
        }
        if self.metrics.tick_count % GIT_REFRESH_TICKS == 0 {
            self.start_git_stats_refresh();
        }
        // Offset half a cadence so the process-table read never shares a tick
        // with the 1 s scans above.
        if self.features.session_memory
            && self.metrics.tick_count % MEMORY_REFRESH_TICKS == MEMORY_REFRESH_TICKS / 2
        {
            self.start_memory_refresh();
        }
        if self.metrics.tick_count % CONFIG_RELOAD_TICKS == 0 {
            self.poll_config_reload();
        }

        // Drain any completed background usage fetches into the cache.
        while let Ok((key, usage)) = self.usage_rx.try_recv() {
            self.usage.insert(key, usage);
        }
        // Kick off usage fetches early and then on a slow cadence.
        if self.metrics.tick_count % USAGE_REFRESH_TICKS == 1 {
            self.spawn_usage_fetches();
        }
    }

    /// Recompute each session's status/activity/notification for this tick.
    ///
    /// Status is **hooks-driven**: agents report `working`/`blocked`/`done` via
    /// `friring-cli session signal` (local sessions) or a tmux pane user option
    /// pushed over the control-mode subscription (remote sessions — drained
    /// below into the same hook columns), persisted in `sessions` and read here
    /// in one batch (see [`derive_session_status`]). A `done` session stays
    /// `Done` until the user moves focus *off* it (acknowledged → `Idle`). The
    /// OSC terminal title is still captured for the live activity line, but no
    /// longer drives status.
    fn refresh_session_statuses(&mut self) {
        // Before the data_version gate, so a persisted remote event reloads the
        // cache in this same tick.
        self.drain_remote_hook_events();
        self.metrics.bump(|p| &mut p.status_refreshes);
        let active_index = self.active_index;
        // Reload the persisted hook columns only when the DB actually changed —
        // an *external* `session signal` bumps `data_version`, but our own
        // `seen_at` writes (below) do not — otherwise reuse the cached map. This
        // replaces a full sessions-table scan on every (~10 ms) tick with a
        // cheap in-memory `PRAGMA data_version` read — itself throttled to
        // every `HOOK_VERSION_CHECK_TICKS` ticks (~100 ms; still ~10 rusqlite
        // round-trips/s saved), except when the cache was explicitly
        // invalidated (a remote hook event / restart wrote on our own
        // connection, which the pragma can't see — check immediately). Worst
        // case an external signal shows ~100 ms late, under any perceptible
        // threshold. See `docs/PERFORMANCE.md` (ADR-P6 + ADR-P10).
        let version_check_due = self.hook_states_version.is_none()
            || self.metrics.tick_count % HOOK_VERSION_CHECK_TICKS == 0;
        if version_check_due {
            self.metrics.bump(|p| &mut p.data_version_checks);
            let version = self.db.data_version().ok();
            if self.hook_states_version.is_none() || version != self.hook_states_version {
                self.metrics.bump(|p| &mut p.hook_state_loads);
                self.cached_hook_states = self.db.load_hook_states().unwrap_or_default();
                self.hook_states_version = version;
            }
        }
        // "Seen" writes are deferred past the &mut self.sessions borrow below.
        let mut seen_writes: Vec<(crate::session::SessionId, i64)> = Vec::new();

        // A `done` session is acknowledged ("seen" → Idle) when the user moves
        // *off* it — not the instant it finishes under them — so the blue `Done`
        // state is actually visible after a turn for the session you're watching.
        // Detect the focus change and mark the session you just left, if it was
        // an unseen `done`.
        let active_id = self.sessions.get(active_index).map(|s| s.info.id);
        if active_id != self.last_active_session_id {
            if let Some((prev, state_at)) =
                self.unseen_done_on_focus_leave(&self.cached_hook_states)
            {
                seen_writes.push((prev, state_at));
            }
            self.last_active_session_id = active_id;
        }

        // Track whether any visible field changed so a quiet transition (no new
        // output, so the output detector won't catch it) still repaints promptly
        // instead of waiting for the forced-redraw floor.
        let (changed, meta_syncs) = Self::apply_session_status_fields(
            &mut self.sessions,
            &self.cached_hook_states,
            &seen_writes,
        );
        self.metrics.perf.agent_meta_syncs =
            self.metrics.perf.agent_meta_syncs.wrapping_add(meta_syncs);

        // Persist the seen marks now that the sessions borrow is released, and
        // mirror them into the cache write-through: our own write doesn't move
        // `data_version`, so without this the next tick would reload nothing and
        // re-derive the just-acknowledged `done` session back to `Done`.
        // Guarded above by `seen_at < state_at`, so the focused session doesn't
        // bump `data_version` every tick.
        for (id, state_at) in seen_writes {
            let _ = self.db.mark_session_seen(id, state_at);
            if let Some(hook) = self.cached_hook_states.get_mut(&id) {
                hook.seen_at = Some(state_at);
            }
        }

        // Advance the spinner unconditionally (it must tick every call, not just
        // when no field changed — `||` would short-circuit past it), then redraw
        // on either trigger.
        let spinner_redraw = self.advance_spinner_frame();
        if changed || spinner_redraw {
            self.request_redraw();
        }
        self.dispatch_status_notifications();
        self.nudge_review_on_idle();
    }

    /// Toast a re-review nudge when a session whose review was sent (`e`,
    /// [`Self::review_nudge_watch`]) finishes working — the moment to reopen
    /// the review and check the agent's fixes. Fires once per send (the watch
    /// entry is consumed), only after a `Working → Idle/Done` edge (the send
    /// itself usually lands while the agent is still idle), and only when
    /// `[review] nudge_on_idle` is on. No auto-rebuild — a hint only.
    fn nudge_review_on_idle(&mut self) {
        if self.review_nudge_watch.is_empty() {
            return;
        }
        let statuses: std::collections::HashMap<SessionId, (SessionStatus, String)> = self
            .sessions
            .iter()
            .map(|s| (s.info.id, (s.info.status, s.info.name.clone())))
            .collect();
        let active = self.active_session_id();
        let mut fired: Option<String> = None;
        self.review_nudge_watch.retain(|id, prev| {
            // A deleted session's watch is dropped, bounding the map.
            let Some((cur, name)) = statuses.get(id) else {
                return false;
            };
            let finished = *prev == SessionStatus::Working
                && matches!(cur, SessionStatus::Idle | SessionStatus::Done);
            *prev = *cur;
            if finished {
                fired = Some(if active == Some(*id) {
                    "Agent idle — F7 to re-review, F5 to reload".to_string()
                } else {
                    format!("Agent idle in {name} — F7 to re-review")
                });
            }
            !finished
        });
        if let Some(msg) = fired {
            if self.review_settings.nudge_on_idle {
                self.set_info(msg);
            }
        }
    }

    /// Force [`Self::cached_hook_states`] to reload on the next status refresh.
    /// Needed after this process writes hook columns on its *own* DB connection
    /// (e.g. clearing state on restart): such writes don't move our connection's
    /// `data_version`, so the version gate wouldn't otherwise notice them.
    fn invalidate_hook_state_cache(&mut self) {
        self.hook_states_version = None;
    }

    /// Drain remote-hook status events from every backend and persist them,
    /// exactly as `friring-cli session signal` would have done locally.
    ///
    /// A remote agent's hooks set a tmux pane user option; the backend's
    /// control-mode subscription queues `(pane_id, state)` pairs (see
    /// [`crate::agent::backend::SessionBackend::take_hook_state_events`]).
    /// Each is resolved to a session by **backend name + pane id** — pane ids
    /// collide across hosts — and written through [`set_hook_state`]
    /// (`crate::storage`), so the whole derivation downstream (Done→seen
    /// acknowledgment, OS notifications, rollups, the stuck-working fallback)
    /// is shared with local sessions.
    fn drain_remote_hook_events(&mut self) {
        // The value is remote-host-controlled free text: allow-list it (the
        // same states `session signal` accepts) and never interpolate it.
        const VALID_STATES: [&str; 4] = ["working", "blocked", "done", "idle"];
        // Unmatched events are retried this long — comfortably past a slow
        // host's background restore — then dropped (bounded below, so another
        // instance's panes can't accumulate).
        const PENDING_TTL: std::time::Duration = std::time::Duration::from_secs(120);
        const PENDING_CAP: usize = 256;
        // Collect first: the registry borrow must end before &mut self below.
        let batches: Vec<(String, Vec<(String, String)>)> = self
            .backends
            .all_backends()
            .map(|b| (b.name().to_string(), b.take_hook_state_events()))
            .filter(|(_, events)| !events.is_empty())
            .collect();
        // Older pending retries first, so per-pane event order is preserved.
        let now = clock::now();
        let mut queue = std::mem::take(&mut self.pending_remote_hook_events);
        for (backend_name, events) in batches {
            for (pane_id, state) in events {
                queue.push((backend_name.clone(), pane_id, state, now));
            }
        }
        // States applied *this drain*: the dedupe below must compare against
        // the latest write, not `cached_hook_states` (only invalidated, not
        // reloaded, mid-loop) — else the second event of a `working`→`done`
        // batch that matches the stale cached value is swallowed.
        let mut applied: HashMap<crate::session::SessionId, String> = HashMap::new();
        for (backend_name, pane_id, state, arrived) in queue {
            if !VALID_STATES.contains(&state.as_str()) {
                continue;
            }
            let Some(id) = self
                .sessions
                .iter()
                .find(|s| {
                    s.backend_name() == backend_name
                        && s.info.backend_id.as_deref() == Some(pane_id.as_str())
                })
                .map(|s| s.info.id)
            else {
                // No matching session *yet*: the subscription's initial report
                // often lands before the background restore adopts the pane,
                // so park the event for a later tick instead of losing it.
                if now.duration_since(arrived) < PENDING_TTL
                    && self.pending_remote_hook_events.len() < PENDING_CAP
                {
                    self.pending_remote_hook_events
                        .push((backend_name, pane_id, state, arrived));
                }
                continue;
            };
            // Dedupe against the current value: the subscription re-reports it
            // on (re)connect, and re-stamping an already-acknowledged `done`
            // would resurrect it as unseen and re-fire its OS notification on
            // every TUI restart.
            let current = applied.get(&id).map(String::as_str).or_else(|| {
                self.cached_hook_states
                    .get(&id)
                    .and_then(|h| h.state.as_deref())
            });
            if current == Some(state.as_str()) {
                continue;
            }
            if self.db.set_hook_state(id, &state).is_ok() {
                // Own-connection write: data_version won't move, force the
                // reload so this tick's derivation sees the exact row.
                self.invalidate_hook_state_cache();
                applied.insert(id, state);
            }
        }
    }

    /// If the just-left session (`last_active_session_id`) is an unseen `done`,
    /// return its `(id, state_at)` so the caller can queue a "seen" write.
    fn unseen_done_on_focus_leave(
        &self,
        hooks: &HashMap<crate::session::SessionId, crate::storage::HookRow>,
    ) -> Option<(crate::session::SessionId, i64)> {
        let prev = self.last_active_session_id?;
        let hook = hooks.get(&prev)?;
        if hook.state.as_deref() != Some("done") {
            return None;
        }
        let state_at = hook.state_at.unwrap_or(0);
        if hook.seen_at.unwrap_or(0) < state_at {
            Some((prev, state_at))
        } else {
            None
        }
    }

    /// Recompute each session's status/activity/notification from the hook rows
    /// and apply them in place. Returns whether any visible field changed.
    fn apply_session_status_fields(
        sessions: &mut [Session],
        hooks: &HashMap<crate::session::SessionId, crate::storage::HookRow>,
        seen_writes: &[(crate::session::SessionId, i64)],
    ) -> (bool, u64) {
        let mut changed = false;
        let mut meta_syncs = 0u64;
        for session in sessions.iter_mut() {
            // A placeholder (unreachable remote) has no live pane / hooks; keep
            // its `Unreachable` status until the host recovers and it adopts.
            if session.is_placeholder() {
                continue;
            }
            let id = session.info.id;
            // `just_seen`: the focus-leave check above queued this session's seen
            // mark this tick (the DB write lands after this loop), so reflect it
            // now rather than waiting a tick.
            let just_seen = seen_writes.iter().any(|(sid, _)| *sid == id);
            let new_status = derive_session_status(
                hooks.get(&id),
                session.has_exited(),
                just_seen,
                session.millis_since_last_output(),
            );
            if session.info.status != new_status {
                changed = true;
            }
            session.info.status = new_status;

            // Live activity text (OSC title) + latest pushed notification
            // (OSC 9/777): re-read only when the reader thread wrote something
            // new — its generation counter gates the two mutex locks + String
            // clones that otherwise ran per session per ~10 ms tick (ADR-P10).
            if let Some((new_activity, new_notification)) = session.sync_agent_meta() {
                meta_syncs += 1;
                if session.info.agent_activity != new_activity
                    || session.info.notification != new_notification
                {
                    changed = true;
                }
                session.info.agent_activity = new_activity;
                session.info.notification = new_notification;
            }
        }
        (changed, meta_syncs)
    }

    /// Advance the Working spinner from the (deterministic) tick counter, and
    /// report whether a repaint is needed because the frame ticked over *while*
    /// something is working — so an idle TUI still rests at ~4 fps but a working
    /// session animates smoothly.
    fn advance_spinner_frame(&mut self) -> bool {
        let new_frame = (self.metrics.tick_count / SPINNER_TICKS_PER_FRAME) as usize
            % crate::ui::SPINNER_FRAMES.len();
        let spinner_advanced = new_frame != self.spinner_frame;
        self.spinner_frame = new_frame;
        let any_working = self
            .sessions
            .iter()
            .any(|s| s.info.status == SessionStatus::Working);
        any_working && spinner_advanced
    }

    /// Current `Working`-spinner frame index (into [`crate::ui::SPINNER_FRAMES`]).
    pub(crate) fn spinner_frame(&self) -> usize {
        self.spinner_frame
    }

    /// Fire OS notifications for any session that just crossed into a
    /// needs-attention state this tick. No-op when the feature is disabled.
    fn dispatch_status_notifications(&mut self) {
        let Some(state) = self.notification_state.as_mut() else {
            return;
        };
        let active_index = self.active_index;
        let now = clock::now();
        for (idx, session) in self.sessions.iter().enumerate() {
            let id = session.info.id;
            let status = session.info.status;
            let is_active = idx == active_index;
            if state.observe(id, status, is_active, now) != notify_state::TransitionDecision::Fire {
                continue;
            }
            let n = NotificationState::build_notification(
                id,
                &session.info.name,
                &session.info.agent,
                session.info.notification.as_deref(),
                state.sound_enabled(),
            );
            state.send(n);
        }
        // Cheap: bounds the bookkeeping after deletions / restarts.
        let live: Vec<SessionId> = self.sessions.iter().map(|s| s.info.id).collect();
        state.prune_to(&live);
    }

    /// Poll for external state changes from other friring instances (DB-based)
    /// and apply any theme change / session delta they produced.
    fn poll_external_changes(&mut self) {
        let Ok(Some(result)) = sync::poll_for_changes(&mut self.sync_state, &mut self.db) else {
            // Throttled (or errored): no `data_version` check ran this tick, so
            // it isn't counted. Even with no broader DB change, a notification
            // click may have landed: it writes a single row and the sync layer
            // doesn't distinguish, so we always check.
            self.apply_pending_focus_request();
            return;
        };
        // A `Some` result means the cheap `PRAGMA data_version` check actually
        // ran; `db_changed` further means it found a change and did the full
        // shared-state reload.
        self.metrics.bump(|p| &mut p.external_poll_checks);
        if result.db_changed {
            self.metrics.bump(|p| &mut p.external_poll_reloads);
            self.apply_external_theme_change();
            self.apply_pending_focus_request();
        }
        if !result.delta.is_empty() {
            self.handle_external_state_change(result.delta);
        }
    }

    /// Drain the OS-notification click handler's "focus this session" request
    /// (written from another thread/process) and switch the active session.
    /// Silently no-ops when the session has since been deleted or the
    /// stored value isn't a valid UUID.
    fn apply_pending_focus_request(&mut self) {
        let Ok(Some(raw)) = self.db.take_pending_focus_session_id() else {
            return;
        };
        let Ok(id) = raw.parse::<SessionId>() else {
            debug!("ignoring malformed pending_focus_session_id: {raw}");
            return;
        };
        let Some(idx) = self.sessions.iter().position(|s| s.info.id == id) else {
            debug!("focus request for unknown session {id}; ignoring");
            return;
        };
        self.set_active_index(idx);
        self.focus = InputFocus::Terminal;
        info!("focused session {id} from notification click");
    }

    /// Pick up theme changes made by other friring processes (e.g. an MCP
    /// `set_theme` call from another session).
    fn apply_external_theme_change(&mut self) {
        let Ok(Some(name)) = self.db.get_active_theme() else {
            return;
        };
        if name == self.active_theme.name {
            return;
        }
        let Some(entry) = crate::ui::theme::find_theme_entry(&name) else {
            return;
        };
        crate::ui::theme::set_active(entry.palette.clone());
        self.active_theme = entry;
    }

    /// Plan the account-usage refreshes for the current session list: one
    /// entry per distinct [`UsageKey`] (agent + host), covering every case —
    /// local sessions use local credentials; a remote session resolves its
    /// `HostDef` so credentials are read on that host; a placeholder session
    /// (host currently unreachable) or a host missing from `hosts.toml`
    /// yields a static note instead of a doomed ssh attempt. Pure (no spawns)
    /// so the scoping rules are unit-testable.
    fn plan_usage_fetches(&self) -> Vec<(UsageKey, UsageFetchPlan)> {
        let mut seen: HashMap<UsageKey, usize> = HashMap::new();
        let mut plan: Vec<(UsageKey, UsageFetchPlan)> = Vec::new();
        for session in &self.sessions {
            if !crate::usage::is_supported(&session.info.agent) {
                continue;
            }
            let key: UsageKey = (session.info.agent.clone(), session.info.remote_host.clone());
            let entry = match &key.1 {
                None => UsageFetchPlan::Fetch(None),
                Some(_) if session.is_placeholder() => {
                    UsageFetchPlan::Unavailable("usage unavailable (host unreachable)")
                }
                Some(host_name) => match self.hosts.get(host_name) {
                    Some(host) => UsageFetchPlan::Fetch(Some(host.clone())),
                    None => UsageFetchPlan::Unavailable("usage unavailable (host not configured)"),
                },
            };
            match seen.get(&key) {
                None => {
                    seen.insert(key.clone(), plan.len());
                    plan.push((key, entry));
                }
                // Mid-reconnect a host's sessions can be mixed (one adopted,
                // one still a placeholder): a live session's fetch beats a
                // placeholder's note for the same scope.
                Some(&i) => {
                    if matches!(plan[i].1, UsageFetchPlan::Unavailable(_))
                        && matches!(entry, UsageFetchPlan::Fetch(_))
                    {
                        plan[i].1 = entry;
                    }
                }
            }
        }
        plan
    }

    /// Spawn background usage/rate-limit fetches for each distinct supported
    /// (agent, host) scope currently in the session list (see
    /// [`Self::plan_usage_fetches`]). Results return via `usage_tx` and are
    /// drained in [`Self::tick`]. Network/process work runs off the UI thread.
    fn spawn_usage_fetches(&mut self) {
        for (key, entry) in self.plan_usage_fetches() {
            match entry {
                UsageFetchPlan::Fetch(host) => {
                    let tx = self.usage_tx.clone();
                    tokio::spawn(async move {
                        let usage = crate::usage::fetch(&key.0, host.as_ref()).await;
                        let _ = tx.send((key, usage));
                    });
                }
                // Keep last-known data over a transient outage; the note only
                // fills a scope that never fetched successfully.
                UsageFetchPlan::Unavailable(note) => {
                    self.usage
                        .entry(key)
                        .or_insert_with(|| crate::session::AgentUsage {
                            note: Some(note.to_string()),
                            ..Default::default()
                        });
                }
            }
        }
    }

    /// Kick off a background git-stats refresh for the active session.
    ///
    /// Gathers the worktree paths on the UI thread (cheap) and shells out to
    /// `git` on a blocking task; the result is applied in [`Self::poll_git_stats`].
    /// Only one refresh runs at a time.
    fn start_git_stats_refresh(&mut self) {
        if self.git_stats.in_progress() {
            return;
        }
        let Some(session) = self.sessions.get(self.active_index) else {
            return;
        };
        let session_id = session.info.id;

        // Aggregate across all worktrees; fall back to the cwd if there are none.
        let paths: Vec<PathBuf> = if session.info.worktrees.is_empty() {
            session.info.cwd.iter().cloned().collect()
        } else {
            session
                .info
                .worktrees
                .iter()
                .map(|wt| wt.worktree_path.clone())
                .collect()
        };

        let tx = self.git_stats.start();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send((session_id, aggregate_git_stats(&paths)));
        });
    }

    /// Apply a completed background git-stats refresh, if one has finished.
    fn poll_git_stats(&mut self) {
        // A dead worker (e.g. panic) just clears the guard so the next
        // cadence can retry.
        if let background::TaskPoll::Done((session_id, stats)) = self.git_stats.poll() {
            if let Some(session) = self.sessions.iter_mut().find(|s| s.info.id == session_id) {
                session.info.git_stats = stats;
            }
        }
    }

    /// Kick off a background system-metrics refresh.
    ///
    /// The sysinfo collector is moved into the worker (and returned via the
    /// result) so CPU deltas persist across refreshes; statusline file reads and
    /// the active session's PID lookup (a control-mode round-trip) all run off
    /// the UI thread. Only one refresh runs at a time; the result is applied in
    /// [`Self::poll_metrics_refresh`].
    fn start_metrics_refresh(&mut self) {
        if self.metrics_refresh.in_progress() {
            return;
        }
        let Some(sys) = self.metrics.sys.take() else {
            return;
        };

        // Skip a placeholder (unreachable remote): it has no live pane, and its
        // dummy backend/empty id would trigger a pointless ssh round-trip.
        let active = self
            .sessions
            .get(self.active_index)
            .filter(|s| !s.is_placeholder())
            .map(|s| s.backend_handle());

        let metrics_files: Vec<(SessionId, PathBuf)> = self
            .sessions
            .iter()
            .filter_map(|s| {
                let sid = s.info.agent_session_id.as_deref()?;
                Some((s.info.id, crate::paths::session_metrics_file(sid)?))
            })
            .collect();

        let tx = self.metrics_refresh.start();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(collect_system_metrics(sys, active, metrics_files));
        });
    }

    /// Apply a completed background metrics refresh, if one has finished.
    fn poll_metrics_refresh(&mut self) {
        match self.metrics_refresh.poll() {
            background::TaskPoll::Pending => {}
            background::TaskPoll::Done(refresh) => {
                self.metrics.sys = Some(refresh.sys);
                self.metrics.system_metrics = refresh.metrics;
                for (session_id, metrics) in refresh.agent_metrics {
                    if let Some(session) =
                        self.sessions.iter_mut().find(|s| s.info.id == session_id)
                    {
                        session.info.agent_metrics = Some(metrics);
                    }
                }
            }
            // Worker died without returning `sys` (e.g. panic): recreate the
            // collector so metrics resume next cadence. CPU-delta history is
            // lost, not correctness.
            background::TaskPoll::Died => {
                self.metrics.sys.get_or_insert_with(sysinfo::System::new);
            }
        }
    }

    /// Send deferred inputs whose scheduled tick has arrived.
    fn drain_deferred_inputs(&mut self) {
        let tick = self.metrics.tick_count;
        // Partition: send the ones that are ready, keep the rest.
        let mut remaining = Vec::new();
        for (session_id, data, send_at) in std::mem::take(&mut self.deferred_inputs) {
            if tick >= send_at {
                if let Some(session) = self.sessions.iter().find(|s| s.info.id == session_id) {
                    if let Err(e) = session.send_input(data) {
                        error!("Failed to send deferred input: {e}");
                    }
                }
            } else {
                remaining.push((session_id, data, send_at));
            }
        }
        self.deferred_inputs = remaining;
    }

    /// Poll for completed worktree sync results and handle them.
    fn poll_sync_results(&mut self) {
        let Some(rx) = &self.worktree_sync.rx else {
            return;
        };

        // Drain everything currently buffered. A worker thread that *panics*
        // drops its sender without sending every result, so `completed` may
        // never reach `pending`; detecting the channel disconnecting (all
        // senders gone) lets us finalize with whatever arrived instead of
        // leaving `in_progress` stuck forever.
        let mut disconnected = false;
        loop {
            match rx.try_recv() {
                Ok((session_id, result)) => {
                    self.worktree_sync.completed.push((session_id, result));
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        let all_received = self.worktree_sync.completed.len() >= self.worktree_sync.pending;
        if all_received || disconnected {
            self.worktree_sync.in_progress = false;
            self.worktree_sync.rx = None;
            self.finish_sync();
        }
    }

    /// Finalize sync: compose status message and send conflict prompts.
    fn finish_sync(&mut self) {
        let results = std::mem::take(&mut self.worktree_sync.completed);
        let mut synced = 0usize;
        let mut conflicts = 0usize;
        let mut errors = Vec::new();

        for (session_id, result) in results {
            match result {
                git::SyncResult::Synced => synced += 1,
                git::SyncResult::Conflict { base_ref } => {
                    conflicts += 1;
                    self.send_conflict_prompt(session_id, &base_ref);
                }
                git::SyncResult::Error(msg) => errors.push(msg),
            }
        }

        if !errors.is_empty() {
            self.set_error(format!("Sync failed: {}", errors.join(", ")));
        } else if conflicts > 0 {
            self.set_status(
                StatusLevel::Info,
                format!("{synced} synced, {conflicts} conflict(s) (sent to Claude)"),
            );
        } else {
            self.set_status(StatusLevel::Success, format!("{synced} worktree(s) synced"));
        }
    }

    /// Send a conflict resolution prompt to a session via bracketed paste,
    /// with a deferred Enter so the app processes the text first. `base_ref` is
    /// the ref the rebase actually targeted (resolved per-worktree by
    /// [`git::sync_worktree`]), so the prompt names it exactly rather than
    /// assuming `origin/main`.
    fn send_conflict_prompt(&mut self, session_id: SessionId, base_ref: &str) {
        // `git fetch --all`, not bare `git fetch`: the base ref may point at a
        // user-chosen non-default remote (e.g. `fork/main`), which a bare fetch
        // (default remote only) would leave stale before the rebase.
        let prompt = format!(
            "Please sync this worktree with {base_ref}. Run: git fetch --all && git rebase \
             {base_ref} -- if there are conflicts, resolve them and continue the \
             rebase with git rebase --continue."
        );
        if let Some(session) = self.sessions.iter().find(|s| s.info.id == session_id) {
            let mut paste = b"\x1b[200~".to_vec();
            paste.extend_from_slice(prompt.as_bytes());
            paste.extend_from_slice(b"\x1b[201~");
            if let Err(e) = session.send_input(paste) {
                error!("Failed to send sync prompt to session: {e}");
            } else {
                self.deferred_inputs.push((
                    session_id,
                    b"\r".to_vec(),
                    self.metrics.tick_count + DEFERRED_INPUT_DELAY_TICKS,
                ));
            }
        }
    }

    /// Start syncing the active session's worktrees with their base ref.
    ///
    /// The `git remote` listing for the involved repos runs on a background
    /// thread first (no git on the UI thread, the ADR-P12 discipline), polled
    /// by [`Self::poll_sync_remotes`]. Repos with more than one remote route
    /// through the sync base picker before the run launches; the rest use the
    /// default origin chain.
    pub(crate) fn start_sync(&mut self) {
        if self.worktree_sync.in_progress
            || self.worktree_sync.remotes_load.in_progress()
            || self.worktree_sync.awaiting.is_some()
        {
            return;
        }

        // Resolve the active session's host (an SSH/WSL `HostDef`, or `None`
        // for a local session) so every git subcommand runs on the host that
        // actually owns the worktree — a remote worktree path doesn't exist
        // locally, so syncing it locally failed with "no such file or
        // directory".
        let host = self
            .active_session()
            .and_then(|s| s.info.remote_host.as_deref())
            .and_then(|name| self.hosts.get(name))
            .cloned();

        let worktree_sessions: Vec<_> = self
            .active_session()
            .into_iter()
            .flat_map(|s| {
                s.info
                    .worktrees
                    .iter()
                    .map(move |wt| (s.info.id, wt.worktree_path.clone(), wt.repo_path.clone()))
            })
            .collect();

        if worktree_sessions.is_empty() {
            self.set_status(StatusLevel::Info, "No worktrees to sync");
            return;
        }

        let mut repos: Vec<PathBuf> = worktree_sessions
            .iter()
            .map(|(_, _, repo)| repo.clone())
            .collect();
        repos.sort();
        repos.dedup();

        let tx = self.worktree_sync.remotes_load.start();
        std::thread::spawn(move || {
            let remotes = repos
                .into_iter()
                .map(|repo| {
                    let remotes = git::list_remotes(&repo);
                    (repo, remotes)
                })
                .collect::<Vec<_>>();
            let _ = tx.send(remotes);
        });

        self.worktree_sync.awaiting = Some(sync_state::PendingSyncRun {
            worktrees: worktree_sessions,
            queue: Vec::new(),
            chosen: std::collections::HashMap::new(),
            host,
        });
        self.set_status(StatusLevel::Info, "Preparing sync...");
    }

    /// Apply a completed background remote listing to the parked sync run:
    /// launch it directly when every repo has at most one remote, else open
    /// the base picker for the first multi-remote repo.
    fn poll_sync_remotes(&mut self) {
        let remotes = match self.worktree_sync.remotes_load.poll() {
            background::TaskPoll::Pending => return,
            background::TaskPoll::Died => {
                self.worktree_sync.awaiting = None;
                self.set_error("Sync failed (remote listing worker died)");
                return;
            }
            background::TaskPoll::Done(remotes) => remotes,
        };
        let Some(mut run) = self.worktree_sync.awaiting.take() else {
            return;
        };

        for (repo, remotes) in remotes {
            match remotes.as_slice() {
                // Multi-remote repos need an explicit base — queue a picker.
                [_, _, ..] => run.queue.push((repo, remotes)),
                // A single remote named other than `origin` would fail the
                // default chain's hardcoded `git fetch origin` — pin it.
                [only] if only != "origin" => {
                    run.chosen.insert(repo, only.clone());
                }
                // `origin` only (or no remotes): the default chain applies.
                _ => {}
            }
        }

        if run.queue.is_empty() {
            self.launch_sync_run(run);
        } else {
            self.worktree_sync.awaiting = Some(run);
            self.open_sync_base_picker();
        }
    }

    /// Open the base picker for the front of the parked run's repo queue,
    /// preselecting the repo's saved default remote (falling back to `origin`).
    fn open_sync_base_picker(&mut self) {
        let Some(run) = &self.worktree_sync.awaiting else {
            return;
        };
        let Some((repo, remotes)) = run.queue.first() else {
            return;
        };
        let saved = self.db.get_sync_base_remote(repo).ok().flatten();
        let index = saved
            .and_then(|s| remotes.iter().position(|r| *r == s))
            .or_else(|| remotes.iter().position(|r| r == "origin"))
            .unwrap_or(0);
        self.modal = modals::Modal::SyncBasePicker(modals::SyncBasePickerModal {
            repo_name: repo
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| repo.display().to_string()),
            remotes: remotes.clone(),
            index,
        });
        self.request_redraw();
    }

    /// Record the picker's choice for the front repo of the parked run,
    /// persist it as that repo's default, and advance: next multi-remote repo
    /// (another picker) or launch.
    pub(crate) fn confirm_sync_base(&mut self, remote: String) {
        // Peek before taking: the picker is only open with a non-empty queue,
        // so an empty one means a lost invariant — leave the parked run intact
        // rather than silently cancelling it (and dropping the whole sync).
        let has_pending = self
            .worktree_sync
            .awaiting
            .as_ref()
            .is_some_and(|run| !run.queue.is_empty());
        if !has_pending {
            error!("confirm_sync_base with no queued repo; leaving the run parked");
            return;
        }
        let mut run = self
            .worktree_sync
            .awaiting
            .take()
            .expect("awaiting checked non-empty above");
        let (repo, _) = run.queue.remove(0);
        if let Err(e) = self.db.set_sync_base_remote(&repo, &remote) {
            // Non-fatal: the run still uses the choice, only the default is lost.
            error!("Failed to save sync base for {}: {e}", repo.display());
        }
        run.chosen.insert(repo, remote);

        if run.queue.is_empty() {
            self.launch_sync_run(run);
        } else {
            self.worktree_sync.awaiting = Some(run);
            self.open_sync_base_picker();
        }
    }

    /// Cancel a parked sync run from the base picker (Esc): nothing has
    /// synced yet, so the whole run is dropped.
    pub(crate) fn cancel_sync_base(&mut self) {
        self.worktree_sync.awaiting = None;
        self.set_status(StatusLevel::Info, "Sync cancelled");
    }

    /// Launch the sync threads for a fully-decided run.
    ///
    /// Worktrees sharing the same parent repo are synced sequentially (to avoid
    /// concurrent `index.lock` contention), while different repos sync in parallel.
    fn launch_sync_run(&mut self, run: sync_state::PendingSyncRun) {
        let count = run.worktrees.len();
        let (tx, rx) = mpsc::channel();

        // Group worktrees by repo so those sharing a repo sync sequentially.
        let mut by_repo = std::collections::HashMap::<PathBuf, Vec<(SessionId, PathBuf)>>::new();
        for (session_id, worktree_path, repo_path) in run.worktrees {
            by_repo
                .entry(repo_path)
                .or_default()
                .push((session_id, worktree_path));
        }

        for (repo, worktrees) in by_repo {
            let tx = tx.clone();
            let host = run.host.clone();
            // The picked (or single non-origin) base remote; `None` derives
            // the rebase target per-worktree (upstream → origin/HEAD →
            // origin/main → origin/master). The resolved ref rides back on
            // `SyncResult::Conflict` so the prompt names it per-worktree.
            let remote = run.chosen.get(&repo).cloned();
            std::thread::spawn(move || {
                for (session_id, worktree_path) in worktrees {
                    let result =
                        git::sync_worktree_on(host.as_ref(), &worktree_path, remote.as_deref());
                    let _ = tx.send((session_id, result));
                }
            });
        }

        self.worktree_sync.in_progress = true;
        self.worktree_sync.rx = Some(rx);
        self.worktree_sync.pending = count;
        self.worktree_sync.completed.clear();
        self.set_status(StatusLevel::Info, format!("Syncing {count} worktree(s)..."));
    }

    /// Handle external state changes detected from other instances.
    fn handle_external_state_change(&mut self, delta: StateDelta) {
        self.session_counter = self.session_counter.max(delta.counter_increment);
        self.apply_removed_sessions(delta.removed_sessions);
        self.apply_updated_sessions(delta.updated_sessions);
        self.apply_added_sessions(delta.added_sessions);
    }

    /// Drop sessions deleted by other instances.
    fn apply_removed_sessions(&mut self, removed: Vec<SessionId>) {
        for session_id in removed {
            if let Some(pos) = self.sessions.iter().position(|s| s.info.id == session_id) {
                // Detach rather than silently drop: detach unregisters the
                // pane, which EOFs the blocked reader thread. A plain drop
                // would leak that spawn_blocking thread for the process
                // lifetime (the deleting instance owns the actual teardown).
                self.sessions.remove(pos).detach();
                // Keep `active_index` anchored to the *same* session. When a
                // session before the active one is removed every later session
                // shifts down by one, so the active index must follow; removing
                // the active session itself falls through to the clamp below.
                if pos < self.active_index {
                    self.active_index -= 1;
                }
                // Clamp into bounds (handles removing the active/last session
                // and an emptied list) so later raw-index access can't panic.
                self.sync_active_session_to_project();
            }
        }
    }

    /// Apply metadata changes made to existing sessions by other instances.
    fn apply_updated_sessions(&mut self, updated: Vec<sync::SharedSession>) {
        for shared_session in updated {
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|s| s.info.id == shared_session.id)
            {
                Self::apply_shared_session_metadata(session, &shared_session);
            }
        }
    }

    /// Adopt or spawn sessions added by other instances.
    ///
    /// Headless spawns (CLI/MCP) persist the DB row with an empty
    /// `backend_id` because only the TUI knows the real tmux pane id
    /// (`%N`). Before spawning, call `discover()` and look up the
    /// existing window by sanitized name — otherwise we'd create a
    /// duplicate `tb-<name>` window for the one the CLI already
    /// opened, and exact-match `send-keys` would then fail on
    /// "ambiguous window".
    ///
    /// `discover()` is cached per backend_type so a burst of added
    /// sessions only hits tmux once per backend.
    fn apply_added_sessions(&mut self, added: Vec<sync::SharedSession>) {
        let mut discovered_by_backend: HashMap<
            String,
            Vec<crate::agent::backend::DiscoveredSession>,
        > = HashMap::new();
        for shared_session in added {
            if self.sessions.iter().any(|s| s.info.id == shared_session.id) {
                continue;
            }

            // Skip sessions whose backend this instance can't manage (e.g. a
            // remote host not in our hosts.toml) — adopting them locally would
            // corrupt backend_type and risk a pane-id collision.
            let Some(backend) = self.resolve_persisted_backend(&shared_session.backend_type) else {
                continue;
            };

            let matching_backend_id = {
                let discovered = discovered_by_backend
                    .entry(shared_session.backend_type.clone())
                    .or_insert_with(|| {
                        // A remote backend needs its control-mode connection up
                        // before adopt(); ready it lazily on first use here.
                        match backend.ensure_ready() {
                            Ok(()) => backend.discover().unwrap_or_default(),
                            Err(e) => {
                                tracing::warn!(
                                    backend = backend.name(),
                                    "Backend not ready for adopted session: {e}"
                                );
                                Vec::new()
                            }
                        }
                    });
                Self::find_matching_discovered(&shared_session, discovered)
                    .map(|disc| disc.backend_id.clone())
            };

            let (rows, cols) = self.content_area_size();
            if let Some(backend_id) = matching_backend_id {
                // Either adoption succeeds or the discovered window already
                // exists and adoption fails transiently — in both cases we
                // must NOT fall through to spawn, because that would create
                // a second window with the same name.
                self.adopt_shared_session(&shared_session, &backend_id, &backend, rows, cols);
                continue;
            }

            // No matching discovered window. If the session has an
            // `agent_session_id`, spawn a fresh window with
            // `--session-id` so claude creates the conversation (e.g.
            // CLI-created sessions whose claude process never persisted
            // a conversation before we first adopt them).
            if shared_session.agent_session_id.is_some() {
                self.spawn_restored_session(&shared_session, &backend, rows, cols);
            }
        }
    }

    /// Adopt an already-running discovered window into our session list.
    fn adopt_shared_session(
        &mut self,
        shared_session: &sync::SharedSession,
        backend_id: &str,
        backend: &Arc<dyn crate::agent::backend::SessionBackend>,
        rows: u16,
        cols: u16,
    ) {
        let provider = {
            let cfg = SessionConfig {
                agent: shared_session.agent.clone(),
                ..SessionConfig::default()
            };
            self.provider_for(&cfg)
        };
        match Session::adopt(
            shared_session.name.clone(),
            rows,
            cols,
            backend_id,
            backend,
            &provider,
            HashMap::new(),
            None,
        ) {
            Ok(mut adopted_session) => {
                // Preserve the original session ID from shared state
                // (Session::adopt creates a new one, but we need the
                // consistent ID).
                adopted_session.info.id = shared_session.id;
                Self::apply_shared_session_metadata(&mut adopted_session, shared_session);
                self.sessions.push(adopted_session);
                // Persist the real pane_id (`%N`) back to the DB so future
                // lookups short-circuit on the backend_id match instead of
                // always falling back to name matching.
                self.save_state();
                tracing::debug!(
                    "Adopted session {} from another instance",
                    shared_session.name
                );
            }
            Err(e) => {
                tracing::debug!(
                    "Failed to adopt session {} by discovered id {}: {}",
                    shared_session.name,
                    backend_id,
                    e
                );
            }
        }
    }

    /// Build the [`SessionConfig`] for relaunching an *existing* session — either
    /// a startup-restore respawn ([`Self::spawn_restored_session`]) or a `Ctrl+U`
    /// undelete ([`Self::restore_deleted_session`]). Both reuse the session's
    /// stable `SessionId` and must inject the `FRIRING_*` identity/dir env so the
    /// agent's status hooks can attribute their `session signal` — without it the
    /// row's `hook_state` never updates and the session renders Idle forever
    /// (the bug these paths previously hit by calling `Session::spawn` directly).
    /// The caller sets `resume_session_id` afterward. Mirrors the headless
    /// `session_ops::restart_session_headless` shape.
    fn restored_session_config(
        id: crate::session::SessionId,
        agent_session_id: Option<String>,
        agent: String,
        name: String,
        cwd: Option<PathBuf>,
        backend_type: &str,
    ) -> SessionConfig {
        let mut config = SessionConfig {
            agent_session_id: agent_session_id.clone(),
            session_id: Some(id),
            cwd,
            agent,
            // Preserve a persisted off-local (`ssh:<host>` / `wsl:<distro>`)
            // backend — set *before* env injection, which skips the local-path
            // dir vars for remote sessions. Local stays `None`.
            backend: crate::session::is_remote_backend(backend_type)
                .then(|| backend_type.to_string()),
            // Only reaches the args when the relaunch starts a fresh
            // conversation (new_session_args); a resume never renames.
            session_name: Some(name),
            ..SessionConfig::default()
        };
        // `FRIRING_SESSION` (derived from `session_id`) is the identity that
        // matters; an empty `FRIRING_SESSION_ID` for an id-less agent is harmless
        // since the CLI resolves identity from `FRIRING_SESSION` first.
        crate::session_ops::inject_friring_env(
            &mut config,
            agent_session_id.as_deref().unwrap_or_default(),
            None,
        );
        config
    }

    /// Spawn a fresh window for a restored session that has an
    /// `agent_session_id` but no matching discovered window.
    fn spawn_restored_session(
        &mut self,
        shared_session: &sync::SharedSession,
        backend: &Arc<dyn crate::agent::backend::SessionBackend>,
        rows: u16,
        cols: u16,
    ) {
        let Some(agent_sid) = shared_session.agent_session_id.as_ref() else {
            return;
        };
        let worktree_infos = Self::recreate_worktrees(&shared_session.worktrees);
        let cwd = worktree_infos
            .first()
            .map(|wt| wt.worktree_path.clone())
            .or(shared_session.cwd.clone());

        // Build the relaunch config reusing the existing SessionId and injecting
        // identity/dir env, so the agent's status hooks can attribute their
        // `session signal` (otherwise the row stays Idle). The injector runs
        // before `resume_trigger_for`, which only reads `CLAUDE_CONFIG_DIR`.
        let mut config = Self::restored_session_config(
            shared_session.id,
            Some(agent_sid.clone()),
            shared_session.agent.clone(),
            shared_session.name.clone(),
            cwd,
            &shared_session.backend_type,
        );
        let def = self.agent_def_for(&config.agent);
        config.resume_session_id =
            crate::session_ops::resume_trigger_for(&def, agent_sid, &config.env);
        let provider = self.launch_provider_for(&config);

        if let Ok(mut spawned) = Session::spawn(
            shared_session.name.clone(),
            rows,
            cols,
            &config,
            backend,
            &provider,
        ) {
            spawned.info.id = shared_session.id;
            spawned.info.worktrees = worktree_infos;
            spawned.info.additional_dirs = shared_session.additional_dirs.clone();
            spawned.info.workspace_dir = shared_session.workspace_dir.clone();
            spawned.info.parent_session_id = shared_session.parent_session_id;
            spawned.info.display_order = shared_session.display_order;
            self.sessions.push(spawned);
            self.save_state();
            tracing::debug!(
                "Spawned restored session {} with --resume",
                shared_session.name
            );
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Whether the quit was a [`crate::session::Action::ReloadApp`] — `main`
    /// re-execs the on-disk binary after [`Self::shutdown`] when set.
    pub fn reload_requested(&self) -> bool {
        self.reload_requested
    }

    /// Persist state, detach every session, then tear down the backends
    /// themselves. The order is forced: `Session::detach` consumes the session
    /// by value, so `save_state` (which reads `session.info`) must run while
    /// `self.sessions` is intact. A hung save is bounded by the SQLite
    /// busy_timeout, after which upsert errors are logged and detach still runs.
    ///
    /// Backend teardown (`shutdown_backends`) comes last and is what actually
    /// ends the control-mode connections; without it they would be closed
    /// one-by-one by `Drop` after `main` returns, which is invisible in the log
    /// and used to dominate quit.
    pub fn shutdown(mut self) {
        self.finalize_pending_delete();
        self.save_state();
        self.persist_shutdown_frames();
        // Do NOT remove worktrees — they persist for resume.
        // Detach from backend sessions without killing them — they persist in tmux.
        // `take` rather than consuming `self.sessions`: that would partially move
        // `self` and make the `shutdown_backends` call below unreachable.
        for session in std::mem::take(&mut self.sessions) {
            session.detach();
        }
        // Sessions first: detach unregisters each pane, so the per-session reader
        // threads are already unwinding while the connections are torn down.
        self.shutdown_backends();
    }

    /// Tear down every backend's control-mode connection, concurrently.
    ///
    /// Each connection's teardown blocks on its child exiting, so doing this
    /// serially cost the sum over backends — and the backend count grows with
    /// every configured SSH host and auto-discovered WSL distro. One thread per
    /// backend makes quit cost the slowest connection instead.
    ///
    /// Bounded by [`BACKEND_SHUTDOWN_TIMEOUT`]: a wedged transport (an ssh child
    /// ignoring its kill, say) must not hang the process. Abandoning a straggler
    /// is safe — these are the last threads doing anything, the agent panes live
    /// on in tmux regardless, and the process is about to exit.
    fn shutdown_backends(&self) {
        let backends: Vec<_> = self.backends.all_backends().cloned().collect();
        let total = backends.len();
        if total == 0 {
            return;
        }

        // Completion is signalled over a channel rather than by `join`ing the
        // handles: a `join` on a wedged thread blocks past the deadline, which
        // is the exact hang this timeout exists to prevent. Each worker sends on
        // its way out; we wait for `total` sends or the deadline, whichever
        // comes first, and simply never join — a straggler is left detached and
        // dies with the process.
        let (tx, rx) = mpsc::channel();
        for b in backends {
            let tx = tx.clone();
            std::thread::spawn(move || {
                b.shutdown();
                let _ = tx.send(());
            });
        }
        // Drop the extra sender so `recv_timeout` can't wait on a live handle
        // this thread still owns.
        drop(tx);

        let deadline = std::time::Instant::now() + BACKEND_SHUTDOWN_TIMEOUT;
        for _ in 0..total {
            // A deadline already past yields a zero timeout, which `recv_timeout`
            // reports as an immediate error — so this needs no separate check.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if rx.recv_timeout(remaining).is_err() {
                warn!("backend shutdown timed out; abandoning remaining teardown");
                return;
            }
        }
    }

    /// Save every live session's ghost frame at shutdown, so a reboot (or a
    /// lazy next launch) has a fresh frame to show. Local sessions capture
    /// through the backend (an independent subprocess each, ~10 ms, and its
    /// output keeps logical lines); remote sessions serialize the in-memory
    /// visible screen instead — a per-session ssh round-trip could hang the
    /// exit on a dying host, and the two produce the same screen either way.
    fn persist_shutdown_frames(&self) {
        for session in &self.sessions {
            if session.is_placeholder() {
                continue;
            }
            let frame = if crate::session::is_remote_backend(session.backend_name()) {
                session.serialize_visible_frame()
            } else {
                session.capture_unload_frame()
            };
            if let Some((rows, cols, bytes)) = frame {
                if let Err(e) = self
                    .db
                    .save_session_frame(session.info.id, rows, cols, &bytes)
                {
                    error!(
                        "Failed to save shutdown frame for '{}': {e}",
                        session.info.name
                    );
                }
            }
        }
    }

    /// Crash-safety frame debounce: persist the **visible screen** of every
    /// session that produced output since its last save. Pure in-memory
    /// serialization (~50 µs/session) + one small UPDATE each, so a hard
    /// crash / reboot leaves ghosts at most one interval stale. Scrollback is
    /// deliberately not captured here — that costs a subprocess (or an ssh
    /// round-trip) per session and belongs to unload/shutdown.
    fn persist_dirty_frames(&mut self) {
        let now = crate::agent::backend::now_millis();
        for session in &self.sessions {
            if session.is_placeholder() {
                continue;
            }
            let id = session.info.id;
            let saved_at = self.frame_saved_at.get(&id).copied().unwrap_or(0);
            if session.last_output_at() <= saved_at {
                continue;
            }
            if let Some((rows, cols, bytes)) = session.serialize_visible_frame() {
                match self.db.save_session_frame(id, rows, cols, &bytes) {
                    Ok(()) => {
                        self.frame_saved_at.insert(id, now);
                    }
                    Err(e) => error!("Failed to save frame for '{}': {e}", session.info.name),
                }
            }
        }
    }

    /// Rebuild the file viewer tree from the currently active session's
    /// worktrees and additional directories. Called when the active session
    /// changes or when the file viewer is first opened.
    pub(crate) fn rebuild_file_viewer_for_active(&mut self) {
        if let Some(session) = self.sessions.get(self.active_index) {
            self.file_viewer.rebuild_from_session(&session.info);
        } else {
            self.file_viewer.clear();
        }
    }

    /// Set status bar message with the given severity level.
    fn set_status(&mut self, level: StatusLevel, text: impl Into<String>) {
        self.status_message = Some(StatusMessage {
            text: text.into(),
            level,
            created_at: clock::now(),
        });
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.set_status(StatusLevel::Error, text.into());
    }

    fn set_info(&mut self, text: impl Into<String>) {
        self.set_status(StatusLevel::Info, text.into());
    }

    /// Persist session state to the SQLite database.
    ///
    /// Only writes sessions and the session counter. Project mutations
    /// (add/edit/delete) write to the DB at their point of change, avoiding
    /// race conditions where a blanket re-write overwrites another instance's edits.
    fn save_state(&self) {
        if let Err(e) = self.db.set_session_counter(self.session_counter) {
            error!("Failed to save session counter to DB: {e}");
        }

        for session in &self.sessions {
            // Never persist a placeholder (unreachable remote): its row already
            // exists in the DB, and its empty `backend_id`/`shell_backend_id`
            // (and, for an unknown host, a fallback local `backend_type`) would
            // clobber the real persisted values and break re-adoption.
            if session.is_placeholder() {
                continue;
            }
            let shared_session = self.session_to_shared(session);
            if let Err(e) = self.db.upsert_session(&shared_session) {
                error!("Failed to upsert session to DB: {e}");
            }
        }
    }

    /// Build a SharedSession from a local Session.
    fn session_to_shared(&self, session: &Session) -> sync::SharedSession {
        sync::SharedSession {
            id: session.info.id,
            name: session.info.name.clone(),
            agent: session.info.agent.clone(),
            backend_id: session.backend_id().to_string(),
            backend_type: session.backend_name().to_string(),
            agent_session_id: session.info.agent_session_id.clone(),
            cwd: session.info.cwd.clone(),
            additional_dirs: session.info.additional_dirs.clone(),
            workspace_dir: session.info.workspace_dir.clone(),
            worktrees: session
                .info
                .worktrees
                .iter()
                .cloned()
                .map(Into::into)
                .collect(),
            shell_backend_id: session.info.shell_backend_id.clone(),
            parent_session_id: session.info.parent_session_id,
            display_order: session.info.display_order,
            tombstone: false,
            tombstone_at: None,
        }
    }

    /// Load persisted session state from the database.
    ///
    /// Returns `Some(sessions, counter)` if there are active sessions in the DB,
    /// or `None` if no sessions exist (indicating a fresh start or first run).
    pub fn load_persisted_state_from_db(&self) -> Option<(Vec<sync::SharedSession>, usize)> {
        let sessions = self.db.list_active_sessions().ok()?;
        if sessions.is_empty() {
            return None;
        }

        // Only sessions with an agent_session_id are resumable.
        let resumable: Vec<sync::SharedSession> = sessions
            .into_iter()
            .filter(|s| s.agent_session_id.is_some())
            .collect();

        if resumable.is_empty() {
            return None;
        }

        let counter = self.db.get_session_counter().unwrap_or(0);
        Some((resumable, counter))
    }

    /// Restore sessions from the database on startup.
    ///
    /// Local sessions are restored synchronously by querying the local backend
    /// for its existing tmux windows. Sessions persisted with a remote
    /// (`ssh:<host>` / `wsl:<distro>`) `backend_type` are restored in the
    /// background instead — one discovery thread per host, drained by
    /// `poll_remote_restore` each tick — because readying a remote backend
    /// means an ssh connect that can take tens of seconds (or minutes for a
    /// down host) and must never block the first frame.
    pub fn restore_sessions(&mut self, sessions: Vec<sync::SharedSession>, session_counter: usize) {
        self.session_counter = session_counter;
        // Opt-in startup-restore breakdown (FRIRING_PERF_LOG). Local restore is
        // sequential — each session is adopted with a blocking
        // `capture_pane_text` — so per-backend discover and per-session adopt
        // timings show where the remaining time goes. Read once here, never
        // per tick.
        let perf_log = std::env::var_os("FRIRING_PERF_LOG").is_some();

        // Only sessions with an agent_session_id are resumable.
        let resumable: Vec<sync::SharedSession> = sessions
            .into_iter()
            .filter(|s| s.agent_session_id.is_some())
            .collect();

        let (remote, local): (Vec<_>, Vec<_>) = resumable
            .into_iter()
            .partition(|s| crate::session::is_remote_backend(&s.backend_type));

        // Sessions to ghost instead of respawn: everything explicitly unloaded,
        // plus — with lazy restore on — every session whose pane is gone. Read
        // once; the per-session decision is in `restore_single_session`.
        let unloaded: HashSet<SessionId> = self
            .db
            .unloaded_session_ids()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let discovered_by_backend = self.discover_windows_by_backend(&local, perf_log);

        // Prefetch every matched pane's scrollback capture in parallel before
        // the sequential adopt loop: the captures are independent subprocesses,
        // only the control-mode connect is serialized (ADR-P9).
        let seeds = self.prefetch_capture_seeds(&local, &discovered_by_backend, perf_log);
        self.metrics.perf.restore_seed_prefetches = self
            .metrics
            .perf
            .restore_seed_prefetches
            .wrapping_add(seeds.len() as u64);

        for shared in local {
            let discovered = discovered_by_backend
                .get(&shared.backend_type)
                .cloned()
                .unwrap_or_default();
            let adopt_start = perf_log.then(std::time::Instant::now);
            let name = perf_log.then(|| shared.name.clone());
            self.restore_single_session(shared, &discovered, &seeds, &unloaded);
            if let (Some(start), Some(name)) = (adopt_start, name) {
                tracing::info!(
                    session = %name,
                    adopt_ms = start.elapsed().as_millis() as u64,
                    "restore_adopt"
                );
            }
        }

        self.start_remote_restore(remote, perf_log);

        // Claim ownership of restored sessions in the shared state
        self.save_state();

        self.apply_startup_focus();
    }

    /// Startup default focus: the **terminal** whenever any session was
    /// restored. Selection already tracks the active session, so the list is a
    /// glanceable dashboard plus an explicit "manage" surface (`Ctrl+H`), not
    /// the place keystrokes should land first — starting there is how typed
    /// input ends up triggering list hotkeys instead of reaching the agent.
    /// With no sessions the list keeps focus (its empty state advertises
    /// `Ctrl+N`). Remote sessions adopt in the background and don't count —
    /// by the time one lands the user may already be typing somewhere.
    fn apply_startup_focus(&mut self) {
        if !self.sessions.is_empty() {
            self.focus = InputFocus::Terminal;
        }
    }

    /// Kick off one discovery thread per distinct remote backend and queue its
    /// sessions for adoption in [`Self::poll_remote_restore`]. Sessions on a
    /// backend this instance can't manage (an unknown host) are left
    /// un-adopted, exactly like the synchronous path.
    fn start_remote_restore(&mut self, remote: Vec<sync::SharedSession>, perf_log: bool) {
        if remote.is_empty() {
            return;
        }
        let mut grouped: HashMap<String, Vec<sync::SharedSession>> = HashMap::new();
        for shared in remote {
            grouped
                .entry(shared.backend_type.clone())
                .or_default()
                .push(shared);
        }

        let mut restore = RemoteRestore::new(perf_log, clock::now() + REMOTE_RETRY_INTERVAL);
        for (backend_type, sessions) in grouped {
            // Always show the session, even before (or without) a live host:
            // insert a placeholder row up front so it never silently vanishes.
            for shared in &sessions {
                self.insert_remote_placeholder(shared);
            }
            // A backend we can resolve gets a discovery thread + retry tracking.
            // An unknown host (no config) keeps its placeholder but can't be
            // adopted, so it isn't queued for retries.
            if let Some(backend) = self.resolve_persisted_backend(&backend_type) {
                Self::spawn_remote_discovery(
                    backend,
                    backend_type.clone(),
                    perf_log,
                    restore.tx.clone(),
                );
                restore.inflight.insert(backend_type.clone());
                restore.pending.insert(backend_type, sessions);
            }
        }
        if !restore.pending.is_empty() {
            self.remote_restore = Some(restore);
        }
    }

    /// Ready + discover one remote backend on its own thread, reporting the
    /// result over `tx` (a dropped receiver — app shut down — is fine).
    fn spawn_remote_discovery(
        backend: Arc<dyn SessionBackend>,
        backend_type: String,
        perf_log: bool,
        tx: mpsc::Sender<RemoteDiscovery>,
    ) {
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let (reachable, discovered) = Self::ready_and_discover(&backend);
            if perf_log {
                tracing::info!(
                    backend = %backend_type,
                    reachable,
                    windows = discovered.len() as u64,
                    discover_ms = start.elapsed().as_millis() as u64,
                    "restore_discover"
                );
            }
            let _ = tx.send((backend_type, reachable, discovered));
        });
    }

    /// Rebuild the `(info, backend, provider)` triple for a persisted session
    /// row — shared by the placeholder and ghost builders. The backend and
    /// provider are dummies in both cases (neither does I/O until a real
    /// adopt/spawn replaces the session); an unknown host falls back to the
    /// local default so the row can still render.
    fn persisted_session_parts(
        &self,
        shared: &sync::SharedSession,
    ) -> (SessionInfo, Arc<dyn SessionBackend>, Arc<dyn AgentProvider>) {
        let agent = if shared.agent.is_empty() {
            DEFAULT_AGENT_NAME.to_string()
        } else {
            shared.agent.clone()
        };
        let backend = self
            .resolve_persisted_backend(&shared.backend_type)
            .unwrap_or_else(|| self.backends.default_backend().clone());
        let provider = self.provider_for(&SessionConfig {
            agent: agent.clone(),
            ..SessionConfig::default()
        });

        let mut info = SessionInfo::new(shared.name.clone());
        info.id = shared.id;
        info.agent = agent;
        info.agent_session_id = shared.agent_session_id.clone();
        info.cwd = shared.cwd.clone();
        info.additional_dirs = shared.additional_dirs.clone();
        info.workspace_dir = shared.workspace_dir.clone();
        info.worktrees = shared.worktrees.iter().cloned().map(Into::into).collect();
        info.parent_session_id = shared.parent_session_id;
        info.display_order = shared.display_order;
        info.remote_host = host_label_from_backend_type(&shared.backend_type);
        resolve_repo_display_names(&mut info);
        (info, backend, provider)
    }

    /// Build (but don't insert) a placeholder [`Session`] for a persisted remote
    /// session — a row tagged `SessionStatus::Unreachable` with no live pane. See
    /// [`crate::agent::backend::Session::placeholder`].
    fn build_placeholder_session(&self, shared: &sync::SharedSession) -> Session {
        let (info, backend, provider) = self.persisted_session_parts(shared);
        let (rows, cols) = self.content_area_size();
        Session::placeholder(info, rows, cols, &backend, &provider, HashMap::new())
    }

    /// Build (but don't insert) a **ghost** [`Session`] for a persisted row:
    /// the saved last frame (if any) re-parsed at the *current* pane size —
    /// the line-shaped frame bytes re-wrap, so a ghost restored into a
    /// narrower terminal stays legible and bottom-anchored. See
    /// [`crate::agent::backend::Session::ghost`].
    fn build_ghost_session(&self, shared: &sync::SharedSession) -> Session {
        let frame = self.db.load_session_frame(shared.id).ok().flatten();
        let (info, backend, provider) = self.persisted_session_parts(shared);
        let (rows, cols) = self.content_area_size();
        Session::ghost(
            info,
            rows,
            cols,
            &backend,
            &provider,
            HashMap::new(),
            frame.as_ref().map(|f| f.bytes.as_slice()),
        )
    }

    /// Insert a placeholder row for a persisted remote session whose host is not
    /// yet (or no longer) reachable, so it always appears in the list tagged
    /// `SessionStatus::Unreachable`. Idempotent: skips if a session with this id
    /// already exists (a real adopted session or an earlier placeholder).
    fn insert_remote_placeholder(&mut self, shared: &sync::SharedSession) {
        if self.sessions.iter().any(|s| s.info.id == shared.id) {
            return;
        }
        let session = self.build_placeholder_session(shared);
        self.sessions.push(session);
        self.request_redraw();
    }

    /// Detect **mid-session host loss**: a live (non-placeholder) remote session
    /// whose control-mode connection has died. Because tmux runs with
    /// `remain-on-exit=on`, a normal agent exit keeps its pane alive (no reader
    /// EOF), so for a *remote* session `has_exited()` becoming true reliably means
    /// the host/SSH connection dropped — not a clean agent exit. Such a session is
    /// converted **in place** to an `Unreachable` placeholder and queued for the
    /// reconnect retry loop, so it never looks like a normal idle session and
    /// auto-adopts when the host returns.
    fn detect_lost_remote_sessions(&mut self) {
        let lost: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                !s.is_placeholder()
                    && s.has_exited()
                    && crate::session::is_remote_backend(s.backend_name())
            })
            .map(|(i, _)| i)
            .collect();
        if lost.is_empty() {
            return;
        }
        for i in lost {
            // Capture the persisted shape before swapping in the placeholder, so
            // the reconnect keeps the real `backend_id` / worktrees / identity.
            let shared = self.session_to_shared(&self.sessions[i]);
            // Replace in place (same index) so the active selection is undisturbed.
            self.sessions[i] = self.build_placeholder_session(&shared);
            self.enqueue_remote_reconnect(shared);
        }
        self.set_error("Remote host connection lost — reconnecting…");
        self.request_redraw();
    }

    /// Queue a remote session for the reconnect retry loop, creating the
    /// `remote_restore` state if it isn't running (e.g. host lost after the
    /// startup restore already finished). Sets the retry clock to now so the next
    /// `poll_remote_restore` probes the host immediately. No-op for an unknown
    /// (unconfigured) host — its placeholder simply stays until reconfigured.
    fn enqueue_remote_reconnect(&mut self, shared: sync::SharedSession) {
        let backend_type = shared.backend_type.clone();
        if self.resolve_persisted_backend(&backend_type).is_none() {
            return;
        }
        let now = clock::now();
        if self.remote_restore.is_none() {
            // Reconnect as soon as the next tick, not after the retry interval.
            self.remote_restore = Some(RemoteRestore::new(false, now));
        }
        if let Some(s) = self.remote_restore.as_mut() {
            let queue = s.pending.entry(backend_type).or_default();
            if !queue.iter().any(|q| q.id == shared.id) {
                queue.push(shared);
            }
            s.next_retry_at = now;
        }
    }

    /// Drain finished remote-backend discoveries, adopt reachable ones (replacing
    /// their placeholder rows in place), keep unreachable ones as placeholders,
    /// and periodically retry the still-down backends. Adoption runs on the main
    /// thread but talks to the control-mode connection the background thread
    /// already brought up, and remote ssh is fail-fast
    /// ([`crate::shell::SSH_HARDENING_OPTS`]), so it never blocks a frame. The
    /// restore state is dropped only once every remote session has been adopted.
    fn poll_remote_restore(&mut self) {
        if self.remote_restore.is_none() {
            return;
        }

        let mut ready: Vec<RemoteDiscovery> = Vec::new();
        if let Some(state) = &self.remote_restore {
            loop {
                match state.rx.try_recv() {
                    Ok(msg) => ready.push(msg),
                    Err(mpsc::TryRecvError::Empty) => break,
                    // The channel can't disconnect while `remote_restore` holds a
                    // `tx`; treat it as "nothing more this tick".
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }

        // Each reported backend's discovery thread has finished.
        if let Some(state) = &mut self.remote_restore {
            for (backend_type, _, _) in &ready {
                state.inflight.remove(backend_type);
            }
        }

        if !ready.is_empty() {
            self.adopt_remote_discoveries(ready);
        }

        self.maybe_retry_remote_restore();

        // Done once nothing is left to adopt (all placeholders replaced).
        if self
            .remote_restore
            .as_ref()
            .is_some_and(|s| s.pending.is_empty())
        {
            self.remote_restore = None;
        }
    }

    /// Re-spawn discovery threads for still-pending (unreachable) backends once
    /// [`REMOTE_RETRY_INTERVAL`] has elapsed, so a recovered host auto-adopts its
    /// placeholder sessions without a restart.
    fn maybe_retry_remote_restore(&mut self) {
        let now = clock::now();
        if !self
            .remote_restore
            .as_ref()
            .is_some_and(|s| now >= s.next_retry_at)
        {
            return;
        }
        let to_retry: Vec<String> = match &self.remote_restore {
            Some(s) => s
                .pending
                .keys()
                .filter(|b| !s.inflight.contains(*b))
                .cloned()
                .collect(),
            None => return,
        };
        for backend_type in to_retry {
            if let Some(backend) = self.resolve_persisted_backend(&backend_type) {
                let (tx, perf_log) = match &self.remote_restore {
                    Some(s) => (s.tx.clone(), s.perf_log),
                    None => return,
                };
                Self::spawn_remote_discovery(backend, backend_type.clone(), perf_log, tx);
                if let Some(s) = self.remote_restore.as_mut() {
                    s.inflight.insert(backend_type);
                }
            }
        }
        if let Some(s) = self.remote_restore.as_mut() {
            s.next_retry_at = now + REMOTE_RETRY_INTERVAL;
        }
    }

    /// Trigger an immediate retry sweep for a backend (used by the manual
    /// restart of an unreachable placeholder) — resets the retry clock so the
    /// next `poll_remote_restore` re-spawns discovery right away.
    fn retry_remote_backend_now(&mut self, backend_type: &str) {
        if let Some(s) = self.remote_restore.as_mut() {
            if s.pending.contains_key(backend_type) {
                s.next_retry_at = clock::now();
            }
        }
    }

    /// Adopt every reachable backend's pending sessions, replacing their
    /// placeholder rows in place; keep unreachable backends' placeholders and
    /// toast the host once.
    fn adopt_remote_discoveries(&mut self, ready: Vec<RemoteDiscovery>) {
        // Adoption reorders `self.sessions`; a late-arriving host must not steal
        // the user's current selection, so snapshot + restore it by id.
        let prior_active = self.sessions.get(self.active_index).map(|s| s.info.id);
        let prior_focus = self.focus;
        let mut restored = 0usize;
        let mut unreachable_hosts: Vec<String> = Vec::new();
        // Same ghost-vs-respawn decision as the startup restore (a returned
        // host whose pane is gone means the host rebooted).
        let unloaded: HashSet<SessionId> = self
            .db
            .unloaded_session_ids()
            .unwrap_or_default()
            .into_iter()
            .collect();

        for (backend_type, reachable, discovered) in ready {
            if !reachable {
                let first_time = self
                    .remote_restore
                    .as_mut()
                    .is_some_and(|s| s.notified_unreachable.insert(backend_type.clone()));
                if first_time {
                    if let Some(h) = host_label_from_backend_type(&backend_type) {
                        unreachable_hosts.push(h);
                    }
                }
                continue;
            }
            // Reachable again: allow a future drop to re-toast.
            if let Some(s) = self.remote_restore.as_mut() {
                s.notified_unreachable.remove(&backend_type);
            }
            let Some(sessions) = self
                .remote_restore
                .as_mut()
                .and_then(|s| s.pending.remove(&backend_type))
            else {
                continue;
            };
            let mut still_pending: Vec<sync::SharedSession> = Vec::new();
            for shared in sessions {
                let id = shared.id;
                let has_real = self
                    .sessions
                    .iter()
                    .any(|s| s.info.id == id && !s.is_placeholder());
                // Already adopted for real (e.g. via the DB sync) — just drop any
                // leftover placeholder.
                if has_real {
                    self.remove_remote_placeholder(id);
                    continue;
                }
                // No placeholder left and no real session → the user deleted it
                // while the host was down; don't resurrect it (drop from pending).
                let has_placeholder = self
                    .sessions
                    .iter()
                    .any(|s| s.info.id == id && s.is_placeholder());
                if !has_placeholder {
                    continue;
                }
                let retry_copy = shared.clone();
                // Remote adoption keeps the inline capture (`seed: None` path)
                // — the SSH control-mode round-trips dominate there anyway.
                self.restore_single_session(shared, &discovered, &HashMap::new(), &unloaded);
                // A ghost counts as restored: the host is back but the pane is
                // gone, and lazy restore (or the unloaded flag) chose a frozen
                // frame over respawning on the host.
                if self
                    .sessions
                    .iter()
                    .any(|s| s.info.id == id && (!s.is_placeholder() || s.is_ghost()))
                {
                    self.remove_remote_placeholder(id);
                    restored += 1;
                } else {
                    // Adopt/respawn failed (host dropped again mid-adopt); keep
                    // the placeholder and re-queue for the next retry.
                    still_pending.push(retry_copy);
                }
            }
            if !still_pending.is_empty() {
                if let Some(s) = self.remote_restore.as_mut() {
                    s.pending.insert(backend_type, still_pending);
                }
            }
        }

        // Restore prior selection/focus (indices shifted during adoption).
        if let Some(id) = prior_active {
            if let Some(idx) = self.sessions.iter().position(|s| s.info.id == id) {
                self.active_index = idx;
                self.focus = prior_focus;
            }
        }

        for host in &unreachable_hosts {
            self.set_error(format!("Remote host '{host}' unavailable"));
        }
        if restored > 0 {
            self.save_state();
            self.set_status(
                StatusLevel::Info,
                format!("Restored {restored} remote session(s)"),
            );
        }
        if restored > 0 || !unreachable_hosts.is_empty() {
            self.request_redraw();
        }
    }

    /// Remove the *unreachable* placeholder row for `id` (leaving a real
    /// adopted session — or the ghost the retry produced — with the same id
    /// untouched). See [`Self::insert_remote_placeholder`].
    fn remove_remote_placeholder(&mut self, id: SessionId) {
        self.sessions
            .retain(|s| !(s.info.id == id && s.is_placeholder() && !s.is_ghost()));
    }

    /// Discover existing backend windows once per distinct `backend_type`.
    ///
    /// Each backend is readied + discovered at most once. Only local backends
    /// reach this at startup (remote ones go through
    /// [`Self::start_remote_restore`]); unknown backend types map to an empty
    /// list so their sessions are left un-adopted rather than misadopted.
    fn discover_windows_by_backend(
        &self,
        resumable: &[sync::SharedSession],
        perf_log: bool,
    ) -> HashMap<String, Vec<crate::agent::backend::DiscoveredSession>> {
        let mut discovered_by_backend: HashMap<
            String,
            Vec<crate::agent::backend::DiscoveredSession>,
        > = HashMap::new();
        for shared in resumable {
            if discovered_by_backend.contains_key(&shared.backend_type) {
                continue;
            }
            let discover_start = perf_log.then(std::time::Instant::now);
            let disc = self.discover_windows_for_backend(&shared.backend_type);
            if let Some(start) = discover_start {
                tracing::info!(
                    backend = %shared.backend_type,
                    windows = disc.len() as u64,
                    discover_ms = start.elapsed().as_millis() as u64,
                    "restore_discover"
                );
            }
            discovered_by_backend.insert(shared.backend_type.clone(), disc);
        }
        discovered_by_backend
    }

    /// Ready + discover a single backend's windows, degrading to an empty list
    /// when the backend is unknown or not reachable (logged, never fatal).
    fn discover_windows_for_backend(
        &self,
        backend_type: &str,
    ) -> Vec<crate::agent::backend::DiscoveredSession> {
        // Skip discovery for backends this instance can't manage (unknown
        // remote hosts); their sessions are left un-adopted.
        let Some(backend) = self.resolve_persisted_backend(backend_type) else {
            return Vec::new();
        };
        // Local restore only cares about the windows; reachability is a
        // remote-restore concern.
        Self::ready_and_discover(&backend).1
    }

    /// Ready a backend and list its windows. Returns `(reachable, windows)`:
    /// `reachable` is `false` when `ensure_ready` failed (host down / SSH auth /
    /// network), which the remote restore uses to keep placeholder rows and
    /// schedule a retry rather than treating an empty list as "no windows".
    /// Errors are logged, never fatal. Associated (no `&self`) so the remote
    /// restore threads can run it off the UI thread.
    fn ready_and_discover(
        backend: &Arc<dyn SessionBackend>,
    ) -> (bool, Vec<crate::agent::backend::DiscoveredSession>) {
        if let Err(e) = backend.ensure_ready() {
            warn!(
                backend = backend.name(),
                "Backend not ready during restore; keeping its sessions as unreachable: {e}"
            );
            return (false, Vec::new());
        }

        let windows = backend.discover().unwrap_or_else(|e| {
            warn!(
                backend = backend.name(),
                "Failed to discover sessions from backend: {e}"
            );
            Vec::new()
        });
        (true, windows)
    }

    /// Restore a single session synchronously (used during startup). The
    /// backend is selected from the session's persisted `backend_type`.
    /// Capture every matched local pane's scrollback in parallel, keyed by
    /// pane id, for [`Self::restore_single_session`] to pass into
    /// [`Session::adopt`]. `tmux capture-pane` is an independent subprocess
    /// per pane, so overlapping them shrinks the sequential restore's
    /// `capture_ms` slice to ~0 (ADR-P9); the control-mode connect stays
    /// sequential. Concurrency is bounded so a big restore doesn't fork one
    /// subprocess per session at once. A failed capture is simply absent from
    /// the map — the adopt falls back to its inline capture.
    fn prefetch_capture_seeds(
        &self,
        local: &[sync::SharedSession],
        discovered_by_backend: &HashMap<String, Vec<crate::agent::backend::DiscoveredSession>>,
        perf_log: bool,
    ) -> HashMap<String, Vec<u8>> {
        const MAX_CONCURRENT_CAPTURES: usize = 8;

        let mut jobs: Vec<(String, Arc<dyn SessionBackend>)> = Vec::new();
        for shared in local {
            let Some(discovered) = discovered_by_backend.get(&shared.backend_type) else {
                continue;
            };
            let Some(disc) = Self::find_matching_discovered(shared, discovered) else {
                continue;
            };
            let Some(backend) = self.resolve_persisted_backend(&shared.backend_type) else {
                continue;
            };
            jobs.push((disc.backend_id.clone(), backend));
        }
        if jobs.is_empty() {
            return HashMap::new();
        }

        let start = std::time::Instant::now();
        let count = jobs.len();
        let queue = std::sync::Mutex::new(jobs);
        let results = std::sync::Mutex::new(HashMap::new());
        let workers = MAX_CONCURRENT_CAPTURES.min(count);
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let job = queue.lock().ok().and_then(|mut q| q.pop());
                    let Some((pane, backend)) = job else { break };
                    match backend.capture_history(&pane) {
                        Ok(seed) => {
                            if let Ok(mut r) = results.lock() {
                                r.insert(pane, seed);
                            }
                        }
                        Err(e) => warn!("Failed to prefetch history for pane {pane}: {e}"),
                    }
                });
            }
        });
        if perf_log {
            tracing::info!(
                sessions = count as u64,
                prefetch_ms = start.elapsed().as_millis() as u64,
                "restore_capture_prefetch"
            );
        }
        results.into_inner().unwrap_or_default()
    }

    fn restore_single_session(
        &mut self,
        shared: sync::SharedSession,
        discovered: &[crate::agent::backend::DiscoveredSession],
        seeds: &HashMap<String, Vec<u8>>,
        unloaded: &HashSet<SessionId>,
    ) {
        let name = shared.name.clone();

        let agent = if shared.agent.is_empty() {
            DEFAULT_AGENT_NAME.to_string()
        } else {
            shared.agent.clone()
        };

        let worktrees: Vec<WorktreeInfo> =
            shared.worktrees.iter().cloned().map(Into::into).collect();

        let Some(agent_session_id) = shared.agent_session_id.clone() else {
            return;
        };

        let matching_discovered = Self::find_matching_discovered(&shared, discovered);

        // Select the correct backend based on the persisted backend_type.
        // Skip sessions on a backend this instance can't manage (unknown remote
        // host) rather than misadopting them on local.
        let Some(backend) = self.resolve_persisted_backend(&shared.backend_type) else {
            return;
        };

        // Try to adopt the existing backend session.
        let provider = self.provider_for(&SessionConfig {
            agent: agent.clone(),
            ..SessionConfig::default()
        });
        let adopted = matching_discovered.and_then(|disc| {
            let (rows, cols) = self.content_area_size();
            match Session::adopt(
                name.clone(),
                rows,
                cols,
                &disc.backend_id,
                &backend,
                &provider,
                HashMap::new(),
                seeds.get(&disc.backend_id).cloned(),
            ) {
                Ok(session) => Some(session),
                Err(e) => {
                    error!("Failed to adopt session '{name}': {e}");
                    None
                }
            }
        });

        if let Some(session) = adopted {
            // A live pane trumps the unloaded flag (e.g. a headless spawn
            // re-created the window after an unload): adopting it is free —
            // no process starts — so clear the flag rather than ghost a
            // session that is actually running.
            if unloaded.contains(&shared.id) {
                let _ = self.db.set_session_unloaded(shared.id, false);
            }
            self.finish_adopted_session(session, &shared, agent, worktrees, discovered);
        } else if unloaded.contains(&shared.id)
            || crate::session::settings::global().lazy_session_restore
        {
            // No live pane, and either the user unloaded this session or lazy
            // restore is on: show the greyed last-frame ghost instead of
            // spawning an agent process. Enter / restart loads it.
            let session = self.build_ghost_session(&shared);
            self.sessions.push(session);
            self.request_redraw();
        } else {
            self.respawn_stale_session(name, shared, agent, agent_session_id, worktrees);
        }
    }

    /// Wire a freshly-adopted backend session into the app: copy persisted
    /// metadata, re-adopt its shell pane, and make it the active session.
    fn finish_adopted_session(
        &mut self,
        mut session: Session,
        shared: &sync::SharedSession,
        agent: String,
        worktrees: Vec<WorktreeInfo>,
        discovered: &[crate::agent::backend::DiscoveredSession],
    ) {
        session.info.id = shared.id;
        session.info.agent_session_id = shared.agent_session_id.clone();
        session.info.cwd = shared.cwd.clone();
        session.info.additional_dirs = shared.additional_dirs.clone();
        session.info.workspace_dir = shared.workspace_dir.clone();
        session.info.agent = agent;
        session.info.worktrees = worktrees;
        session.info.parent_session_id = shared.parent_session_id;
        session.info.display_order = shared.display_order;
        resolve_repo_display_names(&mut session.info);

        // Re-adopt shell pane if one was persisted
        if let Some(shell_bid) = &shared.shell_backend_id {
            let (rows, cols) = self.content_area_size();
            Self::readopt_shell_pane(&mut session, shell_bid, discovered, rows, cols);
        }

        self.sessions.push(session);
        self.active_index = self.sessions.len() - 1;
        self.focus = InputFocus::Terminal;
    }

    /// Re-adopt a persisted shell pane onto `session` if its backend window is
    /// still alive. Failures are non-fatal (logged only).
    fn readopt_shell_pane(
        session: &mut Session,
        shell_bid: &str,
        discovered: &[crate::agent::backend::DiscoveredSession],
        rows: u16,
        cols: u16,
    ) {
        if !discovered
            .iter()
            .any(|d| d.backend_id == *shell_bid && d.is_alive)
        {
            return;
        }
        if let Err(e) = session.adopt_shell_pane(shell_bid, rows, cols) {
            tracing::warn!("Failed to re-adopt shell pane: {e}");
        }
    }

    /// No matching backend session or adopt failed — respawn resuming when the
    /// agent supports it (a claude transcript exists for this
    /// `agent_session_id`, or a `resume_latest` agent resumes its last session
    /// in the cwd), otherwise start fresh (e.g. claude with `--session-id` so it
    /// creates the conversation, or any agent whose process never persisted one).
    fn respawn_stale_session(
        &mut self,
        name: String,
        shared: sync::SharedSession,
        agent: String,
        agent_session_id: String,
        worktrees: Vec<WorktreeInfo>,
    ) {
        // Reuse the original SessionId so the session's identity is stable across
        // restarts: `do_spawn_session` upserts in place (no soft-delete + new-row
        // churn), and `FRIRING_SESSION` is re-injected with the same id. Any
        // cached id / queued message addressed to this session stays valid.
        // Preserving a remote `backend` keeps the respawn on its own host —
        // without it `do_spawn_session` would silently relaunch the session on
        // the local tmux, pointed at worktree paths that only exist remotely.
        let backend = crate::session::is_remote_backend(&shared.backend_type)
            .then(|| shared.backend_type.clone());
        let mut config = SessionConfig {
            session_id: Some(shared.id),
            resume_session_id: None,
            agent_session_id: Some(agent_session_id.clone()),
            cwd: shared.cwd,
            agent,
            fork_session_id: None,
            backend,
            ..SessionConfig::default()
        };
        let def = self.agent_def_for(&config.agent);
        config.resume_session_id =
            crate::session_ops::resume_trigger_for(&def, &agent_session_id, &config.env);
        self.new_session.additional_dirs = shared.additional_dirs;
        self.new_session.workspace_dir = shared.workspace_dir;
        self.new_session.parent_session_id = shared.parent_session_id;
        // After a reboot every session takes this path (the tmux server died),
        // so the manual list position must survive the respawn or one restart
        // would scramble the whole order. `do_spawn_session` pushes + persists
        // the fresh session; stamp the inherited order on it afterwards.
        let display_order = shared.display_order;
        let before = self.sessions.len();
        self.do_spawn_session(name, &config, worktrees);
        if self.sessions.len() > before && display_order.is_some() {
            if let Some(session) = self.sessions.last_mut() {
                session.info.display_order = display_order;
            }
            self.save_state();
        }
    }

    /// Find a discovered backend session matching a shared session.
    ///
    /// Tries to match by `backend_id` first; if that fails (e.g. the row
    /// was created by the headless CLI/MCP path which doesn't know the
    /// real tmux pane id yet), falls back to matching by the sanitized
    /// window name (`tb-<safe_name>`).
    fn find_matching_discovered<'a>(
        shared: &sync::SharedSession,
        discovered: &'a [crate::agent::backend::DiscoveredSession],
    ) -> Option<&'a crate::agent::backend::DiscoveredSession> {
        if !shared.backend_id.is_empty() {
            if let Some(d) = discovered
                .iter()
                .find(|d| d.backend_id == shared.backend_id && d.is_alive)
            {
                return Some(d);
            }
        }
        let expected_name = crate::agent::tmux::agent_window_name(&shared.name);
        discovered
            .iter()
            .find(|d| d.name == expected_name && d.is_alive)
    }

    /// Paste `text` into a session as a bracketed paste, then queue an Enter.
    /// `boot_delay_ticks` delays the paste itself — pass 0 for a session that is
    /// already running, or [`AGENT_BOOT_DELAY_TICKS`] for one just spawned so
    /// its agent CLI has time to come up.
    fn send_prompt_to_session(&mut self, session_id: SessionId, text: &str, boot_delay_ticks: u64) {
        let _ = self.send_prompt_steps_to_session(
            session_id,
            &[crate::session::PromptStep::new(text)],
            boot_delay_ticks,
        );
    }

    /// Deliver an ordered list of prompt steps to a session, each as its own
    /// bracketed paste + Enter separated by that step's settle delay.
    ///
    /// One paste carrying newlines submits as a *single* prompt, so a sequence
    /// like `/model x` → `/effort y` → "do the work" only works as separate
    /// submissions; the gap also lets a slash command's autocomplete popup close
    /// before the next paste lands. `boot_delay_ticks` delays the *first* step
    /// (0 = send it inline to an already-running session).
    ///
    /// Only the inline leg can report: a deferred step is written by a later
    /// tick, long after this returns. `Err` therefore means the caller's very
    /// first delivery attempt failed, which an automation records as an error
    /// run instead of a success.
    fn send_prompt_steps_to_session(
        &mut self,
        session_id: SessionId,
        steps: &[crate::session::PromptStep],
        boot_delay_ticks: u64,
    ) -> Result<(), String> {
        let mut offset = boot_delay_ticks;
        for step in steps {
            let mut paste = b"\x1b[200~".to_vec();
            paste.extend_from_slice(step.text.as_bytes());
            paste.extend_from_slice(b"\x1b[201~");

            if offset == 0 {
                let Some(session) = self.sessions.iter().find(|s| s.info.id == session_id) else {
                    return Err(format!("session {session_id} is no longer open"));
                };
                if let Err(e) = session.send_input(paste) {
                    error!("Failed to send prompt to session {session_id}: {e}");
                    return Err(format!("failed to send prompt to {session_id}: {e}"));
                }
            } else {
                self.deferred_inputs
                    .push((session_id, paste, self.metrics.tick_count + offset));
            }
            let enter_at = offset + DEFERRED_INPUT_DELAY_TICKS;
            self.deferred_inputs.push((
                session_id,
                b"\r".to_vec(),
                self.metrics.tick_count + enter_at,
            ));
            offset = enter_at + step.delay().div_ceil(TICK_MS);
        }
        Ok(())
    }

    /// A "spawn (or reuse) a named session and prompt it" request — the shared
    /// shape behind automations (`auto-<id>`) and tasks (`<title> · #<id>`).
    /// A struct rather than a parameter list because the two callers differ in
    /// several optional fields (host, worktree, extras, step count).
    fn spawn_and_prompt(&mut self, req: SpawnPromptRequest<'_>) -> Result<SessionId, String> {
        let SpawnPromptRequest {
            name,
            repo_path,
            worktree_branch,
            base_branch,
            agent,
            host,
            extra_repos,
            steps,
        } = req;
        // Reuse an existing session (this run or restored after restart).
        if let Some(existing) = self.sessions.iter().find(|s| s.info.name == name) {
            let id = existing.info.id;
            let _ = self.send_prompt_steps_to_session(id, steps, 0);
            return Ok(id);
        }

        // Expand a leading `~` — the path may have been typed by hand in the
        // editor (or set via the CLI), and git/`current_dir` don't expand it.
        let repo_path = crate::paths::expand_tilde(&repo_path.to_string_lossy());
        let repo_path = repo_path.as_path();

        let mut worktrees: Vec<WorktreeInfo> = Vec::new();
        if let Some(branch) = worktree_branch {
            let base = base_branch.unwrap_or("main");
            // Idempotent: a recurring caller reuses the worktree it made on the
            // first invocation instead of failing because the branch exists.
            let path = git::create_or_attach_worktree(repo_path, branch, base)
                .map_err(|e| format!("create worktree {branch} off {base}: {e}"))?;
            worktrees.push(WorktreeInfo {
                repo_path: repo_path.to_path_buf(),
                worktree_path: path,
                branch: branch.to_string(),
            });
        }

        // Multi-repo: each extra repo gets its own worktree on the shared branch
        // (off its own base, falling back to the primary's) or is attached as-is.
        let mut additional_dirs: Vec<PathBuf> = Vec::new();
        for extra in extra_repos {
            let extra_path = crate::paths::expand_tilde(&extra.repo_path.to_string_lossy());
            if extra.worktree {
                let branch = worktree_branch.ok_or_else(|| {
                    "a worktree extra-repo requires a worktree branch".to_string()
                })?;
                let base = extra
                    .base_branch
                    .as_deref()
                    .or(base_branch)
                    .unwrap_or("main");
                let path = git::create_or_attach_worktree(&extra_path, branch, base)
                    .map_err(|e| format!("create worktree {branch} off {base}: {e}"))?;
                worktrees.push(WorktreeInfo {
                    repo_path: extra_path.clone(),
                    worktree_path: path,
                    branch: branch.to_string(),
                });
            } else {
                additional_dirs.push(extra_path);
            }
        }

        let cwd = worktrees
            .first()
            .map(|w| w.worktree_path.clone())
            .unwrap_or_else(|| repo_path.to_path_buf());
        let mut config = SessionConfig {
            cwd: Some(cwd),
            // A remote spawn runs on the host's backend (`ssh:<host>` /
            // `wsl:<host>`), which is also what routes `send_input` — so the
            // prompt steps below reach the right machine.
            backend: self.backend_name_for_host(host)?,
            ..SessionConfig::default()
        };
        if let Some(a) = agent {
            config.agent = a.to_string();
        }

        self.new_session.additional_dirs = additional_dirs;
        self.do_spawn_session(name.clone(), &config, worktrees);
        let session = self
            .sessions
            .iter()
            .find(|s| s.info.name == name)
            .ok_or_else(|| "session spawn failed".to_string())?;
        let id = session.info.id;
        let _ = self.send_prompt_steps_to_session(id, steps, AGENT_BOOT_DELAY_TICKS);
        Ok(id)
    }

    /// The backend name for a `hosts.toml` host — `None` for a local spawn.
    /// Errors when the host isn't configured, so a mistyped host fails at the
    /// fire (with a recorded error run) rather than silently spawning locally.
    fn backend_name_for_host(&self, host: Option<&str>) -> Result<Option<String>, String> {
        let Some(name) = host.filter(|h| !h.is_empty()) else {
            return Ok(None);
        };
        match self.hosts.get(name) {
            Some(h) => Ok(Some(h.backend_name())),
            None => Err(format!(
                "Unknown host '{name}'. Configure it in hosts.toml. Available: [{}]",
                self.hosts.names().join(", ")
            )),
        }
    }

    // ---- Tasks (right-side panel) ----------------------------------------

    /// The full agent prompt for a task (id + title + description + CLI hints),
    /// falling back to `title` if the task is no longer cached. Keeps the
    /// trigger paths from seeding an agent with just the bare title — the agent
    /// gets explicit context that it is solving a Friring task and how to fetch
    /// more / close it out (see [`crate::session::Task::agent_prompt`]).
    fn task_agent_prompt(&self, task_id: i64, title: &str) -> String {
        self.task_ui
            .cached_tasks
            .iter()
            .find(|t| t.id == task_id)
            .map(|t| t.agent_prompt())
            .unwrap_or_else(|| title.to_string())
    }

    /// Validate `m` and persist it. Returns `true` on success; on failure sets an
    /// error status and returns `false` (leaving the editor open).
    fn save_task(&mut self, m: &modals::TaskEditorModal) -> bool {
        let title = m.title.value().trim().to_string();
        if title.is_empty() {
            self.set_error("Title cannot be empty");
            return false;
        }
        // Trimmed-empty description persists as `None`.
        let description = {
            let d = m.description.value().trim();
            (!d.is_empty()).then(|| d.to_string())
        };
        let result = match m.editing_id {
            Some(id) => match self.db.get_task(id) {
                // The editor no longer authors the agent action — preserve any
                // action set out-of-band (e.g. via the CLI). The trigger-time
                // picker (`r`) is how the TUI runs an action.
                Ok(Some(mut task)) => {
                    task.title = title;
                    task.description = description;
                    task.status = m.status;
                    self.db.update_task(&task)
                }
                Ok(None) => {
                    self.set_error("Task no longer exists");
                    return false;
                }
                Err(e) => Err(e),
            },
            None => {
                let new = crate::storage::tasks::NewTask {
                    title,
                    description,
                    status: m.status,
                    action: None,
                    source: crate::session::SOURCE_LOCAL.to_string(),
                    external_id: None,
                    external_url: None,
                };
                self.db.create_task(&new).map(|_| ())
            }
        };
        if let Err(e) = result {
            self.set_error(format!("Failed to save task: {e}"));
            return false;
        }
        self.refresh_tasks();
        self.set_status(StatusLevel::Success, "Task saved");
        true
    }

    /// Compute the panel layout for `area` from the current panel visibility
    /// and feature flags — the single funnel into `layout::compute_layout`,
    /// so the view, mouse routing, and content sizing can never disagree.
    pub(crate) fn layout_for(&self, area: Rect) -> layout::PanelAreas {
        use crate::session::settings::InfoPanelPosition;
        // The measured row counts only steer the inline/auto info dock, so
        // skip the (line-building) measure when the position can't inline.
        let measure = self.show_info_panel && self.info_panel_position != InfoPanelPosition::Column;
        layout::compute_layout(
            area,
            &layout::LayoutParams {
                show_session_list: self.show_session_list,
                show_info_panel: self.show_info_panel,
                info_position: self.info_panel_position,
                info_rows: if measure { self.info_panel_rows() } else { 0 },
                session_rows: if measure { self.session_list_rows() } else { 0 },
                show_tasks_panel: self.show_tasks_panel,
                // The review's changed-files list and the activity view's tree both
                // live in the file-viewer column, so force that column present while
                // either overlay is open.
                show_file_viewer: self.show_file_viewer
                    || self.active_review().is_some()
                    || self.active_cc_activity().is_some(),
                show_global_search: self.global_search.active,
                show_automations_pane: self.features.automations,
                automation_count: self.automation_ui.cached_automations.len(),
                // Carve the transient status row whenever there's a message to show
                // (a status/error toast or the live sync spinner) — must match what
                // `render_status_message_row` renders so the row is never empty.
                show_status_row: self.worktree_sync.in_progress || self.status_message.is_some(),
            },
        )
    }

    /// Rows (incl. borders) the session list needs to show every row — one per
    /// session plus one per repo-group header. Reuses the cached order when its
    /// signature is fresh; on a change frame (`layout_for` runs before
    /// `render_left_panel` refreshes the cache) it recomputes without storing.
    fn session_list_rows(&self) -> u16 {
        let header_count = match &self.cached_session_order {
            Some((sig, order)) if *sig == self.session_order_signature() => {
                order.headers.iter().flatten().count()
            }
            _ => {
                let infos: Vec<&SessionInfo> = self.sessions.iter().map(|s| &s.info).collect();
                crate::ui::project_list::compute_session_order(&infos)
                    .headers
                    .iter()
                    .flatten()
                    .count()
            }
        };
        (self.sessions.len() + header_count).max(1) as u16 + 2
    }

    /// The layout for the whole terminal screen (mouse hit-testing, sizing).
    pub(crate) fn screen_layout(&self) -> layout::PanelAreas {
        self.layout_for(Rect::new(0, 0, self.terminal_cols, self.terminal_rows))
    }

    pub(crate) fn content_area_size(&self) -> (u16, u16) {
        let terminal = self.screen_layout().terminal;
        let inner = Block::default().borders(Borders::ALL).inner(terminal);
        (inner.height, inner.width)
    }
}

/// Bring a backend up (control-mode attach for local tmux, SSH connect +
/// remote tmux bring-up for a remote host), with the status-line-friendly
/// error both spawn paths surface. The async path calls this on its worker;
/// the sync path blocks on it inline (ADR-P12).
fn ensure_backend_ready(backend: &Arc<dyn SessionBackend>) -> Result<(), String> {
    backend
        .ensure_ready()
        .map_err(|e| format!("Backend '{}' not ready: {e:#}", backend.name()))
}

/// Populate `repo_display_names` on a session from worktree repo paths,
/// cwd, and additional_dirs, using git remote names where available.
///
/// Thin wrapper over [`session_member_dirs`] — the single source of truth for
/// *which* directories a session spans and in what order — keeping the displayed
/// repo names and the workspace symlink set from ever drifting.
fn resolve_repo_display_names(info: &mut SessionInfo) {
    info.repo_display_names =
        session_member_dirs(info.cwd.as_deref(), &info.worktrees, &info.additional_dirs)
            .into_iter()
            .filter_map(|(name, _)| name)
            .collect();
}

/// The bare host name behind a remote `backend_type` (`ssh:<name>` /
/// `wsl:<name>`), used to label a placeholder/unreachable session. `None` for a
/// local backend.
fn host_label_from_backend_type(backend_type: &str) -> Option<String> {
    backend_type
        .strip_prefix(crate::session::SSH_BACKEND_PREFIX)
        .or_else(|| backend_type.strip_prefix(crate::session::WSL_BACKEND_PREFIX))
        .map(str::to_string)
}

/// The `(display_name, directory)` pairs a session spans, in display order:
/// worktree repos first (name from the original `repo_path`, dir = the checkout),
/// then non-worktree `additional_dirs`; or the lone `cwd` repo when there are no
/// worktrees. `name` is `None` only for pathological paths with no resolvable
/// name. This is the canonical member set used for both the repo-name display and
/// the multi-repo symlink workspace.
fn session_member_dirs(
    cwd: Option<&std::path::Path>,
    worktrees: &[WorktreeInfo],
    additional_dirs: &[PathBuf],
) -> Vec<(Option<String>, PathBuf)> {
    let mut members: Vec<(Option<String>, PathBuf)> = Vec::new();

    let wt_paths: std::collections::HashSet<&std::path::Path> = worktrees
        .iter()
        .map(|wt| wt.worktree_path.as_path())
        .collect();

    if !worktrees.is_empty() {
        for wt in worktrees {
            members.push((
                git::repo_display_name(&wt.repo_path),
                wt.worktree_path.clone(),
            ));
        }
    } else if let Some(cwd) = cwd {
        members.push((git::repo_display_name(cwd), cwd.to_path_buf()));
    }

    for dir in additional_dirs {
        if !wt_paths.contains(dir.as_path()) {
            members.push((git::repo_display_name(dir), dir.clone()));
        }
    }

    members
}

/// The directory the agent process should launch in.
///
/// For a single-member session that's the member itself (`primary_cwd`). For a
/// multi-member session it is a per-session **symlink workspace** (built
/// idempotently from the members) so the agent sees every repo as a
/// subdirectory — agent-neutral, needing no per-CLI flag. `workspace_dir` is
/// the wizard's optional user-chosen location for that workspace (local spawns
/// only); `None` = the default id-derived path. On any failure it falls back
/// to `primary_cwd`.
fn resolve_process_cwd(
    agent_session_id: Option<&str>,
    primary_cwd: Option<PathBuf>,
    worktrees: &[WorktreeInfo],
    additional_dirs: &[PathBuf],
    host: Option<&crate::session::HostDef>,
    workspace_dir: Option<&std::path::Path>,
) -> Option<PathBuf> {
    let members = session_member_dirs(primary_cwd.as_deref(), worktrees, additional_dirs);
    if members.len() < 2 {
        return primary_cwd;
    }
    let Some(id) = agent_session_id else {
        return primary_cwd;
    };

    let pairs: Vec<(String, PathBuf)> = members
        .into_iter()
        .map(|(name, dir)| {
            let label = name
                .or_else(|| dir.file_name().and_then(|s| s.to_str()).map(String::from))
                .unwrap_or_else(|| "repo".to_string());
            (label, dir)
        })
        .collect();

    crate::session_ops::spawn::build_multi_repo_workspace(host, id, &pairs, workspace_dir)
        .or(primary_cwd)
}

#[cfg(test)]
mod acceptance;

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::agent::SessionBackend;
    use crate::session::{Automation, AutomationAction, AutomationRunStatus, AutomationSchedule};

    // --- Session switching tests ---

    /// Inert backend for unit tests. `detached` counts `detach` calls so
    /// lifecycle tests can assert pane I/O teardown.
    #[derive(Default)]
    struct StubBackend {
        detached: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl SessionBackend for StubBackend {
        fn name(&self) -> &str {
            "stub"
        }
        fn check_available(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn ensure_ready(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn spawn(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: Option<&Path>,
            _: &std::collections::HashMap<String, String>,
            _: u16,
            _: u16,
        ) -> anyhow::Result<crate::agent::backend::SpawnedSession> {
            anyhow::bail!("stub backend does not spawn")
        }
        fn adopt(
            &self,
            _: &str,
            _: u16,
            _: u16,
            _: Option<Vec<u8>>,
        ) -> anyhow::Result<crate::agent::backend::AdoptedSession> {
            anyhow::bail!("stub backend does not adopt")
        }
        fn discover(&self) -> anyhow::Result<Vec<crate::agent::backend::DiscoveredSession>> {
            Ok(vec![])
        }
        fn resize(&self, _: &str, _: u16, _: u16) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_dead(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn kill(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn detach(&self, _: &str) -> anyhow::Result<()> {
            self.detached
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn pane_pid(&self, _: &str) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
    }

    fn stub_backend_arc() -> Arc<dyn SessionBackend> {
        Arc::new(StubBackend::default())
    }

    fn stub_provider() -> Arc<dyn crate::agent::AgentProvider> {
        Arc::new(crate::agent::GenericProvider::new(
            crate::agent::agent_config::builtin_registry()
                .default_agent()
                .unwrap()
                .clone(),
        ))
    }

    fn stub_agents() -> AgentRegistry {
        crate::agent::agent_config::builtin_registry()
    }

    fn stub_backend() -> BackendRegistry {
        BackendRegistry::new(stub_backend_arc())
    }

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// Backend whose `shutdown` sleeps, to exercise `shutdown_backends`. With
    /// `name` unique per instance so several can share one registry (which is
    /// keyed by name).
    struct SlowShutdownBackend {
        backend_name: String,
        delay: std::time::Duration,
        done: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl SessionBackend for SlowShutdownBackend {
        fn name(&self) -> &str {
            &self.backend_name
        }
        fn check_available(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn ensure_ready(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn spawn(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: Option<&Path>,
            _: &std::collections::HashMap<String, String>,
            _: u16,
            _: u16,
        ) -> anyhow::Result<crate::agent::backend::SpawnedSession> {
            unimplemented!()
        }
        fn adopt(
            &self,
            _: &str,
            _: u16,
            _: u16,
            _: Option<Vec<u8>>,
        ) -> anyhow::Result<crate::agent::backend::AdoptedSession> {
            unimplemented!()
        }
        fn discover(&self) -> anyhow::Result<Vec<crate::agent::backend::DiscoveredSession>> {
            Ok(vec![])
        }
        fn resize(&self, _: &str, _: u16, _: u16) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_dead(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn kill(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn detach(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn pane_pid(&self, _: &str) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
        fn shutdown(&self) {
            std::thread::sleep(self.delay);
            self.done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn slow_backend_app(
        count: usize,
        delay: std::time::Duration,
    ) -> (App, Arc<std::sync::atomic::AtomicUsize>) {
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = BackendRegistry::new(stub_backend_arc());
        for i in 0..count {
            registry.register(Arc::new(SlowShutdownBackend {
                backend_name: format!("slow-{i}"),
                delay,
                done: Arc::clone(&done),
            }));
        }
        let app = App::new(24, 80, registry, stub_agents(), test_db());
        (app, done)
    }

    /// Backends tear down concurrently, so the cost is the slowest connection
    /// rather than the sum. Serially this would be ~1.2 s; the assertion has
    /// wide headroom so it fails only on an actual regression to serial.
    #[test]
    fn shutdown_backends_runs_concurrently() {
        let delay = std::time::Duration::from_millis(200);
        let (app, done) = slow_backend_app(6, delay);

        let start = std::time::Instant::now();
        app.shutdown_backends();
        let elapsed = start.elapsed();

        assert_eq!(
            done.load(std::sync::atomic::Ordering::SeqCst),
            6,
            "every backend torn down"
        );
        assert!(
            elapsed < delay * 3,
            "expected concurrent teardown, took {elapsed:?} for 6 × {delay:?}"
        );
    }

    /// A wedged backend must not hang quit: `shutdown_backends` gives up at
    /// `BACKEND_SHUTDOWN_TIMEOUT` and returns, leaving the straggler detached.
    #[test]
    fn shutdown_backends_gives_up_on_wedged_backend() {
        let (app, _done) = slow_backend_app(1, BACKEND_SHUTDOWN_TIMEOUT * 10);

        let start = std::time::Instant::now();
        app.shutdown_backends();
        let elapsed = start.elapsed();

        assert!(
            elapsed < BACKEND_SHUTDOWN_TIMEOUT * 3,
            "expected the timeout to bound teardown, took {elapsed:?}"
        );
    }

    #[test]
    fn poll_config_reload_picks_up_agents_toml_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        app.status_message = None;

        // No change → no reload, no toast.
        app.poll_config_reload();
        assert!(app.status_message.is_none());

        // An external edit appears on the next poll without a restart.
        let path = crate::agent::agent_config::agents_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "default = \"mine\"\n[[agents]]\nname = \"mine\"\ncommand = \"x\"\n",
        )
        .unwrap();

        app.poll_config_reload();
        assert_eq!(app.agents.default, "mine");
        assert_eq!(
            app.status_message.as_ref().map(|m| m.level),
            Some(StatusLevel::Info)
        );

        // Stable afterwards: no repeated toasts.
        app.status_message = None;
        app.poll_config_reload();
        assert!(app.status_message.is_none());
    }

    #[test]
    fn poll_config_reload_picks_up_keybindings_edit_but_not_self_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        app.status_message = None;

        // External edit → rebind applies live.
        crate::storage::keybindings::save_keybindings_json(r#"{ "QuitApp": ["ctrl+x"] }"#).unwrap();
        app.poll_config_reload();
        assert_eq!(
            app.keybindings.chord_for(crate::session::Action::QuitApp),
            Some(&crate::session::KeyChord::ctrl('x'))
        );
        assert!(app.status_message.is_some());

        // A self-write (the F1 editor persisting) refreshes the stored mtime,
        // so the next poll stays quiet.
        app.status_message = None;
        crate::storage::keybindings::save_keybindings_json(r#"{ "QuitApp": ["ctrl+z"] }"#).unwrap();
        app.mark_keybindings_saved();
        app.poll_config_reload();
        assert!(app.status_message.is_none());
    }

    #[test]
    fn poll_config_reload_applies_settings_live_feature_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        app.status_message = None;
        assert!(app.features.tasks);

        // An external edit disabling a live flag applies on the next poll.
        let path = crate::agent::settings_config::settings_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[features]\ntasks = false\n").unwrap();

        app.poll_config_reload();
        assert!(!app.features.tasks, "live feature flag reloaded from disk");
        assert!(app.status_message.is_some(), "reload toasts");

        // Stable afterwards: no repeated toasts.
        app.status_message = None;
        app.poll_config_reload();
        assert!(app.status_message.is_none());
    }

    #[test]
    fn mark_settings_saved_suppresses_self_write_toast() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        app.status_message = None;

        let path = crate::agent::settings_config::settings_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[features]\nfile_viewer = false\n").unwrap();
        app.mark_settings_saved();

        app.poll_config_reload();
        assert!(
            app.status_message.is_none(),
            "a recorded self-write doesn't re-toast"
        );
    }

    /// A notification click handler writes `pending_focus_session_id` to the
    /// shared SQLite metadata; the TUI's poll picks it up and switches the
    /// active session + focus.
    #[test]
    fn apply_pending_focus_request_switches_active_session() {
        let mut app = app_with_sessions(3);
        let target_id = app.sessions[2].info.id;
        app.active_index = 0;
        app.focus = InputFocus::SessionList;

        // Simulate the click-handler write.
        app.db
            .conn_ref()
            .execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![
                    crate::session::PENDING_FOCUS_SESSION_ID_KEY,
                    target_id.to_string()
                ],
            )
            .unwrap();

        app.apply_pending_focus_request();
        assert_eq!(app.active_index, 2);
        assert_eq!(app.focus, InputFocus::Terminal);
        // The row is consumed atomically, so a second call is a no-op.
        let prev_active = app.active_index;
        app.apply_pending_focus_request();
        assert_eq!(app.active_index, prev_active);
    }

    /// A focus request that doesn't match any current session is dropped
    /// (the session may have been deleted before the click landed).
    #[test]
    fn apply_pending_focus_request_ignores_unknown_session() {
        let mut app = app_with_sessions(2);
        app.active_index = 0;
        app.focus = InputFocus::SessionList;
        let bogus = crate::session::SessionId::default();
        app.db
            .conn_ref()
            .execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![
                    crate::session::PENDING_FOCUS_SESSION_ID_KEY,
                    bogus.to_string()
                ],
            )
            .unwrap();

        app.apply_pending_focus_request();
        assert_eq!(app.active_index, 0, "no match → leave selection alone");
        assert_eq!(app.focus, InputFocus::SessionList);
        // But the row is still consumed so a stale id doesn't sit forever.
        assert_eq!(app.db.take_pending_focus_session_id().unwrap(), None);
    }

    /// Garbage in the metadata key is ignored gracefully and the row consumed.
    #[test]
    fn apply_pending_focus_request_tolerates_garbage() {
        let mut app = app_with_sessions(2);
        app.db
            .conn_ref()
            .execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![crate::session::PENDING_FOCUS_SESSION_ID_KEY, "not-a-uuid"],
            )
            .unwrap();
        app.apply_pending_focus_request();
        assert_eq!(app.active_index, 0);
        assert_eq!(app.db.take_pending_focus_session_id().unwrap(), None);
    }

    #[test]
    fn apply_removed_sessions_detaches_pane_io() {
        let detached = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend_arc: Arc<dyn SessionBackend> = Arc::new(StubBackend {
            detached: Arc::clone(&detached),
        });
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );
        app.sessions.push(Session::stub(
            "removed-elsewhere",
            &backend_arc,
            &stub_provider(),
        ));
        let session_id = app.sessions[0].info.id;

        app.apply_removed_sessions(vec![session_id]);

        assert!(app.sessions.is_empty());
        assert_eq!(
            detached.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "externally removed session must detach so the reader thread EOFs"
        );
    }

    /// Create an App with N stub sessions.
    /// An unfiltered theme picker selecting `index`, as `open_theme_picker`
    /// would build it (match list = every entry, so filtered index == entry
    /// index).
    fn theme_picker_at(index: usize) -> modals::ThemePickerModal {
        let count = crate::ui::theme::all_theme_entries().len();
        modals::ThemePickerModal {
            index,
            original: crate::ui::theme::current(),
            filter: None,
            matches: (0..count).collect(),
        }
    }

    fn app_with_sessions(count: usize) -> App {
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );
        for _i in 0..count {
            let session = Session::stub("test-session", &backend_arc, &provider);
            app.sessions.push(session);
        }
        if !app.sessions.is_empty() {
            app.active_index = 0;
        }
        app
    }

    /// Persist `app.sessions[idx]` to the DB so `load_hook_states` can find it
    /// by id (the stub harness pushes sessions without DB rows).
    fn persist_session(app: &App, idx: usize) -> crate::session::SessionId {
        let shared = app.session_to_shared(&app.sessions[idx]);
        app.db.upsert_session(&shared).unwrap();
        shared.id
    }

    /// Simulate an external `session signal`: write the hook state, then
    /// invalidate the status cache the way a real out-of-process signal would
    /// (its commit bumps this connection's `data_version`). The tests share one
    /// in-memory connection, so the bump must be emulated explicitly.
    fn signal_hook(app: &mut App, id: crate::session::SessionId, state: &str) {
        app.db.set_hook_state(id, state).unwrap();
        app.invalidate_hook_state_cache();
    }

    #[test]
    fn restored_session_config_injects_identity_env() {
        // Regression: a restored/undeleted session must carry `FRIRING_SESSION`
        // so its status hooks can attribute `session signal` — otherwise the row
        // stays Idle forever. The two relaunch paths previously skipped this.
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let id = crate::session::SessionId::default();
        let config = App::restored_session_config(
            id,
            Some("agent-conv-uuid".into()),
            "claude".into(),
            "restored".into(),
            None,
            "local-tmux",
        );
        assert_eq!(config.session_id, Some(id));
        assert_eq!(config.backend, None, "local backend stays None");
        assert_eq!(
            config.env.get("FRIRING_SESSION"),
            Some(&id.to_string()),
            "FRIRING_SESSION must match the reused SessionId"
        );
        assert_eq!(
            config.env.get("FRIRING_SESSION_ID"),
            Some(&"agent-conv-uuid".to_string())
        );
        // The config/data dir overrides pin the hook's `friring-cli` to this DB.
        assert!(config
            .env
            .contains_key(crate::paths::CONFIG_DIR_OVERRIDE_ENV));
        assert!(config.env.contains_key(crate::paths::DATA_DIR_OVERRIDE_ENV));
    }

    #[test]
    fn restored_session_config_idless_agent_still_has_session_identity() {
        // An agent that can't report its own id (None) still gets `FRIRING_SESSION`
        // from the reused SessionId — the identity the CLI resolves from first.
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let id = crate::session::SessionId::default();
        let config = App::restored_session_config(
            id,
            None,
            "codex".into(),
            "restored".into(),
            None,
            "local-tmux",
        );
        assert_eq!(config.env.get("FRIRING_SESSION"), Some(&id.to_string()));
    }

    #[test]
    fn restored_session_config_remote_backend_carries_and_skips_local_dirs() {
        // A restored off-local session must set `backend` *before* env injection
        // so the local-path dir vars are skipped (they don't exist on the host)
        // — and so the relaunch provider adapts the def's args for the host.
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let id = crate::session::SessionId::default();
        let config = App::restored_session_config(
            id,
            Some("agent-conv-uuid".into()),
            "claude".into(),
            "restored".into(),
            None,
            "ssh:devbox",
        );
        assert_eq!(config.backend.as_deref(), Some("ssh:devbox"));
        assert!(config.env.contains_key("FRIRING_SESSION"));
        assert!(!config
            .env
            .contains_key(crate::paths::CONFIG_DIR_OVERRIDE_ENV));
        assert!(!config.env.contains_key(crate::paths::DATA_DIR_OVERRIDE_ENV));
    }

    #[test]
    fn derive_session_status_covers_every_state() {
        use crate::storage::HookRow;
        let row = |state: &str, state_at: i64, seen_at: i64| HookRow {
            state: Some(state.into()),
            state_at: Some(state_at),
            seen_at: Some(seen_at),
        };

        // Exited forces Idle, even with a live hook state.
        assert_eq!(
            derive_session_status(Some(&row("working", 1, 0)), true, false, 0),
            SessionStatus::Idle
        );
        // No hook / idle / unknown → Idle.
        assert_eq!(
            derive_session_status(None, false, false, 0),
            SessionStatus::Idle
        );
        assert_eq!(
            derive_session_status(Some(&row("idle", 1, 0)), false, false, 0),
            SessionStatus::Idle
        );
        assert_eq!(
            derive_session_status(Some(&row("nonsense", 1, 0)), false, false, 0),
            SessionStatus::Idle
        );
        // working (with live output) / blocked map straight through.
        assert_eq!(
            derive_session_status(Some(&row("working", 1, 0)), false, false, 0),
            SessionStatus::Working
        );
        // Quiet *up to and including* the threshold is still live (boundary).
        assert_eq!(
            derive_session_status(
                Some(&row("working", 1, 0)),
                false,
                false,
                WORKING_OUTPUT_STALE_MS
            ),
            SessionStatus::Working,
            "quiescence at exactly the threshold is still Working"
        );
        assert_eq!(
            derive_session_status(Some(&row("blocked", 1, 0)), false, false, 0),
            SessionStatus::Blocked
        );
        // A `working` state that's gone quiet past the threshold is a stuck
        // edge (interrupt / crash) → fall back to Idle; blocked is unaffected.
        assert_eq!(
            derive_session_status(
                Some(&row("working", 1, 0)),
                false,
                false,
                WORKING_OUTPUT_STALE_MS + 1
            ),
            SessionStatus::Idle,
            "a quiet 'working' state past the staleness window reverts to Idle"
        );
        assert_eq!(
            derive_session_status(
                Some(&row("blocked", 1, 0)),
                false,
                false,
                WORKING_OUTPUT_STALE_MS + 1
            ),
            SessionStatus::Blocked,
            "blocked never times out on quiescence"
        );
        // done: unseen → Done; already-seen or just-seen → Idle.
        assert_eq!(
            derive_session_status(Some(&row("done", 5, 0)), false, false, 0),
            SessionStatus::Done
        );
        assert_eq!(
            derive_session_status(Some(&row("done", 5, 5)), false, false, 0),
            SessionStatus::Idle
        );
        assert_eq!(
            derive_session_status(Some(&row("done", 5, 0)), false, true, 0),
            SessionStatus::Idle
        );
    }

    #[test]
    fn refresh_maps_hook_state_to_status() {
        let mut app = app_with_sessions(1);
        let id = persist_session(&app, 0);

        // No hook fired yet → Idle (never-active default).
        app.refresh_session_statuses();
        assert_eq!(app.sessions[0].info.status, SessionStatus::Idle);

        for (state, expected) in [
            ("working", SessionStatus::Working),
            ("blocked", SessionStatus::Blocked),
        ] {
            signal_hook(&mut app, id, state);
            app.refresh_session_statuses();
            assert_eq!(
                app.sessions[0].info.status, expected,
                "hook '{state}' should map to {expected:?}"
            );
        }
    }

    #[test]
    fn refresh_recovers_stuck_working_after_output_goes_quiet() {
        // A `working` session whose `done`/`idle` edge never fired (e.g. the turn
        // was interrupted with Esc — Claude Code emits no hook for that) must not
        // spin forever: once its terminal goes quiet past the staleness window it
        // falls back to Idle. While output is still fresh it stays Working.
        let mut app = app_with_sessions(1);
        let id = persist_session(&app, 0);
        signal_hook(&mut app, id, "working");

        // Fresh output → genuinely working.
        app.refresh_session_statuses();
        assert_eq!(app.sessions[0].info.status, SessionStatus::Working);

        // Terminal goes quiet past the threshold (interrupt, no further hook) →
        // the stuck state is rescued to Idle even though the DB still says working.
        app.sessions[0].backdate_output_for_test(WORKING_OUTPUT_STALE_MS + 1_000);
        app.refresh_session_statuses();
        assert_eq!(
            app.sessions[0].info.status,
            SessionStatus::Idle,
            "an interrupted (quiet) 'working' session must not spin forever"
        );
    }

    #[test]
    fn refresh_done_shows_until_focus_leaves_then_idle() {
        // Two sessions; session 0 is the active (focused) one.
        let mut app = app_with_sessions(2);
        let _id0 = persist_session(&app, 0);
        let id1 = persist_session(&app, 1);
        app.active_index = 0;
        app.refresh_session_statuses(); // establish focus baseline (on session 0)

        // The FOCUSED session finishes: `Done` is visible (not instantly Idle) —
        // you should see the blue "done" for the session you're watching.
        app.active_index = 1;
        app.refresh_session_statuses(); // focus moves to 1 (baseline update)
        signal_hook(&mut app, id1, "done");
        app.refresh_session_statuses();
        assert_eq!(
            app.sessions[1].info.status,
            SessionStatus::Done,
            "a done session you're viewing shows Done, not instant Idle"
        );

        // Move focus OFF it → acknowledged → seen → Idle (persisted).
        app.active_index = 0;
        app.refresh_session_statuses();
        assert_eq!(app.sessions[1].info.status, SessionStatus::Idle);
        let row = app.db.load_hook_states().unwrap();
        let row = row.get(&id1).unwrap();
        assert!(
            row.seen_at.unwrap_or(0) >= row.state_at.unwrap_or(i64::MAX),
            "seen_at persisted at/after the done timestamp"
        );
    }

    #[test]
    fn refresh_seen_done_stays_idle_without_reload() {
        // Regression for the status-cache write-through (ADR-P6): once a `done`
        // session is acknowledged (focus left → seen), it must stay Idle on
        // later ticks even when nothing reloads the cache. Marking it seen is a
        // same-connection write that does NOT bump `data_version`, so without
        // mirroring `seen_at` into the cached row the next derive would see a
        // stale `seen_at < state_at` and flip it back to Done.
        let mut app = app_with_sessions(2);
        persist_session(&app, 0);
        let id1 = persist_session(&app, 1);
        app.active_index = 0;
        app.refresh_session_statuses(); // baseline focus on 0

        app.active_index = 1;
        app.refresh_session_statuses(); // focus → 1
        signal_hook(&mut app, id1, "done");
        app.refresh_session_statuses();
        assert_eq!(app.sessions[1].info.status, SessionStatus::Done);

        // Acknowledge by leaving focus: seen_at written + mirrored into cache.
        app.active_index = 0;
        app.refresh_session_statuses();
        assert_eq!(app.sessions[1].info.status, SessionStatus::Idle);

        // A further refresh with no external change must NOT reload the cache,
        // yet the session stays Idle (proves the write-through, not a reload).
        let loads_before = app.perf_counters().hook_state_loads;
        app.refresh_session_statuses();
        assert_eq!(
            app.perf_counters().hook_state_loads,
            loads_before,
            "no external change ⇒ no cache reload on the follow-up tick"
        );
        assert_eq!(
            app.sessions[1].info.status,
            SessionStatus::Idle,
            "an acknowledged done session must stay Idle via the seen_at write-through"
        );
    }

    #[test]
    fn refresh_done_unfocused_shows_done() {
        // A background session that finishes shows Done (blue) until visited.
        let mut app = app_with_sessions(2);
        persist_session(&app, 0);
        let id1 = persist_session(&app, 1);
        app.active_index = 0;
        signal_hook(&mut app, id1, "done");
        app.refresh_session_statuses();
        assert_eq!(app.sessions[1].info.status, SessionStatus::Done);
    }

    #[test]
    fn refresh_marks_dirty_on_status_change() {
        let mut app = app_with_sessions(1);
        let id = persist_session(&app, 0);
        app.refresh_session_statuses();
        app.mark_redrawn();
        assert!(!app.should_redraw(), "quiescent after a redraw");

        // An external hook write must make the next refresh repaint.
        signal_hook(&mut app, id, "blocked");
        app.refresh_session_statuses();
        assert!(
            app.should_redraw(),
            "a hook-driven status change must mark the UI dirty"
        );
    }

    #[test]
    fn working_session_animates_spinner_and_repaints() {
        let mut app = app_with_sessions(1);
        let id = persist_session(&app, 0);
        signal_hook(&mut app, id, "working");

        // Advance enough ticks to cross a spinner-frame boundary and confirm the
        // frame moves and the UI is marked dirty (so the live list animates).
        app.metrics.tick_count = 0;
        app.refresh_session_statuses();
        let f0 = app.spinner_frame();
        app.mark_redrawn();
        app.metrics.tick_count = SPINNER_TICKS_PER_FRAME; // next frame
        app.refresh_session_statuses();
        assert_ne!(app.spinner_frame(), f0, "spinner frame advances");
        assert!(
            app.should_redraw(),
            "a working session keeps the list repainting"
        );

        // The live glyph for Working is a spinner frame, not the static icon.
        let g = crate::ui::status_glyph(
            SessionStatus::Working,
            crate::ui::SPINNER_FRAMES[app.spinner_frame()],
        );
        assert!(crate::ui::SPINNER_FRAMES.contains(&g));
    }

    #[test]
    fn spinner_advances_even_when_a_status_field_also_changes() {
        // Regression: the spinner must tick on *every* refresh, never be
        // short-circuited past by the `||` when another visible field changed
        // the same tick. Session 0 stays Working (driving the spinner); session
        // 1 flips Idle→Blocked on the second refresh so `changed` is true.
        let mut app = app_with_sessions(2);
        let id0 = persist_session(&app, 0);
        let id1 = persist_session(&app, 1);
        signal_hook(&mut app, id0, "working");

        app.metrics.tick_count = 0;
        app.refresh_session_statuses();
        let f0 = app.spinner_frame();

        // Cross a spinner-frame boundary AND change session 1's status together.
        app.metrics.tick_count = SPINNER_TICKS_PER_FRAME;
        signal_hook(&mut app, id1, "blocked");
        app.refresh_session_statuses();

        assert_eq!(app.sessions[1].info.status, SessionStatus::Blocked);
        assert_ne!(
            app.spinner_frame(),
            f0,
            "spinner must advance even when another field changed the same tick"
        );
    }

    #[test]
    fn idle_session_does_not_force_spinner_repaints() {
        let mut app = app_with_sessions(1);
        persist_session(&app, 0); // no hook → Idle
        app.refresh_session_statuses();
        app.mark_redrawn();
        // No working session: crossing a spinner boundary must NOT force a paint.
        app.metrics.tick_count += SPINNER_TICKS_PER_FRAME;
        app.refresh_session_statuses();
        assert!(
            !app.should_redraw(),
            "an idle TUI must not repaint just to animate a (nonexistent) spinner"
        );
    }

    #[test]
    fn refresh_exited_session_is_idle_regardless_of_hook() {
        let mut app = app_with_sessions(1);
        let id = persist_session(&app, 0);
        signal_hook(&mut app, id, "blocked");
        app.sessions[0].mark_exited_for_test();
        app.refresh_session_statuses();
        assert_eq!(app.sessions[0].info.status, SessionStatus::Idle);
    }

    #[test]
    fn start_new_session_skips_host_picker_when_no_hosts() {
        let mut app = app_with_sessions(0);
        app.start_new_session();
        // No hosts configured → straight to the repo picker, no host step.
        assert!(matches!(app.modal, modals::Modal::RepoPicker(_)));
        assert!(app.new_session.backend.is_none());
    }

    #[test]
    fn start_new_session_shows_host_picker_with_hosts() {
        let mut app = app_with_sessions(0);
        app.set_hosts(crate::session::HostRegistry {
            config_version: None,
            hosts: vec![
                crate::session::HostDef {
                    name: "devbox".into(),
                    destination: "me@devbox".into(),
                    ..Default::default()
                },
                crate::session::HostDef::wsl("Ubuntu"),
            ],
        });
        app.start_new_session();
        match app.modal {
            modals::Modal::HostPicker(ref hp) => {
                // "local" first, then each off-local host (ssh + wsl).
                assert_eq!(hp.choices.len(), 3);
                assert_eq!(hp.choices[0].backend, "");
                assert_eq!(hp.choices[1].backend, "ssh:devbox");
                assert_eq!(hp.choices[2].backend, "wsl:Ubuntu");
                assert!(hp.choices[2].label.contains("WSL"));
            }
            ref other => panic!("expected host picker, got {other:?}"),
        }
    }

    #[test]
    fn host_for_backend_resolves_ssh_and_wsl() {
        let mut app = app_with_sessions(0);
        app.set_hosts(crate::session::HostRegistry {
            config_version: None,
            hosts: vec![
                crate::session::HostDef {
                    name: "devbox".into(),
                    destination: "me@devbox".into(),
                    ..Default::default()
                },
                crate::session::HostDef::wsl("Ubuntu"),
            ],
        });
        assert!(app.host_for_backend(None).is_none());
        assert!(app.host_for_backend(Some("local-tmux")).is_none());
        assert_eq!(
            app.host_for_backend(Some("ssh:devbox"))
                .unwrap()
                .destination,
            "me@devbox"
        );
        assert!(app.host_for_backend(Some("wsl:Ubuntu")).unwrap().is_wsl());
        assert!(app.host_for_backend(Some("ssh:unknown")).is_none());
    }

    fn devbox_registry() -> crate::session::HostRegistry {
        crate::session::HostRegistry {
            config_version: None,
            hosts: vec![crate::session::HostDef {
                name: "devbox".into(),
                destination: "me@devbox".into(),
                ..Default::default()
            }],
        }
    }

    #[test]
    fn usage_plan_scopes_by_agent_and_host() {
        let mut app = app_with_sessions(3);
        app.set_hosts(devbox_registry());
        // Two local claude sessions (deduped) + one on devbox (own scope).
        app.sessions[0].info.agent = "claude".into();
        app.sessions[1].info.agent = "claude".into();
        app.sessions[1].info.remote_host = Some("devbox".into());
        app.sessions[2].info.agent = "claude".into();

        let plan = app.plan_usage_fetches();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, ("claude".to_string(), None));
        assert!(matches!(plan[0].1, UsageFetchPlan::Fetch(None)));
        assert_eq!(plan[1].0, ("claude".to_string(), Some("devbox".into())));
        match &plan[1].1 {
            UsageFetchPlan::Fetch(Some(h)) => assert_eq!(h.destination, "me@devbox"),
            other => panic!("expected host-scoped fetch, got {other:?}"),
        }
    }

    #[test]
    fn usage_plan_notes_unknown_host_and_skips_unsupported() {
        let mut app = app_with_sessions(2);
        // A remote session whose host vanished from hosts.toml, plus an
        // agent without usage support: only the former appears, as a note.
        app.sessions[0].info.agent = "claude".into();
        app.sessions[0].info.remote_host = Some("ghost".into());
        app.sessions[1].info.agent = "aider".into();

        let plan = app.plan_usage_fetches();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0, ("claude".to_string(), Some("ghost".into())));
        match plan[0].1 {
            UsageFetchPlan::Unavailable(note) => assert!(note.contains("not configured")),
            _ => panic!("expected an unavailable note for an unknown host"),
        }
    }

    #[test]
    fn usage_unreachable_host_notes_but_keeps_last_known_data() {
        let mut app = app_with_sessions(0);
        app.set_hosts(devbox_registry());
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut info = crate::session::SessionInfo::new("remote".into());
        info.agent = "claude".into();
        info.remote_host = Some("devbox".into());
        app.sessions.push(Session::placeholder(
            info,
            24,
            80,
            &backend_arc,
            &provider,
            HashMap::new(),
        ));

        // No cached data → the outage note fills the scope (no ssh spawned).
        app.spawn_usage_fetches();
        let key = ("claude".to_string(), Some("devbox".to_string()));
        let note = app.usage.get(&key).unwrap().note.clone().unwrap();
        assert!(note.contains("unreachable"), "got note: {note}");

        // With real data cached from before the outage, the note must not
        // clobber it — last-known usage beats a transient host drop.
        app.usage.insert(
            key.clone(),
            crate::session::AgentUsage {
                windows: vec![],
                plan: Some("max".into()),
                note: None,
            },
        );
        app.spawn_usage_fetches();
        assert_eq!(app.usage.get(&key).unwrap().plan.as_deref(), Some("max"));
    }

    #[test]
    fn usage_plan_live_session_beats_placeholder_for_same_scope() {
        // Mid-reconnect a host's sessions can be mixed: a placeholder seen
        // first must not shadow an adopted (live) session's fetch.
        let mut app = app_with_sessions(1);
        app.set_hosts(devbox_registry());
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut info = crate::session::SessionInfo::new("remote".into());
        info.agent = "claude".into();
        info.remote_host = Some("devbox".into());
        let placeholder =
            Session::placeholder(info, 24, 80, &backend_arc, &provider, HashMap::new());
        app.sessions.insert(0, placeholder);
        app.sessions[1].info.agent = "claude".into();
        app.sessions[1].info.remote_host = Some("devbox".into());

        let plan = app.plan_usage_fetches();
        assert_eq!(plan.len(), 1);
        assert!(matches!(plan[0].1, UsageFetchPlan::Fetch(Some(_))));
    }

    #[test]
    fn resolve_persisted_backend_skips_unknown_ssh_but_falls_back_for_local() {
        let app = app_with_sessions(0);
        // Known backend → resolved.
        assert_eq!(
            app.resolve_persisted_backend("stub")
                .map(|b| b.name().to_string()),
            Some("stub".to_string())
        );
        // Legacy/local values fall back to the default backend.
        for legacy in ["", "tmux", "local-tmux"] {
            assert_eq!(
                app.resolve_persisted_backend(legacy)
                    .map(|b| b.name().to_string()),
                Some("stub".to_string()),
                "legacy '{legacy}' should fall back to default"
            );
        }
        // Unknown remote backend → skipped (None), never misadopted on local.
        assert!(app.resolve_persisted_backend("ssh:nope").is_none());
    }

    #[test]
    fn backend_for_defaults_to_registry_default() {
        let app = app_with_sessions(0);
        let config = SessionConfig::default();
        let backend = app.backend_for(&config).expect("default backend");
        assert_eq!(backend.name(), "stub");
    }

    #[test]
    fn backend_for_empty_name_uses_default() {
        let app = app_with_sessions(0);
        let config = SessionConfig {
            backend: Some(String::new()),
            ..SessionConfig::default()
        };
        let backend = app.backend_for(&config).expect("default backend");
        assert_eq!(backend.name(), "stub");
    }

    #[test]
    fn backend_for_unknown_backend_errors() {
        let app = app_with_sessions(0);
        let config = SessionConfig {
            backend: Some("ssh:does-not-exist".into()),
            ..SessionConfig::default()
        };
        match app.backend_for(&config) {
            Ok(b) => panic!("expected error, got backend {}", b.name()),
            Err(err) => assert!(err.contains("Unknown backend"), "got: {err}"),
        }
    }

    #[test]
    fn switch_forward_advances_to_next_session() {
        let mut app = app_with_sessions(3);
        app.active_index = 0;
        app.switch_session_forward();
        assert_eq!(app.active_index, 1);
    }

    #[test]
    fn switch_forward_at_last_session_wraps() {
        let mut app = app_with_sessions(3);
        app.active_index = 2;
        app.switch_session_forward();
        assert_eq!(app.active_index, 0);
    }

    #[test]
    fn switch_backward_moves_to_previous_session() {
        let mut app = app_with_sessions(3);
        app.active_index = 2;
        app.switch_session_backward();
        assert_eq!(app.active_index, 1);
    }

    #[test]
    fn switch_backward_at_first_session_wraps() {
        let mut app = app_with_sessions(3);
        app.active_index = 0;
        app.switch_session_backward();
        assert_eq!(app.active_index, 2);
    }

    #[test]
    fn switch_with_no_sessions_is_noop() {
        let mut app = app_with_sessions(0);
        app.switch_session_forward();
        assert_eq!(app.active_index, 0);
        app.switch_session_backward();
        assert_eq!(app.active_index, 0);
    }

    #[test]
    fn apply_removed_keeps_active_anchored_when_earlier_session_removed() {
        // Regression: a CLI `session delete` of a session *before* the active
        // one must shift `active_index` down so it keeps pointing at the SAME
        // session, not silently jump to a different one. [A, B(active), C];
        // delete A → [B, C], active must stay on B (now index 0).
        let mut app = app_with_sessions(3);
        app.active_index = 1;
        let active_id = app.sessions[1].info.id;
        let removed_id = app.sessions[0].info.id;

        app.apply_removed_sessions(vec![removed_id]);

        assert_eq!(app.sessions.len(), 2);
        assert_eq!(
            app.active_index, 0,
            "active_index should follow its session down after an earlier removal"
        );
        assert_eq!(
            app.sessions[app.active_index].info.id, active_id,
            "active session identity must be preserved across external removal"
        );
    }

    #[test]
    fn apply_removed_active_session_clamps_in_bounds() {
        // Deleting the active session (the last one) must leave `active_index`
        // in bounds so subsequent raw-index access (restart, shell toggle)
        // can't panic. [A, B, C(active)]; delete C → [A, B], active in bounds.
        let mut app = app_with_sessions(3);
        app.active_index = 2;
        let removed_id = app.sessions[2].info.id;

        app.apply_removed_sessions(vec![removed_id]);

        assert_eq!(app.sessions.len(), 2);
        assert!(
            app.active_index < app.sessions.len(),
            "active_index must stay in bounds after the active session is removed"
        );
    }

    #[test]
    fn apply_removed_all_sessions_resets_index() {
        // A CLI clearing every session must not leave a dangling index.
        let mut app = app_with_sessions(2);
        app.active_index = 1;
        let ids: Vec<_> = app.sessions.iter().map(|s| s.info.id).collect();

        app.apply_removed_sessions(ids);

        assert!(app.sessions.is_empty());
        assert_eq!(app.active_index, 0);
    }

    #[test]
    fn restart_with_stale_active_index_does_not_panic() {
        // If external state shrank the list and left `active_index` out of
        // bounds, hitting restart must degrade gracefully, never panic.
        let mut app = app_with_sessions(1);
        app.active_index = 5; // stale, out of bounds
                              // Should be a no-op, not an index-out-of-bounds panic.
        app.restart_active_session();
    }

    #[test]
    fn switch_follows_activity_and_repo_group_order() {
        use crate::session::SessionStatus;
        // DB order: [webapp/Working, infra/Blocked, webapp/Working].
        let mut app = app_with_sessions(3);
        app.sessions[0].info.repo_display_names = vec!["webapp".to_string()];
        app.sessions[0].info.status = SessionStatus::Working;
        app.sessions[1].info.repo_display_names = vec!["infra".to_string()];
        app.sessions[1].info.status = SessionStatus::Blocked;
        app.sessions[2].info.repo_display_names = vec!["webapp".to_string()];
        app.sessions[2].info.status = SessionStatus::Working;

        // Order is status-independent: by repo group (infra before webapp by
        // group label), so navigation visits [1] then [0, 2].
        app.active_index = 1;
        app.switch_session_forward();
        assert_eq!(app.active_index, 0, "infra → webapp/0");
        app.switch_session_forward();
        assert_eq!(app.active_index, 2, "webapp/0 → webapp/2");
        app.switch_session_forward();
        assert_eq!(app.active_index, 1, "webapp/2 → wrap to infra");
    }

    #[test]
    fn switch_with_single_session_is_noop() {
        let mut app = app_with_sessions(1);
        app.active_index = 0;
        app.switch_session_forward();
        assert_eq!(app.active_index, 0);
        app.switch_session_backward();
        assert_eq!(app.active_index, 0);
    }

    // --- Scroll tests ---

    fn parser_with_scrollback() -> vt100::Parser {
        let mut parser = vt100::Parser::new(24, 80, 100);
        for i in 0..50 {
            parser.process(format!("line {i}\r\n").as_bytes());
        }
        parser
    }

    #[test]
    fn scrollback_starts_at_zero() {
        let parser = parser_with_scrollback();
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn scrollback_increments() {
        let mut parser = parser_with_scrollback();
        parser.screen_mut().set_scrollback(5);
        assert_eq!(parser.screen().scrollback(), 5);
    }

    #[test]
    fn scrollback_clamps_to_max() {
        let mut parser = parser_with_scrollback();
        parser.screen_mut().set_scrollback(usize::MAX);
        let max = parser.screen().scrollback();
        // Should be clamped to the actual scrollback content, not usize::MAX
        assert!(max < usize::MAX);
        assert!(max > 0);
    }

    #[test]
    fn scrollback_restores_after_probe() {
        let mut parser = parser_with_scrollback();
        parser.screen_mut().set_scrollback(3);

        // Probe total scrollback (same technique as render_terminal)
        let saved = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(usize::MAX);
        let _total = parser.screen().scrollback();
        parser.screen_mut().set_scrollback(saved);

        assert_eq!(parser.screen().scrollback(), 3);
    }

    #[test]
    fn scrollback_zero_stays_at_bottom() {
        let mut parser = parser_with_scrollback();
        assert_eq!(parser.screen().scrollback(), 0);

        // New output while at bottom keeps offset at 0
        parser.process(b"new line\r\n");
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn page_scroll_amount_is_half_content_height() {
        let app = App::new(50, 100, stub_backend(), stub_agents(), test_db());
        // rows = 50 - 4 = 46, half = 23
        assert_eq!(app.page_scroll_amount(), 23);
    }

    #[test]
    fn page_scroll_amount_small_terminal() {
        let app = App::new(6, 80, stub_backend(), stub_agents(), test_db());
        // rows = 6 - 4 = 2, half = 1
        assert_eq!(app.page_scroll_amount(), 1);
    }

    #[test]
    fn mouse_scroll_lines_constant() {
        assert_eq!(MOUSE_SCROLL_LINES, 3);
    }

    // --- Session naming tests ---

    #[test]
    fn next_session_name_starts_at_one() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        assert_eq!(app.next_session_name(), "1");
    }

    #[test]
    fn next_session_name_increments() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        assert_eq!(app.next_session_name(), "1");
        assert_eq!(app.next_session_name(), "2");
        assert_eq!(app.next_session_name(), "3");
    }

    #[test]
    fn next_session_name_continues_from_restored_counter() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.session_counter = 5;
        assert_eq!(app.next_session_name(), "6");
    }

    #[test]
    fn automations_pane_focus_cycle_and_nav() {
        use crate::session::{AutomationAction, AutomationSchedule};
        let make = |id: i64, name: &str| Automation {
            id,
            name: name.into(),
            enabled: true,
            schedule: AutomationSchedule::Once { at: 0 },
            timezone: None,
            action: AutomationAction::send_to(SessionId::default()),
            prompt: "p".into(),
            created_at: 0,
            updated_at: 0,
            last_run_at: None,
            next_run_at: None,
            prompt_steps: Vec::new(),
        };
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());

        // The automations pane is reached via j/k (part of the left column), and
        // its central counterpart is the editor: Ctrl+L/H from the pane move
        // into the editor and back, just like SessionList ↔ Terminal.
        app.focus = InputFocus::SessionList;
        assert_eq!(app.cycle_focus_forward(), InputFocus::Terminal);
        app.focus = InputFocus::Terminal;
        assert_eq!(app.cycle_focus_backward(), InputFocus::SessionList);
        app.focus = InputFocus::Automations;
        assert_eq!(app.cycle_focus_forward(), InputFocus::AutomationEditor);
        assert_eq!(app.cycle_focus_backward(), InputFocus::AutomationEditor);
        app.focus = InputFocus::AutomationEditor;
        assert_eq!(app.cycle_focus_backward(), InputFocus::Automations);

        // j/k navigate within the pane; past either end they loop out into the
        // session list (the column is circular).
        // Keys route through the real pipeline: focus = Automations scopes the
        // lookup to `KeyContext::Automations`, resolving j/k to the pane actions.
        app.automation_ui.cached_automations = vec![make(1, "a"), make(2, "b")];
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.automation_ui.automation_panel_index, 1);
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.automation_ui.automation_panel_index, 0);
        // j past the last automation loops out to the session list.
        app.automation_ui.automation_panel_index = 1;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::SessionList);
    }

    #[test]
    fn session_and_automation_navigation_forms_a_loop() {
        use crate::session::{AutomationAction, AutomationSchedule};
        let make = |id: i64, name: &str| Automation {
            id,
            name: name.into(),
            enabled: true,
            schedule: AutomationSchedule::Once { at: 0 },
            timezone: None,
            action: AutomationAction::send_to(SessionId::default()),
            prompt: "p".into(),
            created_at: 0,
            updated_at: 0,
            last_run_at: None,
            next_run_at: None,
            prompt_steps: Vec::new(),
        };
        let mut app = app_with_sessions(2);
        app.automation_ui.cached_automations = vec![make(1, "a"), make(2, "b")];

        // Down past the last session drops into the automations pane.
        app.focus = InputFocus::SessionList;
        app.active_index = 1; // last session in render order
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::Automations);
        assert_eq!(app.automation_ui.automation_panel_index, 0);

        // Down past the last automation loops to the TOP session.
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE); // a → b
        assert_eq!(app.automation_ui.automation_panel_index, 1);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE); // past last → top session
        assert_eq!(app.focus, InputFocus::SessionList);
        assert_eq!(app.active_index, 0, "looped to first session");

        // Up from the first session loops to the LAST automation.
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::Automations);
        assert_eq!(
            app.automation_ui.automation_panel_index, 1,
            "looped to last automation"
        );

        // And k at the top automation hands back up to the last session.
        app.automation_ui.automation_panel_index = 0;
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::SessionList);
        assert_eq!(app.active_index, 1, "back to last session");
    }

    // --- Role editor tests ---

    #[test]
    fn ctrl_h_cycles_focus_backward_from_terminal() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::Terminal;
        // Backward from the terminal lands on the session list (the automations
        // pane is not a cycle stop — it's reached via j/k).
        app.handle_key(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::SessionList);
    }

    #[test]
    fn ctrl_h_cycles_focus_backward_from_session_list() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::SessionList;
        app.handle_key(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::Terminal);
    }

    #[test]
    fn ctrl_c_copies_selection_or_falls_through() {
        let mut app = app_with_sessions(1);
        let initial_count = app.sessions.len();
        // With no selection, Ctrl+C should NOT close session (it falls through to terminal)
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.sessions.len(), initial_count);
    }

    #[test]
    fn any_key_clears_text_selection() {
        let mut app = app_with_sessions(1);
        app.text_selection = Some(Selection::new(
            TermPos { row: 0, col: 0 },
            PaneBounds::from_rect(ratatui::layout::Rect::new(0, 0, 80, 24)),
        ));
        assert!(app.text_selection.is_some());

        // Any non-copy key should clear the selection
        app.handle_key(KeyCode::Char('a'), KeyModifiers::empty());
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn ctrl_v_clears_selection() {
        let mut app = app_with_sessions(1);
        app.text_selection = Some(Selection::new(
            TermPos { row: 0, col: 0 },
            PaneBounds::from_rect(ratatui::layout::Rect::new(0, 0, 80, 24)),
        ));

        // Ctrl+V should clear selection (paste)
        app.handle_key(KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert!(app.text_selection.is_none());
    }

    // --- Cmd/Super chord routing (kitty keyboard protocol) ---

    #[test]
    fn super_chord_dispatches_bound_global_action() {
        let mut app = app_with_sessions(1);
        app.keybindings.rebind(
            crate::session::Action::ToggleFileViewer,
            crate::session::KeyChord::cmd('e'),
        );
        assert!(!app.show_file_viewer);
        app.handle_key(KeyCode::Char('e'), KeyModifiers::SUPER);
        assert!(app.show_file_viewer, "bound Cmd chord must dispatch");
    }

    #[test]
    fn super_chord_never_types_into_modal_text_input() {
        let mut app = app_with_sessions(1);
        app.modal = modals::Modal::SessionName(modals::SessionNameModal::default());
        app.handle_key(KeyCode::Char('j'), KeyModifiers::SUPER);
        let modals::Modal::SessionName(ref sn) = app.modal else {
            panic!("modal must stay open");
        };
        assert_eq!(
            sn.name.value(),
            "",
            "Cmd+J must not insert a bare 'j' into the text input"
        );
    }

    /// Cmd+C through the full key pipeline copies the active selection —
    /// the macOS alternate for Ctrl+C. Bound explicitly (not via defaults)
    /// so the test is platform-independent; the macOS default set carrying
    /// `cmd+c` is asserted in `session::keybindings`.
    #[test]
    fn cmd_c_copies_active_selection() {
        let mut app = app_with_sessions(1);
        app.captured_clipboard = Some(Vec::new());
        app.keybindings.rebind(
            crate::session::Action::Copy,
            crate::session::KeyChord::cmd('c'),
        );
        app.focus = InputFocus::Terminal;
        app.text_selection = Some(Selection::new(
            TermPos { row: 0, col: 0 },
            PaneBounds::from_rect(ratatui::layout::Rect::new(0, 0, 80, 24)),
        ));
        app.selected_text_cache = Some("copied text".into());

        app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER);

        assert_eq!(
            app.captured_clipboard.as_deref(),
            Some(&["copied text".to_string()][..])
        );
        assert!(app.text_selection.is_none(), "copy consumes the selection");
        assert_eq!(
            app.status_message.as_ref().map(|m| m.text.as_str()),
            Some("Copied to clipboard")
        );
    }

    /// Cmd+C with no selection is inert in a focused terminal: no copy, and
    /// nothing reaches the PTY — neither a SIGINT byte nor a stray literal
    /// `c`. (Ctrl+C's no-selection fallthrough to SIGINT is the next test.)
    #[test]
    fn cmd_c_without_selection_sends_nothing_to_the_pty() {
        use tokio::sync::mpsc::error::TryRecvError;
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );
        let (session, mut input_rx) = Session::stub_with_input_rx("s", &backend_arc, &provider);
        app.sessions.push(session);
        app.active_index = 0;
        app.captured_clipboard = Some(Vec::new());
        app.keybindings.rebind(
            crate::session::Action::Copy,
            crate::session::KeyChord::cmd('c'),
        );
        app.focus = InputFocus::Terminal;

        app.handle_key(KeyCode::Char('c'), KeyModifiers::SUPER);

        assert!(
            matches!(input_rx.try_recv(), Err(TryRecvError::Empty)),
            "Cmd+C must not forward anything to the PTY"
        );
        assert!(
            app.captured_clipboard.as_ref().unwrap().is_empty(),
            "nothing to copy without a selection"
        );
    }

    /// The regression guard in the other direction: with the default
    /// bindings, Ctrl+C with no selection keeps its interrupt meaning — the
    /// Copy binding falls through and the PTY receives the SIGINT byte.
    #[test]
    fn ctrl_c_without_selection_still_interrupts_the_pty() {
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );
        let (session, mut input_rx) = Session::stub_with_input_rx("s", &backend_arc, &provider);
        app.sessions.push(session);
        app.active_index = 0;
        app.captured_clipboard = Some(Vec::new());
        app.focus = InputFocus::Terminal;

        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(
            input_rx.try_recv().ok(),
            Some(vec![0x03]),
            "Ctrl+C without a selection must reach the PTY as SIGINT"
        );
        assert!(app.captured_clipboard.as_ref().unwrap().is_empty());
    }

    /// Refusing to read the clipboard over SSH is the *designed* behaviour —
    /// the host's clipboard is not the user's — so it must read as a hint, not
    /// as a red failure banner on every paste, and must say which key does
    /// work instead.
    #[test]
    fn paste_refusal_over_ssh_is_an_info_hint_naming_the_terminals_key() {
        // A non-loopback SSH with no forwarded display: native is the host's.
        let _env = clipboard::scoped_env(&[
            ("SSH_TTY", Some("/dev/pts/0")),
            ("SSH_CONNECTION", Some("10.0.0.1 5555 10.0.0.2 22")),
            ("DISPLAY", None),
            ("WAYLAND_DISPLAY", None),
        ]);

        let mut app = app_with_sessions(1);
        app.paste_from_clipboard();

        let status = app.status_message.clone().expect("the refusal is surfaced");
        assert_eq!(
            status.level,
            StatusLevel::Info,
            "an expected, correct refusal is not an error: {}",
            status.text
        );
        // Both chords: over SSH the compile-time target is the host, not the
        // machine whose keyboard the user is on, so neither may be dropped.
        assert!(
            status.text.contains("Ctrl+Shift+V") && status.text.contains("Cmd+V"),
            "the hint names the terminal's own paste key: {}",
            status.text
        );
    }

    /// The other refusal: locally, but with no clipboard handle at all (no
    /// display server). Same reasoning as over SSH — an expected state, so a
    /// hint naming the key that works, not a red banner.
    #[test]
    fn paste_refusal_without_a_clipboard_handle_is_an_info_hint() {
        // Not over SSH, so the refusal comes from the missing handle below.
        let _env = clipboard::scoped_env(&[("SSH_TTY", None), ("SSH_CONNECTION", None)]);

        let mut app = app_with_sessions(1);
        app.clipboard = None;
        app.paste_from_clipboard();

        let status = app.status_message.clone().expect("the refusal is surfaced");
        assert_eq!(
            status.level,
            StatusLevel::Info,
            "having no clipboard to read is not a fault: {}",
            status.text
        );
        assert!(
            status.text.contains("No clipboard to read here")
                && status.text.contains("Ctrl+Shift+V"),
            "the hint says why and names the key that works: {}",
            status.text
        );
    }

    /// An `App` with one session, focused terminal, plus the receiving end of
    /// that session's PTY input channel — the seam for asserting exactly which
    /// bytes a key does (or doesn't) forward to the agent.
    fn app_with_pty_input_rx() -> (App, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );
        let (session, input_rx) = Session::stub_with_input_rx("s", &backend_arc, &provider);
        app.sessions.push(session);
        app.active_index = 0;
        app.focus = InputFocus::Terminal;
        (app, input_rx)
    }

    /// tmux's `send-prefix`: `<leader> <leader>` hands the leader's own bytes
    /// to the agent, without which the chord would be permanently unreachable
    /// by the CLI running inside the pane.
    #[test]
    fn leader_twice_sends_the_leader_byte_to_the_pty() {
        let (mut app, mut input_rx) = app_with_pty_input_rx();

        app.handle_key(KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(app.prefix_state.is_armed());
        app.handle_key(KeyCode::Char('f'), KeyModifiers::CONTROL);

        assert_eq!(
            input_rx.try_recv().ok(),
            Some(vec![0x06]),
            "the second leader press reaches the agent as Ctrl+F"
        );
        assert!(!app.prefix_state.is_armed(), "and it disarms");
    }

    /// A mistyped leader sequence must be swallowed, not injected: an unbound
    /// key after the leader reports instead of typing into the agent's prompt.
    #[test]
    fn leader_then_unbound_key_sends_nothing_to_the_pty() {
        use tokio::sync::mpsc::error::TryRecvError;
        let (mut app, mut input_rx) = app_with_pty_input_rx();

        app.handle_key(KeyCode::Char('f'), KeyModifiers::CONTROL);
        app.handle_key(KeyCode::Char('§'), KeyModifiers::NONE);

        assert!(
            matches!(input_rx.try_recv(), Err(TryRecvError::Empty)),
            "an unbound leader key must not reach the PTY"
        );
        assert!(!app.prefix_state.is_armed());
    }

    /// The point of `prefix-only`: a global chord no longer dispatches, so its
    /// bytes go where the agent CLI can use them.
    #[test]
    fn prefix_only_lets_a_blocked_global_chord_reach_the_pty() {
        let (mut app, mut input_rx) = app_with_pty_input_rx();
        app.prefix_settings.mode = crate::session::PrefixMode::PrefixOnly;

        app.handle_key(KeyCode::Char('b'), KeyModifiers::CONTROL);

        assert_eq!(
            input_rx.try_recv().ok(),
            Some(vec![0x02]),
            "Ctrl+B reaches the agent instead of toggling a panel"
        );
        assert!(!app.show_info_panel);
    }

    /// Inside a modal text input, readline `Ctrl+W` (delete word) and `Ctrl+U`
    /// (kill to line start) edit the text like a terminal — and never insert a
    /// literal `w`/`u`, nor fire the global `FocusTasks`/`OpenRestoreSessions`
    /// chords those keys carry.
    #[test]
    fn ctrl_w_and_ctrl_u_edit_modal_text_like_a_terminal() {
        let mut app = app_with_sessions(1);
        app.modal = modals::Modal::SessionName(modals::SessionNameModal::default());
        if let modals::Modal::SessionName(ref mut sn) = app.modal {
            sn.name.set("hello world");
        }

        // Ctrl+W deletes the word before the cursor.
        app.handle_key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        let modals::Modal::SessionName(ref sn) = app.modal else {
            panic!("modal must stay open");
        };
        assert_eq!(sn.name.value(), "hello ");

        // Ctrl+U clears to the start of the line.
        app.handle_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        let modals::Modal::SessionName(ref sn) = app.modal else {
            panic!("modal must stay open");
        };
        assert_eq!(sn.name.value(), "");
    }

    /// The same readline editing works in the other modal text inputs that
    /// share `apply_text_input_key` — here the worktree/branch-name field and
    /// the repo-palette input.
    #[test]
    fn ctrl_w_edits_worktree_name_and_repo_palette_input() {
        let mut app = app_with_sessions(1);

        app.modal = modals::Modal::WorktreeName(modals::WorktreeNameModal::default());
        if let modals::Modal::WorktreeName(ref mut wn) = app.modal {
            wn.name.set("feature branch");
        }
        app.handle_key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        let modals::Modal::WorktreeName(ref wn) = app.modal else {
            panic!("modal must stay open");
        };
        assert_eq!(wn.name.value(), "feature ");

        let mut rp = modals::RepoPickerModal::default();
        rp.input.set("foo bar");
        app.modal = modals::Modal::RepoPicker(rp);
        app.handle_key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        let modals::Modal::RepoPicker(ref rp) = app.modal else {
            panic!("modal must stay open");
        };
        assert_eq!(rp.input.value(), "foo ");
    }

    #[test]
    fn unbound_super_chord_skips_focus_letter_hotkeys() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::TaskList;
        // Cmd+N is unbound: it must be swallowed, not treated as the task
        // list's plain `n` (new task) hotkey.
        app.handle_key(KeyCode::Char('n'), KeyModifiers::SUPER);
        assert_eq!(app.focus, InputFocus::TaskList, "no editor must open");
    }

    #[test]
    fn scroll_clears_selection() {
        let mut app = app_with_sessions(1);
        app.text_selection = Some(Selection::new(
            TermPos { row: 0, col: 0 },
            PaneBounds::from_rect(ratatui::layout::Rect::new(0, 0, 80, 24)),
        ));

        app.scroll_terminal_up(1);
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn ctrl_d_deletes_session_from_session_list() {
        let mut app = app_with_sessions(2);
        app.focus = InputFocus::SessionList;
        let initial_count = app.sessions.len();
        app.handle_key(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(app.sessions.len() < initial_count);
    }

    #[test]
    fn ctrl_d_from_session_list_deletes_session() {
        let mut app = app_with_sessions(2);
        app.focus = InputFocus::SessionList;
        let initial_count = app.sessions.len();
        app.handle_key(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(app.sessions.len() < initial_count);
    }

    #[test]
    fn ctrl_r_no_crash_without_sessions() {
        let mut app = app_with_sessions(0);
        // App::new may toast warnings from the developer's real keybindings
        // file; this test only cares that Ctrl+R itself stays silent.
        app.status_message = None;
        app.focus = InputFocus::Terminal;
        // Should not crash when there are no sessions
        app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(app.status_message.is_none());
    }

    #[test]
    fn f1_shows_help_from_any_context() {
        let mut app = app_with_sessions(0);
        for focus in [InputFocus::SessionList, InputFocus::Terminal] {
            app.modal = modals::Modal::None;
            app.focus = focus;
            app.handle_key(KeyCode::F(1), KeyModifiers::NONE);
            assert!(
                matches!(app.modal, modals::Modal::Help(_)),
                "F1 should show help from {focus:?}"
            );
        }
    }

    #[test]
    fn f1_does_not_activate_during_modal() {
        let mut app = app_with_sessions(0);
        app.modal = modals::Modal::RepoPicker(modals::RepoPickerModal::default());
        app.handle_key(KeyCode::F(1), KeyModifiers::NONE);
        assert!(!matches!(app.modal, modals::Modal::Help(_)));
    }

    #[test]
    fn help_modal_navigation_clamps() {
        let mut app = app_with_sessions(0);
        app.modal = modals::Modal::Help(modals::HelpModal::default());

        // `k` at the top stays at index 0.
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        let modals::Modal::Help(ref h) = app.modal else {
            panic!("help modal closed unexpectedly");
        };
        assert_eq!(h.selected, 0);

        // `j` past the end clamps to the last rebindable action.
        let last = crate::session::Action::rebindable_in_order().len() - 1;
        for _ in 0..(last + 5) {
            app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        }
        let modals::Modal::Help(ref h) = app.modal else {
            panic!("help modal closed unexpectedly");
        };
        assert_eq!(h.selected, last);
    }

    #[test]
    fn help_capture_then_key_rebinds_and_clears_capturing() {
        let base = std::env::temp_dir().join("friring-help-rebind-test");
        let _ = std::fs::remove_dir_all(&base);
        let _g = crate::paths::TestPathGuard::new(&base);

        let mut app = app_with_sessions(0);
        app.handle_key(KeyCode::F(1), KeyModifiers::NONE); // open help (selected = 0)
        app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE); // begin capture
        app.handle_key(KeyCode::Char('a'), KeyModifiers::CONTROL); // bind ctrl+a (free)

        let action = crate::session::Action::rebindable_in_order()[0];
        assert_eq!(
            app.keybindings
                .lookup(KeyCode::Char('a'), KeyModifiers::CONTROL),
            Some(action)
        );
        let modals::Modal::Help(ref h) = app.modal else {
            panic!("help modal closed unexpectedly");
        };
        assert!(!h.capturing, "capture flag should clear after binding");
    }

    #[test]
    fn help_capture_esc_cancels_without_rebinding() {
        let mut app = app_with_sessions(0);
        app.handle_key(KeyCode::F(1), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE); // begin capture
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE); // cancel capture

        // Still in the help modal, no longer capturing, and nothing was bound.
        // Ctrl+A is the probe because it is unbound by default (Ctrl+X is now
        // ToggleReview's default chord).
        let modals::Modal::Help(ref h) = app.modal else {
            panic!("Esc during capture should not close the help modal");
        };
        assert!(!h.capturing);
        assert_eq!(
            app.keybindings
                .lookup(KeyCode::Char('a'), KeyModifiers::CONTROL),
            None
        );
    }

    #[test]
    fn help_capturing_ctrl_q_rebinds_not_quits() {
        let base = std::env::temp_dir().join("friring-help-ctrlq-test");
        let _ = std::fs::remove_dir_all(&base);
        let _g = crate::paths::TestPathGuard::new(&base);

        let mut app = app_with_sessions(0);
        app.handle_key(KeyCode::F(1), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE); // begin capture
        app.handle_key(KeyCode::Char('q'), KeyModifiers::CONTROL); // would normally quit

        assert!(!app.should_quit, "capturing ctrl+q must rebind, not quit");
        let action = crate::session::Action::rebindable_in_order()[0];
        assert_eq!(
            app.keybindings
                .lookup(KeyCode::Char('q'), KeyModifiers::CONTROL),
            Some(action)
        );
    }

    #[test]
    fn help_reset_d_restores_defaults() {
        let base = std::env::temp_dir().join("friring-help-reset-test");
        let _ = std::fs::remove_dir_all(&base);
        let _g = crate::paths::TestPathGuard::new(&base);

        let mut app = app_with_sessions(0);
        app.handle_key(KeyCode::F(1), KeyModifiers::NONE);
        let action = crate::session::Action::rebindable_in_order()[0];

        // Rebind the selected action to ctrl+x...
        app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(
            app.keybindings
                .lookup(KeyCode::Char('x'), KeyModifiers::CONTROL),
            Some(action)
        );

        // ...then `d` restores its compiled-in defaults.
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(
            app.keybindings.chords_for(action),
            action.default_chords().as_slice()
        );
    }

    #[test]
    fn help_reset_all_restores_every_default() {
        let base = std::env::temp_dir().join("friring-help-reset-all-test");
        let _ = std::fs::remove_dir_all(&base);
        let _g = crate::paths::TestPathGuard::new(&base);

        let mut app = app_with_sessions(0);
        app.handle_key(KeyCode::F(1), KeyModifiers::NONE);

        // Rebind two distinct actions away from their defaults.
        let actions = crate::session::Action::rebindable_in_order();
        app.keybindings
            .rebind(actions[0], crate::session::KeyChord::ctrl('x'));
        app.keybindings
            .rebind(actions[1], crate::session::KeyChord::ctrl('y'));

        // Shift+D resets everything.
        app.handle_key(KeyCode::Char('D'), KeyModifiers::SHIFT);

        for action in crate::session::Action::all() {
            assert_eq!(
                app.keybindings.chords_for(*action),
                action.default_chords().as_slice(),
                "{action:?} should be reset to defaults"
            );
        }
        // The override file is gone, so defaults stay authoritative.
        assert_eq!(
            crate::storage::keybindings::load_keybindings_json().unwrap(),
            None
        );
    }

    #[test]
    fn focus_key_context_maps_focus_to_scope() {
        use crate::session::KeyContext;
        let mut app = app_with_sessions(1);

        app.focus = InputFocus::SessionList;
        assert_eq!(app.focus_key_context(), KeyContext::SessionList);
        app.focus = InputFocus::Terminal;
        assert_eq!(app.focus_key_context(), KeyContext::Terminal);
        app.focus = InputFocus::FileViewer;
        assert_eq!(app.focus_key_context(), KeyContext::FileViewer);

        // While the file-viewer search field is active, fall back to Global so
        // typed letters edit the query instead of navigating the tree.
        app.file_viewer.search_active = true;
        assert_eq!(app.focus_key_context(), KeyContext::Global);
        app.file_viewer.search_active = false;

        // The automations and tasks panes are their own scoped contexts, so
        // single letters (j/k/n/r/d/…) resolve there without leaking to the PTY.
        app.focus = InputFocus::Automations;
        assert_eq!(app.focus_key_context(), KeyContext::Automations);
        app.focus = InputFocus::TaskList;
        assert_eq!(app.focus_key_context(), KeyContext::Tasks);

        // The in-pane editors / run-history are capture sub-modes handled before
        // the lookup, so they stay on Global.
        app.focus = InputFocus::AutomationEditor;
        assert_eq!(app.focus_key_context(), KeyContext::Global);
        app.focus = InputFocus::TaskEditor;
        assert_eq!(app.focus_key_context(), KeyContext::Global);
    }

    #[test]
    fn file_viewer_scoped_keys_route_through_keybindings() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::FileViewer;
        assert!(!app.file_viewer.search_active);

        // `/` is the rebindable FileViewerSearch action.
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(app.file_viewer.search_active, "'/' should start the search");

        // Now in search mode the context falls back to Global, so plain letters
        // edit the query (routed to the literal search handler) rather than
        // triggering FileViewerDown etc.
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(app.file_viewer.search_query, "ab");
    }

    #[test]
    fn file_viewer_search_action_is_rebindable() {
        let base = std::env::temp_dir().join("friring-fv-rebind-test");
        let _ = std::fs::remove_dir_all(&base);
        let _g = crate::paths::TestPathGuard::new(&base);

        let mut app = app_with_sessions(1);
        // Rebind FileViewerSearch from `/` to `s`.
        app.keybindings.rebind(
            crate::session::Action::FileViewerSearch,
            crate::session::KeyChord::plain('s'),
        );
        app.focus = InputFocus::FileViewer;

        // `/` no longer opens search...
        app.handle_key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(!app.file_viewer.search_active);
        // ...the new `s` chord does.
        app.handle_key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(app.file_viewer.search_active);
    }

    #[test]
    fn f2_toggles_info_panel() {
        let mut app = app_with_sessions(0);
        assert!(!app.show_info_panel);
        app.handle_key(KeyCode::F(2), KeyModifiers::NONE);
        assert!(app.show_info_panel);
        app.handle_key(KeyCode::F(2), KeyModifiers::NONE);
        assert!(!app.show_info_panel);
    }

    #[test]
    fn f5_toggles_tasks_panel() {
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));
        assert!(!app.show_tasks_panel);
        // F5 shows + focuses the tasks panel (like F3 for the file viewer).
        app.handle_key(KeyCode::F(5), KeyModifiers::NONE);
        assert!(app.show_tasks_panel);
        assert_eq!(app.focus, InputFocus::TaskList);
        // F5 again hides it and drops focus back to the session list.
        app.handle_key(KeyCode::F(5), KeyModifiers::NONE);
        assert!(!app.show_tasks_panel);
        assert_eq!(app.focus, InputFocus::SessionList);
    }

    /// The theme picker's own opener chord (`F4`/`Ctrl+Y`) closes it when it's
    /// already open, restoring the live-preview original like `Esc`.
    #[test]
    fn f4_toggles_theme_picker_closed() {
        let mut app = app_with_sessions(1);
        app.handle_key(KeyCode::F(4), KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::ThemePicker(_)));
        // Re-pressing the opener dismisses it instead of being swallowed.
        app.handle_key(KeyCode::F(4), KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
    }

    /// The Settings panel's own opener chord (`F6`/`Ctrl+,`) closes it when it's
    /// already open (discarding the draft, like `Esc`).
    #[test]
    fn f6_toggles_settings_closed() {
        let mut app = app_with_sessions(1);
        app.handle_key(KeyCode::F(6), KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::Settings(_)));
        app.handle_key(KeyCode::F(6), KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
    }

    /// When the terminal is focused, readline/shell `Ctrl+<letter>` chords
    /// (here `Ctrl+W` = delete-word) defer to the PTY instead of running their
    /// friring command — but the same chord still works from the session list,
    /// and the `F`-key alternate works everywhere.
    #[test]
    fn terminal_focus_defers_readline_ctrl_chords_to_pty() {
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));

        // From the session list, Ctrl+W (FocusTasks) toggles the tasks panel.
        app.focus = InputFocus::SessionList;
        app.handle_key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert!(app.show_tasks_panel, "Ctrl+W toggles tasks from the list");

        // Reset, then focus the terminal: Ctrl+W now forwards to the PTY, so
        // the tasks panel is left untouched.
        app.show_tasks_panel = false;
        app.focus = InputFocus::Terminal;
        app.handle_key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert!(
            !app.show_tasks_panel,
            "Ctrl+W should defer to the PTY when the terminal is focused"
        );
        assert_eq!(app.focus, InputFocus::Terminal);

        // The F5 alternate is not a Ctrl+letter chord, so it still toggles.
        app.handle_key(KeyCode::F(5), KeyModifiers::NONE);
        assert!(
            app.show_tasks_panel,
            "F5 keeps toggling tasks even in the terminal"
        );
    }

    /// Navigation chords are the keyboard escape route, so they keep working in
    /// the terminal even though `Ctrl+H` collides with readline's backspace.
    #[test]
    fn terminal_focus_keeps_navigation_chords_active() {
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));
        app.focus = InputFocus::Terminal;

        app.handle_key(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_ne!(
            app.focus,
            InputFocus::Terminal,
            "Ctrl+H (FocusBackward) must still leave the terminal"
        );
    }

    /// Every `[features]` flag blocks its keybinding with a toast: state stays
    /// untouched and the chord is consumed (never forwarded to the PTY).
    #[test]
    fn disabled_features_block_actions_with_a_toast() {
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));
        app.features = crate::session::settings::FeatureFlags {
            tasks: false,
            automations: false,
            file_viewer: false,
            global_search: false,
            double_shift_search: false,
            info_panel: false,
            shell_pane: false,
            code_review: false,
            cc_activity: false,
            session_memory: false,
            perf_hud: false,
            mouse: true,
            notifications: false,
            soft_delete: true,
            version_check: false,
            auto_update: false,
        };

        app.handle_key(KeyCode::F(5), KeyModifiers::NONE);
        assert!(!app.show_tasks_panel);
        assert_eq!(app.focus, InputFocus::SessionList);
        assert!(app.status_message.take().unwrap().text.contains("disabled"));

        app.handle_key(KeyCode::F(3), KeyModifiers::NONE);
        assert!(!app.show_file_viewer);
        assert!(app.status_message.take().unwrap().text.contains("disabled"));

        app.handle_key(KeyCode::F(2), KeyModifiers::NONE);
        assert!(!app.show_info_panel);
        assert!(app.status_message.take().unwrap().text.contains("disabled"));

        app.handle_key(KeyCode::Char('/'), KeyModifiers::CONTROL);
        assert!(!app.global_search.active);
        assert!(app.status_message.take().unwrap().text.contains("disabled"));

        app.handle_key(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(matches!(app.modal, modals::Modal::None));
        assert_eq!(app.focus, InputFocus::SessionList);
        assert!(app.status_message.take().unwrap().text.contains("disabled"));

        app.handle_key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(app.active_terminal_view(), TerminalView::Claude);
        assert!(app.status_message.take().unwrap().text.contains("disabled"));

        // F7 (ToggleReview) is gated by the code_review flag.
        app.handle_key(KeyCode::F(7), KeyModifiers::NONE);
        assert!(app.active_review().is_none());
        assert_ne!(app.focus, InputFocus::CodeReview);
        assert!(app.status_message.take().unwrap().text.contains("disabled"));

        // F9 (ToggleCcActivity) is gated by the cc_activity flag.
        app.handle_key(KeyCode::F(9), KeyModifiers::NONE);
        assert!(app.active_cc_activity().is_none());
        assert_ne!(app.focus, InputFocus::CcActivity);
        assert!(app.status_message.take().unwrap().text.contains("disabled"));
    }

    /// The automations flag flows through `screen_layout` (the shared layout
    /// funnel): disabling it removes the pane and gives the session list the
    /// whole left column.
    #[test]
    fn screen_layout_drops_automations_pane_when_disabled() {
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(100, 30));
        let with = app.screen_layout();
        assert!(with.automations_panel.is_some());

        app.features.automations = false;
        let without = app.screen_layout();
        assert!(without.automations_panel.is_none());
        assert_eq!(
            without.left_panel.unwrap().height,
            with.left_panel.unwrap().height + with.automations_panel.unwrap().height,
            "session list absorbs the pane's rows"
        );
    }

    /// With automations disabled there is no pane beneath the session list, so
    /// `j`/`k` wrap within the list instead of flowing into the pane.
    #[test]
    fn session_list_wraps_when_automations_disabled() {
        let mut app = app_with_sessions(2);
        app.features.automations = false;
        app.focus = InputFocus::SessionList;
        app.active_index = 1; // last session in render order

        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::SessionList, "no pane to flow into");
        assert_eq!(app.active_index, 0, "j past the last wraps to the first");

        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::SessionList);
        assert_eq!(app.active_index, 1, "k above the first wraps to the last");
    }

    #[test]
    fn opening_tasks_panel_populates_central_preview() {
        // Focusing the tasks panel (F5/Ctrl+W) must build the central-pane
        // preview for the selected task, not leave the empty hint showing.
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));
        app.db
            .create_task(&crate::storage::tasks::NewTask::local("only task"))
            .unwrap();
        app.refresh_tasks();

        app.handle_key(KeyCode::F(5), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::TaskList);
        let editor = app
            .task_ui
            .task_editor
            .as_ref()
            .expect("the central pane must mirror the selected task");
        assert_eq!(editor.title.value(), "only task");
    }

    fn session_parser_size(app: &App, index: usize) -> (u16, u16) {
        let parser = app.sessions[index].parser.lock().unwrap();
        parser.screen().size()
    }

    #[test]
    fn f3_toggle_resizes_session_parser() {
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));
        let before = session_parser_size(&app, 0);

        app.handle_key(KeyCode::F(3), KeyModifiers::NONE);
        let after_open = session_parser_size(&app, 0);
        assert!(app.show_file_viewer);
        assert!(
            after_open.1 < before.1,
            "terminal width must shrink when file viewer opens: before={before:?}, after={after_open:?}",
        );

        app.handle_key(KeyCode::F(3), KeyModifiers::NONE);
        let after_close = session_parser_size(&app, 0);
        assert!(!app.show_file_viewer);
        assert_eq!(
            after_close, before,
            "terminal size must return to original after file viewer closes",
        );
    }

    #[test]
    fn f2_toggle_resizes_session_parser() {
        let mut app = app_with_sessions(1);
        // Pin the classic column dock — `auto` would inline at this size and
        // deliberately leave the terminal width alone.
        app.info_panel_position = crate::session::settings::InfoPanelPosition::Column;
        app.update(AppMessage::Resize(160, 40));
        let before = session_parser_size(&app, 0);

        app.handle_key(KeyCode::F(2), KeyModifiers::NONE);
        let after = session_parser_size(&app, 0);
        assert!(app.show_info_panel);
        assert!(
            after.1 < before.1,
            "terminal width must shrink when info panel opens: before={before:?}, after={after:?}",
        );
    }

    #[test]
    fn f2_auto_position_inlines_under_sessions_when_it_fits() {
        // Default `auto`: at 160×40 the left column holds the full session
        // list plus the full info content, so F2 docks the pane inline and the
        // terminal keeps its width instead of losing the dedicated column.
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));
        let before = session_parser_size(&app, 0);

        app.handle_key(KeyCode::F(2), KeyModifiers::NONE);
        assert!(app.show_info_panel);
        assert_eq!(
            session_parser_size(&app, 0),
            before,
            "inline dock must not carve a column off the terminal"
        );

        let areas = app.screen_layout();
        let sessions = areas.left_panel.unwrap();
        let info = areas.info_panel.expect("info pane inlined");
        assert_eq!(info.x, sessions.x, "docked in the left column");
        assert!(info.y > sessions.y, "docked below the session list");
    }

    #[test]
    fn narrow_resize_collapses_info_panel_only_when_pinned_to_column() {
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));
        app.handle_key(KeyCode::F(2), KeyModifiers::NONE);
        assert!(app.show_info_panel);

        // `auto`: the pane lives in the left column, which narrow terminals
        // still show — the toggle survives dropping below 120 cols.
        app.update(AppMessage::Resize(100, 40));
        assert!(app.show_info_panel, "auto dock survives < 120 cols");

        // `column`: the dedicated column can't render below 120 → collapse.
        app.info_panel_position = crate::session::settings::InfoPanelPosition::Column;
        app.update(AppMessage::Resize(90, 40));
        assert!(!app.show_info_panel);
    }

    #[test]
    fn apply_live_settings_updates_info_panel_position() {
        let mut app = app_with_sessions(0);
        let mut settings = crate::session::settings::Settings::default();
        settings.info_panel_position = crate::session::settings::InfoPanelPosition::Inline;
        app.apply_live_settings(&settings);
        assert_eq!(
            app.info_panel_position,
            crate::session::settings::InfoPanelPosition::Inline
        );
    }

    #[test]
    fn auto_dock_flip_repushes_pty_sizes_via_tick_drift_check() {
        // Growing info content can flip the `auto` dock inline → column with
        // no resize event; the tick's drift check must re-push PTY sizes.
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));
        app.handle_key(KeyCode::F(2), KeyModifiers::NONE);
        let inline_width = session_parser_size(&app, 0).1;

        // 30 scheduled automations blow the info content (one row each) past
        // the left column's height → the pane falls back to the column.
        let now = crate::sync::current_time_millis();
        app.automation_ui.cached_automations = (0..30)
            .map(|i| crate::session::Automation {
                id: i,
                name: format!("auto-{i}"),
                enabled: true,
                schedule: crate::session::AutomationSchedule::Once { at: 0 },
                timezone: None,
                action: crate::session::AutomationAction::send_to(SessionId::default()),
                prompt: "p".into(),
                created_at: 0,
                updated_at: 0,
                last_run_at: None,
                next_run_at: Some(now + 3_600_000),
                prompt_steps: Vec::new(),
            })
            .collect();

        app.sync_content_size();
        assert!(
            session_parser_size(&app, 0).1 < inline_width,
            "flip to the column must shrink the pushed PTY width"
        );
    }

    #[test]
    fn sync_content_size_is_gated_to_auto_with_panel_open() {
        // The drift check runs on every unthrottled tick, so it must short-
        // circuit unless the `auto` dock (the only content-driven terminal
        // resize) is actually in play. Stale `last_content_size` + a size that
        // would differ must NOT trigger a re-push when the panel is closed or
        // the position is pinned.
        let mut app = app_with_sessions(1);
        app.update(AppMessage::Resize(160, 40));

        // Panel closed: gated out even though last_content_size is stale.
        app.last_content_size = Some((1, 1));
        app.sync_content_size();
        assert_eq!(
            app.last_content_size,
            Some((1, 1)),
            "closed panel must skip the measure entirely"
        );

        // Panel open but pinned to the column: still gated out (column never
        // resizes the terminal from content).
        app.info_panel_position = crate::session::settings::InfoPanelPosition::Column;
        app.show_info_panel = true;
        app.last_content_size = Some((1, 1));
        app.sync_content_size();
        assert_eq!(
            app.last_content_size,
            Some((1, 1)),
            "column mode must skip the measure entirely"
        );

        // Auto + open: the gate opens and the stale size is reconciled.
        app.info_panel_position = crate::session::settings::InfoPanelPosition::Auto;
        app.sync_content_size();
        assert_ne!(
            app.last_content_size,
            Some((1, 1)),
            "auto + open must run the drift check"
        );
    }

    #[test]
    fn ctrl_l_cycles_focus() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::SessionList;
        // SessionList → Terminal → SessionList (no file viewer). The automations
        // pane is not a cycle stop — it's reached via j/k from the list.
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::Terminal);
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::SessionList);
    }

    #[test]
    fn ctrl_l_includes_tasks_panel_when_visible() {
        let mut app = app_with_sessions(1);
        // With the tasks panel showing, the cycle is
        // SessionList → Terminal → TaskList → SessionList (no file viewer).
        app.show_tasks_panel = true;
        app.focus = InputFocus::SessionList;
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::Terminal);
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::TaskList);
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::SessionList);
        // Ctrl+H from the tasks panel steps back to the terminal.
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL); // → Terminal
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL); // → TaskList
        assert_eq!(app.focus, InputFocus::TaskList);
        app.handle_key(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::Terminal);
    }

    #[test]
    fn ctrl_l_skips_tasks_panel_when_hidden() {
        let mut app = app_with_sessions(1);
        // Panel off → TaskList is not a cycle stop.
        app.show_tasks_panel = false;
        app.focus = InputFocus::Terminal;
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::SessionList);
    }

    // --- In-pane task editing workflow ---

    #[test]
    fn task_n_opens_central_pane_editor() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::TaskList;
        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        // n starts a new task in the central-pane editor (not a modal).
        assert_eq!(app.focus, InputFocus::TaskEditor);
        assert!(app.task_ui.task_editor.is_some());
        assert!(matches!(app.modal, modals::Modal::None));
    }

    #[test]
    fn task_enter_edits_existing_in_pane_and_esc_returns() {
        let mut app = app_with_sessions(1);
        app.db
            .create_task(&crate::storage::tasks::NewTask::local("fix bug"))
            .unwrap();
        app.refresh_tasks();
        app.focus = InputFocus::TaskList;
        app.task_ui.task_panel_index = 0;
        // Enter opens the editor in the central pane for the selected task.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::TaskEditor);
        assert!(app
            .task_ui
            .task_editor
            .as_ref()
            .unwrap()
            .editing_id
            .is_some());
        // Esc discards and returns to the tasks panel.
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::TaskList);
    }

    #[test]
    fn task_editor_save_persists_and_returns_to_panel() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::TaskList;
        // New task → type a title → Enter saves.
        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        for c in "ship it".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::TaskList);
        // The task was persisted.
        let tasks = app.db.list_tasks().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "ship it");
    }

    #[test]
    fn task_editor_save_creates_with_no_action_and_preserves_on_edit() {
        let mut app = app_with_sessions(1);
        // Create via the editor → action is None (the editor no longer authors it).
        app.focus = InputFocus::TaskList;
        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        for c in "do thing".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let id = app.db.list_tasks().unwrap()[0].id;
        assert!(app.db.get_task(id).unwrap().unwrap().action.is_none());

        // Give it an action out-of-band (as the CLI would), then edit the title
        // through the editor: the action must survive.
        let mut t = app.db.get_task(id).unwrap().unwrap();
        t.action = Some(AutomationAction::send_to(app.sessions[0].info.id));
        app.db.update_task(&t).unwrap();
        app.refresh_tasks();

        app.task_ui.task_panel_index = 0;
        app.enter_task_editor();
        // Append to the title and save.
        app.handle_key(KeyCode::End, KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('!'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        let saved = app.db.get_task(id).unwrap().unwrap();
        assert_eq!(saved.title, "do thing!");
        assert!(
            matches!(saved.action, Some(AutomationAction::Send { .. })),
            "edit must preserve the out-of-band action"
        );
    }

    #[test]
    fn task_action_picker_lists_send_per_session_plus_spawn() {
        let mut app = app_with_sessions(2);
        let id = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("t"))
            .unwrap();
        app.refresh_tasks();
        let task = app.db.get_task(id).unwrap().unwrap();
        app.open_task_action_picker(&task);
        let modals::Modal::TaskActionPicker(ref p) = app.modal else {
            panic!("expected the action picker");
        };
        // Two running sessions → two Send entries + a trailing SpawnNew.
        assert_eq!(p.choices.len(), 3);
        assert!(matches!(p.choices[2], modals::TaskActionChoice::SpawnNew));
        assert!(
            p.choices
                .iter()
                .filter(|c| matches!(c, modals::TaskActionChoice::Send(..)))
                .count()
                == 2
        );
    }

    #[test]
    fn task_r_key_opens_picker_then_enter_sends_and_closes() {
        // End-to-end through the key handlers: `r` opens the picker, `Enter`
        // runs the highlighted Send choice, closing the modal and advancing the
        // task. (One session → first choice is Send.)
        let mut app = app_with_sessions(1);
        let id = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("t"))
            .unwrap();
        app.refresh_tasks();
        app.focus = InputFocus::TaskList;
        app.task_ui.task_panel_index = 0;

        app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(
            matches!(app.modal, modals::Modal::TaskActionPicker(_)),
            "r opens the action picker"
        );

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None), "Enter closes it");
        assert_eq!(
            app.db.get_task(id).unwrap().unwrap().status,
            crate::session::TaskStatus::InProgress
        );
    }

    #[test]
    fn task_action_picker_esc_closes_without_running() {
        let mut app = app_with_sessions(1);
        let id = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("t"))
            .unwrap();
        app.refresh_tasks();
        let task = app.db.get_task(id).unwrap().unwrap();
        app.open_task_action_picker(&task);
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
        // Status is untouched.
        assert_eq!(
            app.db.get_task(id).unwrap().unwrap().status,
            crate::session::TaskStatus::Todo
        );
    }

    #[test]
    fn send_task_to_session_advances_to_in_progress() {
        let mut app = app_with_sessions(1);
        let id = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("t"))
            .unwrap();
        app.refresh_tasks();
        let sid = app.sessions[0].info.id;
        app.send_task_to_session(id, "t", crate::session::TaskStatus::Todo, sid);
        assert_eq!(
            app.db.get_task(id).unwrap().unwrap().status,
            crate::session::TaskStatus::InProgress
        );
    }

    #[test]
    fn task_related_sessions_match_spawn_name_and_send_target() {
        let mut app = app_with_sessions(2);
        let id = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("t"))
            .unwrap();
        app.refresh_tasks();

        // No related session yet (generic stub names).
        let task = app.db.get_task(id).unwrap().unwrap();
        assert!(app.task_related_session_indices(&task).is_empty());

        // Rename session 1 to the spawn convention → it's related. Both the
        // current slugged form and the legacy bare `task-<id>` must match.
        app.sessions[1].info.name = task.spawn_session_name();
        assert_eq!(app.task_related_session_indices(&task), vec![1]);
        app.sessions[1].info.name = format!("task-{id}");
        assert_eq!(app.task_related_session_indices(&task), vec![1]);
    }

    #[test]
    fn task_related_sessions_honor_in_memory_link() {
        // A TUI spawn names the session by the user's choice (not `task-<id>`),
        // so the relation is recovered via `task_session_links`.
        let mut app = app_with_sessions(2);
        let id = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("t"))
            .unwrap();
        app.refresh_tasks();
        let task = app.db.get_task(id).unwrap().unwrap();

        // Session 0 keeps its generic stub name — no name/action match.
        assert!(app.task_related_session_indices(&task).is_empty());

        // Record the link the spawn tail would set, then it resolves.
        let sid = app.sessions[0].info.id;
        app.task_ui.task_session_links.insert(id, sid);
        assert_eq!(app.task_related_session_indices(&task), vec![0]);
    }

    #[test]
    fn open_task_related_session_focuses_terminal() {
        let mut app = app_with_sessions(2);
        let id = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("t"))
            .unwrap();
        app.sessions[1].info.name = format!("task-{id}");
        app.refresh_tasks();
        app.focus = InputFocus::TaskList;
        app.task_ui.task_panel_index = 0;

        app.handle_key(KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(app.active_index, 1, "jumps to the related session");
        assert_eq!(app.focus, InputFocus::Terminal);
    }

    #[test]
    fn open_task_related_session_no_session_keeps_focus() {
        let mut app = app_with_sessions(1);
        let _id = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("t"))
            .unwrap();
        app.refresh_tasks();
        app.focus = InputFocus::TaskList;
        app.task_ui.task_panel_index = 0;

        app.handle_key(KeyCode::Char('o'), KeyModifiers::NONE);
        // Nothing related is open → stay in the panel.
        assert_eq!(app.focus, InputFocus::TaskList);
    }

    #[test]
    fn scroll_task_preview_clamps_to_content() {
        let mut app = app_with_sessions(1);
        let id = app
            .db
            .create_task(&crate::storage::tasks::NewTask {
                description: Some("a\nb\nc".into()),
                ..crate::storage::tasks::NewTask::local("t")
            })
            .unwrap();
        let _ = id;
        app.refresh_tasks();
        app.focus = InputFocus::TaskList;
        app.task_ui.task_panel_index = 0;
        // Over-scroll up is clamped to 0.
        app.scroll_task_preview(-5);
        assert_eq!(app.task_ui.task_preview_scroll, 0);
        // Over-scroll down is clamped to the rendered line count.
        app.scroll_task_preview(1000);
        assert!(app.task_ui.task_preview_scroll <= 3);
    }

    #[test]
    fn apply_scrollbar_position_task_preview_clamps() {
        let mut app = app_with_sessions(1);
        app.db
            .create_task(&crate::storage::tasks::NewTask {
                description: Some("a\nb\nc".into()),
                ..crate::storage::tasks::NewTask::local("t")
            })
            .unwrap();
        app.refresh_tasks();
        app.focus = InputFocus::TaskList;
        app.task_ui.task_panel_index = 0;

        let max = app.task_preview_max_scroll();
        // A position past the end clamps to the max.
        app.apply_scrollbar_position(ScrollTarget::TaskPreview, 1000, 1000);
        assert_eq!(app.task_ui.task_preview_scroll, max);
        // Position 0 returns to the top.
        app.apply_scrollbar_position(ScrollTarget::TaskPreview, 0, 1000);
        assert_eq!(app.task_ui.task_preview_scroll, 0);
    }

    #[test]
    fn apply_scrollbar_position_terminal_inverts() {
        let mut app = app_with_sessions(1);
        // The stub parser keeps zero scrollback; swap in one that retains it.
        app.sessions[0].parser = Arc::new(std::sync::Mutex::new(
            vt100::Parser::new_with_callbacks(24, 80, 100, crate::agent::TermSignals::default()),
        ));
        // Seed the active session's parser with scrollback content.
        app.with_active_parser(|p| {
            for i in 0..50 {
                p.process(format!("line {i}\r\n").as_bytes());
            }
        });
        // Probe the total scrollback (the scrollbar's `content_len`).
        let mut total = 0usize;
        app.with_active_parser(|p| {
            let saved = p.screen().scrollback();
            p.screen_mut().set_scrollback(usize::MAX);
            total = p.screen().scrollback();
            p.screen_mut().set_scrollback(saved);
        });
        assert!(total > 0, "expected scrollback content");

        // Thumb at the top (pos 0) → fully scrolled up (scrollback == total).
        app.apply_scrollbar_position(ScrollTarget::Terminal, 0, total);
        let mut at_top = 0usize;
        app.with_active_parser(|p| at_top = p.screen().scrollback());
        assert_eq!(at_top, total);

        // Thumb at the bottom (pos == total) → back to the live tail (0).
        app.apply_scrollbar_position(ScrollTarget::Terminal, total, total);
        let mut at_bottom = 1usize;
        app.with_active_parser(|p| at_bottom = p.screen().scrollback());
        assert_eq!(at_bottom, 0);
    }

    #[test]
    fn scrollbar_click_starts_drag_not_selection() {
        let mut app = app_with_sessions(1);
        // Record a scrollbar track at a known location (as `view()` would).
        let track = Rect::new(40, 5, 1, 10);
        app.scrollbar_hits.push(ScrollbarHit {
            geom: ScrollbarGeom {
                track,
                content_len: 100,
                viewport: 10,
            },
            target: ScrollTarget::Terminal,
        });

        // A click on the track grabs the thumb — no text selection starts.
        app.handle_mouse_click(40, 7, KeyModifiers::NONE);
        assert_eq!(app.dragging_scrollbar, Some(ScrollTarget::Terminal));
        assert!(app.text_selection.is_none());

        // Mouse-up ends the drag.
        app.handle_mouse_up(40, 7);
        assert!(app.dragging_scrollbar.is_none());
    }

    #[test]
    fn pane_at_central_pane_follows_focus() {
        let mut app = app_with_sessions(1);
        // Pick a point guaranteed to be inside the central (terminal) pane.
        let areas = app.screen_layout();
        let cx = areas.terminal.x + areas.terminal.width / 2;
        let cy = areas.terminal.y + areas.terminal.height / 2;

        // The same central-pane point routes the wheel by focus.
        app.focus = InputFocus::Terminal;
        assert_eq!(app.pane_at(cx, cy), Some(ScrollPane::Terminal));

        app.focus = InputFocus::TaskList;
        assert_eq!(app.pane_at(cx, cy), Some(ScrollPane::TaskPreview));

        app.focus = InputFocus::AutomationRunHistory;
        assert_eq!(app.pane_at(cx, cy), Some(ScrollPane::RunHistory));
    }

    #[test]
    fn click_outside_scrollbar_starts_selection() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::Terminal;
        app.scrollbar_hits.push(ScrollbarHit {
            geom: ScrollbarGeom {
                track: Rect::new(118, 1, 1, 20),
                content_len: 100,
                viewport: 10,
            },
            target: ScrollTarget::Terminal,
        });

        // A click well away from the track falls through to text selection.
        app.handle_mouse_click(10, 10, KeyModifiers::NONE);
        assert!(app.dragging_scrollbar.is_none());
        assert!(app.text_selection.is_some());
    }

    // --- Mouse click targets (click-to-select/focus + modal rows) ---

    #[test]
    fn click_session_row_selects_and_focuses_terminal() {
        let mut app = app_with_sessions(3);
        app.focus = InputFocus::SessionList;
        // As recorded by view(): a row hitbox inside the left panel.
        app.click_targets.push(ClickTarget {
            rect: Rect::new(1, 3, 20, 1),
            action: ClickAction::SelectSession(2),
        });

        app.handle_mouse_click(5, 3, KeyModifiers::NONE);

        let order = app.render_order_indices();
        assert_eq!(app.active_index, order[2]);
        // Clicking a row is activation: focus lands in the terminal (like
        // Enter), so typing right after the click reaches the agent.
        assert_eq!(app.focus, InputFocus::Terminal);
        // The same press still arms drag-select inside the left panel.
        assert!(app.text_selection.is_some());
    }

    #[test]
    fn esc_in_session_list_returns_to_terminal() {
        let mut app = app_with_sessions(2);
        app.focus = InputFocus::SessionList;
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::Terminal);
    }

    #[test]
    fn esc_in_empty_session_list_stays_put() {
        let mut app = app_with_sessions(0);
        app.focus = InputFocus::SessionList;
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::SessionList);
    }

    #[test]
    fn startup_focus_lands_in_terminal_with_sessions() {
        let mut app = app_with_sessions(2);
        app.apply_startup_focus();
        assert_eq!(app.focus, InputFocus::Terminal);
    }

    #[test]
    fn startup_focus_stays_on_list_without_sessions() {
        let mut app = app_with_sessions(0);
        app.apply_startup_focus();
        assert_eq!(app.focus, InputFocus::SessionList);
    }

    #[test]
    fn click_terminal_pane_focuses_terminal_and_arms_selection() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::SessionList;
        let areas = app.screen_layout();
        app.click_targets.push(ClickTarget {
            rect: areas.terminal,
            action: ClickAction::FocusPane(InputFocus::Terminal),
        });
        let cx = areas.terminal.x + areas.terminal.width / 2;
        let cy = areas.terminal.y + areas.terminal.height / 2;

        app.handle_mouse_click(cx, cy, KeyModifiers::NONE);

        assert_eq!(app.focus, InputFocus::Terminal);
        assert!(app.text_selection.is_some());
    }

    #[test]
    fn click_with_modal_open_is_swallowed() {
        let mut app = app_with_sessions(1);
        app.modal = modals::Modal::ThemePicker(theme_picker_at(0));
        // A scrollbar track and a pane target beneath the overlay must both
        // be unreachable while the modal is open.
        app.scrollbar_hits.push(ScrollbarHit {
            geom: ScrollbarGeom {
                track: Rect::new(40, 5, 1, 10),
                content_len: 100,
                viewport: 10,
            },
            target: ScrollTarget::Terminal,
        });
        app.click_targets.push(ClickTarget {
            rect: Rect::new(0, 0, 120, 24),
            action: ClickAction::FocusPane(InputFocus::Terminal),
        });

        app.handle_mouse_click(40, 7, KeyModifiers::NONE);

        assert!(app.dragging_scrollbar.is_none());
        assert!(app.text_selection.is_none());
        assert!(matches!(app.modal, modals::Modal::ThemePicker(_)));
    }

    #[test]
    fn click_theme_picker_row_confirms_theme() {
        let mut app = app_with_sessions(1);
        app.modal = modals::Modal::ThemePicker(theme_picker_at(2));
        // As in a real frame: the pane targets beneath the overlay are
        // recorded first and overlap the modal — the modal row must still
        // win while a modal is open.
        app.click_targets.push(ClickTarget {
            rect: Rect::new(0, 0, 120, 24),
            action: ClickAction::FocusPane(InputFocus::Terminal),
        });
        app.click_targets.push(ClickTarget {
            rect: Rect::new(30, 8, 20, 1),
            action: ClickAction::ModalRow(0),
        });

        // Single click selects the row and confirms it (Enter-equivalent).
        app.handle_mouse_click(35, 8, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::None));
        assert_eq!(
            app.active_theme.name,
            crate::ui::theme::all_theme_entries()[0].name
        );
    }

    #[test]
    fn click_repo_picker_row_toggles_not_confirms() {
        let mut app = app_with_sessions(0);
        app.start_new_session(); // no hosts → opens the repo picker
        let modals::Modal::RepoPicker(ref mut rp) = app.modal else {
            panic!("expected repo picker");
        };
        // Seed two plain bookmarks (dropping the pinned helper rows the fresh
        // open added, so row indices are deterministic).
        rp.rows.clear();
        rp.push_row("/tmp/a".into(), modals::RepoRowKind::Repo { child: false });
        rp.push_row("/tmp/b".into(), modals::RepoRowKind::Repo { child: false });
        rp.filtered_indices = vec![0, 1];
        app.click_targets.push(ClickTarget {
            rect: Rect::new(30, 9, 40, 1),
            action: ClickAction::ModalRow(1),
        });

        app.handle_mouse_click(35, 9, KeyModifiers::NONE);

        // The click toggled the row's checkbox (Ctrl+Space), not Enter: the
        // modal stays open and nothing was spawned.
        let modals::Modal::RepoPicker(ref rp) = app.modal else {
            panic!("repo picker must stay open after a row click");
        };
        assert_eq!(rp.list_index, 1);
        assert!(rp.selected.contains(std::path::Path::new("/tmp/b")));
    }

    /// Open the palette (no hosts) and replace its rows with deterministic
    /// repos + the pinned "start here" row, dropping any machine-dependent
    /// first-run import suggestions.
    fn seeded_repo_picker(app: &mut App, repos: &[&str]) {
        app.start_new_session();
        let modals::Modal::RepoPicker(ref mut rp) = app.modal else {
            panic!("expected repo picker");
        };
        rp.rows.clear();
        for r in repos {
            rp.push_row((*r).into(), modals::RepoRowKind::Repo { child: false });
        }
        rp.push_row(std::path::PathBuf::new(), modals::RepoRowKind::StartHere);
        rp.recompute_filter();
    }

    fn picker_state(app: &App) -> &modals::RepoPickerModal {
        let modals::Modal::RepoPicker(ref rp) = app.modal else {
            panic!("expected the repo picker to stay open");
        };
        rp
    }

    #[test]
    fn repo_picker_space_toggles_only_when_input_empty() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &["/tmp/a"]);

        app.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(picker_state(&app)
            .selected
            .contains(std::path::Path::new("/tmp/a")));

        // Once a filter is typed, Space types a literal space instead.
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        let rp = picker_state(&app);
        assert_eq!(rp.input.value(), "a ");
        assert!(
            rp.selected.contains(std::path::Path::new("/tmp/a")),
            "typed space must not have re-toggled the row"
        );
    }

    #[test]
    fn repo_picker_ctrl_space_toggles_even_while_typing() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &["/tmp/alpha"]);

        app.handle_key(KeyCode::Char('l'), KeyModifiers::NONE); // filter: matches alpha
        app.handle_key(KeyCode::Char(' '), KeyModifiers::CONTROL);
        let rp = picker_state(&app);
        assert!(rp.selected.contains(std::path::Path::new("/tmp/alpha")));
        assert_eq!(rp.input.value(), "l", "chord must not type into the input");

        // Legacy terminals deliver Ctrl+Space as NUL — same action.
        app.handle_key(KeyCode::Null, KeyModifiers::CONTROL);
        assert!(!picker_state(&app)
            .selected
            .contains(std::path::Path::new("/tmp/alpha")));
    }

    #[test]
    fn repo_picker_ctrl_t_toggles_worktree_and_selects() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &["/tmp/a"]);

        app.handle_key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        let rp = picker_state(&app);
        assert!(rp.worktree.contains(std::path::Path::new("/tmp/a")));
        assert!(
            rp.selected.contains(std::path::Path::new("/tmp/a")),
            "worktree toggle checks the repo too"
        );

        // Toggling off keeps the selection.
        app.handle_key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        let rp = picker_state(&app);
        assert!(!rp.worktree.contains(std::path::Path::new("/tmp/a")));
        assert!(rp.selected.contains(std::path::Path::new("/tmp/a")));
    }

    #[test]
    fn repo_picker_del_forgets_only_when_input_empty() {
        let mut app = app_with_sessions(0);
        app.db
            .upsert_repo_bookmark("", std::path::Path::new("/tmp/zzz"))
            .unwrap();
        app.start_new_session();

        // With text in the input, Delete is forward-delete, not "forget".
        app.handle_key(KeyCode::Char('z'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Home, KeyModifiers::NONE);
        app.handle_key(KeyCode::Delete, KeyModifiers::NONE);
        let rp = picker_state(&app);
        assert_eq!(rp.input.value(), "");
        assert!(rp.rows.iter().any(|r| r.path.ends_with("zzz")));

        // With the input empty, Delete forgets the highlighted bookmark.
        app.handle_key(KeyCode::Delete, KeyModifiers::NONE);
        assert!(picker_state(&app)
            .rows
            .iter()
            .all(|r| !r.path.ends_with("zzz")));
        assert!(app.db.list_repo_bookmarks("").unwrap().is_empty());
    }

    #[test]
    fn repo_picker_enter_on_highlighted_repo_opens_single_repo_fast_path() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &["/tmp/fast"]);

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::SessionName(_)));
        assert_eq!(
            app.new_session
                .spawn_config
                .as_ref()
                .unwrap()
                .cwd
                .as_deref(),
            Some(std::path::Path::new("/tmp/fast"))
        );
    }

    #[test]
    fn repo_picker_enter_never_falls_through_to_home() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &["/tmp/a"]);

        // A filter with no matches leaves nothing highlighted: Enter is a
        // no-op — not a silent $HOME session (the old fallthrough).
        for c in "zzzz".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::RepoPicker(_)));
        assert!(app.new_session.spawn_config.is_none());
    }

    #[test]
    fn repo_picker_enter_on_start_here_row_spawns_home_session() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &["/tmp/a"]);

        app.handle_key(KeyCode::Down, KeyModifiers::NONE); // highlight "start here"
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::SessionName(_)));
        assert_eq!(
            app.new_session
                .spawn_config
                .as_ref()
                .unwrap()
                .cwd
                .as_deref(),
            crate::paths::home_dir().as_deref()
        );
    }

    #[test]
    fn repo_picker_enter_with_checked_repos_confirms_them_not_highlight() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &["/tmp/a", "/tmp/b"]);

        app.handle_key(KeyCode::Char(' '), KeyModifiers::NONE); // check /tmp/a
        app.handle_key(KeyCode::Down, KeyModifiers::NONE); // highlight /tmp/b
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::SessionName(_)));
        assert_eq!(
            app.new_session
                .spawn_config
                .as_ref()
                .unwrap()
                .cwd
                .as_deref(),
            Some(std::path::Path::new("/tmp/a")),
            "the checked repo wins over the highlight"
        );
        assert!(app.new_session.additional_dirs.is_empty());
    }

    #[test]
    fn repo_picker_tab_never_toggles_or_confirms() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &["/tmp/a"]);

        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);

        let rp = picker_state(&app);
        assert!(rp.selected.is_empty());
        assert_eq!(rp.input.value(), "");
        assert!(app.new_session.spawn_config.is_none());
    }

    #[test]
    fn repo_picker_import_suggestion_enter_imports_parent_and_rescans() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("repo1").join(".git")).unwrap();

        let mut app = app_with_sessions(0);
        app.start_new_session();
        {
            let modals::Modal::RepoPicker(ref mut rp) = app.modal else {
                panic!("expected repo picker");
            };
            rp.rows.clear();
            rp.push_row(
                tmp.path().to_path_buf(),
                modals::RepoRowKind::ImportSuggestion,
            );
            rp.recompute_filter();
        }

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        let rp = picker_state(&app);
        assert!(rp.rows[0].is_header(), "suggestion became a parent header");
        assert!(
            rp.rows.iter().any(|r| r.is_child()),
            "the parent's git children were scanned in"
        );
        assert!(
            !app.db.list_repo_bookmarks("").unwrap().is_empty(),
            "the parent bookmark was persisted"
        );
    }

    /// Type `text` into the palette input, character by character (the way a
    /// user would), so filters/candidates refresh exactly as in production.
    fn type_into_picker(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[test]
    fn repo_picker_path_mode_lists_local_dir_candidates_with_repo_marker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("repo1").join(".git")).unwrap();
        std::fs::create_dir_all(tmp.path().join("plain")).unwrap();

        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, &format!("{}/", tmp.path().display()));

        let rp = picker_state(&app);
        let names: Vec<(&str, bool)> = rp
            .candidates
            .iter()
            .map(|c| (c.name.as_str(), c.is_repo))
            .collect();
        assert_eq!(names, vec![("plain", false), ("repo1", true)]);
        assert_eq!(rp.candidate_index, None, "typed path is the Enter target");
    }

    #[test]
    fn repo_picker_tab_completes_common_prefix_and_descends() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("alpha")).unwrap();

        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, &format!("{}/al", tmp.path().display()));

        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);

        let rp = picker_state(&app);
        assert_eq!(
            rp.input.value(),
            format!("{}/alpha/", tmp.path().display()),
            "unique match completes fully and descends"
        );
        assert!(matches!(app.modal, modals::Modal::RepoPicker(_)));
    }

    #[test]
    fn repo_picker_enter_on_repo_candidate_bookmarks_selects_and_advances() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("repo1").join(".git")).unwrap();

        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, &format!("{}/", tmp.path().display()));
        app.handle_key(KeyCode::Down, KeyModifiers::NONE); // highlight repo1
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::SessionName(_)));
        assert_eq!(
            app.new_session
                .spawn_config
                .as_ref()
                .unwrap()
                .cwd
                .as_deref(),
            Some(tmp.path().join("repo1").as_path())
        );
        assert!(
            !app.db.list_repo_bookmarks("").unwrap().is_empty(),
            "the opened repo was bookmarked for next time"
        );
    }

    #[test]
    fn repo_picker_enter_on_plain_dir_candidate_drills_in() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub").join("inner")).unwrap();

        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, &format!("{}/", tmp.path().display()));
        app.handle_key(KeyCode::Down, KeyModifiers::NONE); // highlight sub
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        let rp = picker_state(&app);
        assert_eq!(
            rp.input.value(),
            format!("{}/sub/", tmp.path().display()),
            "a plain directory drills in instead of opening"
        );
        assert_eq!(
            rp.candidates
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["inner"],
            "the candidate list followed the descent"
        );
        // The drilled input must stay in path mode; a fallback that dropped the
        // path lead (see the `~\…` Windows case in `repo_picker_drill_into`)
        // would silently flip the palette back to bookmark filtering.
        assert_eq!(rp.input_mode(), modals::RepoInputMode::Path);
    }

    #[test]
    fn repo_picker_enter_on_typed_full_path_adds_and_advances() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("repo1").join(".git")).unwrap();

        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, &format!("{}/repo1", tmp.path().display()));
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        // One Enter: bookmarked, selected, and the flow advanced (the old
        // add-then-confirm double-Enter is gone).
        assert!(matches!(app.modal, modals::Modal::SessionName(_)));
        assert_eq!(
            app.new_session
                .spawn_config
                .as_ref()
                .unwrap()
                .cwd
                .as_deref(),
            Some(tmp.path().join("repo1").as_path())
        );
    }

    #[test]
    fn repo_picker_enter_with_trailing_slash_acts_on_typed_dir_not_first_child() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("repo1").join("child")).unwrap();
        std::fs::create_dir_all(tmp.path().join("repo1").join(".git")).unwrap();

        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, &format!("{}/repo1/", tmp.path().display()));
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::SessionName(_)));
        assert_eq!(
            app.new_session
                .spawn_config
                .as_ref()
                .unwrap()
                .cwd
                .as_deref(),
            Some(tmp.path().join("repo1").as_path()),
            "the typed dir itself opens (normalized, no trailing slash) — not its first child"
        );
    }

    #[test]
    fn repo_picker_enter_on_missing_typed_path_errors_and_stays() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, "/definitely/not/a/real/dir");
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::RepoPicker(_)));
        assert!(app.new_session.spawn_config.is_none());
        let msg = app.status_message.as_ref().expect("an error toast");
        assert!(msg.text.contains("Path not found"));
    }

    #[test]
    fn repo_picker_remote_typing_never_refreshes_candidates() {
        let mut app = app_with_sessions(0);
        app.new_session.backend = Some("ssh:nowhere".into());
        app.open_repo_picker();
        type_into_picker(&mut app, "/tm");

        let rp = picker_state(&app);
        assert!(rp.remote);
        assert!(
            rp.candidates.is_empty() && rp.path_suggestion.is_none(),
            "remote paths must not touch the local filesystem per keystroke"
        );

        // Tab against an unknown host resolves no lister — a safe no-op.
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(picker_state(&app).input.value(), "/tm");
    }

    #[test]
    fn repo_picker_multibyte_candidate_completion_never_panics() {
        let tmp = tempfile::tempdir().unwrap();
        // Diverge inside a multibyte char: é (0xC3 0xA9) vs ê (0xC3 0xAA).
        std::fs::create_dir_all(tmp.path().join("répo-a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("rêpo-b")).unwrap();

        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, &format!("{}/r", tmp.path().display()));
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);

        let rp = picker_state(&app);
        assert_eq!(rp.candidates.len(), 2);
        assert!(rp.input.value().ends_with("/r"), "nothing shared beyond r");
    }

    #[test]
    fn repo_picker_hidden_dirs_only_listed_for_dot_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".hidden")).unwrap();
        std::fs::create_dir_all(tmp.path().join("visible")).unwrap();

        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, &format!("{}/", tmp.path().display()));
        let names: Vec<String> = picker_state(&app)
            .candidates
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(names, vec!["visible"]);

        type_into_picker(&mut app, ".");
        let names: Vec<String> = picker_state(&app)
            .candidates
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(names, vec![".hidden"]);
    }

    #[test]
    fn repo_picker_up_from_first_candidate_returns_to_typed_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("one")).unwrap();

        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &[]);
        type_into_picker(&mut app, &format!("{}/", tmp.path().display()));

        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(picker_state(&app).candidate_index, Some(0));
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            picker_state(&app).candidate_index,
            None,
            "the literal typed path stays reachable above the candidates"
        );
    }

    #[test]
    fn session_name_prefill_uses_repo_basename() {
        let mut app = app_with_sessions(0);
        seeded_repo_picker(&mut app, &["/tmp/friring"]);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        let modals::Modal::SessionName(ref sn) = app.modal else {
            panic!("expected the session-name modal");
        };
        assert_eq!(sn.name.value(), "friring");
    }

    #[test]
    fn session_name_prefill_dedupes_with_numeric_suffix() {
        let mut app = app_with_sessions(2);
        app.sessions[0].info.name = "friring".into();
        app.sessions[1].info.name = "friring-2".into();

        assert_eq!(
            app.suggested_session_name(Some(std::path::Path::new("/tmp/friring"))),
            "friring-3"
        );
        assert_eq!(
            app.suggested_session_name(Some(std::path::Path::new("/tmp/other"))),
            "other"
        );
        assert_eq!(app.suggested_session_name(None), "");
    }

    #[test]
    fn wizard_breadcrumb_accumulates_choices() {
        let mut app = app_with_sessions(0);
        assert_eq!(app.wizard_breadcrumb(), None);

        app.new_session.repo_path = Some(PathBuf::from("/tmp/friring"));
        app.new_session.base_branch = Some("main".into());
        app.new_session.normal_repos = vec![PathBuf::from("/tmp/other")];
        assert_eq!(
            app.wizard_breadcrumb().as_deref(),
            Some("friring +1 · wt from main")
        );

        // Normal flow after the backend/cwd moved onto the spawn config.
        let mut app = app_with_sessions(0);
        app.new_session.spawn_config = Some(SessionConfig {
            cwd: Some(PathBuf::from("/tmp/friring")),
            ..SessionConfig::default()
        });
        assert_eq!(app.wizard_breadcrumb().as_deref(), Some("friring"));
    }

    /// Render the app once and return the visible buffer as a flat string.
    fn rendered_text(app: &mut App) -> String {
        let backend = ratatui::backend::TestBackend::new(120, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.view(f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn branch_selector_title_names_the_repo() {
        let mut app = app_with_sessions(0);
        app.new_session.repo_path = Some(PathBuf::from("/tmp/friring"));
        app.modal = modals::Modal::BranchSelector(modals::BranchSelectorModal {
            index: 0,
            branches: vec!["main".into()],
            filter: Default::default(),
            loading: false,
        });
        let text = rendered_text(&mut app);
        assert!(
            text.contains("New Session — Base Branch (friring)"),
            "title must carry the repo context"
        );
    }

    #[test]
    fn session_name_modal_shows_flow_breadcrumb() {
        let mut app = app_with_sessions(0);
        app.new_session.repo_path = Some(PathBuf::from("/tmp/friring"));
        app.new_session.base_branch = Some("main".into());
        app.modal = modals::Modal::SessionName(modals::SessionNameModal::default());
        let text = rendered_text(&mut app);
        assert!(text.contains("New Session — Name"));
        assert!(
            text.contains("friring · wt from main"),
            "the accumulated choices must be visible"
        );

        // Fork and import flows announce themselves in the title.
        app.new_session.base_branch = None;
        app.new_session.repo_path = None;
        app.new_session.fork = true;
        let text = rendered_text(&mut app);
        assert!(text.contains("Fork — Name"));
    }

    #[test]
    fn rebuild_with_no_bookmarks_lists_import_suggestions_then_start_here() {
        let mut rp = modals::RepoPickerModal::default();
        App::rebuild_repo_picker_rows(&mut rp, Vec::new(), vec!["/tmp/sug".into()]);
        assert_eq!(rp.rows[0].kind, modals::RepoRowKind::ImportSuggestion);
        assert_eq!(rp.rows.last().unwrap().kind, modals::RepoRowKind::StartHere);

        // A remote target never suggests local folders.
        let mut rp = modals::RepoPickerModal {
            remote: true,
            ..Default::default()
        };
        App::rebuild_repo_picker_rows(&mut rp, Vec::new(), vec!["/tmp/sug".into()]);
        assert!(rp
            .rows
            .iter()
            .all(|r| r.kind != modals::RepoRowKind::ImportSuggestion));
    }

    #[test]
    fn help_capture_ignores_clicks() {
        let mut app = app_with_sessions(1);
        app.modal = modals::Modal::Help(modals::HelpModal {
            selected: 0,
            capturing: true,
        });
        app.click_targets.push(ClickTarget {
            rect: Rect::new(30, 8, 20, 1),
            action: ClickAction::ModalRow(3),
        });

        app.handle_mouse_click(35, 8, KeyModifiers::NONE);

        // Still capturing, selection untouched.
        let modals::Modal::Help(ref h) = app.modal else {
            panic!("help must stay open");
        };
        assert!(h.capturing);
        assert_eq!(h.selected, 0);
    }

    #[test]
    fn click_while_global_search_open_is_swallowed() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::SessionList;
        app.handle_key(KeyCode::Char('/'), KeyModifiers::CONTROL);
        assert!(app.global_search.active);
        app.click_targets.push(ClickTarget {
            rect: Rect::new(1, 3, 20, 1),
            action: ClickAction::SelectSession(0),
        });

        app.handle_mouse_click(5, 3, KeyModifiers::NONE);

        // The strip keeps focus; no selection armed, no target activated.
        assert!(app.global_search.active);
        assert_eq!(app.focus, InputFocus::GlobalSearch);
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn view_records_session_row_click_targets() {
        let mut app = app_with_sessions(2);
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.view(f)).unwrap();

        let rows: Vec<Rect> = app
            .click_targets
            .iter()
            .filter_map(|t| match t.action {
                ClickAction::SelectSession(_) => Some(t.rect),
                _ => None,
            })
            .collect();
        assert_eq!(rows.len(), 2, "one hitbox per rendered session row");

        // Click the second rendered row end-to-end through the registry.
        let target = app
            .click_targets
            .iter()
            .find(|t| t.action == ClickAction::SelectSession(1))
            .map(|t| t.rect)
            .unwrap();
        app.handle_mouse_click(target.x, target.y, KeyModifiers::NONE);
        let order = app.render_order_indices();
        assert_eq!(app.active_index, order[1]);
        assert_eq!(app.focus, InputFocus::Terminal);
    }

    #[test]
    fn view_drawn_modal_row_click_confirms() {
        // End-to-end through a real frame: the registry holds the pane
        // targets *and* the theme-picker rows; clicking a rendered row must
        // reach the modal, not the pane beneath it.
        let mut app = app_with_sessions(1);
        app.modal = modals::Modal::ThemePicker(theme_picker_at(1));
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.view(f)).unwrap();

        let row = app
            .click_targets
            .iter()
            .find(|t| t.action == ClickAction::ModalRow(0))
            .map(|t| t.rect)
            .expect("theme picker rows must be recorded");
        app.handle_mouse_click(row.x + 1, row.y, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::None));
        assert_eq!(
            app.active_theme.name,
            crate::ui::theme::all_theme_entries()[0].name
        );
    }

    #[test]
    fn mouse_move_updates_hover() {
        let mut app = app_with_sessions(1);
        app.update(AppMessage::MouseMove { x: 7, y: 9 });
        assert_eq!(app.mouse_hover, Some((7, 9)));
    }

    /// Render a frame so `click_targets` are recorded, then return the center of
    /// the first target whose action matches `pred`.
    fn rendered_target(app: &mut App, pred: impl Fn(&ClickAction) -> bool) -> Rect {
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.view(f)).unwrap();
        app.click_targets
            .iter()
            .find(|t| pred(&t.action))
            .map(|t| t.rect)
            .expect("matching click target recorded this frame")
    }

    /// Each footer button dispatches its global action when clicked.
    #[test]
    fn footer_help_button_click_opens_help() {
        let mut app = app_with_sessions(1);
        let r = rendered_target(&mut app, |a| {
            *a == ClickAction::Global(crate::session::Action::ToggleHelp)
        });
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::Help(_)));
    }

    #[test]
    fn footer_settings_button_click_opens_settings() {
        let mut app = app_with_sessions(1);
        let r = rendered_target(&mut app, |a| {
            *a == ClickAction::Global(crate::session::Action::OpenSettings)
        });
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::Settings(_)));
    }

    #[test]
    fn footer_theme_button_click_opens_theme_picker() {
        let mut app = app_with_sessions(1);
        let r = rendered_target(&mut app, |a| {
            *a == ClickAction::Global(crate::session::Action::OpenThemePicker)
        });
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::ThemePicker(_)));
    }

    /// `Ctrl+Alt+R` (ReloadApp) is a quit with the reload flag raised — and it
    /// must dispatch even from a focused terminal (it is no bare
    /// `Ctrl+<letter>`, so no PTY deferral applies).
    #[test]
    fn reload_chord_quits_with_reload_flag() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::Terminal;
        assert!(!app.reload_requested());
        app.handle_key(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(app.should_quit);
        assert!(app.reload_requested());
    }

    /// An ordinary quit (`Ctrl+Q`) and a session restart (`Ctrl+R`) must never
    /// raise the reload flag — only `Ctrl+Alt+R` re-execs the binary. Guards
    /// the flag against an accidental chord overlap.
    #[test]
    fn plain_quit_and_restart_do_not_request_reload() {
        let mut app = app_with_sessions(1);
        app.handle_key(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
        assert!(!app.reload_requested(), "Ctrl+Q must not request reload");

        let mut app = app_with_sessions(1);
        app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(
            !app.reload_requested(),
            "Ctrl+R (session restart) must not request reload"
        );
    }

    /// `ReloadApp` is a quit + re-exec, so — like `QuitApp` — it must escape the
    /// input-capturing panes (review, activity, automation/task editors) that
    /// otherwise swallow Ctrl/Alt chords before the global keybinding lookup.
    #[test]
    fn reload_chord_escapes_capture_panes() {
        for focus in [
            InputFocus::CodeReview,
            InputFocus::ReviewFiles,
            InputFocus::CcActivity,
            InputFocus::CcActivityTree,
            InputFocus::AutomationEditor,
            InputFocus::AutomationRunHistory,
            InputFocus::TaskEditor,
        ] {
            let mut app = app_with_sessions(1);
            app.focus = focus;
            app.handle_key(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            );
            assert!(
                app.reload_requested() && app.should_quit,
                "reload chord was swallowed in {focus:?}"
            );
        }
    }

    #[test]
    fn footer_quit_button_click_quits() {
        let mut app = app_with_sessions(1);
        let r = rendered_target(&mut app, |a| {
            *a == ClickAction::Global(crate::session::Action::QuitApp)
        });
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        assert!(app.should_quit);
    }

    /// A footer button click is swallowed while a modal is open (the modal owns
    /// every click), so it can't quit/navigate from underneath the overlay.
    #[test]
    fn footer_button_swallowed_while_modal_open() {
        let mut app = app_with_sessions(1);
        app.modal = modals::Modal::ThemePicker(theme_picker_at(0));
        // The footer still renders its buttons beneath the overlay.
        let r = rendered_target(&mut app, |a| {
            *a == ClickAction::Global(crate::session::Action::QuitApp)
        });
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        assert!(!app.should_quit, "modal must swallow the footer click");
        assert!(matches!(app.modal, modals::Modal::ThemePicker(_)));
    }

    /// The Settings modal's `[ Cancel ]` button closes it (Esc-equivalent).
    #[test]
    fn modal_cancel_button_closes() {
        let mut app = app_with_sessions(1);
        app.open_settings_panel();
        let r = rendered_target(&mut app, |a| {
            matches!(
                a,
                ClickAction::ModalButton {
                    code: KeyCode::Esc,
                    ..
                }
            )
        });
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
    }

    /// The Settings modal's `[ Save ]` button persists + closes (Ctrl+S).
    #[test]
    fn modal_save_button_saves_and_closes() {
        let mut app = app_with_sessions(1);
        app.open_settings_panel();
        let r = rendered_target(&mut app, |a| {
            matches!(
                a,
                ClickAction::ModalButton {
                    code: KeyCode::Char('s'),
                    mods: KeyModifiers::CONTROL,
                }
            )
        });
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
    }

    /// Render a frame, then return the (rect, index) of the first click target
    /// whose action matches `pred` (mapping the action to its field index).
    fn rendered_indexed_target(
        app: &mut App,
        pred: impl Fn(&ClickAction) -> Option<usize>,
    ) -> (Rect, usize) {
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.view(f)).unwrap();
        app.click_targets
            .iter()
            .find_map(|t| pred(&t.action).map(|i| (t.rect, i)))
            .expect("matching field click target recorded this frame")
    }

    /// Clicking a Settings field row selects that field (like Tab/↑↓).
    #[test]
    fn click_settings_field_selects_it() {
        let mut app = app_with_sessions(1);
        app.open_settings_panel();
        // Any field past the first, so the selection visibly changes.
        let (r, index) = rendered_indexed_target(&mut app, |a| match a {
            ClickAction::ModalField(i) if *i > 0 => Some(*i),
            _ => None,
        });
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        let modals::Modal::Settings(ref s) = app.modal else {
            panic!("settings modal must stay open");
        };
        assert_eq!(s.field, modals::SettingsField::ORDER[index]);
    }

    /// Clicking an Automation-editor field row selects that field.
    #[test]
    fn click_automation_editor_field_selects_it() {
        let mut app = app_with_sessions(1);
        app.open_automation_editor();
        let (r, index) = rendered_indexed_target(&mut app, |a| match a {
            ClickAction::ModalField(i) if *i > 0 => Some(*i),
            _ => None,
        });
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        let modals::Modal::AutomationEditor(ref m) = app.modal else {
            panic!("automation editor must stay open");
        };
        assert_eq!(m.field, m.visible_fields()[index]);
    }

    /// Clicking a field in the in-pane task editor focuses that field.
    #[test]
    fn click_task_editor_field_selects_it() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::TaskEditor;
        app.task_ui.task_editor = Some(modals::TaskEditorModal::new());
        // Index 2 = Status (default is Title), so the change is observable.
        let r = rendered_indexed_target(&mut app, |a| match a {
            ClickAction::PaneField {
                focus: InputFocus::TaskEditor,
                index,
            } if *index == 2 => Some(*index),
            _ => None,
        })
        .0;
        app.handle_mouse_click(r.x, r.y, KeyModifiers::NONE);
        let m = app.task_ui.task_editor.as_ref().unwrap();
        assert_eq!(m.field, modals::TaskField::Status);
    }

    /// A click on the palette's input area (no recorded target since the input
    /// is always focused) is swallowed — it must neither close the modal nor
    /// leak to the panes beneath.
    #[test]
    fn click_inside_repo_picker_chrome_is_swallowed() {
        let mut app = app_with_sessions(0);
        app.start_new_session(); // no hosts → opens the repo palette
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.view(f)).unwrap();
        // The input field sits directly above the footer buttons.
        let btn = app
            .click_targets
            .iter()
            .find(|t| matches!(t.action, ClickAction::ModalButton { .. }))
            .expect("palette renders footer buttons")
            .rect;
        app.handle_mouse_click(btn.x, btn.y.saturating_sub(2), KeyModifiers::NONE);
        assert!(
            matches!(app.modal, modals::Modal::RepoPicker(_)),
            "repo picker must stay open"
        );
    }

    /// Hovering a footer button brightens its fill to `accent_bright` (a
    /// button-like hover), distinct from the background band a list row gets.
    #[test]
    fn hovering_footer_button_brightens_it() {
        let mut app = app_with_sessions(1);
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.view(f)).unwrap();
        let r = app
            .click_targets
            .iter()
            .find(|t| matches!(t.action, ClickAction::Global(_)))
            .map(|t| t.rect)
            .expect("footer buttons recorded");
        app.update(AppMessage::MouseMove { x: r.x, y: r.y });
        terminal.draw(|f| app.view(f)).unwrap();
        let buf = terminal.backend().buffer();
        assert_eq!(
            buf[(r.x, r.y)].bg,
            crate::ui::theme::Theme::accent_bright(),
            "hovered footer button should brighten to accent_bright"
        );
    }

    /// `[features] mouse = false` drops every mouse message before dispatch.
    #[test]
    fn mouse_feature_flag_disables_all_mouse_handling() {
        let mut app = app_with_sessions(2);
        app.features.mouse = false;
        app.click_targets.push(ClickTarget {
            rect: Rect::new(1, 3, 20, 1),
            action: ClickAction::SelectSession(1),
        });

        app.update(AppMessage::MouseMove { x: 5, y: 3 });
        app.update(AppMessage::MouseClick {
            x: 5,
            y: 3,
            modifiers: KeyModifiers::NONE,
        });
        app.update(AppMessage::MouseScrollUp { x: 5, y: 3 });

        assert_eq!(app.mouse_hover, None);
        assert_eq!(app.active_index, 0);
        assert!(app.text_selection.is_none());
        assert_eq!(app.focus, InputFocus::SessionList);
    }

    fn automations_list_modal(count: usize) -> modals::Modal {
        modals::Modal::AutomationsList(modals::AutomationsListModal {
            index: 0,
            entries: (0..count)
                .map(|i| modals::AutomationListEntry {
                    id: i as i64,
                    name: format!("auto-{i}"),
                    summary: "daily".into(),
                    enabled: true,
                })
                .collect(),
        })
    }

    /// The wheel steps an open modal's selection (one row per tick, like j/k)
    /// instead of scrolling the panes beneath.
    #[test]
    fn wheel_in_modal_steps_selection() {
        let mut app = app_with_sessions(1);
        app.modal = automations_list_modal(5);

        app.handle_mouse_scroll(0, 0, false);
        app.handle_mouse_scroll(0, 0, false);
        let modals::Modal::AutomationsList(ref al) = app.modal else {
            panic!("modal must stay open");
        };
        assert_eq!(al.index, 2);

        app.handle_mouse_scroll(0, 0, true);
        let modals::Modal::AutomationsList(ref al) = app.modal else {
            panic!("modal must stay open");
        };
        assert_eq!(al.index, 1);
    }

    /// Clicking + dragging the modal's own scrollbar moves its selection;
    /// the clamp guard stops at the list end.
    #[test]
    fn modal_scrollbar_drag_moves_selection() {
        let mut app = app_with_sessions(1);
        app.modal = automations_list_modal(20);
        let track = Rect::new(70, 5, 1, 10);
        app.scrollbar_hits.push(ScrollbarHit {
            geom: ScrollbarGeom {
                track,
                content_len: 20,
                viewport: 10,
            },
            target: ScrollTarget::Modal,
        });

        // Grab the bottom of the track → selection jumps to the last row.
        app.handle_mouse_click(70, 14, KeyModifiers::NONE);
        assert_eq!(app.dragging_scrollbar, Some(ScrollTarget::Modal));
        let modals::Modal::AutomationsList(ref al) = app.modal else {
            panic!("modal must stay open");
        };
        assert_eq!(al.index, 19);

        // Drag back to the top.
        app.handle_mouse_drag(70, 5);
        let modals::Modal::AutomationsList(ref al) = app.modal else {
            panic!("modal must stay open");
        };
        assert_eq!(al.index, 0);

        app.handle_mouse_up(70, 5);
        assert!(app.dragging_scrollbar.is_none());
    }

    /// While a modal is open, the wheel never reaches the panes beneath it.
    #[test]
    fn wheel_in_modal_does_not_scroll_panes() {
        let mut app = app_with_sessions(2);
        app.modal = automations_list_modal(2);
        let before = app.active_index;
        // Coordinates over the session list, which would normally switch
        // sessions on wheel.
        app.handle_mouse_scroll(2, 3, false);
        assert_eq!(app.active_index, before);
    }

    /// While the F1 editor captures a chord, the wheel must not synthesize a
    /// key (it would become the new binding).
    #[test]
    fn wheel_during_help_capture_is_ignored() {
        let mut app = app_with_sessions(1);
        app.modal = modals::Modal::Help(modals::HelpModal {
            selected: 3,
            capturing: true,
        });
        app.handle_mouse_scroll(0, 0, false);
        let modals::Modal::Help(ref h) = app.modal else {
            panic!("help must stay open");
        };
        assert!(h.capturing, "capture must survive a wheel tick");
        assert_eq!(h.selected, 3);
    }

    // --- Wheel-to-PTY forwarding ---
    //
    // Modern alt-screen TUIs (Claude Code, vim, htop, btop, …) subscribe to
    // wheel events via xterm mouse tracking. Without forwarding, vt100's
    // scrollback no-ops on the alternate screen and the user sees a "dead"
    // wheel.

    /// Build a 1-session app that keeps the session's input-channel receiver so
    /// the test can inspect bytes the app writes to the PTY.
    fn app_with_input_rx() -> (App, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(Arc::clone(&backend_arc)),
            stub_agents(),
            test_db(),
        );
        let (session, rx) = Session::stub_with_input_rx("test", &backend_arc, &provider);
        app.sessions.push(session);
        app.active_index = 0;
        (app, rx)
    }

    /// Drive the active session's vt100 parser with `bytes` (e.g. an alt-screen
    /// enter + mouse-mode DECSET sequence) so subsequent tests see the same
    /// state the agent would have produced.
    fn feed_parser(app: &App, bytes: &[u8]) {
        let session = &app.sessions[app.active_index];
        let mut parser = session.parser.lock().unwrap();
        parser.process(bytes);
    }

    /// Centre-of-terminal hit point in screen-cell coordinates plus the
    /// expected 1-based (col, row) the PTY should see.
    fn click_in_terminal(app: &App) -> ((u16, u16), (u32, u32)) {
        let term = app.screen_layout().terminal;
        let inner = Block::default().borders(Borders::ALL).inner(term);
        let x = inner.x + 5;
        let y = inner.y + 2;
        ((x, y), (6, 3))
    }

    /// With SGR mouse tracking on (`\e[?1000h` + `\e[?1006h`), wheel up is sent
    /// as `\e[<64;col;rowM` — the encoding Claude Code subscribes to. Vt100
    /// scrollback is left alone so the inner app owns the scroll.
    #[test]
    fn wheel_forwards_sgr_mouse_when_inner_app_subscribes() {
        let (mut app, mut rx) = app_with_input_rx();
        // Switch to the alternate screen and enable 1000+1006 mouse tracking,
        // the exact sequence Claude Code emits at startup.
        feed_parser(&app, b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");

        let ((x, y), (col, row)) = click_in_terminal(&app);
        let before = app.sessions[0].parser.lock().unwrap().screen().scrollback();

        app.handle_mouse_scroll(x, y, true);

        let expected = format!("\x1b[<64;{col};{row}M").into_bytes();
        assert_eq!(rx.try_recv().ok(), Some(expected));
        let after = app.sessions[0].parser.lock().unwrap().screen().scrollback();
        assert_eq!(before, after, "vt100 scrollback must not move");
    }

    /// Wheel down uses xterm button 65 — the only thing that changes vs.
    /// wheel up.
    #[test]
    fn wheel_down_uses_button_65() {
        let (mut app, mut rx) = app_with_input_rx();
        feed_parser(&app, b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
        let ((x, y), (col, row)) = click_in_terminal(&app);

        app.handle_mouse_scroll(x, y, false);

        let expected = format!("\x1b[<65;{col};{row}M").into_bytes();
        assert_eq!(rx.try_recv().ok(), Some(expected));
    }

    /// No mouse tracking enabled → wheel still scrolls vt100's scrollback
    /// locally, the long-standing behavior for non-TUI shells. We don't assert
    /// the scrollback advances here (the stub parser is built with `0` history
    /// for hermeticity); the invariant we care about is the negative one — the
    /// PTY never sees the wheel.
    #[test]
    fn wheel_without_mouse_mode_does_not_forward() {
        let (mut app, mut rx) = app_with_input_rx();

        let ((x, y), _) = click_in_terminal(&app);
        app.handle_mouse_scroll(x, y, true);
        app.handle_mouse_scroll(x, y, false);

        assert!(
            rx.try_recv().is_err(),
            "no mouse mode → nothing forwarded to PTY"
        );
    }

    /// Mouse mode on but with the legacy encoding (no `?1006h`): we don't
    /// support the 223-cell-capped encoding, so we fall back to the local
    /// scrollback. The PTY sees nothing.
    #[test]
    fn wheel_with_legacy_encoding_does_not_forward() {
        let (mut app, mut rx) = app_with_input_rx();
        feed_parser(&app, b"\x1b[?1049h\x1b[?1000h");

        let ((x, y), _) = click_in_terminal(&app);
        app.handle_mouse_scroll(x, y, true);

        assert!(rx.try_recv().is_err(), "legacy encoding must not forward");
    }

    /// The hovered clickable row gets a background band in the rendered frame.
    #[test]
    fn hovered_session_row_gets_background_band() {
        let mut app = app_with_sessions(2);
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        // First draw records the row hitboxes; hover over the first row and
        // draw again so the highlight applies.
        terminal.draw(|f| app.view(f)).unwrap();
        let row = app
            .click_targets
            .iter()
            .find(|t| matches!(t.action, ClickAction::SelectSession(0)))
            .map(|t| t.rect)
            .unwrap();
        app.update(AppMessage::MouseMove { x: row.x, y: row.y });
        terminal.draw(|f| app.view(f)).unwrap();

        // The first session's hitbox spans its prepended repo-group header plus
        // the session line; the tint must land on the session line (the bottom
        // row) and spare the header. `row.y` is the header row for a group's
        // first session.
        let session_y = row.y + row.height - 1;
        let band = crate::ui::theme::Theme::selection_bg();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(row.x, session_y)].bg,
            band,
            "hovered session line must get the selection_bg band"
        );
        // The repo-group header above the session line is never tinted.
        assert_ne!(
            buffer[(row.x, row.y)].bg,
            band,
            "repo-group header must not get the hover band"
        );
        // A cell outside any clickable row keeps its non-band background.
        assert_ne!(buffer[(0, 0)].bg, band);
    }

    #[test]
    fn task_editor_e_chord_edits_not_global_binding() {
        // `e` inside the editor must edit the title, not fire the file-viewer
        // toggle / other global binding (capture-before-global).
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::TaskList;
        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(app.task_ui.task_editor.as_ref().unwrap().title.value(), "e");
        assert_eq!(app.focus, InputFocus::TaskEditor);
    }

    // --- Global search (Ctrl+/, fully rebindable) tests ---

    #[test]
    fn ctrl_slash_opens_global_search() {
        // Ctrl+/ is the default binding. Terminals encode it as `Ctrl+/`
        // (kitty protocol) or as the raw 0x1F byte that crossterm decodes as
        // `Ctrl+7` / `Ctrl+_` (legacy) — all three open the strip.
        for c in ['/', '7', '_'] {
            let mut app = app_with_sessions(1);
            app.focus = InputFocus::SessionList;
            app.handle_key(KeyCode::Char(c), KeyModifiers::CONTROL);
            assert!(app.global_search.active, "Ctrl+{c} should open search");
            assert_eq!(app.focus, InputFocus::GlobalSearch);
            // Esc closes and restores the previous focus.
            app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
            assert!(!app.global_search.active);
            assert_eq!(app.focus, InputFocus::SessionList);
        }
    }

    #[test]
    fn ctrl_a_no_longer_opens_global_search() {
        // Ctrl+A was the old default; it's now free (a readline start-of-line
        // chord left to the terminal / modal text fields) and opens nothing.
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::SessionList;
        app.handle_key(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert!(
            !app.global_search.active,
            "Ctrl+A must not open search anymore"
        );
    }

    #[test]
    fn global_search_chord_is_rebindable() {
        let base = std::env::temp_dir().join("friring-gs-rebind-test");
        let _ = std::fs::remove_dir_all(&base);
        let _g = crate::paths::TestPathGuard::new(&base);

        let mut app = app_with_sessions(1);
        // Rebind global search from Ctrl+/ to Ctrl+X via the F1 editor.
        app.keybindings.rebind(
            crate::session::Action::GlobalSearch,
            crate::session::KeyChord::ctrl('x'),
        );

        // The old chord no longer opens it...
        app.focus = InputFocus::SessionList;
        app.handle_key(KeyCode::Char('/'), KeyModifiers::CONTROL);
        assert!(!app.global_search.active, "old Ctrl+/ must not open search");

        // ...and the new chord does.
        app.handle_key(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(app.global_search.active, "new Ctrl+X should open search");
    }

    #[test]
    fn global_search_matches_session_name() {
        let mut app = app_with_sessions(2);
        app.sessions[0].info.name = "alpha".into();
        app.sessions[1].info.name = "bravo".into();
        app.open_global_search();
        for c in "brav".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        let hit = app
            .global_search
            .results
            .iter()
            .find(|r| matches!(r.kind, search::SearchKind::Session))
            .expect("a session result");
        assert_eq!(hit.label, "bravo");
        assert_eq!(hit.target, search::SearchTarget::Session { index: 1 });
    }

    #[test]
    fn global_search_matches_task_and_automation() {
        let mut app = app_with_sessions(1);
        let tid = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("fix the widget"))
            .unwrap();
        app.refresh_tasks();
        let new = crate::storage::automations::NewAutomation {
            name: "widget-nightly".into(),
            enabled: true,
            schedule: crate::session::AutomationSchedule::Once { at: 0 },
            timezone: None,
            action: crate::session::AutomationAction::send_to(SessionId::default()),
            prompt: "go".into(),
            next_run_at: None,
            prompt_steps: Vec::new(),
        };
        let aid = app.db.create_automation(&new).unwrap();
        app.refresh_automations();

        app.open_global_search();
        // The all-scopes search is one `Tab` past the switcher the popup
        // opens on.
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        for c in "widget".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(app
            .global_search
            .results
            .iter()
            .any(|r| r.target == search::SearchTarget::Task { id: tid }));
        assert!(app
            .global_search
            .results
            .iter()
            .any(|r| r.target == search::SearchTarget::Automation { id: aid }));
    }

    /// Disabled features contribute no search results, so a selection can
    /// never preview or jump into a pane the feature flags hide.
    #[test]
    fn global_search_omits_disabled_scopes() {
        let mut app = app_with_sessions(1);
        app.features.tasks = false;
        app.features.automations = false;
        app.db
            .create_task(&crate::storage::tasks::NewTask::local("fix the widget"))
            .unwrap();
        app.refresh_tasks();
        let new = crate::storage::automations::NewAutomation {
            name: "widget-nightly".into(),
            enabled: true,
            schedule: crate::session::AutomationSchedule::Once { at: 0 },
            timezone: None,
            action: crate::session::AutomationAction::send_to(SessionId::default()),
            prompt: "go".into(),
            next_run_at: None,
            prompt_steps: Vec::new(),
        };
        app.db.create_automation(&new).unwrap();
        app.refresh_automations();

        app.open_global_search();
        // The all-scopes search is one `Tab` past the switcher the popup
        // opens on.
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        for c in "widget".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(app.global_search.results.iter().all(|r| !matches!(
            r.kind,
            search::SearchKind::Task | search::SearchKind::Automation
        )));
    }

    /// Automations fully off: the TUI must not claim/fire due automations on
    /// tick or at startup catch-up (the CLI surface stays in charge).
    #[test]
    fn process_automations_noops_when_feature_disabled() {
        let mut app = app_with_sessions(1);
        app.features.automations = false;
        let new = crate::storage::automations::NewAutomation {
            name: "nightly".into(),
            enabled: true,
            schedule: crate::session::AutomationSchedule::Once { at: 1 },
            timezone: None,
            action: crate::session::AutomationAction::send_to(SessionId::default()),
            prompt: "go".into(),
            next_run_at: Some(1),
            prompt_steps: Vec::new(),
        };
        app.db.create_automation(&new).unwrap();
        let now = crate::sync::current_time_millis();
        assert_eq!(app.db.due_automations(now).unwrap().len(), 1);

        app.process_automations(true);
        assert_eq!(
            app.db.due_automations(now).unwrap().len(),
            1,
            "a due automation must stay unclaimed while the feature is off"
        );
    }

    #[test]
    fn global_search_matches_task_description() {
        let mut app = app_with_sessions(1);
        let tid = app
            .db
            .create_task(&crate::storage::tasks::NewTask {
                description: Some("investigate the flaky parser".into()),
                ..crate::storage::tasks::NewTask::local("unrelated title")
            })
            .unwrap();
        app.refresh_tasks();

        app.open_global_search();
        // The all-scopes search is one `Tab` past the switcher the popup
        // opens on.
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        for c in "flaky".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        let result = app
            .global_search
            .results
            .iter()
            .find(|r| r.target == search::SearchTarget::Task { id: tid });
        let result = result.expect("description should match the query");
        assert!(
            result.snippet.as_deref().unwrap_or("").contains("flaky"),
            "a description match carries a snippet"
        );
    }

    #[test]
    fn global_search_previewing_task_selects_it_for_central_pane() {
        // When the strip previews a task result, the owning panel's cursor moves
        // to it and the preview kind is Task — the two facts the central pane
        // uses to render the task's full-screen detail/markdown.
        let mut app = app_with_sessions(1);
        let tid = app
            .db
            .create_task(&crate::storage::tasks::NewTask {
                description: Some("rendered in the main pane".into()),
                ..crate::storage::tasks::NewTask::local("zzz unrelated")
            })
            .unwrap();
        app.refresh_tasks();

        app.open_global_search();
        // The all-scopes search is one `Tab` past the switcher the popup
        // opens on.
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        for c in "main pane".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(
            app.global_search_preview_kind(),
            Some(search::SearchKind::Task),
            "the matched task result should be the live preview"
        );
        assert_eq!(
            app.selected_task().map(|t| t.id),
            Some(tid),
            "the previewed task must be the panel's selection so the central pane shows it"
        );
    }

    #[test]
    fn global_search_fuzzy_matches_task_description() {
        // A gapped (non-substring) query must still hit the description, the
        // same way it would the title — they share the fuzzy matcher.
        let mut app = app_with_sessions(1);
        let tid = app
            .db
            .create_task(&crate::storage::tasks::NewTask {
                description: Some("investigate the flaky parser".into()),
                ..crate::storage::tasks::NewTask::local("unrelated title")
            })
            .unwrap();
        app.refresh_tasks();

        app.open_global_search();
        // The all-scopes search is one `Tab` past the switcher the popup
        // opens on.
        app.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        for c in "invflaky".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert!(
            app.global_search
                .results
                .iter()
                .any(|r| r.target == search::SearchTarget::Task { id: tid }),
            "a gapped query should fuzzy-match the description"
        );
    }

    #[test]
    fn global_search_content_match_finds_session() {
        let mut app = app_with_sessions(2);
        app.sessions[0].info.name = "one".into();
        app.sessions[1].info.name = "two".into();
        // Write a distinctive token into session 1's buffer.
        {
            let mut parser = app.sessions[1].parser.lock().unwrap();
            parser.process(b"deploy failed: exit 1\r\n");
        }
        let snippet = app.session_content_match("deploy failed", 1);
        assert!(snippet.is_some(), "content scan should find the token");
        // The full content-rebuild path includes it as a Session result.
        app.open_global_search();
        for c in "deploy failed".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.recompute_global_search_content();
        assert!(app.global_search.results.iter().any(|r| {
            r.target == search::SearchTarget::Session { index: 1 } && r.snippet.is_some()
        }));
    }

    #[test]
    fn global_search_enter_switches_active_index() {
        let mut app = app_with_sessions(2);
        app.sessions[0].info.name = "alpha".into();
        app.sessions[1].info.name = "bravo".into();
        app.active_index = 0;
        app.open_global_search();
        for c in "bravo".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        // Select the first (only) session result and activate.
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.global_search.active);
        assert_eq!(app.active_index, 1);
        assert_eq!(app.focus, InputFocus::Terminal);
    }

    #[test]
    fn global_search_empty_query_has_no_results() {
        let mut app = app_with_sessions(1);
        app.open_global_search();
        assert!(app.global_search.results.is_empty());
    }

    #[test]
    fn global_search_previews_session_while_typing_and_navigating() {
        let mut app = app_with_sessions(3);
        app.sessions[0].info.name = "alpha".into();
        app.sessions[1].info.name = "bravo".into();
        app.sessions[2].info.name = "bronco".into();
        app.active_index = 0;
        app.open_global_search();
        // Typing "br" matches bravo + bronco; the preview moves to the first.
        for c in "br".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.active_index, 1, "preview follows the top result");
        // Down moves the preview to the next matching session.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.active_index, 2, "preview follows ↓ selection");
        // Focus stays in the search box during preview.
        assert_eq!(app.focus, InputFocus::GlobalSearch);
    }

    #[test]
    fn global_search_cancel_restores_previous_state() {
        let mut app = app_with_sessions(3);
        app.sessions[0].info.name = "alpha".into();
        app.sessions[1].info.name = "bravo".into();
        app.sessions[2].info.name = "bronco".into();
        app.active_index = 0;
        app.focus = InputFocus::Terminal;
        let tasks_before = app.show_tasks_panel;

        app.open_global_search();
        for c in "bro".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        // Preview moved the active session away from 0.
        assert_ne!(app.active_index, 0);

        // Esc cancels → everything snaps back to the pre-search state.
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.global_search.active);
        assert_eq!(app.active_index, 0, "active session restored");
        assert_eq!(app.focus, InputFocus::Terminal, "focus restored");
        assert_eq!(app.show_tasks_panel, tasks_before, "panel toggles restored");
    }

    #[test]
    fn global_search_commit_keeps_jump_no_restore() {
        let mut app = app_with_sessions(2);
        app.sessions[0].info.name = "alpha".into();
        app.sessions[1].info.name = "bravo".into();
        app.active_index = 0;
        app.open_global_search();
        for c in "bravo".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        // Committed: snapshot dropped, jump kept (not restored to 0).
        assert!(app.global_search.snapshot.is_none());
        assert_eq!(app.active_index, 1);
    }

    #[test]
    fn global_search_query_gates_live_highlighting() {
        let mut app = app_with_sessions(1);
        // Inactive → no live-highlight query.
        assert_eq!(app.global_search_query(), None);
        app.open_global_search();
        // Active but empty query → still None (panels render normally).
        assert_eq!(app.global_search_query(), None);
        for c in "lo".chars() {
            app.handle_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.global_search_query(), Some("lo"));
        app.close_global_search();
        assert_eq!(app.global_search_query(), None);
    }

    // --- Context-sensitive Ctrl+J/K tests ---

    #[test]
    fn ctrl_j_switches_session_when_session_list_focused() {
        let mut app = app_with_sessions(3);
        app.focus = InputFocus::SessionList;
        app.active_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(app.active_index, 1);
    }

    #[test]
    fn ctrl_j_defers_to_pty_when_terminal_focused() {
        // Ctrl+J is the LF byte a legacy terminal sends for Ctrl+Enter; with
        // the terminal focused it belongs to the agent (insert newline), not
        // to session cycling — see `Action::terminal_passthrough`.
        let mut app = app_with_sessions(3);
        app.focus = InputFocus::Terminal;
        app.active_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(app.active_index, 0);
    }

    #[test]
    fn alt_j_switches_session_when_terminal_focused() {
        // The non-Ctrl-letter alternate keeps in-terminal cycling alive.
        let mut app = app_with_sessions(3);
        app.focus = InputFocus::Terminal;
        app.active_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::ALT);
        assert_eq!(app.active_index, 1);
    }

    #[test]
    fn ctrl_j_at_last_session_wraps_to_first() {
        let mut app = app_with_sessions(3);
        app.focus = InputFocus::SessionList;
        app.active_index = 2;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(app.active_index, 0);
    }

    #[test]
    fn ctrl_k_at_first_session_wraps_to_last() {
        let mut app = app_with_sessions(3);
        app.focus = InputFocus::SessionList;
        app.active_index = 0;
        app.handle_key(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(app.active_index, 2);
    }

    // --- Last-session toggle (Ctrl+6 / Ctrl+^) ---

    #[test]
    fn ctrl6_bounces_between_the_two_most_recent_sessions() {
        let mut app = app_with_sessions(3);
        app.active_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert_eq!(app.active_index, 1);

        app.handle_key(KeyCode::Char('6'), KeyModifiers::CONTROL);
        assert_eq!(app.active_index, 0);
        assert_eq!(app.focus, InputFocus::Terminal, "the toggle is a jump");

        // The kitty-protocol encoding of the same physical key.
        app.handle_key(KeyCode::Char('^'), KeyModifiers::CONTROL);
        assert_eq!(app.active_index, 1);
    }

    #[test]
    fn last_session_toggle_without_history_reports() {
        let mut app = app_with_sessions(2);
        app.active_index = 0;
        app.handle_key(KeyCode::Char('6'), KeyModifiers::CONTROL);
        assert_eq!(app.active_index, 0);
        let msg = app.status_message.as_ref().expect("status hint set");
        assert!(msg.text.contains("No previous"), "{}", msg.text);
    }

    #[test]
    fn last_session_toggle_drops_a_deleted_previous_session() {
        let mut app = app_with_sessions(2);
        app.active_index = 0;
        app.last_active_session = Some(SessionId::default());
        app.handle_key(KeyCode::Char('6'), KeyModifiers::CONTROL);
        assert_eq!(app.active_index, 0);
        assert_eq!(app.last_active_session, None, "stale id dropped");
        let msg = app.status_message.as_ref().expect("status hint set");
        assert!(msg.text.contains("gone"), "{}", msg.text);
    }

    // --- Next-blocked navigation (F10) ---

    #[test]
    fn f10_walks_blocked_sessions_in_render_order_and_wraps() {
        let mut app = app_with_sessions(4);
        app.sessions[1].info.status = SessionStatus::Blocked;
        app.sessions[3].info.status = SessionStatus::Blocked;
        app.focus = InputFocus::SessionList;
        app.active_index = 0;

        // Routed through the real pipeline so the F10 default binding is
        // covered too.
        app.handle_key(KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.active_index, 1);
        assert_eq!(
            app.focus,
            InputFocus::Terminal,
            "an attention jump lands in the terminal"
        );

        app.handle_key(KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.active_index, 3);

        // Past the last blocked session the walk wraps to the first.
        app.handle_key(KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.active_index, 1);
    }

    #[test]
    fn f10_without_blocked_sessions_reports_and_stays() {
        let mut app = app_with_sessions(2);
        app.focus = InputFocus::Terminal;
        app.active_index = 0;
        app.handle_key(KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.active_index, 0);
        let msg = app.status_message.as_ref().expect("status hint set");
        assert!(msg.text.contains("Nothing needs attention"), "{}", msg.text);
    }

    #[test]
    fn f10_with_only_the_active_session_blocked_stays_put() {
        let mut app = app_with_sessions(2);
        app.sessions[0].info.status = SessionStatus::Blocked;
        app.active_index = 0;
        app.handle_key(KeyCode::F(10), KeyModifiers::NONE);
        assert_eq!(app.active_index, 0);
        let msg = app.status_message.as_ref().expect("status hint set");
        assert!(
            msg.text.contains("Nothing else needs attention"),
            "{}",
            msg.text
        );
    }

    // --- Session jump overlays (Alt hold / Alt+digit / Alt+A) ---

    #[test]
    fn alt_digit_jumps_to_nth_session_and_focuses_terminal() {
        let mut app = app_with_sessions(3);
        app.focus = InputFocus::SessionList;
        app.active_index = 0;
        app.handle_key(KeyCode::Char('2'), KeyModifiers::ALT);
        assert_eq!(app.active_index, 1);
        assert_eq!(app.focus, InputFocus::Terminal);
    }

    #[test]
    fn alt_digit_out_of_range_reports() {
        let mut app = app_with_sessions(3);
        app.active_index = 0;
        app.handle_key(KeyCode::Char('9'), KeyModifiers::ALT);
        assert_eq!(app.active_index, 0);
        let msg = app.status_message.as_ref().expect("status hint set");
        assert!(msg.text.contains("No session #9"), "{}", msg.text);
    }

    #[test]
    fn alt_hold_overlay_appears_after_delay_and_clears_on_release() {
        let mut app = app_with_sessions(2);
        app.update(AppMessage::AltHeld(true));
        // Before the debounce delay nothing shows (a readline M-chord in the
        // shell shouldn't flash numbers).
        assert_eq!(app.jump_overlay_attention_only(), None);
        app.alt_held_since = Some(
            std::time::Instant::now() - std::time::Duration::from_millis(JUMP_OVERLAY_DELAY_MS),
        );
        assert_eq!(app.jump_overlay_attention_only(), Some(false));
        app.update(AppMessage::AltHeld(false));
        assert_eq!(app.jump_overlay_attention_only(), None);
    }

    /// A delayed which-key overlay costs exactly one frame. The armed state
    /// never times out, so a tick that kept re-requesting would pin the app at
    /// the event loop's 10 ms poll rate for as long as the leader stays armed.
    #[test]
    fn delayed_which_key_hint_requests_one_redraw_not_one_per_tick() {
        let mut app = app_with_sessions(1);
        app.prefix_settings.hint_delay_ms = 500;
        app.handle_key(KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(app.prefix_state.is_armed(), "Ctrl+F arms the leader");
        app.mark_redrawn();

        app.tick_prefix_hint();
        assert!(!app.should_redraw(), "nothing to paint before the delay");

        clock::advance(std::time::Duration::from_millis(501));
        app.tick_prefix_hint();
        assert!(
            app.should_redraw(),
            "the overlay's first frame is requested"
        );

        app.mark_redrawn();
        app.tick_prefix_hint();
        assert!(
            !app.should_redraw(),
            "an armed leader must not request a frame every tick"
        );
    }

    #[test]
    fn session_jump_targets_filter_blocked_and_cap_at_nine() {
        let mut app = app_with_sessions(12);
        assert_eq!(app.session_jump_targets(false).len(), 9);
        app.sessions[4].info.status = SessionStatus::Blocked;
        app.sessions[10].info.status = SessionStatus::Blocked;
        assert_eq!(app.session_jump_targets(true), vec![4, 10]);
    }

    #[test]
    fn alt_a_sticky_overlay_numbers_blocked_and_plain_digit_jumps() {
        let mut app = app_with_sessions(4);
        app.sessions[2].info.status = SessionStatus::Blocked;
        app.focus = InputFocus::Terminal;
        app.active_index = 0;

        // Tap (Alt not held → legacy terminal): sticky overlay.
        app.handle_key(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(app.attention_jump, Some(AttentionJumpMode::Sticky));
        assert_eq!(app.jump_overlay_attention_only(), Some(true));

        // A plain digit indexes the *blocked* numbering, not the row number.
        app.handle_key(KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(app.active_index, 2);
        assert_eq!(app.focus, InputFocus::Terminal);
        assert_eq!(app.attention_jump, None, "a jump dismisses the overlay");
    }

    #[test]
    fn alt_a_without_blocked_sessions_reports() {
        let mut app = app_with_sessions(2);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(app.attention_jump, None);
        let msg = app.status_message.as_ref().expect("status hint set");
        assert!(msg.text.contains("Nothing needs attention"), "{}", msg.text);
    }

    /// The queue walks blocked sessions first and only falls through to the
    /// finished-but-unseen ones once nothing is waiting on an answer — so a
    /// blocking prompt is never buried behind a pile of completed runs.
    #[test]
    fn attention_queue_prefers_blocked_then_falls_through_to_done() {
        let mut app = app_with_sessions(3);
        app.sessions[1].info.status = SessionStatus::Done;
        app.sessions[2].info.status = SessionStatus::Blocked;
        app.active_index = 0;

        assert_eq!(app.attention_status(), Some(SessionStatus::Blocked));
        app.focus_next_attention();
        assert_eq!(app.active_index, 2, "the blocked one, not the done one");

        // Answering it leaves only the finished run in the queue.
        app.sessions[2].info.status = SessionStatus::Idle;
        assert_eq!(app.attention_status(), Some(SessionStatus::Done));
        app.focus_next_attention();
        assert_eq!(app.active_index, 1);
    }

    /// With the fall-through switched off the queue is blocked-only again.
    #[test]
    fn attention_queue_can_exclude_finished_sessions() {
        let mut app = app_with_sessions(2);
        app.navigation.attention_includes_done = false;
        app.sessions[1].info.status = SessionStatus::Done;

        assert_eq!(app.attention_status(), None);
        assert!(app.session_jump_targets(true).is_empty());
    }

    /// An overlay left open while the last blocked session unblocks must stop
    /// numbering, not fall back to numbering everything.
    #[test]
    fn attention_overlay_numbers_nothing_once_the_queue_empties() {
        let mut app = app_with_sessions(3);
        app.sessions[2].info.status = SessionStatus::Blocked;
        app.toggle_attention_jump();
        assert_eq!(app.session_jump_targets(true), vec![2]);

        app.sessions[2].info.status = SessionStatus::Idle;
        assert!(
            app.session_jump_targets(true).is_empty(),
            "an empty queue must not degrade into the all-sessions numbering"
        );
    }

    #[test]
    fn sticky_overlay_esc_dismisses_and_other_keys_pass_through() {
        let mut app = app_with_sessions(3);
        app.sessions[1].info.status = SessionStatus::Blocked;
        app.handle_key(KeyCode::Char('a'), KeyModifiers::ALT);
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.attention_jump, None, "Esc dismisses");

        // A non-digit key dismisses the sticky overlay *and* still performs
        // its normal action (here: session-list j moves the selection).
        app.handle_key(KeyCode::Char('a'), KeyModifiers::ALT);
        app.focus = InputFocus::SessionList;
        app.active_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.attention_jump, None);
        assert_eq!(app.active_index, 1, "the key still acted normally");
    }

    #[test]
    fn alt_a_toggles_the_overlay_off() {
        let mut app = app_with_sessions(2);
        app.sessions[0].info.status = SessionStatus::Blocked;
        app.handle_key(KeyCode::Char('a'), KeyModifiers::ALT);
        assert!(app.attention_jump.is_some());
        app.handle_key(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(app.attention_jump, None);
    }

    #[test]
    fn held_mode_blocked_overlay_dismissed_by_alt_release() {
        let mut app = app_with_sessions(2);
        app.sessions[1].info.status = SessionStatus::Blocked;
        app.update(AppMessage::AltHeld(true));
        app.handle_key(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(app.attention_jump, Some(AttentionJumpMode::Held));
        // Other keys don't dismiss a held overlay (Alt chords keep flowing) …
        app.handle_key(KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(app.attention_jump, Some(AttentionJumpMode::Held));
        // … releasing Alt does.
        app.update(AppMessage::AltHeld(false));
        assert_eq!(app.attention_jump, None);
    }

    #[test]
    fn missed_alt_release_self_heals_on_the_next_plain_key() {
        let mut app = app_with_sessions(2);
        app.focus = InputFocus::SessionList;
        app.update(AppMessage::AltHeld(true));
        assert!(app.alt_held);
        // A key event without the ALT bit means the release was lost.
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(!app.alt_held);
    }

    // --- Unified left-column (session list ↔ automations) navigation ---

    /// Add an enabled spawn automation to the DB and refresh the cache.
    fn add_test_automation(app: &mut App, name: &str) {
        let new = crate::storage::automations::NewAutomation {
            name: name.to_string(),
            enabled: true,
            schedule: AutomationSchedule::Cron {
                expr: "0 9 * * *".to_string(),
            },
            timezone: None,
            action: AutomationAction::Spawn {
                repo_path: std::path::PathBuf::from("/tmp/repo"),
                worktree_branch: None,
                base_branch: None,
                agent: None,
                extra_repos: Vec::new(),
                host: None,
                session_mode: Default::default(),
            },
            prompt: "do stuff".to_string(),
            next_run_at: None,
            prompt_steps: Vec::new(),
        };
        app.db.create_automation(&new).unwrap();
        app.refresh_automations();
    }

    #[test]
    fn j_at_last_session_enters_automations_pane() {
        let mut app = app_with_sessions(3);
        app.focus = InputFocus::SessionList;
        app.active_index = 2; // last in render order (no admins)
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::Automations);
        assert_eq!(app.automation_ui.automation_panel_index, 0);
    }

    #[test]
    fn j_mid_session_list_advances_without_leaving() {
        let mut app = app_with_sessions(3);
        app.focus = InputFocus::SessionList;
        app.active_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::SessionList);
        assert_eq!(app.active_index, 1);
    }

    #[test]
    fn k_at_first_session_loops_to_last_automation() {
        let mut app = app_with_sessions(3);
        add_test_automation(&mut app, "a");
        add_test_automation(&mut app, "b");
        app.focus = InputFocus::SessionList;
        app.active_index = 0;
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        // Above the first session the column loops to the bottom: the last
        // automation in the pane.
        assert_eq!(app.focus, InputFocus::Automations);
        assert_eq!(app.automation_ui.automation_panel_index, 1);
    }

    #[test]
    fn k_at_top_of_automations_returns_to_last_session() {
        let mut app = app_with_sessions(3);
        add_test_automation(&mut app, "nightly");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::SessionList);
        assert_eq!(app.active_index, 2); // last in render order
    }

    #[test]
    fn k_in_empty_automations_pane_returns_to_session_list() {
        let mut app = app_with_sessions(2);
        app.focus = InputFocus::Automations;
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::SessionList);
    }

    #[test]
    fn j_at_bottom_automation_loops_to_first_session() {
        let mut app = app_with_sessions(2);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.active_index = 1;
        app.automation_ui.automation_panel_index = 0; // the only (= last) automation
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        // Past the last automation the column loops back to the top session.
        assert_eq!(app.focus, InputFocus::SessionList);
        assert_eq!(app.active_index, 0, "looped to first session");
    }

    #[test]
    fn j_between_automations_advances_selection() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        add_test_automation(&mut app, "b");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.automation_ui.automation_panel_index, 1);
    }

    #[test]
    fn n_in_automations_pane_focuses_new_editor() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::Automations;
        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        // The central-pane editor is focused (no overlay modal), with a new
        // (unsaved) automation.
        assert_eq!(app.focus, InputFocus::AutomationEditor);
        assert!(matches!(app.modal, modals::Modal::None));
        let editor = app
            .automation_ui
            .automation_editor
            .as_ref()
            .expect("editor present");
        assert!(editor.editing_id.is_none(), "should be a new automation");
    }

    #[test]
    fn enter_in_automations_pane_focuses_editor_for_existing() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::AutomationEditor);
        assert!(matches!(app.modal, modals::Modal::None));
        let editor = app
            .automation_ui
            .automation_editor
            .as_ref()
            .expect("editor present");
        assert!(
            editor.editing_id.is_some(),
            "should edit the existing automation"
        );
    }

    #[test]
    fn ctrl_l_from_automations_enters_editor_and_ctrl_h_returns() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        // Ctrl+L moves focus into the central-pane editor (like a session).
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::AutomationEditor);
        // Ctrl+H returns to the automations list.
        app.handle_key(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::Automations);
    }

    #[test]
    fn navigating_automations_rebuilds_editor_preview() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        add_test_automation(&mut app, "b");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        let first = app
            .automation_ui
            .automation_editor
            .as_ref()
            .unwrap()
            .editing_id;
        assert_eq!(first, Some(app.automation_ui.cached_automations[0].id));
        // Moving down rebuilds the preview to mirror the next automation.
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.automation_ui.automation_panel_index, 1);
        let second = app
            .automation_ui
            .automation_editor
            .as_ref()
            .unwrap()
            .editing_id;
        assert_eq!(second, Some(app.automation_ui.cached_automations[1].id));
        assert_ne!(first, second);
    }

    #[test]
    fn leaving_automation_context_clears_editor() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.sync_automation_editor();
        assert!(app.automation_ui.automation_editor.is_some());
        // Focusing a session drops the in-pane editor preview.
        app.focus = InputFocus::SessionList;
        app.sync_automation_editor();
        assert!(app.automation_ui.automation_editor.is_none());
    }

    #[test]
    fn editing_in_pane_and_saving_returns_to_list() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationEditor;
        // Edit the name, then save with Enter.
        if let Some(ed) = app.automation_ui.automation_editor.as_mut() {
            ed.field = AutomationField::Name;
            ed.name.set("renamed");
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::Automations);
        let autos = app.db.list_automations().unwrap();
        assert_eq!(autos.len(), 1);
        assert_eq!(autos[0].name, "renamed");
    }

    #[test]
    fn editing_in_pane_persists_the_edited_prompt_steps() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationEditor;
        // Turn the single-step automation into a two-step one with a custom
        // settle delay between the steps.
        if let Some(ed) = app.automation_ui.automation_editor.as_mut() {
            ed.steps[0].text.set("/model opus");
            ed.steps[0].delay.set("1500");
            ed.add_step();
            ed.steps[1].text.set("do the work");
            ed.field = AutomationField::Name;
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let id = app.db.list_automations().unwrap()[0].id;
        let saved = app.db.get_automation(id).unwrap().expect("row present");
        let texts: Vec<&str> = saved.prompt_steps.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["/model opus", "do the work"]);
        assert_eq!(saved.prompt_steps[0].delay_ms, Some(1500));
        assert_eq!(saved.prompt_steps[1].delay_ms, None);
    }

    #[test]
    fn editing_a_send_by_name_automation_keeps_its_target() {
        let mut app = app_with_sessions(1);
        let new = crate::storage::automations::NewAutomation {
            name: "by-name".to_string(),
            enabled: true,
            schedule: AutomationSchedule::Cron {
                expr: "0 9 * * *".to_string(),
            },
            timezone: None,
            action: AutomationAction::Send {
                target: crate::session::SendTarget::Name("inbox".to_string()),
            },
            prompt: "ping".to_string(),
            next_run_at: None,
            prompt_steps: Vec::new(),
        };
        let id = app.db.create_automation(&new).unwrap();
        app.refresh_automations();
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationEditor;
        // The target selector only offers running sessions, so "inbox" isn't in
        // it — renaming must not retarget the automation at the first session.
        if let Some(ed) = app.automation_ui.automation_editor.as_mut() {
            ed.field = AutomationField::Name;
            ed.name.set("renamed");
        }
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        let saved = app.db.get_automation(id).unwrap().expect("row present");
        assert_eq!(saved.name, "renamed");
        assert_eq!(
            saved.action,
            AutomationAction::Send {
                target: crate::session::SendTarget::Name("inbox".to_string()),
            }
        );
    }

    #[test]
    fn esc_in_pane_editor_discards_and_returns_to_list() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationEditor;
        if let Some(ed) = app.automation_ui.automation_editor.as_mut() {
            ed.name.set("scratch");
        }
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::Automations);
        // The discarded edit was not persisted.
        let autos = app.db.list_automations().unwrap();
        assert_eq!(autos[0].name, "a");
    }

    #[test]
    fn ctrl_e_in_pane_editor_toggles_enabled_not_file_viewer() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationEditor;
        let before = app
            .automation_ui
            .automation_editor
            .as_ref()
            .unwrap()
            .enabled;
        let fv_before = app.show_file_viewer;
        // Ctrl+E is the global file-viewer toggle, but the pane editor must
        // capture it as "toggle enabled" instead.
        app.handle_key(KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(
            app.show_file_viewer, fv_before,
            "file viewer must not toggle"
        );
        assert_eq!(
            app.automation_ui
                .automation_editor
                .as_ref()
                .unwrap()
                .enabled,
            !before,
            "Ctrl+E should flip the editor's enabled flag"
        );
    }

    #[test]
    fn cycle_wraps_back_to_automations_not_session() {
        let mut app = app_with_sessions(2);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        // Automations → editor → run history → back to Automations (never lands
        // on a session, mirroring how Esc returns to the selected automation).
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::AutomationEditor);
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::AutomationRunHistory);
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::Automations);
    }

    #[test]
    fn new_automation_editor_cycle_wraps_to_automations() {
        // A brand-new automation has no run history, so the ring is just
        // Automations ↔ editor — and Ctrl+L still returns to the list.
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::Automations;
        app.handle_key(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::AutomationEditor);
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::Automations);
    }

    #[test]
    fn enter_on_run_opens_related_session() {
        let mut app = app_with_sessions(2);
        add_test_automation(&mut app, "a");
        let auto_id = app.automation_ui.cached_automations[0].id;
        // Record a run with a typed related session (as fire_automation does).
        let target = app.sessions[1].info.id;
        app.db
            .record_automation_run(auto_id, AutomationRunStatus::Success, "sent", Some(target))
            .unwrap();
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationRunHistory;
        app.refresh_selected_automation_runs();
        app.automation_ui.automation_run_index = 0;

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.focus, InputFocus::Terminal);
        assert_eq!(app.active_index, 1, "should jump to the referenced session");
    }

    #[test]
    fn enter_on_legacy_run_parses_session_from_detail() {
        let mut app = app_with_sessions(2);
        add_test_automation(&mut app, "a");
        let auto_id = app.automation_ui.cached_automations[0].id;
        // Pre-v28 rows have no related_session_id; only the free-text detail
        // (e.g. "session <uuid>") references the session.
        let target = app.sessions[1].info.id;
        app.db
            .record_automation_run(
                auto_id,
                AutomationRunStatus::Success,
                &format!("session {target}"),
                None,
            )
            .unwrap();
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationRunHistory;
        app.refresh_selected_automation_runs();
        app.automation_ui.automation_run_index = 0;

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(app.focus, InputFocus::Terminal);
        assert_eq!(app.active_index, 1, "should jump to the referenced session");
    }

    #[test]
    fn enter_on_run_without_session_stays_in_history() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        let auto_id = app.automation_ui.cached_automations[0].id;
        // A skipped run has no session id in its detail.
        app.db
            .record_automation_run(
                auto_id,
                AutomationRunStatus::Skipped,
                "target session not running",
                None,
            )
            .unwrap();
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationRunHistory;
        app.refresh_selected_automation_runs();

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        // No related session → stay put in the run-history panel.
        assert_eq!(app.focus, InputFocus::AutomationRunHistory);
    }

    #[test]
    fn ctrl_l_from_editor_enters_run_history_then_back() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationEditor;
        // Editor → run history → editor.
        app.handle_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::AutomationRunHistory);
        app.handle_key(KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, InputFocus::AutomationEditor);
    }

    #[test]
    fn run_history_jk_moves_selection_and_r_triggers_run() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        let id = app.automation_ui.cached_automations[0].id;
        // Two recorded runs so j/k has something to move over.
        app.db
            .record_automation_run(id, AutomationRunStatus::Success, "one", None)
            .unwrap();
        app.db
            .record_automation_run(id, AutomationRunStatus::Error, "two", None)
            .unwrap();
        app.focus = InputFocus::Automations;
        app.automation_ui.automation_panel_index = 0;
        app.sync_automation_editor();
        app.focus = InputFocus::AutomationRunHistory;
        app.refresh_selected_automation_runs();
        assert_eq!(app.automation_ui.automation_run_index, 0);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.automation_ui.automation_run_index, 1);
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.automation_ui.automation_run_index, 0);
        // `r` marks the automation due (next_run_at in the past/now).
        app.handle_key(KeyCode::Char('r'), KeyModifiers::NONE);
        let auto = app.db.get_automation(id).unwrap().unwrap();
        let now = crate::sync::current_time_millis();
        assert!(
            auto.next_run_at.map(|n| n <= now).unwrap_or(false),
            "run-now should make the automation due"
        );
    }

    #[test]
    fn focusing_automations_loads_selected_run_history() {
        let mut app = app_with_sessions(1);
        add_test_automation(&mut app, "a");
        let id = app.automation_ui.cached_automations[0].id;
        app.db
            .record_automation_run(id, AutomationRunStatus::Success, "spawned x", None)
            .unwrap();
        // While the pane is unfocused the run cache is empty.
        assert!(app.automation_ui.cached_automation_runs.is_empty());
        // Entering the pane (via j from the last/only session) loads it.
        app.focus = InputFocus::SessionList;
        app.active_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::Automations);
        assert_eq!(app.automation_ui.cached_automation_runs_id, Some(id));
        assert_eq!(app.automation_ui.cached_automation_runs.len(), 1);
    }

    #[test]
    fn spawn_automation_expands_tilde_in_repo_path() {
        let mut app = app_with_sessions(0);
        let mut m = modals::AutomationEditorModal::default();
        m.name.set("t");
        m.prompt_mut().set("hi");
        m.action = AutomationActionKind::Spawn;
        m.trigger_kind = TriggerKind::Daily; // yields a future next_run
        m.repo.set("~/Repositories/friring");
        app.modal = modals::Modal::AutomationEditor(Box::new(m));

        app.submit_automation_editor();

        let autos = app.db.list_automations().unwrap();
        assert_eq!(autos.len(), 1, "automation should have been created");
        match &autos[0].action {
            AutomationAction::Spawn { repo_path, .. } => {
                // `~` expands via the platform home dir: `$HOME` on Unix,
                // `%USERPROFILE%` on Windows (see `paths::expand_tilde`).
                let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
                let home = std::env::var(home_var).expect("home var set in tests");
                assert_eq!(
                    repo_path,
                    &std::path::PathBuf::from(home).join("Repositories/friring"),
                    "leading ~ should be expanded to an absolute path"
                );
            }
            other => panic!("expected a spawn action, got {other:?}"),
        }
    }

    #[test]
    fn send_automation_target_defaults_to_active_and_is_selectable() {
        let mut app = app_with_sessions(3);
        app.active_index = 1;
        app.open_automation_editor();

        // The Send target defaults to the active session, and every session is
        // offered as a choice.
        {
            let modals::Modal::AutomationEditor(ref m) = app.modal else {
                panic!("expected the automation editor");
            };
            assert_eq!(m.sessions.len(), 3);
            assert_eq!(
                m.selected_target().map(|(id, _)| *id),
                Some(app.sessions[1].info.id)
            );
        }

        // Cycle the Target selector to the next session, then submit.
        let expected_id;
        {
            let modals::Modal::AutomationEditor(ref mut m) = app.modal else {
                panic!("expected the automation editor");
            };
            m.name.set("ping");
            m.prompt_mut().set("hi");
            m.trigger_kind = TriggerKind::Daily;
            m.field = AutomationField::Target;
            m.adjust(1); // index 1 -> 2
            expected_id = m.selected_target().map(|(id, _)| *id).unwrap();
        }
        app.submit_automation_editor();

        let autos = app.db.list_automations().unwrap();
        assert_eq!(autos.len(), 1);
        match &autos[0].action {
            AutomationAction::Send { target } => assert_eq!(target.id(), Some(expected_id)),
            other => panic!("expected a send action, got {other:?}"),
        }
    }

    #[test]
    fn send_automation_without_sessions_is_rejected() {
        let mut app = app_with_sessions(0);
        app.open_automation_editor();
        {
            let modals::Modal::AutomationEditor(ref mut m) = app.modal else {
                panic!("expected the automation editor");
            };
            m.name.set("x");
            m.prompt_mut().set("y");
            m.trigger_kind = TriggerKind::Daily;
            // action defaults to Send, but there are no sessions to target.
        }
        app.submit_automation_editor();
        assert_eq!(
            app.db.list_automations().unwrap().len(),
            0,
            "a send automation with no target session must not be created"
        );
    }

    // --- DB persistence tests ---

    #[test]
    fn load_persisted_state_empty_db_returns_none() {
        let app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        assert!(app.load_persisted_state_from_db().is_none());
    }
    #[test]
    fn save_state_roundtrips_sessions() {
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );

        // Add a session
        let session = Session::stub("test-session", &backend_arc, &provider);
        app.sessions.push(session);

        // Save to DB (only persists sessions + counter, not projects)
        app.save_state();

        // Verify session in DB
        let sessions = app.db.list_active_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "test-session");
    }

    #[test]
    fn save_state_persists_session_counter() {
        let mut app = App::new(24, 120, stub_backend(), stub_agents(), test_db());
        app.session_counter = 42;

        app.save_state();

        let counter = app.db.get_session_counter().unwrap();
        assert_eq!(counter, 42);
    }

    #[test]
    fn pane_osc52_copies_drain_once_in_stream_order() {
        // A pane program's OSC 52 write (e.g. `/copy`) is captured from the
        // output stream and drained exactly once for the app to route through
        // the clipboard stack.
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut session = Session::stub("s", &backend_arc, &provider);
        session.feed_output_for_test(b"out\x1b]52;c;aGVsbG8=\x07");
        session.feed_output_for_test(b"\x1b]52;c;d29ybGQ=\x07");
        let texts: Vec<String> = session
            .drain_osc52_copies()
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        assert_eq!(texts, ["hello", "world"]);
        assert!(session.drain_osc52_copies().is_empty());
    }

    /// The tick end of the `/copy` chain: a pane's OSC 52 write — in the
    /// exact tmux-passthrough-wrapped shape Claude Code emits under `$TMUX` —
    /// is routed through the clipboard stack with a toast naming the
    /// originating session, exactly once.
    #[test]
    fn tick_routes_pane_osc52_copy_to_clipboard_with_attribution() {
        let mut app = app_with_sessions(1);
        app.captured_clipboard = Some(Vec::new());
        app.sessions[0].feed_output_for_test(b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8gd29ybGQ=\x07\x1b\\");

        app.tick_core();

        assert_eq!(
            app.captured_clipboard.as_deref(),
            Some(&["hello world".to_string()][..])
        );
        assert_eq!(
            app.status_message.as_ref().map(|m| m.text.as_str()),
            Some("Copied from test-session")
        );

        // Drained: another tick must not copy again.
        app.tick_core();
        assert_eq!(app.captured_clipboard.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn session_to_shared_maps_worktree() {
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );

        let mut session = Session::stub("test-session", &backend_arc, &provider);
        session.info.worktrees = vec![WorktreeInfo {
            repo_path: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/repo/.git/wt/feat"),
            branch: "feat".to_string(),
        }];

        app.sessions.push(session);

        let shared = app.session_to_shared(&app.sessions[0]);
        assert_eq!(shared.worktrees.len(), 1);
        let wt = &shared.worktrees[0];
        assert_eq!(wt.branch, "feat");
        assert_eq!(wt.repo_path, PathBuf::from("/repo"));
    }

    #[test]
    fn session_to_shared_maps_parent_session_id() {
        // Regression guard: `save_state` upserts every session via
        // `session_to_shared`, so dropping the parent here would wipe a
        // CLI-set lead/worker link from the DB on the TUI's next save.
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );

        let parent_id = SessionId::default();
        let mut session = Session::stub("worker", &backend_arc, &provider);
        session.info.parent_session_id = Some(parent_id);
        app.sessions.push(session);

        let shared = app.session_to_shared(&app.sessions[0]);
        assert_eq!(shared.parent_session_id, Some(parent_id));

        // And the metadata copy applies it back on adoption/update.
        let mut adopted = Session::stub("worker", &backend_arc, &provider);
        App::apply_shared_session_metadata(&mut adopted, &shared);
        assert_eq!(adopted.info.parent_session_id, Some(parent_id));
    }

    #[test]
    fn session_to_shared_maps_display_order() {
        // Regression guard: `save_state` upserts every session via
        // `session_to_shared`, so dropping the field here would wipe the
        // manual list order from the DB on the TUI's next save.
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );

        let mut session = Session::stub("ordered", &backend_arc, &provider);
        session.info.display_order = Some(7);
        app.sessions.push(session);

        let shared = app.session_to_shared(&app.sessions[0]);
        assert_eq!(shared.display_order, Some(7));

        // And the metadata copy applies it back on adoption/update.
        let mut adopted = Session::stub("ordered", &backend_arc, &provider);
        App::apply_shared_session_metadata(&mut adopted, &shared);
        assert_eq!(adopted.info.display_order, Some(7));
    }

    #[test]
    fn move_active_session_renumbers_and_persists() {
        let mut app = app_with_sessions(3);
        for (i, s) in app.sessions.iter_mut().enumerate() {
            s.info.name = format!("s{i}");
        }
        app.active_index = 0;

        app.move_active_session(true);

        // Render order is now [s1, s0, s2], densely renumbered 0..n.
        assert_eq!(app.render_order_indices(), vec![1, 0, 2]);
        assert_eq!(app.sessions[1].info.display_order, Some(0));
        assert_eq!(app.sessions[0].info.display_order, Some(1));
        assert_eq!(app.sessions[2].info.display_order, Some(2));
        // The selection follows the moved row (input index unchanged).
        assert_eq!(app.active_index, 0);

        // Persisted: the DB lists sessions in the new order.
        let names: Vec<String> = app
            .db
            .list_active_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["s1", "s0", "s2"]);

        // A status change never moves a row.
        app.sessions[2].info.status = SessionStatus::Blocked;
        assert_eq!(app.render_order_indices(), vec![1, 0, 2]);
    }

    #[test]
    fn move_active_session_at_edge_is_noop() {
        let mut app = app_with_sessions(2);
        app.active_index = 0;
        app.move_active_session(false); // already at the top
        assert_eq!(app.render_order_indices(), vec![0, 1]);
        assert!(app.sessions.iter().all(|s| s.info.display_order.is_none()));
    }

    #[test]
    fn sort_sessions_alphabetically_renumbers_and_persists() {
        let mut app = app_with_sessions(3);
        // Names in deliberately non-alphabetical order: c, a, b.
        app.sessions[0].info.name = "c".to_string();
        app.sessions[1].info.name = "a".to_string();
        app.sessions[2].info.name = "b".to_string();
        app.active_index = 0;

        app.sort_sessions_alphabetically();

        // Render order is now [a, b, c], densely renumbered 0..n.
        assert_eq!(app.render_order_indices(), vec![1, 2, 0]);
        assert_eq!(app.sessions[1].info.display_order, Some(0));
        assert_eq!(app.sessions[2].info.display_order, Some(1));
        assert_eq!(app.sessions[0].info.display_order, Some(2));

        // Persisted: the DB lists sessions in the new order.
        let names: Vec<String> = app
            .db
            .list_active_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn sort_sessions_alphabetically_empty_is_noop() {
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(stub_backend_arc()),
            stub_agents(),
            test_db(),
        );
        app.sort_sessions_alphabetically(); // must not panic
        assert!(app.sessions.is_empty());
    }

    #[test]
    fn ctrl_r_no_op_without_agent_session_id() {
        let mut app = app_with_sessions(1);
        // App::new may toast warnings from the developer's real keybindings
        // file; this test only cares that Ctrl+R itself stays silent.
        app.status_message = None;
        // Session exists but has no agent_session_id
        app.sessions[0].info.agent_session_id = None;
        app.focus = InputFocus::Terminal;
        app.handle_key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        // Should be a no-op (no error, no crash)
        assert!(app.status_message.is_none());
    }

    #[test]
    fn session_to_shared_maps_additional_dirs() {
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend_arc.clone()),
            stub_agents(),
            test_db(),
        );

        let mut session = Session::stub("test-session", &backend_arc, &provider);
        session.info.additional_dirs = vec![PathBuf::from("/repo2"), PathBuf::from("/repo3")];

        app.sessions.push(session);

        let shared = app.session_to_shared(&app.sessions[0]);
        assert_eq!(shared.additional_dirs.len(), 2);
        assert_eq!(shared.additional_dirs[0], PathBuf::from("/repo2"));
        assert_eq!(shared.additional_dirs[1], PathBuf::from("/repo3"));
    }

    // --- multi-repo member resolution + workspace cwd ---

    #[test]
    fn member_dirs_worktree_first_then_additional() {
        let worktrees = vec![WorktreeInfo {
            repo_path: PathBuf::from("/src/webapp"),
            worktree_path: PathBuf::from("/wt/webapp/feat"),
            branch: "feat".into(),
        }];
        let additional = vec![PathBuf::from("/src/infra")];
        let members = session_member_dirs(None, &worktrees, &additional);

        // Worktree repo: name from repo_path, dir = the checkout. Then the
        // non-worktree additional dir.
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].0.as_deref(), Some("webapp"));
        assert_eq!(members[0].1, PathBuf::from("/wt/webapp/feat"));
        assert_eq!(members[1].0.as_deref(), Some("infra"));
        assert_eq!(members[1].1, PathBuf::from("/src/infra"));
    }

    #[test]
    fn member_dirs_no_worktrees_uses_cwd_first() {
        let cwd = PathBuf::from("/src/primary");
        let additional = vec![PathBuf::from("/src/other")];
        let members = session_member_dirs(Some(&cwd), &[], &additional);

        assert_eq!(members.len(), 2);
        assert_eq!(members[0].1, PathBuf::from("/src/primary"));
        assert_eq!(members[1].1, PathBuf::from("/src/other"));
    }

    /// Init a real git repo at `dir` on branch `main` with one commit.
    fn init_repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        let git = |args: &[&str]| {
            let ok = crate::git::git_program()
                .args(args)
                .current_dir(dir)
                .output()
                .expect("run git")
                .status
                .success();
            assert!(ok, "git {args:?} failed in {}", dir.display());
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("file.txt"), "hi").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
    }

    #[test]
    fn create_worktrees_falls_back_to_extra_repos_default_branch() {
        // The chosen base exists only in the primary repo; the extra repo must
        // fork from its own default branch instead of failing the whole spawn.
        let tmp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path().join("data"));
        let primary = tmp.path().join("primary");
        let extra = tmp.path().join("extra");
        init_repo(&primary);
        init_repo(&extra);
        let ok = crate::git::git_program()
            .args(["branch", "feat-base"])
            .current_dir(&primary)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "creating feat-base in primary failed");

        let infos = create_worktrees(
            None,
            &[primary.clone(), extra.clone()],
            "wt-branch",
            "feat-base",
        )
        .expect("both worktrees created");

        assert_eq!(infos.len(), 2);
        for info in &infos {
            assert!(info.worktree_path.exists());
            assert_eq!(info.branch, "wt-branch");
        }
        assert_eq!(infos[0].repo_path, primary);
        assert_eq!(infos[1].repo_path, extra);
    }

    #[test]
    fn process_cwd_single_member_is_primary() {
        let cwd = PathBuf::from("/src/only");
        let out = resolve_process_cwd(Some("id-1"), Some(cwd.clone()), &[], &[], None, None);
        assert_eq!(out, Some(cwd));
    }

    #[test]
    fn process_cwd_multi_member_is_workspace() {
        let base = std::env::temp_dir().join("friring-procwd-test");
        let _ = std::fs::remove_dir_all(&base);
        let _g = crate::paths::TestPathGuard::new(&base);

        let primary = base.join("repo-a");
        let other = base.join("repo-b");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        let out = resolve_process_cwd(
            Some("sess-x"),
            Some(primary.clone()),
            &[],
            std::slice::from_ref(&other),
            None,
            None,
        )
        .unwrap();

        // cwd is now a workspace under the workspaces root, with a symlink per repo.
        let ws_root = crate::paths::workspaces_directory().unwrap();
        assert!(out.starts_with(&ws_root), "{out:?} not under {ws_root:?}");
        assert_eq!(std::fs::read_link(out.join("repo-a")).unwrap(), primary);
        assert_eq!(std::fs::read_link(out.join("repo-b")).unwrap(), other);
    }

    #[test]
    fn process_cwd_multi_member_honors_custom_workspace_dir() {
        let base = std::env::temp_dir().join("friring-procwd-custom-test");
        let _ = std::fs::remove_dir_all(&base);
        let _g = crate::paths::TestPathGuard::new(&base);

        let primary = base.join("repo-a");
        let other = base.join("repo-b");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let custom = base.join("named-ws");

        let out = resolve_process_cwd(
            Some("sess-y"),
            Some(primary.clone()),
            &[],
            std::slice::from_ref(&other),
            None,
            Some(custom.as_path()),
        )
        .unwrap();

        assert_eq!(out, custom);
        assert_eq!(std::fs::read_link(custom.join("repo-a")).unwrap(), primary);
        assert_eq!(std::fs::read_link(custom.join("repo-b")).unwrap(), other);
    }

    #[test]
    fn process_cwd_multi_member_without_session_id_falls_back_to_primary() {
        // No agent_session_id → no stable name for a workspace → use the primary
        // repo (and don't touch the filesystem).
        let primary = PathBuf::from("/src/a");
        let other = PathBuf::from("/src/b");
        let out = resolve_process_cwd(
            None,
            Some(primary.clone()),
            &[],
            std::slice::from_ref(&other),
            None,
            None,
        );
        assert_eq!(out, Some(primary));
    }

    #[test]
    fn set_error_creates_error_status() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.set_error("something failed");
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Error);
        assert_eq!(msg.text, "something failed");
    }

    #[test]
    fn set_status_creates_typed_status() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.set_status(StatusLevel::Success, "all good");
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Success);
        assert_eq!(msg.text, "all good");
    }

    #[test]
    fn set_status_replaces_previous() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.set_error("old error");
        app.set_status(StatusLevel::Info, "new info");
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Info);
        assert_eq!(msg.text, "new info");
    }

    // --- Repo picker row building ---

    fn repo_bookmark(path: &Path, is_parent: bool) -> crate::storage::repo_bookmarks::RepoBookmark {
        crate::storage::repo_bookmarks::RepoBookmark {
            repo_path: path.to_path_buf(),
            label: None,
            last_used_at: 0,
            use_count: 1,
            is_parent,
        }
    }

    #[test]
    fn rebuild_rows_dedupes_standalone_that_is_also_a_parent_child() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("alpha").join(".git")).unwrap();
        std::fs::create_dir_all(root.join("beta").join(".git")).unwrap();

        // A standalone bookmark for `alpha` AND a parent bookmark for `root`
        // whose scan also finds `alpha`. `alpha` must appear exactly once.
        let bookmarks = vec![
            repo_bookmark(&root.join("alpha"), false),
            repo_bookmark(root, true),
        ];
        let mut rp = modals::RepoPickerModal::default();
        App::rebuild_repo_picker_rows(&mut rp, bookmarks, Vec::new());

        // Rows: parent header, alpha (child), beta (child), plus the pinned
        // "start here" row. The standalone `alpha` was dropped in favour of
        // the grouped child.
        assert_eq!(rp.rows.len(), 4);
        assert!(rp.rows[0].is_header());
        assert!(!rp.rows[1].is_header() && !rp.rows[2].is_header());
        assert_eq!(rp.rows[3].kind, modals::RepoRowKind::StartHere);
        let alpha_rows = rp.rows.iter().filter(|r| r.path.ends_with("alpha")).count();
        assert_eq!(alpha_rows, 1, "alpha must not be duplicated");
        // The single `alpha` row is the grouped child (nested under the parent).
        let alpha_idx = rp
            .rows
            .iter()
            .position(|r| r.path.ends_with("alpha"))
            .unwrap();
        assert!(rp.rows[alpha_idx].is_child());
    }

    #[test]
    fn rebuild_rows_dedupes_parent_child_that_is_also_a_parent() {
        // `root/sub` is a git repo found by scanning `root`, and is *also*
        // bookmarked as its own parent. Whichever order they are processed, the
        // `sub` path must render exactly once (no duplicate row).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sub").join(".git")).unwrap();
        std::fs::create_dir_all(root.join("sub").join("leaf").join(".git")).unwrap();

        let bookmarks = vec![
            repo_bookmark(root, true),
            repo_bookmark(&root.join("sub"), true),
        ];
        let mut rp = modals::RepoPickerModal::default();
        App::rebuild_repo_picker_rows(&mut rp, bookmarks, Vec::new());

        let sub_rows = rp
            .rows
            .iter()
            .filter(|r| r.path.file_name().is_some_and(|n| n == "sub"))
            .count();
        assert_eq!(sub_rows, 1, "sub must not be duplicated across parents");
    }

    #[test]
    fn rebuild_rows_collapse_hides_children() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("alpha").join(".git")).unwrap();
        std::fs::create_dir_all(root.join("beta").join(".git")).unwrap();

        let mut rp = modals::RepoPickerModal::default();
        App::rebuild_repo_picker_rows(&mut rp, vec![repo_bookmark(root, true)], Vec::new());
        // Header + two children + the pinned "start here" row all visible.
        assert_eq!(rp.filtered_indices.len(), 4);

        // Collapse the parent header (row 0) → the header (and the pinned
        // row) stay visible, the children hide.
        rp.toggle_collapsed(0);
        assert_eq!(rp.filtered_indices, vec![0, 3]);

        // Expanding restores the children.
        rp.toggle_collapsed(0);
        assert_eq!(rp.filtered_indices.len(), 4);
    }

    #[test]
    fn rebuild_preserves_selection_and_worktree_flags_across_rescan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("alpha").join(".git")).unwrap();

        let mut rp = modals::RepoPickerModal::default();
        App::rebuild_repo_picker_rows(&mut rp, vec![repo_bookmark(root, true)], Vec::new());
        let alpha = root.join("alpha");
        rp.toggle_worktree(&alpha); // also checks the repo

        // A refresh (e.g. after a parent import) rebuilds the rows; the
        // path-keyed picks must survive it.
        App::rebuild_repo_picker_rows(&mut rp, vec![repo_bookmark(root, true)], Vec::new());
        assert!(rp.selected.contains(&alpha));
        assert!(rp.worktree.contains(&alpha));
    }

    // --- Worktree sync tests ---

    #[test]
    fn start_sync_with_no_sessions_shows_info() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.start_sync();
        assert!(!app.worktree_sync.in_progress);
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Info);
        assert_eq!(msg.text, "No worktrees to sync");
    }

    #[test]
    fn start_sync_ignores_if_already_in_progress() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.worktree_sync.in_progress = true;
        app.status_message = None;
        app.start_sync();
        // Should not set any new status message
        assert!(app.status_message.is_none());
    }

    #[test]
    fn ctrl_s_triggers_start_sync() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.handle_key(KeyCode::Char('s'), KeyModifiers::CONTROL);
        // No sessions → info message
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.text, "No worktrees to sync");
    }

    /// ADR-P12 discipline: Ctrl+S dispatches the `git remote` listing to a
    /// background worker and parks the run — no git subprocess (and no modal)
    /// on the UI thread at the keypress.
    #[test]
    fn start_sync_parks_run_and_lists_remotes_off_thread() {
        let mut app = app_with_sessions(1);
        app.sessions[0].info.worktrees = vec![WorktreeInfo {
            repo_path: PathBuf::from("/tmp/nonexistent-repo"),
            worktree_path: PathBuf::from("/tmp/nonexistent-wt"),
            branch: "test-branch".to_string(),
        }];

        app.start_sync();
        assert!(!app.worktree_sync.in_progress, "no sync threads yet");
        assert!(app.worktree_sync.remotes_load.in_progress());
        assert!(app.worktree_sync.awaiting.is_some());
        assert!(matches!(app.modal, modals::Modal::None));
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Info);
        assert!(msg.text.contains("Preparing sync"));
    }

    #[test]
    fn start_sync_with_worktree_sessions_sets_in_progress() {
        let mut app = app_with_sessions(1);
        app.sessions[0].info.worktrees = vec![WorktreeInfo {
            repo_path: PathBuf::from("/tmp/nonexistent-repo"),
            worktree_path: PathBuf::from("/tmp/nonexistent-wt"),
            branch: "test-branch".to_string(),
        }];

        app.start_sync();
        // The remote listing runs on a real thread (a nonexistent repo lists
        // no remotes → the run launches straight away); poll until it lands.
        for _ in 0..500 {
            app.poll_sync_remotes();
            if app.worktree_sync.in_progress {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.worktree_sync.in_progress);
        assert_eq!(app.worktree_sync.pending, 1);
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Info);
        assert!(msg.text.contains("Syncing 1 worktree"));
    }

    /// A parked run whose repos all resolve to ≤1 remote launches straight
    /// from the poll — no base picker.
    #[test]
    fn sync_run_with_single_remote_launches_without_picker() {
        let mut app = app_with_sessions(1);
        let repo = PathBuf::from("/tmp/single-remote-repo");
        app.worktree_sync.awaiting = Some(sync_state::PendingSyncRun {
            worktrees: vec![(
                app.sessions[0].info.id,
                PathBuf::from("/tmp/single-remote-wt"),
                repo.clone(),
            )],
            queue: Vec::new(),
            chosen: HashMap::new(),
            host: None,
        });
        let tx = app.worktree_sync.remotes_load.start();
        tx.send(vec![(repo, vec!["origin".to_string()])]).unwrap();

        app.poll_sync_remotes();

        assert!(matches!(app.modal, modals::Modal::None));
        assert!(app.worktree_sync.in_progress);
        assert_eq!(app.worktree_sync.pending, 1);
        assert!(app.worktree_sync.awaiting.is_none());
    }

    /// A repo with more than one remote opens the base picker instead of
    /// launching, with `origin` preselected when no default is saved.
    #[test]
    fn sync_run_multi_remote_opens_base_picker() {
        let mut app = app_with_sessions(1);
        let repo = PathBuf::from("/tmp/multi-remote-repo");
        app.worktree_sync.awaiting = Some(sync_state::PendingSyncRun {
            worktrees: vec![(
                app.sessions[0].info.id,
                PathBuf::from("/tmp/multi-remote-wt"),
                repo.clone(),
            )],
            queue: Vec::new(),
            chosen: HashMap::new(),
            host: None,
        });
        let tx = app.worktree_sync.remotes_load.start();
        tx.send(vec![(repo, vec!["fork".to_string(), "origin".to_string()])])
            .unwrap();

        app.poll_sync_remotes();

        assert!(!app.worktree_sync.in_progress, "launch waits on the picker");
        match app.modal {
            modals::Modal::SyncBasePicker(ref sb) => {
                assert_eq!(sb.repo_name, "multi-remote-repo");
                assert_eq!(sb.remotes, ["fork", "origin"]);
                assert_eq!(sb.index, 1, "origin preselected without a saved default");
            }
            ref other => panic!("expected the sync base picker, got {other:?}"),
        }
    }

    /// A saved default remote wins the preselection over `origin`.
    #[test]
    fn sync_base_picker_preselects_saved_default() {
        let mut app = app_with_sessions(1);
        let repo = PathBuf::from("/tmp/default-remote-repo");
        app.db.set_sync_base_remote(&repo, "fork").unwrap();
        app.worktree_sync.awaiting = Some(sync_state::PendingSyncRun {
            worktrees: vec![(
                app.sessions[0].info.id,
                PathBuf::from("/tmp/default-remote-wt"),
                repo.clone(),
            )],
            queue: Vec::new(),
            chosen: HashMap::new(),
            host: None,
        });
        let tx = app.worktree_sync.remotes_load.start();
        tx.send(vec![(repo, vec!["fork".to_string(), "origin".to_string()])])
            .unwrap();

        app.poll_sync_remotes();

        match app.modal {
            modals::Modal::SyncBasePicker(ref sb) => assert_eq!(sb.index, 0),
            ref other => panic!("expected the sync base picker, got {other:?}"),
        }
    }

    /// Enter in the picker saves the choice as the repo's default and
    /// launches the run.
    #[test]
    fn sync_base_picker_enter_persists_default_and_launches() {
        let mut app = app_with_sessions(1);
        let repo = PathBuf::from("/tmp/pick-remote-repo");
        app.worktree_sync.awaiting = Some(sync_state::PendingSyncRun {
            worktrees: vec![(
                app.sessions[0].info.id,
                PathBuf::from("/tmp/pick-remote-wt"),
                repo.clone(),
            )],
            queue: vec![(repo.clone(), vec!["fork".to_string(), "origin".to_string()])],
            chosen: HashMap::new(),
            host: None,
        });
        app.modal = modals::Modal::SyncBasePicker(modals::SyncBasePickerModal {
            repo_name: "pick-remote-repo".to_string(),
            remotes: vec!["fork".to_string(), "origin".to_string()],
            index: 0,
        });

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert_eq!(
            app.db.get_sync_base_remote(&repo).unwrap().as_deref(),
            Some("fork")
        );
        assert!(matches!(app.modal, modals::Modal::None));
        assert!(app.worktree_sync.in_progress);
        assert!(app.worktree_sync.awaiting.is_none());
    }

    /// `confirm_sync_base` with an empty queue (a lost invariant) leaves the
    /// parked run intact instead of silently taking + dropping it.
    #[test]
    fn confirm_sync_base_with_empty_queue_keeps_run_parked() {
        let mut app = app_with_sessions(1);
        app.worktree_sync.awaiting = Some(sync_state::PendingSyncRun {
            worktrees: vec![(
                app.sessions[0].info.id,
                PathBuf::from("/tmp/empty-queue-wt"),
                PathBuf::from("/tmp/empty-queue-repo"),
            )],
            queue: Vec::new(),
            chosen: HashMap::new(),
            host: None,
        });

        app.confirm_sync_base("origin".to_string());

        assert!(
            app.worktree_sync.awaiting.is_some(),
            "the parked run is preserved, not dropped"
        );
        assert!(!app.worktree_sync.in_progress);
    }

    /// Esc in the picker drops the whole parked run — nothing syncs.
    #[test]
    fn sync_base_picker_esc_cancels_run() {
        let mut app = app_with_sessions(1);
        let repo = PathBuf::from("/tmp/cancel-remote-repo");
        app.worktree_sync.awaiting = Some(sync_state::PendingSyncRun {
            worktrees: vec![(
                app.sessions[0].info.id,
                PathBuf::from("/tmp/cancel-remote-wt"),
                repo.clone(),
            )],
            queue: vec![(repo, vec!["fork".to_string(), "origin".to_string()])],
            chosen: HashMap::new(),
            host: None,
        });
        app.modal = modals::Modal::SyncBasePicker(modals::SyncBasePickerModal {
            repo_name: "cancel-remote-repo".to_string(),
            remotes: vec!["fork".to_string(), "origin".to_string()],
            index: 0,
        });

        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::None));
        assert!(!app.worktree_sync.in_progress);
        assert!(app.worktree_sync.awaiting.is_none());
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.text, "Sync cancelled");
    }

    /// With two multi-remote repos the pickers chain: Enter on the first
    /// opens the second, Enter on the second launches the full run.
    #[test]
    fn sync_base_picker_queue_advances_across_repos() {
        let mut app = app_with_sessions(1);
        let repo_a = PathBuf::from("/tmp/queue-repo-a");
        let repo_b = PathBuf::from("/tmp/queue-repo-b");
        let remotes = vec!["fork".to_string(), "origin".to_string()];
        app.worktree_sync.awaiting = Some(sync_state::PendingSyncRun {
            worktrees: vec![
                (
                    app.sessions[0].info.id,
                    PathBuf::from("/tmp/queue-wt-a"),
                    repo_a.clone(),
                ),
                (
                    app.sessions[0].info.id,
                    PathBuf::from("/tmp/queue-wt-b"),
                    repo_b.clone(),
                ),
            ],
            queue: vec![(repo_a, remotes.clone()), (repo_b.clone(), remotes.clone())],
            chosen: HashMap::new(),
            host: None,
        });
        app.modal = modals::Modal::SyncBasePicker(modals::SyncBasePickerModal {
            repo_name: "queue-repo-a".to_string(),
            remotes,
            index: 0,
        });

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        match app.modal {
            modals::Modal::SyncBasePicker(ref sb) => {
                assert_eq!(sb.repo_name, "queue-repo-b", "second repo's picker opens");
                assert_eq!(sb.index, 1, "each picker re-preselects independently");
            }
            ref other => panic!("expected the second sync base picker, got {other:?}"),
        }
        assert!(!app.worktree_sync.in_progress);

        // Move off the preselected `origin` onto `fork`, then confirm.
        app.handle_key(KeyCode::Up, KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
        assert!(app.worktree_sync.in_progress);
        assert_eq!(app.worktree_sync.pending, 2);
        assert_eq!(
            app.db.get_sync_base_remote(&repo_b).unwrap().as_deref(),
            Some("fork")
        );
    }

    #[test]
    fn start_sync_with_no_worktrees_in_active_project_shows_info() {
        let mut app = app_with_sessions(1);
        // Session has no worktrees
        assert!(app.sessions[0].info.worktrees.is_empty());
        app.start_sync();
        assert!(!app.worktree_sync.in_progress);
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Info);
        assert_eq!(msg.text, "No worktrees to sync");
    }

    #[test]
    fn start_sync_only_syncs_active_session() {
        let mut app = app_with_sessions(1);
        // Active session (index 0) has no worktrees by default.
        // Add a second session with a worktree — it should NOT be synced.
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut other = Session::stub("other-session", &backend_arc, &provider);
        other.info.worktrees = vec![WorktreeInfo {
            repo_path: PathBuf::from("/tmp/other-repo"),
            worktree_path: PathBuf::from("/tmp/other-wt"),
            branch: "other-branch".to_string(),
        }];
        app.sessions.push(other);
        // active_index is 0 (no worktrees), so sync should find nothing
        app.start_sync();
        assert!(!app.worktree_sync.in_progress);
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.text, "No worktrees to sync");
    }

    #[test]
    fn start_sync_ignores_inactive_session_worktrees() {
        let mut app = app_with_sessions(1);
        // Give the active session (index 0) a worktree.
        app.sessions[0].info.worktrees = vec![WorktreeInfo {
            repo_path: PathBuf::from("/tmp/active-repo"),
            worktree_path: PathBuf::from("/tmp/active-wt"),
            branch: "active-branch".to_string(),
        }];
        // Add an inactive session with its own worktree.
        let backend_arc = stub_backend_arc();
        let provider = stub_provider();
        let mut other = Session::stub("inactive-session", &backend_arc, &provider);
        other.info.worktrees = vec![WorktreeInfo {
            repo_path: PathBuf::from("/tmp/inactive-repo"),
            worktree_path: PathBuf::from("/tmp/inactive-wt"),
            branch: "inactive-branch".to_string(),
        }];
        app.sessions.push(other);
        // Only the active session's 1 worktree should be synced, not 2.
        app.start_sync();
        let run = app.worktree_sync.awaiting.as_ref().unwrap();
        assert_eq!(run.worktrees.len(), 1);
        assert_eq!(run.worktrees[0].2, PathBuf::from("/tmp/active-repo"));
    }

    #[test]
    fn tick_increments_tick_count() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        assert_eq!(app.metrics.tick_count, 0);
        app.tick();
        assert_eq!(app.metrics.tick_count, 1);
        app.tick();
        assert_eq!(app.metrics.tick_count, 2);
    }

    #[test]
    fn perf_hook_states_cached_across_idle_ticks() {
        // `refresh_session_statuses` reloads the persisted hook columns only
        // when the DB's `data_version` moves. With no external writer, the first
        // tick loads and every subsequent idle tick reuses the cache — so the
        // expensive sessions-table scan no longer runs ~100×/s. See
        // docs/PERFORMANCE.md (ADR-P2).
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        assert_eq!(app.perf_counters().hook_state_loads, 0);
        for _ in 0..5 {
            app.tick();
        }
        assert_eq!(
            app.perf_counters().hook_state_loads,
            1,
            "only the first tick loads; idle ticks reuse the cache"
        );
    }

    #[test]
    fn perf_hook_states_reload_on_external_change() {
        // An *external* `session signal` commits on another connection, bumping
        // this connection's `data_version` — which must invalidate the cache and
        // trigger exactly one fresh load. A file-backed DB is required so a
        // second connection shares the same database.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path()).unwrap();
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), db);

        app.tick();
        let after_first = app.perf_counters().hook_state_loads;
        assert_eq!(after_first, 1);

        // No external write yet: another idle tick stays cached.
        app.tick();
        assert_eq!(app.perf_counters().hook_state_loads, 1);

        // A different connection commits → `data_version` moves. The pragma
        // read itself is throttled (ADR-P10), so tick past a full
        // `HOOK_VERSION_CHECK_TICKS` window for the change to be observed.
        let db2 = Database::open(tmp.path()).unwrap();
        db2.set_session_counter(7).unwrap();

        for _ in 0..HOOK_VERSION_CHECK_TICKS {
            app.tick();
        }
        assert_eq!(
            app.perf_counters().hook_state_loads,
            2,
            "an external commit must invalidate the cache exactly once"
        );
    }

    #[tokio::test]
    async fn perf_data_version_read_is_throttled() {
        // The status refresh's `PRAGMA data_version` runs on the throttle
        // cadence (~100 ms), not per ~10 ms tick: 100 idle ticks = the forced
        // first check + one per `HOOK_VERSION_CHECK_TICKS` window (ADR-P10).
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        for _ in 0..100 {
            app.tick();
        }
        let checks = app.perf_counters().data_version_checks;
        assert!(
            checks <= 100 / HOOK_VERSION_CHECK_TICKS + 1,
            "expected ≤{} throttled checks over 100 ticks, got {checks}",
            100 / HOOK_VERSION_CHECK_TICKS + 1
        );
        assert!(
            checks >= 100 / HOOK_VERSION_CHECK_TICKS,
            "still polls each window"
        );
    }

    #[tokio::test]
    async fn perf_agent_meta_cached_across_idle_ticks() {
        // The agent title/notification mutexes are re-read only when the
        // reader thread bumped the meta generation: one initial sync per
        // session, then flat across idle ticks — not 2·N locks per tick
        // (ADR-P10).
        let mut app = app_with_sessions(2);
        for _ in 0..50 {
            app.tick();
        }
        assert_eq!(
            app.perf_counters().agent_meta_syncs,
            2,
            "one initial sync per session, then cached"
        );
    }

    #[tokio::test]
    async fn perf_agent_meta_resyncs_on_change() {
        let mut app = app_with_sessions(1);
        app.tick();
        assert_eq!(app.perf_counters().agent_meta_syncs, 1);

        // The reader thread writes a new title → next tick re-reads it.
        app.sessions[0].bump_meta_gen_for_test("build: cargo");
        app.tick();
        assert_eq!(app.perf_counters().agent_meta_syncs, 2);
        assert_eq!(
            app.sessions[0].info.agent_activity.as_deref(),
            Some("build: cargo"),
            "the fresh title landed in the session info"
        );

        // And goes quiet again.
        app.tick();
        assert_eq!(app.perf_counters().agent_meta_syncs, 2);
    }

    #[test]
    fn perf_external_poll_never_reloads_without_external_writes() {
        // With no *other* connection writing, `PRAGMA data_version` never moves,
        // so the cheap poll never escalates to a full shared-state reload. The
        // ratio of reloads to checks is the "is the poll doing real work?"
        // signal; here it must be zero.
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        for _ in 0..8 {
            app.tick();
        }
        assert_eq!(
            app.perf_counters().external_poll_reloads,
            0,
            "no external writes ⇒ no shared-state reload"
        );
        assert!(
            app.perf_counters().external_poll_reloads <= app.perf_counters().external_poll_checks,
            "reloads are a subset of checks"
        );
    }

    #[test]
    fn finish_sync_all_synced_shows_success() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let id = SessionId::default();
        app.worktree_sync.completed = vec![
            (id, git::SyncResult::Synced),
            (SessionId::default(), git::SyncResult::Synced),
        ];
        app.finish_sync();
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Success);
        assert!(msg.text.contains("2 worktree(s) synced"));
    }

    #[test]
    fn finish_sync_with_errors_shows_error() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.worktree_sync.completed = vec![(
            SessionId::default(),
            git::SyncResult::Error("fetch failed".into()),
        )];
        app.finish_sync();
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Error);
        assert!(msg.text.contains("Sync failed"));
        assert!(msg.text.contains("fetch failed"));
    }

    #[test]
    fn finish_sync_with_conflicts_shows_info() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.worktree_sync.completed = vec![
            (SessionId::default(), git::SyncResult::Synced),
            (
                SessionId::default(),
                git::SyncResult::Conflict {
                    base_ref: "origin/main".into(),
                },
            ),
        ];
        app.finish_sync();
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Info);
        assert!(msg.text.contains("1 synced"));
        assert!(msg.text.contains("1 conflict"));
    }

    #[test]
    fn finish_sync_errors_take_priority_over_conflicts() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.worktree_sync.completed = vec![
            (
                SessionId::default(),
                git::SyncResult::Conflict {
                    base_ref: "origin/main".into(),
                },
            ),
            (
                SessionId::default(),
                git::SyncResult::Error("network error".into()),
            ),
        ];
        app.finish_sync();
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Error);
        assert!(msg.text.contains("network error"));
    }

    #[test]
    fn poll_git_stats_applies_result_to_matching_session() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;

        let tx = app.git_stats.start();
        let stats = crate::session::GitStats {
            files_changed: 3,
            insertions: 10,
            deletions: 2,
            untracked: 0,
            dirty: true,
            ahead: 1,
            behind: 0,
        };
        tx.send((sid, Some(stats.clone()))).unwrap();

        app.poll_git_stats();

        assert_eq!(app.sessions[0].info.git_stats, Some(stats));
        assert!(!app.git_stats.in_progress());
    }

    #[test]
    fn poll_git_stats_disconnected_clears_guard() {
        let mut app = app_with_sessions(1);
        drop(app.git_stats.start()); // worker died without delivering

        app.poll_git_stats();

        assert!(!app.git_stats.in_progress());
    }

    #[test]
    fn poll_git_stats_empty_is_noop() {
        let mut app = app_with_sessions(1);
        let _tx = app.git_stats.start();

        // No result yet: guard stays set, rx retained for the next poll.
        app.poll_git_stats();

        assert!(app.git_stats.in_progress());
    }

    #[test]
    fn poll_metrics_refresh_restores_sys_and_applies_metrics() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;

        // Simulate the worker having taken `sys`.
        app.metrics.sys = None;

        let tx = app.metrics_refresh.start();
        let agent_metrics = crate::session::AgentMetrics {
            model_display_name: Some("Opus".into()),
            ..Default::default()
        };
        tx.send(MetricsRefresh {
            sys: sysinfo::System::new(),
            metrics: crate::ui::info_panel::SystemMetrics {
                cpu_percent: 42.0,
                memory_used: 100,
                memory_total: 200,
                session_cpu_percent: 5.0,
            },
            agent_metrics: vec![(sid, agent_metrics)],
        })
        .unwrap();

        app.poll_metrics_refresh();

        assert!(app.metrics.sys.is_some());
        // `SystemMetrics` has no `PartialEq`; assert via a representative field.
        assert_eq!(app.metrics.system_metrics.cpu_percent, 42.0);
        assert_eq!(app.metrics.system_metrics.memory_used, 100);
        // `AgentMetrics` has no `PartialEq`; assert via a representative field.
        assert_eq!(
            app.sessions[0]
                .info
                .agent_metrics
                .as_ref()
                .and_then(|m| m.model_display_name.as_deref()),
            Some("Opus"),
        );
        assert!(!app.metrics_refresh.in_progress());
    }

    #[test]
    fn poll_metrics_refresh_disconnected_recreates_sys() {
        let mut app = app_with_sessions(0);
        app.metrics.sys = None;
        drop(app.metrics_refresh.start()); // worker died without returning `sys`

        app.poll_metrics_refresh();

        assert!(app.metrics.sys.is_some());
        assert!(!app.metrics_refresh.in_progress());
    }

    #[test]
    fn poll_worktree_create_continues_into_name_modal() {
        let mut app = app_with_sessions(0);
        let tx = app.worktree_create.start();
        let wt = WorktreeInfo {
            repo_path: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/repo/.worktrees/feat"),
            branch: "feat".into(),
        };
        tx.send(Ok(vec![wt])).unwrap();
        app.pending_worktree_create = Some(PendingWorktreeCreate {
            backend: None,
            normal_repos: vec![PathBuf::from("/other")],
            session_name: None, // no name yet → routes through the name modal
            base_branch: "main".into(),
            agent_pick: AgentPick::NotOpened,
        });

        app.poll_worktree_create();

        assert!(!app.worktree_create.in_progress());
        assert!(app.pending_worktree_create.is_none());
        // The non-worktree normal repo is carried into additional dirs.
        assert_eq!(
            app.new_session.additional_dirs,
            vec![PathBuf::from("/other")]
        );
        assert!(matches!(app.modal, modals::Modal::SessionName(_)));
        assert!(app.new_session.spawn_config.is_some());
    }

    #[test]
    fn poll_worktree_create_error_sets_status() {
        let mut app = app_with_sessions(0);
        let tx = app.worktree_create.start();
        tx.send(Err("branch exists".into())).unwrap();
        app.pending_worktree_create = Some(PendingWorktreeCreate {
            backend: None,
            normal_repos: vec![],
            session_name: None,
            base_branch: "main".into(),
            agent_pick: AgentPick::NotOpened,
        });

        app.poll_worktree_create();

        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Error);
        assert!(msg.text.contains("branch exists"));
        assert!(!app.worktree_create.in_progress());
        assert!(app.pending_worktree_create.is_none());
    }

    /// ADR-P8: opening a review dispatches the git work to a background
    /// worker; the toggle path itself must leave the diff empty (loading).
    #[tokio::test]
    async fn perf_review_open_never_builds_on_ui_thread() {
        let mut app = app_with_sessions(1);
        app.sessions[0].info.cwd = Some(std::env::temp_dir());

        app.toggle_code_review();

        assert_eq!(app.perf_counters().review_builds_dispatched, 1);
        assert!(app.review_build.in_progress());
        let cr = app.active_review().expect("the pane opens instantly");
        assert!(cr.loading, "opens in the loading state");
        assert!(cr.files.is_empty(), "no git work ran on the UI thread");
        assert_eq!(app.focus, InputFocus::CodeReview);
    }

    /// The worker's result lands via the tick poll: loading clears, the rows
    /// rebuild from the delivered files, and the applied counter bumps.
    #[test]
    fn perf_review_build_result_applied_via_poll() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        let built = code_review::CodeReviewState::for_test(sid, 2);
        let mut pending = code_review::CodeReviewState::for_test(sid, 0);
        pending.loading = true;
        let repos = built.repos.clone();
        app.code_reviews.insert(sid, pending);

        let tx = app.review_build.start();
        tx.send(code_review::ReviewBuildResult {
            session_id: sid,
            elapsed_ms: 7,
            kind: code_review::ReviewBuildKind::Open {
                repos,
                commits: Vec::new(),
                target: code_review::ReviewTarget::Branch,
                files: built.files.clone(),
            },
        })
        .unwrap();
        app.poll_review_build();

        let cr = &app.code_reviews[&sid];
        assert!(!cr.loading);
        assert_eq!(cr.files.len(), 2);
        assert!(!cr.rows.is_empty(), "rows rebuilt from the delivered diff");
        assert_eq!(app.perf_counters().review_builds_applied, 1);
        assert!(!app.review_build.in_progress());
    }

    /// A completed build reconciles persisted "reviewed" marks against the
    /// fresh diff: a matching fingerprint survives, a stale one is deleted from
    /// the DB with a summary toast, and a legacy NULL row is treated as valid
    /// once and backfilled with the computed fingerprint.
    #[test]
    fn review_build_reconciles_stale_marks_with_toast() {
        use crate::session::review::{file_fingerprint, hunk_fingerprint};
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        let built = code_review::CodeReviewState::for_test(sid, 2);
        let fp0 = file_fingerprint(&built.files[0]);
        app.db
            .toggle_review_mark(sid, "src/f0.rs", None, &fp0)
            .unwrap();
        app.db
            .toggle_review_mark(sid, "src/f1.rs", None, "stale-fp")
            .unwrap();
        app.db
            .insert_review_mark_without_fingerprint(sid, "src/f0.rs", Some(0))
            .unwrap();

        let mut pending = code_review::CodeReviewState::for_test(sid, 0);
        pending.loading = true;
        let repos = built.repos.clone();
        app.code_reviews.insert(sid, pending);
        let tx = app.review_build.start();
        tx.send(code_review::ReviewBuildResult {
            session_id: sid,
            elapsed_ms: 1,
            kind: code_review::ReviewBuildKind::Open {
                repos,
                commits: Vec::new(),
                target: code_review::ReviewTarget::Branch,
                files: built.files.clone(),
            },
        })
        .unwrap();
        app.poll_review_build();

        let mut marks = app.db.list_review_marks(sid).unwrap();
        marks.sort();
        assert_eq!(
            marks,
            vec![
                ("src/f0.rs".to_string(), None, Some(fp0)),
                // Legacy hunk row survived and got the computed fingerprint.
                (
                    "src/f0.rs".to_string(),
                    Some(0),
                    Some(hunk_fingerprint(&built.files[0].hunks[0])),
                ),
            ],
            "stale f1 mark deleted; f0 marks intact"
        );
        // The view reloaded the reconciled marks.
        let cr = &app.code_reviews[&sid];
        assert!(cr.reviewed_files.contains("src/f0.rs"));
        assert!(!cr.reviewed_files.contains("src/f1.rs"));
        // The toast summarizes what was cleared.
        let msg = app.status_message.as_ref().expect("toast fired");
        assert!(
            msg.text
                .contains("1 reviewed mark cleared (content changed)"),
            "got: {}",
            msg.text
        );
    }

    /// `F5` rebuilds the current target off-thread; applying the result keeps
    /// the selection where it was when the rows still support it, and falls
    /// back within bounds when the selected file vanished from the diff.
    #[test]
    fn review_reload_preserves_selection_and_survives_missing_file() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        let state = code_review::CodeReviewState::for_test(sid, 2);
        let files = state.files.clone();
        let target = state.target.clone();
        app.code_reviews.insert(sid, state);

        // Select f1's diff line.
        let pos = app.code_reviews[&sid]
            .rows
            .iter()
            .position(|r| matches!(r, code_review::ReviewRow::Line(1, _, _)))
            .unwrap();
        app.code_reviews.get_mut(&sid).unwrap().selected = pos;

        // Reload with the identical diff: the exact position is kept.
        let tx = app.review_build.start();
        tx.send(code_review::ReviewBuildResult {
            session_id: sid,
            elapsed_ms: 1,
            kind: code_review::ReviewBuildKind::Reload {
                target: target.clone(),
                files: files.clone(),
            },
        })
        .unwrap();
        app.poll_review_build();
        assert_eq!(app.code_reviews[&sid].selected, pos);

        // Reload with f1 gone: the selection clamps into the new rows.
        let tx = app.review_build.start();
        tx.send(code_review::ReviewBuildResult {
            session_id: sid,
            elapsed_ms: 1,
            kind: code_review::ReviewBuildKind::Reload {
                target,
                files: files[..1].to_vec(),
            },
        })
        .unwrap();
        app.poll_review_build();
        let cr = &app.code_reviews[&sid];
        assert!(cr.selected < cr.rows.len());
    }

    /// With the Unreviewed filter active, `}`/`{` skip reviewed files' headers
    /// and marking a file reviewed auto-advances to the next unreviewed file.
    #[test]
    fn review_unreviewed_filter_scopes_jumps_and_auto_advances() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        let mut state = code_review::CodeReviewState::for_test(sid, 3);
        state.filter = code_review::ReviewFilter::Unreviewed;
        state.reviewed_files.insert("src/f1.rs".into());
        state.rebuild_rows();
        app.code_reviews.insert(sid, state);

        // `}` from f0's header skips reviewed f1 straight to f2.
        app.cr_jump_file(true);
        let cr = &app.code_reviews[&sid];
        assert!(
            matches!(cr.rows[cr.selected], code_review::ReviewRow::FileHeader(2)),
            "landed on {:?}",
            cr.rows[cr.selected]
        );

        // Marking f2 reviewed advances (wrapping) to f0 — the only file left.
        app.cr_toggle_reviewed(false);
        let cr = &app.code_reviews[&sid];
        assert!(
            matches!(cr.rows[cr.selected], code_review::ReviewRow::FileHeader(0)),
            "auto-advanced to {:?}",
            cr.rows[cr.selected]
        );
    }

    /// `(`/`)` step comment rows across files, wrapping, and reach a comment
    /// hidden inside a folded (reviewed) file by unfolding it; `@` opens the
    /// popup and Enter jumps to the chosen comment.
    #[test]
    fn review_comment_navigation_wraps_and_unfolds() {
        use crate::session::review::{Classification, CommentAnchor, ReviewComment, Side};
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        let mut state = code_review::CodeReviewState::for_test(sid, 2);
        let comment = |id: i64, file: &str| ReviewComment {
            id,
            session_id: sid,
            anchor: CommentAnchor::Line {
                file: file.into(),
                side: Side::New,
                line: 1,
                line_end: None,
            },
            classification: Classification::Note,
            body: format!("c{id}"),
            created_at: 0,
            updated_at: 0,
        };
        state.comments = vec![comment(1, "src/f0.rs"), comment(2, "src/f1.rs")];
        // f1 is reviewed → folded, so its comment row is hidden until a jump
        // targets it.
        state.reviewed_files.insert("src/f1.rs".into());
        state.rebuild_rows();
        app.code_reviews.insert(sid, state);

        // From the top: `)` lands on C1, then C2 (unfolding f1), then wraps to C1.
        app.cr_jump_comment(true);
        assert_eq!(app.code_reviews[&sid].selected_comment_id(), Some(1));
        app.cr_jump_comment(true);
        assert_eq!(app.code_reviews[&sid].selected_comment_id(), Some(2));
        assert!(
            !app.code_reviews[&sid].is_file_folded("src/f1.rs"),
            "the jump unfolded the reviewed file"
        );
        app.cr_jump_comment(true);
        assert_eq!(
            app.code_reviews[&sid].selected_comment_id(),
            Some(1),
            "wraps past the end"
        );
        // `(` steps back (wrapping to the last).
        app.cr_jump_comment(false);
        assert_eq!(app.code_reviews[&sid].selected_comment_id(), Some(2));

        // `@` popup: entries in display order, Enter jumps.
        app.focus = InputFocus::CodeReview;
        app.cr_open_comment_picker();
        {
            let cr = app.code_reviews.get_mut(&sid).unwrap();
            let picker = cr.comment_picker.as_ref().unwrap();
            assert_eq!(picker.entries, vec![1, 2]);
        }
        app.handle_code_review_key(KeyCode::Char('k'), KeyModifiers::NONE);
        app.handle_code_review_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.code_reviews[&sid].comment_picker.is_none());
        assert_eq!(app.code_reviews[&sid].selected_comment_id(), Some(1));
    }

    /// `=` cycles the diff context 3 → 10 → 25 → 3 and rebuilds through the
    /// shared worker; a cycle while a build is in flight is refused.
    #[tokio::test]
    async fn review_context_cycle_steps_and_dispatches_rebuild() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        app.code_reviews
            .insert(sid, code_review::CodeReviewState::for_test(sid, 1));
        assert_eq!(app.code_reviews[&sid].context, 3);

        app.cr_cycle_context();
        assert_eq!(app.code_reviews[&sid].context, 10);
        assert!(
            app.review_build.in_progress(),
            "the cycle rebuilds the diff with -U<n>"
        );

        // Build in flight → the next cycle is refused, context unchanged.
        app.cr_cycle_context();
        assert_eq!(app.code_reviews[&sid].context, 10);
    }

    /// The `V` range flow end to end through the key handler: start on a diff
    /// line, `j` extends the span, `c` opens the compose box carrying a range
    /// anchor; a fresh `V` + Esc cancels without composing.
    #[test]
    fn review_range_keys_extend_and_compose() {
        use crate::session::review::{CommentAnchor, DiffLine, DiffLineKind, Side};
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        let mut state = code_review::CodeReviewState::for_test(sid, 1);
        // A second added line so a span exists (for_test files have one).
        state.files[0].hunks[0].lines.push(DiffLine {
            kind: DiffLineKind::Add,
            old_no: None,
            new_no: Some(2),
            text: "y".into(),
        });
        state.rebuild_rows();
        // Rows: 0 FileHeader, 1 HunkHeader, 2 Line(new:1), 3 Line(new:2).
        state.selected = 2;
        app.code_reviews.insert(sid, state);
        app.focus = InputFocus::CodeReview;

        app.handle_code_review_key(KeyCode::Char('V'), KeyModifiers::SHIFT);
        assert!(app.code_reviews[&sid].range.is_some(), "V starts a range");
        app.handle_code_review_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.code_reviews[&sid].selected, 3, "j extends the span");
        // `j` at the file's last line stays put (same file only).
        app.handle_code_review_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.code_reviews[&sid].selected, 3);

        app.handle_code_review_key(KeyCode::Char('c'), KeyModifiers::NONE);
        let cr = &app.code_reviews[&sid];
        assert!(cr.range.is_none(), "compose consumes the range");
        assert_eq!(
            cr.compose.as_ref().map(|c| c.anchor.clone()),
            Some(CommentAnchor::Line {
                file: "src/f0.rs".into(),
                side: Side::New,
                line: 1,
                line_end: Some(2),
            })
        );

        // Esc cancels a fresh range without opening the compose box.
        app.handle_code_review_key(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_code_review_key(KeyCode::Char('V'), KeyModifiers::SHIFT);
        assert!(app.code_reviews[&sid].range.is_some());
        app.handle_code_review_key(KeyCode::Esc, KeyModifiers::NONE);
        let cr = &app.code_reviews[&sid];
        assert!(cr.range.is_none() && cr.compose.is_none());
    }

    /// Committed (`Tab`) searches join a per-session history that survives
    /// closing the review; `↑`/`↓` in the find bar recall older/newer entries,
    /// the first `↑` stashing the live query and `↓` past the newest restoring
    /// it. Re-committing a query moves it to the newest slot.
    #[test]
    fn review_search_history_recall() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        app.code_reviews
            .insert(sid, code_review::CodeReviewState::for_test(sid, 1));
        app.focus = InputFocus::CodeReview;

        fn commit_query(app: &mut App, q: &str) {
            app.handle_code_review_key(KeyCode::Char('/'), KeyModifiers::NONE);
            for c in q.chars() {
                app.handle_code_review_key(KeyCode::Char(c), KeyModifiers::NONE);
            }
            app.handle_code_review_key(KeyCode::Tab, KeyModifiers::NONE);
            app.handle_code_review_key(KeyCode::Esc, KeyModifiers::NONE);
        }
        commit_query(&mut app, "alpha");
        commit_query(&mut app, "beta");
        assert_eq!(app.review_search_history[&sid], ["alpha", "beta"]);
        commit_query(&mut app, "alpha");
        assert_eq!(
            app.review_search_history[&sid],
            ["beta", "alpha"],
            "re-commit moves the query to the newest slot"
        );

        // The history outlives the review view itself (it closes on every
        // Send→Agent).
        app.close_code_review();
        app.code_reviews
            .insert(sid, code_review::CodeReviewState::for_test(sid, 1));
        app.focus = InputFocus::CodeReview;

        app.handle_code_review_key(KeyCode::Char('/'), KeyModifiers::NONE);
        app.handle_code_review_key(KeyCode::Char('x'), KeyModifiers::NONE);
        let query = |app: &App| {
            app.code_reviews[&sid]
                .search
                .as_ref()
                .unwrap()
                .query
                .clone()
        };
        app.handle_code_review_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(query(&app), "alpha", "↑ recalls the newest commit");
        app.handle_code_review_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(query(&app), "beta");
        app.handle_code_review_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(query(&app), "beta", "the oldest entry pins");
        app.handle_code_review_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(query(&app), "alpha");
        app.handle_code_review_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            query(&app),
            "x",
            "↓ past the newest restores the live query"
        );
    }

    /// `i` opens the read-only review-info popup; j scrolls it (the renderer
    /// clamps), Esc closes it without closing the review.
    #[test]
    fn review_info_popup_opens_scrolls_and_closes() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        app.code_reviews
            .insert(sid, code_review::CodeReviewState::for_test(sid, 1));
        app.focus = InputFocus::CodeReview;

        app.handle_code_review_key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(app.code_reviews[&sid].info_popup, Some(0));
        app.handle_code_review_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.code_reviews[&sid].info_popup, Some(1));
        app.handle_code_review_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.code_reviews[&sid].info_popup.is_none());
        assert!(
            app.code_reviews.contains_key(&sid),
            "Esc closed the popup, not the review"
        );
    }

    /// A sent review watches its session: the first Working → Idle edge after
    /// the send toasts a re-review nudge exactly once; `nudge_on_idle = false`
    /// consumes the edge silently.
    #[test]
    fn review_nudge_fires_once_when_watched_agent_finishes() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        app.status_message = None;
        app.review_nudge_watch.insert(sid, SessionStatus::Idle);

        // The send usually lands while the agent is still idle — no edge yet.
        app.nudge_review_on_idle();
        assert!(app.status_message.is_none());

        // Idle → Working: tracked, still no nudge.
        app.sessions[0].info.status = SessionStatus::Working;
        app.nudge_review_on_idle();
        assert!(app.status_message.is_none());

        // Working → Idle: the nudge fires and consumes the watch.
        app.sessions[0].info.status = SessionStatus::Idle;
        app.nudge_review_on_idle();
        let msg = app.status_message.take().expect("nudge fired");
        assert!(msg.text.contains("F7 to re-review"), "got: {}", msg.text);
        assert!(app.review_nudge_watch.is_empty(), "one nudge per send");

        // Later idle edges without a fresh send stay quiet.
        app.sessions[0].info.status = SessionStatus::Working;
        app.nudge_review_on_idle();
        app.sessions[0].info.status = SessionStatus::Idle;
        app.nudge_review_on_idle();
        assert!(app.status_message.is_none());

        // Setting off: the edge is consumed without a toast.
        app.review_settings.nudge_on_idle = false;
        app.review_nudge_watch.insert(sid, SessionStatus::Working);
        app.nudge_review_on_idle();
        assert!(app.status_message.is_none());
        assert!(app.review_nudge_watch.is_empty());
    }

    /// `E` on a remote (SSH) session's review refuses the editor round-trip —
    /// the editor runs on this machine, the files don't live here.
    #[test]
    fn review_editor_is_local_only() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        let mut state = code_review::CodeReviewState::for_test(sid, 1);
        state.host = Some(crate::session::HostDef::default());
        state.selected = 2; // a Line row
        app.code_reviews.insert(sid, state);
        app.focus = InputFocus::CodeReview;

        app.handle_code_review_key(KeyCode::Char('E'), KeyModifiers::SHIFT);
        assert!(app.take_pending_editor().is_none());
        let msg = app.status_message.take().expect("toast");
        assert!(msg.text.contains("local only"), "got: {}", msg.text);
    }

    /// Toggling a reviewed mark stores the current semantic fingerprint, so
    /// the next build can validate it.
    #[test]
    fn review_toggle_stores_fingerprint() {
        use crate::session::review::file_fingerprint;
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        let state = code_review::CodeReviewState::for_test(sid, 1);
        let expect = file_fingerprint(&state.files[0]);
        app.code_reviews.insert(sid, state);
        // Selection starts on the file header; `r` marks the file.
        app.cr_toggle_reviewed(false);
        let marks = app.db.list_review_marks(sid).unwrap();
        assert_eq!(marks, vec![("src/f0.rs".to_string(), None, Some(expect))]);
    }

    /// A build whose review was closed before delivery is dropped without
    /// panicking or resurrecting state.
    #[test]
    fn review_build_for_closed_review_is_dropped() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;
        let tx = app.review_build.start();
        tx.send(code_review::ReviewBuildResult {
            session_id: sid,
            elapsed_ms: 3,
            kind: code_review::ReviewBuildKind::Retarget {
                target: code_review::ReviewTarget::Working,
                files: Vec::new(),
            },
        })
        .unwrap();

        app.poll_review_build();

        assert!(app.code_reviews.is_empty(), "no state resurrected");
        assert_eq!(app.perf_counters().review_builds_applied, 0);
        assert!(!app.review_build.in_progress());
    }

    /// ADR-P12: the `w` flow's branch selection dispatches the git listing to
    /// a background worker; the open path itself must leave the selector in
    /// its loading state (no git subprocess on the UI thread).
    #[tokio::test]
    async fn perf_branch_selection_never_lists_on_ui_thread() {
        let mut app = app_with_sessions(0);
        app.new_session.repo_path = Some(std::env::temp_dir());

        app.start_branch_selection();

        assert_eq!(app.perf_counters().branch_loads_dispatched, 1);
        assert!(app.branch_load.in_progress());
        match app.modal {
            modals::Modal::BranchSelector(ref bs) => {
                assert!(bs.loading, "opens in the loading state");
                assert!(bs.branches.is_empty(), "no git work ran on the UI thread");
            }
            ref other => panic!("expected the branch selector, got {other:?}"),
        }
        // The origin fetch runs concurrently; its completion signal is parked
        // for the worktree-create worker to wait on.
        assert!(app.new_session.fetch_done.is_some());
    }

    /// The worker's branch list lands via the tick poll: loading clears and
    /// the applied counter bumps.
    #[test]
    fn perf_branch_load_result_applied_via_poll() {
        let mut app = app_with_sessions(0);
        app.modal = modals::Modal::BranchSelector(modals::BranchSelectorModal {
            index: 0,
            branches: Vec::new(),
            filter: Default::default(),
            loading: true,
        });
        let tx = app.branch_load.start();
        tx.send(Ok(vec!["origin/main".into(), "main".into()]))
            .unwrap();

        app.poll_branch_load();

        match app.modal {
            modals::Modal::BranchSelector(ref bs) => {
                assert!(!bs.loading);
                assert_eq!(
                    bs.branches,
                    vec!["origin/main".to_string(), "main".to_string()]
                );
            }
            ref other => panic!("expected the branch selector, got {other:?}"),
        }
        assert_eq!(app.perf_counters().branch_loads_applied, 1);
        assert!(!app.branch_load.in_progress());
    }

    /// A failed listing closes the selector, surfaces the error, and aborts
    /// the pending worktree flow (mirroring the selector's Esc).
    #[test]
    fn branch_load_error_closes_selector_and_clears_flow() {
        let mut app = app_with_sessions(0);
        app.new_session.repo_path = Some(PathBuf::from("/repo"));
        app.new_session.all_repos = Some(vec![PathBuf::from("/repo")]);
        app.new_session.normal_repos = vec![PathBuf::from("/other")];
        app.modal = modals::Modal::BranchSelector(modals::BranchSelectorModal {
            index: 0,
            branches: Vec::new(),
            filter: Default::default(),
            loading: true,
        });
        let tx = app.branch_load.start();
        tx.send(Err("No branches found in repository".into()))
            .unwrap();

        app.poll_branch_load();

        assert!(matches!(app.modal, modals::Modal::None));
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Error);
        assert!(app.new_session.repo_path.is_none());
        assert!(app.new_session.all_repos.is_none());
        assert!(app.new_session.normal_repos.is_empty());
    }

    /// A list whose selector was cancelled (Esc) before delivery is dropped,
    /// not applied (and not counted).
    #[test]
    fn branch_load_for_cancelled_selector_is_dropped() {
        let mut app = app_with_sessions(0);
        let tx = app.branch_load.start();
        tx.send(Ok(vec!["main".into()])).unwrap();

        app.poll_branch_load();

        assert!(matches!(app.modal, modals::Modal::None));
        assert_eq!(app.perf_counters().branch_loads_applied, 0);
        assert!(!app.branch_load.in_progress());
    }

    /// ADR-P12: with the session name known and >1 agents, the agent picker
    /// opens immediately over the in-flight worktree creation instead of
    /// waiting for it.
    #[tokio::test]
    async fn perf_worktree_confirm_opens_agent_picker_during_create() {
        let mut app = app_with_sessions(0);

        app.spawn_worktree_session(
            &[PathBuf::from("/repo")],
            "feat",
            "main",
            Some("sess".into()),
        );

        assert!(app.worktree_create.in_progress());
        assert!(matches!(app.modal, modals::Modal::AgentPicker(_)));
        assert!(matches!(
            app.pending_worktree_create.as_ref().unwrap().agent_pick,
            AgentPick::Open
        ));
    }

    /// The user picked an agent before the create delivered: the choice is
    /// parked and the spawn dispatches straight from the poll.
    #[tokio::test]
    async fn agent_choice_during_create_spawns_on_delivery() {
        let mut app = app_with_sessions(0);
        let tx = app.worktree_create.start();
        app.pending_worktree_create = Some(PendingWorktreeCreate {
            backend: None,
            normal_repos: vec![],
            session_name: Some("sess".into()),
            base_branch: "main".into(),
            agent_pick: AgentPick::Chosen("claude".into()),
        });
        tx.send(Ok(vec![WorktreeInfo {
            repo_path: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/repo/.worktrees/feat"),
            branch: "feat".into(),
        }]))
        .unwrap();

        app.poll_worktree_create();

        assert!(app.session_spawn.in_progress(), "spawn dispatched");
        assert_eq!(
            app.pending_session_spawn.as_ref().map(|p| p.agent.as_str()),
            Some("claude")
        );
    }

    /// The create delivered while the picker is still open: the spawn inputs
    /// are parked for `confirm_agent_picker` and the picker stays up.
    #[test]
    fn worktree_done_while_picker_open_stashes_spawn() {
        let mut app = app_with_sessions(0);
        app.modal =
            modals::Modal::AgentPicker(crate::ui::agent_picker_modal::AgentPickerState::default());
        let tx = app.worktree_create.start();
        app.pending_worktree_create = Some(PendingWorktreeCreate {
            backend: None,
            normal_repos: vec![],
            session_name: Some("sess".into()),
            base_branch: "main".into(),
            agent_pick: AgentPick::Open,
        });
        tx.send(Ok(vec![WorktreeInfo {
            repo_path: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/repo/.worktrees/feat"),
            branch: "feat".into(),
        }]))
        .unwrap();

        app.poll_worktree_create();

        assert!(matches!(app.modal, modals::Modal::AgentPicker(_)));
        assert_eq!(app.new_session.spawn_name.as_deref(), Some("sess"));
        assert!(app.new_session.spawn_config.is_some());
        assert_eq!(app.new_session.spawn_worktrees.len(), 1);
        assert!(!app.session_spawn.in_progress(), "spawn waits for the pick");
    }

    /// Esc on the overlapping picker cancels the pending create: the
    /// delivered worktrees are dropped instead of spawning a session.
    #[test]
    fn agent_picker_esc_during_create_cancels_pending() {
        let mut app = app_with_sessions(0);
        app.modal =
            modals::Modal::AgentPicker(crate::ui::agent_picker_modal::AgentPickerState::default());
        let tx = app.worktree_create.start();
        app.pending_worktree_create = Some(PendingWorktreeCreate {
            backend: None,
            normal_repos: vec![],
            session_name: Some("sess".into()),
            base_branch: "main".into(),
            agent_pick: AgentPick::Open,
        });

        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            app.pending_worktree_create.as_ref().unwrap().agent_pick,
            AgentPick::Cancelled
        ));

        tx.send(Ok(vec![WorktreeInfo {
            repo_path: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/repo/.worktrees/feat"),
            branch: "feat".into(),
        }]))
        .unwrap();
        app.poll_worktree_create();

        assert!(!app.session_spawn.in_progress(), "nothing spawns");
        assert!(matches!(app.modal, modals::Modal::None));
        assert!(app.pending_worktree_create.is_none());
    }

    #[test]
    fn note_slow_op_applies_record_threshold() {
        let mut app = app_with_sessions(0);
        app.note_slow_op("fast", SLOW_OP_RECORD_MS - 1);
        assert!(
            app.metrics.timings.slow_ops.iter_recent().next().is_none(),
            "below the record threshold nothing lands in the ring"
        );
        app.note_slow_op("slow", SLOW_OP_RECORD_MS);
        assert_eq!(
            app.metrics
                .timings
                .slow_ops
                .iter_recent()
                .next()
                .map(|o| o.name),
            Some("slow")
        );
    }

    /// The perf snapshot write bumps *other* connections' `data_version`
    /// (forcing their shared-state reload), so a default-config idle instance
    /// must never publish it — only FRIRING_PERF_LOG or an open HUD opts in
    /// (ADR-P11).
    #[tokio::test]
    async fn perf_snapshot_published_only_while_timing_active() {
        let mut app = app_with_sessions(0);
        app.perf_log_env = false;
        for _ in 0..=PERF_WINDOW_TICKS {
            app.tick();
        }
        assert_eq!(
            app.db.get_perf_snapshot().unwrap(),
            None,
            "no snapshot churn without opt-in"
        );

        app.show_perf_hud = true;
        for _ in 0..=PERF_SNAPSHOT_TICKS {
            app.tick();
        }
        assert!(
            app.db.get_perf_snapshot().unwrap().is_some(),
            "an open HUD publishes the snapshot"
        );
    }

    /// Local backend that records capture/adopt interplay for the ADR-P9
    /// restore-prefetch gate: `capture_history` counts calls and returns
    /// recognizable bytes; `adopt` asserts it always receives a prefetched
    /// seed (never `None`, which would mean an inline capture on the
    /// sequential path).
    struct RecordingCaptureBackend {
        captures: std::sync::atomic::AtomicUsize,
        seeded_adopts: std::sync::atomic::AtomicUsize,
    }
    impl RecordingCaptureBackend {
        fn new() -> Self {
            Self {
                captures: std::sync::atomic::AtomicUsize::new(0),
                seeded_adopts: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    impl SessionBackend for RecordingCaptureBackend {
        fn name(&self) -> &str {
            "capture-stub"
        }
        fn check_available(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn ensure_ready(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn spawn(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: Option<&Path>,
            _: &std::collections::HashMap<String, String>,
            _: u16,
            _: u16,
        ) -> anyhow::Result<crate::agent::backend::SpawnedSession> {
            anyhow::bail!("capture stub does not spawn")
        }
        fn adopt(
            &self,
            _: &str,
            _: u16,
            _: u16,
            seed: Option<Vec<u8>>,
        ) -> anyhow::Result<crate::agent::backend::AdoptedSession> {
            assert!(
                seed.is_some(),
                "restore must pass the prefetched seed (ADR-P9), not capture inline"
            );
            self.seeded_adopts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::agent::backend::AdoptedSession {
                output: Box::new(std::io::empty()),
                input: Box::new(std::io::sink()),
            })
        }
        fn capture_history(&self, backend_id: &str) -> anyhow::Result<Vec<u8>> {
            self.captures
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(format!("history:{backend_id}").into_bytes())
        }
        fn discover(&self) -> anyhow::Result<Vec<crate::agent::backend::DiscoveredSession>> {
            Ok(vec![
                make_discovered("%1", "tb-one", true),
                make_discovered("%2", "tb-two", true),
            ])
        }
        fn resize(&self, _: &str, _: u16, _: u16) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_dead(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn kill(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn detach(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn pane_pid(&self, _: &str) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
    }

    /// ADR-P9: the local restore prefetches every matched pane's scrollback
    /// capture in parallel and hands the seeds to the sequential adopt loop —
    /// one `capture_history` per session, every `adopt` seeded, counter == N.
    #[tokio::test]
    async fn perf_restore_prefetches_capture_seeds() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        let backend = Arc::new(RecordingCaptureBackend::new());
        app.backends.register(backend.clone());

        let mut one = make_shared_session("%1", "one");
        one.backend_type = "capture-stub".to_string();
        let mut two = make_shared_session("%2", "two");
        two.backend_type = "capture-stub".to_string();

        app.restore_sessions(vec![one, two], 2);

        assert_eq!(
            backend.captures.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one prefetched capture per matched pane"
        );
        assert_eq!(
            backend
                .seeded_adopts
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "every adopt received its prefetched seed"
        );
        assert_eq!(app.perf_counters().restore_seed_prefetches, 2);
        assert_eq!(app.sessions.len(), 2);
    }

    #[test]
    fn poll_worktree_create_disconnected_clears_guard() {
        let mut app = app_with_sessions(0);
        drop(app.worktree_create.start());
        app.pending_worktree_create = Some(PendingWorktreeCreate {
            backend: None,
            normal_repos: vec![],
            session_name: None,
            base_branch: "main".into(),
            agent_pick: AgentPick::NotOpened,
        });

        app.poll_worktree_create();

        assert!(!app.worktree_create.in_progress());
        assert!(app.pending_worktree_create.is_none());
    }

    #[test]
    fn poll_session_spawn_error_sets_status_and_adds_no_session() {
        let mut app = app_with_sessions(0);
        let tx = app.session_spawn.start();
        tx.send(Err("tmux exploded".into())).unwrap();
        app.pending_session_spawn = Some(PendingSessionSpawn {
            primary_cwd: None,
            worktrees: vec![],
            additional_dirs: vec![],
            workspace_dir: None,
            parent_session_id: None,
            task_prompt: None,
            agent: "codex".into(),
            base_branch: None,
        });

        app.poll_session_spawn();

        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Error);
        assert!(msg.text.contains("tmux exploded"));
        // The toast names the real agent, not a hardcoded "claude".
        assert!(msg.text.contains("codex"), "got {}", msg.text);
        assert!(app.sessions.is_empty());
        assert!(!app.session_spawn.in_progress());
        assert!(app.pending_session_spawn.is_none());
    }

    #[test]
    fn poll_session_spawn_disconnected_clears_guard() {
        let mut app = app_with_sessions(0);
        drop(app.session_spawn.start());
        app.pending_session_spawn = Some(PendingSessionSpawn {
            primary_cwd: None,
            worktrees: vec![],
            additional_dirs: vec![],
            workspace_dir: None,
            parent_session_id: None,
            task_prompt: None,
            agent: "claude".into(),
            base_branch: None,
        });

        app.poll_session_spawn();

        assert!(!app.session_spawn.in_progress());
        assert!(app.pending_session_spawn.is_none());
    }

    #[tokio::test]
    async fn do_spawn_session_async_roundtrips_to_error_for_stub_backend() {
        // End-to-end: kick off the background spawn, let the blocking task run,
        // and confirm the failure (the stub backend refuses to spawn) is
        // surfaced via the poll path with no session added.
        let mut app = app_with_sessions(0);
        let config = SessionConfig::default();
        app.do_spawn_session_async("x".into(), &config, vec![]);
        assert!(app.session_spawn.in_progress());

        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            app.poll_session_spawn();
            if !app.session_spawn.in_progress() {
                break;
            }
        }

        assert!(!app.session_spawn.in_progress());
        assert!(app.sessions.is_empty());
        assert_eq!(
            app.status_message.as_ref().map(|m| m.level),
            Some(StatusLevel::Error),
        );
    }

    #[test]
    fn do_spawn_session_async_falls_back_to_sync_when_in_flight() {
        // With a spawn already in flight, a second request must not clobber the
        // pending continuation — it falls back to the synchronous path (which,
        // with the stub backend, fails to spawn and reports an error).
        let mut app = app_with_sessions(0);
        let _in_flight_tx = app.session_spawn.start();
        let config = SessionConfig::default();

        app.do_spawn_session_async("second".into(), &config, vec![]);

        // The in-flight task is untouched (no new background task kicked off).
        assert!(app.session_spawn.in_progress());
        // The synchronous fallback ran and surfaced the stub spawn failure.
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Error);
    }

    #[test]
    fn drain_deferred_inputs_sends_at_correct_tick() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let id = SessionId::default();
        app.deferred_inputs.push((id, b"hello".to_vec(), 5));

        // Before target tick: nothing drained
        app.metrics.tick_count = 4;
        app.drain_deferred_inputs();
        assert_eq!(app.deferred_inputs.len(), 1);

        // At target tick: drained (no matching session, but entry is removed)
        app.metrics.tick_count = 5;
        app.drain_deferred_inputs();
        assert!(app.deferred_inputs.is_empty());
    }

    #[test]
    fn drain_deferred_inputs_retains_future_items() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let id = SessionId::default();
        app.deferred_inputs.push((id, b"early".to_vec(), 5));
        app.deferred_inputs.push((id, b"late".to_vec(), 20));

        app.metrics.tick_count = 5;
        app.drain_deferred_inputs();
        assert_eq!(app.deferred_inputs.len(), 1);
        assert_eq!(app.deferred_inputs[0].2, 20);
    }

    #[test]
    fn dry_run_overlay_closes_only_on_its_dismissal_keys() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let open = |app: &mut App| {
            app.modal = modals::Modal::AutomationDryRun(modals::AutomationDryRunModal {
                name: "inbox".into(),
                rows: vec![("action".into(), "spawn".into())],
            });
        };

        // A stray keystroke must not dismiss a plan the user is still reading —
        // and j/k have to stay free for a future scrolling pass.
        open(&mut app);
        for code in [KeyCode::Char('j'), KeyCode::Char('x'), KeyCode::Down] {
            app.handle_modal_key_if_open(code, KeyModifiers::NONE);
            assert!(
                matches!(app.modal, modals::Modal::AutomationDryRun(_)),
                "{code:?} should not close the overlay"
            );
        }
        // The keys the overlay's own hint advertises do close it.
        for code in [KeyCode::Esc, KeyCode::Enter, KeyCode::Char('q')] {
            open(&mut app);
            app.handle_modal_key_if_open(code, KeyModifiers::NONE);
            assert!(
                matches!(app.modal, modals::Modal::None),
                "{code:?} should close the overlay"
            );
        }
    }

    #[test]
    fn send_prompt_steps_schedules_one_paste_and_enter_per_step() {
        use crate::session::PromptStep;
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let id = SessionId::default();
        app.metrics.tick_count = 100;
        let steps = vec![
            PromptStep {
                text: "/model opus".into(),
                delay_ms: Some(1_000),
            },
            PromptStep::new("go"),
        ];
        // A boot delay defers everything, so the whole schedule is inspectable.
        let _ = app.send_prompt_steps_to_session(id, &steps, AGENT_BOOT_DELAY_TICKS);

        assert_eq!(app.deferred_inputs.len(), 4, "paste + Enter per step");
        let at = |i: usize| app.deferred_inputs[i].2 - 100;
        // Step 1 pastes after the boot delay, Enter one beat later.
        assert_eq!(at(0), AGENT_BOOT_DELAY_TICKS);
        assert_eq!(at(1), AGENT_BOOT_DELAY_TICKS + DEFERRED_INPUT_DELAY_TICKS);
        // Step 2 waits out step 1's settle delay (1000 ms = 100 ticks) so the
        // slash command's autocomplete can close before the next paste lands.
        assert_eq!(at(2), at(1) + 1_000 / TICK_MS);
        assert_eq!(at(3), at(2) + DEFERRED_INPUT_DELAY_TICKS);
        // Each paste is bracketed on its own — one multi-line paste would
        // submit as a single prompt.
        let paste = String::from_utf8_lossy(&app.deferred_inputs[0].1).into_owned();
        assert!(paste.starts_with("\x1b[200~") && paste.ends_with("\x1b[201~"));
        assert!(paste.contains("/model opus"));
        assert_eq!(app.deferred_inputs[1].1, b"\r".to_vec());
    }

    #[test]
    fn send_conflict_prompt_noop_for_unknown_session() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        app.send_conflict_prompt(SessionId::default(), "origin/main");
        assert!(app.deferred_inputs.is_empty());
    }

    #[test]
    fn send_conflict_prompt_no_deferred_when_send_fails() {
        let mut app = app_with_sessions(1);
        let sid = app.sessions[0].info.id;

        // Stub's channel rx is dropped, so send_input fails.
        // No deferred input should be created.
        app.send_conflict_prompt(sid, "origin/main");
        assert!(app.deferred_inputs.is_empty());
    }

    #[test]
    fn poll_sync_results_triggers_finish_when_all_received() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let (tx, rx) = mpsc::channel();
        let id = SessionId::default();

        tx.send((id, git::SyncResult::Synced)).unwrap();
        drop(tx);

        app.worktree_sync.in_progress = true;
        app.worktree_sync.rx = Some(rx);
        app.worktree_sync.pending = 1;

        app.poll_sync_results();

        assert!(!app.worktree_sync.in_progress);
        assert!(app.worktree_sync.rx.is_none());
        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Success);
    }

    #[test]
    fn poll_sync_results_waits_for_all_pending() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let (tx, rx) = mpsc::channel();

        tx.send((SessionId::default(), git::SyncResult::Synced))
            .unwrap();
        // Don't drop tx — second result hasn't arrived yet

        app.worktree_sync.in_progress = true;
        app.worktree_sync.rx = Some(rx);
        app.worktree_sync.pending = 2;

        app.poll_sync_results();

        // Still in progress — only 1 of 2 received
        assert!(app.worktree_sync.in_progress);
        assert!(app.worktree_sync.rx.is_some());
        assert_eq!(app.worktree_sync.completed.len(), 1);
    }

    #[test]
    fn poll_auto_update_surfaces_message_and_drops_receiver() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let (tx, rx) = mpsc::channel();
        tx.send("Updated to v9.9.9 — restart friring to apply.".to_string())
            .unwrap();
        app.set_auto_update_receiver(rx);

        app.poll_auto_update();

        let msg = app.status_message.as_ref().unwrap();
        assert_eq!(msg.level, StatusLevel::Info);
        assert!(msg.text.contains("v9.9.9"), "got: {}", msg.text);
        // One-shot: the receiver is dropped so we stop polling.
        assert!(app.auto_update_rx.is_none());
    }

    #[test]
    fn poll_auto_update_disconnected_drops_receiver() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let (tx, rx) = mpsc::channel::<String>();
        drop(tx); // worker finished with nothing to report (up-to-date / failed)
        app.set_auto_update_receiver(rx);

        app.poll_auto_update();

        // No toast, and the dead channel is dropped so we stop polling it.
        assert!(app.status_message.is_none());
        assert!(app.auto_update_rx.is_none());
    }

    #[test]
    fn poll_auto_update_empty_keeps_receiver() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let (tx, rx) = mpsc::channel::<String>();
        app.set_auto_update_receiver(rx);

        app.poll_auto_update();

        // Worker still running (sender alive, nothing sent yet): keep polling.
        assert!(app.status_message.is_none());
        assert!(app.auto_update_rx.is_some());
        drop(tx);
    }

    #[test]
    fn poll_auto_update_without_receiver_is_noop() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        // Feature off / dev build: no thread spawned, so no receiver attached.
        assert!(app.auto_update_rx.is_none());
        app.poll_auto_update();
        assert!(app.status_message.is_none());
    }

    #[test]
    fn poll_sync_results_finishes_when_a_worker_dies_without_sending_all() {
        // A panicked worker drops its sender without sending every result, so
        // `completed` can never reach `pending`. The channel disconnecting must
        // still finalize, or `in_progress` would be stuck forever.
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        let (tx, rx) = mpsc::channel();

        tx.send((SessionId::default(), git::SyncResult::Synced))
            .unwrap();
        // Simulate the other worker panicking: its sender is gone, and so is
        // ours — the channel is now fully disconnected with 1 of 2 results.
        drop(tx);

        app.worktree_sync.in_progress = true;
        app.worktree_sync.rx = Some(rx);
        app.worktree_sync.pending = 2;

        app.poll_sync_results();

        assert!(!app.worktree_sync.in_progress);
        assert!(app.worktree_sync.rx.is_none());
    }

    #[test]
    fn format_time_ago_seconds() {
        let now = crate::sync::current_time_millis();
        assert_eq!(super::view::format_time_ago(now - 5_000), "5s ago");
        assert_eq!(super::view::format_time_ago(now - 30_000), "30s ago");
    }

    #[test]
    fn format_time_ago_minutes() {
        let now = crate::sync::current_time_millis();
        assert_eq!(super::view::format_time_ago(now - 120_000), "2m ago");
        assert_eq!(super::view::format_time_ago(now - 3_540_000), "59m ago");
    }

    #[test]
    fn format_time_ago_hours() {
        let now = crate::sync::current_time_millis();
        assert_eq!(super::view::format_time_ago(now - 3_600_000), "1h ago");
        assert_eq!(super::view::format_time_ago(now - 7_200_000), "2h ago");
    }

    #[test]
    fn format_time_ago_days() {
        let now = crate::sync::current_time_millis();
        assert_eq!(super::view::format_time_ago(now - 86_400_000), "1d ago");
        assert_eq!(super::view::format_time_ago(now - 259_200_000), "3d ago");
    }

    #[test]
    fn format_time_ago_future_timestamp() {
        let now = crate::sync::current_time_millis();
        // Future timestamp should saturate to 0s
        assert_eq!(super::view::format_time_ago(now + 10_000), "0s ago");
    }

    // --- find_matching_discovered tests ---

    fn make_shared_session(backend_id: &str, name: &str) -> sync::SharedSession {
        sync::SharedSession {
            id: crate::session::SessionId::default(),
            name: name.to_string(),
            agent: String::new(),
            backend_id: backend_id.to_string(),
            backend_type: "tmux".to_string(),
            agent_session_id: Some("agent-123".to_string()),
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: Vec::new(),
            shell_backend_id: None,
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
        }
    }

    fn make_discovered(
        backend_id: &str,
        name: &str,
        is_alive: bool,
    ) -> crate::agent::backend::DiscoveredSession {
        crate::agent::backend::DiscoveredSession {
            backend_id: backend_id.to_string(),
            name: name.to_string(),
            is_alive,
        }
    }

    #[test]
    fn find_matching_discovered_by_backend_id() {
        let shared = make_shared_session("friring:@0", "1");
        let discovered = vec![
            make_discovered("friring:@0", "tb-1", true),
            make_discovered("friring:@1", "tb-2", true),
        ];
        let result = App::find_matching_discovered(&shared, &discovered);
        assert!(result.is_some());
        assert_eq!(result.unwrap().backend_id, "friring:@0");
    }

    #[test]
    fn find_matching_discovered_by_name_fallback() {
        let shared = make_shared_session("", "1");
        let discovered = vec![
            make_discovered("friring:@5", "tb-1", true),
            make_discovered("friring:@6", "tb-2", true),
        ];
        let result = App::find_matching_discovered(&shared, &discovered);
        assert!(result.is_some());
        assert_eq!(result.unwrap().backend_id, "friring:@5");
    }

    #[test]
    fn find_matching_discovered_skips_dead() {
        let shared = make_shared_session("friring:@0", "1");
        let discovered = vec![make_discovered("friring:@0", "tb-1", false)];
        let result = App::find_matching_discovered(&shared, &discovered);
        assert!(result.is_none());
    }

    #[test]
    fn find_matching_discovered_no_match() {
        let shared = make_shared_session("friring:@99", "99");
        let discovered = vec![make_discovered("friring:@0", "tb-1", true)];
        let result = App::find_matching_discovered(&shared, &discovered);
        assert!(result.is_none());
    }

    #[test]
    fn find_matching_discovered_empty_list() {
        let shared = make_shared_session("friring:@0", "1");
        let result = App::find_matching_discovered(&shared, &[]);
        assert!(result.is_none());
    }

    // --- Lazy restore / ghost tests ---
    // `settings::init` is never called in the test process, so `global()` is
    // the all-default config: `lazy_session_restore = true` — the path under
    // test. (Flipping it per-test would poison the process-wide OnceLock for
    // in-process `cargo test` runs, so the respawn path stays untested here.)

    /// Lazy restore: a persisted session with no live pane becomes a ghost —
    /// no spawn attempt (the stub backend's `spawn` bails, so reaching it
    /// would error), status `Unloaded`, keystrokes dropped.
    #[test]
    fn lazy_restore_ghosts_session_without_live_pane() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        let shared = make_shared_session("friring:@0", "gone");
        app.db.upsert_session(&shared).unwrap();

        app.restore_sessions(vec![shared], 1);

        assert_eq!(app.sessions.len(), 1);
        let s = &app.sessions[0];
        assert!(s.is_ghost() && s.is_placeholder());
        assert_eq!(s.info.status, SessionStatus::Unloaded);
        assert!(app.active_session_is_ghost());
        // A pre-v45 row has no saved frame: the pane explains itself instead of
        // rendering empty.
        let text = s.parser.lock().map(|p| p.screen().contents()).unwrap();
        assert!(
            text.contains("no saved preview") && text.contains("Press Enter"),
            "got: {text}"
        );
    }

    /// A ghost restored from a saved frame replays that frame into its pane.
    #[test]
    fn ghost_restores_saved_frame_content() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        let shared = make_shared_session("friring:@0", "gone");
        app.db.upsert_session(&shared).unwrap();
        app.db
            .save_session_frame(shared.id, 24, 80, b"frozen \x1b[31mfindings\x1b[0m here")
            .unwrap();

        app.restore_sessions(vec![shared], 1);

        let text = app.sessions[0]
            .parser
            .lock()
            .map(|p| p.screen().contents())
            .unwrap();
        assert!(text.contains("frozen findings here"), "got: {text}");
    }

    /// A ghost renders whatever its stored frame holds, including rows above
    /// the visible screen: those land in the parser's scrollback and the scroll
    /// actions (which have no placeholder gating) reach them. Captures are
    /// visible-screen-only now, but a frame can still be taller than the pane
    /// (a narrower terminal wraps it), so the seeding must not drop the excess.
    #[tokio::test]
    async fn ghost_frame_is_scrollable() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        let shared = make_shared_session("friring:@0", "deep");
        app.db.upsert_session(&shared).unwrap();
        // Far more lines than the pane is tall, so most land in scrollback.
        let frame: String = (0..200).map(|i| format!("line-{i}\r\n")).collect();
        app.db
            .save_session_frame(shared.id, 24, 80, frame.as_bytes())
            .unwrap();
        app.restore_sessions(vec![shared], 1);
        assert!(app.sessions[0].is_ghost());

        // Total scrollback available, read the way `render_terminal` does.
        let total = {
            let mut p = app.sessions[0].parser.lock().unwrap();
            p.screen_mut().set_scrollback(usize::MAX);
            let max = p.screen().scrollback();
            p.screen_mut().set_scrollback(0);
            max
        };
        assert!(total > 0, "a ghost's frame kept no scrollback");

        // And the scroll action actually moves it (no placeholder gating).
        app.scroll_terminal_up(5);
        let offset = app.sessions[0]
            .parser
            .lock()
            .map(|p| p.screen().scrollback())
            .unwrap();
        assert_eq!(offset, 5, "scrolling a ghost did not move its viewport");
        let top = app.sessions[0]
            .parser
            .lock()
            .map(|p| p.screen().contents())
            .unwrap();
        assert!(top.contains("line-"), "scrolled view shows frame content");

        // …and the offset must SURVIVE the tick loop. A ghost re-renders from
        // its seed on resize, which rebuilds the parser and snaps the viewport
        // back to the bottom — so anything that re-resizes a ghost every tick
        // would make it look like scrolling silently does nothing.
        for _ in 0..5 {
            app.tick_core();
        }
        let after_ticks = app.sessions[0]
            .parser
            .lock()
            .map(|p| p.screen().scrollback())
            .unwrap();
        assert_eq!(
            after_ticks, 5,
            "the tick loop reset a ghost's scroll position"
        );
    }

    /// vt100 resizes by truncating each row's cells, so a narrowed pane loses
    /// every cell past the new width and widening back pads with blanks. A
    /// live session's agent repaints that away on SIGWINCH; a ghost has no
    /// process to repaint it, so it must re-render from its saved frame or the
    /// frozen content stays clipped to the narrowest size ever seen.
    #[tokio::test]
    async fn ghost_reflows_its_frame_after_a_shrink_and_regrow() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        let shared = make_shared_session("friring:@0", "wide");
        app.db.upsert_session(&shared).unwrap();
        // A line far wider than the shrunk pane below.
        let wide = format!("LEFT-{}-ENDMARK", "x".repeat(100));
        app.db
            .save_session_frame(shared.id, 24, 200, wide.as_bytes())
            .unwrap();
        app.restore_sessions(vec![shared], 1);
        assert!(app.sessions[0].is_ghost());

        let visible = |app: &App| {
            app.sessions[0]
                .parser
                .lock()
                .map(|p| p.screen().contents())
                .unwrap()
        };
        app.sessions[0].resize(24, 200);
        assert!(visible(&app).contains("ENDMARK"), "baseline");

        // Shrink well under the line's width, then restore the original size.
        app.sessions[0].resize(20, 40);
        app.sessions[0].resize(24, 200);

        let after = visible(&app);
        assert!(
            after.contains("ENDMARK"),
            "the ghost's frame was clipped by the shrink: {after:?}"
        );
    }

    /// A backend whose `kill` fails — the agent process outlives the request.
    struct UnkillableBackend;
    impl SessionBackend for UnkillableBackend {
        fn name(&self) -> &str {
            "unkillable"
        }
        fn check_available(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn ensure_ready(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn spawn(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: Option<&Path>,
            _: &std::collections::HashMap<String, String>,
            _: u16,
            _: u16,
        ) -> anyhow::Result<crate::agent::backend::SpawnedSession> {
            anyhow::bail!("stub backend does not spawn")
        }
        fn adopt(
            &self,
            _: &str,
            _: u16,
            _: u16,
            _: Option<Vec<u8>>,
        ) -> anyhow::Result<crate::agent::backend::AdoptedSession> {
            anyhow::bail!("stub backend does not adopt")
        }
        fn discover(&self) -> anyhow::Result<Vec<crate::agent::backend::DiscoveredSession>> {
            Ok(vec![])
        }
        fn resize(&self, _: &str, _: u16, _: u16) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_dead(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn kill(&self, _: &str) -> anyhow::Result<()> {
            anyhow::bail!("kill-pane refused")
        }
        fn detach(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn pane_pid(&self, _: &str) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
    }

    /// A failed teardown must not produce a ghost. A ghost row asserts "this
    /// session's process is not running"; reaching that state after a failed
    /// kill would strand a live agent still holding its memory, with nothing
    /// left pointing at it. The session stays live, the DB flag stays clear,
    /// and the user is told.
    #[tokio::test]
    async fn unload_aborts_and_reports_when_the_kill_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let backend: Arc<dyn SessionBackend> = Arc::new(UnkillableBackend);
        let provider = stub_provider();
        let mut app = App::new(
            24,
            120,
            BackendRegistry::new(backend.clone()),
            stub_agents(),
            test_db(),
        );
        app.sessions
            .push(Session::stub("doomed", &backend, &provider));
        app.active_index = 0;
        let id = persist_session(&app, 0);
        app.focus = InputFocus::Terminal;

        app.unload_active_session();

        assert!(!app.sessions[0].is_ghost(), "session must stay live");
        assert!(!app.sessions[0].is_placeholder());
        assert!(
            app.db.unloaded_session_ids().unwrap().is_empty(),
            "a failed unload must not flag the row unloaded"
        );
        assert_eq!(
            app.focus,
            InputFocus::Terminal,
            "focus stays on the still-live pane"
        );
        let msg = app.status_message.as_ref().expect("an error is reported");
        assert_eq!(msg.level, StatusLevel::Error);
        assert!(msg.text.contains("Failed to unload"), "got: {}", msg.text);
        let _ = id;
    }

    /// Unload swaps the live session for a ghost in place, persists the frame
    /// + `unloaded` flag, and keeps id/order stable.
    #[tokio::test]
    async fn unload_active_session_swaps_in_a_ghost() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(1);
        app.sessions[0].feed_output_for_test(b"important last words\r\n");
        let id = app.sessions[0].info.id;
        app.focus = InputFocus::Terminal;

        app.unload_active_session();

        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions[0].is_ghost());
        assert_eq!(app.sessions[0].info.id, id, "identity survives the swap");
        // The ghost has no live PTY, so focus leaves the pane for the list.
        assert_eq!(app.focus, InputFocus::SessionList);
        assert_eq!(app.db.unloaded_session_ids().unwrap(), vec![id]);
        let frame = app.db.load_session_frame(id).unwrap().expect("frame saved");
        let text = String::from_utf8_lossy(&frame.bytes).to_string();
        assert!(text.contains("important last words"), "got: {text}");
        // The ghost pane replays the captured frame.
        let ghost_text = app.sessions[0]
            .parser
            .lock()
            .map(|p| p.screen().contents())
            .unwrap();
        assert!(ghost_text.contains("important last words"));
        // Unloading a ghost is a no-op with a hint, not a crash.
        app.unload_active_session();
        assert!(app.sessions[0].is_ghost());
    }

    /// The crash-safety debounce: a session with new output gets its visible
    /// screen persisted, a session with none is skipped (the dirty check is
    /// what keeps this cheap at every interval), and a placeholder is never
    /// written — its frozen frame would be overwritten by the empty pane.
    #[tokio::test]
    async fn persist_dirty_frames_writes_only_changed_live_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(1);
        let id = persist_session(&app, 0);
        // The debounce compares millisecond wall-clock stamps, and the output
        // test seam bumps strictly *past* "now" — so let the clock move on
        // around each feed (the real ~60 s interval is never this tight):
        // after, so a save counts the output as captured; before, so the next
        // output out-stamps the previous save.
        let settle = || std::thread::sleep(std::time::Duration::from_millis(5));

        app.sessions[0].feed_output_for_test(b"first words\r\n");
        settle();
        app.persist_dirty_frames();

        let saved = app.db.load_session_frame(id).unwrap().expect("frame saved");
        assert!(
            String::from_utf8_lossy(&saved.bytes).contains("first words"),
            "got: {:?}",
            String::from_utf8_lossy(&saved.bytes)
        );
        assert!(saved.saved_at > 0);
        assert!(app.frame_saved_at.get(&id).copied().unwrap_or(0) > 0);

        // Overwrite the stored frame with a sentinel: a second pass with no new
        // output must leave it alone.
        app.db.save_session_frame(id, 1, 1, b"SENTINEL").unwrap();
        app.persist_dirty_frames();
        let kept = app.db.load_session_frame(id).unwrap().expect("frame kept");
        assert_eq!(
            kept.bytes, b"SENTINEL",
            "clean session must not be rewritten"
        );

        // New output → the frame is refreshed.
        settle();
        app.sessions[0].feed_output_for_test(b"later words\r\n");
        settle();
        app.persist_dirty_frames();
        let updated = app
            .db
            .load_session_frame(id)
            .unwrap()
            .expect("frame updated");
        assert!(
            String::from_utf8_lossy(&updated.bytes).contains("later words"),
            "got: {:?}",
            String::from_utf8_lossy(&updated.bytes)
        );

        // A ghost keeps the frame it was unloaded with, even if bytes arrive
        // for it: the debounce skips placeholders entirely.
        app.unload_active_session();
        assert!(app.sessions[0].is_ghost());
        app.db.save_session_frame(id, 1, 1, b"SENTINEL").unwrap();
        app.sessions[0].feed_output_for_test(b"ghost noise\r\n");
        app.persist_dirty_frames();
        let ghost_frame = app.db.load_session_frame(id).unwrap().expect("frame kept");
        assert_eq!(ghost_frame.bytes, b"SENTINEL");
    }

    /// Loaded-only cycling skips ghosts in both directions and jumps from a
    /// ghost to the nearest loaded session.
    #[tokio::test]
    async fn switch_loaded_session_skips_ghosts() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(3);
        app.active_index = 1;
        app.unload_active_session();
        assert!(app.sessions[1].is_ghost());

        // From the ghost, keep going in the requested direction: forward is the
        // next loaded row after it, not the first one in the list.
        app.switch_loaded_session(true);
        assert_eq!(app.active_index, 2);
        // Forward from 2 wraps to 0, then skips the ghost at 1 back to 2.
        app.switch_loaded_session(true);
        assert_eq!(app.active_index, 0);
        app.switch_loaded_session(true);
        assert_eq!(app.active_index, 2);
        // Backward wraps and skips the ghost too.
        app.switch_loaded_session(false);
        assert_eq!(app.active_index, 0);
        // Backward from the ghost goes to the loaded row *before* it.
        app.active_index = 1;
        app.switch_loaded_session(false);
        assert_eq!(app.active_index, 0);
    }

    /// `serialize_visible_frame` → fresh parser reproduces the visible screen
    /// exactly (styled text, wide chars, blank-row trim) — the contract the
    /// ghost pane depends on.
    #[test]
    fn visible_frame_serialization_round_trips() {
        let backend = stub_backend_arc();
        let provider = stub_provider();
        let session = Session::stub("frame", &backend, &provider);
        session.feed_output_for_test(
            b"plain then \x1b[1;31mbold red\x1b[0m\r\n\xe6\x97\xa5\xe6\x9c\xac lines\r\n> _",
        );

        let (rows, cols, bytes) = session.serialize_visible_frame().unwrap();
        let mut reparsed = vt100::Parser::new(rows, cols, 0);
        reparsed.process(&bytes);

        let original = session
            .parser
            .lock()
            .map(|p| p.screen().contents())
            .unwrap();
        assert_eq!(reparsed.screen().contents(), original);
        assert!(original.contains("bold red") && original.contains("日本 lines"));

        // A frame is terminal bytes, not a bitmap: replaying the same capture
        // into a smaller pane (a ghost restored at a different terminal/font
        // size) still renders the content. Reflow of lines that were *soft*
        // wrapped at capture size is out of scope — each captured row is its
        // own line, so a narrower pane hard-wraps rather than re-flows.
        let mut resized = vt100::Parser::new(12, 40, 0);
        resized.process(&bytes);
        let smaller = resized.screen().contents();
        assert!(
            smaller.contains("bold red") && smaller.contains("日本 lines"),
            "got: {smaller}"
        );
    }

    // --- Background remote restore tests ---

    /// Stub remote backend for the background remote-restore tests: reports
    /// one live window (`%9`) and adopts it with inert I/O streams.
    struct RemoteStubBackend;
    impl SessionBackend for RemoteStubBackend {
        fn name(&self) -> &str {
            "ssh:test-host"
        }
        fn check_available(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn ensure_ready(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn spawn(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: Option<&Path>,
            _: &std::collections::HashMap<String, String>,
            _: u16,
            _: u16,
        ) -> anyhow::Result<crate::agent::backend::SpawnedSession> {
            anyhow::bail!("remote stub does not spawn")
        }
        fn adopt(
            &self,
            _: &str,
            _: u16,
            _: u16,
            _: Option<Vec<u8>>,
        ) -> anyhow::Result<crate::agent::backend::AdoptedSession> {
            Ok(crate::agent::backend::AdoptedSession {
                output: Box::new(std::io::empty()),
                input: Box::new(std::io::sink()),
            })
        }
        fn discover(&self) -> anyhow::Result<Vec<crate::agent::backend::DiscoveredSession>> {
            Ok(vec![make_discovered("%9", "tb-remote-sess", true)])
        }
        fn resize(&self, _: &str, _: u16, _: u16) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_dead(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn kill(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn detach(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn pane_pid(&self, _: &str) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
    }

    /// Stub remote backend whose host is **down**: `ensure_ready` fails, so
    /// discovery reports unreachable and its sessions stay as placeholders.
    struct DownRemoteStubBackend;
    impl SessionBackend for DownRemoteStubBackend {
        fn name(&self) -> &str {
            "ssh:down-host"
        }
        fn check_available(&self) -> anyhow::Result<()> {
            anyhow::bail!("host down")
        }
        fn ensure_ready(&self) -> anyhow::Result<()> {
            anyhow::bail!("ssh: connect to host down-host port 22: No route to host")
        }
        fn spawn(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: Option<&Path>,
            _: &std::collections::HashMap<String, String>,
            _: u16,
            _: u16,
        ) -> anyhow::Result<crate::agent::backend::SpawnedSession> {
            anyhow::bail!("host down")
        }
        fn adopt(
            &self,
            _: &str,
            _: u16,
            _: u16,
            _: Option<Vec<u8>>,
        ) -> anyhow::Result<crate::agent::backend::AdoptedSession> {
            anyhow::bail!("host down")
        }
        fn discover(&self) -> anyhow::Result<Vec<crate::agent::backend::DiscoveredSession>> {
            anyhow::bail!("host down")
        }
        fn resize(&self, _: &str, _: u16, _: u16) -> anyhow::Result<()> {
            anyhow::bail!("host down")
        }
        fn is_dead(&self, _: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn kill(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn detach(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn pane_pid(&self, _: &str) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn remote_session_on_down_host_stays_unreachable_and_retries() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        app.backends.register(Arc::new(DownRemoteStubBackend));

        let mut shared = make_shared_session("%9", "remote-sess");
        shared.backend_type = "ssh:down-host".to_string();
        let id = shared.id;
        // The real persisted row (as if written by a prior run).
        app.db.upsert_session(&shared).unwrap();
        app.restore_sessions(vec![shared], 1);

        // Immediately visible as an unreachable placeholder.
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions[0].is_placeholder());
        assert_eq!(app.sessions[0].info.status, SessionStatus::Unreachable);

        // Drain the discovery thread's "unreachable" report: the placeholder
        // survives (not adopted) and stays queued for retry.
        for _ in 0..500 {
            app.poll_remote_restore();
            if app
                .remote_restore
                .as_ref()
                .is_some_and(|s| s.notified_unreachable.contains("ssh:down-host"))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions[0].is_placeholder());
        assert_eq!(app.sessions[0].info.id, id);
        // Still pending → the retry loop will keep trying the host.
        assert!(app
            .remote_restore
            .as_ref()
            .is_some_and(|s| s.pending.contains_key("ssh:down-host")));

        // Its persisted row must be untouched by the placeholder (save_state
        // skips placeholders), so re-adoption after recovery still works.
        app.save_state();
        let persisted = app.db.list_active_sessions().unwrap();
        let row = persisted.iter().find(|s| s.id == id).unwrap();
        assert_eq!(row.backend_type, "ssh:down-host");
        assert_eq!(row.backend_id, "%9");
    }

    /// Stub remote backend that is **down at first, then recovers**:
    /// `ensure_ready` fails until `ready_after` calls have been made, then
    /// succeeds and discovers `%9`. Models a failing remote session that later
    /// reconnects.
    struct FlakyRemoteStubBackend {
        calls: std::sync::atomic::AtomicUsize,
        ready_after: usize,
    }
    impl FlakyRemoteStubBackend {
        fn new(ready_after: usize) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                ready_after,
            }
        }
    }
    impl SessionBackend for FlakyRemoteStubBackend {
        fn name(&self) -> &str {
            "ssh:flaky-host"
        }
        fn check_available(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn ensure_ready(&self) -> anyhow::Result<()> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n >= self.ready_after {
                Ok(())
            } else {
                anyhow::bail!("host down")
            }
        }
        fn spawn(
            &self,
            _: &str,
            _: &str,
            _: &[String],
            _: Option<&Path>,
            _: &std::collections::HashMap<String, String>,
            _: u16,
            _: u16,
        ) -> anyhow::Result<crate::agent::backend::SpawnedSession> {
            anyhow::bail!("flaky stub does not spawn")
        }
        fn adopt(
            &self,
            _: &str,
            _: u16,
            _: u16,
            _: Option<Vec<u8>>,
        ) -> anyhow::Result<crate::agent::backend::AdoptedSession> {
            Ok(crate::agent::backend::AdoptedSession {
                output: Box::new(std::io::empty()),
                input: Box::new(std::io::sink()),
            })
        }
        fn discover(&self) -> anyhow::Result<Vec<crate::agent::backend::DiscoveredSession>> {
            Ok(vec![make_discovered("%9", "tb-remote-sess", true)])
        }
        fn resize(&self, _: &str, _: u16, _: u16) -> anyhow::Result<()> {
            Ok(())
        }
        fn is_dead(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn kill(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn detach(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn pane_pid(&self, _: &str) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn live_remote_session_host_loss_becomes_unreachable_then_reconnects() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        let remote: Arc<dyn SessionBackend> = Arc::new(RemoteStubBackend);
        app.backends.register(Arc::clone(&remote));

        // A live, adopted remote session (non-placeholder).
        let mut session = Session::stub("remote-sess", &remote, &stub_provider());
        session.info.agent_session_id = Some("sess-uuid".to_string());
        let id = session.info.id;
        app.sessions.push(session);
        app.active_index = 0;
        assert!(!app.sessions[0].is_placeholder());
        assert!(crate::session::is_remote_backend(
            app.sessions[0].backend_name()
        ));

        // Simulate the SSH/host connection dropping mid-session (control-mode
        // EOF → `exited`); with `remain-on-exit=on` this only happens on host
        // loss, never a clean agent exit.
        app.sessions[0].mark_exited_for_test();

        // The per-tick detector converts it in place to an unreachable
        // placeholder and queues it for reconnect.
        app.detect_lost_remote_sessions();
        assert!(app.sessions[0].is_placeholder());
        assert_eq!(app.sessions[0].info.status, SessionStatus::Unreachable);
        assert_eq!(app.sessions[0].info.id, id);
        assert!(app
            .remote_restore
            .as_ref()
            .is_some_and(|s| s.pending.contains_key("ssh:test-host")));

        // The host is reachable again → reconnects and re-adopts in place.
        drain_remote_restore(&mut app);
        assert_eq!(app.sessions.len(), 1);
        assert!(!app.sessions[0].is_placeholder());
        assert_eq!(app.sessions[0].info.id, id);
    }

    #[tokio::test]
    async fn failing_remote_session_recovers_and_adopts_on_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        // Down for the first discovery, up on the retry.
        app.backends
            .register(Arc::new(FlakyRemoteStubBackend::new(1)));

        let mut shared = make_shared_session("%9", "remote-sess");
        shared.backend_type = "ssh:flaky-host".to_string();
        let id = shared.id;
        app.restore_sessions(vec![shared], 1);
        assert!(app.sessions[0].is_placeholder());

        // Phase 1: the first discovery finds the host down — the placeholder
        // survives as unreachable and stays queued for retry.
        let mut down_seen = false;
        for _ in 0..500 {
            app.poll_remote_restore();
            if app
                .remote_restore
                .as_ref()
                .is_some_and(|s| s.notified_unreachable.contains("ssh:flaky-host"))
            {
                down_seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(down_seen, "first discovery should report the host down");
        assert!(app.sessions[0].is_placeholder());
        assert_eq!(app.sessions[0].info.status, SessionStatus::Unreachable);
        assert!(app
            .remote_restore
            .as_ref()
            .is_some_and(|s| s.pending.contains_key("ssh:flaky-host")));

        // Phase 2: force a retry sweep; the host is up now, so the placeholder is
        // replaced in place by the real adopted session (same id, no duplicate).
        app.retry_remote_backend_now("ssh:flaky-host");
        drain_remote_restore(&mut app);
        assert_eq!(app.sessions.len(), 1);
        assert!(!app.sessions[0].is_placeholder());
        assert_eq!(app.sessions[0].info.id, id);
        assert_eq!(app.sessions[0].backend_name(), "ssh:flaky-host");
    }

    #[tokio::test]
    async fn deleting_placeholder_is_not_resurrected_on_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        app.backends.register(Arc::new(RemoteStubBackend));

        let mut shared = make_shared_session("%9", "remote-sess");
        shared.backend_type = "ssh:test-host".to_string();
        let id = shared.id;
        app.db.upsert_session(&shared).unwrap();
        app.restore_sessions(vec![shared], 1);
        assert!(app.sessions[0].is_placeholder());

        // The user deletes the placeholder before the host is reached.
        app.active_index = 0;
        app.close_active_session();
        assert!(app.sessions.is_empty());

        // The host is reachable now; draining discovery must NOT resurrect the
        // deleted session.
        drain_remote_restore(&mut app);
        assert!(app.sessions.iter().all(|s| s.info.id != id));
    }

    #[tokio::test]
    async fn remote_sessions_restore_in_background() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        app.backends.register(Arc::new(RemoteStubBackend));

        let mut shared = make_shared_session("%9", "remote-sess");
        shared.backend_type = "ssh:test-host".to_string();
        let id = shared.id;

        app.restore_sessions(vec![shared], 1);

        // The first frame must not wait on the remote host: the session shows
        // immediately as an unreachable placeholder, and the real adopt runs on
        // a background thread.
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions[0].is_placeholder());
        assert_eq!(app.sessions[0].info.id, id);
        assert_eq!(app.sessions[0].info.status, SessionStatus::Unreachable);
        assert!(app.remote_restore.is_some());

        // Drain like tick() would until the discovery thread reports; the
        // placeholder is replaced in place by the real adopted session.
        drain_remote_restore(&mut app);
        assert_eq!(app.sessions.len(), 1);
        assert!(!app.sessions[0].is_placeholder());
        assert_eq!(app.sessions[0].info.id, id);
        assert_eq!(app.sessions[0].backend_name(), "ssh:test-host");
    }

    #[test]
    fn remote_session_on_unknown_host_shows_unreachable_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);

        let mut shared = make_shared_session("%9", "remote-sess");
        shared.backend_type = "ssh:unknown-host".to_string();
        let id = shared.id;

        app.restore_sessions(vec![shared], 1);

        // An unmanageable backend (host not in config) can't be adopted, but the
        // session must still appear — as an unreachable placeholder — rather than
        // silently vanish. It isn't queued for retries (nothing to retry against).
        assert_eq!(app.sessions.len(), 1);
        assert!(app.sessions[0].is_placeholder());
        assert_eq!(app.sessions[0].info.id, id);
        assert_eq!(app.sessions[0].info.status, SessionStatus::Unreachable);
        assert!(app.remote_restore.is_none());
    }

    /// Drive [`App::poll_remote_restore`] until the background discovery
    /// thread reports and the pending state clears.
    fn drain_remote_restore(app: &mut App) {
        for _ in 0..500 {
            app.poll_remote_restore();
            if app.remote_restore.is_none() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("discovery result never drained");
    }

    #[tokio::test]
    async fn remote_restore_preserves_active_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(1);
        app.backends.register(Arc::new(RemoteStubBackend));
        let prior_id = app.sessions[0].info.id;

        let mut shared = make_shared_session("%9", "remote-sess");
        shared.backend_type = "ssh:test-host".to_string();
        app.restore_sessions(vec![shared], 1);
        drain_remote_restore(&mut app);

        // The late-arriving host's session lands in the list without stealing
        // the user's current selection.
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions[app.active_index].info.id, prior_id);
    }

    #[test]
    fn remote_restore_skips_session_adopted_meanwhile() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let mut app = app_with_sessions(0);
        app.backends.register(Arc::new(RemoteStubBackend));

        let mut shared = make_shared_session("%9", "remote-sess");
        shared.backend_type = "ssh:test-host".to_string();
        let id = shared.id;
        app.restore_sessions(vec![shared], 1);

        // Another path (e.g. the DB sync) adopts the session while discovery
        // is still in flight.
        let backend_arc = stub_backend_arc();
        let mut session = Session::stub("remote-sess", &backend_arc, &stub_provider());
        session.info.id = id;
        app.sessions.push(session);

        drain_remote_restore(&mut app);

        // The drain must not create a duplicate for the already-present id.
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].info.id, id);
    }

    // --- Modal flow tests ---

    #[test]
    fn handle_paste_clears_selection() {
        let mut app = app_with_sessions(1);
        app.text_selection = Some(Selection::new(
            TermPos { row: 0, col: 0 },
            PaneBounds::from_rect(ratatui::layout::Rect::new(0, 0, 80, 24)),
        ));
        app.selected_text_cache = Some("old".to_string());

        app.handle_paste("hello".to_string());

        assert!(app.text_selection.is_none());
        assert!(app.selected_text_cache.is_none());
    }

    #[test]
    fn paste_message_dispatches_to_handle_paste() {
        let mut app = app_with_sessions(1);
        app.text_selection = Some(Selection::new(
            TermPos { row: 0, col: 0 },
            PaneBounds::from_rect(ratatui::layout::Rect::new(0, 0, 80, 24)),
        ));

        app.update(AppMessage::Paste("pasted text".to_string()));

        assert!(app.text_selection.is_none());
    }

    #[test]
    fn send_paste_to_session_noop_when_no_sessions() {
        let mut app = App::new(24, 80, stub_backend(), stub_agents(), test_db());
        // Should not panic with no active sessions
        app.send_paste_to_session("hello");
    }

    #[test]
    fn send_paste_to_session_noop_for_empty_text() {
        let mut app = app_with_sessions(1);
        // Should return early without error for empty text
        app.send_paste_to_session("");
    }

    /// Value of the focused text field in the current modal (for the
    /// paste-routing tests). `None` for modals without a text field.
    fn focused_modal_text(app: &App) -> Option<String> {
        let text = match &app.modal {
            modals::Modal::WorktreeName(wn) => wn.name.value(),
            modals::Modal::SessionName(sn) => sn.name.value(),
            modals::Modal::RepoPicker(rp) => rp.input.value(),
            _ => return None,
        };
        Some(text.to_string())
    }

    #[test]
    fn paste_routes_into_modal_text_inputs() {
        // (modal, pasted, expected) — single-line fields strip embedded
        // newlines, so a pasted trailing newline must not survive.
        let repo_input = modals::Modal::RepoPicker(modals::RepoPickerModal::default());
        let cases: Vec<(modals::Modal, &str, &str)> = vec![
            (
                modals::Modal::WorktreeName(Default::default()),
                "feature/x",
                "feature/x",
            ),
            (
                modals::Modal::SessionName(Default::default()),
                "my session\n",
                "my session",
            ),
            (repo_input, "/tmp/repo", "/tmp/repo"),
        ];

        for (modal, pasted, expected) in cases {
            let mut app = app_with_sessions(1);
            app.modal = modal;
            assert!(
                app.try_paste_into_modal_input(pasted),
                "paste must be consumed"
            );
            assert_eq!(focused_modal_text(&app).as_deref(), Some(expected));
        }
    }

    #[test]
    fn paste_into_selector_only_modal_is_swallowed_not_sent_to_terminal() {
        let mut app = app_with_sessions(1);
        app.modal = modals::Modal::ThemePicker(theme_picker_at(0));

        // A theme picker has no text field, but the paste must still be
        // consumed so it can't leak into the terminal behind the overlay.
        let consumed = app.try_paste_into_modal_input("oops");

        assert!(consumed);
    }

    #[test]
    fn paste_falls_through_to_terminal_when_no_modal() {
        let mut app = app_with_sessions(1);
        // No modal, terminal focus: paste is NOT consumed here so the caller
        // sends it to the session.
        app.focus = InputFocus::Terminal;
        assert!(!app.try_paste_into_modal_input("to terminal"));
    }

    #[test]
    fn paste_routes_into_in_pane_task_editor_description() {
        let mut app = app_with_sessions(1);
        let mut editor = modals::TaskEditorModal::new();
        editor.field = modals::TaskField::Description;
        app.task_ui.task_editor = Some(editor);
        app.focus = InputFocus::TaskEditor;

        // The description is multi-line, so a pasted newline is preserved.
        let consumed = app.try_paste_into_modal_input("line one\nline two");

        assert!(consumed);
        let editor = app.task_ui.task_editor.as_ref().unwrap();
        assert_eq!(editor.description.value(), "line one\nline two");
    }

    // --- key_handlers: modal open/close + pane chords driven via handle_key ---

    #[test]
    fn ctrl_y_opens_theme_picker_then_j_and_enter_persists() {
        let mut app = app_with_sessions(1);
        app.handle_key(KeyCode::Char('y'), KeyModifiers::CONTROL);
        let presets = crate::session::ThemePreset::all();
        match app.modal {
            modals::Modal::ThemePicker(ref tp) => assert_eq!(tp.index, 0),
            ref other => panic!("expected the theme picker, got {other:?}"),
        }
        // `j` previews the next preset; `Enter` commits + persists it.
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
        assert_eq!(app.active_theme.name, presets[1].as_str());
        assert_eq!(
            app.db.get_active_theme().unwrap().as_deref(),
            Some(presets[1].as_str())
        );
        // The active palette is process-global; restore the default so this
        // test doesn't leak into others (matching `set_active_switches_palette`).
        crate::ui::theme::set_active(crate::session::ThemePalette::default());
    }

    #[test]
    fn theme_picker_esc_closes_without_persisting() {
        let mut app = app_with_sessions(1);
        app.handle_key(KeyCode::F(4), KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::ThemePicker(_)));
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
        // Esc doesn't write a theme choice to the DB.
        assert!(app.db.get_active_theme().unwrap().is_none());
    }

    #[test]
    fn ctrl_u_opens_restore_sessions_modal_and_esc_closes() {
        let mut app = app_with_sessions(1);
        // Empty DB → an empty (but open) restore modal.
        app.handle_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        match app.modal {
            modals::Modal::RestoreSessions(ref rs) => assert!(rs.list.is_empty()),
            ref other => panic!("expected the restore-sessions modal, got {other:?}"),
        }
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
    }

    #[test]
    fn branch_selector_esc_returns_to_repo_picker_and_clears_pending_state() {
        let mut app = app_with_sessions(1);
        app.new_session.repo_path = Some(PathBuf::from("/repo"));
        app.new_session.all_repos = Some(vec![PathBuf::from("/repo")]);
        app.new_session.normal_repos = vec![PathBuf::from("/other")];
        // The palette parked by the forward step, selections intact.
        let mut parked = modals::RepoPickerModal::default();
        parked.push_row("/repo".into(), modals::RepoRowKind::Repo { child: false });
        parked.selected.insert(PathBuf::from("/repo"));
        parked.worktree.insert(PathBuf::from("/repo"));
        parked.input.set("re");
        app.new_session.saved_repo_picker = Some(Box::new(parked));
        // A parked origin-fetch signal (ADR-P12): Esc must drop it too, so no
        // later worktree create consumes a stale receiver (re-submitting
        // re-arms a fresh one).
        let (_tx, rx) = std::sync::mpsc::channel();
        app.new_session.fetch_done = Some(rx);
        app.modal = modals::Modal::BranchSelector(modals::BranchSelectorModal {
            index: 0,
            branches: vec!["main".into(), "dev".into()],
            filter: Default::default(),
            loading: false,
        });
        // ↓ advances the selection; Esc steps back and wipes the pending state.
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        match app.modal {
            modals::Modal::BranchSelector(ref bs) => assert_eq!(bs.index, 1),
            ref other => panic!("expected the branch selector, got {other:?}"),
        }
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.new_session.repo_path.is_none());
        assert!(app.new_session.all_repos.is_none());
        assert!(app.new_session.normal_repos.is_empty());
        assert!(app.new_session.fetch_done.is_none());
        // Back on the palette, exactly as the user left it.
        let rp = picker_state(&app);
        assert!(rp.selected.contains(std::path::Path::new("/repo")));
        assert!(rp.worktree.contains(std::path::Path::new("/repo")));
        assert_eq!(rp.input.value(), "re");
    }

    #[test]
    fn repo_picker_esc_returns_to_host_picker_when_hosts_configured() {
        let mut app = app_with_sessions(0);
        app.hosts.hosts.push(crate::session::HostDef {
            name: "devbox".into(),
            destination: "me@devbox".into(),
            ..Default::default()
        });

        app.start_new_session();
        assert!(matches!(app.modal, modals::Modal::HostPicker(_)));
        // Pick the remote host → the palette opens for that host. The host
        // picker is type-to-filter now (upstream #9), so navigate with ↓, not
        // `j` (which would type into the filter).
        app.handle_key(KeyCode::Down, KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::RepoPicker(_)));
        assert_eq!(app.new_session.backend.as_deref(), Some("ssh:devbox"));

        // Esc: back to the host picker with the previous choice highlighted.
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        match app.modal {
            modals::Modal::HostPicker(ref hp) => assert_eq!(hp.selected_index, 1),
            ref other => panic!("expected the host picker, got {other:?}"),
        }
        // Esc on the first step cancels.
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
    }

    #[tokio::test]
    async fn session_name_esc_returns_to_branch_selector_and_redispatches_load() {
        let mut app = app_with_sessions(0);
        app.new_session.repo_path = Some(std::env::temp_dir());
        app.new_session.base_branch = Some("main".into());
        app.modal = modals::Modal::SessionName(modals::SessionNameModal::default());
        let dispatched_before = app.perf_counters().branch_loads_dispatched;

        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);

        match app.modal {
            modals::Modal::BranchSelector(ref bs) => assert!(bs.loading),
            ref other => panic!("expected the branch selector, got {other:?}"),
        }
        assert_eq!(
            app.perf_counters().branch_loads_dispatched,
            dispatched_before + 1,
            "back-nav re-dispatches the branch load"
        );
        assert!(
            app.new_session.fetch_done.is_some(),
            "the origin fetch is re-armed for the eventual worktree create"
        );
        assert!(
            app.new_session.base_branch.is_some(),
            "the previous choice is kept for preselection"
        );
    }

    #[test]
    fn session_name_esc_returns_to_repo_picker_in_normal_flow_restoring_backend() {
        let mut app = app_with_sessions(0);
        app.new_session.saved_repo_picker = Some(Box::new(modals::RepoPickerModal::default()));
        app.new_session.additional_dirs = vec![PathBuf::from("/stale")];
        app.new_session.spawn_config = Some(SessionConfig {
            cwd: Some(PathBuf::from("/repo")),
            backend: Some("ssh:devbox".into()),
            ..SessionConfig::default()
        });
        app.modal = modals::Modal::SessionName(modals::SessionNameModal::default());

        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::RepoPicker(_)));
        assert_eq!(
            app.new_session.backend.as_deref(),
            Some("ssh:devbox"),
            "the backend consumed by spawn_session_with_config is restored"
        );
        assert!(app.new_session.spawn_config.is_none());
        assert!(
            app.new_session.additional_dirs.is_empty(),
            "stale derived dirs must not leak into the next spawn"
        );
    }

    #[test]
    fn session_name_esc_returns_to_conversation_dir_step_in_import_flow() {
        let mut app = app_with_sessions(0);
        let mut cp = cc_import::ConversationPickerModal::default();
        cp.dir_input.set("/some/dir");
        cp.chosen = Some(0);
        app.new_session.saved_conversation_picker = Some(Box::new(cp));
        app.new_session.import = true;
        app.new_session.spawn_config = Some(SessionConfig::default());
        app.modal = modals::Modal::SessionName(modals::SessionNameModal::default());

        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);

        match app.modal {
            modals::Modal::ConversationPicker(ref cp) => {
                assert_eq!(cp.dir_input.value(), "/some/dir");
                assert_eq!(cp.chosen, Some(0));
            }
            ref other => panic!("expected the conversation picker, got {other:?}"),
        }
        assert!(!app.new_session.import);
        assert!(app.new_session.spawn_config.is_none());
    }

    #[test]
    fn session_name_esc_cancels_fork_flow() {
        let mut app = app_with_sessions(1);
        app.new_session.fork = true;
        app.new_session.spawn_config = Some(SessionConfig::default());
        app.new_session.spawn_worktrees = vec![WorktreeInfo {
            repo_path: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/wt"),
            branch: "b".into(),
        }];
        app.new_session.parent_session_id = Some(crate::session::SessionId::default());
        app.modal = modals::Modal::SessionName(modals::SessionNameModal::default());

        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::None));
        assert!(app.new_session.spawn_config.is_none());
        assert!(app.new_session.spawn_worktrees.is_empty());
        assert!(!app.new_session.fork);
        assert!(app.new_session.parent_session_id.is_none());
        assert!(
            app.status_message.is_none(),
            "a fork's source worktrees must not toast as 'created'"
        );
    }

    #[test]
    fn session_name_esc_after_create_cancels_and_keeps_worktrees() {
        let mut app = app_with_sessions(0);
        app.new_session.spawn_config = Some(SessionConfig::default());
        app.new_session.spawn_worktrees = vec![WorktreeInfo {
            repo_path: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/wt"),
            branch: "b".into(),
        }];
        app.new_session.saved_repo_picker = Some(Box::new(modals::RepoPickerModal::default()));
        app.modal = modals::Modal::SessionName(modals::SessionNameModal::default());

        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);

        // Can't step back past an already-created worktree: full cancel.
        assert!(matches!(app.modal, modals::Modal::None));
        assert!(app.new_session.spawn_worktrees.is_empty());
        assert!(app.new_session.saved_repo_picker.is_none());
        let msg = app.status_message.as_ref().unwrap();
        assert!(msg.text.contains("kept on disk"));
    }

    #[test]
    fn worktree_name_esc_returns_to_session_name_preserving_name_and_base() {
        let mut app = app_with_sessions(0);
        app.new_session.base_branch = Some("main".into());
        app.new_session.repo_path = Some(PathBuf::from("/repo"));
        app.new_session.session_name = Some("my-feature".into());
        let mut wn = modals::WorktreeNameModal::default();
        wn.name.set("my-feature");
        app.modal = modals::Modal::WorktreeName(wn);

        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);

        match app.modal {
            modals::Modal::SessionName(ref sn) => assert_eq!(sn.name.value(), "my-feature"),
            ref other => panic!("expected the session-name modal, got {other:?}"),
        }
        assert_eq!(
            app.new_session.base_branch.as_deref(),
            Some("main"),
            "the worktree flow stays armed for the re-confirm"
        );
        assert!(app.new_session.repo_path.is_some());
    }

    #[test]
    fn agent_picker_esc_without_pending_create_returns_to_session_name() {
        let mut app = app_with_sessions(0);
        app.new_session.spawn_name = Some("chosen-name".into());
        app.new_session.spawn_config = Some(SessionConfig::default());
        app.modal = modals::Modal::AgentPicker(crate::ui::agent_picker_modal::AgentPickerState {
            choices: vec![],
            selected_index: 0,
            filter: Default::default(),
        });

        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);

        match app.modal {
            modals::Modal::SessionName(ref sn) => assert_eq!(sn.name.value(), "chosen-name"),
            ref other => panic!("expected the session-name modal, got {other:?}"),
        }
        assert!(
            app.new_session.spawn_config.is_some(),
            "re-confirming the name re-runs finish_prepare_spawn from this config"
        );
    }

    #[test]
    fn cancel_flow_clears_saved_wizard_state() {
        let mut app = app_with_sessions(0);
        app.new_session.saved_repo_picker = Some(Box::new(modals::RepoPickerModal::default()));
        app.new_session.saved_conversation_picker =
            Some(Box::new(cc_import::ConversationPickerModal::default()));

        // A fresh flow must not resurrect last flow's parked state.
        app.start_new_session();
        assert!(app.new_session.saved_repo_picker.is_none());
        assert!(app.new_session.saved_conversation_picker.is_none());

        // Esc on the palette (first step, no hosts) cancels and clears too.
        let modals::Modal::RepoPicker(_) = app.modal else {
            panic!("expected repo picker");
        };
        app.new_session.saved_repo_picker = Some(Box::new(modals::RepoPickerModal::default()));
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::None));
        assert!(app.new_session.saved_repo_picker.is_none());
    }

    #[tokio::test]
    async fn back_then_forward_tolerates_inflight_branch_load() {
        let mut app = app_with_sessions(0);
        app.new_session.repo_path = Some(std::env::temp_dir());

        app.start_branch_selection();
        assert_eq!(app.perf_counters().branch_loads_dispatched, 1);

        // Esc out while the load is still in flight, then straight back in.
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::RepoPicker(_)));
        app.new_session.repo_path = Some(std::env::temp_dir());
        app.start_branch_selection();

        assert_eq!(
            app.perf_counters().branch_loads_dispatched,
            2,
            "re-entry re-dispatches instead of refusing"
        );
        match app.modal {
            modals::Modal::BranchSelector(ref bs) => assert!(bs.loading),
            ref other => panic!("expected the branch selector, got {other:?}"),
        }
    }

    /// Typing in the branch selector fuzzy-filters the list; Enter picks the
    /// selected *match* (not the row at the raw index), and the flow advances
    /// to the session-name modal.
    #[test]
    fn branch_selector_typing_filters_and_enter_picks_match() {
        let mut app = app_with_sessions(1);
        app.new_session.repo_path = Some(PathBuf::from("/repo"));
        app.modal = modals::Modal::BranchSelector(modals::BranchSelectorModal {
            index: 0,
            branches: vec!["develop".into(), "main".into(), "feature/map".into()],
            filter: Default::default(),
            loading: false,
        });

        app.handle_key(KeyCode::Char('m'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        match app.modal {
            modals::Modal::BranchSelector(ref bs) => {
                assert_eq!(bs.filter.len(bs.branches.len()), 2, "main + feature/map");
                assert_eq!(bs.index, 0, "cursor snapped to the first match");
            }
            ref other => panic!("expected the branch selector, got {other:?}"),
        }

        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.new_session.base_branch.as_deref(), Some("main"));
        assert!(matches!(app.modal, modals::Modal::SessionName(_)));
    }

    /// Esc on the branch selector is two-stage while a query is typed: the
    /// first press only clears the filter (the modal and its pending flow
    /// survive); the second steps back to the repo picker (the fork's
    /// wizard back-navigation) and drops the pending branch flow.
    #[test]
    fn branch_selector_esc_clears_filter_then_steps_back() {
        let mut app = app_with_sessions(1);
        app.new_session.repo_path = Some(PathBuf::from("/repo"));
        app.modal = modals::Modal::BranchSelector(modals::BranchSelectorModal {
            index: 0,
            branches: vec!["main".into(), "dev".into()],
            filter: Default::default(),
            loading: false,
        });

        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        match app.modal {
            modals::Modal::BranchSelector(ref bs) => {
                assert!(!bs.filter.is_active(), "first Esc only drops the query");
            }
            ref other => panic!("expected the branch selector, got {other:?}"),
        }
        assert!(app.new_session.repo_path.is_some(), "flow still pending");

        // Second Esc: step back to the repo picker (wizard back-nav), dropping
        // the pending branch flow.
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.modal, modals::Modal::RepoPicker(_)));
        assert!(app.new_session.repo_path.is_none());
    }

    /// A branch-filter query typed while the list is still loading (ADR-P12)
    /// applies as soon as the background load delivers.
    #[test]
    fn branch_filter_typed_during_load_applies_on_delivery() {
        let mut app = app_with_sessions(0);
        app.modal = modals::Modal::BranchSelector(modals::BranchSelectorModal {
            index: 0,
            branches: Vec::new(),
            filter: Default::default(),
            loading: true,
        });
        let tx = app.branch_load.start();
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        tx.send(Ok(vec!["main".into(), "dev".into()])).unwrap();

        app.poll_branch_load();

        match app.modal {
            modals::Modal::BranchSelector(ref bs) => {
                assert!(!bs.loading);
                assert_eq!(bs.filter.len(bs.branches.len()), 1, "only dev matches");
                assert_eq!(bs.filter.real_index(bs.index, 2), Some(1));
            }
            ref other => panic!("expected the branch selector, got {other:?}"),
        }
    }

    /// Typing in the agent picker filters on the rendered label (name +
    /// command); Enter confirms the match under the cursor.
    #[test]
    fn agent_picker_typing_filters_and_enter_confirms_match() {
        let mut app = app_with_sessions(0);
        app.modal = modals::Modal::AgentPicker(crate::ui::agent_picker_modal::AgentPickerState {
            choices: vec![
                crate::ui::agent_picker_modal::AgentChoice {
                    name: "claude".into(),
                    command: "claude".into(),
                },
                crate::ui::agent_picker_modal::AgentChoice {
                    name: "codex".into(),
                    command: "codex".into(),
                },
            ],
            selected_index: 1,
            filter: Default::default(),
        });
        // The pending create an overlapping picker parks its choice on.
        let _tx = app.worktree_create.start();
        app.pending_worktree_create = Some(PendingWorktreeCreate {
            backend: None,
            normal_repos: vec![],
            session_name: Some("sess".into()),
            base_branch: "main".into(),
            agent_pick: AgentPick::Open,
        });

        app.handle_key(KeyCode::Char('l'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);

        assert!(matches!(app.modal, modals::Modal::None));
        assert!(matches!(
            app.pending_worktree_create.as_ref().unwrap().agent_pick,
            AgentPick::Chosen(ref agent) if agent == "claude"
        ));
    }

    #[test]
    fn task_list_jk_navigates_selection() {
        let mut app = app_with_sessions(1);
        for t in ["one", "two", "three"] {
            app.db
                .create_task(&crate::storage::tasks::NewTask::local(t))
                .unwrap();
        }
        app.refresh_tasks();
        app.focus = InputFocus::TaskList;
        app.task_ui.task_panel_index = 0;
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.task_ui.task_panel_index, 1);
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.task_ui.task_panel_index, 2);
        // j at the last row stays put (no wrap).
        app.handle_key(KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.task_ui.task_panel_index, 2);
        app.handle_key(KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.task_ui.task_panel_index, 1);
    }

    #[test]
    fn task_list_space_cycles_selected_status() {
        let mut app = app_with_sessions(1);
        let id = app
            .db
            .create_task(&crate::storage::tasks::NewTask::local("t"))
            .unwrap();
        app.refresh_tasks();
        app.focus = InputFocus::TaskList;
        app.task_ui.task_panel_index = 0;
        assert_eq!(
            app.db.get_task(id).unwrap().unwrap().status,
            crate::session::TaskStatus::Todo
        );
        app.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(
            app.db.get_task(id).unwrap().unwrap().status,
            crate::session::TaskStatus::InProgress
        );
    }

    #[test]
    fn task_list_d_soft_deletes_selected() {
        let mut app = app_with_sessions(1);
        app.db
            .create_task(&crate::storage::tasks::NewTask::local("doomed"))
            .unwrap();
        app.refresh_tasks();
        app.focus = InputFocus::TaskList;
        app.task_ui.task_panel_index = 0;
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(
            app.db.list_tasks().unwrap().is_empty(),
            "d should soft-delete the selected task"
        );
    }

    #[test]
    fn task_list_esc_returns_to_session_list() {
        let mut app = app_with_sessions(1);
        app.focus = InputFocus::TaskList;
        app.task_ui.task_editor = Some(modals::TaskEditorModal::new());
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.focus, InputFocus::SessionList);
        assert!(
            app.task_ui.task_editor.is_none(),
            "leaving the panel clears the editor"
        );
    }
}
