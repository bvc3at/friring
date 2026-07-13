//! Claude Code workflow + subagent activity — pure data model and parsers.
//!
//! friring surfaces what happens *inside* a running Claude Code session: the
//! Task subagents and multi-agent workflows it spawns, and the actual transcript
//! text (thinking / tool calls / output) of each. Claude Code persists all of it
//! as flat JSONL under
//! `~/.claude/projects/<slug>/<agent_session_id>/subagents/`:
//!
//! - `subagents/agent-<id>.jsonl` (+ `agent-<id>.meta.json`) — a standalone
//!   `Task`-tool subagent.
//! - `subagents/workflows/wf_<id>/` — one workflow run: a `journal.jsonl` of
//!   `started`/`result` edges, one `agent-<id>.jsonl` (+ `.meta.json`) per
//!   spawned agent, and a sibling `workflows/wf_<id>.json` **completion record**
//!   (top-level `phases[]` + `workflowProgress[]` grid) written once the run
//!   finishes.
//!
//! The sibling **top-level conversation transcript**
//! (`projects/<slug>/<agent_session_id>.jsonl`) shares the line format;
//! [`parse_conversation_head`] reads its head for the identity metadata
//! (cwd / branch / title) the conversation-import picker lists.
//!
//! This module is the **pure** layer (arch rule `ui ← session`, no filesystem):
//! it defines the [`CcActivity`] index the app polls onto `SessionInfo`, the
//! [`TranscriptBlock`] stream the transcript view renders, and defensive parsers
//! over the (undocumented, version-specific) on-disk JSON. Everything is
//! `serde_json::Value`-based and skips shapes it doesn't recognise, so a Claude
//! Code layout change degrades to a partial tree rather than an error. The
//! filesystem walk that *calls* these parsers lives in the app layer.
//!
//! Verified against Claude Code v2.1.201.

use std::collections::HashMap;
use std::path::PathBuf;

/// Live/finished state of a single agent (workflow agent or standalone
/// subagent). `Error` is only known for workflow agents (from the completion
/// grid); standalone subagents are `Active` while their transcript is freshly
/// appended, else `Done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcAgentState {
    Active,
    Done,
    Error,
}

/// Whether a workflow run is still in flight. A run is `Completed` once its
/// sibling `workflows/wf_<id>.json` record exists (written at completion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcRunStatus {
    Running,
    Completed,
}

/// One phase of a workflow (from the completion record's `phases[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcPhase {
    pub index: u64,
    pub title: String,
}

/// One agent leaf in the activity tree: a workflow-spawned agent or a
/// standalone `Task` subagent. The heavy transcript is *not* held here — it is
/// parsed on demand from [`transcript_path`](Self::transcript_path) when the
/// user selects the node.
#[derive(Debug, Clone, PartialEq)]
pub struct CcAgent {
    /// Claude Code agent id (e.g. `a00bf4eda17e2f92f`).
    pub agent_id: String,
    /// Absolute path to this agent's `agent-<id>.jsonl` transcript.
    pub transcript_path: PathBuf,
    /// `agentType` from the sidecar `.meta.json` (`workflow-subagent`,
    /// `Explore`, `general-purpose`, …). Empty when the meta is missing.
    pub agent_type: String,
    /// `description` from a standalone subagent's `.meta.json` (workflow agents
    /// omit it).
    pub description: Option<String>,
    /// Human label from the workflow completion grid (e.g. `fs-ground-truth`),
    /// richer than `agent_type`. `None` while the run is live.
    pub label: Option<String>,
    /// Phase this agent belongs to (from the completion grid), if any.
    pub phase_title: Option<String>,
    pub state: CcAgentState,
    /// Transcript file mtime in nanoseconds since the epoch — drives the
    /// mtime-gated re-read and active-agent detection.
    pub mtime_ns: u128,
    /// Transcript file length in bytes.
    pub size: u64,
    // --- Enrichment from the completion grid (all `None` while live) ---
    pub tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub last_tool: Option<String>,
    pub model: Option<String>,
}

/// Roll-up totals for a finished workflow (from the completion record).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CcWorkflowSummary {
    pub status: Option<String>,
    pub total_tokens: Option<u64>,
    pub total_tool_calls: Option<u64>,
    pub duration_ms: Option<u64>,
    pub default_model: Option<String>,
    /// Narrator/stall lines the workflow emitted (e.g. retry notices).
    pub logs: Vec<String>,
}

/// One workflow run and its spawned agents.
#[derive(Debug, Clone, PartialEq)]
pub struct CcWorkflow {
    /// Run id (`wf_d2b69900-2d6`).
    pub run_id: String,
    /// Workflow name (from the completion record / script), if known.
    pub name: Option<String>,
    /// Absolute path to the `wf_<id>/` run directory.
    pub dir: PathBuf,
    pub status: CcRunStatus,
    pub phases: Vec<CcPhase>,
    pub agents: Vec<CcAgent>,
    pub summary: Option<CcWorkflowSummary>,
    /// Live pace of a **background/daemon** run (from `jobs/<short>/state.json`),
    /// e.g. `blocked` while awaiting an approval. `None` for in-process runs and
    /// for completed runs read from the completion record.
    pub tempo: Option<String>,
    /// What a `blocked` background run is waiting on (the approval prompt text),
    /// if any.
    pub needs: Option<String>,
}

