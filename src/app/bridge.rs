//! The orchestration bridge's broker, on the TUI tick (ADR-30).
//!
//! A sandboxed agent writes a request file; this is what reads it, decides
//! whether it may, does the work, and writes the answer. It runs where the
//! authority is — in the friring process that owns the sessions, the profiles
//! and the database — and it is the only thing in the system that does.
//!
//! # It must never starve the render loop
//!
//! The broker shares its thread with `view(model)` (ADR-3), so every pass is
//! bounded rather than drained: at most [`REQUESTS_PER_TICK`] requests are
//! taken **from each session's queue**, at most one nudge is typed, and
//! anything that blocks — a worktree checkout, a `new-window`, a `git status`
//! — runs off the tick and is applied when a later tick polls its result. The
//! budget is per queue rather than global so that one flooding session cannot
//! starve another's, and a queue that hits it leaves the rest for the next
//! tick — which is why a client flooding its queue costs latency rather than
//! frames.
//!
//! # Authority is the row and the directory
//!
//! Every verb resolves its caller from **which queue** the request came out of,
//! and every verb that names a child checks the immutable `bridge_children` row.
//! `sessions.parent_session_id` is display only and is never asked;
//! `SendBody::to` is a name the broker resolves, never one it trusts.
//!
//! # Child-authored text is data
//!
//! A summary, a report, a `blocked` question: bounded, labelled, and never
//! interpolated into a command, a query, a path, a nudge or an OS notification.
//! The nudge is [`BRIDGE_NUDGE`], one exact literal with nothing formatted into
//! it.

use std::collections::HashMap;
use std::time::Duration;

use crate::cli::messages::BRIDGE_NUDGE;
use crate::paths::TakenRequest;
use crate::session::bridge::{
    ErrorCode, InboxBody, MailDirection, MailKind, ReportBody, Request, RequestKey, Response,
    ResultBody, SendBody, Verb, BRIDGE_PROTOCOL, MAX_INBOX_LIMIT, MAX_MAIL_BODY_BYTES,
    MAX_SUMMARY_BYTES, REPORT_PHASES,
};
use crate::session::{BridgeCapability, ChildState, SessionId};
use crate::storage::bridge::{JournalLookup, RequestState};
use crate::storage::messages::NewMessage;

use super::bridge_spawn::LifecycleAnswer;
use super::App;

/// How many requests one tick may take from one session's queue.
///
/// Four, because the work behind a `status` is a handful of indexed reads and
/// the work behind a `create` leaves the tick entirely. A larger budget would
/// buy throughput a leader cannot use and cost frames a user can see.
pub(crate) const REQUESTS_PER_TICK: usize = 4;

/// How often the broker polls at all, in ticks.
///
/// The bridge is a request/response channel with a human-scale client on the
/// other end, so a poll every few ticks is indistinguishable from one every
/// tick — and the difference is a filesystem read per session per tick that
/// almost always finds nothing.
pub(super) const POLL_TICKS: u64 = 3;

/// How often the lease is renewed and `.taking` is swept, in ticks.
const HOUSEKEEPING_TICKS: u64 = 10;

/// How long a broker lease lasts.
///
/// Three times the renewal interval, so an instance that misses a tick or two
/// under load does not hand its sessions to another instance mid-request.
const LEASE_MILLIS: u64 = 30_000;

/// How long a taken request may sit in `.taking` before it is answered
/// `expired` rather than acted on.
const TAKING_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// How long an unacknowledged response file is kept.
pub(super) const RESPONSE_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// How many sessions one housekeeping pass sweeps for stale responses.
///
/// The response GC is the one piece of per-session bridge work a **ghost**
/// still needs: nothing else removes an answer a client never acknowledged, so
/// skipping unloaded sessions the way the queue poll does would mean their
/// `res/` directories never shrink. It does not need the housekeeping cadence
/// to do it, though — the bound it enforces is 24 hours. So the fleet is swept
/// as a rotating slice, which makes the per-pass cost constant in the number of
/// stored sessions instead of linear in it: at this batch a 400-session fleet
/// comes back round in about half a minute, against a day-long bound.
pub(super) const RESPONSE_GC_BATCH: usize = 4;

/// The shortest gap between two nudges to one recipient.
///
/// A nudge is typed into a live agent's prompt, so it interrupts a turn. New
/// mail arriving inside the window coalesces into the next one rather than
/// queueing a second interruption.
const NUDGE_INTERVAL: Duration = Duration::from_secs(60);

/// How many nudges a recipient may be sent with no bridge call in return before
/// friring stops and says so.
///
/// Not a verdict — a long turn looks exactly like this — which is why it raises
/// attention rather than a terminal state. What it prevents is friring typing
/// into a pane forever at an agent that is never going to answer.
pub(super) const MAX_UNANSWERED_NUDGES: u32 = 5;

/// How often the owed-nudge set is rebuilt from the mailbox, in ticks.
///
/// A multiple of `POLL_TICKS * HOUSEKEEPING_TICKS`, so it lands on a
/// housekeeping pass: ~3 s at the tick rate, which is well inside
/// [`NUDGE_INTERVAL`] — the reconcile only has to beat the rate limiter, not
/// the mail.
///
/// It exists because the record that a nudge is owed lives in memory and the
/// mail it is about does not. A restart, a handover between two friring
/// instances on one database, or a recipient that simply was not in this
/// instance's session list when its mail arrived all lose the record — and
/// [`App::owe_bridge_nudge`] recreates one only when **new** mail arrives, so
/// the mail already queued would never be announced to anybody. The mailbox is
/// the durable half of the same fact, so the debt is re-derived from it.
const NUDGE_RECONCILE_TICKS: u64 = 300;

/// How many owed-nudge records this instance keeps before it drops the ones it
/// cannot act on.
///
/// Comfortably past any fleet that is actually being nudged — the population
/// that matters is the loaded bridge sessions — so in normal running nothing is
/// ever dropped. It is a ceiling on the pathological case, not a working limit:
/// see [`App::reconcile_owed_nudges`].
const MAX_NUDGE_RECORDS: usize = 512;

/// What the broker remembers between ticks.
///
/// In memory rather than in a row, because every field is about *this
/// instance's* recent behaviour: how long since it nudged, how many nudges it
/// has sent with nothing back. A restart legitimately starts over — the
/// recipient's own state is in the database, and a fresh instance re-earns its
/// rate limit rather than inheriting one.
#[derive(Debug, Default)]
pub(crate) struct BridgeState {
    /// Per recipient: when it was last nudged, and how many nudges have gone
    /// unanswered.
    nudges: HashMap<SessionId, NudgeRecord>,
    /// This instance's identity for the broker lease. Minted on first use.
    instance_id: Option<String>,
    /// The response GC's roster, re-read from disk once per full cycle, and
    /// where in it the next slice starts. Held between passes so the rotating
    /// slice [`RESPONSE_GC_BATCH`] takes moves on rather than re-sweeping the
    /// same head of the fleet for ever.
    gc_keys: Vec<String>,
    gc_cursor: usize,
}

/// What serving one request came to.
///
/// Most verbs answer on the tick that took them. The three lifecycle verbs do
/// not: a `create` is a worktree, a window and a wait for the child's own hook,
/// so it is **accepted** and the saga writes both the journal entry and the
/// response file when it reaches a final step. Until then nothing is written,
/// and the client — which is polling the response directory — simply waits.
enum Served {
    Answer(Response),
    Deferred,
}

#[derive(Debug, Clone)]
struct NudgeRecord {
    at: std::time::Instant,
    unanswered: u32,
    /// Whether friring has already told the owner this child looks stalled, so
    /// it says so once rather than every tick.
    reported_stalled: bool,
}

