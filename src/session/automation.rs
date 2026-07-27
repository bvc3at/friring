//! Automations — named, schedulable agent runs.
//!
//! An automation fires on a schedule (one-shot or recurring cron) and, when it
//! fires, delivers an ordered list of prompt steps to an existing session
//! (`Send`) or to a session it spawns — optionally on a fresh git worktree,
//! optionally on a remote host (`Spawn`).
//!
//! This module is pure data + schedule math (no local crate imports), matching
//! the architecture rule for `session`. Persistence lives in
//! `storage::automations`; dispatch lives in the `app` tick loop.

use std::path::PathBuf;
use std::str::FromStr;

use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::SessionId;

/// An additional repository attached to a multi-repo `Spawn`, beyond the
/// primary `repo_path`.
///
/// Each extra repo either gets its **own isolated worktree** — on the session's
/// shared `worktree_branch`, off its own `base_branch` — so its changes stay
/// isolated and PR-able per repo (the flow default), or is attached **as-is** as
/// an additional directory in the multi-repo symlink workspace (no new branch).
/// The whole list is persisted as JSON in the `action_extra_repos` column, so an
/// empty list (the common single-repo case) stores `NULL` and is byte-identical
/// to the pre-multi-repo behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraRepo {
    /// Absolute path to the repository (worktree mode) or directory (dir mode).
    pub repo_path: PathBuf,
    /// `true` = create a worktree on the session's shared branch off
    /// `base_branch`; `false` = attach the directory as-is (no branch).
    pub worktree: bool,
    /// Base branch for the worktree when `worktree` is `true`. `None` falls back
    /// to the primary repo's base (`base_branch` on the `Spawn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

/// Default settle delay between two prompt steps, in milliseconds.
///
/// A step is a *separate* paste + Enter, not a newline in one paste (a
/// bracketed paste submits as a single prompt). The gap has to outlast the
/// agent CLI reacting to the previous submission — most importantly a slash
/// command like `/model`, which opens an autocomplete popup that must settle
/// and close before the next paste lands, or the next step is typed into the
/// popup's filter instead of the prompt box.
pub const DEFAULT_STEP_DELAY_MS: u64 = 1_200;

/// One prompt in an automation's ordered delivery list.
///
/// The whole list is persisted as JSON in the `prompt_steps` column; a
/// single-step automation stores `NULL` there and keeps using the plain
/// `prompt` column, so existing rows stay byte-identical (the
/// `action_extra_repos` precedent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptStep {
    /// The text pasted for this step (submitted with its own Enter).
    pub text: String,
    /// Settle delay *after* this step before the next one is pasted.
    /// `None` = [`DEFAULT_STEP_DELAY_MS`]. Ignored on the last step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
}

impl PromptStep {
    /// A step with the default settle delay.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            delay_ms: None,
        }
    }

    /// Settle delay after this step, falling back to [`DEFAULT_STEP_DELAY_MS`].
    pub fn delay(&self) -> u64 {
        self.delay_ms.unwrap_or(DEFAULT_STEP_DELAY_MS)
    }
}

/// A persisted automation definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Automation {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub schedule: AutomationSchedule,
    /// IANA timezone name (e.g. `"Europe/Zurich"`). `None` = system local time.
    pub timezone: Option<String>,
    pub action: AutomationAction,
    /// First prompt sent to the target session when the automation fires. Also
    /// the *only* prompt when [`prompt_steps`](Self::prompt_steps) is empty —
    /// read through [`steps`](Self::steps) rather than directly.
    pub prompt: String,
    /// The full ordered prompt list. Empty = the single [`prompt`](Self::prompt)
    /// above (every pre-v44 row, and any automation the user never gave a
    /// second step).
    pub prompt_steps: Vec<PromptStep>,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_run_at: Option<u64>,
    /// Next fire time (unix millis). `None` once a one-shot has fired or a
    /// schedule can no longer produce an occurrence.
    pub next_run_at: Option<u64>,
}

impl Automation {
    /// The prompt steps to deliver, in order. Falls back to a single step built
    /// from [`prompt`](Self::prompt) when no multi-step list is stored, so every
    /// firing path can treat single- and multi-step automations identically.
    pub fn steps(&self) -> Vec<PromptStep> {
        if self.prompt_steps.is_empty() {
            vec![PromptStep::new(self.prompt.clone())]
        } else {
            self.prompt_steps.clone()
        }
    }

    /// The session name a `Spawn` fire targets.
    ///
    /// [`SpawnSessionMode::Reuse`] always yields `auto-<id>`, so later fires land
    /// in the same conversation. [`SpawnSessionMode::Fresh`] appends a
    /// timestamp derived from the fire time (`auto-<id>-<YYYYmmdd-HHMMSS-mmm>`,
    /// UTC) so each run starts clean; claim-based firing makes that unique
    /// without a counter, since two fires can never share a millisecond-level
    /// claim — which is why the suffix carries milliseconds too.
    pub fn session_name(&self, fire_millis: u64) -> String {
        match self.action.spawn_session_mode() {
            SpawnSessionMode::Reuse => format!("auto-{}", self.id),
            SpawnSessionMode::Fresh => {
                format!("auto-{}-{}", self.id, fire_suffix(fire_millis))
            }
        }
    }
}

