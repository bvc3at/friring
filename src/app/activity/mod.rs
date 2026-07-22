//! Agent-neutral activity — provider dispatch + the off-thread event scan.
//!
//! The F9 activity view reconstructs what a session's agent *did* (commands /
//! edits / reads / web / subagents) from whatever its CLI persists on disk.
//! Each supported CLI gets a **provider**: pure record→event parsers live in
//! [`crate::session::activity`]; this module is the filesystem glue —
//! per-session source discovery, a stat-signature gate, incremental tailing
//! of append-only sources, and the `spawn_blocking` refresh that mirrors
//! `cc_refresh` (see [`super::cc_activity`], which still owns the
//! Claude-specific workflow/subagent tree scan).
//!
//! Ownership model: each session's [`SessionActivity`] accumulator is *moved*
//! into the scan thread and back each cadence ([`App::start_activity_refresh`]
//! / [`App::poll_activity_refresh`]). While in flight the view keeps its last
//! built rows, so rendering never blocks on the scan.

mod aider;
mod cline;
mod codex;
mod copilot;
mod crush;
mod cursor;
mod gemini;
mod goose;
mod opencode;
mod qwen;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::session::activity::vibe::{parse_meta as parse_vibe_meta, VibeMeta, VibeScan};
use crate::session::activity::{
    aggregate_files, claude::ClaudeScan, ActionKind, ActivityCounts, ActivityEvent, ActivityMeta,
    FileTouch,
};
use crate::session::cc_activity::TranscriptBlock;
use crate::session::{SessionId, SessionInfo};

use super::background::TaskPoll;
use super::cc_activity::{CcRow, SparkRow, StatTile, Tone};
use super::App;

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

/// The navigator sections of the activity view, in display order. `Agents`
/// hosts the (Claude-specific) workflow/subagent tree beneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Section {
    Overview,
    Timeline,
    Commands,
    Files,
    Web,
    Agents,
}

pub(crate) const SECTIONS: [Section; 6] = [
    Section::Overview,
    Section::Timeline,
    Section::Commands,
    Section::Files,
    Section::Web,
    Section::Agents,
];

