//! Qwen Code activity source — filesystem discovery + incremental tail.
//!
//! A Qwen session is one append-only JSONL transcript at
//! `<projects-base>/<sanitizeCwd(cwd)>/chats/<session-id>.jsonl` (the daemon
//! may relocate an inactive session into the sibling `chats/archive/`). The
//! project dir is derived deterministically from a launch cwd — every
//! non-ASCII-alphanumeric character becomes `-` — so discovery computes the
//! chats dir per candidate cwd rather than scanning a global root. That key is
//! *lossy* (two cwds can collide to one project dir), so a bound transcript's
//! recorded `cwd` is confirmed against the session's launch dirs before
//! attributing it.
//!
//! The pure record→event parser lives in
//! [`crate::session::activity::qwen`]; this module is the I/O glue, mirroring
//! `super::scan_claude` (single append-only transcript, stat-signature gate,
//! byte-offset tail, full reset on a shrink/rewrite).

use std::path::{Path, PathBuf};

use crate::session::activity::qwen::{record_cwd, QwenScan};

/// Bounded prefix scanned to read a transcript's first record `cwd`: records
/// can embed large file diffs, but the first record is small.
const FIRST_RECORD_SCAN_MAX: u64 = 256 * 1024;

/// Qwen Code: the session's JSONL transcript under
/// `<projects-base>/<sanitizeCwd(cwd)>/chats/`, found by deriving the project
/// dir from the session's launch cwd and confirming the recorded `cwd`.
#[derive(Default)]
pub(super) struct QwenSource {
    pub(super) scan: QwenScan,
    transcript: Option<PathBuf>,
    offset: u64,
    pub(super) backfilling: bool,
}

/// The Qwen Code `projects/` base under the resolved runtime root:
/// `home_override` (tests) → `$QWEN_RUNTIME_DIR` (relocates only `projects/` +
/// `tmp/`) → `$QWEN_HOME` (relocates all of `~/.qwen`) → `~/.qwen`. Env access,
/// so it is resolved on the UI thread like the other provider roots.
pub(super) fn qwen_projects_dir(home_override: Option<&Path>) -> Option<PathBuf> {
    let base = if let Some(p) = home_override {
        p.to_path_buf()
    } else if let Some(env) = std::env::var_os("QWEN_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        PathBuf::from(env)
    } else if let Some(env) = std::env::var_os("QWEN_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(env)
    } else {
        crate::paths::home_dir()?.join(".qwen")
    };
    Some(base.join("projects"))
}

/// Bind (or rebind) and tail the session's Qwen transcript. Returns whether
/// anything new was ingested.
pub(super) fn scan_qwen(
    src: &mut QwenSource,
    sig: &mut u64,
    root: Option<&Path>,
    dirs: &[String],
    own_id: Option<&str>,
) -> bool {
    // The daemon MOVES an archived session from chats/ to chats/archive/; if
    // the bound file vanished, drop the binding so discovery re-finds (and
    // re-ingests) the relocated copy.
    if src.transcript.as_deref().is_some_and(|p| !p.is_file()) {
        *src = QwenSource::default();
        *sig = 0;
    }
    if src.transcript.is_none() {
        if let Some(root) = root {
            src.transcript = discover_qwen_transcript(root, dirs, own_id);
        }
    }
    let Some(path) = src.transcript.clone() else {
        return false;
    };
    super::tail_source(&path, sig, &mut src.offset, &mut src.backfilling, |chunk| {
        src.scan.ingest(chunk)
    })
    .unwrap_or_else(|| {
        // Shrunk (rewritten): reset the streaming parser and re-ingest.
        src.scan = QwenScan::default();
        src.offset = 0;
        src.backfilling = false;
        super::tail_source(&path, sig, &mut src.offset, &mut src.backfilling, |chunk| {
            src.scan.ingest(chunk)
        })
        .unwrap_or(false)
    })
}