/// The per-session activity index polled onto `SessionInfo.cc_activity`. It is
/// an *index* only (no transcript bodies), refreshed off the UI thread and cheap
/// to rebuild.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CcActivity {
    pub workflows: Vec<CcWorkflow>,
    /// Standalone `Task`-tool subagents (not nested under a workflow).
    pub subagents: Vec<CcAgent>,
}

impl CcActivity {
    /// No workflows and no standalone subagents.
    pub fn is_empty(&self) -> bool {
        self.workflows.is_empty() && self.subagents.is_empty()
    }

    /// Total agent leaves across all workflows plus standalone subagents.
    pub fn agent_count(&self) -> usize {
        self.workflows.iter().map(|w| w.agents.len()).sum::<usize>() + self.subagents.len()
    }

    /// Whether any agent anywhere is currently `Active`.
    pub fn any_active(&self) -> bool {
        self.subagents
            .iter()
            .any(|a| a.state == CcAgentState::Active)
            || self
                .workflows
                .iter()
                .any(|w| w.agents.iter().any(|a| a.state == CcAgentState::Active))
    }

    /// Find a workflow by run id.
    pub fn workflow(&self, run_id: &str) -> Option<&CcWorkflow> {
        self.workflows.iter().find(|w| w.run_id == run_id)
    }

    /// Find a workflow-spawned agent by (run id, agent id).
    pub fn workflow_agent(&self, run_id: &str, agent_id: &str) -> Option<&CcAgent> {
        self.workflow(run_id)?
            .agents
            .iter()
            .find(|a| a.agent_id == agent_id)
    }

    /// Find a standalone subagent by agent id.
    pub fn subagent(&self, agent_id: &str) -> Option<&CcAgent> {
        self.subagents.iter().find(|a| a.agent_id == agent_id)
    }
}

/// One rendered unit of an agent transcript. A block is one *logical* row in the
/// transcript view; multi-line bodies (thinking, tool output) wrap into several
/// visual rows sharing the logical index.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptBlock {
    /// The agent's initial task prompt (first `user` string message).
    Prompt(String),
    /// An assistant reasoning block.
    Thinking(String),
    /// An assistant text block.
    Text(String),
    /// A tool call: the tool name plus a human-readable rendering of its input
    /// (the raw command for `Bash`, else pretty JSON).
    ToolUse { name: String, input: String },
    /// A tool result body (already normalised to text).
    ToolResult { content: String, is_error: bool },
    /// One normalized activity event — the F9 section views (timeline /
    /// commands / web) render event streams through the same block engine as
    /// transcripts, so folds, find, and wrap behave identically.
    Event(super::activity::ActivityEvent),
}

// -------------------------------------------------------------------------
// Parsers — all pure, all defensive (`serde_json::Value`, skip on mismatch).
// -------------------------------------------------------------------------

/// The sidecar `agent-<id>.meta.json`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CcMeta {
    pub agent_type: String,
    pub description: Option<String>,
    pub tool_use_id: Option<String>,
}

/// Parse an `agent-<id>.meta.json`. Missing/garbled input yields a default
/// (empty `agent_type`) rather than an error.
pub fn parse_meta(s: &str) -> CcMeta {
    let v: serde_json::Value = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => return CcMeta::default(),
    };
    CcMeta {
        agent_type: str_field(&v, "agentType").unwrap_or_default(),
        description: str_field(&v, "description"),
        tool_use_id: str_field(&v, "toolUseId"),
    }
}

/// Per-agent state derived from a workflow's `journal.jsonl`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalEntry {
    pub started: bool,
    pub has_result: bool,
}

/// Parse a workflow `journal.jsonl` into per-agent `started`/`result` flags.
/// Lines are `{"type":"started"|"result","agentId":"…"}`; unknown lines are
/// skipped. An agent that has `started` but no `result` is still running.
pub fn parse_journal(s: &str) -> HashMap<String, JournalEntry> {
    let mut map: HashMap<String, JournalEntry> = HashMap::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(id) = str_field(&v, "agentId") else {
            continue;
        };
        let entry = map.entry(id).or_default();
        match str_field(&v, "type").as_deref() {
            Some("started") => entry.started = true,
            Some("result") => entry.has_result = true,
            _ => {}
        }
    }
    map
}

/// The subset of a workflow completion record (`workflows/wf_<id>.json`) the
/// activity view needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkflowCompletion {
    pub workflow_name: Option<String>,
    pub summary: CcWorkflowSummary,
    pub phases: Vec<CcPhase>,
    /// `agentId` → its grid row (label / phase / state / metrics).
    pub agents: HashMap<String, AgentProgress>,
}

/// One `workflow_agent` row from the completion record's `workflowProgress[]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentProgress {
    pub label: Option<String>,
    pub phase_title: Option<String>,
    pub state: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub last_tool: Option<String>,
}

