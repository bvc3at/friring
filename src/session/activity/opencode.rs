//! opencode (sst) activity provider — normalizes rows of the SQLite store at
//! `~/.local/share/opencode/opencode.db` into [`ActivityEvent`]s.
//!
//! Since opencode v1.2.0 every session/message/part lives in one SQLite DB
//! (drizzle-orm, WAL mode); there are no per-session JSON files. The activity
//! gold is the `part` table: each row has a `data` JSON blob whose `type`
//! discriminates the record. A `type:"tool"` part is one tool call, and —
//! unlike Claude Code / Vibe, where the call and its result are separate
//! transcript lines — the whole lifecycle is *self-contained* in the one row:
//! `state.status` (`pending`/`running`/`completed`/`error`), `state.input`,
//! `state.output`/`state.error`, `state.metadata`, and `state.time`. So this
//! layer is a pure per-row mapper, not a streaming accumulator: no cross-record
//! correlation is needed. `type:"subtask"` parts record subagent delegations.
//! Session metadata lives on the `session` row's columns (`title`, `model`,
//! `tokens_output`). The `id`/`sessionID`/`messageID` fields are stripped from
//! the blob and stored as columns, so they never appear here.
//!
//! Provenance: schemas verified against sst/opencode dev HEAD (v1.17.18,
//! 2026-07-12) — `packages/schema/src/v1/session.ts` (`ToolPart`,
//! `SubtaskPart`, `ToolStateCompleted`/`Error`), `packages/core/src/tool/*.ts`
//! (the built-in `name` literals), cross-checked against a live schema dump of
//! a docker-installed `opencode-ai@1.17.18` database. Defensive like every
//! provider: unknown shapes are skipped, never errors.

use serde_json::Value;

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for compact-input notes (`Other`/`Mcp`) and prompt-derived details.
const NOTE_MAX: usize = 160;

/// Built-in tools that record no action on the world — plan/answer
/// bookkeeping, skipped so the timeline shows work. `todowrite` is the agent's
/// task list; `question` asks the user something. (Names verified in
/// `packages/core/src/tool/{todowrite,question}.ts`.)
const SKIPPED_TOOLS: &[&str] = &["todowrite", "question"];

/// Map one `part` row to its events. `data` is the row's `data` JSON column;
/// `time_created` is the row's epoch-ms timestamp column. Most parts yield zero
/// (non-activity records) or one event; a multi-file `apply_patch` yields one
/// [`ActionKind::Edit`] per touched file. Malformed rows yield nothing.
pub fn part_events(data: &str, time_created: Option<i64>) -> Vec<ActivityEvent> {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return Vec::new();
    };
    let ts = time_created.and_then(|t| u64::try_from(t).ok());
    let mut events = match v.get("type").and_then(|t| t.as_str()) {
        Some("tool") => tool_events(&v),
        Some("subtask") => subtask_event(&v).into_iter().collect(),
        _ => Vec::new(),
    };
    for e in &mut events {
        e.ts_ms = ts;
    }
    events
}

/// Events for a `type:"tool"` part: classify by `tool`, then stamp the shared
/// outcome (`ok`, `result_head`) read from the same `state`.
fn tool_events(part: &Value) -> Vec<ActivityEvent> {
    let Some(tool) = part.get("tool").and_then(|t| t.as_str()) else {
        return Vec::new();
    };
    if SKIPPED_TOOLS.contains(&tool) {
        return Vec::new();
    }
    let state = part.get("state");
    let status = state.and_then(|s| s.get("status")).and_then(|s| s.as_str());
    let input = state.and_then(|s| s.get("input")).unwrap_or(&Value::Null);
    let metadata = state.and_then(|s| s.get("metadata"));

    let mut events = classify(tool, input, metadata);
    if events.is_empty() {
        return events;
    }
    let ok = tool_ok(tool, status, metadata);
    let result = result_head(status, state);
    for e in &mut events {
        e.ok = ok;
        e.result_head = result.clone();
    }
    events
}

