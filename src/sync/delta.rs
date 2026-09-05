use std::collections::{HashMap, HashSet};

use crate::session::SessionId;

use super::state::{SharedSession, SharedState};

/// Represents the delta (changes) between two shared states.
///
/// Used to communicate to an instance what changed externally
/// so it can update its local view accordingly.
///
/// Every vector below preserves the order of the [`SharedState`] it was derived
/// from — see [`StateDelta::compute`] for why that matters.
#[derive(Debug, Default, Clone)]
pub struct StateDelta {
    /// Sessions that were created by other instances, in `new` state order.
    pub added_sessions: Vec<SharedSession>,

    /// Session IDs that were deleted by other instances, in `old` state order.
    pub removed_sessions: Vec<SessionId>,

    /// Sessions that were updated (metadata changed), in `new` state order.
    pub updated_sessions: Vec<SharedSession>,

    /// Latest session counter from external state.
    /// Should be merged using max(local, external).
    pub counter_increment: usize,
}

impl StateDelta {
    /// Compute the delta between two states.
    ///
    /// Determines which sessions were added, removed, or updated
    /// by comparing the old state (what we knew) with the new state
    /// (what other instances know).
    ///
    /// The two collections below are only ever *probed*; the delta's vectors
    /// are built by walking `new.sessions` / `old.sessions` so each keeps that
    /// state's order. That order is load-bearing, not cosmetic: `SharedState`
    /// comes from `list_active_sessions`, which sorts by `display_order` then
    /// `created_at`, and `App::apply_added_sessions` adopts in delta order
    /// while [`crate::ui::project_list::compute_session_order`] renders
    /// never-moved sessions (`display_order == None`) in adoption order.
    /// Iterating a `HashMap` here instead would re-randomize that order per
    /// process, so several sessions created between two 250 ms polls — a
    /// scripted burst of `friring-cli session create`, or another instance
    /// spawning a fleet — would land in the session list shuffled.
    pub fn compute(old: &SharedState, new: &SharedState) -> Self {
        let old_session_map: HashMap<SessionId, &SharedSession> = old
            .sessions
            .iter()
            .filter(|s| !s.tombstone)
            .map(|s| (s.id, s))
            .collect();

        let new_session_ids: HashSet<SessionId> = new
            .sessions
            .iter()
            .filter(|s| !s.tombstone)
            .map(|s| s.id)
            .collect();

        let mut delta = StateDelta::default();

        for session in new.sessions.iter().filter(|s| !s.tombstone) {
            match old_session_map.get(&session.id) {
                None => delta.added_sessions.push(session.clone()),
                Some(old_session) => {
                    if session_changed(old_session, session) {
                        delta.updated_sessions.push(session.clone());
                    }
                }
            }
        }

        for session in old.sessions.iter().filter(|s| !s.tombstone) {
            if !new_session_ids.contains(&session.id) {
                delta.removed_sessions.push(session.id);
            }
        }

        delta.counter_increment = new.session_counter;

        delta
    }

    /// Check if this delta has any meaningful changes.
    pub fn is_empty(&self) -> bool {
        self.added_sessions.is_empty()
            && self.removed_sessions.is_empty()
            && self.updated_sessions.is_empty()
    }
}

