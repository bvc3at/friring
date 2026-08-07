//! opencode (sst) activity source — read-only SQLite discovery + full re-query.
//!
//! opencode keeps all activity in one SQLite DB (`opencode.db`, WAL mode) under
//! `$XDG_DATA_HOME/opencode` (→ `~/.local/share/opencode`); see
//! [`crate::session::activity::opencode`] for the row→event mapping. Because a
//! tool part is UPSERTed in place as its state advances (a live poll can catch
//! `running`), there is no append-only offset to tail — this source uses the
//! **Replace** strategy: on any change to the DB's stat signature (the db plus
//! its `-wal`/`-shm` sidecars) it re-opens read-only and rebuilds the whole
//! event vec from the most recent matching session. Discovery matches
//! `session.directory` against the friring session's candidate cwds (newest
//! `time_updated` wins), or the opencode session id directly when known.
//!
//! The DB is opened `READ_ONLY | NO_MUTEX` with a short `busy_timeout` and
//! never written; SQLite applies the WAL so a concurrent live writer is safe
//! and recent committed rows are visible. Root resolution honors
//! `$OPENCODE_DB` / `$XDG_DATA_HOME` (mirrors
//! [`crate::paths::vibe_sessions_dir`], with a test override that bypasses env).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension, Row};

use crate::session::activity::opencode::{part_events, session_meta};
use crate::session::activity::{ActivityEvent, ActivityMeta};

/// Cap on parts read for one session: the newest this many, oldest clipped
/// (surfaced via [`OpencodeSource::truncated`]). A months-old session can hold
/// tens of thousands of parts; the cap is high enough that
/// only pathological histories clip.
const MAX_PARTS: usize = 100_000;

/// Read timeout while a live opencode writer holds the DB — matches opencode's
/// own `busy_timeout` (`packages/core/src/database/sqlite.node.ts`).
const BUSY_TIMEOUT: Duration = Duration::from_millis(2_000);

/// opencode scan state: the last rebuilt event vec + session metadata. Stateless
/// beyond that — each pass re-discovers the newest matching session and rebuilds
/// (Replace), so no bound source or byte offset is kept (the stat-signature gate
/// lives in the shared [`super::SessionActivity`]).
#[derive(Default)]
pub(crate) struct OpencodeSource {
    pub(super) events: Vec<ActivityEvent>,
    pub(super) meta: ActivityMeta,
    pub(super) truncated: bool,
}

/// Re-query the session's activity when the DB changed. `db` is the resolved
/// `opencode.db` path (see [`opencode_db_path`]); `own_id` is the opencode
/// session id when known; `dirs` are the session's normalized candidate cwds.
/// Returns whether the rebuilt view differs from the last one.
pub(crate) fn scan_opencode(
    src: &mut OpencodeSource,
    sig: &mut u64,
    db: Option<&Path>,
    own_id: Option<&str>,
    dirs: &[String],
) -> bool {
    let Some(db) = db else {
        return false;
    };
    // The WAL sidecars carry the not-yet-checkpointed rows, so they gate the
    // re-query too (a commit bumps `-wal`/`-shm` before it lands in the db).
    let wal = sidecar(db, "-wal");
    let shm = sidecar(db, "-shm");
    let new_sig = super::stat_signature(&[db, &wal, &shm]);
    if new_sig == *sig {
        return false;
    }
    *sig = new_sig;
    let Some((events, meta, truncated)) = query_session_activity(db, own_id, dirs) else {
        // The DB moved but our session isn't there yet (another project wrote,
        // or the read raced a checkpoint) — keep the last view; the next change
        // re-attempts discovery.
        return false;
    };
    let changed = events != src.events || meta != src.meta || truncated != src.truncated;
    src.events = events;
    src.meta = meta;
    src.truncated = truncated;
    changed
}

/// One `session` row's columns needed for discovery + metadata.
struct SessionRow {
    id: String,
    title: Option<String>,
    model: Option<String>,
    tokens_output: Option<i64>,
}