/// Map a tool's `name` + `state.input` to base events (kind/detail/note only;
/// `ok`/`result_head`/`ts_ms` are filled by the caller). Empty ⇒ the input is
/// malformed for its tool (e.g. a `bash` without `command`) and is skipped.
fn classify(tool: &str, input: &Value, metadata: Option<&Value>) -> Vec<ActivityEvent> {
    let get = |key: &str| input.get(key).and_then(|x| x.as_str()).map(String::from);

    match tool {
        "bash" => one(get("command").map(|c| base(ActionKind::Command, c, None))),
        "edit" | "write" => one(get("path").map(|p| base(ActionKind::Edit, p, None))),
        "apply_patch" => patch_events(metadata),
        "read" => one(get("path").map(|p| base(ActionKind::Read, p, None))),
        "grep" | "glob" => one(get("pattern").map(|p| base(ActionKind::Search, p, get("path")))),
        "webfetch" => one(get("url").map(|u| base(ActionKind::WebFetch, u, None))),
        "websearch" => one(get("query").map(|q| base(ActionKind::WebSearch, q, None))),
        // Tool-call form of a delegation (the durable record is a `subtask`
        // part, but older builds surface it as a `task` tool).
        "task" => {
            let detail = get("description").or_else(|| get("prompt").map(|p| head(&p, NOTE_MAX)));
            let agent = get("agent").or_else(|| get("subagent_type"));
            one(detail.map(|d| base(ActionKind::Subagent, d, agent)))
        }
        _ => {
            let (kind, detail) = match mcp_name(tool) {
                Some(pretty) => (ActionKind::Mcp, pretty),
                None => (ActionKind::Other, tool.to_string()),
            };
            vec![base(kind, detail, compact_input(input))]
        }
    }
}

/// One `Edit` per file in an `apply_patch`'s `metadata.files` (`FileDiff[]`,
/// each with a `path`). No file metadata ⇒ nothing (defensive: an
/// unrecognized patch shape is skipped rather than logged as a pathless edit
/// that would pollute the Files aggregation).
fn patch_events(metadata: Option<&Value>) -> Vec<ActivityEvent> {
    let Some(files) = metadata
        .and_then(|m| m.get("files"))
        .and_then(|f| f.as_array())
    else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(|f| f.get("path").and_then(|p| p.as_str()))
        .map(|path| base(ActionKind::Edit, path.to_string(), None))
        .collect()
}

