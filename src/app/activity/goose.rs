//! goose (Block) activity I/O — discovers a session's row in the global
//! `sessions.db` and re-parses its `messages` into events on each change.
//!
//! goose stores *every* project's sessions in one SQLite DB
//! (`<data>/goose/sessions/sessions.db`, WAL mode), so there is no per-cwd file
//! to tail: a session is matched by its `working_dir` column (the newest
//! `session_type='user'` row for one of the friring session's launch dirs, or an
//! exact `id` when friring knows it). The DB is opened **read-only** and, because
//! rows are rewritten in place under WAL, re-parsed in full whenever its stat
//! signature moves — the same Replace strategy the opencode provider uses, cheap
//! because the signature gate skips untouched DBs. See
//! [`crate::session::activity::goose`] for the pure record→event parsing.

use std::path::{Path, PathBuf};

use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};

use crate::session::activity::goose::{session_meta, GooseScan};
use crate::session::activity::ActivityMeta;

/// Cap on messages re-parsed per pass: a long-lived session can accumulate tens
/// of thousands of rows, so only the most recent are ingested and older turns
/// are surfaced as [`GooseSource::truncated`]. Rows are pulled newest-first then
/// restored to chronological order, so clipping drops the oldest.
const MESSAGE_CAP: i64 = 10_000;

/// goose scan state: the streaming parser (rebuilt on each re-parse), the
/// session's activity metadata, and whether older turns were clipped.
#[derive(Default)]
pub(super) struct GooseSource {
    pub(super) scan: GooseScan,
    pub(super) meta: ActivityMeta,
    pub(super) truncated: bool,
}

/// Re-parse the session's goose activity from the global DB when it changed.
/// Mirrors [`super::scan_claude`]/[`super::scan_vibe`]: a stat-signature gate,
/// then a full Replace of the accumulator (goose rewrites rows in place, so
/// there is no append-only offset to advance). Returns whether events/meta were
/// refreshed this pass.
pub(super) fn scan_goose(
    src: &mut GooseSource,
    sig: &mut u64,
    sessions_dir: Option<&Path>,
    dirs: &[String],
    own_id: Option<&str>,
) -> bool {
    let Some(sessions_dir) = sessions_dir else {
        return false;
    };
    let db = sessions_dir.join("sessions.db");
    // WAL keeps recent writes in the -wal sibling, so gate on both files.
    let wal = sessions_dir.join("sessions.db-wal");
    let new_sig = super::stat_signature(&[&db, &wal]);
    if new_sig == *sig {
        return false;
    }
    *sig = new_sig;
    let Some(parsed) = read_session(&db, dirs, own_id, MESSAGE_CAP) else {
        // DB present but no matching session yet (or a transient open race):
        // keep any prior state and wait for the next change.
        return false;
    };
    src.scan = parsed.0;
    src.meta = parsed.1;
    src.truncated = parsed.2;
    true
}

/// One read pass: resolve the target session, then load its meta + messages.
/// `None` when the DB can't be opened or no session matches.
fn read_session(
    db: &Path,
    dirs: &[String],
    own_id: Option<&str>,
    cap: i64,
) -> Option<(GooseScan, ActivityMeta, bool)> {
    if !db.is_file() {
        return None;
    }
    let conn = open_readonly(db)?;
    let session_id = resolve_session(&conn, dirs, own_id)?;
    let meta = load_meta(&conn, &session_id)?;
    let (scan, truncated) = load_messages(&conn, &session_id, cap)?;
    Some((scan, meta, truncated))
}

/// Open the DB read-only (WAL-aware): goose writes it live, so we never take a
/// write lock; a short busy timeout rides out a concurrent checkpoint.
fn open_readonly(db: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(500));
    Some(conn)
}

