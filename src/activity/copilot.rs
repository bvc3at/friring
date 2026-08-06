//! GitHub Copilot CLI activity — filesystem discovery + incremental tail.
//!
//! Copilot keeps per-session state under `<COPILOT_HOME>/session-state/<uuid>/`
//! (default `~/.copilot`), each dir holding an append-mostly `events.jsonl`
//! transcript (parsed by [`crate::session::activity::copilot`]) plus a flat
//! `workspace.yaml` sidecar carrying the session's `cwd`, id, title, and start
//! time. State is keyed by session UUID, not a cwd path-hash, so discovery
//! reads each `workspace.yaml`'s `cwd` and binds the newest session whose
//! working directory matches one of the friring session's launch dirs (or, when
//! the agent session id is known, the `session-state/<id>/` dir directly).
//!
//! `events.jsonl` is tailed incrementally by byte offset; Copilot rewrites it
//! on compaction/rewind, so a shrink resets the streaming parser and re-ingests
//! from scratch — mirroring [`super::scan_vibe`].

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::session::activity::copilot::{parse_workspace, CopilotScan, CopilotWorkspace};
use crate::session::activity::ActivityMeta;

/// Copilot's per-session state root: `<COPILOT_HOME>/session-state` →
/// `~/.copilot/session-state`. `COPILOT_HOME` relocates the entire config+state
/// dir (the SDK's precedence is `configDir > $COPILOT_HOME > ~/.copilot`; the
/// on-disk env override is `COPILOT_HOME`). `home_override` is the test hook,
/// mirroring [`crate::paths::vibe_sessions_dir`]'s `home_override`.
pub(crate) fn copilot_sessions_dir(home_override: Option<&Path>) -> Option<PathBuf> {
    let home = if let Some(p) = home_override {
        p.to_path_buf()
    } else if let Some(env) = std::env::var_os("COPILOT_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(env)
    } else {
        crate::paths::home_dir()?.join(".copilot")
    };
    Some(home.join("session-state"))
}

/// GitHub Copilot: the `session-state/<uuid>/` dir whose `workspace.yaml`
/// working directory matches the friring session (or the id-named dir directly),
/// newest-first. `events.jsonl` tails by byte offset; `workspace.yaml` (small,
/// atomically rewritten) is re-parsed on any change for its title.
#[derive(Default)]
pub(crate) struct CopilotSource {
    pub(super) scan: CopilotScan,
    workspace: CopilotWorkspace,
    dir: Option<PathBuf>,
    offset: u64,
    pub(super) backfilling: bool,
    /// Hash of the session-state subdir names at the last discovery — the cheap
    /// rebind trigger (a new session dir appearing flips it without re-reading
    /// any `workspace.yaml`).
    roster: u64,
}

impl CopilotSource {
    /// Session metadata: the event-stream fields (model, tokens, prompt-derived
    /// title) with the `workspace.yaml` display title layered on top when the
    /// session was named (the transcript never records the title itself).
    pub(super) fn meta(&self) -> ActivityMeta {
        let mut m = self.scan.meta.clone();
        if self.workspace.title.is_some() {
            m.title = self.workspace.title.clone();
        }
        m
    }
}

/// Bind (or rebind) and tail a Copilot session dir. Returns whether anything
/// changed (new events, or a `workspace.yaml` metadata update).
pub(crate) fn scan_copilot(
    src: &mut CopilotSource,
    sig: &mut u64,
    root: Option<&Path>,
    own_id: Option<&str>,
    dirs: &[String],
) -> bool {
    let Some(root) = root else {
        return false;
    };
    let roster = roster_signature(root);
    if src.dir.is_none() || roster != src.roster {
        src.roster = roster;
        let bound = discover_copilot_dir(root, own_id, dirs);
        let rebound = bound.is_some() && bound != src.dir;
        if rebound {
            // A newer matching session (agent restarted / new conversation):
            // start fresh on it.
            *src = CopilotSource {
                dir: bound,
                roster: src.roster,
                ..CopilotSource::default()
            };
            *sig = 0;
        } else if src.dir.is_none() {
            src.dir = bound;
        }
    }
    let Some(dir) = src.dir.clone() else {
        return false;
    };
    let events = dir.join("events.jsonl");
    let workspace = dir.join("workspace.yaml");
    let new_sig = super::stat_signature(&[&events, &workspace]);
    // Keep draining a large backlog even when the signature is unchanged; the
    // inner per-call sig is fresh-zero, so only this gate guards the pass and
    // would otherwise strand the remaining chunks (see scan_vibe).
    if new_sig == *sig && !src.backfilling {
        return false;
    }
    // workspace.yaml is tiny and atomically replaced — re-parse on any change.
    if let Ok(s) = std::fs::read_to_string(&workspace) {
        src.workspace = parse_workspace(&s);
    }
    // The dir-level signature above is the real gate; the per-call one here
    // never gates (fresh zero), it only drives the offset bookkeeping.
    let mut ev_sig = 0u64;
    if super::tail_source(
        &events,
        &mut ev_sig,
        &mut src.offset,
        &mut src.backfilling,
        |chunk| src.scan.ingest(chunk),
    )
    .is_none()
    {
        // Compaction/rewind rewrote events.jsonl in full — reset + re-ingest.
        src.scan = CopilotScan::default();
        src.offset = 0;
        src.backfilling = false;
        let _ = super::tail_source(
            &events,
            &mut ev_sig,
            &mut src.offset,
            &mut src.backfilling,
            |chunk| src.scan.ingest(chunk),
        );
    }
    *sig = new_sig;
    // A workspace-only change (title/rename) counts as a change too.
    true
}

