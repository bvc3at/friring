//! Cline (standalone CLI / cline-core, v4) activity provider — normalizes a
//! session's `<sid>.messages.json` (+ sibling `<sid>.json` manifest) under
//! `~/.cline/data/sessions/<sid>/` into [`ActivityEvent`]s.
//!
//! Unlike the JSONL providers, cline-core persists the whole conversation as a
//! **single JSON object rewritten in full on every turn**
//! (`session-manifest-store.ts persistSessionMessages → writeFileSync`), so the
//! app layer re-parses the file wholesale on each change rather than tailing
//! byte offsets. The envelope is `{version, updated_at, agent, sessionId,
//! messages:[…], system_prompt?}`; every action is a `tool_use` block inside an
//! assistant message's `content[]`, and each outcome is a `tool_result` block
//! in a following user message, correlated by `tool_use_id` (falling back to
//! `call_id`). Per-message `ts` is epoch-ms, so events carry real timestamps.
//!
//! The v4 default tool set is provider-agnostic and distinct from the older
//! XML-style names: `run_commands` (shell), `read_files`, `editor` +
//! `apply_patch` (edits), `search_codebase` (a *local* regex search, not web),
//! and `fetch_web_content` (the only web op the CLI records — there is no
//! web-search tool). MCP tool calls surface under their own dynamic names with
//! no stable prefix, so they land in [`ActionKind::Other`], named by the tool.
//!
//! Verified against cline source at HEAD 6309971 (post-v4.0.8, 2026-07-11):
//! `sdk/packages/core/src/session/stores/session-manifest-store.ts`,
//! `.../extensions/tools/{constants,schemas}.ts`,
//! `.../session/models/session-manifest.ts`, `shared/storage/paths.ts`.
//! Defensive like every provider: unknown shapes are skipped, never errors.

use std::collections::HashMap;

use serde_json::Value;

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for compact-input notes (`Other` events, fetch prompts).
const NOTE_MAX: usize = 160;

/// Plan/dialogue tools that act on the conversation, not the workspace — kept
/// off the timeline so it shows work, not turn bookkeeping. Names that never
/// occur are harmless no-ops; genuinely unknown tools still surface as
/// [`ActionKind::Other`].
const SKIPPED_TOOLS: &[&str] = &[
    "todo",
    "update_todo_list",
    "plan_mode_respond",
    "attempt_completion",
    "ask_followup_question",
];

/// Parse one cline `<sid>.messages.json` object into its event stream.
///
/// Returns `None` only when the top-level JSON fails to parse — the file was
/// caught mid-`writeFileSync` (a full rewrite is not atomic), so the caller
/// should keep its prior events and retry. `Some(vec)` (possibly empty) means a
/// well-formed object was read. Malformed *inner* records are skipped, never
/// fatal.
pub fn parse_messages(json: &str) -> Option<Vec<ActivityEvent>> {
    let root: Value = serde_json::from_str(json).ok()?;
    let origin = origin_of(&root);
    let Some(messages) = root.get("messages").and_then(|m| m.as_array()) else {
        return Some(Vec::new());
    };
    let mut events: Vec<ActivityEvent> = Vec::new();
    // `tool_use` id (and its `call_id`) → the event indices it produced; a
    // single call can touch several files (multi-file `apply_patch`, batched
    // `read_files`), so its result patches every one.
    let mut pending: HashMap<String, Vec<usize>> = HashMap::new();
    for msg in messages {
        let ts = msg.get("ts").and_then(|t| t.as_u64());
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => ingest_tool_use(block, ts, &origin, &mut events, &mut pending),
                Some("tool_result") => ingest_tool_result(block, &mut events, &mut pending),
                _ => {}
            }
        }
    }
    Some(events)
}

/// The whole-transcript `agent` marker: a subagent/teammate file is stemmed by
/// agent id in the root session's dir, so its records get a generic origin
/// label; the lead's records have none.
fn origin_of(root: &Value) -> Option<String> {
    match root.get("agent").and_then(|a| a.as_str()) {
        None | Some("lead") => None,
        Some(other) => Some(other.to_string()),
    }
}

