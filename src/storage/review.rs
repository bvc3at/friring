//! Persistence for the native code-review view: review comments and per-file /
//! per-hunk "reviewed" marks, keyed by session id. CRUD modeled on
//! [`super::messages`]; the pure data types live in
//! [`crate::session::review`].

use rusqlite::{params, OptionalExtension};

use crate::session::review::{Classification, CommentAnchor, ReviewComment, Side};
use crate::session::SessionId;
use crate::sync::current_time_millis;

use super::Database;

/// Sentinel `hunk_index` meaning "the whole file is marked reviewed" (SQLite
/// PKs can't rely on NULL uniqueness, so a real value is used).
const WHOLE_FILE_HUNK: i64 = -1;

/// One persisted "reviewed" mark: `(file_path, hunk_index, fingerprint)`.
/// `hunk_index = None` means the whole file; a `None` fingerprint is a legacy
/// (pre-v41) row awaiting backfill.
pub type ReviewMarkRow = (String, Option<usize>, Option<String>);

/// Decompose a [`CommentAnchor`] into the four nullable columns it persists as.
fn anchor_columns(
    anchor: &CommentAnchor,
) -> (
    Option<String>,
    Option<&'static str>,
    Option<i64>,
    Option<i64>,
) {
    match anchor {
        CommentAnchor::Line {
            file,
            side,
            line,
            line_end,
        } => (
            Some(file.clone()),
            Some(side.as_str()),
            Some(i64::from(*line)),
            line_end.map(i64::from),
        ),
        CommentAnchor::File { file } => (Some(file.clone()), None, None, None),
        CommentAnchor::Review => (None, None, None, None),
    }
}

/// Rebuild a [`CommentAnchor`] from the persisted columns. A `line_end` that
/// doesn't extend past `line_no` (hand-edited rows) is dropped rather than
/// producing an inverted range.
fn anchor_from_columns(
    file_path: Option<String>,
    side: Option<String>,
    line_no: Option<i64>,
    line_end: Option<i64>,
) -> CommentAnchor {
    match (file_path, line_no) {
        (Some(file), Some(line)) => {
            let line = line.max(0) as u32;
            CommentAnchor::Line {
                file,
                side: side.as_deref().and_then(Side::parse).unwrap_or(Side::New),
                line,
                line_end: line_end.map(|e| e.max(0) as u32).filter(|&e| e > line),
            }
        }
        (Some(file), None) => CommentAnchor::File { file },
        (None, _) => CommentAnchor::Review,
    }
}

impl Database {
    /// Insert a review comment, returning its new id.
    pub fn add_review_comment(
        &self,
        session_id: SessionId,
        anchor: &CommentAnchor,
        classification: Classification,
        body: &str,
    ) -> rusqlite::Result<i64> {
        let now = current_time_millis() as i64;
        let (file_path, side, line_no, line_end) = anchor_columns(anchor);
        self.conn.execute(
            "INSERT INTO review_comments \
             (session_id, file_path, side, line_no, line_end, classification, body, \
              created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                session_id.to_string(),
                file_path,
                side,
                line_no,
                line_end,
                classification.as_str(),
                body,
                now,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// List a session's review comments (oldest first), summary comments
    /// included.
    pub fn list_review_comments(
        &self,
        session_id: SessionId,
    ) -> rusqlite::Result<Vec<ReviewComment>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, file_path, side, line_no, line_end, classification, body, \
             created_at, updated_at \
             FROM review_comments \
             WHERE session_id = ?1 AND deleted_at IS NULL \
             ORDER BY created_at, id",
        )?;
        let sid = session_id.to_string();
        let rows = stmt.query_map(params![sid], |row| {
            let file_path: Option<String> = row.get(1)?;
            let side: Option<String> = row.get(2)?;
            let line_no: Option<i64> = row.get(3)?;
            let line_end: Option<i64> = row.get(4)?;
            let class: String = row.get(5)?;
            Ok(ReviewComment {
                id: row.get(0)?,
                session_id,
                anchor: anchor_from_columns(file_path, side, line_no, line_end),
                classification: Classification::parse(&class).unwrap_or_default(),
                body: row.get(6)?,
                created_at: row.get::<_, i64>(7)? as u64,
                updated_at: row.get::<_, i64>(8)? as u64,
            })
        })?;
        rows.collect()
    }

    /// Update a comment's classification + body (e.g. editing an existing note).
    pub fn update_review_comment(
        &self,
        id: i64,
        classification: Classification,
        body: &str,
    ) -> rusqlite::Result<()> {
        let now = current_time_millis() as i64;
        self.conn.execute(
            "UPDATE review_comments SET classification = ?1, body = ?2, updated_at = ?3 \
             WHERE id = ?4 AND deleted_at IS NULL",
            params![classification.as_str(), body, now, id],
        )?;
        Ok(())
    }

