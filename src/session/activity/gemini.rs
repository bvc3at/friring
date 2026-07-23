//! Gemini CLI (Google) activity provider — normalizes a chat transcript
//! (`~/.gemini/tmp/<slug>/chats/session-<ts>-<id8>.jsonl`) into
//! [`ActivityEvent`]s.
//!
//! The transcript is append-only JSON Lines (one record per
//! `fs.appendFileSync`, since the JSONL-streaming migration — PR #23749):
//!
//! - **line 1** — a session metadata record (`sessionId`, `projectHash`,
//!   `startTime`, `kind`, `directories`);
//! - **message records** — `{"type":"gemini"|"user", id, timestamp, model,
//!   content[], tokens{}, toolCalls[]}`. A `gemini` message's `toolCalls[]`
//!   carry the agent's actions: each has `name`, `args` (a flat object keyed
//!   by the tool's params — the source of truth, per the research note that
//!   `content[]` `functionCall` parts may be truncated), an inline `status`,
//!   and an inline `result` — so, unlike Claude/Vibe, outcomes need no
//!   cross-record correlation;
//! - **`{"$set":{...}}`** — a metadata update appended later; the model's
//!   `summary` (taken as the title) arrives this way, never by rewriting
//!   line 1;
//! - **`{"$rewindTo":"<messageId>"}`** — logically truncates history back to a
//!   message *without* deleting prior lines, so a streaming consumer must
//!   discard the events of records appended after that id.
//!
//! [`GeminiScan`] is the streaming accumulator: feed it line-aligned chunks in
//! file order and it appends events, folds in metadata, and honors rewinds.
//!
//! This is the **pure** layer (arch rule `session` ← nothing): filesystem
//! discovery (cwd → slug via `projects.json`, newest session file) lives in
//! `app::activity::gemini`. Verified against @google/gemini-cli 0.50.0 (docker
//! ground-truth) and `main`@0.52.0-nightly source (`chatRecordingService.ts`,
//! tool param declarations). Defensive like every provider: unknown shapes are
//! skipped, never errors.

use super::{head, ActionKind, ActivityEvent, ActivityMeta, RESULT_HEAD_MAX};

/// Cap for titles and compact-input notes (`Other` events).
const NOTE_MAX: usize = 160;

/// Streaming scanner over one Gemini chat transcript. Feed complete lines via
/// [`ingest`](Self::ingest); read [`events`](Self::events) / [`meta`](Self::meta)
/// between feeds. Callers own chunking: a chunk must end on a line boundary
/// (a torn tail line is skipped as malformed and its events lost until the
/// next feed completes it).
#[derive(Debug, Clone, Default)]
pub struct GeminiScan {
    pub events: Vec<ActivityEvent>,
    pub meta: ActivityMeta,
    /// `sessionId` from the line-1 metadata record.
    pub session_id: Option<String>,
    /// Project roots from the metadata record's `directories`.
    pub directories: Vec<String>,
    /// `startTime` as epoch ms.
    pub start_ms: Option<u64>,
    /// Append order of records carrying an `id`, each tagged with the
    /// event-stream slice it produced — lets a `$rewindTo` record logically
    /// truncate the stream back to a message.
    marks: Vec<RecordMark>,
}

/// One recorded message's footprint on the event stream, for rewinds.
#[derive(Debug, Clone)]
struct RecordMark {
    id: String,
    /// Index of the first event this record appended.
    event_start: usize,
    /// Output tokens this record contributed to [`ActivityMeta::output_tokens`]
    /// — subtracted back out if a rewind discards it.
    out_tokens: u64,
}

