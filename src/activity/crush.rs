//! Crush (Charmbracelet) filesystem glue — discover a session's per-project
//! `crush.db`, read it read-only, and (re)build its [`CrushScan`] each changed
//! pass.
//!
//! Unlike the append-only JSONL providers, Crush persists to a per-project
//! SQLite DB at `<cwd>/.crush/crush.db` (WAL, debounced writes), shared by every
//! Crush session in that project. There is no byte offset to tail, so the
//! strategy is **Replace**: a stat-signature over the DB plus its `-wal`/`-shm`
//! sidecars gates re-reads, and on any change the chosen session is re-parsed
//! from scratch (the pure [`CrushScan`] rebuilds cleanly, so a delete/rewrite
//! needs no special reset — the fresh parse simply reflects the new rows).
//!
//! The DB stores no cwd and one file holds all sessions, so discovery walks up
//! from each candidate launch dir for the closest existing `.crush` (mirroring
//! Crush's own `LookupClosestBounded` resolution) and picks the newest
//! top-level session — or the friring `agent_session_id` when it names one.
//! That is the Vibe newest-dir rule, adapted to rows.
//!
//! The DB is opened normally read-only (never `immutable=1`) so SQLite applies
//! the `-wal`, exposing the newest un-checkpointed writes; `immutable=1` would
//! pin an older, pre-WAL snapshot. Crush has no per-project data-dir env var
//! (unlike Vibe's `VIBE_HOME`); its only relocation hooks are the `--data-dir`
//! flag and the `data_directory` config key, threaded here as
//! `data_dir_override` (also the test seam).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OpenFlags};

use crate::session::activity::crush::{session_meta, CrushScan};

use super::stat_signature;

/// Cap on messages read for one session, newest kept. A very long session
/// clips oldest history (surfaced via [`CrushSource::truncated`]) so the
/// in-memory event stream stays bounded, high enough that only pathological
/// histories clip (the JSONL providers backfill in full instead).
const MAX_MESSAGES: usize = 50_000;

/// Crush: the session's bound per-project `crush.db` and the [`CrushScan`]
/// rebuilt from it each changed pass.
#[derive(Default)]
pub(crate) struct CrushSource {
    pub(super) scan: CrushScan,
    /// The bound `crush.db` (sticky once discovered — the project doesn't move).
    db: Option<PathBuf>,
    pub(super) truncated: bool,
}

/// Bind (or reuse) and re-read a Crush `crush.db`. Returns whether the parse
/// changed. Mirrors [`super::scan_vibe`]'s contract: a stat-signature gate over
/// the DB + sidecars, then a full re-parse (Replace) on any change.
pub(crate) fn scan_crush(
    src: &mut CrushSource,
    sig: &mut u64,
    dirs: &[String],
    own_id: Option<&str>,
    data_dir_override: Option<&Path>,
) -> bool {
    if src.db.is_none() {
        src.db = discover_crush_db(dirs, data_dir_override);
    }
    let Some(db) = src.db.clone() else {
        return false;
    };
    // The `-wal` holds the newest un-checkpointed writes, so both sidecars are
    // part of the change signature (and are applied on read — see read_crush).
    let wal = sidecar(&db, "-wal");
    let shm = sidecar(&db, "-shm");
    let new_sig = stat_signature(&[db.as_path(), wal.as_path(), shm.as_path()]);
    if new_sig == *sig {
        return false;
    }
    // Only commit the signature on a successful read so a transient
    // mid-write/locked pass (or a not-yet-created session) is retried.
    let Some((scan, truncated)) = read_crush(&db, own_id) else {
        return false;
    };
    *sig = new_sig;
    src.scan = scan;
    src.truncated = truncated;
    true
}

/// The newest-`crush.db` reachable from the candidate launch dirs. Each dir
/// resolves via [`crush_db_path`]; among the resolved DBs the most recently
/// modified wins (the project the session is actually working in).
fn discover_crush_db(dirs: &[String], data_dir_override: Option<&Path>) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for dir in dirs {
        let Some(db) = crush_db_path(Path::new(dir), data_dir_override) else {
            continue;
        };
        let mtime = std::fs::metadata(&db)
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        let replace = match &best {
            None => true,
            Some((_, best_mtime)) => mtime >= *best_mtime,
        };
        if replace {
            best = Some((db, mtime));
        }
    }
    best.map(|(db, _)| db)
}

