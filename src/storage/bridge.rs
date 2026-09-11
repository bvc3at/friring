//! The orchestration bridge's rows (schema v49).
//!
//! Eight tables, and the thing to know about them is which one is authority.
//!
//! **`bridge_children` is the ownership record, and it is insert-only in SQL.**
//! Every verb's authority is "the caller's directory identity, and this row" —
//! so a row that could be updated could be re-pointed at another owner by
//! anything holding a write handle to the file, including a bug in friring
//! itself. The table's `BEFORE UPDATE` and `BEFORE DELETE` triggers refuse both,
//! which is why archival is a stamp on `bridge_child_state` and never a delete
//! here. `sessions.parent_session_id` is display only and is never asked.
//!
//! Everything else churns and is pruned: the request journal, the host-verified
//! results and the child-authored reports at 14 days, sagas 14 days after they
//! stop moving, and a child hidden from the default views 90 days after it
//! reached a terminal state — the horizon the audit log already uses.
//!
//! Nothing here renders an egress token. It is the live credential for a running
//! boundary; [`ChildSaga`]'s own `Debug` withholds it, and no query in this
//! module selects it into a log line.

use rusqlite::{params, Connection, OptionalExtension as _};

use crate::session::bridge::{
    BridgeChild, BridgeChildState, BridgeReport, BridgeResult, ChildSaga, ChildState, Outcome,
    SagaStep, MAX_REPORTS_PER_CHILD,
};
use crate::storage::Database;
use crate::sync::state::current_time_millis;

/// How long a request journal entry, a result and a report are kept.
///
/// Long enough that a replay after a weekend still finds its answer, short
/// enough that a busy leader's traffic does not accumulate forever.
pub const RETENTION_DAYS: u64 = 14;

/// How long after a child reaches a terminal state it stays in the default
/// views. The audit log's horizon, so an operator has one number to remember.
pub const ARCHIVE_DAYS: u64 = 90;

const DAY_MILLIS: u64 = 24 * 60 * 60 * 1000;

/// What a request has come to in the journal.
///
/// A replay of a key with the **same** body hash returns the stored response
/// verbatim, whatever state it is in; a replay with a *different* hash is
/// `key_reused` and has no effect at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    /// Taken, not yet answered. A replay waits rather than acting twice.
    Accepted,
    /// Answered successfully; `response` holds the exact bytes.
    Done,
    /// Answered with a refusal; `response` holds those bytes.
    Failed,
}

impl RequestState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    fn from_db(raw: &str) -> Self {
        match raw {
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => Self::Accepted,
        }
    }
}

/// Whether a stored `body_hash` was written by a binary that had pinned the
/// digest algorithm.
///
/// The pinned form is SHA-256 rendered as 64 lowercase hex characters. Anything
/// else is from before the pin — `DefaultHasher`'s 16 — and cannot be
/// recomputed, so [`Database::take_bridge_request`] replays it rather than
/// calling a legitimate retry a reused key.
fn is_pinned_digest(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One journaled request: what it was, and what it has come to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournaledRequest {
    pub owner_id: String,
    pub key: String,
    pub verb: String,
    /// A hash of the request body, so a key reused for a *different* request is
    /// refused rather than answered from the first one's result.
    pub body_hash: String,
    pub state: RequestState,
    /// The exact response bytes a replay returns. `None` while `accepted`.
    pub response: Option<String>,
    pub deadline: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Whether a journal lookup found the same request, a different one under the
/// same key, or nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalLookup {
    /// No such key for this owner: a fresh request.
    Fresh,
    /// The same key **and** the same body: a replay. Answer from this row.
    Replay(Box<JournaledRequest>),
    /// The same key with a different body. Refuse `key_reused` and do nothing —
    /// a client that reuses a key for a second request must not get the first
    /// one's answer, and must not silently get a second effect either.
    KeyReused,
}

/// Everything the S6 transaction writes, so its statements take one argument
/// rather than a dozen.
///
/// The **saga row** is in here for a reason worth stating: the committed step
/// has to land in the same transaction as the rows it describes. Recovery
/// decides purely from `saga.step`, so a step written afterwards and
/// best-effort would let a crash in between run pre-commit cleanup — kill the
/// pane, drop the directories, reclaim the worktree — over a durable ownership
/// row, leaving a child stuck `Starting` and holding a fan-out slot for good.
#[derive(Debug)]
pub struct ChildCommit<'a> {
    pub session: &'a crate::sync::state::SharedSession,
    pub child_id: &'a str,
    pub owner_id: &'a str,
    /// The `create` key this child was made for. Empty for a relaunch, which
    /// writes no ownership row.
    pub request_key: &'a str,
    /// Whether to insert the immutable ownership row. `false` for a relaunch of
    /// a child that already has one — the table refuses a second insert anyway,
    /// and asking for one would fail a legitimate relaunch.
    pub new_ownership: bool,
    pub repo_root: &'a str,
    pub worktree_path: Option<&'a str>,
    pub branch: &'a str,
    pub task_kind: &'a str,
    /// The task body. Empty means no first mail is inserted.
    pub task_body: &'a str,
    pub role_hint: Option<&'a str>,
    /// The saga row to advance to `committed`, in this same transaction.
    ///
    /// Recovery decides only from `child_sagas.step`, so a step written *after*
    /// the commit leaves a window where the rows below exist and the saga still
    /// reads as pre-commit — and reconciliation would then tear down a child
    /// whose immutable ownership row it cannot delete, stranding a fan-out slot.
    /// `None` for a job with no request key: nothing journals it, so there is no
    /// saga to advance.
    pub saga: Option<&'a crate::session::ChildSaga>,
}

impl Database {
    // ── Ownership ────────────────────────────────────────────────────────

    /// Record that `child_id` belongs to `owner_id`, for the life of the
    /// database.
    ///
    /// Insert-only: the table's triggers refuse an update, so a second call with
    /// the same `(owner_id, request_key)` fails the unique constraint rather
    /// than moving anything. That is what makes a `create` replay find the same
    /// child instead of making a second one.
    pub fn insert_bridge_child(
        &self,
        child_id: &str,
        owner_id: &str,
        request_key: &str,
    ) -> rusqlite::Result<()> {
        insert_bridge_child_on(&self.conn, child_id, owner_id, request_key)
    }

    /// The ownership row for one child, or `None`.
    pub fn bridge_child(&self, child_id: &str) -> rusqlite::Result<Option<BridgeChild>> {
        self.conn
            .query_row(
                "SELECT child_id, owner_id, request_key, created_at
                 FROM bridge_children WHERE child_id = ?1",
                params![child_id],
                map_child,
            )
            .optional()
    }

    /// Every child one session owns, oldest first.
    pub fn bridge_children_of(&self, owner_id: &str) -> rusqlite::Result<Vec<BridgeChild>> {
        let mut stmt = self.conn.prepare(
            "SELECT child_id, owner_id, request_key, created_at
             FROM bridge_children WHERE owner_id = ?1 ORDER BY created_at, child_id",
        )?;
        let rows = stmt.query_map(params![owner_id], map_child)?.collect();
        rows
    }

    /// Every ownership row there is, oldest first.
    ///
    /// For a caller that has to answer "is this session in an orchestration?"
    /// about a whole fleet — `friring-cli session list` renders one JSON
    /// document per session, and the answer is `None` for nearly all of them.
    /// Asking per session costs two statements each; the table holds one row
    /// per child that has ever been created, which is bounded by the fan-out
    /// caps rather than by the session count.
    pub fn all_bridge_children(&self) -> rusqlite::Result<Vec<BridgeChild>> {
        let mut stmt = self.conn.prepare(
            "SELECT child_id, owner_id, request_key, created_at
             FROM bridge_children ORDER BY created_at, child_id",
        )?;
        let rows = stmt.query_map([], map_child)?.collect();
        rows
    }

    /// Whether `owner_id` owns `child_id` — the authority question, asked of the
    /// immutable row and of nothing else.
    pub fn owns_bridge_child(&self, owner_id: &str, child_id: &str) -> rusqlite::Result<bool> {
        Ok(self
            .bridge_child(child_id)?
            .is_some_and(|child| child.owner_id == owner_id))
    }