impl GeminiScan {
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
            if let Some(target) = v.get("$rewindTo").and_then(|t| t.as_str()) {
                self.rewind_to(target);
            } else if let Some(set) = v.get("$set") {
                // The human-facing title is the model summary, appended here;
                // the last one wins.
                if let Some(summary) = set.get("summary").and_then(|s| s.as_str()) {
                    self.meta.title = Some(head(summary, NOTE_MAX));
                }
            } else {
                match str_field(&v, "type").as_deref() {
                    Some("gemini") => self.ingest_gemini(&v),
                    Some("user") => self.ingest_user(&v),
                    // A record with no `type` and no `$…` op is the line-1
                    // session metadata.
                    _ => self.ingest_metadata(&v),
                }
            }
        }
    }

    fn ingest_metadata(&mut self, v: &serde_json::Value) {
        let Some(sid) = str_field(v, "sessionId") else {
            return;
        };
        self.session_id = Some(sid);
        if let Some(dirs) = v.get("directories").and_then(|d| d.as_array()) {
            self.directories = dirs
                .iter()
                .filter_map(|d| d.as_str().map(String::from))
                .collect();
        }
        self.start_ms = v
            .get("startTime")
            .and_then(|t| t.as_str())
            .and_then(parse_iso_ms);
    }

    fn ingest_user(&mut self, v: &serde_json::Value) {
        self.note_record(v);
        // Title fallback: the first typed prompt (a `$set.summary` overrides it).
        if self.meta.title.is_none() {
            if let Some(t) = first_text(v.get("content")) {
                let t = t.trim();
                if !t.is_empty() {
                    self.meta.title = Some(head(t, NOTE_MAX));
                }
            }
        }
    }

    fn ingest_gemini(&mut self, v: &serde_json::Value) {
        let mark = self.note_record(v);
        if let Some(model) = str_field(v, "model") {
            self.meta.model = Some(model);
        }
        // No cumulative total is recorded — sum `output` across gemini messages.
        if let Some(out) = v.pointer("/tokens/output").and_then(|t| t.as_u64()) {
            self.meta.output_tokens = Some(self.meta.output_tokens.unwrap_or(0) + out);
            if let Some(i) = mark {
                self.marks[i].out_tokens += out;
            }
        }
        let msg_ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_iso_ms);
        let Some(calls) = v.get("toolCalls").and_then(|c| c.as_array()) else {
            return;
        };
        for call in calls {
            let Some(name) = str_field(call, "name") else {
                continue;
            };
            let Some(mut event) = classify(&name, call.get("args")) else {
                continue;
            };
            event.ts_ms = call
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(parse_iso_ms)
                .or(msg_ts);
            event.ok = status_ok(call);
            event.result_head = tool_result_head(call);
            // A tool executed inside a subagent carries the delegate's id.
            event.origin = str_field(call, "agentId");
            self.events.push(event);
        }
    }

    /// Record a mark for `v`'s `id` (rewind target), returning its index.
    fn note_record(&mut self, v: &serde_json::Value) -> Option<usize> {
        let id = str_field(v, "id")?;
        self.marks.push(RecordMark {
            id,
            event_start: self.events.len(),
            out_tokens: 0,
        });
        Some(self.marks.len() - 1)
    }

    /// Discard the events (and token contributions) of every record appended
    /// after `target`; keep `target` and earlier. A target we never saw (e.g.
    /// clipped by the initial-ingest cap) is a no-op.
    fn rewind_to(&mut self, target: &str) {
        let Some(pos) = self.marks.iter().position(|m| m.id == target) else {
            return;
        };
        let cutoff = self
            .marks
            .get(pos + 1)
            .map(|m| m.event_start)
            .unwrap_or(self.events.len());
        self.events.truncate(cutoff);
        let removed: u64 = self.marks[pos + 1..].iter().map(|m| m.out_tokens).sum();
        if removed > 0 {
            self.meta.output_tokens = self.meta.output_tokens.map(|t| t.saturating_sub(removed));
        }
        self.marks.truncate(pos + 1);
    }
}

