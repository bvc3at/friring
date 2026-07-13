//! Claude Code activity provider — normalizes a conversation transcript
//! (`~/.claude/projects/<slug>/<session-id>.jsonl`) into [`ActivityEvent`]s.
//!
//! The transcript is append-only JSONL: `assistant` lines carry `tool_use`
//! content blocks (the actions), `user` lines carry the matching
//! `tool_result` blocks (outcome), correlated by `tool_use_id`. A result can
//! land many lines — or, on a live tail, many *reads* — after its call, so
//! [`ClaudeScan`] is a streaming accumulator: feed it line-aligned chunks in
//! file order and it appends events and patches outcomes as they arrive.
//!
//! Same defensiveness contract as [`crate::session::cc_activity`]: the format
//! is undocumented and version-specific (verified against v2.1.20x), so
//! unrecognized shapes are skipped, never errors.

use std::collections::HashMap;

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for compact-input notes (`Other`/`Mcp` events).
const NOTE_MAX: usize = 160;

/// Pure-bookkeeping tools that record no action on the world — skipped
/// entirely so the timeline shows work, not plan churn.
const SKIPPED_TOOLS: &[&str] = &[
    "TodoWrite",
    "BashOutput",
    "TaskOutput",
    "TaskList",
    "TaskGet",
];

/// Streaming scanner over one Claude Code transcript. Feed complete lines via
/// [`ingest`](Self::ingest); read the accumulated [`events`](Self::events)
/// and [`meta`](Self::meta) between feeds. Callers own chunking: a chunk must
/// end on a line boundary (feed up to the last `\n`; a torn tail line would
/// be skipped as malformed and its events lost).
#[derive(Debug, Clone, Default)]
pub struct ClaudeScan {
    pub events: Vec<ActivityEvent>,
    pub meta: ActivityMeta,
    /// `tool_use` id → index into [`Self::events`], awaiting its result.
    pending: HashMap<String, usize>,
    /// Streaming writes split one API message across JSONL lines sharing
    /// `message.id` with cumulative `usage` — track the last id and what it
    /// contributed so re-seen ids replace rather than double-count.
    last_msg_id: Option<String>,
    last_usage_added: u64,
}

impl ClaudeScan {
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
                Some("user") => self.ingest_user(&v),
                Some("summary") => {
                    // Claude Code's own conversation summary — the best title.
                    if let Some(s) = str_field(&v, "summary") {
                        self.meta.title = Some(s);
                    }
                }
                _ => {}
            }
        }
    }

    fn ingest_assistant(&mut self, entry: &serde_json::Value) {
        let ts = ts_ms(entry);
        let origin = origin_of(entry);
        let msg = entry.get("message");
        if let Some(m) = msg {
            if let Some(model) = str_field(m, "model") {
                self.meta.model = Some(model);
            }
            self.accumulate_usage(m);
        }
        let Some(content) = msg
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            return;
        };
        for block in content {
            if str_field(block, "type").as_deref() != Some("tool_use") {
                continue;
            }
            let Some(name) = str_field(block, "name") else {
                continue;
            };
            if SKIPPED_TOOLS.contains(&name.as_str()) {
                continue;
            }
            let Some(mut event) = classify(&name, block.get("input")) else {
                continue;
            };
            event.ts_ms = ts;
            event.origin = origin.clone();
            if let Some(id) = str_field(block, "id") {
                self.pending.insert(id, self.events.len());
            }
            self.events.push(event);
        }
    }

    fn ingest_user(&mut self, entry: &serde_json::Value) {
        // Title fallback: the first real typed prompt (a `summary` line wins).
        if self.meta.title.is_none() {
            if let Some(t) = crate::session::cc_activity::user_prompt_text(entry) {
                self.meta.title = Some(head(&t, NOTE_MAX));
            }
        }
        let Some(arr) = entry.pointer("/message/content").and_then(|c| c.as_array()) else {
            return;
        };
        for block in arr {
            if str_field(block, "type").as_deref() != Some("tool_result") {
                continue;
            }
            let Some(id) = str_field(block, "tool_use_id") else {
                continue;
            };
            let Some(&idx) = self.pending.get(&id) else {
                continue;
            };
            self.pending.remove(&id);
            let Some(event) = self.events.get_mut(idx) else {
                continue;
            };
            event.ok = Some(
                !block
                    .get("is_error")
                    .and_then(|e| e.as_bool())
                    .unwrap_or(false),
            );
            let mut text = crate::session::cc_activity::normalize_tool_result(block.get("content"));
            if text.trim().is_empty() {
                // Bash results often carry the useful text in the richer
                // top-level `toolUseResult` metadata instead.
                if let Some(s) = entry
                    .pointer("/toolUseResult/stdout")
                    .and_then(|s| s.as_str())
                {
                    text = s.to_string();
                }
            }
            let text = text.trim();
            if !text.is_empty() {
                event.result_head = Some(head(text, RESULT_HEAD_MAX));
            }
        }
    }

    fn accumulate_usage(&mut self, message: &serde_json::Value) {
        let Some(out_tokens) = message
            .pointer("/usage/output_tokens")
            .and_then(|t| t.as_u64())
        else {
            return;
        };
        let id = str_field(message, "id");
        let total = self.meta.output_tokens.unwrap_or(0);
        if id.is_some() && id == self.last_msg_id {
            // Same API message re-logged with updated cumulative usage.
            self.meta.output_tokens = Some(total - self.last_usage_added + out_tokens);
        } else {
            self.meta.output_tokens = Some(total + out_tokens);
            self.last_msg_id = id;
        }
        self.last_usage_added = out_tokens;
    }
}

