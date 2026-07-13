//! Gemini CLI filesystem glue: discover a session's chat transcript and tail
//! it into a [`GeminiScan`].
//!
//! Layout (Gemini home = `$GEMINI_CLI_HOME/.gemini` or `~/.gemini`):
//!
//! - `projects.json` — `{"projects":{"<abs cwd>":"<slug>"}}`. The cwd → slug
//!   lookup; slugs collide-suffix (`-1`/`-2`), so friring always *reads* the
//!   map rather than reconstructing a slug.
//! - `tmp/<slug>/chats/session-<ts>-<id8>.jsonl` — append-only transcripts
//!   (the [`GeminiScan`] source). The filename embeds the ISO start time, so
//!   lexical order is recency; newest wins, and a newer file (agent restart)
//!   triggers a rebind + fresh scan, mirroring `scan_vibe`.
//!
//! Only the current `session-*.jsonl` streaming format is tailed; the legacy
//! monolithic `<sessionId>.json` store was rewritten in full each turn and is
//! not safely tailable, so it is ignored.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::session::activity::gemini::GeminiScan;

use super::tail_source;

/// Resolve the Gemini home directory: `home_override` (test hook) →
/// `$GEMINI_CLI_HOME/.gemini` → `~/.gemini`. Mirrors
/// [`crate::paths::vibe_sessions_dir`]'s style; the env var relocates the whole
/// home (a hardcoded `.gemini` is always appended). Resolved on the UI thread
/// (env access) and passed to the scan thread.
pub(in crate::app) fn gemini_root(home_override: Option<&Path>) -> Option<PathBuf> {
    let root = if let Some(p) = home_override {
        p.to_path_buf()
    } else if let Some(env) = std::env::var_os("GEMINI_CLI_HOME") {
        PathBuf::from(env).join(".gemini")
    } else {
        crate::paths::home_dir()?.join(".gemini")
    };
    Some(root)
}

/// Gemini scan state: the resolved `chats/` dir (once `projects.json` maps the
/// cwd), the bound session file, its tail offset, and the newest file name
/// seen at the last discovery (the rebind trigger).
#[derive(Default)]
pub(in crate::app) struct GeminiSource {
    pub(super) scan: GeminiScan,
    chats_dir: Option<PathBuf>,
    file: Option<PathBuf>,
    offset: u64,
    pub(super) truncated: bool,
    newest_seen: Option<OsString>,
}

/// Bind (or rebind) and tail a Gemini chat transcript. Returns whether
/// anything new was ingested.
pub(in crate::app) fn scan_gemini(
    src: &mut GeminiSource,
    sig: &mut u64,
    root: Option<&Path>,
    dirs: &[String],
    own_id: Option<&str>,
) -> bool {
    let Some(root) = root else {
        return false;
    };
    // `projects.json` may not map the cwd yet (session created after friring
    // started), so keep retrying resolution until the chats dir appears.
    if src.chats_dir.is_none() {
        src.chats_dir = resolve_chats_dir(root, dirs);
    }
    let Some(chats) = src.chats_dir.clone() else {
        return false;
    };

    let newest = newest_session_file(&chats);
    if src.file.is_none() || newest != src.newest_seen {
        let bound = discover_session_file(&chats, own_id);
        let rebound = bound.is_some() && bound != src.file;
        src.newest_seen = newest;
        if rebound {
            // A newer matching session (agent restarted): start fresh on it,
            // keeping the resolved chats dir.
            *src = GeminiSource {
                chats_dir: Some(chats.clone()),
                file: bound,
                newest_seen: src.newest_seen.clone(),
                ..GeminiSource::default()
            };
            *sig = 0;
        } else if src.file.is_none() {
            src.file = bound;
        }
    }
    let Some(file) = src.file.clone() else {
        return false;
    };
    tail_source(&file, sig, &mut src.offset, &mut src.truncated, |chunk| {
        src.scan.ingest(chunk)
    })
    .unwrap_or_else(|| {
        // Shrunk (rewritten): reset the streaming parser and re-ingest.
        src.scan = GeminiScan::default();
        src.offset = 0;
        src.truncated = false;
        tail_source(&file, sig, &mut src.offset, &mut src.truncated, |chunk| {
            src.scan.ingest(chunk)
        })
        .unwrap_or(false)
    })
}

