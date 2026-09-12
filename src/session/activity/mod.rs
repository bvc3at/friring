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

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Which provider reads a session's on-disk records — one variant per
/// supported transcript format.
///
/// Resolution is the registry entry's explicit
/// [`AgentDef::activity_provider`](crate::session::AgentDef::activity_provider)
/// when it declares one, else the **command basename**
/// ([`Self::for_command`]), so a custom registry name wrapping a known CLI
/// (`claude-opus` → `claude`) resolves without an allowlist of names.
///
/// Lives in the pure layer rather than beside the scan (`crate::activity`)
/// because an `agents.toml` entry names one, and `session` is the dependency
/// sink an [`AgentDef`](crate::session::AgentDef) can embed a type from.
///
/// The serde names are [`Self::id`] verbatim: what a config file writes is
/// what `friring-cli session activity` and the F9 identity line report back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "claude-code")]
    Claude,
    #[serde(rename = "vibe")]
    Vibe,
    #[serde(rename = "qwen-code")]
    Qwen,
    #[serde(rename = "cursor-agent")]
    Cursor,
    #[serde(rename = "gemini-cli")]
    Gemini,
    #[serde(rename = "crush")]
    Crush,
    #[serde(rename = "copilot")]
    Copilot,
    #[serde(rename = "aider")]
    Aider,
    #[serde(rename = "goose")]
    Goose,
    #[serde(rename = "opencode")]
    Opencode,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "cline")]
    Cline,
}

impl ProviderKind {
    /// Every variant, in the order the config documentation lists them.
    /// Exhaustiveness is held by [`Self::id`]'s `match`, which fails to
    /// compile when a variant is added without an id.
    pub const ALL: [Self; 12] = [
        Self::Claude,
        Self::Vibe,
        Self::Qwen,
        Self::Cursor,
        Self::Gemini,
        Self::Crush,
        Self::Copilot,
        Self::Aider,
        Self::Goose,
        Self::Opencode,
        Self::Codex,
        Self::Cline,
    ];

    /// The provider a bare command implies, by basename — the fallback when an
    /// entry declares no [`ProviderKind`] of its own. `None` for a command
    /// friring has no parser for, which is not the same as one it has decided
    /// it cannot read (`activity::unsupported_reason` names those).
    pub fn for_command(command: &str) -> Option<Self> {
        let base = Path::new(command)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(command);
        match base {
            "claude" => Some(Self::Claude),
            "vibe" => Some(Self::Vibe),
            "qwen" => Some(Self::Qwen),
            "cursor-agent" => Some(Self::Cursor),
            "gemini" => Some(Self::Gemini),
            "crush" => Some(Self::Crush),
            "copilot" => Some(Self::Copilot),
            "aider" => Some(Self::Aider),
            "goose" => Some(Self::Goose),
            "opencode" => Some(Self::Opencode),
            "codex" => Some(Self::Codex),
            "cline" => Some(Self::Cline),
            _ => None,
        }
    }

    /// Stable identifier: the config-file spelling, the `provider` field of
    /// `friring-cli session activity --json`, and the Overview identity line.
    pub fn id(self) -> &'static str {
        match self {
            ProviderKind::Claude => "claude-code",
            ProviderKind::Vibe => "vibe",
            ProviderKind::Qwen => "qwen-code",
            ProviderKind::Cursor => "cursor-agent",
            ProviderKind::Gemini => "gemini-cli",
            ProviderKind::Crush => "crush",
            ProviderKind::Copilot => "copilot",
            ProviderKind::Aider => "aider",
            ProviderKind::Goose => "goose",
            ProviderKind::Opencode => "opencode",
            ProviderKind::Codex => "codex",
            ProviderKind::Cline => "cline",
        }
    }
}

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

    /// Short fixed-width tag prefixing an event row (timeline, overview
    /// recent list).
    pub fn tag(self) -> &'static str {
        match self {
            ActionKind::Prompt => "▶",
            ActionKind::Command => "$",
            ActionKind::Edit => "edit",
            ActionKind::Read => "read",
            ActionKind::Search => "grep",
            ActionKind::WebSearch => "web",
            ActionKind::WebFetch => "fetch",
            ActionKind::Subagent => "agent",
            ActionKind::Mcp => "mcp",
            ActionKind::Other => "tool",
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

/// Trim trailing path separators so `/repo` and `/repo/` compare equal (the
/// daemon records `--add-dir /repo/` but the worker `cwd` as `/repo`).
///
/// Lives here because both cwd-matching users are outside each other's reach:
/// the providers' session binding (`crate::activity`) and the Claude
/// workflow/worker attribution (`app::cc_activity`).
pub fn normalize_dir(s: &str) -> String {
    let t = s.trim_end_matches('/');
    if t.is_empty() {
        s.to_string()
    } else {
        t.to_string()
    }
}

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
    /// Cumulative non-cached input tokens.
    pub input_tokens: Option<u64>,
    /// Cumulative cache-read input tokens.
    pub cache_read_tokens: Option<u64>,
    /// Cumulative cache-creation (write) input tokens.
    pub cache_write_tokens: Option<u64>,
}