fn ingest_tool_use(
    block: &Value,
    ts: Option<u64>,
    origin: &Option<String>,
    events: &mut Vec<ActivityEvent>,
    pending: &mut HashMap<String, Vec<usize>>,
) {
    let Some(name) = block.get("name").and_then(|n| n.as_str()) else {
        return;
    };
    if SKIPPED_TOOLS.contains(&name) {
        return;
    }
    let mut produced = classify(name, block.get("input"));
    if produced.is_empty() {
        return;
    }
    for ev in &mut produced {
        ev.ts_ms = ts;
        ev.origin = origin.clone();
    }
    let start = events.len();
    events.append(&mut produced);
    let indices: Vec<usize> = (start..events.len()).collect();
    for key in ["id", "call_id"] {
        if let Some(id) = block.get(key).and_then(|v| v.as_str()) {
            pending.entry(id.to_string()).or_default().extend(&indices);
        }
    }
}

fn ingest_tool_result(
    block: &Value,
    events: &mut [ActivityEvent],
    pending: &mut HashMap<String, Vec<usize>>,
) {
    let key = block
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .or_else(|| block.get("call_id").and_then(|v| v.as_str()));
    let Some(key) = key else {
        return;
    };
    let Some(indices) = pending.remove(key) else {
        return;
    };
    // `is_error` is the sole explicit success/failure marker cline records; its
    // presence (a result arrived) means the action ran, error or not.
    let ok = Some(
        !block
            .get("is_error")
            .and_then(|e| e.as_bool())
            .unwrap_or(false),
    );
    let text = result_text(block.get("content"));
    let result_head = {
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| head(trimmed, RESULT_HEAD_MAX))
    };
    for idx in indices {
        if let Some(ev) = events.get_mut(idx) {
            ev.ok = ok;
            if let Some(h) = &result_head {
                ev.result_head = Some(h.clone());
            }
        }
    }
}

/// Map one `tool_use` block to zero or more events. A batching tool (multi-file
/// `apply_patch` / `read_files` / multi-request `fetch_web_content`) yields one
/// event per target so the Files/Web aggregations stay accurate; a malformed
/// block yields none (skipped).
fn classify(name: &str, input: Option<&Value>) -> Vec<ActivityEvent> {
    let null = Value::Null;
    let input = input.unwrap_or(&null);
    match name {
        "run_commands" => collect_commands(input)
            .into_iter()
            .map(|c| event(ActionKind::Command, c, None))
            .collect(),
        "editor" => str_at(input, "path")
            .map(|p| vec![event(ActionKind::Edit, p, None)])
            .unwrap_or_default(),
        "apply_patch" => patch_paths(input)
            .into_iter()
            .map(|p| event(ActionKind::Edit, p, None))
            .collect(),
        "read_files" => collect_read_paths(input)
            .into_iter()
            .map(|p| event(ActionKind::Read, p, None))
            .collect(),
        "search_codebase" => collect_queries(input)
            .into_iter()
            .map(|q| event(ActionKind::Search, q, None))
            .collect(),
        "fetch_web_content" => fetch_events(input),
        _ => vec![event(
            ActionKind::Other,
            name.to_string(),
            compact_input(input),
        )],
    }
}

/// Commands from a `run_commands` input, tolerating every observed union shape:
/// `{commands:[…]}` (strings or `{command,args}` structs), `{command}`,
/// `{cmd}`, or a bare string.
fn collect_commands(input: &Value) -> Vec<String> {
    if let Some(arr) = input.get("commands").and_then(|c| c.as_array()) {
        return arr.iter().filter_map(command_from_value).collect();
    }
    for key in ["command", "cmd"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            return vec![s.to_string()];
        }
    }
    input
        .as_str()
        .map(|s| vec![s.to_string()])
        .unwrap_or_default()
}

/// One command entry: a bare string, or a structured `{command, args?}` (an
/// executable + argv, rejoined for display).
fn command_from_value(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    let cmd = v.get("command").and_then(|c| c.as_str())?;
    let args: Vec<&str> = v
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    if args.is_empty() {
        Some(cmd.to_string())
    } else {
        Some(format!("{cmd} {}", args.join(" ")))
    }
}

