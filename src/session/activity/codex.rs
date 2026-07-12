//! Codex CLI (OpenAI) activity provider — normalizes a session's append-only
//! rollout transcript into [`ActivityEvent`]s.
//!
//! A rollout lives at
//! `$CODEX_HOME/sessions/<YYYY>/<MM>/<DD>/rollout-<local-start>-<thread_id>.jsonl`
//! (`CODEX_HOME` defaults to `~/.codex`; year unpadded, month/day zero-padded).
//! Each physical line is a `RolloutLine`
//! `{timestamp: RFC3339-UTC, ordinal?, type: <snake_case>, payload: <body>}`;
//! the writer flushes after every line, so it is tailable. The first line is
//! `session_meta` (cwd / thread id / `history_mode`); a subagent runs as its
//! own rollout whose meta carries `parent_thread_id`.
//!
//! Codex records activity two ways depending on `SessionMeta.history_mode`
//! (default `legacy`): in **legacy** mode the durable stream is `response_item`
//! (raw model tool calls — `local_shell_call`/`function_call` for commands,
//! `apply_patch` for edits, `web_search_call` for web) plus a few legacy render
//! `event_msg`s (`patch_apply_end`, `mcp_tool_call_end`, `sub_agent_activity`)
//! that carry nicer structured outcomes and are correlated back to their call
//! by `call_id`; in **paginated** mode the same actions arrive as
//! `event_msg` → `item_completed` with a self-contained `item`
//! (`command_execution`, `file_change`, `web_search`, `image_view`, …). To
//! avoid double-counting (both representations can coexist), the scanner
//! branches on `history_mode` and processes exactly one family.
//!
//! Verified against the openai/codex writer source (`codex-rs/rollout/*`,
//! `codex-rs/protocol/{protocol,models,items,parse_command}.rs`,
//! `codex-rs/utils/home-dir`) as of 2026-07-12 (release `rust-v0.144.1`).
//! Defensive like every provider: unknown line/item/payload shapes are skipped,
//! never errors (the format is under active churn and uses `serde(other)`
//! catch-alls throughout).

use std::collections::HashMap;

use serde_json::Value;

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for compact-input / invocation-argument notes.
const NOTE_MAX: usize = 160;

/// Which record family carries the durable activity, from
/// `SessionMeta.history_mode`. Defaults to [`HistoryMode::Legacy`] (the writer
/// default, and the fallback when the meta line was clipped off a huge tail).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum HistoryMode {
    #[default]
    Legacy,
    Paginated,
}

/// Attribution metadata from a rollout's first `session_meta` line — the
/// fields the app layer needs to bind a rollout to a friring session (cwd
/// match, id, subagent detection).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CodexSessionMeta {
    pub session_id: Option<String>,
    /// `id` — the thread UUID (also the trailing token of the filename).
    pub thread_id: Option<String>,
    /// Set only on a subagent rollout; its presence means "not a top-level
    /// session".
    pub parent_thread_id: Option<String>,
    /// `cwd` — the working-directory match key for discovery.
    pub cwd: Option<String>,
    /// `timestamp` (session start) as epoch ms.
    pub start_ms: Option<u64>,
}

/// Parse the head `session_meta` line of a rollout for discovery. Returns
/// `None` when the line is missing/garbled or is not a `session_meta` line.
pub fn parse_session_meta(line: &str) -> Option<CodexSessionMeta> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if str_field(&v, "type").as_deref() != Some("session_meta") {
        return None;
    }
    Some(session_from_payload(v.get("payload")?))
}