/// The `crush.db` for a working directory. With an override (the `--data-dir`
/// flag / `data_directory` config key), the DB is `<override>/crush.db`.
/// Otherwise walk up from `cwd` for the closest existing `.crush/crush.db`,
/// matching Crush's own resolution.
fn crush_db_path(cwd: &Path, data_dir_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = data_dir_override {
        let db = dir.join("crush.db");
        return db.is_file().then_some(db);
    }
    for ancestor in cwd.ancestors() {
        let db = ancestor.join(".crush").join("crush.db");
        if db.is_file() {
            return Some(db);
        }
    }
    None
}

/// `crush.db` + `-wal`/`-shm` sidecar path.
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    db.with_file_name(name)
}

/// One `sessions` row's display fields.
struct SessionRow {
    id: String,
    title: Option<String>,
    completion_tokens: Option<i64>,
}

/// One `messages` row's parse inputs.
struct MessageRow {
    parts: String,
    model: Option<String>,
    ts_ms: Option<u64>,
}

/// Open the DB read-only, pick the session, and fold its messages into a fresh
/// [`CrushScan`]. `None` on any DB/read error or when the DB has no session yet.
fn read_crush(db: &Path, own_id: Option<&str>) -> Option<(CrushScan, bool)> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let session = pick_session(&conn, own_id)?;
    let mut scan = CrushScan::default();
    scan.meta = session_meta(session.title.as_deref(), session.completion_tokens);
    let (messages, truncated) = read_messages(&conn, &session.id)?;
    for m in messages {
        scan.ingest_message(&m.parts, m.model.as_deref(), m.ts_ms);
    }
    Some((scan, truncated))
}

/// The friring session's own id when it names a real top-level session, else
/// the newest top-level session (Crush stores no cwd and one DB holds every
/// session, so newest-by-`updated_at` is "the" session for this cwd). Columns
/// are resolved dynamically so an older, additively-migrated schema still reads.
fn pick_session(conn: &Connection, own_id: Option<&str>) -> Option<SessionRow> {
    let cols = table_columns(conn, "sessions");
    if !cols.contains("id") {
        return None;
    }
    let title = if cols.contains("title") {
        "title"
    } else {
        "NULL"
    };
    let tokens = if cols.contains("completion_tokens") {
        "completion_tokens"
    } else {
        "NULL"
    };
    let top_level = if cols.contains("parent_session_id") {
        "parent_session_id IS NULL"
    } else {
        "1 = 1"
    };
    let order = if cols.contains("updated_at") {
        "updated_at DESC, rowid DESC"
    } else if cols.contains("created_at") {
        "created_at DESC, rowid DESC"
    } else {
        "rowid DESC"
    };

    if let Some(id) = own_id {
        let sql = format!(
            "SELECT id, {title}, {tokens} FROM sessions WHERE id = ?1 AND {top_level} LIMIT 1"
        );
        if let Ok(row) = conn.query_row(&sql, params![id], map_session_row) {
            return Some(row);
        }
    }
    // `title-<id>` helper sessions can also be parentless — exclude them.
    let sql = format!(
        "SELECT id, {title}, {tokens} FROM sessions \
         WHERE {top_level} AND id NOT LIKE 'title-%' ORDER BY {order} LIMIT 1"
    );
    conn.query_row(&sql, [], map_session_row).ok()
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        title: row
            .get::<_, Option<String>>(1)?
            .filter(|t| !t.trim().is_empty()),
        completion_tokens: row.get(2)?,
    })
}

