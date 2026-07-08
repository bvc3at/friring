//! Claude Code workflow + subagent activity — the off-thread scan that indexes
//! each local session's `~/.claude/.../subagents/` tree onto
//! [`SessionInfo::cc_activity`].
//!
//! Mirrors the `metrics_refresh` background job (`collect_system_metrics` /
//! `start_metrics_refresh` / `poll_metrics_refresh`): build a cheap input list
//! on the UI thread, hand it to `spawn_blocking`, and apply the result by
//! session id. A per-session directory **signature** (hash of every file's
//! name + mtime + len) gates the expensive parse — an unchanged tree returns
//! `None` and the field is left untouched, so idle sessions cost only a stat
//! walk. Nothing is persisted (it is fully reconstructable from disk).
//!
//! The pure parsers live in [`crate::session::cc_activity`]; this module is the
//! filesystem glue (the app layer is where I/O belongs).

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::session::cc_activity::{
    parse_journal, parse_meta, parse_transcript, parse_workflow_completion, JournalEntry,
    TranscriptBlock, WorkflowCompletion,
};
use crate::session::{CcActivity, CcAgent, CcAgentState, CcRunStatus, CcWorkflow, SessionId};

use super::{background, App, InputFocus};

/// A standalone subagent (or a journal-less workflow agent) is treated as
/// `Active` while its transcript was appended within this window, else `Done`.
/// Workflow agents prefer the authoritative `journal.jsonl` signal.
const ACTIVE_WINDOW_NS: u128 = 10_000_000_000; // 10s

/// Result of a background CC-activity scan, delivered via `App::cc_refresh`.
/// Each entry is `(session, new dir signature, activity)`, where the activity
/// is `Some` only when the signature moved (an unchanged tree yields `None`,
/// so the field is left as-is).
pub(super) struct CcRefresh {
    updates: Vec<(SessionId, u64, Option<CcActivity>)>,
}

impl App {
    /// Kick off a background scan of every local session's `subagents/` tree.
    /// Skips remote sessions (their `~/.claude` lives on the host) and sessions
    /// without an agent conversation id. Non-claude local sessions simply
    /// resolve to no directory (empty activity) — cheap, and it means
    /// claude-based agents under custom names (flow/shepherd workers) are
    /// covered without a name allowlist.
    pub(super) fn start_cc_refresh(&mut self) {
        if self.cc_refresh.in_progress() {
            return;
        }
        let Some(projects) = crate::paths::claude_projects_dir(None) else {
            return;
        };
        let inputs: Vec<(SessionId, String, Option<u64>)> = self
            .sessions
            .iter()
            .filter_map(|s| {
                if s.info.remote_host.is_some() {
                    return None;
                }
                let sid = s.info.agent_session_id.clone()?;
                Some((
                    s.info.id,
                    sid,
                    self.cached_cc_signatures.get(&s.info.id).copied(),
                ))
            })
            .collect();
        if inputs.is_empty() {
            return;
        }
        let tx = self.cc_refresh.start();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(collect_cc_activity(projects, inputs));
        });
    }

    /// Apply a completed CC-activity scan: update the cached signatures and, for
    /// each session whose tree changed, replace its `cc_activity` index. When
    /// the changed session's transcript view is open, rebuild + repaint it (the
    /// live-tail path); see [`Self::on_cc_activity_updated`].
    pub(super) fn poll_cc_refresh(&mut self) {
        let background::TaskPoll::Done(refresh) = self.cc_refresh.poll() else {
            return;
        };
        for (id, sig, activity) in refresh.updates {
            self.cached_cc_signatures.insert(id, sig);
            if let Some(a) = activity {
                let changed = self
                    .sessions
                    .iter_mut()
                    .find(|s| s.info.id == id)
                    .map(|s| {
                        let differs = s.info.cc_activity.as_ref() != Some(&a);
                        s.info.cc_activity = Some(a);
                        differs
                    })
                    .unwrap_or(false);
                if changed {
                    self.on_cc_activity_updated(id);
                }
            }
        }
    }
}

/// Off-thread: scan each session's `subagents/` tree into a [`CcActivity`]
/// index, skipping the parse when the directory signature is unchanged.
fn collect_cc_activity(
    projects: PathBuf,
    inputs: Vec<(SessionId, String, Option<u64>)>,
) -> CcRefresh {
    // List the project slug dirs once; each session's tree is one of their
    // `<agent_session_id>/subagents` children. Scanning sidesteps Claude Code's
    // slug rule (which replaces `/` *and* `.` — and likely all non-alnum — with
    // `-`, so a computed slug is wrong for any dotted path).
    let project_dirs: Vec<PathBuf> = std::fs::read_dir(&projects)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();

    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let mut updates = Vec::with_capacity(inputs.len());
    for (id, sid, prior) in inputs {
        match resolve_subagents_dir(&project_dirs, &sid) {
            Some(dir) => {
                let sig = dir_signature(&dir);
                if Some(sig) == prior {
                    updates.push((id, sig, None));
                } else {
                    updates.push((id, sig, Some(build_activity(&dir, now_ns))));
                }
            }
            // No subagents tree (yet, or a non-claude session): clear any stale
            // activity, otherwise leave the field untouched.
            None => match prior {
                Some(p) if p != 0 => updates.push((id, 0, Some(CcActivity::default()))),
                _ => updates.push((id, 0, None)),
            },
        }
    }
    CcRefresh { updates }
}