/// Open read-only, discover the session, and map its parts to events.
fn query_session_activity(
    db: &Path,
    own_id: Option<&str>,
    dirs: &[String],
) -> Option<(Vec<ActivityEvent>, ActivityMeta, bool)> {
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.busy_timeout(BUSY_TIMEOUT);
    let session = discover_session(&conn, own_id, dirs)?;
    let meta = session_meta(
        session.title.as_deref(),
        session.model.as_deref(),
        session.tokens_output,
    );
    let (events, truncated) = load_events(&conn, &session.id)?;
    Some((events, meta, truncated))
}

/// The session to attribute to this friring session: the explicit opencode
/// session id when it resolves, else the newest one whose `directory` is a
/// candidate cwd.
fn discover_session(
    conn: &Connection,
    own_id: Option<&str>,
    dirs: &[String],
) -> Option<SessionRow> {
    if let Some(id) = own_id.filter(|s| !s.is_empty()) {
        if let Some(row) = session_by_id(conn, id) {
            return Some(row);
        }
    }
    session_by_dirs(conn, dirs)
}

fn session_by_id(conn: &Connection, id: &str) -> Option<SessionRow> {
    conn.query_row(
        "SELECT id, title, model, tokens_output FROM session WHERE id = ?1",
        params![id],
        map_session,
    )
    .optional()
    .ok()
    .flatten()
}

/// `session.directory` is the working directory the session ran in; several
/// sessions may share a cwd across restarts, so the newest `time_updated` wins.
fn session_by_dirs(conn: &Connection, dirs: &[String]) -> Option<SessionRow> {
    if dirs.is_empty() {
        return None;
    }
    let placeholders = std::iter::repeat("?")
        .take(dirs.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id, title, model, tokens_output FROM session \
         WHERE directory IN ({placeholders}) ORDER BY time_updated DESC, id DESC LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    stmt.query_row(params_from_iter(dirs.iter()), map_session)
        .optional()
        .ok()
        .flatten()
}

fn map_session(row: &Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        title: row.get(1)?,
        model: row.get(2)?,
        tokens_output: row.get(3)?,
    })
}

/// The session's parts, mapped to events in chronological order. Part ids are
/// timestamp-encoded and lexically sortable, so `ORDER BY id` is chronological;
/// the newest [`MAX_PARTS`] are taken (querying descending) then reversed.
fn load_events(conn: &Connection, session_id: &str) -> Option<(Vec<ActivityEvent>, bool)> {
    let mut stmt = conn
        .prepare(
            "SELECT time_created, data FROM part \
             WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
        )
        .ok()?;
    let cap = (MAX_PARTS + 1) as i64;
    let rows = stmt
        .query_map(params![session_id, cap], |r| {
            Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?))
        })
        .ok()?;
    let mut newest_first: Vec<(Option<i64>, String)> = rows.flatten().collect();
    let truncated = newest_first.len() > MAX_PARTS;
    if truncated {
        newest_first.truncate(MAX_PARTS);
    }
    newest_first.reverse();
    let mut events = Vec::new();
    for (ts, data) in &newest_first {
        events.extend(part_events(data, *ts));
    }
    Some((events, truncated))
}

/// Resolve opencode's activity DB file. `data_dir_override` is the test hook
/// (the dir holding `opencode.db`) and bypasses env, mirroring
/// [`crate::paths::vibe_sessions_dir`]'s `home_override`. In production
/// `$OPENCODE_DB` wins (`:memory:` ⇒ no on-disk file; absolute ⇒ verbatim; a
/// bare name ⇒ resolved under the data dir); otherwise the DB lives under
/// `$XDG_DATA_HOME/opencode` → `~/.local/share/opencode`.
pub(crate) fn opencode_db_path(data_dir_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = data_dir_override {
        return Some(pick_db_file(dir));
    }
    if let Some(db) = std::env::var_os("OPENCODE_DB").filter(|s| !s.is_empty()) {
        if db == *std::ffi::OsStr::new(":memory:") {
            return None;
        }
        let p = PathBuf::from(&db);
        if p.is_absolute() {
            return Some(p);
        }
        return Some(opencode_data_dir()?.join(p));
    }
    Some(pick_db_file(&opencode_data_dir()?))
}

/// opencode's data dir: `$XDG_DATA_HOME/opencode` → `~/.local/share/opencode`.
fn opencode_data_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(x).join("opencode"));
    }
    crate::paths::home_dir().map(|h| h.join(".local").join("share").join("opencode"))
}

