pub mod activity;
pub mod agent_def;
pub mod automation;
pub mod cc_activity;
pub mod extension_def;
pub mod host_def;
pub mod keybindings;
pub mod memory;
pub mod message;
pub mod review;
pub mod settings;
pub mod task;
pub mod theme_config;

pub use agent_def::{AgentDef, AgentRegistry};
pub use automation::{
    parse_hhmm, preset_to_cron, Automation, AutomationAction, AutomationRun, AutomationRunStatus,
    AutomationSchedule, ExtraRepo, PromptStep, SchedulePreset, SendTarget, SpawnSessionMode,
};
pub use cc_activity::{
    CcActivity, CcAgent, CcAgentState, CcPhase, CcRunStatus, CcWorkflow, CcWorkflowSummary,
    TranscriptBlock,
};
pub use extension_def::{
    AgentPatch, ConfigMerge, ExtensionAutomation, ExtensionDef, ExtensionFile, ExtensionSession,
    ExtensionSymlink, ExternalFile, PromptStepDecl,
};
pub use host_def::{
    is_remote_backend, is_ssh_backend, is_wsl_backend, HostDef, HostKind, HostRegistry,
    SSH_BACKEND_PREFIX, WSL_BACKEND_PREFIX,
};
pub use keybindings::{
    compact_shortcut, prefix_sections, Action, KeyBindings, KeyChord, KeyContext, PrefixEntry,
    PrefixMode,
};
pub use memory::SessionMemory;
pub use message::SessionMessage;
pub use review::{
    parse_unified_diff, Classification, CommentAnchor, DiffFile, DiffHunk, DiffLine, DiffLineKind,
    FileStatus, ReviewComment, Side,
};
pub use task::{Task, TaskStatus, SOURCE_LOCAL};
pub use theme_config::{ThemePalette, ThemePreset};

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default agent name used when none is configured. Matches the `default`
/// entry of the seeded `agents.toml`; the live default comes from the loaded
/// [`AgentRegistry`].
pub const DEFAULT_AGENT_NAME: &str = "claude";

/// SQLite `metadata` key used to signal "focus this session" between
/// processes. The `notifications` module writes it from the OS notification
/// click handler; the running TUI reads + clears it on each tick via
/// [`crate::storage::Database::take_pending_focus_session_id`]. Defined here
/// in the pure-data layer so both sides reference one source of truth
/// without crossing module boundaries.
pub const PENDING_FOCUS_SESSION_ID_KEY: &str = "pending_focus_session_id";

/// tmux **pane user option** a remote agent's hooks set to report status
/// (`tmux set-option -p @friring_state <working|blocked|done|idle>`). The
/// remote-side replacement for `friring-cli session signal`, which can't work
/// off-local (no CLI on the host, and it would write the host's own DB). The
/// local TUI receives changes over its control-mode connection via a format
/// subscription (see [`REMOTE_HOOK_SUBSCRIPTION`]). Defined in the pure-data
/// layer so `agent` (subscription) and `session_ops` (hook-command rewrite)
/// share one source of truth.
pub const REMOTE_HOOK_STATE_OPTION: &str = "@friring_state";

/// Name of the control-mode format subscription
/// (`refresh-client -B <name>:%*:#{@friring_state}`) that pushes
/// [`REMOTE_HOOK_STATE_OPTION`] changes as `%subscription-changed`
/// notifications for every pane of the attached session.
pub const REMOTE_HOOK_SUBSCRIPTION: &str = "friring-status";

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub repo_path: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(Uuid);

