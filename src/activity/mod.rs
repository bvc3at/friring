//! Agent-neutral activity — provider dispatch, source discovery, and the
//! incremental on-disk scan.
//!
//! Reconstructs what a session's agent *did* (commands / edits / reads / web /
//! subagents / tokens) from whatever its CLI persists on disk. Each supported
//! CLI gets a **provider**: pure record→event parsers live in
//! [`crate::session::activity`]; this module is the filesystem glue — per-session
//! source discovery, a stat-signature gate, incremental tailing of append-only
//! sources, and the scan pass itself.
//!
//! Two callers share it, which is why it is a top-level module rather than part
//! of `app`: the TUI's F9 activity view (`app::activity` owns the scheduling and
//! the rendering) and `friring-cli session activity`, which cannot reach into
//! `app` (see `tests/architecture_rules.rs`) and must work with no TUI running.
//! Discovery is the expensive, agent-specific knowledge here — duplicating it
//! for the CLI is exactly what this split avoids.

pub(crate) mod aider;
pub(crate) mod cline;
pub(crate) mod codex;
pub(crate) mod copilot;
pub(crate) mod crush;
pub(crate) mod cursor;
pub(crate) mod gemini;
pub(crate) mod goose;
pub(crate) mod opencode;
pub(crate) mod qwen;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::session::activity::vibe::{parse_meta as parse_vibe_meta, VibeMeta, VibeScan};
use crate::session::activity::{claude::ClaudeScan, ActionKind, ActivityEvent, ActivityMeta};
use crate::session::{SessionId, SessionInfo};

/// Per-pass ingest budget for an append-only source. History is never
/// clipped: a months-old transcript is ingested front-to-back across
/// successive ~1 s scan passes, this many bytes per pass, so one huge file
/// delays completeness (surfaced as a loader via
/// [`SessionActivity::backfilling`]) instead of stalling a whole pass or
/// dropping its oldest activity.
const INGEST_CHUNK: u64 = 8 * 1024 * 1024;

/// Cap for **snapshot** sources that re-read from byte 0 on every change
/// (cursor's regenerated transcripts): a tail-window read keeps the per-change
/// cost bounded; the clip is permanent and surfaced via
/// [`SessionActivity::truncated`].
const SNAPSHOT_INGEST_MAX: u64 = 32 * 1024 * 1024;

/// Which activity provider reads a session's on-disk records, resolved from
/// the **command basename** of the session's registry entry — so custom
/// registry names wrapping the same CLI (`claude-opus` → `claude`) resolve
/// without an allowlist of names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderKind {
    Claude,
    Vibe,
    Qwen,
    Cursor,
    Gemini,
    Crush,
    Copilot,
    Aider,
    Goose,
    Opencode,
    Codex,
    Cline,
}

impl ProviderKind {
    pub(crate) fn for_command(command: &str) -> Option<Self> {
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

    pub(crate) fn id(self) -> &'static str {
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

/// Why a known agent CLI has no provider — shown in the Overview instead of
/// the generic unsupported line, so the gap reads as a decision, not a bug.
/// (Findings from the July 2026 on-disk-format research; see FORK.md.)
pub(crate) fn unsupported_reason(command: &str) -> Option<&'static str> {
    let base = Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    match base {
        "agy" => Some(
            "agy encrypts its conversation store (AES-GCM trajectories + opaque SQLite \
             blobs); hook-based capture is the planned follow-up.",
        ),
        "amp" => Some(
            "amp keeps threads server-side; its local logs lack a stable session-to-cwd \
             mapping to read from.",
        ),
        _ => None,
    }
}

/// One session's accumulated activity: the provider, its scan state (bound
/// sources + streaming parsers + byte offsets), and the stat signature that
/// gates re-reads.
pub(crate) struct SessionActivity {
    pub(crate) provider: ProviderKind,
    scan: ProviderScan,
    sig: u64,
}

impl SessionActivity {
    pub(crate) fn new(provider: ProviderKind) -> Self {
        let scan = match provider {
            ProviderKind::Claude => ProviderScan::Claude(ClaudeSource::default()),
            ProviderKind::Vibe => ProviderScan::Vibe(VibeSource::default()),
            ProviderKind::Qwen => ProviderScan::Qwen(qwen::QwenSource::default()),
            ProviderKind::Cursor => ProviderScan::Cursor(cursor::CursorSource::default()),
            ProviderKind::Gemini => ProviderScan::Gemini(gemini::GeminiSource::default()),
            ProviderKind::Crush => ProviderScan::Crush(crush::CrushSource::default()),
            ProviderKind::Copilot => ProviderScan::Copilot(copilot::CopilotSource::default()),
            ProviderKind::Aider => ProviderScan::Aider(aider::AiderSource::default()),
            ProviderKind::Goose => ProviderScan::Goose(goose::GooseSource::default()),
            ProviderKind::Opencode => ProviderScan::Opencode(opencode::OpencodeSource::default()),
            ProviderKind::Codex => ProviderScan::Codex(codex::CodexSource::default()),
            ProviderKind::Cline => ProviderScan::Cline(cline::ClineSource::default()),
        };
        Self {
            provider,
            scan,
            sig: 0,
        }
    }

    pub(crate) fn events(&self) -> &[ActivityEvent] {
        match &self.scan {
            ProviderScan::Claude(s) => &s.merged,
            ProviderScan::Vibe(s) => &s.scan.events,
            ProviderScan::Qwen(s) => &s.scan.events,
            ProviderScan::Cursor(s) => &s.scan.events,
            ProviderScan::Gemini(s) => &s.scan.events,
            ProviderScan::Crush(s) => &s.scan.events,
            ProviderScan::Copilot(s) => &s.scan.events,
            ProviderScan::Aider(s) => &s.scan.events,
            ProviderScan::Goose(s) => &s.scan.events,
            ProviderScan::Opencode(s) => &s.events,
            ProviderScan::Codex(s) => &s.scan.events,
            ProviderScan::Cline(s) => &s.events,
        }
    }

