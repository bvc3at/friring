//! Persistence for [`Automation`]s and their run history.
//!
//! Schedules are stored as `(schedule_kind, schedule_spec)`; actions as
//! `(action_kind, …)` with the action-specific columns. The `next_run_at`
//! column is the dispatcher's scan key — see `app::process_automations`.

use rusqlite::{params, OptionalExtension};

use crate::session::{
    Automation, AutomationAction, AutomationRun, AutomationRunStatus, AutomationSchedule,
    PromptStep, SessionId,
};
use crate::sync::current_time_millis;

use super::Database;

/// Serialize the prompt-step list for the `prompt_steps` column.
///
/// A single step (the common case, and every pre-v44 row) stores `NULL` and
/// lives in the plain `prompt` column alone, so an untouched automation is
/// byte-identical to what an older friring wrote — the `action_extra_repos`
/// precedent. A lone step carrying a custom delay still needs the column.
fn prompt_steps_to_json(steps: &[PromptStep]) -> Option<String> {
    match steps {
        [] => None,
        [only] if only.delay_ms.is_none() => None,
        steps => serde_json::to_string(steps).ok(),
    }
}

/// Decode the `prompt_steps` column. `NULL`/empty/malformed → an empty list,
/// which [`Automation::steps`](crate::session::Automation::steps) resolves to
/// the single `prompt` column. Never an error.
fn prompt_steps_from_json(raw: Option<String>) -> Vec<PromptStep> {
    raw.filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Fields needed to create an automation. `next_run_at` is computed by the
/// caller (it depends on the schedule + timezone, which live in `session`).
pub struct NewAutomation {
    pub name: String,
    pub enabled: bool,
    pub schedule: AutomationSchedule,
    pub timezone: Option<String>,
    pub action: AutomationAction,
    /// The first (often only) prompt step — also what a pre-v44 friring reads.
    pub prompt: String,
    /// The full step list; empty = the single `prompt` above.
    pub prompt_steps: Vec<PromptStep>,
    pub next_run_at: Option<u64>,
}

impl Database {
    /// Insert a new automation, returning its row id.
    pub fn create_automation(&self, new: &NewAutomation) -> rusqlite::Result<i64> {
        let now = current_time_millis() as i64;
        let cols = super::action_to_columns(&new.action);
        self.conn.execute(
            "INSERT INTO automations
                (name, enabled, schedule_kind, schedule_spec, timezone,
                 action_kind, target_session, repo_path, worktree_branch,
                 base_branch, agent, prompt, created_at, updated_at,
                 last_run_at, next_run_at, action_extra_repos, action_command,
                 action_target_name, action_host, action_session_mode,
                 action_timeout_secs, prompt_steps)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13, NULL, ?14, ?15, \
             ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                new.name,
                new.enabled as i64,
                new.schedule.kind(),
                new.schedule.spec(),
                new.timezone,
                new.action.kind(),
                cols.target_session,
                cols.repo_path,
                cols.worktree_branch,
                cols.base_branch,
                cols.agent,
                new.prompt,
                now,
                new.next_run_at.map(|v| v as i64),
                cols.extra_repos,
                cols.command,
                cols.target_name,
                cols.host,
                cols.session_mode,
                cols.timeout_secs,
                prompt_steps_to_json(&new.prompt_steps),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Fetch a single automation by id.
    pub fn get_automation(&self, id: i64) -> rusqlite::Result<Option<Automation>> {
        self.conn
            .query_row(
                &format!("SELECT {COLS} FROM automations WHERE id = ?1"),
                params![id],
                map_automation,
            )
            .optional()
    }

    /// List all automations, newest first.
    pub fn list_automations(&self) -> rusqlite::Result<Vec<Automation>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLS} FROM automations ORDER BY id DESC"))?;
        let rows = stmt.query_map([], map_automation)?;
        rows.collect()
    }

    /// All enabled automations whose `next_run_at` is due (`<= now`).
    pub fn due_automations(&self, now_millis: u64) -> rusqlite::Result<Vec<Automation>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM automations \
             WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?1 \
             ORDER BY next_run_at",
        ))?;
        let rows = stmt.query_map(params![now_millis as i64], map_automation)?;
        rows.collect()
    }

    /// Replace an automation's definition (everything except id/created_at).
    pub fn update_automation(&self, auto: &Automation) -> rusqlite::Result<()> {
        let cols = super::action_to_columns(&auto.action);
        let now = current_time_millis() as i64;
        self.conn.execute(
            "UPDATE automations SET
                name = ?2, enabled = ?3, schedule_kind = ?4, schedule_spec = ?5,
                timezone = ?6, action_kind = ?7, target_session = ?8, repo_path = ?9,
                worktree_branch = ?10, base_branch = ?11, agent = ?12, prompt = ?13,
                updated_at = ?14, last_run_at = ?15, next_run_at = ?16,
                action_extra_repos = ?17, action_command = ?18,
                action_target_name = ?19, action_host = ?20, action_session_mode = ?21,
                action_timeout_secs = ?22, prompt_steps = ?23
             WHERE id = ?1",
            params![
                auto.id,
                auto.name,
                auto.enabled as i64,
                auto.schedule.kind(),
                auto.schedule.spec(),
                auto.timezone,
                auto.action.kind(),
                cols.target_session,
                cols.repo_path,
                cols.worktree_branch,
                cols.base_branch,
                cols.agent,
                auto.prompt,
                now,
                auto.last_run_at.map(|v| v as i64),
                auto.next_run_at.map(|v| v as i64),
                cols.extra_repos,
                cols.command,
                cols.target_name,
                cols.host,
                cols.session_mode,
                cols.timeout_secs,
                prompt_steps_to_json(&auto.prompt_steps),
            ],
        )?;
        Ok(())
    }

    /// Enable or disable an automation. Disabling clears `next_run_at` so the
    /// due-scan skips it; enabling leaves `next_run_at` for the caller to set.
    pub fn set_automation_enabled(&self, id: i64, enabled: bool) -> rusqlite::Result<bool> {
        let now = current_time_millis() as i64;
        let updated = if enabled {
            self.conn.execute(
                "UPDATE automations SET enabled = 1, updated_at = ?2 WHERE id = ?1",
                params![id, now],
            )?
        } else {
            self.conn.execute(
                "UPDATE automations SET enabled = 0, next_run_at = NULL, updated_at = ?2 \
                 WHERE id = ?1",
                params![id, now],
            )?
        };
        Ok(updated > 0)
    }

    /// Record that an automation fired: advance `last_run_at` and store the
    /// freshly computed `next_run_at` (`None` disables a spent one-shot).
    pub fn set_automation_next_run(
        &self,
        id: i64,
        last_run_at: u64,
        next_run_at: Option<u64>,
    ) -> rusqlite::Result<()> {
        // A one-shot with no further occurrence is also disabled.
        let enabled = next_run_at.is_some();
        self.conn.execute(
            "UPDATE automations SET last_run_at = ?2, next_run_at = ?3, enabled = \
             CASE WHEN ?4 THEN enabled ELSE 0 END, updated_at = ?5 WHERE id = ?1",
            params![
                id,
                last_run_at as i64,
                next_run_at.map(|v| v as i64),
                enabled as i64,
                current_time_millis() as i64,
            ],
        )?;
        Ok(())
    }

    /// Atomically claim a due automation for firing: advance its schedule **iff**
    /// `next_run_at` still equals `expected` (the value the caller observed as
    /// due). Returns `true` for the single winner; concurrent firers (a running
    /// TUI plus a headless `automation tick`) get `false` and must not fire.
    ///
    /// Claim-first ordering (advance, then fire) gives at-most-once semantics: a
    /// crash between claim and side effect loses a run rather than double-firing.
    /// `next` is the recomputed next occurrence (`None` disables a spent
    /// one-shot, mirroring [`set_automation_next_run`](Self::set_automation_next_run)).
    pub fn claim_due_automation(
        &self,
        id: i64,
        expected_next_run_at: u64,
        next: Option<u64>,
        now: u64,
    ) -> rusqlite::Result<bool> {
        let still_enabled = next.is_some();
        let updated = self.conn.execute(
            "UPDATE automations SET next_run_at = ?3, last_run_at = ?4, \
             enabled = CASE WHEN ?5 THEN enabled ELSE 0 END, updated_at = ?4 \
             WHERE id = ?1 AND next_run_at = ?2",
            params![
                id,
                expected_next_run_at as i64,
                next.map(|v| v as i64),
                now as i64,
                still_enabled as i64,
            ],
        )?;
        Ok(updated == 1)
    }

    /// Force an automation to fire on the next dispatcher tick (manual run-now):
    /// set `next_run_at` to now and ensure it is enabled.
    pub fn trigger_automation_now(&self, id: i64) -> rusqlite::Result<bool> {
        let now = current_time_millis() as i64;
        let updated = self.conn.execute(
            "UPDATE automations SET enabled = 1, next_run_at = ?2, updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        Ok(updated > 0)
    }

    /// Delete an automation and its run history.
    pub fn delete_automation(&self, id: i64) -> rusqlite::Result<bool> {
        self.conn.execute(
            "DELETE FROM automation_runs WHERE automation_id = ?1",
            params![id],
        )?;
        let updated = self
            .conn
            .execute("DELETE FROM automations WHERE id = ?1", params![id])?;
        Ok(updated > 0)
    }

    /// Disable every `send` automation that targets a session — used when a
    /// session is force-deleted so its one-shots don't fire against a dead pane.
    pub fn disable_send_automations_for_session(
        &self,
        session_id: SessionId,
    ) -> rusqlite::Result<usize> {
        let now = current_time_millis() as i64;
        let updated = self.conn.execute(
            "UPDATE automations SET enabled = 0, next_run_at = NULL, updated_at = ?2 \
             WHERE action_kind = 'send' AND target_session = ?1",
            params![session_id.to_string(), now],
        )?;
        Ok(updated)
    }

    /// Append a run-history entry, returning its row id. `related_session` is
    /// the session the run sent to / spawned, when one exists.
    ///
    /// A final status also stamps `finished_at`; a
    /// [`Running`](AutomationRunStatus::Running) row leaves it `NULL` until
    /// [`finish_automation_run`](Self::finish_automation_run) closes it out.
    pub fn record_automation_run(
        &self,
        automation_id: i64,
        status: AutomationRunStatus,
        detail: &str,
        related_session: Option<SessionId>,
    ) -> rusqlite::Result<i64> {
        let now = current_time_millis() as i64;
        self.conn.execute(
            "INSERT INTO automation_runs \
             (automation_id, started_at, status, detail, related_session_id, finished_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                automation_id,
                now,
                status.as_str(),
                detail,
                related_session.map(|id| id.to_string()),
                status.is_final().then_some(now),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close out a [`Running`](AutomationRunStatus::Running) run with its final
    /// status + detail, stamping `finished_at`.
    ///
    /// This is the post-hoc half of the async `Exec` contract: the tick thread
    /// claims, records `Running`, and hands the command to a worker, which calls
    /// this when the command exits (or is killed on timeout). Returns whether a
    /// row was updated.
    pub fn finish_automation_run(
        &self,
        run_id: i64,
        status: AutomationRunStatus,
        detail: &str,
    ) -> rusqlite::Result<bool> {
        let updated = self.conn.execute(
            "UPDATE automation_runs SET status = ?2, detail = ?3, finished_at = ?4 WHERE id = ?1",
            params![
                run_id,
                status.as_str(),
                detail,
                current_time_millis() as i64,
            ],
        )?;
        Ok(updated > 0)
    }

    /// Mark abandoned `running` runs as errored — a run whose worker died with
    /// its process (a TUI crash, or a headless `tick` killed mid-command) would
    /// otherwise show as running forever.
    ///
    /// Only rows older than the longest a command may legally run are touched:
    /// another friring instance (or a concurrent headless `tick`) may own a
    /// perfectly healthy `running` row right now, and this must never yank it
    /// out from under it. Called once on startup; returns the rows reaped.
    pub fn reap_orphaned_automation_runs(&self) -> rusqlite::Result<usize> {
        let now = current_time_millis();
        let cutoff = now.saturating_sub(
            crate::session::automation::DEFAULT_EXEC_TIMEOUT_SECS.saturating_mul(1_000),
        );
        self.conn.execute(
            "UPDATE automation_runs \
             SET status = 'error', \
                 detail = 'interrupted (friring exited while the command was running)', \
                 finished_at = ?1 \
             WHERE status = 'running' AND started_at < ?2",
            params![now as i64, cutoff as i64],
        )
    }

    /// List the most recent runs for an automation, newest first.
    pub fn list_automation_runs(
        &self,
        automation_id: i64,
        limit: u32,
    ) -> rusqlite::Result<Vec<AutomationRun>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, automation_id, started_at, status, detail, related_session_id, \
             finished_at \
             FROM automation_runs \
             WHERE automation_id = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![automation_id, limit], |row| {
            Ok(AutomationRun {
                id: row.get(0)?,
                automation_id: row.get(1)?,
                started_at: row.get::<_, i64>(2)? as u64,
                status: AutomationRunStatus::from_db(&row.get::<_, String>(3)?),
                detail: row.get(4)?,
                // Tolerate malformed ids — treat them as "no related session".
                related_session_id: row
                    .get::<_, Option<String>>(5)?
                    .and_then(|s| s.parse().ok()),
                finished_at: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
            })
        })?;
        rows.collect()
    }
}