impl Default for SessionId {
    fn default() -> Self {
        Self(Uuid::new_v4())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// A session's lifecycle state, driven by agent hooks (see
/// `friring-cli session signal`). Repo groups in the session list roll up to
/// their most-urgent member so the whole list scans at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// 🟡 The agent is actively running (reported by a hook).
    Working,
    /// 🔴 The agent needs user input or approval (reported by a hook).
    Blocked,
    /// 🔵 A turn just finished; shown until the user switches focus off it.
    Done,
    /// 🟢 Acknowledged (focus moved off a `Done`), at rest, or never-active.
    Idle,
    /// Reserved for a crashed agent. **Not currently derived** — process exit has
    /// no failure signal yet (a clean or crashed exit both map to `Idle`), so this
    /// variant is wired through colour/glyph/rollup but never assigned. Kept for
    /// when exit-code plumbing lands.
    Error,
    /// The session lives on a remote host that is currently unreachable (SSH
    /// down / auth failing / host offline). Assigned to placeholder rows so a
    /// remote session never silently vanishes from the list; cleared to the
    /// real hook-driven status once the host recovers and the session adopts.
    Unreachable,
    /// A **ghost**: the agent process is not running — the session was unloaded
    /// (or lazily restored) and its pane shows the greyed last-saved frame.
    /// No live pane / hooks; loading it (Enter / restart) respawns the agent
    /// and hands the status back to the hook pipeline.
    Unloaded,
}

impl SessionStatus {
    /// A status glyph chosen for **shape** distinctiveness, not just colour, so
    /// the state survives in greyscale / for colour-blind users: a spinner
    /// (working — the live session list animates it, see `ui::SPINNER_FRAMES`)
    /// vs. diamond (blocked) vs. filled circle (done, unseen) vs. hollow circle
    /// (idle, seen) vs. cross (error). The filled/hollow pair makes
    /// done-vs-idle read at a glance.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Working => "◐",
            Self::Blocked => "◆",
            Self::Done => "●",
            Self::Idle => "○",
            Self::Error => "✗",
            Self::Unreachable => "⊘",
            // Dotted circle: reads as "outline of a session" — present but not
            // running — and stays distinct from the hollow Idle circle.
            Self::Unloaded => "◌",
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Working => write!(f, "Working"),
            Self::Blocked => write!(f, "Blocked"),
            Self::Done => write!(f, "Done"),
            Self::Idle => write!(f, "Idle"),
            Self::Error => write!(f, "Error"),
            Self::Unreachable => write!(f, "Unreachable"),
            Self::Unloaded => write!(f, "Unloaded"),
        }
    }
}

/// Agent metrics collected from the Claude CLI statusline mechanism.
///
/// `Serialize` is the `friring-cli session metrics` wire shape: field names are
/// the JSON keys, and absent fields serialize as explicit `null` so a consumer
/// sees a stable key set regardless of what the statusline emitted.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgentMetrics {
    pub model_id: Option<String>,
    pub model_display_name: Option<String>,
    pub total_cost_usd: Option<f64>,
    pub total_duration_ms: Option<u64>,
    pub total_api_duration_ms: Option<u64>,
    pub total_lines_added: Option<u64>,
    pub total_lines_removed: Option<u64>,
    pub total_input_tokens: Option<u64>,
    pub total_output_tokens: Option<u64>,
    pub context_window_size: Option<u64>,
    pub used_percentage: Option<u8>,
    pub current_input_tokens: Option<u64>,
    pub current_output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cli_version: Option<String>,
}

impl AgentMetrics {
    /// Parse a Claude CLI statusline JSON payload.
    ///
    /// Every field is optional and read by JSON pointer: the statusline
    /// contract is the agent's, not ours, so an absent or reshaped key yields
    /// `None` rather than an error. Lives here (pure `session`) rather than in
    /// the app because both readers of the file — the TUI's per-tick refresh
    /// and `friring-cli session metrics` — must agree on the shape.
    pub fn from_statusline_json(raw: &serde_json::Value) -> Self {
        let u64_at = |ptr: &str| raw.pointer(ptr).and_then(serde_json::Value::as_u64);
        let str_at = |ptr: &str| {
            raw.pointer(ptr)
                .and_then(serde_json::Value::as_str)
                .map(String::from)
        };
        Self {
            model_id: str_at("/model/id"),
            model_display_name: str_at("/model/display_name"),
            total_cost_usd: raw
                .pointer("/cost/total_cost_usd")
                .and_then(serde_json::Value::as_f64),
            total_duration_ms: u64_at("/cost/total_duration_ms"),
            total_api_duration_ms: u64_at("/cost/total_api_duration_ms"),
            total_lines_added: u64_at("/cost/total_lines_added"),
            total_lines_removed: u64_at("/cost/total_lines_removed"),
            total_input_tokens: u64_at("/context_window/total_input_tokens"),
            total_output_tokens: u64_at("/context_window/total_output_tokens"),
            context_window_size: u64_at("/context_window/context_window_size"),
            // Clamped: a vendor percentage over 100 would otherwise wrap the u8.
            used_percentage: u64_at("/context_window/used_percentage").map(|v| v.min(100) as u8),
            current_input_tokens: u64_at("/context_window/current_usage/input_tokens"),
            current_output_tokens: u64_at("/context_window/current_usage/output_tokens"),
            cache_creation_input_tokens: u64_at(
                "/context_window/current_usage/cache_creation_input_tokens",
            ),
            cache_read_input_tokens: u64_at(
                "/context_window/current_usage/cache_read_input_tokens",
            ),
            cli_version: str_at("/version"),
        }
    }