impl App {
    /// Serve the bridge for every session that has one — the tick entry point.
    ///
    /// Ordered so the cheap gate comes first: most friring runs have no bridge
    /// at all, and those pay one modulo and one empty vector.
    ///
    /// # Two session sets, not one (ADR-P16)
    ///
    /// `stored` is every session with a bridge; `live` is the subset something
    /// could actually be writing to. A **placeholder** has no agent process on
    /// this host — a ghost is deliberately unloaded, an unreachable remote never
    /// started here — so by construction nothing is filling its `req/`, and a
    /// poll of it can only ever find the directory empty. Everything driven by a
    /// client goes to `live`; only the response GC, which is friring's own
    /// bookkeeping about files friring wrote, still walks `stored`.
    ///
    /// It is self-healing: loading a session clears the flag and the very next
    /// pass polls it normally, without waiting for a lease or a housekeeping
    /// tick — an absent or lapsed lease reads as "nobody holds it".
    pub(crate) fn tick_bridge(&mut self) {
        if self.metrics.tick_count % POLL_TICKS != 0 {
            return;
        }
        let mut stored: Vec<SessionId> = Vec::new();
        let mut live: Vec<SessionId> = Vec::new();
        for session in self
            .sessions
            .iter()
            .filter(|s| s.info.sandbox_profile.is_some())
        {
            stored.push(session.info.id);
            if !session.is_placeholder() {
                live.push(session.info.id);
            }
        }
        if stored.is_empty() {
            return;
        }
        if self.metrics.tick_count % HOUSEKEEPING_TICKS == 0 {
            self.bridge_housekeeping(&live);
            self.mirror_child_states(&live);
            self.sweep_bridge_responses();
        }
        if self.metrics.tick_count % NUDGE_RECONCILE_TICKS == 0 {
            self.reconcile_owed_nudges(&live);
        }
        self.serve_bridge_queues(&live);
        self.tick_bridge_nudges();
    }

    /// Take a bounded batch from every live session's queue, in one pass.
    ///
    /// The staging directory is minted **once** here rather than once inside
    /// each take: it is one shared directory for every session, and re-minting
    /// it per session cost a `symlink_metadata` + `mkdir` + `chmod` per session
    /// per poll — the largest single leaf under this tick on a real fleet. Its
    /// security properties are unchanged, because the call is unchanged; only
    /// how often it is made is (ADR-P16).
    ///
    /// A pass that cannot mint the directory serves nothing: a take is a
    /// `rename` into it, so without it there is nowhere to move a request that
    /// is out of the agent's reach.
    fn serve_bridge_queues(&mut self, live: &[SessionId]) {
        if live.is_empty() {
            return;
        }
        let Some(taking) = crate::paths::mint_bridge_taking_dir() else {
            return;
        };
        for session in live {
            self.serve_bridge_queue(*session, &taking);
        }
    }

    /// Renew this instance's leases, sweep `.taking`, and drop stale answers.
    ///
    /// The lease keeps two running TUIs from both polling one directory. It is a
    /// courtesy, not the guard: the guard is the `rename(2)` a take makes, which
    /// exactly one process can win whatever the leases say.
    ///
    /// Only the sessions this instance actually polls are claimed. A lease over
    /// a session nobody is serving buys nothing and denies it to an instance
    /// that has the session loaded, so letting it lapse is the correct outcome
    /// (ADR-P16).
    fn bridge_housekeeping(&mut self, live: &[SessionId]) {
        let instance = self.bridge_instance_id();
        for session in live {
            let _ = self
                .db
                .claim_broker_lease(&session.to_string(), &instance, LEASE_MILLIS);
        }
        // Nothing left in `.taking` is ever silently dropped: each file has a
        // client still polling for an answer that will only exist if something
        // produces one.
        for taken in crate::paths::recover_taken_requests(TAKING_MAX_AGE) {
            self.answer_taken_request(taken);
        }
    }

    /// Drop unacknowledged responses for one rotating slice of the fleet.
    ///
    /// A `res/` file is one friring wrote and a client never read, so nothing
    /// but this removes it and a directory nobody is serving would otherwise
    /// only ever grow. What the slice changes is the cadence, not the reach —
    /// see [`RESPONSE_GC_BATCH`].
    ///
    /// The roster is the **disk**, not the session list, and that is the
    /// difference between reaching every case and reaching most of them: a
    /// cleanly stopped child is retired from `sessions` while its channel stays
    /// (ADR-32), and a session deleted while friring was not running leaves a
    /// directory no list mentions. It is re-read once per full cycle, so the
    /// per-pass cost is the slice and nothing else.
    fn sweep_bridge_responses(&mut self) {
        if self.bridge.gc_cursor >= self.bridge.gc_keys.len() {
            self.bridge.gc_keys = crate::paths::bridge_session_keys();
            self.bridge.gc_cursor = 0;
        }
        if self.bridge.gc_keys.is_empty() {
            return;
        }
        let end = (self.bridge.gc_cursor + RESPONSE_GC_BATCH).min(self.bridge.gc_keys.len());
        for key in &self.bridge.gc_keys[self.bridge.gc_cursor..end] {
            crate::paths::prune_bridge_responses(key, RESPONSE_MAX_AGE);
        }
        self.bridge.gc_cursor = end;
        self.metrics.bump(|p| &mut p.bridge_response_sweeps);
    }

    /// Carry a running child's hook state into its lifecycle state.
    ///
    /// `working` and `blocked` are the two the owner's `status` needs and that
    /// nothing else writes: the saga sets `ready` once and then leaves, and every
    /// other transition is a verdict friring reaches deliberately. So only the
    /// three interchangeable running states move here, and a child that has
    /// reached anything else — `finishing`, `dirty`, a terminal state — is left
    /// exactly where the host put it. A hook cannot talk friring out of a verdict.
    fn mirror_child_states(&mut self, sessions: &[SessionId]) {
        for session in sessions {
            let child_id = session.to_string();
            let Ok(Some(row)) = self.db.bridge_child_state(&child_id) else {
                continue;
            };
            if !matches!(
                row.state,
                ChildState::Ready | ChildState::Working | ChildState::Blocked
            ) {
                continue;
            }
            let Some(status) = self
                .sessions
                .iter()
                .find(|s| s.info.id == *session)
                .map(|s| s.info.status)
            else {
                continue;
            };
            let next = match status {
                crate::session::SessionStatus::Working => ChildState::Working,
                crate::session::SessionStatus::Blocked => ChildState::Blocked,
                crate::session::SessionStatus::Idle | crate::session::SessionStatus::Done => {
                    ChildState::Ready
                }
                _ => continue,
            };
            if next != row.state {
                let _ = self.db.set_bridge_child_state(&child_id, next);
            }
        }
    }

    /// This instance's identity for the broker lease.
    ///
    /// Per process and per run: two friring processes on one machine must not
    /// look like the same lease holder, and a restarted one must not inherit a
    /// lease it is not renewing.
    pub(super) fn bridge_instance_id(&mut self) -> String {
        self.bridge
            .instance_id
            .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone()
    }

    /// Take and answer a bounded batch from one session's queue.
    ///
    /// `taking` is the pass's staging directory, minted once by
    /// [`Self::serve_bridge_queues`].
    fn serve_bridge_queue(&mut self, session: SessionId, taking: &std::path::Path) {
        let key = session.to_string();
        // Only the lease holder takes. The rename below is the real guard, so
        // this costs a lost tick rather than correctness when two instances
        // disagree about who holds it.
        let instance = self.bridge_instance_id();
        if self
            .db
            .broker_lease_holder(&key)
            .ok()
            .flatten()
            .is_some_and(|holder| holder != instance)
        {
            return;
        }
        self.metrics.bump(|p| &mut p.bridge_queue_polls);
        let (taken, refused) =
            crate::paths::take_bridge_requests_in(taking, &key, REQUESTS_PER_TICK);
        for refusal in refused {
            match refusal {
                // A queue over quota is the operator's problem, not a debug
                // line: an agent is writing faster than friring can answer, and
                // its later requests are sitting unread. Rate-limited to the
                // housekeeping cadence so a flood does not become a toast
                // flood.
                crate::paths::TakeRefusal::Quota => {
                    if self.metrics.tick_count % HOUSEKEEPING_TICKS == 0 {
                        tracing::warn!(
                            session = %key,
                            "bridge: this session has more unanswered requests than the \
                             protocol holds; the excess is waiting unread"
                        );
                    }
                }
                other => {
                    tracing::debug!(session = %key, "bridge: refused a request file: {other:?}");
                }
            }
        }
        for request in taken {
            self.answer_taken_request(request);
        }
    }

