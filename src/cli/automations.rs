//! Automation CRUD subcommands for `friring-cli`.
//!
//! Automations are persisted to the shared database; the running TUI's tick
//! loop is what actually fires them. `run` just marks an automation due so the
//! TUI picks it up on its next tick.

use clap::Subcommand;
use serde_json::{json, Value};

use crate::cli::action::{self, SpawnDeliverError};
use crate::cli::output::{self, CommandOutput};
use crate::session::automation::parse_trigger;
use crate::session::{Automation, AutomationAction, AutomationRun, AutomationRunStatus, SessionId};
use crate::session_ops::SpawnRequest;
use crate::storage::automations::NewAutomation;
use crate::storage::Database;
use crate::sync::current_time_millis;

/// The action-shaping flags, shared verbatim by `create` and `edit` so an
/// automation's action is editable in place rather than delete-and-recreate.
///
/// On `create` the set picks the action (exactly one of `--session` /
/// `--session-name` / `--repo` / `--command`); on `edit` any of those *switches*
/// the action, and the rest amend the current one.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct ActionArgs {
    /// Send action: target an existing session by UUID.
    #[arg(long)]
    pub session: Option<String>,
    /// Send action: target whichever session currently has this name. Survives
    /// the session being closed and recreated.
    #[arg(long)]
    pub session_name: Option<String>,
    /// Spawn action: repository path to run a new session in.
    #[arg(long)]
    pub repo: Option<String>,
    /// Spawn action: optional worktree branch (created if missing).
    #[arg(long)]
    pub worktree: Option<String>,
    /// Spawn action: base branch for a new worktree (default `main`).
    #[arg(long)]
    pub base: Option<String>,
    /// Agent name (spawn action; default registry agent).
    #[arg(long)]
    pub agent: Option<String>,
    /// Spawn action: host from `hosts.toml` to run on (empty = local).
    #[arg(long)]
    pub host: Option<String>,
    /// Spawn action: `reuse` one session across fires (default) or spawn a
    /// `fresh` one per fire.
    #[arg(long)]
    pub session_mode: Option<String>,
    /// Spawn action: extra repo on its own worktree, `path[@base]`. Repeatable.
    #[arg(long = "add-repo")]
    pub add_repo: Vec<String>,
    /// Spawn action: extra directory attached as-is. Repeatable.
    #[arg(long = "add-dir")]
    pub add_dir: Vec<String>,
    /// Exec action: shell command to run headlessly on fire (no session, no
    /// agent). Mutually exclusive with --session/--session-name/--repo.
    #[arg(long)]
    pub command: Option<String>,
    /// Exec action: seconds before the command is killed (default 900).
    #[arg(long)]
    pub timeout: Option<u64>,
}

impl ActionArgs {
    /// Whether any flag was supplied at all — `edit` leaves the stored action
    /// untouched when none were.
    fn is_empty(&self) -> bool {
        self.session.is_none()
            && self.session_name.is_none()
            && self.repo.is_none()
            && self.command.is_none()
            && self.worktree.is_none()
            && self.base.is_none()
            && self.agent.is_none()
            && self.host.is_none()
            && self.session_mode.is_none()
            && self.add_repo.is_empty()
            && self.add_dir.is_empty()
            && self.timeout.is_none()
    }

