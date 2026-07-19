//! Agent-neutral activity model — what a coding agent *did* in a session.
//!
//! The activity view (F9) reconstructs a per-session retrospective — shell
//! commands run, files edited, files read, web searches/fetches, subagent
//! delegations — from whatever the agent CLI persists on disk. Every agent
//! records this differently (JSONL transcripts, per-message JSON stores,
//! SQLite, markdown logs), so each supported CLI gets a **provider**: a pure
//! parser submodule here (`claude`, …) that normalizes its on-disk records
//! into the [`ActivityEvent`] stream, plus filesystem discovery glue in
//! `app::activity`.
//!
//! This module is the **pure** layer (arch rule `session` ← nothing): types
//! and parsers only, no filesystem access. Parsers are defensive — agent
//! on-disk formats are undocumented and version-specific, so unknown shapes
//! degrade to skipped records, never errors.

pub mod aider;
pub mod claude;
pub mod cline;
pub mod codex;
pub mod copilot;
pub mod crush;
pub mod cursor;
pub mod gemini;
pub mod goose;
pub mod opencode;
pub mod qwen;
pub mod vibe;

/// What kind of action an [`ActivityEvent`] records, normalized across
/// agents. `Other` carries actions worth showing on the timeline that fit no
/// dedicated category (its tool name goes in [`ActivityEvent::detail`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    /// A user prompt — the timeline's turn marker, not an agent action.
    /// Excluded from [`ActivityCounts::total`] and from subagent-origin
    /// streams (a subagent's task prompt is not a conversation turn).
    Prompt,
    /// A shell command the agent executed.
    Command,
    /// A file the agent edited, wrote, or patched.
    Edit,
    /// A file the agent read.
    Read,
    /// A codebase search (grep/glob-style), kept distinct from file reads.
    Search,
    WebSearch,
    WebFetch,
    /// A delegation to a subagent / task.
    Subagent,
    /// An MCP tool call (`detail` holds `server:tool`).
    Mcp,
    Other,
}

impl ActionKind {
    /// Stable one-word label (column headers, filter footer).
    pub fn label(self) -> &'static str {
        match self {
            ActionKind::Prompt => "prompt",
            ActionKind::Command => "command",
            ActionKind::Edit => "edit",
            ActionKind::Read => "read",
            ActionKind::Search => "search",
            ActionKind::WebSearch => "web-search",
            ActionKind::WebFetch => "web-fetch",
            ActionKind::Subagent => "subagent",
            ActionKind::Mcp => "mcp",
            ActionKind::Other => "other",
        }
    }
}

/// One thing the agent did, in normalized form. Events order by their
/// position in the stream (append order); `ts_ms` is display metadata, not
/// the sort key, because several agents don't timestamp individual records.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityEvent {
    /// Epoch milliseconds when it happened, if the source records it.
    pub ts_ms: Option<u64>,
    pub kind: ActionKind,
    /// Primary text: the command line, file path, query, URL, subagent
    /// description, or tool name (for `Other`).
    pub detail: String,
    /// Secondary text: a tool's self-description, compact input rendering, or
    /// similar — whatever the source offers beyond `detail`.
    pub note: Option<String>,
    /// Head of the action's result (stdout, fetch summary, …), capped at
    /// [`RESULT_HEAD_MAX`] bytes at parse time.
    pub result_head: Option<String>,
    /// Whether the action succeeded — `None` when the source doesn't say
    /// (or the result hasn't arrived yet on a live tail).
    pub ok: Option<bool>,
    /// `None` for the session's main thread; `Some(label)` when the event
    /// happened inside a subagent (the label names it as well as the source
    /// allows).
    pub origin: Option<String>,
    /// Bookkeeping actions (todo churn, output polling) — shown dim and
    /// excluded from the per-kind tallies so counts stay signal.
    pub minor: bool,
    /// Wall-clock duration until the action's result landed, when both ends
    /// are timestamped.
    pub dur_ms: Option<u64>,
}

/// Cap for [`ActivityEvent::result_head`], applied at parse time so a huge
/// tool output never bloats the in-memory stream (char-boundary safe).
pub const RESULT_HEAD_MAX: usize = 400;

/// Truncate to at most `max` bytes on a char boundary, marking the cut.
pub(crate) fn head(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Session-level metadata a provider can extract alongside the event stream.
/// All optional — providers fill what their format offers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityMeta {
    /// Human title of the conversation (agent-generated summary or first
    /// prompt).
    pub title: Option<String>,
    /// Model identifier last seen in the stream.
    pub model: Option<String>,
    /// Cumulative output tokens, when per-message usage is recorded.
    pub output_tokens: Option<u64>,
}

/// Per-kind tallies over an event stream — drives the navigator counts and
/// the overview section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ActivityCounts {
    pub commands: usize,
    pub edits: usize,
    pub reads: usize,
    pub searches: usize,
    pub web: usize,
    pub subagents: usize,
    pub other: usize,
    /// User turns ([`ActionKind::Prompt`]) — not actions, tallied apart.
    pub prompts: usize,
    /// Actions whose result reported failure (`ok == Some(false)`).
    pub failed: usize,
}