/// Format a fire timestamp as the `YYYYmmdd-HHMMSS-mmm` suffix of a
/// fresh-session name. UTC keeps the suffix stable across DST and host timezone
/// changes; the millisecond component matches the resolution of the claim that
/// produced the fire, so two claims inside one second can't derive one name.
fn fire_suffix(fire_millis: u64) -> String {
    Utc.timestamp_millis_opt(fire_millis as i64)
        .single()
        .map(|dt| format!("{}-{:03}", dt.format("%Y%m%d-%H%M%S"), fire_millis % 1000))
        .unwrap_or_else(|| fire_millis.to_string())
}

/// When an automation fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationSchedule {
    /// Fire exactly once at the given unix-millisecond timestamp.
    Once { at: u64 },
    /// Fire on a recurring cron schedule. Stored in standard 5-field form
    /// (`min hour dom month dow`); 6-field (with leading seconds) is also
    /// accepted for power users.
    Cron { expr: String },
}

impl AutomationSchedule {
    /// Storage discriminant (`schedule_kind` column).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Once { .. } => "once",
            Self::Cron { .. } => "cron",
        }
    }

    /// Storage payload (`schedule_spec` column).
    pub fn spec(&self) -> String {
        match self {
            Self::Once { at } => at.to_string(),
            Self::Cron { expr } => expr.clone(),
        }
    }

    /// Reconstruct from the stored `(schedule_kind, schedule_spec)` columns.
    pub fn from_parts(kind: &str, spec: &str) -> Option<Self> {
        match kind {
            "once" => spec.parse().ok().map(|at| Self::Once { at }),
            "cron" => Some(Self::Cron {
                expr: spec.to_string(),
            }),
            _ => None,
        }
    }

    /// Compute the next fire time strictly after `now_millis`, evaluated in the
    /// given IANA `timezone` (or system local when `None`). Returns `None` when
    /// the schedule has no future occurrence (a past one-shot, or an
    /// unparsable cron expression).
    pub fn next_after(&self, now_millis: u64, timezone: Option<&str>) -> Option<u64> {
        match self {
            Self::Once { at } => (*at > now_millis).then_some(*at),
            Self::Cron { expr } => {
                let schedule = cron::Schedule::from_str(&normalize_cron(expr)).ok()?;
                let now_utc = Utc.timestamp_millis_opt(now_millis as i64).single()?;
                let next_millis = match timezone.and_then(|tz| chrono_tz::Tz::from_str(tz).ok()) {
                    Some(tz) => schedule
                        .after(&now_utc.with_timezone(&tz))
                        .next()?
                        .timestamp_millis(),
                    None => schedule
                        .after(&now_utc.with_timezone(&chrono::Local))
                        .next()?
                        .timestamp_millis(),
                };
                u64::try_from(next_millis).ok()
            }
        }
    }
}

/// Which session a `Send` action delivers to.
///
/// A session **id** is exact but dies with the session: force-deleting the
/// session disables every automation pointing at it
/// (`Database::disable_send_automations_for_session`). A session **name**
/// survives — it is re-resolved at fire time, so an automation keeps working
/// against a session that was closed and recreated under the same name (the
/// behavior extension-declared automations already got by re-linking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendTarget {
    /// Exactly this session, by id.
    Id(SessionId),
    /// Whichever session currently carries this name.
    Name(String),
}

impl SendTarget {
    /// The targeted session id, or `None` for a name target (resolved at fire
    /// time against the live session list).
    pub fn id(&self) -> Option<SessionId> {
        match self {
            Self::Id(id) => Some(*id),
            Self::Name(_) => None,
        }
    }

    /// The targeted session name, or `None` for an id target.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Id(_) => None,
            Self::Name(name) => Some(name),
        }
    }
}

/// Whether a recurring `Spawn` keeps one long-lived session or starts a new one
/// per fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpawnSessionMode {
    /// Reuse `auto-<id>` on every fire, so runs pile into one conversation.
    /// The pre-v44 behavior and still the default.
    #[default]
    Reuse,
    /// Spawn a brand-new session per fire, so each run starts clean. See
    /// [`Automation::session_name`] for the naming scheme.
    Fresh,
}

impl SpawnSessionMode {
    /// Storage value (`action_session_mode` column). `Reuse` stores `NULL`, so
    /// pre-v44 rows decode to the unchanged default.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reuse => "reuse",
            Self::Fresh => "fresh",
        }
    }

    /// Parse a stored / CLI value; unknown values fall back to `Reuse`.
    pub fn from_str_or_default(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "fresh" => Self::Fresh,
            _ => Self::Reuse,
        }
    }
}