impl ActivityMeta {
    /// Fold `other`'s token tallies into this meta (identity fields keep
    /// self's) — how a session's total absorbs its subagent/workflow
    /// transcripts. `None + Some(n) = Some(n)`, so a stream that never
    /// records a field doesn't zero the sum.
    pub fn add_tokens(&mut self, other: &ActivityMeta) {
        let add = |a: &mut Option<u64>, b: Option<u64>| {
            if let Some(n) = b {
                *a = Some(a.unwrap_or(0) + n);
            }
        };
        add(&mut self.output_tokens, other.output_tokens);
        add(&mut self.input_tokens, other.input_tokens);
        add(&mut self.cache_read_tokens, other.cache_read_tokens);
        add(&mut self.cache_write_tokens, other.cache_write_tokens);
    }
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
        assert_eq!(c.prompts, 0);
        assert_eq!(c.failed, 0);
    }

    #[test]
    fn counts_separate_prompts_skip_minor_and_track_failures() {
        let with = |kind, ok: Option<bool>, minor: bool| ActivityEvent {
            ts_ms: None,
            kind,
            detail: "x".into(),
            note: None,
            result_head: None,
            ok,
            origin: None,
            minor,
            dur_ms: None,
        };
        let events = [
            with(ActionKind::Prompt, None, false), // a turn, not an action
            with(ActionKind::Command, Some(true), false), // ok command
            with(ActionKind::Command, Some(false), false), // failed command → failed++
            with(ActionKind::Read, Some(false), true), // minor + failed → ignored
            with(ActionKind::Edit, None, false),   // no outcome recorded
        ];
        let c = ActivityCounts::tally(&events);
        assert_eq!(c.prompts, 1, "prompts tallied apart");
        assert_eq!(c.commands, 2);
        assert_eq!(c.edits, 1);
        assert_eq!(c.reads, 0, "minor bookkeeping is excluded from tiles");
        assert_eq!(c.failed, 1, "only the non-minor failure counts");
        assert_eq!(c.total(), 3, "prompts and minor rows are not actions");
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

    /// The config vocabulary and the reported vocabulary are one vocabulary:
    /// every variant's serde name is its [`ProviderKind::id`], so a value read
    /// out of `session activity --json` can be pasted straight into
    /// `agents.toml`. A derive that drifts from `id()` fails here.
    #[derive(Debug, Serialize, Deserialize)]
    struct ProviderWrapper {
        provider: ProviderKind,
    }

    #[test]
    fn every_provider_round_trips_through_its_id() {
        for kind in ProviderKind::ALL {
            let doc = format!("provider = \"{}\"\n", kind.id());
            let parsed: ProviderWrapper = toml::from_str(&doc)
                .unwrap_or_else(|e| panic!("{:?} does not deserialize from its own id: {e}", kind));
            assert_eq!(parsed.provider, kind);
            assert_eq!(
                toml::to_string(&ProviderWrapper { provider: kind }).unwrap(),
                doc,
                "{kind:?} serializes to something other than its id"
            );
        }
    }

    #[test]
    fn provider_ids_are_unique_and_all_lists_every_variant() {
        let mut ids: Vec<&str> = ProviderKind::ALL.iter().map(|k| k.id()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two providers share an id");
        // Every basename-inferred provider is reachable from ALL — a variant
        // omitted there would be undocumentable and unwritable in a config.
        for command in [
            "claude",
            "vibe",
            "qwen",
            "cursor-agent",
            "gemini",
            "crush",
            "copilot",
            "aider",
            "goose",
            "opencode",
            "codex",
            "cline",
        ] {
            let kind = ProviderKind::for_command(command)
                .unwrap_or_else(|| panic!("'{command}' resolves to no provider"));
            assert!(ProviderKind::ALL.contains(&kind), "{kind:?} missing in ALL");
        }
    }

    #[test]
    fn an_unknown_provider_name_names_the_valid_ones() {
        // The diagnostic `friring-cli config validate` shows: serde's
        // unknown-variant error enumerates what the user could have written.
        let err = toml::from_str::<ProviderWrapper>("provider = \"claude\"\n")
            .expect_err("'claude' is not a provider id");
        let msg = err.to_string();
        assert!(
            msg.contains("claude-code"),
            "error must list the ids: {msg}"
        );
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
