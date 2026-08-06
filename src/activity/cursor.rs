//! Cursor CLI (`cursor-agent`) activity — filesystem discovery + snapshot
//! re-parse for the agent-neutral scan.
//!
//! The pure record→event parser is [`crate::session::activity::cursor`]; this
//! module binds a session to its on-disk transcript and feeds the parser.
//!
//! Source: `<root>/projects/<sanitize(cwd)>/agent-transcripts/<chatId>/<chatId>.jsonl`,
//! where `root` is `$CURSOR_DATA_DIR` (when set) else `~/.cursor`. Cursor also
//! keeps an authoritative SQLite `store.db` under `chats/<md5(cwd)>/`, but the
//! md5 chats path is deliberately not used (friring has no md5 dependency and
//! adds none) — the sanitize-encoded transcript path suffices to find every
//! session for a known cwd. See the pure module for the metadata this trades
//! away (title comes from the transcript instead).
//!
//! Unlike the append-only Claude/Vibe transcripts, Cursor **regenerates** this
//! file as a full snapshot from its blob chain, so each change triggers a full
//! re-parse (fresh [`CursorScan`]) rather than an incremental tail — there are
//! no byte offsets to keep.

use std::path::{Path, PathBuf};

use crate::session::activity::cursor::CursorScan;

/// One Cursor session's scan state: the bound transcript and the accumulated
/// snapshot parse. A shrink or rewrite is handled by the unconditional
/// re-parse in [`scan_cursor`], so no byte offset is tracked.
#[derive(Default)]
pub(crate) struct CursorSource {
    pub(super) scan: CursorScan,
    transcript: Option<PathBuf>,
    pub(super) truncated: bool,
}

/// The Cursor CLI state root: `CURSOR_DATA_DIR` (when set & non-empty) else
/// `~/.cursor`. `home_override` is the test hook, mirroring
/// [`crate::paths::vibe_sessions_dir`]'s override param. The env is read on the
/// UI thread and the resolved root handed to the scan thread.
pub(crate) fn cursor_root(home_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = home_override {
        return Some(p.to_path_buf());
    }
    if let Some(env) = std::env::var_os("CURSOR_DATA_DIR").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(env));
    }
    crate::paths::home_dir().map(|h| h.join(".cursor"))
}

/// Bind (or rebind) and re-parse a session's Cursor transcript. Returns whether
/// the accumulated events/meta changed this pass. Mirrors `scan_claude` /
/// `scan_vibe`'s contract: a stat-signature gates the (re-)read.
pub(crate) fn scan_cursor(
    src: &mut CursorSource,
    sig: &mut u64,
    root: Option<&Path>,
    session_id: Option<&str>,
    dirs: &[String],
) -> bool {
    let Some(root) = root else {
        return false;
    };
    // Re-resolve every pass: with a pinned chatId the path is deterministic and
    // stable, and without one the newest matching transcript wins — a freshly
    // started session (new chatId, newest mtime) then takes over. Rebinding is
    // lossless here because each pass re-parses the whole snapshot anyway.
    if let Some(path) = discover_transcript(root, session_id, dirs) {
        if src.transcript.as_deref() != Some(path.as_path()) {
            src.transcript = Some(path);
            *sig = 0; // force the re-parse below
        }
    } else if src.transcript.is_none() {
        return false;
    }
    let Some(path) = src.transcript.clone() else {
        return false;
    };
    let new_sig = super::stat_signature(&[&path]);
    if new_sig == *sig {
        return false;
    }
    *sig = new_sig;
    // Full re-parse from a fresh scanner — the transcript is a regenerated
    // snapshot, not append-only, so a stale prefix must never be reused. The
    // tail-window cap keeps a months-old transcript bounded (oldest lines
    // dropped, surfaced via `truncated`).
    let mut scan = CursorScan::default();
    let mut truncated = false;
    if let Some((chunk, clipped)) = super::read_tail_window(&path, super::SNAPSHOT_INGEST_MAX) {
        truncated = clipped;
        scan.ingest(&chunk);
    }
    src.scan = scan;
    src.truncated = truncated;
    true
}

