//! Global search — a centered popup (`Ctrl+/` or double-`Shift`, JetBrains
//! Search-Everywhere-style).
//!
//! It opens in the **Sessions** scope, where it is the session switcher: no
//! query lists every session most-recently-used first (so `Enter` alone is
//! "back to the last one"), and typing ranks the whole fleet by relevance
//! rather than truncating it. `Tab` widens to **Everything** — the original
//! all-scopes search over session metadata + live buffer content, automation
//! names, task titles, and the file tree of the session active at open.
//!
//! The state lives here; building results and dispatching a selection live on
//! `App` (they touch `self.sessions`/vt100/caches). The renderer is
//! [`crate::ui::global_search`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::background::{BackgroundTask, TaskPoll};
use super::modals::TextInput;
use super::{clock, App, InputFocus};
use crate::session::SessionStatus;
use crossterm::event::{KeyCode, KeyModifiers};

/// Max results kept per group (sessions/tasks/automations/files) in the
/// Everything scope, so a broad query can't flood the popup. The Sessions
/// scope is deliberately **uncapped**: a switcher that silently hides the
/// session you are looking for is worse than one that makes you scroll.
pub(crate) const MAX_PER_GROUP: usize = 8;

/// Score added to a session result whose agent is waiting on the user, so the
/// sessions that need answering surface first among equally good name matches.
const BLOCKED_BOOST: i32 = 12;
/// Score added to a session result that just finished (unseen `Done`).
const DONE_BOOST: i32 = 6;

/// Per-field handicaps: a name hit always outranks the same-quality hit on the
/// agent, a branch, a repo or the working directory.
const AGENT_HANDICAP: i32 = 12;
const BRANCH_HANDICAP: i32 = 8;
const REPO_HANDICAP: i32 = 8;
const CWD_HANDICAP: i32 = 16;

/// Which scope the popup is searching. Toggled with `Tab`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SearchScope {
    /// Sessions only — the switcher. The scope the popup opens in, because
    /// switching sessions is the common errand and everything else is a
    /// once-in-a-while one.
    #[default]
    Sessions,
    /// Every scope at once: sessions (metadata + buffer content), tasks,
    /// automations, files.
    Everything,
}

impl SearchScope {
    /// The other scope — `Tab` flips between exactly two.
    fn toggled(self) -> Self {
        match self {
            SearchScope::Sessions => SearchScope::Everything,
            SearchScope::Everything => SearchScope::Sessions,
        }
    }

    /// Label for the popup's title chip.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SearchScope::Sessions => "Sessions",
            SearchScope::Everything => "Everything",
        }
    }
}

/// How many trailing lines of a session's buffer the content scan inspects.
pub(crate) const CONTENT_LINE_CAP: usize = 500;

/// Debounce before the expensive session-content scan runs after a keystroke.
pub(crate) const CONTENT_DEBOUNCE_MS: u64 = 150;

/// What a result jumps to when activated with `Enter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchTarget {
    /// Switch to this session (index into `App::sessions`) and focus its terminal.
    Session { index: usize },
    /// Focus the tasks panel and select this task.
    Task { id: i64 },
    /// Focus the automations pane and select this automation.
    Automation { id: i64 },
    /// Open the file viewer on this path.
    File { root: PathBuf, path: PathBuf },
}

/// The scope a result belongs to (drives grouping + the group header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchKind {
    Session,
    Task,
    Automation,
    File,
}

/// A single match shown in the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlobalSearchResult {
    pub kind: SearchKind,
    /// Primary display text (session name, task title, file label, …).
    pub label: String,
    /// Matching line for content matches, shown dimmed beneath the label.
    pub snippet: Option<String>,
    /// Right-aligned context for a session row (agent · repo), so two
    /// same-named sessions in different repos are told apart without opening
    /// them. `None` for the other scopes.
    pub detail: Option<String>,
    /// Session status, drawn as the row's leading dot — the same glyph the
    /// sidebar uses, so a blocked session is recognisable in the switcher.
    pub status: Option<SessionStatus>,
    pub target: SearchTarget,
}

/// One entry of the Files-scope index: a snapshot of the active session's
/// tree captured when the popup opened, so per-keystroke matching never
/// touches the filesystem (the bounded walk used to run on **every**
/// keystroke — the dominant cost of the old strip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileIndexEntry {
    pub root: PathBuf,
    pub path: PathBuf,
    pub name: String,
    /// Lowercased `name`, precomputed off-thread so matching allocates nothing.
    pub name_lc: String,
}

/// Ranking nudge for a session's status: the sessions asking for the user's
/// attention come first among comparable name matches, because those are the
/// ones being reached for. Everything else — including a ghost, which the
/// switcher lists like any other session — scores on its name alone.
fn status_boost(status: SessionStatus) -> i32 {
    match status {
        SessionStatus::Blocked => BLOCKED_BOOST,
        SessionStatus::Done => DONE_BOOST,
        _ => 0,
    }
}

/// The dim right-hand context on a session row: agent, then the repos it spans
/// (or its branch when it has exactly one worktree). Enough to tell two
/// same-named sessions apart without opening either.
fn session_detail(info: &crate::session::SessionInfo) -> String {
    let mut parts: Vec<String> = vec![info.agent.clone()];
    match info.worktrees.as_slice() {
        [only] => parts.push(only.branch.clone()),
        _ if !info.repo_display_names.is_empty() => {
            parts.push(info.repo_display_names.join(" + "));
        }
        _ => {}
    }
    parts.join(" · ")
}