/// Parse a `workflows/wf_<id>.json` completion record. The grid
/// (`workflowProgress[]`) interleaves `workflow_phase` markers with
/// `workflow_agent` rows; both `phases[]` and the grid are read defensively.
///
/// NB (verified across CC versions): the grid can live either at the top level
/// or nested under `.result` — both are checked.
pub fn parse_workflow_completion(s: &str) -> Option<WorkflowCompletion> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    // The rich fields sit at the top level in v2.1.201, but older builds nested
    // them under `.result`; prefer whichever actually carries `workflowProgress`.
    let root = if v.get("workflowProgress").is_some() {
        &v
    } else {
        v.get("result")
            .filter(|r| r.get("workflowProgress").is_some())
            .unwrap_or(&v)
    };

    let mut out = WorkflowCompletion {
        workflow_name: str_field(root, "workflowName"),
        summary: CcWorkflowSummary {
            status: str_field(root, "status"),
            total_tokens: u64_field(root, "totalTokens"),
            total_tool_calls: u64_field(root, "totalToolCalls"),
            duration_ms: u64_field(root, "durationMs"),
            default_model: str_field(root, "defaultModel"),
            logs: root
                .get("logs")
                .and_then(|l| l.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        },
        phases: root
            .get("phases")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .enumerate()
                    .filter_map(|(i, p)| {
                        str_field(p, "title").map(|title| CcPhase {
                            index: u64_field(p, "index").unwrap_or(i as u64 + 1),
                            title,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        agents: HashMap::new(),
    };

    if let Some(grid) = root.get("workflowProgress").and_then(|g| g.as_array()) {
        for row in grid {
            if str_field(row, "type").as_deref() != Some("workflow_agent") {
                continue;
            }
            let Some(id) = str_field(row, "agentId") else {
                continue;
            };
            out.agents.insert(
                id,
                AgentProgress {
                    label: str_field(row, "label"),
                    phase_title: str_field(row, "phaseTitle"),
                    state: str_field(row, "state"),
                    model: str_field(row, "model"),
                    tokens: u64_field(row, "tokens"),
                    tool_calls: u64_field(row, "toolCalls"),
                    last_tool: str_field(row, "lastToolName"),
                },
            );
        }
    }
    Some(out)
}

// -------------------------------------------------------------------------
// Background / daemon workers — attribution + live status.
//
// Claude Code can dispatch a whole session as a *detached* background worker
// (the fleet/daemon path): a claimed spare process gets its **own** new session
// id and writes its subagents/workflows under it — not under the launching
// friring session's id. There is no parent→child lineage on disk, so a worker is
// correlated back to the session that launched it via the one thing the daemon
// **replays**: the `--settings <hooks>/claude.json` flag captured from the origin
// session's CLI args (plus a cwd match). Two on-disk homes carry the state:
//
// - `~/.claude/daemon/roster.json` — the live worker registry (may prune settled
//   workers), parsed by [`parse_roster`].
// - `~/.claude/jobs/<short>/state.json` — per-job state that **persists after the
//   run settles** (so a finished background workflow stays attributable) and
//   carries the live `fan[]` agent grid, `tempo`, `needs`, and token total,
//   parsed by [`parse_job_state`]. Its `fan[].id` matches the on-disk
//   `agent-<id>.jsonl` under the run dir, so it enriches a live workflow before
//   any completion record exists.
//
// Verified against Claude Code v2.1.201–2.1.204 (undocumented, version-specific).
// -------------------------------------------------------------------------

/// A background/daemon worker from `roster.json`: its own session id (the key to
/// its `subagents/` dir), the replayed `--settings` path + `cwd` used to
/// attribute it to a friring session, and the claim `source` (`slash`/`fleet`/
/// `spare`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CcWorker {
    pub session_id: String,
    pub short: String,
    pub settings_path: Option<String>,
    pub cwd: Option<String>,
    pub source: Option<String>,
}

/// One agent cell of a daemon job's live `fan[]` grid.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CcFanEntry {
    /// Matches the on-disk `agent-<id>.jsonl` under the run dir.
    pub id: String,
    pub kind: String,
    pub label: String,
    /// Phase group (`Review`/`Verify`/…) — the live equivalent of a phase title.
    pub group: Option<String>,
    pub failed: bool,
    /// Terminal (carries a `doneAt`), whether success or failure.
    pub done: bool,
}

/// The subset of `jobs/<short>/state.json` the activity view needs: the worker's
/// session id (its `subagents/` dir key), attribution fields (`settings_path`,
/// `cwd`), the live status (`state`/`tempo`/`needs`/`tokens`), and the `fan[]`
/// agent grid.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CcJobState {
    pub session_id: String,
    pub daemon_short: Option<String>,
    pub state: Option<String>,
    pub tempo: Option<String>,
    pub needs: Option<String>,
    pub tokens: Option<u64>,
    pub cwd: Option<String>,
    pub settings_path: Option<String>,
    pub fan: Vec<CcFanEntry>,
}

impl CcJobState {
    /// Index the `fan[]` grid by agent id for per-agent enrichment lookups.
    pub fn fan_by_id(&self) -> HashMap<&str, &CcFanEntry> {
        self.fan.iter().map(|e| (e.id.as_str(), e)).collect()
    }
}

/// Extract the value of a `--settings` flag from a launch-arg vector, handling
/// both the split (`--settings`, `<path>`) and joined (`--settings=<path>`)
/// forms. Returns the first occurrence.
pub fn settings_from_args(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--settings" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--settings=") {
            return Some(v.to_string());
        }
    }
    None
}