impl Section {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Section::Overview => "Overview",
            Section::Timeline => "Timeline",
            Section::Commands => "Commands",
            Section::Files => "Files",
            Section::Web => "Web",
            Section::Agents => "Agents",
        }
    }
}

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
fn unsupported_reason(command: &str) -> Option<&'static str> {
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
    fn new(provider: ProviderKind) -> Self {
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

    /// Test seam: an accumulator pre-filled with `events`, as if a scan pass
    /// had ingested them (acceptance tests can't run the fs scan).
    #[cfg(test)]
    pub(super) fn seeded(provider: ProviderKind, events: Vec<ActivityEvent>) -> Self {
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
pub(super) struct ActivityRefresh {
    updates: Vec<(SessionId, SessionActivity, bool)>,
}

/// One session's scan input: identity for discovery plus the accumulator
/// (taken from [`App::activity`] for the duration of the pass).
struct ActivityInput {
    id: SessionId,
    own_id: Option<String>,
    /// Normalized launch dirs (worktrees / additional dirs / process cwd) —
    /// the cwd-match key for providers that don't record a session id.
    dirs: Vec<String>,
    /// Claude only: the subagent/workflow transcripts the cc tree scan has
    /// indexed, `(path, origin label)` — the event scan tails these too.
    sub_sources: Vec<(PathBuf, String)>,
    state: SessionActivity,
}

/// Provider state roots, resolved on the UI thread (env access) and read on
/// the scan thread.
struct ScanRoots {
    claude_projects: Option<PathBuf>,
    vibe_sessions: Option<PathBuf>,
    qwen_projects: Option<PathBuf>,
    cursor_root: Option<PathBuf>,
    gemini_home: Option<PathBuf>,
    copilot_sessions: Option<PathBuf>,
    aider_history: Option<PathBuf>,
    goose_sessions: Option<PathBuf>,
    opencode_db: Option<PathBuf>,
    codex_sessions: Option<PathBuf>,
    cline_sessions: Option<PathBuf>,
}

impl App {
    /// The CLI command behind a session's agent (the registry name itself
    /// when the entry is gone) — the provider-dispatch key.
    pub(super) fn session_command(&self, info: &SessionInfo) -> String {
        self.agents
            .get(&info.agent)
            .map(|a| a.command.clone())
            .unwrap_or_else(|| info.agent.clone())
    }

    /// The activity provider for a session, from its registry entry's command
    /// basename.
    pub(crate) fn session_provider(&self, info: &SessionInfo) -> Option<ProviderKind> {
        ProviderKind::for_command(&self.session_command(info))
    }

    /// Kick off a background event scan for every local session with a
    /// provider. Accumulators are moved into the pass and returned by
    /// [`Self::poll_activity_refresh`].
    pub(super) fn start_activity_refresh(&mut self) {
        if self.activity_refresh.in_progress() {
            return;
        }
        type PreInput = (
            SessionId,
            ProviderKind,
            Option<String>,
            Vec<String>,
            Vec<(PathBuf, String)>,
        );
        let pre: Vec<PreInput> = self
            .sessions
            .iter()
            .filter(|s| s.info.remote_host.is_none())
            .filter_map(|s| {
                let provider = self.session_provider(&s.info)?;
                let sub_sources = if provider == ProviderKind::Claude {
                    claude_sub_sources(&s.info)
                } else {
                    Vec::new()
                };
                Some((
                    s.info.id,
                    provider,
                    s.info.agent_session_id.clone(),
                    self.session_candidate_dirs(&s.info),
                    sub_sources,
                ))
            })
            .collect();
        // Evict accumulators for sessions no longer eligible (deleted, gone
        // remote, or repointed to an unsupported command) so their event
        // vectors don't leak across session churn. Safe here: the in_progress
        // guard means no accumulator is checked out, and it must run even when
        // `pre` is empty so every stale entry clears.
        let ids: std::collections::HashSet<SessionId> = pre.iter().map(|(id, ..)| *id).collect();
        self.activity.retain(|id, _| ids.contains(id));
        if pre.is_empty() {
            return;
        }
        let inputs: Vec<ActivityInput> = pre
            .into_iter()
            .map(|(id, provider, own_id, dirs, sub_sources)| {
                let state = self
                    .activity
                    .remove(&id)
                    // An agents.toml edit can repoint a session's provider —
                    // stale accumulators must not survive that.
                    .filter(|a| a.provider == provider)
                    .unwrap_or_else(|| SessionActivity::new(provider));
                ActivityInput {
                    id,
                    own_id,
                    dirs,
                    sub_sources,
                    state,
                }
            })
            .collect();
        let roots = ScanRoots {
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
        };
        let tx = self.activity_refresh.start();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(collect_activity(roots, inputs));
        });
    }

    /// Apply a finished scan pass: put the accumulators back and live-refresh
    /// the open view.
    pub(super) fn poll_activity_refresh(&mut self) {
        let TaskPoll::Done(refresh) = self.activity_refresh.poll() else {
            return;
        };
        let mut any_changed = false;
        for (id, state, changed) in refresh.updates {
            self.activity.insert(id, state);
            any_changed |= changed;
        }
        // The open view rebuilds its snapshot every pass (not only on change):
        // a section opened while the accumulator was in flight rendered from
        // nothing and needs the returned state even if the scan saw no growth.
        self.refresh_activity_view(any_changed);
    }
}

