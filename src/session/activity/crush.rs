//! Crush (Charmbracelet) activity provider — normalizes the `parts` JSON of a
//! Crush session's `messages` rows into [`ActivityEvent`]s.
//!
//! Crush stores each project's history in a per-project SQLite DB
//! (`<cwd>/.crush/crush.db`); the app layer (`app::activity::crush`) reads it
//! and feeds this pure parser one message at a time. A message's `parts` column
//! is a JSON array of `{"type","data"}` wrappers: `tool_call` parts are the
//! actions (agent `bash`/`edit`/`view`/…, where `data.input` is itself a
//! JSON-encoded STRING, so it is double-decoded), `tool_result` parts carry the
//! outcome (matched to the call by `data.tool_call_id`), and `shell_command`
//! parts are the user's in-Crush bang-mode runs. Per-record timestamps come
//! from each row's `created_at` (Unix seconds, converted to ms by the app
//! layer); the title and cumulative output tokens come from the `sessions` row
//! (see [`session_meta`]) because Crush records the model/provider per message,
//! not on the session.
//!
//! Verified against Crush main HEAD 24a000d (2026-07-11) ≈ v0.84.1. Defensive
//! like every provider (arch rule `session` ← nothing): unknown part types,
//! unknown tool names, and malformed JSON degrade to skipped records, never
//! errors.

use std::collections::HashMap;

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for compact-input notes (`Other` events) and inlined subagent prompts.
const NOTE_MAX: usize = 160;

/// Pure plan/status tools that record no action on the world — skipped so the
/// timeline shows work, not bookkeeping (mirrors the Claude/Vibe skip lists).
/// `todos` is the todo list; `job_output`/`job_kill` poll and control a
/// background job; `crush_info`/`crush_logs` are self-introspection.
const SKIPPED_TOOLS: &[&str] = &[
    "todos",
    "job_output",
    "job_kill",
    "crush_info",
    "crush_logs",
];

/// Streaming accumulator over one Crush session's `messages` rows. Feed rows in
/// `created_at` order via [`ingest_message`](Self::ingest_message); a
/// `tool_call` and its `tool_result` live in different rows (assistant vs
/// `tool` role), so outcomes are correlated across feeds by `tool_call_id`.
///
/// Crush's per-project DB is read whole each pass (the app layer's Replace
/// strategy), so the accumulator is rebuilt from scratch rather than tailed —
/// but the streaming shape matches [`super::claude::ClaudeScan`] and keeps the
/// call/result correlation identical.
#[derive(Debug, Clone, Default)]
pub struct CrushScan {
    pub events: Vec<ActivityEvent>,
    pub meta: ActivityMeta,
    /// `tool_call` id → index into [`Self::events`], awaiting its result.
    pending: HashMap<String, usize>,
}