/// Column list for automation SELECTs (keep in sync with [`map_automation`]).
const COLS: &str = "id, name, enabled, schedule_kind, schedule_spec, timezone, \
    action_kind, target_session, repo_path, worktree_branch, base_branch, agent, \
    prompt, created_at, updated_at, last_run_at, next_run_at, action_extra_repos, \
    action_command, action_target_name, action_host, action_session_mode, \
    action_timeout_secs, prompt_steps";

fn map_automation(row: &rusqlite::Row) -> rusqlite::Result<Automation> {
    let id: i64 = row.get(0)?;
    let schedule_kind: String = row.get(3)?;
    let schedule_spec: String = row.get(4)?;
    let action_kind: String = row.get(6)?;
    let cols = super::ActionColumns {
        target_session: row.get(7)?,
        repo_path: row.get(8)?,
        worktree_branch: row.get(9)?,
        base_branch: row.get(10)?,
        agent: row.get(11)?,
        extra_repos: row.get(17)?,
        command: row.get(18)?,
        target_name: row.get(19)?,
        host: row.get(20)?,
        session_mode: row.get(21)?,
        timeout_secs: row.get(22)?,
    };

    let schedule =
        AutomationSchedule::from_parts(&schedule_kind, &schedule_spec).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                id as usize,
                rusqlite::types::Type::Text,
                format!("invalid schedule {schedule_kind}/{schedule_spec}").into(),
            )
        })?;

    let action = super::action_from_columns(&action_kind, cols);

    Ok(Automation {
        id,
        name: row.get(1)?,
        enabled: row.get::<_, i64>(2)? != 0,
        schedule,
        timezone: row.get(5)?,
        action,
        prompt: row.get(12)?,
        prompt_steps: prompt_steps_from_json(row.get(23)?),
        created_at: row.get::<_, i64>(13)? as u64,
        updated_at: row.get::<_, i64>(14)? as u64,
        last_run_at: row.get::<_, Option<i64>>(15)?.map(|v| v as u64),
        next_run_at: row.get::<_, Option<i64>>(16)?.map(|v| v as u64),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn send_automation(name: &str, next: Option<u64>) -> NewAutomation {
        NewAutomation {
            name: name.to_string(),
            enabled: true,
            schedule: AutomationSchedule::Once {
                at: next.unwrap_or(0),
            },
            timezone: None,
            action: AutomationAction::send_to(SessionId::default()),
            prompt: "run tests".to_string(),
            next_run_at: next,
            prompt_steps: Vec::new(),
        }
    }

    #[test]
    fn create_get_and_list_round_trip() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("nightly", Some(123)))
            .unwrap();
        assert!(id > 0);

        let fetched = db.get_automation(id).unwrap().unwrap();
        assert_eq!(fetched.name, "nightly");
        assert_eq!(fetched.prompt, "run tests");
        assert_eq!(fetched.next_run_at, Some(123));
        assert!(matches!(fetched.action, AutomationAction::Send { .. }));

        assert_eq!(db.list_automations().unwrap().len(), 1);
    }

    #[test]
    fn spawn_action_columns_round_trip() {
        let db = Database::open_in_memory().unwrap();
        let new = NewAutomation {
            name: "triage".into(),
            enabled: true,
            schedule: AutomationSchedule::Cron {
                expr: "0 9 * * 1-5".into(),
            },
            timezone: Some("Europe/Zurich".into()),
            action: AutomationAction::Spawn {
                repo_path: PathBuf::from("/tmp/repo"),
                worktree_branch: Some("feat/auto".into()),
                base_branch: Some("main".into()),
                agent: Some("codex".into()),
                extra_repos: Vec::new(),
                host: None,
                session_mode: Default::default(),
            },
            prompt: "triage issues".into(),
            next_run_at: Some(999),
            prompt_steps: Vec::new(),
        };
        let id = db.create_automation(&new).unwrap();
        let got = db.get_automation(id).unwrap().unwrap();
        assert_eq!(got.timezone.as_deref(), Some("Europe/Zurich"));
        assert!(matches!(got.schedule, AutomationSchedule::Cron { .. }));
        match got.action {
            AutomationAction::Spawn {
                repo_path,
                worktree_branch,
                base_branch,
                agent,
                extra_repos,
                ..
            } => {
                assert_eq!(repo_path, PathBuf::from("/tmp/repo"));
                assert_eq!(worktree_branch.as_deref(), Some("feat/auto"));
                assert_eq!(base_branch.as_deref(), Some("main"));
                assert_eq!(agent.as_deref(), Some("codex"));
                assert!(extra_repos.is_empty());
            }
            _ => panic!("expected spawn"),
        }
    }

    #[test]
    fn exec_action_columns_round_trip() {
        let db = Database::open_in_memory().unwrap();
        let new = NewAutomation {
            name: "sync".into(),
            enabled: true,
            schedule: AutomationSchedule::Cron {
                expr: "*/15 * * * *".into(),
            },
            timezone: None,
            action: AutomationAction::Exec {
                command: "~/github-issues/sync.sh".into(),
                timeout_secs: None,
            },
            prompt: String::new(),
            next_run_at: Some(42),
            prompt_steps: Vec::new(),
        };
        let id = db.create_automation(&new).unwrap();
        let got = db.get_automation(id).unwrap().unwrap();
        match got.action {
            AutomationAction::Exec { command, .. } => {
                assert_eq!(command, "~/github-issues/sync.sh");
            }
            other => panic!("expected exec, got {other:?}"),
        }
    }

    #[test]
    fn due_honors_enabled_and_next_run() {
        let db = Database::open_in_memory().unwrap();
        let due_id = db
            .create_automation(&send_automation("due", Some(100)))
            .unwrap();
        let _future = db
            .create_automation(&send_automation("future", Some(10_000)))
            .unwrap();
        let disabled = NewAutomation {
            enabled: false,
            ..send_automation("disabled", Some(100))
        };
        db.create_automation(&disabled).unwrap();

        let due = db.due_automations(500).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, due_id);
    }

    #[test]
    fn set_next_run_disables_spent_one_shot() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("once", Some(100)))
            .unwrap();
        db.set_automation_next_run(id, 200, None).unwrap();
        let got = db.get_automation(id).unwrap().unwrap();
        assert!(!got.enabled);
        assert_eq!(got.next_run_at, None);
        assert_eq!(got.last_run_at, Some(200));
        assert!(db.due_automations(10_000).unwrap().is_empty());
    }

    #[test]
    fn set_next_run_keeps_recurring_enabled() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("recurring", Some(100)))
            .unwrap();
        db.set_automation_next_run(id, 200, Some(5000)).unwrap();
        let got = db.get_automation(id).unwrap().unwrap();
        assert!(got.enabled);
        assert_eq!(got.next_run_at, Some(5000));
    }

    #[test]
    fn trigger_now_makes_due() {
        // A far-future bound that stays positive when cast to i64.
        let far_future = i64::MAX as u64;
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("manual", None))
            .unwrap();
        assert!(db.due_automations(far_future).unwrap().is_empty());
        assert!(db.trigger_automation_now(id).unwrap());
        assert_eq!(db.due_automations(far_future).unwrap().len(), 1);
    }

    #[test]
    fn claim_is_won_only_once() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("recurring", Some(100)))
            .unwrap();
        // First claim with the observed next_run_at wins and advances it.
        assert!(db.claim_due_automation(id, 100, Some(5000), 200).unwrap());
        // A second claim with the now-stale expected value loses.
        assert!(!db.claim_due_automation(id, 100, Some(9000), 200).unwrap());
        let got = db.get_automation(id).unwrap().unwrap();
        assert_eq!(got.next_run_at, Some(5000));
        assert_eq!(got.last_run_at, Some(200));
        assert!(got.enabled);
    }

    #[test]
    fn claim_with_none_disables_one_shot() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("once", Some(100)))
            .unwrap();
        assert!(db.claim_due_automation(id, 100, None, 200).unwrap());
        let got = db.get_automation(id).unwrap().unwrap();
        assert!(!got.enabled);
        assert_eq!(got.next_run_at, None);
        assert!(db.due_automations(i64::MAX as u64).unwrap().is_empty());
    }

    #[test]
    fn claim_with_wrong_expected_does_not_fire() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("x", Some(100)))
            .unwrap();
        assert!(!db.claim_due_automation(id, 999, Some(5000), 200).unwrap());
        let got = db.get_automation(id).unwrap().unwrap();
        assert_eq!(got.next_run_at, Some(100));
        assert_eq!(got.last_run_at, None);
    }

    #[test]
    fn disable_send_for_session() {
        let db = Database::open_in_memory().unwrap();
        let sid = SessionId::default();
        let new = NewAutomation {
            action: AutomationAction::send_to(sid),
            ..send_automation("s", Some(100))
        };
        let id = db.create_automation(&new).unwrap();
        assert_eq!(db.disable_send_automations_for_session(sid).unwrap(), 1);
        assert!(!db.get_automation(id).unwrap().unwrap().enabled);
    }

    #[test]
    fn run_history_insert_and_list() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("h", Some(100)))
            .unwrap();
        db.record_automation_run(id, AutomationRunStatus::Success, "ok", None)
            .unwrap();
        db.record_automation_run(id, AutomationRunStatus::Skipped, "no session", None)
            .unwrap();
        let runs = db.list_automation_runs(id, 10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].status, AutomationRunStatus::Skipped);
    }

    #[test]
    fn run_history_roundtrips_related_session() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("h", Some(100)))
            .unwrap();
        let sid = SessionId::default();
        db.record_automation_run(id, AutomationRunStatus::Success, "sent", Some(sid))
            .unwrap();
        db.record_automation_run(id, AutomationRunStatus::Skipped, "no session", None)
            .unwrap();
        let runs = db.list_automation_runs(id, 10).unwrap();
        assert_eq!(runs[0].related_session_id, None);
        assert_eq!(runs[1].related_session_id, Some(sid));
    }

    #[test]
    fn delete_removes_automation_and_runs() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("d", Some(100)))
            .unwrap();
        db.record_automation_run(id, AutomationRunStatus::Success, "ok", None)
            .unwrap();
        assert!(db.delete_automation(id).unwrap());
        assert!(db.get_automation(id).unwrap().is_none());
        assert!(db.list_automation_runs(id, 10).unwrap().is_empty());
    }

    #[test]
    fn single_step_prompt_stores_no_json() {
        // The pre-v44 shape must stay byte-identical: one plain step lives in
        // the `prompt` column and leaves `prompt_steps` NULL.
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("one", Some(1)))
            .unwrap();
        let raw: Option<String> = db
            .conn
            .query_row(
                "SELECT prompt_steps FROM automations WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, None);
        // ...and it still reads back as exactly one step.
        let got = db.get_automation(id).unwrap().unwrap();
        assert!(got.prompt_steps.is_empty());
        assert_eq!(got.steps().len(), 1);
    }

    #[test]
    fn multi_step_prompt_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let new = NewAutomation {
            prompt: "/model opus".into(),
            prompt_steps: vec![
                PromptStep {
                    text: "/model opus".into(),
                    delay_ms: Some(2_000),
                },
                PromptStep::new("summarize my inbox"),
            ],
            ..send_automation("multi", Some(1))
        };
        let id = db.create_automation(&new).unwrap();
        let got = db.get_automation(id).unwrap().unwrap();
        assert_eq!(got.prompt_steps.len(), 2);
        assert_eq!(got.prompt_steps[0].delay_ms, Some(2_000));
        assert_eq!(got.prompt_steps[1].text, "summarize my inbox");
        // The legacy column still holds step 1 for an older reader.
        assert_eq!(got.prompt, "/model opus");
    }

    #[test]
    fn spawn_host_and_session_mode_round_trip() {
        let db = Database::open_in_memory().unwrap();
        let new = NewAutomation {
            action: AutomationAction::Spawn {
                repo_path: std::path::PathBuf::from("/repo"),
                worktree_branch: None,
                base_branch: None,
                agent: None,
                extra_repos: Vec::new(),
                host: Some("devbox".into()),
                session_mode: crate::session::SpawnSessionMode::Fresh,
            },
            ..send_automation("remote", Some(1))
        };
        let id = db.create_automation(&new).unwrap();
        match db.get_automation(id).unwrap().unwrap().action {
            AutomationAction::Spawn {
                host, session_mode, ..
            } => {
                assert_eq!(host.as_deref(), Some("devbox"));
                assert_eq!(session_mode, crate::session::SpawnSessionMode::Fresh);
            }
            other => panic!("expected spawn, got {other:?}"),
        }
    }

    #[test]
    fn reuse_session_mode_stores_null() {
        // `reuse` is the pre-v44 behavior, so an untouched spawn row keeps
        // writing NULL rather than a new discriminant.
        let db = Database::open_in_memory().unwrap();
        let new = NewAutomation {
            action: AutomationAction::Spawn {
                repo_path: std::path::PathBuf::from("/repo"),
                worktree_branch: None,
                base_branch: None,
                agent: None,
                extra_repos: Vec::new(),
                host: None,
                session_mode: crate::session::SpawnSessionMode::Reuse,
            },
            ..send_automation("local", Some(1))
        };
        let id = db.create_automation(&new).unwrap();
        let raw: Option<String> = db
            .conn
            .query_row(
                "SELECT action_session_mode FROM automations WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, None);
    }

    #[test]
    fn send_by_name_round_trips_and_survives_session_deletion() {
        let db = Database::open_in_memory().unwrap();
        let sid = SessionId::default();
        let by_name = NewAutomation {
            action: AutomationAction::Send {
                target: crate::session::SendTarget::Name("inbox".into()),
            },
            ..send_automation("by-name", Some(100))
        };
        let name_id = db.create_automation(&by_name).unwrap();
        let by_id = NewAutomation {
            action: AutomationAction::send_to(sid),
            ..send_automation("by-id", Some(100))
        };
        let id_id = db.create_automation(&by_id).unwrap();

        match db.get_automation(name_id).unwrap().unwrap().action {
            AutomationAction::Send { target } => assert_eq!(target.name(), Some("inbox")),
            other => panic!("expected send, got {other:?}"),
        }
        // Force-deleting the session disables only the id-targeted automation;
        // a name target is re-resolved at fire time, so it keeps working.
        assert_eq!(db.disable_send_automations_for_session(sid).unwrap(), 1);
        assert!(!db.get_automation(id_id).unwrap().unwrap().enabled);
        assert!(db.get_automation(name_id).unwrap().unwrap().enabled);
    }

    #[test]
    fn exec_timeout_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let new = NewAutomation {
            action: AutomationAction::Exec {
                command: "sync.sh".into(),
                timeout_secs: Some(30),
            },
            ..send_automation("sync", Some(1))
        };
        let id = db.create_automation(&new).unwrap();
        match db.get_automation(id).unwrap().unwrap().action {
            AutomationAction::Exec { timeout_secs, .. } => assert_eq!(timeout_secs, Some(30)),
            other => panic!("expected exec, got {other:?}"),
        }
    }

    #[test]
    fn running_run_is_closed_out_by_finish() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("exec", Some(100)))
            .unwrap();
        let run = db
            .record_automation_run(id, AutomationRunStatus::Running, "sync.sh", None)
            .unwrap();
        let listed = &db.list_automation_runs(id, 10).unwrap()[0];
        assert_eq!(listed.status, AutomationRunStatus::Running);
        // A run still in flight has no finish stamp.
        assert_eq!(listed.finished_at, None);

        assert!(db
            .finish_automation_run(run, AutomationRunStatus::Success, "ok")
            .unwrap());
        let listed = &db.list_automation_runs(id, 10).unwrap()[0];
        assert_eq!(listed.status, AutomationRunStatus::Success);
        assert_eq!(listed.detail, "ok");
        assert!(listed.finished_at.is_some());
        // One fire keeps exactly one history row.
        assert_eq!(db.list_automation_runs(id, 10).unwrap().len(), 1);
    }

    #[test]
    fn reap_leaves_recent_running_rows_alone() {
        let db = Database::open_in_memory().unwrap();
        let id = db
            .create_automation(&send_automation("exec", Some(100)))
            .unwrap();
        let fresh = db
            .record_automation_run(id, AutomationRunStatus::Running, "sync.sh", None)
            .unwrap();
        // A row another instance may still own is untouched...
        assert_eq!(db.reap_orphaned_automation_runs().unwrap(), 0);

        // ...but one older than any legal timeout is closed out as interrupted.
        db.conn
            .execute(
                "UPDATE automation_runs SET started_at = 0 WHERE id = ?1",
                params![fresh],
            )
            .unwrap();
        assert_eq!(db.reap_orphaned_automation_runs().unwrap(), 1);
        let listed = &db.list_automation_runs(id, 10).unwrap()[0];
        assert_eq!(listed.status, AutomationRunStatus::Error);
        assert!(
            listed.detail.contains("interrupted"),
            "got {}",
            listed.detail
        );
    }
}