    /// Whether the statusline produced nothing at all (every field absent) —
    /// an empty or unrecognized payload, reported as "no metrics" rather than
    /// as a row of nulls.
    pub fn is_empty(&self) -> bool {
        // Every field is an `Option`, so the default *is* "all absent" — and a
        // newly added metric is covered without editing this.
        *self == Self::default()
    }
}

/// Real git state for a session's worktree(s), computed by the app/git layer
/// and surfaced in the info panel. Aggregated across all of a session's
/// worktrees. Agent-neutral (derived from git, not the agent CLI).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStats {
    /// Tracked files with staged/unstaged changes vs HEAD (excludes untracked).
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    /// Untracked files (git status `??`), which a worktree removal would lose.
    pub untracked: usize,
    /// Whether the worktree has uncommitted changes (tracked or untracked).
    pub dirty: bool,
    /// Commits ahead of the upstream/base branch.
    pub ahead: usize,
    /// Commits behind the upstream/base branch.
    pub behind: usize,
}

/// One account-level rate-limit window (e.g. Claude's 5-hour or weekly), as
/// shown by an agent's `/usage` command. Agent-neutral.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageWindow {
    /// Short label, e.g. "5h", "Week", or a model id.
    pub label: String,
    /// Percent of the window consumed, 0–100.
    pub used_percent: f32,
    /// Unix epoch seconds when the window resets, if known.
    pub resets_at: Option<u64>,
}

/// Account-level usage/rate-limit info for an agent, fetched from the vendor
/// (the `/usage`-equivalent). Account-global — but the *account* is scoped to
/// wherever the agent's credentials live, i.e. the host the session runs on
/// (local machine, SSH host, or WSL distro). Agent-neutral.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgentUsage {
    pub windows: Vec<UsageWindow>,
    /// Plan/tier label when known (e.g. "max", "pro").
    pub plan: Option<String>,
    /// Human note when no windows are available (not logged in, API error…).
    pub note: Option<String>,
}

impl AgentUsage {
    /// Nothing worth rendering (no windows and no note).
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty() && self.note.is_none()
    }
}

pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub status: SessionStatus,
    /// Name of the coding agent driving this session (e.g. `"claude"`).
    pub agent: String,
    pub worktrees: Vec<WorktreeInfo>,
    pub agent_session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub additional_dirs: Vec<PathBuf>,
    /// User-chosen directory for the multi-repo symlink workspace (new-session
    /// wizard, local sessions only). `None` = the default id-derived path under
    /// the workspaces root. Persisted so restart, the shell pane, and delete
    /// resolve the same directory the agent was launched in.
    pub workspace_dir: Option<PathBuf>,
    pub backend_id: Option<String>,
    pub shell_backend_id: Option<String>,
    /// Bare host name (e.g. `devbox`) when the session runs on a remote
    /// `ssh:<host>` backend; `None` for local sessions. Drives the remote
    /// indicator in the session list. Set by the agent layer at spawn/adopt.
    pub remote_host: Option<String>,
    /// Agent metrics from the agent's statusline (Claude only).
    pub agent_metrics: Option<AgentMetrics>,
    /// Latest OSC window title the agent emitted (live activity text),
    /// captured from the terminal and refreshed each tick. Agent-neutral.
    pub agent_activity: Option<String>,
    /// Claude Code workflow + subagent activity index (Claude, local sessions
    /// only), polled off-thread from `~/.claude/.../subagents/`. Drives the
    /// activity view's tree; `None` until first scanned (or for non-claude /
    /// remote sessions). Derived from on-disk JSONL, never persisted.
    pub cc_activity: Option<CcActivity>,
    /// Message text from the agent's latest attention notification (OSC 9/777),
    /// shown as the status when `status == SessionStatus::Blocked`.
    pub notification: Option<String>,
    /// Real git state of the session's worktree(s), refreshed periodically by
    /// the app layer. `None` until first computed (or for non-git sessions).
    pub git_stats: Option<GitStats>,
    /// Resident memory of the session's agent process tree, sampled off-thread
    /// by the app layer (see `app::memory`). `None` = not known: a remote
    /// session (the process lives on the host), a pid that wouldn't resolve, or
    /// no scan yet — never rendered as a zero. Derived from the OS process
    /// table, never persisted.
    pub memory: Option<SessionMemory>,
    /// Cached display names for repos, resolved from git remote or directory name.
    /// Order: worktree repos first, then non-worktree additional dirs.
    /// Populated by the app layer at spawn/restore time.
    pub repo_display_names: Vec<String>,
    /// Parent session (lead/worker relationship for orchestration).
    /// `None` for top-level sessions. Purely informational.
    pub parent_session_id: Option<SessionId>,
    /// Manual position in the session list. `None` = never moved: renders
    /// after all ordered sessions, in creation order.
    pub display_order: Option<i64>,
}