/// Parse `~/.claude/daemon/roster.json` into its workers. Each worker's
/// `--settings` path is read from `dispatch.launch.args`,
/// `dispatch.launch.flagArgs`, or `dispatch.respawnFlags` (whichever carries it).
/// Defensive: unknown shapes yield an empty list rather than an error.
pub fn parse_roster(s: &str) -> Vec<CcWorker> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else {
        return Vec::new();
    };
    let Some(workers) = v.get("workers").and_then(|w| w.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(workers.len());
    for (short, w) in workers {
        let Some(session_id) = str_field(w, "sessionId") else {
            continue;
        };
        let dispatch = w.get("dispatch");
        out.push(CcWorker {
            session_id,
            short: short.clone(),
            settings_path: dispatch.and_then(worker_settings_path),
            cwd: str_field(w, "cwd").or_else(|| dispatch.and_then(|d| str_field(d, "cwd"))),
            source: dispatch.and_then(|d| str_field(d, "source")),
        });
    }
    out
}

/// The `--settings` path a `dispatch` replays, checked across the arg vectors it
/// can appear in (`launch.args` for a prompt spawn, `launch.flagArgs` for a
/// resume, or the `respawnFlags` fallback).
fn worker_settings_path(dispatch: &serde_json::Value) -> Option<String> {
    let launch = dispatch.get("launch");
    for key in ["args", "flagArgs"] {
        if let Some(arr) = launch.and_then(|l| l.get(key)).and_then(|a| a.as_array()) {
            if let Some(p) = settings_from_args(&string_vec(arr)) {
                return Some(p);
            }
        }
    }
    dispatch
        .get("respawnFlags")
        .and_then(|a| a.as_array())
        .and_then(|arr| settings_from_args(&string_vec(arr)))
}

fn string_vec(arr: &[serde_json::Value]) -> Vec<String> {
    arr.iter()
        .filter_map(|x| x.as_str().map(String::from))
        .collect()
}

/// Parse a `jobs/<short>/state.json`. Missing/garbled input, or a record with no
/// `sessionId`, yields `None`.
pub fn parse_job_state(s: &str) -> Option<CcJobState> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let session_id = str_field(&v, "sessionId")?;
    let settings_path = v
        .get("respawnFlags")
        .and_then(|a| a.as_array())
        .and_then(|arr| settings_from_args(&string_vec(arr)));
    let fan = v
        .get("fan")
        .and_then(|f| f.as_array())
        .map(|arr| arr.iter().filter_map(parse_fan_entry).collect())
        .unwrap_or_default();
    Some(CcJobState {
        session_id,
        daemon_short: str_field(&v, "daemonShort"),
        state: str_field(&v, "state"),
        tempo: str_field(&v, "tempo"),
        needs: str_field(&v, "needs"),
        tokens: u64_field(&v, "tokens"),
        cwd: str_field(&v, "cwd"),
        settings_path,
        fan,
    })
}

fn parse_fan_entry(v: &serde_json::Value) -> Option<CcFanEntry> {
    let id = str_field(v, "id")?;
    Some(CcFanEntry {
        id,
        kind: str_field(v, "kind").unwrap_or_default(),
        label: str_field(v, "label").unwrap_or_default(),
        group: str_field(v, "group"),
        failed: v.get("failed").and_then(|x| x.as_bool()).unwrap_or(false),
        done: v.get("doneAt").is_some_and(|x| !x.is_null()),
    })
}

/// Identity metadata of one top-level conversation transcript
/// (`projects/<slug>/<session-id>.jsonl`), extracted from the file's head by
/// [`parse_conversation_head`]. Feeds the conversation-import picker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CcConversationMeta {
    /// The conversation's original working directory (the `cwd` field the CLI
    /// stamps on user/assistant/system lines).
    pub cwd: Option<String>,
    /// `gitBranch` from the same lines, when the cwd was a git checkout.
    pub git_branch: Option<String>,
    /// Best-available human title: a `summary` line if one appears in the head
    /// (Claude Code's own conversation summary, present on compacted/continued
    /// files), else the first real typed user prompt.
    pub title: Option<String>,
}

/// True for user-line content that is harness plumbing rather than something
/// the user typed. Skipped when picking a conversation title. Every harness
/// envelope opens with a kebab-case tag (`<command-name>`, `<system-reminder>`,
/// `<task-notification>`, `<local-command-stdout>`, …) — people don't type
/// those — plus the local-command caveat wrapper.
fn is_meta_prompt(text: &str) -> bool {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix('<') {
        if let Some((tag, _)) = rest.split_once('>') {
            if !tag.is_empty()
                && tag
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-' || c == '_')
            {
                return true;
            }
        }
    }
    t.starts_with("Caveat: The messages below were generated by the user")
}