    /// Decide one request and write its answer.
    ///
    /// The journal comes first and is what makes a retry safe: the same key with
    /// the same body returns the first attempt's **exact bytes** rather than
    /// doing the work twice, and the same key with a different body is refused
    /// `key_reused` with no effect at all.
    fn answer_taken_request(&mut self, taken: TakenRequest) {
        let Ok(caller) = taken.session_key.parse::<SessionId>() else {
            // A queue directory whose name is not a session id belongs to
            // nothing friring can attribute a request to, so there is no caller
            // to check and nothing to answer.
            crate::paths::finish_taken_request(&taken);
            return;
        };
        let parsed: Result<Request, String> =
            serde_json::from_str(&taken.text).map_err(|e| e.to_string());
        let key = match RequestKey::new(taken.key.clone()) {
            Ok(key) => key,
            Err(_) => {
                crate::paths::finish_taken_request(&taken);
                return;
            }
        };

        let served = match parsed {
            // A body friring cannot read is refused rather than guessed at:
            // `deny_unknown_fields` means an unrecognised field is a request
            // that does not mean here what it meant where it was written.
            Err(detail) => Served::Answer(Response::refused(
                key.clone(),
                ErrorCode::Failed,
                format!("this request could not be read: {detail}"),
            )),
            Ok(request) if request.protocol != BRIDGE_PROTOCOL => {
                Served::Answer(Response::refused(
                    key.clone(),
                    ErrorCode::Failed,
                    format!(
                        "this friring speaks bridge protocol {BRIDGE_PROTOCOL}; the request says \
                         {}",
                        request.protocol
                    ),
                ))
            }
            // The key in the body and the key in the filename must agree, or a
            // client could get one request journaled under another's key.
            Ok(request) if request.key != key => Served::Answer(Response::refused(
                key.clone(),
                ErrorCode::Failed,
                "the request's key does not match the file it arrived in".to_string(),
            )),
            Ok(request) => self.dispatch_bridge_request(caller, request),
        };

        // A deferred request gets **no** response file now: the client is
        // polling for one, and the saga writes it when it finishes. Writing an
        // interim answer would end that wait with a non-answer.
        if let Served::Answer(response) = served {
            self.write_bridge_answer(&taken, &response);
        }
        crate::paths::finish_taken_request(&taken);
        self.metrics_bridge_served();
    }

    /// Journal, authorize, and run one well-formed request.
    fn dispatch_bridge_request(&mut self, caller: SessionId, request: Request) -> Served {
        // Any bridge call resets the unanswered-nudge **count**: the rule the
        // stall watch enforces is that the agent is still talking to friring,
        // not that it read its mail. A `status` or a `report` answers a nudge as
        // much as an `inbox --claim` does.
        //
        // The count, and not the record. The record is the only trace that a
        // nudge is *owed*, and `owe_bridge_nudge` recreates it only when new
        // mail arrives — so dropping it here would lose the reminder for mail
        // this caller has left unclaimed, and with it the stall signal for a
        // recipient that answers every other verb and never reads its inbox.
        if let Some(record) = self.bridge.nudges.get_mut(&caller) {
            record.unanswered = 0;
            record.reported_stalled = false;
        }
        let key = request.key.clone();
        let owner = caller.to_string();
        // A deadline governs whether friring *starts* work, never whether it may
        // repeat the answer it already gave. A key with a journal row is left to
        // the replay below: refusing it `expired` would destroy the only record
        // a caller that timed out once has of the child it already created.
        let journaled = self
            .db
            .bridge_request(&owner, key.as_str())
            .ok()
            .flatten()
            .is_some();
        if let (false, Some(deadline)) = (journaled, request.deadline) {
            if crate::sync::state::current_time_millis() > deadline {
                return Served::Answer(Response::refused(
                    key,
                    ErrorCode::Expired,
                    "this request's deadline had passed before friring reached it",
                ));
            }
        }
        let hash = body_hash(&request);
        match self.db.take_bridge_request(
            &owner,
            key.as_str(),
            request.verb.as_str(),
            &hash,
            request.deadline,
        ) {
            Ok(JournalLookup::Replay(row)) => {
                return match row
                    .response
                    .as_deref()
                    .map(serde_json::from_str::<Response>)
                {
                    // The stored bytes, verbatim: a re-rendered answer could
                    // differ in field order and read as a second request.
                    Some(Ok(stored)) => Served::Answer(stored),
                    // Taken and not yet answered. A lifecycle verb is *still
                    // running* — the saga owns the answer — so the caller is
                    // left waiting rather than handed an interim refusal that
                    // would end its wait.
                    _ if matches!(request.verb, Verb::Create | Verb::Stop | Verb::Resume) => {
                        Served::Deferred
                    }
                    _ => Served::Answer(Response::refused(
                        key,
                        ErrorCode::Failed,
                        "this request is still being served; poll again with the same key",
                    )),
                };
            }
            Ok(JournalLookup::KeyReused) => {
                return Served::Answer(Response::refused(
                    key,
                    ErrorCode::KeyReused,
                    "this key was already used for a different request, so friring did nothing",
                ))
            }
            Ok(JournalLookup::Fresh) => {}
            Err(e) => {
                return Served::Answer(Response::refused(
                    key,
                    ErrorCode::Failed,
                    format!("friring could not journal this request: {e}"),
                ))
            }
        }

        let response = match self.authorize(caller, request.verb) {
            Err(refusal) => Response::refused(key.clone(), refusal.0, refusal.1),
            Ok(()) => match self.run_bridge_verb(caller, &request) {
                // The saga journals and answers this one when it finishes; the
                // journal entry stays `accepted` until then, which is what makes
                // a client's retry with the same key wait rather than start a
                // second child.
                Served::Deferred => return Served::Deferred,
                Served::Answer(response) => response,
            },
        };
        let state = if response.ok {
            RequestState::Done
        } else {
            RequestState::Failed
        };
        let encoded = serde_json::to_string(&response).unwrap_or_default();
        if let Err(e) = self
            .db
            .finish_bridge_request(&owner, key.as_str(), state, &encoded)
        {
            tracing::warn!("bridge: could not journal the answer to '{key}': {e}");
        }
        Served::Answer(response)
    }

    /// Whether this caller may use this verb at all.
    ///
    /// Two questions, in order. The **capability** comes from the caller's own
    /// sandbox profile, so a session whose profile grants nothing has no bridge
    /// however its agent is configured. The **depth** rule is structural rather
    /// than a counter: a session that is itself a child never holds
    /// `child-lifecycle`, so orchestration is one level deep by construction and
    /// there is no depth to miscount.
    fn authorize(&self, caller: SessionId, verb: Verb) -> Result<(), (ErrorCode, String)> {
        let needed = verb.requires();
        let granted = self.bridge_grants(caller);
        if needed == BridgeCapability::ChildLifecycle && self.is_bridge_child(caller) {
            return Err((
                ErrorCode::DepthExceeded,
                "a bridge child may not create, stop or resume children; orchestration is one \
                 level deep"
                    .to_string(),
            ));
        }
        if !granted.contains(&needed) {
            return Err((
                ErrorCode::GrantMissing,
                format!(
                    "this session's sandbox profile does not grant '{needed}', which '{verb}' \
                     needs"
                ),
            ));
        }
        Ok(())
    }