/// File paths from a `read_files` input: `{files:[{path}|str]}`,
/// `{file_paths:[…]}`, `{paths:[…]}`, `{path}`, or a bare string.
fn collect_read_paths(input: &Value) -> Vec<String> {
    if let Some(arr) = input.get("files").and_then(|f| f.as_array()) {
        return arr.iter().filter_map(path_from_value).collect();
    }
    for key in ["file_paths", "paths"] {
        if let Some(arr) = input.get(key).and_then(|p| p.as_array()) {
            return arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }
    if let Some(s) = str_at(input, "path") {
        return vec![s];
    }
    input
        .as_str()
        .map(|s| vec![s.to_string()])
        .unwrap_or_default()
}

/// A read entry: a bare path string or a `{path, start_line?, end_line?}`.
fn path_from_value(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    v.get("path").and_then(|p| p.as_str()).map(String::from)
}

/// The touched file paths in an `apply_patch` body: the canonical
/// `*** {Update,Add,Delete} File: <path>` markers. The payload may also arrive
/// as a bare string; either way an unparsable patch yields nothing.
fn patch_paths(input: &Value) -> Vec<String> {
    let patch = input
        .get("input")
        .and_then(|p| p.as_str())
        .or_else(|| input.as_str());
    let Some(patch) = patch else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in patch.lines() {
        let Some(rest) = line.trim_start().strip_prefix("***") else {
            continue;
        };
        // `Begin Patch` / `End Patch` have no `File:` and fall through.
        if let Some((_, path)) = rest.split_once("File:") {
            let path = path.trim();
            if !path.is_empty() && !out.iter().any(|p| p == path) {
                out.push(path.to_string());
            }
        }
    }
    out
}

/// Search queries from a `search_codebase` input (`{queries:[…]}` or the
/// singular `{query}`).
fn collect_queries(input: &Value) -> Vec<String> {
    if let Some(arr) = input.get("queries").and_then(|q| q.as_array()) {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
    }
    str_at(input, "query").map(|q| vec![q]).unwrap_or_default()
}

/// Web-fetch events from a `fetch_web_content` input (`{requests:[{url,
/// prompt}]}`, or a singular `{url, prompt}`), one per request.
fn fetch_events(input: &Value) -> Vec<ActivityEvent> {
    if let Some(arr) = input.get("requests").and_then(|r| r.as_array()) {
        return arr
            .iter()
            .filter_map(|r| {
                let url = r.get("url").and_then(|u| u.as_str())?;
                let note = r
                    .get("prompt")
                    .and_then(|p| p.as_str())
                    .map(|p| head(p, NOTE_MAX));
                Some(event(ActionKind::WebFetch, url.to_string(), note))
            })
            .collect();
    }
    match str_at(input, "url") {
        Some(url) => vec![event(
            ActionKind::WebFetch,
            url,
            str_at(input, "prompt").map(|p| head(&p, NOTE_MAX)),
        )],
        None => Vec::new(),
    }
}

/// Compact one tool input into a `note`, capped — skipped when empty.
fn compact_input(input: &Value) -> Option<String> {
    if input.is_null() || input.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    Some(head(&input.to_string(), NOTE_MAX))
}

/// Flatten a `tool_result` `content` (a string, or Anthropic-style
/// `[{type:"text", text}]` / bare-string blocks) into display text.
fn result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| b.as_str())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn event(kind: ActionKind, detail: String, note: Option<String>) -> ActivityEvent {
    ActivityEvent {
        ts_ms: None,
        kind,
        detail,
        note: note.filter(|n| !n.trim().is_empty()),
        result_head: None,
        ok: None,
        origin: None,
        minor: false,
        dur_ms: None,
    }
}

/// Session metadata from a cline `<sid>.json` manifest (title, model, output
/// tokens) plus the fields the app layer needs to attribute a flat-store
/// session to a launch dir (`cwd`/`workspace_root`, id, start time).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClineMeta {
    pub meta: ActivityMeta,
    pub session_id: Option<String>,
    /// `cwd` — the primary launch-dir match key.
    pub cwd: Option<String>,
    /// `workspace_root` — the fallback match key (often equal to `cwd`).
    pub workspace_root: Option<String>,
    /// `started_at` (ISO-8601) as epoch ms.
    pub started_at_ms: Option<u64>,
}

