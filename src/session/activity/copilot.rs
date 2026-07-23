//! GitHub Copilot CLI activity provider — normalizes a session's
//! `events.jsonl` (+ sidecar `workspace.yaml`) under
//! `~/.copilot/session-state/<session-id>/` into [`ActivityEvent`]s.
//!
//! `events.jsonl` is an append-mostly transcript: one `SessionEvent` per line
//! (`{id, parentId, timestamp, type, data, agentId?}`). Every tool the agent
//! runs is a `tool.execution_start` (carrying `toolName` + `arguments`) paired
//! by `data.toolCallId` with a later `tool.execution_complete` (carrying
//! `success` + `result`) — so [`CopilotScan`] is a streaming accumulator like
//! [`super::claude::ClaudeScan`]: feed it line-aligned chunks in file order and
//! it appends events and patches outcomes as the completions arrive. Events
//! emitted by a sub-agent carry a top-level `agentId` (absent for the root
//! agent), used to attribute their origin.
//!
//! `tool_name` classification mirrors Copilot's own category mapper (`p_i` in
//! the bundled `app.js`): shell (`bash`/`local_shell`/`stop_bash`), edits
//! (`edit`/`create`/`str_replace`/`str_replace_editor`/`insert`/`apply_patch`/
//! `write`/`write_bash`/`delete`/`move`), reads (`read`/`view`), searches
//! (`grep`/`glob`/`rg`), web (`web_search`/`web_fetch`), and the subagent
//! `task` tool. The `think` category (`update_todo`/`report_progress`) is pure
//! plan bookkeeping and is skipped. Copilot has no reliable MCP-name marker on
//! disk, so unrecognized tools degrade to `Other` (with a compact input note)
//! rather than a guessed `Mcp`.
//!
//! The transcript's title lives only in `workspace.yaml`/the session DB, never
//! in `events.jsonl` (`session.title_changed` is `ephemeral`), so the event
//! scan derives a fallback title from the first user prompt and the app layer
//! overrides it with [`parse_workspace`]'s `name` when present.
//!
//! Verified against `@github/copilot` v1.0.70 (bundled `app.js` +
//! `sdk/index.d.ts`, session event schema v6). Defensive like every provider:
//! unknown shapes are skipped, never errors.

use std::collections::HashMap;

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for compact-input notes and prompt-derived titles.
const NOTE_MAX: usize = 160;

/// Copilot's `think` category — plan bookkeeping that records no action on the
/// world, skipped so the timeline shows work, not plan churn (mirrors `p_i`).
const SKIPPED_TOOLS: &[&str] = &["update_todo", "report_progress"];

/// Streaming scanner over one Copilot `events.jsonl`. Same contract as
/// [`super::claude::ClaudeScan`]: feed line-aligned chunks in file order, read
/// [`events`](Self::events) / [`meta`](Self::meta) between feeds.
#[derive(Debug, Clone, Default)]
pub struct CopilotScan {
    pub events: Vec<ActivityEvent>,
    pub meta: ActivityMeta,
    /// `tool.execution_start` `toolCallId` → index into [`Self::events`],
    /// awaiting its `tool.execution_complete`.
    pending: HashMap<String, usize>,
    /// `agentId` → sub-agent display label, learned from `subagent.started`,
    /// used to name the origin of events that sub-agent later emits.
    agents: HashMap<String, String>,
}