impl ActivityCounts {
    /// Tally per-kind action counts. Prompts count as turns, not actions;
    /// minor (bookkeeping) events are skipped entirely so the counts read as
    /// work done.
    pub fn tally(events: &[ActivityEvent]) -> Self {
        let mut c = Self::default();
        for e in events {
            if e.kind == ActionKind::Prompt {
                c.prompts += 1;
                continue;
            }
            if e.minor {
                continue;
            }
            if e.ok == Some(false) {
                c.failed += 1;
            }
            match e.kind {
                ActionKind::Prompt => {} // tallied above
                ActionKind::Command => c.commands += 1,
                ActionKind::Edit => c.edits += 1,
                ActionKind::Read => c.reads += 1,
                ActionKind::Search => c.searches += 1,
                ActionKind::WebSearch | ActionKind::WebFetch => c.web += 1,
                ActionKind::Subagent => c.subagents += 1,
                ActionKind::Mcp | ActionKind::Other => c.other += 1,
            }
        }
        c
    }

    pub fn total(&self) -> usize {
        self.commands
            + self.edits
            + self.reads
            + self.searches
            + self.web
            + self.subagents
            + self.other
    }
}

/// One file the agent touched, aggregated from the event stream: the "Files"
/// section's row. Built by [`aggregate_files`].
#[derive(Debug, Clone, PartialEq)]
pub struct FileTouch {
    pub path: String,
    pub edits: usize,
    pub reads: usize,
    /// Timestamp of the most recent touch, if any event carried one.
    pub last_ts_ms: Option<u64>,
}

/// Aggregate edit/read events by path, most recently touched first —
/// recency is the timestamp when the source records one, else the event's
/// position in the stream. Pure, view-building helper.
pub fn aggregate_files(events: &[ActivityEvent]) -> Vec<FileTouch> {
    let mut map: std::collections::HashMap<String, (FileTouch, usize)> =
        std::collections::HashMap::new();
    for (seq, e) in events.iter().enumerate() {
        let is_edit = e.kind == ActionKind::Edit;
        if !is_edit && e.kind != ActionKind::Read {
            continue;
        }
        let (entry, last_seq) = map.entry(e.detail.clone()).or_insert_with(|| {
            (
                FileTouch {
                    path: e.detail.clone(),
                    edits: 0,
                    reads: 0,
                    last_ts_ms: None,
                },
                seq,
            )
        });
        if is_edit {
            entry.edits += 1;
        } else {
            entry.reads += 1;
        }
        entry.last_ts_ms = entry.last_ts_ms.max(e.ts_ms);
        *last_seq = seq;
    }
    let mut out: Vec<(FileTouch, usize)> = map.into_values().collect();
    out.sort_by_key(|(t, seq)| std::cmp::Reverse((t.last_ts_ms, *seq)));
    out.into_iter().map(|(t, _)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: ActionKind, detail: &str, ts: Option<u64>) -> ActivityEvent {
        ActivityEvent {
            ts_ms: ts,
            kind,
            detail: detail.into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor: false,
            dur_ms: None,
        }
    }

    #[test]
    fn counts_tally_by_kind() {
        let events = [
            ev(ActionKind::Command, "ls", None),
            ev(ActionKind::Command, "cargo test", None),
            ev(ActionKind::Edit, "/a.rs", None),
            ev(ActionKind::Read, "/a.rs", None),
            ev(ActionKind::WebSearch, "docs", None),
            ev(ActionKind::WebFetch, "https://x", None),
            ev(ActionKind::Subagent, "explore", None),
            ev(ActionKind::Mcp, "srv:tool", None),
        ];
        let c = ActivityCounts::tally(&events);
        assert_eq!(c.commands, 2);
        assert_eq!(c.edits, 1);
        assert_eq!(c.reads, 1);
        assert_eq!(c.web, 2);
        assert_eq!(c.subagents, 1);
        assert_eq!(c.other, 1);
        assert_eq!(c.total(), 8);
    }

    #[test]
    fn aggregate_files_groups_and_orders_by_recency() {
        let events = [
            ev(ActionKind::Read, "/old.rs", Some(100)),
            ev(ActionKind::Edit, "/hot.rs", Some(200)),
            ev(ActionKind::Edit, "/hot.rs", Some(900)),
            ev(ActionKind::Read, "/hot.rs", Some(300)),
            ev(ActionKind::Command, "ls", Some(999)), // not a file touch
        ];
        let files = aggregate_files(&events);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "/hot.rs");
        assert_eq!((files[0].edits, files[0].reads), (2, 1));
        assert_eq!(files[0].last_ts_ms, Some(900));
        assert_eq!(files[1].path, "/old.rs");
    }

    #[test]
    fn aggregate_files_without_timestamps_keeps_stream_recency() {
        let events = [
            ev(ActionKind::Edit, "/first.rs", None),
            ev(ActionKind::Edit, "/second.rs", None),
            ev(ActionKind::Read, "/first.rs", None),
        ];
        let files = aggregate_files(&events);
        // `/first.rs` was touched last (the read) → most recent.
        assert_eq!(files[0].path, "/first.rs");
        assert_eq!(files[1].path, "/second.rs");
    }

    #[test]
    fn head_truncates_on_char_boundary() {
        assert_eq!(head("short", 10), "short");
        let s = "aé".repeat(100);
        let h = head(&s, 7);
        assert!(h.ends_with('…'));
        assert!(h.len() <= 7 + '…'.len_utf8());
    }
}