/// Snapshot of the UI state taken when the popup opens, so cancelling (`Esc`)
/// restores exactly what the user had before searching — including selections,
/// focus, and which optional panels were visible. Live result previews mutate
/// these same fields, so without the snapshot a cancel would leave the cursor
/// wherever the last preview moved it.
#[derive(Clone)]
pub(crate) struct SearchSnapshot {
    pub focus: InputFocus,
    pub active_index: usize,
    pub task_panel_index: usize,
    pub automation_panel_index: usize,
    pub show_tasks_panel: bool,
    pub show_file_viewer: bool,
}

/// State for the global-search popup.
pub(crate) struct GlobalSearchState {
    pub active: bool,
    /// Which scope `Tab` last selected. Reset to the default on every open, so
    /// the popup is always the switcher when it appears.
    pub scope: SearchScope,
    pub query: TextInput,
    pub results: Vec<GlobalSearchResult>,
    /// Selected flat index into `results`.
    pub selected: usize,
    /// UI state captured at open time (incl. the focus to restore), applied on
    /// cancel.
    pub snapshot: Option<SearchSnapshot>,
    /// When the query last changed — anchors the content-scan debounce.
    pub query_changed_at: Option<Instant>,
    /// A content scan is pending (set on edit, cleared once it runs).
    pub content_dirty: bool,
    /// Files-scope index, rebuilt per open (empty until the walk delivers).
    pub file_index: Vec<FileIndexEntry>,
    /// The in-flight off-thread build of [`Self::file_index`].
    pub file_index_task: BackgroundTask<Vec<FileIndexEntry>>,
}

impl Default for GlobalSearchState {
    fn default() -> Self {
        Self {
            active: false,
            scope: SearchScope::default(),
            query: TextInput::new(),
            results: Vec::new(),
            selected: 0,
            snapshot: None,
            query_changed_at: None,
            content_dirty: false,
            file_index: Vec::new(),
            file_index_task: BackgroundTask::default(),
        }
    }
}

impl GlobalSearchState {
    /// Clamp `selected` into the current result range.
    pub(crate) fn clamp_selection(&mut self) {
        if self.results.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.results.len() {
            self.selected = self.results.len() - 1;
        }
    }
}

impl App {
    // ---- Global search (Ctrl+/ or double-Shift centered popup) ------------

    /// Open the global-search popup: snapshot the current UI state (so cancel
    /// can restore it), clear the query, focus the popup, seed the (cheap)
    /// metadata results, and kick off the off-thread Files-index build.
    ///
    /// Unlike the old bottom strip, the popup floats over the content and does
    /// not change any panel's size, so opening it pushes no PTY resize — the
    /// content area is identical to the previous frame. (`close` still resizes:
    /// restoring `show_tasks_panel`/`show_file_viewer` a preview may have
    /// changed does alter the layout.)
    pub(crate) fn open_global_search(&mut self) {
        self.global_search.snapshot = Some(SearchSnapshot {
            focus: self.focus,
            active_index: self.active_index,
            task_panel_index: self.task_ui.task_panel_index,
            automation_panel_index: self.automation_ui.automation_panel_index,
            show_tasks_panel: self.show_tasks_panel,
            show_file_viewer: self.show_file_viewer,
        });
        self.global_search.active = true;
        // Always open as the switcher, whatever `Tab` last selected: the popup
        // is muscle memory for "go to a session", and a sticky Everything scope
        // would make the same keystrokes mean different things on each open.
        self.global_search.scope = SearchScope::default();
        self.global_search.query.clear();
        self.global_search.results.clear();
        self.global_search.selected = 0;
        self.global_search.query_changed_at = None;
        self.global_search.content_dirty = false;
        self.focus = InputFocus::GlobalSearch;
        self.start_global_search_file_index();
        self.recompute_global_search_metadata();
    }

    /// Snapshot the Files-scope index off-thread: the bounded tree walk (up to
    /// thousands of `read_dir` calls — seconds on a network mount) must never
    /// run on the UI thread, let alone per keystroke like the old strip did.
    /// The scope is pinned to the session that is active **at open time**;
    /// live-previewing a session result mid-search doesn't retarget it.
    fn start_global_search_file_index(&mut self) {
        // Drop any receiver a prior open installed first, so even the early
        // returns below (feature off / no active session / no roots) can't leave
        // a stale walk's delivery to be folded into this open's pinned scope.
        self.global_search.file_index_task.cancel();
        self.global_search.file_index.clear();
        if !self.features.file_viewer {
            return;
        }
        let Some(info) = self.sessions.get(self.active_index).map(|s| &s.info) else {
            return;
        };
        let roots = crate::ui::file_viewer::search_roots(info);
        if roots.is_empty() {
            return;
        }
        // Re-opening while a previous walk is still in flight replaces the
        // receiver: the stale walk's send fails harmlessly and only the fresh
        // session's index can ever be delivered.
        let tx = self.global_search.file_index_task.start();
        std::thread::spawn(move || {
            let entries: Vec<FileIndexEntry> =
                crate::ui::file_viewer::enumerate_paths_under(&roots)
                    .into_iter()
                    .map(|(root, path, name)| {
                        let name_lc = name.to_lowercase();
                        FileIndexEntry {
                            root,
                            path,
                            name,
                            name_lc,
                        }
                    })
                    .collect();
            let _ = tx.send(entries);
        });
    }