/// The whole scan pass, run on a blocking thread.
fn collect_activity(roots: ScanRoots, inputs: Vec<ActivityInput>) -> ActivityRefresh {
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
        let mut last = 0u64;
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
fn claude_sub_sources(info: &SessionInfo) -> Vec<(PathBuf, String)> {
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
        let wd = super::cc_activity::normalize_dir(&wd);
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

// ─── Section content builders (view side) ───────────────────────────────────

/// Which event kinds an event-list section shows.
fn section_includes(section: Section, kind: ActionKind) -> bool {
    match section {
        Section::Timeline => true,
        Section::Commands => kind == ActionKind::Command,
        Section::Web => matches!(kind, ActionKind::WebSearch | ActionKind::WebFetch),
        _ => false,
    }
}

/// Blocks for an event-list section (timeline / commands / web) — rendered by
/// the same engine as transcripts, so folds/find/wrap come for free. The
/// Timeline additionally folds repetition runs (see [`timeline_blocks`]).
pub(super) fn section_blocks(events: &[ActivityEvent], section: Section) -> Vec<TranscriptBlock> {
    if section == Section::Timeline {
        return timeline_blocks(events);
    }
    events
        .iter()
        .filter(|e| section_includes(section, e.kind))
        .cloned()
        .map(TranscriptBlock::Event)
        .collect()
}

/// The Timeline's block list: every event, with consecutive runs of the same
/// read/search/bookkeeping action folded into one `detail ×N` row (keeping
/// the run's newest timestamp/result) so a re-read loop doesn't drown the
/// turn it belongs to. Commands/edits never fold — each is its own action.
fn timeline_blocks(events: &[ActivityEvent]) -> Vec<TranscriptBlock> {
    let foldable =
        |e: &ActivityEvent| matches!(e.kind, ActionKind::Read | ActionKind::Search) || e.minor;
    let same_run = |a: &ActivityEvent, b: &ActivityEvent| {
        a.kind == b.kind && a.detail == b.detail && a.origin == b.origin && a.minor == b.minor
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < events.len() {
        let e = &events[i];
        let mut n = 1;
        if foldable(e) {
            while i + n < events.len() && same_run(e, &events[i + n]) {
                n += 1;
            }
        }
        if n > 1 {
            // The newest occurrence carries the freshest result/timestamp.
            let mut folded = events[i + n - 1].clone();
            folded.detail = format!("{}  ×{n}", folded.detail);
            out.push(TranscriptBlock::Event(folded));
        } else {
            out.push(TranscriptBlock::Event(e.clone()));
        }
        i += n;
    }
    out
}

/// The Files section: edited then read-only paths, most recently touched
/// first within each group.
pub(super) fn files_rows(events: &[ActivityEvent]) -> Vec<CcRow> {
    let files = aggregate_files(events);
    if files.is_empty() {
        return vec![CcRow::Info("No file activity yet.".to_string())];
    }
    let mut rows = Vec::new();
    let (edited, read_only): (Vec<&FileTouch>, Vec<&FileTouch>) =
        files.iter().partition(|f| f.edits > 0);
    if !edited.is_empty() {
        rows.push(CcRow::Header(format!("Edited ({})", edited.len())));
        rows.extend(edited.iter().map(|f| CcRow::Text(file_line(f))));
    }
    if !read_only.is_empty() {
        if !rows.is_empty() {
            rows.push(CcRow::Info(String::new()));
        }
        rows.push(CcRow::Header(format!("Read ({})", read_only.len())));
        rows.extend(read_only.iter().map(|f| CcRow::Text(file_line(f))));
    }
    rows
}

fn file_line(f: &FileTouch) -> String {
    let mut line = format!("  {}", f.path);
    if f.edits > 0 {
        line.push_str(&format!("  ✎{}", f.edits));
    }
    if f.reads > 0 {
        line.push_str(&format!("  r{}", f.reads));
    }
    if let Some(ts) = f.last_ts_ms {
        line.push_str(&format!("  {}", fmt_time(ts)));
    }
    line
}

/// Local wall-clock `HH:MM:SS` of an epoch-ms timestamp. Out-of-range values
/// (a malformed record's garbage `ts`) render a placeholder rather than
/// panicking the render thread — `from_timestamp_millis` rejects them.
pub(crate) fn fmt_time(ts_ms: u64) -> String {
    match i64::try_from(ts_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        Some(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string(),
        None => "--:--:--".to_string(),
    }
}

/// The Overview section, as a dashboard: identity line, stat tiles, an
/// activity sparkline, hottest files, the last failure, and the agents-tree
/// summary. `provider` reflects whether the agent is supported at all;
/// `activity` is its accumulator once a scan pass has run.
pub(super) fn overview_rows(
    agent_name: &str,
    command: &str,
    provider: Option<ProviderKind>,
    activity: Option<&SessionActivity>,
    info: &SessionInfo,
) -> Vec<CcRow> {
    let mut rows = Vec::new();
    match activity {
        None => {
            rows.push(CcRow::Text(format!("Agent: {agent_name}")));
            rows.push(CcRow::Info(String::new()));
            rows.push(CcRow::Info(match provider {
                Some(_) => "No activity captured yet.".to_string(),
                None => unsupported_reason(command)
                    .unwrap_or("Activity capture isn't supported for this agent yet.")
                    .to_string(),
            }));
        }
        Some(act) => {
            let meta = act.meta();
            let mut identity = format!("{agent_name} · {}", act.provider.id());
            if let Some(m) = &meta.model {
                identity.push_str(&format!(" · {m}"));
            }
            rows.push(CcRow::Text(identity));
            if let Some(t) = &meta.title {
                rows.push(CcRow::Text(format!("Title: {t}")));
            }
            if let Some(line) = token_line(&meta) {
                rows.push(CcRow::Text(line));
            }
            let events = act.events();
            let c = ActivityCounts::tally(events);
            rows.push(CcRow::Info(String::new()));
            rows.push(CcRow::Tiles(stat_tiles(&c)));
            if let Some(spark) = spark_row(events) {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Spark(spark));
            }
            if let Some(line) = turns_line(&c, events) {
                rows.push(CcRow::Text(line));
            }
            if act.backfilling() {
                rows.push(CcRow::Info(
                    "⟳ indexing history — older activity still loading…".to_string(),
                ));
            } else if act.truncated() {
                rows.push(CcRow::Info(
                    "(long history — oldest activity clipped)".to_string(),
                ));
            }
            let files = aggregate_files(events);
            if !files.is_empty() {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Header("Hot files".to_string()));
                rows.extend(files.iter().take(5).map(|f| CcRow::Text(file_line(f))));
            }
            let top = top_commands(events);
            if !top.is_empty() {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Header("Top commands".to_string()));
                rows.extend(
                    top.into_iter()
                        .map(|(n, cmd)| CcRow::Text(format!("  ×{n}  {cmd}"))),
                );
            }
            let recent = recent_rows(events);
            if !recent.is_empty() {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Header("Recent".to_string()));
                rows.extend(recent);
            }
            if let Some(e) = events
                .iter()
                .rev()
                .find(|e| e.ok == Some(false) && !e.minor)
            {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Header("Last error".to_string()));
                let mut line = String::from("✗ ");
                if let Some(ts) = e.ts_ms {
                    line.push_str(&format!("{}  ", fmt_time(ts)));
                }
                line.push_str(e.detail.lines().next().unwrap_or(""));
                rows.push(CcRow::Text(line));
                if let Some(r) = &e.result_head {
                    if let Some(first) = r.lines().find(|l| !l.trim().is_empty()) {
                        rows.push(CcRow::Info(format!("  ⎿ {first}")));
                    }
                }
            }
        }
    }
    let (wf, sub) = info
        .cc_activity
        .as_ref()
        .map(|a| (a.workflows.len(), a.subagents.len()))
        .unwrap_or((0, 0));
    if wf + sub > 0 {
        rows.push(CcRow::Info(String::new()));
        rows.push(CcRow::Text(format!(
            "{wf} workflows · {sub} subagents — see the Agents section"
        )));
    }
    rows
}

