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
//! hook state is authoritative but only exists for agents whose hooks are
//! wired, while the pane scrape works for any agent but is a heuristic. Either
//! one is enough to refuse.

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
    db.load_hook_states()
        .ok()
        .and_then(|states| states.get(&session).and_then(|h| h.state.clone()))
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
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
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