/// The goose session id to read: an exact `own_id` match when friring knows it,
/// else the newest `session_type='user'` row whose `working_dir` is one of the
/// session's launch dirs (an agent restart makes a newer row, so this rebinds).
fn resolve_session(conn: &Connection, dirs: &[String], own_id: Option<&str>) -> Option<String> {
    if let Some(id) = own_id {
        let found: Option<String> = conn
            .query_row(
                "SELECT id FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .ok()?;
        if found.is_some() {
            return found;
        }
    }
    if dirs.is_empty() {
        return None;
    }
    let placeholders = vec!["?"; dirs.len()].join(",");
    let sql = format!(
        "SELECT id FROM sessions \
         WHERE working_dir IN ({placeholders}) AND session_type = 'user' \
         ORDER BY created_at DESC LIMIT 1"
    );
    conn.query_row(&sql, params_from_iter(dirs.iter()), |row| row.get(0))
        .optional()
        .ok()?
}

/// The session row's activity metadata. Selects only columns present since the
/// v1.41.0 schema (never `SELECT *`, and no newer-only column like
/// `parent_session_id`), so it reads every schema version.
fn load_meta(conn: &Connection, session_id: &str) -> Option<ActivityMeta> {
    conn.query_row(
        "SELECT name, user_set_name, description, model_config_json, output_tokens \
         FROM sessions WHERE id = ?1",
        params![session_id],
        |row| {
            let name: Option<String> = row.get(0)?;
            let user_set_name: Option<i64> = row.get(1)?;
            let description: Option<String> = row.get(2)?;
            let model_config_json: Option<String> = row.get(3)?;
            let output_tokens: Option<i64> = row.get(4)?;
            Ok(session_meta(
                name.as_deref(),
                user_set_name.unwrap_or(0) != 0,
                description.as_deref(),
                model_config_json.as_deref(),
                output_tokens,
            ))
        },
    )
    .optional()
    .ok()?
}

/// The session's messages, newest-`cap` in chronological order, fed to a fresh
/// [`GooseScan`]. `truncated` when older turns were clipped by the cap.
fn load_messages(conn: &Connection, session_id: &str, cap: i64) -> Option<(GooseScan, bool)> {
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()
        .ok()?
        .unwrap_or(0);
    // Pull the newest rows (DESC) then restore chronological order so a
    // toolRequest is ingested before its paired toolResponse.
    let mut stmt = conn
        .prepare(
            "SELECT content_json, created_timestamp FROM messages \
             WHERE session_id = ?1 ORDER BY created_timestamp DESC, id DESC LIMIT ?2",
        )
        .ok()?;
    let mut rows: Vec<(String, Option<i64>)> = stmt
        .query_map(params![session_id, cap], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })
        .ok()?
        .filter_map(Result::ok)
        .collect();
    rows.reverse();
    let mut scan = GooseScan::default();
    for (content_json, ts) in rows {
        scan.ingest_message(&content_json, ts);
    }
    Some((scan, total > cap))
}

