//! In-progress new-session wizard state
//! (host → repo → [base branch] → name → [branch name] → agent).
//!
//! Grouped out of the [`App`](super::App) god object. Fields are `pub(crate)`
//! so call-sites keep direct access (`self.new_session.repo_path`). The whole
//! struct describes one pending flow; it is populated step by step by the
//! picker modals and consumed (or reset) when the flow completes or is
//! cancelled.

use std::path::PathBuf;
use std::sync::mpsc;

use crate::session::{SessionConfig, SessionId, WorktreeInfo};

/// State accumulated across the multi-step new-session flow. Also reused by
/// the fork (`Ctrl+F`) and restart (`Ctrl+R`) flows, which pre-seed parts of
/// it (`fork` / `restart` flag the variant).
#[derive(Default)]
pub(crate) struct NewSessionWizardState {
    /// Backend chosen for the flow (`ssh:<host>`), or `None` for the local
    /// default. Set by the host picker, cleared when the flow completes or is
    /// cancelled.
    pub(crate) backend: Option<String>,
    pub(crate) repo_path: Option<PathBuf>,
    pub(crate) all_repos: Option<Vec<PathBuf>>,
    /// Normal (non-worktree) repos to include alongside worktree repos.
    pub(crate) normal_repos: Vec<PathBuf>,
    pub(crate) base_branch: Option<String>,
    pub(crate) session_name: Option<String>,
    pub(crate) spawn_config: Option<SessionConfig>,
    pub(crate) spawn_worktrees: Vec<WorktreeInfo>,
    /// Extra working directories (non-primary worktrees + normal repos) to
    /// attach to the spawned session's `SessionInfo`. Consumed by
    /// `do_spawn_session`.
    pub(crate) additional_dirs: Vec<PathBuf>,
    /// Base branch the worktrees were forked from, carried from the worktree
    /// flow to the spawn so it can be persisted (scopes the code-review view to
    /// `<base>..HEAD`). `None` for bare-repo / fork spawns. Consumed by
    /// `do_spawn_session`/`do_spawn_session_async`.
    pub(crate) spawn_base_branch: Option<String>,
    /// Parent session for the spawned session (lead/worker linkage). Set by
    /// fork (`Ctrl+F`, the forked-from session) and stale-session respawn;
    /// consumed by `do_spawn_session`/`do_spawn_session_async`.
    pub(crate) parent_session_id: Option<SessionId>,
    pub(crate) fork: bool,
    pub(crate) restart: bool,
    /// A conversation-import spawn (`i` in the session list): the agent and the
    /// pinned resume ids are already on `spawn_config`, so the name modal
    /// spawns directly instead of opening the agent picker (mirrors `fork`).
    pub(crate) import: bool,
    pub(crate) spawn_name: Option<String>,
    /// Completion signal of the background `git fetch origin` kicked off when
    /// branch selection starts (ADR-P12). The worktree-create worker waits on
    /// it (bounded) before `git worktree add`, so the new worktree still forks
    /// from a fresh `origin/<default>` without the fetch ever blocking the UI.
    /// Dropped (not waited on) when the flow is cancelled; a stale receiver is
    /// simply overwritten by the next flow.
    pub(crate) fetch_done: Option<mpsc::Receiver<()>>,
    /// The repo palette parked when the wizard advances past it, so Esc from a
    /// later step restores selections/flags/input as they were instead of
    /// rebuilding (the recency order may be stale until the next fresh open —
    /// accepted). Cleared when the flow completes or is cancelled.
    pub(crate) saved_repo_picker: Option<Box<super::modals::RepoPickerModal>>,
    /// Import flow: the conversation picker parked at its directory step, so
    /// Esc on the name modal returns there. Cleared like `saved_repo_picker`.
    pub(crate) saved_conversation_picker: Option<Box<super::cc_import::ConversationPickerModal>>,
}