/// Find `<project>/<agent_session_id>/subagents` across the project slug dirs.
fn resolve_subagents_dir(project_dirs: &[PathBuf], sid: &str) -> Option<PathBuf> {
    for p in project_dirs {
        let d = p.join(sid).join("subagents");
        if d.is_dir() {
            return Some(d);
        }
    }
    None
}

/// Hash of the whole `subagents/` subtree (each file's path + mtime + len). A
/// live JSONL append bumps mtime → the hash moves → the tree is re-parsed; an
/// idle tree hashes identically and skips the parse.
fn dir_signature(root: &Path) -> u64 {
    let mut items: Vec<(String, u128, u64)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else {
                continue;
            };
            if md.is_dir() {
                stack.push(e.path());
            } else {
                items.push((
                    e.path().to_string_lossy().into_owned(),
                    file_mtime_ns(&md),
                    md.len(),
                ));
            }
        }
    }
    items.sort();
    let mut hasher = DefaultHasher::new();
    items.hash(&mut hasher);
    hasher.finish()
}

/// Build the activity index from a `subagents/` directory: standalone `Task`
/// subagents (top-level `agent-*.jsonl`) plus each `workflows/wf_*/` run.
fn build_activity(subagents: &Path, now_ns: u128) -> CcActivity {
    let mut act = CcActivity::default();

    if let Ok(entries) = std::fs::read_dir(subagents) {
        for e in entries.flatten() {
            let path = e.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(agent_id_from)
                .map(str::to_string)
            else {
                continue;
            };
            let (mtime_ns, size) = stat_file(&path);
            let meta = read_agent_meta(subagents, &id);
            // Standalone subagents have no journal — freshness is the only live
            // signal available.
            let state = if now_ns.saturating_sub(mtime_ns) < ACTIVE_WINDOW_NS {
                CcAgentState::Active
            } else {
                CcAgentState::Done
            };
            act.subagents.push(CcAgent {
                agent_id: id,
                transcript_path: path,
                agent_type: meta.agent_type,
                description: meta.description,
                label: None,
                phase_title: None,
                state,
                mtime_ns,
                size,
                tokens: None,
                tool_calls: None,
                last_tool: None,
                model: None,
            });
        }
    }

    let wf_root = subagents.join("workflows");
    if let Ok(entries) = std::fs::read_dir(&wf_root) {
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let Some(run_id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| n.starts_with("wf_"))
                .map(String::from)
            else {
                continue;
            };
            if let Some(wf) = build_workflow(&wf_root, &path, run_id) {
                act.workflows.push(wf);
            }
        }
    }

    // Most-recently-active first, in both lists.
    act.subagents.sort_by_key(|a| std::cmp::Reverse(a.mtime_ns));
    act.workflows
        .sort_by_key(|w| std::cmp::Reverse(workflow_mtime(w)));
    act
}

fn build_workflow(wf_root: &Path, run_dir: &Path, run_id: String) -> Option<CcWorkflow> {
    let journal = std::fs::read_to_string(run_dir.join("journal.jsonl"))
        .map(|s| parse_journal(&s))
        .unwrap_or_default();
    let completion = std::fs::read_to_string(wf_root.join(format!("{run_id}.json")))
        .ok()
        .and_then(|s| parse_workflow_completion(&s));
    let status = if completion.is_some() {
        CcRunStatus::Completed
    } else {
        CcRunStatus::Running
    };

    let mut agents = Vec::new();
    if let Ok(entries) = std::fs::read_dir(run_dir) {
        for e in entries.flatten() {
            let path = e.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(agent_id_from)
                .map(str::to_string)
            else {
                continue;
            };
            let (mtime_ns, size) = stat_file(&path);
            let meta = read_agent_meta(run_dir, &id);
            let progress = completion.as_ref().and_then(|c| c.agents.get(id.as_str()));
            let state = workflow_agent_state(&journal, completion.as_ref(), &id);
            agents.push(CcAgent {
                agent_id: id,
                transcript_path: path,
                agent_type: meta.agent_type,
                description: meta.description,
                label: progress.and_then(|p| p.label.clone()),
                phase_title: progress.and_then(|p| p.phase_title.clone()),
                state,
                mtime_ns,
                size,
                tokens: progress.and_then(|p| p.tokens),
                tool_calls: progress.and_then(|p| p.tool_calls),
                last_tool: progress.and_then(|p| p.last_tool.clone()),
                model: progress.and_then(|p| p.model.clone()),
            });
        }
    }
    if agents.is_empty() && completion.is_none() {
        return None;
    }
    agents.sort_by_key(|a| a.mtime_ns); // spawn order

    let (name, phases, summary) = match completion {
        Some(c) => (c.workflow_name, c.phases, Some(c.summary)),
        None => (None, Vec::new(), None),
    };
    Some(CcWorkflow {
        run_id,
        name,
        dir: run_dir.to_path_buf(),
        status,
        phases,
        agents,
        summary,
    })
}

