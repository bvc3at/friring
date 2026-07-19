//! goose (Block) activity provider — normalizes a session's rows from the
//! single SQLite `sessions.db` (`~/.local/share/goose/sessions/sessions.db`)
//! into [`ActivityEvent`]s.
//!
//! Unlike the per-cwd file layouts other agents use, goose keeps **one** global
//! DB for every project. A session is a row in `sessions` (keyed by a
//! `YYYYMMDD_N` id, with `working_dir` = cwd); its turns are append-only rows in
//! `messages`, each carrying a `content_json` TEXT column that is a JSON
//! **string** whose value is an array of MessageContent blocks (a `type`-tagged,
//! camelCase enum). Tool calls are `toolRequest` blocks — name + arguments under
//! `toolCall.value`, wrapped in a `{status,value}` envelope — and their results
//! are `toolResponse` blocks, appended as later `role='user'` messages and
//! correlated by the block's top-level `id`. The developer platform extension
//! exposes its tools *unprefixed* in current goose (`shell`/`write`/`edit`/
//! `tree`/`read_image`); older DBs carry the `developer__` prefix, MCP-extension
//! tools are `<extension>__<tool>`, and text-file reads are not recorded
//! structurally (they run through `shell`).
//!
//! This is the **pure** layer (arch rule `session` ← nothing): it parses record
//! strings and column values that `app::activity::goose` hands it, never
//! touching the DB. Verified against goose stable v1.41.0 (a real binary run in
//! docker) and `main`/v2 source. Defensive like every provider: unknown block
//! and tool shapes are skipped, never errors.

use std::collections::HashMap;

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for compact-input notes (`Other`/`Mcp` events).
const NOTE_MAX: usize = 160;

/// Tool names that act on goose's own plan/bookkeeping rather than the
/// codebase — skipped so the timeline shows work, not churn (mirrors the
/// Claude/Vibe providers' skip lists). Matched on the developer-unprefixed name.
const SKIPPED_TOOLS: &[&str] = &["todo", "final_output"];

/// Streaming scanner over one goose session's `messages`. Feed each row's
/// `content_json` in `(created_timestamp, id)` order via
/// [`ingest_message`](Self::ingest_message); read the accumulated
/// [`events`](Self::events) between feeds. Same accumulator contract as
/// [`super::claude::ClaudeScan`], adapted to a per-message (not per-line) unit.
#[derive(Debug, Clone, Default)]
pub struct GooseScan {
    pub events: Vec<ActivityEvent>,
    /// `toolRequest` id → index into [`Self::events`], awaiting its result.
    pending: HashMap<String, usize>,
}

impl GooseScan {
    /// Ingest one `messages` row: its `content_json` (the JSON **string** whose
    /// value is an array of MessageContent blocks) plus `created_timestamp`
    /// (unix seconds, `None` when the column is null). Feed rows in
    /// `(created_timestamp, id)` order so a `toolRequest` precedes its
    /// `toolResponse`.
    pub fn ingest_message(&mut self, content_json: &str, created_ts_secs: Option<i64>) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(content_json) else {
            return;
        };
        let Some(blocks) = v.as_array() else {
            return;
        };
        let ts = created_ts_secs
            .and_then(|s| u64::try_from(s).ok())
            .map(|s| s.saturating_mul(1000));
        for block in blocks {
            match str_field(block, "type").as_deref() {
                Some("toolRequest") => self.ingest_request(block, ts),
                Some("toolResponse") => self.ingest_response(block),
                _ => {}
            }
        }
    }

    fn ingest_request(&mut self, block: &serde_json::Value, ts: Option<u64>) {
        // `toolCall` is a {status,value} envelope; a `status:'error'` request
        // carries an `error` string and no `value`, so it has no name and is
        // skipped.
        let value = block.pointer("/toolCall/value");
        let Some(name) = value.and_then(|v| v.get("name")).and_then(|n| n.as_str()) else {
            return;
        };
        let Some(mut event) = classify(name, value.and_then(|v| v.get("arguments"))) else {
            return;
        };
        event.ts_ms = ts;
        if let Some(id) = str_field(block, "id") {
            self.pending.insert(id, self.events.len());
        }
        self.events.push(event);
    }

    fn ingest_response(&mut self, block: &serde_json::Value) {
        let Some(id) = str_field(block, "id") else {
            return;
        };
        let Some(idx) = self.pending.remove(&id) else {
            return;
        };
        let Some(event) = self.events.get_mut(idx) else {
            return;
        };
        let (ok, text) = tool_result(block.get("toolResult"));
        if ok.is_some() {
            event.ok = ok;
        }
        let text = text.trim();
        if !text.is_empty() {
            event.result_head = Some(head(text, RESULT_HEAD_MAX));
        }
    }
}

