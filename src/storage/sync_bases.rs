//! Per-repo default base remote for the Ctrl+S worktree sync
//! (`repo_sync_bases`). Keyed on the parent repo path only: the sync runs
//! local git against local worktrees, so there is no host dimension (unlike
//! `repo_bookmarks`).

use std::path::Path;

use rusqlite::{params, OptionalExtension};

use super::Database;

impl Database {
    /// The saved default base remote for a repo, if one was chosen in the
    /// sync base picker. May name a remote that no longer exists — callers
    /// validate against the repo's live remote list.
    pub fn get_sync_base_remote(&self, repo_path: &Path) -> rusqlite::Result<Option<String>> {
        let path_str = repo_path.to_string_lossy().to_string();
        self.conn
            .query_row(
                "SELECT remote FROM repo_sync_bases WHERE repo_path = ?1",
                params![path_str],
                |row| row.get::<_, String>(0),
            )
            .optional()
    }

    /// Save a repo's default base remote (the last picker choice wins).
    pub fn set_sync_base_remote(&self, repo_path: &Path, remote: &str) -> rusqlite::Result<()> {
        let path_str = repo_path.to_string_lossy().to_string();
        self.conn.execute(
            "INSERT INTO repo_sync_bases (repo_path, remote) VALUES (?1, ?2) \
             ON CONFLICT(repo_path) DO UPDATE SET remote = excluded.remote",
            params![path_str, remote],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_base_remote_round_trips_and_overwrites() {
        let db = Database::open_in_memory().unwrap();
        let repo = Path::new("/repo/a");
        assert_eq!(db.get_sync_base_remote(repo).unwrap(), None);

        db.set_sync_base_remote(repo, "upstream").unwrap();
        assert_eq!(
            db.get_sync_base_remote(repo).unwrap().as_deref(),
            Some("upstream")
        );

        // The last choice wins.
        db.set_sync_base_remote(repo, "origin").unwrap();
        assert_eq!(
            db.get_sync_base_remote(repo).unwrap().as_deref(),
            Some("origin")
        );

        // Scoped per repo path.
        assert_eq!(db.get_sync_base_remote(Path::new("/repo/b")).unwrap(), None);
    }
}
