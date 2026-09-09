//! The child lifecycle: `create`, `stop` and `resume` (ADR-32).
//!
//! Where an authorized lifecycle request enters the spawn saga. Everything the
//! saga does outside SQLite is recorded **before** it is done, so recovery
//! reconciles by exact identity and never by a prefix scan or by "kill what has
//! no row".
//!
//! # Why a saga and not a function
//!
//! Creating one child is a worktree checkout, five minted directories, a state
//! seeding, a proxy bind, a multiplexer window, a database transaction, an
//! acknowledgement, a `rename(2)` and a wait for the child's own hook. Each of
//! those can fail, and friring can be killed between any two of them. A function
//! that did them in order would leave, on the failures alone, an orphaned
//! worktree, a listener nothing is behind, or a pane running an agent with no
//! session row — and on a crash it would leave no record of which.
//!
//! So each step's identity is written to `child_sagas` **first** and the effect
//! is made second. [`SagaStep::is_committed`] is the line recovery turns on:
//! before it the saga's effects are removed and the request is failed, after it
//! the child is a real session that is adopted and carried on.
//!
//! # Why the agent is gated
//!
//! The pane exists at S5 and the agent does not start until S8. That window is
//! what makes every step between them able to fail *without the agent ever
//! having run*: no turn to interrupt, no file written, no token spent. The gate
//! (ADR-33) is a file the host renames into a read-only directory, so the one
//! thing that can open it is the host.
//!
//! # It must never starve the render loop
//!
//! Three steps block: the worktree checkout, the window spawn and the
//! post-stop `git` inspection. Each runs on a blocking task and is polled from
//! the tick, exactly as [`crate::app::App::poll_session_spawn`] does (ADR-3).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent::backend::{GatedSession, Session, SessionBackend};
use crate::agent::sandboxing::BridgeLaunch;
use crate::sandbox::child_state::SeedPlan;
use crate::session::bridge::MailKind;
use crate::session::bridge::{
    CreateBody, ErrorCode, Request, RequestKey, Response, ResumeBody, StopBody, Verb,
};
use crate::session::{
    ChildSaga, ChildState, Outcome, SagaStep, SandboxOverlay, SandboxProfile, SessionId,
};

use super::App;

/// How long S7 waits for the egress supervisor to acknowledge the commit.
///
/// A commit is a message to the supervisor thread, so it returns before anything
/// has agreed to filter. A supervisor that never answers leaves the record
/// `Preparing`, and this saga will not release a gate on that: an agent that
/// believes it is filtered and is not is the one outcome worse than a failed
/// `create`.
pub(super) const EGRESS_ACK_TIMEOUT: Duration = Duration::from_secs(15);

/// How long S9 waits for the child's own hook to report.
///
/// The hook report is the **only** accepted proof of readiness, because it is
/// what shows the agent is running from the relocated private state directory
/// (ADR-31). A live pane process shows nothing of the sort, so the pane-pid
/// fallback ordinary sessions use is not accepted here.
pub(super) const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a `stop` waits for the child to send a finish intent of its own
/// before the host stops it anyway.
const DEFAULT_GRACE: Duration = Duration::from_secs(120);

/// How many lifecycle jobs one instance drives at once.
///
/// A fan-out cap is per owner; this is per friring, and it bounds the blocking
/// tasks a burst of `create`s can put on the runtime.
const MAX_CONCURRENT_JOBS: usize = 4;

/// How long a saga's lease lasts before another instance may reconcile it.
///
/// Longer than any single step, so an instance that is merely slow does not have
/// its child adopted out from under it. The step it must outlive is
/// `bridge_saga::BLOCKING_STEP_TIMEOUT` (600s), the ceiling on a worktree
/// checkout — named here because the lease is only renewed at step transitions,
/// never while a worker is pending, so a lease shorter than that ceiling would
/// let a second instance reclaim the worktree of a saga that is still working.
pub(super) const SAGA_LEASE_MILLIS: u64 = 660_000;

// ── The seam every external effect goes through ──────────────────────────

/// What the child lifecycle does outside SQLite, behind one trait.
///
/// The saga's whole contract is about what survives a failure at each step, and
/// that is only testable if each step can be *made* to fail. `git` refusing a
/// worktree, a supervisor that never acknowledges, a seeding that cannot be
/// carried out: each is one method here, so an acceptance test scripts the
/// failure rather than arranging for the real thing to break.
///
/// The multiplexer is deliberately **not** here: the backend trait is already
/// that seam, and a second one would let a test pass against a fake while the
/// real spawn path drifted.
pub(crate) trait ChildEffects: Send + Sync {
    /// S2 — claim the child's branch and put its worktree on it.
    ///
    /// The failure carries **whether the branch was claimed**, which is what
    /// decides whether the unwind may touch the planned path at all — see
    /// [`crate::git::claim_child_worktree`].
    fn create_worktree(
        &self,
        repo: &Path,
        branch: &str,
        base: &str,
    ) -> Result<PathBuf, crate::git::ClaimFailure>;

    /// The commit a branch was cut from, recorded so recovery can ask whether
    /// the branch carries work before deleting it.
    fn head_commit(&self, cwd: &Path) -> Option<String>;

    /// Quiesce step 4 — what the host found after the pane stopped.
    fn verify_worktree(
        &self,
        worktree: &Path,
        base_head: Option<&str>,
    ) -> crate::git::WorktreeVerdict;

    /// Recovery — does git say the directory at `worktree` is on `branch`?
    ///
    /// Asked before any removal, because a claimed *branch* is not a claimed
    /// *path*: two branch names can sanitize to one directory, so the loser of
    /// that collision has the winner's worktree recorded against its own saga.
    /// Three-valued on purpose — `None` is "git would not say" (not a registered
    /// worktree, detached, or the command failed), which is a reason to leave
    /// the directory alone rather than to remove it.
    fn worktree_is_on(&self, repo: &Path, worktree: &Path, branch: &str) -> Option<bool>;

    /// Recovery — remove a worktree this saga made.
    fn remove_worktree(&self, repo: &Path, worktree: &Path) -> Result<(), String>;

    /// Recovery — delete a branch this saga cut, once it is known to be empty.
    fn delete_branch(&self, repo: &Path, branch: &str) -> Result<(), String>;

    /// How many commits `<base>..<tip>` carries. `None` is never read as zero.
    fn commits_ahead(&self, repo: &Path, base: &str, tip: &str) -> Option<u32>;

    /// S7 — whether the egress supervisor holds this session's committed
    /// instance.
    fn egress_acknowledged(&self, session_key: &str) -> bool;

    /// S3 — fill the child's private state directory.
    fn seed_child_state(&self, plan: &SeedPlan) -> Result<(), String>;
}

/// The real effects: `git`, the egress supervisor, the filesystem.
pub(crate) struct HostEffects;

impl ChildEffects for HostEffects {
    fn create_worktree(
        &self,
        repo: &Path,
        branch: &str,
        base: &str,
    ) -> Result<PathBuf, crate::git::ClaimFailure> {
        // Never `create_or_attach_worktree`. Attaching hands back an existing
        // directory, so two creates naming one branch would share a workspace
        // and a branch, and the host's per-branch verification could no longer
        // attribute the work. The `git branch` inside this is the guard the
        // in-flight scan in `validate_create` cannot be: that scan sees only
        // *this* process's jobs, and friring supports several instances on one
        // database (ADR-7b). Exactly one ref creation wins; the loser fails its
        // create loudly, with nothing shared and nothing of the winner's touched.
        crate::git::claim_child_worktree(repo, branch, base)
    }

    fn head_commit(&self, cwd: &Path) -> Option<String> {
        crate::git::head_commit(cwd)
    }

    fn verify_worktree(
        &self,
        worktree: &Path,
        base_head: Option<&str>,
    ) -> crate::git::WorktreeVerdict {
        crate::git::verify_worktree(worktree, base_head)
    }

    fn worktree_is_on(&self, repo: &Path, worktree: &Path, branch: &str) -> Option<bool> {
        let on = crate::git::worktree_branch_at(repo, worktree)?;
        // The other branch's name goes to the log rather than into the boolean:
        // the caller's banner names the path, and an operator chasing a
        // collision wants to know which launch actually holds it.
        if on != branch {
            tracing::warn!(
                "bridge: {} is a worktree of '{on}', not of this launch's '{branch}'",
                worktree.display()
            );
        }
        Some(on == branch)
    }

    fn remove_worktree(&self, repo: &Path, worktree: &Path) -> Result<(), String> {
        crate::git::remove_worktree(repo, worktree).map_err(|e| format!("{e:#}"))
    }

    fn delete_branch(&self, repo: &Path, branch: &str) -> Result<(), String> {
        crate::git::delete_branch(repo, branch).map_err(|e| format!("{e:#}"))
    }

    fn commits_ahead(&self, repo: &Path, base: &str, tip: &str) -> Option<u32> {
        crate::git::commits_ahead(repo, base, tip)
    }

    fn egress_acknowledged(&self, session_key: &str) -> bool {
        crate::agent::sandboxing::egress_acknowledged(session_key)
    }

    fn seed_child_state(&self, plan: &SeedPlan) -> Result<(), String> {
        plan.apply()
    }
}

// ── What the broker gets back ────────────────────────────────────────────

/// What a lifecycle verb answers with.
///
/// A `create` is a worktree, a window and a wait for a hook: seconds at best,
/// and up to [`READY_TIMEOUT`] at worst. It cannot be answered on the tick that
/// accepted it, so the journal entry stays `accepted`, **no response file is
/// written**, and the saga writes both when it finishes. The client is already
/// polling the response directory, so it simply waits.
pub(crate) enum LifecycleAnswer {
    /// Answer now, and journal it now.
    Now(Box<Response>),
    /// Accepted. The saga answers when it reaches a final step.
    Accepted,
}

impl LifecycleAnswer {
    fn refused(key: RequestKey, error: ErrorCode, message: impl Into<String>) -> Self {
        Self::Now(Box::new(Response::refused(key, error, message)))
    }
}

// ── The driver's state ───────────────────────────────────────────────────

/// Every lifecycle job this instance is driving.
pub(crate) struct ChildLifecycle {
    pub(super) jobs: Vec<Job>,
    /// Whether startup reconciliation has run. Once, before requests are served.
    pub(super) recovered: bool,
    /// Epoch ms this instance came up, stamped before any request can be taken.
    ///
    /// Recovery closes out journal rows its predecessor died holding, and this
    /// is what tells those apart from a request *this* instance accepted moments
    /// ago: the broker runs in `tick_core` and the saga driver in
    /// `tick_background`, so on the very first tick a fresh request exists
    /// before reconciliation runs.
    pub(super) started_at: u64,
    /// The seam above, swappable in a test.
    pub(super) effects: Arc<dyn ChildEffects>,
}