/// Resolve `tmp/<slug>/chats` for one of the session's launch dirs via
/// `projects.json`. `None` until the map contains a matching cwd and the dir
/// exists.
fn resolve_chats_dir(root: &Path, dirs: &[String]) -> Option<PathBuf> {
    let projects_json = std::fs::read_to_string(root.join("projects.json")).ok()?;
    let slug = slug_for_dirs(&projects_json, dirs)?;
    let chats = root.join("tmp").join(slug).join("chats");
    chats.is_dir().then_some(chats)
}

/// The slug `projects.json` maps one of `dirs` to. Keys are absolute project
/// paths (normalized to match [`super::super::cc_activity::normalize_dir`]'s
/// trailing-slash trim on `dirs`).
fn slug_for_dirs(projects_json: &str, dirs: &[String]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(projects_json).ok()?;
    let map = v.get("projects")?.as_object()?;
    for (path, slug) in map {
        if dirs.contains(&super::super::cc_activity::normalize_dir(path)) {
            return slug.as_str().map(String::from);
        }
    }
    None
}

/// Newest (lexically greatest) `session-*.jsonl` file name in a chats dir — the
/// rebind trigger.
fn newest_session_file(chats: &Path) -> Option<OsString> {
    std::fs::read_dir(chats)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name())
        .filter(|n| is_session_file(n))
        .max()
}

/// Pick the session file to tail: the one whose name carries the known agent
/// session id's 8-char prefix (`session-<ts>-<id8>.jsonl`), else the newest.
fn discover_session_file(chats: &Path, own_id: Option<&str>) -> Option<PathBuf> {
    let mut files: Vec<OsString> = std::fs::read_dir(chats)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.file_name())
        .filter(|n| is_session_file(n))
        .collect();
    files.sort();
    if let Some(id8) = own_id
        .map(|s| s.chars().take(8).collect::<String>())
        .filter(|s| !s.is_empty())
    {
        let suffix = format!("-{id8}.jsonl");
        if let Some(m) = files
            .iter()
            .rev()
            .find(|n| n.to_string_lossy().ends_with(&suffix))
        {
            return Some(chats.join(m));
        }
    }
    files.into_iter().max().map(|n| chats.join(n))
}