/// A workflow agent's state: authoritative from the completion grid when the
/// run has finished, else from the `journal.jsonl` `started`/`result` edges.
fn workflow_agent_state(
    journal: &HashMap<String, JournalEntry>,
    completion: Option<&WorkflowCompletion>,
    agent_id: &str,
) -> CcAgentState {
    if let Some(c) = completion {
        return match c.agents.get(agent_id).and_then(|p| p.state.as_deref()) {
            Some("error") => CcAgentState::Error,
            _ => CcAgentState::Done, // a completed run's agents are all terminal
        };
    }
    match journal.get(agent_id) {
        Some(j) if j.has_result => CcAgentState::Done,
        Some(j) if j.started => CcAgentState::Active,
        _ => CcAgentState::Done,
    }
}

/// Newest transcript mtime across a workflow's agents (for tree ordering).
fn workflow_mtime(wf: &CcWorkflow) -> u128 {
    wf.agents.iter().map(|a| a.mtime_ns).max().unwrap_or(0)
}

/// Extract `<id>` from an `agent-<id>.jsonl` filename; `None` for anything else
/// (including the sibling `agent-<id>.meta.json`).
fn agent_id_from(name: &str) -> Option<&str> {
    name.strip_prefix("agent-")
        .and_then(|r| r.strip_suffix(".jsonl"))
}

fn read_agent_meta(dir: &Path, id: &str) -> crate::session::cc_activity::CcMeta {
    let s = std::fs::read_to_string(dir.join(format!("agent-{id}.meta.json"))).unwrap_or_default();
    parse_meta(&s)
}

fn stat_file(path: &Path) -> (u128, u64) {
    match std::fs::metadata(path) {
        Ok(md) => (file_mtime_ns(&md), md.len()),
        Err(_) => (0, 0),
    }
}

fn file_mtime_ns(md: &std::fs::Metadata) -> u128 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// =========================================================================
// View state — the open activity view (side tree + central transcript).
//
// Mirrors the code-review view (app/code_review.rs): per-session state in
// `App::cc_activities` (so it survives session switches), two focuses —
// `CcActivityTree` (the side tree, the nav) and `CcActivity` (the central
// transcript, the content) — captured before the global keybinding lookup.
// =========================================================================

/// Which node the central transcript pane is showing. Referenced by stable ids
/// (not indices) so it survives an off-thread index refresh that reorders the
/// tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CcNodeRef {
    /// A workflow's overview (phases + per-agent grid + logs) — `run_id`.
    WorkflowOverview(String),
    /// One workflow-spawned agent's transcript — `(run_id, agent_id)`.
    WorkflowAgent(String, String),
    /// A standalone Task subagent's transcript — `agent_id`.
    Subagent(String),
}

/// One row of the side tree. Indices point into the state's [`CcActivity`]
/// snapshot; a collapsed workflow hides its agent rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CcTreeRow {
    Workflow(usize),
    WorkflowAgent(usize, usize),
    Subagent(usize),
    Info(String),
}

impl CcTreeRow {
    pub(crate) fn is_selectable(&self) -> bool {
        !matches!(self, CcTreeRow::Info(_))
    }
}

/// One logical row of the central transcript pane. `Block` renders the
/// transcript block at that index; `Text`/`Info` are synthetic lines (workflow
/// overview, hints) — `Info` is non-selectable.
#[derive(Debug, Clone)]
pub(crate) enum CcRow {
    Block(usize),
    Text(String),
    Info(String),
}

impl CcRow {
    pub(crate) fn is_selectable(&self) -> bool {
        !matches!(self, CcRow::Info(_))
    }
}

/// The open activity view for one session (persisted in `App::cc_activities`).
pub(crate) struct CcActivityState {
    /// Snapshot of the session's activity index, refreshed from
    /// `SessionInfo.cc_activity` while the view is open (live-tail).
    pub activity: CcActivity,
    // ── side tree ────────────────────────────────────────────────────────
    pub tree: Vec<CcTreeRow>,
    pub tree_selected: usize,
    /// Workflow run ids folded in the tree (their agents hidden).
    pub collapsed_workflows: HashSet<String>,
    // ── central transcript ───────────────────────────────────────────────
    /// Which node's content is shown; `None` before the first selection.
    pub open: Option<CcNodeRef>,
    /// Parsed transcript of the open agent (empty for a workflow overview).
    pub blocks: Vec<TranscriptBlock>,
    /// mtime (ns) of the open transcript file, for the live-tail re-read gate.
    pub open_mtime: u128,
    pub rows: Vec<CcRow>,
    pub selected: usize,
    pub scroll: usize,
    pub h_scroll: usize,
    pub wrap: bool,
    /// Tool block indices rendered collapsed (body hidden).
    pub collapsed_tools: HashSet<usize>,
    /// Sticky-bottom live-tail: keep the selection pinned to the newest row
    /// while it sits at the end (cleared once the user scrolls up).
    pub follow: bool,
}

impl CcActivityState {
    fn new(activity: CcActivity) -> Self {
        Self {
            activity,
            tree: Vec::new(),
            tree_selected: 0,
            collapsed_workflows: HashSet::new(),
            open: None,
            blocks: Vec::new(),
            open_mtime: 0,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            h_scroll: 0,
            wrap: false,
            collapsed_tools: HashSet::new(),
            follow: true,
        }
    }