    pub(crate) fn meta(&self) -> ActivityMeta {
        match &self.scan {
            // The session's cost is the whole tree's: fold every subagent /
            // workflow transcript's tallies into the main meta.
            ProviderScan::Claude(s) => {
                let mut meta = s.scan.meta.clone();
                for tail in s.subs.values() {
                    meta.add_tokens(&tail.scan.meta);
                }
                meta
            }
            // The transcript has no meta records — everything lives in the
            // sidecar meta.json.
            ProviderScan::Vibe(s) => s.meta.meta.clone(),
            ProviderScan::Qwen(s) => s.scan.meta.clone(),
            ProviderScan::Cursor(s) => s.scan.meta.clone(),
            ProviderScan::Gemini(s) => s.scan.meta.clone(),
            ProviderScan::Crush(s) => s.scan.meta.clone(),
            ProviderScan::Copilot(s) => s.meta(),
            ProviderScan::Aider(s) => s.scan.meta.clone(),
            // Meta comes from the sessions table, not the message stream.
            ProviderScan::Goose(s) => s.meta.clone(),
            ProviderScan::Opencode(s) => s.meta.clone(),
            ProviderScan::Codex(s) => s.scan.meta.clone(),
            ProviderScan::Cline(s) => s.meta.meta.clone(),
        }
    }

    /// Whether older history is still being ingested (large source, drained
    /// chunk by chunk across passes) — drives the view's loader. Resolves to
    /// `false` once the backlog is drained.
    pub(crate) fn backfilling(&self) -> bool {
        match &self.scan {
            ProviderScan::Claude(s) => s.backfilling || s.subs.values().any(|t| t.backfilling),
            ProviderScan::Vibe(s) => s.backfilling,
            ProviderScan::Qwen(s) => s.backfilling,
            ProviderScan::Gemini(s) => s.backfilling,
            ProviderScan::Copilot(s) => s.backfilling,
            ProviderScan::Aider(s) => s.backfilling,
            ProviderScan::Codex(s) => s.backfilling,
            _ => false,
        }
    }

    /// Whether a snapshot/DB cap permanently clipped the oldest history
    /// (cursor's regenerated transcripts, the SQLite providers' row caps).
    pub(crate) fn truncated(&self) -> bool {
        match &self.scan {
            ProviderScan::Cursor(s) => s.truncated,
            ProviderScan::Crush(s) => s.truncated,
            ProviderScan::Goose(s) => s.truncated,
            ProviderScan::Opencode(s) => s.truncated,
            ProviderScan::Cline(s) => s.truncated,
            _ => false,
        }
    }

    /// [`Self::seeded`] plus a session meta (title / model / token tallies) —
    /// for view tests that render the header, not just the event rows.
    ///
    /// Only the providers whose meta rides in a sidecar rather than the event
    /// stream are covered; the rest derive it while parsing.
    #[cfg(test)]
    pub(crate) fn seeded_with_meta(
        provider: ProviderKind,
        events: Vec<ActivityEvent>,
        meta: ActivityMeta,
    ) -> Self {
        let mut act = Self::seeded(provider, events);
        match &mut act.scan {
            ProviderScan::Vibe(s) => s.meta.meta = meta,
            ProviderScan::Goose(s) => s.meta = meta,
            ProviderScan::Opencode(s) => s.meta = meta,
            ProviderScan::Cline(s) => s.meta.meta = meta,
            _ => panic!("{provider:?} derives its meta from the event stream"),
        }
        act
    }