    /// Soft-delete a review comment.
    pub fn delete_review_comment(&self, id: i64) -> rusqlite::Result<()> {
        let now = current_time_millis() as i64;
        self.conn.execute(
            "UPDATE review_comments SET deleted_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    /// Toggle a file/hunk "reviewed" mark. `hunk_index = None` marks the whole
    /// file. `fingerprint` is the semantic content hash of what is being
    /// marked ([`crate::session::review::file_fingerprint`] /
    /// [`crate::session::review::hunk_fingerprint`]), stored so a later diff
    /// rebuild can detect the content changed under the mark. Returns the new
    /// state (`true` = now reviewed).
    pub fn toggle_review_mark(
        &self,
        session_id: SessionId,
        file_path: &str,
        hunk_index: Option<usize>,
        fingerprint: &str,
    ) -> rusqlite::Result<bool> {
        let sid = session_id.to_string();
        let idx = hunk_index.map(|h| h as i64).unwrap_or(WHOLE_FILE_HUNK);
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM review_marks \
                 WHERE session_id = ?1 AND file_path = ?2 AND hunk_index = ?3",
                params![sid, file_path, idx],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_some() {
            self.conn.execute(
                "DELETE FROM review_marks \
                 WHERE session_id = ?1 AND file_path = ?2 AND hunk_index = ?3",
                params![sid, file_path, idx],
            )?;
            Ok(false)
        } else {
            let now = current_time_millis() as i64;
            self.conn.execute(
                "INSERT INTO review_marks \
                 (session_id, file_path, hunk_index, created_at, fingerprint) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![sid, file_path, idx, now, fingerprint],
            )?;
            Ok(true)
        }
    }

    /// List a session's "reviewed" marks as `(file_path, hunk_index,
    /// fingerprint)` where `hunk_index = None` means the whole file and a
    /// `None` fingerprint is a legacy (pre-v41) row.
    pub fn list_review_marks(&self, session_id: SessionId) -> rusqlite::Result<Vec<ReviewMarkRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT file_path, hunk_index, fingerprint FROM review_marks WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map(params![session_id.to_string()], |row| {
            let file: String = row.get(0)?;
            let idx: i64 = row.get(1)?;
            let hunk = if idx == WHOLE_FILE_HUNK {
                None
            } else {
                Some(idx.max(0) as usize)
            };
            Ok((file, hunk, row.get(2)?))
        })?;
        rows.collect()
    }

    /// Backfill a mark's fingerprint (a legacy NULL row observed by the
    /// reconciliation pass — treated as valid once, then pinned).
    pub fn set_review_mark_fingerprint(
        &self,
        session_id: SessionId,
        file_path: &str,
        hunk_index: Option<usize>,
        fingerprint: &str,
    ) -> rusqlite::Result<()> {
        let idx = hunk_index.map(|h| h as i64).unwrap_or(WHOLE_FILE_HUNK);
        self.conn.execute(
            "UPDATE review_marks SET fingerprint = ?4 \
             WHERE session_id = ?1 AND file_path = ?2 AND hunk_index = ?3",
            params![session_id.to_string(), file_path, idx, fingerprint],
        )?;
        Ok(())
    }

    /// Delete a single mark — the reconciliation pass removing a stale mark
    /// whose content changed (deleted for real, not hidden, so it can't
    /// resurrect on the next build).
    pub fn delete_review_mark(
        &self,
        session_id: SessionId,
        file_path: &str,
        hunk_index: Option<usize>,
    ) -> rusqlite::Result<()> {
        let idx = hunk_index.map(|h| h as i64).unwrap_or(WHOLE_FILE_HUNK);
        self.conn.execute(
            "DELETE FROM review_marks \
             WHERE session_id = ?1 AND file_path = ?2 AND hunk_index = ?3",
            params![session_id.to_string(), file_path, idx],
        )?;
        Ok(())
    }
}

#[cfg(test)]
impl Database {
    /// Insert a mark with a NULL fingerprint — the pre-v41 row shape — so the
    /// reconciliation pass's legacy branch is testable through the public API.
    pub fn insert_review_mark_without_fingerprint(
        &self,
        session_id: SessionId,
        file_path: &str,
        hunk_index: Option<usize>,
    ) -> rusqlite::Result<()> {
        let idx = hunk_index.map(|h| h as i64).unwrap_or(WHOLE_FILE_HUNK);
        self.conn.execute(
            "INSERT INTO review_marks (session_id, file_path, hunk_index, created_at) \
             VALUES (?1, ?2, ?3, 0)",
            params![session_id.to_string(), file_path, idx],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::review::Classification;

    #[test]
    fn comment_round_trip_all_anchor_levels() {
        let db = Database::open_in_memory().unwrap();
        let sid = SessionId::default();

        let line = CommentAnchor::Line {
            file: "src/foo.rs".into(),
            side: Side::New,
            line: 42,
            line_end: None,
        };
        db.add_review_comment(sid, &line, Classification::Issue, "bug here")
            .unwrap();
        db.add_review_comment(
            sid,
            &CommentAnchor::File {
                file: "src/foo.rs".into(),
            },
            Classification::Suggestion,
            "rename this file",
        )
        .unwrap();
        db.add_review_comment(
            sid,
            &CommentAnchor::Review,
            Classification::Note,
            "LGTM overall",
        )
        .unwrap();

        let comments = db.list_review_comments(sid).unwrap();
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[0].anchor, line);
        assert_eq!(comments[0].classification, Classification::Issue);
        assert!(matches!(comments[1].anchor, CommentAnchor::File { .. }));
        assert_eq!(comments[2].anchor, CommentAnchor::Review);
    }

    #[test]
    fn range_comment_round_trips_and_inverted_end_is_dropped() {
        let db = Database::open_in_memory().unwrap();
        let sid = SessionId::default();
        let range = CommentAnchor::Line {
            file: "src/foo.rs".into(),
            side: Side::New,
            line: 10,
            line_end: Some(24),
        };
        let id = db
            .add_review_comment(sid, &range, Classification::Issue, "span bug")
            .unwrap();
        assert_eq!(db.list_review_comments(sid).unwrap()[0].anchor, range);

        // A line_end that doesn't extend past line_no (a hand-edited row)
        // must not surface as an inverted range.
        db.conn
            .execute(
                "UPDATE review_comments SET line_end = 10 WHERE id = ?1",
                [id],
            )
            .unwrap();
        assert_eq!(
            db.list_review_comments(sid).unwrap()[0].anchor,
            CommentAnchor::Line {
                file: "src/foo.rs".into(),
                side: Side::New,
                line: 10,
                line_end: None,
            }
        );
    }

    #[test]
    fn update_and_soft_delete_comment() {
        let db = Database::open_in_memory().unwrap();
        let sid = SessionId::default();
        let id = db
            .add_review_comment(sid, &CommentAnchor::Review, Classification::Note, "draft")
            .unwrap();

        db.update_review_comment(id, Classification::Praise, "nice work")
            .unwrap();
        let c = &db.list_review_comments(sid).unwrap()[0];
        assert_eq!(c.classification, Classification::Praise);
        assert_eq!(c.body, "nice work");

        db.delete_review_comment(id).unwrap();
        assert!(db.list_review_comments(sid).unwrap().is_empty());
    }

    #[test]
    fn marks_toggle_file_and_hunk_independently() {
        let db = Database::open_in_memory().unwrap();
        let sid = SessionId::default();

        assert!(db.toggle_review_mark(sid, "a.rs", None, "fp-file").unwrap()); // file reviewed
        assert!(db
            .toggle_review_mark(sid, "a.rs", Some(2), "fp-h2")
            .unwrap()); // hunk 2 reviewed
        let mut marks = db.list_review_marks(sid).unwrap();
        marks.sort();
        assert_eq!(
            marks,
            vec![
                ("a.rs".to_string(), None, Some("fp-file".to_string())),
                ("a.rs".to_string(), Some(2), Some("fp-h2".to_string())),
            ]
        );

        // Toggling the file mark again clears just it.
        assert!(!db.toggle_review_mark(sid, "a.rs", None, "fp-file").unwrap());
        let marks = db.list_review_marks(sid).unwrap();
        assert_eq!(
            marks,
            vec![("a.rs".to_string(), Some(2), Some("fp-h2".to_string()))]
        );
    }

    #[test]
    fn mark_fingerprint_backfill_and_targeted_delete() {
        let db = Database::open_in_memory().unwrap();
        let sid = SessionId::default();
        db.toggle_review_mark(sid, "a.rs", None, "old-fp").unwrap();
        db.toggle_review_mark(sid, "a.rs", Some(1), "h1").unwrap();

        // Backfill rewrites only the addressed mark.
        db.set_review_mark_fingerprint(sid, "a.rs", None, "new-fp")
            .unwrap();
        let mut marks = db.list_review_marks(sid).unwrap();
        marks.sort();
        assert_eq!(marks[0].2.as_deref(), Some("new-fp"));
        assert_eq!(marks[1].2.as_deref(), Some("h1"));

        // Targeted delete removes only the addressed mark.
        db.delete_review_mark(sid, "a.rs", Some(1)).unwrap();
        let marks = db.list_review_marks(sid).unwrap();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].1, None);
    }

    #[test]
    fn base_branch_round_trips_write_once() {
        let db = Database::open_in_memory().unwrap();
        let sid = SessionId::default();
        // No session row yet: a no-op update leaves it unset.
        assert_eq!(db.get_session_base_branch(sid).unwrap(), None);
    }
}
