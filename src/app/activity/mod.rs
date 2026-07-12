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
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::session::activity::vibe::{parse_meta as parse_vibe_meta, VibeMeta, VibeScan};
use crate::session::activity::{
    aggregate_files, claude::ClaudeScan, ActionKind, ActivityCounts, ActivityEvent, ActivityMeta,
    FileTouch,
};
use crate::session::cc_activity::TranscriptBlock;
use crate::session::{SessionId, SessionInfo};

use super::background::TaskPoll;
use super::cc_activity::CcRow;
use super::App;

/// Cap on the first ingest of an existing source: a months-old transcript can
/// be tens of MB, so the initial read starts this far from the end (clipped
/// history is surfaced via [`SessionActivity::truncated`]). Growth past the
/// first read is always ingested in full.
const INITIAL_INGEST_MAX: u64 = 8 * 1024 * 1024;

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
            ProviderScan::Claude(s) => &s.scan.events,
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
            ProviderScan::Claude(s) => s.scan.meta.clone(),
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

    /// Whether the initial ingest clipped old history (huge source file).
    pub(crate) fn truncated(&self) -> bool {
        match &self.scan {
            ProviderScan::Claude(s) => s.truncated,
            ProviderScan::Vibe(s) => s.truncated,
            ProviderScan::Qwen(s) => s.truncated,
            ProviderScan::Cursor(s) => s.truncated,
            ProviderScan::Gemini(s) => s.truncated,
            ProviderScan::Crush(s) => s.truncated,
            ProviderScan::Copilot(s) => s.truncated,
            ProviderScan::Aider(s) => s.truncated,
            ProviderScan::Goose(s) => s.truncated,
            ProviderScan::Opencode(s) => s.truncated,
            ProviderScan::Codex(s) => s.truncated,
            ProviderScan::Cline(s) => s.truncated,
        }
    }

    /// Test seam: an accumulator pre-filled with `events`, as if a scan pass
    /// had ingested them (acceptance tests can't run the fs scan).
    #[cfg(test)]
    pub(super) fn seeded(provider: ProviderKind, events: Vec<ActivityEvent>) -> Self {
        let mut act = Self::new(provider);
        match &mut act.scan {
            ProviderScan::Claude(s) => s.scan.events = events,
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
/// `projects/<slug>/<agent_session_id>.jsonl`, found by slug-dir scan.
#[derive(Default)]
struct ClaudeSource {
    scan: ClaudeScan,
    transcript: Option<PathBuf>,
    offset: u64,
    truncated: bool,
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
    truncated: bool,
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
        let pre: Vec<(SessionId, ProviderKind, Option<String>, Vec<String>)> = self
            .sessions
            .iter()
            .filter(|s| s.info.remote_host.is_none())
            .filter_map(|s| {
                let provider = self.session_provider(&s.info)?;
                Some((
                    s.info.id,
                    provider,
                    s.info.agent_session_id.clone(),
                    self.session_candidate_dirs(&s.info),
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
            .map(|(id, provider, own_id, dirs)| {
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

/// Tail the session's main Claude transcript. Returns whether anything new
/// was ingested.
fn scan_claude(
    src: &mut ClaudeSource,
    sig: &mut u64,
    projects: Option<&Path>,
    own_id: Option<&str>,
) -> bool {
    if src.transcript.is_none() {
        if let (Some(projects), Some(id)) = (projects, own_id) {
            src.transcript = find_claude_transcript(projects, id);
        }
    }
    let Some(path) = src.transcript.clone() else {
        return false;
    };
    tail_source(&path, sig, &mut src.offset, &mut src.truncated, |chunk| {
        src.scan.ingest(chunk)
    })
    .unwrap_or_else(|| {
        // Shrunk (rotated/rewritten): reset the streaming parser and re-ingest.
        src.scan = ClaudeScan::default();
        src.offset = 0;
        src.truncated = false;
        tail_source(&path, sig, &mut src.offset, &mut src.truncated, |chunk| {
            src.scan.ingest(chunk)
        })
        .unwrap_or(false)
    })
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
    if new_sig == *sig {
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
        &mut src.truncated,
        |chunk| src.scan.ingest(chunk),
    )
    .is_none()
    {
        // Vibe rewrites messages.jsonl in full on rewind/compact.
        src.scan = VibeScan::default();
        src.offset = 0;
        src.truncated = false;
        let _ = tail_source(
            &messages,
            &mut msg_sig,
            &mut src.offset,
            &mut src.truncated,
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
/// to `ingest`, and advance `offset`. Returns `None` when the file shrank
/// (caller resets its parser and retries), else `Some(ingested-anything)`.
fn tail_source(
    path: &Path,
    sig: &mut u64,
    offset: &mut u64,
    truncated: &mut bool,
    ingest: impl FnOnce(&str),
) -> Option<bool> {
    let new_sig = stat_signature(&[path]);
    if new_sig == *sig {
        return Some(false);
    }
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        return None; // shrank — caller resets and re-ingests
    }
    *sig = new_sig;
    if len == *offset {
        return Some(false); // mtime moved but nothing new (touch)
    }
    let Some((chunk, new_offset, clipped)) = read_new_lines(path, *offset, INITIAL_INGEST_MAX)
    else {
        return Some(false);
    };
    *truncated |= clipped;
    ingest(&chunk);
    *offset = new_offset;
    Some(true)
}

/// Read the complete lines appended past `offset`. A first read (`offset ==
/// 0`) of a file larger than `cap` starts `cap` bytes from the end, dropping
/// the torn first line and reporting the clip. The returned offset points
/// just past the last complete line (a torn tail line — a live append racing
/// the read — is left for the next pass).
fn read_new_lines(path: &Path, offset: u64, cap: u64) -> Option<(String, u64, bool)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len <= offset {
        return None;
    }
    let (start, clipped) = if offset == 0 && len > cap {
        (len - cap, true)
    } else {
        (offset, false)
    };
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    let last_nl = buf.iter().rposition(|&b| b == b'\n')?;
    buf.truncate(last_nl + 1);
    let mut chunk = String::from_utf8_lossy(&buf).into_owned();
    if clipped {
        let first_nl = chunk.find('\n')?;
        chunk.drain(..=first_nl);
    }
    Some((chunk, start + last_nl as u64 + 1, clipped))
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
/// the same engine as transcripts, so folds/find/wrap come for free.
pub(super) fn section_blocks(events: &[ActivityEvent], section: Section) -> Vec<TranscriptBlock> {
    events
        .iter()
        .filter(|e| section_includes(section, e.kind))
        .cloned()
        .map(TranscriptBlock::Event)
        .collect()
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
        rows.push(CcRow::Info(format!("Edited ({})", edited.len())));
        rows.extend(edited.iter().map(|f| CcRow::Text(file_line(f))));
    }
    if !read_only.is_empty() {
        if !rows.is_empty() {
            rows.push(CcRow::Info(String::new()));
        }
        rows.push(CcRow::Info(format!("Read ({})", read_only.len())));
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

/// The Overview section: session/provider identity, metadata, per-kind
/// counts, hottest files, and the agents-tree summary. `provider` reflects
/// whether the agent is supported at all; `activity` is its accumulator once
/// a scan pass has run.
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
            rows.push(CcRow::Text(format!(
                "Agent: {agent_name} · provider {}",
                act.provider.id()
            )));
            if let Some(t) = &meta.title {
                rows.push(CcRow::Text(format!("Title: {t}")));
            }
            let mut line = String::new();
            if let Some(m) = &meta.model {
                line.push_str(&format!("model {m}  "));
            }
            if let Some(t) = meta.output_tokens {
                line.push_str(&format!("{t} output tokens"));
            }
            if !line.trim().is_empty() {
                rows.push(CcRow::Text(line.trim_end().to_string()));
            }
            let events = act.events();
            let c = ActivityCounts::tally(events);
            rows.push(CcRow::Info(String::new()));
            rows.push(CcRow::Text(format!(
                "{} actions · {} commands · {} edits · {} reads · {} web · {} subagents",
                c.total(),
                c.commands,
                c.edits,
                c.reads,
                c.web,
                c.subagents
            )));
            if let Some(ts) = events.iter().rev().find_map(|e| e.ts_ms) {
                rows.push(CcRow::Text(format!("Last action: {}", fmt_time(ts))));
            }
            if act.truncated() {
                rows.push(CcRow::Info(
                    "(long history — oldest activity clipped)".to_string(),
                ));
            }
            let files = aggregate_files(events);
            if !files.is_empty() {
                rows.push(CcRow::Info(String::new()));
                rows.push(CcRow::Info("Hottest files".to_string()));
                rows.extend(files.iter().take(5).map(|f| CcRow::Text(file_line(f))));
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
        }
    }

    #[test]
    fn provider_resolves_by_command_basename() {
        assert_eq!(
            ProviderKind::for_command("claude"),
            Some(ProviderKind::Claude)
        );
        assert_eq!(
            ProviderKind::for_command("/usr/local/bin/claude"),
            Some(ProviderKind::Claude)
        );
        assert_eq!(ProviderKind::for_command("vibe"), Some(ProviderKind::Vibe));
        assert_eq!(
            ProviderKind::for_command("codex"),
            Some(ProviderKind::Codex)
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
        let (chunk, offset, clipped) = read_new_lines(&path, 0, INITIAL_INGEST_MAX).expect("read");
        assert_eq!(chunk, "line one\nline two\n");
        assert!(!clipped);
        assert_eq!(offset, chunk.len() as u64);

        // The torn tail completes later and is picked up from the offset.
        std::fs::write(&path, "line one\nline two\npartial done\n").expect("write");
        let (chunk2, offset2, _) = read_new_lines(&path, offset, INITIAL_INGEST_MAX).expect("read");
        assert_eq!(chunk2, "partial done\n");
        assert_eq!(offset2, 31);
        // Nothing new → None.
        assert!(read_new_lines(&path, offset2, INITIAL_INGEST_MAX).is_none());
    }

    #[test]
    fn read_new_lines_caps_first_ingest_from_the_tail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("big.jsonl");
        let line = "x".repeat(99) + "\n"; // 100 bytes per line
        std::fs::write(&path, line.repeat(50)).expect("write"); // 5000 bytes
        let (chunk, offset, clipped) = read_new_lines(&path, 0, 250).expect("read");
        assert!(clipped);
        // 250-byte window from the end covers two complete lines after the
        // torn first one is dropped.
        assert_eq!(chunk.len(), 200);
        assert!(chunk.ends_with('\n'));
        assert_eq!(offset, 5000);
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
            Some("sid-1")
        ));
        assert_eq!(src.scan.events.len(), 1);
        // Unchanged file → gated, no re-ingest.
        assert!(!scan_claude(
            &mut src,
            &mut sig,
            Some(&projects),
            Some("sid-1")
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
            Some("sid-1")
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
            Some("sid-1")
        ));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "pwd");
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
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.contains("provider vibe"))));
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.contains("Title: Build it"))));
        assert!(rows
            .iter()
            .any(|r| matches!(r, CcRow::Text(s) if s.contains("1 commands"))));
    }
}