/// goose's sessions directory (which holds `sessions.db`), resolved like the
/// CLI: `path_root_override` / `$GOOSE_PATH_ROOT` reroute the whole state tree to
/// `<root>/data/sessions`; otherwise `$XDG_DATA_HOME/goose/sessions` →
/// `~/.local/share/goose/sessions`. `path_root_override` is the test hook,
/// mirroring [`crate::paths::vibe_sessions_dir`]'s `home_override`.
pub(super) fn goose_sessions_dir(path_root_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(root) = path_root_override {
        return Some(root.join("data").join("sessions"));
    }
    if let Some(root) = std::env::var_os("GOOSE_PATH_ROOT").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(root).join("data").join("sessions"));
    }
    let data_dir = if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(xdg)
    } else {
        crate::paths::home_dir()?.join(".local").join("share")
    };
    Some(data_dir.join("goose").join("sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ActionKind;

    /// Create the minimal `sessions` + `messages` schema (only the columns the
    /// provider selects) and return a writable connection.
    fn write_db(db: &Path) -> Connection {
        let conn = Connection::open(db).expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                name TEXT,
                description TEXT,
                user_set_name INTEGER,
                session_type TEXT,
                working_dir TEXT,
                created_at TEXT,
                model_config_json TEXT,
                output_tokens INTEGER
             );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                session_id TEXT,
                content_json TEXT,
                created_timestamp INTEGER
             );",
        )
        .expect("schema");
        conn
    }

    fn add_session(conn: &Connection, id: &str, working_dir: &str, created_at: &str, desc: &str) {
        conn.execute(
            "INSERT INTO sessions \
             (id, name, description, user_set_name, session_type, working_dir, created_at, \
              model_config_json, output_tokens) \
             VALUES (?1, '', ?2, 0, 'user', ?3, ?4, ?5, 2234)",
            params![
                id,
                desc,
                working_dir,
                created_at,
                r#"{"model_name":"claude-sonnet-4-20250514"}"#
            ],
        )
        .expect("insert session");
    }

    fn add_message(conn: &Connection, session_id: &str, ts: i64, content_json: &str) {
        conn.execute(
            "INSERT INTO messages (session_id, content_json, created_timestamp) \
             VALUES (?1, ?2, ?3)",
            params![session_id, content_json, ts],
        )
        .expect("insert message");
    }

    fn shell_request(id: &str, command: &str) -> String {
        format!(
            r#"[{{"type":"toolRequest","id":"{id}","toolCall":{{"status":"success","value":{{"name":"shell","arguments":{{"command":"{command}"}}}}}}}}]"#
        )
    }

    fn sessions_dir(tmp: &Path) -> PathBuf {
        let dir = tmp.join("data").join("sessions");
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn scan_goose_discovers_by_cwd_ingests_and_gates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = sessions_dir(tmp.path());
        let db = dir.join("sessions.db");
        {
            let conn = write_db(&db);
            add_session(
                &conn,
                "20260712_1",
                "/repo/a",
                "2026-07-12 14:22:01",
                "Fix auth test",
            );
            add_message(
                &conn,
                "20260712_1",
                1783826948,
                &shell_request("c1", "cargo test"),
            );
            add_message(
                &conn,
                "20260712_1",
                1783826950,
                r#"[{"type":"toolResponse","id":"c1","toolResult":{"status":"success","value":{"content":[{"type":"text","text":"ok"}],"isError":false}}}]"#,
            );
        }

        let mut src = GooseSource::default();
        let mut sig = 0u64;
        let dirs = vec!["/repo/a".to_string()];
        assert!(scan_goose(&mut src, &mut sig, Some(&dir), &dirs, None));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "cargo test");
        assert_eq!(src.scan.events[0].ok, Some(true));
        assert_eq!(src.meta.title.as_deref(), Some("Fix auth test"));
        assert_eq!(src.meta.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(src.meta.output_tokens, Some(2234));

        // Unchanged DB → gated, no re-parse.
        assert!(!scan_goose(&mut src, &mut sig, Some(&dir), &dirs, None));

        // A session in another cwd never binds.
        let mut other = GooseSource::default();
        let mut other_sig = 0u64;
        assert!(!scan_goose(
            &mut other,
            &mut other_sig,
            Some(&dir),
            &["/elsewhere".to_string()],
            None,
        ));
        assert!(other.scan.events.is_empty());
    }

    #[test]
    fn scan_goose_rebinds_to_newer_session_on_change() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = sessions_dir(tmp.path());
        let db = dir.join("sessions.db");
        {
            let conn = write_db(&db);
            add_session(
                &conn,
                "20260712_1",
                "/repo/a",
                "2026-07-12 14:00:00",
                "First",
            );
            add_message(
                &conn,
                "20260712_1",
                1783820000,
                &shell_request("c1", "make"),
            );
        }

        let mut src = GooseSource::default();
        let mut sig = 0u64;
        let dirs = vec!["/repo/a".to_string()];
        assert!(scan_goose(&mut src, &mut sig, Some(&dir), &dirs, None));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "make");

        // A newer session for the same cwd (agent restart). A large padding
        // text block guarantees the DB file grows, so the signature moves.
        {
            let conn = Connection::open(&db).expect("open");
            add_session(
                &conn,
                "20260712_2",
                "/repo/a",
                "2026-07-12 15:00:00",
                "Second",
            );
            let big = "x".repeat(5000);
            let content = format!(
                r#"[{{"type":"text","text":"{big}"}},{{"type":"toolRequest","id":"r1","toolCall":{{"status":"success","value":{{"name":"read_image","arguments":{{"path":"/repo/a/diagram.png"}}}}}}}}]"#
            );
            add_message(&conn, "20260712_2", 1783830000, &content);
        }

        assert!(scan_goose(&mut src, &mut sig, Some(&dir), &dirs, None));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].kind, ActionKind::Read);
        assert_eq!(src.scan.events[0].detail, "/repo/a/diagram.png");
        assert_eq!(src.meta.title.as_deref(), Some("Second"));
    }

    #[test]
    fn scan_goose_binds_by_explicit_session_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = sessions_dir(tmp.path());
        let db = dir.join("sessions.db");
        {
            let conn = write_db(&db);
            add_session(
                &conn,
                "sess-x",
                "/some/other/cwd",
                "2026-07-12 09:00:00",
                "Explicit",
            );
            add_message(&conn, "sess-x", 1783800000, &shell_request("c1", "pwd"));
        }

        let mut src = GooseSource::default();
        let mut sig = 0u64;
        // The cwd does not match, but the explicit id does.
        assert!(scan_goose(
            &mut src,
            &mut sig,
            Some(&dir),
            &["/nope".to_string()],
            Some("sess-x"),
        ));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "pwd");
    }

    #[test]
    fn load_messages_caps_and_flags_truncation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = sessions_dir(tmp.path());
        let db = dir.join("sessions.db");
        {
            let conn = write_db(&db);
            add_session(&conn, "s", "/repo/a", "2026-07-12 10:00:00", "Capped");
            add_message(&conn, "s", 100, &shell_request("m1", "first"));
            add_message(&conn, "s", 200, &shell_request("m2", "second"));
            add_message(&conn, "s", 300, &shell_request("m3", "third"));
        }

        let conn = open_readonly(&db).expect("open");
        // Cap below the row count → clip the oldest, keep chronological order.
        let (scan, truncated) = load_messages(&conn, "s", 2).expect("messages");
        assert!(truncated);
        let details: Vec<&str> = scan.events.iter().map(|e| e.detail.as_str()).collect();
        assert_eq!(details, vec!["second", "third"]);

        // Cap above the row count → keep everything, not truncated.
        let (scan, truncated) = load_messages(&conn, "s", 100).expect("messages");
        assert!(!truncated);
        assert_eq!(scan.events.len(), 3);
    }

    #[test]
    fn goose_sessions_dir_uses_override() {
        assert_eq!(
            goose_sessions_dir(Some(Path::new("/opt/goose-root"))),
            Some(PathBuf::from("/opt/goose-root/data/sessions"))
        );
    }

    #[test]
    fn goose_sessions_dir_honors_path_root_env() {
        // Mutate + restore the env on a spawned thread so no concurrent test
        // observes the change (mirrors `paths` tests).
        let dir = std::thread::spawn(|| {
            let saved = std::env::var_os("GOOSE_PATH_ROOT");
            std::env::set_var("GOOSE_PATH_ROOT", "/goose/root");
            let d = goose_sessions_dir(None);
            match saved {
                Some(v) => std::env::set_var("GOOSE_PATH_ROOT", v),
                None => std::env::remove_var("GOOSE_PATH_ROOT"),
            }
            d
        })
        .join()
        .expect("thread");
        assert_eq!(dir, Some(PathBuf::from("/goose/root/data/sessions")));
    }
}