    /// What the caller's profile grants, intersected with what a **child** may
    /// ever hold.
    ///
    /// A child's effective grant is `{mailbox, report} ∩ owner grants` — never
    /// `child-lifecycle`, whatever the profile says. The profile is shared by
    /// the owner and its children (the child launches under a narrowed copy of
    /// it), so the intersection is where the depth rule is enforced rather than
    /// in a second profile nobody wrote.
    fn bridge_grants(&self, caller: SessionId) -> Vec<BridgeCapability> {
        let Some(profile) = self
            .sessions
            .iter()
            .find(|s| s.info.id == caller)
            .and_then(|s| s.info.sandbox_profile.clone())
        else {
            return Vec::new();
        };
        let Ok(Some(stored)) = self.db.get_sandbox_profile(&profile) else {
            return Vec::new();
        };
        // A profile friring cannot fully decode grants nothing: an undecoded
        // column reads as the narrowest value, and a bridge grant is exactly the
        // kind of thing that must not be guessed at.
        if !stored.is_intact() {
            return Vec::new();
        }
        let granted = stored.profile.bridge_grants;
        if self.is_bridge_child(caller) {
            return granted
                .into_iter()
                .filter(|cap| BridgeCapability::CHILD_MAX.contains(cap))
                .collect();
        }
        granted
    }

    /// Whether this session is somebody's bridge child.
    ///
    /// Asked of the immutable ownership row, never of
    /// `sessions.parent_session_id`: that column is display only and a user can
    /// change it, so a verb that consulted it would be a verb a user could
    /// redirect.
    ///
    /// Also what keeps a child off every **generic relaunch** path (ADR-32): the
    /// narrowing, gate and private state that make it a child are rebuilt by the
    /// spawn saga and by nothing else, so startup restore and `Ctrl+R` ask this
    /// and decline.
    ///
    /// A lookup that **fails** reads as "child". Every caller treats `true` as
    /// the narrower answer — one less level of nesting, the child capability
    /// intersection, no `Ctrl+R`, no startup restore — so an unreadable
    /// `bridge_children` costs a refusal a user can retry, where a `false` would
    /// silently relaunch a child under its owner's un-narrowed profile.
    pub(crate) fn is_bridge_child(&self, session: SessionId) -> bool {
        !matches!(self.db.bridge_child(&session.to_string()), Ok(None))
    }

    /// Run one authorized verb.
    ///
    /// Every body is read against the **verb** rather than matched on whichever
    /// arm the untagged enum picked — see [`Request::body_as`]. The verb is what
    /// the journal recorded and what authorization was checked against, so it is
    /// what decides what the request said.
    fn run_bridge_verb(&mut self, caller: SessionId, request: &Request) -> Served {
        let key = request.key.clone();
        let mismatch = |detail: String| Response::refused(key.clone(), ErrorCode::Failed, detail);
        let answer = match request.verb {
            Verb::Status => self.bridge_status(caller, key),
            Verb::Inbox => match request.body_as::<InboxBody>() {
                Ok(body) => self.bridge_inbox(caller, key, &body),
                Err(detail) => mismatch(detail),
            },
            Verb::Send => match request.body_as::<SendBody>() {
                Ok(body) => self.bridge_send(caller, key, &body),
                Err(detail) => mismatch(detail),
            },
            Verb::Report => match request.body_as::<ReportBody>() {
                Ok(body) => self.bridge_report(caller, key, &body),
                Err(detail) => mismatch(detail),
            },
            Verb::Create | Verb::Stop | Verb::Resume => {
                return match self.run_child_lifecycle(caller, request) {
                    LifecycleAnswer::Now(response) => Served::Answer(*response),
                    LifecycleAnswer::Accepted => Served::Deferred,
                }
            }
        };
        Served::Answer(answer)
    }