/// Hash of the visible session-state subdir names — flips when a session dir
/// appears/disappears, cheaply triggering rediscovery without reading metas.
fn roster_signature(root: &Path) -> u64 {
    let mut names: Vec<OsString> = match std::fs::read_dir(root) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name())
            .filter(|n| !n.to_string_lossy().starts_with('.'))
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    let mut h = DefaultHasher::new();
    names.hash(&mut h);
    h.finish()
}

/// Resolve the session dir: the `session-state/<sanitized-id>/` dir directly
/// when the agent session id is known, else the newest `workspace.yaml` whose
/// working directory matches one of the session's launch dirs.
fn discover_copilot_dir(root: &Path, own_id: Option<&str>, dirs: &[String]) -> Option<PathBuf> {
    if let Some(id) = own_id.filter(|s| !s.is_empty()) {
        let cand = root.join(sanitize_session_id(id));
        if cand.join("workspace.yaml").is_file() {
            return Some(cand);
        }
    }
    // cwd match, newest-first by workspace `created_at` (dir names are random
    // UUIDs, so time lives in the sidecar, not the name).
    let mut matches: Vec<(u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let dir = entry.path();
        let Ok(s) = std::fs::read_to_string(dir.join("workspace.yaml")) else {
            continue;
        };
        let ws = parse_workspace(&s);
        let Some(cwd) = ws.cwd else {
            continue;
        };
        if dirs.contains(&crate::session::activity::normalize_dir(&cwd)) {
            matches.push((ws.created_ms.unwrap_or(0), dir));
        }
    }
    matches.sort_by_key(|(ts, _)| *ts);
    matches.pop().map(|(_, dir)| dir)
}

