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
    parse_job_state, parse_journal, parse_meta, parse_roster, parse_transcript,
    parse_workflow_completion, CcFanEntry, CcJobState, CcPhase, CcWorkflowSummary, JournalEntry,
    TranscriptBlock, WorkflowCompletion,
};
use crate::session::{
    CcActivity, CcAgent, CcAgentState, CcRunStatus, CcWorkflow, SessionId, SessionInfo,
};

use super::{activity, background, session_member_dirs, App, InputFocus};

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

/// One session's scan input. Besides its own `agent_session_id`, a session may
/// **own** additional `subagents/` trees written by **background/daemon
/// workers** it launched (a detached workflow runs under the worker's own
/// session id, not this session's). Those are attributed on the scan thread by
/// matching the worker's replayed `--settings` path against this instance's
/// hooks settings and its `cwd` against `candidate_dirs`.
struct CcSessionInput {
    id: SessionId,
    own_id: String,
    /// Normalized (trailing-slash-trimmed) launch dirs this session's agent
    /// could have started in — the disambiguator for the worker `cwd` match.
    candidate_dirs: Vec<String>,
    prior_sig: Option<u64>,
}

impl App {
    /// Kick off a background scan of every local session's `subagents/` tree —
    /// its own, plus any owned by background/daemon workers it launched (see
    /// [`CcSessionInput`]). Skips remote sessions (their `~/.claude` lives on the
    /// host) and sessions without an agent conversation id. Non-claude local
    /// sessions simply resolve to no directory (empty activity) — cheap, and it
    /// means claude-based agents under custom names (flow/shepherd workers) are
    /// covered without a name allowlist.
    pub(super) fn start_cc_refresh(&mut self) {
        if self.cc_refresh.in_progress() {
            return;
        }
        let Some(projects) = crate::paths::claude_projects_dir(None) else {
            return;
        };
        let inputs: Vec<CcSessionInput> = self
            .sessions
            .iter()
            .filter_map(|s| {
                if s.info.remote_host.is_some() {
                    return None;
                }
                let own_id = s.info.agent_session_id.clone()?;
                Some(CcSessionInput {
                    id: s.info.id,
                    own_id,
                    candidate_dirs: self.session_candidate_dirs(&s.info),
                    prior_sig: self.cached_cc_signatures.get(&s.info.id).copied(),
                })
            })
            .collect();
        if inputs.is_empty() {
            return;
        }
        // Attribution inputs: the daemon roster + jobs dir, and this instance's
        // hooks `--settings` path (the flag the daemon replays). All resolved on
        // the UI thread; the reads happen off-thread in `collect_cc_activity`.
        let roster = crate::paths::claude_daemon_roster(None);
        let jobs_dir = crate::paths::claude_jobs_dir(None);
        let hooks_settings = crate::session_ops::builtin_hooks::hooks_settings_path();
        let tx = self.cc_refresh.start();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(collect_cc_activity(
                projects,
                roster,
                jobs_dir,
                hooks_settings,
                inputs,
            ));
        });
    }

    /// The set of launch dirs a session's agent could have started in, normalized
    /// for the worker-`cwd` match: every member dir (worktree / additional dir /
    /// primary cwd) plus the resolved process cwd (the symlink workspace for a
    /// multi-repo session). Uniqued. Shared with the agent-neutral activity
    /// scan, whose cwd-keyed providers match against the same set.
    pub(super) fn session_candidate_dirs(&self, info: &SessionInfo) -> Vec<String> {
        let mut dirs: Vec<PathBuf> =
            session_member_dirs(info.cwd.as_deref(), &info.worktrees, &info.additional_dirs)
                .into_iter()
                .map(|(_, p)| p)
                .collect();
        if let Some(cwd) = info.cwd.clone() {
            dirs.push(cwd);
        }
        if let Some(pcwd) = self.session_process_cwd_existing(info) {
            dirs.push(pcwd);
        }
        let mut out: Vec<String> = Vec::new();
        for d in dirs {
            let n = normalize_dir(&d.to_string_lossy());
            if !out.contains(&n) {
                out.push(n);
            }
        }
        out
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

/// Off-thread: scan each session's owned `subagents/` trees into one merged
/// [`CcActivity`] index, skipping the parse when the combined signature is
/// unchanged. "Owned" = the session's own conversation id plus any
/// background/daemon worker it launched (attributed via [`attribute_workers`]).
fn collect_cc_activity(
    projects: PathBuf,
    roster_path: Option<PathBuf>,
    jobs_dir: Option<PathBuf>,
    hooks_settings: Option<String>,
    inputs: Vec<CcSessionInput>,
) -> CcRefresh {
    // List the project slug dirs once; each owned tree is one of their
    // `<session_id>/subagents` children. Scanning sidesteps Claude Code's slug
    // rule (which replaces `/` *and* `.` — and likely all non-alnum — with `-`,
    // so a computed slug is wrong for any dotted path).
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

    // Load the daemon worker pool once (persistent jobs + live roster), then
    // attribute each worker to at most one session by its replayed --settings +
    // cwd. Cheap: a handful of small JSON files, read once per scan.
    let jobs = load_jobs(jobs_dir.as_deref());
    let workers = collect_workers(roster_path.as_deref(), &jobs);
    let owners = attribute_workers(&workers, hooks_settings.as_deref(), &inputs);

    let mut updates = Vec::with_capacity(inputs.len());
    for (idx, input) in inputs.iter().enumerate() {
        // Owned session ids: this session's own conversation id, plus every
        // attributed worker's own session id.
        let mut owned_ids: Vec<String> = vec![input.own_id.clone()];
        for (wsid, owner_idx) in &owners {
            if *owner_idx == idx && *wsid != input.own_id {
                owned_ids.push(wsid.clone());
            }
        }
        // Resolve each owned id to its `subagents/` dir; a worker dir also
        // carries its live job state (drives the overview + change signature).
        let owned: Vec<(PathBuf, Option<CcJobState>)> = owned_ids
            .iter()
            .filter_map(|oid| {
                resolve_subagents_dir(&project_dirs, oid).map(|dir| (dir, jobs.get(oid).cloned()))
            })
            .collect();

        if owned.is_empty() {
            // No subagents tree (yet, or a non-claude session): clear any stale
            // activity, otherwise leave the field untouched.
            match input.prior_sig {
                Some(p) if p != 0 => updates.push((input.id, 0, Some(CcActivity::default()))),
                _ => updates.push((input.id, 0, None)),
            }
            continue;
        }

        let sig = union_signature(&owned);
        if Some(sig) == input.prior_sig {
            updates.push((input.id, sig, None));
        } else {
            updates.push((input.id, sig, Some(build_union(&owned, now_ns))));
        }
    }
    CcRefresh { updates }
}

/// Find `<project>/<session_id>/subagents` across the project slug dirs.
fn resolve_subagents_dir(project_dirs: &[PathBuf], sid: &str) -> Option<PathBuf> {
    for p in project_dirs {
        let d = p.join(sid).join("subagents");
        if d.is_dir() {
            return Some(d);
        }
    }
    None
}

/// Read every `jobs/<short>/state.json` into a map keyed by the worker's own
/// session id (the key to its `subagents/` dir). Persistent — a settled run
/// stays attributable, so a finished background workflow is still shown.
fn load_jobs(jobs_dir: Option<&Path>) -> HashMap<String, CcJobState> {
    let mut out = HashMap::new();
    let Some(dir) = jobs_dir else {
        return out;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        if let Ok(s) = std::fs::read_to_string(e.path().join("state.json")) {
            if let Some(job) = parse_job_state(&s) {
                out.insert(job.session_id.clone(), job);
            }
        }
    }
    out
}

/// A background/daemon worker candidate for attribution: its own session id (the
/// `subagents/` dir key) plus the replayed `--settings` path and `cwd` used to
/// match it to a session.
struct WorkerRef {
    session_id: String,
    settings_path: Option<String>,
    cwd: Option<String>,
}

/// The worker pool: every persistent daemon job, plus any live roster worker not
/// yet backed by a job file (freshly claimed, no `state.json` written).
fn collect_workers(
    roster_path: Option<&Path>,
    jobs: &HashMap<String, CcJobState>,
) -> Vec<WorkerRef> {
    let mut out: Vec<WorkerRef> = jobs
        .values()
        .map(|j| WorkerRef {
            session_id: j.session_id.clone(),
            settings_path: j.settings_path.clone(),
            cwd: j.cwd.clone(),
        })
        .collect();
    if let Some(s) = roster_path.and_then(|p| std::fs::read_to_string(p).ok()) {
        for w in parse_roster(&s) {
            if !jobs.contains_key(&w.session_id) {
                out.push(WorkerRef {
                    session_id: w.session_id,
                    settings_path: w.settings_path,
                    cwd: w.cwd,
                });
            }
        }
    }
    out
}

/// Attribute each worker to the friring session that launched it, by the
/// `--settings` path the daemon replays. Two forms (both scoped to this
/// instance's hooks dir):
///
/// - **Exact** (Phase 2): a per-session `<hooks_home>/sessions/<id>.json` names
///   the session whose conversation id is `<id>` — cwd is irrelevant, so two
///   non-worktree sessions on one repo are unambiguous.
/// - **Legacy** (shared `<hooks_home>/claude.json`, pre-Phase-2 or non-unix):
///   fall back to matching the worker's cwd against a session's candidate launch
///   dirs, first session by input order.
///
/// A worker is owned by **at most one** session. Returns
/// `worker_session_id → session_index`.
fn attribute_workers(
    workers: &[WorkerRef],
    hooks_settings: Option<&str>,
    inputs: &[CcSessionInput],
) -> HashMap<String, usize> {
    let mut owners = HashMap::new();
    // No hooks settings → nothing to correlate a worker against.
    let Some(shared) = hooks_settings.map(normalize_dir) else {
        return owners;
    };
    // The per-session symlinks live at `<hooks_home>/sessions/<id>.json`.
    let sessions_prefix = shared
        .strip_suffix("/claude.json")
        .map(|home| format!("{home}/sessions/"));

    for w in workers {
        let Some(wsettings) = w.settings_path.as_deref().map(normalize_dir) else {
            continue;
        };
        // Exact match by the per-session path's `<id>` basename.
        if let Some(id) = sessions_prefix
            .as_deref()
            .and_then(|p| wsettings.strip_prefix(p))
            .and_then(|f| f.strip_suffix(".json"))
        {
            if let Some(idx) = inputs
                .iter()
                .position(|s| s.own_id.as_str() == id && s.own_id != w.session_id)
            {
                owners.entry(w.session_id.clone()).or_insert(idx);
            }
            // A per-session path is authoritative — never fall through to cwd.
            continue;
        }
        // Legacy shared path: must be *our* shared claude.json, then match by cwd.
        if wsettings != shared {
            continue;
        }
        let Some(wcwd) = w.cwd.as_deref().map(normalize_dir) else {
            continue;
        };
        let owner = inputs
            .iter()
            .position(|s| s.own_id != w.session_id && s.candidate_dirs.contains(&wcwd));
        if let Some(idx) = owner {
            owners.entry(w.session_id.clone()).or_insert(idx);
        }
    }
    owners
}

/// Trim trailing path separators so `/repo` and `/repo/` compare equal (the
/// daemon records `--add-dir /repo/` but the worker `cwd` as `/repo`). Shared
/// with the activity providers' cwd matching (`super::activity`).
pub(super) fn normalize_dir(s: &str) -> String {
    let t = s.trim_end_matches('/');
    if t.is_empty() {
        s.to_string()
    } else {
        t.to_string()
    }
}

/// Combined change signature over a session's owned trees plus the live fields
/// of any attributed daemon job — so a fan/tempo/needs change re-triggers a
/// parse just like a transcript append does.
fn union_signature(owned: &[(PathBuf, Option<CcJobState>)]) -> u64 {
    let mut items: Vec<(String, u128, u64)> = Vec::new();
    let mut job_states: Vec<&CcJobState> = Vec::new();
    for (dir, job) in owned {
        collect_dir_items(dir, &mut items);
        if let Some(j) = job {
            job_states.push(j);
        }
    }
    items.sort();
    job_states.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    let mut hasher = DefaultHasher::new();
    items.hash(&mut hasher);
    job_states.hash(&mut hasher);
    hasher.finish()
}

/// Hash of the whole `subagents/` subtree (each file's path + mtime + len). A
/// live JSONL append bumps mtime → the hash moves → the tree is re-parsed; an
/// idle tree hashes identically and skips the parse. (The production path uses
/// [`union_signature`], which folds several trees + job state; this single-tree
/// form backs the change-detection unit test.)
#[cfg(test)]
fn dir_signature(root: &Path) -> u64 {
    let mut items: Vec<(String, u128, u64)> = Vec::new();
    collect_dir_items(root, &mut items);
    items.sort();
    let mut hasher = DefaultHasher::new();
    items.hash(&mut hasher);
    hasher.finish()
}

/// Append every file's (path, mtime, len) under `root` to `items`.
fn collect_dir_items(root: &Path, items: &mut Vec<(String, u128, u64)>) {
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
}

/// Merge a session's owned `subagents/` trees into one activity index. Each
/// owned entry is a dir plus (for a background-worker dir) its live job state,
/// which enriches an in-flight workflow before any completion record exists.
fn build_union(owned: &[(PathBuf, Option<CcJobState>)], now_ns: u128) -> CcActivity {
    let mut act = CcActivity::default();
    for (dir, job) in owned {
        build_activity_into(&mut act, dir, job.as_ref(), now_ns);
    }
    // Most-recently-active first, in both lists.
    act.subagents.sort_by_key(|a| std::cmp::Reverse(a.mtime_ns));
    act.workflows
        .sort_by_key(|w| std::cmp::Reverse(workflow_mtime(w)));
    act
}

/// Single-tree convenience wrapper (an in-process session with no daemon job).
#[cfg(test)]
fn build_activity(subagents: &Path, now_ns: u128) -> CcActivity {
    build_union(&[(subagents.to_path_buf(), None)], now_ns)
}

/// Index one `subagents/` directory into `act`: standalone `Task` subagents
/// (top-level `agent-*.jsonl`) plus each `workflows/wf_*/` run. `job` is the
/// live daemon state when this dir belongs to a background worker.
fn build_activity_into(
    act: &mut CcActivity,
    subagents: &Path,
    job: Option<&CcJobState>,
    now_ns: u128,
) {
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
            if let Some(wf) = build_workflow(&wf_root, &path, run_id, job) {
                act.workflows.push(wf);
            }
        }
    }
}