fn is_session_file(name: &OsStr) -> bool {
    let n = name.to_string_lossy();
    n.starts_with("session-") && n.ends_with(".jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chats dir under `root/tmp/<slug>/chats` with `projects.json` mapping
    /// `cwd → slug`. Returns the chats dir.
    fn setup_project(root: &Path, cwd: &str, slug: &str) -> PathBuf {
        std::fs::write(
            root.join("projects.json"),
            format!(r#"{{"projects":{{"{cwd}":"{slug}"}}}}"#),
        )
        .expect("projects.json");
        let chats = root.join("tmp").join(slug).join("chats");
        std::fs::create_dir_all(&chats).expect("mkdir chats");
        chats
    }

    fn session_line(session_id: &str, cwd: &str) -> String {
        format!(
            r#"{{"sessionId":"{session_id}","projectHash":"h","startTime":"2026-07-12T14:03:11.482Z","kind":"main","directories":["{cwd}"]}}"#,
        )
    }

    fn shell_line(id: &str, command: &str) -> String {
        format!(
            r#"{{"id":"{id}","type":"gemini","toolCalls":[{{"id":"c-{id}","name":"run_shell_command","args":{{"command":"{command}"}},"status":"success"}}]}}"#,
        )
    }

    #[test]
    fn root_honors_override_and_env() {
        assert_eq!(
            gemini_root(Some(Path::new("/tmp/x/.gemini"))),
            Some(PathBuf::from("/tmp/x/.gemini"))
        );
        // Env var relocates the home; `.gemini` is always appended. Run on a
        // scratch thread so the mutation never leaks to other tests.
        let got = std::thread::spawn(|| {
            std::env::set_var("GEMINI_CLI_HOME", "/custom/home");
            let r = gemini_root(None);
            std::env::remove_var("GEMINI_CLI_HOME");
            r
        })
        .join()
        .unwrap();
        assert_eq!(got, Some(PathBuf::from("/custom/home/.gemini")));
    }

    #[test]
    fn scan_discovers_ingests_and_gates_on_signature() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(".gemini");
        std::fs::create_dir_all(&root).expect("mkdir root");
        let chats = setup_project(&root, "/repo/a", "myproj");
        let file = chats.join("session-2026-07-12T14-03-aabbccdd.jsonl");
        std::fs::write(
            &file,
            format!(
                "{}\n{}\n",
                session_line("aabbccdd-1111", "/repo/a"),
                shell_line("g1", "cargo test")
            ),
        )
        .expect("write");

        let dirs = vec!["/repo/a".to_string()];
        let mut src = GeminiSource::default();
        let mut sig = 0u64;
        assert!(scan_gemini(&mut src, &mut sig, Some(&root), &dirs, None));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "cargo test");
        assert_eq!(src.scan.session_id.as_deref(), Some("aabbccdd-1111"));

        // Unchanged file → gated, no re-ingest.
        assert!(!scan_gemini(&mut src, &mut sig, Some(&root), &dirs, None));

        // Append → incremental ingest (events grow, not reset).
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .expect("open");
        writeln!(f, "{}", shell_line("g2", "cargo build")).expect("append");
        assert!(scan_gemini(&mut src, &mut sig, Some(&root), &dirs, None));
        assert_eq!(src.scan.events.len(), 2);

        // Shrink (rewrite) → full reset + re-ingest.
        std::fs::write(
            &file,
            format!(
                "{}\n{}\n",
                session_line("aabbccdd-1111", "/repo/a"),
                shell_line("g9", "pwd")
            ),
        )
        .expect("rewrite");
        assert!(scan_gemini(&mut src, &mut sig, Some(&root), &dirs, None));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "pwd");
    }

    #[test]
    fn scan_ignores_unmapped_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(".gemini");
        std::fs::create_dir_all(&root).expect("mkdir root");
        let chats = setup_project(&root, "/repo/a", "myproj");
        std::fs::write(
            chats.join("session-2026-07-12T14-03-aabbccdd.jsonl"),
            format!("{}\n", shell_line("g1", "ls")),
        )
        .expect("write");

        // A session whose cwd is not in projects.json never binds.
        let mut src = GeminiSource::default();
        let mut sig = 0u64;
        assert!(!scan_gemini(
            &mut src,
            &mut sig,
            Some(&root),
            &["/elsewhere".to_string()],
            None
        ));
        assert!(src.scan.events.is_empty());
    }

    #[test]
    fn scan_rebinds_to_a_newer_session_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(".gemini");
        std::fs::create_dir_all(&root).expect("mkdir root");
        let chats = setup_project(&root, "/repo/a", "myproj");
        std::fs::write(
            chats.join("session-2026-07-12T10-00-aaaaaaaa.jsonl"),
            format!(
                "{}\n{}\n",
                session_line("aaaaaaaa-1", "/repo/a"),
                shell_line("g1", "ls")
            ),
        )
        .expect("old");

        let dirs = vec!["/repo/a".to_string()];
        let mut src = GeminiSource::default();
        let mut sig = 0u64;
        assert!(scan_gemini(&mut src, &mut sig, Some(&root), &dirs, None));
        assert_eq!(src.scan.session_id.as_deref(), Some("aaaaaaaa-1"));
        assert_eq!(src.scan.events[0].detail, "ls");

        // A newer file (lexically greater name → later start time) appears.
        std::fs::write(
            chats.join("session-2026-07-12T11-00-bbbbbbbb.jsonl"),
            format!(
                "{}\n{}\n",
                session_line("bbbbbbbb-2", "/repo/a"),
                shell_line("g2", "cargo test")
            ),
        )
        .expect("new");
        assert!(scan_gemini(&mut src, &mut sig, Some(&root), &dirs, None));
        assert_eq!(src.scan.session_id.as_deref(), Some("bbbbbbbb-2"));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "cargo test");
    }

    #[test]
    fn scan_prefers_the_session_matching_the_known_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(".gemini");
        std::fs::create_dir_all(&root).expect("mkdir root");
        let chats = setup_project(&root, "/repo/a", "myproj");
        // Newer file by name, but a different session id.
        std::fs::write(
            chats.join("session-2026-07-12T11-00-bbbbbbbb.jsonl"),
            format!(
                "{}\n{}\n",
                session_line("bbbbbbbb-2", "/repo/a"),
                shell_line("g2", "newer")
            ),
        )
        .expect("newer");
        std::fs::write(
            chats.join("session-2026-07-12T10-00-aaaaaaaa.jsonl"),
            format!(
                "{}\n{}\n",
                session_line("aaaaaaaa-1", "/repo/a"),
                shell_line("g1", "wanted")
            ),
        )
        .expect("wanted");

        let dirs = vec!["/repo/a".to_string()];
        let mut src = GeminiSource::default();
        let mut sig = 0u64;
        // own_id's 8-char prefix (`aaaaaaaa`) selects the older file.
        assert!(scan_gemini(
            &mut src,
            &mut sig,
            Some(&root),
            &dirs,
            Some("aaaaaaaa-1111-2222")
        ));
        assert_eq!(src.scan.events[0].detail, "wanted");
    }
}