/// Copilot sanitizes a session id into its dir name via
/// `replace(/[^A-Za-z0-9_-]/g, '')`; a UUID passes through unchanged, but
/// apply the same rule so a lookup matches whatever the CLI wrote.
fn sanitize_session_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ActionKind;

    /// Write a Copilot session dir with a `workspace.yaml` (cwd + created_at)
    /// and an `events.jsonl` body.
    fn write_session(root: &Path, name: &str, cwd: &str, created: &str, events: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("workspace.yaml"),
            format!("id: {name}\ncwd: {cwd}\ncreated_at: {created}\n"),
        )
        .expect("workspace");
        std::fs::write(dir.join("events.jsonl"), events).expect("events");
        dir
    }

    fn tool_start(call: &str, tool: &str, args: &str) -> String {
        format!(
            r#"{{"type":"tool.execution_start","data":{{"toolCallId":"{call}","toolName":"{tool}","arguments":{args}}}}}"#
        )
    }

    #[test]
    fn copilot_sessions_dir_honors_override_and_env() {
        let base = Path::new("/custom/home");
        assert_eq!(
            copilot_sessions_dir(Some(base)),
            Some(base.join("session-state"))
        );

        std::env::set_var("COPILOT_HOME", "/env/copilot");
        assert_eq!(
            copilot_sessions_dir(None),
            Some(PathBuf::from("/env/copilot/session-state"))
        );
        std::env::remove_var("COPILOT_HOME");
    }

    #[test]
    fn scan_copilot_drains_large_backfill_across_unchanged_passes() {
        // A backlog larger than INGEST_CHUNK must keep draining across passes
        // even though the file is never touched again — regression for the outer
        // signature gate stranding the tail (mirrors scan_vibe).
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("session-state");
        let line = tool_start("c1", "bash", r#"{"command":"ls"}"#) + "\n";
        let repeats = (super::super::INGEST_CHUNK as usize / line.len()) + 500;
        let dir = write_session(
            &root,
            "e57ef7a5-9452-4a54-b182-8d18f1058e94",
            "/repo/a",
            "2026-07-12T10:00:00.000Z",
            &line.repeat(repeats),
        );
        let file_len = std::fs::metadata(dir.join("events.jsonl")).unwrap().len();

        let dirs = vec!["/repo/a".to_string()];
        let mut src = CopilotSource::default();
        let mut sig = 0u64;
        assert!(scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs));
        assert!(src.backfilling, "one pass cannot drain a >8 MiB backlog");
        assert!(src.offset < file_len);

        let mut passes = 0;
        while src.backfilling && passes < 8 {
            scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs);
            passes += 1;
        }
        assert!(!src.backfilling, "the outer gate must not strand the tail");
        assert_eq!(src.offset, file_len, "every byte was eventually ingested");
        assert_eq!(src.scan.events.len(), repeats);
    }

    #[test]
    fn scan_copilot_binds_by_cwd_ingests_and_gates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("session-state");
        let dir = write_session(
            &root,
            "e57ef7a5-9452-4a54-b182-8d18f1058e94",
            "/repo/a",
            "2026-07-12T10:00:00.000Z",
            &(tool_start("c1", "bash", r#"{"command":"cargo test"}"#) + "\n"),
        );

        let dirs = vec!["/repo/a".to_string()];
        let mut src = CopilotSource::default();
        let mut sig = 0u64;
        assert!(scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.workspace.cwd.as_deref(), Some("/repo/a"));

        // Unchanged files → gated, no re-ingest.
        assert!(!scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs));

        // Append → incremental ingest (events grow, not reset).
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join("events.jsonl"))
            .expect("open");
        use std::io::Write as _;
        writeln!(
            f,
            "{}",
            tool_start("c2", "read", r#"{"path":"/repo/a/x.rs"}"#)
        )
        .expect("append");
        assert!(scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs));
        assert_eq!(src.scan.events.len(), 2);
        assert_eq!(src.scan.events[1].kind, ActionKind::Read);

        // A session in another cwd never binds.
        let mut other = CopilotSource::default();
        let mut other_sig = 0u64;
        assert!(!scan_copilot(
            &mut other,
            &mut other_sig,
            Some(&root),
            None,
            &["/elsewhere".to_string()],
        ));
        assert!(other.scan.events.is_empty());
    }

    #[test]
    fn scan_copilot_rebinds_to_newer_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("session-state");
        write_session(
            &root,
            "aaaaaaaa-0000-0000-0000-000000000000",
            "/repo/a",
            "2026-07-12T10:00:00.000Z",
            &(tool_start("c1", "bash", r#"{"command":"ls"}"#) + "\n"),
        );

        let dirs = vec!["/repo/a".to_string()];
        let mut src = CopilotSource::default();
        let mut sig = 0u64;
        assert!(scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs));
        assert_eq!(src.scan.events[0].detail, "ls");

        // A newer matching session appears → rebind + fresh scan.
        write_session(
            &root,
            "bbbbbbbb-1111-1111-1111-111111111111",
            "/repo/a",
            "2026-07-12T11:00:00.000Z",
            &(tool_start("c9", "bash", r#"{"command":"pwd"}"#) + "\n"),
        );
        assert!(scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "pwd");
    }

    #[test]
    fn scan_copilot_resets_on_rewrite() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("session-state");
        let dir = write_session(
            &root,
            "cccccccc-2222-2222-2222-222222222222",
            "/repo/b",
            "2026-07-12T10:00:00.000Z",
            &([
                tool_start("c1", "bash", r#"{"command":"one"}"#),
                tool_start("c2", "bash", r#"{"command":"two"}"#),
            ]
            .join("\n")
                + "\n"),
        );
        let dirs = vec!["/repo/b".to_string()];
        let mut src = CopilotSource::default();
        let mut sig = 0u64;
        assert!(scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs));
        assert_eq!(src.scan.events.len(), 2);

        // Compaction rewrites the file smaller → reset + re-ingest from scratch.
        std::fs::write(
            dir.join("events.jsonl"),
            tool_start("c3", "bash", r#"{"command":"fresh"}"#) + "\n",
        )
        .expect("rewrite");
        assert!(scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "fresh");
    }

    #[test]
    fn scan_copilot_binds_directly_by_session_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("session-state");
        // cwd does NOT match the launch dirs — only the id path finds it.
        let id = "dddddddd-3333-3333-3333-333333333333";
        write_session(
            &root,
            id,
            "/some/other/cwd",
            "2026-07-12T10:00:00.000Z",
            &(tool_start("c1", "web_fetch", r#"{"url":"https://docs.rs"}"#) + "\n"),
        );

        let mut src = CopilotSource::default();
        let mut sig = 0u64;
        assert!(scan_copilot(&mut src, &mut sig, Some(&root), Some(id), &[]));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].kind, ActionKind::WebFetch);
    }

    #[test]
    fn scan_copilot_workspace_title_overrides_meta() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("session-state");
        let dir = root.join("eeeeeeee-4444-4444-4444-444444444444");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("workspace.yaml"),
            "id: e\ncwd: /repo/c\nname: Fix the retry logic\ncreated_at: 2026-07-12T10:00:00.000Z\n",
        )
        .expect("workspace");
        std::fs::write(
            dir.join("events.jsonl"),
            [
                r#"{"type":"user.message","data":{"content":"do the thing"}}"#.to_string(),
                tool_start("c1", "bash", r#"{"command":"make"}"#),
            ]
            .join("\n")
                + "\n",
        )
        .expect("events");

        let dirs = vec!["/repo/c".to_string()];
        let mut src = CopilotSource::default();
        let mut sig = 0u64;
        assert!(scan_copilot(&mut src, &mut sig, Some(&root), None, &dirs));
        // workspace `name` wins over the prompt-derived title.
        assert_eq!(src.meta().title.as_deref(), Some("Fix the retry logic"));
    }
}