    // ── Lifecycle state ──────────────────────────────────────────────────

    /// Set a child's state, stamping the timestamps that state implies.
    pub fn set_bridge_child_state(
        &self,
        child_id: &str,
        state: ChildState,
    ) -> rusqlite::Result<()> {
        set_bridge_child_state_on(&self.conn, child_id, state)
    }

    /// One child's mutable state, or `None` when nothing has been recorded.
    pub fn bridge_child_state(&self, child_id: &str) -> rusqlite::Result<Option<BridgeChildState>> {
        self.conn
            .query_row(
                &format!("SELECT {STATE_COLS} FROM bridge_child_state WHERE child_id = ?1"),
                params![child_id],
                map_child_state,
            )
            .optional()
    }

    /// The states of every child one session owns, in ownership order.
    ///
    /// A child with an ownership row and no state row is skipped rather than
    /// invented: the two are written in the same transaction, so the only way to
    /// see one without the other is a database somebody edited.
    pub fn bridge_child_states_of(
        &self,
        owner_id: &str,
    ) -> rusqlite::Result<Vec<BridgeChildState>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM bridge_child_state s
             JOIN bridge_children c ON c.child_id = s.child_id
             WHERE c.owner_id = ?1
             ORDER BY c.created_at, c.child_id",
            STATE_COLS
                .split(", ")
                .map(|col| format!("s.{col}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
        let rows = stmt
            .query_map(params![owner_id], map_child_state)?
            .collect();
        rows
    }

    /// How many of one session's children hold a slot.
    ///
    /// Counted from the live states rather than from "not tombstoned": a dirty
    /// or unstoppable child still has a worktree somebody has to deal with, and
    /// letting the owner create a replacement while it does is how a fan-out cap
    /// stops meaning anything.
    pub fn live_bridge_children(&self, owner_id: &str) -> rusqlite::Result<usize> {
        Ok(self
            .bridge_child_states_of(owner_id)?
            .into_iter()
            .filter(|row| row.state.is_live())
            .count())
    }

    /// Note that a child claimed mail, which is what resets the stall counter.
    pub fn touch_bridge_child_claim(&self, child_id: &str) -> rusqlite::Result<()> {
        let now = current_time_millis() as i64;
        self.conn.execute(
            "UPDATE bridge_child_state SET claimed_at = ?2, updated_at = ?2 WHERE child_id = ?1",
            params![child_id, now],
        )?;
        Ok(())
    }

    /// Stamp `force_deleted_at` — an operator removed the child themselves.
    pub fn mark_bridge_child_force_deleted(&self, child_id: &str) -> rusqlite::Result<()> {
        let now = current_time_millis() as i64;
        self.conn.execute(
            "UPDATE bridge_child_state
             SET force_deleted_at = ?2, updated_at = ?2 WHERE child_id = ?1",
            params![child_id, now],
        )?;
        Ok(())
    }

    // ── Request journal ──────────────────────────────────────────────────

    /// Take a request into the journal, or say what the key already means.
    ///
    /// The whole of the exactly-once guarantee. A fresh key is inserted
    /// `accepted` and answered [`Fresh`](JournalLookup::Fresh); the same key
    /// with the same body is a [`Replay`](JournalLookup::Replay) and is answered
    /// from the stored row; the same key with a *different* body is
    /// [`KeyReused`](JournalLookup::KeyReused) and nothing is written.
    pub fn take_bridge_request(
        &self,
        owner_id: &str,
        key: &str,
        verb: &str,
        body_hash: &str,
        deadline: Option<u64>,
    ) -> rusqlite::Result<JournalLookup> {
        let tx = self.conn.unchecked_transaction()?;
        let existing: Option<JournaledRequest> = tx
            .query_row(
                &format!(
                    "SELECT {REQUEST_COLS} FROM bridge_requests WHERE owner_id = ?1 AND key = ?2"
                ),
                params![owner_id, key],
                map_request,
            )
            .optional()?;
        let answer = match existing {
            Some(row) if row.body_hash == body_hash => JournalLookup::Replay(Box::new(row)),
            // A row written before the digest algorithm was pinned. Its hash
            // cannot be recomputed, so the *only* two readings are "replay it"
            // and "call it a reused key" — and the second is the damaging one: a
            // `create` that already made a child would be refused rather than
            // answered, and the caller would never learn the child's id. The
            // narrow window this covers is one journal retention after a binary
            // upgrade, on a database that ran an unreleased schema v49.
            Some(row) if !is_pinned_digest(&row.body_hash) => JournalLookup::Replay(Box::new(row)),
            Some(_) => JournalLookup::KeyReused,
            None => {
                let now = current_time_millis() as i64;
                tx.execute(
                    "INSERT INTO bridge_requests
                         (owner_id, key, verb, body_hash, state, response, deadline,
                          created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, 'accepted', NULL, ?5, ?6, ?6)",
                    params![
                        owner_id,
                        key,
                        verb,
                        body_hash,
                        deadline.map(|d| d as i64),
                        now
                    ],
                )?;
                JournalLookup::Fresh
            }
        };
        tx.commit()?;
        Ok(answer)
    }

    /// Record the exact bytes a replay of this request must return.
    pub fn finish_bridge_request(
        &self,
        owner_id: &str,
        key: &str,
        state: RequestState,
        response: &str,
    ) -> rusqlite::Result<()> {
        let now = current_time_millis() as i64;
        self.conn.execute(
            "UPDATE bridge_requests
             SET state = ?3, response = ?4, updated_at = ?5
             WHERE owner_id = ?1 AND key = ?2",
            params![owner_id, key, state.as_str(), response, now],
        )?;
        Ok(())
    }

    /// One journaled request, whatever state it is in.
    pub fn bridge_request(
        &self,
        owner_id: &str,
        key: &str,
    ) -> rusqlite::Result<Option<JournaledRequest>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {REQUEST_COLS} FROM bridge_requests WHERE owner_id = ?1 AND key = ?2"
                ),
                params![owner_id, key],
                map_request,
            )
            .optional()
    }

    /// Every request still `accepted`, oldest first.
    ///
    /// What recovery needs to keep a promise the journal makes on its own: a
    /// deferred verb leaves its row `accepted` and writes no response, and a
    /// replay of that key **waits** rather than acting twice. So a request whose
    /// work died with the process would leave its caller polling for an answer
    /// nothing will ever produce. Recovery answers the ones it cannot resume.
    ///
    /// Oldest first so a reconciliation that is bounded takes the callers who
    /// have waited longest.
    pub fn accepted_bridge_requests(&self) -> rusqlite::Result<Vec<JournaledRequest>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {REQUEST_COLS} FROM bridge_requests WHERE state = 'accepted' \
             ORDER BY created_at ASC"
        ))?;
        let rows = stmt.query_map([], map_request)?;
        rows.collect()
    }

    // ── Sagas ────────────────────────────────────────────────────────────

    /// Write a saga row, or advance the one already there.
    ///
    /// Every field is recorded **before** the effect it names is made, so
    /// recovery reconciles by exact identity: it removes the worktree at
    /// `worktree_path`, kills the pane at `mux_pane_id` whose pid is
    /// `mux_pane_pid`, and touches nothing it cannot name.
    pub fn upsert_child_saga(&self, saga: &ChildSaga) -> rusqlite::Result<()> {
        upsert_child_saga_on(&self.conn, saga)
    }

    /// Record a child's relaunch saga and `starting` state as one fact.
    ///
    /// For a `stopped` or `unusable` child, the state transition is also its
    /// fan-out capacity claim. For every resume, atomicity keeps recovery from
    /// finding a saga whose corresponding lifecycle transition never landed.
    pub fn begin_bridge_child_resume(
        &self,
        saga: &ChildSaga,
        child_id: &str,
    ) -> Result<(), String> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        upsert_child_saga_on(&self.conn, saga)
            .map_err(|e| format!("the child's resume saga: {e}"))?;
        set_bridge_child_state_on(&self.conn, child_id, ChildState::Starting)
            .map_err(|e| format!("the child's resumed slot: {e}"))?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// One saga by its owner and request key.
    pub fn child_saga(&self, owner_id: &str, key: &str) -> rusqlite::Result<Option<ChildSaga>> {
        self.conn
            .query_row(
                &format!("SELECT {SAGA_COLS} FROM child_sagas WHERE owner_id = ?1 AND key = ?2"),
                params![owner_id, key],
                map_saga,
            )
            .optional()
    }

    /// Every saga that has not reached a final step — what recovery walks.
    pub fn unfinished_child_sagas(&self) -> rusqlite::Result<Vec<ChildSaga>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SAGA_COLS} FROM child_sagas
             WHERE step NOT IN ('done', 'failed')
             ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([], map_saga)?.collect();
        rows
    }

    /// How many of one owner's launches have claimed a fan-out slot without
    /// having a durable child row yet.
    ///
    /// A `create` is accepted, and its saga row written, long before S6 commits
    /// the child — so between those two moments the claim exists in
    /// `child_sagas` and nowhere else. Counting it from the broker's own
    /// in-flight jobs answers only for **this process**, and a friring's bridge
    /// lease can move: an instance that took over an owner's queue mid-launch
    /// would see the durable children and none of the launches its peer is
    /// still running, and admit that many children past the cap.
    ///
    /// Disjoint from [`Self::live_bridge_children`] by construction, so the two
    /// add rather than overlap: S6 writes the state row and the `committed`
    /// step in one transaction, so a saga past that line has a state row and a
    /// saga before it has none. A `resume` is on the durable side for the same
    /// reason — `begin_bridge_child_resume` writes `starting` with its saga.
    ///
    /// A saga whose lease has expired is still counted. The lease says an
    /// instance is working on it, so an expired one means a friring went away
    /// mid-launch — which is exactly when what it made is unaccounted for, and
    /// capacity uncertainty is refused rather than resolved in favour of one
    /// more child. Recovery finalizes them on the next start.
    pub fn pending_child_slot_claims(&self, owner_id: &str) -> rusqlite::Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM child_sagas s
             WHERE s.owner_id = ?1
               AND s.step NOT IN ('committed', 'egress_live', 'released', 'done', 'failed')
               AND s.child_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM bridge_child_state c WHERE c.child_id = s.child_id
               )",
            params![owner_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Every fan-out slot one owner has claimed: its live children and its
    /// launches that have not committed one yet.
    ///
    /// **One read transaction, and that is load-bearing rather than tidy.** The
    /// two halves are disjoint at any single instant — S6 writes the child's
    /// state row and the `committed` step in one transaction — and that is
    /// exactly what makes reading them at two instants wrong. A peer instance
    /// that commits in between moves its child from the pending side to the
    /// live side, so a count taken before the move on one side and after it on
    /// the other sees it on neither, and the cap admits one child too many.
    /// A SQLite read transaction takes its snapshot at the first statement and
    /// holds it, so both halves answer about one database.
    ///
    /// The halves stay in their own functions rather than becoming one
    /// statement, because the live set is [`ChildState::is_live`] and a second
    /// spelling of it in SQL would be free to drift from it.
    pub fn reserved_child_slots(&self, owner_id: &str) -> rusqlite::Result<usize> {
        let snapshot = self.conn.unchecked_transaction()?;
        let live = self.live_bridge_children(owner_id)?;
        let pending = self.pending_child_slot_claims(owner_id)?;
        // Explicit, not left to the drop: a rollback in a destructor cannot
        // report a failure, and a capacity read that silently half-finished is
        // the one outcome this whole path exists to refuse.
        snapshot.finish()?;
        Ok(live + pending)
    }

    /// The most recent saga naming one child.
    ///
    /// What a quiesce and a `resume` read their worktree, branch and base commit
    /// out of. Both need the **recorded** launch rather than the request's own
    /// arguments: a caller that could name a worktree would be creating a new
    /// child under an old child's ownership row.
    pub fn child_saga_of_child(&self, child_id: &str) -> rusqlite::Result<Option<ChildSaga>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {SAGA_COLS} FROM child_sagas
                     WHERE child_id = ?1 ORDER BY created_at DESC LIMIT 1"
                ),
                params![child_id],
                map_saga,
            )
            .optional()
    }

    /// How many launches one child has had.
    ///
    /// Recovery relaunches a committed child with no live pane **once**; a
    /// second saga for the same child is what says the once is spent, so the
    /// counter is the rows themselves rather than a column somebody has to
    /// remember to bump.
    pub fn child_saga_count(&self, child_id: &str) -> rusqlite::Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM child_sagas WHERE child_id = ?1",
            params![child_id],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Every child currently in one state — what recovery reads to find the
    /// quiesces a previous run was part-way through.
    pub fn bridge_children_in_state(&self, state: ChildState) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT child_id FROM bridge_child_state WHERE state = ?1 ORDER BY updated_at",
        )?;
        let rows = stmt
            .query_map(params![state.as_str()], |row| row.get(0))?
            .collect();
        rows
    }

    /// The **one transaction** that makes a child real (ADR-32 S6).
    ///
    /// Session row, immutable ownership, starting state, the repository it works
    /// in, its first task mail, and the saga step that records all of it —
    /// together or not at all. They are one fact: a session row with no ownership
    /// row would be a session no verb can act on, an ownership row with no task
    /// would be a worker with nothing to do and no record of why, and a saga
    /// still reading as pre-commit would send recovery to tear down rows it
    /// cannot delete.
    ///
    /// A rollback leaves **nothing**, which is what lets the saga's unwind treat
    /// a failure here as "the child was never created" rather than having to
    /// work out which half landed.
    ///
    /// # Errors
    ///
    /// Any of the writes failed, including the ownership row's unique
    /// `(owner_id, request_key)` — which is what makes a replayed `create` find
    /// the first attempt's child instead of making a second one.
    pub fn commit_bridge_child(&self, commit: &ChildCommit<'_>) -> Result<(), String> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        self.upsert_session(commit.session)
            .map_err(|e| format!("the child's session row: {e}"))?;
        if commit.new_ownership {
            insert_bridge_child_on(
                &self.conn,
                commit.child_id,
                commit.owner_id,
                commit.request_key,
            )
            .map_err(|e| format!("the child's ownership row: {e}"))?;
        }
        set_bridge_child_state_on(&self.conn, commit.child_id, ChildState::Starting)
            .map_err(|e| format!("the child's starting state: {e}"))?;
        self.upsert_session_repo(
            commit.child_id,
            commit.repo_root,
            "child",
            commit.worktree_path,
            Some(commit.branch),
        )
        .map_err(|e| format!("the child's repository: {e}"))?;
        // The task the child was created with, delivered as its first mail in
        // the same commit as the row. Nothing is inserted for a relaunch: the
        // mailbox already holds it, and a second copy would read as a second
        // assignment.
        if !commit.task_body.is_empty() {
            let body = serde_json::json!({
                "kind": commit.task_kind,
                "role_hint": commit.role_hint,
                "task": commit.task_body,
            })
            .to_string();
            let owner = commit
                .owner_id
                .parse::<crate::session::SessionId>()
                .map_err(|_| "the owner's id is not readable".to_string())?;
            let child = commit
                .child_id
                .parse::<crate::session::SessionId>()
                .map_err(|_| "the child's id is not readable".to_string())?;
            // The recipient is a child by construction — this is the row that
            // makes it one — so the child cap, not the generic ceiling.
            self.enqueue_message_capped(
                &crate::storage::messages::NewMessage {
                    to_session_id: child,
                    from_session_id: Some(owner),
                    from_task_id: None,
                    kind: crate::session::bridge::MailKind::Task.as_str().to_string(),
                    body,
                    in_reply_to: None,
                },
                crate::session::bridge::MAX_UNREAD_PER_CHILD,
            )
            .map_err(|e| format!("the child's first task: {e}"))?;
        }
        // Last, and inside: the saga's `committed` step is the record *of* this
        // transaction, so it lands with the rows it describes or not at all.
        if let Some(saga) = commit.saga {
            upsert_child_saga_on(&self.conn, saga)
                .map_err(|e| format!("the child's saga step: {e}"))?;
        }
        tx.commit().map_err(|e| e.to_string())
    }

    // ── Host-verified results ────────────────────────────────────────────

    /// Record what the host found in a child's worktree after it stopped the
    /// pane. Overwrites any earlier verification of the same child: a `resume`
    /// and a second finish intent produce a second, later verdict.
    pub fn upsert_bridge_result(&self, result: &BridgeResult) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO bridge_results
                 (child_id, message_id, outcome, branch, head, dirty, ahead_of_base, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(child_id) DO UPDATE SET
                 message_id = excluded.message_id,
                 outcome = excluded.outcome,
                 branch = excluded.branch,
                 head = excluded.head,
                 dirty = excluded.dirty,
                 ahead_of_base = excluded.ahead_of_base,
                 verified_at = excluded.verified_at",
            params![
                result.child_id,
                result.message_id,
                result.outcome.as_str(),
                result.branch,
                result.head,
                result.dirty as i64,
                i64::from(result.ahead_of_base),
                result.verified_at as i64,
            ],
        )?;
        Ok(())
    }

    /// The host's verdict on one child, or `None` when it has not been verified.
    pub fn bridge_result(&self, child_id: &str) -> rusqlite::Result<Option<BridgeResult>> {
        self.conn
            .query_row(
                "SELECT child_id, message_id, outcome, branch, head, dirty, ahead_of_base,
                        verified_at
                 FROM bridge_results WHERE child_id = ?1",
                params![child_id],
                map_result,
            )
            .optional()
    }

    // ── Reports ──────────────────────────────────────────────────────────

    /// File one report and drop the child's oldest if it now has too many.
    ///
    /// The cap is enforced by the **writer**, not by a trigger, because the
    /// number to keep is a product decision and a trigger would make it a schema
    /// migration to change. An unbounded stream of child-authored text is a
    /// disk-space channel out of a boundary.
    pub fn insert_bridge_report(&self, report: &BridgeReport) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO bridge_reports
                 (child_id, seq, phase, progress, summary, needs_operator, artifact_paths,
                  created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                report.child_id,
                i64::from(report.seq),
                report.phase,
                i64::from(report.progress),
                report.summary,
                report.needs_operator as i64,
                serde_json::to_string(&report.artifact_paths).unwrap_or_else(|_| "[]".to_string()),
                report.created_at as i64,
            ],
        )?;
        tx.execute(
            "DELETE FROM bridge_reports
             WHERE child_id = ?1 AND seq <= (
                 SELECT MAX(seq) FROM bridge_reports WHERE child_id = ?1
             ) - ?2",
            params![report.child_id, i64::from(MAX_REPORTS_PER_CHILD)],
        )?;
        let now = current_time_millis() as i64;
        tx.execute(
            "UPDATE bridge_child_state
             SET last_report_at = ?2, updated_at = ?2 WHERE child_id = ?1",
            params![report.child_id, now],
        )?;
        tx.commit()
    }

    /// One child's reports, newest first, at most `limit`.
    pub fn bridge_reports(
        &self,
        child_id: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<BridgeReport>> {
        let mut stmt = self.conn.prepare(
            "SELECT child_id, seq, phase, progress, summary, needs_operator, artifact_paths,
                    created_at
             FROM bridge_reports WHERE child_id = ?1 ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![child_id, limit as i64], map_report)?
            .collect();
        rows
    }

    /// The next report sequence number for a child.
    pub fn next_report_seq(&self, child_id: &str) -> rusqlite::Result<u32> {
        let highest: Option<i64> = self.conn.query_row(
            "SELECT MAX(seq) FROM bridge_reports WHERE child_id = ?1",
            params![child_id],
            |row| row.get(0),
        )?;
        Ok(highest.unwrap_or(0).saturating_add(1).max(1) as u32)
    }

    // ── Broker leases ────────────────────────────────────────────────────

    /// Take or renew the broker lease for one session's bridge directory.
    ///
    /// Returns whether this instance now holds it. The lease is a *courtesy*: it
    /// keeps two running TUIs from both polling the same directory. The
    /// cross-instance guard that actually matters is the `rename(2)` a take
    /// makes, which exactly one process can win.
    pub fn claim_broker_lease(
        &self,
        session_id: &str,
        instance_id: &str,
        lease_millis: u64,
    ) -> rusqlite::Result<bool> {
        let now = current_time_millis();
        let until = (now + lease_millis) as i64;
        // `prepare_cached`, like `load_hook_states` under ADR-P6 and for the
        // same reason: this runs once per bridge session per housekeeping pass,
        // and `Connection::execute` re-parses its SQL on every call. Profiling a
        // real fleet put SQLite's *parser* at the top of the bridge tick.
        let changed = self
            .conn
            .prepare_cached(
                "INSERT INTO bridge_brokers (session_id, instance_id, lease_until)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET
                     instance_id = excluded.instance_id,
                     lease_until = excluded.lease_until
                 WHERE bridge_brokers.instance_id = excluded.instance_id
                    OR bridge_brokers.lease_until < ?4",
            )?
            .execute(params![session_id, instance_id, until, now as i64])?;
        if changed > 0 {
            return Ok(true);
        }
        // No row changed: either this instance already holds an identical lease,
        // or another instance holds a live one. Asking settles which.
        Ok(self
            .broker_lease_holder(session_id)?
            .is_some_and(|holder| holder == instance_id))
    }

    /// Who holds a session's broker lease right now, if the lease is still live.
    ///
    /// The hottest statement the bridge runs — once per polled session per
    /// broker pass — so it is `prepare_cached` for the reason
    /// [`Self::claim_broker_lease`] is.
    pub fn broker_lease_holder(&self, session_id: &str) -> rusqlite::Result<Option<String>> {
        let now = current_time_millis() as i64;
        self.conn
            .prepare_cached(
                "SELECT instance_id FROM bridge_brokers
                 WHERE session_id = ?1 AND lease_until >= ?2",
            )?
            .query_row(params![session_id, now], |row| row.get(0))
            .optional()
    }

    /// Give up every lease this instance holds — a clean shutdown, so another
    /// instance picks the work up without waiting the lease out.
    pub fn release_broker_leases(&self, instance_id: &str) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM bridge_brokers WHERE instance_id = ?1",
            params![instance_id],
        )
    }

    // ── Session repositories ─────────────────────────────────────────────

    /// Record the repositories a session works in — the authority a `create`
    /// request's `repo_root` is checked against.
    ///
    /// A row, not a derivation from the sandbox profile's grants: deriving it
    /// would make editing a profile into an authority change, and a leader could
    /// then create children in any repository its boundary happened to reach.
    pub fn upsert_session_repo(
        &self,
        session_id: &str,
        repo_root: &str,
        role: &str,
        worktree_path: Option<&str>,
        branch: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO session_repos (session_id, repo_root, role, worktree_path, branch)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id, repo_root) DO UPDATE SET
                 role = excluded.role,
                 worktree_path = excluded.worktree_path,
                 branch = excluded.branch",
            params![session_id, repo_root, role, worktree_path, branch],
        )?;
        Ok(())
    }

    /// Every repository root a session works in.
    pub fn session_repo_roots(&self, session_id: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT repo_root FROM session_repos WHERE session_id = ?1 ORDER BY repo_root",
        )?;
        let rows = stmt
            .query_map(params![session_id], |row| row.get(0))?
            .collect();
        rows
    }

    /// Whether `session_id` works in `repo_root` — asked before a `create`.
    pub fn session_owns_repo(&self, session_id: &str, repo_root: &str) -> rusqlite::Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM session_repos WHERE session_id = ?1 AND repo_root = ?2",
                params![session_id, repo_root],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    // ── Retention ────────────────────────────────────────────────────────

    /// Drop what has aged out, and hide what has been dead long enough.
    ///
    /// Ownership is never touched. Returns how many rows were removed, for the
    /// log line.
    pub fn prune_bridge_state(&self) -> rusqlite::Result<usize> {
        let now = current_time_millis();
        let retention = (now.saturating_sub(RETENTION_DAYS * DAY_MILLIS)) as i64;
        let archive = (now.saturating_sub(ARCHIVE_DAYS * DAY_MILLIS)) as i64;
        let tx = self.conn.unchecked_transaction()?;
        let mut removed = 0;
        removed += tx.execute(
            "DELETE FROM bridge_requests WHERE updated_at < ?1",
            params![retention],
        )?;
        removed += tx.execute(
            "DELETE FROM bridge_results WHERE verified_at < ?1",
            params![retention],
        )?;
        removed += tx.execute(
            "DELETE FROM bridge_reports WHERE created_at < ?1",
            params![retention],
        )?;
        removed += tx.execute(
            "DELETE FROM child_sagas
             WHERE step IN ('done', 'failed') AND updated_at < ?1",
            params![retention],
        )?;
        // Archival hides; it never deletes. The ownership row outlives every
        // one of these.
        tx.execute(
            "UPDATE bridge_child_state
             SET archived_at = ?2
             WHERE archived_at IS NULL
               AND tombstoned_at IS NOT NULL
               AND tombstoned_at < ?1",
            params![archive, now as i64],
        )?;
        tx.commit()?;
        Ok(removed)
    }
}