/// The Overview's tile row. Zero-count tiles stay (a stable layout reads
/// faster than a shifting one); the failure tile appears only on failure.
fn stat_tiles(c: &ActivityCounts) -> Vec<StatTile> {
    let tile = |glyph, value: usize, label, tone| StatTile {
        glyph,
        value: value.to_string(),
        label,
        tone,
    };
    let mut tiles = vec![
        tile("$", c.commands, "cmds", Tone::Accent),
        tile("✎", c.edits, "edits", Tone::Working),
        tile("⊙", c.reads, "reads", Tone::Normal),
        tile("⌕", c.searches, "greps", Tone::Normal),
        tile("⚲", c.web, "web", Tone::Done),
        tile("⚙", c.subagents, "agents", Tone::Accent),
    ];
    if c.failed > 0 {
        tiles.push(tile("✗", c.failed, "failed", Tone::Danger));
    }
    tiles
}

/// Bucket the timestamped, non-minor events across the session's span. `None`
/// when fewer than two distinct timestamps exist (no span to draw).
fn spark_row(events: &[ActivityEvent]) -> Option<SparkRow> {
    const BUCKETS: u64 = 32;
    let ts: Vec<u64> = events
        .iter()
        .filter(|e| !e.minor)
        .filter_map(|e| e.ts_ms)
        .collect();
    let first = *ts.iter().min()?;
    let last = *ts.iter().max()?;
    if first == last {
        return None;
    }
    let span = last - first + 1;
    let mut buckets = vec![0u64; BUCKETS as usize];
    let last_bucket = buckets.len() - 1;
    for t in &ts {
        let i = ((t - first) * BUCKETS / span) as usize;
        buckets[i.min(last_bucket)] += 1;
    }
    Some(SparkRow {
        start: fmt_hm(first),
        end: fmt_hm(last),
        buckets,
        caption: format!("{} events · {}", ts.len(), fmt_span_ms(span)),
    })
}