/// The transcript to bind for a session: a known session id opens
/// `<id>.jsonl` directly (authoritative); otherwise the newest-by-mtime
/// transcript in the candidate project dirs whose recorded `cwd` matches.
fn discover_qwen_transcript(root: &Path, dirs: &[String], own_id: Option<&str>) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let chats = root.join(sanitize_cwd(dir)).join("chats");
        collect_jsonl(&chats, &mut files);
        // A retrospective scan unions live and archived sessions.
        collect_jsonl(&chats.join("archive"), &mut files);
    }
    if let Some(id) = own_id {
        if let Some(p) = files
            .iter()
            .find(|p| p.file_stem().and_then(|s| s.to_str()) == Some(id))
        {
            if session_cwd_matches(p, dirs) {
                return Some(p.clone());
            }
        }
    }
    files.sort_by_key(|p| std::cmp::Reverse(file_mtime(p)));
    files.into_iter().find(|p| session_cwd_matches(p, dirs))
}

/// `*.jsonl` files directly under `dir` (missing dir ⇒ nothing).
fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") && path.is_file() {
            out.push(path);
        }
    }
}

fn file_mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Confirm a transcript's recorded `cwd` matches one of the session's launch
/// dirs — the collision guard for the lossy sanitizeCwd project key. A
/// transcript with no readable `cwd` is accepted, since its containing dir was
/// already derived from a candidate cwd.
fn session_cwd_matches(path: &Path, dirs: &[String]) -> bool {
    match first_record_cwd(path) {
        Some(cwd) => dirs.contains(&crate::app::cc_activity::normalize_dir(&cwd)),
        None => true,
    }
}

/// The `cwd` from the first record carrying one, reading only a bounded prefix.
fn first_record_cwd(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader, Read};
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(FIRST_RECORD_SCAN_MAX));
    let mut line = String::new();
    while reader.read_line(&mut line).ok()? > 0 {
        if let Some(cwd) = record_cwd(&line) {
            return Some(cwd);
        }
        line.clear();
    }
    None
}

