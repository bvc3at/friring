//! Driving the child lifecycle across ticks, and reconciling it after a crash
//! (ADR-32).
//!
//! [`super::bridge_spawn`] validates a request and starts a job; this advances
//! one step per job per tick, unwinds what a failure leaves behind, and — once,
//! before the broker serves anything — reconciles every saga a previous run left
//! unfinished.
//!
//! # Recovery acts on recorded identities and on nothing else
//!
//! Every branch below names a directory, a branch, a pane id and a pid that the
//! saga wrote down before it made them. There is no prefix scan, no "kill every
//! window whose name starts with", and no "remove the worktrees that have no
//! row" — each of which would act on something a *user* created that happened to
//! look like friring's. A saga step friring cannot read reconciles nothing.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::session::bridge::MailKind;
use crate::session::bridge::{ErrorCode, Response};
use crate::session::{ChildState, Outcome, SagaStep, SessionConfig, SessionId};
use crate::storage::bridge::RequestState;

use super::bridge_spawn::{
    bridge_launch, child_backend, child_overlay, hook_reported_since, mint_child_dirs, Goal,
    Pending, Step, WorktreeFacts, EGRESS_ACK_TIMEOUT, READY_TIMEOUT,
};
use super::App;

/// How many driver passes separate the host's `ack` from the kill.
///
/// Passes rather than a wall-clock wait, and deliberately: what has to be true
/// before the pane dies is that the `ack` is *durable* — which it is the moment
/// it is enqueued — and that the child has had a poll of its own to see it. A
/// clock would make the same guarantee depend on how loaded the machine is, and
/// would make every test of the quiesce wait out a timer.
pub(super) const ACK_SETTLE_PASSES: u8 = 3;

/// How long a `stop` waits for a finish intent before stopping the child anyway.
const GRACE_STEP: Duration = Duration::from_secs(120);

/// How long a single blocking step may run before the saga gives up on it.
///
/// A worktree checkout of a large repository is minutes; a window spawn is
/// seconds. One ceiling covers both, and its job is to stop a job living forever
/// rather than to be a tight bound.
pub(super) const BLOCKING_STEP_TIMEOUT: Duration = Duration::from_secs(600);

/// Which conversation one child launch runs as — see
/// [`App::child_resume_identity`].
pub(super) struct ResumeIdentity {
    /// The conversation's id. Minted for a `create`, and the child's own for a
    /// `resume`, so the id on its session row keeps naming one thread.
    pub(super) agent_session_id: String,
    /// What makes [`crate::session::AgentDef::build_args`] emit the agent's
    /// resume group. `None` is a fresh conversation, which is a `create` or an
    /// agent with no resume contract to honour.
    pub(super) resume_trigger: Option<String>,
}

impl App {
    /// Advance every lifecycle job by one step.
    ///
    /// In the **spawning** half of the tick ([`App::tick_background`]) rather
    /// than the deterministic half, because three of its steps put work on a
    /// blocking task and `tick_core` is documented never to spawn one. Nothing
    /// here blocks: a step either finishes on the tick or hands itself to a
    /// worker and is polled by a later one.
    pub(crate) fn tick_child_sagas(&mut self) {
        if !self.child_lifecycle.recovered {
            self.recover_child_sagas();
            self.child_lifecycle.recovered = true;
        }
        if self.child_lifecycle.jobs.is_empty() {
            return;
        }
        for index in (0..self.child_lifecycle.jobs.len()).rev() {
            if self.advance_child_job(index) {
                let mut job = self.child_lifecycle.jobs.remove(index);
                let follow = job.follow_up.take();
                let (owner, child) = (job.owner, job.child);
                // Dropping a job releases whatever egress claim it still holds:
                // a launch that never committed must not leave a listener behind.
                drop(job);
                // After the drop, so a quiesce this starts sees a child whose
                // launch has fully let go of it.
                if let Some(follow) = follow {
                    self.start_follow_up(owner, child, follow);
                }
            }
        }
    }

    /// Advance one job. Returns whether it is finished with.
    fn advance_child_job(&mut self, index: usize) -> bool {
        let step = self.child_lifecycle.jobs[index].step;
        // A step that is waiting on a worker either has its answer or does not.
        if self.child_lifecycle.jobs[index].pending.is_some() {
            return self.poll_child_job(index);
        }
        match step {
            Step::Named => self.job_start_worktree(index),
            Step::Worktree => self.job_mint_dirs(index),
            Step::Dirs => self.job_start_pane(index),
            Step::Pane => self.job_commit(index),
            Step::Committed => self.job_await_egress(index),
            Step::EgressLive => self.job_release_gate(index),
            Step::Released => self.job_await_ready(index),
            Step::Grace => self.job_await_finish_intent(index),
            Step::Acked => self.job_stop_pane(index),
            // Handled above: these steps only exist while a worker is running.
            Step::WorktreeRunning | Step::PaneRunning | Step::VerifyRunning => false,
        }
    }

    // ── S2: the worktree ─────────────────────────────────────────────────

    /// Record the worktree this saga is about to make, then make it off the
    /// tick.
    ///
    /// The path is recorded **first** and it is deterministic, which is what
    /// makes recording it before the effect possible: `git worktree add` puts a
    /// branch's worktree in one place, so friring knows where before git does.
    fn job_start_worktree(&mut self, index: usize) -> bool {
        let job = &self.child_lifecycle.jobs[index];
        // A resume relaunches an existing child, whose worktree is already
        // recorded. Nothing to create.
        if job.goal == Goal::Resume {
            if job.worktree_path.is_none() {
                return self.fail_child_job(
                    index,
                    ErrorCode::Failed,
                    "this child has no recorded worktree to relaunch in".to_string(),
                );
            }
            self.child_lifecycle.jobs[index].step = Step::Worktree;
            self.child_lifecycle.jobs[index].since = Instant::now();
            return false;
        }
        let repo = job.plan.repo_root.clone();
        let branch = job.plan.branch.clone();
        let Some(planned) = crate::git::planned_worktree_path(&repo, &branch) else {
            return self.fail_child_job(
                index,
                ErrorCode::Failed,
                "friring could not resolve a worktree directory for this child".to_string(),
            );
        };
        if self.record_saga_or_fail(index, |saga| {
            saga.step = Some(SagaStep::Worktree);
            saga.worktree_path = Some(planned.display().to_string());
        }) {
            return true;
        }
        let effects = Arc::clone(&self.child_lifecycle.effects);
        let (tx, rx) = mpsc::channel();
        let context = TestContext::capture();
        tokio::task::spawn_blocking(move || {
            let _context = context.install();
            // On the worker with the checkout, not on the tick: naming the base
            // runs two git subprocesses (ADR-3).
            let base = child_base_branch(&repo, &branch);
            let outcome =
                effects
                    .create_worktree(&repo, &branch, &base)
                    .map(|path| WorktreeFacts {
                        base_head: effects.head_commit(&path),
                        path,
                    });
            let _ = tx.send(outcome);
        });
        let job = &mut self.child_lifecycle.jobs[index];
        job.worktree_path = Some(planned);
        job.step = Step::WorktreeRunning;
        job.since = Instant::now();
        job.pending = Some(Pending::Worktree(rx));
        false
    }

    // ── S3: the directories and the private state ────────────────────────

    /// Mint the child's five directories and seed its private agent state.
    ///
    /// On the tick rather than on a worker: these are `mkdir`s and a handful of
    /// small files (a credential link, a rewritten hook configuration), all
    /// under friring's own data directory. A seeding that would be large is
    /// refused by [`crate::sandbox::child_state::MAX_REWRITE_BYTES`] rather than
    /// carried out slowly.
    fn job_mint_dirs(&mut self, index: usize) -> bool {
        let job = &self.child_lifecycle.jobs[index];
        let child_key = job.child.to_string();
        let Some(worktree) = job.worktree_path.clone() else {
            return self.fail_child_job(
                index,
                ErrorCode::Failed,
                "this child has no worktree to run in".to_string(),
            );
        };
        let agent_name = job.plan.agent.clone();
        let profile_name = job.plan.profile.clone();

        let dirs = match mint_child_dirs(&child_key, &worktree) {
            Ok(dirs) => dirs,
            Err(detail) => {
                return self.fail_child_job(
                    index,
                    ErrorCode::Failed,
                    format!("friring could not mint this child's directories: {detail}"),
                )
            }
        };
        if self.record_saga_or_fail(index, |saga| {
            saga.step = Some(SagaStep::Dirs);
            saga.scratch_minted = true;
            saga.gate_dir = Some(dirs.gate.display().to_string());
        }) {
            return true;
        }

        let profile = match self.db.get_sandbox_profile(&profile_name) {
            Ok(Some(stored)) if stored.is_intact() => stored.profile,
            _ => {
                return self.fail_child_job(
                    index,
                    ErrorCode::GrantMissing,
                    format!("sandbox profile '{profile_name}' could not be read for this child"),
                )
            }
        };
        let def = self.agent_def_for(&agent_name);
        let Some(agent_sandbox) = def.sandbox.clone() else {
            return self.fail_child_job(
                index,
                ErrorCode::StateUnrelocatable,
                format!(
                    "agent '{agent_name}' declares no sandbox block, so friring cannot give it \
                     private state — and a bridge child never runs from its family's shared state"
                ),
            );
        };
        let Some(home) = crate::paths::home_dir().map(|h| h.display().to_string()) else {
            return self.fail_child_job(
                index,
                ErrorCode::Failed,
                "friring could not resolve a home directory".to_string(),
            );
        };
        let plan = match crate::sandbox::child_state::SeedPlan::build(
            &agent_sandbox,
            &profile.child_seed_allow,
            &dirs.state,
            &home,
            &|path| path.exists(),
        ) {
            Ok(plan) => plan,
            Err(detail) => {
                return self.fail_child_job(index, ErrorCode::StateUnrelocatable, detail);
            }
        };
        if let Err(detail) = self.child_lifecycle.effects.seed_child_state(&plan) {
            return self.fail_child_job(
                index,
                ErrorCode::StateUnrelocatable,
                format!("this child's private agent state could not be seeded: {detail}"),
            );
        }
        // Every writable git root this child is about to get, made protectable
        // before it is granted: a protected name that does not exist is not a
        // protected name at all under bwrap or a container, and one of them
        // (`config.worktree`) is host command execution when the child creates
        // it. Refused rather than launched with a hole the backend cannot close.
        let repo_root = self.child_lifecycle.jobs[index].plan.repo_root.clone();
        let mut git_roots: Vec<String> = profile
            .child_shared_rw
            .iter()
            .map(|path| crate::session::expand_tilde(path, &home))
            .collect();
        git_roots.extend(super::bridge_spawn::child_git_dir(
            &dirs.worktree,
            &repo_root,
        ));
        for root in &git_roots {
            if let Err(detail) = super::bridge_spawn::ensure_protected_placeholders(root) {
                return self.fail_child_job(index, ErrorCode::StateUnrelocatable, detail);
            }
        }
        let overlay = child_overlay(
            &profile,
            &self.child_lifecycle.jobs[index].owner.to_string(),
            &dirs,
            Some(&agent_sandbox),
            &home,
            &plan,
            &repo_root,
        );
        let job = &mut self.child_lifecycle.jobs[index];
        job.dirs = Some(dirs);
        job.overlay = Some(overlay);
        job.step = Step::Dirs;
        job.since = Instant::now();
        false
    }

