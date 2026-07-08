//! Claude Code workflow + subagent activity — pure data model and parsers.
//!
//! thurbox surfaces what happens *inside* a running Claude Code session: the
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
//! This module is the **pure** layer (arch rule `ui ← session`, no filesystem):
//! it defines the [`CcActivity`] index the app polls onto [`SessionInfo`], the
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
/// blocks; normalise either to a single string.
fn normalize_tool_result(content: Option<&serde_json::Value>) -> String {
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
}