impl CrushScan {
    /// Ingest one `messages` row: its `parts` JSON, the row's `model` (last
    /// seen wins for [`ActivityMeta::model`]), and its `created_at` as epoch ms.
    pub fn ingest_message(&mut self, parts: &str, model: Option<&str>, ts_ms: Option<u64>) {
        if let Some(m) = model.filter(|m| !m.is_empty()) {
            self.meta.model = Some(m.to_string());
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(parts) else {
            return;
        };
        let Some(arr) = value.as_array() else {
            return;
        };
        for part in arr {
            let Some(ty) = part.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            let Some(data) = part.get("data") else {
                continue;
            };
            match ty {
                "tool_call" => self.ingest_tool_call(data, ts_ms),
                "tool_result" => self.ingest_tool_result(data),
                "shell_command" => self.ingest_shell_command(data, ts_ms),
                _ => {}
            }
        }
    }

    fn ingest_tool_call(&mut self, data: &serde_json::Value, ts_ms: Option<u64>) {
        let Some(name) = data.get("name").and_then(|n| n.as_str()) else {
            return;
        };
        if SKIPPED_TOOLS.contains(&name) {
            return;
        }
        let input = tool_input(data);
        let Some(mut event) = classify(name, &input) else {
            return;
        };
        event.ts_ms = ts_ms;
        if let Some(id) = data.get("id").and_then(|i| i.as_str()) {
            self.pending.insert(id.to_string(), self.events.len());
        }
        self.events.push(event);
    }

    fn ingest_tool_result(&mut self, data: &serde_json::Value) {
        let Some(id) = data.get("tool_call_id").and_then(|i| i.as_str()) else {
            return;
        };
        let Some(idx) = self.pending.remove(id) else {
            return;
        };
        let Some(event) = self.events.get_mut(idx) else {
            return;
        };
        // Success is recorded only when the explicit `is_error` marker is
        // present — absent means "unknown", never assumed-ok.
        if let Some(err) = data.get("is_error").and_then(|e| e.as_bool()) {
            event.ok = Some(!err);
        }
        if let Some(content) = data.get("content").and_then(|c| c.as_str()) {
            let content = content.trim();
            if !content.is_empty() {
                event.result_head = Some(head(content, RESULT_HEAD_MAX));
            }
        }
    }

    fn ingest_shell_command(&mut self, data: &serde_json::Value, ts_ms: Option<u64>) {
        let Some(command) = data.get("command").and_then(|c| c.as_str()) else {
            return;
        };
        let mut event = ActivityEvent {
            ts_ms,
            kind: ActionKind::Command,
            detail: command.to_string(),
            // Distinguish a user's in-Crush bang-mode run from an agent `bash`
            // tool call (which carries a description note instead).
            note: Some("manual run".to_string()),
            result_head: None,
            // `exit_code` is the explicit outcome marker for bang-mode runs.
            ok: data
                .get("exit_code")
                .and_then(|c| c.as_i64())
                .map(|c| c == 0),
            origin: None,
        };
        if let Some(output) = data.get("output").and_then(|o| o.as_str()) {
            let output = output.trim();
            if !output.is_empty() {
                event.result_head = Some(head(output, RESULT_HEAD_MAX));
            }
        }
        self.events.push(event);
    }
}

/// A `tool_call`'s `data.input` — a JSON-encoded STRING in the stable format,
/// so parse the string then json-parse again; tolerate an already-decoded
/// object (older/looser writers) and anything unparseable as an empty input.
fn tool_input(data: &serde_json::Value) -> serde_json::Value {
    match data.get("input") {
        Some(serde_json::Value::String(s)) => {
            serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
        }
        Some(v) => v.clone(),
        None => serde_json::Value::Null,
    }
}

/// Map one tool call to an event by its builtin name. `None` ⇒ the call is
/// malformed for its tool (e.g. `bash` without `command`) and is skipped.
/// MCP tools carry server-defined names with no namespace, so any name outside
/// the builtin set falls through to `Other` (Crush cannot be told apart from a
/// custom tool at this layer).
fn classify(name: &str, input: &serde_json::Value) -> Option<ActivityEvent> {
    let get = |key: &str| input.get(key).and_then(|x| x.as_str()).map(String::from);

    let (kind, detail, note) = match name {
        "bash" => (ActionKind::Command, get("command")?, get("description")),
        "edit" | "write" | "multiedit" => (ActionKind::Edit, get("file_path")?, None),
        "view" => (ActionKind::Read, get("file_path")?, None),
        "grep" | "glob" => (ActionKind::Search, get("pattern")?, get("path")),
        "ls" => (ActionKind::Search, get("path")?, None),
        "web_search" | "sourcegraph" => (ActionKind::WebSearch, get("query")?, None),
        "fetch" | "web_fetch" => (ActionKind::WebFetch, get("url")?, None),
        // A saved download is primarily a network fetch; the local target is
        // the note.
        "download" => (ActionKind::WebFetch, get("url")?, get("file_path")),
        // The agentic fetch may name only its prompt when it lets the model
        // pick the URL.
        "agentic_fetch" => match get("url") {
            Some(url) => (
                ActionKind::WebFetch,
                url,
                get("prompt").map(|p| head(&p, NOTE_MAX)),
            ),
            None => (
                ActionKind::WebFetch,
                get("prompt").map(|p| head(&p, NOTE_MAX))?,
                None,
            ),
        },
        // Delegation via Crush's agent/task tool (sub-sessions link by
        // `parent_session_id` in the DB; the call itself is the timeline entry).
        "agent" | "task" => {
            let detail = get("prompt")
                .map(|p| head(&p, NOTE_MAX))
                .or_else(|| get("description"))
                .or_else(|| get("task"))?;
            (
                ActionKind::Subagent,
                detail,
                get("subagent_type").or_else(|| get("agent")),
            )
        }
        _ => (ActionKind::Other, name.to_string(), compact_input(input)),
    };
    Some(ActivityEvent {
        ts_ms: None,
        kind,
        detail,
        note: note.filter(|n| !n.trim().is_empty()),
        result_head: None,
        ok: None,
        origin: None,
    })
}

/// One-line compact rendering of an unknown tool's input for `note`, capped.
fn compact_input(input: &serde_json::Value) -> Option<String> {
    if input.is_null() || input.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    Some(head(&input.to_string(), NOTE_MAX))
}

/// Display metadata for a Crush session from its `sessions` row fields. The
/// model is not stored on the session (it lives per message), so the scan fills
/// [`ActivityMeta::model`] from the message stream; `completion_tokens` is the
/// cumulative output-token count.
pub fn session_meta(title: Option<&str>, completion_tokens: Option<i64>) -> ActivityMeta {
    ActivityMeta {
        title: title.map(str::to_string).filter(|t| !t.trim().is_empty()),
        model: None,
        output_tokens: completion_tokens.and_then(|t| u64::try_from(t).ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(input: &str) -> String {
        serde_json::to_string(input).expect("encode input")
    }

    /// One `tool_call` part with a double-encoded `input` (as Crush stores it).
    fn tool_call(id: &str, name: &str, input: &str) -> String {
        format!(
            r#"{{"type":"tool_call","data":{{"id":"{id}","name":"{name}","input":{},"finished":true}}}}"#,
            encode(input)
        )
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> String {
        format!(
            r#"{{"type":"tool_result","data":{{"tool_call_id":"{id}","content":{},"is_error":{is_error}}}}}"#,
            encode(content)
        )
    }

    fn tool_result_no_marker(id: &str, content: &str) -> String {
        format!(
            r#"{{"type":"tool_result","data":{{"tool_call_id":"{id}","content":{}}}}}"#,
            encode(content)
        )
    }

    fn shell_command(command: &str, output: &str, exit_code: i64) -> String {
        format!(
            r#"{{"type":"shell_command","data":{{"command":{},"output":{},"exit_code":{exit_code}}}}}"#,
            encode(command),
            encode(output)
        )
    }

    fn parts(elems: &[&str]) -> String {
        format!("[{}]", elems.join(","))
    }

    #[test]
    fn classifies_crush_tools_and_double_decodes_input() {
        let mut s = CrushScan::default();
        let msg = parts(&[
            &tool_call(
                "t1",
                "bash",
                r#"{"command":"cargo test","description":"run tests"}"#,
            ),
            &tool_call(
                "t2",
                "edit",
                r#"{"file_path":"/p/main.go","old_string":"a","new_string":"b"}"#,
            ),
            &tool_call("t3", "write", r#"{"file_path":"/p/new.go","content":"x"}"#),
            &tool_call("t4", "view", r#"{"file_path":"/p/README.md","limit":200}"#),
            &tool_call("t5", "grep", r#"{"pattern":"fn main","path":"/p"}"#),
            &tool_call(
                "t6",
                "web_search",
                r#"{"query":"ratatui table","max_results":10}"#,
            ),
            &tool_call(
                "t7",
                "fetch",
                r#"{"url":"https://example.com","format":"markdown"}"#,
            ),
            &tool_call(
                "t8",
                "agent",
                r#"{"prompt":"Search the codebase","subagent_type":"explore"}"#,
            ),
            &tool_call("t9", "todos", r#"{"todos":[]}"#),
            &tool_call("t10", "some_mcp_tool", r#"{"foo":"bar"}"#),
        ]);
        s.ingest_message(&msg, Some("anthropic/claude-opus"), Some(1_752_300_000_000));

        let kinds: Vec<ActionKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Command,
                ActionKind::Edit,
                ActionKind::Edit,
                ActionKind::Read,
                ActionKind::Search,
                ActionKind::WebSearch,
                ActionKind::WebFetch,
                ActionKind::Subagent,
                ActionKind::Other, // todos skipped
            ]
        );
        assert_eq!(s.events[0].detail, "cargo test");
        assert_eq!(s.events[0].note.as_deref(), Some("run tests"));
        assert_eq!(s.events[0].ts_ms, Some(1_752_300_000_000));
        assert_eq!(s.events[7].detail, "Search the codebase");
        assert_eq!(s.events[7].note.as_deref(), Some("explore"));
        assert_eq!(s.events[8].detail, "some_mcp_tool");
        assert!(s.events[8].note.as_deref().unwrap().contains("bar"));
        assert_eq!(s.meta.model.as_deref(), Some("anthropic/claude-opus"));
    }

    #[test]
    fn maps_web_file_and_search_variants() {
        let mut s = CrushScan::default();
        let msg = parts(&[
            &tool_call("a", "multiedit", r#"{"file_path":"/p/x.go","edits":[]}"#),
            &tool_call("b", "ls", r#"{"path":"/p/src"}"#),
            &tool_call("c", "glob", r#"{"pattern":"**/*.go","path":"/p"}"#),
            &tool_call(
                "d",
                "download",
                r#"{"url":"https://x/f.zip","file_path":"/p/f.zip"}"#,
            ),
            &tool_call("e", "agentic_fetch", r#"{"prompt":"read the docs"}"#),
            &tool_call("f", "sourcegraph", r#"{"query":"repo:foo bar"}"#),
            &tool_call("g", "web_fetch", r#"{"url":"https://y"}"#),
        ]);
        s.ingest_message(&msg, None, None);

        let kinds: Vec<ActionKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Edit,
                ActionKind::Search,
                ActionKind::Search,
                ActionKind::WebFetch,
                ActionKind::WebFetch,
                ActionKind::WebSearch,
                ActionKind::WebFetch,
            ]
        );
        assert_eq!(s.events[1].detail, "/p/src"); // ls → path
        assert_eq!(s.events[3].detail, "https://x/f.zip"); // download → url
        assert_eq!(s.events[3].note.as_deref(), Some("/p/f.zip"));
        assert_eq!(s.events[4].detail, "read the docs"); // agentic_fetch → prompt fallback
    }

    #[test]
    fn tool_result_patches_ok_and_head_by_id() {
        let mut s = CrushScan::default();
        s.ingest_message(
            &parts(&[&tool_call("t1", "bash", r#"{"command":"ls"}"#)]),
            None,
            None,
        );
        assert_eq!(s.events[0].ok, None); // pending
        s.ingest_message(
            &parts(&[&tool_result("t1", "total 8\nmain.go", false)]),
            None,
            None,
        );
        assert_eq!(s.events[0].ok, Some(true));
        assert!(s.events[0]
            .result_head
            .as_deref()
            .unwrap()
            .contains("main.go"));

        s.ingest_message(
            &parts(&[&tool_call("t2", "view", r#"{"file_path":"/gone"}"#)]),
            None,
            None,
        );
        s.ingest_message(
            &parts(&[&tool_result("t2", "no such file", true)]),
            None,
            None,
        );
        assert_eq!(s.events[1].ok, Some(false));
        assert_eq!(s.events[1].result_head.as_deref(), Some("no such file"));
    }

    #[test]
    fn tool_result_without_error_marker_keeps_ok_unknown() {
        let mut s = CrushScan::default();
        s.ingest_message(
            &parts(&[&tool_call("t1", "view", r#"{"file_path":"/a.rs"}"#)]),
            None,
            None,
        );
        s.ingest_message(
            &parts(&[&tool_result_no_marker("t1", "fn a() {}")]),
            None,
            None,
        );
        assert_eq!(s.events[0].ok, None);
        assert!(s.events[0].result_head.is_some());
    }

    #[test]
    fn user_shell_command_is_a_command_with_exit_code() {
        let mut s = CrushScan::default();
        s.ingest_message(
            &parts(&[&shell_command("git status", "On branch main", 0)]),
            None,
            None,
        );
        assert_eq!(s.events[0].kind, ActionKind::Command);
        assert_eq!(s.events[0].detail, "git status");
        assert_eq!(s.events[0].ok, Some(true));
        assert_eq!(s.events[0].note.as_deref(), Some("manual run"));
        assert!(s.events[0].result_head.is_some());

        s.ingest_message(&parts(&[&shell_command("false", "", 1)]), None, None);
        assert_eq!(s.events[1].ok, Some(false));
        assert_eq!(s.events[1].result_head, None); // empty output stays unset
    }

    #[test]
    fn malformed_parts_and_inputs_are_skipped() {
        let mut s = CrushScan::default();
        s.ingest_message("not json", None, None);
        s.ingest_message(r#"{"type":"tool_call"}"#, None, None); // object, not an array
        let msg = parts(&[
            &tool_call("x", "bash", r#"{"command""#), // truncated input JSON → no command
            &tool_call("y", "bash", r#"{"command":"ok"}"#),
            r#"{"type":"reasoning","data":{"text":"thinking"}}"#, // ignored part type
            r#"{"type":"tool_call","data":{"name":"bash"}}"#,     // no input → no command
            r#"{"garbage":true}"#,                                // no type
        ]);
        s.ingest_message(&msg, None, None);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].detail, "ok");
    }

    #[test]
    fn session_meta_extracts_title_and_tokens() {
        let m = session_meta(Some("Add activity tab"), Some(3200));
        assert_eq!(m.title.as_deref(), Some("Add activity tab"));
        assert_eq!(m.output_tokens, Some(3200));
        assert_eq!(m.model, None);

        // Blank title dropped; a nonsensical negative token count is ignored.
        let m = session_meta(Some("   "), Some(-5));
        assert_eq!(m.title, None);
        assert_eq!(m.output_tokens, None);
    }
}