impl CopilotScan {
    /// Ingest the next line-aligned chunk of `events.jsonl`.
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
                Some("session.start") => {
                    if let Some(m) = v.pointer("/data/selectedModel").and_then(|m| m.as_str()) {
                        self.meta.model = Some(m.to_string());
                    }
                }
                Some("user.message") => {
                    if self.meta.title.is_none() {
                        if let Some(c) = v.pointer("/data/content").and_then(|c| c.as_str()) {
                            let c = c.trim();
                            if !c.is_empty() {
                                self.meta.title = Some(head(c, NOTE_MAX));
                            }
                        }
                    }
                }
                Some("assistant.message") => {
                    if let Some(m) = v.pointer("/data/model").and_then(|m| m.as_str()) {
                        self.meta.model = Some(m.to_string());
                    }
                    if let Some(t) = v.pointer("/data/outputTokens").and_then(|t| t.as_u64()) {
                        self.meta.output_tokens = Some(self.meta.output_tokens.unwrap_or(0) + t);
                    }
                }
                Some("tool.execution_start") => self.ingest_start(&v),
                Some("tool.execution_complete") => self.ingest_complete(&v),
                Some("subagent.started") => self.ingest_subagent(&v),
                _ => {}
            }
        }
    }

    fn ingest_start(&mut self, entry: &serde_json::Value) {
        let Some(data) = entry.get("data") else {
            return;
        };
        let Some(name) = str_field(data, "toolName") else {
            return;
        };
        if SKIPPED_TOOLS.contains(&name.as_str()) {
            return;
        }
        let Some(mut event) = classify(&name, data.get("arguments")) else {
            return;
        };
        event.ts_ms = ts_ms(entry);
        event.origin = self.origin_of(entry);
        if let Some(id) = str_field(data, "toolCallId") {
            self.pending.insert(id, self.events.len());
        }
        self.events.push(event);
    }

    fn ingest_complete(&mut self, entry: &serde_json::Value) {
        let Some(data) = entry.get("data") else {
            return;
        };
        let Some(id) = str_field(data, "toolCallId") else {
            return;
        };
        let Some(idx) = self.pending.remove(&id) else {
            return;
        };
        let Some(event) = self.events.get_mut(idx) else {
            return;
        };
        // `success` is Copilot's explicit outcome marker; absent → stays unknown.
        if let Some(ok) = data.get("success").and_then(|s| s.as_bool()) {
            event.ok = Some(ok);
        }
        let text = result_text(data.get("result"));
        let text = text.trim();
        if !text.is_empty() {
            event.result_head = Some(head(text, RESULT_HEAD_MAX));
        }
    }

    /// A `subagent.started` names a sub-agent whose id tags every event it
    /// later emits — remember the label so those events attribute back to it.
    fn ingest_subagent(&mut self, entry: &serde_json::Value) {
        let Some(id) = str_field(entry, "agentId") else {
            return;
        };
        let label = entry
            .pointer("/data/agentDisplayName")
            .and_then(|n| n.as_str())
            .or_else(|| entry.pointer("/data/agentName").and_then(|n| n.as_str()))
            .filter(|n| !n.trim().is_empty())
            .map(String::from)
            .unwrap_or_else(|| "subagent".to_string());
        self.agents.insert(id, label);
    }

    /// Origin label for an event: `None` on the root thread, else the
    /// sub-agent's known display name (or a generic `subagent` before its
    /// `subagent.started` was seen).
    fn origin_of(&self, entry: &serde_json::Value) -> Option<String> {
        let id = str_field(entry, "agentId")?;
        Some(
            self.agents
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "subagent".to_string()),
        )
    }
}