impl Default for ChildLifecycle {
    fn default() -> Self {
        Self {
            jobs: Vec::new(),
            recovered: false,
            started_at: crate::sync::state::current_time_millis(),
            effects: Arc::new(HostEffects),
        }
    }
}

impl std::fmt::Debug for ChildLifecycle {
    /// Hand-written: the effects seam is a trait object with nothing to render,
    /// and the jobs carry a live `Session`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildLifecycle")
            .field("jobs", &self.jobs.len())
            .field("recovered", &self.recovered)
            .finish()
    }
}

impl ChildLifecycle {
    /// Replace the effects seam — the failure-injection entry point.
    #[cfg(test)]
    pub(crate) fn with_effects(effects: Arc<dyn ChildEffects>) -> Self {
        Self {
            effects,
            ..Self::default()
        }
    }

    /// How many jobs are in flight — what a test drives a saga against.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.jobs.len()
    }
}

/// Which lifecycle a job is carrying out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Goal {
    /// A fresh child: S1 through S9.
    Create,
    /// The same `child_id` relaunched, keeping its ownership and its worktree.
    Resume,
    /// The owner asked for a stop: `cancel`, then the quiesce, landing in
    /// [`ChildState::Stopped`].
    Stop,
    /// The child asked to finish: the quiesce, landing in the outcome's own
    /// terminal state.
    Finish,
}

impl Goal {
    /// Whether this goal ends in a launched agent (and so writes a saga row).
    pub(super) fn is_launch(self) -> bool {
        matches!(self, Self::Create | Self::Resume)
    }
}

/// Where a job is. The launch steps mirror [`SagaStep`]; the quiesce steps have
/// no saga row because the durable record of a quiesce is
/// [`ChildState::Finishing`] itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// S1 done: the child is named and the saga row exists.
    Named,
    /// S2 running on a blocking task.
    WorktreeRunning,
    /// S2 done, S3 next.
    Worktree,
    /// S3 done (dirs minted and seeded), S4/S5 next.
    Dirs,
    /// S5 running on a blocking task.
    PaneRunning,
    /// S5 done: a gated pane exists at the recorded identity.
    Pane,
    /// S6 done: the transaction committed.
    Committed,
    /// S7 done: the supervisor acknowledged.
    EgressLive,
    /// S8 done: the gate is open and the agent is running.
    Released,
    /// A `stop` is waiting for the child's own finish intent, or for the grace
    /// period to run out.
    Grace,
    /// The finish intent is accepted and the host's `ack` has been sent.
    Acked,
    /// The post-stop inspection is running on a blocking task.
    VerifyRunning,
}

/// The blocking work a job is waiting on.
pub(super) enum Pending {
    Worktree(mpsc::Receiver<Result<WorktreeFacts, crate::git::ClaimFailure>>),
    Pane(Box<mpsc::Receiver<Result<GatedSession, String>>>),
    Verify(mpsc::Receiver<crate::git::WorktreeVerdict>),
}

/// What S2 found out.
pub(super) struct WorktreeFacts {
    pub(super) path: PathBuf,
    pub(super) base_head: Option<String>,
}

/// One lifecycle in progress.
pub(super) struct Job {
    pub(super) owner: SessionId,
    /// The request this job answers. A quiesce the *child* started has none.
    pub(super) key: Option<RequestKey>,
    pub(super) child: SessionId,
    /// The child's session name, minted at S1.
    pub(super) name: String,
    pub(super) goal: Goal,
    pub(super) step: Step,
    /// When the current step started, for its timeout.
    pub(super) since: Instant,
    pub(super) pending: Option<Pending>,
    /// What `create` was asked for. Empty for a quiesce.
    pub(super) plan: LaunchPlan,
    /// The gate key this launch waits on.
    pub(super) gate_key: String,
    /// The finish intent being carried out, for a quiesce.
    pub(super) intent: Option<FinishIntent>,
    /// The proxy claim, held from S5 until S7 commits it. Dropping the job
    /// releases it.
    pub(super) egress: Option<crate::sandbox::egress::PendingEgress>,
    /// The child's own session, from S5 until S6 moves it into the app.
    pub(super) spawned: Option<Box<Session>>,
    /// The directories minted at S3, and the narrowing built over them.
    pub(super) dirs: Option<ChildDirs>,
    pub(super) overlay: Option<SandboxOverlay>,
    /// Recorded facts, mirrored from the saga row so the job need not re-read
    /// it every tick.
    pub(super) worktree_path: Option<PathBuf>,
    pub(super) base_head: Option<String>,
    /// Whether S2's own `git branch` created this child's branch.
    ///
    /// The unwind's licence to touch anything at [`Self::worktree_path`] or to
    /// delete the branch: a launch that lost the ref to another instance made
    /// none of it. Mirrors `ChildSaga::branch_claimed` — see
    /// [`crate::git::claim_child_worktree`].
    pub(super) branch_claimed: bool,
    /// Further request keys accepted against this job while it was running.
    ///
    /// A second `stop` of a child already quiescing, or a second `resume` of a
    /// child already relaunching, is journaled `accepted` and answered by
    /// nothing of its own — and a replay of that key waits rather than acting
    /// twice, so without this the caller waits for ever. Each waiter gets this
    /// job's own result, under its own key.
    ///
    /// **Only a request this job's own outcome answers.** A `stop` may not
    /// attach to a *launch*: that job's success says the child is `ready`, which
    /// is the opposite of what the caller asked for. Those go to
    /// [`Self::follow_up`] instead.
    pub(super) waiters: Vec<RequestKey>,
    /// What must happen once this job ends, because it could not happen while it
    /// ran.
    ///
    /// Two things land on a launch that has not reached S9 and neither may be
    /// dropped or answered from the launch's own result — see [`FollowUp`].
    pub(super) follow_up: Option<FollowUp>,
    /// Epoch ms the gate was opened, which is what makes S9's proof a proof:
    /// only a hook report stamped at or after it belongs to this launch. `None`
    /// until S8 — see [`hook_reported_since`].
    pub(super) gate_opened_at: Option<i64>,
    /// Set once the child's pane has been stopped, so a retry does not kill
    /// twice.
    pub(super) stopped: bool,
    /// Driver passes still owed between the host's `ack` and the kill — see
    /// `ACK_SETTLE_PASSES`.
    pub(super) settle: u8,
}

/// What a `create` asked for, validated.
#[derive(Debug, Clone, Default)]
pub(super) struct LaunchPlan {
    pub(super) repo_root: PathBuf,
    pub(super) branch: String,
    pub(super) agent: String,
    pub(super) task_kind: String,
    pub(super) task_body: String,
    pub(super) role_hint: Option<String>,
    /// The owner's profile name, which the child launches under a narrowed copy
    /// of.
    pub(super) profile: String,
}

/// Work that arrived while a launch was still running, held until it ends.
///
/// A child being created is not a child that can be stopped: there may be no
/// pane yet, the gate may not be open, and the worktree the quiesce inspects may
/// not exist. So the two things that can arrive mid-launch are held here and
/// become a real quiesce the moment the launch job is done with:
///
/// - **A `stop`.** Journaled `accepted`, and a replay of an accepted key waits
///   rather than acting, so dropping it hangs the caller for good. Answering it
///   from the launch's own response is worse: it would tell the caller
///   quiescence completed, with `state: "ready"`, over a child nothing stopped.
/// - **The child's own `result`.** ADR-32 makes it the one finish intent, and
///   the broker has already answered the `send` that carried it with an `ok` and
///   a `message_id`. A worker small enough to finish before its first hook report
///   is the ordinary case, not an edge one — dropped, it would sit `ready` for
///   ever, holding a fan-out slot with no verdict ever written.
#[derive(Debug, Default)]
pub(super) struct FollowUp {
    /// `stop` keys waiting on the quiesce this becomes. The first is the job's
    /// key and the rest its waiters, so every one is answered.
    pub(super) stop_keys: Vec<RequestKey>,
    /// The shortest grace any of those `stop`s asked for.
    pub(super) grace: Option<Duration>,
    /// The child's own finish intent, if it arrived before the launch finished.
    pub(super) intent: Option<FinishIntent>,
    /// Whether that intent arrived **before** the first `stop` did.
    ///
    /// The live path resolves this pair by arrival order: a `result` first
    /// creates the quiesce and a later `stop` joins it as a waiter, so the
    /// child's own outcome is what both callers are answered from; a `stop`
    /// first wins over a later result, deliberately. A held pair has no such
    /// order unless it is recorded, and without it the deferred path always
    /// behaved as if the stop came first — which turned a child that reported
    /// `completed` mid-launch into `failed`/`stopped`.
    pub(super) intent_first: bool,
}

/// The finish intent a quiesce is carrying out.
#[derive(Debug, Clone)]
pub(super) struct FinishIntent {
    pub(super) outcome: Outcome,
    /// The mail row it arrived on, for provenance.
    pub(super) message_id: Option<i64>,
    /// The terminal state a clean, stopped worktree reaches. `stop` overrides
    /// the outcome's own.
    pub(super) clean_state: ChildState,
}

impl App {
    // ── Entry from the broker ────────────────────────────────────────────

    /// Run one authorized `create`, `stop` or `resume`.
    ///
    /// The verb is already authorized when this is reached: the broker has
    /// checked the caller's grant and the depth rule. What is left is the
    /// validation that needs the request's own arguments — the repository, the
    /// agent, the fan-out, the dependencies — and then the saga.
    pub(crate) fn run_child_lifecycle(
        &mut self,
        caller: SessionId,
        request: &Request,
    ) -> LifecycleAnswer {
        let key = request.key.clone();
        if self.child_lifecycle.jobs.len() >= MAX_CONCURRENT_JOBS {
            return LifecycleAnswer::refused(
                key,
                ErrorCode::Quota,
                format!(
                    "friring is already carrying out {MAX_CONCURRENT_JOBS} child lifecycle \
                     requests; retry with the same key"
                ),
            );
        }
        // Read against the **verb**, not against whichever untagged arm serde
        // matched: `{"child": "c1"}` is a well-formed `stop` and a well-formed
        // `resume`, so matching on the arm would let the enum's declaration
        // order decide what a request meant. See [`Request::body_as`].
        let refused =
            |detail: String| LifecycleAnswer::refused(key.clone(), ErrorCode::Failed, detail);
        match request.verb {
            Verb::Create => match request.body_as::<CreateBody>() {
                Ok(body) => self.begin_create(caller, key, &body),
                Err(detail) => refused(detail),
            },
            Verb::Stop => match request.body_as::<StopBody>() {
                Ok(body) => self.begin_stop(caller, key, &body),
                Err(detail) => refused(detail),
            },
            Verb::Resume => match request.body_as::<ResumeBody>() {
                Ok(body) => self.begin_resume(caller, key, &body),
                Err(detail) => refused(detail),
            },
            verb => refused(format!("'{verb}' is not a lifecycle verb")),
        }
    }