/// Parse a `<sid>.json` manifest. Missing/garbled input yields defaults.
pub fn parse_manifest(s: &str) -> ClineMeta {
    let Ok(v) = serde_json::from_str::<Value>(s) else {
        return ClineMeta::default();
    };
    let ptr_str = |ptr: &str| v.pointer(ptr).and_then(|x| x.as_str()).map(String::from);
    ClineMeta {
        meta: ActivityMeta {
            title: ptr_str("/metadata/title"),
            model: str_at(&v, "model"),
            output_tokens: v
                .pointer("/metadata/usage/outputTokens")
                .and_then(|t| t.as_u64()),
        },
        session_id: str_at(&v, "session_id"),
        cwd: str_at(&v, "cwd"),
        workspace_root: str_at(&v, "workspace_root"),
        started_at_ms: str_at(&v, "started_at")
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
            .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `<sid>.messages.json` envelope around raw message JSON literals.
    fn envelope(agent: &str, messages: &[&str]) -> String {
        format!(
            r#"{{"version":1,"agent":"{agent}","sessionId":"sid","messages":[{}]}}"#,
            messages.join(",")
        )
    }

    fn kinds(events: &[ActivityEvent]) -> Vec<ActionKind> {
        events.iter().map(|e| e.kind).collect()
    }

    #[test]
    fn classifies_v4_default_tools() {
        let json = envelope(
            "lead",
            &[r#"{"role":"assistant","ts":1752300010000,"content":[
                    {"type":"tool_use","id":"t1","name":"run_commands","input":{"commands":["cargo build","cargo test"]}},
                    {"type":"tool_use","id":"t2","name":"editor","input":{"path":"/repo/src/main.rs","new_text":"x"}},
                    {"type":"tool_use","id":"t3","name":"read_files","input":{"files":[{"path":"/repo/Cargo.toml"},{"path":"/repo/src/big.rs","start_line":1,"end_line":200}]}},
                    {"type":"tool_use","id":"t4","name":"fetch_web_content","input":{"requests":[{"url":"https://docs.rs/tokio","prompt":"summarize"}]}},
                    {"type":"tool_use","id":"t5","name":"search_codebase","input":{"queries":["fn main"]}},
                    {"type":"tool_use","id":"t6","name":"todo","input":{"items":[]}},
                    {"type":"tool_use","id":"t7","name":"some_mcp_tool","input":{"q":"weather"}}
                ]}"#],
        );
        let events = parse_messages(&json).expect("valid");
        assert_eq!(
            kinds(&events),
            vec![
                ActionKind::Command, // cargo build
                ActionKind::Command, // cargo test
                ActionKind::Edit,
                ActionKind::Read, // Cargo.toml
                ActionKind::Read, // big.rs
                ActionKind::WebFetch,
                ActionKind::Search,
                // todo skipped
                ActionKind::Other, // unknown/MCP tool
            ]
        );
        assert_eq!(events[0].detail, "cargo build");
        assert_eq!(events[2].detail, "/repo/src/main.rs");
        assert_eq!(events[3].detail, "/repo/Cargo.toml");
        assert_eq!(events[5].detail, "https://docs.rs/tokio");
        assert_eq!(events[5].note.as_deref(), Some("summarize"));
        assert_eq!(events[6].detail, "fn main");
        assert_eq!(events.last().unwrap().detail, "some_mcp_tool");
        // Per-message ts propagates to every event of that message.
        assert!(events.iter().all(|e| e.ts_ms == Some(1752300010000)));
    }

    #[test]
    fn apply_patch_yields_one_edit_per_touched_file() {
        let json = envelope(
            "lead",
            &[r#"{"role":"assistant","content":[
                {"type":"tool_use","id":"p1","name":"apply_patch","input":{"input":"*** Begin Patch\n*** Update File: /repo/src/lib.rs\n@@\n-old\n+new\n*** Add File: /repo/src/new.rs\n+created\n*** End Patch"}}
            ]}"#],
        );
        let events = parse_messages(&json).expect("valid");
        assert_eq!(kinds(&events), vec![ActionKind::Edit, ActionKind::Edit]);
        assert_eq!(events[0].detail, "/repo/src/lib.rs");
        assert_eq!(events[1].detail, "/repo/src/new.rs");
    }

    #[test]
    fn run_commands_and_read_files_union_shapes() {
        // Structured commands, singular {command}, bare-string path arrays.
        let json = envelope(
            "lead",
            &[
                r#"{"role":"assistant","content":[
                    {"type":"tool_use","id":"a","name":"run_commands","input":{"commands":[{"command":"grep","args":["-r","fn","src"]}]}}
                ]}"#,
                r#"{"role":"assistant","content":[
                    {"type":"tool_use","id":"b","name":"run_commands","input":{"command":"ls -la"}}
                ]}"#,
                r#"{"role":"assistant","content":[
                    {"type":"tool_use","id":"c","name":"read_files","input":{"paths":["/a.rs","/b.rs"]}}
                ]}"#,
            ],
        );
        let events = parse_messages(&json).expect("valid");
        assert_eq!(events[0].detail, "grep -r fn src");
        assert_eq!(events[1].detail, "ls -la");
        assert_eq!(
            kinds(&events),
            vec![
                ActionKind::Command,
                ActionKind::Command,
                ActionKind::Read,
                ActionKind::Read
            ]
        );
        assert_eq!(events[2].detail, "/a.rs");
        assert_eq!(events[3].detail, "/b.rs");
    }

    #[test]
    fn tool_result_patches_ok_and_head_including_multi_file() {
        let json = envelope(
            "lead",
            &[
                r#"{"role":"assistant","content":[
                    {"type":"tool_use","id":"t1","call_id":"c1","name":"run_commands","input":{"commands":["make"]}}
                ]}"#,
                r#"{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t1","content":"Compiling... Finished","is_error":false}
                ]}"#,
                r#"{"role":"assistant","content":[
                    {"type":"tool_use","id":"t2","name":"read_files","input":{"files":[{"path":"/gone"}]}}
                ]}"#,
                r#"{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t2","content":[{"type":"text","text":"no such file"}],"is_error":true}
                ]}"#,
                r#"{"role":"assistant","content":[
                    {"type":"tool_use","id":"t3","name":"apply_patch","input":{"input":"*** Begin Patch\n*** Update File: /a\n*** Update File: /b\n*** End Patch"}}
                ]}"#,
                r#"{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"t3","content":"applied","is_error":false}
                ]}"#,
            ],
        );
        let events = parse_messages(&json).expect("valid");
        assert_eq!(events[0].ok, Some(true));
        assert_eq!(
            events[0].result_head.as_deref(),
            Some("Compiling... Finished")
        );
        assert_eq!(events[1].ok, Some(false));
        assert_eq!(events[1].result_head.as_deref(), Some("no such file"));
        // A single patch result patches every file it produced.
        assert_eq!(events[2].ok, Some(true));
        assert_eq!(events[3].ok, Some(true));
        assert_eq!(events[3].result_head.as_deref(), Some("applied"));
    }

    #[test]
    fn subagent_envelope_labels_origin() {
        let json = envelope(
            "subagent",
            &[r#"{"role":"assistant","content":[
                {"type":"tool_use","id":"t1","name":"read_files","input":{"files":[{"path":"/repo/x.rs"}]}}
            ]}"#],
        );
        let events = parse_messages(&json).expect("valid");
        assert_eq!(events[0].origin.as_deref(), Some("subagent"));
    }

    #[test]
    fn malformed_input_is_skipped_and_torn_json_returns_none() {
        // A half-written file (truncated object) is unparseable → None.
        assert!(parse_messages(r#"{"version":1,"messages":[{"role":"#).is_none());
        // No `messages` array → parsed, but empty.
        assert_eq!(parse_messages(r#"{"version":1}"#), Some(Vec::new()));
        // Missing required fields per tool → that block skipped, others kept.
        let json = envelope(
            "lead",
            &[r#"{"role":"assistant","content":[
                {"type":"tool_use","id":"a","name":"editor","input":{"new_text":"x"}},
                {"type":"tool_use","id":"b","name":"run_commands","input":{"commands":[]}},
                {"type":"tool_use","id":"c","name":"read_files","input":{"files":[{"path":"/kept.rs"}]}}
            ]}"#],
        );
        let events = parse_messages(&json).expect("valid");
        assert_eq!(kinds(&events), vec![ActionKind::Read]);
        assert_eq!(events[0].detail, "/kept.rs");
    }

    #[test]
    fn manifest_extracts_title_model_tokens_cwd_and_start() {
        let m = parse_manifest(
            r#"{
              "version":1,"session_id":"1752300000000_a1B2c","source":"cli",
              "started_at":"2026-07-12T14:00:00.000Z","status":"completed",
              "provider":"anthropic","model":"claude-opus-4",
              "cwd":"/repo","workspace_root":"/repo",
              "prompt":"fix the build",
              "metadata":{"title":"Fix the build","totalCost":0.19,
                "usage":{"inputTokens":34120,"outputTokens":2210,"totalCost":0.19}}
            }"#,
        );
        assert_eq!(m.meta.title.as_deref(), Some("Fix the build"));
        assert_eq!(m.meta.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(m.meta.output_tokens, Some(2210));
        assert_eq!(m.cwd.as_deref(), Some("/repo"));
        assert_eq!(m.workspace_root.as_deref(), Some("/repo"));
        assert_eq!(m.session_id.as_deref(), Some("1752300000000_a1B2c"));
        assert!(m.started_at_ms.is_some());

        assert_eq!(parse_manifest("garbage"), ClineMeta::default());
    }
}