/// What an automation does when it fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationAction {
    /// Paste the prompt steps into an existing running session.
    Send { target: SendTarget },
    /// Spawn a new session (optionally on a fresh worktree) and prompt it.
    ///
    /// A single-repo spawn leaves `extra_repos` empty (the common case). When it
    /// is non-empty the session spans multiple repos: the primary plus each
    /// extra, launched in a per-session symlink workspace. See [`ExtraRepo`].
    Spawn {
        repo_path: PathBuf,
        /// `None` = run in the repo root; `Some` = create/use a worktree branch.
        worktree_branch: Option<String>,
        /// Base branch for a new worktree (default `main`).
        base_branch: Option<String>,
        /// Agent name; `None` = registry default.
        agent: Option<String>,
        /// Additional repositories spanned by this session (empty = single-repo).
        extra_repos: Vec<ExtraRepo>,
        /// Host name from `hosts.toml`; `None` = local. The worktree, the tmux
        /// window and the prompt delivery all happen on that host.
        host: Option<String>,
        /// Reuse one session across fires, or spawn a fresh one each time.
        session_mode: SpawnSessionMode,
    },
    /// Run a shell command headlessly (`sh -c <command>`), no agent/session.
    ///
    /// Used by deterministic scheduled jobs (e.g. the task-integration sync
    /// extensions): the command is run by friring's automation scheduler — TUI
    /// and headless `automation tick` alike — and its exit status is recorded in
    /// the run history. There is no model in the loop.
    Exec {
        command: String,
        /// Kill the command after this many seconds. `None` =
        /// [`DEFAULT_EXEC_TIMEOUT_SECS`].
        timeout_secs: Option<u64>,
    },
}

/// How long an `Exec` automation may run before it is killed. Generous enough
/// for a real sync job, finite so a hung command can't pin a `Running` row (and,
/// before the async move, the whole render loop) forever.
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 900;

impl AutomationAction {
    /// Storage discriminant (`action_kind` column).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Send { .. } => "send",
            Self::Spawn { .. } => "spawn",
            Self::Exec { .. } => "exec",
        }
    }

    /// Convenience constructor for the common "send to this exact session" case.
    pub fn send_to(session_id: SessionId) -> Self {
        Self::Send {
            target: SendTarget::Id(session_id),
        }
    }

    /// The `Spawn` session-reuse policy; `Reuse` for every other action.
    pub fn spawn_session_mode(&self) -> SpawnSessionMode {
        match self {
            Self::Spawn { session_mode, .. } => *session_mode,
            _ => SpawnSessionMode::Reuse,
        }
    }

    /// The host this action runs on (`Spawn` only); `None` = local.
    pub fn host(&self) -> Option<&str> {
        match self {
            Self::Spawn { host, .. } => host.as_deref(),
            _ => None,
        }
    }
}

/// Outcome of a single automation fire, kept for history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationRunStatus {
    /// The action was started but hasn't finished. Only an `Exec` run is ever
    /// recorded this way: it runs off the tick thread and updates its own row
    /// on completion, so a long command shows history while it runs.
    Running,
    Success,
    Error,
    Skipped,
}

impl AutomationRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Error => "error",
            Self::Skipped => "skipped",
        }
    }

    /// Parse a status stored in the database, defaulting unknown values to
    /// `Error`.
    pub fn from_db(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "success" => Self::Success,
            "skipped" => Self::Skipped,
            _ => Self::Error,
        }
    }

    /// Whether the run has reached a final state (everything but `Running`).
    pub fn is_final(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// A recorded automation fire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationRun {
    pub id: i64,
    pub automation_id: i64,
    pub started_at: u64,
    pub status: AutomationRunStatus,
    /// Free-text detail: error message, skip reason, human summary.
    pub detail: String,
    /// Session this run sent to / spawned, when one exists. `None` on
    /// pre-v28 rows (the TUI falls back to parsing `detail` for those).
    pub related_session_id: Option<super::SessionId>,
    /// When the run reached a final status (unix millis). `None` while it is
    /// still `Running`, and on every pre-v44 row.
    pub finished_at: Option<u64>,
}

/// A scheduling preset offered in the CLI/TUI. Each compiles to a cron string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulePreset {
    Hourly,
    Daily,
    Weekdays,
    Weekly,
}

impl SchedulePreset {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "hourly" => Some(Self::Hourly),
            "daily" => Some(Self::Daily),
            "weekdays" => Some(Self::Weekdays),
            "weekly" => Some(Self::Weekly),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekdays => "weekdays",
            Self::Weekly => "weekly",
        }
    }
}

/// Compile a preset + `HH:MM` time into a standard 5-field cron expression.
/// `hour`/`minute` come from the time (defaults 0:0). `weekly` fires on the
/// given `dow` (0 = Sunday), defaulting to Monday.
pub fn preset_to_cron(preset: SchedulePreset, hour: u32, minute: u32, dow: u32) -> String {
    match preset {
        SchedulePreset::Hourly => format!("{minute} * * * *"),
        SchedulePreset::Daily => format!("{minute} {hour} * * *"),
        SchedulePreset::Weekdays => format!("{minute} {hour} * * 1-5"),
        SchedulePreset::Weekly => format!("{minute} {hour} * * {dow}"),
    }
}