    // ── create ───────────────────────────────────────────────────────────

    /// Validate a `create` and start its saga at S1.
    fn begin_create(
        &mut self,
        owner: SessionId,
        key: RequestKey,
        body: &CreateBody,
    ) -> LifecycleAnswer {
        let plan = match self.validate_create(owner, body) {
            Ok(plan) => plan,
            Err((code, detail)) => return LifecycleAnswer::refused(key, code, detail),
        };
        // A replay of a `create` finds the same child rather than making a
        // second one: the ownership row's `(owner_id, request_key)` is unique,
        // so a saga that already named a child under this key is that child's.
        if let Ok(Some(saga)) = self.db.child_saga(&owner.to_string(), key.as_str()) {
            if saga.step.is_some_and(|step| !step.is_final()) {
                return LifecycleAnswer::Accepted;
            }
        }

        let child = SessionId::default();
        let name = child_name(&self.session_name(owner), key.as_str());
        let saga = ChildSaga {
            owner_id: owner.to_string(),
            key: key.as_str().to_string(),
            child_id: Some(child.to_string()),
            step: Some(SagaStep::Named),
            task_kind: Some(plan.task_kind.clone()),
            task_body: Some(plan.task_body.clone()),
            branch: Some(plan.branch.clone()),
            instance_id: Some(self.bridge_instance_id()),
            lease_until: Some(crate::sync::state::current_time_millis() + SAGA_LEASE_MILLIS),
            ..ChildSaga::default()
        };
        if let Err(e) = self.db.upsert_child_saga(&saga) {
            return LifecycleAnswer::refused(
                key,
                ErrorCode::Failed,
                format!("friring could not record this child's saga: {e}"),
            );
        }
        self.child_lifecycle.jobs.push(Job {
            owner,
            key: Some(key),
            child,
            name,
            goal: Goal::Create,
            step: Step::Named,
            since: Instant::now(),
            pending: None,
            plan,
            gate_key: gate_key(),
            intent: None,
            egress: None,
            spawned: None,
            dirs: None,
            overlay: None,
            worktree_path: None,
            base_head: None,
            branch_claimed: false,
            waiters: Vec::new(),
            follow_up: None,
            gate_opened_at: None,
            stopped: false,
            settle: super::bridge_saga::ACK_SETTLE_PASSES,
        });
        LifecycleAnswer::Accepted
    }

    /// Every check a `create` must pass before anything is minted.
    ///
    /// Each one is a refusal with its own code, because a leader deciding what
    /// to do next has to tell "you may not create children in that repository"
    /// from "you already have as many as your profile allows".
    fn validate_create(
        &self,
        owner: SessionId,
        body: &CreateBody,
    ) -> Result<LaunchPlan, (ErrorCode, String)> {
        let profile = self.owner_profile(owner)?;
        // The repository is checked against `session_repos` — a row, never a
        // derivation from the boundary's grants. Deriving it would make editing
        // a profile into an authority change.
        if !self
            .db
            .session_owns_repo(&owner.to_string(), &body.repo_root)
            .unwrap_or(false)
        {
            return Err((
                ErrorCode::RepoNotOwned,
                format!(
                    "this session does not work in '{}', so it may not create children there",
                    body.repo_root
                ),
            ));
        }
        if !profile.child_agents.iter().any(|a| a == &body.agent) {
            return Err((
                ErrorCode::AgentNotAllowed,
                format!(
                    "sandbox profile '{}' does not list '{}' as a child agent",
                    profile.name, body.agent
                ),
            ));
        }
        // An exact registry lookup, because `agent_def_for` falls back to the
        // registry default: a profile naming an agent `agents.toml` does not
        // define would otherwise launch a *different* agent — its credentials,
        // its state, its hook schema — under this profile's grants.
        if self.agents.get(&body.agent).is_none() {
            return Err((
                ErrorCode::AgentNotAllowed,
                format!(
                    "'{}' is listed as a child agent but no agent of that name is defined, so \
                     friring will not launch a different one in its place",
                    body.agent
                ),
            ));
        }
        let live = self.reserved_child_slots(owner)?;
        if live >= profile.max_children as usize {
            return Err((
                ErrorCode::FanoutExhausted,
                format!(
                    "this session already has {live} live children, which is what sandbox \
                     profile '{}' allows",
                    profile.name
                ),
            ));
        }
        for dependency in &body.depends_on_children {
            let owned = self
                .db
                .owns_bridge_child(&owner.to_string(), dependency)
                .unwrap_or(false);
            if !owned {
                return Err((
                    ErrorCode::NotOwner,
                    format!("'{dependency}' is not this session's child"),
                ));
            }
            let finished = self
                .db
                .bridge_child_state(dependency)
                .ok()
                .flatten()
                .is_some_and(|row| row.state == ChildState::Done);
            if !finished {
                return Err((
                    ErrorCode::DependencyUnfinished,
                    format!("child '{dependency}' has not finished"),
                ));
            }
        }
        // The branch is caller-supplied and it decides a **path**: the worktree
        // is `<worktrees>/<repo-hash>/<branch with '/' replaced by '-'>`. So it
        // is checked as a ref name and the path it produces is checked as a
        // path, and neither check is trusted to imply the other.
        let branch = body.branch.trim();
        if let Err(detail) = crate::git::check_child_branch_name(branch) {
            return Err((ErrorCode::Failed, detail));
        }
        let Some(worktree) =
            crate::git::planned_worktree_path(&PathBuf::from(&body.repo_root), branch)
        else {
            return Err((
                ErrorCode::Failed,
                "friring could not resolve a worktree directory for that branch".to_string(),
            ));
        };
        // Refuse rather than attach. `create_or_attach_worktree` returns an
        // existing deterministic path unchecked, so a `create` naming a branch
        // the operator already has a worktree for would run the child **in the
        // operator's worktree** — and two children on one branch would share
        // one. A child's workspace is friring's to make, or the create fails.
        if worktree.exists() {
            return Err((
                ErrorCode::Failed,
                format!(
                    "there is already a worktree at the directory branch '{branch}' resolves to, \
                     so friring will not put a child in it. Pick a branch name no worktree exists \
                     for"
                ),
            ));
        }
        // …and the same refusal for a worktree that does not exist *yet*. The
        // broker accepts several requests in one pass and S2 cuts the directory
        // off the tick, so two creates naming one branch — or two spellings that
        // collapse onto one `worktree_segments` path — would both pass the check
        // above and the second would silently attach to the first's workspace.
        // A `Resume` is excluded: it relaunches a child in the worktree it
        // already owns.
        if let Some(other) = self.child_lifecycle.jobs.iter().find(|job| {
            job.goal != Goal::Resume
                && crate::git::planned_worktree_path(&job.plan.repo_root, job.plan.branch.trim())
                    .is_some_and(|planned| planned == worktree)
        }) {
            return Err((
                ErrorCode::Failed,
                format!(
                    "a create for branch '{}' is already in flight and resolves to the same \
                     worktree directory as '{branch}'; wait for it or pick another branch name",
                    other.plan.branch.trim()
                ),
            ));
        }
        Ok(LaunchPlan {
            repo_root: PathBuf::from(&body.repo_root),
            branch: body.branch.clone(),
            agent: body.agent.clone(),
            task_kind: body.task_kind.clone(),
            task_body: body.task_body.clone(),
            role_hint: body.role_hint.clone(),
            profile: profile.name.clone(),
        })
    }

    // ── stop ─────────────────────────────────────────────────────────────

    /// Ask a child to finish, then quiesce it.
    fn begin_stop(
        &mut self,
        owner: SessionId,
        key: RequestKey,
        body: &StopBody,
    ) -> LifecycleAnswer {
        let child = match self.owned_child(owner, &body.child) {
            Ok(child) => child,
            Err((code, detail)) => return LifecycleAnswer::refused(key, code, detail),
        };
        let state = self.child_state(&body.child);
        // Idempotent by state as well as by journal: a child that has already
        // reached a terminal state is stopped, and saying so is the answer.
        if state.is_some_and(ChildState::is_terminal) {
            return LifecycleAnswer::Now(Box::new(Response::ok(
                key,
                Some(serde_json::json!({
                    "child": body.child,
                    "state": state.map(|s| s.as_str()),
                })),
            )));
        }
        let grace = body
            .grace_secs
            .map_or(DEFAULT_GRACE, Duration::from_secs)
            .min(READY_TIMEOUT * 10);
        // A job is already carrying this child. Attach to it rather than
        // starting a second one — and **register the key**, so this caller is
        // answered when that job finishes. Journaled `accepted` with nothing
        // attached, it would wait for ever: a replay of an accepted key waits by
        // design rather than acting twice.
        if let Some(running) = self.job_for_child(child) {
            match self.child_lifecycle.jobs[running].goal {
                // A quiesce is already running. Its outcome — the pane stopped,
                // the worktree inspected, a verdict written — is exactly this
                // caller's answer too.
                Goal::Stop | Goal::Finish => {
                    self.child_lifecycle.jobs[running].waiters.push(key);
                }
                // A launch. Its success says the child is `ready`, which is not
                // an answer to "stop it": held until the launch ends and carried
                // out then, against a child that exists.
                Goal::Create | Goal::Resume => {
                    let follow = self.child_lifecycle.jobs[running]
                        .follow_up
                        .get_or_insert_with(FollowUp::default);
                    follow.stop_keys.push(key);
                    follow.grace = Some(follow.grace.map_or(grace, |held| held.min(grace)));
                }
            }
            return LifecycleAnswer::Accepted;
        }
        self.push_stop_job(owner, child, vec![key], grace);
        LifecycleAnswer::Accepted
    }