/// The canonical `opencode.db`, else the lexically-first `opencode*.db`
/// (release channels use `opencode.db`; other channels append `-<channel>`, so
/// `opencode.db` sorts before `opencode-local.db`). Falls back to the canonical
/// name even when absent so a later appearance still moves the stat signature.
fn pick_db_file(dir: &Path) -> PathBuf {
    let canonical = dir.join("opencode.db");
    if canonical.is_file() {
        return canonical;
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_opencode_db(p))
        .collect();
    candidates.sort();
    candidates.into_iter().next().unwrap_or(canonical)
}

fn is_opencode_db(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("opencode") && n.ends_with(".db"))
}

/// Append a WAL sidecar suffix to a db path (`opencode.db` → `opencode.db-wal`).
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ActionKind;

    /// A writable opencode-shaped DB (only the columns the scan reads). WAL mode
    /// mirrors the real store; dropping the returned connection checkpoints it.
    fn make_db(path: &Path) -> Connection {
        let conn = Connection::open(path).expect("open");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session (
                 id TEXT PRIMARY KEY, title TEXT, model TEXT, tokens_output INTEGER,
                 directory TEXT, parent_id TEXT, time_updated INTEGER);
             CREATE TABLE IF NOT EXISTS part (
                 id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                 time_created INTEGER, data TEXT);",
        )
        .expect("schema");
        conn
    }

    fn insert_session(conn: &Connection, id: &str, dir: &str, updated: i64) {
        conn.execute(
            "INSERT INTO session (id, title, model, tokens_output, directory, time_updated) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                "New session - 2026-07-12T03:18:02.840Z",
                r#"{"id":"claude-3-5-haiku-latest","providerID":"anthropic"}"#,
                0_i64,
                dir,
                updated
            ],
        )
        .expect("insert session");
    }

    fn insert_part(conn: &Connection, id: &str, session_id: &str, data: &str) {
        conn.execute(
            "INSERT INTO part (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
            params![id, session_id, 1_783_826_285_000_i64, data],
        )
        .expect("insert part");
    }

    fn bash_part(command: &str) -> String {
        format!(
            r#"{{"type":"tool","tool":"bash","state":{{"status":"completed","input":{{"command":"{command}"}},"output":"ok","title":"{command}","metadata":{{"exit":0}}}}}}"#
        )
    }

    #[test]
    fn discovers_by_cwd_ingests_and_gates_on_signature() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("opencode.db");
        {
            let conn = make_db(&db);
            insert_session(&conn, "ses_a", "/repo/a", 100);
            insert_part(&conn, "prt_1", "ses_a", &bash_part("ls"));
        }

        let dirs = vec!["/repo/a".to_string()];
        let mut src = OpencodeSource::default();
        let mut sig = 0u64;
        assert!(scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));
        assert_eq!(src.events.len(), 1);
        assert_eq!(src.events[0].kind, ActionKind::Command);
        assert_eq!(src.events[0].detail, "ls");
        assert!(!src.truncated);

        // Unchanged DB → gated, no re-query.
        assert!(!scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));

        // New part committed → signature moves → full re-query (Replace).
        {
            let conn = make_db(&db);
            insert_part(&conn, "prt_2", "ses_a", &bash_part("pwd"));
        }
        assert!(scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));
        assert_eq!(src.events.len(), 2);
        assert_eq!(src.events[1].detail, "pwd");
    }

    #[test]
    fn a_session_in_another_cwd_never_binds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("opencode.db");
        {
            let conn = make_db(&db);
            insert_session(&conn, "ses_a", "/repo/a", 100);
            insert_part(&conn, "prt_1", "ses_a", &bash_part("ls"));
        }
        let mut src = OpencodeSource::default();
        let mut sig = 0u64;
        assert!(!scan_opencode(
            &mut src,
            &mut sig,
            Some(&db),
            None,
            &["/elsewhere".to_string()]
        ));
        assert!(src.events.is_empty());
    }

    #[test]
    fn binds_by_explicit_session_id_over_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("opencode.db");
        {
            let conn = make_db(&db);
            // Two sessions share the cwd; the id pins the older one.
            insert_session(&conn, "ses_old", "/repo/a", 100);
            insert_part(&conn, "prt_1", "ses_old", &bash_part("old"));
            insert_session(&conn, "ses_new", "/repo/a", 200);
            insert_part(&conn, "prt_2", "ses_new", &bash_part("new"));
        }
        let dirs = vec!["/repo/a".to_string()];
        let mut src = OpencodeSource::default();
        let mut sig = 0u64;
        assert!(scan_opencode(
            &mut src,
            &mut sig,
            Some(&db),
            Some("ses_old"),
            &dirs
        ));
        assert_eq!(src.events.len(), 1);
        assert_eq!(src.events[0].detail, "old");
    }

    #[test]
    fn newest_session_wins_when_matching_by_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("opencode.db");
        {
            let conn = make_db(&db);
            insert_session(&conn, "ses_old", "/repo/a", 100);
            insert_part(&conn, "prt_1", "ses_old", &bash_part("old"));
        }
        let dirs = vec!["/repo/a".to_string()];
        let mut src = OpencodeSource::default();
        let mut sig = 0u64;
        assert!(scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));
        assert_eq!(src.events[0].detail, "old");

        // A newer session in the same cwd (agent restart) → rebind to it.
        {
            let conn = make_db(&db);
            insert_session(&conn, "ses_new", "/repo/a", 200);
            insert_part(&conn, "prt_2", "ses_new", &bash_part("new"));
        }
        assert!(scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));
        assert_eq!(src.events.len(), 1);
        assert_eq!(src.events[0].detail, "new");
    }

    #[test]
    fn rewrite_that_drops_rows_shrinks_the_view() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("opencode.db");
        {
            let conn = make_db(&db);
            insert_session(&conn, "ses_a", "/repo/a", 100);
            insert_part(&conn, "prt_1", "ses_a", &bash_part("one"));
            insert_part(&conn, "prt_2", "ses_a", &bash_part("two"));
        }
        let dirs = vec!["/repo/a".to_string()];
        let mut src = OpencodeSource::default();
        let mut sig = 0u64;
        assert!(scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));
        assert_eq!(src.events.len(), 2);

        // A compaction deletes a part; the Replace strategy reflects the smaller
        // set with no stale events.
        {
            let conn = make_db(&db);
            conn.execute("DELETE FROM part WHERE id = 'prt_1'", [])
                .expect("delete");
        }
        assert!(scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));
        assert_eq!(src.events.len(), 1);
        assert_eq!(src.events[0].detail, "two");
    }

    #[test]
    fn reads_committed_rows_while_a_writer_holds_the_db() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("opencode.db");
        // Keep the writer open (live opencode): the read-only scan must still
        // see committed rows through the WAL.
        let writer = make_db(&db);
        insert_session(&writer, "ses_a", "/repo/a", 100);
        insert_part(&writer, "prt_1", "ses_a", &bash_part("live"));

        let dirs = vec!["/repo/a".to_string()];
        let mut src = OpencodeSource::default();
        let mut sig = 0u64;
        assert!(scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));
        assert_eq!(src.events.len(), 1);
        assert_eq!(src.events[0].detail, "live");
        drop(writer);
    }

    #[test]
    fn missing_db_yields_no_events_then_gates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = tmp.path().join("opencode.db"); // never created
        let dirs = vec!["/repo/a".to_string()];
        let mut src = OpencodeSource::default();
        let mut sig = 0u64;
        assert!(!scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));
        assert!(src.events.is_empty());
        // The absent-file signature is now recorded → subsequent passes gate.
        assert!(!scan_opencode(&mut src, &mut sig, Some(&db), None, &dirs));
    }

    #[test]
    fn db_path_prefers_canonical_over_channel_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Only a channel-specific DB exists → it is chosen.
        std::fs::write(tmp.path().join("opencode-local.db"), b"").expect("write");
        assert_eq!(
            opencode_db_path(Some(tmp.path())),
            Some(tmp.path().join("opencode-local.db"))
        );
        // Once the canonical DB appears it wins.
        std::fs::write(tmp.path().join("opencode.db"), b"").expect("write");
        assert_eq!(
            opencode_db_path(Some(tmp.path())),
            Some(tmp.path().join("opencode.db"))
        );
        // An empty dir still resolves to the canonical name (so its later
        // appearance moves the stat signature).
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            opencode_db_path(Some(empty.path())),
            Some(empty.path().join("opencode.db"))
        );
    }
}
