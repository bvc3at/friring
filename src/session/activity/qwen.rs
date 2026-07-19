//! Qwen Code activity provider — normalizes a session's JSONL transcript
//! (`~/.qwen/projects/<sanitizeCwd(cwd)>/chats/<session-id>.jsonl`) into
//! [`ActivityEvent`]s.
//!
//! Qwen Code is a heavily-diverged fork of gemini-cli. Its
//! `ChatRecordingService` appends one JSON object (a `ChatRecord`) per line as
//! a turn progresses: `assistant` records carry the model's `functionCall`
//! parts (the actions), `tool_result` records carry the matching
//! `functionResponse` parts (the outcome), correlated by `functionCall.id ==
//! functionResponse.id` (mirrored as `toolCallResult.callId`). A `system`
//! record with `subtype == "custom_title"` names the conversation. Unlike
//! Mistral Vibe, `functionCall.args` is a real JSON object (no double-encoded
//! string), and each record carries its own `timestamp`, `model`,
//! `usageMetadata`, and `cwd`.
//!
//! Kept a standalone parser rather than sharing gemini-cli's: qwen-code
//! abandoned gemini's monolithic `tmp/<hash>/logs.json` for this per-project
//! JSONL store and renamed its tool args to mirror Claude Code
//! (`file_path`/`old_string`/`new_string`), so the two formats drift
//! independently.
//!
//! Verified against qwen-code v0.19.9 source (`chatRecordingService.ts`,
//! `storage.ts`, `tool-names.ts`, `shell.ts`, `edit.ts`, `web-fetch.ts`).
//! Defensive like every provider: the record schema is a large, still-growing
//! union, so unknown `type`/`subtype`/tool names degrade to skipped records,
//! never errors.

use std::collections::HashMap;

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for compact-input notes (`Other` events) and subagent prompts.
const NOTE_MAX: usize = 160;

/// Pure plan/bookkeeping tools that record no action on the world — skipped so
/// the timeline shows work, not task-list churn. `task_create` / `agent` /
/// `create_sub_session` are *not* here: those launch real subagents.
const SKIPPED_TOOLS: &[&str] = &["task_list", "task_update", "task_get", "todo_write", "todo"];

/// Streaming scanner over one Qwen Code transcript. Feed complete lines via
/// [`ingest`](Self::ingest) in file order; a `tool_result` can land many lines
/// after its call (or, on a live tail, a later pass), so outcomes are patched
/// onto their pending event as they arrive. Callers own chunking: a chunk must
/// end on a line boundary (a torn tail line is skipped as malformed).
#[derive(Debug, Clone, Default)]
pub struct QwenScan {
    pub events: Vec<ActivityEvent>,
    pub meta: ActivityMeta,
    /// `functionCall.id` → index into [`Self::events`], awaiting its result.
    pending: HashMap<String, usize>,
}