    /// Rebuild [`Self::tree`] from the activity snapshot, honouring folds.
    fn rebuild_tree(&mut self) {
        let mut tree = Vec::new();
        for (wi, w) in self.activity.workflows.iter().enumerate() {
            tree.push(CcTreeRow::Workflow(wi));
            if !self.collapsed_workflows.contains(&w.run_id) {
                for ai in 0..w.agents.len() {
                    tree.push(CcTreeRow::WorkflowAgent(wi, ai));
                }
            }
        }
        for si in 0..self.activity.subagents.len() {
            tree.push(CcTreeRow::Subagent(si));
        }
        if tree.is_empty() {
            tree.push(CcTreeRow::Info(
                "No workflows or subagents yet.".to_string(),
            ));
        }
        self.tree = tree;
        if self.tree_selected >= self.tree.len() {
            self.tree_selected = self.tree.len().saturating_sub(1);
        }
    }

    /// The node a tree row points at (`None` for the info placeholder).
    fn node_ref_for(&self, row: &CcTreeRow) -> Option<CcNodeRef> {
        match row {
            CcTreeRow::Workflow(wi) => self
                .activity
                .workflows
                .get(*wi)
                .map(|w| CcNodeRef::WorkflowOverview(w.run_id.clone())),
            CcTreeRow::WorkflowAgent(wi, ai) => {
                let w = self.activity.workflows.get(*wi)?;
                let a = w.agents.get(*ai)?;
                Some(CcNodeRef::WorkflowAgent(
                    w.run_id.clone(),
                    a.agent_id.clone(),
                ))
            }
            CcTreeRow::Subagent(si) => self
                .activity
                .subagents
                .get(*si)
                .map(|a| CcNodeRef::Subagent(a.agent_id.clone())),
            CcTreeRow::Info(_) => None,
        }
    }