/// Map one `toolCalls[]` entry to an event. `None` ⇒ the entry is malformed
/// for its tool (e.g. a `run_shell_command` without `command`) and is skipped.
/// Tool names are the current, legacy-neutral strings (`run_shell_command`,
/// `replace` for edits, `grep_search` for grep, `list_directory` for ls);
/// arg keys are tolerated as missing since they've drifted across versions.
/// Gemini has no dedicated plan/todo tool to filter (unlike Claude's
/// `TodoWrite`), so nothing is skipped by name — unknown tools surface as
/// `Other`.
fn classify(name: &str, args: Option<&serde_json::Value>) -> Option<ActivityEvent> {
    let args = args.unwrap_or(&serde_json::Value::Null);
    let get = |key: &str| args.get(key).and_then(|x| x.as_str()).map(String::from);

    let (kind, detail, note) = match name {
        "run_shell_command" => (ActionKind::Command, get("command")?, get("description")),
        "write_file" | "replace" => (ActionKind::Edit, get("file_path")?, None),
        "read_file" => (ActionKind::Read, get("file_path")?, None),
        "read_many_files" => (ActionKind::Read, include_globs(args)?, None),
        "glob" | "grep_search" => (ActionKind::Search, get("pattern")?, get("path")),
        "list_directory" => (ActionKind::Search, get("path")?, None),
        "google_web_search" => (ActionKind::WebSearch, get("query")?, None),
        // Standard mode carries the URL(s) inside `prompt`; the experimental
        // direct-fetch mode uses `url`.
        "web_fetch" => (
            ActionKind::WebFetch,
            get("url").or_else(|| get("prompt"))?,
            None,
        ),
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

/// The `include` glob patterns of a `read_many_files` call, comma-joined.
fn include_globs(args: &serde_json::Value) -> Option<String> {
    let arr = args.get("include").and_then(|i| i.as_array())?;
    let joined = arr
        .iter()
        .filter_map(|g| g.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    (!joined.is_empty()).then_some(joined)
}

/// One-line compact rendering of a tool's args for `note`, capped.
fn compact_input(args: &serde_json::Value) -> Option<String> {
    if args.is_null() || args.as_object().is_some_and(|o| o.is_empty()) {
        return None;
    }
    Some(head(&args.to_string(), NOTE_MAX))
}

/// A tool call's success — only the explicit terminal `status` markers speak;
/// the transient states (`executing`, `validating`, `awaiting_approval`, …)
/// and `cancelled` leave `ok` unknown.
fn status_ok(call: &serde_json::Value) -> Option<bool> {
    match str_field(call, "status").as_deref() {
        Some("success") => Some(true),
        Some("error") => Some(false),
        _ => None,
    }
}

/// Head of a tool call's result — the `functionResponse.response.output`
/// string, else a plain-string `resultDisplay` — capped at parse time.
fn tool_result_head(call: &serde_json::Value) -> Option<String> {
    let text = call
        .get("result")
        .and_then(result_output_text)
        .or_else(|| {
            call.get("resultDisplay")
                .and_then(|d| d.as_str().map(String::from))
        })?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| head(trimmed, RESULT_HEAD_MAX))
}

/// First `functionResponse.response.output` string in a `result` parts array.
fn result_output_text(result: &serde_json::Value) -> Option<String> {
    let parts = result.as_array()?;
    parts.iter().find_map(|part| {
        part.pointer("/functionResponse/response/output")
            .and_then(|o| o.as_str())
            .map(String::from)
    })
}

/// First text from a message's `content`, which is a `@google/genai`
/// `PartListUnion`: a bare string, a single `Part`, or a `Part[]`.
fn first_text(content: Option<&serde_json::Value>) -> Option<String> {
    let c = content?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = c.as_array() {
        return arr
            .iter()
            .find_map(|part| part.get("text").and_then(|t| t.as_str()).map(String::from));
    }
    c.get("text").and_then(|t| t.as_str()).map(String::from)
}

/// Epoch ms from an ISO-8601 timestamp, when valid.
fn parse_iso_ms(ts: &str) -> Option<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    u64::try_from(dt.timestamp_millis()).ok()
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> GeminiScan {
        let mut scan = GeminiScan::default();
        scan.ingest(s);
        scan
    }

    #[test]
    fn classifies_builtin_tools() {
        // One gemini message with the full builtin tool spread (folded to one
        // JSONL line below).
        let jsonl = r#"{"id":"g1","timestamp":"2026-07-12T14:04:02.220Z","type":"gemini","model":"gemini-2.5-pro","toolCalls":[
                {"id":"c1","name":"run_shell_command","args":{"command":"cargo test","description":"Run the test suite"},"status":"success"},
                {"id":"c2","name":"replace","args":{"file_path":"/work/myproj/src/main.rs","old_string":"a","new_string":"b"},"status":"success"},
                {"id":"c3","name":"write_file","args":{"file_path":"/work/myproj/README.md","content":"hello"},"status":"success"},
                {"id":"c4","name":"read_file","args":{"file_path":"/work/myproj/Cargo.toml","start_line":1,"end_line":40},"status":"success"},
                {"id":"c5","name":"read_many_files","args":{"include":["src/**/*.rs"],"exclude":["target/**"]},"status":"success"},
                {"id":"c6","name":"grep_search","args":{"pattern":"fn main","path":"src/"},"status":"success"},
                {"id":"c7","name":"google_web_search","args":{"query":"ratatui table widget"},"status":"success"},
                {"id":"c8","name":"web_fetch","args":{"prompt":"Summarize https://docs.rs/ratatui"},"status":"success"},
                {"id":"c9","name":"save_memory","args":{"fact":"prefers tabs"},"status":"success"}
            ]}"#
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
                ActionKind::Read,
                ActionKind::Search,
                ActionKind::WebSearch,
                ActionKind::WebFetch,
                ActionKind::Other, // save_memory is unrecognized
            ]
        );
        assert_eq!(s.events[0].detail, "cargo test");
        assert_eq!(s.events[0].note.as_deref(), Some("Run the test suite"));
        assert_eq!(s.events[0].ok, Some(true));
        assert_eq!(s.events[0].ts_ms, Some(1783865042220)); // 2026-07-12T14:04:02.220Z
        assert_eq!(s.events[1].detail, "/work/myproj/src/main.rs");
        assert_eq!(s.events[4].detail, "src/**/*.rs"); // read_many_files include
        assert_eq!(s.events[5].note.as_deref(), Some("src/")); // grep path
        assert_eq!(s.events[7].detail, "Summarize https://docs.rs/ratatui"); // web_fetch prompt
        assert_eq!(s.events[8].detail, "save_memory");
        assert!(s.events[8]
            .note
            .as_deref()
            .unwrap()
            .contains("prefers tabs"));
    }

    #[test]
    fn tool_status_and_result_are_inline_per_call() {
        let s = scan(
            r#"{"id":"g1","type":"gemini","toolCalls":[
                {"id":"c1","name":"run_shell_command","args":{"command":"cargo test"},"status":"success","result":[{"functionResponse":{"name":"run_shell_command","response":{"output":"test result: ok. 42 passed"}}}]},
                {"id":"c2","name":"run_shell_command","args":{"command":"false"},"status":"error","result":[{"functionResponse":{"name":"run_shell_command","response":{"output":"exit 1"}}}]},
                {"id":"c3","name":"read_file","args":{"file_path":"/x"},"status":"executing"}
            ]}"#
                .replace('\n', " ")
                .as_str(),
        );
        assert_eq!(s.events[0].ok, Some(true));
        assert_eq!(
            s.events[0].result_head.as_deref(),
            Some("test result: ok. 42 passed")
        );
        assert_eq!(s.events[1].ok, Some(false));
        // A transient status leaves the outcome unknown.
        assert_eq!(s.events[2].ok, None);
        assert!(s.events[2].result_head.is_none());
    }

    #[test]
    fn tool_call_inside_subagent_gets_origin() {
        let s = scan(
            r#"{"id":"g1","type":"gemini","toolCalls":[{"id":"c1","name":"run_shell_command","args":{"command":"pwd"},"status":"success","agentId":"sub-7"}]}"#,
        );
        assert_eq!(s.events[0].origin.as_deref(), Some("sub-7"));
    }

    #[test]
    fn metadata_record_and_summary_and_prompt_title() {
        let mut s = GeminiScan::default();
        s.ingest(
            r#"{"sessionId":"b1e6f0a2-7c4d-4e2a-9a1f-3d5b8c0e2f11","projectHash":"9f2c","startTime":"2026-07-12T14:03:11.482Z","kind":"main","directories":["/work/myproj"]}"#,
        );
        assert_eq!(
            s.session_id.as_deref(),
            Some("b1e6f0a2-7c4d-4e2a-9a1f-3d5b8c0e2f11")
        );
        assert_eq!(s.directories, vec!["/work/myproj".to_string()]);
        assert!(s.start_ms.is_some());

        // First user prompt seeds the title.
        s.ingest(
            r#"{"id":"u1","type":"user","content":[{"text":"add a test for the jsonl parser"}]}"#,
        );
        assert_eq!(
            s.meta.title.as_deref(),
            Some("add a test for the jsonl parser")
        );

        // A later $set.summary overrides it.
        s.ingest(r#"{"$set":{"summary":"Added unit tests for the JSONL parser.","lastUpdated":"2026-07-12T14:07:55.101Z"}}"#);
        assert_eq!(
            s.meta.title.as_deref(),
            Some("Added unit tests for the JSONL parser.")
        );
    }

    #[test]
    fn model_and_output_tokens_sum_across_messages() {
        let s = scan(concat!(
            r#"{"id":"g1","type":"gemini","model":"gemini-2.5-pro","tokens":{"input":5123,"output":88,"total":5211},"toolCalls":[]}"#,
            "\n",
            r#"{"id":"g2","type":"gemini","model":"gemini-2.5-flash","tokens":{"output":42},"toolCalls":[]}"#,
        ));
        assert_eq!(s.meta.model.as_deref(), Some("gemini-2.5-flash"));
        assert_eq!(s.meta.output_tokens, Some(130));
    }

    #[test]
    fn rewind_discards_later_events_and_their_tokens() {
        let mut s = GeminiScan::default();
        s.ingest(concat!(
            r#"{"id":"g1","type":"gemini","tokens":{"output":10},"toolCalls":[{"id":"c1","name":"read_file","args":{"file_path":"/keep"},"status":"success"}]}"#,
            "\n",
            r#"{"id":"g2","type":"gemini","tokens":{"output":20},"toolCalls":[{"id":"c2","name":"run_shell_command","args":{"command":"rm -rf /"},"status":"success"}]}"#,
        ));
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.meta.output_tokens, Some(30));

        // Rewind to g1 keeps g1's read, drops g2's command and its tokens.
        s.ingest(r#"{"$rewindTo":"g1"}"#);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].detail, "/keep");
        assert_eq!(s.meta.output_tokens, Some(10));

        // Further appends resume cleanly and remain rewindable.
        s.ingest(
            r#"{"id":"g3","type":"gemini","tokens":{"output":5},"toolCalls":[{"id":"c3","name":"read_file","args":{"file_path":"/new"},"status":"success"}]}"#,
        );
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.meta.output_tokens, Some(15));

        // A rewind to an unknown id is a no-op.
        s.ingest(r#"{"$rewindTo":"nope"}"#);
        assert_eq!(s.events.len(), 2);
    }

    #[test]
    fn incremental_chunks_accumulate() {
        let mut s = GeminiScan::default();
        s.ingest(
            r#"{"id":"g1","type":"gemini","toolCalls":[{"id":"c1","name":"read_file","args":{"file_path":"/a"},"status":"success"}]}"#,
        );
        assert_eq!(s.events.len(), 1);
        s.ingest(
            r#"{"id":"g2","type":"gemini","toolCalls":[{"id":"c2","name":"read_file","args":{"file_path":"/b"},"status":"success"}]}"#,
        );
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.events[1].detail, "/b");
    }

    #[test]
    fn malformed_and_incomplete_records_are_skipped() {
        let s = scan(concat!(
            "not json\n",
            // Missing `command` → the shell call is skipped.
            r#"{"id":"g1","type":"gemini","toolCalls":[{"id":"c1","name":"run_shell_command","args":{"description":"no command"},"status":"success"}]}"#,
            "\n",
            // A tool call with no name is skipped, a valid one survives.
            r#"{"id":"g2","type":"gemini","toolCalls":[{"id":"c2","args":{"x":1}},{"id":"c3","name":"read_file","args":{"file_path":"/c"},"status":"success"}]}"#,
        ));
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].detail, "/c");
    }

    #[test]
    fn web_fetch_prefers_url_over_prompt() {
        let s = scan(
            r#"{"id":"g1","type":"gemini","toolCalls":[{"id":"c1","name":"web_fetch","args":{"url":"https://docs.rs/ratatui","prompt":"summarize it"},"status":"success"}]}"#,
        );
        assert_eq!(s.events[0].kind, ActionKind::WebFetch);
        assert_eq!(s.events[0].detail, "https://docs.rs/ratatui");
    }
}
