//! Cline (standalone CLI / cline-core) filesystem glue for the activity scan.
//!
//! Discovery + ingest for the flat session store at
//! `<root>/<sid>/` (`<sid> = <13-digit-ms>_<nanoid>`). Unlike the append-only
//! JSONL providers, cline rewrites `<sid>.messages.json` *in full* on every
//! persist, so there is no byte-offset tail: on any stat change the whole file
//! is re-parsed and the event stream **replaced** (see
//! [`crate::session::activity::cline`]). The sibling `<sid>.json` manifest holds
//! the session metadata (title/model/tokens) and the `cwd`/`workspace_root`
//! used to attribute a flat-store session to a friring launch dir.
//!
//! Sessions are stored flat (not keyed by cwd), so binding mirrors the Vibe
//! provider: the ms-epoch dir-name prefix sorts by recency, and the newest dir
//! whose manifest `cwd`/`workspace_root` matches a launch dir wins — with a
//! rebind when a newer matching session appears (agent restart).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::session::activity::cline::{parse_manifest, parse_messages, ClineMeta};
use crate::session::activity::ActivityEvent;

use super::{newest_session_dir, stat_signature};

/// One cline session's scan state: the bound session dir, its full-rewrite
/// event stream (replaced each re-parse), and the manifest-derived metadata.
#[derive(Default)]
pub(super) struct ClineSource {
    pub(super) events: Vec<ActivityEvent>,
    pub(super) meta: ClineMeta,
    /// A single JSON object is always read whole, so history is never clipped —
    /// kept for parity with the tailing providers' accessor.
    pub(super) truncated: bool,
    dir: Option<PathBuf>,
    /// Newest session-dir name at the last discovery — the rebind trigger.
    newest_seen: Option<OsString>,
}

/// The cline standalone CLI's per-session store. Resolution mirrors the CLI's
/// own `paths.ts`: `override` → `$CLINE_SESSION_DATA_DIR` (points *at* the
/// sessions dir) → `$CLINE_DATA_DIR/sessions` → `$CLINE_DIR/data/sessions` →
/// `~/.cline/data/sessions`. `override` is the test hook, mirroring
/// [`crate::paths::vibe_sessions_dir`]'s `home_override`.
pub(super) fn cline_sessions_dir(override_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = override_dir {
        return Some(p.to_path_buf());
    }
    if let Some(env) = std::env::var_os("CLINE_SESSION_DATA_DIR") {
        return Some(PathBuf::from(env));
    }
    if let Some(env) = std::env::var_os("CLINE_DATA_DIR") {
        return Some(PathBuf::from(env).join("sessions"));
    }
    if let Some(env) = std::env::var_os("CLINE_DIR") {
        return Some(PathBuf::from(env).join("data").join("sessions"));
    }
    crate::paths::home_dir().map(|h| h.join(".cline").join("data").join("sessions"))
}

/// Bind (or rebind) and re-parse a cline session. Returns whether anything was
/// re-read this pass.
pub(super) fn scan_cline(
    src: &mut ClineSource,
    sig: &mut u64,
    root: Option<&Path>,
    dirs: &[String],
    own_id: Option<&str>,
) -> bool {
    let Some(root) = root else {
        return false;
    };
    let newest = newest_session_dir(root);
    if src.dir.is_none() || newest != src.newest_seen {
        let bound = discover_cline_dir(root, dirs, own_id);
        let rebound = bound.is_some() && bound != src.dir;
        src.newest_seen = newest;
        if rebound {
            // A newer matching session (agent restarted): start fresh on it.
            *src = ClineSource {
                dir: bound,
                newest_seen: src.newest_seen.clone(),
                ..ClineSource::default()
            };
            *sig = 0;
        } else if src.dir.is_none() {
            src.dir = bound;
        }
    }
    let Some(dir) = src.dir.clone() else {
        return false;
    };
    // The dir name is the sessionId; its files are `<sid>.json` (manifest) and
    // `<sid>.messages.json` (activity).
    let Some(sid) = dir.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let manifest = dir.join(format!("{sid}.json"));
    let messages = dir.join(format!("{sid}.messages.json"));
    let new_sig = stat_signature(&[&manifest, &messages]);
    if new_sig == *sig {
        return false;
    }
    // A completed rewrite always advances mtime, so recording the signature now
    // (even if the messages read races a half-written file) still recovers next
    // pass; a torn parse below keeps the prior events rather than clearing them.
    *sig = new_sig;
    if let Ok(s) = std::fs::read_to_string(&manifest) {
        src.meta = parse_manifest(&s);
    }
    if let Ok(s) = std::fs::read_to_string(&messages) {
        if let Some(events) = parse_messages(&s) {
            src.events = events;
        }
    }
    true
}