/// A `type:"subtask"` part: a subagent delegation. `description` (short label)
/// is the detail, falling back to a capped `prompt`; the target `agent` is the
/// note.
fn subtask_event(part: &Value) -> Option<ActivityEvent> {
    let detail = part
        .get("description")
        .and_then(|d| d.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .or_else(|| {
            part.get("prompt")
                .and_then(|p| p.as_str())
                .map(|p| head(p, NOTE_MAX))
        })?;
    let agent = part.get("agent").and_then(|a| a.as_str()).map(String::from);
    Some(base(ActionKind::Subagent, detail, agent))
}

/// Success from explicit markers only: an `error` status fails; a `completed`
/// status succeeds, except a `bash` whose `metadata.exit` is non-zero (the
/// command ran to completion but returned a failing code). A still-running or
/// pending part has no known outcome yet.
fn tool_ok(tool: &str, status: Option<&str>, metadata: Option<&Value>) -> Option<bool> {
    match status {
        Some("error") => Some(false),
        Some("completed") => {
            if tool == "bash" {
                if let Some(exit) = metadata
                    .and_then(|m| m.get("exit"))
                    .and_then(|e| e.as_i64())
                {
                    return Some(exit == 0);
                }
            }
            Some(true)
        }
        _ => None,
    }
}

/// Head of the action's result: `state.error` on an error, else the
/// model-facing `state.output` (a string per the schema; non-string outputs
/// carry no head).
fn result_head(status: Option<&str>, state: Option<&Value>) -> Option<String> {
    let field = if status == Some("error") {
        "error"
    } else {
        "output"
    };
    let text = state.and_then(|s| s.get(field)).and_then(|t| t.as_str())?;
    let text = text.trim();
    (!text.is_empty()).then(|| head(text, RESULT_HEAD_MAX))
}

/// An opencode MCP tool is keyed `<server>_<tool>` (verified: opencode groups
/// them by `key.split("_")[0]`), so the first `_` separates server from tool.
/// A name without one is never MCP.
fn mcp_name(tool: &str) -> Option<String> {
    let (server, rest) = tool.split_once('_')?;
    (!server.is_empty() && !rest.is_empty()).then(|| format!("{server}:{rest}"))
}

/// One-line compact rendering of a tool input for `note`, capped.
fn compact_input(input: &Value) -> Option<String> {
    if input.is_null() || input.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    Some(head(&input.to_string(), NOTE_MAX))
}

/// Base event with defaults; `note` is dropped when blank.
fn base(kind: ActionKind, detail: String, note: Option<String>) -> ActivityEvent {
    ActivityEvent {
        ts_ms: None,
        kind,
        detail,
        note: note.filter(|n| !n.trim().is_empty()),
        result_head: None,
        ok: None,
        origin: None,
    }
}

/// `Some(event)` → a one-element vec, `None` → empty.
fn one(event: Option<ActivityEvent>) -> Vec<ActivityEvent> {
    event.into_iter().collect()
}

/// Session metadata from a `session` row's columns. `model` is the JSON
/// `{"id":…,"providerID":…}` blob (rendered `provider/id`); a placeholder
/// `title` (opencode's default `New session - <ISO>`) and a zero token total
/// are treated as absent so the overview shows nothing rather than noise.
pub fn session_meta(
    title: Option<&str>,
    model: Option<&str>,
    output_tokens: Option<i64>,
) -> ActivityMeta {
    ActivityMeta {
        title: title
            .map(str::trim)
            .filter(|t| !t.is_empty() && !is_placeholder_title(t))
            .map(String::from),
        model: model.and_then(model_id),
        output_tokens: output_tokens
            .and_then(|t| u64::try_from(t).ok())
            .filter(|&t| t > 0),
    }
}

/// opencode's default, pre-summary session title.
fn is_placeholder_title(title: &str) -> bool {
    title.starts_with("New session -")
}

/// `provider/id` from a `session.model` JSON blob, or just `id` when the
/// provider is absent.
fn model_id(model: &str) -> Option<String> {
    let v: Value = serde_json::from_str(model).ok()?;
    let id = v.get("id").and_then(|x| x.as_str())?;
    match v.get("providerID").and_then(|x| x.as_str()) {
        Some(p) if !p.is_empty() => Some(format!("{p}/{id}")),
        _ => Some(id.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap a `state.data` JSON blob in the row's timestamp for `part_events`.
    fn events(data: &str) -> Vec<ActivityEvent> {
        part_events(data, Some(1_783_826_285_000))
    }

    #[test]
    fn classifies_builtin_tools() {
        let bash = events(
            r#"{"type":"tool","callID":"c1","tool":"bash","state":{"status":"completed","input":{"command":"echo hello","workdir":"/proj"},"output":"hello\n","title":"echo hello","metadata":{"exit":0},"time":{"start":1,"end":2}}}"#,
        );
        assert_eq!(bash.len(), 1);
        assert_eq!(bash[0].kind, ActionKind::Command);
        assert_eq!(bash[0].detail, "echo hello");
        assert_eq!(bash[0].ok, Some(true));
        assert_eq!(bash[0].result_head.as_deref(), Some("hello"));
        assert_eq!(bash[0].ts_ms, Some(1_783_826_285_000));

        let edit = events(
            r#"{"type":"tool","tool":"edit","state":{"status":"completed","input":{"path":"src/main.rs","oldString":"a","newString":"b"},"output":"@@","title":"src/main.rs","metadata":{"replacements":1}}}"#,
        );
        assert_eq!(edit[0].kind, ActionKind::Edit);
        assert_eq!(edit[0].detail, "src/main.rs");

        let write = events(
            r##"{"type":"tool","tool":"write","state":{"status":"completed","input":{"path":"NEW.md","content":"# hi"},"output":"","title":"NEW.md","metadata":{}}}"##,
        );
        assert_eq!(write[0].kind, ActionKind::Edit);
        assert_eq!(write[0].detail, "NEW.md");

        let read = events(
            r#"{"type":"tool","tool":"read","state":{"status":"completed","input":{"path":"README.md","offset":1,"limit":2000},"output":"<1| hi\n","title":"README.md","metadata":{}}}"#,
        );
        assert_eq!(read[0].kind, ActionKind::Read);
        assert_eq!(read[0].detail, "README.md");

        let grep = events(
            r#"{"type":"tool","tool":"grep","state":{"status":"completed","input":{"pattern":"fn main","path":"src/"},"output":"","title":"","metadata":{}}}"#,
        );
        assert_eq!(grep[0].kind, ActionKind::Search);
        assert_eq!(grep[0].detail, "fn main");
        assert_eq!(grep[0].note.as_deref(), Some("src/"));

        let fetch = events(
            r##"{"type":"tool","tool":"webfetch","state":{"status":"completed","input":{"url":"https://example.com","format":"markdown"},"output":"# Example","title":"https://example.com","metadata":{}}}"##,
        );
        assert_eq!(fetch[0].kind, ActionKind::WebFetch);
        assert_eq!(fetch[0].detail, "https://example.com");

        let search = events(
            r#"{"type":"tool","tool":"websearch","state":{"status":"completed","input":{"query":"rust sqlite wal","numResults":5},"output":"1. ...","title":"rust sqlite wal","metadata":{"provider":"exa"}}}"#,
        );
        assert_eq!(search[0].kind, ActionKind::WebSearch);
        assert_eq!(search[0].detail, "rust sqlite wal");
    }

    #[test]
    fn bash_nonzero_exit_is_a_failure_even_when_completed() {
        let e = events(
            r#"{"type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"false"},"output":"","title":"false","metadata":{"exit":1}}}"#,
        );
        assert_eq!(e[0].ok, Some(false));
    }

    #[test]
    fn error_status_fails_and_uses_error_text() {
        let e = events(
            r#"{"type":"tool","callID":"c6","tool":"bash","state":{"status":"error","input":{"command":"false"},"error":"Command exited with code 1","time":{"start":1,"end":2}}}"#,
        );
        assert_eq!(e[0].ok, Some(false));
        assert_eq!(
            e[0].result_head.as_deref(),
            Some("Command exited with code 1")
        );
    }

    #[test]
    fn running_part_has_unknown_outcome_and_no_head() {
        let e = events(
            r#"{"type":"tool","tool":"bash","state":{"status":"running","input":{"command":"sleep 5"},"time":{"start":1}}}"#,
        );
        assert_eq!(e[0].kind, ActionKind::Command);
        assert_eq!(e[0].ok, None);
        assert_eq!(e[0].result_head, None);
    }

    #[test]
    fn apply_patch_emits_one_edit_per_file() {
        let e = events(
            r#"{"type":"tool","tool":"apply_patch","state":{"status":"completed","input":{"patchText":"..."},"output":"applied","title":"patch","metadata":{"files":[{"path":"a.rs","additions":2,"deletions":0},{"path":"b.rs","additions":0,"deletions":1}]}}}"#,
        );
        assert_eq!(e.len(), 2);
        assert!(e.iter().all(|x| x.kind == ActionKind::Edit));
        assert_eq!(e[0].detail, "a.rs");
        assert_eq!(e[1].detail, "b.rs");
        // The shared outcome is stamped on every emitted edit.
        assert!(e.iter().all(|x| x.ok == Some(true)));
    }

    #[test]
    fn subtask_part_is_a_subagent_delegation() {
        let e = events(
            r#"{"type":"subtask","prompt":"Investigate the failing test","description":"debug flaky test","agent":"general","model":{"providerID":"anthropic","modelID":"claude-3-5-haiku-latest"},"command":null}"#,
        );
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, ActionKind::Subagent);
        assert_eq!(e[0].detail, "debug flaky test");
        assert_eq!(e[0].note.as_deref(), Some("general"));
    }

    #[test]
    fn mcp_tools_split_server_and_tool_unknowns_are_other() {
        let mcp = events(
            r#"{"type":"tool","tool":"github_create_issue","state":{"status":"completed","input":{"title":"bug"},"output":"ok","title":"","metadata":{}}}"#,
        );
        assert_eq!(mcp[0].kind, ActionKind::Mcp);
        assert_eq!(mcp[0].detail, "github:create_issue");
        assert!(mcp[0].note.as_deref().unwrap().contains("bug"));

        // A single-word unknown tool has no server prefix → Other.
        let other = events(
            r#"{"type":"tool","tool":"list","state":{"status":"completed","input":{"path":"/x"},"output":"a\nb","title":"","metadata":{}}}"#,
        );
        assert_eq!(other[0].kind, ActionKind::Other);
        assert_eq!(other[0].detail, "list");
    }

    #[test]
    fn bookkeeping_tools_and_malformed_rows_are_skipped() {
        assert!(events(
            r#"{"type":"tool","tool":"todowrite","state":{"status":"completed","input":{"todos":[]},"output":"","title":"","metadata":{}}}"#,
        )
        .is_empty());
        assert!(events(
            r#"{"type":"tool","tool":"question","state":{"status":"completed","input":{"questions":[]},"output":"","title":"","metadata":{}}}"#,
        )
        .is_empty());
        // bash with no command → malformed → skipped.
        assert!(events(
            r#"{"type":"tool","tool":"bash","state":{"status":"completed","input":{},"output":"","title":"","metadata":{}}}"#,
        )
        .is_empty());
        // Non-activity part types and non-JSON are skipped.
        assert!(events(r#"{"type":"step-finish","reason":"stop"}"#).is_empty());
        assert!(part_events("not json", None).is_empty());
        // apply_patch without file metadata → skipped, no pathless edit.
        assert!(events(
            r#"{"type":"tool","tool":"apply_patch","state":{"status":"completed","input":{"patchText":"x"},"output":"","title":"","metadata":{}}}"#,
        )
        .is_empty());
    }

    #[test]
    fn session_meta_extracts_title_model_and_tokens() {
        let m = session_meta(
            Some("Refactor the scanner"),
            Some(
                r#"{"id":"claude-3-5-haiku-latest","providerID":"anthropic","variant":"default"}"#,
            ),
            Some(2811),
        );
        assert_eq!(m.title.as_deref(), Some("Refactor the scanner"));
        assert_eq!(
            m.model.as_deref(),
            Some("anthropic/claude-3-5-haiku-latest")
        );
        assert_eq!(m.output_tokens, Some(2811));
    }

    #[test]
    fn session_meta_drops_placeholder_title_and_zero_tokens() {
        let m = session_meta(
            Some("New session - 2026-07-12T03:18:02.840Z"),
            Some(r#"{"id":"gpt-5","providerID":""}"#),
            Some(0),
        );
        assert_eq!(m.title, None);
        // Empty providerID → bare model id.
        assert_eq!(m.model.as_deref(), Some("gpt-5"));
        assert_eq!(m.output_tokens, None);

        // Garbled model JSON and absent columns degrade to None.
        let empty = session_meta(None, Some("{"), None);
        assert_eq!(empty, ActivityMeta::default());
    }
}