    /// Test seam: an accumulator pre-filled with `events`, as if a scan pass
    /// had ingested them (acceptance tests can't run the fs scan).
    #[cfg(test)]
    pub(crate) fn seeded(provider: ProviderKind, events: Vec<ActivityEvent>) -> Self {
        let mut act = Self::new(provider);
        match &mut act.scan {
            ProviderScan::Claude(s) => {
                s.scan.events = events.clone();
                s.merged = events;
            }
            ProviderScan::Vibe(s) => s.scan.events = events,
            ProviderScan::Qwen(s) => s.scan.events = events,
            ProviderScan::Cursor(s) => s.scan.events = events,
            ProviderScan::Gemini(s) => s.scan.events = events,
            ProviderScan::Crush(s) => s.scan.events = events,
            ProviderScan::Copilot(s) => s.scan.events = events,
            ProviderScan::Aider(s) => s.scan.events = events,
            ProviderScan::Goose(s) => s.scan.events = events,
            ProviderScan::Opencode(s) => s.events = events,
            ProviderScan::Codex(s) => s.scan.events = events,
            ProviderScan::Cline(s) => s.events = events,
        }
        act
    }
}

/// Per-provider scan state. Append-only JSONL sources tail incrementally by
/// byte offset; a shrink (rewrite/rotation) resets the streaming parser and
/// re-ingests from scratch.
enum ProviderScan {
    Claude(ClaudeSource),
    Vibe(VibeSource),
    Qwen(qwen::QwenSource),
    Cursor(cursor::CursorSource),
    Gemini(gemini::GeminiSource),
    Crush(crush::CrushSource),
    Copilot(copilot::CopilotSource),
    Aider(aider::AiderSource),
    Goose(goose::GooseSource),
    Opencode(opencode::OpencodeSource),
    Codex(codex::CodexSource),
    Cline(cline::ClineSource),
}

/// Claude Code: the session's main conversation transcript
/// `projects/<slug>/<agent_session_id>.jsonl` (found by slug-dir scan), plus
/// one tail per subagent / workflow-agent transcript the cc tree scan has
/// indexed — their tool activity merges into one timestamp-ordered stream so
/// the Timeline shows delegated work, not just the lead's.
#[derive(Default)]
struct ClaudeSource {
    scan: ClaudeScan,
    transcript: Option<PathBuf>,
    offset: u64,
    backfilling: bool,
    /// Subagent/workflow transcript tails, keyed by path (ordered, so the
    /// merge is deterministic).
    subs: std::collections::BTreeMap<PathBuf, SubTail>,
    /// Merged main + subagent stream — what [`SessionActivity::events`]
    /// serves. Rebuilt only on ingest, never per render.
    merged: Vec<ActivityEvent>,
}

/// One subagent transcript tail: its own streaming parser + offset, and the
/// origin label stamped onto every merged event.
struct SubTail {
    scan: ClaudeScan,
    sig: u64,
    offset: u64,
    backfilling: bool,
    origin: String,
}

/// Mistral Vibe: the newest `logs/session/<prefix>_<ts>_<id>/` dir whose
/// `meta.json` working directory matches the session — dir names embed the
/// UTC start time, so lexical order is recency and rebinding only re-parses
/// metas when a new dir appears.
#[derive(Default)]
struct VibeSource {
    scan: VibeScan,
    dir: Option<PathBuf>,
    meta: VibeMeta,
    offset: u64,
    backfilling: bool,
    /// Newest session-dir name at the last discovery — the rebind trigger.
    newest_seen: Option<std::ffi::OsString>,
}

/// Result of one background scan pass: every input session's accumulator
/// (moved back), with a changed flag driving view refreshes.
pub(crate) struct ActivityRefresh {
    pub(crate) updates: Vec<(SessionId, SessionActivity, bool)>,
}

/// One session's scan input: identity for discovery plus the accumulator
/// (taken from [`App::activity`] for the duration of the pass).
pub(crate) struct ActivityInput {
    pub(crate) id: SessionId,
    pub(crate) own_id: Option<String>,
    /// Normalized launch dirs (worktrees / additional dirs / process cwd) —
    /// the cwd-match key for providers that don't record a session id.
    pub(crate) dirs: Vec<String>,
    /// Claude only: the subagent/workflow transcripts the cc tree scan has
    /// indexed, `(path, origin label)` — the event scan tails these too.
    pub(crate) sub_sources: Vec<(PathBuf, String)>,
    pub(crate) state: SessionActivity,
}

/// Provider state roots, resolved on the UI thread (env access) and read on
/// the scan thread.
pub(crate) struct ScanRoots {
    pub(crate) claude_projects: Option<PathBuf>,
    pub(crate) vibe_sessions: Option<PathBuf>,
    pub(crate) qwen_projects: Option<PathBuf>,
    pub(crate) cursor_root: Option<PathBuf>,
    pub(crate) gemini_home: Option<PathBuf>,
    pub(crate) copilot_sessions: Option<PathBuf>,
    pub(crate) aider_history: Option<PathBuf>,
    pub(crate) goose_sessions: Option<PathBuf>,
    pub(crate) opencode_db: Option<PathBuf>,
    pub(crate) codex_sessions: Option<PathBuf>,
    pub(crate) cline_sessions: Option<PathBuf>,
}

impl ScanRoots {
    /// Resolve every provider's state root from the environment.
    ///
    /// Env access, so the TUI calls it on the UI thread and hands the result to
    /// its scan thread; the CLI calls it inline. Shared so the two can never
    /// look in different places for the same agent's records.
    pub(crate) fn discover() -> Self {
        Self {
            claude_projects: crate::paths::claude_projects_dir(None),
            vibe_sessions: crate::paths::vibe_sessions_dir(None),
            qwen_projects: qwen::qwen_projects_dir(None),
            cursor_root: cursor::cursor_root(None),
            gemini_home: gemini::gemini_root(None),
            copilot_sessions: copilot::copilot_sessions_dir(None),
            aider_history: aider::aider_history_override(None),
            goose_sessions: goose::goose_sessions_dir(None),
            opencode_db: opencode::opencode_db_path(None),
            codex_sessions: codex::codex_sessions_dir(None),
            cline_sessions: cline::cline_sessions_dir(None),
        }
    }
}

/// One session's activity, scanned from scratch in a single call — the
/// accumulator-free entry point for `friring-cli session activity`.
///
/// The TUI tails incrementally across passes because it rescans every second;
/// a one-shot caller has no next pass, so this drains the backlog here. Each
/// pass ingests at most [`INGEST_CHUNK`], so a months-old transcript needs
/// several: loop until the source stops reporting `backfilling`, bounded by
/// `max_passes` so a pathological source can't spin forever (the returned flag
/// says whether the history is complete).
pub(crate) fn scan_once(
    provider: ProviderKind,
    own_id: Option<String>,
    dirs: Vec<String>,
    max_passes: usize,
) -> (SessionActivity, bool) {
    let mut state = SessionActivity::new(provider);
    for _ in 0..max_passes.max(1) {
        let refresh = collect_activity(
            ScanRoots::discover(),
            vec![ActivityInput {
                id: SessionId::default(),
                own_id: own_id.clone(),
                dirs: dirs.clone(),
                // Claude subagent transcripts hang off the cc tree scan, which
                // is TUI state; a one-shot read covers the main thread only.
                sub_sources: Vec::new(),
                state,
            }],
        );
        // `collect_activity` returns exactly one update per input.
        state = refresh
            .updates
            .into_iter()
            .next()
            .map(|(_, state, _)| state)
            .expect("collect_activity returns one update per input");
        if !state.backfilling() {
            return (state, true);
        }
    }
    (state, false)
}

/// The whole scan pass, run on a blocking thread.
pub(crate) fn collect_activity(roots: ScanRoots, inputs: Vec<ActivityInput>) -> ActivityRefresh {
    let mut updates = Vec::with_capacity(inputs.len());
    for input in inputs {
        let mut state = input.state;
        let changed = match &mut state.scan {
            ProviderScan::Claude(src) => scan_claude(
                src,
                &mut state.sig,
                roots.claude_projects.as_deref(),
                input.own_id.as_deref(),
                &input.sub_sources,
            ),
            ProviderScan::Vibe(src) => scan_vibe(
                src,
                &mut state.sig,
                roots.vibe_sessions.as_deref(),
                &input.dirs,
            ),
            ProviderScan::Qwen(src) => qwen::scan_qwen(
                src,
                &mut state.sig,
                roots.qwen_projects.as_deref(),
                &input.dirs,
                input.own_id.as_deref(),
            ),
            ProviderScan::Cursor(src) => cursor::scan_cursor(
                src,
                &mut state.sig,
                roots.cursor_root.as_deref(),
                input.own_id.as_deref(),
                &input.dirs,
            ),
            ProviderScan::Gemini(src) => gemini::scan_gemini(
                src,
                &mut state.sig,
                roots.gemini_home.as_deref(),
                &input.dirs,
                input.own_id.as_deref(),
            ),
            ProviderScan::Crush(src) => crush::scan_crush(
                src,
                &mut state.sig,
                &input.dirs,
                input.own_id.as_deref(),
                None,
            ),
            ProviderScan::Copilot(src) => copilot::scan_copilot(
                src,
                &mut state.sig,
                roots.copilot_sessions.as_deref(),
                input.own_id.as_deref(),
                &input.dirs,
            ),
            ProviderScan::Aider(src) => aider::scan_aider(
                src,
                &mut state.sig,
                roots.aider_history.as_deref(),
                &input.dirs,
            ),
            ProviderScan::Goose(src) => goose::scan_goose(
                src,
                &mut state.sig,
                roots.goose_sessions.as_deref(),
                &input.dirs,
                input.own_id.as_deref(),
            ),
            ProviderScan::Opencode(src) => opencode::scan_opencode(
                src,
                &mut state.sig,
                roots.opencode_db.as_deref(),
                input.own_id.as_deref(),
                &input.dirs,
            ),
            ProviderScan::Codex(src) => codex::scan_codex(
                src,
                &mut state.sig,
                roots.codex_sessions.as_deref(),
                &input.dirs,
                input.own_id.as_deref(),
            ),
            ProviderScan::Cline(src) => cline::scan_cline(
                src,
                &mut state.sig,
                roots.cline_sessions.as_deref(),
                &input.dirs,
                input.own_id.as_deref(),
            ),
        };
        updates.push((input.id, state, changed));
    }
    ActivityRefresh { updates }
}

/// Tail the session's main Claude transcript plus every indexed subagent /
/// workflow transcript, rebuilding the merged stream when anything new was
/// ingested. Returns whether the merged stream changed.
fn scan_claude(
    src: &mut ClaudeSource,
    sig: &mut u64,
    projects: Option<&Path>,
    own_id: Option<&str>,
    subs: &[(PathBuf, String)],
) -> bool {
    if src.transcript.is_none() {
        if let (Some(projects), Some(id)) = (projects, own_id) {
            src.transcript = find_claude_transcript(projects, id);
        }
    }
    let mut changed = false;
    if let Some(path) = src.transcript.clone() {
        changed = tail_source(&path, sig, &mut src.offset, &mut src.backfilling, |chunk| {
            src.scan.ingest(chunk)
        })
        .unwrap_or_else(|| {
            // Shrunk (rotated/rewritten/cleared): reset the streaming parser and
            // re-ingest. The reset itself is a change — the prior events are now
            // gone — so force a merge rebuild even when the rewrite is empty and
            // the re-ingest reads nothing, else `merged` would keep stale rows.
            src.scan = ClaudeScan::default();
            src.offset = 0;
            src.backfilling = false;
            let _ = tail_source(&path, sig, &mut src.offset, &mut src.backfilling, |chunk| {
                src.scan.ingest(chunk)
            });
            true
        });
    }
    changed |= sync_claude_subs(src, subs);
    if changed {
        rebuild_merged(src);
    }
    changed
}

/// Reconcile the subagent tails with the cc tree scan's transcript index
/// (added agents start tailing, removed ones drop), then tail each. Returns
/// whether any tail ingested or the set itself changed.
fn sync_claude_subs(src: &mut ClaudeSource, subs: &[(PathBuf, String)]) -> bool {
    let mut changed = false;
    let listed: std::collections::HashSet<&Path> = subs.iter().map(|(p, _)| p.as_path()).collect();
    let before = src.subs.len();
    src.subs.retain(|p, _| listed.contains(p.as_path()));
    changed |= src.subs.len() != before;
    for (path, origin) in subs {
        let tail = src.subs.entry(path.clone()).or_insert_with(|| {
            changed = true;
            SubTail {
                scan: ClaudeScan::default(),
                sig: 0,
                offset: 0,
                backfilling: false,
                origin: origin.clone(),
            }
        });
        // A fan label can arrive after the transcript appears — track it.
        if &tail.origin != origin {
            tail.origin = origin.clone();
            changed = true;
        }
        let ingested = tail_source(
            path,
            &mut tail.sig,
            &mut tail.offset,
            &mut tail.backfilling,
            |chunk| tail.scan.ingest(chunk),
        )
        .unwrap_or_else(|| {
            // Shrink resets this tail — a change even if the re-ingest is empty
            // (see the main-transcript path), so the merge drops its stale rows.
            tail.scan = ClaudeScan::default();
            tail.offset = 0;
            tail.backfilling = false;
            let _ = tail_source(
                path,
                &mut tail.sig,
                &mut tail.offset,
                &mut tail.backfilling,
                |chunk| tail.scan.ingest(chunk),
            );
            true
        });
        changed |= ingested;
    }
    changed
}

/// Rebuild [`ClaudeSource::merged`]: the main stream plus every subagent
/// stream, ordered by timestamp (stable — ties keep main-before-sub, and each
/// stream's own order). Unstamped events inherit their stream's last seen
/// timestamp so they sort with their neighbours. Subagent task prompts are
/// dropped (the lead's `Task` event already marks the delegation, and a
/// subagent's prompt is not a conversation turn); every subagent event gets
/// its origin label.
fn rebuild_merged(src: &mut ClaudeSource) {
    let mut all: Vec<(u64, ActivityEvent)> = Vec::new();
    let mut push_stream = |events: &[ActivityEvent], origin: Option<&str>| {
        // Seed with the stream's first real timestamp so *leading* unstamped
        // events inherit it (sort just ahead of their neighbours) rather than
        // falling to 0 and jumping to the front of the whole merged stream. A
        // stream with no timestamps at all keeps its append order via the
        // stable sort.
        let mut last = events.iter().find_map(|e| e.ts_ms).unwrap_or(0);
        for e in events {
            if origin.is_some() && e.kind == ActionKind::Prompt {
                continue;
            }
            let key = e.ts_ms.unwrap_or(last);
            last = key;
            let mut ev = e.clone();
            if let Some(o) = origin {
                ev.origin = Some(o.to_string());
            }
            all.push((key, ev));
        }
    };
    push_stream(&src.scan.events, None);
    for tail in src.subs.values() {
        push_stream(&tail.scan.events, Some(&tail.origin));
    }
    all.sort_by_key(|(key, _)| *key); // stable: ties keep push order
    src.merged = all.into_iter().map(|(_, e)| e).collect();
}

/// The subagent/workflow transcripts the cc tree scan indexed for a session,
/// with the origin label each merged event will carry. Resolved on the UI
/// thread (a clone of small metadata), read on the scan thread.
pub(crate) fn claude_sub_sources(info: &SessionInfo) -> Vec<(PathBuf, String)> {
    let Some(a) = &info.cc_activity else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for s in &a.subagents {
        let label = s.label.clone().unwrap_or_else(|| s.agent_type.clone());
        out.push((s.transcript_path.clone(), label));
    }
    for w in &a.workflows {
        for ag in &w.agents {
            let label = ag.label.clone().unwrap_or_else(|| ag.agent_type.clone());
            out.push((ag.transcript_path.clone(), label));
        }
    }
    out
}

/// `projects/*/<id>.jsonl` by slug-dir scan (the slug rule is undocumented, so
/// friring never computes it for lookups — same policy as `cc_import`).
fn find_claude_transcript(projects: &Path, id: &str) -> Option<PathBuf> {
    let target = format!("{id}.jsonl");
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        let candidate = entry.path().join(&target);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Bind (or rebind) and tail a Vibe session dir.
fn scan_vibe(src: &mut VibeSource, sig: &mut u64, root: Option<&Path>, dirs: &[String]) -> bool {
    let Some(root) = root else {
        return false;
    };
    let newest = newest_session_dir(root);
    if src.dir.is_none() || newest != src.newest_seen {
        let bound = discover_vibe_dir(root, dirs);
        let rebound = bound.is_some() && bound != src.dir;
        src.newest_seen = newest;
        if rebound {
            // A newer matching session (agent restarted): start fresh on it.
            *src = VibeSource {
                dir: bound,
                newest_seen: src.newest_seen.clone(),
                ..VibeSource::default()
            };
            *sig = 0;
        } else if src.dir.is_none() {
            src.dir = bound;
        }
    }
    let Some(dir) = src.dir.clone() else {
        return false;
    };
    let messages = dir.join("messages.jsonl");
    let meta_path = dir.join("meta.json");
    let new_sig = stat_signature(&[&messages, &meta_path]);
    // While a large transcript is still draining, keep scanning even on an
    // unchanged signature — else the outer gate would strand the backlog and
    // the loader would never resolve (the inner per-call sig is fresh-zero, so
    // only this gate protects the pass).
    if new_sig == *sig && !src.backfilling {
        return false;
    }
    // meta.json is small and atomically replaced — re-parse on any change.
    if let Ok(s) = std::fs::read_to_string(&meta_path) {
        src.meta = parse_vibe_meta(&s);
    }
    // The dir-level signature above is the real gate; the per-call one here
    // never gates (fresh zero), it only drives the offset bookkeeping.
    let mut msg_sig = 0u64;
    if tail_source(
        &messages,
        &mut msg_sig,
        &mut src.offset,
        &mut src.backfilling,
        |chunk| src.scan.ingest(chunk),
    )
    .is_none()
    {
        // Vibe rewrites messages.jsonl in full on rewind/compact.
        src.scan = VibeScan::default();
        src.offset = 0;
        src.backfilling = false;
        let _ = tail_source(
            &messages,
            &mut msg_sig,
            &mut src.offset,
            &mut src.backfilling,
            |chunk| src.scan.ingest(chunk),
        );
    }
    *sig = new_sig;
    // Meta-only changes (title/tokens updates) count as a change too.
    true
}

/// Newest (lexically greatest) visible session dir name under a Vibe root —
/// names embed the UTC start timestamp, so this is the rebind trigger.
fn newest_session_dir(root: &Path) -> Option<std::ffi::OsString> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name())
        .filter(|n| !n.to_string_lossy().starts_with('.'))
        .max()
}