    /// Start the quiesce a `stop` asks for, answering `keys` when it ends.
    ///
    /// Shared by [`Self::begin_stop`] and by the follow-up a launch defers, so a
    /// deferred `stop` is carried out by the same steps as an immediate one
    /// rather than by a second implementation of them.
    fn push_stop_job(
        &mut self,
        owner: SessionId,
        child: SessionId,
        keys: Vec<RequestKey>,
        grace: Duration,
    ) {
        let child_id = child.to_string();
        // The `cancel` goes out before anything is stopped, so a child that is
        // between turns can finish on its own terms and send a `result`.
        self.host_mail(child, MailKind::Cancel, &child_id);
        let mut keys = keys.into_iter();
        let key = keys.next();
        self.child_lifecycle.jobs.push(Job {
            owner,
            key,
            child,
            name: self.session_name(child),
            goal: Goal::Stop,
            step: Step::Grace,
            since: Instant::now() - (DEFAULT_GRACE.saturating_sub(grace)),
            pending: None,
            plan: LaunchPlan::default(),
            gate_key: String::new(),
            intent: Some(FinishIntent {
                // A stop's verdict is `stopped`, whatever the child said on its
                // way out. `bridge_results.outcome` still records the intent the
                // host accepted, which for a stop friring never asked for is
                // `failed`: nothing was reported complete.
                outcome: Outcome::Failed,
                message_id: None,
                clean_state: ChildState::Stopped,
            }),
            egress: None,
            spawned: None,
            dirs: None,
            overlay: None,
            worktree_path: None,
            base_head: None,
            branch_claimed: false,
            waiters: keys.collect(),
            follow_up: None,
            gate_opened_at: None,
            stopped: false,
            settle: super::bridge_saga::ACK_SETTLE_PASSES,
        });
    }

    // ── resume ───────────────────────────────────────────────────────────

    /// Relaunch a dirty, stalled, stopped or unusable child, keeping its
    /// ownership.
    fn begin_resume(
        &mut self,
        owner: SessionId,
        key: RequestKey,
        body: &ResumeBody,
    ) -> LifecycleAnswer {
        let child = match self.owned_child(owner, &body.child) {
            Ok(child) => child,
            Err((code, detail)) => return LifecycleAnswer::refused(key, code, detail),
        };
        let Some(state) = self.child_state(&body.child) else {
            return LifecycleAnswer::refused(
                key,
                ErrorCode::UnknownChild,
                format!("child '{}' has no recorded state", body.child),
            );
        };
        if !state.is_resumable() {
            return LifecycleAnswer::refused(
                key,
                ErrorCode::Failed,
                format!(
                    "a child in '{state}' is not resumable; only dirty, stalled, stopped and \
                     unusable children are"
                ),
            );
        }
        // A relaunch is already running. Attach to it rather than starting a
        // second one — and **register the key**, so this caller is answered when
        // that job finishes. Journaled `accepted` with nothing attached, it
        // would wait for ever: a replay of an accepted key waits by design
        // rather than acting twice.
        //
        // Only onto another relaunch. The resumable-state check above already
        // excludes a child that is starting (`starting` is not resumable) or
        // quiescing (`finishing` is not either), so any other goal here means
        // the state and the job disagree — refused rather than answered from a
        // job that is doing something else.
        if let Some(running) = self.job_for_child(child) {
            if self.child_lifecycle.jobs[running].goal != Goal::Resume {
                return LifecycleAnswer::refused(
                    key,
                    ErrorCode::NotReady,
                    format!(
                        "friring is already carrying out something else for child '{}'; ask for \
                         its status and send this again once it is resumable",
                        body.child
                    ),
                );
            }
            self.child_lifecycle.jobs[running].waiters.push(key);
            return LifecycleAnswer::Accepted;
        }
        // The worktree and the branch come from the *recorded* saga, never from
        // the request: a resume relaunches the child that exists, and a caller
        // that could name a worktree would be creating a new child under an old
        // child's ownership row.
        let Some(prior) = self.db.child_saga_of_child(&body.child).ok().flatten() else {
            return LifecycleAnswer::refused(
                key,
                ErrorCode::Failed,
                format!(
                    "friring has no recorded launch for child '{}', so there is nothing to \
                     relaunch it from",
                    body.child
                ),
            );
        };
        let profile = match self.owner_profile(owner) {
            Ok(profile) => profile,
            Err((code, detail)) => return LifecycleAnswer::refused(key, code, detail),
        };
        // `dirty` and `stalled` are live and already count against the cap.
        // `stopped` and `unusable` released their slot when their runtime was
        // retired, so resuming either is a fresh capacity claim. Reserve it
        // before killing or mutating anything. `begin_bridge_child_resume`
        // below changes it to `starting` synchronously, so the next request in
        // this broker pass sees the claim in `reserved_child_slots`.
        if !state.is_live() {
            let live = match self.reserved_child_slots(owner) {
                Ok(live) => live,
                Err((code, detail)) => return LifecycleAnswer::refused(key, code, detail),
            };
            if live >= profile.max_children as usize {
                return LifecycleAnswer::refused(
                    key,
                    ErrorCode::FanoutExhausted,
                    format!(
                        "this session already has {live} live children, which is what sandbox \
                         profile '{}' allows",
                        profile.name
                    ),
                );
            }
        }
        let Some((repo_root, agent)) = self.child_repo_and_agent(&body.child) else {
            return LifecycleAnswer::refused(
                key,
                ErrorCode::Failed,
                format!(
                    "friring has no recorded repository for child '{}'",
                    body.child
                ),
            );
        };
        // The conversation, before anything is mutated and for the same reason
        // capacity is: a `resume` that cannot reach the child's own thread is
        // refused, never launched into a blank one, and a refusal must leave the
        // child exactly as it found it. The saga checks again at S5 as a
        // backstop, but by then the child is `starting` and a failure there
        // costs it — so the answer has to be known here.
        if let Err((code, detail)) =
            self.child_resume_identity(child, &agent, &self.child_env(child))
        {
            return LifecycleAnswer::refused(key, code, detail);
        }
        // Everything the relaunch needs is now known, so the old pane can go.
        // A resumable state does not mean a dead pane: `stalled` is set from the
        // nudge counter alone, with the child's agent still running, so a resume
        // that just relaunched would put a second agent in one worktree under
        // one session id — and the identity revalidation at S8 would find the
        // *old* pane, pass, and open the *new* one's gate. Last, because a
        // refusal above must not have killed anything.
        if let Err(detail) = self.stop_resumed_child_pane(child) {
            return LifecycleAnswer::refused(
                key,
                ErrorCode::QuiesceFailed,
                format!(
                    "friring could not stop child '{}' before relaunching it, so it will not \
                     start a second agent in that worktree: {detail}",
                    body.child
                ),
            );
        }
        let plan = LaunchPlan {
            repo_root,
            branch: prior.branch.clone().unwrap_or_default(),
            agent,
            task_kind: prior
                .task_kind
                .clone()
                .unwrap_or_else(|| "task".to_string()),
            // A relaunch delivers no second copy of the task: the child's
            // mailbox already holds it, and re-inserting it would read as a
            // second assignment.
            task_body: String::new(),
            role_hint: None,
            profile: profile.name.clone(),
        };
        let saga = ChildSaga {
            owner_id: owner.to_string(),
            key: key.as_str().to_string(),
            child_id: Some(child.to_string()),
            step: Some(SagaStep::Named),
            task_kind: prior.task_kind.clone(),
            worktree_path: prior.worktree_path.clone(),
            branch: prior.branch.clone(),
            base_head: prior.base_head.clone(),
            instance_id: Some(self.bridge_instance_id()),
            lease_until: Some(crate::sync::state::current_time_millis() + SAGA_LEASE_MILLIS),
            ..ChildSaga::default()
        };
        if let Err(e) = self.db.begin_bridge_child_resume(&saga, &body.child) {
            return LifecycleAnswer::refused(
                key,
                ErrorCode::Failed,
                format!("friring could not record this relaunch: {e}"),
            );
        }
        self.child_lifecycle.jobs.push(Job {
            owner,
            key: Some(key),
            child,
            name: self.session_name(child),
            goal: Goal::Resume,
            step: Step::Named,
            since: Instant::now(),
            pending: None,
            plan,
            gate_key: gate_key(),
            intent: None,
            egress: None,
            spawned: None,
            dirs: None,
            overlay: None,
            worktree_path: prior.worktree_path.map(PathBuf::from),
            base_head: prior.base_head,
            branch_claimed: false,
            waiters: Vec::new(),
            follow_up: None,
            gate_opened_at: None,
            stopped: false,
            settle: super::bridge_saga::ACK_SETTLE_PASSES,
        });
        LifecycleAnswer::Accepted
    }

    /// Slots already consumed or promised by this owner.
    ///
    /// Two halves, and both are **durable**: the children that have a live row,
    /// and the launches that have claimed a slot without having one yet. A
    /// create is accepted, and its saga written, long before S6 commits the
    /// child, and the broker accepts several requests in one pass — so the
    /// second half is what stops two creates in a tick from sharing one slot.
    ///
    /// It is read from `child_sagas` rather than from this broker's own
    /// in-flight jobs, and the difference is a friring's bridge lease moving.
    /// The jobs answer for this process; an instance that took an owner's queue
    /// over mid-launch has none of them and would have admitted its peer's
    /// launches all over again. The saga row is written before the job is
    /// pushed, so the durable count is a superset of the in-memory one and
    /// replaces it rather than adding to it.
    ///
    /// Both halves come from **one** [`Database::reserved_child_slots`]
    /// statement, and that is load-bearing rather than tidy: they are disjoint
    /// at any single instant, so counting them at two instants is how a child
    /// escapes. A peer that commits between the reads moves its child from the
    /// pending side to the live side, and a count taken before that on one side
    /// and after it on the other sees it on neither.
    ///
    /// # Errors
    ///
    /// The count could not be read. Capacity **fails closed**: the cap is the
    /// only thing standing between an owner and an unbounded number of agents
    /// inside its boundary, so an unreadable table is refused rather than
    /// counted as zero — which would have authorized a whole `max_children`
    /// worth of extra children at exactly the moment friring could no longer
    /// see the ones it already had.
    fn reserved_child_slots(&self, owner: SessionId) -> Result<usize, (ErrorCode, String)> {
        self.db
            .reserved_child_slots(&owner.to_string())
            .map_err(|e| {
                (
                    ErrorCode::Failed,
                    format!(
                        "friring could not establish how many children this session already \
                         has, so it will not start another: {e}"
                    ),
                )
            })
    }

    // ── The finish intent ────────────────────────────────────────────────