/// Resolve the transcript for a session. A known `session_id` (the Cursor
/// `chatId`) wins deterministically across all candidate cwds; otherwise the
/// newest transcript under any matching cwd's `agent-transcripts/` is used.
fn discover_transcript(root: &Path, session_id: Option<&str>, dirs: &[String]) -> Option<PathBuf> {
    if let Some(id) = session_id {
        for dir in dirs {
            let transcripts = agent_transcripts_dir(root, dir);
            let candidate = transcripts.join(id).join(format!("{id}.jsonl"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // Globally newest across all candidate dirs — a multi-repo session must
    // not bind an older transcript just because its dir sorts first.
    dirs.iter()
        .filter_map(|dir| newest_transcript(&agent_transcripts_dir(root, dir)))
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, p)| p)
}

/// `<root>/projects/<sanitize(cwd)>/agent-transcripts` for a candidate cwd.
fn agent_transcripts_dir(root: &Path, cwd: &str) -> PathBuf {
    root.join("projects")
        .join(sanitize_project_dir(cwd))
        .join("agent-transcripts")
}

/// The newest `<chatId>/<chatId>.jsonl` transcript directly under
/// `agent-transcripts`, by file mtime — chatIds are random UUIDs (not
/// time-sortable), so recency comes from the filesystem. Subagent transcripts
/// (nested in a `subagents/` subdir) are ignored: only `<name>/<name>.jsonl`
/// parent transcripts match.
fn newest_transcript(transcripts: &Path) -> Option<(std::time::SystemTime, PathBuf)> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(transcripts).ok()?.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let jsonl = entry.path().join(format!("{name}.jsonl"));
        let Ok(mtime) = std::fs::metadata(&jsonl).and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, jsonl));
        }
    }
    best
}