    /// Poll the off-thread Files-index build (from `tick_core`). On delivery,
    /// store the index and — if the popup is still open with a live query —
    /// fold file matches into the visible results. A died walker just means no
    /// file results this open.
    pub(super) fn poll_global_search_file_index(&mut self) {
        match self.global_search.file_index_task.poll() {
            TaskPoll::Done(index) => {
                self.global_search.file_index = index;
                if self.global_search.active && !self.global_search.query.value().trim().is_empty()
                {
                    if self.global_search.content_dirty {
                        // A content scan is already queued behind the debounce —
                        // rebuild cheaply now and let it fold content matches in.
                        self.recompute_global_search_metadata();
                    } else {
                        // Rebuild on the content path so already-shown buffer
                        // matches aren't dropped by a metadata-only pass.
                        self.recompute_global_search_content();
                    }
                    // Paint the newly-folded file matches now instead of waiting
                    // for the 250 ms forced-redraw floor. Gated on `active` so a
                    // delivery to a closed popup doesn't force a needless repaint.
                    self.request_redraw();
                }
            }
            TaskPoll::Pending | TaskPoll::Died => {}
        }
    }

    /// Cancel the popup: restore the exact UI state captured at open time
    /// (selections, focus, and panel visibility the live preview may have
    /// changed). Bound to `Esc`.
    pub(crate) fn close_global_search(&mut self) {
        if let Some(snap) = self.global_search.snapshot.take() {
            self.active_index = snap.active_index.min(self.sessions.len().saturating_sub(1));
            self.task_ui.task_panel_index = snap.task_panel_index;
            self.automation_ui.automation_panel_index = snap.automation_panel_index;
            self.show_tasks_panel = snap.show_tasks_panel;
            self.show_file_viewer = snap.show_file_viewer;
            self.focus = snap.focus;
        }
        self.global_search.active = false;
        self.global_search.results.clear();
        self.global_search.query.clear();
        // The snapshot predates any feature flag flipped while the popup was
        // open (settings live-reload): re-enforce so the restore can't
        // resurrect a panel/focus whose feature was just disabled. Runs after
        // `active = false`, so its own close-search branch is a no-op.
        self.enforce_feature_visibility();
        self.resize_sessions_to_content_area();
    }

    /// Note that the query changed: recompute the cheap metadata results now,
    /// live-preview the new top result, and flag the expensive content scan to
    /// run after the debounce settles.
    pub(crate) fn on_global_search_query_changed(&mut self) {
        self.recompute_global_search_metadata();
        self.preview_global_search_result();
        self.global_search.content_dirty = true;
        self.global_search.query_changed_at = Some(clock::now());
    }

    /// Rebuild the metadata results (sessions/tasks/automations/files) — fast
    /// enough to run on every keystroke. Session buffer **content** matches are
    /// added separately by [`Self::recompute_global_search_content`].
    pub(crate) fn recompute_global_search_metadata(&mut self) {
        let query = self.global_search.query.value().to_string();
        let results = self.build_global_search_results(&query, false);
        self.global_search.results = results;
        self.global_search.clamp_selection();
    }

    /// Rebuild results including the debounced per-session buffer content scan.
    pub(crate) fn recompute_global_search_content(&mut self) {
        let query = self.global_search.query.value().to_string();
        let results = self.build_global_search_results(&query, true);
        self.global_search.results = results;
        self.global_search.clamp_selection();
        // The result set may have grown (content matches) — keep the preview in
        // sync with whatever is now selected.
        self.preview_global_search_result();
    }

    /// Assemble the result list. In the **Sessions** scope that is the ranked
    /// session list alone (and, with no query, the most-recently-used order).
    /// In **Everything** it is the grouped all-scopes list, which still needs a
    /// query to mean anything.
    fn build_global_search_results(
        &self,
        query: &str,
        with_content: bool,
    ) -> Vec<GlobalSearchResult> {
        let scope = self.global_search.scope;
        if query.trim().is_empty() {
            return match scope {
                SearchScope::Sessions => self.recent_sessions(),
                SearchScope::Everything => Vec::new(),
            };
        }
        let query_lc = query.to_lowercase();
        // The switcher shows every match; Everything caps each group so one
        // broad query can't push the other scopes off the popup.
        let cap = match scope {
            SearchScope::Sessions => usize::MAX,
            SearchScope::Everything => MAX_PER_GROUP,
        };
        let mut out = self.search_sessions(query, &query_lc, with_content, cap);
        if scope == SearchScope::Sessions {
            return out;
        }
        // Group order: Sessions → Tasks → Automations → Files. Disabled
        // features contribute no results, so a selection can never preview or
        // jump into a pane the feature flags hide.
        if self.features.tasks {
            out.extend(self.search_tasks(query, &query_lc));
        }
        if self.features.automations {
            out.extend(self.search_automations(query));
        }
        if self.features.file_viewer {
            out.extend(self.search_files(&query_lc));
        }
        out
    }

    /// The no-query switcher list: every session, most-recently-used first,
    /// with the one already on screen dropped. That makes row 1 the session you
    /// were just in, so opening the popup and pressing `Enter` is the same
    /// "bounce back" gesture as [`Action::LastSession`](crate::session::Action::LastSession)
    /// — and arrowing down walks further back through the same history.
    fn recent_sessions(&self) -> Vec<GlobalSearchResult> {
        self.mru_order_indices()
            .into_iter()
            .filter(|&i| i != self.active_index)
            .map(|i| self.session_result(i, None))
            .collect()
    }

    /// Build a session row for the popup.
    fn session_result(&self, index: usize, snippet: Option<String>) -> GlobalSearchResult {
        let info = &self.sessions[index].info;
        GlobalSearchResult {
            kind: SearchKind::Session,
            label: info.name.clone(),
            snippet,
            detail: Some(session_detail(info)),
            status: Some(info.status),
            target: SearchTarget::Session { index },
        }
    }