/// Check if a session's key metadata changed.
/// Ignores tombstone state since delta computation filters those out.
fn session_changed(old: &SharedSession, new: &SharedSession) -> bool {
    old.name != new.name
        || old.agent != new.agent
        || old.backend_id != new.backend_id
        || old.backend_type != new.backend_type
        || old.agent_session_id != new.agent_session_id
        || old.cwd != new.cwd
        || old.additional_dirs != new.additional_dirs
        || old.workspace_dir != new.workspace_dir
        || old.worktrees != new.worktrees
        || old.shell_backend_id != new.shell_backend_id
        || old.sandbox_profile != new.sandbox_profile
        // The applied half changes without the desired half moving — a relaunch
        // that lost (or regained) the boundary rewrites only this — so an
        // instance that misses it keeps rendering the previous verdict.
        || old.sandbox_enforcement != new.sandbox_enforcement
        || old.parent_session_id != new.parent_session_id
        || old.display_order != new.display_order
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_session(id: SessionId, name: &str) -> SharedSession {
        SharedSession {
            id,
            name: name.to_string(),
            agent: "claude".to_string(),
            backend_id: "friring:@0".to_string(),
            backend_type: "tmux".to_string(),
            agent_session_id: None,
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: Vec::new(),
            shell_backend_id: None,
            sandbox_profile: None,
            sandbox_enforcement: crate::session::SandboxEnforcement::default(),
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
        }
    }

    #[test]
    fn empty_delta_when_no_changes() {
        let state = SharedState::new();
        let delta = StateDelta::compute(&state, &state);
        assert!(delta.is_empty());
    }

    #[test]
    fn added_sessions_detected() {
        let old_state = SharedState::new();

        let mut new_state = SharedState::new();
        new_state
            .sessions
            .push(make_session(SessionId::default(), "New Session"));

        let delta = StateDelta::compute(&old_state, &new_state);

        assert_eq!(delta.added_sessions.len(), 1);
        assert_eq!(delta.added_sessions[0].name, "New Session");
        assert!(delta.removed_sessions.is_empty());
        assert!(delta.updated_sessions.is_empty());
    }

    #[test]
    fn removed_sessions_detected() {
        let session_id = SessionId::default();
        let mut old_state = SharedState::new();
        old_state
            .sessions
            .push(make_session(session_id, "Old Session"));

        let new_state = SharedState::new();

        let delta = StateDelta::compute(&old_state, &new_state);

        assert!(delta.added_sessions.is_empty());
        assert_eq!(delta.removed_sessions.len(), 1);
        assert_eq!(delta.removed_sessions[0], session_id);
        assert!(delta.updated_sessions.is_empty());
    }

    #[test]
    fn updated_sessions_detected() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        old_state
            .sessions
            .push(make_session(session_id, "Session A"));

        let mut new_state = SharedState::new();
        new_state
            .sessions
            .push(make_session(session_id, "Session A (renamed)"));

        let delta = StateDelta::compute(&old_state, &new_state);

        assert!(delta.added_sessions.is_empty());
        assert!(delta.removed_sessions.is_empty());
        assert_eq!(delta.updated_sessions.len(), 1);
        assert_eq!(delta.updated_sessions[0].name, "Session A (renamed)");
    }

    #[test]
    fn tombstoned_sessions_ignored() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        old_state.sessions.push(make_session(session_id, "Session"));

        let mut new_state = SharedState::new();
        let mut s = make_session(session_id, "Session");
        s.tombstone = true;
        s.tombstone_at = Some(0);
        new_state.sessions.push(s);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert!(delta.added_sessions.is_empty());
        assert_eq!(delta.removed_sessions.len(), 1);
        assert!(delta.updated_sessions.is_empty());
    }

    #[test]
    fn counter_increment_tracked() {
        let mut old_state = SharedState::new();
        old_state.session_counter = 5;

        let mut new_state = SharedState::new();
        new_state.session_counter = 10;

        let delta = StateDelta::compute(&old_state, &new_state);

        assert_eq!(delta.counter_increment, 10);
    }

    #[test]
    fn session_changed_detects_agent_change() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        old_state.sessions.push(make_session(session_id, "Session"));

        let mut new_state = SharedState::new();
        let mut s = make_session(session_id, "Session");
        s.agent = "codex".to_string();
        new_state.sessions.push(s);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert_eq!(delta.updated_sessions.len(), 1);
        assert_eq!(delta.updated_sessions[0].agent, "codex");
    }

    #[test]
    fn session_changed_detects_cwd_change() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        let mut s1 = make_session(session_id, "Session");
        s1.cwd = Some(PathBuf::from("/home/user"));
        old_state.sessions.push(s1);

        let mut new_state = SharedState::new();
        let mut s2 = make_session(session_id, "Session");
        s2.cwd = Some(PathBuf::from("/home/user/project"));
        new_state.sessions.push(s2);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert_eq!(delta.updated_sessions.len(), 1);
    }

    #[test]
    fn session_changed_detects_display_order_change() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        let mut s1 = make_session(session_id, "Session");
        s1.display_order = Some(0);
        old_state.sessions.push(s1);

        let mut new_state = SharedState::new();
        let mut s2 = make_session(session_id, "Session");
        s2.display_order = Some(2);
        new_state.sessions.push(s2);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert_eq!(delta.updated_sessions.len(), 1);
        assert_eq!(delta.updated_sessions[0].display_order, Some(2));
    }

    #[test]
    fn session_changed_detects_shell_backend_id_change() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        old_state.sessions.push(make_session(session_id, "Session"));

        let mut new_state = SharedState::new();
        let mut s = make_session(session_id, "Session");
        s.shell_backend_id = Some("friring:@1".to_string());
        new_state.sessions.push(s);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert_eq!(delta.updated_sessions.len(), 1);
        assert_eq!(
            delta.updated_sessions[0].shell_backend_id,
            Some("friring:@1".to_string())
        );
    }

    #[test]
    fn session_changed_detects_sandbox_profile_change() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        old_state.sessions.push(make_session(session_id, "Session"));

        let mut new_state = SharedState::new();
        let mut s = make_session(session_id, "Session");
        s.sandbox_profile = Some("dev".to_string());
        new_state.sessions.push(s);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert_eq!(delta.updated_sessions.len(), 1);
        assert_eq!(
            delta.updated_sessions[0].sandbox_profile,
            Some("dev".to_string())
        );
    }

    /// A relaunch that lost the boundary changes only the *applied* half — the
    /// profile it asked for is untouched — so an instance whose delta ignored
    /// this field would go on showing a shield over an agent on the host.
    #[test]
    fn session_changed_detects_sandbox_enforcement_change() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        let mut before = make_session(session_id, "Session");
        before.sandbox_profile = Some("dev".to_string());
        before.sandbox_enforcement = crate::session::SandboxEnforcement::Unrecorded;
        old_state.sessions.push(before);

        let mut new_state = SharedState::new();
        let mut after = make_session(session_id, "Session");
        after.sandbox_profile = Some("dev".to_string());
        after.sandbox_enforcement =
            crate::session::SandboxEnforcement::Unenforced("bwrap is not installed".to_string());
        new_state.sessions.push(after);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert_eq!(delta.updated_sessions.len(), 1);
        assert_eq!(
            delta.updated_sessions[0]
                .sandbox_enforcement
                .unenforced_reason(),
            Some("bwrap is not installed"),
        );

        // …and the way back: a relaunch that regains the boundary clears the
        // warning, which is just as much a change to notice.
        let delta = StateDelta::compute(&new_state, &old_state);
        assert_eq!(delta.updated_sessions.len(), 1);
        assert_eq!(
            delta.updated_sessions[0]
                .sandbox_enforcement
                .unenforced_reason(),
            None
        );
    }

    #[test]
    fn no_update_when_session_unchanged() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        let mut s1 = make_session(session_id, "Session");
        s1.agent_session_id = Some("claude-123".to_string());
        s1.cwd = Some(PathBuf::from("/home/user"));
        old_state.sessions.push(s1);

        let mut new_state = SharedState::new();
        let mut s2 = make_session(session_id, "Session");
        s2.agent_session_id = Some("claude-123".to_string());
        s2.cwd = Some(PathBuf::from("/home/user"));
        new_state.sessions.push(s2);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert!(delta.is_empty());
    }

    #[test]
    fn session_changed_detects_additional_dirs_change() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        let mut s1 = make_session(session_id, "S");
        s1.cwd = Some(PathBuf::from("/repo1"));
        s1.additional_dirs = vec![PathBuf::from("/repo2")];
        old_state.sessions.push(s1);

        let mut new_state = SharedState::new();
        let mut s2 = make_session(session_id, "S");
        s2.cwd = Some(PathBuf::from("/repo1"));
        s2.additional_dirs = vec![PathBuf::from("/repo2"), PathBuf::from("/repo3")];
        new_state.sessions.push(s2);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert!(!delta.is_empty());
        assert_eq!(delta.updated_sessions.len(), 1);
    }

    /// Twelve sessions make a false pass (a `HashMap` that happens to iterate
    /// in insertion order) a 1-in-479-million accident rather than a flake.
    fn ordered_state(names: &[&str]) -> SharedState {
        let mut state = SharedState::new();
        for name in names {
            state
                .sessions
                .push(make_session(SessionId::default(), name));
        }
        state
    }

    const TWELVE: [&str; 12] = [
        "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11",
    ];

    /// `added_sessions` must come out in `new.sessions` order — that is the
    /// order `list_active_sessions` sorted the DB into, and the session list
    /// renders never-moved sessions (`display_order == None`) in the order the
    /// app adopted them (see `crate::ui::project_list::compute_session_order`).
    #[test]
    fn added_sessions_preserve_new_state_order() {
        let new_state = ordered_state(&TWELVE);

        let delta = StateDelta::compute(&SharedState::new(), &new_state);

        let expected: Vec<SessionId> = new_state.sessions.iter().map(|s| s.id).collect();
        let actual: Vec<SessionId> = delta.added_sessions.iter().map(|s| s.id).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn removed_sessions_preserve_old_state_order() {
        let old_state = ordered_state(&TWELVE);

        let delta = StateDelta::compute(&old_state, &SharedState::new());

        let expected: Vec<SessionId> = old_state.sessions.iter().map(|s| s.id).collect();
        assert_eq!(delta.removed_sessions, expected);
    }

    #[test]
    fn updated_sessions_preserve_new_state_order() {
        let old_state = ordered_state(&TWELVE);
        let mut new_state = old_state.clone();
        for session in &mut new_state.sessions {
            session.agent = "codex".to_string();
        }

        let delta = StateDelta::compute(&old_state, &new_state);

        let expected: Vec<SessionId> = new_state.sessions.iter().map(|s| s.id).collect();
        let actual: Vec<SessionId> = delta.updated_sessions.iter().map(|s| s.id).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn session_changed_detects_workspace_dir_change() {
        let session_id = SessionId::default();

        let mut old_state = SharedState::new();
        old_state.sessions.push(make_session(session_id, "S"));

        let mut new_state = SharedState::new();
        let mut s2 = make_session(session_id, "S");
        s2.workspace_dir = Some(PathBuf::from("/home/dev/named-ws"));
        new_state.sessions.push(s2);

        let delta = StateDelta::compute(&old_state, &new_state);

        assert!(!delta.is_empty());
        assert_eq!(delta.updated_sessions.len(), 1);
        assert_eq!(
            delta.updated_sessions[0].workspace_dir,
            Some(PathBuf::from("/home/dev/named-ws"))
        );
    }
}