// ── Row mapping ──────────────────────────────────────────────────────────

const STATE_COLS: &str = "child_id, state, acked_at, claimed_at, finishing_at, last_report_at, \
                          tombstoned_at, force_deleted_at, archived_at, updated_at";

const REQUEST_COLS: &str =
    "owner_id, key, verb, body_hash, state, response, deadline, created_at, updated_at";

const SAGA_COLS: &str = "owner_id, key, child_id, step, task_kind, task_body, worktree_path, \
                         branch, base_head, branch_claimed, finish_outcome, finish_message_id, \
                         scratch_minted, gate_dir, egress_endpoint, egress_token, mux_server, \
                         mux_window_id, mux_pane_id, mux_pane_pid, mux_launch_key, instance_id, \
                         lease_until, created_at, updated_at";

fn insert_bridge_child_on(
    conn: &Connection,
    child_id: &str,
    owner_id: &str,
    request_key: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO bridge_children (child_id, owner_id, request_key, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            child_id,
            owner_id,
            request_key,
            current_time_millis() as i64
        ],
    )?;
    Ok(())
}

/// The state write, against whatever connection the caller holds.
///
/// The timestamps a state implies are stamped here rather than by each caller,
/// so `finishing_at` cannot be forgotten on one of the paths that reaches
/// `finishing` and `tombstoned_at` cannot be forgotten on one that reaches a
/// terminal state. Both are set once and kept: a second `finishing` after a
/// `resume` keeps the first attempt's timestamp, which is what an operator
/// looking at a stuck child wants to know.
fn set_bridge_child_state_on(
    conn: &Connection,
    child_id: &str,
    state: ChildState,
) -> rusqlite::Result<()> {
    let now = current_time_millis() as i64;
    let finishing = (state == ChildState::Finishing).then_some(now);
    let tombstoned = state.is_terminal().then_some(now);
    conn.execute(
        "INSERT INTO bridge_child_state
             (child_id, state, finishing_at, tombstoned_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(child_id) DO UPDATE SET
             state = excluded.state,
             finishing_at = COALESCE(bridge_child_state.finishing_at, excluded.finishing_at),
             tombstoned_at = COALESCE(bridge_child_state.tombstoned_at, excluded.tombstoned_at),
             updated_at = excluded.updated_at",
        params![child_id, state.as_str(), finishing, tombstoned, now],
    )?;
    Ok(())
}

fn upsert_child_saga_on(conn: &Connection, saga: &ChildSaga) -> rusqlite::Result<()> {
    let now = current_time_millis() as i64;
    conn.execute(
        "INSERT INTO child_sagas
             (owner_id, key, child_id, step, task_kind, task_body, worktree_path, branch,
              base_head, branch_claimed, finish_outcome, finish_message_id, scratch_minted,
              gate_dir, egress_endpoint, egress_token, mux_server, mux_window_id, mux_pane_id,
              mux_pane_pid, mux_launch_key, instance_id, lease_until, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?24)
         ON CONFLICT(owner_id, key) DO UPDATE SET
             child_id = excluded.child_id,
             step = excluded.step,
             task_kind = excluded.task_kind,
             task_body = excluded.task_body,
             worktree_path = excluded.worktree_path,
             branch = excluded.branch,
             base_head = excluded.base_head,
             branch_claimed = excluded.branch_claimed,
             finish_outcome = excluded.finish_outcome,
             finish_message_id = excluded.finish_message_id,
             scratch_minted = excluded.scratch_minted,
             gate_dir = excluded.gate_dir,
             egress_endpoint = excluded.egress_endpoint,
             egress_token = excluded.egress_token,
             mux_server = excluded.mux_server,
             mux_window_id = excluded.mux_window_id,
             mux_pane_id = excluded.mux_pane_id,
             mux_pane_pid = excluded.mux_pane_pid,
             mux_launch_key = excluded.mux_launch_key,
             instance_id = excluded.instance_id,
             lease_until = excluded.lease_until,
             updated_at = excluded.updated_at",
        params![
            saga.owner_id,
            saga.key,
            saga.child_id,
            saga.step.unwrap_or(SagaStep::Accepted).as_str(),
            saga.task_kind,
            saga.task_body,
            saga.worktree_path,
            saga.branch,
            saga.base_head,
            saga.branch_claimed as i64,
            saga.finish_outcome,
            saga.finish_message_id,
            saga.scratch_minted as i64,
            saga.gate_dir,
            saga.egress_endpoint,
            saga.egress_token,
            saga.mux_server,
            saga.mux_window_id,
            saga.mux_pane_id,
            saga.mux_pane_pid.map(i64::from),
            saga.mux_launch_key,
            saga.instance_id,
            saga.lease_until.map(|v| v as i64),
            now,
        ],
    )?;
    Ok(())
}

fn map_child(row: &rusqlite::Row) -> rusqlite::Result<BridgeChild> {
    Ok(BridgeChild {
        child_id: row.get(0)?,
        owner_id: row.get(1)?,
        request_key: row.get(2)?,
        created_at: row.get::<_, i64>(3)? as u64,
    })
}

fn map_child_state(row: &rusqlite::Row) -> rusqlite::Result<BridgeChildState> {
    Ok(BridgeChildState {
        child_id: row.get(0)?,
        // A state friring cannot read is `unusable`: terminal, never
        // integrated, and visible to an operator — the narrow reading, where
        // guessing `ready` would put an unknown child back in the fan-out.
        state: row
            .get::<_, String>(1)?
            .parse()
            .unwrap_or(ChildState::Unusable),
        acked_at: opt_u64(row, 2)?,
        claimed_at: opt_u64(row, 3)?,
        finishing_at: opt_u64(row, 4)?,
        last_report_at: opt_u64(row, 5)?,
        tombstoned_at: opt_u64(row, 6)?,
        force_deleted_at: opt_u64(row, 7)?,
        archived_at: opt_u64(row, 8)?,
        updated_at: row.get::<_, i64>(9)? as u64,
    })
}

fn map_request(row: &rusqlite::Row) -> rusqlite::Result<JournaledRequest> {
    Ok(JournaledRequest {
        owner_id: row.get(0)?,
        key: row.get(1)?,
        verb: row.get(2)?,
        body_hash: row.get(3)?,
        state: RequestState::from_db(&row.get::<_, String>(4)?),
        response: row.get(5)?,
        deadline: opt_u64(row, 6)?,
        created_at: row.get::<_, i64>(7)? as u64,
        updated_at: row.get::<_, i64>(8)? as u64,
    })
}

fn map_saga(row: &rusqlite::Row) -> rusqlite::Result<ChildSaga> {
    Ok(ChildSaga {
        owner_id: row.get(0)?,
        key: row.get(1)?,
        child_id: row.get(2)?,
        // A step friring cannot read reconciles nothing: `failed` is the narrow
        // reading, and recovery acts only on what a step positively names.
        step: Some(row.get::<_, String>(3)?.parse().unwrap_or(SagaStep::Failed)),
        task_kind: row.get(4)?,
        task_body: row.get(5)?,
        worktree_path: row.get(6)?,
        branch: row.get(7)?,
        base_head: row.get(8)?,
        branch_claimed: row.get::<_, i64>(9)? != 0,
        finish_outcome: row.get(10)?,
        finish_message_id: row.get(11)?,
        scratch_minted: row.get::<_, i64>(12)? != 0,
        gate_dir: row.get(13)?,
        egress_endpoint: row.get(14)?,
        egress_token: row.get(15)?,
        mux_server: row.get(16)?,
        mux_window_id: row.get(17)?,
        mux_pane_id: row.get(18)?,
        mux_pane_pid: row
            .get::<_, Option<i64>>(19)?
            .and_then(|v| u32::try_from(v).ok()),
        mux_launch_key: row.get(20)?,
        instance_id: row.get(21)?,
        lease_until: opt_u64(row, 22)?,
        created_at: row.get::<_, i64>(23)? as u64,
        updated_at: row.get::<_, i64>(24)? as u64,
    })
}

fn map_result(row: &rusqlite::Row) -> rusqlite::Result<BridgeResult> {
    Ok(BridgeResult {
        child_id: row.get(0)?,
        message_id: row.get(1)?,
        // An outcome friring cannot read is `failed`: the reading that never
        // integrates a branch.
        outcome: row.get::<_, String>(2)?.parse().unwrap_or(Outcome::Failed),
        branch: row.get(3)?,
        head: row.get(4)?,
        dirty: row.get::<_, i64>(5)? != 0,
        ahead_of_base: u32::try_from(row.get::<_, i64>(6)?).unwrap_or(0),
        verified_at: row.get::<_, i64>(7)? as u64,
    })
}

fn map_report(row: &rusqlite::Row) -> rusqlite::Result<BridgeReport> {
    Ok(BridgeReport {
        child_id: row.get(0)?,
        seq: u32::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
        phase: row.get(2)?,
        progress: u8::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
        summary: row.get(4)?,
        needs_operator: row.get::<_, i64>(5)? != 0,
        artifact_paths: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
        created_at: row.get::<_, i64>(7)? as u64,
    })
}

fn opt_u64(row: &rusqlite::Row, index: usize) -> rusqlite::Result<Option<u64>> {
    Ok(row.get::<_, Option<i64>>(index)?.map(|v| v as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().expect("an in-memory database")
    }

    /// Authority is this row, so nothing may move it. The triggers are in SQL
    /// rather than in Rust because the rule has to hold against every writer of
    /// the file, including a friring with a bug in it.
    #[test]
    fn ownership_cannot_be_updated_or_deleted_through_sql() {
        let db = db();
        db.insert_bridge_child("c1", "owner", "k-1").unwrap();

        let moved = db.conn.execute(
            "UPDATE bridge_children SET owner_id = 'someone-else' WHERE child_id = 'c1'",
            [],
        );
        assert!(moved.is_err(), "an ownership row must not be updatable");
        assert!(moved.unwrap_err().to_string().contains("insert-only"));

        let removed = db
            .conn
            .execute("DELETE FROM bridge_children WHERE child_id = 'c1'", []);
        assert!(removed.is_err(), "an ownership row must not be deletable");
        assert!(removed.unwrap_err().to_string().contains("insert-only"));

        assert_eq!(db.bridge_child("c1").unwrap().unwrap().owner_id, "owner");
        assert!(db.owns_bridge_child("owner", "c1").unwrap());
        assert!(!db.owns_bridge_child("someone-else", "c1").unwrap());
    }

    /// The capacity read answers about **one** database.
    ///
    /// The defect it closes needs a peer instance to commit a child between the
    /// live read and the pending read, and no deterministic test in this
    /// process can arrange that: there is no point inside
    /// [`Database::reserved_child_slots`] where another connection can be made
    /// to write. What is pinnable is the mechanism, so that is what this
    /// asserts — the halves are read inside one transaction, and the fan-out
    /// check reads the pair rather than the halves. The arithmetic itself is
    /// pinned by `a_peers_uncommitted_launch_still_holds_a_slot`.
    #[test]
    fn the_capacity_read_takes_one_snapshot() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/storage/bridge.rs"),
        )
        .expect("this module's own source");
        let body = source
            .split_once("pub fn reserved_child_slots")
            .expect("the capacity read")
            .1
            .split_once("\n    }")
            .expect("its body")
            .0;
        assert!(
            body.contains("unchecked_transaction"),
            "the two halves must be read inside one transaction:\n{body}"
        );

        // And the caller must not have gone back to reading them separately.
        let spawn = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app/bridge_spawn.rs"),
        )
        .expect("the fan-out check's own source");
        let code = spawn
            .split_once("mod tests {")
            .map_or(spawn.as_str(), |(code, _)| code);
        for half in ["live_bridge_children", "pending_child_slot_claims"] {
            assert!(
                !code.contains(half),
                "the fan-out check must read the pair, never `{half}` on its own"
            );
        }
    }

    /// One `create` key makes one child, however many times it is replayed.
    #[test]
    fn one_request_key_makes_one_child() {
        let db = db();
        db.insert_bridge_child("c1", "owner", "k-1").unwrap();
        assert!(db.insert_bridge_child("c2", "owner", "k-1").is_err());
        assert_eq!(db.bridge_children_of("owner").unwrap().len(), 1);
    }

    #[test]
    fn a_replay_is_answered_from_the_journal_and_a_reused_key_is_refused() {
        let db = db();
        // Digest-shaped, like what `app::bridge::body_hash` writes: the
        // comparison below distinguishes a pinned digest from a legacy one, so a
        // fixture that was not one would exercise the wrong branch.
        let hash_a = "a".repeat(64);
        let hash_b = "b".repeat(64);
        assert_eq!(
            db.take_bridge_request("owner", "k-1", "create", &hash_a, None)
                .unwrap(),
            JournalLookup::Fresh
        );
        db.finish_bridge_request("owner", "k-1", RequestState::Done, "{\"ok\":true}")
            .unwrap();

        let replay = db
            .take_bridge_request("owner", "k-1", "create", &hash_a, None)
            .unwrap();
        let JournalLookup::Replay(row) = replay else {
            panic!("the same key and body is a replay: {replay:?}");
        };
        assert_eq!(row.response.as_deref(), Some("{\"ok\":true}"));
        assert_eq!(row.state, RequestState::Done);

        assert_eq!(
            db.take_bridge_request("owner", "k-1", "create", &hash_b, None)
                .unwrap(),
            JournalLookup::KeyReused
        );
        // And nothing was written for the second body.
        assert_eq!(
            db.bridge_request("owner", "k-1")
                .unwrap()
                .unwrap()
                .body_hash,
            hash_a
        );
    }

    /// A row written before the digest algorithm was pinned cannot have its hash
    /// recomputed, so the only two readings are "replay" and "reused key" — and
    /// the second would refuse a legitimate retry of a `create` that already made
    /// a child, leaving the caller with no way to learn its id.
    #[test]
    fn a_journal_row_from_before_the_pinned_digest_replays_rather_than_refusing() {
        let db = db();
        // What `DefaultHasher` used to write: sixteen hex characters.
        let legacy = "0123456789abcdef";
        db.take_bridge_request("owner", "k-legacy", "create", legacy, None)
            .unwrap();
        db.finish_bridge_request("owner", "k-legacy", RequestState::Done, "{\"ok\":true}")
            .unwrap();

        // A binary that has since pinned the algorithm computes a 64-hex digest
        // for the same body, and must still answer the retry.
        let pinned = "c".repeat(64);
        let replay = db
            .take_bridge_request("owner", "k-legacy", "create", &pinned, None)
            .unwrap();
        let JournalLookup::Replay(row) = replay else {
            panic!("a legacy row must replay, not refuse: {replay:?}");
        };
        assert_eq!(row.response.as_deref(), Some("{\"ok\":true}"));

        // A **pinned** row that disagrees is still a reused key: the compatibility
        // is for rows whose hash cannot be recomputed, and nothing else.
        db.take_bridge_request("owner", "k-pinned", "create", &pinned, None)
            .unwrap();
        assert_eq!(
            db.take_bridge_request("owner", "k-pinned", "create", &"d".repeat(64), None)
                .unwrap(),
            JournalLookup::KeyReused
        );
    }

    /// A replay returns the **exact bytes** of the first answer, not an
    /// equivalent one: a client comparing two answers to the same key must see
    /// one request, and a re-rendered response could differ in field order or in
    /// a timestamp and read as two.
    #[test]
    fn a_replay_returns_the_first_answers_exact_bytes() {
        let db = db();
        let first = r#"{"protocol":1,"key":"abc-1234","ok":true,"data":{"child":"c1"}}"#;
        db.take_bridge_request("owner", "abc-1234", "create", "h", None)
            .unwrap();
        db.finish_bridge_request("owner", "abc-1234", RequestState::Done, first)
            .unwrap();

        for _ in 0..3 {
            let JournalLookup::Replay(row) = db
                .take_bridge_request("owner", "abc-1234", "create", "h", None)
                .unwrap()
            else {
                panic!("a replay");
            };
            assert_eq!(row.response.as_deref(), Some(first));
        }
    }

    /// A key taken and not yet answered is a replay too, with no response: the
    /// caller waits rather than acting a second time.
    #[test]
    fn an_unanswered_key_replays_without_a_response() {
        let db = db();
        db.take_bridge_request("owner", "abc-1234", "create", "h", None)
            .unwrap();
        let JournalLookup::Replay(row) = db
            .take_bridge_request("owner", "abc-1234", "create", "h", None)
            .unwrap()
        else {
            panic!("a replay");
        };
        assert_eq!(row.state, RequestState::Accepted);
        assert_eq!(row.response, None);
    }

    /// One key per owner: the same key from two different sessions is two
    /// requests, because the key is only ever unique within a queue.
    #[test]
    fn a_key_is_scoped_to_its_owner() {
        let db = db();
        assert_eq!(
            db.take_bridge_request("owner-a", "abc-1234", "status", "h", None)
                .unwrap(),
            JournalLookup::Fresh
        );
        assert_eq!(
            db.take_bridge_request("owner-b", "abc-1234", "status", "h", None)
                .unwrap(),
            JournalLookup::Fresh
        );
    }

    #[test]
    fn a_dirty_child_still_holds_its_slot() {
        let db = db();
        for (id, state) in [
            ("c1", ChildState::Ready),
            ("c2", ChildState::Dirty),
            ("c3", ChildState::Done),
        ] {
            db.insert_bridge_child(id, "owner", id).unwrap();
            db.set_bridge_child_state(id, state).unwrap();
        }
        assert_eq!(db.live_bridge_children("owner").unwrap(), 2);
    }

    /// The timestamps a state implies are stamped once and kept, so a second
    /// finish after a `resume` does not erase when the first one started.
    #[test]
    fn finishing_and_tombstone_timestamps_are_stamped_once() {
        let db = db();
        db.insert_bridge_child("c1", "owner", "k-1").unwrap();
        db.set_bridge_child_state("c1", ChildState::Finishing)
            .unwrap();
        let first = db.bridge_child_state("c1").unwrap().unwrap();
        assert!(first.finishing_at.is_some());
        assert!(first.tombstoned_at.is_none());

        db.set_bridge_child_state("c1", ChildState::Dirty).unwrap();
        db.set_bridge_child_state("c1", ChildState::Finishing)
            .unwrap();
        let again = db.bridge_child_state("c1").unwrap().unwrap();
        assert_eq!(again.finishing_at, first.finishing_at);

        db.set_bridge_child_state("c1", ChildState::Done).unwrap();
        assert!(db
            .bridge_child_state("c1")
            .unwrap()
            .unwrap()
            .tombstoned_at
            .is_some());
    }

    #[test]
    fn a_child_keeps_only_its_most_recent_reports() {
        let db = db();
        db.insert_bridge_child("c1", "owner", "k-1").unwrap();
        db.set_bridge_child_state("c1", ChildState::Ready).unwrap();
        for seq in 1..=(MAX_REPORTS_PER_CHILD + 5) {
            db.insert_bridge_report(&BridgeReport {
                child_id: "c1".to_string(),
                seq,
                phase: "implementing".to_string(),
                progress: 10,
                summary: format!("report {seq}"),
                needs_operator: false,
                artifact_paths: Vec::new(),
                created_at: u64::from(seq),
            })
            .unwrap();
        }
        let kept = db.bridge_reports("c1", 100).unwrap();
        assert_eq!(kept.len() as u32, MAX_REPORTS_PER_CHILD);
        assert_eq!(kept.first().unwrap().seq, MAX_REPORTS_PER_CHILD + 5);
        assert!(db
            .bridge_child_state("c1")
            .unwrap()
            .unwrap()
            .last_report_at
            .is_some());
    }

    /// The lease keeps two TUIs from both polling one directory, and hands over
    /// once it expires rather than deadlocking on a dead instance.
    #[test]
    fn only_one_instance_holds_a_brokers_lease() {
        let db = db();
        assert!(db.claim_broker_lease("s1", "inst-a", 60_000).unwrap());
        assert!(!db.claim_broker_lease("s1", "inst-b", 60_000).unwrap());
        assert_eq!(
            db.broker_lease_holder("s1").unwrap().as_deref(),
            Some("inst-a")
        );
        // A renewal by the holder keeps it.
        assert!(db.claim_broker_lease("s1", "inst-a", 60_000).unwrap());
        // An expired lease is taken over.
        db.conn
            .execute("UPDATE bridge_brokers SET lease_until = 0", [])
            .unwrap();
        assert!(db.claim_broker_lease("s1", "inst-b", 60_000).unwrap());
        db.release_broker_leases("inst-b").unwrap();
        assert!(db.broker_lease_holder("s1").unwrap().is_none());
    }

    /// What the lease read costs, per call, prepared each time against
    /// prepared once — the evidence behind ADR-P16's decision **not** to cache
    /// the held-lease set in `App::bridge`.
    ///
    /// A measurement, not a gate: it prints and asserts nothing about a clock,
    /// so it is `#[ignore]`d per ADR-P5. Re-run it with
    ///
    /// ```text
    /// cargo nextest run --run-ignored only -E 'test(measure_broker_lease_read)' --no-capture
    /// ```
    #[test]
    #[ignore = "a timing measurement, not a regression gate (ADR-P5)"]
    fn measure_broker_lease_read_cost() {
        const SESSIONS: usize = 400;
        const CALLS: usize = 100_000;
        let db = db();
        for n in 0..SESSIONS {
            db.claim_broker_lease(&format!("s{n}"), "inst-a", 600_000)
                .unwrap();
        }
        let keys: Vec<String> = (0..SESSIONS).map(|n| format!("s{n}")).collect();
        let now = current_time_millis() as i64;

        let started = std::time::Instant::now();
        for n in 0..CALLS {
            let holder = db.broker_lease_holder(&keys[n % SESSIONS]).unwrap();
            assert!(holder.is_some());
        }
        let cached = started.elapsed();

        let started = std::time::Instant::now();
        for n in 0..CALLS {
            let holder: Option<String> = db
                .conn
                .query_row(
                    "SELECT instance_id FROM bridge_brokers
                     WHERE session_id = ?1 AND lease_until >= ?2",
                    params![&keys[n % SESSIONS], now],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert!(holder.is_some());
        }
        let uncached = started.elapsed();

        println!(
            "broker_lease_holder over {SESSIONS} rows, {CALLS} calls:\n  \
             prepare_cached: {:?} total, {:.0} ns/call\n  \
             prepare each time: {:?} total, {:.0} ns/call",
            cached,
            cached.as_nanos() as f64 / CALLS as f64,
            uncached,
            uncached.as_nanos() as f64 / CALLS as f64,
        );
    }

    #[test]
    fn a_saga_round_trips_and_recovery_sees_only_the_unfinished_ones() {
        let db = db();
        let saga = ChildSaga {
            owner_id: "owner".to_string(),
            key: "k-1".to_string(),
            child_id: Some("c1".to_string()),
            step: Some(SagaStep::Pane),
            worktree_path: Some("/w/c1".to_string()),
            branch: Some("feat/c1".to_string()),
            base_head: Some("abc123".to_string()),
            scratch_minted: true,
            gate_dir: Some("/g/c1".to_string()),
            egress_endpoint: Some("tcp:8123".to_string()),
            egress_token: Some("secret".to_string()),
            mux_pane_id: Some("%7".to_string()),
            mux_pane_pid: Some(4242),
            ..ChildSaga::default()
        };
        db.upsert_child_saga(&saga).unwrap();
        let read = db.child_saga("owner", "k-1").unwrap().unwrap();
        assert_eq!(read.step, Some(SagaStep::Pane));
        assert_eq!(read.mux_pane_pid, Some(4242));
        assert_eq!(read.egress_token.as_deref(), Some("secret"));
        assert_eq!(db.unfinished_child_sagas().unwrap().len(), 1);

        db.upsert_child_saga(&ChildSaga {
            step: Some(SagaStep::Done),
            ..read
        })
        .unwrap();
        assert!(db.unfinished_child_sagas().unwrap().is_empty());
    }

    /// A test-only session row, so the S6 transaction has something to commit.
    fn child_session(id: crate::session::SessionId) -> crate::sync::state::SharedSession {
        crate::sync::state::SharedSession {
            id,
            name: "tbs-child".to_string(),
            agent: "worker".to_string(),
            backend_id: "friring:@9".to_string(),
            backend_type: "tmux".to_string(),
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

    /// The saga's `committed` step is the record *of* this transaction, so a
    /// failure writing it must leave the child unmade.
    ///
    /// Recovery decides only from `saga.step`. A commit that landed the rows and
    /// then failed to advance the step would be reconciled down the pre-commit
    /// branch — against an immutable ownership row it cannot delete — and the
    /// child would sit in `Starting` holding one of the owner's fan-out slots.
    #[test]
    fn a_commit_whose_saga_step_cannot_be_written_leaves_no_child() {
        let db = db();
        let child = crate::session::SessionId::default();
        let owner = crate::session::SessionId::default();
        let session = child_session(child);
        // The saga this commit is trying to advance, recorded at the step the
        // spawn saga really reaches before S6.
        db.upsert_child_saga(&ChildSaga {
            owner_id: owner.to_string(),
            key: "k-1".to_string(),
            child_id: Some(child.to_string()),
            step: Some(SagaStep::Pane),
            ..ChildSaga::default()
        })
        .unwrap();
        // The only seam that fails a write *inside* the transaction and nowhere
        // else: the saga statement is the last one, so every row above it has
        // already been written when it fails. A trigger rather than a dropped
        // table, so the row whose step must not have advanced is still there to
        // read afterwards.
        db.conn_ref()
            .execute(
                "CREATE TRIGGER no_saga BEFORE INSERT ON child_sagas \
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
                [],
            )
            .unwrap();

        let saga = ChildSaga {
            owner_id: owner.to_string(),
            key: "k-1".to_string(),
            child_id: Some(child.to_string()),
            step: Some(SagaStep::Committed),
            ..ChildSaga::default()
        };
        let outcome = db.commit_bridge_child(&ChildCommit {
            session: &session,
            child_id: &child.to_string(),
            owner_id: &owner.to_string(),
            request_key: "k-1",
            new_ownership: true,
            repo_root: "/repo",
            worktree_path: Some("/w/c1"),
            branch: "feat/c1",
            task_kind: "team-node",
            task_body: "do the thing",
            role_hint: None,
            saga: Some(&saga),
        });
        assert!(outcome.is_err(), "the saga write must fail the commit");
        assert!(outcome.unwrap_err().contains("saga step"));

        // Nothing above it survived: the child was never created.
        assert!(db.bridge_child(&child.to_string()).unwrap().is_none());
        assert!(db.bridge_child_state(&child.to_string()).unwrap().is_none());
        assert!(db
            .session_repo_roots(&child.to_string())
            .unwrap()
            .is_empty());
        assert!(db.get_session_by_id(child).unwrap().is_none());
        assert_eq!(
            db.child_saga(&owner.to_string(), "k-1")
                .unwrap()
                .unwrap()
                .step,
            Some(SagaStep::Pane),
            "a failed commit must leave the saga below the committed line"
        );
    }

    /// A released resume reserves its slot and records its saga atomically.
    /// Recovery must never see a relaunch whose `starting` capacity claim did
    /// not land, and the child must not consume a slot when either write fails.
    #[test]
    fn a_resume_whose_slot_claim_fails_leaves_no_saga_and_stays_stopped() {
        let db = db();
        db.insert_bridge_child("child", "owner", "create-key")
            .unwrap();
        db.set_bridge_child_state("child", ChildState::Stopped)
            .unwrap();
        db.conn_ref()
            .execute(
                "CREATE TRIGGER no_resume_state \
                 BEFORE UPDATE OF state ON bridge_child_state \
                 WHEN NEW.state = 'starting' \
                 BEGIN SELECT RAISE(ABORT, 'refused'); END",
                [],
            )
            .unwrap();

        let saga = ChildSaga {
            owner_id: "owner".to_string(),
            key: "resume-key".to_string(),
            child_id: Some("child".to_string()),
            step: Some(SagaStep::Named),
            ..ChildSaga::default()
        };
        let error = db
            .begin_bridge_child_resume(&saga, "child")
            .expect_err("the injected state refusal must fail the resume reservation");
        assert!(error.contains("resumed slot"), "{error}");
        assert!(
            db.child_saga("owner", "resume-key").unwrap().is_none(),
            "the failed slot claim left an unfinished resume saga"
        );
        assert_eq!(
            db.bridge_child_state("child").unwrap().map(|row| row.state),
            Some(ChildState::Stopped),
            "the failed slot claim consumed the parked state"
        );
    }

    #[test]
    fn a_session_may_only_create_children_in_a_repository_it_records() {
        let db = db();
        db.upsert_session_repo("s1", "/repo/a", "worktree", Some("/w/a"), Some("main"))
            .unwrap();
        assert!(db.session_owns_repo("s1", "/repo/a").unwrap());
        assert!(!db.session_owns_repo("s1", "/repo/b").unwrap());
        assert!(!db.session_owns_repo("s2", "/repo/a").unwrap());
        assert_eq!(db.session_repo_roots("s1").unwrap(), ["/repo/a"]);
    }

    /// Pruning drops what has aged out and hides a long-dead child, and it
    /// never touches ownership.
    #[test]
    fn pruning_never_removes_ownership() {
        let db = db();
        db.insert_bridge_child("c1", "owner", "k-1").unwrap();
        db.set_bridge_child_state("c1", ChildState::Done).unwrap();
        db.conn
            .execute("UPDATE bridge_child_state SET tombstoned_at = 0", [])
            .unwrap();
        db.take_bridge_request("owner", "k-old", "status", "h", None)
            .unwrap();
        db.conn
            .execute("UPDATE bridge_requests SET updated_at = 0", [])
            .unwrap();

        db.prune_bridge_state().unwrap();

        assert!(db.bridge_request("owner", "k-old").unwrap().is_none());
        assert!(db.bridge_child("c1").unwrap().is_some());
        assert!(db
            .bridge_child_state("c1")
            .unwrap()
            .unwrap()
            .archived_at
            .is_some());
    }
}