/// The chosen session's messages, chronological, newest [`MAX_MESSAGES`] kept
/// (with a truncation flag). `parts` is required; `model`/`created_at` are
/// resolved dynamically and default absent on an older schema.
fn read_messages(conn: &Connection, session_id: &str) -> Option<(Vec<MessageRow>, bool)> {
    let cols = table_columns(conn, "messages");
    if !cols.contains("parts") {
        return None;
    }
    let model = if cols.contains("model") {
        "model"
    } else {
        "NULL"
    };
    let created = if cols.contains("created_at") {
        "created_at"
    } else {
        "NULL"
    };
    let order = if cols.contains("created_at") {
        "created_at DESC, rowid DESC"
    } else {
        "rowid DESC"
    };
    let sql = format!(
        "SELECT parts, {model}, {created} FROM messages \
         WHERE session_id = ?1 ORDER BY {order} LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let limit = MAX_MESSAGES as i64 + 1;
    let mapped = stmt
        .query_map(params![session_id, limit], |row| {
            Ok(MessageRow {
                parts: row.get(0)?,
                model: row.get::<_, Option<String>>(1)?.filter(|m| !m.is_empty()),
                ts_ms: to_ms(row.get::<_, Option<i64>>(2)?),
            })
        })
        .ok()?;
    let mut newest: Vec<MessageRow> = mapped.flatten().collect();
    let truncated = newest.len() > MAX_MESSAGES;
    newest.truncate(MAX_MESSAGES);
    // Selected newest-first for the LIMIT clip; ingest wants chronological.
    newest.reverse();
    Some((newest, truncated))
}

/// The column names of `table`, via `PRAGMA table_info`. Empty on any error.
fn table_columns(conn: &Connection, table: &str) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return set;
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(1)) else {
        return set;
    };
    for name in rows.flatten() {
        set.insert(name);
    }
    set
}

