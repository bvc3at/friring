//! Codex CLI (OpenAI) activity source — discovery + incremental tail.
//!
//! Codex writes one append-only rollout JSONL per thread under
//! `$CODEX_HOME/sessions/<YYYY>/<MM>/<DD>/rollout-<local-start>-<thread_id>.jsonl`
//! (`CODEX_HOME` defaults to `~/.codex`; it does **not** honour `XDG_*`). The
//! writer flushes after every line, so the active file tails like a Claude
//! transcript via [`super::tail_source`]. A subagent runs as its own rollout
//! whose head `session_meta` carries `parent_thread_id`.
//!
//! Discovery matches the head line's `cwd` against the session's launch dirs,
//! newest rollout first, walking only the most recent date shards so the
//! `readdir` cost stays bounded. Compressed cold rollouts (`.jsonl.zst`) are
//! skipped: only the plain, actively-appended `.jsonl` is tailable, and the
//! `zst` sibling is re-materialised before a resume appends to it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::session::activity::codex::{parse_session_meta, CodexScan, CodexSessionMeta};

/// Recent date shards (`sessions/Y/M/D/`) to walk during discovery and the
/// newest-file rebind check — wide enough to find a session idle for weeks
/// (a resumed thread keeps appending to its original shard's file) while the
/// walk stays a bounded `readdir`, not a full-tree scan.
const MAX_DAY_DIRS: usize = 45;

/// What every codex session in one scan pass asks of the same tree, answered
/// once (ADR-P15): the recent-rollout listing, and each rollout's head
/// `session_meta`.
///
/// Both are pure functions of `$CODEX_HOME/sessions`, and neither depends on
/// which session is asking — but they were computed per session. The listing
/// walks [`MAX_DAY_DIRS`] shards on *every* scan (it is also the rebind
/// trigger), and a session that has not bound yet head-parses every rollout in
/// it, every pass, forever. Shared, a pass costs one walk and at most one parse
/// per file however many codex sessions it carries.
///
/// Scoped to the pass, so there is nothing to invalidate: the next pass builds a
/// fresh one and sees whatever appeared in between.
#[derive(Default)]
pub(crate) struct CodexDiscovery {
    files: Option<Rc<[PathBuf]>>,
    heads: HashMap<PathBuf, Option<CodexSessionMeta>>,
}

impl CodexDiscovery {
    /// Rollout files under the recent date shards, newest-first.
    fn files(&mut self, root: &Path) -> Rc<[PathBuf]> {
        Rc::clone(
            self.files
                .get_or_insert_with(|| recent_rollout_files(root).into()),
        )
    }

    /// One rollout's head `session_meta`, or `None` when it has none.
    fn head(&mut self, path: &Path) -> Option<&CodexSessionMeta> {
        if !self.heads.contains_key(path) {
            self.heads.insert(path.to_path_buf(), head_meta(path));
        }
        self.heads.get(path).and_then(Option::as_ref)
    }
}

/// Codex: the session's rollout transcript, found by matching the head
/// `session_meta.cwd` (or a known thread id) against the session's launch dirs,
/// newest-first. One file per thread for its whole life — a new thread in the
/// same cwd (agent restart) appears as a newer file and rebinds.
#[derive(Default)]
pub(crate) struct CodexSource {
    pub(super) scan: CodexScan,
    file: Option<PathBuf>,
    offset: u64,
    pub(super) backfilling: bool,
    /// Newest rollout filename at the last discovery — the rebind trigger.
    newest_seen: Option<OsString>,
}

/// The Codex `sessions/` root: `$CODEX_HOME/sessions` → `~/.codex/sessions`.
/// `home_override` is the test hook, mirroring
/// [`crate::paths::vibe_sessions_dir`]'s `home_override`. Codex does not honour
/// `XDG_*`, so only `CODEX_HOME` is consulted.
pub(crate) fn codex_sessions_dir(home_override: Option<&Path>) -> Option<PathBuf> {
    let home = if let Some(p) = home_override {
        p.to_path_buf()
    } else if let Some(env) = std::env::var_os("CODEX_HOME") {
        PathBuf::from(env)
    } else {
        crate::paths::home_dir()?.join(".codex")
    };
    Some(home.join("sessions"))
}

