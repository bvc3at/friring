//! The database half of the pane-input guard: refuse to type into a session
//! the agent itself has reported as waiting for approval.
//!
//! Every headless sender ends with a synthetic `Enter`, which a modal reads as
//! the operator answering it — see [`crate::agent::tmux::MODAL_MARKERS`] for
//! what that costs. `agent::tmux` catches this by scraping the visible pane;
//! this module catches it from the other side, using the state the agent's own
//! lifecycle hooks already report through `friring-cli session signal`
//! (`extensions/hooks/claude.json` maps Claude Code's permission `Notification`
//! to `--state blocked`).
//!
//! Two signals rather than one because each covers the other's blind spot: the
//! hook state catches a dialog whose wording no marker matches, but only for
//! agents whose hooks are wired, while the pane scrape works for any agent and
//! is a heuristic.
//!
//! # Which callers use which signal, and why
//!
//! Not every caller gets both vetoes, and that asymmetry is deliberate.
//!
//! `blocked` is **not** "a dialog is on screen now" — it is "a dialog was
//! raised at some point in this turn". Claude Code has no after-approval hook:
//! `PreToolUse` fires *before* the permission prompt, and the next signal is
//! `Stop` at the end of the turn, so an **approved** tool call reports
//! `blocked` for its entire run. Measured, and pinned by the
//! `claude-blocked-spans-tool-run` e2e: 38 consecutive `blocked` samples across
//! a 20 s approved `Bash` call, then straight to `done`, never `working`.
//!
//! So the signal is only safe to act on where a false refusal is cheap:
//!
//! - **`message send`/`reply` wake** and **`session send`** consult it. A
//!   deferred wake is retried, and `session send` is interactive with `--force`
//!   one keystroke away.
//! - **`send` automations** and **task delivery** deliberately do **not**.
//!   They fire on a schedule and don't retry until the next one, so honoring a
//!   stale `blocked` would silently skip every fire aimed at a session that is
//!   merely running a long approved tool call — an ordinary thing to be doing.
//!   Those paths take the pane scrape alone, which cannot go stale.
//!
//! The residual gap that leaves is narrow and accepted: an agent that wires
//! friring's status hooks *and* renders a dialog no marker matches can still be
//! typed into by an automation. Widen [`crate::agent::tmux::MODAL_MARKERS`] to
//! close it rather than reintroducing the stale veto.

use crate::session::SessionId;
use crate::storage::Database;

/// The hook state an agent reports while it waits for the user to approve
/// something (`extensions/hooks/*`). Mirrors
/// [`SessionStatus::Blocked`](crate::session::SessionStatus).
const BLOCKED_STATE: &str = "blocked";

/// Whether `session` has told friring it is waiting on a permission prompt.
///
/// Best-effort: an unreadable `sessions` row reports "not blocked" and leaves
/// the decision to the pane scrape in `agent::tmux`, so a database hiccup can't
/// stop every delivery on its own.
pub(crate) fn blocked_on_prompt(db: &Database, session: SessionId) -> bool {
    // One row, not the whole map: this runs on every send and once per owed
    // wake in the retry sweep, so a full hook-state scan per check would cost
    // sessions × pending for an answer about a single session.
    db.hook_state_of(session)
        .ok()
        .flatten()
        .is_some_and(|state| state == BLOCKED_STATE)
}

/// The reason string reported when [`blocked_on_prompt`] refuses a write.
/// Phrased for a CLI caller that has no view of the recipient's screen.
pub(crate) const BLOCKED_REASON: &str = "recipient reported itself blocked on a permission prompt";

/// Why the guard refused to type into a pane, ready for a `--json` field or an
/// error message. `marker` is the [`crate::agent::tmux::MODAL_MARKERS`] entry a
/// pane scrape matched.
pub(crate) fn modal_reason(marker: &str) -> String {
    format!("recipient's pane is showing a modal ({marker:?}) awaiting a keypress")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::SharedSession;

    fn db_with_session() -> (Database, SessionId) {
        let db = Database::open_in_memory().unwrap();
        let id = SessionId::default();
        let shared = SharedSession {
            id,
            name: "guarded".into(),
            agent: "dev".into(),
            backend_id: String::new(),
            backend_type: "local-tmux".into(),
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
        };
        db.upsert_session(&shared).unwrap();
        (db, id)
    }

    #[test]
    fn only_the_blocked_state_refuses() {
        let (db, id) = db_with_session();
        // No hook has fired yet.
        assert!(!blocked_on_prompt(&db, id));

        for state in ["idle", "working", "done"] {
            db.set_hook_state(id, state).unwrap();
            assert!(!blocked_on_prompt(&db, id), "{state} must not refuse");
        }

        db.set_hook_state(id, "blocked").unwrap();
        assert!(blocked_on_prompt(&db, id));

        // Answering the prompt runs a tool, whose PreToolUse hook reports
        // `working` — so the guard lifts on its own with no extra bookkeeping.
        db.set_hook_state(id, "working").unwrap();
        assert!(!blocked_on_prompt(&db, id));
    }

    #[test]
    fn unknown_session_is_not_blocked() {
        let (db, _) = db_with_session();
        assert!(!blocked_on_prompt(&db, SessionId::default()));
    }

    #[test]
    fn modal_reason_quotes_the_marker() {
        assert!(modal_reason("Do you want to").contains("\"Do you want to\""));
    }
}