    /// The part of a child's launch environment that decides where its agent
    /// keeps its conversation.
    ///
    /// One variable: the agent's own `config_dir_env`, pointed at the private
    /// state directory ADR-31 gives the child. That is what a transcript lookup
    /// has to resolve against, and reading it from the default location instead
    /// would ask about the *operator's* conversations — a question with no
    /// bearing on a child and, for a claude worker, an answer that is wrong in
    /// both directions.
    pub(super) fn child_env(&self, child: SessionId) -> std::collections::HashMap<String, String> {
        let mut env = std::collections::HashMap::new();
        let agent = self
            .db
            .get_session_by_id(child)
            .ok()
            .flatten()
            .map(|row| row.agent.clone())
            .unwrap_or_default();
        let def = self.agent_def_for(&agent);
        let (Some(var), Some(dir)) = (
            def.sandbox
                .as_ref()
                .and_then(|s| s.config_dir_env.clone())
                .filter(|v| !v.is_empty()),
            crate::sandbox::dirs::child_state_dir(&child.to_string()),
        ) else {
            return env;
        };
        env.insert(var, dir.to_string_lossy().into_owned());
        env
    }

    /// Which conversation a child's launch runs as.
    ///
    /// A `resume` **comes back to the child's own conversation**, which is what
    /// makes parking mean anything for an agent that has one: the worktree, the
    /// branch, the mailbox and the private state directory are only half of what
    /// an owner parked. The other half is the thread.
    ///
    /// Two facts decide it, and both are the child's own:
    ///
    /// - the `agent_session_id` friring minted when it created this child, kept
    ///   on its session row, which is the conversation to come back to;
    /// - the agent's resume contract
    ///   ([`session_ops::resume_trigger_for`](crate::session_ops)), shared with
    ///   the ordinary restart path so a bridge child and a hand-restarted
    ///   session resume the same way.
    ///
    /// # Errors
    ///
    /// A resume that **cannot be proved** to reach the conversation is refused
    /// rather than launched blank, because a blank one looks identical from the
    /// outside: the child comes up, answers its mail and has forgotten the task,
    /// and nothing says so. Three ways that happens, and the third is why the
    /// contract is declarative:
    ///
    /// - the child's session row carries no `agent_session_id` at all;
    /// - the agent declares where its conversations live
    ///   ([`TranscriptDef`](crate::session::TranscriptDef)) and this child's is
    ///   not there — a claude transcript deleted, a codex state directory that
    ///   was never written;
    /// - the agent declares a resume contract but **no** transcript block —
    ///   whether it resumes by id or by latest. `resume --last` in an empty
    ///   directory is not a resume, and the only fallback core has is one
    ///   vendor's on-disk layout, which for any other agent asks the wrong
    ///   directory and could authorize a resume on a stale file of that
    ///   vendor's. So friring will not guess: the refusal names the block to
    ///   add. The check is generic — it asks the registry what the agent
    ///   declared, never what the agent *is*.
    ///
    /// An agent that declares no resume contract at all is none of them. A
    /// `/bin/sh` worker has no conversation to lose, so its relaunch is a fresh
    /// process by definition and nothing is being silently dropped.
    pub(super) fn child_resume_identity(
        &self,
        child: SessionId,
        agent: &str,
        env: &std::collections::HashMap<String, String>,
    ) -> Result<ResumeIdentity, (ErrorCode, String)> {
        // `agent_def_for`, the same resolution `launch_provider_for` uses to
        // build the argv a moment later. A second source would let the decision
        // and the command line disagree about what this agent can do.
        let def = self.agent_def_for(agent);
        let recorded = self
            .db
            .get_session_by_id(child)
            .ok()
            .flatten()
            .and_then(|row| row.agent_session_id.clone());
        if !def.resumes_latest() && !def.resumes_by_id() {
            // No thread to preserve. `create` behaviour, unchanged.
            return Ok(ResumeIdentity {
                agent_session_id: recorded.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                resume_trigger: None,
            });
        }
        let Some(agent_session_id) = recorded else {
            return Err((
                ErrorCode::Failed,
                format!(
                    "this child has no recorded conversation for agent '{agent}' to come back \
                     to, and friring will not resume it into a blank one"
                ),
            ));
        };
        // What the agent declares about where its conversations live. `None`
        // is "friring cannot tell", which for a child is a refusal: an
        // unprovable resume and a blank one are the same launch.
        match crate::session_ops::conversation_exists(&def, &agent_session_id, env) {
            Some(true) => {}
            Some(false) => {
                return Err((
                    ErrorCode::Failed,
                    format!(
                        "agent '{agent}' can resume a conversation but friring cannot reach this \
                         child's — nothing matching it is in the private state directory it was \
                         given. Resuming would start a new one, which is not what a resume means"
                    ),
                ))
            }
            // Including an agent that resumes **by id**. The fallback
            // `resume_trigger_for` would take is `resume_id_if_transcript_exists`,
            // which knows one vendor's on-disk layout: for any other by-id agent
            // it asks the wrong directory, and a stale file of that vendor's
            // could authorize a resume of a conversation that is not there. A
            // bridge child's acceptance must not depend on core knowing an
            // agent — that is what the declaration is for.
            None => {
                return Err((
                    ErrorCode::Failed,
                    format!(
                        "agent '{agent}' declares that it can resume a conversation, but its \
                         registry entry declares no [agents.{agent}.transcript] block — so \
                         friring cannot tell a resume that reaches this child's conversation \
                         from one that silently starts a new one, and will not guess"
                    ),
                ))
            }
        }
        let Some(trigger) = crate::session_ops::resume_trigger_for(&def, &agent_session_id, env)
        else {
            return Err((
                ErrorCode::Failed,
                format!(
                    "agent '{agent}' can resume a conversation but friring cannot reach this \
                     child's — its transcript is gone. Resuming would start a new one, which \
                     is not what a resume means"
                ),
            ));
        };
        Ok(ResumeIdentity {
            agent_session_id,
            resume_trigger: Some(trigger),
        })
    }

    // ── S4 + S5: the proxy and the gated pane ────────────────────────────