/// Parse a `--trigger`/editor trigger value into a schedule.
///
/// Accepts: `cron:"<expr>"`, `at:<unix_millis>`, or a preset name
/// (`hourly`/`daily`/`weekdays`/`weekly`) combined with an optional `HH:MM`
/// `time` and `weekday` (0..=6 = Sun..Sat, or 7 = Sun, for `weekly`; default
/// Monday).
pub fn parse_trigger(
    trigger: &str,
    time: Option<&str>,
    weekday: Option<u32>,
) -> Result<AutomationSchedule, String> {
    let trigger = trigger.trim();
    if let Some(expr) = trigger.strip_prefix("cron:") {
        let expr = expr.trim().trim_matches('"').to_string();
        if expr.is_empty() {
            return Err("cron expression must not be empty".into());
        }
        return Ok(AutomationSchedule::Cron { expr });
    }
    if let Some(at) = trigger.strip_prefix("at:") {
        let at: u64 = at
            .trim()
            .parse()
            .map_err(|_| format!("invalid `at:` timestamp: {at}"))?;
        return Ok(AutomationSchedule::Once { at });
    }
    let preset = SchedulePreset::parse(trigger).ok_or_else(|| {
        format!("unknown trigger `{trigger}` (use hourly|daily|weekdays|weekly|cron:…|at:…)")
    })?;
    let (hour, minute) = match time {
        Some(t) => parse_hhmm(t).ok_or_else(|| format!("invalid time `{t}` (expected HH:MM)"))?,
        None => (0, 0),
    };
    // Unix cron day-of-week: 0 and 7 both mean Sunday. Accept the full 0..=7
    // range (normalize_cron remaps it for the `cron` crate) rather than
    // clamping 7 down to 6 (Saturday), which silently broke Sunday.
    let dow = match weekday {
        Some(d) if d > 7 => {
            return Err(format!(
                "invalid weekday `{d}` (use 0=Sun..6=Sat, or 7=Sun)"
            ))
        }
        Some(d) => d,
        None => 1,
    };
    Ok(AutomationSchedule::Cron {
        expr: preset_to_cron(preset, hour, minute, dow),
    })
}

/// Resolved preview of what an automation *would* do on its next fire, as
/// ordered `(label, value)` pairs — the schedule and its next occurrence, the
/// resolved target / spawn parameters / host, and every prompt step in delivery
/// order. Fires nothing and touches no session.
///
/// Shared by the TUI dry-run overlay and `friring-cli automation dry-run` so
/// both describe the same plan; `now_millis` anchors the next-fire computation.
pub fn dry_run_plan(auto: &Automation, now_millis: u64) -> Vec<(String, String)> {
    // The persisted `next_run_at` is the fire the next tick will claim, so it —
    // not a recomputed occurrence — is what the preview must describe. An
    // overdue one-shot has no *future* occurrence yet is about to fire.
    let fire_at = auto
        .enabled
        .then_some(auto.next_run_at)
        .flatten()
        .or_else(|| {
            auto.schedule
                .next_after(now_millis, auto.timezone.as_deref())
        });
    let mut rows = vec![
        ("name".to_string(), auto.name.clone()),
        (
            "enabled".to_string(),
            if auto.enabled { "yes" } else { "no" }.to_string(),
        ),
        (
            "schedule".to_string(),
            format!("{} ({})", auto.schedule.kind(), auto.schedule.spec()),
        ),
        (
            "timezone".to_string(),
            auto.timezone.clone().unwrap_or_else(|| "(local)".into()),
        ),
        (
            "next fire".to_string(),
            match fire_at {
                Some(at) if at <= now_millis => {
                    format!(
                        "due now ({})",
                        format_fire_time(at, auto.timezone.as_deref())
                    )
                }
                Some(at) => format_fire_time(at, auto.timezone.as_deref()),
                None => "never (no further occurrence)".into(),
            },
        ),
        ("action".to_string(), auto.action.kind().to_string()),
    ];
    rows.extend(action_plan_rows(auto, fire_at.unwrap_or(now_millis)));
    if !matches!(auto.action, AutomationAction::Exec { .. }) {
        let steps = auto.steps();
        let total = steps.len();
        for (i, step) in steps.iter().enumerate() {
            rows.push((format!("step {}/{total}", i + 1), step.text.clone()));
            // The delay after the last step is never waited on.
            if i + 1 < total {
                rows.push((
                    format!("  then wait {}", i + 1),
                    format!("{} ms", step.delay()),
                ));
            }
        }
    }
    rows
}