    /// `status` — the caller's own state, and its children's when it owns any.
    ///
    /// Answered from cached state and indexed reads: a `status` is what a
    /// polling leader calls most, and it must not be the verb that costs a
    /// frame.
    fn bridge_status(&mut self, caller: SessionId, key: RequestKey) -> Response {
        let own = self.bridge_self_view(caller);
        let children: Vec<serde_json::Value> = self
            .db
            .bridge_children_of(&caller.to_string())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|child| self.bridge_child_view(&child.child_id))
            .collect();
        Response::ok(
            key,
            Some(serde_json::json!({
                "session": own,
                "children": children,
            })),
        )
    }

    /// What a caller may know about itself.
    fn bridge_self_view(&self, caller: SessionId) -> serde_json::Value {
        let session = self.sessions.iter().find(|s| s.info.id == caller);
        let owner = self
            .db
            .bridge_child(&caller.to_string())
            .ok()
            .flatten()
            .map(|child| child.owner_id);
        serde_json::json!({
            "id": caller.to_string(),
            "name": session.map(|s| s.info.name.clone()),
            "agent": session.map(|s| s.info.agent.clone()),
            "state": self
                .db
                .bridge_child_state(&caller.to_string())
                .ok()
                .flatten()
                .map(|row| row.state.as_str()),
            // The owner's *name*, never its directory or its id's meaning: a
            // child needs to know who to write to, which the broker resolves.
            "owner": owner.and_then(|id| {
                id.parse::<SessionId>()
                    .ok()
                    .and_then(|id| self.sessions.iter().find(|s| s.info.id == id))
                    .map(|s| s.info.name.clone())
            }),
            "unread": self.db.count_unread_messages(caller).unwrap_or(0),
            "egress_state": session.map(|s| s.info.egress_state.label()),
        })
    }

    /// What an owner may know about one of its children.
    ///
    /// Host-known fields and the host's own verdict. The child's last report is
    /// included and is labelled `child_authored`, because an owner deciding what
    /// to do next needs it and must not mistake it for something friring
    /// verified.
    fn bridge_child_view(&self, child_id: &str) -> Option<serde_json::Value> {
        let state = self.db.bridge_child_state(child_id).ok().flatten()?;
        let session = child_id
            .parse::<SessionId>()
            .ok()
            .and_then(|id| self.sessions.iter().find(|s| s.info.id == id));
        let result = self.db.bridge_result(child_id).ok().flatten();
        let report = self
            .db
            .bridge_reports(child_id, 1)
            .unwrap_or_default()
            .into_iter()
            .next();
        Some(serde_json::json!({
            "id": child_id,
            "name": session.map(|s| s.info.name.clone()),
            "agent": session.map(|s| s.info.agent.clone()),
            "state": state.state.as_str(),
            "hook_state": session.map(|s| s.info.status.to_string()),
            "is_dead": session.is_none(),
            "age_ms": crate::sync::state::current_time_millis()
                .saturating_sub(state.updated_at),
            "unread": child_id
                .parse::<SessionId>()
                .ok()
                .and_then(|id| self.db.count_unread_messages(id).ok())
                .unwrap_or(0),
            "egress_state": session.map(|s| s.info.egress_state.label()),
            "result": result.map(|r| serde_json::json!({
                "outcome": r.outcome.as_str(),
                "branch": r.branch,
                "head": r.head,
                "dirty": r.dirty,
                "ahead_of_base": r.ahead_of_base,
                "verified_at": r.verified_at,
            })),
            "last_report": report.map(|r| serde_json::json!({
                "phase": r.phase,
                "progress": r.progress,
                // Labelled at the boundary it crosses, not where it is
                // rendered: an integration step reading this must not mistake
                // the child's own words for friring's verdict.
                "summary": r.summary,
                "child_authored": true,
                "needs_operator": r.needs_operator,
                "created_at": r.created_at,
            })),
        }))
    }

    /// `inbox` — the caller's **own** mail and nobody else's.
    ///
    /// The recipient is the caller, resolved from the queue the request came out
    /// of. There is no parameter for it and no way to ask for another session's:
    /// a mailbox verb that took a recipient would be a read of any session's
    /// mail by anything holding a bridge.
    fn bridge_inbox(&mut self, caller: SessionId, key: RequestKey, body: &InboxBody) -> Response {
        let limit = body.limit.unwrap_or(MAX_INBOX_LIMIT).min(MAX_INBOX_LIMIT);
        let messages = if body.claim {
            self.db.claim_messages(caller, Some(limit))
        } else {
            self.db.list_messages(caller, true, Some(limit))
        };
        match messages {
            Ok(messages) => {
                if body.claim {
                    // Recorded separately from the nudge reset (which every
                    // bridge call earns): this is the last time the child took
                    // delivery of its mail, which an owner can read back.
                    let _ = self.db.touch_bridge_child_claim(&caller.to_string());
                }
                Response::ok(
                    key,
                    Some(serde_json::json!({
                        "messages": messages
                            .iter()
                            .map(|m| serde_json::json!({
                                "id": m.id,
                                "kind": m.kind,
                                "body": m.body,
                                "from": m.from_session_id.map(|id| id.to_string()),
                                "created_at": m.created_at,
                            }))
                            .collect::<Vec<_>>(),
                    })),
                )
            }
            Err(e) => Response::refused(
                key,
                ErrorCode::Failed,
                format!("friring could not read this session's mail: {e}"),
            ),
        }
    }

    /// `send` — to the caller's owner, or to a child it owns.
    ///
    /// Three checks and none of them trusts the request: the **recipient** is
    /// resolved through the ownership rows, the **sender** is forced to the
    /// caller whatever the body says, and the **kind** must be one this
    /// direction allows. A child that could send `task` would be assigning work
    /// to its owner; one that could send `child.done` would be reporting a
    /// verdict only the host may reach.
    fn bridge_send(&mut self, caller: SessionId, key: RequestKey, body: &SendBody) -> Response {
        let kind: MailKind = match body.kind.parse() {
            Ok(kind) => kind,
            Err(detail) => return Response::refused(key, ErrorCode::Failed, detail),
        };
        if !kind.is_sendable_by_caller() {
            return Response::refused(
                key,
                ErrorCode::Failed,
                format!("'{kind}' is a message friring sends, not one a session may send"),
            );
        }
        if body.body.len() > MAX_MAIL_BODY_BYTES {
            return Response::refused(
                key,
                ErrorCode::Quota,
                format!("a message body is at most {MAX_MAIL_BODY_BYTES} bytes"),
            );
        }
        let (recipient, direction) = match self.resolve_bridge_recipient(caller, &body.to) {
            Ok(resolved) => resolved,
            Err(refusal) => return Response::refused(key, refusal.0, refusal.1),
        };
        if kind.direction() != direction {
            return Response::refused(
                key,
                ErrorCode::Failed,
                format!("'{kind}' may not be sent in this direction"),
            );
        }
        // The one finish intent has a shape, and a `result` that is not it
        // starts no quiesce: "the child asked to finish" is the message that
        // must not be inferred from free text.
        let mut intent = None;
        if kind == MailKind::Result {
            match serde_json::from_str::<ResultBody>(&body.body) {
                Ok(parsed) if parsed.summary.len() <= MAX_SUMMARY_BYTES => {
                    intent = Some(parsed.outcome);
                }
                Ok(_) => {
                    return Response::refused(
                        key,
                        ErrorCode::Quota,
                        format!("a result summary is at most {MAX_SUMMARY_BYTES} bytes"),
                    )
                }
                Err(detail) => {
                    return Response::refused(
                        key,
                        ErrorCode::Failed,
                        format!(
                            "a 'result' body must be {{\"outcome\":\"completed|failed\", \
                             \"summary\":\"…\"}}: {detail}"
                        ),
                    )
                }
            }
        }
        let enqueued = self.db.enqueue_message_capped(
            &NewMessage {
                to_session_id: recipient,
                // Forced to the caller: a sender field the request could set
                // would let one session put mail in another's inbox under a
                // third's name.
                from_session_id: Some(caller),
                from_task_id: None,
                kind: kind.as_str().to_string(),
                body: body.body.clone(),
                in_reply_to: None,
            },
            self.bridge_unread_cap(recipient),
        );
        match enqueued {
            Ok(id) => {
                self.owe_bridge_nudge(recipient);
                // The one finish intent, accepted the moment its mail is
                // durable. Accepting it is not a verdict: the host stops the
                // pane, inspects the worktree and decides (ADR-32).
                if let Some(outcome) = intent {
                    self.accept_finish_intent(caller, outcome, Some(id));
                }
                Response::ok(key, Some(serde_json::json!({ "message_id": id })))
            }
            // Never a silent loss: a full inbox is a refusal the sender can act
            // on, where a dropped message is one nobody ever learns about.
            Err(e) => Response::refused(
                key,
                ErrorCode::Quota,
                format!("friring could not enqueue this message: {e}"),
            ),
        }
    }

    /// Resolve `to` into a session, and say which direction that is.
    ///
    /// `owner` is the caller's own owner from the immutable row; anything else
    /// must be a child the caller owns. There is no third case: a name that is
    /// neither is `unknown_child` or `not_owner`, never a session friring found
    /// some other way.
    fn resolve_bridge_recipient(
        &self,
        caller: SessionId,
        to: &str,
    ) -> Result<(SessionId, MailDirection), (ErrorCode, String)> {
        if to == "owner" {
            let owner = self
                .db
                .bridge_child(&caller.to_string())
                .ok()
                .flatten()
                .ok_or((
                    ErrorCode::NotOwner,
                    "this session has no owner to write to".to_string(),
                ))?
                .owner_id;
            let owner = owner.parse::<SessionId>().map_err(|_| {
                (
                    ErrorCode::Failed,
                    "this session's owner id is not readable".to_string(),
                )
            })?;
            return Ok((owner, MailDirection::ChildToOwner));
        }
        let child = self
            .db
            .bridge_child(to)
            .ok()
            .flatten()
            .ok_or((ErrorCode::UnknownChild, format!("no child '{to}'")))?;
        if child.owner_id != caller.to_string() {
            return Err((
                ErrorCode::NotOwner,
                format!("child '{to}' is not this session's"),
            ));
        }
        let id = child.child_id.parse::<SessionId>().map_err(|_| {
            (
                ErrorCode::Failed,
                format!("child '{to}' has an unreadable id"),
            )
        })?;
        Ok((id, MailDirection::OwnerToChild))
    }

    /// `report` — a bounded progress note about the caller itself.
    fn bridge_report(&mut self, caller: SessionId, key: RequestKey, body: &ReportBody) -> Response {
        let validated = match validate_report(body) {
            Ok(validated) => validated,
            Err(detail) => return Response::refused(key, ErrorCode::Failed, detail),
        };
        let child_id = caller.to_string();
        let seq = self.db.next_report_seq(&child_id).unwrap_or(1);
        let record = crate::session::BridgeReport {
            child_id: child_id.clone(),
            seq,
            phase: validated.phase,
            progress: validated.progress,
            summary: validated.summary,
            needs_operator: body.needs_operator,
            artifact_paths: validated.artifact_paths,
            created_at: crate::sync::state::current_time_millis(),
        };
        if let Err(e) = self.db.insert_bridge_report(&record) {
            return Response::refused(
                key,
                ErrorCode::Failed,
                format!("friring could not record this report: {e}"),
            );
        }
        if body.needs_operator || record.phase == "blocked" {
            self.escalate_child(caller, &record);
        }
        Response::ok(key, Some(serde_json::json!({ "seq": seq })))
    }

    /// A child said it is stuck. Move it to `blocked` and tell its owner.
    ///
    /// Two things, and they are different audiences. The **state** is what the
    /// owner's `status` reads and what the UI marks, so it has to be a state
    /// friring set rather than a flag buried in the newest report. The
    /// **mail** is what an owner polling its inbox acts on, and it carries the
    /// report's own text — labelled `child_authored`, as everywhere else it is
    /// shown, because an owner deciding what to do next needs it and must not
    /// mistake it for something friring verified.
    ///
    /// Only a **live, running** child is moved: a `blocked` report arriving
    /// while the host is mid-quiesce, or after a verdict, must not put a child
    /// back into the fan-out.
    fn escalate_child(&mut self, child: SessionId, report: &crate::session::BridgeReport) {
        let child_id = child.to_string();
        let Ok(Some(row)) = self.db.bridge_child(&child_id) else {
            return;
        };
        let Ok(owner) = row.owner_id.parse::<SessionId>() else {
            return;
        };
        if matches!(
            self.db
                .bridge_child_state(&child_id)
                .ok()
                .flatten()
                .map(|r| r.state),
            Some(ChildState::Ready | ChildState::Working)
        ) {
            let _ = self
                .db
                .set_bridge_child_state(&child_id, ChildState::Blocked);
        }
        let body = serde_json::json!({
            "child": child_id,
            "phase": report.phase,
            "progress": report.progress,
            "summary": report.summary,
            "child_authored": true,
            "needs_operator": report.needs_operator,
        })
        .to_string();
        if let Err(e) = self.db.enqueue_message_capped(
            &NewMessage {
                to_session_id: owner,
                // The child is the author, and the row says so: this is a mirror
                // of what it filed, not a verdict friring reached.
                from_session_id: Some(child),
                from_task_id: None,
                kind: MailKind::Report.as_str().to_string(),
                body,
                in_reply_to: None,
            },
            self.bridge_unread_cap(owner),
        ) {
            tracing::warn!("bridge: could not mirror a report from '{child_id}': {e}");
            return;
        }
        self.owe_bridge_nudge(owner);
    }

    /// Fire the OS notification a child's state change earns.
    ///
    /// Only the states an operator has to *do* something about, and only from
    /// host-known fields — see [`NotificationState::child_notification`]. A
    /// child moving from `ready` to `working` is not news; one that came back
    /// `dirty`, could not be stopped, or is stuck is.
    pub(crate) fn notify_child_state(&mut self, child: SessionId, state: ChildState) {
        if !matches!(
            state,
            ChildState::Dirty
                | ChildState::StopFailed
                | ChildState::Stalled
                | ChildState::Unusable
                | ChildState::Failed
                | ChildState::Done
        ) {
            return;
        }
        let child_id = child.to_string();
        let Ok(Some(row)) = self.db.bridge_child(&child_id) else {
            return;
        };
        let owner_name = row
            .owner_id
            .parse::<SessionId>()
            .ok()
            .and_then(|id| self.sessions.iter().find(|s| s.info.id == id))
            .map(|s| s.info.name.clone())
            .unwrap_or_else(|| "friring".to_string());
        let child_name = self
            .sessions
            .iter()
            .find(|s| s.info.id == child)
            .map(|s| s.info.name.clone())
            .unwrap_or_else(|| child_id.clone());
        let Some(notifications) = self.notification_state.as_ref() else {
            return;
        };
        let sound = notifications.sound_enabled();
        let notification = crate::app::notify_state::NotificationState::child_notification(
            child,
            &owner_name,
            &child_name,
            state,
            sound,
        );
        notifications.send(notification);
    }

    // ── Nudges ───────────────────────────────────────────────────────────

    /// Note that a recipient has mail it has not been told about.
    ///
    /// Recorded rather than typed here: the nudge is rate limited per recipient
    /// and at most one is typed per tick, so the decision belongs on the tick
    /// rather than in the middle of serving a request.
    fn owe_bridge_nudge(&mut self, recipient: SessionId) {
        self.bridge.nudges.entry(recipient).or_insert(NudgeRecord {
            // Far enough in the past that the first nudge is due immediately.
            at: std::time::Instant::now() - NUDGE_INTERVAL,
            unanswered: 0,
            reported_stalled: false,
        });
    }

    /// Re-owe a nudge to every session friring is serving that has unread mail.
    ///
    /// The repair for everything that loses the in-memory record while the mail
    /// it is about stays in the database — see [`NUDGE_RECONCILE_TICKS`].
    /// Strictly additive: `owe_bridge_nudge` inserts and never resets, so a
    /// recipient already being nudged keeps its interval, its unanswered count
    /// and its stall report.
    fn reconcile_owed_nudges(&mut self, live: &[SessionId]) {
        let kinds: Vec<&str> = MailKind::ALL.iter().map(|kind| kind.as_str()).collect();
        match self.db.sessions_with_unread_messages(live, &kinds) {
            Ok(recipients) => {
                for recipient in recipients {
                    self.owe_bridge_nudge(recipient);
                }
            }
            Err(e) => tracing::debug!("bridge: could not re-derive the owed nudges: {e}"),
        }
        // Bounded here, on the same slow pass, because the record for a
        // recipient this instance cannot reach is deliberately kept — and a
        // parked child's `cancel` stays unread for as long as it is parked, so
        // an owner that parks children over a long run would otherwise
        // accumulate one record per child for ever, each of them scanned by
        // every later pass. Dropping one costs nothing: the mail is the durable
        // half, and the loop above re-derives the record the moment its
        // recipient is a live bridge session here again.
        if self.bridge.nudges.len() > MAX_NUDGE_RECORDS {
            let live: std::collections::HashSet<SessionId> = live.iter().copied().collect();
            self.bridge.nudges.retain(|id, _| live.contains(id));
        }
    }

    /// Owe a nudge to `recipient` — the test entry point for the rate limiter.
    #[cfg(test)]
    pub(crate) fn owe_bridge_nudge_for_test(&mut self, recipient: SessionId) {
        self.owe_bridge_nudge(recipient);
    }

    /// Whether a nudge is still owed to `recipient`, and how many have gone
    /// unanswered — the test entry point for the give-up counter.
    #[cfg(test)]
    pub(crate) fn bridge_nudge_owed_for_test(&self, recipient: SessionId) -> Option<u32> {
        self.bridge.nudges.get(&recipient).map(|r| r.unanswered)
    }

    /// Whether `recipient` is due a nudge right now — the test entry point for
    /// the fairness rules.
    ///
    /// This is the predicate `tick_bridge_nudges` selects on, so it is also how
    /// a test sees that a pass *considered* a recipient at all: every branch
    /// that ends without typing still restarts the interval, and a recipient
    /// left permanently due is one no pass ever reached.
    #[cfg(test)]
    pub(crate) fn bridge_nudge_due_for_test(&self, recipient: SessionId) -> Option<bool> {
        self.bridge
            .nudges
            .get(&recipient)
            .map(|record| record.at.elapsed() >= NUDGE_INTERVAL)
    }

    /// Drop every owed nudge, as a restart does — the broker's nudge state is
    /// per process.
    #[cfg(test)]
    pub(crate) fn forget_bridge_nudges_for_test(&mut self) {
        self.bridge.nudges.clear();
    }

    /// Age one recipient's nudge record by one unanswered nudge, so a test
    /// reaches the give-up rule without a real multiplexer to type into and
    /// without waiting out `NUDGE_INTERVAL` five times.
    #[cfg(test)]
    pub(crate) fn force_bridge_nudge_unanswered_for_test(&mut self, recipient: SessionId) {
        if let Some(record) = self.bridge.nudges.get_mut(&recipient) {
            record.at = std::time::Instant::now() - NUDGE_INTERVAL;
            record.unanswered += 1;
        }
    }

    /// Type at most **one** nudge, into at most one pane.
    ///
    /// One per tick because a nudge interrupts a live agent's turn, and because
    /// `send_prompt_now` scrapes the pane before it types — work that belongs
    /// nowhere near a loop over every session.
    fn tick_bridge_nudges(&mut self) {
        // The longest-waiting recipient, so a busy fan-out cannot leave one
        // child permanently behind whichever iteration order the map happens to
        // have.
        let due: Option<SessionId> = self
            .bridge
            .nudges
            .iter()
            .filter(|(_, record)| record.at.elapsed() >= NUDGE_INTERVAL)
            .max_by_key(|(_, record)| record.at.elapsed())
            .map(|(id, _)| *id);
        let Some(recipient) = due else { return };
        let record = self.bridge.nudges.get(&recipient).cloned();
        let Some(mut record) = record else { return };

        // Nothing left to be told about. The record means "this recipient has
        // mail it has not taken delivery of", and every drain path settles it —
        // `inbox --claim`, the TUI's own read, a cron tick — so the debt is
        // discharged here rather than in each of them. A count that cannot be
        // read is not evidence the mailbox is empty, so it nudges.
        if self.db.count_unread_messages(recipient) == Ok(0) {
            self.bridge.nudges.remove(&recipient);
            return;
        }

        // **Every** outcome below restarts the interval and puts the record
        // back, so that is done once here rather than in each branch. It was
        // spread across the branches, and the fairness bug this closes was one
        // branch not doing it: an `at` that never moves makes its recipient win
        // `max_by_key(elapsed)` on every later pass and return without typing,
        // which starves every other recipient for as long as that condition
        // lasts. A `stalled` child, or one loaded in another friring, lasts
        // indefinitely. With one exit there is no branch left that can forget.
        let mut sent = false;
        'decide: {
            // Past the allowance. Told about once, then quietened rather than
            // dropped: the mail is still unread, and a resume should find the
            // reminder for it.
            if record.unanswered >= MAX_UNANSWERED_NUDGES {
                if !record.reported_stalled {
                    self.report_stalled_child(recipient);
                    record.reported_stalled = true;
                }
                break 'decide;
            }
            // Nothing on this instance to type into: unloaded, parked, or
            // loaded in another friring against the same database. The debt is
            // kept — `owe_bridge_nudge` recreates a record only when *new* mail
            // arrives, so discarding it here means the mail already queued is
            // never announced by anybody, however long the recipient runs
            // afterwards.
            let Some(session) = self
                .sessions
                .iter()
                .find(|s| s.info.id == recipient && !s.is_placeholder())
            else {
                break 'decide;
            };
            let name = session.info.name.clone();
            // A nudge is typed into a pane, which `Session::revalidate_identity`
            // names as one of the actions that must check first:
            // `send_prompt_now` resolves by **window name**, so after a pane is
            // recycled — or a same-name window is created by anything else
            // holding the socket — the text would go to whatever now answers to
            // that name.
            if session.revalidate_identity().is_err() {
                break 'decide;
            }
            // The pane guard and the hook state both get a veto, exactly as an
            // ordinary wake does: typing into a pane that is asking the operator
            // a question would answer it on their behalf.
            if crate::cli::pane_guard::blocked_on_prompt(&self.db, recipient) {
                break 'decide;
            }
            // A refusal here is a modal, or a pane that is gone. Either way the
            // nudge is owed rather than spent.
            sent = matches!(
                crate::agent::tmux::send_prompt_now(&name, BRIDGE_NUDGE),
                Ok(write) if write.refused().is_none()
            );
        }
        record.at = std::time::Instant::now();
        if sent {
            record.unanswered += 1;
            self.metrics.bump(|p| &mut p.bridge_nudges_sent);
        }
        self.bridge.nudges.insert(recipient, record);
    }

    /// Tell an owner that a child has stopped answering.
    ///
    /// Attention, not a verdict: a long turn looks exactly like this. The child
    /// keeps its slot and its worktree, and the owner decides.
    fn report_stalled_child(&mut self, child: SessionId) {
        let child_id = child.to_string();
        let Ok(Some(row)) = self.db.bridge_child(&child_id) else {
            return;
        };
        let Ok(owner) = row.owner_id.parse::<SessionId>() else {
            return;
        };
        let _ = self
            .db
            .set_bridge_child_state(&child_id, ChildState::Stalled);
        self.host_mail(owner, MailKind::ChildStalled, &child_id);
        self.notify_child_state(child, ChildState::Stalled);
    }

    /// Mail an owner about one of its children, in friring's own words.
    ///
    /// A **fixed template over host-known fields**: the child's id and the kind
    /// of event, and nothing the child wrote. A host message is what an owner's
    /// integration step keys on, so a sentence carrying child-authored text
    /// would be a channel for one sandboxed session to put chosen words into
    /// another's decision.
    pub(crate) fn host_mail(&mut self, owner: SessionId, kind: MailKind, child_id: &str) {
        let body = serde_json::json!({ "child": child_id }).to_string();
        if let Err(e) = self.db.enqueue_message_capped(
            &NewMessage {
                to_session_id: owner,
                // From friring itself: a host message has no sender session, and
                // attributing one would let an owner reply to it.
                from_session_id: None,
                from_task_id: None,
                kind: kind.as_str().to_string(),
                body,
                in_reply_to: None,
            },
            self.bridge_unread_cap(owner),
        ) {
            tracing::warn!("bridge: could not mail the owner about '{child_id}': {e}");
            return;
        }
        self.owe_bridge_nudge(owner);
    }

    /// The unread cap this recipient's mailbox is held to.
    ///
    /// A session with an ownership row is a **child**, and both sides of its
    /// mailbox are agent-controlled; anything else on the bridge is an owner,
    /// which legitimately accumulates a deeper backlog. An ownership row that
    /// cannot be read takes the tighter of the two: a cap chosen from a failed
    /// read must not be the wider one.
    fn bridge_unread_cap(&self, recipient: SessionId) -> usize {
        match self.db.bridge_child(&recipient.to_string()) {
            Ok(None) => crate::session::bridge::MAX_UNREAD_PER_OWNER,
            _ => crate::session::bridge::MAX_UNREAD_PER_CHILD,
        }
    }

    /// Write one answer into the caller's response directory.
    fn write_bridge_answer(&self, taken: &TakenRequest, response: &Response) {
        let encoded = serde_json::to_string(response).unwrap_or_else(|e| {
            tracing::warn!("bridge: could not encode an answer: {e}");
            String::new()
        });
        if let Err(e) =
            crate::paths::write_bridge_response(&taken.session_key, &taken.key, &encoded)
        {
            tracing::warn!("bridge: could not write the answer to '{}': {e}", taken.key);
        }
    }

    fn metrics_bridge_served(&mut self) {
        self.metrics.perf.bridge_requests_served =
            self.metrics.perf.bridge_requests_served.wrapping_add(1);
    }
}