/// Locate the session dir for a friring session. Prefers an exact bind when the
/// agent's session id is known; otherwise the newest flat-store dir whose
/// manifest `cwd`/`workspace_root` matches a normalized launch dir. Walks
/// newest-first (ms-epoch prefix ⇒ lexical order is chronological), so only a
/// few manifests are parsed.
fn discover_cline_dir(root: &Path, dirs: &[String], own_id: Option<&str>) -> Option<PathBuf> {
    if let Some(id) = own_id {
        let dir = root.join(id);
        if dir.join(format!("{id}.json")).is_file() {
            return Some(dir);
        }
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    entries.sort();
    for dir in entries.into_iter().rev() {
        let Some(sid) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(s) = std::fs::read_to_string(dir.join(format!("{sid}.json"))) else {
            continue;
        };
        let m = parse_manifest(&s);
        for key in [m.cwd.as_deref(), m.workspace_root.as_deref()]
            .into_iter()
            .flatten()
        {
            if dirs.contains(&crate::app::cc_activity::normalize_dir(key)) {
                return Some(dir);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ActionKind;

    /// Write a `<sid>/` session (manifest + messages) under `root`.
    fn write_session(root: &Path, sid: &str, cwd: &str, messages: &str) -> PathBuf {
        let dir = root.join(sid);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join(format!("{sid}.json")),
            format!(
                r#"{{"version":1,"session_id":"{sid}","cwd":"{cwd}","workspace_root":"{cwd}","model":"claude-opus-4","metadata":{{"title":"T"}}}}"#
            ),
        )
        .expect("manifest");
        std::fs::write(dir.join(format!("{sid}.messages.json")), messages).expect("messages");
        dir
    }

    fn msgs(inner: &str) -> String {
        format!(r#"{{"version":1,"agent":"lead","sessionId":"s","messages":[{inner}]}}"#)
    }

    fn run_cmd(id: &str, cmd: &str) -> String {
        msgs(&format!(
            r#"{{"role":"assistant","content":[{{"type":"tool_use","id":"{id}","name":"run_commands","input":{{"commands":["{cmd}"]}}}}]}}"#
        ))
    }

    #[test]
    fn cline_sessions_dir_honors_override_and_env_precedence() {
        assert_eq!(
            cline_sessions_dir(Some(Path::new("/x/sessions"))),
            Some(PathBuf::from("/x/sessions"))
        );
        // Env precedence, isolated from any inherited CLINE_* vars.
        for k in ["CLINE_SESSION_DATA_DIR", "CLINE_DATA_DIR", "CLINE_DIR"] {
            std::env::remove_var(k);
        }
        std::env::set_var("CLINE_DATA_DIR", "/data");
        assert_eq!(
            cline_sessions_dir(None),
            Some(PathBuf::from("/data/sessions"))
        );
        std::env::set_var("CLINE_SESSION_DATA_DIR", "/direct");
        assert_eq!(cline_sessions_dir(None), Some(PathBuf::from("/direct")));
        for k in ["CLINE_SESSION_DATA_DIR", "CLINE_DATA_DIR"] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn scan_cline_discovers_ingests_and_gates_on_signature() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let dir = write_session(root, "1752300000000_aaaaa", "/repo/a", &run_cmd("t1", "ls"));

        let dirs = vec!["/repo/a".to_string()];
        let mut src = ClineSource::default();
        let mut sig = 0u64;
        assert!(scan_cline(&mut src, &mut sig, Some(root), &dirs, None));
        assert_eq!(src.events.len(), 1);
        assert_eq!(src.events[0].detail, "ls");
        assert_eq!(src.meta.session_id.as_deref(), Some("1752300000000_aaaaa"));
        assert_eq!(src.meta.meta.model.as_deref(), Some("claude-opus-4"));

        // Unchanged files → gated, no re-parse.
        assert!(!scan_cline(&mut src, &mut sig, Some(root), &dirs, None));

        // A session in another cwd never binds.
        let mut other = ClineSource::default();
        let mut other_sig = 0u64;
        assert!(!scan_cline(
            &mut other,
            &mut other_sig,
            Some(root),
            &["/elsewhere".to_string()],
            None
        ));

        // Full rewrite (two new commands) → events replaced, not appended.
        std::fs::write(
            dir.join("1752300000000_aaaaa.messages.json"),
            msgs(r#"{"role":"assistant","content":[
                {"type":"tool_use","id":"t9","name":"run_commands","input":{"commands":["pwd"]}},
                {"type":"tool_use","id":"t10","name":"read_files","input":{"files":[{"path":"/repo/a/x.rs"}]}}
            ]}"#),
        )
        .expect("rewrite");
        assert!(scan_cline(&mut src, &mut sig, Some(root), &dirs, None));
        assert_eq!(src.events.len(), 2);
        assert_eq!(src.events[0].detail, "pwd");
        assert_eq!(src.events[1].kind, ActionKind::Read);
    }

    #[test]
    fn scan_cline_rebinds_to_newer_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write_session(root, "1752300000000_aaaaa", "/repo/a", &run_cmd("t1", "ls"));

        let dirs = vec!["/repo/a".to_string()];
        let mut src = ClineSource::default();
        let mut sig = 0u64;
        assert!(scan_cline(&mut src, &mut sig, Some(root), &dirs, None));
        assert_eq!(src.events[0].detail, "ls");
        assert_eq!(src.meta.session_id.as_deref(), Some("1752300000000_aaaaa"));

        // A newer matching dir appears (agent restart) → rebind + fresh scan.
        write_session(
            root,
            "1752300009999_bbbbb",
            "/repo/a",
            &run_cmd("t2", "cargo build"),
        );
        assert!(scan_cline(&mut src, &mut sig, Some(root), &dirs, None));
        assert_eq!(src.meta.session_id.as_deref(), Some("1752300009999_bbbbb"));
        assert_eq!(src.events.len(), 1);
        assert_eq!(src.events[0].detail, "cargo build");
    }

    #[test]
    fn scan_cline_binds_exact_session_id_when_known() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        // Two sessions in the same cwd; own_id pins the older, non-newest one.
        write_session(
            root,
            "1752300000000_aaaaa",
            "/repo/a",
            &run_cmd("t1", "old"),
        );
        write_session(
            root,
            "1752300009999_bbbbb",
            "/repo/a",
            &run_cmd("t2", "new"),
        );

        let mut src = ClineSource::default();
        let mut sig = 0u64;
        assert!(scan_cline(
            &mut src,
            &mut sig,
            Some(root),
            &[],
            Some("1752300000000_aaaaa")
        ));
        assert_eq!(src.events[0].detail, "old");
    }

    #[test]
    fn scan_cline_recovers_from_a_torn_read_then_full_rewrite() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let dir = write_session(root, "1752300000000_aaaaa", "/repo/a", &run_cmd("t1", "ls"));
        let dirs = vec!["/repo/a".to_string()];
        let mut src = ClineSource::default();
        let mut sig = 0u64;
        assert!(scan_cline(&mut src, &mut sig, Some(root), &dirs, None));
        assert_eq!(src.events.len(), 1);

        // A half-written messages file (unparseable) keeps the prior events.
        let messages = dir.join("1752300000000_aaaaa.messages.json");
        std::fs::write(&messages, r#"{"version":1,"messages":[{"role":"#).expect("torn");
        scan_cline(&mut src, &mut sig, Some(root), &dirs, None);
        assert_eq!(src.events.len(), 1);
        assert_eq!(src.events[0].detail, "ls");

        // The writer completes → events refresh on the next pass.
        std::fs::write(&messages, run_cmd("t2", "make")).expect("complete");
        assert!(scan_cline(&mut src, &mut sig, Some(root), &dirs, None));
        assert_eq!(src.events[0].detail, "make");
    }
}