impl QwenScan {
    /// Ingest the next line-aligned chunk of the transcript.
    pub fn ingest(&mut self, chunk: &str) {
        for line in chunk.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            match str_field(&v, "type").as_deref() {
                Some("assistant") => self.ingest_assistant(&v),
                Some("tool_result") => self.ingest_tool_result(&v),
                Some("user") => self.ingest_user(&v),
                Some("system") => self.ingest_system(&v),
                _ => {}
            }
        }
    }

    fn ingest_assistant(&mut self, record: &serde_json::Value) {
        if let Some(model) = str_field(record, "model") {
            self.meta.model = Some(model);
        }
        if let Some(n) = record
            .pointer("/usageMetadata/candidatesTokenCount")
            .and_then(|t| t.as_u64())
        {
            // qwen writes one record per response, each with its own
            // (non-cumulative) usage, so summing over the stream is the total.
            self.meta.output_tokens = Some(self.meta.output_tokens.unwrap_or(0) + n);
        }
        let ts = ts_ms(record);
        let origin = origin_of(record);
        let Some(parts) = record.pointer("/message/parts").and_then(|p| p.as_array()) else {
            return;
        };
        for part in parts {
            let Some(call) = part.get("functionCall") else {
                continue;
            };
            let Some(name) = str_field(call, "name") else {
                continue;
            };
            if SKIPPED_TOOLS.contains(&name.as_str()) {
                continue;
            }
            let Some(mut event) = classify(&name, call.get("args")) else {
                continue;
            };
            event.ts_ms = ts;
            event.origin = origin.clone();
            if let Some(id) = str_field(call, "id") {
                self.pending.insert(id, self.events.len());
            }
            self.events.push(event);
        }
    }

    fn ingest_tool_result(&mut self, record: &serde_json::Value) {
        let parts = record.pointer("/message/parts").and_then(|p| p.as_array());
        let response = parts.and_then(|arr| arr.iter().find_map(|p| p.get("functionResponse")));
        // The id lives on the functionResponse; toolCallResult.callId mirrors
        // it and is the fallback when the response part is malformed.
        let id = response.and_then(|r| str_field(r, "id")).or_else(|| {
            record
                .pointer("/toolCallResult/callId")
                .and_then(|c| c.as_str())
                .map(String::from)
        });
        let Some(id) = id else {
            return;
        };
        let Some(idx) = self.pending.remove(&id) else {
            return;
        };
        let Some(event) = self.events.get_mut(idx) else {
            return;
        };
        // `toolCallResult.status` is the only explicit success/failure marker;
        // absent it, `ok` stays unknown.
        event.ok = record
            .pointer("/toolCallResult/status")
            .and_then(|s| s.as_str())
            .and_then(status_ok);
        if let Some(text) = result_text(response, record) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                event.result_head = Some(head(trimmed, RESULT_HEAD_MAX));
            }
        }
    }

    fn ingest_user(&mut self, record: &serde_json::Value) {
        // Title fallback: the first typed prompt (a `custom_title` record wins).
        if self.meta.title.is_none() {
            if let Some(t) = first_text(record) {
                self.meta.title = Some(head(&t, NOTE_MAX));
            }
        }
    }

    fn ingest_system(&mut self, record: &serde_json::Value) {
        if str_field(record, "subtype").as_deref() != Some("custom_title") {
            return;
        }
        if let Some(title) = record
            .pointer("/systemPayload/title")
            .and_then(|t| t.as_str())
        {
            self.meta.title = Some(title.to_string());
        }
    }
}

/// Map one `functionCall` to an event. `None` ⇒ the call is malformed for its
/// tool (e.g. `run_shell_command` without `command`) or pure bookkeeping, and
/// is skipped.
fn classify(name: &str, args: Option<&serde_json::Value>) -> Option<ActivityEvent> {
    let args = args.unwrap_or(&serde_json::Value::Null);
    let get = |key: &str| args.get(key).and_then(|x| x.as_str()).map(String::from);

    let (kind, detail, note) = match name {
        "run_shell_command" => (ActionKind::Command, get("command")?, get("description")),
        "edit" | "write_file" => (ActionKind::Edit, get("file_path")?, None),
        "notebook_edit" => (
            ActionKind::Edit,
            get("file_path").or_else(|| get("notebook_path"))?,
            None,
        ),
        "read_file" => (ActionKind::Read, get("file_path")?, None),
        "grep_search" | "glob" => (ActionKind::Search, get("pattern")?, get("path")),
        "list_directory" => (ActionKind::Search, get("path")?, None),
        // qwen-code has no web-search tool (its web-search endpoint 404s); only
        // web_fetch is recorded.
        "web_fetch" => (ActionKind::WebFetch, get("url")?, get("prompt")),
        "agent" | "create_sub_session" | "task_create" | "team_create" => {
            let detail = get("description")
                .or_else(|| get("prompt").map(|p| head(&p, NOTE_MAX)))
                .or_else(|| get("name"))?;
            (
                ActionKind::Subagent,
                detail,
                get("subagent_type")
                    .or_else(|| get("agent"))
                    .or_else(|| get("name")),
            )
        }
        _ => (ActionKind::Other, name.to_string(), compact_input(args)),
    };
    Some(ActivityEvent {
        ts_ms: None,
        kind,
        detail,
        note: note.filter(|n| !n.trim().is_empty()),
        result_head: None,
        ok: None,
        origin: None,
        minor: false,
        dur_ms: None,
    })
}

/// A `toolCallResult.status` string to an explicit success flag; unknown
/// statuses leave `ok` unresolved.
fn status_ok(status: &str) -> Option<bool> {
    match status {
        "success" => Some(true),
        "error" | "failed" | "cancelled" | "canceled" => Some(false),
        _ => None,
    }
}

/// Result text for an event's head: the tool's `functionResponse.response
/// .output`, else a plain-string `toolCallResult.resultDisplay` (shell / read).
/// A `resultDisplay` that is a `FileDiff` object (edit / write) is skipped —
/// the diff isn't a compact result line.
fn result_text(response: Option<&serde_json::Value>, record: &serde_json::Value) -> Option<String> {
    if let Some(out) = response
        .and_then(|r| r.pointer("/response/output"))
        .and_then(|o| o.as_str())
        .filter(|o| !o.trim().is_empty())
    {
        return Some(out.to_string());
    }
    record
        .pointer("/toolCallResult/resultDisplay")
        .and_then(|d| d.as_str())
        .map(String::from)
}