/// The action-specific rows of a [`dry_run_plan`] — resolved target, spawn
/// parameters + host, or the exec command and its timeout. `fire_at` is the
/// timestamp the pending fire will use, so a fresh spawn previews the very
/// session and branch names that fire will produce.
fn action_plan_rows(auto: &Automation, fire_at: u64) -> Vec<(String, String)> {
    match &auto.action {
        AutomationAction::Send { target } => match target {
            SendTarget::Id(id) => vec![("target session".into(), id.to_string())],
            SendTarget::Name(name) => vec![(
                "target session".into(),
                format!("{name} (resolved by name at fire time)"),
            )],
        },
        AutomationAction::Spawn {
            repo_path,
            worktree_branch,
            base_branch,
            agent,
            extra_repos,
            host,
            session_mode,
        } => {
            let mut rows = vec![
                (
                    "host".into(),
                    host.clone().unwrap_or_else(|| "(local)".into()),
                ),
                ("session".into(), {
                    let name = auto.session_name(fire_at);
                    match session_mode {
                        SpawnSessionMode::Reuse => format!("{name} (reused across fires)"),
                        SpawnSessionMode::Fresh => format!("{name} (fresh per fire)"),
                    }
                }),
                ("repo".into(), repo_path.display().to_string()),
                (
                    "worktree".into(),
                    match worktree_branch {
                        Some(b) => format!(
                            "{} off {}",
                            spawn_branch_for(b, *session_mode, fire_at),
                            base_branch.as_deref().unwrap_or("main")
                        ),
                        None => "(repo root)".into(),
                    },
                ),
                (
                    "agent".into(),
                    agent.clone().unwrap_or_else(|| "(registry default)".into()),
                ),
            ];
            for extra in extra_repos {
                rows.push((
                    "extra repo".into(),
                    format!(
                        "{} ({})",
                        extra.repo_path.display(),
                        if extra.worktree {
                            format!(
                                "worktree off {}",
                                extra.base_branch.as_deref().unwrap_or("—")
                            )
                        } else {
                            "attached dir".into()
                        }
                    ),
                ));
            }
            rows
        }
        AutomationAction::Exec {
            command,
            timeout_secs,
        } => vec![
            ("command".into(), command.clone()),
            (
                "timeout".into(),
                format!("{} s", timeout_secs.unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)),
            ),
        ],
    }
}

/// The worktree branch a `Spawn` fire uses.
///
/// [`SpawnSessionMode::Fresh`] suffixes the configured branch with the same
/// fire stamp as the session name: two concurrently-live fresh sessions must not
/// share one worktree, and `create_or_attach_worktree` is idempotent — it would
/// happily hand the second run the first run's checkout.
pub fn spawn_branch_for(branch: &str, mode: SpawnSessionMode, fire_millis: u64) -> String {
    match mode {
        SpawnSessionMode::Reuse => branch.to_string(),
        SpawnSessionMode::Fresh => format!("{branch}-{}", fire_suffix(fire_millis)),
    }
}

/// Format an absolute fire time in the automation's timezone (system local when
/// unset), for the dry-run preview.
fn format_fire_time(at_millis: u64, timezone: Option<&str>) -> String {
    let Some(utc) = Utc.timestamp_millis_opt(at_millis as i64).single() else {
        return at_millis.to_string();
    };
    match timezone.and_then(|tz| chrono_tz::Tz::from_str(tz).ok()) {
        Some(tz) => utc
            .with_timezone(&tz)
            .format("%Y-%m-%d %H:%M:%S %Z")
            .to_string(),
        None => utc
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S %Z")
            .to_string(),
    }
}

/// Validate an IANA timezone name against `chrono-tz`.
///
/// The schedule math silently falls back to system local time for an
/// unrecognized name (`next_after`), which turns a typo into an automation that
/// fires hours off with no signal — so every authoring path (TUI editor, CLI
/// create/edit, manifest import) rejects it up front instead.
pub fn validate_timezone(tz: &str) -> Result<(), String> {
    let tz = tz.trim();
    if tz.is_empty() || chrono_tz::Tz::from_str(tz).is_ok() {
        return Ok(());
    }
    Err(format!(
        "unknown timezone `{tz}` (use an IANA name like Europe/Zurich or UTC)"
    ))
}

/// Parse a relative duration like `30m`, `2h`, `1h30m`, `45s`, or `1d` into
/// milliseconds. Units: `s`, `m`, `h`, `d`. Whitespace between tokens is
/// allowed. Returns `None` if empty, malformed, or a number lacks a unit.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    let mut num = String::new();
    let mut any = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else if !ch.is_whitespace() {
            total = total.checked_add(duration_token_ms(&num, ch)?)?;
            num.clear();
            any = true;
        }
    }
    // A trailing number with no unit is invalid.
    if !num.is_empty() {
        return None;
    }
    any.then_some(total)
}

/// Convert one `<digits><unit>` token into milliseconds. `digits` is the number
/// accumulated so far (must be non-empty) and `unit` the unit char (`s`/`m`/
/// `h`/`d`, case-insensitive). Returns `None` on a missing number, unknown
/// unit, or arithmetic overflow.
fn duration_token_ms(digits: &str, unit: char) -> Option<u64> {
    if digits.is_empty() {
        return None;
    }
    let n: u64 = digits.parse().ok()?;
    let unit_ms: u64 = match unit.to_ascii_lowercase() {
        's' => 1_000,
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        _ => return None,
    };
    n.checked_mul(unit_ms)
}

/// Parse an `HH:MM` string into `(hour, minute)`. Returns `None` if malformed
/// or out of range.
pub fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let hour: u32 = h.parse().ok()?;
    let minute: u32 = m.parse().ok()?;
    (hour < 24 && minute < 60).then_some((hour, minute))
}

