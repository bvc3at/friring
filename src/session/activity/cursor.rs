//! Cursor CLI (`cursor-agent`) activity provider — normalizes a session's
//! human-readable agent transcript into [`ActivityEvent`]s.
//!
//! Cursor keeps two per-session stores: an authoritative SQLite `store.db`
//! (message blobs + a hex-JSON `meta` row) under a `chats/<md5(cwd)>/` dir,
//! and a derived NDJSON transcript at
//! `projects/<sanitize(cwd)>/agent-transcripts/<chatId>/<chatId>.jsonl`. This
//! provider reads the **transcript** only: it needs no md5 of the cwd (friring
//! has no md5 dependency and adds none), and the transcript is a plain,
//! parseable line format. The tradeoff is that tool *outputs* (stdout, exit
//! codes, file contents), token counts, the human title, and the model live
//! only in the undecoded protobuf blobs / `store.db meta` — so this provider
//! surfaces *what the agent did* but leaves `ok`, `result_head`, tokens, and
//! model unset, and derives the title from the user's first prompt.
//!
//! Each transcript line is `{"role":…,"message":{"content":[<blocks>]}}`.
//! Only two block types are written on disk: `{"type":"text","text":…}` and
//! `{"type":"tool_use","name":<tool>,"input":<args>}` (tool results are never
//! written). `assistant` lines carry the actions; the first `user` line's text
//! (wrapped in `<user_query>…</user_query>`) is the title fallback. Lines have
//! no timestamps, ids, or model, so events order by stream position and
//! `ts_ms` is always `None`.
//!
//! Verified against `cursor-agent` v2026.07.09-a3815c0 (its on-disk format is
//! undocumented and explicitly volatile). Defensive like every provider:
//! unknown shapes are skipped, never errors. One field the evidence could not
//! fully pin is the on-disk name of the web-search tool; per the verified
//! sample a bare `search` (with `query`) is a web search, while the semantic
//! codebase search surfaces under `codebase_search`/`grep`/`glob`/`ls`.

use super::{head, ActionKind, ActivityEvent, ActivityMeta};

/// Cap for compact-input notes (`Other`/`Mcp` events) and the title fallback.
const NOTE_MAX: usize = 160;

/// Streaming scanner over one Cursor `<chatId>.jsonl` transcript. Feed
/// line-aligned chunks in file order via [`ingest`](Self::ingest); read the
/// accumulated [`events`](Self::events) and [`meta`](Self::meta) between feeds.
///
/// Unlike the append-only Claude/Vibe transcripts, Cursor regenerates this
/// file as a full snapshot from its blob chain, so the app-layer scanner feeds
/// a fresh `CursorScan` the whole file each change rather than tailing — there
/// is no cross-call result correlation to keep, hence no pending map.
#[derive(Debug, Clone, Default)]
pub struct CursorScan {
    pub events: Vec<ActivityEvent>,
    pub meta: ActivityMeta,
}

impl CursorScan {
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
            match v.get("role").and_then(|r| r.as_str()) {
                Some("assistant") => self.ingest_assistant(&v),
                Some("user") => self.ingest_user(&v),
                _ => {}
            }
        }
    }

    fn ingest_assistant(&mut self, msg: &serde_json::Value) {
        let Some(content) = msg.pointer("/message/content").and_then(|c| c.as_array()) else {
            return;
        };
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let Some(name) = block.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if let Some(event) = classify(name, block.get("input")) {
                self.events.push(event);
            }
        }
    }

    fn ingest_user(&mut self, msg: &serde_json::Value) {
        // Title fallback: the first user text. The store.db `meta.name`
        // (agent-generated title) would be better but requires the md5 chats
        // path this provider deliberately avoids.
        if self.meta.title.is_some() {
            return;
        }
        let Some(content) = msg.pointer("/message/content").and_then(|c| c.as_array()) else {
            return;
        };
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) != Some("text") {
                continue;
            }
            let Some(text) = block.get("text").and_then(|t| t.as_str()) else {
                continue;
            };
            let cleaned = strip_user_query(text);
            if !cleaned.is_empty() {
                self.meta.title = Some(head(cleaned, NOTE_MAX));
                return;
            }
        }
    }
}