    /// Accept a child's `result` as its one finish intent (ADR-32).
    ///
    /// Called by the broker the moment the mail is enqueued. The intent is
    /// *not* a terminal state: the host stops the pane, inspects the worktree
    /// and decides. A child claiming `completed` over a dirty worktree lands in
    /// [`ChildState::Dirty`], not in `done`.
    pub(crate) fn accept_finish_intent(
        &mut self,
        child: SessionId,
        outcome: Outcome,
        message_id: Option<i64>,
    ) {
        let child_id = child.to_string();
        let Ok(Some(row)) = self.db.bridge_child(&child_id) else {
            return;
        };
        let Ok(owner) = row.owner_id.parse::<SessionId>() else {
            return;
        };
        // A stop already in flight owns the quiesce; the child's own intent
        // merely tells it not to wait out the grace period.
        if let Some(job) = self.job_for_child(child) {
            if self.child_lifecycle.jobs[job].step == Step::Grace {
                self.child_lifecycle.jobs[job].step = Step::Acked;
                self.child_lifecycle.jobs[job].since = Instant::now();
                if let Some(intent) = self.child_lifecycle.jobs[job].intent.as_mut() {
                    intent.message_id = message_id;
                }
                let _ = self
                    .db
                    .set_bridge_child_state(&child_id, ChildState::Finishing);
                self.host_mail(child, MailKind::Ack, &child_id);
                return;
            }
            // A launch that has not finished. The child's agent is running — the
            // gate opened at S8 — so a small worker can genuinely finish before
            // S9 sees its first hook report. Held rather than run now (there is
            // no verdict to take of a child still being built) and rather than
            // dropped (it is the child's one finish intent, and the `send` that
            // carried it was already answered `ok`).
            if self.child_lifecycle.jobs[job].goal.is_launch() {
                let held = self.child_lifecycle.jobs[job]
                    .follow_up
                    .as_ref()
                    .is_some_and(|f| f.intent.is_some());
                let follow = self.child_lifecycle.jobs[job]
                    .follow_up
                    .get_or_insert_with(FollowUp::default);
                // The **first** intent wins, as it does everywhere else: ADR-32
                // gives a child one, and a second `result` is not a correction.
                if follow.intent.is_none() {
                    // Order, inferred from what is already held: no `stop` key
                    // yet means this result arrived first, which is what the
                    // live path would have resolved in its favour.
                    follow.intent_first = follow.stop_keys.is_empty();
                    follow.intent = Some(FinishIntent {
                        outcome,
                        message_id,
                        clean_state: outcome.terminal_state(),
                    });
                }
                // Persisted, because nothing else on record would carry it: the
                // launch has not written `finishing`, and the `send` that
                // brought it has already been answered `ok` — so a replay after
                // a crash returns that answer without ever reaching here again.
                if !held {
                    self.record_saga_after(job, |saga| {
                        saga.finish_outcome = Some(outcome.as_str().to_string());
                        saga.finish_message_id = message_id;
                    });
                }
            }
            // Otherwise the quiesce is already past its grace: this child's one
            // intent has been taken and is being carried out.
            return;
        }
        let _ = self
            .db
            .set_bridge_child_state(&child_id, ChildState::Finishing);
        // The host's own `ack`, so an agent polling its inbox learns the intent
        // was taken rather than watching its pane die unexplained.
        self.host_mail(child, MailKind::Ack, &child_id);
        self.child_lifecycle.jobs.push(Job {
            owner,
            key: None,
            child,
            name: self.session_name(child),
            goal: Goal::Finish,
            step: Step::Acked,
            since: Instant::now(),
            pending: None,
            plan: LaunchPlan::default(),
            gate_key: String::new(),
            intent: Some(FinishIntent {
                outcome,
                message_id,
                clean_state: outcome.terminal_state(),
            }),
            egress: None,
            spawned: None,
            dirs: None,
            overlay: None,
            worktree_path: None,
            base_head: None,
            branch_claimed: false,
            waiters: Vec::new(),
            follow_up: None,
            gate_opened_at: None,
            stopped: false,
            settle: super::bridge_saga::ACK_SETTLE_PASSES,
        });
    }

    /// How many `stop` keys the job running for `child` is holding behind its
    /// launch.
    ///
    /// The test entry point for arrival order: a test that has to establish
    /// "the stop was seen first" needs to observe that the broker really took
    /// it, rather than assume a number of passes is enough.
    #[cfg(test)]
    pub(crate) fn held_stop_keys_for_test(&self, child: SessionId) -> usize {
        self.job_for_child(child)
            .and_then(|job| self.child_lifecycle.jobs[job].follow_up.as_ref())
            .map_or(0, |follow| follow.stop_keys.len())
    }

    /// Carry out what a finished job was holding — see [`FollowUp`].
    ///
    /// Called once, as the job is dropped, so the quiesce it starts runs against
    /// a child whose launch is over one way or the other.
    pub(super) fn start_follow_up(&mut self, owner: SessionId, child: SessionId, follow: FollowUp) {
        let child_id = child.to_string();
        let state = self.child_state(&child_id);
        // The launch this was queued behind failed, or the child is already
        // terminal: there is no pane to stop and no worktree friring has not
        // already read. Answer the `stop` from the state on record — which is
        // the same idempotent answer `begin_stop` gives for a terminal child —
        // and drop the intent, because the child it belonged to is gone.
        if state.map_or(true, ChildState::is_terminal) {
            for key in follow.stop_keys {
                let response = match state {
                    Some(state) => Response::ok(
                        key,
                        Some(serde_json::json!({
                            "child": child_id,
                            "state": state.as_str(),
                        })),
                    ),
                    None => Response::refused(
                        key,
                        ErrorCode::UnknownChild,
                        format!("child '{child_id}' was not created, so there was nothing to stop"),
                    ),
                };
                self.answer_bridge_request(owner, response);
            }
            return;
        }
        // The result arrived first, so it decides the verdict — as it would have
        // on the live path, where a `result` creates the quiesce and a later
        // `stop` joins it as a waiter. Order matters here and nowhere else in
        // this function: `accept_finish_intent`'s `Step::Grace` branch only
        // carries a `message_id` across, so applying it *after* a stop job would
        // leave the stop's own `failed`/`stopped` standing over the child's
        // `completed`.
        let FollowUp {
            mut stop_keys,
            grace,
            intent,
            intent_first,
        } = follow;
        if intent_first {
            if let Some(intent) = &intent {
                self.accept_finish_intent(child, intent.outcome, intent.message_id);
                if let Some(job) = self.job_for_child(child) {
                    self.child_lifecycle.jobs[job]
                        .waiters
                        .append(&mut stop_keys);
                    return;
                }
                // No quiesce was started — the child's row became unreadable
                // between the two. The `stop`s are still owed an answer, so they
                // fall through to a stop job of their own.
            }
        }
        if !stop_keys.is_empty() {
            self.push_stop_job(owner, child, stop_keys, grace.unwrap_or(DEFAULT_GRACE));
        }
        // Applied through the ordinary path, so a held intent moves the stop job
        // just pushed off its grace exactly as a live one does — and starts a
        // quiesce of its own when no `stop` was waiting.
        if !intent_first {
            if let Some(intent) = intent {
                self.accept_finish_intent(child, intent.outcome, intent.message_id);
            }
        }
    }

    // ── Small reads ──────────────────────────────────────────────────────

    /// The owner's own sandbox profile, or why there is none to read.
    fn owner_profile(&self, owner: SessionId) -> Result<SandboxProfile, (ErrorCode, String)> {
        let name = self
            .sessions
            .iter()
            .find(|s| s.info.id == owner)
            .and_then(|s| s.info.sandbox_profile.clone())
            .ok_or((
                ErrorCode::GrantMissing,
                "this session carries no sandbox profile, so it grants no bridge".to_string(),
            ))?;
        match self.db.get_sandbox_profile(&name) {
            Ok(Some(stored)) if stored.is_intact() => Ok(stored.profile),
            // A profile friring cannot fully decode grants nothing — the same
            // narrow reading the broker's grant check applies.
            Ok(Some(_)) => Err((
                ErrorCode::GrantMissing,
                format!("sandbox profile '{name}' could not be fully decoded"),
            )),
            Ok(None) => Err((
                ErrorCode::GrantMissing,
                format!("sandbox profile '{name}' no longer exists"),
            )),
            Err(e) => Err((
                ErrorCode::Failed,
                format!("friring could not read sandbox profile '{name}': {e}"),
            )),
        }
    }

    /// Resolve a child id the caller claims to own, against the immutable row.
    fn owned_child(&self, owner: SessionId, child: &str) -> Result<SessionId, (ErrorCode, String)> {
        let row = self
            .db
            .bridge_child(child)
            .ok()
            .flatten()
            .ok_or((ErrorCode::UnknownChild, format!("no child '{child}'")))?;
        if row.owner_id != owner.to_string() {
            return Err((
                ErrorCode::NotOwner,
                format!("child '{child}' is not this session's"),
            ));
        }
        row.child_id.parse::<SessionId>().map_err(|_| {
            (
                ErrorCode::Failed,
                format!("child '{child}' has an unreadable id"),
            )
        })
    }

    fn child_state(&self, child_id: &str) -> Option<ChildState> {
        self.db
            .bridge_child_state(child_id)
            .ok()
            .flatten()
            .map(|row| row.state)
    }