/// Normalize a standard Unix cron expression to the form the `cron` crate
/// expects:
///
/// 1. A 5-field expression gets a `0` seconds field prepended (the crate is
///    seconds-leading: `sec min hour dom month dow`).
/// 2. The day-of-week field is translated from Unix numbering (0–6, 0 = Sunday,
///    7 = Sunday) to the crate's numbering (1–7, 1 = Sunday). Named days and
///    `*` pass through untouched.
fn normalize_cron(expr: &str) -> String {
    let mut fields: Vec<String> = expr.split_whitespace().map(String::from).collect();
    if fields.len() == 5 {
        fields.insert(0, "0".to_string());
    }
    // Day-of-week is the sixth field (index 5) in the seconds-leading form.
    if let Some(dow) = fields.get_mut(5) {
        *dow = remap_dow_numbers(dow);
    }
    fields.join(" ")
}

/// Map every integer in a day-of-week field from Unix numbering (0–6/7,
/// 0 = Sunday) to the `cron` crate's numbering (1–7, 1 = Sunday): `n -> n % 7 + 1`.
/// Separators (`*`, `-`, `,`, `/`) and named days pass through unchanged.
fn remap_dow_numbers(field: &str) -> String {
    let mut out = String::new();
    let mut digits = String::new();
    let flush = |digits: &mut String, out: &mut String| {
        if !digits.is_empty() {
            if let Ok(n) = digits.parse::<u32>() {
                out.push_str(&(n % 7 + 1).to_string());
            } else {
                out.push_str(digits);
            }
            digits.clear();
        }
    };
    for ch in field.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            flush(&mut digits, &mut out);
            out.push(ch);
        }
    }
    flush(&mut digits, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2024-01-01 00:00:00 UTC (a Monday) in millis.
    const MON_2024: u64 = 1_704_067_200_000;

    #[test]
    fn once_in_future_returns_at() {
        let s = AutomationSchedule::Once { at: 1000 };
        assert_eq!(s.next_after(500, None), Some(1000));
    }

    #[test]
    fn once_in_past_returns_none() {
        let s = AutomationSchedule::Once { at: 500 };
        assert_eq!(s.next_after(1000, None), None);
    }

    #[test]
    fn cron_daily_next_is_after_now_in_utc() {
        // 09:00 every day.
        let s = AutomationSchedule::Cron {
            expr: "0 9 * * *".to_string(),
        };
        let next = s.next_after(MON_2024, Some("UTC")).unwrap();
        // Same day 09:00 UTC = base + 9h.
        assert_eq!(next, MON_2024 + 9 * 3_600_000);
    }

    #[test]
    fn cron_weekdays_skips_to_weekday() {
        // Saturday 2024-01-06 00:00 UTC.
        let sat = MON_2024 + 5 * 86_400_000;
        let s = AutomationSchedule::Cron {
            expr: "0 9 * * 1-5".to_string(),
        };
        let next = s.next_after(sat, Some("UTC")).unwrap();
        // Next weekday fire = Monday 2024-01-08 09:00 UTC.
        let expected = MON_2024 + 7 * 86_400_000 + 9 * 3_600_000;
        assert_eq!(next, expected);
    }

    #[test]
    fn cron_invalid_returns_none() {
        let s = AutomationSchedule::Cron {
            expr: "not a cron".to_string(),
        };
        assert_eq!(s.next_after(MON_2024, Some("UTC")), None);
    }

    #[test]
    fn schedule_round_trips_through_parts() {
        for s in [
            AutomationSchedule::Once { at: 42 },
            AutomationSchedule::Cron {
                expr: "0 9 * * 1-5".to_string(),
            },
        ] {
            let back = AutomationSchedule::from_parts(s.kind(), &s.spec()).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn preset_compiles_to_expected_cron() {
        assert_eq!(
            preset_to_cron(SchedulePreset::Hourly, 9, 30, 1),
            "30 * * * *"
        );
        assert_eq!(preset_to_cron(SchedulePreset::Daily, 9, 0, 1), "0 9 * * *");
        assert_eq!(
            preset_to_cron(SchedulePreset::Weekdays, 9, 0, 1),
            "0 9 * * 1-5"
        );
        assert_eq!(preset_to_cron(SchedulePreset::Weekly, 9, 0, 0), "0 9 * * 0");
    }

    #[test]
    fn parse_duration_valid_and_invalid() {
        assert_eq!(parse_duration("30m"), Some(1_800_000));
        assert_eq!(parse_duration("2h"), Some(7_200_000));
        assert_eq!(parse_duration("1h30m"), Some(5_400_000));
        assert_eq!(parse_duration("1d"), Some(86_400_000));
        assert_eq!(parse_duration("45s"), Some(45_000));
        assert_eq!(parse_duration("1h 30m"), Some(5_400_000));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("30"), None); // no unit
        assert_eq!(parse_duration("m"), None); // no number
        assert_eq!(parse_duration("30x"), None); // bad unit
    }

    #[test]
    fn parse_trigger_weekly_handles_sunday_both_ways() {
        // Sunday is 0 or 7 in Unix cron; neither must clamp to Saturday (6).
        for sun in [0, 7] {
            let sched = parse_trigger("weekly", Some("09:00"), Some(sun)).unwrap();
            let AutomationSchedule::Cron { expr } = sched else {
                panic!("expected cron schedule");
            };
            assert_eq!(expr, format!("0 9 * * {sun}"));
            // ...and it actually lands on a Sunday: from Saturday 2024-01-06 the
            // next fire is Sunday 2024-01-07 09:00 UTC.
            let sat = MON_2024 + 5 * 86_400_000;
            let next = AutomationSchedule::Cron { expr }
                .next_after(sat, Some("UTC"))
                .unwrap();
            assert_eq!(next, MON_2024 + 6 * 86_400_000 + 9 * 3_600_000);
        }
    }

    #[test]
    fn parse_trigger_rejects_out_of_range_weekday() {
        let err = parse_trigger("weekly", None, Some(8)).unwrap_err();
        assert!(err.contains("weekday"), "got {err}");
    }

    #[test]
    fn parse_hhmm_valid_and_invalid() {
        assert_eq!(parse_hhmm("09:30"), Some((9, 30)));
        assert_eq!(parse_hhmm("23:59"), Some((23, 59)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("9"), None);
        assert_eq!(parse_hhmm("bad"), None);
    }

    #[test]
    fn normalize_cron_prepends_seconds_and_remaps_dow() {
        // 5-field: prepend seconds, remap dow 1-5 (Mon-Fri Unix) -> 2-6 (crate).
        assert_eq!(normalize_cron("0 9 * * 1-5"), "0 0 9 * * 2-6");
        // 6-field: only remap dow.
        assert_eq!(normalize_cron("0 0 9 * * 1-5"), "0 0 9 * * 2-6");
        // Sunday 0 -> 1; star untouched.
        assert_eq!(normalize_cron("0 9 * * 0"), "0 0 9 * * 1");
        assert_eq!(normalize_cron("30 * * * *"), "0 30 * * * *");
    }

    fn sample(action: AutomationAction) -> Automation {
        Automation {
            id: 3,
            name: "nightly".into(),
            enabled: true,
            schedule: AutomationSchedule::Cron {
                expr: "0 9 * * *".into(),
            },
            timezone: Some("UTC".into()),
            action,
            prompt: "do it".into(),
            prompt_steps: Vec::new(),
            created_at: 0,
            updated_at: 0,
            last_run_at: None,
            next_run_at: None,
        }
    }

    fn spawn(session_mode: SpawnSessionMode) -> AutomationAction {
        AutomationAction::Spawn {
            repo_path: PathBuf::from("/repo"),
            worktree_branch: Some("auto/nightly".into()),
            base_branch: None,
            agent: Some("claude".into()),
            extra_repos: Vec::new(),
            host: None,
            session_mode,
        }
    }

    #[test]
    fn steps_falls_back_to_the_single_prompt_column() {
        // Every pre-v44 row: one step, taken from `prompt`.
        let auto = sample(AutomationAction::send_to(SessionId::default()));
        assert_eq!(auto.steps(), vec![PromptStep::new("do it")]);
        assert_eq!(auto.steps()[0].delay(), DEFAULT_STEP_DELAY_MS);
    }

    #[test]
    fn steps_prefers_the_multi_step_list() {
        let mut auto = sample(AutomationAction::send_to(SessionId::default()));
        auto.prompt_steps = vec![
            PromptStep {
                text: "/model opus".into(),
                delay_ms: Some(2_000),
            },
            PromptStep::new("summarize my inbox"),
        ];
        let steps = auto.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].delay(), 2_000);
        assert_eq!(steps[1].delay(), DEFAULT_STEP_DELAY_MS);
    }

    #[test]
    fn reuse_mode_keeps_one_session_name_across_fires() {
        let auto = sample(spawn(SpawnSessionMode::Reuse));
        assert_eq!(auto.session_name(MON_2024), "auto-3");
        assert_eq!(auto.session_name(MON_2024 + 86_400_000), "auto-3");
    }

    #[test]
    fn fresh_mode_stamps_the_session_and_branch_per_fire() {
        let auto = sample(spawn(SpawnSessionMode::Fresh));
        assert_eq!(auto.session_name(MON_2024), "auto-3-20240101-000000-000");
        // A later fire lands on a different session...
        assert_ne!(
            auto.session_name(MON_2024),
            auto.session_name(MON_2024 + 3_600_000)
        );
        // ...and its own branch, so two live runs never share a worktree.
        assert_eq!(
            spawn_branch_for("auto/nightly", SpawnSessionMode::Fresh, MON_2024),
            "auto/nightly-20240101-000000-000"
        );
        assert_eq!(
            spawn_branch_for("auto/nightly", SpawnSessionMode::Reuse, MON_2024),
            "auto/nightly"
        );
    }

    #[test]
    fn fresh_names_stay_distinct_within_one_second() {
        // A manual `automation run` landing right after a scheduled fire (or a
        // 6-field seconds cron) claims twice inside the same second; the derived
        // names must not collide, or the second fire would silently reuse the
        // first run's session and worktree.
        let auto = sample(spawn(SpawnSessionMode::Fresh));
        assert_ne!(
            auto.session_name(MON_2024 + 100),
            auto.session_name(MON_2024 + 900)
        );
        assert_ne!(
            spawn_branch_for("auto/nightly", SpawnSessionMode::Fresh, MON_2024 + 100),
            spawn_branch_for("auto/nightly", SpawnSessionMode::Fresh, MON_2024 + 900)
        );
    }

    #[test]
    fn validate_timezone_accepts_iana_and_empty_but_rejects_typos() {
        assert!(validate_timezone("Europe/Zurich").is_ok());
        assert!(validate_timezone("UTC").is_ok());
        // Empty = "system local", the documented default.
        assert!(validate_timezone("").is_ok());
        let err = validate_timezone("Europe/Zurihc").unwrap_err();
        assert!(err.contains("unknown timezone"), "got {err}");
    }

    #[test]
    fn session_mode_round_trips_and_defaults() {
        assert_eq!(
            SpawnSessionMode::from_str_or_default("fresh"),
            SpawnSessionMode::Fresh
        );
        assert_eq!(
            SpawnSessionMode::from_str_or_default("reuse"),
            SpawnSessionMode::Reuse
        );
        // An unknown (or pre-v44 NULL-decoded) value keeps the old behavior.
        assert_eq!(
            SpawnSessionMode::from_str_or_default("wat"),
            SpawnSessionMode::Reuse
        );
        assert_eq!(SpawnSessionMode::default(), SpawnSessionMode::Reuse);
    }

    #[test]
    fn send_target_exposes_only_its_own_flavor() {
        let id = SessionId::default();
        let by_id = SendTarget::Id(id);
        assert_eq!(by_id.id(), Some(id));
        assert_eq!(by_id.name(), None);
        let by_name = SendTarget::Name("inbox".into());
        assert_eq!(by_name.id(), None);
        assert_eq!(by_name.name(), Some("inbox"));
    }

    #[test]
    fn dry_run_plan_resolves_spawn_parameters_and_every_step() {
        let mut auto = sample(spawn(SpawnSessionMode::Fresh));
        auto.prompt_steps = vec![
            PromptStep {
                text: "/model opus".into(),
                delay_ms: Some(2_000),
            },
            PromptStep::new("go"),
        ];
        let rows = dry_run_plan(&auto, MON_2024);
        let get = |label: &str| {
            rows.iter()
                .find(|(k, _)| k == label)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("no `{label}` row in {rows:?}"))
        };
        assert_eq!(get("action"), "spawn");
        assert_eq!(get("host"), "(local)");
        assert!(get("session").contains("fresh per fire"));
        assert!(get("worktree").starts_with("auto/nightly-"));
        assert_eq!(get("agent"), "claude");
        // Both steps appear, in order, with the settle delay between them.
        assert_eq!(get("step 1/2"), "/model opus");
        assert_eq!(get("  then wait 1"), "2000 ms");
        assert_eq!(get("step 2/2"), "go");
        // The last step's delay is never waited on, so it isn't shown.
        assert!(!rows.iter().any(|(k, _)| k == "  then wait 2"));
    }

    #[test]
    fn dry_run_plan_shows_a_name_target_and_exec_timeout() {
        let auto = sample(AutomationAction::Send {
            target: SendTarget::Name("inbox".into()),
        });
        let rows = dry_run_plan(&auto, MON_2024);
        assert!(rows
            .iter()
            .any(|(k, v)| k == "target session" && v.contains("inbox")));

        let auto = sample(AutomationAction::Exec {
            command: "sync.sh".into(),
            timeout_secs: None,
        });
        let rows = dry_run_plan(&auto, MON_2024);
        assert!(rows
            .iter()
            .any(|(k, v)| k == "timeout" && v == &format!("{DEFAULT_EXEC_TIMEOUT_SECS} s")));
        // An exec has no agent turn, so the plan lists no prompt steps.
        assert!(!rows.iter().any(|(k, _)| k.starts_with("step ")));
    }

    #[test]
    fn dry_run_plan_previews_the_pending_fire_of_an_overdue_automation() {
        // A one-shot whose time has passed has no *future* occurrence, but the
        // next tick will still claim its `next_run_at` — the plan must describe
        // that fire, not report "never".
        let mut auto = sample(spawn(SpawnSessionMode::Fresh));
        auto.schedule = AutomationSchedule::Once { at: MON_2024 };
        auto.next_run_at = Some(MON_2024);
        let rows = dry_run_plan(&auto, MON_2024 + 60_000);
        let get = |label: &str| {
            rows.iter()
                .find(|(k, _)| k == label)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("no `{label}` row in {rows:?}"))
        };
        assert!(get("next fire").starts_with("due now"), "{rows:?}");
        // The previewed names are the ones the pending fire will derive.
        assert!(get("session").contains(&auto.session_name(MON_2024)));
        assert!(get("worktree").starts_with(&spawn_branch_for(
            "auto/nightly",
            SpawnSessionMode::Fresh,
            MON_2024
        )));
    }

    #[test]
    fn run_status_running_round_trips_and_is_not_final() {
        assert_eq!(
            AutomationRunStatus::from_db("running"),
            AutomationRunStatus::Running
        );
        assert!(!AutomationRunStatus::Running.is_final());
        assert!(AutomationRunStatus::Success.is_final());
        assert!(AutomationRunStatus::Skipped.is_final());
        // Anything unrecognized still decodes to Error, as before.
        assert_eq!(
            AutomationRunStatus::from_db("??"),
            AutomationRunStatus::Error
        );
    }
}