    /// Which action kind these flags select, when they select one at all.
    fn selected_kind(&self) -> Option<&'static str> {
        if self.command.is_some() {
            Some("exec")
        } else if self.repo.is_some() {
            Some("spawn")
        } else if self.session.is_some() || self.session_name.is_some() {
            Some("send")
        } else {
            None
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Create an automation.
    Create {
        /// Human-readable name.
        #[arg(long)]
        name: String,
        /// When to fire: `hourly` | `daily` | `weekdays` | `weekly` |
        /// `cron:"<expr>"` | `at:<unix_millis>`.
        #[arg(long)]
        trigger: String,
        /// Time of day `HH:MM` for presets (default `00:00`).
        #[arg(long)]
        time: Option<String>,
        /// Day of week (0=Sun..6=Sat, or 7=Sun) for the `weekly` preset
        /// (default Mon).
        #[arg(long)]
        weekday: Option<u32>,
        /// IANA timezone (e.g. `Europe/Zurich`); default system local.
        #[arg(long)]
        timezone: Option<String>,
        /// Prompt text sent on fire (send/spawn actions). Repeat for a
        /// multi-step delivery: each `--prompt` is a separate paste + Enter, in
        /// order (e.g. `--prompt '/model opus' --prompt 'summarize my inbox'`).
        /// Unused by `--command`.
        #[arg(long = "prompt")]
        prompts: Vec<String>,
        /// Milliseconds to settle between prompt steps (default 1200).
        #[arg(long)]
        step_delay: Option<u64>,
        #[command(flatten)]
        action: ActionArgs,
        /// Create the automation disabled.
        #[arg(long)]
        disabled: bool,
    },
    /// List all automations.
    List,
    /// Show one automation by id.
    Show {
        /// Automation id.
        id: i64,
    },
    /// Show what an automation would do on its next fire, without firing it.
    DryRun {
        /// Automation id.
        id: i64,
    },
    /// Print automations as a TOML `[[automations]]` manifest (the same grammar
    /// extensions use), for backup or transfer.
    Export {
        /// Export only this automation (default: all of them).
        #[arg(long)]
        id: Option<i64>,
    },
    /// Create automations from a TOML `[[automations]]` manifest.
    Import {
        /// Path to the manifest file.
        file: String,
        /// Overwrite an existing automation of the same name instead of
        /// skipping it.
        #[arg(long)]
        replace: bool,
    },
    /// Edit an automation.
    Edit {
        /// Automation id.
        id: i64,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        trigger: Option<String>,
        #[arg(long)]
        time: Option<String>,
        #[arg(long)]
        weekday: Option<u32>,
        #[arg(long)]
        timezone: Option<String>,
        /// Replace the whole prompt list. Repeat for multiple steps.
        #[arg(long = "prompt")]
        prompts: Vec<String>,
        /// Milliseconds to settle between prompt steps.
        #[arg(long)]
        step_delay: Option<u64>,
        #[command(flatten)]
        action: ActionArgs,
        /// Enable the automation.
        #[arg(long)]
        enabled: bool,
        /// Disable the automation.
        #[arg(long)]
        disabled: bool,
    },
    /// Remove an automation.
    Remove {
        /// Automation id.
        id: i64,
    },
    /// Fire an automation now (marks it due for the running TUI).
    Run {
        /// Automation id.
        id: i64,
    },
    /// Show an automation's run history.
    Runs {
        /// Automation id.
        id: i64,
        /// Maximum entries (default 20).
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Fire all currently-due automations headlessly (no TUI required). This is
    /// the entry point the tmux heartbeat keeper and any systemd/cron timer call.
    Tick,
}

pub fn run(action: Action, db: &Database) -> Result<CommandOutput, String> {
    match action {
        Action::Create {
            name,
            trigger,
            time,
            weekday,
            timezone,
            prompts,
            step_delay,
            action,
            disabled,
        } => create_automation(
            db,
            CreateArgs {
                name,
                trigger,
                time,
                weekday,
                timezone,
                prompts,
                step_delay,
                action,
                disabled,
            },
        ),
        Action::List => list_automations(db),
        Action::Show { id } => {
            let auto = load(db, id)?;
            Ok(CommandOutput::new(
                automation_to_json(&auto),
                render_automation_detail(&auto),
            ))
        }
        Action::DryRun { id } => dry_run(db, id),
        Action::Export { id } => export_automations(db, id),
        Action::Import { file, replace } => import_automations(db, &file, replace),
        Action::Edit {
            id,
            name,
            trigger,
            time,
            weekday,
            timezone,
            prompts,
            step_delay,
            action,
            enabled,
            disabled,
        } => edit_automation(
            db,
            id,
            EditArgs {
                name,
                trigger,
                time,
                weekday,
                timezone,
                prompts,
                step_delay,
                action,
                enabled,
                disabled,
            },
        ),
        Action::Remove { id } => remove_automation(db, id),
        Action::Run { id } => trigger_automation(db, id),
        Action::Runs { id, limit } => {
            let runs = db
                .list_automation_runs(id, limit.unwrap_or(20))
                .map_err(|e| format!("list_automation_runs: {e}"))?;
            let json = Value::Array(runs.iter().map(run_to_json).collect());
            Ok(CommandOutput::new(json, render_run_history(id, &runs)))
        }
        Action::Tick => {
            let json = tick(db)?;
            let human = render_tick(&json);
            Ok(CommandOutput::new(json, human))
        }
    }
}

/// Parsed `automation create` arguments (the flags, grouped so the handler
/// isn't a dozen positional parameters).
struct CreateArgs {
    name: String,
    trigger: String,
    time: Option<String>,
    weekday: Option<u32>,
    timezone: Option<String>,
    prompts: Vec<String>,
    step_delay: Option<u64>,
    action: ActionArgs,
    disabled: bool,
}

/// Parsed `automation edit` arguments.
struct EditArgs {
    name: Option<String>,
    trigger: Option<String>,
    time: Option<String>,
    weekday: Option<u32>,
    timezone: Option<String>,
    prompts: Vec<String>,
    step_delay: Option<u64>,
    action: ActionArgs,
    enabled: bool,
    disabled: bool,
}

/// Build the persisted prompt-step list from repeated `--prompt` flags.
/// `step_delay` applies to every step but the last (where it is never waited on).
fn build_steps(prompts: &[String], step_delay: Option<u64>) -> Vec<crate::session::PromptStep> {
    let last = prompts.len().saturating_sub(1);
    prompts
        .iter()
        .enumerate()
        .map(|(i, text)| crate::session::PromptStep {
            text: text.clone(),
            delay_ms: (i < last).then_some(step_delay).flatten(),
        })
        .collect()
}

/// Handle `automation create`: validate, persist, and arm the heartbeat.
fn create_automation(db: &Database, args: CreateArgs) -> Result<CommandOutput, String> {
    // An exec automation carries the command, not a prompt; send/spawn need one.
    let prompts: Vec<String> = args
        .prompts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect();
    if args.action.command.is_none() && prompts.is_empty() {
        return Err("prompt must not be empty".into());
    }
    // An exec has no agent to prompt, so a prompt on one would be silently
    // dropped — the same combination `ExtensionAutomation::validate` rejects.
    if args.action.command.is_some() && !prompts.is_empty() {
        return Err("--prompt does not apply to --command (exec has no agent turn)".into());
    }
    let schedule = parse_trigger(&args.trigger, args.time.as_deref(), args.weekday)?;
    let timezone = crate::session::automation::validate_timezone(
        args.timezone.as_deref().unwrap_or_default(),
    )?;
    let action = resolve_action(&args.action, db)?;
    let next_run_at = if args.disabled {
        None
    } else {
        schedule.next_after(current_time_millis(), timezone.as_deref())
    };
    let steps = build_steps(&prompts, args.step_delay);
    let new = NewAutomation {
        name: args.name,
        enabled: !args.disabled,
        schedule,
        timezone,
        action,
        // The `prompt` column keeps the first step so an older friring reading
        // this row still finds a usable prompt.
        prompt: steps.first().map(|s| s.text.clone()).unwrap_or_default(),
        prompt_steps: steps,
        next_run_at,
    };
    let id = db
        .create_automation(&new)
        .map_err(|e| format!("create_automation: {e}"))?;
    if !args.disabled {
        arm_heartbeat();
    }
    let auto = db
        .get_automation(id)
        .map_err(|e| format!("get_automation: {e}"))?
        .ok_or("automation vanished after insert")?;
    let human = format!(
        "Created automation #{} '{}' ({}){}",
        auto.id,
        auto.name,
        auto.schedule.kind(),
        if auto.enabled { "" } else { " — disabled" }
    );
    Ok(CommandOutput::new(automation_to_json(&auto), human))
}

/// Handle `automation list`.
fn list_automations(db: &Database) -> Result<CommandOutput, String> {
    let autos = db
        .list_automations()
        .map_err(|e| format!("list_automations: {e}"))?;
    let json = Value::Array(autos.iter().map(automation_to_json).collect());
    Ok(CommandOutput::new(json, render_automation_list(&autos)))
}

/// Handle `automation edit`: apply the supplied field overrides and persist.
fn edit_automation(db: &Database, id: i64, args: EditArgs) -> Result<CommandOutput, String> {
    let EditArgs {
        name,
        trigger,
        time,
        weekday,
        timezone,
        prompts,
        step_delay,
        action,
        enabled,
        disabled,
    } = args;
    if enabled && disabled {
        return Err("--enabled and --disabled are mutually exclusive".into());
    }
    let mut auto = load(db, id)?;
    // Remembered before `apply_edit_overrides` consumes the list.
    let prompt_supplied = prompts.iter().any(|p| !p.trim().is_empty());
    apply_edit_overrides(
        &mut auto, name, trigger, time, weekday, timezone, prompts, step_delay,
    )?;
    if !action.is_empty() {
        auto.action = apply_action_overrides(&auto.action, &action, db)?;
        // Switching the kind can leave the prompt out of step with the action:
        // an exec turned into a send/spawn has none to deliver, and a send
        // turned into an exec keeps steps it can never use (and would export as
        // an invalid `command` + `prompt` declaration).
        match &auto.action {
            AutomationAction::Exec { .. } => {
                if prompt_supplied {
                    return Err(
                        "--prompt does not apply to --command (exec has no agent turn)".into(),
                    );
                }
                auto.prompt.clear();
                auto.prompt_steps.clear();
            }
            _ => {
                if auto.steps().iter().all(|s| s.text.trim().is_empty()) {
                    return Err(
                        "this action needs a prompt — pass --prompt with the same edit".into(),
                    );
                }
            }
        }
    }
    if disabled {
        auto.enabled = false;
    }
    if enabled {
        auto.enabled = true;
    }
    // Recompute next fire after any schedule/timezone/enabled change.
    auto.next_run_at = if auto.enabled {
        auto.schedule
            .next_after(current_time_millis(), auto.timezone.as_deref())
    } else {
        None
    };
    db.update_automation(&auto)
        .map_err(|e| format!("update_automation: {e}"))?;
    if auto.enabled {
        arm_heartbeat();
    }
    let auto = load(db, id)?;
    Ok(CommandOutput::new(
        automation_to_json(&auto),
        render_automation_detail(&auto),
    ))
}

/// Apply the name/prompt/timezone/schedule overrides supplied to `edit`.
///
/// `--time`/`--weekday` only shape a preset trigger, so they require `--trigger`
/// in the same call (the stored schedule is a raw cron expression with no
/// recoverable preset to re-apply them to). Supplying them alone is a clear
/// error rather than a silent no-op.
#[allow(clippy::too_many_arguments)]
fn apply_edit_overrides(
    auto: &mut Automation,
    name: Option<String>,
    trigger: Option<String>,
    time: Option<String>,
    weekday: Option<u32>,
    timezone: Option<String>,
    prompts: Vec<String>,
    step_delay: Option<u64>,
) -> Result<(), String> {
    if let Some(n) = name {
        auto.name = n;
    }
    if !prompts.is_empty() {
        if prompts.iter().all(|p| p.trim().is_empty()) {
            return Err("prompt must not be empty".into());
        }
        let steps = build_steps(&prompts, step_delay);
        auto.prompt = steps.first().map(|s| s.text.clone()).unwrap_or_default();
        auto.prompt_steps = steps;
    } else if step_delay.is_some() {
        // The delay belongs to a step, so re-applying it means rewriting the
        // list — which needs the prompts too, not a silent partial edit.
        return Err("--step-delay only applies with --prompt".into());
    }
    if let Some(tz) = timezone {
        auto.timezone = crate::session::automation::validate_timezone(&tz)?;
    }
    if let Some(t) = trigger {
        auto.schedule = parse_trigger(&t, time.as_deref(), weekday)?;
    } else if time.is_some() || weekday.is_some() {
        return Err("--time/--weekday only apply with --trigger (a preset)".into());
    }
    Ok(())
}

/// Apply `edit`'s action flags to the stored action.
///
/// A flag that selects a *different* kind (`--session`/`--session-name`,
/// `--repo`, `--command`) switches the action outright; otherwise the flags
/// amend the current one field by field, so `--agent x` on a spawn doesn't
/// clobber its repo or worktree.
fn apply_action_overrides(
    current: &AutomationAction,
    args: &ActionArgs,
    db: &Database,
) -> Result<AutomationAction, String> {
    if let Some(kind) = args.selected_kind() {
        if kind != current.kind() {
            // Switching kinds: build the new action from the flags alone —
            // nothing in the old one carries over.
            return resolve_action(args, db);
        }
    }
    Ok(match current {
        AutomationAction::Send { target } => AutomationAction::Send {
            target: match (&args.session, &args.session_name) {
                (Some(s), _) => crate::session::SendTarget::Id(action::resolve_send_target(db, s)?),
                (None, Some(n)) => crate::session::SendTarget::Name(n.clone()),
                (None, None) => target.clone(),
            },
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
            // Validate the *resulting* selectors: an override can introduce an
            // unknown agent/host just as `create` can.
            let agent = override_optional(agent, &args.agent);
            let host = override_optional(host, &args.host);
            validate_spawn_selectors(agent.as_deref(), host.as_deref())?;
            AutomationAction::Spawn {
                repo_path: args
                    .repo
                    .as_ref()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| repo_path.clone()),
                worktree_branch: override_optional(worktree_branch, &args.worktree),
                base_branch: override_optional(base_branch, &args.base),
                agent,
                // An empty list means "not supplied" — clearing extras is done by
                // switching the action, not by an ambiguous empty flag.
                extra_repos: if args.add_repo.is_empty() && args.add_dir.is_empty() {
                    extra_repos.clone()
                } else {
                    super::parse_extra_repos(&args.add_repo, &args.add_dir)
                },
                host,
                session_mode: parse_session_mode(args.session_mode.as_deref())?
                    .unwrap_or(*session_mode),
            }
        }
        AutomationAction::Exec {
            command,
            timeout_secs,
        } => AutomationAction::Exec {
            command: args.command.clone().unwrap_or_else(|| command.clone()),
            timeout_secs: args.timeout.or(*timeout_secs),
        },
    })
}

/// Apply an optional string override to an optional field: absent leaves the
/// stored value, an empty string clears it, anything else replaces it.
fn override_optional(current: &Option<String>, flag: &Option<String>) -> Option<String> {
    match flag {
        None => current.clone(),
        Some(v) if v.is_empty() => None,
        Some(v) => Some(v.clone()),
    }
}

/// Handle `automation remove`.
fn remove_automation(db: &Database, id: i64) -> Result<CommandOutput, String> {
    match db.delete_automation(id) {
        Ok(true) => Ok(CommandOutput::new(
            json!({ "removed": true, "id": id }),
            format!("Removed automation #{id}."),
        )),
        Ok(false) => Err(format!("Automation not found: {id}")),
        Err(e) => Err(format!("delete_automation: {e}")),
    }
}

/// Handle `automation run`: mark the automation due for the next tick.
fn trigger_automation(db: &Database, id: i64) -> Result<CommandOutput, String> {
    match db.trigger_automation_now(id) {
        Ok(true) => Ok(CommandOutput::new(
            json!({ "triggered": true, "id": id }),
            format!("Triggered automation #{id} (fires on the next tick)."),
        )),
        Ok(false) => Err(format!("Automation not found: {id}")),
        Err(e) => Err(format!("trigger_automation_now: {e}")),
    }
}

/// Render the automation list as an aligned table (or a friendly empty line).
fn render_automation_list(autos: &[Automation]) -> String {
    if autos.is_empty() {
        return "No automations.".to_string();
    }
    let rows: Vec<Vec<String>> = autos
        .iter()
        .map(|a| {
            vec![
                a.id.to_string(),
                if a.enabled { "on" } else { "off" }.to_string(),
                a.name.clone(),
                a.schedule.kind().to_string(),
                action::action_label(Some(&a.action)),
            ]
        })
        .collect();
    output::table(&["ID", "STATE", "NAME", "SCHEDULE", "ACTION"], &rows)
}

/// Render a single automation as an aligned key/value block. Multi-step
/// prompts list every step — showing only the `prompt` column would report
/// step 1 as if it were the whole delivery.
fn render_automation_detail(a: &Automation) -> String {
    let mut pairs: Vec<(String, String)> = vec![
        ("id".into(), a.id.to_string()),
        ("name".into(), a.name.clone()),
        ("enabled".into(), a.enabled.to_string()),
        (
            "schedule".into(),
            format!("{} ({})", a.schedule.kind(), a.schedule.spec()),
        ),
        ("timezone".into(), output::dash(a.timezone.as_deref())),
        ("action".into(), action::action_label(Some(&a.action))),
    ];
    if let Some(host) = a.action.host() {
        pairs.push(("host".into(), host.to_string()));
    }
    if let AutomationAction::Spawn { session_mode, .. } = &a.action {
        pairs.push(("session".into(), session_mode.as_str().to_string()));
    }
    if !matches!(a.action, AutomationAction::Exec { .. }) {
        let steps = a.steps();
        let total = steps.len();
        for (i, step) in steps.iter().enumerate() {
            let label = if total > 1 {
                format!("step {}/{total}", i + 1)
            } else {
                "prompt".to_string()
            };
            pairs.push((label, step.text.clone()));
        }
    }
    let borrowed: Vec<(&str, String)> =
        pairs.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    output::kv(&borrowed)
}

/// Render an automation's run history as a table.
fn render_run_history(id: i64, runs: &[AutomationRun]) -> String {
    if runs.is_empty() {
        return format!("No run history for automation #{id}.");
    }
    let rows: Vec<Vec<String>> = runs
        .iter()
        .map(|r| {
            vec![
                r.id.to_string(),
                r.status.as_str().to_string(),
                r.detail.clone(),
            ]
        })
        .collect();
    output::table(&["RUN", "STATUS", "DETAIL"], &rows)
}

/// One-line human summary of an `automation tick`.
fn render_tick(v: &Value) -> String {
    let count = |key: &str| v.get(key).and_then(Value::as_array).map_or(0, Vec::len);
    let fired = count("fired");
    let skipped = count("skipped");
    let healed = count("healed");
    format!("Tick: {fired} fired, {skipped} skipped, {healed} extension(s) healed.")
}

/// Fire every due automation headlessly: claim (atomic CAS, so this is safe to
/// run alongside the TUI and other tickers), perform the action, record the run.
fn tick(db: &Database) -> Result<Value, String> {
    // Self-heal active extensions before firing: this runs from the tmux
    // heartbeat keeper every 60s, so a deleted flow session/automation is
    // recreated even with the TUI closed. Best-effort — heal messages are
    // reported but never abort the due-automation pass below.
    let healed = crate::session_ops::heal_active_extensions(db);
    for m in &healed {
        tracing::info!("{m}");
    }
    // Keep the auto-activated built-in `hooks` extension wired up headlessly too.
    for m in &crate::session_ops::ensure_builtin_hooks_extension(db) {
        tracing::info!("{m}");
    }
    // Best-effort retention sweep of the inter-session mailbox (read messages
    // older than the default window), so the queue self-bounds with the TUI
    // closed. Never abort the due-automation pass over it.
    if let Err(e) = db.prune_old_messages() {
        tracing::debug!("prune_old_messages: {e}");
    }
    // A `running` exec row whose worker died with its process would otherwise
    // show as running forever. The TUI does this on startup, but a keeper /
    // OS-timer install may never open one; the reaper's per-run age floor makes
    // it safe to call from any dispatcher. Best-effort.
    match db.reap_orphaned_automation_runs() {
        Ok(n) if n > 0 => tracing::info!("Closed {n} automation run(s) left running by a crash"),
        Ok(_) => {}
        Err(e) => tracing::warn!("reap_orphaned_automation_runs: {e}"),
    }
    let now = current_time_millis();
    let due = db
        .due_automations(now)
        .map_err(|e| format!("due_automations: {e}"))?;
    let mut fired = Vec::new();
    let mut skipped = Vec::new();
    for auto in due {
        let next = auto.schedule.next_after(now, auto.timezone.as_deref());
        let claimed = db
            .claim_due_automation(auto.id, auto.next_run_at.unwrap_or(0), next, now)
            .map_err(|e| format!("claim_due_automation: {e}"))?;
        if !claimed {
            // Another firer (TUI / concurrent tick) won the claim. This logs at
            // debug! (invisible at the default level), so report it in the JSON too.
            tracing::debug!(
                automation_id = auto.id,
                "automation claim lost to a concurrent firer"
            );
            skipped.push(json!({ "id": auto.id, "reason": "claim-lost" }));
            continue;
        }
        let FireOutcome {
            status,
            detail,
            related,
            recorded,
        } = fire_headless(db, &auto, now);
        // `exec` already owns a history row (recorded `Running`, then closed
        // out); every other action records its single row here.
        if !recorded {
            let _ = db.record_automation_run(auto.id, status, &detail, related);
        }
        fired.push(json!({
            "id": auto.id,
            "status": status.as_str(),
            "detail": detail,
        }));
    }
    Ok(json!({ "fired": fired, "skipped": skipped, "healed": healed }))
}

/// Execute one automation's action without a TUI, returning the run outcome.
///
/// `send` types into the still-alive tmux window; `spawn` creates a session
/// headlessly (the TUI adopts it by name on next startup) and delivers the
/// prompt steps via a deferred tmux timer once the agent boots. Both route
/// through the spawn's host (`MuxTarget`), so a remote automation types into the
/// server that actually owns its window.
fn fire_headless(db: &Database, auto: &Automation, now: u64) -> FireOutcome {
    // tmux helpers are reached via fully-qualified paths (no `use crate::agent`)
    // to keep the cli module free of an `agent` import — see
    // tests/architecture_rules.rs::cli_module_isolation.
    match &auto.action {
        AutomationAction::Send { target } => fire_send(db, auto, target).into(),
        AutomationAction::Spawn { .. } => fire_spawn(db, auto, now).into(),
        AutomationAction::Exec {
            command,
            timeout_secs,
        } => fire_exec(db, auto.id, command, *timeout_secs),
    }
}

/// What one headless fire produced.
struct FireOutcome {
    status: AutomationRunStatus,
    detail: String,
    related: Option<SessionId>,
    /// Whether the action already wrote its own history row, so [`tick`] must
    /// not append a second one for the same fire.
    recorded: bool,
}

impl From<(AutomationRunStatus, String, Option<SessionId>)> for FireOutcome {
    fn from((status, detail, related): (AutomationRunStatus, String, Option<SessionId>)) -> Self {
        Self {
            status,
            detail,
            related,
            recorded: false,
        }
    }
}

/// Execute an `exec` automation headlessly via the shared runner. No session is
/// involved (deterministic scheduled job).
///
/// Unlike the TUI this waits for the command inline — a `tick` process that
/// detached the work would exit and strand the row as `running` — but it records
/// the same `Running` → final pair, so a concurrently-open TUI sees the run
/// appear while the command is still going.
fn fire_exec(
    db: &Database,
    automation_id: i64,
    command: &str,
    timeout_secs: Option<u64>,
) -> FireOutcome {
    let run_id = db
        .record_automation_run(automation_id, AutomationRunStatus::Running, command, None)
        .ok();
    let (status, detail) = crate::session_ops::run_exec_command_with_timeout(command, timeout_secs);
    // Close out the `Running` row this fire already owns, so the fire keeps
    // exactly one history entry. If opening it failed, report `recorded: false`
    // and let `tick` write the final row instead of losing the run entirely.
    let recorded = match run_id {
        Some(id) => match db.finish_automation_run(id, status, &detail) {
            Ok(updated) => updated,
            Err(e) => {
                tracing::warn!("Failed to finish automation run {id}: {e}");
                false
            }
        },
        None => false,
    };
    FireOutcome {
        status,
        detail,
        related: None,
        recorded,
    }
}

/// Execute a `send` automation: type the prompt steps into the target session's
/// still-alive tmux window. A name target is re-resolved against the live
/// sessions here, so it survives the session being recreated.
fn fire_send(
    db: &Database,
    auto: &Automation,
    target: &crate::session::SendTarget,
) -> (AutomationRunStatus, String, Option<SessionId>) {
    let resolved = match target {
        crate::session::SendTarget::Id(id) => db
            .get_session_name(*id)
            .map(|name| name.map(|name| (*id, name))),
        crate::session::SendTarget::Name(name) => db
            .list_active_sessions()
            .map(|rows| rows.into_iter().find(|s| s.name == *name))
            .map(|found| found.map(|s| (s.id, s.name))),
    };
    let (session_id, name) = match resolved {
        Ok(Some(found)) => found,
        Ok(None) => {
            return (
                AutomationRunStatus::Skipped,
                "target session not found".into(),
                None,
            )
        }
        Err(e) => return (AutomationRunStatus::Error, format!("{e}"), None),
    };
    // A `send` always targets a session friring already owns, so its window
    // lives on the local server (remote sessions are spawn-authored).
    let mux = crate::agent::tmux::MuxTarget::local();
    if !crate::agent::tmux::window_exists_on(&mux, &name) {
        return (
            AutomationRunStatus::Skipped,
            "target session not running".into(),
            None,
        );
    }
    match crate::agent::tmux::send_prompt_steps_now(&mux, &name, &auto.steps()) {
        Ok(()) => (
            AutomationRunStatus::Success,
            format!("sent to {session_id}"),
            Some(session_id),
        ),
        Err(e) => (AutomationRunStatus::Error, e.to_string(), None),
    }
}

/// Execute a `spawn` automation: reuse an existing window or spawn a new
/// headless session, then deliver the prompt steps once the agent boots.
fn fire_spawn(
    db: &Database,
    auto: &Automation,
    now: u64,
) -> (AutomationRunStatus, String, Option<SessionId>) {
    let AutomationAction::Spawn {
        repo_path,
        worktree_branch,
        base_branch,
        agent,
        extra_repos,
        host,
        session_mode,
    } = &auto.action
    else {
        return (AutomationRunStatus::Error, "not a spawn".into(), None);
    };
    // The same cap the TUI applies, so which firer wins the claim can't change
    // whether an hourly fresh-per-fire automation accumulates sessions.
    if *session_mode == crate::session::SpawnSessionMode::Fresh {
        let prefix = crate::session::automation::fresh_session_prefix(auto.id);
        let live = db
            .list_active_sessions()
            .map(|rows| rows.iter().filter(|s| s.name.starts_with(&prefix)).count())
            .unwrap_or(0);
        if let Some(reason) = crate::session::automation::fresh_session_cap_reason(live) {
            return (AutomationRunStatus::Skipped, reason, None);
        }
    }
    let mux = match crate::agent::tmux::MuxTarget::resolve(host.as_deref()) {
        Ok(m) => m,
        Err(e) => return (AutomationRunStatus::Error, e.to_string(), None),
    };
    let name = auto.session_name(now);
    let steps = auto.steps();
    // Reuse an existing session window (later fires / restored sessions). A
    // fresh-per-fire automation never matches — its name carries this fire's
    // stamp.
    if crate::agent::tmux::window_exists_on(&mux, &name) {
        // The reused window's session id has no cheap lookup here.
        return match crate::agent::tmux::send_prompt_steps_now(&mux, &name, &steps) {
            Ok(()) => (AutomationRunStatus::Success, format!("reused {name}"), None),
            Err(e) => (AutomationRunStatus::Error, e.to_string(), None),
        };
    }
    let req = SpawnRequest {
        name: name.clone(),
        repo_path: repo_path.to_path_buf(),
        // A fresh session gets its own branch, so two live runs never share one
        // worktree (see `session::automation::spawn_branch_for`).
        worktree_branch: worktree_branch
            .as_deref()
            .map(|b| crate::session::automation::spawn_branch_for(b, *session_mode, now)),
        base_branch: base_branch.clone(),
        agent: agent.clone(),
        agent_session_id: None,
        host: host.clone(),
        parent_session_id: None,
        task_id: None,
        extra_repos: extra_repos.to_vec(),
    };
    match action::spawn_and_deliver_steps(db, &name, req, &steps) {
        Ok(session_id) => (
            AutomationRunStatus::Success,
            format!("spawned {name}"),
            Some(session_id),
        ),
        // A spawned-but-undelivered run still records its session id.
        Err(SpawnDeliverError::Deliver {
            session_id,
            message,
        }) => (AutomationRunStatus::Error, message, Some(session_id)),
        Err(SpawnDeliverError::Spawn(e)) => (AutomationRunStatus::Error, e, None),
    }
}

fn load(db: &Database, id: i64) -> Result<Automation, String> {
    db.get_automation(id)
        .map_err(|e| format!("get_automation: {e}"))?
        .ok_or_else(|| format!("Automation not found: {id}"))
}

/// Reject a spawn naming an agent or host that isn't configured.
///
/// Neither is checked downstream: `session_ops::resolve_agent_def` falls back to
/// the registry default for an unknown name, so a typo would silently launch the
/// wrong agent on every fire. The TUI editor performs exactly these checks.
/// (`crate::agent::…` is reached by fully-qualified path only — `cli` may not
/// `use` it; see `tests/architecture_rules.rs`.)
fn validate_spawn_selectors(agent: Option<&str>, host: Option<&str>) -> Result<(), String> {
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

/// Parse an authoring-path `--session-mode` / manifest `session_mode` strictly.
///
/// [`SpawnSessionMode::from_str_or_default`](crate::session::SpawnSessionMode::from_str_or_default)
/// maps anything unknown to `Reuse`, which is right when decoding a stored
/// column (a pre-v44 `NULL` must keep the old behavior) but wrong when a user
/// types `--session-mode frehs` and gets silently reused sessions.
fn parse_session_mode(
    raw: Option<&str>,
) -> Result<Option<crate::session::SpawnSessionMode>, String> {
    use crate::session::SpawnSessionMode;
    let Some(raw) = raw else {
        return Ok(None);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "reuse" => Ok(Some(SpawnSessionMode::Reuse)),
        "fresh" => Ok(Some(SpawnSessionMode::Fresh)),
        other => Err(format!(
            "invalid session mode `{other}` (use reuse or fresh)"
        )),
    }
}

/// Resolve the action from the flags — exactly one of `--session` /
/// `--session-name` (send), `--repo` (spawn), or `--command` (exec).
fn resolve_action(args: &ActionArgs, db: &Database) -> Result<AutomationAction, String> {
    let selectors = [
        args.session.is_some() || args.session_name.is_some(),
        args.repo.is_some(),
        args.command.is_some(),
    ]
    .into_iter()
    .filter(|set| *set)
    .count();
    if selectors > 1 {
        return Err(
            "specify only one of --session/--session-name (send), --repo (spawn), or \
             --command (exec)"
                .into(),
        );
    }
    if let Some(cmd) = &args.command {
        if cmd.trim().is_empty() {
            return Err("--command must not be empty".into());
        }
        return Ok(AutomationAction::Exec {
            command: cmd.clone(),
            timeout_secs: args.timeout,
        });
    }
    if let Some(repo) = &args.repo {
        // Fail here rather than at fire time, hours later, in an error run.
        validate_spawn_selectors(args.agent.as_deref(), args.host.as_deref())?;
        return Ok(AutomationAction::Spawn {
            repo_path: repo.into(),
            worktree_branch: args.worktree.clone(),
            base_branch: args.base.clone(),
            agent: args.agent.clone(),
            extra_repos: super::parse_extra_repos(&args.add_repo, &args.add_dir),
            host: args.host.clone().filter(|h| !h.is_empty()),
            session_mode: parse_session_mode(args.session_mode.as_deref())?.unwrap_or_default(),
        });
    }
    match (&args.session, &args.session_name) {
        (Some(s), _) => Ok(AutomationAction::Send {
            target: crate::session::SendTarget::Id(action::resolve_send_target(db, s)?),
        }),
        (None, Some(n)) => Ok(AutomationAction::Send {
            target: crate::session::SendTarget::Name(n.clone()),
        }),
        (None, None) => Err(
            "specify --session/--session-name (send), --repo (spawn), or --command (exec)".into(),
        ),
    }
}

/// Handle `automation dry-run`: report the resolved plan without firing.
fn dry_run(db: &Database, id: i64) -> Result<CommandOutput, String> {
    let auto = load(db, id)?;
    let rows = crate::session::automation::dry_run_plan(&auto, current_time_millis());
    // An ordered array, not an object: the plan repeats the `extra repo` label
    // once per extra repository, so a map would keep only the last one.
    let json = Value::Array(
        rows.iter()
            .map(|(k, v)| json!({ "label": k, "value": v }))
            .collect(),
    );
    let pairs: Vec<(&str, String)> = rows.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    let human = format!(
        "Dry run — automation #{id} would do this on its next fire (nothing was \
         fired):\n{}",
        output::kv(&pairs)
    );
    Ok(CommandOutput::new(json, human))
}

/// Handle `automation export`: render automations as a TOML
/// `[[automations]]` manifest — the same grammar extensions declare, so an
/// exported file can be dropped into an `extension.toml` unchanged.
fn export_automations(db: &Database, id: Option<i64>) -> Result<CommandOutput, String> {
    let autos = match id {
        Some(id) => vec![load(db, id)?],
        None => db
            .list_automations()
            .map_err(|e| format!("list_automations: {e}"))?,
    };
    let manifest = crate::session::extension_def::AutomationManifest {
        automations: autos.iter().map(automation_to_manifest).collect(),
    };
    let toml = toml::to_string_pretty(&manifest).map_err(|e| format!("serialize TOML: {e}"))?;
    Ok(CommandOutput::new(
        json!({ "count": autos.len(), "toml": toml }),
        toml,
    ))
}

/// Handle `automation import`: create automations from a TOML
/// `[[automations]]` manifest. An existing automation of the same name is
/// skipped unless `replace` is set (name is the manifest's identity, matching
/// how extensions reconcile theirs).
fn import_automations(db: &Database, file: &str, replace: bool) -> Result<CommandOutput, String> {
    let path = crate::paths::expand_tilde(file);
    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let manifest: crate::session::extension_def::AutomationManifest =
        toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let existing: std::collections::HashMap<String, i64> = db
        .list_automations()
        .map_err(|e| format!("list_automations: {e}"))?
        .into_iter()
        .map(|a| (a.name, a.id))
        .collect();

    // Convert everything before touching the database: `--replace` deletes the
    // existing automation *and its whole run history*, so a manifest that only
    // fails on its third entry must not have destroyed the first two.
    let (mut created, mut replaced, mut skipped) = (Vec::new(), Vec::new(), Vec::new());
    let mut plan: Vec<(NewAutomation, Option<i64>)> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for decl in &manifest.automations {
        decl.validate()?;
        // Name is the manifest's identity (the skip/replace key), and the table
        // has no UNIQUE constraint — two entries sharing one would both insert.
        if !seen.insert(decl.name.as_str()) {
            return Err(format!(
                "manifest declares '{}' twice; each automation name must be unique",
                decl.name
            ));
        }
        match existing.get(&decl.name) {
            Some(_) if !replace => {
                skipped.push(decl.name.clone());
                continue;
            }
            Some(&id) => {
                plan.push((manifest_to_new_automation(decl)?, Some(id)));
                replaced.push(decl.name.clone());
            }
            None => {
                plan.push((manifest_to_new_automation(decl)?, None));
                created.push(decl.name.clone());
            }
        }
    }
    // One transaction, so a mid-batch failure leaves nothing half-applied.
    let tx = db
        .conn_ref()
        .unchecked_transaction()
        .map_err(|e| format!("begin transaction: {e}"))?;
    for (new, replaced_id) in &plan {
        if let Some(id) = replaced_id {
            db.delete_automation(*id)
                .map_err(|e| format!("delete_automation: {e}"))?;
        }
        db.create_automation(new)
            .map_err(|e| format!("create_automation: {e}"))?;
    }
    tx.commit().map_err(|e| format!("commit import: {e}"))?;
    if !created.is_empty() || !replaced.is_empty() {
        arm_heartbeat();
    }
    let human = format!(
        "Imported {} automation(s): {} created, {} replaced, {} skipped (already exist).",
        created.len() + replaced.len(),
        created.len(),
        replaced.len(),
        skipped.len()
    );
    Ok(CommandOutput::new(
        json!({ "created": created, "replaced": replaced, "skipped": skipped }),
        human,
    ))
}

/// Turn an imported manifest entry into a row to insert. A `session_ref` stays a
/// **name** target (see `ExtensionAutomation::to_action`), so an imported
/// automation doesn't carry a session UUID from another machine.
fn manifest_to_new_automation(
    decl: &crate::session::ExtensionAutomation,
) -> Result<NewAutomation, String> {
    let timezone = crate::session::automation::validate_timezone(
        decl.timezone.as_deref().unwrap_or_default(),
    )?;
    let schedule = parse_trigger(&decl.trigger, None, None)?;
    let action = decl.to_action(None)?;
    // A manifest is authored by hand as often as it is exported, so its spawn
    // selectors get the same check `create`/`edit` apply.
    if let AutomationAction::Spawn { agent, host, .. } = &action {
        parse_session_mode(decl.session_mode.as_deref())?;
        validate_spawn_selectors(agent.as_deref(), host.as_deref())?;
    }
    let steps = decl.steps();
    if steps.is_empty() && !matches!(action, AutomationAction::Exec { .. }) {
        return Err(format!("automation '{}' has no prompt", decl.name));
    }
    let enabled = decl.enabled.unwrap_or(true);
    let next_run_at = enabled
        .then(|| schedule.next_after(current_time_millis(), timezone.as_deref()))
        .flatten();
    Ok(NewAutomation {
        name: decl.name.clone(),
        enabled,
        schedule,
        timezone,
        action,
        prompt: steps.first().map(|s| s.text.clone()).unwrap_or_default(),
        prompt_steps: steps,
        next_run_at,
    })
}

/// Project a stored automation onto the manifest grammar for `export`.
fn automation_to_manifest(a: &Automation) -> crate::session::ExtensionAutomation {
    use crate::session::{ExtensionAutomation, SpawnSessionMode};
    let mut decl = ExtensionAutomation {
        name: a.name.clone(),
        trigger: match &a.schedule {
            crate::session::AutomationSchedule::Once { at } => format!("at:{at}"),
            crate::session::AutomationSchedule::Cron { expr } => format!("cron:{expr}"),
        },
        timezone: a.timezone.clone(),
        enabled: Some(a.enabled),
        ..ExtensionAutomation::default()
    };
    // Only an agent-turn action carries prompts. `Automation::steps` synthesizes
    // a single empty step for an exec row, and `ExtensionAutomation::validate`
    // rejects a `prompt` next to a `command` — so writing one here would make
    // every exported exec fail its own import.
    let set_prompts = |decl: &mut ExtensionAutomation| {
        let steps = a.steps();
        decl.step_delay_ms = steps.first().and_then(|s| s.delay_ms);
        decl.prompts = steps.into_iter().map(|s| s.text).collect();
        // The single-prompt form stays on `prompt`, so an exported one-step
        // automation is byte-identical to a hand-written manifest entry.
        if decl.prompts.len() == 1 {
            decl.prompt = decl.prompts.pop();
        }
    };
    match &a.action {
        AutomationAction::Send { target } => {
            set_prompts(&mut decl);
            match target {
                crate::session::SendTarget::Id(id) => decl.session_id = Some(id.to_string()),
                crate::session::SendTarget::Name(name) => decl.session_ref = Some(name.clone()),
            }
        }
        AutomationAction::Spawn {
            repo_path,
            worktree_branch,
            base_branch,
            agent,
            extra_repos,
            host,
            session_mode,
        } => {
            set_prompts(&mut decl);
            decl.repo = Some(repo_path.display().to_string());
            decl.worktree = worktree_branch.clone();
            decl.base = base_branch.clone();
            decl.agent = agent.clone();
            decl.host = host.clone();
            decl.session_mode =
                (*session_mode != SpawnSessionMode::Reuse).then(|| session_mode.as_str().into());
            decl.extra_repos = extra_repos.clone();
        }
        AutomationAction::Exec {
            command,
            timeout_secs,
        } => {
            decl.command = Some(command.clone());
            decl.timeout_secs = *timeout_secs;
        }
    }
    decl
}

fn automation_to_json(a: &Automation) -> Value {
    let action = action::action_to_json(Some(&a.action));
    json!({
        "id": a.id,
        "name": a.name,
        "enabled": a.enabled,
        "schedule": { "kind": a.schedule.kind(), "spec": a.schedule.spec() },
        "timezone": a.timezone,
        "action": action,
        "prompt": a.prompt,
        // The resolved delivery list — always present, so a script never has to
        // know whether this row predates multi-step prompts.
        "prompt_steps": a.steps().iter().map(|s| json!({
            "text": s.text,
            "delay_ms": s.delay_ms,
        })).collect::<Vec<_>>(),
        "created_at": a.created_at,
        "updated_at": a.updated_at,
        "last_run_at": a.last_run_at,
        "next_run_at": a.next_run_at,
    })
}

/// Best-effort: ensure the tmux heartbeat keeper is running so the automation
/// fires even when no TUI is attached. Failures (e.g. tmux missing) are
/// non-fatal — the automation still works while the TUI is up.
///
/// Gated on `[features] automations`: when disabled the TUI neither fires
/// schedules nor arms the heartbeat, so the CLI must not arm it either (it
/// would spawn a keeper window that can never fire anything).
pub(crate) fn arm_heartbeat() {
    // Never touch a real multiplexer from the crate's own unit tests: arming
    // creates a *persistent* keeper window on the developer's live server.
    if cfg!(test) {
        return;
    }
    if !crate::session::settings::global().features.automations {
        return;
    }
    let cli = crate::agent::tmux::resolve_cli_binary();
    if let Err(e) = crate::agent::tmux::ensure_automation_heartbeat(&cli) {
        eprintln!("warning: failed to arm automation heartbeat: {e}");
    }
}

fn run_to_json(r: &AutomationRun) -> Value {
    json!({
        "id": r.id,
        "automation_id": r.automation_id,
        "started_at": r.started_at,
        "status": r.status.as_str(),
        "detail": r.detail,
        "related_session_id": r.related_session_id.map(|id| id.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_reports_fired_and_skipped_arrays() {
        let db = Database::open_in_memory().unwrap();
        let v = tick(&db).unwrap();
        assert_eq!(v["fired"], json!([]));
        assert_eq!(v["skipped"], json!([]));
    }

    #[test]
    fn render_tick_counts_fired_skipped_and_healed() {
        let v = json!({
            "fired": [{ "id": 1 }, { "id": 2 }],
            "skipped": [{ "id": 3 }],
            "healed": [],
        });
        assert_eq!(
            render_tick(&v),
            "Tick: 2 fired, 1 skipped, 0 extension(s) healed."
        );
    }

    #[test]
    fn render_automation_list_empty_is_friendly() {
        assert_eq!(render_automation_list(&[]), "No automations.");
    }

    #[test]
    fn action_label_distinguishes_send_and_spawn() {
        assert_eq!(
            action::action_label(Some(&AutomationAction::send_to(SessionId::default()))),
            "send"
        );
        assert_eq!(
            action::action_label(Some(&AutomationAction::Spawn {
                repo_path: "/x".into(),
                worktree_branch: None,
                base_branch: None,
                agent: None,
                extra_repos: Vec::new(),
                host: None,
                session_mode: Default::default(),
            })),
            "spawn"
        );
    }

    fn sample_automation() -> Automation {
        Automation {
            id: 1,
            name: "noop".into(),
            enabled: true,
            schedule: crate::session::AutomationSchedule::Cron {
                expr: "0 9 * * *".into(),
            },
            timezone: None,
            action: AutomationAction::send_to(SessionId::default()),
            prompt: "hi".into(),
            created_at: 0,
            updated_at: 0,
            last_run_at: None,
            next_run_at: None,
            prompt_steps: Vec::new(),
        }
    }

    #[test]
    fn edit_time_or_weekday_without_trigger_errors() {
        let mut auto = sample_automation();
        let err = apply_edit_overrides(
            &mut auto,
            None,
            None,
            Some("09:30".into()),
            None,
            None,
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("--trigger"), "got {err}");

        let mut auto = sample_automation();
        let err =
            apply_edit_overrides(&mut auto, None, None, None, Some(3), None, Vec::new(), None)
                .unwrap_err();
        assert!(err.contains("--trigger"), "got {err}");
    }

    #[test]
    fn edit_time_with_trigger_applies() {
        let mut auto = sample_automation();
        apply_edit_overrides(
            &mut auto,
            None,
            Some("daily".into()),
            Some("06:15".into()),
            None,
            None,
            Vec::new(),
            None,
        )
        .unwrap();
        assert_eq!(auto.schedule.spec(), "15 6 * * *");
    }

    #[test]
    fn edit_rejects_blank_prompt() {
        let mut auto = sample_automation();
        let err = apply_edit_overrides(
            &mut auto,
            None,
            None,
            None,
            None,
            None,
            vec!["   ".into()],
            None,
        )
        .unwrap_err();
        assert!(err.contains("prompt"), "got {err}");
    }

    #[test]
    fn edit_rejects_unknown_timezone() {
        let mut auto = sample_automation();
        let err = apply_edit_overrides(
            &mut auto,
            None,
            None,
            None,
            None,
            Some("Mars/Olympus".into()),
            Vec::new(),
            None,
        )
        .unwrap_err();
        assert!(err.contains("timezone"), "got {err}");
    }

    #[test]
    fn edit_replaces_the_whole_prompt_list() {
        let mut auto = sample_automation();
        apply_edit_overrides(
            &mut auto,
            None,
            None,
            None,
            None,
            None,
            vec!["/model opus".into(), "go".into()],
            Some(500),
        )
        .unwrap();
        // The legacy `prompt` column keeps step 1 for older readers.
        assert_eq!(auto.prompt, "/model opus");
        assert_eq!(auto.prompt_steps.len(), 2);
        assert_eq!(auto.prompt_steps[0].delay_ms, Some(500));
        // The last step's delay is never waited on, so it isn't stored.
        assert_eq!(auto.prompt_steps[1].delay_ms, None);
    }

    #[test]
    fn edit_switches_action_kind_outright() {
        let db = Database::open_in_memory().unwrap();
        let auto = sample_automation(); // a send automation
        let args = ActionArgs {
            command: Some("sync.sh".into()),
            ..ActionArgs::default()
        };
        let action = apply_action_overrides(&auto.action, &args, &db).unwrap();
        assert!(matches!(action, AutomationAction::Exec { .. }));
    }

    #[test]
    fn edit_amends_a_spawn_without_clobbering_it() {
        let db = Database::open_in_memory().unwrap();
        let current = AutomationAction::Spawn {
            repo_path: "/repo".into(),
            worktree_branch: Some("feat".into()),
            base_branch: None,
            agent: Some("claude".into()),
            extra_repos: Vec::new(),
            host: None,
            session_mode: crate::session::SpawnSessionMode::Reuse,
        };
        let args = ActionArgs {
            agent: Some("codex".into()),
            session_mode: Some("fresh".into()),
            ..ActionArgs::default()
        };
        match apply_action_overrides(&current, &args, &db).unwrap() {
            AutomationAction::Spawn {
                repo_path,
                worktree_branch,
                agent,
                session_mode,
                ..
            } => {
                assert_eq!(repo_path, std::path::PathBuf::from("/repo"));
                assert_eq!(worktree_branch.as_deref(), Some("feat"));
                assert_eq!(agent.as_deref(), Some("codex"));
                assert_eq!(session_mode, crate::session::SpawnSessionMode::Fresh);
            }
            other => panic!("expected spawn, got {other:?}"),
        }
    }

    #[test]
    fn create_rejects_blank_prompt() {
        let db = Database::open_in_memory().unwrap();
        let err = create_automation(
            &db,
            CreateArgs {
                name: "n".into(),
                trigger: "daily".into(),
                time: None,
                weekday: None,
                timezone: None,
                prompts: vec!["   ".into()],
                step_delay: None,
                action: ActionArgs {
                    repo: Some("/repo".into()),
                    ..ActionArgs::default()
                },
                disabled: false,
            },
        )
        .unwrap_err();
        assert!(err.contains("prompt"), "got {err}");
    }

    #[test]
    fn resolve_action_spawn_carries_extra_repos() {
        let db = Database::open_in_memory().unwrap();
        let args = ActionArgs {
            repo: Some("/a".into()),
            add_repo: vec!["/b@main".into()],
            add_dir: vec!["/c".into()],
            ..ActionArgs::default()
        };
        match resolve_action(&args, &db).unwrap() {
            AutomationAction::Spawn { extra_repos, .. } => {
                assert_eq!(extra_repos.len(), 2);
                assert!(extra_repos[0].worktree);
                assert_eq!(extra_repos[0].base_branch.as_deref(), Some("main"));
                assert!(!extra_repos[1].worktree);
            }
            other => panic!("expected spawn, got {other:?}"),
        }
    }

    #[test]
    fn resolve_action_command_builds_exec() {
        let db = Database::open_in_memory().unwrap();
        let args = ActionArgs {
            command: Some("~/sync.sh".into()),
            ..ActionArgs::default()
        };
        let action = resolve_action(&args, &db).unwrap();
        assert!(matches!(action, AutomationAction::Exec { command, .. } if command == "~/sync.sh"));
    }

    #[test]
    fn resolve_action_rejects_command_with_session() {
        let db = Database::open_in_memory().unwrap();
        let args = ActionArgs {
            session: Some("s".into()),
            command: Some("cmd".into()),
            ..ActionArgs::default()
        };
        let err = resolve_action(&args, &db).unwrap_err();
        assert!(err.contains("only one"), "got {err}");
    }

    #[test]
    fn resolve_action_builds_a_name_send_target() {
        let db = Database::open_in_memory().unwrap();
        let args = ActionArgs {
            session_name: Some("inbox".into()),
            ..ActionArgs::default()
        };
        match resolve_action(&args, &db).unwrap() {
            AutomationAction::Send { target } => {
                assert_eq!(target.name(), Some("inbox"));
                assert_eq!(target.id(), None);
            }
            other => panic!("expected send, got {other:?}"),
        }
    }

    #[test]
    fn export_import_round_trips_a_multi_step_spawn() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            CreateArgs {
                name: "inbox".into(),
                trigger: "daily".into(),
                time: Some("07:00".into()),
                weekday: None,
                timezone: Some("Europe/Zurich".into()),
                prompts: vec!["/model opus".into(), "summarize my inbox".into()],
                step_delay: Some(2_000),
                action: ActionArgs {
                    repo: Some("/repo".into()),
                    worktree: Some("auto/inbox".into()),
                    session_mode: Some("fresh".into()),
                    ..ActionArgs::default()
                },
                disabled: true,
            },
        )
        .unwrap();
        let toml = export_automations(&db, None).unwrap().human;
        assert!(toml.contains("[[automations]]"), "got {toml}");

        // Re-import into a clean database and compare the model, not the text.
        let fresh = Database::open_in_memory().unwrap();
        let manifest: crate::session::extension_def::AutomationManifest =
            toml::from_str(&toml).unwrap();
        for decl in &manifest.automations {
            fresh
                .create_automation(&manifest_to_new_automation(decl).unwrap())
                .unwrap();
        }
        let got = &fresh.list_automations().unwrap()[0];
        assert_eq!(got.name, "inbox");
        assert_eq!(got.timezone.as_deref(), Some("Europe/Zurich"));
        assert_eq!(got.schedule.spec(), "0 7 * * *");
        let steps = got.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].text, "/model opus");
        assert_eq!(steps[0].delay_ms, Some(2_000));
        match &got.action {
            AutomationAction::Spawn {
                repo_path,
                worktree_branch,
                session_mode,
                ..
            } => {
                assert_eq!(repo_path, &std::path::PathBuf::from("/repo"));
                assert_eq!(worktree_branch.as_deref(), Some("auto/inbox"));
                assert_eq!(*session_mode, crate::session::SpawnSessionMode::Fresh);
            }
            other => panic!("expected spawn, got {other:?}"),
        }
    }

    #[test]
    fn an_exported_exec_re_imports() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            CreateArgs {
                action: ActionArgs {
                    command: Some("sync.sh".into()),
                    timeout: Some(60),
                    ..ActionArgs::default()
                },
                prompts: Vec::new(),
                ..spawn_create("sync", ActionArgs::default())
            },
        )
        .unwrap();
        let toml = export_automations(&db, None).unwrap().human;
        let manifest: crate::session::extension_def::AutomationManifest =
            toml::from_str(&toml).unwrap();
        // An exec carrying a prompt is rejected by the manifest grammar, so the
        // export must not write one.
        manifest.automations[0].validate().unwrap();