/// Streaming scanner over one Codex rollout. Same contract as
/// [`super::claude::ClaudeScan`]: feed line-aligned chunks in file order,
/// reading [`events`](Self::events) / [`meta`](Self::meta) between feeds.
#[derive(Debug, Clone, Default)]
pub struct CodexScan {
    pub events: Vec<ActivityEvent>,
    pub meta: ActivityMeta,
    pub session: CodexSessionMeta,
    mode: HistoryMode,
    /// Subagent label stamped on every event when this rollout is a subagent
    /// (its `session_meta` carried `parent_thread_id`).
    origin: Option<String>,
    /// `call_id` → the event indices it produced, awaiting an outcome record
    /// (`function_call_output`, `patch_apply_end`, `mcp_tool_call_end`). A
    /// single call may touch several files, hence a vector.
    pending: HashMap<String, Vec<usize>>,
}

impl CodexScan {
    /// Ingest the next line-aligned chunk of the rollout.
    pub fn ingest(&mut self, chunk: &str) {
        for line in chunk.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let ts = ts_ms(&v);
            let payload = v.get("payload").unwrap_or(&Value::Null);
            match str_field(&v, "type").as_deref() {
                Some("session_meta") => self.ingest_session_meta(payload),
                Some("turn_context") => {
                    if let Some(m) = str_field(payload, "model") {
                        self.meta.model = Some(m);
                    }
                }
                // Raw model tool calls are the durable stream only in legacy
                // mode; in paginated mode `item_completed` is authoritative and
                // response items would double-count.
                Some("response_item") if self.mode == HistoryMode::Legacy => {
                    self.ingest_response_item(payload, ts)
                }
                Some("event_msg") => self.ingest_event_msg(payload, ts),
                _ => {}
            }
        }
    }

    fn ingest_session_meta(&mut self, payload: &Value) {
        self.session = session_from_payload(payload);
        if str_field(payload, "history_mode").as_deref() == Some("paginated") {
            self.mode = HistoryMode::Paginated;
        }
        if self.session.parent_thread_id.is_some() {
            self.origin =
                Some(str_field(payload, "agent_role").unwrap_or_else(|| "subagent".to_string()));
        }
    }

    fn ingest_response_item(&mut self, payload: &Value, ts: Option<u64>) {
        match str_field(payload, "type").as_deref() {
            Some("local_shell_call") => {
                let action = payload.pointer("/action/command").unwrap_or(&Value::Null);
                if let Some(cmd) = format_command(action) {
                    self.add_event(
                        str_field(payload, "call_id").as_deref(),
                        ts,
                        ActionKind::Command,
                        cmd,
                        None,
                    );
                }
            }
            Some("function_call") => self.ingest_function_call(payload, ts),
            Some("custom_tool_call") => self.ingest_custom_tool_call(payload, ts),
            Some("function_call_output") => {
                if let Some(id) = str_field(payload, "call_id") {
                    let rh = clip_result(&output_text(payload.get("output")));
                    self.patch(&id, None, rh);
                }
            }
            Some("web_search_call") => {
                if let Some((kind, detail, note)) = web_action(payload.get("action")) {
                    self.add_event(None, ts, kind, detail, note);
                }
            }
            _ => {}
        }
    }

    fn ingest_function_call(&mut self, payload: &Value, ts: Option<u64>) {
        let Some(name) = str_field(payload, "name") else {
            return;
        };
        let call_id = str_field(payload, "call_id");
        let raw = str_field(payload, "arguments").unwrap_or_default();
        let args: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        if name == "apply_patch" {
            // The patch envelope is a string field inside the arguments JSON
            // (or, defensively, the raw arguments string itself).
            let body = find_patch_body(&args)
                .map(str::to_string)
                .or_else(|| raw.contains("*** Begin Patch").then_some(raw));
            self.emit_patch(body.as_deref(), call_id.as_deref(), ts);
        } else if name == "view_image" {
            if let Some(p) = args.get("path").and_then(|x| x.as_str()) {
                self.add_event(
                    call_id.as_deref(),
                    ts,
                    ActionKind::Read,
                    p.to_string(),
                    None,
                );
            }
        } else if is_shell_tool(&name) {
            if let Some(cmd) = format_command(args.get("command").unwrap_or(&Value::Null)) {
                self.add_event(call_id.as_deref(), ts, ActionKind::Command, cmd, None);
            }
        } else {
            // Unknown tool: keep it on the timeline. A later `mcp_tool_call_end`
            // with the same `call_id` upgrades it to a labelled MCP event.
            let note = (!args.is_null()).then(|| head(&args.to_string(), NOTE_MAX));
            self.add_event(call_id.as_deref(), ts, ActionKind::Other, name, note);
        }
    }

    fn ingest_custom_tool_call(&mut self, payload: &Value, ts: Option<u64>) {
        let name = str_field(payload, "name").unwrap_or_default();
        let call_id = str_field(payload, "call_id");
        if name == "apply_patch" {
            let body = str_field(payload, "input");
            self.emit_patch(body.as_deref(), call_id.as_deref(), ts);
        } else if !name.is_empty() {
            self.add_event(call_id.as_deref(), ts, ActionKind::Other, name, None);
        }
    }

    /// Emit one [`ActionKind::Edit`] per file named in a patch envelope. When no
    /// path parses, nothing is registered for `call_id`, so a later
    /// `patch_apply_end` (which carries the authoritative path map) still
    /// creates the edits rather than silently patching nothing.
    fn emit_patch(&mut self, body: Option<&str>, call_id: Option<&str>, ts: Option<u64>) {
        for path in body.map(patch_paths).unwrap_or_default() {
            self.add_event(call_id, ts, ActionKind::Edit, path, None);
        }
    }

    fn ingest_event_msg(&mut self, payload: &Value, ts: Option<u64>) {
        match str_field(payload, "type").as_deref() {
            Some("token_count") => {
                if let Some(t) = payload
                    .pointer("/info/total_token_usage/output_tokens")
                    .and_then(|x| x.as_u64())
                {
                    // Cumulative usage — latest total wins (replace, not add).
                    self.meta.output_tokens = Some(t);
                }
            }
            Some("patch_apply_end") => self.ingest_patch_apply_end(payload, ts),
            Some("mcp_tool_call_end") => self.ingest_mcp_tool_call_end(payload, ts),
            Some("sub_agent_activity") => self.ingest_sub_agent(payload, ts),
            // Paginated items are authoritative only in paginated mode; ignoring
            // them in legacy mode avoids double-counting against response items.
            Some("item_completed") if self.mode == HistoryMode::Paginated => {
                self.ingest_item(payload.get("item").unwrap_or(&Value::Null), ts)
            }
            _ => {}
        }
    }

    fn ingest_patch_apply_end(&mut self, payload: &Value, ts: Option<u64>) {
        let call_id = str_field(payload, "call_id");
        let ok = payload.get("success").and_then(|b| b.as_bool());
        let rh = clip_result(&str_field(payload, "stdout").unwrap_or_default());
        // Prefer patching the edits the response item already emitted for this
        // call; only synthesize from `changes` when there was no such call.
        let patched = call_id
            .as_deref()
            .map(|id| self.patch(id, ok, rh.clone()))
            .unwrap_or(false);
        if !patched {
            for path in change_paths(payload.get("changes")) {
                let idx = self.add_event(call_id.as_deref(), ts, ActionKind::Edit, path, None);
                self.events[idx].ok = ok;
                self.events[idx].result_head = rh.clone();
            }
        }
    }

    fn ingest_mcp_tool_call_end(&mut self, payload: &Value, ts: Option<u64>) {
        let call_id = str_field(payload, "call_id");
        let server = payload
            .pointer("/invocation/server")
            .and_then(|x| x.as_str());
        let tool = payload.pointer("/invocation/tool").and_then(|x| x.as_str());
        let detail = mcp_detail(server, tool);
        let note = payload
            .pointer("/invocation/arguments")
            .filter(|a| !a.is_null())
            .map(|a| head(&a.to_string(), NOTE_MAX));
        let ok = mcp_result_ok(payload.get("result"));
        let rh = mcp_result_head(payload.get("result"));
        // Upgrade the raw `function_call` created earlier for this call (it was
        // classified `Other` because MCP tool names aren't self-describing), or
        // create a fresh MCP event when the begin call wasn't persisted.
        if let Some(idxs) = call_id
            .as_deref()
            .and_then(|id| self.pending.get(id).cloned())
        {
            for idx in idxs {
                if let Some(e) = self.events.get_mut(idx) {
                    e.kind = ActionKind::Mcp;
                    e.detail = detail.clone();
                    if e.note.is_none() {
                        e.note = note.clone();
                    }
                    if ok.is_some() {
                        e.ok = ok;
                    }
                    if e.result_head.is_none() {
                        e.result_head = rh.clone();
                    }
                }
            }
            return;
        }
        let idx = self.add_event(call_id.as_deref(), ts, ActionKind::Mcp, detail, note);
        self.events[idx].ok = ok;
        self.events[idx].result_head = rh;
    }

    fn ingest_sub_agent(&mut self, payload: &Value, ts: Option<u64>) {
        // Only the delegation itself is timeline-worthy; `interacted`/
        // `interrupted` follow-ups would just repeat it.
        if str_field(payload, "kind").as_deref() != Some("started") {
            return;
        }
        let path = str_field(payload, "agent_path").unwrap_or_else(|| "subagent".to_string());
        self.add_event(None, ts, ActionKind::Subagent, path, None);
    }

    fn ingest_item(&mut self, item: &Value, ts: Option<u64>) {
        match str_field(item, "type").as_deref() {
            Some("command_execution") => {
                let Some(cmd) = format_command(item.get("command").unwrap_or(&Value::Null)) else {
                    return;
                };
                let ok = command_ok(item);
                let rh = clip_result(&aggregated_output(item));
                let idx = self.add_event(None, ts, ActionKind::Command, cmd, None);
                self.events[idx].ok = ok;
                self.events[idx].result_head = rh;
            }
            Some("file_change") => {
                let ok = item
                    .get("status")
                    .and_then(|s| s.as_str())
                    .map(|s| s == "completed");
                for path in change_paths(item.get("changes")) {
                    let idx = self.add_event(None, ts, ActionKind::Edit, path, None);
                    self.events[idx].ok = ok;
                }
            }
            Some("web_search") => {
                let web = web_action(item.get("action")).or_else(|| {
                    item.get("query")
                        .and_then(|q| q.as_str())
                        .map(|q| (ActionKind::WebSearch, q.to_string(), None))
                });
                if let Some((kind, detail, note)) = web {
                    self.add_event(None, ts, kind, detail, note);
                }
            }
            Some("image_view") => {
                if let Some(p) = item.get("path").and_then(|x| x.as_str()) {
                    self.add_event(None, ts, ActionKind::Read, p.to_string(), None);
                }
            }
            Some("mcp_tool_call") => {
                let server = item
                    .pointer("/invocation/server")
                    .or_else(|| item.get("server"))
                    .and_then(|x| x.as_str());
                let tool = item
                    .pointer("/invocation/tool")
                    .or_else(|| item.get("tool"))
                    .and_then(|x| x.as_str());
                self.add_event(None, ts, ActionKind::Mcp, mcp_detail(server, tool), None);
            }
            Some("sub_agent_activity") if str_field(item, "kind").as_deref() == Some("started") => {
                let path = str_field(item, "agent_path").unwrap_or_else(|| "subagent".to_string());
                self.add_event(None, ts, ActionKind::Subagent, path, None);
            }
            _ => {}
        }
    }

    /// Append an event (stamped with the rollout's subagent origin) and, when a
    /// `call_id` is given, remember its index so an outcome record can patch it.
    fn add_event(
        &mut self,
        call_id: Option<&str>,
        ts: Option<u64>,
        kind: ActionKind,
        detail: String,
        note: Option<String>,
    ) -> usize {
        let idx = self.events.len();
        self.events.push(ActivityEvent {
            ts_ms: ts,
            kind,
            detail,
            note: note.filter(|n| !n.trim().is_empty()),
            result_head: None,
            ok: None,
            origin: self.origin.clone(),
        });
        if let Some(id) = call_id {
            self.pending.entry(id.to_string()).or_default().push(idx);
        }
        idx
    }

    /// Patch the events pending on `call_id` with an outcome. Returns whether
    /// any pending event existed (so callers can fall back to creating one).
    fn patch(&mut self, call_id: &str, ok: Option<bool>, result_head: Option<String>) -> bool {
        let Some(idxs) = self.pending.get(call_id).cloned() else {
            return false;
        };
        for idx in idxs {
            if let Some(e) = self.events.get_mut(idx) {
                if ok.is_some() {
                    e.ok = ok;
                }
                if e.result_head.is_none() {
                    if let Some(r) = &result_head {
                        e.result_head = Some(r.clone());
                    }
                }
            }
        }
        true
    }
}