/// Bind (or rebind) and tail a Codex rollout. Returns whether anything new was
/// ingested.
pub(crate) fn scan_codex(
    src: &mut CodexSource,
    sig: &mut u64,
    root: Option<&Path>,
    dirs: &[String],
    own_id: Option<&str>,
    disc: &mut CodexDiscovery,
) -> bool {
    let Some(root) = root else {
        return false;
    };
    let files = disc.files(root);
    // Names embed the local start time, so the listing's first entry is the
    // newest rollout across the recent shards; a change in it is the rebind
    // trigger.
    let newest = files
        .first()
        .and_then(|p| p.file_name().map(OsString::from));
    if src.file.is_none() || newest != src.newest_seen {
        let bound = discover_codex_file(&files, dirs, own_id, disc);
        let rebound = bound.is_some() && bound != src.file;
        src.newest_seen = newest;
        if rebound {
            // A newer matching rollout (agent restarted): start fresh on it.
            *src = CodexSource {
                file: bound,
                newest_seen: src.newest_seen.clone(),
                ..CodexSource::default()
            };
            *sig = 0;
        } else if src.file.is_none() {
            src.file = bound;
        }
    }
    let Some(path) = src.file.clone() else {
        return false;
    };
    super::tail_source(&path, sig, &mut src.offset, &mut src.backfilling, |chunk| {
        src.scan.ingest(chunk)
    })
    .unwrap_or_else(|| {
        // Shrank (unexpected rewrite): reset the streaming parser and re-ingest.
        src.scan = CodexScan::default();
        src.offset = 0;
        src.backfilling = false;
        super::tail_source(&path, sig, &mut src.offset, &mut src.backfilling, |chunk| {
            src.scan.ingest(chunk)
        })
        .unwrap_or(false)
    })
}

/// The newest rollout whose head `session_meta` attributes it to this session:
/// a known thread id in the filename, else a `cwd` matching one of the launch
/// dirs. Subagent rollouts (`parent_thread_id` set) are skipped so a session
/// binds to its top-level thread, not a child that happens to be newer.
fn discover_codex_file(
    files: &[PathBuf],
    dirs: &[String],
    own_id: Option<&str>,
    disc: &mut CodexDiscovery,
) -> Option<PathBuf> {
    if let Some(id) = own_id.filter(|s| !s.is_empty()) {
        // The first name match settles it either way: a second file carrying
        // the same thread id would be the same thread.
        if let Some(p) = files.iter().find(|p| file_name_contains(p, id)) {
            if disc.head(p).is_some_and(|m| m.parent_thread_id.is_none()) {
                return Some(p.clone());
            }
        }
    }
    files
        .iter()
        .find(|p| {
            disc.head(p).is_some_and(|m| {
                m.parent_thread_id.is_none()
                    && m.cwd.as_deref().is_some_and(|cwd| {
                        dirs.contains(&crate::session::activity::normalize_dir(cwd))
                    })
            })
        })
        .cloned()
}

/// Rollout files under the newest [`MAX_DAY_DIRS`] date shards, newest-first.
/// Walks years → months → days in descending order and stops once enough day
/// shards are seen, so a deep `~/.codex` never triggers a full-tree scan.
fn recent_rollout_files(root: &Path) -> Vec<PathBuf> {
    let mut day_dirs: Vec<PathBuf> = Vec::new();
    'outer: for year in child_dirs_desc(root) {
        for month in child_dirs_desc(&year) {
            for day in child_dirs_desc(&month) {
                day_dirs.push(day);
                if day_dirs.len() >= MAX_DAY_DIRS {
                    break 'outer;
                }
            }
        }
    }
    let mut files = Vec::new();
    for day in day_dirs {
        let mut in_day: Vec<PathBuf> = std::fs::read_dir(&day)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_rollout_file(p))
            .collect();
        in_day.sort();
        in_day.reverse();
        files.extend(in_day);
    }
    files
}

/// Immediate subdirectories of `dir`, sorted by name descending (newest date
/// shard first). Hidden entries are skipped.
fn child_dirs_desc(dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.reverse();
    dirs
}

fn is_rollout_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
}

fn file_name_contains(path: &Path, needle: &str) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains(needle))
}