/// Newest session dir whose `meta.json` working directory matches one of the
/// session's launch dirs. Walks newest-first so only a few metas are parsed.
fn discover_vibe_dir(root: &Path, dirs: &[String]) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    entries.sort();
    for dir in entries.into_iter().rev() {
        let Ok(s) = std::fs::read_to_string(dir.join("meta.json")) else {
            continue;
        };
        let Some(wd) = parse_vibe_meta(&s).working_directory else {
            continue;
        };
        let wd = crate::session::activity::normalize_dir(&wd);
        if dirs.contains(&wd) {
            return Some(dir);
        }
    }
    None
}

/// Stat-hash of a set of files (path + mtime + len). Missing files hash as
/// absent, so appearance/disappearance also moves the signature.
fn stat_signature(paths: &[&Path]) -> u64 {
    let mut h = DefaultHasher::new();
    for p in paths {
        p.hash(&mut h);
        if let Ok(md) = std::fs::metadata(p) {
            md.len().hash(&mut h);
            md.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0)
                .hash(&mut h);
        }
    }
    h.finish()
}

/// Tail one append-only source: stat-gate via `sig`, feed complete new lines
/// to `ingest` (at most ~[`INGEST_CHUNK`] bytes per call), and advance
/// `offset`. `backfilling` reports a remaining backlog — while set, the next
/// pass proceeds even on an unchanged signature, so a large history drains
/// chunk by chunk without blocking anything. Returns `None` when the file
/// shrank (caller resets its parser and retries), else
/// `Some(ingested-anything)`.
fn tail_source(
    path: &Path,
    sig: &mut u64,
    offset: &mut u64,
    backfilling: &mut bool,
    ingest: impl FnOnce(&str),
) -> Option<bool> {
    let new_sig = stat_signature(&[path]);
    if new_sig == *sig && !*backfilling {
        return Some(false);
    }
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        return None; // shrank — caller resets and re-ingests
    }
    *sig = new_sig;
    if len == *offset {
        *backfilling = false;
        return Some(false); // mtime moved but nothing new (touch)
    }
    let Some((chunk, new_offset, more)) = read_new_lines(path, *offset, INGEST_CHUNK) else {
        // Only a torn tail line so far — wait for the append to complete.
        *backfilling = false;
        return Some(false);
    };
    *backfilling = more;
    ingest(&chunk);
    *offset = new_offset;
    Some(true)
}