fn session_from_payload(payload: &Value) -> CodexSessionMeta {
    CodexSessionMeta {
        session_id: str_field(payload, "session_id"),
        thread_id: str_field(payload, "id"),
        parent_thread_id: str_field(payload, "parent_thread_id"),
        cwd: str_field(payload, "cwd"),
        start_ms: str_field(payload, "timestamp")
            .as_deref()
            .and_then(parse_iso_ms),
    }
}

/// The tools whose call is a shell command (arguments carry a `command`).
fn is_shell_tool(name: &str) -> bool {
    matches!(
        name,
        "shell" | "exec_command" | "shell_command" | "unified_exec" | "write_stdin"
    )
}

/// Render a command for display. Codex records commands as an argv array
/// (`["bash","-lc","cargo test"]`) or, for the `shell_command` tool, a string;
/// the argv `bash -lc <script>` wrapper is unwrapped to the script itself.
fn format_command(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    let parts: Vec<&str> = v.as_array()?.iter().filter_map(|x| x.as_str()).collect();
    if parts.is_empty() {
        return None;
    }
    if parts.len() == 3 && matches!(parts[1], "-lc" | "-c" | "-lic") {
        return Some(parts[2].to_string());
    }
    Some(parts.join(" "))
}

/// Find the `*** Begin Patch … *** End Patch` envelope string nested anywhere
/// in a function-call arguments value.
fn find_patch_body(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) if s.contains("*** Begin Patch") => Some(s),
        Value::Object(o) => o.values().find_map(find_patch_body),
        _ => None,
    }
}

