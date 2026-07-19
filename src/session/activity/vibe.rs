//! Mistral Vibe activity provider — normalizes a session's `messages.jsonl`
//! (+ sidecar `meta.json`) under
//! `~/.vibe/logs/session/<prefix>_<utc-ts>_<shortid>/` into [`ActivityEvent`]s.
//!
//! One `LLMMessage` per line: `assistant` lines carry `tool_calls[]` (each
//! with `function.name` + `function.arguments`, the latter a JSON **string**
//! to double-parse), `tool` lines carry the result, correlated by
//! `tool_call_id`. Lines have no timestamps (only session-level start/end in
//! `meta.json`), so events order by stream position. The file is appended
//! per turn (tailable); a rewind/compact rewrites it atomically, which the
//! app-layer tailer detects as truncation and re-feeds from scratch.
//!
//! Verified against mistral-vibe v2.19.1 source (session_logger/loader,
//! core/types.py, tools/builtins/*). Defensive like every provider: unknown
//! shapes are skipped, never errors.

use std::collections::HashMap;

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for compact-input notes.
const NOTE_MAX: usize = 160;

/// Streaming scanner over one Vibe `messages.jsonl`. Same contract as
/// [`super::claude::ClaudeScan`]: feed line-aligned chunks in file order.
#[derive(Debug, Clone, Default)]
pub struct VibeScan {
    pub events: Vec<ActivityEvent>,
    /// `tool_calls[].id` → index into [`Self::events`], awaiting its result.
    pending: HashMap<String, usize>,
}

impl VibeScan {
    /// Ingest the next line-aligned chunk of `messages.jsonl`.
    pub fn ingest(&mut self, chunk: &str) {
        for line in chunk.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            match v.get("role").and_then(|r| r.as_str()) {
                Some("assistant") => self.ingest_assistant(&v),
                Some("tool") => self.ingest_tool_result(&v),
                _ => {}
            }
        }
    }

    fn ingest_assistant(&mut self, msg: &serde_json::Value) {
        let Some(calls) = msg.get("tool_calls").and_then(|c| c.as_array()) else {
            return;
        };
        for call in calls {
            let Some(name) = call.pointer("/function/name").and_then(|n| n.as_str()) else {
                continue;
            };
            // `arguments` is the model's raw output re-encoded as a JSON
            // string — best-effort double-parse.
            let args: serde_json::Value = call
                .pointer("/function/arguments")
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Null);
            let Some(event) = classify(name, &args) else {
                continue;
            };
            if let Some(id) = call.get("id").and_then(|i| i.as_str()) {
                self.pending.insert(id.to_string(), self.events.len());
            }
            self.events.push(event);
        }
    }

    fn ingest_tool_result(&mut self, msg: &serde_json::Value) {
        let Some(id) = msg.get("tool_call_id").and_then(|i| i.as_str()) else {
            return;
        };
        let Some(idx) = self.pending.remove(id) else {
            return;
        };
        let Some(event) = self.events.get_mut(idx) else {
            return;
        };
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        // The bash result text embeds the exit status as a `returncode: N`
        // line (BashResult rendered to text); other tools carry no explicit
        // success marker, so their `ok` stays unknown.
        if event.kind == ActionKind::Command {
            event.ok = returncode(content).map(|rc| rc == 0);
        }
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            event.result_head = Some(head(trimmed, RESULT_HEAD_MAX));
        }
    }
}