/// Read the complete lines appended past `offset`, at most ~`cap` bytes per
/// call (line-aligned). A single line longer than `cap` is read to its end
/// rather than stalling the tail forever. The returned offset points just
/// past the last complete line (a torn tail line — a live append racing the
/// read — is left for the next pass); the final flag reports whether more
/// complete data remains past the returned offset.
fn read_new_lines(path: &Path, offset: u64, cap: u64) -> Option<(String, u64, bool)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len <= offset {
        return None;
    }
    f.seek(SeekFrom::Start(offset)).ok()?;
    let want = (len - offset).min(cap);
    let mut buf = Vec::with_capacity(want as usize);
    (&mut f).take(want).read_to_end(&mut buf).ok()?;
    if !buf.contains(&b'\n') && offset + buf.len() as u64 == len {
        return None; // torn tail only
    }
    if buf.iter().rposition(|&b| b == b'\n').is_none() {
        // One line larger than the chunk — finish it this pass.
        let mut rest = Vec::new();
        std::io::BufReader::new(&mut f)
            .read_until(b'\n', &mut rest)
            .ok()?;
        buf.extend_from_slice(&rest);
    }
    let last_nl = buf.iter().rposition(|&b| b == b'\n')?;
    buf.truncate(last_nl + 1);
    let new_offset = offset + buf.len() as u64;
    let chunk = String::from_utf8_lossy(&buf).into_owned();
    Some((chunk, new_offset, new_offset < len))
}