/// Absolute paths named by the `*** Update/Add/Delete File:` lines of a patch
/// envelope, in envelope order.
fn patch_paths(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in body.lines() {
        for tag in ["*** Update File:", "*** Add File:", "*** Delete File:"] {
            if let Some(rest) = line.strip_prefix(tag) {
                let p = rest.trim();
                if !p.is_empty() {
                    out.push(p.to_string());
                }
            }
        }
    }
    out
}

/// Paths keyed in a `changes` map (`patch_apply_end` / `file_change`). Sorted
/// for a deterministic event order.
fn change_paths(v: Option<&Value>) -> Vec<String> {
    let mut paths: Vec<String> = v
        .and_then(|c| c.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    paths.sort();
    paths
}

/// `server:tool`, degrading to the tool name or a bare `mcp` label.
fn mcp_detail(server: Option<&str>, tool: Option<&str>) -> String {
    match (server, tool) {
        (Some(s), Some(t)) => format!("{s}:{t}"),
        (_, Some(t)) => t.to_string(),
        _ => "mcp".to_string(),
    }
}

fn mcp_result_ok(result: Option<&Value>) -> Option<bool> {
    let r = result?;
    if r.get("Ok").is_some() {
        Some(true)
    } else if r.get("Err").is_some() {
        Some(false)
    } else {
        None
    }
}

fn mcp_result_head(result: Option<&Value>) -> Option<String> {
    let ok = result?.get("Ok")?;
    clip_result(&output_text(ok.get("content").or(Some(ok))))
}

/// `Some(true/false)` from an explicit `exit_code` (preferred) or `status`;
/// `None` when neither is present.
fn command_ok(item: &Value) -> Option<bool> {
    if let Some(code) = item.get("exit_code").and_then(|c| c.as_i64()) {
        return Some(code == 0);
    }
    match item.get("status").and_then(|s| s.as_str()) {
        Some("completed") => Some(true),
        Some("failed") | Some("declined") => Some(false),
        _ => None,
    }
}

fn aggregated_output(item: &Value) -> String {
    item.get("aggregated_output")
        .or_else(|| item.get("stdout"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Flatten a tool output (string, or a content-item array of `{text}`) to text.
fn output_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|it| {
                it.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| it.as_str())
                    .map(String::from)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(o)) => o
            .get("text")
            .and_then(|t| t.as_str())
            .map(String::from)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// A `WebSearchAction` → (kind, detail, note): a `search` is a
/// [`ActionKind::WebSearch`] on the query; `open_page`/`find_in_page` are
/// [`ActionKind::WebFetch`] on the URL.
fn web_action(action: Option<&Value>) -> Option<(ActionKind, String, Option<String>)> {
    let a = action?;
    match a.get("type").and_then(|t| t.as_str())? {
        "search" => {
            if let Some(q) = a.get("query").and_then(|x| x.as_str()) {
                Some((ActionKind::WebSearch, q.to_string(), None))
            } else {
                let joined = a
                    .get("queries")?
                    .as_array()?
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                (!joined.is_empty()).then_some((ActionKind::WebSearch, joined, None))
            }
        }
        "open_page" => a
            .get("url")
            .and_then(|x| x.as_str())
            .map(|u| (ActionKind::WebFetch, u.to_string(), None)),
        "find_in_page" => a.get("url").and_then(|x| x.as_str()).map(|u| {
            (
                ActionKind::WebFetch,
                u.to_string(),
                a.get("pattern").and_then(|x| x.as_str()).map(String::from),
            )
        }),
        _ => None,
    }
}

fn clip_result(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| head(t, RESULT_HEAD_MAX))
}

/// Epoch ms from the line's top-level RFC3339 `timestamp`, when present.
fn ts_ms(v: &Value) -> Option<u64> {
    parse_iso_ms(v.get("timestamp")?.as_str()?)
}

fn parse_iso_ms(ts: &str) -> Option<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    u64::try_from(dt.timestamp_millis()).ok()
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = r#"{"timestamp":"2026-07-12T14:03:11.482Z","type":"session_meta","payload":{"session_id":"019827f4-1c3a-7d21-9e4b-2f6a1b0c9d55","id":"019827f4-1c3a-7d21-9e4b-2f6a1b0c9d55","timestamp":"2026-07-12T14:03:11.482Z","cwd":"/home/jat/project","originator":"codex_cli_rs","cli_version":"0.144.1","source":"cli","model_provider":"openai","history_mode":"legacy"}}"#;

    fn scan(lines: &[&str]) -> CodexScan {
        let mut s = CodexScan::default();
        s.ingest(&lines.join("\n"));
        s
    }

    #[test]
    fn parses_session_meta_head_and_rejects_others() {
        let m = parse_session_meta(META).expect("meta");
        assert_eq!(m.cwd.as_deref(), Some("/home/jat/project"));
        assert_eq!(
            m.thread_id.as_deref(),
            Some("019827f4-1c3a-7d21-9e4b-2f6a1b0c9d55")
        );
        assert_eq!(m.parent_thread_id, None);
        assert!(m.start_ms.is_some());
        // A non-meta line (or garbage) is not a session head.
        assert!(parse_session_meta(r#"{"type":"response_item","payload":{}}"#).is_none());
        assert!(parse_session_meta("not json").is_none());
    }

    #[test]
    fn classifies_legacy_response_items() {
        let s = scan(&[
            META,
            r#"{"type":"turn_context","payload":{"model":"gpt-5-codex","cwd":"/home/jat/project"}}"#,
            r#"{"timestamp":"2026-07-12T14:03:20.101Z","type":"response_item","payload":{"type":"local_shell_call","call_id":"c1","status":"completed","action":{"type":"exec","command":["bash","-lc","cargo test"]}}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"ls -la\"],\"workdir\":\"/p\"}","call_id":"c2"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c3","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/main.rs\n*** End Patch"}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"view_image","arguments":"{\"path\":\"/p/diagram.png\"}","call_id":"c4"}}"#,
            r#"{"type":"response_item","payload":{"type":"web_search_call","status":"completed","action":{"type":"search","query":"rust tokio mpsc"}}}"#,
        ]);
        let kinds: Vec<ActionKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Command,
                ActionKind::Command,
                ActionKind::Edit,
                ActionKind::Read,
                ActionKind::WebSearch,
            ]
        );
        assert_eq!(s.events[0].detail, "cargo test");
        assert_eq!(s.events[0].ts_ms, Some(1_783_865_000_101)); // 2026-07-12T14:03:20.101Z
        assert_eq!(s.events[1].detail, "ls -la");
        assert_eq!(s.events[2].detail, "src/main.rs");
        assert_eq!(s.events[3].detail, "/p/diagram.png");
        assert_eq!(s.events[4].detail, "rust tokio mpsc");
        assert_eq!(s.meta.model.as_deref(), Some("gpt-5-codex"));
    }

    #[test]
    fn command_output_and_patch_end_patch_without_double_counting() {
        let s = scan(&[
            META,
            r#"{"type":"response_item","payload":{"type":"local_shell_call","call_id":"c1","action":{"command":["bash","-lc","make"]}}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"built ok"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c2","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/lib.rs\n*** End Patch"}}"#,
            r#"{"type":"event_msg","payload":{"type":"patch_apply_end","call_id":"c2","stdout":"Success. Updated the following files:\nM src/lib.rs\n","success":true,"status":"completed","changes":{"src/lib.rs":{"type":"update","unified_diff":"@@"}}}}"#,
        ]);
        // Two actions total: one command, one edit — the outcome records patch
        // rather than adding new events.
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.events[0].kind, ActionKind::Command);
        assert_eq!(s.events[0].result_head.as_deref(), Some("built ok"));
        assert_eq!(s.events[0].ok, None); // no exit code recorded in legacy mode
        assert_eq!(s.events[1].kind, ActionKind::Edit);
        assert_eq!(s.events[1].detail, "src/lib.rs");
        assert_eq!(s.events[1].ok, Some(true));
        assert!(s.events[1]
            .result_head
            .as_deref()
            .unwrap()
            .contains("src/lib.rs"));
    }

    #[test]
    fn patch_apply_end_synthesizes_edit_when_no_prior_call() {
        // No response_item apply_patch precedes it → created from `changes`.
        let s = scan(&[
            META,
            r#"{"type":"event_msg","payload":{"type":"patch_apply_end","call_id":"z","success":false,"changes":{"/p/new.rs":{"type":"add","content":"x"}}}}"#,
        ]);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, ActionKind::Edit);
        assert_eq!(s.events[0].detail, "/p/new.rs");
        assert_eq!(s.events[0].ok, Some(false));
    }

    #[test]
    fn unknown_function_call_upgraded_to_mcp_by_end_event() {
        let s = scan(&[
            META,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"create_issue","arguments":"{\"title\":\"bug\"}","call_id":"m1"}}"#,
            r##"{"type":"event_msg","payload":{"type":"mcp_tool_call_end","call_id":"m1","invocation":{"server":"github","tool":"create_issue","arguments":{"title":"bug"}},"result":{"Ok":{"content":[{"type":"text","text":"#123"}]}}}}"##,
        ]);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, ActionKind::Mcp);
        assert_eq!(s.events[0].detail, "github:create_issue");
        assert_eq!(s.events[0].ok, Some(true));
        assert_eq!(s.events[0].result_head.as_deref(), Some("#123"));
    }

    #[test]
    fn mcp_end_creates_event_when_begin_absent() {
        let s = scan(&[
            META,
            r#"{"type":"event_msg","payload":{"type":"mcp_tool_call_end","call_id":"m9","invocation":{"server":"fs","tool":"list"},"result":{"Err":{"content":[{"text":"denied"}]}}}}"#,
        ]);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, ActionKind::Mcp);
        assert_eq!(s.events[0].detail, "fs:list");
        assert_eq!(s.events[0].ok, Some(false));
    }

    #[test]
    fn tokens_subagent_origin_and_web_fetch() {
        let s = scan(&[
            r#"{"type":"session_meta","payload":{"id":"child","parent_thread_id":"parent","agent_role":"explorer","cwd":"/p","history_mode":"legacy"}}"#,
            r#"{"type":"response_item","payload":{"type":"web_search_call","action":{"type":"open_page","url":"https://docs.rs/tokio"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"sub_agent_activity","kind":"started","agent_thread_id":"gc","agent_path":"reviewer"}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"output_tokens":1500,"total_tokens":13500}}}}"#,
        ]);
        assert_eq!(s.events[0].kind, ActionKind::WebFetch);
        assert_eq!(s.events[0].detail, "https://docs.rs/tokio");
        // Every event on a subagent rollout is labelled with its role.
        assert_eq!(s.events[0].origin.as_deref(), Some("explorer"));
        assert_eq!(s.events[1].kind, ActionKind::Subagent);
        assert_eq!(s.events[1].detail, "reviewer");
        assert_eq!(s.meta.output_tokens, Some(1500));
    }

    #[test]
    fn paginated_items_are_authoritative() {
        let meta =
            r#"{"type":"session_meta","payload":{"id":"t","cwd":"/p","history_mode":"paginated"}}"#;
        let s = scan(&[
            meta,
            // A raw response item in paginated mode must be ignored (the
            // item_completed record below is the source of truth).
            r#"{"type":"response_item","payload":{"type":"local_shell_call","call_id":"c1","action":{"command":["bash","-lc","cat src/main.rs"]}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"command_execution","command":["bash","-lc","cat src/main.rs"],"cwd":"/p","exit_code":0,"status":"completed","aggregated_output":"fn main() {}"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"file_change","status":"completed","changes":{"/p/a.rs":{"type":"update"}}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"web_search","query":"ratatui table","action":{"type":"search","query":"ratatui table"}}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"image_view","path":"/p/diagram.png"}}}"#,
        ]);
        let kinds: Vec<ActionKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Command,
                ActionKind::Edit,
                ActionKind::WebSearch,
                ActionKind::Read,
            ]
        );
        assert_eq!(s.events[0].detail, "cat src/main.rs");
        assert_eq!(s.events[0].ok, Some(true)); // explicit exit_code == 0
        assert_eq!(s.events[0].result_head.as_deref(), Some("fn main() {}"));
        assert_eq!(s.events[1].detail, "/p/a.rs");
    }

    #[test]
    fn malformed_and_unknown_shapes_are_skipped() {
        let s = scan(&[
            "not json at all",
            META,
            r#"{"type":"response_item"}"#, // no payload
            r#"{"type":"response_item","payload":{"type":"local_shell_call","action":{"command":[]}}}"#, // empty command
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":"thinking"}}"#, // ignored kind
            r#"{"type":"event_msg","payload":{"type":"turn_complete","duration_ms":10}}"#, // ignored event
            r#"{"type":"response_item","payload":{"type":"function_call","name":"apply_patch","arguments":"{not json"}}"#, // bad args, no path
            r#"{"type":"response_item","payload":{"type":"local_shell_call","call_id":"ok","action":{"command":["echo","hi"]}}}"#,
        ]);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].detail, "echo hi");
    }
}