/// Compact token count: `1.2M`, `128k`, exact below five digits.
fn fmt_tok(t: u64) -> String {
    if t >= 1_000_000 {
        format!("{:.1}M", t as f64 / 1e6)
    } else if t >= 10_000 {
        format!("{}k", t / 1000)
    } else {
        t.to_string()
    }
}

/// `tokens  12k in · 128k out · cache 1.2M r / 340k w` — whatever tallies the
/// stream records (main + subagent/workflow transcripts, via
/// [`SessionActivity::meta`]); `None` when nothing was recorded.
fn token_line(meta: &ActivityMeta) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(t) = meta.input_tokens.filter(|&t| t > 0) {
        parts.push(format!("{} in", fmt_tok(t)));
    }
    if let Some(t) = meta.output_tokens.filter(|&t| t > 0) {
        parts.push(format!("{} out", fmt_tok(t)));
    }
    let read = meta.cache_read_tokens.filter(|&t| t > 0);
    let write = meta.cache_write_tokens.filter(|&t| t > 0);
    match (read, write) {
        (Some(r), Some(w)) => parts.push(format!("cache {} r / {} w", fmt_tok(r), fmt_tok(w))),
        (Some(r), None) => parts.push(format!("cache {} r", fmt_tok(r))),
        (None, Some(w)) => parts.push(format!("cache {} w", fmt_tok(w))),
        (None, None) => {}
    }
    (!parts.is_empty()).then(|| format!("tokens  {}", parts.join(" · ")))
}

/// `8 turns · last action 14:41:07`, under the sparkline.
fn turns_line(c: &ActivityCounts, events: &[ActivityEvent]) -> Option<String> {
    let mut parts = Vec::new();
    match c.prompts {
        0 => {}
        1 => parts.push("1 turn".to_string()),
        n => parts.push(format!("{n} turns")),
    }
    // The last *action* — not a turn marker or bookkeeping row — so a freshly
    // submitted prompt can't misreport when the agent last did work.
    if let Some(ts) = events
        .iter()
        .rev()
        .filter(|e| e.kind != ActionKind::Prompt && !e.minor)
        .find_map(|e| e.ts_ms)
    {
        parts.push(format!("last action {}", fmt_time(ts)));
    }
    (!parts.is_empty()).then(|| format!(" {}", parts.join(" · ")))
}