/// A report that passed validation.
struct ValidReport {
    phase: String,
    progress: u8,
    summary: String,
    artifact_paths: Vec<String>,
}

/// Refuse a report friring will not store.
///
/// Everything here is child-authored, so everything here is bounded and
/// sanitised: the phase against a closed set (the UI groups by it), the progress
/// to `0..=100`, the summary to a cap with control characters stripped — a
/// terminal escape in a summary would be an agent painting the operator's screen
/// — and every artifact path to a **relative** path with no `..`, because it
/// names a file inside the child's own worktree and nothing else.
fn validate_report(body: &ReportBody) -> Result<ValidReport, String> {
    if !REPORT_PHASES.contains(&body.phase.as_str()) {
        return Err(format!(
            "'{}' is not a report phase (expected {})",
            body.phase,
            REPORT_PHASES.join(", ")
        ));
    }
    if body.progress > 100 {
        return Err(format!("progress is 0..=100; got {}", body.progress));
    }
    if body.summary.len() > MAX_SUMMARY_BYTES {
        return Err(format!(
            "a report summary is at most {MAX_SUMMARY_BYTES} bytes"
        ));
    }
    for path in &body.artifact_paths {
        if path.starts_with('/')
            || path.starts_with('\\')
            || path.split(['/', '\\']).any(|part| part == "..")
        {
            return Err(format!(
                "the artifact path '{path}' is not inside this session's own worktree"
            ));
        }
    }
    Ok(ValidReport {
        phase: body.phase.clone(),
        progress: body.progress,
        summary: strip_control(&body.summary),
        artifact_paths: body.artifact_paths.clone(),
    })
}

