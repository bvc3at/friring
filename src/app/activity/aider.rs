//! Aider activity source — filesystem discovery + append-only tailing of a
//! repo's `.aider.chat.history.md`, feeding [`AiderScan`].
//!
//! Aider has no on-disk session id: it appends every run in a project to one
//! per-repo transcript at `<git_root|cwd>/.aider.chat.history.md` (env override
//! `AIDER_CHAT_HISTORY_FILE` / `--chat-history-file`). So a source is bound the
//! way `crush`'s is — from the session's candidate launch dirs — rather than by
//! a session id: the newest `.aider.chat.history.md` among the candidate dirs,
//! unless the global override pins one explicit file. The file is append-only,
//! so growth is tailed incrementally by byte offset (a rewrite/shrink resets
//! the parser and re-ingests), mirroring [`super::scan_claude`].

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::session::activity::aider::AiderScan;

/// The per-repo transcript aider appends to, relative to the repo/cwd.
const HISTORY_FILE: &str = ".aider.chat.history.md";

/// Aider: the session's `.aider.chat.history.md`, bound from the candidate
/// launch dirs (or the `AIDER_CHAT_HISTORY_FILE` override) and tailed as an
/// append-only source.
#[derive(Default)]
pub(super) struct AiderSource {
    pub(super) scan: AiderScan,
    path: Option<PathBuf>,
    offset: u64,
    pub(super) truncated: bool,
}

/// Resolve the `AIDER_CHAT_HISTORY_FILE` override (a single explicit transcript
/// path). `None` when unset — there is no global default file; the default is
/// the per-repo `.aider.chat.history.md`, resolved from candidate dirs at scan
/// time. `env_override` is the test hook, mirroring
/// [`crate::paths::vibe_sessions_dir`]'s override style (env read on the UI
/// thread, path consumed on the scan thread).
pub(super) fn aider_history_override(env_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = env_override {
        return Some(p.to_path_buf());
    }
    std::env::var_os("AIDER_CHAT_HISTORY_FILE").map(PathBuf::from)
}

/// Bind (once) and tail the session's aider transcript. Returns whether
/// anything new was ingested.
pub(super) fn scan_aider(
    src: &mut AiderSource,
    sig: &mut u64,
    override_path: Option<&Path>,
    dirs: &[String],
) -> bool {
    if src.path.is_none() {
        src.path = discover_history(override_path, dirs);
    }
    let Some(path) = src.path.clone() else {
        return false;
    };
    super::tail_source(&path, sig, &mut src.offset, &mut src.truncated, |chunk| {
        src.scan.ingest(chunk)
    })
    .unwrap_or_else(|| {
        // Shrunk (rewritten/rotated): reset the streaming parser and re-ingest.
        src.scan = AiderScan::default();
        src.offset = 0;
        src.truncated = false;
        super::tail_source(&path, sig, &mut src.offset, &mut src.truncated, |chunk| {
            src.scan.ingest(chunk)
        })
        .unwrap_or(false)
    })
}

/// The transcript to read: the explicit override when set, else the newest
/// `.aider.chat.history.md` among the session's candidate dirs (aider writes it
/// at the git root / cwd, which is one of those dirs).
fn discover_history(override_path: Option<&Path>, dirs: &[String]) -> Option<PathBuf> {
    if let Some(p) = override_path {
        return p.is_file().then(|| p.to_path_buf());
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for dir in dirs {
        let candidate = Path::new(dir).join(HISTORY_FILE);
        let Ok(md) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !md.is_file() {
            continue;
        }
        let mtime = md.modified().unwrap_or(UNIX_EPOCH);
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, candidate));
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "\n# aider chat started at 2026-07-12 03:16:28\n\n\
                          > Main model: gpt-4o with diff edit format  \n";

    #[test]
    fn override_wins_over_candidate_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let explicit = tmp.path().join("session-scoped.md");
        std::fs::write(&explicit, HEADER).expect("write");

        // Even with a matching candidate dir file present, the override pins
        // the explicit path.
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(HISTORY_FILE), HEADER).expect("write");

        assert_eq!(
            aider_history_override(Some(&explicit)),
            Some(explicit.clone())
        );
        let got = discover_history(Some(&explicit), &[dir.to_string_lossy().into_owned()]);
        assert_eq!(got, Some(explicit));
        // A missing override path resolves to nothing.
        assert_eq!(discover_history(Some(Path::new("/no/such.md")), &[]), None);
    }

    #[test]
    fn discovers_newest_among_candidate_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let older = tmp.path().join("a");
        let newer = tmp.path().join("b");
        std::fs::create_dir_all(&older).expect("mkdir");
        std::fs::create_dir_all(&newer).expect("mkdir");
        std::fs::write(older.join(HISTORY_FILE), HEADER).expect("write");
        std::fs::write(newer.join(HISTORY_FILE), HEADER).expect("write");
        // Force `newer` to have the later mtime (deterministic, no sleep).
        std::fs::File::open(newer.join(HISTORY_FILE))
            .expect("open")
            .set_modified(SystemTime::now() + std::time::Duration::from_secs(60))
            .expect("set mtime");

        let dirs = vec![
            older.to_string_lossy().into_owned(),
            newer.to_string_lossy().into_owned(),
        ];
        assert_eq!(
            discover_history(None, &dirs),
            Some(newer.join(HISTORY_FILE))
        );
    }

    #[test]
    fn scan_discovers_ingests_gates_appends_and_resets_on_shrink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let history = dir.join(HISTORY_FILE);
        std::fs::write(
            &history,
            format!("{HEADER}#### /run cargo test\n> Applied edit to app.py  \n"),
        )
        .expect("write");

        let dirs = vec![dir.to_string_lossy().into_owned()];
        let mut src = AiderSource::default();
        let mut sig = 0u64;
        assert!(scan_aider(&mut src, &mut sig, None, &dirs));
        assert_eq!(src.scan.events.len(), 2);
        assert_eq!(src.scan.meta.model.as_deref(), Some("gpt-4o"));

        // Unchanged file → stat-gated, no re-ingest.
        assert!(!scan_aider(&mut src, &mut sig, None, &dirs));

        // Append → incremental ingest (events grow, not reset).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&history)
            .expect("open");
        use std::io::Write as _;
        writeln!(f, "#### !ls -la").expect("append");
        assert!(scan_aider(&mut src, &mut sig, None, &dirs));
        assert_eq!(src.scan.events.len(), 3);
        assert_eq!(src.scan.events[2].detail, "ls -la");

        // Shrink (a full rewrite) → parser reset + re-ingest from scratch.
        std::fs::write(&history, format!("{HEADER}#### /run pwd\n")).expect("rewrite");
        assert!(scan_aider(&mut src, &mut sig, None, &dirs));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "pwd");
    }

    #[test]
    fn scan_without_a_matching_source_is_inert() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut src = AiderSource::default();
        let mut sig = 0u64;
        // A candidate dir with no transcript never binds.
        assert!(!scan_aider(
            &mut src,
            &mut sig,
            None,
            &[tmp.path().to_string_lossy().into_owned()]
        ));
        assert!(src.path.is_none());
        assert!(src.scan.events.is_empty());
    }
}