/// Map one `tool_use` block to an event. `None` ⇒ the block is malformed for
/// its tool (e.g. a `Bash` without `command`) and is skipped.
fn classify(name: &str, input: Option<&serde_json::Value>) -> Option<ActivityEvent> {
    let input = input.unwrap_or(&serde_json::Value::Null);
    let get = |key: &str| input.get(key).and_then(|x| x.as_str()).map(String::from);

    let (kind, detail, note) = match name {
        "Bash" => (ActionKind::Command, get("command")?, get("description")),
        "Edit" | "Write" | "MultiEdit" => (ActionKind::Edit, get("file_path")?, None),
        "NotebookEdit" => (ActionKind::Edit, get("notebook_path")?, None),
        "Read" => (ActionKind::Read, get("file_path")?, None),
        "Grep" => (ActionKind::Search, get("pattern")?, get("path")),
        "Glob" => (ActionKind::Search, get("pattern")?, get("path")),
        "WebSearch" => (ActionKind::WebSearch, get("query")?, None),
        "WebFetch" => (ActionKind::WebFetch, get("url")?, get("prompt")),
        "Task" | "Agent" => {
            let detail =
                get("description").or_else(|| get("prompt").map(|p| head(&p, NOTE_MAX)))?;
            (ActionKind::Subagent, detail, get("subagent_type"))
        }
        _ => {
            let (kind, detail) = match mcp_name(name) {
                Some(pretty) => (ActionKind::Mcp, pretty),
                None => (ActionKind::Other, name.to_string()),
            };
            (kind, detail, compact_input(input))
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
    })
}

/// `mcp__server__tool` → `server:tool` (tool names may themselves contain
/// `__`, so only the first split separates server from tool).
fn mcp_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some(format!("{server}:{tool}"))
}

/// One-line compact rendering of a tool input for `note`, capped.
fn compact_input(input: &serde_json::Value) -> Option<String> {
    if input.is_null() || input.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    Some(head(&input.to_string(), NOTE_MAX))
}

/// Epoch ms from the entry's ISO-8601 `timestamp`, when present and valid.
fn ts_ms(entry: &serde_json::Value) -> Option<u64> {
    let ts = entry.get("timestamp")?.as_str()?;
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    u64::try_from(dt.timestamp_millis()).ok()
}