/// The most-repeated command lines (≥2 runs), count-desc — surfacing what the
/// agent looped on. At most three.
fn top_commands(events: &[ActivityEvent]) -> Vec<(usize, String)> {
    let mut freq: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for e in events {
        if e.kind == ActionKind::Command && !e.minor {
            *freq
                .entry(e.detail.lines().next().unwrap_or(""))
                .or_default() += 1;
        }
    }
    let mut top: Vec<(usize, String)> = freq
        .into_iter()
        .filter(|&(_, n)| n >= 2)
        .map(|(cmd, n)| (n, cmd.to_string()))
        .collect();
    top.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    top.truncate(3);
    top
}

/// The newest few actions, oldest-first — "what just happened" without
/// leaving the Overview.
fn recent_rows(events: &[ActivityEvent]) -> Vec<CcRow> {
    let newest: Vec<&ActivityEvent> = events
        .iter()
        .rev()
        .filter(|e| !e.minor && e.kind != ActionKind::Prompt)
        .take(4)
        .collect();
    newest
        .into_iter()
        .rev()
        .map(|e| {
            let mut line = String::from("  ");
            if let Some(ts) = e.ts_ms {
                line.push_str(&format!("{}  ", fmt_time(ts)));
            }
            line.push_str(&format!(
                "{:<5} {}",
                e.kind.tag(),
                e.detail.lines().next().unwrap_or("")
            ));
            if e.ok == Some(false) {
                line.push_str("  ✗");
            }
            CcRow::Text(line)
        })
        .collect()
}

/// Local wall-clock `HH:MM` (sparkline endpoints).
fn fmt_hm(ts_ms: u64) -> String {
    match i64::try_from(ts_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        Some(dt) => dt.with_timezone(&chrono::Local).format("%H:%M").to_string(),
        None => "--:--".to_string(),
    }
}