/// Map one `tool.execution_start` to an event. `None` ⇒ the tool is malformed
/// for its family (e.g. a shell call without `command`) and is skipped.
fn classify(name: &str, arguments: Option<&serde_json::Value>) -> Option<ActivityEvent> {
    let args = arguments.unwrap_or(&serde_json::Value::Null);
    let get = |key: &str| args.get(key).and_then(|x| x.as_str()).map(String::from);

    let (kind, detail, note) = match name {
        "bash" | "local_shell" | "stop_bash" => {
            (ActionKind::Command, get("command")?, get("description"))
        }
        "edit" | "create" | "str_replace" | "str_replace_editor" | "insert" | "apply_patch"
        | "write" | "write_bash" | "delete" | "move" => (ActionKind::Edit, get("path")?, None),
        "read" | "view" => (ActionKind::Read, get("path")?, None),
        "grep" | "glob" | "rg" => (ActionKind::Search, get("pattern")?, get("path")),
        "web_search" => (ActionKind::WebSearch, get("query")?, None),
        "web_fetch" => (ActionKind::WebFetch, get("url")?, None),
        "task" => {
            let detail = get("description")
                .or_else(|| get("prompt").map(|p| head(&p, NOTE_MAX)))
                .or_else(|| get("agentName"))
                .unwrap_or_else(|| name.to_string());
            (
                ActionKind::Subagent,
                detail,
                get("agentName").or_else(|| get("agent")),
            )
        }
        // Unknown / skill / MCP tools: keep them on the timeline as `Other`
        // with a compact input note — no dependable MCP marker to key on.
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

/// Best result text from a `tool.execution_complete` `data.result`: the
/// top-level `content` string, else the concatenated `outputPreview`s of the
/// structured `contents[]` (e.g. a `shell_exit` entry).
fn result_text(result: Option<&serde_json::Value>) -> String {
    let Some(result) = result else {
        return String::new();
    };
    if let Some(s) = result.get("content").and_then(|c| c.as_str()) {
        return s.to_string();
    }
    let Some(items) = result.get("contents").and_then(|c| c.as_array()) else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|it| it.get("outputPreview").and_then(|p| p.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One-line compact rendering of a tool's arguments for `note`, capped.
fn compact_input(args: &serde_json::Value) -> Option<String> {
    if args.is_null() || args.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    Some(head(&args.to_string(), NOTE_MAX))
}

/// Epoch ms from an event's ISO-8601 top-level `timestamp`, when valid.
fn ts_ms(entry: &serde_json::Value) -> Option<u64> {
    let ts = entry.get("timestamp")?.as_str()?;
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    u64::try_from(dt.timestamp_millis()).ok()
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

/// The `workspace.yaml` fields the app layer needs to attribute a session: its
/// id, working directory (the cwd-match key), optional display title, and
/// start time (the recency key for newest-first discovery).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CopilotWorkspace {
    pub session_id: Option<String>,
    /// `cwd` — the launch-dir match key.
    pub cwd: Option<String>,
    /// `name` — the user/auto display title, absent on unnamed sessions.
    pub title: Option<String>,
    /// `created_at` (ISO-8601) as epoch ms.
    pub created_ms: Option<u64>,
}

/// Parse a Copilot `workspace.yaml`. It is a flat map of scalar `key: value`
/// pairs (`id`, `cwd`, `name`, `created_at`, …), so a minimal top-level line
/// scan avoids a YAML dependency; indented/nested lines and unknown keys are
/// ignored. Missing/garbled input yields defaults.
pub fn parse_workspace(s: &str) -> CopilotWorkspace {
    let mut ws = CopilotWorkspace::default();
    for line in s.lines() {
        // Only top-level scalars — a leading space means a nested mapping the
        // flat scan doesn't model.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = yaml_scalar(value);
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "id" => ws.session_id = Some(value.to_string()),
            "cwd" => ws.cwd = Some(value.to_string()),
            "name" => ws.title = Some(value.to_string()),
            "created_at" => {
                ws.created_ms = chrono::DateTime::parse_from_rfc3339(value)
                    .ok()
                    .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok());
            }
            _ => {}
        }
    }
    ws
}

/// Trim a scalar YAML value: strip surrounding whitespace, a trailing `#`
/// comment on unquoted values, and matching single/double quotes.
fn yaml_scalar(raw: &str) -> &str {
    let mut v = raw.trim();
    if let Some(inner) = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
    {
        return inner;
    }
    if let Some((before, _)) = v.split_once(" #") {
        v = before.trim_end();
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> CopilotScan {
        let mut scan = CopilotScan::default();
        scan.ingest(s);
        scan
    }

    #[test]
    fn classifies_builtin_tools() {
        let jsonl = [
            r#"{"type":"tool.execution_start","timestamp":"2026-07-12T03:29:25.00Z","data":{"toolCallId":"call_01","toolName":"bash","arguments":{"command":"cargo test","description":"Run the test suite"}}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_02","toolName":"str_replace","arguments":{"command":"str_replace","path":"/work/src/net.rs","old_str":"a","new_str":"b"}}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_02b","toolName":"create","arguments":{"command":"create","path":"/work/src/new.rs","file_text":"x"}}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_03","toolName":"read","arguments":{"path":"/work/src/net.rs","view_range":[1,80]}}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_03b","toolName":"grep","arguments":{"pattern":"fetch","path":"/work/src"}}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_04","toolName":"web_search","arguments":{"query":"rust reqwest retry"}}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_05","toolName":"web_fetch","arguments":{"url":"https://docs.rs/reqwest"}}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_06","toolName":"task","arguments":{"description":"Review the diff","agentName":"reviewer"}}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_07","toolName":"update_todo","arguments":{"todos":[]}}}"#,
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_08","toolName":"get_pull_request","arguments":{"number":7}}}"#,
        ]
        .join("\n");
        let s = scan(&jsonl);
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
                // update_todo skipped (plan bookkeeping)
                ActionKind::Other,
            ]
        );
        assert_eq!(s.events[0].detail, "cargo test");
        assert_eq!(s.events[0].note.as_deref(), Some("Run the test suite"));
        assert_eq!(s.events[0].ts_ms, Some(1783826965000)); // 2026-07-12T03:29:25Z
        assert_eq!(s.events[1].detail, "/work/src/net.rs");
        assert_eq!(s.events[4].detail, "fetch");
        assert_eq!(s.events[4].note.as_deref(), Some("/work/src"));
        assert_eq!(s.events[7].detail, "Review the diff");
        assert_eq!(s.events[7].note.as_deref(), Some("reviewer"));
        assert_eq!(s.events[8].detail, "get_pull_request"); // unknown → Other
        assert!(s.events[8].note.as_deref().unwrap().contains("number"));
    }

    #[test]
    fn completion_patches_ok_and_head_across_chunks() {
        let mut s = CopilotScan::default();
        s.ingest(
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_01","toolName":"bash","arguments":{"command":"cargo test --all"}}}"#,
        );
        assert_eq!(s.events[0].ok, None); // pending

        // Success carries the head in `result.contents[].outputPreview`.
        s.ingest(
            r#"{"type":"tool.execution_complete","data":{"toolCallId":"call_01","success":true,"result":{"contents":[{"type":"shell_exit","exitCode":0,"outputPreview":"test result: ok. 42 passed"}]}}}"#,
        );
        assert_eq!(s.events[0].ok, Some(true));
        assert_eq!(
            s.events[0].result_head.as_deref(),
            Some("test result: ok. 42 passed")
        );

        // Failure with a top-level `result.content` string.
        s.ingest(
            r#"{"type":"tool.execution_start","data":{"toolCallId":"call_02","toolName":"read","arguments":{"path":"/gone"}}}"#,
        );
        s.ingest(
            r#"{"type":"tool.execution_complete","data":{"toolCallId":"call_02","success":false,"result":{"content":"no such file"}}}"#,
        );
        assert_eq!(s.events[1].ok, Some(false));
        assert_eq!(s.events[1].result_head.as_deref(), Some("no such file"));
    }

    #[test]
    fn subagent_events_attribute_to_their_agent() {
        let s = scan(
            &[
                r#"{"type":"subagent.started","agentId":"agent_2","data":{"agentName":"reviewer","agentDisplayName":"Code Reviewer"}}"#,
                r#"{"type":"tool.execution_start","agentId":"agent_2","data":{"toolCallId":"c1","toolName":"read","arguments":{"path":"/work/src/net.rs"}}}"#,
                r#"{"type":"tool.execution_start","data":{"toolCallId":"c2","toolName":"bash","arguments":{"command":"pwd"}}}"#,
            ]
            .join("\n"),
        );
        assert_eq!(s.events[0].origin.as_deref(), Some("Code Reviewer"));
        assert_eq!(s.events[1].origin, None); // root thread
    }

    #[test]
    fn meta_collects_title_model_and_summed_tokens() {
        let s = scan(
            &[
                r#"{"type":"session.start","data":{"sessionId":"e57","selectedModel":"claude-sonnet-4.5"}}"#,
                r#"{"type":"user.message","data":{"content":"add a retry to fetch()","agentMode":"agent"}}"#,
                r#"{"type":"assistant.message","data":{"model":"claude-sonnet-4.5","outputTokens":412}}"#,
                r#"{"type":"assistant.message","data":{"outputTokens":88}}"#,
            ]
            .join("\n"),
        );
        assert_eq!(s.meta.title.as_deref(), Some("add a retry to fetch()"));
        assert_eq!(s.meta.model.as_deref(), Some("claude-sonnet-4.5"));
        assert_eq!(s.meta.output_tokens, Some(500));
    }

    #[test]
    fn malformed_and_unmatched_lines_are_skipped() {
        let mut s = CopilotScan::default();
        s.ingest(
            &[
                "not json",
                r#"{"type":"tool.execution_start","data":{"toolCallId":"c1","toolName":"bash","arguments":{}}}"#, // no command → skipped
                r#"{"type":"tool.execution_start","data":{"toolName":"read"}}"#, // no path → skipped
                r#"{"type":"tool.execution_complete","data":{"toolCallId":"nope","success":true}}"#, // no pending
                r#"{"type":"session.idle"}"#,
                r#"{"type":"tool.execution_start","data":{"toolCallId":"c2","toolName":"bash","arguments":{"command":"ls"}}}"#,
            ]
            .join("\n"),
        );
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].detail, "ls");
    }

    #[test]
    fn workspace_extracts_id_cwd_title_and_created() {
        let ws = parse_workspace(
            "id: e57ef7a5-9452-4a54-b182-8d18f1058e94\n\
             cwd: /work\n\
             client_name: github/cli\n\
             user_named: true\n\
             name: Add retry to fetch()\n\
             created_at: 2026-07-12T03:29:17.731Z\n\
             updated_at: 2026-07-12T03:29:17.993Z\n",
        );
        assert_eq!(
            ws.session_id.as_deref(),
            Some("e57ef7a5-9452-4a54-b182-8d18f1058e94")
        );
        assert_eq!(ws.cwd.as_deref(), Some("/work"));
        assert_eq!(ws.title.as_deref(), Some("Add retry to fetch()"));
        assert!(ws.created_ms.is_some());
    }

    #[test]
    fn workspace_ignores_nesting_and_quotes_and_garbage() {
        let ws = parse_workspace(
            "id: \"quoted-id\"\n\
             cwd: '/home/me/proj'\n\
             environment:\n  \
             cwd: /should/not/win\n\
             name: \n",
        );
        assert_eq!(ws.session_id.as_deref(), Some("quoted-id"));
        // The nested `cwd` under `environment:` is indented → ignored.
        assert_eq!(ws.cwd.as_deref(), Some("/home/me/proj"));
        // An empty `name:` yields no title.
        assert_eq!(ws.title, None);

        assert_eq!(parse_workspace(""), CopilotWorkspace::default());
    }
}