/// Sidechain lines in the main transcript are Task-subagent traffic (old
/// single-file layout); the file doesn't name the agent, so the origin label
/// is generic.
fn origin_of(entry: &serde_json::Value) -> Option<String> {
    (entry.get("isSidechain").and_then(|x| x.as_bool()) == Some(true))
        .then(|| "subagent".to_string())
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> ClaudeScan {
        let mut scan = ClaudeScan::default();
        scan.ingest(s);
        scan
    }

    #[test]
    fn classifies_builtin_tools() {
        // One transcript line (newlines only for readability — folded below).
        let jsonl = r#"{"type":"assistant","timestamp":"2026-07-08T12:00:00.000Z","message":{"content":[
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test","description":"Run tests"}},
                {"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/a.rs","old_string":"x","new_string":"y"}},
                {"type":"tool_use","id":"t3","name":"Read","input":{"file_path":"/b.rs"}},
                {"type":"tool_use","id":"t4","name":"WebSearch","input":{"query":"ratatui table"}},
                {"type":"tool_use","id":"t5","name":"WebFetch","input":{"url":"https://x.dev","prompt":"summarize"}},
                {"type":"tool_use","id":"t6","name":"Task","input":{"description":"Explore backend","subagent_type":"Explore","prompt":"..."}},
                {"type":"tool_use","id":"t7","name":"Grep","input":{"pattern":"fn main","path":"src/"}},
                {"type":"tool_use","id":"t8","name":"TodoWrite","input":{"todos":[]}},
                {"type":"tool_use","id":"t9","name":"mcp__github__create_issue","input":{"title":"bug"}}
            ]}}"#
            .replace('\n', " ");
        let s = scan(&jsonl);
        let kinds: Vec<ActionKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Command,
                ActionKind::Edit,
                ActionKind::Read,
                ActionKind::WebSearch,
                ActionKind::WebFetch,
                ActionKind::Subagent,
                ActionKind::Search,
                ActionKind::Mcp, // TodoWrite skipped
            ]
        );
        assert_eq!(s.events[0].detail, "cargo test");
        assert_eq!(s.events[0].note.as_deref(), Some("Run tests"));
        assert_eq!(s.events[0].ts_ms, Some(1783512000000)); // 2026-07-08T12:00Z
        assert_eq!(s.events[1].detail, "/a.rs");
        assert_eq!(s.events[5].note.as_deref(), Some("Explore"));
        assert_eq!(s.events[7].detail, "github:create_issue");
        assert!(s.events[7].note.as_deref().unwrap().contains("bug"));
    }

    #[test]
    fn results_patch_ok_and_head_across_chunks() {
        let mut s = ClaudeScan::default();
        s.ingest(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
        );
        assert_eq!(s.events[0].ok, None); // pending
        s.ingest(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"a.rs\nb.rs"}]}}"#,
        );
        assert_eq!(s.events[0].ok, Some(true));
        assert_eq!(s.events[0].result_head.as_deref(), Some("a.rs\nb.rs"));

        // Error result, content as block array.
        s.ingest(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/gone"}}]}}"#,
        );
        s.ingest(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":[{"type":"text","text":"no such file"}]}]}}"#,
        );
        assert_eq!(s.events[1].ok, Some(false));
        assert_eq!(s.events[1].result_head.as_deref(), Some("no such file"));
    }

    #[test]
    fn result_falls_back_to_tool_use_result_stdout() {
        let mut s = ClaudeScan::default();
        s.ingest(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make"}}]}}"#,
        );
        s.ingest(
            r#"{"type":"user","toolUseResult":{"stdout":"built ok","stderr":""},"message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":""}]}}"#,
        );
        assert_eq!(s.events[0].result_head.as_deref(), Some("built ok"));
    }

    #[test]
    fn sidechain_lines_get_subagent_origin() {
        let s = scan(
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"pwd"}}]}}"#,
        );
        assert_eq!(s.events[0].origin.as_deref(), Some("subagent"));
    }

    #[test]
    fn meta_collects_title_model_and_usage_without_double_count() {
        let mut s = ClaudeScan::default();
        s.ingest(concat!(
            r#"{"type":"user","message":{"role":"user","content":"Fix the flaky test"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","usage":{"output_tokens":10},"content":[{"type":"text","text":"ok"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","usage":{"output_tokens":25},"content":[{"type":"text","text":"more"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m2","usage":{"output_tokens":5},"content":[]}}"#,
        ));
        assert_eq!(s.meta.title.as_deref(), Some("Fix the flaky test"));
        assert_eq!(s.meta.model.as_deref(), Some("claude-opus-4-8"));
        // m1 counted once at its final value (25), plus m2's 5.
        assert_eq!(s.meta.output_tokens, Some(30));

        // A summary line overrides the prompt-derived title.
        s.ingest(r#"{"type":"summary","summary":"Flaky test fix"}"#);
        assert_eq!(s.meta.title.as_deref(), Some("Flaky test fix"));
    }

    #[test]
    fn malformed_and_unknown_lines_are_skipped() {
        let s = scan(concat!(
            "not json\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#, // no command
            "\n",
            r#"{"type":"system","subtype":"turn_duration"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"UnknownTool","input":{"x":1}}]}}"#,
        ));
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, ActionKind::Other);
        assert_eq!(s.events[0].detail, "UnknownTool");
    }

    #[test]
    fn mcp_names_split_server_and_tool() {
        assert_eq!(
            mcp_name("mcp__srv__do_thing").as_deref(),
            Some("srv:do_thing")
        );
        assert_eq!(
            mcp_name("mcp__srv__ns__deep").as_deref(),
            Some("srv:ns__deep")
        );
        assert_eq!(mcp_name("Bash"), None);
        assert_eq!(mcp_name("mcp__lonely"), None);
    }
}