/// Map one `tool_use` block to an event. `None` ⇒ the block is malformed for
/// its tool (e.g. a `shell` without `command`) or is pure plan/mode
/// bookkeeping, and is skipped.
///
/// Tool names are the terse lowercase on-disk forms (`shell`, `read`, `edit`,
/// …); Cursor mirrors Claude's `tool_use` schema and input keys, so an
/// `mcp__server__tool` name maps like Claude's. Result text / exit codes are
/// never in the transcript, so `ok` and `result_head` stay unset.
fn classify(name: &str, input: Option<&serde_json::Value>) -> Option<ActivityEvent> {
    let input = input.unwrap_or(&serde_json::Value::Null);
    let get = |key: &str| input.get(key).and_then(|x| x.as_str()).map(String::from);

    let (kind, detail, note) = match name {
        "shell" => (ActionKind::Command, get("command")?, get("description")),
        "edit" | "write" | "delete" => (ActionKind::Edit, get("path")?, None),
        "read" => (ActionKind::Read, get("path")?, None),
        "ls" => (ActionKind::Search, get("path")?, None),
        "grep" => (ActionKind::Search, get("pattern")?, get("path")),
        "glob" => (
            ActionKind::Search,
            get("globPattern").or_else(|| get("pattern"))?,
            get("target_directory").or_else(|| get("path")),
        ),
        "codebase_search" | "semantic_search" | "sem_search" => {
            (ActionKind::Search, get("query")?, None)
        }
        "search" | "web_search" | "web-search" | "websearch" => {
            (ActionKind::WebSearch, get("query")?, None)
        }
        "fetch" | "web_fetch" | "web-fetch" | "webfetch" => {
            (ActionKind::WebFetch, get("url")?, None)
        }
        "task" => {
            let detail =
                get("description").or_else(|| get("prompt").map(|p| head(&p, NOTE_MAX)))?;
            (
                ActionKind::Subagent,
                detail,
                get("subagent_type").or_else(|| get("agent")),
            )
        }
        // Pure plan / mode bookkeeping — no action on the world.
        "todo" | "todos" | "update_todos" | "read_todos" | "create_plan" | "switch_mode"
        | "exit_plan_mode" => return None,
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
        minor: false,
        dur_ms: None,
    })
}

/// `mcp__server__tool` → `server:tool` (tool names may themselves contain
/// `__`, so only the first split separates server from tool). Mirrors the
/// Claude provider, since Cursor uses the same `tool_use` naming.
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