/// Compact duration: `38s`, `42m`, `1h12m`.
fn fmt_span_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: ActionKind, detail: &str) -> ActivityEvent {
        ActivityEvent {
            ts_ms: None,
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
    fn fmt_time_never_panics_on_out_of_range_ts() {
        // A malformed record can carry an arbitrary `ts`; the conversion must
        // degrade to a placeholder instead of panicking the render thread.
        assert_eq!(fmt_time(u64::MAX), "--:--:--");
        // A sane in-range timestamp still formats to wall-clock.
        assert_ne!(fmt_time(1_783_512_000_000), "--:--:--");
    }

    #[test]
    fn section_blocks_filter_by_kind() {
        let events = [
            ev(ActionKind::Command, "ls"),
            ev(ActionKind::Edit, "/a.rs"),
            ev(ActionKind::WebSearch, "docs"),
            ev(ActionKind::WebFetch, "https://x"),
        ];
        assert_eq!(section_blocks(&events, Section::Timeline).len(), 4);
        assert_eq!(section_blocks(&events, Section::Commands).len(), 1);
        assert_eq!(section_blocks(&events, Section::Web).len(), 2);
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
    fn overview_rows_cover_supported_and_unsupported() {
        let mut info = SessionInfo::new("s".to_string());
        let rows = overview_rows("my-agent", "my-agent-cli", None, None, &info);
        assert!(rows.iter().any(
            |r| matches!(r, CcRow::Info(s) if s.contains("isn't supported for this agent yet"))
        ));
        // Supported provider, but no scan pass yet.
        let rows = overview_rows("claude", "claude", Some(ProviderKind::Claude), None, &info);
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Info(s) if s.contains("No activity captured yet"))));
        // A researched-but-unsupported CLI names its reason.
        let rows = overview_rows("antigravity", "agy", None, None, &info);
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Info(s) if s.contains("encrypts its conversation store"))));

        let mut act = SessionActivity::new(ProviderKind::Vibe);
        if let ProviderScan::Vibe(s) = &mut act.scan {
            s.scan.ingest(
                r#"{"role":"assistant","content":"","tool_calls":[{"id":"c1","function":{"name":"bash","arguments":"{\"command\": \"make\"}"},"type":"function"}]}"#,
            );
            s.meta.meta.title = Some("Build it".into());
        }
        info.cc_activity = None;
        let rows = overview_rows("vibe", "vibe", Some(ProviderKind::Vibe), Some(&act), &info);
        // Identity line: `<agent> · <provider id> · …`.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.starts_with("vibe · vibe"))));
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.contains("Title: Build it"))));
        // The counts render as stat tiles now — one command tile with value 1.
        assert!(rows.iter().any(|r| matches!(
            r,
            CcRow::Tiles(t) if t.iter().any(|x| x.label == "cmds" && x.value == "1")
        )));
    }

    #[test]
    fn overview_dashboard_rows_cover_spark_error_and_loader() {
        let ts = |m: u64| Some(1_783_512_000_000 + m * 60_000);
        let mk = |kind, detail: &str, ts_ms: Option<u64>, ok| ActivityEvent {
            ts_ms,
            kind,
            detail: detail.into(),
            note: None,
            result_head: Some("boom: exit 101".into()),
            ok,
            origin: None,
            minor: false,
            dur_ms: None,
        };
        let events = vec![
            mk(ActionKind::Prompt, "Fix it", ts(0), None),
            mk(ActionKind::Command, "cargo test", ts(1), Some(false)),
            mk(ActionKind::Edit, "/repo/a.rs", ts(30), Some(true)),
        ];
        let act = SessionActivity::seeded(ProviderKind::Claude, events);
        let info = SessionInfo::new("s".to_string());
        let rows = overview_rows(
            "claude",
            "claude",
            Some(ProviderKind::Claude),
            Some(&act),
            &info,
        );
        // A sparkline spans the 30-minute session.
        assert!(rows.iter().any(|r| matches!(
            r,
            CcRow::Spark(s) if s.caption.contains("events") && s.caption.contains("30m")
        )));
        // The failed command surfaces as the Last error block.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Header(s) if s == "Last error")));
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.contains('✗') && s.contains("cargo test"))));
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Info(s) if s.contains("boom: exit 101"))));
        // The failed tile appears only because a failure exists.
        assert!(rows.iter().any(|r| matches!(
            r,
            CcRow::Tiles(t) if t.iter().any(|x| x.label == "failed" && x.value == "1")
        )));
        // Hot files header present for the edited file.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Header(s) if s == "Hot files")));
        // The turns line counts the single prompt.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.contains("1 turn ·"))));
        // Recent lists the newest actions with their kind tags.
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Header(s) if s == "Recent")));
        assert!(rows.iter().any(
            |r| matches!(r, CcRow::Text(s) if s.contains("edit") && s.contains("/repo/a.rs"))
        ));
    }

    #[test]
    fn token_line_reports_all_recorded_tallies() {
        let meta = ActivityMeta {
            output_tokens: Some(128_000),
            input_tokens: Some(12_400),
            cache_read_tokens: Some(1_234_567),
            cache_write_tokens: Some(340_000),
            ..Default::default()
        };
        assert_eq!(
            token_line(&meta).as_deref(),
            Some("tokens  12k in · 128k out · cache 1.2M r / 340k w")
        );
        // Output-only stream (most non-claude providers).
        let meta = ActivityMeta {
            output_tokens: Some(500),
            ..Default::default()
        };
        assert_eq!(token_line(&meta).as_deref(), Some("tokens  500 out"));
        assert_eq!(token_line(&ActivityMeta::default()), None);
    }

    #[test]
    fn top_commands_surface_only_repeats() {
        let cmd = |detail: &str| ActivityEvent {
            ts_ms: None,
            kind: ActionKind::Command,
            detail: detail.into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor: false,
            dur_ms: None,
        };
        let events = vec![
            cmd("cargo test"),
            cmd("cargo test"),
            cmd("cargo test"),
            cmd("cargo fmt"),
            cmd("cargo fmt"),
            cmd("git status"), // ran once → not "top"
        ];
        let top = top_commands(&events);
        assert_eq!(
            top,
            vec![(3, "cargo test".to_string()), (2, "cargo fmt".to_string())]
        );
        assert!(top_commands(&[cmd("once")]).is_empty());
    }

    #[test]
    fn timeline_blocks_fold_repeated_reads() {
        let mk = |kind, detail: &str| ActivityEvent {
            ts_ms: None,
            kind,
            detail: detail.into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor: false,
            dur_ms: None,
        };
        let events = vec![
            mk(ActionKind::Read, "/a.rs"),
            mk(ActionKind::Read, "/a.rs"),
            mk(ActionKind::Read, "/a.rs"),
            mk(ActionKind::Command, "ls"),
            mk(ActionKind::Command, "ls"), // commands never fold
            mk(ActionKind::Read, "/a.rs"), // non-consecutive → its own row
        ];
        let blocks = timeline_blocks(&events);
        assert_eq!(blocks.len(), 4);
        let TranscriptBlock::Event(first) = &blocks[0] else {
            panic!("event block");
        };
        assert_eq!(first.detail, "/a.rs  ×3");
        let TranscriptBlock::Event(last) = &blocks[3] else {
            panic!("event block");
        };
        assert_eq!(last.detail, "/a.rs");
    }

    #[test]
    fn timeline_folding_covers_searches_minor_and_boundaries() {
        let mk = |kind, detail: &str, origin: Option<&str>, minor: bool| ActivityEvent {
            ts_ms: None,
            kind,
            detail: detail.into(),
            note: None,
            result_head: None,
            ok: None,
            origin: origin.map(String::from),
            minor,
            dur_ms: None,
        };
        // Searches fold; a changed detail breaks the run; bookkeeping (minor)
        // folds; and a differing origin is a separate run even at equal detail.
        let events = vec![
            mk(ActionKind::Search, "fn main", None, false),
            mk(ActionKind::Search, "fn main", None, false),
            mk(ActionKind::Search, "fn other", None, false), // detail changed → new row
            mk(ActionKind::Other, "TodoWrite", None, true),
            mk(ActionKind::Other, "TodoWrite", None, true), // minor run folds
            mk(ActionKind::Read, "/a.rs", None, false),
            mk(ActionKind::Read, "/a.rs", Some("sub"), false), // origin differs → separate
        ];
        let details: Vec<String> = timeline_blocks(&events)
            .iter()
            .map(|b| match b {
                TranscriptBlock::Event(e) => e.detail.clone(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            details,
            vec![
                "fn main  ×2".to_string(),
                "fn other".to_string(),
                "TodoWrite  ×2".to_string(),
                "/a.rs".to_string(),
                "/a.rs".to_string(), // not folded across the origin boundary
            ]
        );
    }

    #[test]
    fn turns_line_last_action_ignores_prompts_and_minor() {
        let ev = |kind, ts: u64, minor: bool| ActivityEvent {
            ts_ms: Some(ts),
            kind,
            detail: "x".into(),
            note: None,
            result_head: None,
            ok: None,
            origin: None,
            minor,
            dur_ms: None,
        };
        // A command at T, then a bookkeeping row and a fresh prompt after it —
        // "last action" must report the command's time, not the later rows.
        let base = 1_783_512_000_000;
        let events = vec![
            ev(ActionKind::Command, base, false),
            ev(ActionKind::Other, base + 60_000, true), // minor
            ev(ActionKind::Prompt, base + 120_000, false),
        ];
        let counts = ActivityCounts::tally(&events);
        let line = turns_line(&counts, &events).expect("a turn was counted");
        assert!(line.contains("1 turn"), "{line}");
        assert!(
            line.contains(&format!("last action {}", fmt_time(base))),
            "last action must be the command, not the prompt/minor: {line}"
        );
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
}