/// The first typed-prompt text of a `user` line, if it has one. Content is a
/// plain string on old lines and an array of blocks on new ones; either way,
/// meta/sidechain/compact-summary lines never yield a title. Shared with the
/// activity provider (`activity::claude`), which derives a session title the
/// same way.
pub(crate) fn user_prompt_text(v: &serde_json::Value) -> Option<String> {
    if v.get("isSidechain").and_then(|x| x.as_bool()) == Some(true)
        || v.get("isMeta").and_then(|x| x.as_bool()) == Some(true)
        || v.get("isCompactSummary").and_then(|x| x.as_bool()) == Some(true)
    {
        return None;
    }
    let content = v.pointer("/message/content")?;
    if let Some(s) = content.as_str() {
        return (!is_meta_prompt(s) && !s.trim().is_empty()).then(|| s.to_string());
    }
    content.as_array()?.iter().find_map(|block| {
        let text = str_field(block, "text")?;
        (str_field(block, "type").as_deref() == Some("text")
            && !is_meta_prompt(&text)
            && !text.trim().is_empty())
        .then_some(text)
    })
}

/// Extract a conversation's identity metadata from the head of its top-level
/// `<session-id>.jsonl`. Callers pass only the first chunk of the file (these
/// transcripts can be huge and `cwd`/title appear within the first real
/// entries); a truncated final line is skipped like any malformed line. Each
/// field keeps its first occurrence, so the original cwd wins over any later
/// resumed-elsewhere rewrite.
pub fn parse_conversation_head(s: &str) -> CcConversationMeta {
    let mut meta = CcConversationMeta::default();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if meta.cwd.is_none() {
            meta.cwd = str_field(&v, "cwd");
        }
        if meta.git_branch.is_none() {
            meta.git_branch = str_field(&v, "gitBranch").filter(|b| !b.is_empty());
        }
        if meta.title.is_none() {
            meta.title = match str_field(&v, "type").as_deref() {
                Some("summary") => str_field(&v, "summary"),
                Some("user") => user_prompt_text(&v),
                _ => None,
            };
        }
        if meta.cwd.is_some() && meta.title.is_some() && meta.git_branch.is_some() {
            break;
        }
    }
    meta
}

/// Parse an `agent-<id>.jsonl` transcript into the block stream the view
/// renders. Each line is one conversation entry; malformed/partial lines (a
/// tail read racing a live append) are skipped, so a live transcript never
/// errors.
pub fn parse_transcript(s: &str) -> Vec<TranscriptBlock> {
    let mut blocks = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match str_field(&v, "type").as_deref() {
            Some("assistant") => push_assistant_blocks(&v, &mut blocks),
            Some("user") => push_user_blocks(&v, &mut blocks),
            _ => {}
        }
    }
    blocks
}

fn push_assistant_blocks(entry: &serde_json::Value, out: &mut Vec<TranscriptBlock>) {
    let Some(content) = entry.pointer("/message/content").and_then(|c| c.as_array()) else {
        return;
    };
    for block in content {
        match str_field(block, "type").as_deref() {
            Some("thinking") => {
                if let Some(t) = str_field(block, "thinking") {
                    out.push(TranscriptBlock::Thinking(t));
                }
            }
            Some("text") => {
                if let Some(t) = str_field(block, "text") {
                    out.push(TranscriptBlock::Text(t));
                }
            }
            Some("tool_use") => {
                let name = str_field(block, "name").unwrap_or_else(|| "tool".to_string());
                let input = render_tool_input(&name, block.get("input"));
                out.push(TranscriptBlock::ToolUse { name, input });
            }
            _ => {}
        }
    }
}

fn push_user_blocks(entry: &serde_json::Value, out: &mut Vec<TranscriptBlock>) {
    let Some(content) = entry.pointer("/message/content") else {
        return;
    };
    // A plain string is the agent's initial task prompt.
    if let Some(s) = content.as_str() {
        out.push(TranscriptBlock::Prompt(s.to_string()));
        return;
    }
    // Otherwise an array of blocks, primarily `tool_result`s.
    if let Some(arr) = content.as_array() {
        for block in arr {
            if str_field(block, "type").as_deref() == Some("tool_result") {
                out.push(TranscriptBlock::ToolResult {
                    content: normalize_tool_result(block.get("content")),
                    is_error: block
                        .get("is_error")
                        .and_then(|e| e.as_bool())
                        .unwrap_or(false),
                });
            }
        }
    }
}