/// Unwrap the `<user_query>…</user_query>` envelope Cursor writes around the
/// user's first prompt, trimming surrounding whitespace. Text without the
/// envelope is returned trimmed as-is.
fn strip_user_query(text: &str) -> &str {
    let t = text.trim();
    match t
        .strip_prefix("<user_query>")
        .and_then(|r| r.strip_suffix("</user_query>"))
    {
        Some(inner) => inner.trim(),
        None => t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> CursorScan {
        let mut scan = CursorScan::default();
        scan.ingest(s);
        scan
    }

    #[test]
    fn classifies_builtin_tools() {
        // One transcript line (newlines only for readability — folded below).
        let jsonl = r#"{"role":"assistant","message":{"content":[
                {"type":"text","text":"Working."},
                {"type":"tool_use","name":"shell","input":{"command":"ls -la","description":"List directory"}},
                {"type":"tool_use","name":"edit","input":{"path":"src/main.rs","old_string":"foo","new_string":"bar"}},
                {"type":"tool_use","name":"write","input":{"path":"notes.md","file_text":"notes body"}},
                {"type":"tool_use","name":"read","input":{"path":"src/main.rs","offset":1,"limit":200}},
                {"type":"tool_use","name":"grep","input":{"pattern":"fn main","path":"src/"}},
                {"type":"tool_use","name":"fetch","input":{"url":"https://example.com/docs"}},
                {"type":"tool_use","name":"search","input":{"query":"ratatui table widget"}},
                {"type":"tool_use","name":"task","input":{"description":"Investigate failing test","prompt":"..."}},
                {"type":"tool_use","name":"todo","input":{"items":[]}},
                {"type":"tool_use","name":"mcp__github__create_issue","input":{"title":"bug"}}
            ]}}"#
            .replace('\n', " ");
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
                ActionKind::WebFetch,
                ActionKind::WebSearch,
                ActionKind::Subagent,
                ActionKind::Mcp, // todo skipped
            ]
        );
        assert_eq!(s.events[0].detail, "ls -la");
        assert_eq!(s.events[0].note.as_deref(), Some("List directory"));
        assert_eq!(s.events[1].detail, "src/main.rs");
        assert_eq!(s.events[2].detail, "notes.md");
        assert_eq!(s.events[4].detail, "fn main");
        assert_eq!(s.events[4].note.as_deref(), Some("src/"));
        assert_eq!(s.events[5].detail, "https://example.com/docs");
        assert_eq!(s.events[6].detail, "ratatui table widget");
        assert_eq!(s.events[7].detail, "Investigate failing test");
        assert_eq!(s.events[8].detail, "github:create_issue");
        assert!(s.events[8].note.as_deref().unwrap().contains("bug"));
        // The transcript records no results/timestamps: these stay unset.
        assert!(s.events.iter().all(|e| e.ts_ms.is_none()));
        assert!(s.events.iter().all(|e| e.ok.is_none()));
        assert!(s.events.iter().all(|e| e.result_head.is_none()));
        assert!(s.events.iter().all(|e| e.origin.is_none()));
    }

    #[test]
    fn distinguishes_codebase_and_web_search() {
        let s = scan(concat!(
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"codebase_search","input":{"query":"where is the parser"}}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"glob","input":{"globPattern":"**/*.rs","target_directory":"src"}}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"ls","input":{"path":"src"}}]}}"#,
        ));
        assert_eq!(
            s.events.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![ActionKind::Search, ActionKind::Search, ActionKind::Search]
        );
        assert_eq!(s.events[0].detail, "where is the parser");
        assert_eq!(s.events[1].detail, "**/*.rs");
        assert_eq!(s.events[1].note.as_deref(), Some("src"));
        assert_eq!(s.events[2].detail, "src");
    }

    #[test]
    fn user_query_becomes_title() {
        let s = scan(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nfix the activity tab\n</user_query>"}]}}"#,
        );
        assert_eq!(s.meta.title.as_deref(), Some("fix the activity tab"));
        assert!(s.meta.model.is_none());
        assert!(s.meta.output_tokens.is_none());
    }

    #[test]
    fn first_user_prompt_wins_and_plain_text_is_kept() {
        let mut s = CursorScan::default();
        s.ingest(r#"{"role":"user","message":{"content":[{"type":"text","text":"just do it"}]}}"#);
        s.ingest(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>later</user_query>"}]}}"#,
        );
        assert_eq!(s.meta.title.as_deref(), Some("just do it"));
    }

    #[test]
    fn malformed_and_unknown_shapes_are_skipped() {
        let s = scan(concat!(
            "not json\n",
            // A shell without a command → malformed for its tool → skipped.
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"shell","input":{}}]}}"#,
            "\n",
            // A tool_use without a name → skipped.
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","input":{"x":1}}]}}"#,
            "\n",
            // No content array → skipped, no panic.
            r#"{"role":"assistant","message":{}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"unknown_tool","input":{"x":1}}]}}"#,
        ));
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, ActionKind::Other);
        assert_eq!(s.events[0].detail, "unknown_tool");
        assert!(s.events[0].note.as_deref().unwrap().contains("\"x\":1"));
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
        assert_eq!(mcp_name("shell"), None);
    }

    #[test]
    fn strip_user_query_handles_envelope_and_bare_text() {
        assert_eq!(
            strip_user_query("<user_query>\n  hi  \n</user_query>"),
            "hi"
        );
        assert_eq!(strip_user_query("  bare prompt  "), "bare prompt");
        assert_eq!(
            strip_user_query("<user_query>unterminated"),
            "<user_query>unterminated"
        );
    }
}
