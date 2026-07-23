//! Worktree-to-main git sync state (Ctrl+S).
//!
//! Grouped out of the [`App`](super::App) god object. Fields are `pub(crate)`
//! so call-sites keep direct access (`self.worktree_sync.in_progress`). Kept
//! distinct from the inter-instance [`SyncState`](crate::sync::SyncState),
//! which polls for DB changes from other friring instances.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;

use super::background;
use crate::git;
use crate::session::{HostDef, SessionId};

/// A Ctrl+S run parked between the keypress and the actual sync threads: the
/// remote listing runs off-thread first (no git on the UI thread, the ADR-P12
/// discipline), and any repo with more than one remote routes through the
/// [`SyncBasePickerModal`](super::modals::SyncBasePickerModal) before the run
/// launches.
pub(crate) struct PendingSyncRun {
    /// `(session, worktree, repo)` triples captured at the keypress, so a
    /// session switch while the picker is open can't retarget the run.
    pub(crate) worktrees: Vec<(SessionId, PathBuf, PathBuf)>,
    /// Repos (with their remotes) still awaiting a base choice; front drives
    /// the open picker.
    pub(crate) queue: Vec<(PathBuf, Vec<String>)>,
    /// Chosen/derived base remote per repo. Absent = the default origin chain.
    pub(crate) chosen: HashMap<PathBuf, String>,
    /// The host owning these worktrees (`None` = local), captured with them:
    /// resolving it at launch time instead would let a session switch while
    /// the picker is open point the run at the wrong machine.
    pub(crate) host: Option<HostDef>,
}

/// State for the worktree-to-main git sync (`Ctrl+S`): a background thread
/// rebases each session's worktree onto its base ref and reports results over
/// a channel polled in `tick`.
#[derive(Default)]
pub(crate) struct WorktreeSyncState {
    pub(crate) in_progress: bool,
    /// Receives results from the background sync thread.
    pub(crate) rx: Option<mpsc::Receiver<(SessionId, git::SyncResult)>>,
    /// Number of sessions the in-flight run is syncing.
    pub(crate) pending: usize,
    pub(crate) completed: Vec<(SessionId, git::SyncResult)>,
    /// Background `git remote` listing for the repos of a just-requested run,
    /// polled each tick ([`App::poll_sync_remotes`](super::App)).
    pub(crate) remotes_load: background::BackgroundTask<Vec<(PathBuf, Vec<String>)>>,
    /// The run those remotes are for; consumed when the run launches or the
    /// base picker is cancelled.
    pub(crate) awaiting: Option<PendingSyncRun>,
}