/// Map one tool call to an event. `None` ⇒ the call is bookkeeping or malformed
/// for its tool (e.g. a `shell` without `command`) and is skipped.
fn classify(name: &str, input: Option<&serde_json::Value>) -> Option<ActivityEvent> {
    let input = input.unwrap_or(&serde_json::Value::Null);
    let get = |key: &str| input.get(key).and_then(|x| x.as_str()).map(String::from);

    // The developer extension is unprefixed in current goose; older DBs prefix
    // its tools `developer__`. Fold both onto the bare name.
    let bare = name.strip_prefix("developer__").unwrap_or(name);
    if SKIPPED_TOOLS.contains(&bare) {
        return None;
    }

    let (kind, detail, note) = match bare {
        "shell" => (ActionKind::Command, get("command")?, None),
        "write" | "edit" => (ActionKind::Edit, get("path")?, None),
        // Legacy single editor tool: `view` is a text read, the rest mutate.
        "text_editor" => {
            let path = get("path")?;
            match get("command").as_deref() {
                Some("view") => (ActionKind::Read, path, None),
                _ => (ActionKind::Edit, path, None),
            }
        }
        // `read_image` targets a local path (a file read) or an http(s) image
        // URL (a web fetch — a URL in the Files section would be wrong).
        "read_image" => match (get("path"), get("url")) {
            (Some(path), _) => (ActionKind::Read, path, None),
            (None, Some(url)) => (ActionKind::WebFetch, url, None),
            (None, None) => return None,
        },
        // `tree` lists a directory's structure — the closest kind to a search.
        "tree" => (
            ActionKind::Search,
            get("path").unwrap_or_else(|| ".".to_string()),
            None,
        ),
        _ => classify_extension(name, input)?,
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

/// Non-developer tools: the built-in web fetch
/// (`computercontroller__web_scrape`) and any user MCP tool
/// (`<extension>__<tool>`). Bare unknown names fall to `Other`.
fn classify_extension(
    name: &str,
    input: &serde_json::Value,
) -> Option<(ActionKind, String, Option<String>)> {
    if let Some((_, tool)) = name.split_once("__") {
        if tool == "web_scrape" {
            let url = input.get("url").and_then(|u| u.as_str())?;
            return Some((ActionKind::WebFetch, url.to_string(), None));
        }
    }
    match mcp_name(name) {
        Some(pretty) => Some((ActionKind::Mcp, pretty, compact_input(input))),
        None => Some((ActionKind::Other, name.to_string(), compact_input(input))),
    }
}

/// `<extension>__<tool>` → `extension:tool` (goose prefixes MCP tools without a
/// leading `mcp__`; the tool half may itself contain `__`, so split only once).
fn mcp_name(name: &str) -> Option<String> {
    let (server, tool) = name.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then(|| format!("{server}:{tool}"))
}

/// One-line compact rendering of a tool input for `note`, capped.
fn compact_input(input: &serde_json::Value) -> Option<String> {
    if input.is_null() || input.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    Some(head(&input.to_string(), NOTE_MAX))
}

/// Extract `(ok, result_text)` from a `toolResponse`'s `toolResult` envelope.
/// A `status:'error'` envelope or an inner `isError` flag are the only success
/// signals goose records, so `ok` stays unknown when neither is present.
fn tool_result(result: Option<&serde_json::Value>) -> (Option<bool>, String) {
    let Some(result) = result else {
        return (None, String::new());
    };
    if result.get("status").and_then(|s| s.as_str()) == Some("error") {
        let text = result
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or_default()
            .to_string();
        return (Some(false), text);
    }
    let value = result.get("value");
    let ok = value
        .and_then(|v| v.get("isError"))
        .and_then(|e| e.as_bool())
        .map(|is_err| !is_err);
    let text = value
        .and_then(|v| v.get("content"))
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    (ok, text)
}

/// Session metadata from a `sessions` row's columns: title, model, and output
/// tokens. Pure — the app layer reads the columns and hands their values here.
pub fn session_meta(
    name: Option<&str>,
    user_set_name: bool,
    description: Option<&str>,
    model_config_json: Option<&str>,
    output_tokens: Option<i64>,
) -> ActivityMeta {
    ActivityMeta {
        title: pick_title(name, user_set_name, description),
        model: model_config_json.and_then(model_name),
        output_tokens: output_tokens.and_then(|t| u64::try_from(t).ok()),
    }
}

/// The row's title: the user-set `name` when they named the session, else the
/// LLM-generated `description`, else any non-empty `name`.
fn pick_title(
    name: Option<&str>,
    user_set_name: bool,
    description: Option<&str>,
) -> Option<String> {
    let non_empty = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    if user_set_name {
        if let Some(n) = non_empty(name) {
            return Some(n);
        }
    }
    non_empty(description).or_else(|| non_empty(name))
}

/// `model_config_json`'s `model_name` field (the column is itself a JSON string).
fn model_name(model_config_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(model_config_json).ok()?;
    v.get("model_name")
        .and_then(|m| m.as_str())
        .map(String::from)
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(content_json: &str, ts: Option<i64>) -> GooseScan {
        let mut s = GooseScan::default();
        s.ingest_message(content_json, ts);
        s
    }

    #[test]
    fn classifies_goose_tools() {
        // One message's content_json array (folded onto one line below).
        let content = r#"[
            {"type":"text","text":"working on it"},
            {"type":"toolRequest","id":"a1","toolCall":{"status":"success","value":{"name":"shell","arguments":{"command":"cargo nextest run auth","timeout_secs":300}}}},
            {"type":"toolRequest","id":"a2","toolCall":{"status":"success","value":{"name":"write","arguments":{"path":"src/lib.rs","content":"fn main(){}\n"}}}},
            {"type":"toolRequest","id":"a3","toolCall":{"status":"success","value":{"name":"edit","arguments":{"path":"src/auth.rs","before":"a","after":"b"}}}},
            {"type":"toolRequest","id":"a4","toolCall":{"status":"success","value":{"name":"read_image","arguments":{"path":"docs/diagram.png"}}}},
            {"type":"toolRequest","id":"a5","toolCall":{"status":"success","value":{"name":"tree","arguments":{"path":"src"}}}},
            {"type":"toolRequest","id":"a6","toolCall":{"status":"success","value":{"name":"computercontroller__web_scrape","arguments":{"url":"https://example.com/api","save_as":"text"}}}},
            {"type":"toolRequest","id":"a7","toolCall":{"status":"success","value":{"name":"tavily__search","arguments":{"query":"rust sqlx wal"}}}},
            {"type":"toolRequest","id":"a8","toolCall":{"status":"success","value":{"name":"developer__shell","arguments":{"command":"ls"}}}},
            {"type":"toolRequest","id":"a9","toolCall":{"status":"error","error":"invalid tool call"}}
        ]"#
        .replace('\n', " ");
        let s = scan(&content, Some(1783826948));
        let kinds: Vec<ActionKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Command,
                ActionKind::Edit,
                ActionKind::Edit,
                ActionKind::Read,
                ActionKind::Search,
                ActionKind::WebFetch,
                ActionKind::Mcp, // text + error request skipped
                ActionKind::Command,
            ]
        );
        assert_eq!(s.events[0].detail, "cargo nextest run auth");
        assert_eq!(s.events[0].ts_ms, Some(1_783_826_948_000));
        assert_eq!(s.events[1].detail, "src/lib.rs");
        assert_eq!(s.events[3].detail, "docs/diagram.png");
        assert_eq!(s.events[4].detail, "src");
        assert_eq!(s.events[5].detail, "https://example.com/api");
        assert_eq!(s.events[6].detail, "tavily:search");
        assert!(s.events[6]
            .note
            .as_deref()
            .unwrap()
            .contains("rust sqlx wal"));
        assert_eq!(s.events[7].detail, "ls"); // developer__shell folded to shell
    }

    #[test]
    fn read_image_with_url_is_a_web_fetch_not_a_file_read() {
        let content = r#"[
            {"type":"toolRequest","id":"a1","toolCall":{"status":"success","value":{"name":"read_image","arguments":{"url":"https://example.com/chart.png"}}}}
        ]"#
        .replace('\n', " ");
        let s = scan(&content, None);
        assert_eq!(s.events.len(), 1);
        // A URL in the Files section would be wrong — it belongs under Web.
        assert_eq!(s.events[0].kind, ActionKind::WebFetch);
        assert_eq!(s.events[0].detail, "https://example.com/chart.png");
    }

    #[test]
    fn legacy_text_editor_splits_view_from_edits() {
        let s = scan(
            &r#"[
                {"type":"toolRequest","id":"v","toolCall":{"status":"success","value":{"name":"developer__text_editor","arguments":{"command":"view","path":"src/lib.rs"}}}},
                {"type":"toolRequest","id":"w","toolCall":{"status":"success","value":{"name":"developer__text_editor","arguments":{"command":"str_replace","path":"src/lib.rs"}}}}
            ]"#
            .replace('\n', " "),
            None,
        );
        assert_eq!(s.events[0].kind, ActionKind::Read);
        assert_eq!(s.events[0].detail, "src/lib.rs");
        assert_eq!(s.events[1].kind, ActionKind::Edit);
    }

    #[test]
    fn results_patch_ok_and_head_by_id() {
        let mut s = GooseScan::default();
        s.ingest_message(
            r#"[{"type":"toolRequest","id":"t1","toolCall":{"status":"success","value":{"name":"shell","arguments":{"command":"cargo test"}}}}]"#,
            Some(1783826948),
        );
        assert_eq!(s.events[0].ok, None); // pending
        s.ingest_message(
            r#"[{"type":"toolResponse","id":"t1","toolResult":{"status":"success","value":{"content":[{"type":"text","text":"1 passed; 0 failed"}],"isError":false}}}]"#,
            Some(1783826950),
        );
        assert_eq!(s.events[0].ok, Some(true));
        assert_eq!(
            s.events[0].result_head.as_deref(),
            Some("1 passed; 0 failed")
        );

        // An inner isError flag marks failure.
        s.ingest_message(
            r#"[{"type":"toolRequest","id":"t2","toolCall":{"status":"success","value":{"name":"edit","arguments":{"path":"/gone"}}}}]"#,
            None,
        );
        s.ingest_message(
            r#"[{"type":"toolResponse","id":"t2","toolResult":{"status":"success","value":{"content":[{"type":"text","text":"file not found"}],"isError":true}}}]"#,
            None,
        );
        assert_eq!(s.events[1].ok, Some(false));
        assert_eq!(s.events[1].result_head.as_deref(), Some("file not found"));

        // An error-status envelope carries the error string, no `value`.
        s.ingest_message(
            r#"[{"type":"toolRequest","id":"t3","toolCall":{"status":"success","value":{"name":"shell","arguments":{"command":"boom"}}}}]"#,
            None,
        );
        s.ingest_message(
            r#"[{"type":"toolResponse","id":"t3","toolResult":{"status":"error","error":"tool crashed"}}]"#,
            None,
        );
        assert_eq!(s.events[2].ok, Some(false));
        assert_eq!(s.events[2].result_head.as_deref(), Some("tool crashed"));
    }

    #[test]
    fn malformed_and_orphan_records_are_skipped() {
        let mut s = GooseScan::default();
        s.ingest_message("not json", None);
        s.ingest_message(r#"{"type":"toolRequest"}"#, None); // not an array
        s.ingest_message(
            r#"[{"type":"toolRequest","id":"x","toolCall":{"status":"success","value":{"name":"shell"}}}]"#,
            None,
        ); // no command → skipped
        s.ingest_message(
            r#"[{"type":"toolResponse","id":"orphan","toolResult":{"status":"success","value":{"content":[]}}}]"#,
            None,
        ); // no pending request → ignored
        s.ingest_message(
            r#"[{"type":"toolRequest","id":"ok","toolCall":{"status":"success","value":{"name":"shell","arguments":{"command":"pwd"}}}}]"#,
            None,
        );
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].detail, "pwd");
    }

    #[test]
    fn session_meta_picks_title_model_and_tokens() {
        // A user-set name wins over the description.
        let m = session_meta(
            Some("Nightly build"),
            true,
            Some("auto description"),
            Some(r#"{"model_name":"claude-sonnet-4-20250514","context_limit":200000}"#),
            Some(2234),
        );
        assert_eq!(m.title.as_deref(), Some("Nightly build"));
        assert_eq!(m.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(m.output_tokens, Some(2234));

        // No user-set name → the LLM description is the title.
        let m = session_meta(
            Some("   "),
            false,
            Some("Fix flaky auth integration test"),
            None,
            None,
        );
        assert_eq!(m.title.as_deref(), Some("Fix flaky auth integration test"));
        assert_eq!(m.model, None);
        assert_eq!(m.output_tokens, None);

        // Malformed model JSON and a negative token count degrade to defaults.
        let m = session_meta(None, false, None, Some("garbage"), Some(-5));
        assert_eq!(m, ActivityMeta::default());
    }

    #[test]
    fn mcp_names_split_server_and_tool() {
        assert_eq!(mcp_name("tavily__search").as_deref(), Some("tavily:search"));
        assert_eq!(
            mcp_name("srv__ns__deep").as_deref(),
            Some("srv:ns__deep") // only the first `__` separates server from tool
        );
        assert_eq!(mcp_name("shell"), None);
        assert_eq!(mcp_name("__lonely"), None);
    }
}