/// Render a tool call's input for display: the raw `command` for `Bash` (what
/// you actually want to read), else pretty-printed JSON. Robust to any tool.
fn render_tool_input(name: &str, input: Option<&serde_json::Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    if name == "Bash" {
        if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
            return cmd.to_string();
        }
    }
    if let Some(s) = input.as_str() {
        return s.to_string();
    }
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// A `tool_result.content` is a string or an array of `{type:"text",text}`
/// blocks; normalise either to a single string. Shared with the activity
/// provider (`activity::claude`), which extracts result heads from the same
/// block shape.
pub(crate) fn normalize_tool_result(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(v) if v.is_string() => v.as_str().unwrap_or_default().to_string(),
        Some(v) if v.is_array() => v
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|b| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| b.as_str())
                    .map(String::from)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn u64_field(v: &serde_json::Value, key: &str) -> Option<u64> {
    v.get(key).and_then(|x| x.as_u64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meta_reads_workflow_and_task_shapes() {
        let wf = parse_meta(r#"{"agentType":"workflow-subagent","spawnDepth":1}"#);
        assert_eq!(wf.agent_type, "workflow-subagent");
        assert_eq!(wf.description, None);

        let task = parse_meta(
            r#"{"agentType":"Explore","description":"Overview backend","toolUseId":"toolu_01Ux","spawnDepth":1}"#,
        );
        assert_eq!(task.agent_type, "Explore");
        assert_eq!(task.description.as_deref(), Some("Overview backend"));
        assert_eq!(task.tool_use_id.as_deref(), Some("toolu_01Ux"));
    }

    #[test]
    fn parse_meta_is_defensive() {
        assert_eq!(parse_meta("not json").agent_type, "");
        assert_eq!(parse_meta("{}").agent_type, "");
    }

    #[test]
    fn parse_journal_tracks_started_and_result() {
        let j = r#"
{"type":"started","agentId":"a1"}
{"type":"started","agentId":"a2"}
{"type":"result","agentId":"a1","result":"done text"}
garbage line that is not json
{"type":"weird","agentId":"a3"}
"#;
        let m = parse_journal(j);
        assert!(m["a1"].started && m["a1"].has_result); // finished
        assert!(m["a2"].started && !m["a2"].has_result); // still running
        assert!(!m["a3"].started && !m["a3"].has_result); // unknown edge type
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn parse_completion_reads_top_level_grid() {
        let json = r#"{
          "workflowName":"demo",
          "status":"completed",
          "totalTokens":1000,
          "totalToolCalls":12,
          "durationMs":5000,
          "defaultModel":"claude-opus-4-8[1m]",
          "logs":["[stall] agent x retrying"],
          "phases":[{"index":1,"title":"Investigate"},{"index":2,"title":"Synthesize"}],
          "workflowProgress":[
            {"type":"workflow_phase","index":1,"title":"Investigate"},
            {"type":"workflow_agent","agentId":"aec6","label":"fs-ground-truth","phaseTitle":"Investigate","state":"done","model":"claude-opus-4-8[1m]","tokens":161885,"toolCalls":37,"lastToolName":"Bash"},
            {"type":"workflow_agent","agentId":"a2e8","label":"cc-workflows","phaseTitle":"Investigate","state":"error","tokens":1,"toolCalls":0}
          ]
        }"#;
        let c = parse_workflow_completion(json).expect("parses");
        assert_eq!(c.workflow_name.as_deref(), Some("demo"));
        assert_eq!(c.summary.total_tokens, Some(1000));
        assert_eq!(c.summary.logs.len(), 1);
        assert_eq!(
            c.phases,
            vec![
                CcPhase {
                    index: 1,
                    title: "Investigate".into()
                },
                CcPhase {
                    index: 2,
                    title: "Synthesize".into()
                },
            ]
        );
        let aec6 = &c.agents["aec6"];
        assert_eq!(aec6.label.as_deref(), Some("fs-ground-truth"));
        assert_eq!(aec6.phase_title.as_deref(), Some("Investigate"));
        assert_eq!(aec6.state.as_deref(), Some("done"));
        assert_eq!(aec6.tokens, Some(161885));
        assert_eq!(aec6.last_tool.as_deref(), Some("Bash"));
        assert_eq!(c.agents["a2e8"].state.as_deref(), Some("error"));
    }

    #[test]
    fn parse_completion_reads_nested_result_grid() {
        // Older shape: the grid lives under `.result`.
        let json = r#"{"runId":"wf_x","result":{
          "workflowName":"nested","status":"completed",
          "workflowProgress":[
            {"type":"workflow_agent","agentId":"z1","label":"only","state":"done"}
          ]}}"#;
        let c = parse_workflow_completion(json).expect("parses");
        assert_eq!(c.workflow_name.as_deref(), Some("nested"));
        assert_eq!(c.agents["z1"].label.as_deref(), Some("only"));
    }

    #[test]
    fn parse_transcript_extracts_all_block_kinds() {
        let jsonl = r#"
{"type":"user","message":{"role":"user","content":"Your task: do the thing"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me plan"},{"type":"text","text":"Working on it"},{"type":"tool_use","name":"Bash","id":"toolu_1","input":{"command":"ls -la","description":"list"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":false,"content":"total 0"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","id":"toolu_2","input":{"file_path":"/x"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","is_error":true,"content":[{"type":"text","text":"boom"}]}]}}
{"type":"system","subtype":"turn_duration"}
"#;
        let b = parse_transcript(jsonl);
        assert_eq!(
            b[0],
            TranscriptBlock::Prompt("Your task: do the thing".into())
        );
        assert_eq!(b[1], TranscriptBlock::Thinking("Let me plan".into()));
        assert_eq!(b[2], TranscriptBlock::Text("Working on it".into()));
        assert_eq!(
            b[3],
            TranscriptBlock::ToolUse {
                name: "Bash".into(),
                input: "ls -la".into()
            }
        );
        assert_eq!(
            b[4],
            TranscriptBlock::ToolResult {
                content: "total 0".into(),
                is_error: false
            }
        );
        // Non-Bash tool renders pretty JSON of its input.
        let TranscriptBlock::ToolUse { name, input } = &b[5] else {
            panic!("expected tool_use");
        };
        assert_eq!(name, "Read");
        assert!(input.contains("file_path") && input.contains("/x"));
        // Array-form tool_result content is flattened to text; error flag kept.
        assert_eq!(
            b[6],
            TranscriptBlock::ToolResult {
                content: "boom".into(),
                is_error: true
            }
        );
        // The `system` line contributes nothing.
        assert_eq!(b.len(), 7);
    }

    #[test]
    fn parse_transcript_skips_partial_trailing_line() {
        // A tail read racing a live append can end mid-line — must not panic.
        let jsonl = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n{\"type\":\"assist";
        let b = parse_transcript(jsonl);
        assert_eq!(b, vec![TranscriptBlock::Text("ok".into())]);
    }

    #[test]
    fn conversation_head_reads_cwd_branch_and_first_typed_prompt() {
        // Real head shape: id-only preamble lines first, then the first user
        // line carrying cwd/gitBranch. Meta lines (slash-command envelopes,
        // caveat wrappers) must not become the title.
        let jsonl = concat!(
            r#"{"type":"mode","mode":"normal","sessionId":"s1"}"#,
            "\n",
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<command-name>/clear</command-name>"},"cwd":"/repo/a","gitBranch":"main"}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"Fix the flaky test"},"cwd":"/repo/a","gitBranch":"main"}"#,
            "\n",
        );
        let m = parse_conversation_head(jsonl);
        assert_eq!(m.cwd.as_deref(), Some("/repo/a"));
        assert_eq!(m.git_branch.as_deref(), Some("main"));
        assert_eq!(m.title.as_deref(), Some("Fix the flaky test"));
    }

    #[test]
    fn conversation_head_prefers_summary_and_skips_sidechain_and_blocks() {
        // A `summary` line (compacted/continued files) beats the first prompt;
        // sidechain lines and non-text blocks in array content are skipped.
        let jsonl = concat!(
            r#"{"type":"summary","summary":"Ship the release","leafUuid":"x"}"#,
            "\n",
            r#"{"type":"user","isSidechain":true,"message":{"content":"subagent task"},"cwd":"/repo/side"}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"out"},{"type":"text","text":"typed prompt"}]},"cwd":"/repo/b"}"#,
            "\n",
        );
        let m = parse_conversation_head(jsonl);
        assert_eq!(m.title.as_deref(), Some("Ship the release"));
        // First cwd wins even on a sidechain line — it is still the real dir.
        assert_eq!(m.cwd.as_deref(), Some("/repo/side"));

        let no_summary = parse_conversation_head(
            r#"{"type":"user","message":{"content":[{"type":"text","text":"typed prompt"}]},"cwd":"/repo/b"}"#,
        );
        assert_eq!(no_summary.title.as_deref(), Some("typed prompt"));
    }

    #[test]
    fn conversation_head_survives_garbage_and_empty_input() {
        assert_eq!(parse_conversation_head(""), CcConversationMeta::default());
        let m = parse_conversation_head("not json\n{\"type\":\"user\",\"mess");
        assert_eq!(m, CcConversationMeta::default());
    }

    #[test]
    fn meta_prompt_detects_harness_envelopes_not_user_text() {
        assert!(is_meta_prompt("<task-notification>...</task-notification>"));
        assert!(is_meta_prompt("<system-reminder>x</system-reminder>"));
        assert!(is_meta_prompt("  <command-name>/clear</command-name>"));
        assert!(is_meta_prompt(
            "Caveat: The messages below were generated by the user while running local commands"
        ));
        assert!(!is_meta_prompt("fix the parser"));
        // Angle brackets in real prose don't false-positive: not a kebab tag.
        assert!(!is_meta_prompt("<T> as a generic parameter breaks"));
        assert!(!is_meta_prompt("< 5% of requests fail"));
    }

    #[test]
    fn cc_activity_helpers() {
        let mut act = CcActivity::default();
        assert!(act.is_empty());
        act.subagents.push(CcAgent {
            agent_id: "a".into(),
            transcript_path: PathBuf::from("/x"),
            agent_type: "Explore".into(),
            description: None,
            label: None,
            phase_title: None,
            state: CcAgentState::Active,
            mtime_ns: 0,
            size: 0,
            tokens: None,
            tool_calls: None,
            last_tool: None,
            model: None,
        });
        assert!(!act.is_empty());
        assert_eq!(act.agent_count(), 1);
        assert!(act.any_active());
    }

    #[test]
    fn settings_from_args_both_forms() {
        let split = [
            "--session-id",
            "x",
            "--settings",
            "/h/claude.json",
            "--add-dir",
            "/r",
        ]
        .map(String::from);
        assert_eq!(
            settings_from_args(&split).as_deref(),
            Some("/h/claude.json")
        );
        let joined = ["--settings=/h/c.json", "/seed"].map(String::from);
        assert_eq!(settings_from_args(&joined).as_deref(), Some("/h/c.json"));
        let none = ["--model", "opus"].map(String::from);
        assert_eq!(settings_from_args(&none), None);
    }

    #[test]
    fn parse_roster_extracts_workers_and_settings() {
        // Shape verified against Claude Code v2.1.204: `workers` keyed by short
        // id; the `--settings` path lives in `dispatch.launch.args` (prompt) or
        // `dispatch.launch.flagArgs` (resume), with `respawnFlags` as fallback.
        let json = r#"{
          "proto":1,
          "workers":{
            "95c38d32":{
              "sessionId":"95c38d32-39d4-4102-82df-24602ac3a2a0",
              "cwd":"/mnt/shared/projects/friring",
              "dispatch":{
                "source":"slash",
                "cwd":"/mnt/shared/projects/friring",
                "launch":{"mode":"prompt","args":[
                  "--session-id","95c38d32-39d4-4102-82df-24602ac3a2a0",
                  "--settings","/mnt/shared/projects/friring/target/dev-sandbox/default/friring-config/hooks/claude.json",
                  "--add-dir","/mnt/shared/projects/friring/"]},
                "respawnFlags":["--settings","/mnt/shared/projects/friring/target/dev-sandbox/default/friring-config/hooks/claude.json"]
              }
            },
            "7492d0aa":{
              "sessionId":"7492d0aa-a09b-4f52-b178-8fcb0c03b7ee",
              "cwd":"/mnt/shared/projects/agterm",
              "dispatch":{
                "source":"fleet",
                "launch":{"mode":"resume","flagArgs":["--effort","max","--model","claude-opus-4-8[1m]"]},
                "respawnFlags":["--effort","max"]
              }
            }
          }
        }"#;
        let workers = parse_roster(json);
        assert_eq!(workers.len(), 2);
        let tbx = workers
            .iter()
            .find(|w| w.short == "95c38d32")
            .expect("friring worker");
        assert_eq!(tbx.session_id, "95c38d32-39d4-4102-82df-24602ac3a2a0");
        assert_eq!(tbx.source.as_deref(), Some("slash"));
        assert_eq!(tbx.cwd.as_deref(), Some("/mnt/shared/projects/friring"));
        assert_eq!(
            tbx.settings_path.as_deref(),
            Some("/mnt/shared/projects/friring/target/dev-sandbox/default/friring-config/hooks/claude.json")
        );
        // A worker launched outside friring carries no `--settings` (won't match).
        let fleet = workers.iter().find(|w| w.short == "7492d0aa").unwrap();
        assert_eq!(fleet.settings_path, None);
        assert_eq!(fleet.source.as_deref(), Some("fleet"));
    }

    #[test]
    fn parse_roster_is_defensive() {
        assert!(parse_roster("not json").is_empty());
        assert!(parse_roster("{}").is_empty());
        assert!(parse_roster(r#"{"workers":{}}"#).is_empty());
        // A worker with no sessionId is skipped, not fatal.
        assert!(parse_roster(r#"{"workers":{"x":{"cwd":"/r"}}}"#).is_empty());
    }

    #[test]
    fn parse_job_state_reads_fan_grid_and_status() {
        // Shape verified against v2.1.204: `fan[]` entries carry `id`
        // (matching agent-<id>.jsonl), `label`, `group`, optional `doneAt`/
        // `failed`; a running entry has no `doneAt`.
        let json = r#"{
          "state":"working",
          "tempo":"blocked",
          "needs":"approve Bash: ls -la",
          "tokens":19030,
          "cwd":"/mnt/shared/projects/friring",
          "sessionId":"95c38d32-39d4-4102-82df-24602ac3a2a0",
          "daemonShort":"95c38d32",
          "backend":"daemon",
          "respawnFlags":["--settings","/h/claude.json","--add-dir","/r"],
          "fan":[
            {"id":"a6d17521b393df437","kind":"workflow","label":"review:correctness","startedAt":1,"doneAt":2,"group":"Review"},
            {"id":"aafa530d9d6c56ecc","kind":"workflow","label":"synthesize","startedAt":3,"doneAt":4,"failed":true,"group":"Synthesize"},
            {"id":"arunning0000000","kind":"workflow","label":"verify:x","startedAt":5,"group":"Verify"}
          ]
        }"#;
        let j = parse_job_state(json).expect("parses");
        assert_eq!(j.session_id, "95c38d32-39d4-4102-82df-24602ac3a2a0");
        assert_eq!(j.daemon_short.as_deref(), Some("95c38d32"));
        assert_eq!(j.tempo.as_deref(), Some("blocked"));
        assert_eq!(j.needs.as_deref(), Some("approve Bash: ls -la"));
        assert_eq!(j.tokens, Some(19030));
        assert_eq!(j.settings_path.as_deref(), Some("/h/claude.json"));
        assert_eq!(j.fan.len(), 3);

        let by_id = j.fan_by_id();
        let done = by_id["a6d17521b393df437"];
        assert!(done.done && !done.failed);
        assert_eq!(done.group.as_deref(), Some("Review"));
        let failed = by_id["aafa530d9d6c56ecc"];
        assert!(failed.done && failed.failed);
        let running = by_id["arunning0000000"];
        assert!(!running.done && !running.failed); // no doneAt yet
    }

    #[test]
    fn parse_job_state_is_defensive() {
        assert_eq!(parse_job_state("nope"), None);
        assert_eq!(parse_job_state("{}"), None); // no sessionId
        let minimal = parse_job_state(r#"{"sessionId":"s"}"#).expect("minimal");
        assert_eq!(minimal.session_id, "s");
        assert!(minimal.fan.is_empty());
        assert_eq!(minimal.settings_path, None);
    }
}