    /// Session results, ranked. Every metadata field is scored with the same
    /// fuzzy matcher and the best field wins, minus a per-field handicap so a
    /// name hit always beats an equally good agent/branch/repo/cwd hit. Blocked
    /// and just-finished sessions get a nudge (they are the ones you are most
    /// likely reaching for), and recency breaks the remaining ties.
    ///
    /// `with_content` appends the debounced buffer-content scan, which stays
    /// *below* every metadata hit: a name match is a deliberate target, a
    /// scrollback match is a lucky one.
    fn search_sessions(
        &self,
        query: &str,
        query_lc: &str,
        with_content: bool,
        cap: usize,
    ) -> Vec<GlobalSearchResult> {
        let mut scored: Vec<(i32, usize, usize)> = Vec::new(); // (score, mru rank, index)
        for (i, session) in self.sessions.iter().enumerate() {
            if let Some(score) = self.session_score(query, &session.info) {
                scored.push((score, self.mru_rank(i), i));
            }
        }
        // Best score first; then most recently used; then a stable index so the
        // order never flickers between frames.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        let mut sessions: Vec<GlobalSearchResult> = scored
            .into_iter()
            .take(cap)
            .map(|(_, _, i)| self.session_result(i, None))
            .collect();
        if with_content {
            self.push_session_content_matches(query_lc, &mut sessions, cap);
        }
        sessions
    }

    /// The best score any of a session's metadata fields gives `query`, or
    /// `None` when none of them matches.
    fn session_score(&self, query: &str, info: &crate::session::SessionInfo) -> Option<i32> {
        let field = |text: &str, handicap: i32| {
            crate::fuzzy::fuzzy_match(query, text).map(|m| m.score - handicap)
        };
        let best = [
            field(&info.name, 0),
            field(&info.agent, AGENT_HANDICAP),
            info.worktrees
                .iter()
                .filter_map(|w| field(&w.branch, BRANCH_HANDICAP))
                .max(),
            info.repo_display_names
                .iter()
                .filter_map(|r| field(r, REPO_HANDICAP))
                .max(),
            info.cwd
                .as_ref()
                .and_then(|c| field(&c.to_string_lossy(), CWD_HANDICAP)),
        ]
        .into_iter()
        .flatten()
        .max()?;
        Some(best + status_boost(info.status))
    }

    /// Append vt100 buffer-content matches to `out`, skipping sessions already
    /// present (matched on metadata) and respecting the scope's cap.
    fn push_session_content_matches(
        &self,
        query_lc: &str,
        out: &mut Vec<GlobalSearchResult>,
        cap: usize,
    ) {
        let already: std::collections::HashSet<usize> = out
            .iter()
            .filter_map(|r| match r.target {
                SearchTarget::Session { index } => Some(index),
                _ => None,
            })
            .collect();
        for i in 0..self.sessions.len() {
            if out.len() >= cap {
                break;
            }
            if already.contains(&i) {
                continue;
            }
            if let Some(snippet) = self.session_content_match(query_lc, i) {
                out.push(self.session_result(i, Some(snippet)));
            }
        }
    }

    /// Task results: fuzzy title, falling back to a fuzzy description match with
    /// a context snippet.
    fn search_tasks(&self, query: &str, query_lc: &str) -> Vec<GlobalSearchResult> {
        let mut tasks: Vec<GlobalSearchResult> = Vec::new();
        for task in &self.task_ui.cached_tasks {
            if tasks.len() >= MAX_PER_GROUP {
                break;
            }
            let title_hit = crate::fuzzy::fuzzy_match(query, &task.title).is_some();
            // Title missed — match the description with the same fuzzy matcher
            // used for titles (so gapped queries hit too).
            let desc_hit = !title_hit
                && task
                    .description
                    .as_deref()
                    .is_some_and(|d| crate::fuzzy::fuzzy_match(query, d).is_some());
            if !title_hit && !desc_hit {
                continue;
            }
            // Snippet (description hits only): prefer a line containing the query
            // verbatim, else the first non-empty line, for useful context.
            let snippet = desc_hit.then(|| {
                let desc = task.description.as_deref().unwrap_or("");
                desc.lines()
                    .find(|l| l.to_lowercase().contains(query_lc))
                    .or_else(|| desc.lines().find(|l| !l.trim().is_empty()))
                    .map(|l| l.trim().chars().take(120).collect::<String>())
                    .unwrap_or_default()
            });
            tasks.push(GlobalSearchResult {
                kind: SearchKind::Task,
                label: task.title.clone(),
                snippet,
                detail: None,
                status: None,
                target: SearchTarget::Task { id: task.id },
            });
        }
        tasks
    }

    /// Automation results: fuzzy name.
    fn search_automations(&self, query: &str) -> Vec<GlobalSearchResult> {
        let mut automations: Vec<GlobalSearchResult> = Vec::new();
        for auto in &self.automation_ui.cached_automations {
            if automations.len() >= MAX_PER_GROUP {
                break;
            }
            if crate::fuzzy::fuzzy_match(query, &auto.name).is_some() {
                automations.push(GlobalSearchResult {
                    kind: SearchKind::Automation,
                    label: auto.name.clone(),
                    snippet: None,
                    detail: None,
                    status: None,
                    target: SearchTarget::Automation { id: auto.id },
                });
            }
        }
        automations
    }

    /// File results: case-insensitive substring over the [`FileIndexEntry`]
    /// snapshot captured at open — pure in-memory matching, no filesystem I/O
    /// (empty until the off-thread walk delivers).
    fn search_files(&self, query_lc: &str) -> Vec<GlobalSearchResult> {
        let mut files: Vec<GlobalSearchResult> = Vec::new();
        for entry in &self.global_search.file_index {
            if files.len() >= MAX_PER_GROUP {
                break;
            }
            if entry.name_lc.contains(query_lc) {
                files.push(GlobalSearchResult {
                    kind: SearchKind::File,
                    label: entry.name.clone(),
                    snippet: None,
                    detail: None,
                    status: None,
                    target: SearchTarget::File {
                        root: entry.root.clone(),
                        path: entry.path.clone(),
                    },
                });
            }
        }
        files
    }