/// qwen-code's `sanitizeCwd`: every non-ASCII-alphanumeric character becomes
/// `-` (the path is lowercased first only on Windows). Distinct from Claude's
/// per-*byte* slug — qwen replaces per UTF-16 code unit, matching `chars()`
/// for the ASCII paths that are realistic here.
fn sanitize_cwd(cwd: &str) -> String {
    let lowered = cwd.to_lowercase();
    let source = if cfg!(windows) { lowered.as_str() } else { cwd };
    source
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ActionKind;

    /// A minimal one-command transcript for `cwd`, with the given call id.
    fn transcript(cwd: &str, command: &str, call_id: &str) -> String {
        format!(
            concat!(
                r#"{{"type":"assistant","cwd":"{cwd}","model":"qwen3-coder-plus","#,
                r#""message":{{"role":"model","parts":[{{"functionCall":"#,
                r#"{{"id":"{id}","name":"run_shell_command","args":{{"command":"{cmd}"}}}}}}]}}}}"#,
                "\n"
            ),
            cwd = cwd,
            cmd = command,
            id = call_id
        )
    }

    /// The chats dir a launch `cwd` maps to under a projects base.
    fn chats_dir(root: &Path, cwd: &str) -> PathBuf {
        root.join(sanitize_cwd(cwd)).join("chats")
    }

    #[test]
    fn projects_dir_prefers_runtime_then_home_override() {
        assert_eq!(
            qwen_projects_dir(Some(Path::new("/sandbox/.qwen"))),
            Some(PathBuf::from("/sandbox/.qwen/projects"))
        );
    }

    #[test]
    fn sanitize_cwd_dashes_non_alphanumerics() {
        assert_eq!(sanitize_cwd("/home/user/my-proj"), "-home-user-my-proj");
        assert_eq!(sanitize_cwd("/a/b.c"), "-a-b-c");
    }

    #[test]
    fn scan_qwen_discovers_by_session_id_ingests_and_gates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let cwd = "/repo/a";
        let chats = chats_dir(&root, cwd);
        std::fs::create_dir_all(&chats).expect("mkdir");
        let path = chats.join("sid-1.jsonl");
        std::fs::write(&path, transcript(cwd, "cargo test", "c1")).expect("write");

        let dirs = vec![cwd.to_string()];
        let mut src = QwenSource::default();
        let mut sig = 0u64;
        assert!(scan_qwen(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            Some("sid-1")
        ));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "cargo test");
        assert_eq!(src.transcript.as_ref(), Some(&path));

        // Unchanged file → stat-gated, no re-ingest.
        assert!(!scan_qwen(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            Some("sid-1")
        ));

        // Append → incremental ingest (events grow, not reset).
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        let read_record = r#"{"type":"assistant","cwd":"/repo/a","message":{"role":"model","parts":[{"functionCall":{"id":"c2","name":"read_file","args":{"file_path":"/repo/a/x.rs"}}}]}}"#;
        writeln!(f, "{read_record}").expect("append");
        assert!(scan_qwen(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            Some("sid-1")
        ));
        assert_eq!(src.scan.events.len(), 2);
        assert_eq!(src.scan.events[1].kind, ActionKind::Read);

        // Shrink (rewrite) → full reset + re-ingest.
        std::fs::write(&path, transcript(cwd, "pwd", "c9")).expect("rewrite");
        assert!(scan_qwen(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            Some("sid-1")
        ));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "pwd");
    }

    #[test]
    fn scan_qwen_matches_by_cwd_when_id_unknown() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let cwd = "/repo/b";
        let chats = chats_dir(&root, cwd);
        std::fs::create_dir_all(&chats).expect("mkdir");
        std::fs::write(chats.join("aaaa.jsonl"), transcript(cwd, "ls", "c1")).expect("write");

        let dirs = vec![cwd.to_string()];
        let mut src = QwenSource::default();
        let mut sig = 0u64;
        assert!(scan_qwen(&mut src, &mut sig, Some(&root), &dirs, None));
        assert_eq!(src.scan.events.len(), 1);

        // A session in another cwd never binds (project dir is derived from the
        // launch dir, so the collision-safe path simply finds nothing).
        let mut other = QwenSource::default();
        let mut other_sig = 0u64;
        assert!(!scan_qwen(
            &mut other,
            &mut other_sig,
            Some(&root),
            &["/elsewhere".to_string()],
            None
        ));
    }

    #[test]
    fn discovery_rejects_colliding_cwd_by_recorded_field() {
        // Two distinct cwds sanitize to the same project dir; only the one
        // whose recorded `cwd` matches the session's launch dir is bound.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        // `/repo-x` and `/repo/x` both sanitize to `-repo-x`.
        assert_eq!(sanitize_cwd("/repo-x"), sanitize_cwd("/repo/x"));
        let chats = chats_dir(&root, "/repo/x");
        std::fs::create_dir_all(&chats).expect("mkdir");
        std::fs::write(chats.join("wrong.jsonl"), transcript("/repo-x", "ls", "c1"))
            .expect("write");

        let dirs = vec!["/repo/x".to_string()];
        let mut src = QwenSource::default();
        let mut sig = 0u64;
        // The only transcript records the *other* cwd → no match, no bind.
        assert!(!scan_qwen(&mut src, &mut sig, Some(&root), &dirs, None));
        assert!(src.transcript.is_none());
    }

    #[test]
    fn scan_qwen_rebinds_when_archived_file_moves() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("projects");
        let cwd = "/repo/c";
        let chats = chats_dir(&root, cwd);
        let archive = chats.join("archive");
        std::fs::create_dir_all(&archive).expect("mkdir");
        let live = chats.join("sid-2.jsonl");
        std::fs::write(&live, transcript(cwd, "make", "c1")).expect("write");

        let dirs = vec![cwd.to_string()];
        let mut src = QwenSource::default();
        let mut sig = 0u64;
        assert!(scan_qwen(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            Some("sid-2")
        ));
        assert_eq!(src.transcript.as_ref(), Some(&live));

        // Daemon archives the session: move the file to chats/archive/.
        let archived = archive.join("sid-2.jsonl");
        std::fs::rename(&live, &archived).expect("archive");
        assert!(scan_qwen(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            Some("sid-2")
        ));
        assert_eq!(src.transcript.as_ref(), Some(&archived));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "make");
    }
}