    /// Rebuild [`Self::rows`] for the open node: the transcript block list, or a
    /// workflow overview.
    fn rebuild_rows(&mut self) {
        self.rows = match &self.open {
            Some(CcNodeRef::WorkflowOverview(run)) => self
                .activity
                .workflow(run)
                .map(overview_rows)
                .unwrap_or_else(|| vec![CcRow::Info("Workflow no longer present.".to_string())]),
            Some(_) if self.blocks.is_empty() => {
                vec![CcRow::Info("(empty transcript)".to_string())]
            }
            Some(_) => (0..self.blocks.len()).map(CcRow::Block).collect(),
            None => vec![CcRow::Info("Select a workflow or subagent.".to_string())],
        };
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    fn first_selectable(&self) -> usize {
        self.rows.iter().position(CcRow::is_selectable).unwrap_or(0)
    }

    /// Keep the selection within the scroll window (the lower bound is applied
    /// by the renderer, which knows the height).
    fn ensure_visible(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
    }
}

/// A workflow overview rendered as a small synthetic transcript.
fn overview_rows(w: &CcWorkflow) -> Vec<CcRow> {
    let mut rows = Vec::new();
    let title = w.name.clone().unwrap_or_else(|| w.run_id.clone());
    let status = match w.status {
        CcRunStatus::Running => "running",
        CcRunStatus::Completed => "completed",
    };
    rows.push(CcRow::Text(format!("Workflow: {title}  [{status}]")));
    rows.push(CcRow::Text(format!("Run: {}", w.run_id)));
    if let Some(s) = &w.summary {
        let mut line = String::new();
        if let Some(m) = &s.default_model {
            line.push_str(&format!("model {m}  "));
        }
        if let Some(t) = s.total_tokens {
            line.push_str(&format!("{t} tokens  "));
        }
        if let Some(c) = s.total_tool_calls {
            line.push_str(&format!("{c} tool calls  "));
        }
        if let Some(d) = s.duration_ms {
            line.push_str(&format!("{}s", d / 1000));
        }
        if !line.trim().is_empty() {
            rows.push(CcRow::Text(line.trim_end().to_string()));
        }
    }
    if !w.phases.is_empty() {
        rows.push(CcRow::Info(String::new()));
        rows.push(CcRow::Info("Phases".to_string()));
        for p in &w.phases {
            rows.push(CcRow::Text(format!("  {}. {}", p.index, p.title)));
        }
    }
    rows.push(CcRow::Info(String::new()));
    rows.push(CcRow::Info(format!("Agents ({})", w.agents.len())));
    for a in &w.agents {
        let label = a.label.clone().unwrap_or_else(|| a.agent_type.clone());
        let mut line = format!("  {} {}", agent_state_glyph(a.state), label);
        if let Some(ph) = &a.phase_title {
            line.push_str(&format!("  [{ph}]"));
        }
        if let Some(t) = a.tokens {
            line.push_str(&format!("  {t}tok"));
        }
        if let Some(lt) = &a.last_tool {
            line.push_str(&format!("  {lt}"));
        }
        rows.push(CcRow::Text(line));
    }
    if let Some(s) = &w.summary {
        if !s.logs.is_empty() {
            rows.push(CcRow::Info(String::new()));
            rows.push(CcRow::Info("Logs".to_string()));
            for l in &s.logs {
                rows.push(CcRow::Text(format!("  {l}")));
            }
        }
    }
    rows
}

fn agent_state_glyph(state: CcAgentState) -> char {
    match state {
        CcAgentState::Active => '◐',
        CcAgentState::Done => '●',
        CcAgentState::Error => '✗',
    }
}

fn file_mtime(path: &Path) -> u128 {
    std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

impl App {
    /// The active session's open activity view, if any (persisted per session in
    /// [`Self::cc_activities`] so it survives session switches).
    pub(crate) fn active_cc_activity(&self) -> Option<&CcActivityState> {
        self.cc_activities.get(&self.active_session_id()?)
    }

    pub(crate) fn active_cc_activity_mut(&mut self) -> Option<&mut CcActivityState> {
        let sid = self.active_session_id()?;
        self.cc_activities.get_mut(&sid)
    }

    /// Toggle the Claude Code activity view for the active session. Central-pane
    /// overlays are mutually exclusive, so opening this closes any open review.
    pub(crate) fn toggle_cc_activity(&mut self) {
        if self.active_cc_activity().is_some() {
            self.close_cc_activity();
            return;
        }
        if self.active_review().is_some() {
            self.close_code_review();
        }
        let Some(session) = self.sessions.get(self.active_index) else {
            return;
        };
        let session_id = session.info.id;
        let activity = session.info.cc_activity.clone().unwrap_or_default();
        let mut state = CcActivityState::new(activity);
        state.rebuild_tree();
        state.tree_selected = state
            .tree
            .iter()
            .position(CcTreeRow::is_selectable)
            .unwrap_or(0);
        self.cc_activities.insert(session_id, state);
        self.ca_sync_open();
        self.focus = InputFocus::CcActivityTree;
    }

    pub(crate) fn close_cc_activity(&mut self) {
        if let Some(sid) = self.active_session_id() {
            self.cc_activities.remove(&sid);
        }
        if matches!(
            self.focus,
            InputFocus::CcActivity | InputFocus::CcActivityTree
        ) {
            self.focus = InputFocus::Terminal;
        }
    }

    /// Keep the central-pane focus consistent with the active session's activity
    /// view (mirrors [`Self::sync_review_focus`]).
    pub(crate) fn sync_cc_activity_focus(&mut self) {
        let open = self.active_cc_activity().is_some();
        match self.focus {
            InputFocus::CcActivity | InputFocus::CcActivityTree if !open => {
                self.focus = InputFocus::Terminal
            }
            InputFocus::Terminal if open => {
                // Returning to a session whose view stayed open: promote focus and
                // refresh the snapshot, since the background scan updates
                // `SessionInfo.cc_activity` but only live-tails the *active*
                // session — so a workflow that finished while away is now current.
                self.focus = InputFocus::CcActivityTree;
                self.reload_cc_activity();
            }
            _ => {}
        }
    }

    /// Replaces the Phase-2 stub: a background scan updated `session`'s activity
    /// index. If its view is open and active, refresh the snapshot, re-read the
    /// open transcript when it grew, and repaint (the live-tail path).
    pub(super) fn on_cc_activity_updated(&mut self, session: SessionId) {
        if self.active_session_id() != Some(session) || self.active_cc_activity().is_none() {
            return;
        }
        self.reload_cc_activity();
        self.request_redraw();
    }

    /// Refresh the open view from the session's latest `cc_activity` index:
    /// rebuild the tree, then re-read the open transcript if it changed.
    pub(crate) fn reload_cc_activity(&mut self) {
        let Some(sid) = self.active_session_id() else {
            return;
        };
        let activity = self
            .sessions
            .iter()
            .find(|s| s.info.id == sid)
            .and_then(|s| s.info.cc_activity.clone())
            .unwrap_or_default();
        if let Some(ca) = self.active_cc_activity_mut() {
            ca.activity = activity;
            ca.rebuild_tree();
        }
        self.reload_open_transcript();
    }

    /// Re-read the open transcript file when its mtime moved, rebuilding rows.
    /// Sticky-bottom follow jumps to the newest row when already at the end.
    fn reload_open_transcript(&mut self) {
        let (open, prior_mtime, path) = {
            let Some(ca) = self.active_cc_activity() else {
                return;
            };
            let open = ca.open.clone();
            let path = match &open {
                Some(CcNodeRef::WorkflowAgent(run, aid)) => ca
                    .activity
                    .workflow_agent(run, aid)
                    .map(|a| a.transcript_path.clone()),
                Some(CcNodeRef::Subagent(aid)) => {
                    ca.activity.subagent(aid).map(|a| a.transcript_path.clone())
                }
                _ => None,
            };
            (open, ca.open_mtime, path)
        };
        // A workflow overview has no file — rebuild from the fresh snapshot.
        if matches!(open, Some(CcNodeRef::WorkflowOverview(_))) {
            if let Some(ca) = self.active_cc_activity_mut() {
                ca.rebuild_rows();
            }
            return;
        }
        // The open node vanished from the index — re-resolve from the tree.
        let Some(path) = path else {
            self.ca_sync_open();
            return;
        };
        let mtime = file_mtime(&path);
        if mtime == prior_mtime {
            return;
        }
        let blocks = std::fs::read_to_string(&path)
            .map(|s| parse_transcript(&s))
            .unwrap_or_default();
        if let Some(ca) = self.active_cc_activity_mut() {
            let at_end = ca.selected + 1 >= ca.rows.len();
            ca.blocks = blocks;
            ca.open_mtime = mtime;
            ca.rebuild_rows();
            // Follow the newest row when pinned to the bottom; otherwise just
            // keep the selection in bounds after the rebuild.
            if (ca.follow && at_end) || ca.selected >= ca.rows.len() {
                ca.selected = ca.rows.len().saturating_sub(1);
            }
            ca.ensure_visible();
        }
    }

    /// Sync the open node to the tree selection, loading its content when it
    /// changed (auto-preview as you move through the tree).
    fn ca_sync_open(&mut self) {
        let Some((new_open, changed)) = self.active_cc_activity().map(|ca| {
            let n = ca
                .tree
                .get(ca.tree_selected)
                .and_then(|r| ca.node_ref_for(r));
            let changed = n != ca.open;
            (n, changed)
        }) else {
            return;
        };
        if changed {
            self.load_cc_node(new_open);
        }
    }

    /// Load a node's content into the central pane: read + parse an agent
    /// transcript, or build a workflow overview.
    fn load_cc_node(&mut self, node: Option<CcNodeRef>) {
        let path = {
            let Some(ca) = self.active_cc_activity() else {
                return;
            };
            match &node {
                Some(CcNodeRef::WorkflowAgent(run, aid)) => ca
                    .activity
                    .workflow_agent(run, aid)
                    .map(|a| a.transcript_path.clone()),
                Some(CcNodeRef::Subagent(aid)) => {
                    ca.activity.subagent(aid).map(|a| a.transcript_path.clone())
                }
                _ => None, // overview (or None): no file
            }
        };
        let (blocks, mtime) = match &path {
            Some(p) => (
                std::fs::read_to_string(p)
                    .map(|s| parse_transcript(&s))
                    .unwrap_or_default(),
                file_mtime(p),
            ),
            None => (Vec::new(), 0),
        };
        if let Some(ca) = self.active_cc_activity_mut() {
            ca.open = node;
            ca.blocks = blocks;
            ca.open_mtime = mtime;
            ca.collapsed_tools.clear();
            ca.rebuild_rows();
            ca.selected = ca.first_selectable();
            ca.scroll = 0;
            ca.follow = true;
        }
    }

    // ── Tree navigation (side column) ────────────────────────────────────

    pub(crate) fn ca_tree_move(&mut self, delta: isize) {
        {
            let Some(ca) = self.active_cc_activity_mut() else {
                return;
            };
            if ca.tree.is_empty() {
                return;
            }
            let step = delta.signum();
            if step == 0 {
                return;
            }
            let mut idx = ca.tree_selected as isize;
            let len = ca.tree.len() as isize;
            loop {
                idx += step;
                if idx < 0 || idx >= len {
                    return; // hit an edge; leave the selection put
                }
                if ca.tree[idx as usize].is_selectable() {
                    ca.tree_selected = idx as usize;
                    break;
                }
            }
        }
        self.ca_sync_open();
    }

    fn ca_tree_home_end(&mut self, end: bool) {
        {
            let Some(ca) = self.active_cc_activity_mut() else {
                return;
            };
            if ca.tree.is_empty() {
                return;
            }
            if end {
                ca.tree_selected = ca.tree.len() - 1;
                while ca.tree_selected > 0 && !ca.tree[ca.tree_selected].is_selectable() {
                    ca.tree_selected -= 1;
                }
            } else {
                ca.tree_selected = 0;
                while ca.tree_selected + 1 < ca.tree.len()
                    && !ca.tree[ca.tree_selected].is_selectable()
                {
                    ca.tree_selected += 1;
                }
            }
        }
        self.ca_sync_open();
    }

    /// Fold/unfold the workflow the tree selection sits in (Space).
    fn ca_toggle_workflow_fold(&mut self) {
        let Some(ca) = self.active_cc_activity_mut() else {
            return;
        };
        let run_id = match ca.tree.get(ca.tree_selected) {
            Some(CcTreeRow::Workflow(wi)) | Some(CcTreeRow::WorkflowAgent(wi, _)) => {
                ca.activity.workflows.get(*wi).map(|w| w.run_id.clone())
            }
            _ => None,
        };
        if let Some(run_id) = run_id {
            if !ca.collapsed_workflows.remove(&run_id) {
                ca.collapsed_workflows.insert(run_id);
            }
            let sel = ca.tree_selected;
            ca.rebuild_tree();
            ca.tree_selected = sel.min(ca.tree.len().saturating_sub(1));
        }
    }

    /// Jump the tree to a row (a click in the tree column).
    pub(crate) fn ca_jump_to_tree_row(&mut self, idx: usize) {
        if let Some(ca) = self.active_cc_activity_mut() {
            if idx < ca.tree.len() && ca.tree[idx].is_selectable() {
                ca.tree_selected = idx;
            }
        }
        self.ca_sync_open();
    }

    // ── Transcript navigation (central pane) ─────────────────────────────

    fn ca_viewport(&self) -> usize {
        let (rows, _) = self.content_area_size();
        (rows as usize).saturating_sub(3)
    }

    pub(crate) fn ca_move(&mut self, delta: isize) {
        let Some(ca) = self.active_cc_activity_mut() else {
            return;
        };
        if ca.rows.is_empty() {
            return;
        }
        let step = delta.signum();
        if step == 0 {
            return;
        }
        let mut idx = ca.selected as isize;
        let len = ca.rows.len() as isize;
        loop {
            idx += step;
            if idx < 0 || idx >= len {
                break;
            }
            if ca.rows[idx as usize].is_selectable() {
                ca.selected = idx as usize;
                break;
            }
        }
        // Drop sticky-bottom follow when moving off the last row; re-arm it on
        // return, so live-tail resumes once you scroll back down.
        ca.follow = ca.selected + 1 >= ca.rows.len();
        ca.ensure_visible();
    }

    fn ca_page(&mut self, down: bool) {
        let page = self.ca_viewport().max(1) as isize;
        let step = if down { 1 } else { -1 };
        for _ in 0..page {
            self.ca_move(step);
        }
    }

    fn ca_home_end(&mut self, end: bool) {
        let Some(ca) = self.active_cc_activity_mut() else {
            return;
        };
        if ca.rows.is_empty() {
            return;
        }
        if end {
            ca.selected = ca.rows.len() - 1;
            while ca.selected > 0 && !ca.rows[ca.selected].is_selectable() {
                ca.selected -= 1;
            }
        } else {
            ca.selected = 0;
            while ca.selected + 1 < ca.rows.len() && !ca.rows[ca.selected].is_selectable() {
                ca.selected += 1;
            }
        }
        ca.follow = ca.selected + 1 >= ca.rows.len();
        ca.ensure_visible();
    }

    fn ca_scroll_h(&mut self, delta: isize) {
        if let Some(ca) = self.active_cc_activity_mut() {
            if ca.wrap {
                return;
            }
            ca.h_scroll = (ca.h_scroll as isize + delta).max(0) as usize;
        }
    }

    fn ca_toggle_wrap(&mut self) {
        if let Some(ca) = self.active_cc_activity_mut() {
            ca.wrap = !ca.wrap;
            if ca.wrap {
                ca.h_scroll = 0;
            }
        }
    }

    /// Collapse/expand the tool block under the selection (Enter on a tool row).
    fn ca_toggle_tool(&mut self) {
        let Some(ca) = self.active_cc_activity_mut() else {
            return;
        };
        let Some(&CcRow::Block(bi)) = ca.rows.get(ca.selected) else {
            return;
        };
        let is_tool = matches!(
            ca.blocks.get(bi),
            Some(TranscriptBlock::ToolResult { .. }) | Some(TranscriptBlock::ToolUse { .. })
        );
        if is_tool && !ca.collapsed_tools.remove(&bi) {
            ca.collapsed_tools.insert(bi);
        }
    }

    /// Select a transcript row (a click / scrollbar drag in the central pane).
    pub(crate) fn ca_select_row(&mut self, idx: usize) {
        if let Some(ca) = self.active_cc_activity_mut() {
            if idx < ca.rows.len() && ca.rows[idx].is_selectable() {
                ca.selected = idx;
                ca.follow = ca.selected + 1 >= ca.rows.len();
                ca.ensure_visible();
            }
        }
    }

    // ── Key capture (before the global keybinding lookup) ────────────────

    /// Global chords the activity panes let through so the user can always
    /// leave (mirrors [`Self::review_escape_chord`]).
    fn cc_escape_chord(&self, code: KeyCode, mods: KeyModifiers) -> bool {
        matches!(
            self.keybindings.lookup(code, mods),
            Some(
                crate::session::Action::FocusForward
                    | crate::session::Action::FocusBackward
                    | crate::session::Action::QuitApp
                    | crate::session::Action::ToggleCcActivity
                    | crate::session::Action::ToggleReview
                    | crate::session::Action::ToggleShell
                    | crate::session::Action::ToggleHelp
                    | crate::session::Action::OpenSettings
                    | crate::session::Action::OpenThemePicker
                    | crate::session::Action::ToggleInfoPanel
                    | crate::session::Action::GlobalSearch
            )
        )
    }

    /// Central transcript pane key capture. Returns `true` when consumed.
    pub(crate) fn handle_cc_activity_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if self.focus != InputFocus::CcActivity {
            return false;
        }
        if self.cc_escape_chord(code, mods) {
            return false;
        }
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('d') => self.ca_page(true),
                KeyCode::Char('u') => self.ca_page(false),
                _ => {}
            }
            return true;
        }
        if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return true;
        }
        match code {
            KeyCode::Esc => self.close_cc_activity(),
            KeyCode::Down | KeyCode::Char('j') => self.ca_move(1),
            KeyCode::Up | KeyCode::Char('k') => self.ca_move(-1),
            KeyCode::PageDown => self.ca_page(true),
            KeyCode::PageUp => self.ca_page(false),
            KeyCode::Home | KeyCode::Char('g') => self.ca_home_end(false),
            KeyCode::End | KeyCode::Char('G') => self.ca_home_end(true),
            KeyCode::Char('w') => self.ca_toggle_wrap(),
            KeyCode::Left => self.ca_scroll_h(-8),
            KeyCode::Right => self.ca_scroll_h(8),
            // `h` steps back to the tree; `l` has no pane further right.
            KeyCode::Char('h') => self.focus = InputFocus::CcActivityTree,
            KeyCode::Enter => self.ca_toggle_tool(),
            _ => {}
        }
        true
    }

    /// Side tree pane key capture. Returns `true` when consumed.
    pub(crate) fn handle_cc_activity_tree_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> bool {
        if self.focus != InputFocus::CcActivityTree {
            return false;
        }
        if self.cc_escape_chord(code, mods) {
            return false;
        }
        if mods.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Char('d') => self.ca_page(true),
                KeyCode::Char('u') => self.ca_page(false),
                _ => {}
            }
            return true;
        }
        if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return true;
        }
        match code {
            KeyCode::Esc => self.close_cc_activity(),
            KeyCode::Down | KeyCode::Char('j') => self.ca_tree_move(1),
            KeyCode::Up | KeyCode::Char('k') => self.ca_tree_move(-1),
            KeyCode::Home | KeyCode::Char('g') => self.ca_tree_home_end(false),
            KeyCode::End | KeyCode::Char('G') => self.ca_tree_home_end(true),
            KeyCode::Char(' ') => self.ca_toggle_workflow_fold(),
            // Drop into the transcript (already previewing the selected node).
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                self.focus = InputFocus::CcActivity;
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn build_activity_indexes_workflows_and_subagents() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("subagents");

        // A standalone Task subagent.
        write(&sub.join("agent-t1.jsonl"), "{\"type\":\"assistant\"}\n");
        write(
            &sub.join("agent-t1.meta.json"),
            r#"{"agentType":"Explore","description":"scout"}"#,
        );

        // A running workflow: two agents, one finished, one still going. No
        // completion record yet.
        let wf = sub.join("workflows/wf_run/");
        write(&wf.join("agent-a1.jsonl"), "x\n");
        write(
            &wf.join("agent-a1.meta.json"),
            r#"{"agentType":"workflow-subagent"}"#,
        );
        write(&wf.join("agent-a2.jsonl"), "y\n");
        write(
            &wf.join("agent-a2.meta.json"),
            r#"{"agentType":"claude-code-guide"}"#,
        );
        write(
            &wf.join("journal.jsonl"),
            "{\"type\":\"started\",\"agentId\":\"a1\"}\n{\"type\":\"result\",\"agentId\":\"a1\"}\n{\"type\":\"started\",\"agentId\":\"a2\"}\n",
        );

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let act = build_activity(&sub, now);

        assert_eq!(act.subagents.len(), 1);
        assert_eq!(act.subagents[0].agent_type, "Explore");
        assert_eq!(act.subagents[0].description.as_deref(), Some("scout"));

        assert_eq!(act.workflows.len(), 1);
        let w = &act.workflows[0];
        assert_eq!(w.run_id, "wf_run");
        assert_eq!(w.status, CcRunStatus::Running);
        assert_eq!(w.agents.len(), 2);
        let a1 = w.agents.iter().find(|a| a.agent_id == "a1").unwrap();
        let a2 = w.agents.iter().find(|a| a.agent_id == "a2").unwrap();
        assert_eq!(a1.state, CcAgentState::Done); // has result
        assert_eq!(a2.state, CcAgentState::Active); // started, no result
        assert!(act.any_active());
    }

    #[test]
    fn build_activity_uses_completion_grid_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("subagents");
        let wf = sub.join("workflows/wf_done/");
        write(&wf.join("agent-x9.jsonl"), "z\n");
        write(
            &wf.join("agent-x9.meta.json"),
            r#"{"agentType":"workflow-subagent"}"#,
        );
        // Completion record is a sibling FILE of the run dir.
        write(
            &sub.join("workflows/wf_done.json"),
            r#"{"workflowName":"demo","status":"completed","phases":[{"index":1,"title":"Go"}],
                "workflowProgress":[{"type":"workflow_agent","agentId":"x9","label":"the-agent","phaseTitle":"Go","state":"done","tokens":42,"toolCalls":3,"lastToolName":"Read","model":"m"}]}"#,
        );

        let now = 0;
        let act = build_activity(&sub, now);
        let w = &act.workflows[0];
        assert_eq!(w.status, CcRunStatus::Completed);
        assert_eq!(w.name.as_deref(), Some("demo"));
        assert_eq!(w.phases.len(), 1);
        let a = &w.agents[0];
        assert_eq!(a.label.as_deref(), Some("the-agent"));
        assert_eq!(a.phase_title.as_deref(), Some("Go"));
        assert_eq!(a.state, CcAgentState::Done);
        assert_eq!(a.tokens, Some(42));
        assert_eq!(a.last_tool.as_deref(), Some("Read"));
    }

    #[test]
    fn dir_signature_moves_on_append_only() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("subagents");
        write(&sub.join("agent-a.jsonl"), "one\n");
        let s1 = dir_signature(&sub);
        // Same content → same signature.
        assert_eq!(s1, dir_signature(&sub));
        // Grow the file → different signature (len changes even if mtime ties).
        write(&sub.join("agent-a.jsonl"), "one\ntwo\n");
        assert_ne!(s1, dir_signature(&sub));
    }

    #[test]
    fn agent_id_from_filenames() {
        assert_eq!(agent_id_from("agent-abc123.jsonl"), Some("abc123"));
        assert_eq!(agent_id_from("agent-abc123.meta.json"), None);
        assert_eq!(agent_id_from("journal.jsonl"), None);
    }

    #[test]
    fn resolve_scans_project_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let projects = tmp.path();
        let want = projects.join("-some-slug/sid-123/subagents");
        std::fs::create_dir_all(&want).unwrap();
        std::fs::create_dir_all(projects.join("-other/unrelated")).unwrap();
        let dirs: Vec<PathBuf> = std::fs::read_dir(projects)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(resolve_subagents_dir(&dirs, "sid-123"), Some(want));
        assert_eq!(resolve_subagents_dir(&dirs, "missing"), None);
    }
}