impl SessionInfo {
    pub fn new(name: String) -> Self {
        Self {
            id: SessionId::default(),
            name,
            status: SessionStatus::Working,
            agent: DEFAULT_AGENT_NAME.to_string(),
            worktrees: Vec::new(),
            agent_session_id: None,
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            backend_id: None,
            shell_backend_id: None,
            remote_host: None,
            agent_metrics: None,
            agent_activity: None,
            cc_activity: None,
            notification: None,
            git_stats: None,
            memory: None,
            repo_display_names: Vec::new(),
            parent_session_id: None,
            display_order: None,
        }
    }
}

/// A queued command for a session, inserted by MCP and processed by the TUI.
#[derive(Debug, Clone)]
pub struct SessionCommand {
    pub id: i64,
    pub session_id: SessionId,
    pub command: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// Desired friring [`SessionId`] for the spawned session. When set, the
    /// spawn path uses this id instead of minting a fresh one — so the id is
    /// known *before* launch (to inject it into the process env as
    /// `FRIRING_SESSION`) and can be reused across a respawn so a session's
    /// identity is stable for life. `None` mints a new id at spawn.
    pub session_id: Option<SessionId>,
    /// Resume an existing agent session (process restart of a known session).
    pub resume_session_id: Option<String>,
    /// Pin a session id on a fresh spawn (agents that support it).
    pub agent_session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Name of the agent definition to launch (looked up in the registry).
    pub agent: String,
    /// Backend to spawn on (registry name, e.g. `ssh:devbox`). `None`/empty
    /// selects the registry default (`local-tmux`).
    pub backend: Option<String>,
    /// Fork from an existing session's conversation (agents that support it).
    pub fork_session_id: Option<String>,
    /// The friring session name, filling `{name}` tokens in the agent's arg
    /// templates (e.g. claude's `-n {name}`) so the conversation carries the
    /// same name inside the agent. `None`/empty drops the name-carrying
    /// tokens — see [`AgentDef::build_args`].
    pub session_name: Option<String>,
    /// Environment variables injected into the spawned session process
    /// (friring-internal: session id, metrics dir, etc.).
    pub env: HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_metrics_parse_full_statusline_json() {
        let json = serde_json::json!({
            "version": "2.1.58",
            "model": { "id": "claude-opus-4-6", "display_name": "Opus 4.6" },
            "cost": {
                "total_cost_usd": 0.0123,
                "total_duration_ms": 5000,
                "total_api_duration_ms": 3000,
                "total_lines_added": 156,
                "total_lines_removed": 23,
            },
            "context_window": {
                "total_input_tokens": 15200,
                "total_output_tokens": 4500,
                "context_window_size": 200000,
                "used_percentage": 8,
                "current_usage": {
                    "input_tokens": 1200,
                    "output_tokens": 300,
                    "cache_creation_input_tokens": 5000,
                    "cache_read_input_tokens": 2000,
                }
            }
        });
        let m = AgentMetrics::from_statusline_json(&json);
        assert_eq!(m.model_id.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(m.model_display_name.as_deref(), Some("Opus 4.6"));
        assert!((m.total_cost_usd.unwrap() - 0.0123).abs() < 1e-6);
        assert_eq!(m.total_input_tokens, Some(15200));
        assert_eq!(m.total_output_tokens, Some(4500));
        assert_eq!(m.context_window_size, Some(200000));
        assert_eq!(m.used_percentage, Some(8));
        assert_eq!(m.total_lines_added, Some(156));
        assert_eq!(m.total_lines_removed, Some(23));
        assert_eq!(m.cache_read_input_tokens, Some(2000));
        assert_eq!(m.cache_creation_input_tokens, Some(5000));
        assert_eq!(m.cli_version.as_deref(), Some("2.1.58"));
        assert!(!m.is_empty());
    }

    #[test]
    fn agent_metrics_parse_empty_statusline_json() {
        let m = AgentMetrics::from_statusline_json(&serde_json::json!({}));
        assert!(m.model_id.is_none());
        assert!(m.total_cost_usd.is_none());
        assert!(m.used_percentage.is_none());
        assert!(m.is_empty(), "an empty payload reports as no metrics");
    }

    #[test]
    fn agent_metrics_parse_partial_statusline_json() {
        let json = serde_json::json!({
            "model": { "display_name": "Sonnet" },
            "cost": { "total_cost_usd": 0.05 }
        });
        let m = AgentMetrics::from_statusline_json(&json);
        assert_eq!(m.model_display_name.as_deref(), Some("Sonnet"));
        assert!(m.model_id.is_none());
        assert!((m.total_cost_usd.unwrap() - 0.05).abs() < 1e-6);
        assert!(m.total_input_tokens.is_none());
        assert!(!m.is_empty(), "one present field is still metrics");
    }

    #[test]
    fn agent_metrics_clamp_out_of_range_percentage() {
        let json = serde_json::json!({ "context_window": { "used_percentage": 4000 } });
        assert_eq!(
            AgentMetrics::from_statusline_json(&json).used_percentage,
            Some(100)
        );
    }

    #[test]
    fn session_id_display_is_uuid_format() {
        let id = SessionId::default();
        let display = id.to_string();
        assert_eq!(display.len(), 36);
        assert_eq!(display.chars().filter(|&c| c == '-').count(), 4);
    }

    #[test]
    fn session_id_default_is_unique() {
        assert_ne!(SessionId::default(), SessionId::default());
    }

    #[test]
    fn session_status_display_and_icon() {
        assert_eq!(SessionStatus::Working.to_string(), "Working");
        // Glyphs are shape-distinct (not all circles) so status reads without colour.
        assert_eq!(SessionStatus::Working.icon(), "◐");
        assert_eq!(SessionStatus::Blocked.icon(), "◆");
        assert_eq!(SessionStatus::Done.icon(), "●");
        assert_eq!(SessionStatus::Idle.icon(), "○");
        assert_eq!(SessionStatus::Error.icon(), "✗");
    }

    #[test]
    fn session_info_new_defaults() {
        let info = SessionInfo::new("Test".to_string());
        assert_eq!(info.name, "Test");
        assert_eq!(info.status, SessionStatus::Working);
        assert_eq!(info.agent, DEFAULT_AGENT_NAME);
        assert!(info.worktrees.is_empty());
        assert!(info.agent_session_id.is_none());
        assert!(info.cwd.is_none());
        assert!(info.backend_id.is_none());
        assert!(info.agent_metrics.is_none());
        assert!(info.agent_activity.is_none());
        assert!(info.notification.is_none());
    }

    #[test]
    fn default_agent_name_is_claude() {
        assert_eq!(DEFAULT_AGENT_NAME, "claude");
    }

    #[test]
    fn session_config_default_is_empty() {
        let config = SessionConfig::default();
        assert!(config.resume_session_id.is_none());
        assert!(config.agent_session_id.is_none());
        assert!(config.cwd.is_none());
        assert_eq!(config.agent, "");
        assert!(config.env.is_empty());
    }

    #[test]
    fn worktree_info_stores_fields() {
        let wt = WorktreeInfo {
            repo_path: PathBuf::from("/repo"),
            worktree_path: PathBuf::from("/repo/.git/friring-worktrees/feat"),
            branch: "feat".to_string(),
        };
        assert_eq!(wt.repo_path, PathBuf::from("/repo"));
        assert_eq!(wt.branch, "feat");
    }
}