/// Unix seconds → epoch ms, dropping non-positive/absent values.
fn to_ms(seconds: Option<i64>) -> Option<u64> {
    seconds
        .filter(|&s| s > 0)
        .and_then(|s| u64::try_from(s).ok())
        .map(|s| s * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ActionKind;

    fn encode(input: &str) -> String {
        serde_json::to_string(input).expect("encode input")
    }

    fn tool_call(id: &str, name: &str, input: &str) -> String {
        format!(
            r#"{{"type":"tool_call","data":{{"id":"{id}","name":"{name}","input":{},"finished":true}}}}"#,
            encode(input)
        )
    }

    fn parts(elems: &[&str]) -> String {
        format!("[{}]", elems.join(","))
    }

    fn create_schema(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, parent_session_id TEXT, title TEXT,
                 message_count INTEGER, prompt_tokens INTEGER, completion_tokens INTEGER,
                 cost REAL, updated_at INTEGER, created_at INTEGER,
                 summary_message_id TEXT, todos TEXT);
             CREATE TABLE messages (
                 id TEXT PRIMARY KEY, session_id TEXT, role TEXT, parts TEXT,
                 model TEXT, provider TEXT, created_at INTEGER, updated_at INTEGER);",
        )
        .expect("schema");
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_session(
        conn: &Connection,
        id: &str,
        parent: Option<&str>,
        title: &str,
        completion_tokens: i64,
        updated_at: i64,
        created_at: i64,
    ) {
        conn.execute(
            "INSERT INTO sessions
                 (id, parent_session_id, title, completion_tokens, updated_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, parent, title, completion_tokens, updated_at, created_at],
        )
        .expect("insert session");
    }

    fn insert_message(conn: &Connection, id: &str, session_id: &str, parts: &str, created_at: i64) {
        conn.execute(
            "INSERT INTO messages (id, session_id, role, parts, model, created_at)
             VALUES (?1, ?2, 'assistant', ?3, 'anthropic/x', ?4)",
            params![id, session_id, parts, created_at],
        )
        .expect("insert message");
    }

    /// Advance wall-clock enough that the next commit moves the DB mtime, so the
    /// stat-signature gate reliably sees a change (SQLite may reuse pages, so
    /// file length alone is not a dependable signal in-test).
    fn tick() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    fn write_db<F: FnOnce(&Connection)>(db: &Path, f: F) {
        let conn = Connection::open(db).expect("open rw");
        f(&conn);
    }

    #[test]
    fn scan_crush_discovers_reads_and_gates_on_signature() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(cwd.join(".crush")).expect("mkdir");
        let db = cwd.join(".crush").join("crush.db");
        write_db(&db, |conn| {
            create_schema(conn);
            insert_session(conn, "s1", None, "Build it", 3200, 100, 90);
            insert_message(
                conn,
                "m1",
                "s1",
                &parts(&[&tool_call("t1", "bash", r#"{"command":"ls"}"#)]),
                110,
            );
        });

        let dirs = vec![cwd.to_string_lossy().to_string()];
        let mut src = CrushSource::default();
        let mut sig = 0u64;
        assert!(scan_crush(&mut src, &mut sig, &dirs, None, None));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "ls");
        assert_eq!(src.scan.events[0].ts_ms, Some(110_000));
        assert_eq!(src.scan.meta.title.as_deref(), Some("Build it"));
        assert_eq!(src.scan.meta.output_tokens, Some(3200));
        assert_eq!(src.scan.meta.model.as_deref(), Some("anthropic/x"));

        // Unchanged DB → gated, no re-parse.
        assert!(!scan_crush(&mut src, &mut sig, &dirs, None, None));

        // New rows (a result patches t1, a new call appends) → re-parse.
        tick();
        write_db(&db, |conn| {
            conn.execute(
                "INSERT INTO messages (id, session_id, role, parts, created_at)
                 VALUES ('m2', 's1', 'tool', ?1, 120)",
                params![parts(&[
                    r#"{"type":"tool_result","data":{"tool_call_id":"t1","content":"a.rs","is_error":false}}"#,
                ])],
            )
            .expect("result");
            insert_message(
                conn,
                "m3",
                "s1",
                &parts(&[&tool_call("t2", "view", r#"{"file_path":"/x"}"#)]),
                130,
            );
        });
        assert!(scan_crush(&mut src, &mut sig, &dirs, None, None));
        assert_eq!(src.scan.events.len(), 2);
        assert_eq!(src.scan.events[0].ok, Some(true)); // patched by the result
        assert_eq!(src.scan.events[1].kind, ActionKind::Read);
    }

    #[test]
    fn scan_crush_replaces_on_rewrite_and_follows_newest_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(cwd.join(".crush")).expect("mkdir");
        let db = cwd.join(".crush").join("crush.db");
        write_db(&db, |conn| {
            create_schema(conn);
            insert_session(conn, "s1", None, "First", 10, 100, 90);
            insert_message(
                conn,
                "m1",
                "s1",
                &parts(&[&tool_call("t1", "bash", r#"{"command":"ls"}"#)]),
                110,
            );
        });

        let dirs = vec![cwd.to_string_lossy().to_string()];
        let mut src = CrushSource::default();
        let mut sig = 0u64;
        assert!(scan_crush(&mut src, &mut sig, &dirs, None, None));
        assert_eq!(src.scan.meta.title.as_deref(), Some("First"));

        // A newer top-level session appears (a fresh `crush` run) → follow it.
        tick();
        write_db(&db, |conn| {
            insert_session(conn, "s2", None, "Second", 20, 200, 190);
            insert_message(
                conn,
                "m2",
                "s2",
                &parts(&[&tool_call("t2", "view", r#"{"file_path":"/y"}"#)]),
                210,
            );
        });
        assert!(scan_crush(&mut src, &mut sig, &dirs, None, None));
        assert_eq!(src.scan.meta.title.as_deref(), Some("Second"));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].kind, ActionKind::Read);

        // Shrink: the newest session's rows are deleted (rewind/compact) →
        // Replace reflects the empty stream without a stale-offset artifact.
        tick();
        write_db(&db, |conn| {
            conn.execute("DELETE FROM messages WHERE session_id = 's2'", [])
                .expect("delete");
        });
        assert!(scan_crush(&mut src, &mut sig, &dirs, None, None));
        assert_eq!(src.scan.meta.title.as_deref(), Some("Second"));
        assert!(src.scan.events.is_empty());
    }

    #[test]
    fn session_id_hint_selects_matching_top_level_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(cwd.join(".crush")).expect("mkdir");
        let db = cwd.join(".crush").join("crush.db");
        write_db(&db, |conn| {
            create_schema(conn);
            // s_new is newer, but the hint pins s_old.
            insert_session(conn, "s_old", None, "Old", 1, 100, 90);
            insert_session(conn, "s_new", None, "New", 1, 200, 190);
            insert_message(
                conn,
                "m1",
                "s_old",
                &parts(&[&tool_call("t1", "bash", r#"{"command":"pwd"}"#)]),
                110,
            );
        });

        let dirs = vec![cwd.to_string_lossy().to_string()];
        let mut src = CrushSource::default();
        let mut sig = 0u64;
        assert!(scan_crush(&mut src, &mut sig, &dirs, Some("s_old"), None));
        assert_eq!(src.scan.meta.title.as_deref(), Some("Old"));
        assert_eq!(src.scan.events[0].detail, "pwd");
    }

    #[test]
    fn discovers_via_override_and_walks_up_to_closest_crush() {
        // Override points straight at a data dir with no `.crush` component.
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("custom-data");
        std::fs::create_dir_all(&data).expect("mkdir");
        let db = data.join("crush.db");
        write_db(&db, |conn| {
            create_schema(conn);
            insert_session(conn, "s1", None, "Overridden", 1, 100, 90);
            insert_message(
                conn,
                "m1",
                "s1",
                &parts(&[&tool_call("t1", "bash", r#"{"command":"id"}"#)]),
                110,
            );
        });
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("mkdir");
        let dirs = vec![cwd.to_string_lossy().to_string()];
        let mut src = CrushSource::default();
        let mut sig = 0u64;
        assert!(scan_crush(&mut src, &mut sig, &dirs, None, Some(&data)));
        assert_eq!(src.scan.meta.title.as_deref(), Some("Overridden"));

        // Walk-up: the `.crush` lives at a monorepo root above the launch dir.
        let root = tmp.path().join("mono");
        let sub = root.join("packages").join("app");
        std::fs::create_dir_all(sub.join("nested")).expect("mkdir");
        std::fs::create_dir_all(root.join(".crush")).expect("mkdir");
        let root_db = root.join(".crush").join("crush.db");
        write_db(&root_db, |conn| {
            create_schema(conn);
            insert_session(conn, "s1", None, "Monorepo", 1, 100, 90);
            insert_message(
                conn,
                "m1",
                "s1",
                &parts(&[&tool_call("t1", "bash", r#"{"command":"go build"}"#)]),
                110,
            );
        });
        let dirs = vec![sub.join("nested").to_string_lossy().to_string()];
        let mut src = CrushSource::default();
        let mut sig = 0u64;
        assert!(scan_crush(&mut src, &mut sig, &dirs, None, None));
        assert_eq!(src.db.as_deref(), Some(root_db.as_path()));
        assert_eq!(src.scan.meta.title.as_deref(), Some("Monorepo"));
    }

    #[test]
    fn reads_minimal_schema_without_model_or_timestamps() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(cwd.join(".crush")).expect("mkdir");
        let db = cwd.join(".crush").join("crush.db");
        write_db(&db, |conn| {
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT);
                 CREATE TABLE messages (id TEXT PRIMARY KEY, session_id TEXT, parts TEXT);",
            )
            .expect("minimal schema");
            conn.execute(
                "INSERT INTO sessions (id, title) VALUES ('s1', 'Legacy')",
                [],
            )
            .expect("session");
            conn.execute(
                "INSERT INTO messages (id, session_id, parts) VALUES ('m1', 's1', ?1)",
                params![parts(&[&tool_call("t1", "bash", r#"{"command":"ls"}"#)])],
            )
            .expect("message");
        });

        let dirs = vec![cwd.to_string_lossy().to_string()];
        let mut src = CrushSource::default();
        let mut sig = 0u64;
        assert!(scan_crush(&mut src, &mut sig, &dirs, None, None));
        assert_eq!(src.scan.meta.title.as_deref(), Some("Legacy"));
        assert_eq!(src.scan.events.len(), 1);
        assert_eq!(src.scan.events[0].detail, "ls");
        assert_eq!(src.scan.events[0].ts_ms, None); // no created_at column
        assert_eq!(src.scan.meta.model, None); // no model column
    }

    #[test]
    fn missing_db_yields_no_events_and_no_signature() {
        let dirs = vec!["/nonexistent/dir".to_string()];
        let mut src = CrushSource::default();
        let mut sig = 0u64;
        assert!(!scan_crush(&mut src, &mut sig, &dirs, None, None));
        assert!(src.scan.events.is_empty());
        assert_eq!(sig, 0);
    }
}