/// Drop control characters from child-authored text.
///
/// A summary is shown in the info panel and in a mailbox, both of which render
/// into a terminal. An escape sequence in one would let a sandboxed agent paint
/// the operator's screen — move the cursor, clear the display, claim to be
/// friring. Newlines and tabs survive because they are the only two a summary
/// legitimately carries.
fn strip_control(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

/// A stable digest of what a request **asks for**, for the journal.
///
/// The verb and the body, never the key or the deadline: two requests differing
/// only in when they expire are the same request, and the key is what is being
/// looked up. A client retrying with the same key and the same body gets the
/// first answer; one reusing a key for different work is refused.
///
/// The algorithm is **fixed** because this value is *persisted* in
/// `bridge_requests` for the journal's whole retention and compared against a
/// freshly computed one on replay — possibly by a differently built binary.
/// `std::collections::hash_map::DefaultHasher` is documented as not stable
/// across Rust versions, so it would let a toolchain bump refuse a legitimate
/// retry as `key_reused` and lose the child a caller already created. Same
/// reasoning as [`crate::git`]'s `stable_repo_hash`, which pins FNV-1a for the
/// worktree path for exactly this reason.
fn body_hash(request: &Request) -> String {
    use sha2::{Digest as _, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(request.verb.as_str().as_bytes());
    // A separator no verb can contain, so `verb + body` cannot be re-split into
    // a different pair that hashes the same.
    hasher.update([0x1f]);
    hasher.update(
        serde_json::to_string(&request.body)
            .unwrap_or_default()
            .as_bytes(),
    );
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::bridge::RequestBody;

    /// The nudge is one exact literal with nothing formatted into it, and no
    /// code path builds it from anything else.
    ///
    /// A nudge is typed into a live agent's prompt, so any part of it that came
    /// from a *message* would be a way for one sandboxed session to put chosen
    /// text in front of another agent.
    #[test]
    fn the_nudge_is_one_literal_with_no_placeholder() {
        for placeholder in ['{', '}', '%'] {
            assert!(
                !BRIDGE_NUDGE.contains(placeholder),
                "the nudge must carry no placeholder: {BRIDGE_NUDGE}"
            );
        }
        // And nothing in this module builds it: the only use is the constant
        // handed straight to `send_prompt_now`.
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app/bridge.rs"),
        )
        .expect("this module's own source");
        // Split at the test **module**, not at the first `#[cfg(test)]`: this
        // module has test-only helpers above it, and splitting on the attribute
        // would truncate the code being examined and make the count below pass
        // for the wrong reason.
        let code = source
            .split_once("mod tests {")
            .map_or(source.as_str(), |(code, _)| code);
        assert!(
            !code.contains("format!(\"friring: you have"),
            "the nudge must never be formatted"
        );
        // The module doc, the import, and exactly one use: the constant handed
        // straight to `send_prompt_now`.
        assert_eq!(
            code.matches("BRIDGE_NUDGE").count(),
            3,
            "the nudge must be referenced in the doc, the import and one call"
        );
    }

    /// Child-authored text reaches a terminal, so an escape sequence in it would
    /// let a sandboxed agent paint the operator's screen.
    #[test]
    fn control_characters_are_stripped_from_child_text() {
        let hostile = "line one\x1b[2Jcleared\x07\nline two\ttabbed";
        let clean = strip_control(hostile);
        assert!(!clean.contains('\x1b'), "{clean:?}");
        assert!(!clean.contains('\x07'), "{clean:?}");
        // The two a summary legitimately carries survive.
        assert!(clean.contains('\n'));
        assert!(clean.contains('\t'));
    }

    /// A report is child-authored, so every field is bounded and closed.
    #[test]
    fn a_report_outside_its_bounds_is_refused() {
        let ok = ReportBody {
            phase: "implementing".to_string(),
            progress: 50,
            summary: "halfway".to_string(),
            needs_operator: false,
            artifact_paths: vec!["docs/notes.md".to_string()],
        };
        assert!(validate_report(&ok).is_ok());

        for (what, body) in [
            (
                "an unknown phase",
                ReportBody {
                    phase: "vibing".to_string(),
                    ..ok.clone()
                },
            ),
            (
                "progress past 100",
                ReportBody {
                    progress: 101,
                    ..ok.clone()
                },
            ),
            (
                "an oversized summary",
                ReportBody {
                    summary: "x".repeat(MAX_SUMMARY_BYTES + 1),
                    ..ok.clone()
                },
            ),
            (
                "an absolute artifact path",
                ReportBody {
                    artifact_paths: vec!["/etc/passwd".to_string()],
                    ..ok.clone()
                },
            ),
            (
                "an escaping artifact path",
                ReportBody {
                    artifact_paths: vec!["../../secrets".to_string()],
                    ..ok.clone()
                },
            ),
        ] {
            assert!(validate_report(&body).is_err(), "{what} must be refused");
        }
    }

    /// The digest is over what a request *asks for*, so a retry that differs
    /// only in its deadline is the same request and one that differs in its body
    /// is not.
    #[test]
    fn the_body_hash_ignores_the_deadline_and_the_key() {
        let base = Request {
            protocol: BRIDGE_PROTOCOL,
            key: RequestKey::new("abc-1234").unwrap(),
            verb: Verb::Status,
            deadline: None,
            body: RequestBody::Empty(crate::session::bridge::EmptyBody {}),
        };
        let later = Request {
            deadline: Some(99),
            key: RequestKey::new("zzz-9999").unwrap(),
            ..base.clone()
        };
        assert_eq!(body_hash(&base), body_hash(&later));

        let different = Request {
            verb: Verb::Inbox,
            body: RequestBody::Inbox(InboxBody {
                claim: true,
                limit: None,
            }),
            ..base.clone()
        };
        assert_ne!(body_hash(&base), body_hash(&different));
    }

    /// The digest is written to `bridge_requests` and compared against a freshly
    /// computed one for the journal's whole retention — across binary upgrades.
    /// Pinning one value here is what makes a swap back to an unspecified hasher
    /// (`DefaultHasher`, whose output std does not promise across releases) fail
    /// here instead of silently refusing every pre-upgrade retry as `key_reused`.
    #[test]
    fn the_body_hash_is_a_named_algorithm_pinned_to_its_output() {
        let request = Request {
            protocol: BRIDGE_PROTOCOL,
            key: RequestKey::new("abc-1234").unwrap(),
            verb: Verb::Status,
            deadline: None,
            body: RequestBody::Empty(crate::session::bridge::EmptyBody {}),
        };
        // sha256("status" || 0x1f || "{}")
        assert_eq!(
            body_hash(&request),
            "e9378c6b8c58c8f37d692587c2efdbce4c9dcf4689eeaff6fb0fbcea08bf0d5e"
        );
    }

    /// A child never holds `child-lifecycle`, whatever its profile says: the
    /// intersection is where the one-level rule is enforced.
    #[test]
    fn a_childs_effective_grant_never_carries_child_lifecycle() {
        assert!(!BridgeCapability::CHILD_MAX.contains(&BridgeCapability::ChildLifecycle));
        assert!(BridgeCapability::CHILD_MAX.contains(&BridgeCapability::Mailbox));
        assert!(BridgeCapability::CHILD_MAX.contains(&BridgeCapability::Report));
    }
}