    /// Search a session's visible buffer for `query_lc`, returning the first
    /// matching (trimmed) line as a snippet. Scans only the last
    /// [`CONTENT_LINE_CAP`] lines and tolerates a poisoned lock.
    pub(super) fn session_content_match(&self, query_lc: &str, idx: usize) -> Option<String> {
        if query_lc.trim().is_empty() {
            return None;
        }
        let session = self.sessions.get(idx)?;
        let parser = session.parser.lock().ok()?;
        let contents = parser.screen().contents();
        drop(parser);
        let lines: Vec<&str> = contents.lines().collect();
        let start = lines.len().saturating_sub(CONTENT_LINE_CAP);
        for line in &lines[start..] {
            if line.to_lowercase().contains(query_lc) {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.chars().take(120).collect());
                }
            }
        }
        None
    }

    /// The active global-search query for live in-panel highlighting: `Some`
    /// when the popup is open with a non-empty query, else `None` (panels render
    /// normally). Used by the view to highlight matched rows and dim the rest.
    pub(crate) fn global_search_query(&self) -> Option<&str> {
        if !self.global_search.active {
            return None;
        }
        let q = self.global_search.query.value();
        (!q.trim().is_empty()).then_some(q)
    }

    /// The scope of the currently selected global-search result, while the popup
    /// is active. Lets the view force-show the selected (previewed) row in the
    /// owning panel even though focus stays in the search box.
    pub(crate) fn global_search_preview_kind(&self) -> Option<SearchKind> {
        if !self.global_search.active {
            return None;
        }
        self.global_search
            .results
            .get(self.global_search.selected)
            .map(|r| r.kind)
    }

    /// Live-preview the selected result without leaving the search box: move the
    /// matching panel's cursor (active session / task row / automation row) so
    /// the user sees where `Enter` would land. Files are *not* previewed (opening
    /// the file viewer per keystroke is heavy) — they only act on `Enter`.
    /// Cancelling restores all of this from the snapshot.
    pub(crate) fn preview_global_search_result(&mut self) {
        let Some(result) = self
            .global_search
            .results
            .get(self.global_search.selected)
            .cloned()
        else {
            return;
        };
        match result.target {
            SearchTarget::Session { index } => {
                if index < self.sessions.len() {
                    self.active_index = index;
                }
            }
            SearchTarget::Task { id } => {
                self.show_tasks_panel = true;
                // Preview from the in-memory cache the results were built from —
                // no SQLite read per keystroke/arrow; `Enter` still re-reads the
                // DB (see `activate_global_search_result`).
                self.recompute_task_filter();
                if let Some(pos) = self
                    .task_ui
                    .filtered_task_indices
                    .iter()
                    .position(|&i| self.task_ui.cached_tasks.get(i).map(|t| t.id) == Some(id))
                {
                    self.task_ui.task_panel_index = pos;
                }
            }
            SearchTarget::Automation { id } => {
                if let Some(pos) = self
                    .automation_ui
                    .cached_automations
                    .iter()
                    .position(|a| a.id == id)
                {
                    self.automation_ui.automation_panel_index = pos;
                }
            }
            // Files aren't previewed live — they only open on `Enter`.
            SearchTarget::File { .. } => {}
        }
    }

    /// Jump to the selected search result's target, then close the popup.
    pub(crate) fn activate_global_search_result(&mut self) {
        let Some(result) = self
            .global_search
            .results
            .get(self.global_search.selected)
            .cloned()
        else {
            self.close_global_search();
            return;
        };
        // Commit: discard the snapshot (we keep the jump, don't restore) and
        // tear the popup down, then apply the jump target. Capture the pre-search
        // focus first, as the fallback when a stale target can't be opened.
        let fallback_focus = self
            .global_search
            .snapshot
            .as_ref()
            .map(|s| s.focus)
            .unwrap_or_else(|| self.focus_fallback());
        // For the LastSession toggle the meaningful "previous" is the session
        // active before the search *opened* — live previews already moved
        // `active_index` while browsing results, so recording via
        // `set_active_index` here would remember an arbitrary preview.
        let prior_index = self
            .global_search
            .snapshot
            .as_ref()
            .map(|s| s.active_index.min(self.sessions.len().saturating_sub(1)))
            .unwrap_or(self.active_index);
        let prior_id = self.sessions.get(prior_index).map(|s| s.info.id);
        // The Files scope is pinned to the session active at open (=
        // snapshot.active_index), but live preview may have retargeted
        // `active_index` to a previewed session result. Capture the pinned index
        // so the File branch rebuilds the correct session's viewer.
        let pinned_active = self.global_search.snapshot.as_ref().map(|s| s.active_index);
        self.global_search.active = false;
        self.global_search.results.clear();
        self.global_search.query.clear();
        self.global_search.snapshot = None;
        match result.target {
            SearchTarget::Session { index } => {
                if index < self.sessions.len() {
                    if index != prior_index {
                        self.last_active_session = prior_id;
                    }
                    self.active_index = index;
                    self.note_session_use();
                    self.focus = InputFocus::Terminal;
                    // Same contract as `Enter` on a ghost row in the session
                    // list: selection never starts an agent, but choosing one
                    // does. Without this the switcher could reach a ghost and
                    // then strand the user on a frozen frame, which is exactly
                    // the dead end that made the sidebar the only way to load
                    // an unloaded session.
                    if self.active_session_is_ghost() {
                        self.restart_active_session();
                    }
                } else {
                    self.focus = fallback_focus;
                }
            }
            SearchTarget::Task { id } => {
                self.show_tasks_panel = true;
                self.refresh_tasks();
                if let Some(pos) = self
                    .task_ui
                    .filtered_task_indices
                    .iter()
                    .position(|&i| self.task_ui.cached_tasks.get(i).map(|t| t.id) == Some(id))
                {
                    self.task_ui.task_panel_index = pos;
                }
                self.focus = InputFocus::TaskList;
            }
            SearchTarget::Automation { id } => {
                if let Some(pos) = self
                    .automation_ui
                    .cached_automations
                    .iter()
                    .position(|a| a.id == id)
                {
                    self.automation_ui.automation_panel_index = pos;
                }
                // The pane lives in the left column, so a jump into it has to
                // bring that column back — the one route into the automations
                // context that survives a collapse (every other one starts
                // from a rendered row).
                self.show_session_list = true;
                self.focus = InputFocus::Automations;
                self.refresh_automation_view();
            }
            SearchTarget::File { root: _, path } => {
                self.show_file_viewer = true;
                // Reveal against the pinned session's viewer, not whatever
                // session live preview last selected.
                if let Some(idx) = pinned_active {
                    if idx < self.sessions.len() {
                        self.active_index = idx;
                    }
                }
                self.rebuild_file_viewer_for_active();
                self.file_viewer.reveal_path(&path);
                self.focus = InputFocus::FileViewer;
            }
        }
        self.resize_sessions_to_content_area();
    }

    /// Run the debounced global-search content scan once the query has been
    /// settled for the debounce window (Instant-based, since tick cadence is
    /// event-load-dependent).
    pub(super) fn tick_global_search_content(&mut self) {
        if !(self.global_search.active && self.global_search.content_dirty) {
            return;
        }
        let settled = self
            .global_search
            .query_changed_at
            .map(|t| clock::elapsed_since(t) >= Duration::from_millis(CONTENT_DEBOUNCE_MS))
            .unwrap_or(false);
        if settled {
            self.recompute_global_search_content();
            self.global_search.content_dirty = false;
        }
    }

    /// Widen from the Sessions switcher to the all-scopes search, or back
    /// (`Tab`). The query survives the flip, so a search that came up empty in
    /// one scope is one keystroke from being re-run in the other.
    fn toggle_global_search_scope(&mut self) {
        self.global_search.scope = self.global_search.scope.toggled();
        self.global_search.selected = 0;
        self.recompute_global_search_metadata();
        // Everything's session rows come from the same scan, so a query typed
        // in the switcher still needs its content pass once it widens.
        self.global_search.content_dirty = true;
        self.global_search.query_changed_at = Some(clock::now());
    }

    /// Handle keys while the global-search popup is focused. Typed characters
    /// edit the query (so plain `j`/`k` insert, like the other search inputs);
    /// `Up`/`Down` and `Ctrl+P`/`Ctrl+N` move the selection; `Tab` switches
    /// scope; `Enter` activates the selected result; `Esc` closes the popup.
    pub(super) fn handle_global_search_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Esc => self.close_global_search(),
            KeyCode::Enter => self.activate_global_search_result(),
            KeyCode::Tab | KeyCode::BackTab => self.toggle_global_search_scope(),
            KeyCode::Down => self.move_global_search_selection(1),
            KeyCode::Up => self.move_global_search_selection(-1),
            KeyCode::Char('n') if ctrl => self.move_global_search_selection(1),
            KeyCode::Char('p') if ctrl => self.move_global_search_selection(-1),
            KeyCode::Backspace => {
                self.global_search.query.backspace();
                self.on_global_search_query_changed();
            }
            KeyCode::Delete => {
                self.global_search.query.delete();
                self.on_global_search_query_changed();
            }
            KeyCode::Left => self.global_search.query.move_left(),
            KeyCode::Right => self.global_search.query.move_right(),
            KeyCode::Home => self.global_search.query.home(),
            KeyCode::End => self.global_search.query.end(),
            // Plain chars edit the query; ignore other Ctrl-chords.
            KeyCode::Char(c) if !ctrl => {
                self.global_search.query.insert(c);
                self.on_global_search_query_changed();
            }
            _ => {}
        }
    }

    /// Move the global-search selection by `delta`, clamped to the result range,
    /// and live-preview the newly selected result.
    fn move_global_search_selection(&mut self, delta: i32) {
        let len = self.global_search.results.len();
        if len == 0 {
            self.global_search.selected = 0;
            return;
        }
        let next = (self.global_search.selected as i32 + delta).clamp(0, len as i32 - 1);
        self.global_search.selected = next as usize;
        self.preview_global_search_result();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(label: &str) -> GlobalSearchResult {
        GlobalSearchResult {
            kind: SearchKind::Task,
            label: label.to_string(),
            snippet: None,
            detail: None,
            status: None,
            target: SearchTarget::Task { id: 1 },
        }
    }

    fn state_with(results: usize, selected: usize) -> GlobalSearchState {
        GlobalSearchState {
            results: (0..results).map(|i| result(&format!("r{i}"))).collect(),
            selected,
            ..GlobalSearchState::default()
        }
    }

    #[test]
    fn clamp_resets_to_zero_when_empty() {
        let mut s = state_with(0, 5);
        s.clamp_selection();
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn clamp_pins_to_last_when_out_of_range() {
        let mut s = state_with(3, 9);
        s.clamp_selection();
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn clamp_leaves_in_range_selection_untouched() {
        let mut s = state_with(3, 1);
        s.clamp_selection();
        assert_eq!(s.selected, 1);
    }

    // ── the switcher (Sessions scope) ──

    use crate::app::state::tests::app_with_sessions;

    /// Every session result, by name, in the order the popup lists them.
    fn session_names(app: &App) -> Vec<String> {
        app.global_search
            .results
            .iter()
            .filter(|r| r.kind == SearchKind::Session)
            .map(|r| r.label.clone())
            .collect()
    }

    #[test]
    fn switcher_opens_on_the_sessions_scope_with_the_recent_list() {
        let (mut app, _g, _t) = app_with_sessions(4);
        // Visit two sessions, then come back to the first.
        app.set_active_index(2);
        app.set_active_index(3);
        app.set_active_index(0);

        app.open_global_search();

        assert_eq!(app.global_search.scope, SearchScope::Sessions);
        // Most-recently-used first, with the session already on screen dropped
        // — so `Enter` on the untouched popup is "back to the last one".
        assert_eq!(
            session_names(&app),
            vec!["session-3", "session-2", "session-1"],
            "recent-first, active excluded, never-visited last"
        );
    }

    #[test]
    fn switcher_enter_with_no_query_returns_to_the_previous_session() {
        let (mut app, _g, _t) = app_with_sessions(3);
        app.set_active_index(2);
        app.set_active_index(1);

        app.open_global_search();
        app.activate_global_search_result();

        assert_eq!(app.active_index, 2, "row 1 is the session just left");
        assert!(!app.global_search.active);
    }

    #[test]
    fn switcher_ranks_the_best_name_match_first() {
        let (mut app, _g, _t) = app_with_sessions(3);
        app.sessions[0].info.name = "alpha-pipeline".into();
        app.sessions[1].info.name = "api".into();
        app.sessions[2].info.name = "grapikeeper".into();

        app.open_global_search();
        for c in "api".chars() {
            app.global_search.query.insert(c);
        }
        app.on_global_search_query_changed();

        // Exact/prefix beats the tight mid-word hit, which beats the one
        // spelled out of scattered letters.
        assert_eq!(
            session_names(&app),
            vec!["api", "grapikeeper", "alpha-pipeline"]
        );
    }

    #[test]
    fn switcher_lists_every_match_past_the_everything_cap() {
        let (mut app, _g, _t) = app_with_sessions(MAX_PER_GROUP + 5);
        app.open_global_search();
        for c in "session".chars() {
            app.global_search.query.insert(c);
        }
        app.on_global_search_query_changed();

        assert_eq!(
            session_names(&app).len(),
            MAX_PER_GROUP + 5,
            "the switcher must never hide a session behind a cap"
        );
    }

    #[test]
    fn tab_widens_to_everything_and_recaps_sessions() {
        let (mut app, _g, _t) = app_with_sessions(MAX_PER_GROUP + 5);
        app.open_global_search();
        for c in "session".chars() {
            app.global_search.query.insert(c);
        }
        app.on_global_search_query_changed();

        app.handle_global_search_key(KeyCode::Tab, KeyModifiers::NONE);

        assert_eq!(app.global_search.scope, SearchScope::Everything);
        assert_eq!(session_names(&app).len(), MAX_PER_GROUP);
        assert_eq!(
            app.global_search.query.value(),
            "session",
            "the query survives the scope flip"
        );
    }

    #[test]
    fn reopening_resets_the_scope_to_the_switcher() {
        let (mut app, _g, _t) = app_with_sessions(2);
        app.open_global_search();
        app.handle_global_search_key(KeyCode::Tab, KeyModifiers::NONE);
        app.close_global_search();

        app.open_global_search();

        assert_eq!(app.global_search.scope, SearchScope::Sessions);
    }

    // ── label jump (Alt+G / <leader> A) ──

    #[test]
    fn label_jump_reaches_past_the_nine_digit_ceiling() {
        let (mut app, _g, _t) = app_with_sessions(12);
        app.toggle_label_jump();

        // Row 12 has no digit at all; its label is the 12th letter.
        let labels = crate::ui::project_list::session_labels(12);
        let target = labels[11].chars().next().unwrap();
        assert!(app.push_label_jump_char(target));

        assert_eq!(app.active_index, 11);
        assert!(app.label_jump.is_none(), "a hit closes the overlay");
        assert_eq!(app.focus, InputFocus::Terminal);
    }

    #[test]
    fn label_jump_waits_for_the_second_key_of_a_two_key_label() {
        let (mut app, _g, _t) = app_with_sessions(30);
        let labels = crate::ui::project_list::session_labels(30);
        let two_key = labels
            .iter()
            .position(|l| l.chars().count() == 2)
            .expect("30 sessions need two-key labels");
        let mut chars = labels[two_key].chars();
        let (first, second) = (chars.next().unwrap(), chars.next().unwrap());

        app.toggle_label_jump();
        assert!(app.push_label_jump_char(first));
        assert!(app.label_jump.is_some(), "prefix keeps the overlay open");
        assert_eq!(app.active_index, 0, "and switches nothing yet");

        assert!(app.push_label_jump_char(second));
        assert_eq!(app.active_index, two_key);
    }

    #[test]
    fn label_jump_chips_narrow_to_the_typed_prefix() {
        let (mut app, _g, _t) = app_with_sessions(30);
        let labels = crate::ui::project_list::session_labels(30);
        let prefix = labels
            .iter()
            .find(|l| l.chars().count() == 2)
            .and_then(|l| l.chars().next())
            .unwrap();

        app.toggle_label_jump();
        app.push_label_jump_char(prefix);

        let chips = app.label_jump_chips();
        let shown = chips.iter().filter(|c| c.is_some()).count();
        assert!(shown > 0 && shown < 30, "only the reachable rows stay lit");
        assert!(
            chips.iter().flatten().all(|c| c.chars().count() == 1),
            "each remaining row shows just the key left to press"
        );
    }

    #[test]
    fn label_jump_reports_a_key_that_matches_nothing() {
        let (mut app, _g, _t) = app_with_sessions(3);
        app.toggle_label_jump();
        // Three sessions take `a`, `s`, `d`; `m` is the last letter of the
        // alphabet and unreachable here.
        assert!(!app.push_label_jump_char('m'));
        assert!(app.label_jump.is_none(), "a miss ends the mode");
        assert_eq!(app.active_index, 0);
    }

    // ── collapsed repo groups & the ghost shelf ──

    /// Give each session a repo so `compute_session_order` puts them in real
    /// groups (the stub sessions otherwise all land in `(no repo)`).
    fn in_repo(app: &mut App, idx: usize, repo: &str) {
        app.sessions[idx].info.repo_display_names = vec![repo.to_string()];
    }

    #[test]
    fn folding_a_group_hides_its_rows_from_the_list_and_from_navigation() {
        let (mut app, _g, _t) = app_with_sessions(4);
        for i in 0..3 {
            in_repo(&mut app, i, "alpha");
        }
        in_repo(&mut app, 3, "beta");
        app.set_active_index(3); // stand outside the group being folded
        app.set_active_index(0);
        app.set_active_group_folded(true);
        // Folding selects the group's head, so the cursor isn't stranded on a
        // row that is about to be hidden.
        assert_eq!(app.active_index, 0);

        let visible = app.visible_order_indices();
        assert_eq!(visible, vec![0, 3], "only the head and the other group");
        // Ctrl+J steps over the folded rows rather than appearing to stall.
        app.switch_session_forward();
        assert_eq!(app.active_index, 3);
        app.switch_session_forward();
        assert_eq!(app.active_index, 0, "and wraps within what's on screen");
    }

    #[test]
    fn unfolding_brings_the_rows_back_and_the_set_persists() {
        let (mut app, _g, _t) = app_with_sessions(3);
        for i in 0..3 {
            in_repo(&mut app, i, "alpha");
        }
        app.set_active_group_folded(true);
        assert_eq!(app.visible_order_indices().len(), 1);
        assert_eq!(
            app.db.get_folded_session_groups().unwrap(),
            vec!["alpha".to_string()],
            "the arrangement outlives the process"
        );

        app.set_active_group_folded(false);
        assert_eq!(app.visible_order_indices().len(), 3);
        assert!(app.db.get_folded_session_groups().unwrap().is_empty());
    }

    #[test]
    fn folding_never_hides_the_active_session() {
        let (mut app, _g, _t) = app_with_sessions(3);
        for i in 0..3 {
            in_repo(&mut app, i, "alpha");
        }
        app.folded_groups.insert("alpha".to_string());
        // Selection moved onto a folded row by some other route (a search
        // commit, a notification click): its row has to come back.
        app.set_active_index(2);
        assert!(
            app.visible_order_indices().contains(&2),
            "a hidden cursor would make the list lie about where you are"
        );
    }

    #[test]
    fn folding_leaves_display_order_alone() {
        let (mut app, _g, _t) = app_with_sessions(4);
        for i in 0..4 {
            in_repo(&mut app, i, "alpha");
        }
        app.sort_sessions_alphabetically();
        let before: Vec<Option<i64>> = app.sessions.iter().map(|s| s.info.display_order).collect();

        app.set_active_group_folded(true);
        app.sort_sessions_alphabetically();

        let after: Vec<Option<i64>> = app.sessions.iter().map(|s| s.info.display_order).collect();
        assert_eq!(
            before, after,
            "reordering must see the whole list, not just what's on screen"
        );
    }

    #[test]
    fn ghost_shelf_hides_unloaded_sessions_but_keeps_the_active_one() {
        let (mut app, _g, _t) = app_with_sessions(4);
        app.sessions[1].info.status = SessionStatus::Unloaded;
        app.sessions[2].info.status = SessionStatus::Unloaded;

        app.toggle_ghost_shelf();
        assert!(app.ghost_shelf);
        assert_eq!(app.visible_order_indices(), vec![0, 3]);

        // Reaching a ghost from the switcher must not leave it invisible.
        app.set_active_index(2);
        assert!(app.visible_order_indices().contains(&2));

        app.toggle_ghost_shelf();
        assert_eq!(app.visible_order_indices(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn group_leap_walks_group_heads_and_wraps() {
        let (mut app, _g, _t) = app_with_sessions(5);
        in_repo(&mut app, 0, "alpha");
        in_repo(&mut app, 1, "alpha");
        in_repo(&mut app, 2, "beta");
        in_repo(&mut app, 3, "gamma");
        in_repo(&mut app, 4, "gamma");
        app.set_active_index(1); // second row of the first group

        app.jump_to_adjacent_group(true);
        assert_eq!(app.active_index, 2, "lands on the next group's first row");
        app.jump_to_adjacent_group(true);
        assert_eq!(app.active_index, 3);
        app.jump_to_adjacent_group(true);
        assert_eq!(app.active_index, 0, "wraps to the top group");
        app.jump_to_adjacent_group(false);
        assert_eq!(app.active_index, 3, "and back the other way");
    }

    #[test]
    fn blocked_sessions_outrank_equal_name_matches() {
        let (mut app, _g, _t) = app_with_sessions(2);
        app.sessions[0].info.name = "review".into();
        app.sessions[1].info.name = "review".into();
        app.sessions[1].info.status = SessionStatus::Blocked;

        app.open_global_search();
        for c in "review".chars() {
            app.global_search.query.insert(c);
        }
        app.on_global_search_query_changed();

        let blocked_first = matches!(
            app.global_search.results.first().map(|r| r.target.clone()),
            Some(SearchTarget::Session { index: 1 })
        );
        assert!(blocked_first, "the session waiting on the user comes first");
    }
}