/// Build one workflow run. When a completion record (`wf_<id>.json`) exists it is
/// authoritative; otherwise, for a live **background/daemon** run, the job's
/// `fan[]` grid enriches each agent (label / phase-group / done-fail-run state)
/// by id and supplies a live summary (`tempo`/`needs`/`tokens`) — the record
/// isn't written until the run settles. In-process runs with neither fall back
/// to the `journal.jsonl` edges.
fn build_workflow(
    wf_root: &Path,
    run_dir: &Path,
    run_id: String,
    job: Option<&CcJobState>,
) -> Option<CcWorkflow> {
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
    // The live fan grid only matters before a completion record lands.
    let fan = completion
        .is_none()
        .then(|| job.map(|j| j.fan_by_id()))
        .flatten();

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
            let fan_entry = fan.as_ref().and_then(|f| f.get(id.as_str()).copied());
            let state = workflow_agent_state(&journal, completion.as_ref(), fan_entry, &id);
            agents.push(CcAgent {
                agent_id: id,
                transcript_path: path,
                agent_type: meta.agent_type,
                description: meta.description,
                // Completion label wins; else the live fan label (skip a blank).
                label: progress
                    .and_then(|p| p.label.clone())
                    .or_else(|| fan_entry.map(|f| f.label.clone()).filter(|l| !l.is_empty())),
                phase_title: progress
                    .and_then(|p| p.phase_title.clone())
                    .or_else(|| fan_entry.and_then(|f| f.group.clone())),
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

    let (name, mut phases, summary, tempo, needs) = match completion {
        Some(c) => (c.workflow_name, c.phases, Some(c.summary), None, None),
        // Live background run: synthesize a summary from the job's totals; the
        // completion record's phase list isn't written yet.
        None => match job {
            Some(j) => (
                None,
                Vec::new(),
                Some(CcWorkflowSummary {
                    status: j.state.clone(),
                    total_tokens: j.tokens,
                    ..Default::default()
                }),
                j.tempo.clone(),
                j.needs.clone(),
            ),
            None => (None, Vec::new(), None, None, None),
        },
    };
    // Derive phases from the agents' (fan-supplied) phase titles, first-seen
    // order — the live substitute for a completion record's `phases[]`.
    if phases.is_empty() {
        phases = derive_phases(&agents);
    }
    Some(CcWorkflow {
        run_id,
        name,
        dir: run_dir.to_path_buf(),
        status,
        phases,
        agents,
        summary,
        tempo,
        needs,
    })
}

/// A workflow agent's state, in precedence order: the completion grid when the
/// run has finished, else a live daemon `fan[]` entry (failed → `Error`, has a
/// `doneAt` → `Done`, else `Active`), else the `journal.jsonl` edges.
fn workflow_agent_state(
    journal: &HashMap<String, JournalEntry>,
    completion: Option<&WorkflowCompletion>,
    fan: Option<&CcFanEntry>,
    agent_id: &str,
) -> CcAgentState {
    if let Some(c) = completion {
        return match c.agents.get(agent_id).and_then(|p| p.state.as_deref()) {
            Some("error") => CcAgentState::Error,
            _ => CcAgentState::Done, // a completed run's agents are all terminal
        };
    }
    if let Some(f) = fan {
        return if f.failed {
            CcAgentState::Error
        } else if f.done {
            CcAgentState::Done
        } else {
            CcAgentState::Active
        };
    }
    match journal.get(agent_id) {
        Some(j) if j.has_result => CcAgentState::Done,
        Some(j) if j.started => CcAgentState::Active,
        _ => CcAgentState::Done,
    }
}

/// Distinct phase titles across a workflow's agents, in first-seen order — the
/// live substitute for a completion record's `phases[]` (a background run's
/// record isn't written until it settles).
fn derive_phases(agents: &[CcAgent]) -> Vec<CcPhase> {
    let mut seen: Vec<String> = Vec::new();
    for a in agents {
        if let Some(t) = &a.phase_title {
            if !seen.contains(t) {
                seen.push(t.clone());
            }
        }
    }
    seen.into_iter()
        .enumerate()
        .map(|(i, title)| CcPhase {
            index: i as u64 + 1,
            title,
        })
        .collect()
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
    /// One of the activity sections (overview / timeline / commands / files /
    /// web / agents), rendered from the session's normalized event stream.
    Section(activity::Section),
    /// A workflow's overview (phases + per-agent grid + logs) — `run_id`.
    WorkflowOverview(String),
    /// One workflow-spawned agent's transcript — `(run_id, agent_id)`.
    WorkflowAgent(String, String),
    /// A standalone Task subagent's transcript — `agent_id`.
    Subagent(String),
}

/// One row of the side navigator. Sections lead; the workflow/subagent rows
/// nest under the Agents section (indices point into the state's
/// [`CcActivity`] snapshot; a collapsed workflow hides its agent rows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CcTreeRow {
    Section(activity::Section),
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
/// overview, hints); `Header`/`Tiles`/`Spark` are the Overview dashboard's
/// rows — `Info` and `Header` are non-selectable scaffolding.
#[derive(Debug, Clone)]
pub(crate) enum CcRow {
    Block(usize),
    Text(String),
    Info(String),
    /// A dashboard section header ("Hot files", "Last error").
    Header(String),
    /// One row of stat tiles (glyph + value + label each).
    Tiles(Vec<StatTile>),
    /// An activity-over-time sparkline with its caption.
    Spark(SparkRow),
}

impl CcRow {
    pub(crate) fn is_selectable(&self) -> bool {
        !matches!(self, CcRow::Info(_) | CcRow::Header(_))
    }
}

/// One Overview stat tile. Pure data — the ui layer maps [`Tone`] onto the
/// theme (arch rule: `app` never styles).
#[derive(Debug, Clone)]
pub(crate) struct StatTile {
    pub glyph: &'static str,
    pub value: String,
    pub label: &'static str,
    pub tone: Tone,
}

/// Semantic color tone for dashboard elements; resolved to theme colors by
/// the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Accent,
    Working,
    Done,
    Danger,
    Normal,
}

/// The Overview's activity-over-time sparkline: event counts bucketed across
/// the session's timestamped span.
#[derive(Debug, Clone)]
pub(crate) struct SparkRow {
    /// Wall-clock `HH:MM` of the first/last timestamped event.
    pub start: String,
    pub end: String,
    pub buckets: Vec<u64>,
    /// e.g. `47 events · 42m`.
    pub caption: String,
}

/// In-transcript text search (the `/`-triggered find sub-mode), mirroring the
/// code-review view's find. Matches rows whose text — prose, tool names/inputs,
/// tool output — contains the query (case-insensitive). While [`Self::editing`]
/// every key edits the query (the selection jumps to the first match as you
/// type); `Enter`/`↓`/`Ctrl+N` step to the next match, `↑`/`Ctrl+P` the previous,
/// and `Tab` commits — after which the bar stays for highlighting and `n`/`N`
/// step matches just like the file viewer.
pub(crate) struct CcSearch {
    pub query: String,
    pub editing: bool,
    /// Matching row indices (into [`CcActivityState::rows`]), in row order.
    pub matches: Vec<usize>,
}

/// The open activity view for one session (persisted in `App::cc_activities`).
pub(crate) struct CcActivityState {
    /// Snapshot of the session's activity index, refreshed from
    /// `SessionInfo.cc_activity` while the view is open (live-tail).
    pub activity: CcActivity,
    /// Per-kind tallies of the session's normalized event stream — the
    /// navigator's section counts, refreshed with the event scan.
    pub counts: crate::session::activity::ActivityCounts,
    /// Distinct files touched (the Files section's count).
    pub files_count: usize,
    // ── side navigator (sections + agents tree) ──────────────────────────
    pub tree: Vec<CcTreeRow>,
    pub tree_selected: usize,
    /// Workflow run ids folded in the tree (their agents hidden).
    pub collapsed_workflows: HashSet<String>,
    /// Whether the Agents section's subtree is folded.
    pub agents_folded: bool,
    // ── central transcript ───────────────────────────────────────────────
    /// Which node's content is shown; `None` before the first selection.
    pub open: Option<CcNodeRef>,
    /// Parsed transcript of the open agent, or the open section's event
    /// blocks (empty for a workflow overview / synthetic-row sections).
    pub blocks: Vec<TranscriptBlock>,
    /// Synthetic rows for sections that aren't event lists (overview/files):
    /// consumed by [`Self::rebuild_rows`] when a section is open.
    pub section_rows: Vec<CcRow>,
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
    /// The open find-in-transcript search, if any (see [`CcSearch`]).
    pub search: Option<CcSearch>,
}

impl CcActivityState {
    fn new(activity: CcActivity) -> Self {
        Self {
            activity,
            counts: crate::session::activity::ActivityCounts::default(),
            files_count: 0,
            tree: Vec::new(),
            tree_selected: 0,
            collapsed_workflows: HashSet::new(),
            agents_folded: false,
            open: None,
            blocks: Vec::new(),
            section_rows: Vec::new(),
            open_mtime: 0,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            h_scroll: 0,
            wrap: false,
            collapsed_tools: HashSet::new(),
            follow: true,
            search: None,
        }
    }

    /// The searchable text of a row: prose (`Text`), or a transcript block's
    /// rendered text (prompt/thinking/text body, tool name + input, tool result).
    /// Drives both `/` matching and the in-row highlight, so the two never
    /// disagree about what a row "contains". `Info` rows (hints / overview
    /// scaffolding) aren't searchable.
    fn row_text(&self, row: &CcRow) -> Option<String> {
        match row {
            // Non-selectable scaffolding is unsearchable — a match must be a
            // row the selection can land on.
            CcRow::Info(_) | CcRow::Header(_) => None,
            CcRow::Text(s) => Some(s.clone()),
            CcRow::Block(bi) => self.blocks.get(*bi).map(block_search_text),
            CcRow::Tiles(tiles) => Some(
                tiles
                    .iter()
                    .map(|t| format!("{} {}", t.value, t.label))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            CcRow::Spark(s) => Some(s.caption.clone()),
        }
    }

    /// Row indices whose [`Self::row_text`] contains `query` case-insensitively.
    /// Empty/whitespace query → no matches. Pure, so it is unit-testable.
    fn search_matches(&self, query: &str) -> Vec<usize> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        (0..self.rows.len())
            .filter(|&i| {
                self.row_text(&self.rows[i])
                    .is_some_and(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }

    /// Recompute the open search's match set against the current [`Self::rows`]
    /// (no-op when no search is open) — called after the query changes and after
    /// the rows are rebuilt (live-tail), so `n`/`N` + the highlight stay anchored.
    fn refresh_search_matches(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            return;
        };
        let matches = self.search_matches(&query);
        if let Some(s) = self.search.as_mut() {
            s.matches = matches;
        }
    }

    /// The active (non-empty) search query, lowercased for the in-row highlight —
    /// `None` unless a search is open with text, so the renderer highlights
    /// exactly the rows [`Self::search_matches`] counted.
    pub(crate) fn active_query(&self) -> Option<String> {
        self.search
            .as_ref()
            .map(|s| s.query.trim().to_lowercase())
            .filter(|q| !q.is_empty())
    }

    /// Rebuild [`Self::tree`]: the sections, then the workflow/subagent rows
    /// nested under the Agents section, honouring folds.
    fn rebuild_tree(&mut self) {
        let mut tree = Vec::new();
        for section in activity::SECTIONS {
            tree.push(CcTreeRow::Section(section));
        }
        if !self.agents_folded {
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
            if self.activity.is_empty() {
                tree.push(CcTreeRow::Info(
                    "No workflows or subagents yet.".to_string(),
                ));
            }
        }
        self.tree = tree;
        if self.tree_selected >= self.tree.len() {
            self.tree_selected = self.tree.len().saturating_sub(1);
        }
    }

    /// The navigator row of a section (sections lead the tree in
    /// [`activity::SECTIONS`] order).
    pub(crate) fn section_row(&self, section: activity::Section) -> Option<usize> {
        self.tree
            .iter()
            .position(|r| matches!(r, CcTreeRow::Section(s) if *s == section))
    }

    /// The node a tree row points at (`None` for the info placeholder).
    fn node_ref_for(&self, row: &CcTreeRow) -> Option<CcNodeRef> {
        match row {
            CcTreeRow::Section(s) => Some(CcNodeRef::Section(*s)),
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

    /// Rebuild [`Self::rows`] for the open node: the transcript/event block
    /// list, a section's synthetic rows, or a workflow overview.
    fn rebuild_rows(&mut self) {
        self.rows = match &self.open {
            Some(CcNodeRef::WorkflowOverview(run)) => self
                .activity
                .workflow(run)
                .map(overview_rows)
                .unwrap_or_else(|| vec![CcRow::Info("Workflow no longer present.".to_string())]),
            // Synthetic-row sections (overview / files / agents summary).
            Some(CcNodeRef::Section(_)) if !self.section_rows.is_empty() => {
                self.section_rows.clone()
            }
            Some(CcNodeRef::Section(_)) if self.blocks.is_empty() => {
                vec![CcRow::Info("No such activity yet.".to_string())]
            }
            Some(_) if self.blocks.is_empty() => {
                vec![CcRow::Info("(empty transcript)".to_string())]
            }
            Some(_) => (0..self.blocks.len()).map(CcRow::Block).collect(),
            None => vec![CcRow::Info("Select a section above.".to_string())],
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

/// The searchable text of a transcript block — matches what the renderer shows,
/// so `/` find and the highlight agree.
fn block_search_text(block: &TranscriptBlock) -> String {
    match block {
        TranscriptBlock::Prompt(s)
        | TranscriptBlock::Thinking(s)
        | TranscriptBlock::Text(s)
        | TranscriptBlock::ToolResult { content: s, .. } => s.clone(),
        TranscriptBlock::ToolUse { name, input } => format!("{name} {input}"),
        TranscriptBlock::Event(e) => {
            let mut text = format!("{} {}", e.kind.label(), e.detail);
            if let Some(n) = &e.note {
                text.push(' ');
                text.push_str(n);
            }
            if let Some(r) = &e.result_head {
                text.push(' ');
                text.push_str(r);
            }
            text
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
    // Live pace of a background/daemon run (blocked-on-approval, etc.).
    if let Some(t) = &w.tempo {
        rows.push(CcRow::Text(format!("Pace: {t}")));
    }
    if let Some(n) = &w.needs {
        rows.push(CcRow::Text(format!("Waiting on: {n}")));
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
        // Seed the navigator's section counts from the accumulated events.
        self.refresh_activity_view(false);
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
                // refresh the snapshot, since the background scans update
                // `SessionInfo.cc_activity` / `App::activity` but only live-tail
                // the *active* session — so a workflow that finished (or events
                // that accrued) while away are now current.
                self.focus = InputFocus::CcActivityTree;
                self.reload_cc_activity();
                self.refresh_activity_view(false);
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
        // Sections live-tail via `reload_open_section` on the event scan's
        // cadence, not the tree scan's.
        if matches!(open, Some(CcNodeRef::Section(_))) {
            return;
        }
        // A workflow overview has no file — rebuild from the fresh snapshot.
        if matches!(open, Some(CcNodeRef::WorkflowOverview(_))) {
            if let Some(ca) = self.active_cc_activity_mut() {
                ca.rebuild_rows();
                ca.refresh_search_matches();
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
            ca.refresh_search_matches();
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
    /// transcript, build a workflow overview, or build a section's content
    /// from the session's normalized event stream.
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
                _ => None, // section / overview (or None): no file
            }
        };
        let (blocks, section_rows) = match &node {
            Some(CcNodeRef::Section(s)) => self.section_content(*s),
            _ => (
                path.as_deref()
                    .map(|p| {
                        std::fs::read_to_string(p)
                            .map(|s| parse_transcript(&s))
                            .unwrap_or_default()
                    })
                    .unwrap_or_default(),
                Vec::new(),
            ),
        };
        let mtime = path.as_deref().map(file_mtime).unwrap_or(0);
        if let Some(ca) = self.active_cc_activity_mut() {
            ca.open = node;
            ca.blocks = blocks;
            ca.section_rows = section_rows;
            ca.open_mtime = mtime;
            ca.collapsed_tools.clear();
            ca.rebuild_rows();
            ca.refresh_search_matches();
            ca.selected = ca.first_selectable();
            ca.scroll = 0;
            ca.follow = true;
        }
    }

    /// Build a section's central-pane content from the active session's
    /// event stream: event blocks for the list sections, synthetic rows for
    /// overview / files / the agents summary.
    fn section_content(&self, section: activity::Section) -> (Vec<TranscriptBlock>, Vec<CcRow>) {
        let Some(info) = self.sessions.get(self.active_index).map(|s| &s.info) else {
            return (Vec::new(), Vec::new());
        };
        let act = self.activity.get(&info.id);
        let events = act.map(|a| a.events()).unwrap_or_default();
        match section {
            activity::Section::Overview => (
                Vec::new(),
                activity::overview_rows(
                    &info.agent,
                    &self.session_command(info),
                    self.session_provider(info),
                    act,
                    info,
                ),
            ),
            activity::Section::Files => (Vec::new(), activity::files_rows(events)),
            activity::Section::Agents => {
                let (wf, sub) = (
                    self.active_cc_activity()
                        .map(|ca| ca.activity.workflows.len())
                        .unwrap_or(0),
                    self.active_cc_activity()
                        .map(|ca| ca.activity.subagents.len())
                        .unwrap_or(0),
                );
                (
                    Vec::new(),
                    vec![
                        CcRow::Text(format!("{wf} workflows · {sub} subagents")),
                        CcRow::Info(
                            "Select a workflow or subagent in the navigator for its transcript."
                                .to_string(),
                        ),
                    ],
                )
            }
            list => (activity::section_blocks(events, list), Vec::new()),
        }
    }

    /// Refresh the counts snapshot + the open section after an event-scan
    /// pass. `changed` gates the redraw (an idle pass costs nothing); the
    /// rebuild itself always runs so a section opened while the accumulator
    /// was in flight backfills.
    pub(super) fn refresh_activity_view(&mut self, changed: bool) {
        let Some(sid) = self.active_session_id() else {
            return;
        };
        if !self.cc_activities.contains_key(&sid) {
            return;
        }
        let (counts, files_count) = self
            .activity
            .get(&sid)
            .map(|a| {
                let events = a.events();
                (
                    crate::session::activity::ActivityCounts::tally(events),
                    crate::session::activity::aggregate_files(events).len(),
                )
            })
            .unwrap_or_default();
        if let Some(ca) = self.cc_activities.get_mut(&sid) {
            ca.counts = counts;
            ca.files_count = files_count;
        }
        self.reload_open_section();
        if changed {
            self.request_redraw();
        }
    }

    /// Rebuild an open section's content in place, keeping the live-tail
    /// contract of [`Self::reload_open_transcript`]: sticky-bottom follow and
    /// stable selection/scroll otherwise.
    fn reload_open_section(&mut self) {
        let Some(section) = self.active_cc_activity().and_then(|ca| match &ca.open {
            Some(CcNodeRef::Section(s)) => Some(*s),
            _ => None,
        }) else {
            return;
        };
        let (blocks, section_rows) = self.section_content(section);
        if let Some(ca) = self.active_cc_activity_mut() {
            let at_end = ca.selected + 1 >= ca.rows.len();
            ca.blocks = blocks;
            ca.section_rows = section_rows;
            ca.rebuild_rows();
            ca.refresh_search_matches();
            if (ca.follow && at_end) || ca.selected >= ca.rows.len() {
                ca.selected = ca.rows.len().saturating_sub(1);
            }
            ca.ensure_visible();
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

    /// Fold/unfold at the tree selection (Space): a workflow's agents, or the
    /// whole agents subtree when on the Agents section row.
    fn ca_toggle_workflow_fold(&mut self) {
        let Some(ca) = self.active_cc_activity_mut() else {
            return;
        };
        let sel = ca.tree_selected;
        match ca.tree.get(sel) {
            Some(CcTreeRow::Section(activity::Section::Agents)) => {
                ca.agents_folded = !ca.agents_folded;
            }
            Some(CcTreeRow::Workflow(wi)) | Some(CcTreeRow::WorkflowAgent(wi, _)) => {
                let Some(run_id) = ca.activity.workflows.get(*wi).map(|w| w.run_id.clone()) else {
                    return;
                };
                if !ca.collapsed_workflows.remove(&run_id) {
                    ca.collapsed_workflows.insert(run_id);
                }
            }
            _ => return,
        }
        ca.rebuild_tree();
        ca.tree_selected = sel.min(ca.tree.len().saturating_sub(1));
    }

    /// Jump the navigator straight to a section (the `1`–`6` keys, working
    /// from either activity pane).
    fn ca_jump_section(&mut self, section: activity::Section) {
        {
            let Some(ca) = self.active_cc_activity_mut() else {
                return;
            };
            let Some(row) = ca.section_row(section) else {
                return;
            };
            ca.tree_selected = row;
        }
        self.ca_sync_open();
    }

    /// The section a digit key addresses, in navigator order.
    fn section_for_digit(code: KeyCode) -> Option<activity::Section> {
        let KeyCode::Char(c) = code else { return None };
        let idx = c.to_digit(10)?.checked_sub(1)? as usize;
        activity::SECTIONS.get(idx).copied()
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
        // Minus the block border/footer, and the search bar row when open.
        let search = usize::from(
            self.active_cc_activity()
                .is_some_and(|ca| ca.search.is_some()),
        );
        (rows as usize).saturating_sub(3 + search)
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
        // Event blocks are foldable too: for them `collapsed_tools` membership
        // is read inverted (present = *expanded*), so Enter still toggles the
        // set — see the `TranscriptBlock::Event` arm in ui/cc_activity.rs.
        let is_foldable = matches!(
            ca.blocks.get(bi),
            Some(TranscriptBlock::ToolResult { .. })
                | Some(TranscriptBlock::ToolUse { .. })
                | Some(TranscriptBlock::Event(_))
        );
        if is_foldable && !ca.collapsed_tools.remove(&bi) {
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

    // ── Find in transcript (`/`) ─────────────────────────────────────────

    /// Open the find-in-transcript search, capturing keystrokes into the query.
    pub(crate) fn ca_start_search(&mut self) {
        if let Some(ca) = self.active_cc_activity_mut() {
            ca.search = Some(CcSearch {
                query: String::new(),
                editing: true,
                matches: Vec::new(),
            });
        }
    }

    /// Close the search entirely (drops the query + highlights).
    fn ca_close_search(&mut self) {
        if let Some(ca) = self.active_cc_activity_mut() {
            ca.search = None;
        }
    }

    /// `Tab`: commit the query — stop editing but keep the search open so the
    /// highlight stays and `n`/`N` step matches (an empty query just closes).
    fn ca_commit_search(&mut self) {
        let Some(ca) = self.active_cc_activity_mut() else {
            return;
        };
        match ca.search.as_mut() {
            Some(s) if s.query.trim().is_empty() => ca.search = None,
            Some(s) => s.editing = false,
            None => {}
        }
    }

    /// Edit the query (append a char / backspace), refresh matches, and jump the
    /// selection to the first match — the file viewer's incremental search.
    fn ca_edit_search(&mut self, push: Option<char>) {
        if let Some(ca) = self.active_cc_activity_mut() {
            if let Some(s) = ca.search.as_mut() {
                match push {
                    Some(c) => s.query.push(c),
                    None => {
                        s.query.pop();
                    }
                }
            }
            ca.refresh_search_matches();
            if let Some(&first) = ca.search.as_ref().and_then(|s| s.matches.first()) {
                ca.selected = first;
                ca.follow = ca.selected + 1 >= ca.rows.len();
                ca.ensure_visible();
            }
        }
    }

    /// Step to the next/previous match (`n`/`N`, `Enter`/arrows while typing),
    /// scanning from the current selection and wrapping — always relative to the
    /// cursor, like the file viewer's `next_match`.
    fn ca_search_step(&mut self, forward: bool) {
        let Some(ca) = self.active_cc_activity_mut() else {
            return;
        };
        let Some(s) = ca.search.as_ref() else {
            return;
        };
        if s.matches.is_empty() {
            return;
        }
        let next = if forward {
            s.matches
                .iter()
                .find(|&&i| i > ca.selected)
                .copied()
                .or_else(|| s.matches.first().copied())
        } else {
            s.matches
                .iter()
                .rev()
                .find(|&&i| i < ca.selected)
                .copied()
                .or_else(|| s.matches.last().copied())
        };
        if let Some(sel) = next {
            ca.selected = sel;
            ca.follow = ca.selected + 1 >= ca.rows.len();
            ca.ensure_visible();
        }
    }

    /// Key handling while the search query line is being typed (mirrors the file
    /// viewer / code review): `Enter`/`↓`/`Ctrl+N` next, `↑`/`Ctrl+P` previous
    /// (all stay in the input), `Tab` commits, `Esc` cancels, chars edit.
    fn handle_cc_search_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            KeyCode::Esc => self.ca_close_search(),
            KeyCode::Enter | KeyCode::Down => self.ca_search_step(true),
            KeyCode::Up => self.ca_search_step(false),
            KeyCode::Tab => self.ca_commit_search(),
            KeyCode::Char('n') if ctrl => self.ca_search_step(true),
            KeyCode::Char('p') if ctrl => self.ca_search_step(false),
            KeyCode::Backspace => self.ca_edit_search(None),
            KeyCode::Char(c) if !ctrl => self.ca_edit_search(Some(c)),
            _ => {}
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
        // While typing a search query, capture every (non-escape) key.
        let searching = self
            .active_cc_activity()
            .is_some_and(|ca| ca.search.as_ref().is_some_and(|s| s.editing));
        if searching {
            self.handle_cc_search_key(code, mods);
            return true;
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
            // Esc clears a committed search before closing the view, so a stray
            // `/` is one keystroke to undo.
            KeyCode::Esc
                if self
                    .active_cc_activity()
                    .is_some_and(|ca| ca.search.is_some()) =>
            {
                self.ca_close_search()
            }
            KeyCode::Esc => self.close_cc_activity(),
            KeyCode::Char('/') => self.ca_start_search(),
            KeyCode::Char('n') => self.ca_search_step(true),
            KeyCode::Char('N') => self.ca_search_step(false),
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
            code => {
                if let Some(section) = Self::section_for_digit(code) {
                    self.ca_jump_section(section);
                }
            }
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
            // Drop into the content pane (already previewing the selection).
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                self.focus = InputFocus::CcActivity;
            }
            code => {
                if let Some(section) = Self::section_for_digit(code) {
                    self.ca_jump_section(section);
                }
            }
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

    /// Build a minimal on-disk `projects/` + `jobs/` layout for the union tests:
    /// a foreground session with *no* tree of its own, and one daemon worker that
    /// ran a live workflow, with a matching job `state.json`.
    fn daemon_fixture(
        root: &Path,
        worker_id: &str,
        settings: &str,
        cwd: &str,
    ) -> (PathBuf, PathBuf) {
        let projects = root.join("projects");
        let jobs = root.join("jobs");
        let wf = projects
            .join("-repo-w")
            .join(worker_id)
            .join("subagents/workflows/wf_run");
        write(&wf.join("agent-a6d17521.jsonl"), "x\n");
        write(
            &wf.join("agent-a6d17521.meta.json"),
            r#"{"agentType":"workflow-subagent"}"#,
        );
        // A running fan entry (no doneAt) blocked on an approval.
        let state = format!(
            r#"{{"sessionId":"{worker_id}","daemonShort":"95c38d32","state":"working",
                "tempo":"blocked","needs":"approve Bash: ls","tokens":19030,"cwd":"{cwd}",
                "backend":"daemon","respawnFlags":["--settings","{settings}","--add-dir","{cwd}/"],
                "fan":[{{"id":"a6d17521","kind":"workflow","label":"review:correctness","group":"Review","startedAt":1}}]}}"#
        );
        write(&jobs.join("95c38d32/state.json"), &state);
        (projects, jobs)
    }

    #[test]
    fn collect_unions_daemon_worker_by_settings_and_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let hooks = "/cfg/hooks/claude.json";
        let repo = "/repo/friring";
        let worker = "95c38d32-39d4-4102-82df-24602ac3a2a0";
        let (projects, jobs) = daemon_fixture(tmp.path(), worker, hooks, repo);

        // Foreground session owns no tree of its own (the real bug); the worker's
        // cwd matches its candidate dir and the worker replays our hooks settings.
        let input = CcSessionInput {
            id: SessionId::default(),
            own_id: "0e6bcb32-fore".into(),
            candidate_dirs: vec![repo.into()],
            prior_sig: None,
        };
        let refresh =
            collect_cc_activity(projects, None, Some(jobs), Some(hooks.into()), vec![input]);

        let (_, sig, act) = &refresh.updates[0];
        assert_ne!(*sig, 0);
        let act = act.as_ref().expect("worker workflow merged in");
        assert_eq!(act.workflows.len(), 1, "daemon workflow attributed");
        let w = &act.workflows[0];
        assert_eq!(w.run_id, "wf_run");
        assert_eq!(w.status, CcRunStatus::Running);
        // Live enrichment straight from jobs/state.json (no completion record).
        assert_eq!(w.tempo.as_deref(), Some("blocked"));
        assert_eq!(w.needs.as_deref(), Some("approve Bash: ls"));
        assert_eq!(w.summary.as_ref().and_then(|s| s.total_tokens), Some(19030));
        let a = &w.agents[0];
        assert_eq!(a.label.as_deref(), Some("review:correctness")); // fan label
        assert_eq!(a.phase_title.as_deref(), Some("Review")); // fan group
        assert_eq!(a.state, CcAgentState::Active); // running: no doneAt
        assert_eq!(w.phases.len(), 1); // synthesized from the fan group
        assert_eq!(w.phases[0].title, "Review");
    }

    #[test]
    fn collect_skips_worker_with_foreign_settings_or_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let worker = "95c38d32-39d4-4102-82df-24602ac3a2a0";
        let (projects, jobs) = daemon_fixture(
            tmp.path(),
            worker,
            "/OTHER/hooks/claude.json",
            "/repo/friring",
        );

        // Same session, but our hooks settings differ from the worker's replayed
        // one → not attributed. (A different instance / a non-friring launch.)
        let input = CcSessionInput {
            id: SessionId::default(),
            own_id: "0e6bcb32-fore".into(),
            candidate_dirs: vec!["/repo/friring".into()],
            prior_sig: None,
        };
        let refresh = collect_cc_activity(
            projects.clone(),
            None,
            Some(jobs.clone()),
            Some("/cfg/hooks/claude.json".into()),
            vec![input],
        );
        // No owned tree at all → cleared/empty, not the worker's workflow.
        assert!(
            refresh.updates[0].2.is_none() || refresh.updates[0].2.as_ref().unwrap().is_empty()
        );

        // Settings match but the cwd is on a different repo → also not attributed.
        let (projects2, jobs2) = daemon_fixture(
            tmp.path().join("b").as_path(),
            worker,
            "/cfg/hooks/claude.json",
            "/some/other/repo",
        );
        let input2 = CcSessionInput {
            id: SessionId::default(),
            own_id: "fore2".into(),
            candidate_dirs: vec!["/repo/friring".into()],
            prior_sig: None,
        };
        let refresh2 = collect_cc_activity(
            projects2,
            None,
            Some(jobs2),
            Some("/cfg/hooks/claude.json".into()),
            vec![input2],
        );
        assert!(
            refresh2.updates[0].2.is_none() || refresh2.updates[0].2.as_ref().unwrap().is_empty()
        );
    }

    #[test]
    fn attribute_workers_matches_first_session_only() {
        // Two sessions on the same repo (the coarse non-worktree case): a matching
        // worker is assigned to exactly one, deterministically the first.
        let workers = vec![WorkerRef {
            session_id: "w1".into(),
            settings_path: Some("/h/claude.json".into()),
            cwd: Some("/repo".into()),
        }];
        let inputs = vec![
            CcSessionInput {
                id: SessionId::default(),
                own_id: "s0".into(),
                candidate_dirs: vec!["/repo".into()],
                prior_sig: None,
            },
            CcSessionInput {
                id: SessionId::default(),
                own_id: "s1".into(),
                candidate_dirs: vec!["/repo".into()],
                prior_sig: None,
            },
        ];
        let owners = attribute_workers(&workers, Some("/h/claude.json"), &inputs);
        assert_eq!(owners.get("w1"), Some(&0)); // first session wins, once
                                                // No hooks settings → no attribution at all.
        assert!(attribute_workers(&workers, None, &inputs).is_empty());
    }

    #[test]
    fn attribute_workers_exact_by_per_session_path() {
        // A per-session `--settings` path names the exact session — the cwd is
        // irrelevant, so it beats the coarse heuristic even when a *different*
        // session's cwd matches.
        let workers = vec![WorkerRef {
            session_id: "w1".into(),
            settings_path: Some("/h/sessions/FORE-ID.json".into()),
            cwd: Some("/some/unrelated/dir".into()),
        }];
        let inputs = vec![
            CcSessionInput {
                id: SessionId::default(),
                own_id: "OTHER".into(),
                candidate_dirs: vec!["/some/unrelated/dir".into()],
                prior_sig: None,
            },
            CcSessionInput {
                id: SessionId::default(),
                own_id: "FORE-ID".into(),
                candidate_dirs: vec![], // no cwd hint at all
                prior_sig: None,
            },
        ];
        let owners = attribute_workers(&workers, Some("/h/claude.json"), &inputs);
        assert_eq!(owners.get("w1"), Some(&1)); // exact id wins over cwd
                                                // A per-session path naming an absent session attributes to nobody (it is
                                                // authoritative — no cwd fall-through to a wrong session).
        let orphan = vec![WorkerRef {
            session_id: "w2".into(),
            settings_path: Some("/h/sessions/GONE.json".into()),
            cwd: Some("/some/unrelated/dir".into()),
        }];
        assert!(attribute_workers(&orphan, Some("/h/claude.json"), &inputs).is_empty());
    }

    #[test]
    fn normalize_dir_trims_trailing_slash() {
        assert_eq!(normalize_dir("/repo/x/"), "/repo/x");
        assert_eq!(normalize_dir("/repo/x"), "/repo/x");
        assert_eq!(normalize_dir("/"), "/"); // never collapses to empty
    }

    #[test]
    fn transcript_search_matches_blocks_case_insensitively() {
        let mut ca = CcActivityState::new(CcActivity::default());
        ca.open = Some(CcNodeRef::Subagent("x".into()));
        ca.blocks = vec![
            TranscriptBlock::Text("hello World".into()),
            TranscriptBlock::Thinking("plan the WORLD".into()),
            TranscriptBlock::ToolUse {
                name: "Bash".into(),
                input: "ls world".into(),
            },
            TranscriptBlock::ToolResult {
                content: "nothing here".into(),
                is_error: false,
            },
        ];
        ca.rebuild_rows();
        assert_eq!(ca.rows.len(), 4);
        // Matches the text, thinking, and the tool-use input — not the result.
        assert_eq!(ca.search_matches("world"), vec![0, 1, 2]);
        // The tool name is searchable (block_search_text = "Bash ls world").
        assert_eq!(ca.search_matches("bash"), vec![2]);
        assert!(ca.search_matches("absent").is_empty());
        assert!(ca.search_matches("   ").is_empty()); // whitespace → no matches

        // refresh_search_matches populates the open search's set.
        ca.search = Some(CcSearch {
            query: "world".into(),
            editing: false,
            matches: Vec::new(),
        });
        ca.refresh_search_matches();
        assert_eq!(ca.search.as_ref().unwrap().matches, vec![0, 1, 2]);
        // Info rows (overview scaffolding) are not searchable.
        assert_eq!(ca.row_text(&CcRow::Info("Phases".into())), None);
    }
}