/// Cursor's `projects/<name>` encoding of a working directory: every
/// non-ASCII-alphanumeric char becomes `-`, runs of `-` collapse to one, and
/// leading/trailing `-` are trimmed (`/home/u/p` → `home-u-p`).
///
/// This encoding is lossy (dots/underscores/slashes all fold to `-`), so
/// friring only ever forward-encodes a known cwd to *find* a dir — never
/// decodes a dir name. It is char-based (matching Cursor's JS `String.replace`)
/// rather than byte-based like [`crate::paths::claude_project_slug`]; the two
/// agree on ASCII paths and diverge only on non-ASCII components, which is
/// acceptable for existence-checked forward lookups.
fn sanitize_project_dir(cwd: &str) -> String {
    let mut out = String::with_capacity(cwd.len());
    let mut last_dash = false;
    for c in cwd.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL_LS: &str = r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"shell","input":{"command":"ls"}}]}}"#;

    fn write_transcript(root: &Path, cwd: &str, chat: &str, body: &str) -> PathBuf {
        let dir = super::agent_transcripts_dir(root, cwd).join(chat);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!("{chat}.jsonl"));
        std::fs::write(&path, body).expect("write transcript");
        path
    }

    #[test]
    fn sanitize_project_dir_matches_cursor_encoding() {
        assert_eq!(
            sanitize_project_dir("/home/user/project"),
            "home-user-project"
        );
        assert_eq!(sanitize_project_dir("/repo/a"), "repo-a");
        assert_eq!(sanitize_project_dir("/a/b.c_d"), "a-b-c-d");
        assert_eq!(sanitize_project_dir("/repo//a/"), "repo-a");
        assert_eq!(sanitize_project_dir("///"), "");
    }

    #[test]
    fn cursor_root_honors_override_then_env() {
        assert_eq!(
            cursor_root(Some(Path::new("/x/cursor"))),
            Some(PathBuf::from("/x/cursor"))
        );

        let saved = std::env::var_os("CURSOR_DATA_DIR");
        std::env::set_var("CURSOR_DATA_DIR", "/env/cursor");
        assert_eq!(cursor_root(None), Some(PathBuf::from("/env/cursor")));
        std::env::set_var("CURSOR_DATA_DIR", "");
        // An empty override falls through to ~/.cursor rather than binding "".
        assert!(cursor_root(None).is_some_and(|p| p.ends_with(".cursor")));
        match saved {
            Some(v) => std::env::set_var("CURSOR_DATA_DIR", v),
            None => std::env::remove_var("CURSOR_DATA_DIR"),
        }
    }

    #[test]
    fn scan_cursor_discovers_by_cwd_ingests_and_gates_on_signature() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_transcript(root, "/repo/a", "chat-1111", &format!("{SHELL_LS}\n"));

        let dirs = vec!["/repo/a".to_string()];
        let mut src = CursorSource::default();
        let mut sig = 0u64;
        assert!(scan_cursor(&mut src, &mut sig, Some(root), None, &dirs));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "ls");

        // Unchanged file → gated, no re-parse.
        assert!(!scan_cursor(&mut src, &mut sig, Some(root), None, &dirs));

        // A session in another cwd never binds.
        let mut other = CursorSource::default();
        let mut other_sig = 0u64;
        assert!(!scan_cursor(
            &mut other,
            &mut other_sig,
            Some(root),
            None,
            &["/elsewhere".to_string()]
        ));
    }

    #[test]
    fn scan_cursor_reparses_on_rewrite_and_shrink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let path = write_transcript(root, "/repo/a", "chat-1111", &format!("{SHELL_LS}\n"));
        let dirs = vec!["/repo/a".to_string()];
        let mut src = CursorSource::default();
        let mut sig = 0u64;
        assert!(scan_cursor(&mut src, &mut sig, Some(root), None, &dirs));
        assert_eq!(src.scan.events.len(), 1);

        // Snapshot regenerated with an extra message → full re-parse grows.
        let read = r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"read","input":{"path":"/x"}}]}}"#;
        std::fs::write(&path, format!("{SHELL_LS}\n{read}\n")).expect("rewrite grow");
        assert!(scan_cursor(&mut src, &mut sig, Some(root), None, &dirs));
        assert_eq!(src.scan.events.len(), 2);

        // Snapshot rewritten shorter (compaction) → re-parse drops the stale
        // events rather than tailing garbage.
        let pwd = r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"shell","input":{"command":"pwd"}}]}}"#;
        std::fs::write(&path, format!("{pwd}\n")).expect("rewrite shrink");
        assert!(scan_cursor(&mut src, &mut sig, Some(root), None, &dirs));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "pwd");
    }

    #[test]
    fn scan_cursor_prefers_pinned_session_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // Two sessions in the same cwd; the pinned id must win regardless of
        // which is newest on disk.
        write_transcript(root, "/repo/a", "aaaa", &format!("{SHELL_LS}\n"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"read","input":{"path":"/y"}}]}}"#;
        write_transcript(root, "/repo/a", "bbbb", &format!("{newer}\n"));

        let dirs = vec!["/repo/a".to_string()];
        let mut src = CursorSource::default();
        let mut sig = 0u64;
        assert!(scan_cursor(
            &mut src,
            &mut sig,
            Some(root),
            Some("aaaa"),
            &dirs
        ));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "ls");
        assert!(src
            .transcript
            .as_ref()
            .unwrap()
            .ends_with("aaaa/aaaa.jsonl"));
    }

    #[test]
    fn scan_cursor_rebinds_to_newer_session_without_pinned_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_transcript(root, "/repo/a", "aaaa", &format!("{SHELL_LS}\n"));

        let dirs = vec!["/repo/a".to_string()];
        let mut src = CursorSource::default();
        let mut sig = 0u64;
        assert!(scan_cursor(&mut src, &mut sig, Some(root), None, &dirs));
        assert_eq!(src.scan.events[0].detail, "ls");

        // A newer session (agent restart, new chatId) → rebind + fresh parse.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let read = r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"read","input":{"path":"/z"}}]}}"#;
        write_transcript(root, "/repo/a", "bbbb", &format!("{read}\n"));
        assert!(scan_cursor(&mut src, &mut sig, Some(root), None, &dirs));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(
            src.scan.events[0].kind,
            crate::session::activity::ActionKind::Read
        );
        assert!(src
            .transcript
            .as_ref()
            .unwrap()
            .ends_with("bbbb/bbbb.jsonl"));
    }

    #[test]
    fn scan_cursor_clips_long_history_and_reports_truncation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // Build a transcript larger than the ingest cap so the initial read
        // clips from the tail.
        let line = format!("{SHELL_LS}\n");
        let repeats = (super::super::SNAPSHOT_INGEST_MAX as usize / line.len()) + 100;
        let body = line.repeat(repeats);
        write_transcript(root, "/repo/a", "chat-1111", &body);

        let dirs = vec!["/repo/a".to_string()];
        let mut src = CursorSource::default();
        let mut sig = 0u64;
        assert!(scan_cursor(&mut src, &mut sig, Some(root), None, &dirs));
        assert!(src.truncated);
        assert!(!src.scan.events.is_empty());
        assert!(src.scan.events.len() < repeats);
    }

    #[test]
    fn scan_cursor_no_root_and_no_match_are_noops() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut src = CursorSource::default();
        let mut sig = 0u64;
        assert!(!scan_cursor(&mut src, &mut sig, None, None, &[]));
        // Root exists but nothing on disk → unbound, no change.
        assert!(!scan_cursor(
            &mut src,
            &mut sig,
            Some(tmp.path()),
            None,
            &["/repo/a".to_string()]
        ));
        assert!(src.transcript.is_none());
    }
}