    /// The repository a child works in and the agent it runs, from its own
    /// `session_repos` row and its session row.
    fn child_repo_and_agent(&self, child_id: &str) -> Option<(PathBuf, String)> {
        let repo = self
            .db
            .session_repo_roots(child_id)
            .ok()?
            .into_iter()
            .next()?;
        // From the **row**, not the live list. A resume has to work for a child
        // whose entry is gone: after a crash it was never rebuilt, and a resume
        // retires the old entry before it relaunches. The list is only the
        // fallback for a session not yet written back.
        let id = child_id.parse::<SessionId>().ok()?;
        let agent = self
            .db
            .get_session_by_id(id)
            .ok()
            .flatten()
            .map(|row| row.agent)
            .filter(|agent| !agent.is_empty())
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|s| s.info.id == id)
                    .map(|s| s.info.agent.clone())
            })?;
        Some((PathBuf::from(repo), agent))
    }

    fn session_name(&self, id: SessionId) -> String {
        self.sessions
            .iter()
            .find(|s| s.info.id == id)
            .map(|s| s.info.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    fn job_for_child(&self, child: SessionId) -> Option<usize> {
        self.child_lifecycle
            .jobs
            .iter()
            .position(|job| job.child == child)
    }

    // ── Cascades ─────────────────────────────────────────────────────────

    /// What an operator's hard delete means for the bridge (ADR-32).
    ///
    /// Two directions, and both matter. **Downwards**: a session being deleted
    /// takes away the one place its children's `result` mail could go, so every
    /// live child is stopped first. **Upwards**: a *child* being deleted leaves
    /// its owner polling `status` for a session that stopped existing, so the
    /// owner is told in friring's own words.
    ///
    /// The children are stopped, never deleted: an operator removing a leader is
    /// not an instruction to throw away what its workers wrote.
    ///
    /// Fails closed on either read, exactly as the headless force-delete does
    /// (`session_ops::delete::stop_owned_children`): an unreadable ownership or
    /// state row is not evidence that there is nothing to stop, and deleting the
    /// one session that could stop them would strand running agents.
    pub(crate) fn cascade_bridge_delete(&mut self, session: SessionId) -> Result<(), String> {
        let session_key = session.to_string();
        let children = self.db.bridge_children_of(&session_key).map_err(|e| {
            format!(
                "Session {session} owns bridge children and friring could not read them ({e}), so \
                 it will not delete the one session that could stop them. Repair the database, or \
                 force-delete each child by id first"
            )
        })?;
        for child in children {
            let live = match self.db.bridge_child_state(&child.child_id) {
                Ok(row) => row.is_some_and(|row| row.state.is_live()),
                Err(e) => {
                    return Err(format!(
                        "friring could not read the state of child '{}' ({e}), so it cannot tell \
                         whether stopping it is still needed and will not delete its owner",
                        child.child_id
                    ))
                }
            };
            let Ok(id) = child.child_id.parse::<SessionId>() else {
                continue;
            };
            if !live || self.job_for_child(id).is_some() {
                continue;
            }
            // No grace: the owner is going away, so there is nobody left to
            // receive the `result` the grace period exists to allow.
            self.child_lifecycle.jobs.push(Job {
                owner: session,
                key: None,
                child: id,
                name: self.session_name(id),
                goal: Goal::Stop,
                step: Step::Acked,
                since: Instant::now(),
                pending: None,
                plan: LaunchPlan::default(),
                gate_key: String::new(),
                intent: Some(FinishIntent {
                    outcome: Outcome::Failed,
                    message_id: None,
                    clean_state: ChildState::Stopped,
                }),
                egress: None,
                spawned: None,
                dirs: None,
                overlay: None,
                worktree_path: None,
                base_head: None,
                branch_claimed: false,
                waiters: Vec::new(),
                follow_up: None,
                gate_opened_at: None,
                stopped: false,
                settle: super::bridge_saga::ACK_SETTLE_PASSES,
            });
        }
        if let Ok(Some(row)) = self.db.bridge_child(&session_key) {
            let _ = self.db.mark_bridge_child_force_deleted(&session_key);
            if let Ok(owner) = row.owner_id.parse::<SessionId>() {
                self.host_mail(owner, MailKind::ChildRemovedByOperator, &session_key);
            }
        }
        Ok(())
    }
}

/// A child's session name: its owner's, plus the request key's first eight
/// characters.
///
/// Derived from the key rather than from a counter so a replay of the same
/// `create` names the same child, and bounded so a long owner name does not
/// make a window name the multiplexer truncates.
fn child_name(owner: &str, key: &str) -> String {
    let stem: String = owner
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect();
    let stem = if stem.is_empty() { "child" } else { &stem };
    format!("{stem}-{}", &key[..key.len().min(8)])
}

/// A fresh key for one launch gate.
///
/// Per launch, not per child: a relaunch waits on a new key, so a release file
/// left by a previous attempt cannot open this one's gate.
fn gate_key() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The overlay a child launches under (ADR-31).
///
/// Built from the host's own directories and the profile's `child_shared_rw`.
/// Nothing in it comes from the request: a `create` chooses *which* of the
/// things already permitted, never what is permitted.
pub(crate) fn child_overlay(
    profile: &SandboxProfile,
    owner_key: &str,
    dirs: &ChildDirs,
    agent: Option<&crate::session::AgentSandboxDef>,
    home: &str,
    seed: &SeedPlan,
    repo: &std::path::Path,
) -> SandboxOverlay {
    let mut own_dirs = vec![
        dirs.scratch.display().to_string(),
        dirs.signal.display().to_string(),
        dirs.bridge.display().to_string(),
    ];
    own_dirs.extend(child_git_dir(&dirs.worktree, repo));
    // Every sibling's metadata goes, and this child's comes back.
    //
    // A profile that shares `<repo>/.git` — which is what a linked worktree
    // makes siblings share, and what lets a child commit at all — otherwise
    // hands each child write access to **every** sibling's
    // `.git/worktrees/<id>`: its index, its `HEAD`, its refs. Granting this
    // child its own directory does not narrow that ancestor grant, so the tree
    // is subtracted here and `own_dirs` is re-granted after the subtraction,
    // which is the same shape the family state directory and its seeds use.
    let mut subtract = crate::sandbox::child_state::subtract_set(agent, home, owner_key);
    let siblings = repo.join(".git").join("worktrees").display().to_string();
    if !subtract.iter().any(|entry| entry.path == siblings) {
        subtract.push(crate::session::SubtractPath {
            path: siblings,
            is_dir: true,
        });
    }
    SandboxOverlay {
        worktree: Some(dirs.worktree.display().to_string()),
        own_dirs,
        gate_dir: Some(dirs.gate.display().to_string()),
        state_dir: Some(dirs.state.display().to_string()),
        shared_rw: profile.child_shared_rw.clone(),
        ro_extra: Vec::new(),
        seed: seed.grants(),
        subtract,
    }
}

/// The child's own git metadata directory, when its worktree is a **linked**
/// git worktree.
///
/// A linked worktree holds only a `.git` *file* saying `gitdir: <path>`; its
/// index, `HEAD` and per-worktree refs live at that path, under the source
/// repository's `.git/worktrees/<id>`. Without it a child can edit its files and
/// never commit them — `git commit` fails on `index.lock` with `Operation not
/// permitted` — and a worktree a child could not commit is one friring reads as
/// **dirty**, which is the single outcome that is never merged. So a bridge
/// child that does any work at all would end every run needing a person.
///
/// It is per-child by construction: `git::claim_child_worktree` mints one
/// directory per worktree, named for this child's branch, and no sibling's
/// metadata is inside it. Granted with the child's own directories rather than
/// through `child_shared_rw`, which is the operator's knob for things siblings
/// genuinely share.
///
/// # The marker does not decide, the repository's own record does
///
/// `.git` is a file **inside the child's own worktree**, which the child may
/// write, and `own_dirs` is exempt from the parent-grant check precisely
/// because friring is supposed to have minted every path in it. Containment is
/// not enough on its own: every sibling's metadata is *also* inside
/// `<repo>/.git/worktrees`, so a child that rewrote its marker to name a
/// sibling would be handed that sibling's index, `HEAD` and refs read-write on
/// its next `resume` — the same escape by a shorter route.
///
/// So the direction of the question is reversed. git records ownership on the
/// **host** side: it writes `<repo>/.git/worktrees/<id>/gitdir` holding the path
/// of that worktree's `.git` file, which is how git itself resolves a linked
/// worktree back to its metadata. This finds the entry whose record names *this*
/// worktree, and grants that. The child's marker is then required to agree with
/// it, so a rewritten marker grants nothing rather than something else.
///
/// Every branch fails closed:
///
/// - the marker is not a regular file — nothing is granted;
/// - no entry names this worktree — nothing is granted;
/// - **two** entries name it — nothing is granted, for either of them, because
///   friring cannot tell which record is git's;
/// - the marker names a different entry than the record — nothing is granted.
///
/// The record lives in a directory the child is granted, but not one it can
/// write: `gitdir` and `commondir` are taken back read-only inside a child's own
/// metadata directory
/// ([`PROTECTED_IN_WORKTREE_METADATA`](crate::sandbox::backend::PROTECTED_IN_WORKTREE_METADATA)),
/// so the two-record case is a record something **other than a child** wrote — a
/// stale one after a `git worktree move`, or a repair. It costs that worktree
/// its ability to commit, which friring reports as dirty and a person resolves.
/// `repo` is the launch's own recorded repository root, not anything the child
/// can influence.
///
/// Compared after canonicalizing every side, because `/tmp` is a symlink on
/// macOS and a string prefix test would both refuse a legitimate pointer and
/// accept a crafted one.
///
/// `None` when the worktree is an ordinary checkout (its `.git` is a directory
/// inside the worktree, already covered by the worktree grant) as well as in
/// every case above. None of those is an error here: a launch that genuinely
/// has no worktree fails elsewhere, with a better message than this could give.
pub(super) fn child_git_dir(worktree: &std::path::Path, repo: &std::path::Path) -> Option<String> {
    let marker_path = worktree.join(".git");
    // A **regular file**, not a link to one. The marker is the one input here
    // the child owns, and every `canonicalize` below follows links: a marker
    // symlinked at a *sibling's* `.git` would otherwise make the host resolve
    // this worktree to that sibling's, match that sibling's record, and grant
    // its metadata — the same escape the record check exists to stop, through
    // the one path that never reads the marker's contents.
    if !std::fs::symlink_metadata(&marker_path)
        .ok()?
        .file_type()
        .is_file()
    {
        return None;
    }
    let marker = std::fs::read_to_string(&marker_path).ok()?;
    let named = marker.strip_prefix("gitdir:")?.trim();
    if named.is_empty() {
        return None;
    }
    let claimed = std::fs::canonicalize(named).ok()?;
    let root = std::fs::canonicalize(repo.join(".git").join("worktrees")).ok()?;
    // The **worktree** git's record has to name, rather than the marker file
    // inside it. Canonicalizing the directory resolves the symlinks a data
    // directory really has (`/tmp` on macOS) without following the one thing a
    // child can replace.
    let want = std::fs::canonicalize(worktree).ok()?;

    let mut recorded: Option<std::path::PathBuf> = None;
    for entry in std::fs::read_dir(&root).ok()? {
        let dir = entry.ok()?.path();
        let Ok(gitdir) = std::fs::read_to_string(dir.join("gitdir")) else {
            continue;
        };
        // git writes `<worktree>/.git` here.
        let named = std::path::Path::new(gitdir.trim());
        if named.file_name() != Some(std::ffi::OsStr::new(".git")) {
            continue;
        }
        let Some(parent) = named.parent() else {
            continue;
        };
        if std::fs::canonicalize(parent).ok() != Some(want.clone()) {
            continue;
        }
        if recorded.is_some() {
            // Two records claim one worktree, so at least one was written by
            // something other than git.
            return None;
        }
        recorded = Some(std::fs::canonicalize(&dir).ok()?);
    }
    let recorded = recorded?;
    // A child of the metadata root, and not the root itself: an entry reached
    // through a symlink could otherwise resolve anywhere.
    (recorded == claimed && recorded.parent() == Some(root.as_path()))
        .then(|| recorded.to_string_lossy().into_owned())
}

/// Make every protected name inside one writable git root exist, so the
/// boundary can take it back.
///
/// Only seatbelt denies by pathname, which covers a path that is not there yet.
/// bwrap's `--ro-bind-try` skips a missing source and a place mounts only what
/// exists, because an engine asked to bind a missing one invents a root-owned
/// file inside somebody's repository. So on those two backends an **absent**
/// `config.worktree` is not a protected name at all — it is a name the child can
/// create, and a repository with `extensions.worktreeConfig` already enabled
/// (`git sparse-checkout` turns it on) then reads `core.fsmonitor` out of what
/// the child wrote, the next time the host runs `git` in that worktree. That is
/// host command execution, so it is not a documentation matter.
///
/// friring therefore creates the file, empty, before the boundary is built. An
/// empty config is inert whether or not that extension is on, and this is the
/// same treatment the child's own metadata directory gets — the difference is
/// that a shared `<repo>/.git` is the operator's repository, so the fact that
/// friring adds a file to it is written down (`docs/SANDBOX.md`).
///
/// # Errors
///
/// The name is missing and could not be created — a read-only filesystem, a
/// permission, a path that is not a directory. The launch is then **refused**
/// rather than started with a hole the backend cannot close, which is the whole
/// point of doing this before the grant.
///
/// A root that is neither a git directory nor one worktree's metadata has no
/// protected names of this kind, so this is a no-op for it.
pub(super) fn ensure_protected_placeholders(root: &str) -> Result<(), String> {
    let protected = crate::sandbox::backend::protected_paths_in(root);
    for name in crate::sandbox::backend::PROTECTED_CREATED_IF_ABSENT {
        let path = std::path::Path::new(root).join(name);
        if !protected.contains(&path.display().to_string()) {
            continue;
        }
        if path.exists() {
            continue;
        }
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| {
                format!(
                    "friring could not create '{}', which a sandbox backend has to be able to \
                     take back read-only before a child may write that directory: {e}",
                    path.display()
                )
            })?;
    }
    Ok(())
}