        let fresh = Database::open_in_memory().unwrap();
        fresh
            .create_automation(&manifest_to_new_automation(&manifest.automations[0]).unwrap())
            .unwrap();
        let got = &fresh.list_automations().unwrap()[0];
        assert!(got.prompt.is_empty(), "got {:?}", got.prompt);
        match &got.action {
            AutomationAction::Exec {
                command,
                timeout_secs,
            } => {
                assert_eq!(command, "sync.sh");
                assert_eq!(*timeout_secs, Some(60));
            }
            other => panic!("expected exec, got {other:?}"),
        }
    }

    #[test]
    fn import_skips_an_existing_name_unless_replacing() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("autos.toml");
        std::fs::write(
            &file,
            "[[automations]]\nname = \"sync\"\ntrigger = \"hourly\"\ncommand = \"sync.sh\"\n",
        )
        .unwrap();
        let path = file.display().to_string();

        let out = import_automations(&db, &path, false).unwrap();
        assert_eq!(out.json["created"], json!(["sync"]));
        assert_eq!(db.list_automations().unwrap().len(), 1);

        // A second import is a no-op by default...
        let out = import_automations(&db, &path, false).unwrap();
        assert_eq!(out.json["skipped"], json!(["sync"]));
        assert_eq!(db.list_automations().unwrap().len(), 1);

        // ...and replaces on request, still leaving exactly one row.
        let out = import_automations(&db, &path, true).unwrap();
        assert_eq!(out.json["replaced"], json!(["sync"]));
        assert_eq!(db.list_automations().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_replace_leaves_the_existing_automation_and_its_runs() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let good = dir.path().join("good.toml");
        std::fs::write(
            &good,
            "[[automations]]\nname = \"sync\"\ntrigger = \"hourly\"\ncommand = \"sync.sh\"\n",
        )
        .unwrap();
        import_automations(&db, &good.display().to_string(), false).unwrap();
        let before = db.list_automations().unwrap()[0].clone();
        db.record_automation_run(before.id, AutomationRunStatus::Success, "ok", None)
            .unwrap();

        // The trigger only fails once the entry is converted — after the point
        // the old code had already deleted the row and its history.
        let bad = dir.path().join("bad.toml");
        std::fs::write(
            &bad,
            "[[automations]]\nname = \"sync\"\ntrigger = \"neverly\"\ncommand = \"sync.sh\"\n",
        )
        .unwrap();
        let err = import_automations(&db, &bad.display().to_string(), true).unwrap_err();
        assert!(err.contains("trigger"), "got {err}");

        let after = db.list_automations().unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0], before);
        assert_eq!(db.list_automation_runs(before.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn import_rejects_a_manifest_with_duplicate_names() {
        let db = Database::open_in_memory().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("autos.toml");
        std::fs::write(
            &file,
            "[[automations]]\nname = \"sync\"\ntrigger = \"hourly\"\ncommand = \"a.sh\"\n\n\
             [[automations]]\nname = \"sync\"\ntrigger = \"daily\"\ncommand = \"b.sh\"\n",
        )
        .unwrap();
        let err = import_automations(&db, &file.display().to_string(), false).unwrap_err();
        assert!(err.contains("twice"), "got {err}");
        assert!(db.list_automations().unwrap().is_empty(), "nothing created");
    }

    #[test]
    fn an_exec_fire_keeps_exactly_one_history_row() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            CreateArgs {
                name: "sync".into(),
                trigger: "at:1".into(),
                time: None,
                weekday: None,
                timezone: None,
                prompts: Vec::new(),
                step_delay: None,
                action: ActionArgs {
                    command: Some("true".into()),
                    ..ActionArgs::default()
                },
                disabled: true,
            },
        )
        .unwrap();
        let id = db.list_automations().unwrap()[0].id;
        db.trigger_automation_now(id).unwrap();

        tick(&db).unwrap();

        // The `Running` row the fire opened is the row it closes out — `tick`
        // must not append a second one for the same fire.
        let runs = db.list_automation_runs(id, 10).unwrap();
        assert_eq!(runs.len(), 1, "got {runs:?}");
        assert_eq!(runs[0].status, AutomationRunStatus::Success);
        assert!(runs[0].finished_at.is_some());
    }

    #[test]
    fn tick_closes_out_a_stale_running_row() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            CreateArgs {
                action: ActionArgs {
                    command: Some("true".into()),
                    ..ActionArgs::default()
                },
                prompts: Vec::new(),
                ..spawn_create("sync", ActionArgs::default())
            },
        )
        .unwrap();
        let id = db.list_automations().unwrap()[0].id;
        let run = db
            .record_automation_run(id, AutomationRunStatus::Running, "true", None)
            .unwrap();
        db.conn_ref()
            .execute(
                "UPDATE automation_runs SET started_at = 0 WHERE id = ?1",
                [run],
            )
            .unwrap();

        // A keeper-only install never opens the TUI, so `tick` has to do the
        // reaping the TUI's startup pass would.
        tick(&db).unwrap();
        let listed = &db.list_automation_runs(id, 10).unwrap()[0];
        assert_eq!(listed.status, AutomationRunStatus::Error);
        assert!(listed.detail.contains("interrupted"), "got {listed:?}");
    }

    #[test]
    fn a_headless_fresh_spawn_stops_at_the_live_session_cap() {
        use crate::session::automation::MAX_LIVE_FRESH_SESSIONS;
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            spawn_create(
                "nightly",
                ActionArgs {
                    repo: Some("/repo".into()),
                    session_mode: Some("fresh".into()),
                    ..ActionArgs::default()
                },
            ),
        )
        .unwrap();
        let auto = db.list_automations().unwrap().remove(0);
        // The cap's worth of sessions this automation left open.
        for i in 0..MAX_LIVE_FRESH_SESSIONS {
            let shared = crate::sync::SharedSession {
                id: SessionId::default(),
                name: format!(
                    "{}2024010{i}",
                    crate::session::automation::fresh_session_prefix(auto.id)
                ),
                agent: "claude".into(),
                backend_id: String::new(),
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
            };
            db.upsert_session(&shared).unwrap();
        }

        // Whichever firer wins the claim must skip on the same terms, or a
        // keeper-only install accumulates sessions and worktrees unboundedly.
        let (status, detail, _) = fire_spawn(&db, &auto, current_time_millis());
        assert_eq!(status, AutomationRunStatus::Skipped);
        assert!(detail.contains("still open"), "got {detail}");
    }

    #[test]
    fn dry_run_reports_the_plan_without_firing() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            CreateArgs {
                name: "sync".into(),
                trigger: "hourly".into(),
                time: None,
                weekday: None,
                timezone: None,
                prompts: Vec::new(),
                step_delay: None,
                action: ActionArgs {
                    command: Some("sync.sh".into()),
                    ..ActionArgs::default()
                },
                disabled: true,
            },
        )
        .unwrap();
        let id = db.list_automations().unwrap()[0].id;
        let out = dry_run(&db, id).unwrap();
        let value_of = |label: &str| {
            out.json
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["label"] == json!(label))
                .map(|row| row["value"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(value_of("action"), vec![json!("exec")]);
        assert_eq!(value_of("command"), vec![json!("sync.sh")]);
        // A dry run must not touch the history.
        assert!(db.list_automation_runs(id, 10).unwrap().is_empty());
    }

    #[test]
    fn dry_run_json_lists_every_extra_repo() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            spawn_create(
                "multi",
                ActionArgs {
                    repo: Some("/repo".into()),
                    add_repo: vec!["/extra-one@main".into()],
                    add_dir: vec!["/extra-two".into()],
                    ..ActionArgs::default()
                },
            ),
        )
        .unwrap();
        let id = db.list_automations().unwrap()[0].id;
        let out = dry_run(&db, id).unwrap();
        let extras: Vec<String> = out
            .json
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["label"] == json!("extra repo"))
            .map(|row| row["value"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(extras.len(), 2, "got {extras:?}");
        assert!(extras[0].contains("/extra-one"), "got {extras:?}");
        assert!(extras[1].contains("/extra-two"), "got {extras:?}");
    }

    #[test]
    fn create_rejects_an_unknown_timezone() {
        let db = Database::open_in_memory().unwrap();
        let err = create_automation(
            &db,
            CreateArgs {
                name: "n".into(),
                trigger: "daily".into(),
                time: None,
                weekday: None,
                timezone: Some("Europe/Zurihc".into()),
                prompts: vec!["go".into()],
                step_delay: None,
                action: ActionArgs {
                    repo: Some("/repo".into()),
                    ..ActionArgs::default()
                },
                disabled: false,
            },
        )
        .unwrap_err();
        assert!(err.contains("timezone"), "got {err}");
    }

    /// A spawn `create` with the given action flags, for the negative tests.
    fn spawn_create(name: &str, action: ActionArgs) -> CreateArgs {
        CreateArgs {
            name: name.into(),
            trigger: "daily".into(),
            time: None,
            weekday: None,
            timezone: None,
            prompts: vec!["go".into()],
            step_delay: None,
            action,
            disabled: true,
        }
    }

    #[test]
    fn create_rejects_an_unknown_agent() {
        let db = Database::open_in_memory().unwrap();
        let err = create_automation(
            &db,
            spawn_create(
                "n",
                ActionArgs {
                    repo: Some("/repo".into()),
                    agent: Some("not-a-real-agent".into()),
                    ..ActionArgs::default()
                },
            ),
        )
        .unwrap_err();
        assert!(err.contains("Unknown agent"), "got {err}");
        assert!(db.list_automations().unwrap().is_empty(), "nothing saved");
    }

    #[test]
    fn create_rejects_an_unknown_session_mode() {
        let db = Database::open_in_memory().unwrap();
        let err = create_automation(
            &db,
            spawn_create(
                "n",
                ActionArgs {
                    repo: Some("/repo".into()),
                    session_mode: Some("frehs".into()),
                    ..ActionArgs::default()
                },
            ),
        )
        .unwrap_err();
        assert!(err.contains("session mode"), "got {err}");
        assert!(db.list_automations().unwrap().is_empty(), "nothing saved");
    }

    #[test]
    fn edit_rejects_an_unknown_host_and_leaves_the_row_alone() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            spawn_create(
                "nightly",
                ActionArgs {
                    repo: Some("/repo".into()),
                    ..ActionArgs::default()
                },
            ),
        )
        .unwrap();
        let before = db.list_automations().unwrap()[0].clone();
        let err = edit_automation(
            &db,
            before.id,
            EditArgs {
                name: None,
                trigger: None,
                time: None,
                weekday: None,
                timezone: None,
                prompts: Vec::new(),
                step_delay: None,
                action: ActionArgs {
                    host: Some("no-such-host".into()),
                    ..ActionArgs::default()
                },
                enabled: false,
                disabled: false,
            },
        )
        .unwrap_err();
        assert!(err.contains("Unknown host"), "got {err}");
        assert_eq!(db.get_automation(before.id).unwrap().unwrap(), before);
    }

    /// `edit` with only the given action flags (plus optional prompts).
    fn edit_action(action: ActionArgs, prompts: Vec<String>) -> EditArgs {
        EditArgs {
            name: None,
            trigger: None,
            time: None,
            weekday: None,
            timezone: None,
            prompts,
            step_delay: None,
            action,
            enabled: false,
            disabled: false,
        }
    }

    #[test]
    fn switching_an_exec_to_a_spawn_requires_a_prompt() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            CreateArgs {
                action: ActionArgs {
                    command: Some("sync.sh".into()),
                    ..ActionArgs::default()
                },
                prompts: Vec::new(),
                ..spawn_create("sync", ActionArgs::default())
            },
        )
        .unwrap();
        let id = db.list_automations().unwrap()[0].id;

        // Without a prompt the spawn would submit a blank turn on every fire.
        let err = edit_automation(
            &db,
            id,
            edit_action(
                ActionArgs {
                    repo: Some("/repo".into()),
                    ..ActionArgs::default()
                },
                Vec::new(),
            ),
        )
        .unwrap_err();
        assert!(err.contains("needs a prompt"), "got {err}");
        assert!(matches!(
            db.get_automation(id).unwrap().unwrap().action,
            AutomationAction::Exec { .. }
        ));

        // With one, the switch goes through.
        edit_automation(
            &db,
            id,
            edit_action(
                ActionArgs {
                    repo: Some("/repo".into()),
                    ..ActionArgs::default()
                },
                vec!["go".into()],
            ),
        )
        .unwrap();
        let auto = db.get_automation(id).unwrap().unwrap();
        assert!(matches!(auto.action, AutomationAction::Spawn { .. }));
        assert_eq!(auto.steps()[0].text, "go");
    }

    #[test]
    fn switching_a_spawn_to_an_exec_clears_its_prompt() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            spawn_create(
                "nightly",
                ActionArgs {
                    repo: Some("/repo".into()),
                    ..ActionArgs::default()
                },
            ),
        )
        .unwrap();
        let id = db.list_automations().unwrap()[0].id;
        edit_automation(
            &db,
            id,
            edit_action(
                ActionArgs {
                    command: Some("sync.sh".into()),
                    ..ActionArgs::default()
                },
                Vec::new(),
            ),
        )
        .unwrap();
        // An exec carrying a prompt exports as a declaration its own validation
        // rejects, so the switch has to drop it.
        let auto = db.get_automation(id).unwrap().unwrap();
        assert!(auto.prompt.is_empty(), "got {:?}", auto.prompt);
        assert!(auto.prompt_steps.is_empty());
    }

    #[test]
    fn create_rejects_a_prompt_alongside_a_command() {
        let db = Database::open_in_memory().unwrap();
        let err = create_automation(
            &db,
            CreateArgs {
                action: ActionArgs {
                    command: Some("sync.sh".into()),
                    ..ActionArgs::default()
                },
                ..spawn_create("sync", ActionArgs::default())
            },
        )
        .unwrap_err();
        assert!(err.contains("--prompt"), "got {err}");
    }

    #[test]
    fn create_stores_a_padded_timezone_trimmed() {
        let db = Database::open_in_memory().unwrap();
        create_automation(
            &db,
            CreateArgs {
                timezone: Some(" UTC ".into()),
                ..spawn_create(
                    "n",
                    ActionArgs {
                        repo: Some("/repo".into()),
                        ..ActionArgs::default()
                    },
                )
            },
        )
        .unwrap();
        // Stored untrimmed, `AutomationSchedule::next_after` fails to parse it
        // and silently schedules in local time instead.
        assert_eq!(
            db.list_automations().unwrap()[0].timezone.as_deref(),
            Some("UTC")
        );
    }

    #[test]
    fn run_to_json_emits_related_session_id() {
        let sid = SessionId::default();
        let run = AutomationRun {
            id: 1,
            automation_id: 2,
            started_at: 3,
            status: AutomationRunStatus::Success,
            detail: "sent".into(),
            related_session_id: Some(sid),
            finished_at: None,
        };
        assert_eq!(run_to_json(&run)["related_session_id"], sid.to_string());

        let run = AutomationRun {
            related_session_id: None,
            ..run
        };
        assert_eq!(run_to_json(&run)["related_session_id"], Value::Null);
    }
}