/// One-line compact rendering of a tool's args for `note`, capped.
fn compact_input(args: &serde_json::Value) -> Option<String> {
    if args.is_null() || args.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    Some(head(&args.to_string(), NOTE_MAX))
}

/// Epoch ms from the record's ISO-8601 `timestamp`, when present and valid.
fn ts_ms(record: &serde_json::Value) -> Option<u64> {
    let ts = record.get("timestamp")?.as_str()?;
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    u64::try_from(dt.timestamp_millis()).ok()
}

/// Subagent/sidechain attribution: records written by a launched subagent
/// carry `agentName` (and `isSidechain: true`); the main thread carries
/// neither.
fn origin_of(record: &serde_json::Value) -> Option<String> {
    if let Some(name) = record.get("agentName").and_then(|n| n.as_str()) {
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    (record.get("isSidechain").and_then(|x| x.as_bool()) == Some(true))
        .then(|| "subagent".to_string())
}

/// First `text` part of a record's `message.parts`.
fn first_text(record: &serde_json::Value) -> Option<String> {
    let parts = record.pointer("/message/parts")?.as_array()?;
    parts
        .iter()
        .find_map(|p| p.get("text").and_then(|t| t.as_str()))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

/// The `cwd` a transcript line records, for the app layer's collision-safe
/// attribution: `sanitizeCwd` is lossy, so two different working directories
/// can map to one project dir — the recorded `cwd` disambiguates. Pure so it
/// stays testable without touching the filesystem.
pub fn record_cwd(line: &str) -> Option<String> {
    let v = serde_json::from_str::<serde_json::Value>(line.trim()).ok()?;
    str_field(&v, "cwd")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> QwenScan {
        let mut scan = QwenScan::default();
        scan.ingest(s);
        scan
    }

    #[test]
    fn classifies_builtin_tools() {
        // One record per line (folded here for readability).
        let jsonl = [
            r#"{"type":"assistant","timestamp":"2026-07-12T10:00:01.123Z","cwd":"/p","model":"qwen3-coder-plus","message":{"role":"model","parts":[{"functionCall":{"id":"call_01","name":"run_shell_command","args":{"command":"cargo test","description":"Run tests"}}}]},"usageMetadata":{"candidatesTokenCount":42}}"#,
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_02","name":"edit","args":{"file_path":"/p/main.rs","old_string":"a","new_string":"b"}}}]}}"#,
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_03","name":"read_file","args":{"file_path":"/p/Cargo.toml","offset":0}}}]}}"#,
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_04","name":"web_fetch","args":{"url":"https://docs.rs/tokio","prompt":"summarize"}}}]}}"#,
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_05","name":"grep_search","args":{"pattern":"fn main","path":"src/"}}}]}}"#,
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_06","name":"task_create","args":{"description":"Explore backend","subagent_type":"explore"}}}]}}"#,
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_07","name":"task_list","args":{}}}]}}"#,
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_08","name":"list_directory","args":{"path":"/p/src"}}}]}}"#,
        ]
        .join("\n");
        let s = scan(&jsonl);
        let kinds: Vec<ActionKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Command,
                ActionKind::Edit,
                ActionKind::Read,
                ActionKind::WebFetch,
                ActionKind::Search,
                ActionKind::Subagent,
                // task_list skipped between grep and list_directory.
                ActionKind::Search,
            ]
        );
        assert_eq!(s.events[0].detail, "cargo test");
        assert_eq!(s.events[0].note.as_deref(), Some("Run tests"));
        assert_eq!(s.events[0].ts_ms, Some(1_783_850_401_123)); // 2026-07-12T10:00:01.123Z
        assert_eq!(s.events[1].detail, "/p/main.rs");
        assert_eq!(s.events[3].note.as_deref(), Some("summarize"));
        assert_eq!(s.events[4].note.as_deref(), Some("src/"));
        assert_eq!(s.events[5].note.as_deref(), Some("explore"));
        assert_eq!(s.meta.model.as_deref(), Some("qwen3-coder-plus"));
    }

    #[test]
    fn results_patch_ok_and_head_by_call_id() {
        let mut s = QwenScan::default();
        s.ingest(
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_01","name":"run_shell_command","args":{"command":"cargo test"}}}]}}"#,
        );
        assert_eq!(s.events[0].ok, None); // pending
        s.ingest(
            r#"{"type":"tool_result","message":{"role":"user","parts":[{"functionResponse":{"id":"call_01","name":"run_shell_command","response":{"output":"test result: ok. 12 passed"}}}]},"toolCallResult":{"callId":"call_01","status":"success","resultDisplay":"test result: ok. 12 passed"}}"#,
        );
        assert_eq!(s.events[0].ok, Some(true));
        assert_eq!(
            s.events[0].result_head.as_deref(),
            Some("test result: ok. 12 passed")
        );

        // An error status flips ok=false.
        s.ingest(
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_02","name":"run_shell_command","args":{"command":"false"}}}]}}"#,
        );
        s.ingest(
            r#"{"type":"tool_result","message":{"role":"user","parts":[{"functionResponse":{"id":"call_02","name":"run_shell_command","response":{"output":""}}}]},"toolCallResult":{"callId":"call_02","status":"error","resultDisplay":"exit 1"}}"#,
        );
        assert_eq!(s.events[1].ok, Some(false));
        assert_eq!(s.events[1].result_head.as_deref(), Some("exit 1"));
    }

    #[test]
    fn edit_result_skips_file_diff_object() {
        let mut s = QwenScan::default();
        s.ingest(
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"call_01","name":"edit","args":{"file_path":"/p/main.rs","old_string":"a","new_string":"b"}}}]}}"#,
        );
        s.ingest(
            r#"{"type":"tool_result","message":{"role":"user","parts":[{"functionResponse":{"id":"call_01","name":"edit","response":{"output":"Successfully modified file: /p/main.rs"}}}]},"toolCallResult":{"callId":"call_01","status":"success","resultDisplay":{"fileName":"main.rs","fileDiff":"@@ -1 +1 @@\n-a\n+b","originalContent":"a","newContent":"b"}}}"#,
        );
        // Head comes from response.output, not the FileDiff resultDisplay object.
        assert_eq!(
            s.events[0].result_head.as_deref(),
            Some("Successfully modified file: /p/main.rs")
        );
        assert_eq!(s.events[0].ok, Some(true));
    }

    #[test]
    fn subagent_records_get_agent_origin() {
        let s = scan(
            r#"{"type":"assistant","agentName":"explore","isSidechain":true,"message":{"role":"model","parts":[{"functionCall":{"id":"c1","name":"read_file","args":{"file_path":"/p/a.rs"}}}]}}"#,
        );
        assert_eq!(s.events[0].origin.as_deref(), Some("explore"));
    }

    #[test]
    fn meta_prefers_custom_title_over_first_prompt() {
        let mut s = QwenScan::default();
        s.ingest(
            r#"{"type":"user","message":{"role":"user","parts":[{"text":"Fix the failing cargo tests"}]}}"#,
        );
        assert_eq!(s.meta.title.as_deref(), Some("Fix the failing cargo tests"));
        s.ingest(
            r#"{"type":"system","subtype":"custom_title","systemPayload":{"title":"Fix failing tests","source":"auto"}}"#,
        );
        assert_eq!(s.meta.title.as_deref(), Some("Fix failing tests"));
    }

    #[test]
    fn output_tokens_sum_across_records() {
        let mut s = QwenScan::default();
        s.ingest(
            r#"{"type":"assistant","message":{"role":"model","parts":[]},"usageMetadata":{"candidatesTokenCount":40}}"#,
        );
        s.ingest(
            r#"{"type":"assistant","message":{"role":"model","parts":[]},"usageMetadata":{"candidatesTokenCount":15}}"#,
        );
        assert_eq!(s.meta.output_tokens, Some(55));
    }

    #[test]
    fn malformed_and_unknown_records_are_skipped() {
        let s = scan(concat!(
            "not json\n",
            // run_shell_command without a command → skipped.
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"c1","name":"run_shell_command","args":{}}}]}}"#,
            "\n",
            r#"{"type":"system","subtype":"agent_bootstrap"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"model","parts":[{"functionCall":{"id":"c2","name":"unknown_tool","args":{"x":1}}}]}}"#,
        ));
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, ActionKind::Other);
        assert_eq!(s.events[0].detail, "unknown_tool");
        assert!(s.events[0].note.as_deref().unwrap().contains("\"x\":1"));
    }

    #[test]
    fn record_cwd_extracts_and_tolerates_garbage() {
        assert_eq!(
            record_cwd(r#"{"type":"assistant","cwd":"/home/user/proj"}"#).as_deref(),
            Some("/home/user/proj")
        );
        assert_eq!(record_cwd("not json"), None);
        assert_eq!(record_cwd(r#"{"type":"assistant"}"#), None);
    }
}