/// The five directories a child owns, minted at S3.
#[derive(Debug, Clone)]
pub(crate) struct ChildDirs {
    pub worktree: PathBuf,
    pub scratch: PathBuf,
    pub signal: PathBuf,
    pub bridge: PathBuf,
    pub gate: PathBuf,
    pub state: PathBuf,
}

/// Mint every directory a child needs, and seed its private agent state.
///
/// # Errors
///
/// A directory could not be created or adopted, or the seeding plan could not be
/// built or carried out — each of which fails the saga with
/// [`ErrorCode::StateUnrelocatable`] rather than launching a child that would
/// share its family's state.
pub(crate) fn mint_child_dirs(child_key: &str, worktree: &Path) -> Result<ChildDirs, String> {
    let scratch = crate::sandbox::create_session_scratch(child_key).map_err(|e| e.to_string())?;
    let signal = crate::paths::create_session_signal_dir(child_key)?;
    let bridge = crate::paths::create_session_bridge_dirs(child_key)?;
    let gate =
        crate::sandbox::dirs::create_session_gate_dir(child_key).map_err(|e| e.to_string())?;
    let state =
        crate::sandbox::dirs::create_child_state_dir(child_key).map_err(|e| e.to_string())?;
    Ok(ChildDirs {
        worktree: worktree.to_path_buf(),
        scratch,
        signal: signal.dir,
        bridge,
        gate,
        state,
    })
}

impl App {
    /// Stop the pane of a child that is about to be relaunched, and retire its
    /// entry.
    ///
    /// Both halves matter. The **kill** is by exact recorded identity, so a
    /// stalled-but-live child does not end up with two agents in one worktree.
    /// The **retire** is what keeps `self.sessions` holding at most one entry
    /// per id: S6 pushes the relaunched session, and every later lookup is a
    /// `find` that returns the first match — so an entry left behind would make
    /// the gate release revalidate the dead pane and open the live one's gate.
    ///
    /// A child with no entry is already stopped, which is the resume-after-crash
    /// case and not an error.
    fn stop_resumed_child_pane(&mut self, child: SessionId) -> Result<(), String> {
        let Some(session) = self.sessions.iter().find(|s| s.info.id == child) else {
            return Ok(());
        };
        // A failed revalidation is **not** a refusal here: it says the pane
        // friring recorded is no longer the one answering to that id, which is
        // exactly what a `dirty` child looks like after the quiesce already
        // killed it — there is nothing left to stop, and killing by id alone
        // would aim at whatever now holds it. What refuses is a pane that *is*
        // still ours and will not die, because relaunching over one of those
        // puts two agents in one worktree.
        if !session.is_placeholder() && session.revalidate_identity().is_ok() {
            session.kill_checked().map_err(|e| format!("{e:#}"))?;
        }
        self.sessions.retain(|s| s.info.id != child);
        self.session_terminal_views.remove(&child);
        if self.active_index >= self.sessions.len() {
            self.active_index = self.sessions.len().saturating_sub(1);
        }
        Ok(())
    }
}

/// Whether a session's own hook has reported it running (S9's only proof).
///
/// The **timestamp** is the proof, not the state word. `SessionInfo::new` sets
/// `Working` before any hook has fired, so a rule that read the derived status
/// would call every child ready on the first pass after its gate opened —
/// including one whose agent died instantly or ignored its seeded private state,
/// which is exactly what S9 exists to catch (see `READY_TIMEOUT`).
///
/// `since` is when the gate was released, in epoch ms. A hook row stamped at or
/// after it is a report this launch produced; anything older belongs to a
/// previous life of the same session id, which a `resume` has.
pub(crate) fn hook_reported_since(row: Option<&crate::storage::HookRow>, since: i64) -> bool {
    row.is_some_and(|row| row.state.is_some() && row.state_at.is_some_and(|at| at >= since))
}

/// The `BridgeLaunch` a child's composition needs.
pub(crate) fn bridge_launch(
    overlay: SandboxOverlay,
    dirs: &ChildDirs,
    gate_key: &str,
) -> BridgeLaunch {
    BridgeLaunch {
        overlay,
        gate: Some((dirs.gate.display().to_string(), gate_key.to_string())),
        bridge_dir: Some(dirs.bridge.display().to_string()),
        state_dir: Some(dirs.state.display().to_string()),
    }
}