/// Map one tool call to an event. Tool names are the snake_cased builtin
/// classes (`bash`, `read_file`, `write_file`, `edit`, `grep`, `web_search`,
/// `web_fetch`, `task`, …); an experimental/managed shell surfaces as a
/// `*_bash` variant, so the shell match is by family.
fn classify(name: &str, args: &serde_json::Value) -> Option<ActivityEvent> {
    let get = |key: &str| args.get(key).and_then(|x| x.as_str()).map(String::from);

    let (kind, detail, note) = if name == "bash" || name.ends_with("_bash") {
        (ActionKind::Command, get("command")?, None)
    } else {
        match name {
            "write_file" | "edit" => (ActionKind::Edit, get("file_path")?, None),
            "read_file" => (ActionKind::Read, get("file_path")?, None),
            "grep" => (ActionKind::Search, get("pattern")?, get("path")),
            "web_search" => (ActionKind::WebSearch, get("query")?, None),
            "web_fetch" => (ActionKind::WebFetch, get("url")?, None),
            "task" => (
                ActionKind::Subagent,
                get("task").map(|t| head(&t, NOTE_MAX))?,
                get("agent"),
            ),
            // Plan/bookkeeping tools record no action on the world.
            "todo" | "exit_plan_mode" => return None,
            _ => {
                let note = (!args.is_null()).then(|| head(&args.to_string(), NOTE_MAX));
                (ActionKind::Other, name.to_string(), note)
            }
        }
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

/// The `returncode: N` line of a rendered bash result.
fn returncode(content: &str) -> Option<i64> {
    content
        .lines()
        .find_map(|l| l.strip_prefix("returncode:"))
        .and_then(|n| n.trim().parse().ok())
}

/// Session metadata from a Vibe `meta.json` (title, model alias, completion
/// tokens) plus the fields the app layer needs to attribute the session
/// (working directory, ids, start time).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VibeMeta {
    pub meta: ActivityMeta,
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    /// `environment.working_directory` — the cwd match key.
    pub working_directory: Option<String>,
    /// `start_time` (ISO-8601) as epoch ms.
    pub start_ms: Option<u64>,
}

/// Parse a `meta.json`. Missing/garbled input yields defaults.
pub fn parse_meta(s: &str) -> VibeMeta {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else {
        return VibeMeta::default();
    };
    let str_at = |ptr: &str| v.pointer(ptr).and_then(|x| x.as_str()).map(String::from);
    VibeMeta {
        meta: ActivityMeta {
            title: str_at("/title"),
            model: str_at("/config/active_model"),
            output_tokens: v
                .pointer("/stats/session_completion_tokens")
                .and_then(|t| t.as_u64()),
        },
        session_id: str_at("/session_id"),
        parent_session_id: str_at("/parent_session_id"),
        working_directory: str_at("/environment/working_directory"),
        start_ms: str_at("/start_time")
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
            .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_call(id: &str, name: &str, args: &str) -> String {
        format!(
            r#"{{"role":"assistant","content":"","injected":false,"tool_calls":[{{"id":"{id}","index":0,"function":{{"name":"{name}","arguments":"{}"}},"type":"function"}}],"message_id":"m"}}"#,
            args.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }

    #[test]
    fn classifies_vibe_tools_and_parses_double_encoded_args() {
        let mut s = VibeScan::default();
        s.ingest(
            &[
                assistant_call("c1", "bash", r#"{"command": "ls -la", "timeout": null}"#),
                assistant_call(
                    "c2",
                    "write_file",
                    r#"{"file_path": "/p/foo.py", "content": "x"}"#,
                ),
                assistant_call(
                    "c3",
                    "edit",
                    r#"{"file_path": "/p/main.py", "old_string": "a", "new_string": "b"}"#,
                ),
                assistant_call(
                    "c4",
                    "read_file",
                    r#"{"file_path": "/p/main.py", "offset": 1}"#,
                ),
                assistant_call("c5", "web_search", r#"{"query": "ratatui table"}"#),
                assistant_call("c6", "web_fetch", r#"{"url": "https://docs.rs/ratatui"}"#),
                assistant_call(
                    "c7",
                    "task",
                    r#"{"task": "Find call sites", "agent": "explore"}"#,
                ),
                assistant_call("c8", "todo", r#"{"items": []}"#),
                assistant_call("c9", "grep", r#"{"pattern": "fn main", "path": "/p"}"#),
            ]
            .join("\n"),
        );
        let kinds: Vec<ActionKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Command,
                ActionKind::Edit,
                ActionKind::Edit,
                ActionKind::Read,
                ActionKind::WebSearch,
                ActionKind::WebFetch,
                ActionKind::Subagent,
                ActionKind::Search, // todo skipped
            ]
        );
        assert_eq!(s.events[0].detail, "ls -la");
        assert_eq!(s.events[6].note.as_deref(), Some("explore"));
        assert_eq!(s.events[7].note.as_deref(), Some("/p"));
    }

    #[test]
    fn bash_result_patches_ok_from_returncode() {
        let mut s = VibeScan::default();
        s.ingest(&assistant_call("c1", "bash", r#"{"command": "make"}"#));
        s.ingest(
            r#"{"role":"tool","content":"command: make\nstdout: built\nstderr: \nreturncode: 0","injected":false,"name":"bash","tool_call_id":"c1"}"#,
        );
        assert_eq!(s.events[0].ok, Some(true));
        assert!(s.events[0]
            .result_head
            .as_deref()
            .unwrap()
            .contains("built"));

        s.ingest(&assistant_call("c2", "bash", r#"{"command": "false"}"#));
        s.ingest(
            r#"{"role":"tool","content":"command: false\nstdout: \nstderr: \nreturncode: 1","injected":false,"name":"bash","tool_call_id":"c2"}"#,
        );
        assert_eq!(s.events[1].ok, Some(false));
    }

    #[test]
    fn non_bash_results_keep_ok_unknown() {
        let mut s = VibeScan::default();
        s.ingest(&assistant_call(
            "c1",
            "read_file",
            r#"{"file_path": "/p/a.rs"}"#,
        ));
        s.ingest(
            r#"{"role":"tool","content":"file_path: /p/a.rs\ncontent: fn a(){}","injected":false,"name":"read_file","tool_call_id":"c1"}"#,
        );
        assert_eq!(s.events[0].ok, None);
        assert!(s.events[0].result_head.is_some());
    }

    #[test]
    fn experimental_bash_family_matches() {
        let mut s = VibeScan::default();
        s.ingest(&assistant_call(
            "c1",
            "experimental_bash",
            r#"{"command": "pwd"}"#,
        ));
        assert_eq!(s.events[0].kind, ActionKind::Command);
        assert_eq!(s.events[0].detail, "pwd");
    }

    #[test]
    fn malformed_lines_and_args_are_skipped() {
        let mut s = VibeScan::default();
        s.ingest(&format!(
            "not json\n{}\n{}",
            // Arguments string that is not valid JSON → no command → skipped.
            assistant_call("c1", "bash", r#"{"command""#),
            assistant_call("c2", "bash", r#"{"command": "ok"}"#),
        ));
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].detail, "ok");
    }

    #[test]
    fn meta_extracts_title_model_tokens_cwd_and_start() {
        let m = parse_meta(
            r#"{
              "session_id":"a1b2c3d4-1111-2222-3333-abcdef012345",
              "parent_session_id":null,
              "start_time":"2026-07-12T14:30:22.123456+00:00",
              "environment":{"working_directory":"/home/user/project"},
              "title":"Refactor the main function","title_source":"auto",
              "stats":{"session_prompt_tokens":18234,"session_completion_tokens":2811},
              "config":{"active_model":"mistral-medium-3.5"}
            }"#,
        );
        assert_eq!(m.meta.title.as_deref(), Some("Refactor the main function"));
        assert_eq!(m.meta.model.as_deref(), Some("mistral-medium-3.5"));
        assert_eq!(m.meta.output_tokens, Some(2811));
        assert_eq!(m.working_directory.as_deref(), Some("/home/user/project"));
        assert_eq!(
            m.session_id.as_deref(),
            Some("a1b2c3d4-1111-2222-3333-abcdef012345")
        );
        assert_eq!(m.parent_session_id, None);
        assert!(m.start_ms.is_some());

        assert_eq!(parse_meta("garbage"), VibeMeta::default());
    }
}