    /// Compose the child's launch and open its gated pane off the tick.
    ///
    /// S4 (the proxy bind) happens inside the composition, which is where argv
    /// has to name the endpoint. The instance it binds is **claimed, not
    /// committed**: it comes back on the [`crate::agent::backend::GatedSession`]
    /// and becomes the child's at S7, so a failure between here and there
    /// releases it.
    fn job_start_pane(&mut self, index: usize) -> bool {
        let job = &self.child_lifecycle.jobs[index];
        let (Some(dirs), Some(overlay)) = (job.dirs.clone(), job.overlay.clone()) else {
            return self.fail_child_job(
                index,
                ErrorCode::Failed,
                "this child's directories were not minted".to_string(),
            );
        };
        let child = job.child;
        let name = job.name.clone();
        let gate_key = job.gate_key.clone();
        let agent = job.plan.agent.clone();
        let profile_name = job.plan.profile.clone();
        let goal = job.goal;

        let profile = match self.load_session_sandbox(Some(&profile_name)) {
            Ok(Some(profile)) => profile,
            Ok(None) => {
                return self.fail_child_job(
                    index,
                    ErrorCode::GrantMissing,
                    format!("sandbox profile '{profile_name}' is empty"),
                )
            }
            Err(detail) => {
                return self.fail_child_job(index, ErrorCode::GrantMissing, detail);
            }
        };
        let mut config = SessionConfig {
            session_id: Some(child),
            agent: agent.clone(),
            session_name: Some(name.clone()),
            cwd: Some(dirs.worktree.clone()),
            sandbox: Some(profile),
            ..SessionConfig::default()
        };
        // A `create` mints a conversation; a `resume` comes back to the one this
        // child already had. Both need the id in the environment, so it is
        // decided before the env is injected.
        let identity = if goal == Goal::Resume {
            // `child_env`, not `config.env`: the config is a moment old and its
            // environment is still empty, so a transcript lookup would ask about
            // the operator's own conversations instead of this child's — and
            // answer "gone" for a child whose state is right there. `begin_resume`
            // asked the same question with the same environment.
            match self.child_resume_identity(child, &agent, &self.child_env(child)) {
                Ok(id) => id,
                Err((code, detail)) => return self.fail_child_job(index, code, detail),
            }
        } else {
            ResumeIdentity {
                agent_session_id: uuid::Uuid::new_v4().to_string(),
                resume_trigger: None,
            }
        };
        let agent_session_id = identity.agent_session_id;
        config.agent_session_id = Some(agent_session_id.clone());
        config.resume_session_id = identity.resume_trigger;
        crate::session_ops::inject_friring_env(&mut config, &agent_session_id, None);

        // Recorded before the window exists. The identity itself is only known
        // afterwards; until it is, a crash leaves a gated pane that the launch
        // helper's own timeout closes (ADR-33) — the backstop this step relies
        // on rather than a scan for panes friring might have made.
        if self.record_saga_or_fail(index, |saga| {
            saga.step = Some(SagaStep::Pane);
        }) {
            return true;
        }

        let backend = child_backend(self);
        let provider = self.launch_provider_for(&config);
        let bridge = bridge_launch(overlay, &dirs, &gate_key);
        let (rows, cols) = self.content_area_size();
        let (tx, rx) = mpsc::channel();
        let context = TestContext::capture();
        tokio::task::spawn_blocking(move || {
            let _context = context.install();
            let outcome = crate::agent::backend::Session::spawn_gated(
                name, rows, cols, &config, &backend, &provider, &bridge,
            )
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(outcome);
        });
        let job = &mut self.child_lifecycle.jobs[index];
        job.step = Step::PaneRunning;
        job.since = Instant::now();
        job.pending = Some(Pending::Pane(Box::new(rx)));
        false
    }

    // ── S6: the one transaction ──────────────────────────────────────────

    /// Commit the child: session row, ownership, starting state, its first mail
    /// and the saga's `committed` step, together.
    ///
    /// One transaction because they are one fact. A child with a session row and
    /// no ownership row would be a session no verb can act on; one with an
    /// ownership row and no task would be a worker with nothing to do and no
    /// record of why; and one whose saga still read as pre-commit would be torn
    /// down by recovery, which cannot delete the ownership row it leaves behind.
    fn job_commit(&mut self, index: usize) -> bool {
        let Some(session) = self.child_lifecycle.jobs[index].spawned.take() else {
            return self.fail_child_job(
                index,
                ErrorCode::Failed,
                "this child has no pane to commit".to_string(),
            );
        };
        let job = &self.child_lifecycle.jobs[index];
        let child_id = job.child.to_string();
        let owner_id = job.owner.to_string();
        let request_key = job
            .key
            .as_ref()
            .map(|k| k.as_str().to_string())
            .unwrap_or_default();
        let relaunch = job.goal == Goal::Resume;
        let repo_root = job.plan.repo_root.display().to_string();
        let branch = job.plan.branch.clone();
        let worktree = job.worktree_path.as_ref().map(|p| p.display().to_string());
        let task_kind = job.plan.task_kind.clone();
        let task_body = job.plan.task_body.clone();
        let role_hint = job.plan.role_hint.clone();

        let shared = self.child_shared_session(&session);
        // The `committed` step goes in with the rows, not after them: recovery
        // reads only `saga.step`, and a crash between the two would send it down
        // the pre-commit branch against an ownership row it cannot delete — the
        // child would stay `Starting` and hold one of the owner's slots forever.
        let saga = self.saga_row(index, |saga| saga.step = Some(SagaStep::Committed));
        let committed = self
            .db
            .commit_bridge_child(&crate::storage::bridge::ChildCommit {
                session: &shared,
                child_id: &child_id,
                owner_id: &owner_id,
                request_key: &request_key,
                new_ownership: !relaunch,
                repo_root: &repo_root,
                worktree_path: worktree.as_deref(),
                branch: &branch,
                task_kind: &task_kind,
                task_body: &task_body,
                role_hint: role_hint.as_deref(),
                saga: saga.as_ref(),
            });
        if let Err(e) = committed {
            // Nothing was written: the transaction rolled back whole, and the
            // pane still holds a gate nothing will open. The unwind kills it.
            self.child_lifecycle.jobs[index].spawned = Some(session);
            return self.fail_child_job(
                index,
                ErrorCode::Failed,
                format!("friring could not commit this child: {e}"),
            );
        }
        // The session becomes the app's only once its rows exist, so a rollback
        // above leaves nothing in the list to reconcile.
        self.learn_launched_place(&session);
        // **Replace**, never push a second entry for one id. Every later lookup
        // is `sessions.iter().find(|s| s.info.id == child)`, which returns the
        // first match — so a leftover entry would make S8 revalidate the old
        // pane's identity, pass, and then open the new pane's gate. A resume
        // retires the old entry before it starts (`stop_resumed_child_pane`);
        // this is the invariant rather than the courtesy.
        let child_id = session.info.id;
        self.sessions.retain(|s| s.info.id != child_id);
        self.sessions.push(*session);
        self.request_redraw();
        let job = &mut self.child_lifecycle.jobs[index];
        job.step = Step::Committed;
        job.since = Instant::now();
        false
    }

    // ── S7: the supervisor's acknowledgement ─────────────────────────────

    /// Commit the proxy claim, then wait for the supervisor to say it holds it.
    ///
    /// A commit is a message to a thread, so it returns before anything has
    /// agreed to filter. Releasing the gate on that would start an agent that
    /// believes it is filtered and is not, so a silent supervisor fails the
    /// saga instead.
    fn job_await_egress(&mut self, index: usize) -> bool {
        let job = &mut self.child_lifecycle.jobs[index];
        if let Some(claim) = job.egress.take() {
            claim.commit();
        }
        let child = job.child;
        let key = child.to_string();
        let waited = job.since.elapsed();
        let expected = self
            .sessions
            .iter()
            .find(|s| s.info.id == child)
            .map(|s| s.info.egress_state.clone());
        // A child whose profile needs no proxy has nothing to acknowledge.
        let needs_proxy = matches!(expected, Some(crate::session::EgressState::Preparing));
        if !needs_proxy || self.child_lifecycle.effects.egress_acknowledged(&key) {
            if needs_proxy {
                self.set_session_egress_state(child, crate::session::EgressState::Active);
            }
            self.record_saga_after(index, |saga| saga.step = Some(SagaStep::EgressLive));
            let job = &mut self.child_lifecycle.jobs[index];
            job.step = Step::EgressLive;
            job.since = Instant::now();
            return false;
        }
        if waited >= EGRESS_ACK_TIMEOUT {
            return self.fail_child_job(
                index,
                ErrorCode::EgressNotRestorable,
                "this child's egress proxy never acknowledged the commit, so friring did not \
                 release its gate"
                    .to_string(),
            );
        }
        false
    }

    // ── S8: the gate ─────────────────────────────────────────────────────