/// The backend a child spawns on: always the owner's own local one.
///
/// A bridge child is never remote and never in a place — both are refused
/// before this by [`crate::agent::sandboxing::bridge_refusal`] — so there is one
/// answer and it is the registry's default.
pub(crate) fn child_backend(app: &App) -> Arc<dyn SessionBackend> {
    Arc::clone(app.backends.default_backend())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child's name is derived from the request key, so a replay of the same
    /// `create` names the same child rather than a second one.
    #[test]
    fn a_childs_name_is_stable_for_one_request_key() {
        let first = child_name("lead", "abcdefgh-1234");
        assert_eq!(first, child_name("lead", "abcdefgh-1234"));
        assert_ne!(first, child_name("lead", "zzzzzzzz-1234"));
        assert!(first.starts_with("lead-"));
    }

    /// A name friring puts in a window must not carry anything a multiplexer
    /// would read as syntax.
    #[test]
    fn a_childs_name_is_one_plain_word() {
        let name = child_name("lead: '$(whoami)' \u{1b}[2J", "abcdefgh");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{name}"
        );
    }

    /// Every gate key is its own: a release file left by a previous attempt
    /// must never open the next one's gate.
    #[test]
    fn each_launch_waits_on_its_own_gate_key() {
        assert_ne!(gate_key(), gate_key());
    }

    /// The `omx` client must wait out friring's own ceiling on a `create`.
    ///
    /// A create is answered by the saga, not by the request, and the longest a
    /// saga can legitimately take is two blocking steps (the worktree and the
    /// window) plus the egress acknowledgement and the ready wait. A client
    /// timeout under that sum abandons a child friring is still building — the
    /// worktree the client will never be told about. Asserted from the Rust
    /// constants rather than from a number copied into the JS suite, so raising
    /// any of them fails here instead of silently reintroducing the abandonment.
    #[test]
    fn the_omx_client_outwaits_the_hosts_create_ceiling() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/extensions/omx/lib/friring-omx.mjs"
        ));
        const NEEDLE: &str = "export const CREATE_TIMEOUT_SECS = ";
        let at = source
            .find(NEEDLE)
            .expect("the omx client declares CREATE_TIMEOUT_SECS");
        let digits: String = source[at + NEEDLE.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let client: u64 = digits.parse().expect("CREATE_TIMEOUT_SECS is an integer");

        let ceiling = (2 * super::super::bridge_saga::BLOCKING_STEP_TIMEOUT
            + EGRESS_ACK_TIMEOUT
            + READY_TIMEOUT)
            .as_secs();
        assert!(
            client >= ceiling,
            "the omx client gives up after {client}s on a create friring may take {ceiling}s to \
             answer"
        );
    }

    /// A child has to be able to commit in the worktree it was given.
    ///
    /// Its index and `HEAD` are not in the worktree — a linked worktree holds a
    /// `.git` *file* pointing at `<repo>/.git/worktrees/<id>` — so a child
    /// granted only the worktree edits files it can never commit, and friring
    /// then reads the worktree as dirty and refuses to merge it. Observed
    /// exactly that way by `just omx-team-e2e`: `fatal: Unable to create
    /// '…/.git/worktrees/omx-demo-alpha/index.lock': Operation not permitted`.
    /// Lay out a linked worktree the way `git worktree add` does: a `.git`
    /// *file* in the worktree naming the metadata directory, and a `gitdir`
    /// record in that directory naming the `.git` file back. The second half is
    /// the host's, and is what [`super::child_git_dir`] decides on.
    fn link_worktree(repo: &std::path::Path, id: &str, worktree: &std::path::Path) -> PathBuf {
        let git_dir = repo.join(".git/worktrees").join(id);
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        std::fs::write(
            git_dir.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        git_dir
    }

    #[test]
    fn a_child_is_granted_the_git_directory_of_its_own_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");

        // A linked worktree: the marker file names the directory elsewhere.
        let linked = tmp.path().join("linked");
        let git_dir = link_worktree(&repo, "child", &linked);
        let real = std::fs::canonicalize(&git_dir).unwrap();
        assert_eq!(
            super::child_git_dir(&linked, &repo).as_deref(),
            Some(real.to_string_lossy().as_ref()),
            "a linked worktree's own git directory must be granted"
        );

        // An ordinary checkout keeps `.git` inside the worktree, which the
        // worktree grant already covers, so there is nothing extra to add.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(plain.join(".git")).unwrap();
        assert_eq!(super::child_git_dir(&plain, &repo), None);

        // And a directory that is not a worktree at all names nothing.
        let bare = tmp.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(super::child_git_dir(&bare, &repo), None);
    }

    /// The marker is inside the child's **own writable worktree**, so a child
    /// can rewrite it — and `own_dirs` is exempt from the parent-grant check.
    ///
    /// A pointer that resolves outside `<repo>/.git/worktrees` must therefore
    /// grant nothing at all. Without the containment check this is a sandbox
    /// escape a child performs on itself: write one line, ask its owner to
    /// resume it, and the next launch hands it that path read-write.
    #[test]
    fn a_child_cannot_point_its_git_marker_somewhere_else() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let worktree = tmp.path().join("worktree");
        // Properly registered first, so every case below fails on the pointer
        // rather than on there being no record at all.
        link_worktree(&repo, "child", &worktree);

        // Somewhere else entirely — the shape that matters.
        let elsewhere = tmp.path().join("secrets");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", elsewhere.display()),
        )
        .unwrap();
        assert_eq!(super::child_git_dir(&worktree, &repo), None);

        // The metadata root itself, which holds every sibling's directory.
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees").display()),
        )
        .unwrap();
        assert_eq!(super::child_git_dir(&worktree, &repo), None);

        // And a traversal back out of it.
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", repo.join(".git/worktrees/../..").display()),
        )
        .unwrap();
        assert_eq!(super::child_git_dir(&worktree, &repo), None);
    }

    /// The escape containment alone does not stop: a **sibling's** metadata is
    /// inside `<repo>/.git/worktrees` too.
    ///
    /// A child rewrites one line of its own `.git` file to name the directory
    /// next to its own and asks its owner to resume it. If the marker decided,
    /// the next launch would hand it that sibling's index, `HEAD` and refs
    /// read-write, with `own_dirs` exempt from every later check. The host's
    /// `gitdir` record is what refuses it.
    #[test]
    fn a_child_cannot_claim_a_siblings_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let mine = tmp.path().join("mine");
        let theirs = tmp.path().join("theirs");
        link_worktree(&repo, "mine", &mine);
        let sibling = link_worktree(&repo, "theirs", &theirs);

        // One line, inside the child's own writable worktree.
        std::fs::write(
            mine.join(".git"),
            format!("gitdir: {}\n", sibling.display()),
        )
        .unwrap();
        assert_eq!(
            super::child_git_dir(&mine, &repo),
            None,
            "a marker naming a sibling's metadata must grant nothing"
        );
        // The sibling is unaffected: its own record still names it.
        assert!(super::child_git_dir(&theirs, &repo).is_some());
    }

    /// The escape that survives the record check when the *marker* is a link.
    ///
    /// A child cannot write any record — `gitdir` is read-only inside its own
    /// metadata directory — but it owns the `.git` file in its worktree, and
    /// every `canonicalize` follows a link. Replacing that file with a symlink
    /// to a sibling's `.git` makes the host resolve this worktree to the
    /// sibling's, match the sibling's record, and grant the sibling's metadata,
    /// with no record ever being written.
    ///
    /// Unix-only because the escape needs a symlink to exist, and creating one
    /// on Windows takes a privilege a test runner does not have. The refusal
    /// itself is not: `child_git_dir` reads the marker's own file type on every
    /// platform.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_git_marker_grants_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let mine = tmp.path().join("mine");
        let theirs = tmp.path().join("theirs");
        link_worktree(&repo, "mine", &mine);
        let sibling = link_worktree(&repo, "theirs", &theirs);

        std::fs::remove_file(mine.join(".git")).unwrap();
        std::os::unix::fs::symlink(theirs.join(".git"), mine.join(".git")).unwrap();
        assert_eq!(
            super::child_git_dir(&mine, &repo),
            None,
            "a symlinked marker reached a sibling's metadata"
        );
        // And the sibling still has its own, so the refusal is about the link
        // rather than about the pair of worktrees existing.
        assert_eq!(
            super::child_git_dir(&theirs, &repo).as_deref(),
            Some(
                std::fs::canonicalize(&sibling)
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }

    /// Two records naming one worktree grant nothing, **whichever** of them the
    /// marker agrees with.
    ///
    /// Only friring writes a record now — a child's own `gitdir` is read-only —
    /// so this is a record something else wrote: a stale one after a `git
    /// worktree move`, or a repair. Both marker positions are exercised, because
    /// an implementation that took the first match and skipped the uniqueness
    /// check would still refuse the case where the marker names the *other*
    /// entry, and pass the case where it names the one enumeration found.
    #[test]
    fn two_records_naming_one_worktree_grant_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let mine = tmp.path().join("mine");
        let theirs = tmp.path().join("theirs");
        let stale = link_worktree(&repo, "mine", &mine);
        let real = link_worktree(&repo, "theirs", &theirs);

        std::fs::write(
            stale.join("gitdir"),
            format!("{}\n", theirs.join(".git").display()),
        )
        .unwrap();
        for named in [&real, &stale] {
            std::fs::write(
                theirs.join(".git"),
                format!("gitdir: {}\n", named.display()),
            )
            .unwrap();
            assert_eq!(
                super::child_git_dir(&theirs, &repo),
                None,
                "an ambiguous record must fail closed however the marker points"
            );
        }
        // And `mine` gains nothing: no record names its own worktree any more.
        assert_eq!(super::child_git_dir(&mine, &repo), None);
    }

    /// A worktree with no record of its own is not a worktree friring minted.
    ///
    /// The case a child reaches by deleting the record inside its own metadata
    /// directory, and the case a hand-made directory with a plausible `.git`
    /// file reaches. Both grant nothing.
    #[test]
    fn an_unrecorded_worktree_is_granted_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let worktree = tmp.path().join("wt");
        let git_dir = link_worktree(&repo, "wt", &worktree);
        std::fs::remove_file(git_dir.join("gitdir")).unwrap();
        assert_eq!(super::child_git_dir(&worktree, &repo), None);
    }

    /// Sharing `<repo>/.git` must not share the **siblings** inside it.
    ///
    /// A profile that shares the repository's git directory — which is what
    /// lets a child commit — otherwise hands every child write access to every
    /// other child's `.git/worktrees/<id>`: its index, its `HEAD`, its refs. A
    /// worker could rewrite what a sibling is about to commit, and friring's
    /// verdict would be reached over it. The subtraction is what stops that,
    /// and the child's own directory comes back after it.
    #[test]
    fn a_child_cannot_reach_a_siblings_worktree_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let worktree = tmp.path().join("wt");
        let mine = link_worktree(&repo, "mine", &worktree);
        link_worktree(&repo, "sibling", &tmp.path().join("sibling-wt"));

        let dirs = ChildDirs {
            worktree: worktree.clone(),
            scratch: tmp.path().join("scratch"),
            signal: tmp.path().join("signal"),
            bridge: tmp.path().join("bridge"),
            gate: tmp.path().join("gate"),
            state: tmp.path().join("state"),
        };
        let profile = crate::session::SandboxProfile::new("p", vec![]);
        let seed = crate::sandbox::child_state::SeedPlan {
            state_dir: dirs.state.clone(),
            source_dir: tmp.path().join("family"),
            steps: Vec::new(),
        };
        let overlay = super::child_overlay(&profile, "owner", &dirs, None, "/home/u", &seed, &repo);

        let siblings = repo.join(".git/worktrees").display().to_string();
        assert!(
            overlay
                .subtract
                .iter()
                .any(|e| e.path == siblings && e.is_dir),
            "the sibling metadata tree is not subtracted: {:?}",
            overlay.subtract
        );
        // And this child's own comes back after the subtraction.
        let real = std::fs::canonicalize(&mine).unwrap().display().to_string();
        assert!(
            overlay.own_dirs.contains(&real),
            "the child lost its own git directory: {:?}",
            overlay.own_dirs
        );
    }

    /// A protected name that does not exist is not protected at all under two
    /// of the three backends — so it is created before the grant, and a launch
    /// that cannot create it is refused.
    ///
    /// `config.worktree` is the one that matters: a child that can create it in
    /// a git directory it may write chooses `core.fsmonitor`, and the **host**
    /// runs that program the next time friring inspects the worktree. Under
    /// seatbelt the pathname deny already covers a file that is not there;
    /// bwrap's `--ro-bind-try` skips a missing source and a place mounts only
    /// what exists, so on those two the name is simply free.
    #[test]
    fn a_protected_name_that_does_not_exist_yet_is_created_before_the_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("repo/.git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let shared = git_dir.display().to_string();
        super::ensure_protected_placeholders(&shared).expect("the placeholder is created");
        for name in crate::sandbox::backend::PROTECTED_CREATED_IF_ABSENT {
            let path = git_dir.join(name);
            assert!(path.is_file(), "'{name}' was not created");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        }

        // The same for one worktree's metadata directory, which every bridge
        // child is granted whether the operator shares anything or not.
        let metadata = tmp.path().join("repo/.git/worktrees/child");
        std::fs::create_dir_all(&metadata).unwrap();
        super::ensure_protected_placeholders(&metadata.display().to_string()).unwrap();
        assert!(metadata.join("config.worktree").is_file());

        // An existing file is left exactly as it is: this creates a placeholder,
        // it does not reset a repository's configuration.
        std::fs::write(git_dir.join("config.worktree"), "[core]\n").unwrap();
        super::ensure_protected_placeholders(&shared).unwrap();
        assert_eq!(
            std::fs::read_to_string(git_dir.join("config.worktree")).unwrap(),
            "[core]\n"
        );

        // And a root that is neither shape has no such names, so nothing is
        // created inside somebody's ordinary directory.
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        super::ensure_protected_placeholders(&plain.display().to_string()).unwrap();
        assert!(!plain.join("config.worktree").exists());
    }

    /// And when it cannot be created, the launch is refused rather than started
    /// with a hole the backend cannot close.
    #[test]
    fn a_placeholder_that_cannot_be_created_refuses_the_launch() {
        let tmp = tempfile::tempdir().unwrap();
        // A git directory that is not a directory at all: `create_new` on a path
        // whose parent is a file fails, which is the shape a read-only
        // filesystem or a permission reaches by another route.
        let git_dir = tmp.path().join("repo/.git");
        std::fs::create_dir_all(git_dir.parent().unwrap()).unwrap();
        std::fs::write(&git_dir, "gitdir: elsewhere\n").unwrap();

        let err = super::ensure_protected_placeholders(&git_dir.display().to_string())
            .expect_err("a placeholder that cannot be created must refuse");
        assert!(
            err.contains("config.worktree"),
            "the refusal must name the path: {err}"
        );
    }
}