/// Parse just the head `session_meta` line of a rollout (cheap discovery
/// probe). Returns `None` when the file is empty/unreadable or its first line
/// isn't a session head.
fn head_meta(path: &Path) -> Option<CodexSessionMeta> {
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    parse_session_meta(&first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ActionKind;

    fn meta_line(cwd: &str, id: &str, parent: Option<&str>) -> String {
        let parent = parent
            .map(|p| format!(r#","parent_thread_id":"{p}""#))
            .unwrap_or_default();
        format!(
            r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}","history_mode":"legacy"{parent}}}}}"#
        )
    }

    fn command_line(cmd: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"local_shell_call","action":{{"command":["bash","-lc","{cmd}"]}}}}}}"#
        )
    }

    fn write_rollout(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir");
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n") + "\n").expect("write");
        path
    }

    #[test]
    fn codex_sessions_dir_honours_override() {
        let dir = codex_sessions_dir(Some(Path::new("/x/home"))).expect("dir");
        assert_eq!(dir, Path::new("/x/home/sessions"));
    }

    #[test]
    fn scan_codex_discovers_ingests_and_gates_on_signature() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sessions");
        let day = root.join("2026/07/12");
        let file = write_rollout(
            &day,
            "rollout-2026-07-12T10-00-00-aaaa.jsonl",
            &[
                meta_line("/repo/a", "aaaa", None),
                command_line("cargo test"),
            ],
        );

        let dirs = vec!["/repo/a".to_string()];
        let mut src = CodexSource::default();
        let mut sig = 0u64;
        assert!(scan_codex(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            None,
            &mut CodexDiscovery::default()
        ));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "cargo test");
        assert_eq!(src.scan.session.cwd.as_deref(), Some("/repo/a"));

        // Unchanged file → gated, no re-ingest.
        assert!(!scan_codex(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            None,
            &mut CodexDiscovery::default()
        ));

        // A session in another cwd never binds.
        let mut other = CodexSource::default();
        let mut other_sig = 0u64;
        assert!(!scan_codex(
            &mut other,
            &mut other_sig,
            Some(&root),
            &["/elsewhere".to_string()],
            None,
            &mut CodexDiscovery::default(),
        ));

        // Append → incremental ingest (events grow, not reset).
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .expect("open");
        writeln!(f, "{}", command_line("ls -la")).expect("append");
        assert!(scan_codex(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            None,
            &mut CodexDiscovery::default()
        ));
        assert_eq!(src.scan.events.len(), 2);
        assert_eq!(src.scan.events[1].detail, "ls -la");

        // Shrink (rewrite) → full reset + re-ingest.
        std::fs::write(
            &file,
            meta_line("/repo/a", "aaaa", None) + "\n" + &command_line("pwd") + "\n",
        )
        .expect("rewrite");
        assert!(scan_codex(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            None,
            &mut CodexDiscovery::default()
        ));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "pwd");
    }

    #[test]
    fn scan_codex_skips_subagent_and_rebinds_to_newer_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sessions");
        let day = root.join("2026/07/12");
        // Top-level session (older) + a newer subagent in the same cwd.
        write_rollout(
            &day,
            "rollout-2026-07-12T10-00-00-parent.jsonl",
            &[meta_line("/repo/a", "parent", None), command_line("make")],
        );
        write_rollout(
            &day,
            "rollout-2026-07-12T11-00-00-child.jsonl",
            &[
                meta_line("/repo/a", "child", Some("parent")),
                command_line("grep x"),
            ],
        );

        let dirs = vec!["/repo/a".to_string()];
        let mut src = CodexSource::default();
        let mut sig = 0u64;
        // Binds to the top-level thread despite the child being newer.
        assert!(scan_codex(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            None,
            &mut CodexDiscovery::default()
        ));
        assert_eq!(src.scan.session.thread_id.as_deref(), Some("parent"));
        assert_eq!(src.scan.events[0].detail, "make");

        // A newer top-level session appears (agent restart) → rebind + fresh scan.
        let day2 = root.join("2026/07/13");
        write_rollout(
            &day2,
            "rollout-2026-07-13T09-00-00-fresh.jsonl",
            &[
                meta_line("/repo/a", "fresh", None),
                command_line("cargo build"),
            ],
        );
        assert!(scan_codex(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            None,
            &mut CodexDiscovery::default()
        ));
        assert_eq!(src.scan.session.thread_id.as_deref(), Some("fresh"));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "cargo build");
        assert_eq!(src.scan.events[0].kind, ActionKind::Command);
    }

    #[test]
    fn perf_one_pass_answers_every_session_from_one_walk() {
        // ADR-P15. Discovery is a question about the tree, not about the asking
        // session: the pass walks the shards and parses each head once, however
        // many codex sessions it carries.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sessions");
        write_rollout(
            &root.join("2026/07/12"),
            "rollout-2026-07-12T10-00-00-aaaa.jsonl",
            &[
                meta_line("/repo/a", "aaaa", None),
                command_line("cargo test"),
            ],
        );
        let dirs = vec!["/repo/a".to_string()];

        let mut pass = CodexDiscovery::default();
        let mut first = CodexSource::default();
        let mut first_sig = 0u64;
        assert!(scan_codex(
            &mut first,
            &mut first_sig,
            Some(&root),
            &dirs,
            None,
            &mut pass,
        ));

        // With the tree removed, anything a second session in the same pass
        // still resolves came from the shared listing and head — not from a
        // second walk of its own.
        std::fs::remove_dir_all(&root).expect("rm");
        let mut second = CodexSource::default();
        let mut second_sig = 0u64;
        scan_codex(
            &mut second,
            &mut second_sig,
            Some(&root),
            &dirs,
            None,
            &mut pass,
        );
        assert_eq!(second.file, first.file);

        // And the sharing never outlives its pass: a fresh one re-walks and
        // sees the tree is gone, so there is nothing to invalidate.
        let mut third = CodexSource::default();
        let mut third_sig = 0u64;
        scan_codex(
            &mut third,
            &mut third_sig,
            Some(&root),
            &dirs,
            None,
            &mut CodexDiscovery::default(),
        );
        assert_eq!(third.file, None);
    }

    #[test]
    fn scan_codex_binds_by_thread_id_when_known() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("sessions");
        let day = root.join("2026/07/12");
        write_rollout(
            &day,
            "rollout-2026-07-12T10-00-00-target.jsonl",
            &[meta_line("/other/cwd", "target", None), command_line("id")],
        );

        // cwd does not match, but the known thread id does.
        let mut src = CodexSource::default();
        let mut sig = 0u64;
        assert!(scan_codex(
            &mut src,
            &mut sig,
            Some(&root),
            &["/repo/a".to_string()],
            Some("target"),
            &mut CodexDiscovery::default(),
        ));
        assert_eq!(src.scan.events[0].detail, "id");
    }
}