    /// Prove the pane is the recorded one, then open its gate.
    ///
    /// The revalidation is not a formality: the release file is what starts an
    /// agent, and a pane id is reused after a server restart. Writing a key into
    /// a gate whose pane is now somebody else's would start *their* process
    /// under this child's boundary.
    fn job_release_gate(&mut self, index: usize) -> bool {
        let child = self.child_lifecycle.jobs[index].child;
        let gate_key = self.child_lifecycle.jobs[index].gate_key.clone();
        let identity = self
            .sessions
            .iter()
            .find(|s| s.info.id == child)
            .map(|s| s.revalidate_identity());
        match identity {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                return self.fail_child_job(index, ErrorCode::IdentityMismatch, format!("{e:#}"))
            }
            None => {
                return self.fail_child_job(
                    index,
                    ErrorCode::Failed,
                    "this child's session is no longer in the list".to_string(),
                )
            }
        }
        // Recorded **before** the rename, like every other effect in this
        // module: opening the gate starts the agent, and a crash between the two
        // would leave a running agent whose saga still says `EgressLive` — a
        // step recovery treats as not-yet-started. Written first, the worst case
        // is a `Released` row for a gate that never opened, which the readiness
        // timeout resolves.
        if self.record_saga_or_fail(index, |saga| saga.step = Some(SagaStep::Released)) {
            return true;
        }
        if let Err(detail) = release_gate(&child.to_string(), &gate_key) {
            return self.fail_child_job(
                index,
                ErrorCode::Failed,
                format!("friring could not open this child's launch gate: {detail}"),
            );
        }
        // The retiring launch's hook row goes now, so only the new launch's
        // report can satisfy S9. `apply_status_signals` ignores a file repeating
        // the recorded state — deliberately, so a repeated `done` does not
        // resurrect as unseen — which means a resumed child whose new agent
        // first reports the same word as its previous life would never re-stamp
        // `state_at` and would die at the readiness timeout. It has to be here
        // and not in `begin_resume`: cleared any earlier, the *old* agent's last
        // report could land at or after `opened_at` below and falsely prove the
        // new one ready.
        let _ = self.db.clear_hook_state(child);
        self.cached_hook_states.remove(&child);
        // Stamped from the same clock the hook channel writes `state_at` with,
        // because S9 compares the two: only a report at or after this moment is
        // one this launch produced.
        let opened_at = crate::sync::current_time_millis() as i64;
        let job = &mut self.child_lifecycle.jobs[index];
        job.step = Step::Released;
        job.since = Instant::now();
        job.gate_opened_at = Some(opened_at);
        false
    }

    // ── S9: the child's own hook ─────────────────────────────────────────

    /// Wait for the child's own hook to report, and only for that.
    ///
    /// The hook fires from inside the boundary, from the relocated private state
    /// directory the launch pointed the agent at. That is why it is the accepted
    /// proof and a live pane process is not: a pane proves something is running,
    /// not that it is running from the state friring seeded.
    fn job_await_ready(&mut self, index: usize) -> bool {
        let child = self.child_lifecycle.jobs[index].child;
        // The hook row, not the derived `SessionInfo::status`: that field is
        // `Working` from the moment the session is constructed, so reading it
        // would call every child ready on the first pass after its gate opened.
        // `gate_opened_at` is `None` only if S8 has not run, which this step is
        // not reached without.
        let since = self.child_lifecycle.jobs[index].gate_opened_at;
        let ready = since
            .is_some_and(|since| hook_reported_since(self.cached_hook_states.get(&child), since));
        if ready {
            let child_id = child.to_string();
            let _ = self.db.set_bridge_child_state(&child_id, ChildState::Ready);
            self.record_saga_after(index, |saga| saga.step = Some(SagaStep::Done));
            let owner = self.child_lifecycle.jobs[index].owner;
            let name = self.child_lifecycle.jobs[index].name.clone();
            let branch = self.child_lifecycle.jobs[index].plan.branch.clone();
            let worktree = self.child_lifecycle.jobs[index]
                .worktree_path
                .as_ref()
                .map(|p| p.display().to_string());
            let relaunch = self.child_lifecycle.jobs[index].goal == Goal::Resume;
            self.answer_child_job(
                index,
                Response::ok(
                    self.child_lifecycle.jobs[index]
                        .key
                        .clone()
                        .unwrap_or_else(placeholder_key),
                    Some(serde_json::json!({
                        "child_id": child_id,
                        "name": name,
                        "branch": branch,
                        "worktree_path": worktree,
                        "state": ChildState::Ready.as_str(),
                    })),
                ),
            );
            self.host_mail(
                owner,
                if relaunch {
                    MailKind::ChildRelaunched
                } else {
                    MailKind::ChildReady
                },
                &child_id,
            );
            return true;
        }
        if self.child_lifecycle.jobs[index].since.elapsed() >= READY_TIMEOUT {
            return self.fail_child_job(
                index,
                ErrorCode::NotReady,
                format!(
                    "this child's own hook did not report within {}s, so friring cannot prove it \
                     is running from the private state it was given",
                    READY_TIMEOUT.as_secs()
                ),
            );
        }
        false
    }

    // ── The quiesce ──────────────────────────────────────────────────────

    /// A `stop` waiting for the child's own finish intent, or for the grace to
    /// run out.
    fn job_await_finish_intent(&mut self, index: usize) -> bool {
        let child_id = self.child_lifecycle.jobs[index].child.to_string();
        // The child answered: `accept_finish_intent` moved this job on already.
        if self.child_lifecycle.jobs[index].since.elapsed() < GRACE_STEP {
            return false;
        }
        let child = self.child_lifecycle.jobs[index].child;
        let _ = self
            .db
            .set_bridge_child_state(&child_id, ChildState::Finishing);
        self.host_mail(child, MailKind::Cancel, &child_id);
        let job = &mut self.child_lifecycle.jobs[index];
        job.step = Step::Acked;
        job.since = Instant::now();
        false
    }

    /// Quiesce step 3: stop the exact child pane.
    ///
    /// After this the agent cannot write, which is the whole point: everything
    /// the host reads next is final. A kill that is refused, or a pane whose
    /// identity does not match what friring recorded, lands the child in
    /// [`ChildState::StopFailed`] — never integrated, never reused, and left for
    /// an operator. friring will not guess which process to kill.
    fn job_stop_pane(&mut self, index: usize) -> bool {
        if self.child_lifecycle.jobs[index].settle > 0 {
            self.child_lifecycle.jobs[index].settle -= 1;
            return false;
        }
        let child = self.child_lifecycle.jobs[index].child;
        let child_id = child.to_string();
        if !self.child_lifecycle.jobs[index].stopped {
            match self.stop_child_pane(child) {
                Ok(()) => self.child_lifecycle.jobs[index].stopped = true,
                Err(detail) => {
                    let _ = self
                        .db
                        .set_bridge_child_state(&child_id, ChildState::StopFailed);
                    self.notify_child_state(child, ChildState::StopFailed);
                    let owner = self.child_lifecycle.jobs[index].owner;
                    self.host_mail(owner, MailKind::ChildStopFailed, &child_id);
                    let key = self.child_lifecycle.jobs[index]
                        .key
                        .clone()
                        .unwrap_or_else(placeholder_key);
                    self.answer_child_job(
                        index,
                        Response::refused(key, ErrorCode::QuiesceFailed, detail),
                    );
                    return true;
                }
            }
        }
        // What the host reads, in a worktree nothing can write to any more.
        let worktree = self.child_worktree(&child_id);
        let Some(worktree) = worktree else {
            return self.decide_child_outcome(
                index,
                crate::git::WorktreeVerdict {
                    dirty: true,
                    unreadable: true,
                    ..crate::git::WorktreeVerdict::default()
                },
            );
        };
        let base_head = self.child_base_head(&child_id);
        let effects = Arc::clone(&self.child_lifecycle.effects);
        let (tx, rx) = mpsc::channel();
        let context = TestContext::capture();
        tokio::task::spawn_blocking(move || {
            let _context = context.install();
            let _ = tx.send(effects.verify_worktree(&worktree, base_head.as_deref()));
        });
        let job = &mut self.child_lifecycle.jobs[index];
        job.step = Step::VerifyRunning;
        job.since = Instant::now();
        job.pending = Some(Pending::Verify(rx));
        false
    }

    /// Quiesce step 5: write the host's verdict and decide the terminal state.
    ///
    /// A **dirty** worktree is never integrated, whatever the intent said, and
    /// the child keeps its slot: the owner may `resume` so the worker can commit,
    /// or `stop`. That is the one rule here that a caller cannot talk friring
    /// out of.
    fn decide_child_outcome(&mut self, index: usize, verdict: crate::git::WorktreeVerdict) -> bool {
        let job = &self.child_lifecycle.jobs[index];
        let child_id = job.child.to_string();
        let owner = job.owner;
        let intent = job
            .intent
            .clone()
            .unwrap_or(super::bridge_spawn::FinishIntent {
                outcome: Outcome::Failed,
                message_id: None,
                clean_state: ChildState::Failed,
            });
        let key = job.key.clone().unwrap_or_else(placeholder_key);

        let result = crate::session::BridgeResult {
            child_id: child_id.clone(),
            message_id: intent.message_id,
            outcome: intent.outcome,
            branch: verdict.branch.clone(),
            head: verdict.head.clone(),
            dirty: verdict.dirty,
            ahead_of_base: verdict.ahead_of_base,
            verified_at: crate::sync::state::current_time_millis(),
        };
        let state = if verdict.dirty {
            ChildState::Dirty
        } else {
            intent.clean_state
        };
        // The verdict and the state are what the quiesce *is*. Written before
        // anything is answered or retired, and a failure of either ends the job
        // as a refusal rather than as a `done` nothing recorded: `bridge_results`
        // is what an integration step reads, and a caller told `done` over an
        // empty row would merge a branch friring never verified.
        //
        // The child is left in `finishing`, which is deliberate and is the
        // recoverable half: it is live (so the slot is held and an operator sees
        // it), and `recover_child_sagas` re-runs the quiesce for every child in
        // that state on the next start. The pane is already dead, so the second
        // pass reads the same worktree and reaches the same verdict.
        if let Err(detail) = self
            .db
            .upsert_bridge_result(&result)
            .map_err(|e| format!("the verdict: {e}"))
            .and_then(|()| {
                self.db
                    .set_bridge_child_state(&child_id, state)
                    .map_err(|e| format!("the child's state: {e}"))
            })
        {
            tracing::warn!("bridge: could not record the quiesce of '{child_id}': {detail}");
            self.set_error(format!(
                "friring stopped child '{child_id}' and could not record what it found: {detail}"
            ));
            self.host_mail(owner, MailKind::ChildStopFailed, &child_id);
            self.answer_child_job(
                index,
                Response::refused(
                    key,
                    ErrorCode::QuiesceFailed,
                    format!(
                        "friring stopped child '{child_id}' and could not record what it found \
                         ({detail}). The child is left 'finishing' and the verdict will be \
                         written again when friring next starts; nothing has been reported \
                         complete"
                    ),
                ),
            );
            return true;
        }
        self.retire_quiesced_child(self.child_lifecycle.jobs[index].child, state);
        self.notify_child_state(self.child_lifecycle.jobs[index].child, state);
        let kind = match state {
            ChildState::Dirty => MailKind::ChildDirty,
            ChildState::Done => MailKind::ChildDone,
            ChildState::Stopped | ChildState::Failed => MailKind::ChildFailed,
            _ => MailKind::ChildFailed,
        };
        self.host_mail(owner, kind, &child_id);
        self.answer_child_job(
            index,
            Response::ok(
                key,
                Some(serde_json::json!({
                    "child": child_id,
                    "state": state.as_str(),
                    "outcome": intent.outcome.as_str(),
                    "branch": verdict.branch,
                    "head": verdict.head,
                    "dirty": verdict.dirty,
                    "ahead_of_base": verdict.ahead_of_base,
                })),
            ),
        );
        true
    }

    /// Retire the runtime of a child the quiesce has stopped.
    ///
    /// The quiesce kills the child's exact pane, so what is left in
    /// `self.sessions` is an entry for a process that is gone. Left there it is
    /// an active session row with an `agent_session_id` and no pane, which is
    /// precisely what startup restore relaunches — restarting an agent inside a
    /// worktree friring has already verified, over the verdict a later decision
    /// reads.
    ///
    /// The **rows** stay: ownership, the worktree, the result and the mail are
    /// what `status` and integration are made of, and none of them is runtime.
    /// Only the live `Session` and the loaded flag go.
    ///
    /// A `dirty` or `stop_failed` child is deliberately *not* retired: its state
    /// is live, it may still have a pane, and it is the one an operator has to
    /// look at.
    fn retire_quiesced_child(&mut self, child: SessionId, state: ChildState) {
        if !state.is_terminal() {
            return;
        }
        self.sessions.retain(|s| s.info.id != child);
        self.session_terminal_views.remove(&child);
        if let Err(e) = self.db.set_session_unloaded(child, true) {
            tracing::warn!("bridge: could not retire the child session '{child}': {e}");
        }
        if self.active_index >= self.sessions.len() {
            self.active_index = self.sessions.len().saturating_sub(1);
        }
        self.request_redraw();
    }

    // ── Polling the blocking steps ───────────────────────────────────────

    /// Take a worker's answer, if it has one. Returns whether the job is done.
    fn poll_child_job(&mut self, index: usize) -> bool {
        let received = match self.child_lifecycle.jobs[index].pending.as_ref() {
            Some(Pending::Worktree(rx)) => match rx.try_recv() {
                Ok(outcome) => Some(Received::Worktree(outcome)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Received::Died),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            Some(Pending::Pane(rx)) => match rx.try_recv() {
                Ok(outcome) => Some(Received::Pane(Box::new(outcome))),
                Err(mpsc::TryRecvError::Disconnected) => Some(Received::Died),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            Some(Pending::Verify(rx)) => match rx.try_recv() {
                Ok(verdict) => Some(Received::Verify(verdict)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Received::Died),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            None => None,
        };
        let Some(received) = received else {
            if self.child_lifecycle.jobs[index].since.elapsed() >= BLOCKING_STEP_TIMEOUT {
                self.child_lifecycle.jobs[index].pending = None;
                return self.fail_child_job(
                    index,
                    ErrorCode::Failed,
                    format!(
                        "a step of this child's launch did not finish within {}s",
                        BLOCKING_STEP_TIMEOUT.as_secs()
                    ),
                );
            }
            return false;
        };
        self.child_lifecycle.jobs[index].pending = None;
        match received {
            Received::Died => self.fail_child_job(
                index,
                ErrorCode::Failed,
                "a step of this child's launch died without answering".to_string(),
            ),
            Received::Worktree(Err(failure)) => {
                // Whose the leftovers are is not inferred from the planned path
                // — it is what the two-phase claim reports. Without the branch
                // this attempt created nothing, and another instance that won
                // the ref owns whatever sits at that path; the path was recorded
                // optimistically before the worker ran, so it is dropped and the
                // unwind leaves it alone. *With* the branch, the path is this
                // saga's whatever git's exit code said — a repository whose
                // `post-checkout` hook fails makes `git worktree add` report
                // failure over a worktree it created and registered — so it is
                // kept and reclaimed.
                if !failure.owns_branch {
                    self.child_lifecycle.jobs[index].worktree_path = None;
                }
                self.child_lifecycle.jobs[index].branch_claimed = failure.owns_branch;
                self.record_saga_after(index, |saga| saga.branch_claimed = failure.owns_branch);
                self.fail_child_job(
                    index,
                    ErrorCode::Failed,
                    format!(
                        "friring could not create this child's worktree: {}",
                        failure.detail
                    ),
                )
            }
            Received::Worktree(Ok(facts)) => {
                let base_head = facts.base_head.clone();
                let path = facts.path.clone();
                self.record_saga_after(index, |saga| {
                    saga.worktree_path = Some(path.display().to_string());
                    saga.base_head = base_head.clone();
                    // Recorded on the success side too, and load-bearing there:
                    // this is what tells a *later* process's recovery that the
                    // worktree at the recorded path was made by this saga and is
                    // safe to reclaim.
                    saga.branch_claimed = true;
                });
                let job = &mut self.child_lifecycle.jobs[index];
                job.worktree_path = Some(facts.path);
                job.base_head = facts.base_head;
                job.branch_claimed = true;
                job.step = Step::Worktree;
                job.since = Instant::now();
                false
            }
            Received::Pane(outcome) => match *outcome {
                Err(detail) => self.fail_child_job(index, ErrorCode::Failed, detail),
                Ok(gated) => {
                    let identity = gated.session.info.mux.clone();
                    self.record_saga_after(index, |saga| {
                        saga.mux_server = identity.server.clone();
                        saga.mux_window_id = identity.window_id.clone();
                        saga.mux_pane_id = identity.pane_id.clone();
                        saga.mux_pane_pid = identity.pane_pid;
                        // Recorded too, and load-bearing: without the marker
                        // `MuxIdentity::is_recorded` is false, so recovery's
                        // exact-pane kill would return early every time.
                        saga.mux_launch_key = identity.launch_key.clone();
                        saga.egress_endpoint = None;
                    });
                    let job = &mut self.child_lifecycle.jobs[index];
                    job.egress = Some(gated.egress);
                    job.spawned = Some(Box::new(gated.session));
                    job.step = Step::Pane;
                    job.since = Instant::now();
                    false
                }
            },
            Received::Verify(verdict) => self.decide_child_outcome(index, verdict),
        }
    }

    // ── Finishing a job ──────────────────────────────────────────────────

    /// Answer the request this job was started for, and journal the answer.
    ///
    /// A `create` that nobody is waiting on any more still gets its response
    /// file: the client may have timed out and be about to retry with the same
    /// key, and the journal is what makes that retry find this answer rather
    /// than doing the work again.
    fn answer_child_job(&mut self, index: usize, response: Response) {
        let job = &self.child_lifecycle.jobs[index];
        let owner = job.owner.to_string();
        // Every key this job answers: its own, plus any request that arrived
        // while it was running and was accepted against it. Without the second
        // list those callers would be journaled `accepted` and never answered,
        // and a replay of such a key waits rather than acting — so a `stop`
        // issued during a create would hang for good.
        let keys: Vec<crate::session::bridge::RequestKey> =
            job.key.iter().chain(job.waiters.iter()).cloned().collect();
        for key in keys {
            // Each answer carries the key it answers, so a client matches the
            // response to the request it sent.
            let mut answer = response.clone();
            answer.key = key;
            self.answer_bridge_request_at(&owner, answer);
        }
    }

    /// Journal and write one answer to a request nothing is carrying any more.
    ///
    /// The deferred verbs are answered by their job; this is for the ones whose
    /// job ended without ever becoming theirs to answer — a `stop` held behind a
    /// launch that then failed (see
    /// [`App::start_follow_up`](super::App::start_follow_up)).
    pub(super) fn answer_bridge_request(&mut self, owner: SessionId, response: Response) {
        self.answer_bridge_request_at(&owner.to_string(), response);
    }

    fn answer_bridge_request_at(&mut self, owner: &str, response: Response) {
        let state = if response.ok {
            RequestState::Done
        } else {
            RequestState::Failed
        };
        let key = response.key.clone();
        let encoded = serde_json::to_string(&response).unwrap_or_default();
        if let Err(e) = self
            .db
            .finish_bridge_request(owner, key.as_str(), state, &encoded)
        {
            tracing::warn!("bridge: could not journal the answer to '{key}': {e}");
        }
        if let Err(e) = crate::paths::write_bridge_response(owner, key.as_str(), &encoded) {
            tracing::warn!("bridge: could not write the answer to '{key}': {e}");
        }
    }

    /// Fail a job: unwind what it made, mark the saga, answer the request.
    ///
    /// Returns `true` so the caller drops the job.
    fn fail_child_job(&mut self, index: usize, code: ErrorCode, detail: String) -> bool {
        let job = &self.child_lifecycle.jobs[index];
        let child_id = job.child.to_string();
        let goal = job.goal;
        let key = job.key.clone().unwrap_or_else(placeholder_key);
        tracing::warn!(
            "bridge: child '{child_id}' failed at {:?}: {detail}",
            job.step
        );

        if goal.is_launch() {
            self.unwind_child_launch(index);
            self.record_saga_after(index, |saga| saga.step = Some(SagaStep::Failed));
            // A launch that never committed has no ownership row to mark; one
            // that did is a real child, and `failed` is what it reached.
            if self.db.bridge_child(&child_id).ok().flatten().is_some() {
                let _ = self
                    .db
                    .set_bridge_child_state(&child_id, ChildState::Failed);
                let owner = self.child_lifecycle.jobs[index].owner;
                self.host_mail(owner, MailKind::ChildFailed, &child_id);
            }
        }
        self.answer_child_job(index, Response::refused(key, code, detail));
        true
    }

    /// Remove what a failed launch made, by recorded identity only.
    ///
    /// In reverse order of creation, and each step guarded by what the saga
    /// actually recorded: a pane is killed only when its identity matches, a
    /// worktree removed only when it is clean, a branch deleted only when it
    /// provably carries nothing.
    fn unwind_child_launch(&mut self, index: usize) {
        let child = self.child_lifecycle.jobs[index].child;
        let child_id = child.to_string();
        // The pane, whether it is still the job's or already the app's.
        if let Some(session) = self.child_lifecycle.jobs[index].spawned.take() {
            session.kill();
        }
        if self.sessions.iter().any(|s| s.info.id == child) {
            let _ = self.stop_child_pane(child);
            self.sessions.retain(|s| s.info.id != child);
            self.sync_active_session_to_project();
        }
        // The proxy claim, released rather than committed.
        self.child_lifecycle.jobs[index].egress = None;
        crate::sandbox::egress::stop(&child_id);
        // The directories, which are friring's own and named by the child's id.
        crate::sandbox::cleanup_session(&child_id);
        crate::paths::remove_session_signal_dir(&child_id);
        // The worktree and the branch. A resume relaunches an existing child, so
        // its worktree is not this attempt's to remove.
        if self.child_lifecycle.jobs[index].goal == Goal::Resume {
            return;
        }
        let job = &self.child_lifecycle.jobs[index];
        // Nothing of the repository's is this attempt's unless it won the branch
        // — the loser of a cross-instance race recorded the *winner's* directory
        // against its own saga, and removing it is the one mistake that destroys
        // work nobody can recover.
        if !job.branch_claimed {
            return;
        }
        let repo = job.plan.repo_root.clone();
        let branch = job.plan.branch.clone();
        let worktree = job.worktree_path.clone();
        let base_head = job.base_head.clone();
        self.reclaim_child_worktree(&repo, worktree.as_deref(), &branch, base_head.as_deref());
    }

    /// Remove a worktree and its branch, but only when neither carries work.
    ///
    /// Ownership is established **twice**, because the branch and the path are
    /// two different claims. `ChildSaga::branch_claimed` says the *ref* is this
    /// saga's, and every caller must have it — see
    /// [`Self::reconcile_saga_worktree`]. It does not say the *directory* is:
    /// `worktree_segments` maps `/` to `-`, so `feat/one` and `feat-one` resolve
    /// to one path, and the loser of that collision recorded the winner's
    /// worktree against its own failed saga. So this asks git which branch the
    /// directory is actually on, and removes it only on its own.
    ///
    /// On top of both sits the independent question: not "is it mine" but "does
    /// it hold anything". Every condition is conservative on purpose: a worktree
    /// with uncommitted changes and a branch with commits are somebody's work,
    /// and a saga that failed is not evidence that the work in it is worthless.
    ///
    /// A claimed branch with **no** worktree is the ordinary shape of a
    /// `git worktree add` that failed after the ref was created, so the branch
    /// is reclaimed on its own rather than left behind.
    fn reclaim_child_worktree(
        &mut self,
        repo: &std::path::Path,
        worktree: Option<&std::path::Path>,
        branch: &str,
        base_head: Option<&str>,
    ) {
        let effects = Arc::clone(&self.child_lifecycle.effects);
        if let Some(worktree) = worktree.filter(|path| path.exists()) {
            // Whose directory this is, asked of git rather than inferred from
            // the plan. A claimed branch proves the *ref* is this attempt's; it
            // does not prove the *path*, because `worktree_segments` maps `/` to
            // `-` and two agent-chosen names — `feat/one` and `feat-one` — land
            // on one directory. The loser of that collision recorded the
            // winner's worktree against its own failed saga, and removing it
            // would destroy a healthy child's workspace.
            match effects.worktree_is_on(repo, worktree, branch) {
                Some(true) => {}
                mine => {
                    self.set_error(match mine {
                        Some(_) => format!(
                            "A child's launch failed, but {} is another launch's worktree rather \
                             than a worktree of its own branch '{branch}' — friring left it alone",
                            worktree.display()
                        ),
                        None => format!(
                            "A child's launch failed and git would not say which branch {} is on \
                             — friring left it alone",
                            worktree.display()
                        ),
                    });
                    // The branch is still this attempt's, and reclaiming it is
                    // what stops a collision leaking a ref per failed attempt.
                    return self.reclaim_child_branch(repo, branch, base_head);
                }
            }
            let verdict = effects.verify_worktree(worktree, base_head);
            if verdict.dirty {
                self.set_error(format!(
                    "A child's launch failed and left uncommitted work in {} — friring did not \
                     remove it",
                    worktree.display()
                ));
                return;
            }
            if let Err(e) = effects.remove_worktree(repo, worktree) {
                tracing::warn!("bridge: could not remove {}: {e}", worktree.display());
                return;
            }
        }
        self.reclaim_child_branch(repo, branch, base_head);
    }

    /// Delete a branch this saga cut, once it is known to carry nothing.
    ///
    /// Deleted only on a positive answer of zero. `None` is "git would not say",
    /// which is not the same as "there is nothing there".
    ///
    /// No recorded base is not the same either, and it has one honest reading:
    /// `base_head` is stamped from the created worktree, so its absence means no
    /// worktree was ever made and therefore nothing was ever committed on this
    /// branch. `delete_branch` is `git branch -d`, whose own merged-check is the
    /// second opinion — a branch git will not delete stays.
    fn reclaim_child_branch(
        &mut self,
        repo: &std::path::Path,
        branch: &str,
        base_head: Option<&str>,
    ) {
        let effects = Arc::clone(&self.child_lifecycle.effects);
        match base_head {
            Some(base) if effects.commits_ahead(repo, base, branch) == Some(0) => {
                let _ = effects.delete_branch(repo, branch);
            }
            Some(_) => {}
            None => {
                let _ = effects.delete_branch(repo, branch);
            }
        }
    }

    // ── Small helpers over the app ───────────────────────────────────────

    /// Update this job's saga row through a closure, reporting whether the write
    /// landed.
    ///
    /// The return value is what makes the module header's promise true — "every
    /// branch below names a directory, a branch, a pane id and a pid that the
    /// saga wrote down **before** it made them". A step that recorded an
    /// identity and then made the effect anyway, having failed to write it,
    /// would leave a worktree or a gated pane that recovery cannot name and will
    /// not reconcile.
    ///
    /// So a **pre-effect** call site fails the job on `Err`
    /// ([`Self::record_saga_or_fail`]), while a post-effect one only warns: by
    /// then the effect exists, and failing would lose the very thing the row
    /// describes.
    ///
    /// A job with no request key (a quiesce the child started) has no row to
    /// write and reports success — there is nothing being promised.
    #[must_use = "a pre-effect record must fail the job when it does not land"]
    fn record_saga(
        &mut self,
        index: usize,
        edit: impl FnOnce(&mut crate::session::ChildSaga),
    ) -> Result<(), String> {
        let Some(saga) = self.saga_row(index, edit) else {
            return Ok(());
        };
        self.db.upsert_child_saga(&saga).map_err(|e| {
            tracing::warn!("bridge: could not record a saga step: {e}");
            e.to_string()
        })
    }

    /// The row [`Self::record_saga`] would write, without writing it.
    ///
    /// Split out for S6, which hands the row to `commit_bridge_child` so the
    /// `committed` step lands inside the same transaction as the rows it
    /// describes. `None` for a job with no request key — nothing journals it.
    fn saga_row(
        &self,
        index: usize,
        edit: impl FnOnce(&mut crate::session::ChildSaga),
    ) -> Option<crate::session::ChildSaga> {
        let job = &self.child_lifecycle.jobs[index];
        let owner = job.owner.to_string();
        let key = job.key.as_ref().map(|k| k.as_str().to_string())?;
        let mut saga =
            self.db
                .child_saga(&owner, &key)
                .ok()
                .flatten()
                .unwrap_or(crate::session::ChildSaga {
                    owner_id: owner.clone(),
                    key: key.clone(),
                    child_id: Some(job.child.to_string()),
                    ..crate::session::ChildSaga::default()
                });
        saga.lease_until = Some(
            crate::sync::state::current_time_millis() + super::bridge_spawn::SAGA_LEASE_MILLIS,
        );
        edit(&mut saga);
        Some(saga)
    }

    /// [`Self::record_saga`] for a **pre-effect** step: fail the job rather than
    /// make an effect friring did not manage to write down.
    ///
    /// Returns whether the job is finished with, so a call site reads
    /// `if self.record_saga_or_fail(..) { return true }`.
    #[must_use]
    fn record_saga_or_fail(
        &mut self,
        index: usize,
        edit: impl FnOnce(&mut crate::session::ChildSaga),
    ) -> bool {
        match self.record_saga(index, edit) {
            Ok(()) => false,
            Err(detail) => self.fail_child_job(
                index,
                ErrorCode::Failed,
                format!(
                    "friring could not record this step before carrying it out, and will not \
                     carry out what it cannot recover: {detail}"
                ),
            ),
        }
    }

    /// [`Self::record_saga`] for a **post-effect** step: the effect already
    /// exists, so a failed write is logged and the job carries on.
    pub(super) fn record_saga_after(
        &mut self,
        index: usize,
        edit: impl FnOnce(&mut crate::session::ChildSaga),
    ) {
        let _ = self.record_saga(index, edit);
    }

    /// Kill the exact pane friring recorded for a child, or say why not.
    ///
    /// The revalidation is the whole safety property: a pane id is reused after
    /// a server restart, so killing by id alone would eventually kill somebody
    /// else's window.
    fn stop_child_pane(&mut self, child: SessionId) -> Result<(), String> {
        let Some(session) = self.sessions.iter().find(|s| s.info.id == child) else {
            // Nothing to stop. A child whose pane is already gone is stopped,
            // and the verification that follows reads the same worktree.
            return Ok(());
        };
        session
            .revalidate_identity()
            .map_err(|e| format!("{e:#}"))?;
        session.kill_checked().map_err(|e| format!("{e:#}"))
    }

    /// The worktree friring recorded for a child.
    fn child_worktree(&self, child_id: &str) -> Option<PathBuf> {
        self.db
            .child_saga_of_child(child_id)
            .ok()
            .flatten()
            .and_then(|saga| saga.worktree_path)
            .map(PathBuf::from)
            .or_else(|| {
                child_id
                    .parse::<SessionId>()
                    .ok()
                    .and_then(|id| self.sessions.iter().find(|s| s.info.id == id))
                    .and_then(|s| s.info.cwd.clone())
            })
    }

    /// The commit a child's branch was cut from.
    fn child_base_head(&self, child_id: &str) -> Option<String> {
        self.db
            .child_saga_of_child(child_id)
            .ok()
            .flatten()
            .and_then(|saga| saga.base_head)
    }

    /// The row S6 writes for a child, built the same way every other session's
    /// is.
    fn child_shared_session(
        &self,
        session: &crate::agent::backend::Session,
    ) -> crate::sync::SharedSession {
        self.session_to_shared(session)
    }
}

impl App {
    // ── Recovery ─────────────────────────────────────────────────────────

    /// Reconcile every saga a previous run left unfinished.
    ///
    /// Runs once, before the broker serves anything, so a `create` arriving on
    /// the first tick cannot race a reconciliation of the child it replays.
    /// **Only sagas nobody is driving**: a lease this instance does not hold and
    /// has not expired belongs to another running friring, and adopting it would
    /// be two instances reconciling one child.
    pub(crate) fn recover_child_sagas(&mut self) {
        let now = crate::sync::state::current_time_millis();
        let instance = self.bridge_instance_id();
        for saga in self.db.unfinished_child_sagas().unwrap_or_default() {
            let mine = saga.instance_id.as_deref() == Some(instance.as_str());
            let leased = saga.lease_until.is_some_and(|until| until > now);
            if leased && !mine {
                continue;
            }
            self.reconcile_child_saga(saga);
        }
        // A child left mid-quiesce: the fact that its agent was told to stop is
        // persisted precisely so a crash here does not lose it.
        for child_id in self
            .db
            .bridge_children_in_state(ChildState::Finishing)
            .unwrap_or_default()
        {
            let Ok(child) = child_id.parse::<SessionId>() else {
                continue;
            };
            if self.job_index_for_child(child).is_some() {
                continue;
            }
            // The intent's own outcome is gone with the process; the recorded
            // verdict is not yet written. `failed` is the narrow reading — a
            // quiesce friring did not finish never reports work complete.
            self.accept_finish_intent(child, Outcome::Failed, None);
        }
        self.answer_abandoned_lifecycle_requests();
    }

    /// Answer the lifecycle requests this instance's predecessor died holding.
    ///
    /// A deferred verb leaves its journal row `accepted` and writes no response,
    /// and a replay of an accepted key **waits** rather than acting twice — so a
    /// caller whose request died with the process would poll for ever. The
    /// launch verbs are covered by their saga rows, which the loop above
    /// reconciles and answers. `stop` has no saga row (`Goal::is_launch`
    /// excludes it, and the durable record of a quiesce is
    /// [`ChildState::Finishing`] itself), so this is what closes it out.
    ///
    /// A typed refusal rather than silence: the child's state is on record and
    /// the caller can ask for it, so "friring restarted, ask again" is something
    /// it can act on. Only rows nothing is now carrying are answered, so a
    /// request this instance has already picked up is left alone.
    fn answer_abandoned_lifecycle_requests(&mut self) {
        let started_at = self.child_lifecycle.started_at;
        for row in self.db.accepted_bridge_requests().unwrap_or_default() {
            // Only rows older than this instance. The broker runs in `tick_core`
            // and this driver in `tick_background`, so on the first tick a
            // request this instance has just accepted is already in the journal
            // — and refusing that one would break the verb rather than recover
            // it.
            if row.created_at >= started_at {
                continue;
            }
            let carried = self.child_lifecycle.jobs.iter().any(|job| {
                job.key.as_ref().is_some_and(|k| k.as_str() == row.key)
                    || job.waiters.iter().any(|k| k.as_str() == row.key)
            });
            if carried {
                continue;
            }
            let Ok(key) = row.key.parse::<crate::session::bridge::RequestKey>() else {
                continue;
            };
            let response = Response::refused(
                key.clone(),
                ErrorCode::BrokerAbsent,
                format!(
                    "friring restarted while this '{}' was in flight, so it was not carried out. \
                     Ask for the child's status and send it again if it is still needed.",
                    row.verb
                ),
            );
            let encoded = serde_json::to_string(&response).unwrap_or_default();
            if let Err(e) = self.db.finish_bridge_request(
                &row.owner_id,
                key.as_str(),
                RequestState::Failed,
                &encoded,
            ) {
                tracing::warn!("bridge: could not close out '{key}': {e}");
                continue;
            }
            if let Err(e) =
                crate::paths::write_bridge_response(&row.owner_id, key.as_str(), &encoded)
            {
                tracing::warn!("bridge: could not write the answer to '{key}': {e}");
            }
        }
    }

    /// Reconcile one interrupted saga, by what its step positively names.
    fn reconcile_child_saga(&mut self, saga: crate::session::ChildSaga) {
        let step = saga.step.unwrap_or(SagaStep::Failed);
        let Some(child_id) = saga.child_id.clone() else {
            // A saga with no child named nothing external. Mark it and move on.
            self.mark_saga_failed(&saga, "this launch was interrupted before it named a child");
            return;
        };
        if step.is_committed() {
            self.adopt_recovered_child(&saga, &child_id);
            return;
        }
        // Below the committed line: the child is not a session, so everything
        // this saga made is removed and the request is failed.
        if step >= SagaStep::Pane {
            self.kill_recorded_child_pane(&saga);
        }
        if step >= SagaStep::Dirs {
            crate::sandbox::egress::stop(&child_id);
            crate::sandbox::cleanup_session(&child_id);
            crate::paths::remove_session_signal_dir(&child_id);
        }
        if step >= SagaStep::Worktree {
            self.reconcile_saga_worktree(&saga);
        }
        self.mark_saga_failed(&saga, "this launch was interrupted and has been reconciled");
    }

    /// Reclaim the worktree an interrupted saga made — and only one it *made*.
    ///
    /// The recorded `worktree_path` is where this saga was going to put a
    /// worktree, not evidence that it did: it is written before `git` runs, so
    /// the loser of a cross-instance race has the winner's directory recorded
    /// against its own failed saga. `branch_claimed` is the evidence, and it is
    /// only ever set by an attempt that atomically created the branch (see
    /// [`crate::git::claim_child_worktree`]).
    ///
    /// The one case it cannot answer is a crash *during* `git worktree add`,
    /// after the branch was claimed and before the tick could record it. That
    /// leaves the flag unset over a worktree that really is this saga's, so this
    /// declines to remove it and tells the operator where it is: a leaked
    /// directory can be removed by hand, and a wrongly deleted one cannot be
    /// brought back.
    fn reconcile_saga_worktree(&mut self, saga: &crate::session::ChildSaga) {
        let (Some(repo), Some(worktree), Some(branch)) = (
            self.saga_repo_root(saga),
            saga.worktree_path.as_deref().map(PathBuf::from),
            saga.branch.clone(),
        ) else {
            return;
        };
        if saga.branch_claimed {
            self.reclaim_child_worktree(&repo, Some(&worktree), &branch, saga.base_head.as_deref());
            return;
        }
        if worktree.exists() {
            self.set_error(format!(
                "An interrupted child launch may have left {} on branch '{branch}' — friring \
                 could not prove that launch created it, so it left it alone",
                worktree.display()
            ));
        }
    }

    /// Kill the pane a saga recorded, and only if it is still that pane.
    ///
    /// Safe to do without asking what the pane is running, because of where in
    /// the saga this is reached: below the committed line the gate was never
    /// released, so a pane whose whole recorded identity still matches is by
    /// construction the launch helper waiting on a gate that will never open.
    /// The helper's own timeout (ADR-33) is the backstop for a pane this cannot
    /// name.
    fn kill_recorded_child_pane(&mut self, saga: &crate::session::ChildSaga) {
        let expected = crate::session::MuxIdentity {
            server: saga.mux_server.clone(),
            window_id: saga.mux_window_id.clone(),
            pane_id: saga.mux_pane_id.clone(),
            pane_pid: saga.mux_pane_pid,
            launch_key: saga.mux_launch_key.clone(),
        };
        // A saga written before the marker was recorded, or by a server that
        // would not take a pane option, names no pane this may kill: the guard
        // is what keeps a recycled pane id from aiming the kill at somebody
        // else's window. The launch helper's own timeout is the backstop.
        if !expected.is_recorded() {
            return;
        }
        let backend = child_backend(self);
        if let Err(e) = crate::agent::backend::kill_recorded_pane(&backend, &expected) {
            tracing::warn!("bridge: {e:#}");
        }
    }

    /// A child whose rows exist: restore its egress and get it running again.
    ///
    /// Past the committed line the child is a real session with an ownership row
    /// and a mailbox, so nothing is removed. What is left is whether it is
    /// *running*: a friring that died between S6 and S8 left a pane waiting on a
    /// gate that has since timed out.
    fn adopt_recovered_child(&mut self, saga: &crate::session::ChildSaga, child_id: &str) {
        let Ok(child) = child_id.parse::<SessionId>() else {
            self.mark_saga_failed(saga, "this child's id is not readable");
            return;
        };
        self.restore_session_egress(child);
        // A finish intent this launch was holding outlives the process, and it
        // settles the child outright: the agent already reported, and the `send`
        // that carried it was answered `ok` — an answer a replay returns
        // verbatim, so nothing will bring it here a second time. Applied before
        // the running/stalled question below, which is about a child that has
        // *not* finished; without this the child is adopted with no verdict and
        // holds its owner's fan-out slot until someone stops it by hand.
        if let Some(outcome) = saga
            .finish_outcome
            .as_deref()
            .and_then(|held| held.parse::<Outcome>().ok())
        {
            self.mark_saga_done(saga);
            self.accept_finish_intent(child, outcome, saga.finish_message_id);
            return;
        }
        // Across a restart the gate-release moment is gone with the process, so
        // the question is the durable one the saga row can still answer: has
        // this child's agent reported **at any point since this child was
        // created**? A report older than that belongs to a previous session with
        // the same id. Deliberately not "since the gate opened": recovery runs
        // once, at startup, and a healthy child that simply has not reported in
        // the last instant must not be relaunched underneath itself.
        let in_list = self
            .sessions
            .iter()
            .any(|s| s.info.id == child && !s.is_placeholder());
        let running = in_list
            && hook_reported_since(self.cached_hook_states.get(&child), saga.created_at as i64);
        if running {
            let _ = self.db.set_bridge_child_state(child_id, ChildState::Ready);
            self.mark_saga_done(saga);
            return;
        }
        // Relaunched **once**. The rows themselves are the counter: a second
        // saga naming this child is what says the once is spent.
        let launches = self.db.child_saga_count(child_id).unwrap_or(0);
        if launches > 1 {
            let _ = self
                .db
                .set_bridge_child_state(child_id, ChildState::Unusable);
            if let Ok(owner) = saga.owner_id.parse::<SessionId>() {
                self.host_mail(owner, MailKind::ChildFailed, child_id);
            }
            self.mark_saga_failed(
                saga,
                "this child was relaunched once and is still not usable",
            );
            return;
        }
        let _ = self
            .db
            .set_bridge_child_state(child_id, ChildState::Stalled);
        self.mark_saga_failed(
            saga,
            "friring stopped while this child was starting; it is resumable",
        );
        if let Ok(owner) = saga.owner_id.parse::<SessionId>() {
            self.host_mail(owner, MailKind::ChildStalled, child_id);
        }
    }

    /// The repository a saga's child works in, from the child's own
    /// `session_repos` row.
    fn saga_repo_root(&self, saga: &crate::session::ChildSaga) -> Option<PathBuf> {
        let child_id = saga.child_id.as_deref()?;
        self.db
            .session_repo_roots(child_id)
            .ok()?
            .into_iter()
            .next()
            .map(PathBuf::from)
            .or_else(|| {
                // A saga that failed before S6 wrote no `session_repos` row, so
                // the owner's is the only recorded repository. It is the one the
                // `create` was checked against.
                self.db
                    .session_repo_roots(&saga.owner_id)
                    .ok()?
                    .into_iter()
                    .next()
                    .map(PathBuf::from)
            })
    }

    /// Mark a saga failed and journal the refusal a replay returns.
    fn mark_saga_failed(&mut self, saga: &crate::session::ChildSaga, detail: &str) {
        let mut row = saga.clone();
        row.step = Some(SagaStep::Failed);
        row.lease_until = None;
        if let Err(e) = self.db.upsert_child_saga(&row) {
            tracing::warn!("bridge: could not mark a saga failed: {e}");
        }
        let Ok(key) = crate::session::bridge::RequestKey::new(saga.key.clone()) else {
            return;
        };
        let response = Response::refused(key.clone(), ErrorCode::Failed, detail.to_string());
        let encoded = serde_json::to_string(&response).unwrap_or_default();
        let _ = self.db.finish_bridge_request(
            &saga.owner_id,
            key.as_str(),
            RequestState::Failed,
            &encoded,
        );
        let _ = crate::paths::write_bridge_response(&saga.owner_id, key.as_str(), &encoded);
    }

    /// Mark a saga done, and journal the answer a replay returns.
    fn mark_saga_done(&mut self, saga: &crate::session::ChildSaga) {
        let mut row = saga.clone();
        row.step = Some(SagaStep::Done);
        row.lease_until = None;
        if let Err(e) = self.db.upsert_child_saga(&row) {
            tracing::warn!("bridge: could not mark a saga done: {e}");
        }
        let Ok(key) = crate::session::bridge::RequestKey::new(saga.key.clone()) else {
            return;
        };
        let response = Response::ok(
            key.clone(),
            Some(serde_json::json!({
                "child_id": saga.child_id,
                "branch": saga.branch,
                "worktree_path": saga.worktree_path,
                "state": ChildState::Ready.as_str(),
            })),
        );
        let encoded = serde_json::to_string(&response).unwrap_or_default();
        let _ = self.db.finish_bridge_request(
            &saga.owner_id,
            key.as_str(),
            RequestState::Done,
            &encoded,
        );
        let _ = crate::paths::write_bridge_response(&saga.owner_id, key.as_str(), &encoded);
    }

    /// Where a child's job sits in the list, if it has one.
    pub(super) fn job_index_for_child(&self, child: SessionId) -> Option<usize> {
        self.child_lifecycle
            .jobs
            .iter()
            .position(|job| job.child == child)
    }
}

/// The thread-local overrides a test pinned, carried onto a worker thread.
///
/// Empty in a release build, and the compiler removes it. In a test it is the
/// difference between a blocking step resolving the harness's tempdir and it
/// resolving the developer's real `~/.local/share/friring` — see
/// [`crate::paths::test_dir_override`].
#[derive(Default)]
pub(super) struct TestContext {
    #[cfg(test)]
    paths: Option<PathBuf>,
    #[cfg(test)]
    host: Option<Arc<crate::sandbox::SandboxHost>>,
}

impl TestContext {
    /// Capture this thread's overrides.
    pub(super) fn capture() -> Self {
        #[cfg(test)]
        {
            Self {
                paths: crate::paths::test_dir_override(),
                host: crate::agent::sandboxing::TestSandboxHost::installed(),
            }
        }
        #[cfg(not(test))]
        Self::default()
    }

    /// Install them on the thread this is called from, until the guards drop.
    pub(super) fn install(self) -> TestContextGuard {
        #[cfg(test)]
        {
            TestContextGuard {
                _paths: self.paths.map(crate::paths::TestPathGuard::new),
                _host: self
                    .host
                    .map(crate::agent::sandboxing::TestSandboxHost::install),
            }
        }
        #[cfg(not(test))]
        TestContextGuard {}
    }
}

/// What [`TestContext::install`] returns. Nothing, outside a test.
pub(super) struct TestContextGuard {
    #[cfg(test)]
    _paths: Option<crate::paths::TestPathGuard>,
    #[cfg(test)]
    _host: Option<crate::agent::sandboxing::TestSandboxHost>,
}

/// What a worker sent back.
enum Received {
    Worktree(Result<WorktreeFacts, crate::git::ClaimFailure>),
    Pane(Box<Result<crate::agent::backend::GatedSession, String>>),
    Verify(crate::git::WorktreeVerdict),
    /// The worker dropped its sender without answering.
    Died,
}

/// What a child's branch is cut from: the repository's default branch, or its
/// current `HEAD` when there is no default to name.
///
/// Two git subprocesses, so it is called from the worktree worker rather than
/// from the tick that starts it.
fn child_base_branch(repo: &std::path::Path, branch: &str) -> String {
    let branches = crate::git::list_branches(repo).unwrap_or_default();
    crate::git::default_branch(repo, &branches)
        .filter(|base| base != branch)
        .unwrap_or_else(|| "HEAD".to_string())
}

/// Write the release file into a child's gate, staged and renamed (ADR-33).
///
/// `rename(2)` because a gate directory is read-only inside the boundary: the
/// file has to arrive whole, from a source the sandbox cannot see, or an agent
/// could write the key into a partially-written file and release its own gate.
fn release_gate(child_key: &str, gate_key: &str) -> Result<(), String> {
    let staging = crate::sandbox::dirs::create_gate_staging_dir().map_err(|e| e.to_string())?;
    let target = crate::sandbox::dirs::gate_release_file(child_key)
        .ok_or_else(|| "friring has no data directory for a launch gate".to_string())?;
    let staged = staging.join(format!("{child_key}.tmp"));
    let _ = std::fs::remove_file(&staged);
    std::fs::write(&staged, gate_key).map_err(|e| format!("{}: {e}", staged.display()))?;
    std::fs::rename(&staged, &target).map_err(|e| format!("{}: {e}", target.display()))
}

/// The key a response carries when a job has none of its own.
///
/// A quiesce a *child* started answers no request, so nothing is written for it
/// — but the response type needs a key, and this one is never journaled.
fn placeholder_key() -> crate::session::bridge::RequestKey {
    crate::session::bridge::RequestKey::new("00000000")
        .expect("a fixed eight-character key is valid")
}