/// One tail-window read for **snapshot** sources that regenerate in full each
/// change (see [`SNAPSHOT_INGEST_MAX`]): read the last `cap` bytes, dropping
/// the torn first line, reporting whether older content was clipped.
fn read_tail_window(path: &Path, cap: u64) -> Option<(String, bool)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let (start, clipped) = if len > cap {
        (len - cap, true)
    } else {
        (0, false)
    };
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    let mut chunk = String::from_utf8_lossy(&buf).into_owned();
    if clipped {
        let first_nl = chunk.find('\n')?;
        chunk.drain(..=first_nl);
    }
    Some((chunk, clipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_resolves_by_command_basename() {
        // Every supported command's bare basename must route to its variant —
        // a typo in any arm would silently misroute or disable that provider.
        let table = [
            ("claude", ProviderKind::Claude),
            ("vibe", ProviderKind::Vibe),
            ("qwen", ProviderKind::Qwen),
            ("cursor-agent", ProviderKind::Cursor),
            ("gemini", ProviderKind::Gemini),
            ("crush", ProviderKind::Crush),
            ("copilot", ProviderKind::Copilot),
            ("aider", ProviderKind::Aider),
            ("goose", ProviderKind::Goose),
            ("opencode", ProviderKind::Opencode),
            ("codex", ProviderKind::Codex),
            ("cline", ProviderKind::Cline),
        ];
        for (command, kind) in table {
            assert_eq!(
                ProviderKind::for_command(command),
                Some(kind),
                "command {command:?} should resolve to {kind:?}"
            );
        }
        // A path-qualified command resolves by basename.
        assert_eq!(
            ProviderKind::for_command("/usr/local/bin/claude"),
            Some(ProviderKind::Claude)
        );
        // agy (encrypted store) and amp (server-side threads) are deliberate
        // gaps with named reasons, not providers.
        assert_eq!(ProviderKind::for_command("agy"), None);
        assert!(unsupported_reason("agy").is_some());
        assert!(unsupported_reason("amp").is_some());
        assert_eq!(unsupported_reason("my-agent-cli"), None);
    }

    #[test]
    fn read_new_lines_tails_complete_lines_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("t.jsonl");
        std::fs::write(&path, "line one\nline two\npartial").expect("write");
        let (chunk, offset, more) = read_new_lines(&path, 0, INGEST_CHUNK).expect("read");
        assert_eq!(chunk, "line one\nline two\n");
        // Only the torn tail remains — nothing complete to backfill.
        assert!(more, "the torn tail still counts as unread bytes");
        assert_eq!(offset, chunk.len() as u64);

        // The torn tail completes later and is picked up from the offset.
        std::fs::write(&path, "line one\nline two\npartial done\n").expect("write");
        let (chunk2, offset2, more2) = read_new_lines(&path, offset, INGEST_CHUNK).expect("read");
        assert_eq!(chunk2, "partial done\n");
        assert_eq!(offset2, 31);
        assert!(!more2);
        // Nothing new → None.
        assert!(read_new_lines(&path, offset2, INGEST_CHUNK).is_none());
    }

    #[test]
    fn read_new_lines_chunks_forward_without_dropping_history() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("big.jsonl");
        let line = "x".repeat(99) + "\n"; // 100 bytes per line
        std::fs::write(&path, line.repeat(50)).expect("write 5000 bytes");
        // First pass: at most ~cap bytes from the FRONT, line-aligned.
        let (chunk, offset, more) = read_new_lines(&path, 0, 250).expect("read");
        assert_eq!(chunk.len(), 200, "250-byte budget covers two whole lines");
        assert!(chunk.starts_with('x') && chunk.ends_with('\n'));
        assert_eq!(offset, 200);
        assert!(more, "4800 bytes of backlog remain");
        // Draining passes walk the whole file — nothing is ever clipped.
        let mut offset = offset;
        let mut total = chunk.len();
        while let Some((c, o, m)) = read_new_lines(&path, offset, 250) {
            total += c.len();
            offset = o;
            if !m {
                break;
            }
        }
        assert_eq!(total, 5000);
        assert_eq!(offset, 5000);
    }

    #[test]
    fn read_new_lines_finishes_an_oversized_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("giant.jsonl");
        let giant = "y".repeat(1000) + "\n" + &"z".repeat(50) + "\n";
        std::fs::write(&path, &giant).expect("write");
        // A 100-byte budget cannot hold the first line — it must be read to
        // its end anyway, or the tail would stall forever.
        let (chunk, offset, more) = read_new_lines(&path, 0, 100).expect("read");
        assert_eq!(chunk.len(), 1001);
        assert!(more);
        let (chunk2, _, more2) = read_new_lines(&path, offset, 100).expect("read");
        assert_eq!(chunk2, "z".repeat(50) + "\n");
        assert!(!more2);
    }

    #[test]
    fn read_tail_window_clips_oldest_for_snapshot_sources() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("snap.jsonl");
        let line = "x".repeat(99) + "\n";
        std::fs::write(&path, line.repeat(50)).expect("write");
        let (chunk, clipped) = read_tail_window(&path, 250).expect("read");
        assert!(clipped);
        // The torn first line inside the window is dropped.
        assert_eq!(chunk.len(), 200);
        let (full, clipped) = read_tail_window(&path, 1 << 20).expect("read");
        assert!(!clipped);
        assert_eq!(full.len(), 5000);
    }

    #[test]
    fn scan_claude_discovers_ingests_and_gates_on_signature() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        let slug = projects.join("-repo-a");
        std::fs::create_dir_all(&slug).expect("mkdir");
        let transcript = slug.join("sid-1.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#
                .to_string() + "\n",
        )
        .expect("write");

        let mut src = ClaudeSource::default();
        let mut sig = 0u64;
        assert!(scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1"),
            &[]
        ));
        assert_eq!(src.scan.events.len(), 1);
        // Unchanged file → gated, no re-ingest.
        assert!(!scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1"),
            &[]
        ));

        // Append → incremental ingest (events grow, not reset).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .expect("open");
        use std::io::Write as _;
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t2","name":"Read","input":{{"file_path":"/x"}}}}]}}}}"#
        )
        .expect("append");
        assert!(scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1"),
            &[]
        ));
        assert_eq!(src.scan.events.len(), 2);

        // Shrink (rewrite) → full reset + re-ingest.
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t9","name":"Bash","input":{"command":"pwd"}}]}}"#
                .to_string() + "\n",
        )
        .expect("rewrite");
        assert!(scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1"),
            &[]
        ));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "pwd");
    }

    #[test]
    fn scan_claude_merges_subagent_streams_by_timestamp() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        let slug = projects.join("-repo-a");
        let sub_dir = slug.join("sid-1").join("subagents");
        std::fs::create_dir_all(&sub_dir).expect("mkdir");
        std::fs::write(
            slug.join("sid-1.jsonl"),
            concat!(
                r#"{"type":"user","timestamp":"2026-07-08T12:00:00.000Z","message":{"role":"user","content":"Fix the tests"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-07-08T12:00:01.000Z","message":{"id":"m1","usage":{"output_tokens":20,"input_tokens":3,"cache_read_input_tokens":200,"cache_creation_input_tokens":40},"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-07-08T12:00:02.000Z","message":{"content":[{"type":"tool_use","id":"t2","name":"Task","input":{"description":"Explore backend","subagent_type":"Explore"}}]}}"#,
                "\n",
            ),
        )
        .expect("main");
        let sub_path = sub_dir.join("agent-abc.jsonl");
        std::fs::write(
            &sub_path,
            concat!(
                // The subagent's task prompt must NOT become a turn marker.
                r#"{"type":"user","timestamp":"2026-07-08T12:00:03.000Z","message":{"role":"user","content":"Explore the backend"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-07-08T12:00:04.000Z","message":{"id":"sm1","usage":{"output_tokens":10,"input_tokens":2,"cache_read_input_tokens":100,"cache_creation_input_tokens":5},"content":[{"type":"tool_use","id":"s1","name":"Read","input":{"file_path":"/repo/b.rs"}}]}}"#,
                "\n",
            ),
        )
        .expect("sub");

        let subs = vec![(sub_path, "Explore".to_string())];
        let mut src = ClaudeSource::default();
        let mut sig = 0u64;
        assert!(scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1"),
            &subs
        ));
        let kinds: Vec<ActionKind> = src.merged.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Prompt,
                ActionKind::Command,
                ActionKind::Subagent,
                ActionKind::Read, // the subagent's work, merged after by ts
            ]
        );
        assert_eq!(src.merged[3].origin.as_deref(), Some("Explore"));
        assert!(src.merged[..3].iter().all(|e| e.origin.is_none()));
        // The session's token totals fold in the subagent's usage.
        let act = SessionActivity {
            provider: ProviderKind::Claude,
            scan: ProviderScan::Claude(src),
            sig,
        };
        let meta = act.meta();
        assert_eq!(meta.output_tokens, Some(30), "main 20 + subagent 10");
        assert_eq!(meta.input_tokens, Some(5), "main 3 + subagent 2");
        assert_eq!(meta.cache_read_tokens, Some(300), "main 200 + sub 100");
        assert_eq!(meta.cache_write_tokens, Some(45), "main 40 + subagent 5");
        let ProviderScan::Claude(mut src) = act.scan else {
            unreachable!()
        };
        // Idle pass → gated, nothing changes.
        assert!(!scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1"),
            &subs
        ));
        // The sub transcript grows → merged stream picks it up.
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&subs[0].0)
            .expect("open");
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-07-08T12:00:05.000Z","message":{{"content":[{{"type":"tool_use","id":"s2","name":"Grep","input":{{"pattern":"fn main"}}}}]}}}}"#
        )
        .expect("append");
        assert!(scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1"),
            &subs
        ));
        assert_eq!(src.merged.len(), 5);
        assert_eq!(src.merged[4].kind, ActionKind::Search);
        assert_eq!(src.merged[4].origin.as_deref(), Some("Explore"));
        // The agent list shrinks (tree re-indexed) → its events drop out.
        assert!(scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1"),
            &[]
        ));
        assert_eq!(src.merged.len(), 3);
    }

    #[test]
    fn scan_vibe_binds_by_cwd_and_rebinds_to_newer_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("session");
        let old = root.join("session_20260712_100000_aaaaaaaa");
        std::fs::create_dir_all(&old).expect("mkdir");
        std::fs::write(
            old.join("meta.json"),
            r#"{"session_id":"old","environment":{"working_directory":"/repo/a"}}"#,
        )
        .expect("meta");
        std::fs::write(
            old.join("messages.jsonl"),
            r#"{"role":"assistant","content":"","tool_calls":[{"id":"c1","function":{"name":"bash","arguments":"{\"command\": \"ls\"}"},"type":"function"}]}"#
                .to_string() + "\n",
        )
        .expect("messages");

        let dirs = vec!["/repo/a".to_string()];
        let mut src = VibeSource::default();
        let mut sig = 0u64;
        assert!(scan_vibe(&mut src, &mut sig, Some(&root), &dirs));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.meta.session_id.as_deref(), Some("old"));

        // A session in another cwd never binds.
        let mut other = VibeSource::default();
        let mut other_sig = 0u64;
        assert!(!scan_vibe(
            &mut other,
            &mut other_sig,
            Some(&root),
            &["/elsewhere".to_string()]
        ));

        // A newer matching dir appears (agent restart) → rebind + fresh scan.
        let newer = root.join("session_20260712_110000_bbbbbbbb");
        std::fs::create_dir_all(&newer).expect("mkdir");
        std::fs::write(
            newer.join("meta.json"),
            r#"{"session_id":"new","environment":{"working_directory":"/repo/a"}}"#,
        )
        .expect("meta");
        std::fs::write(
            newer.join("messages.jsonl"),
            r#"{"role":"assistant","content":"","tool_calls":[{"id":"c1","function":{"name":"read_file","arguments":"{\"file_path\": \"/repo/a/x.rs\"}"},"type":"function"}]}"#
                .to_string() + "\n",
        )
        .expect("messages");
        assert!(scan_vibe(&mut src, &mut sig, Some(&root), &dirs));
        assert_eq!(src.meta.session_id.as_deref(), Some("new"));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].kind, ActionKind::Read);
    }

    #[test]
    fn scan_claude_clears_merged_events_on_empty_rewrite() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        let slug = projects.join("-repo-a");
        std::fs::create_dir_all(&slug).expect("mkdir");
        let transcript = slug.join("sid-1.jsonl");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#
                .to_string()
                + "\n",
        )
        .expect("write");
        let mut src = ClaudeSource::default();
        let mut sig = 0u64;
        assert!(scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1"),
            &[]
        ));
        assert_eq!(src.merged.len(), 1);

        // The transcript is cleared (rotation / deletion / truncate): the merged
        // stream must drop the vanished event, not keep displaying it.
        std::fs::write(&transcript, "").expect("truncate to empty");
        assert!(
            scan_claude(&mut src, &mut sig, Some(&projects), Some("sid-1"), &[]),
            "an emptying rewrite is a change"
        );
        assert!(src.scan.events.is_empty());
        assert!(
            src.merged.is_empty(),
            "stale events must not survive an empty rewrite"
        );
    }

    #[test]
    fn scan_vibe_drains_large_backfill_across_unchanged_passes() {
        // A backlog larger than INGEST_CHUNK must keep draining over successive
        // passes even though the file never changes again — regression for the
        // outer signature gate stranding the tail (the loader would hang).
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("session");
        let dir = root.join("session_20260712_100000_aaaaaaaa");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("meta.json"),
            r#"{"session_id":"s","environment":{"working_directory":"/repo/a"}}"#,
        )
        .expect("meta");
        let line = r#"{"role":"assistant","content":"","tool_calls":[{"id":"c1","function":{"name":"bash","arguments":"{\"command\": \"ls\"}"},"type":"function"}]}"#
            .to_string()
            + "\n";
        let repeats = (INGEST_CHUNK as usize / line.len()) + 500;
        std::fs::write(dir.join("messages.jsonl"), line.repeat(repeats)).expect("messages");
        let file_len = std::fs::metadata(dir.join("messages.jsonl")).unwrap().len();

        let dirs = vec!["/repo/a".to_string()];
        let mut src = VibeSource::default();
        let mut sig = 0u64;
        assert!(scan_vibe(&mut src, &mut sig, Some(&root), &dirs));
        assert!(src.backfilling, "one pass cannot drain a >8 MiB transcript");
        assert!(src.offset < file_len);

        let mut passes = 0;
        while src.backfilling && passes < 8 {
            scan_vibe(&mut src, &mut sig, Some(&root), &dirs);
            passes += 1;
        }
        assert!(
            !src.backfilling,
            "unchanged-file passes must keep draining the backlog"
        );
        assert_eq!(src.offset, file_len, "every byte was eventually ingested");
        assert_eq!(src.scan.events.len(), repeats);
    }

    #[test]
    fn claude_sub_sources_lists_standalone_and_workflow_agents() {
        use crate::session::{CcActivity, CcAgent, CcAgentState, CcRunStatus, CcWorkflow};
        let agent = |id: &str, path: &str, label: Option<&str>, atype: &str| CcAgent {
            agent_id: id.into(),
            transcript_path: PathBuf::from(path),
            agent_type: atype.into(),
            description: None,
            label: label.map(String::from),
            phase_title: None,
            state: CcAgentState::Done,
            mtime_ns: 0,
            size: 0,
            tokens: None,
            tool_calls: None,
            last_tool: None,
            model: None,
        };
        let mut info = SessionInfo::new("s".into());
        info.cc_activity = Some(CcActivity {
            subagents: vec![agent(
                "a1",
                "/p/sid/subagents/agent-a1.jsonl",
                None,
                "Explore",
            )],
            workflows: vec![CcWorkflow {
                run_id: "wf_1".into(),
                name: None,
                dir: PathBuf::from("/p/sid/subagents/workflows/wf_1"),
                status: CcRunStatus::Completed,
                phases: Vec::new(),
                agents: vec![agent(
                    "w1",
                    "/p/sid/subagents/workflows/wf_1/agent-w1.jsonl",
                    Some("fixer"),
                    "general-purpose",
                )],
                summary: None,
                tempo: None,
                needs: None,
            }],
        });
        let subs = claude_sub_sources(&info);
        // Both a standalone subagent AND a workflow-spawned agent become tail
        // sources (the latter was the "workflow events disappear" gap); the
        // completion label wins over agent_type as the origin.
        assert_eq!(subs.len(), 2);
        assert!(subs.contains(&(
            PathBuf::from("/p/sid/subagents/agent-a1.jsonl"),
            "Explore".to_string()
        )));
        assert!(subs.contains(&(
            PathBuf::from("/p/sid/subagents/workflows/wf_1/agent-w1.jsonl"),
            "fixer".to_string()
        )));
    }

    #[test]
    fn rebuild_merged_keeps_leading_unstamped_events_with_their_stream() {
        let ev_ts = |kind, detail: &str, ts: Option<u64>| ActivityEvent {
            ts_ms: ts,
            kind,
            detail: detail.into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor: false,
            dur_ms: None,
        };
        let mut src = ClaudeSource::default();
        src.scan.events = vec![
            ev_ts(ActionKind::Command, "cargo test", Some(100)),
            ev_ts(ActionKind::Edit, "/a.rs", Some(300)),
        ];
        // A subagent stream whose FIRST event carries no timestamp, followed by
        // one stamped at 200 — the leading unstamped read must sort with its
        // stream (near 200), not jump to the front of the merged stream.
        let mut sub = SubTail {
            scan: ClaudeScan::default(),
            sig: 0,
            offset: 0,
            backfilling: false,
            origin: "Explore".to_string(),
        };
        sub.scan.events = vec![
            ev_ts(ActionKind::Read, "/b.rs", None),
            ev_ts(ActionKind::Search, "fn main", Some(200)),
        ];
        src.subs.insert(PathBuf::from("/sub/agent-x.jsonl"), sub);
        rebuild_merged(&mut src);
        let order: Vec<&str> = src.merged.iter().map(|e| e.detail.as_str()).collect();
        assert_eq!(
            order,
            vec!["cargo test", "/b.rs", "fn main", "/a.rs"],
            "leading unstamped subagent event must not sort ahead of the main stream"
        );
    }
}
