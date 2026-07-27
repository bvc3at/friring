//! SQLite-backed persistent storage for Friring state.
//!
//! Replaces `state.toml` and `shared_state.toml` with a single SQLite database.
//! Provides soft delete with `deleted_at` columns and a full audit trail.
//!
//! # Usage
//!
//! ```ignore
//! let db = Database::open(path)?;
//! db.upsert_session(&session)?;
//! ```

pub mod audit;
pub mod automations;
pub mod keybindings;
pub mod messages;
pub mod repo_bookmarks;
pub mod review;
mod schema;
mod sessions;
mod settings;
pub use sessions::{DeletedSessionInfo, HookRow};
pub mod sync;
mod sync_bases;
pub mod tasks;
mod worktrees;

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use uuid::Uuid;

use crate::session::AutomationAction;

/// Serialize a `Spawn` action's extra-repo list for the `action_extra_repos`
/// column. An empty list (the single-repo common case) stores `NULL`, so old
/// and new single-repo rows are byte-identical. Shared by the `tasks` and
/// `automations` storage layers.
pub(super) fn extra_repos_to_json(extra_repos: &[crate::session::ExtraRepo]) -> Option<String> {
    if extra_repos.is_empty() {
        return None;
    }
    // Serialization of a plain struct list cannot fail; fall back to `None`.
    serde_json::to_string(extra_repos).ok()
}

/// Decode the `action_extra_repos` column back into an extra-repo list.
/// `NULL`/empty/malformed → an empty list (a single-repo spawn), never an error.
pub(super) fn extra_repos_from_json(raw: Option<String>) -> Vec<crate::session::ExtraRepo> {
    raw.filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// The action-specific columns an [`AutomationAction`] is stored as, shared by
/// the `tasks` and `automations` tables (both carry an identical group). The
/// `action_kind` discriminant is stored separately (`AutomationAction::kind`).
///
/// Every field added after v33 is nullable and decodes to the pre-existing
/// default, so a row written by an older friring round-trips unchanged.
#[derive(Debug, Default, Clone)]
pub(super) struct ActionColumns {
    pub target_session: Option<String>,
    pub repo_path: Option<String>,
    pub worktree_branch: Option<String>,
    pub base_branch: Option<String>,
    pub agent: Option<String>,
    /// `action_extra_repos` (JSON list, `NULL` = single-repo).
    pub extra_repos: Option<String>,
    pub command: Option<String>,
    /// `action_target_name` — a `Send` target resolved by session name
    /// (`NULL` = the `target_session` id form).
    pub target_name: Option<String>,
    /// `action_host` — `hosts.toml` name for a remote `Spawn` (`NULL` = local).
    pub host: Option<String>,
    /// `action_session_mode` — `NULL`/`reuse` = the one-session-per-automation
    /// default, `fresh` = a new session per fire.
    pub session_mode: Option<String>,
    /// `action_timeout_secs` — `Exec` kill deadline (`NULL` = the default).
    pub timeout_secs: Option<i64>,
}

/// Encode an action into its persisted columns (sans the `action_kind`
/// discriminant, which the caller derives via [`AutomationAction::kind`]).
pub(super) fn action_to_columns(action: &AutomationAction) -> ActionColumns {
    match action {
        AutomationAction::Send { target } => ActionColumns {
            target_session: target.id().map(|id| id.to_string()),
            target_name: target.name().map(str::to_string),
            ..ActionColumns::default()
        },
        AutomationAction::Spawn {
            repo_path,
            worktree_branch,
            base_branch,
            agent,
            extra_repos,
            host,
            session_mode,
        } => ActionColumns {
            repo_path: Some(repo_path.to_string_lossy().into_owned()),
            worktree_branch: worktree_branch.clone(),
            base_branch: base_branch.clone(),
            agent: agent.clone(),
            extra_repos: extra_repos_to_json(extra_repos),
            host: host.clone(),
            // `Reuse` is the pre-v44 behavior, so it stores NULL and an
            // untouched row stays byte-identical.
            session_mode: (*session_mode != crate::session::SpawnSessionMode::Reuse)
                .then(|| session_mode.as_str().to_string()),
            ..ActionColumns::default()
        },
        AutomationAction::Exec {
            command,
            timeout_secs,
        } => ActionColumns {
            command: Some(command.clone()),
            timeout_secs: timeout_secs.map(|s| s as i64),
            ..ActionColumns::default()
        },
    }
}

/// Reconstruct an action from its `action_kind` discriminant + persisted
/// columns. An unrecognized `kind` decodes to `Spawn` (the automations
/// catch-all); callers that allow an action-less row (tasks) gate this on a
/// non-NULL `action_kind` themselves.
pub(super) fn action_from_columns(kind: &str, cols: ActionColumns) -> AutomationAction {
    match kind {
        // A name target wins when set; otherwise fall back to the id column
        // (every pre-v44 send row).
        "send" => AutomationAction::Send {
            target: match cols.target_name {
                Some(name) => crate::session::SendTarget::Name(name),
                None => crate::session::SendTarget::Id(
                    cols.target_session
                        .unwrap_or_default()
                        .parse()
                        .unwrap_or_default(),
                ),
            },
        },
        "exec" => AutomationAction::Exec {
            command: cols.command.unwrap_or_default(),
            timeout_secs: cols.timeout_secs.and_then(|s| u64::try_from(s).ok()),
        },
        _ => AutomationAction::Spawn {
            repo_path: PathBuf::from(cols.repo_path.unwrap_or_default()),
            worktree_branch: cols.worktree_branch,
            base_branch: cols.base_branch,
            agent: cols.agent,
            extra_repos: extra_repos_from_json(cols.extra_repos),
            host: cols.host,
            session_mode: cols
                .session_mode
                .as_deref()
                .map(crate::session::SpawnSessionMode::from_str_or_default)
                .unwrap_or_default(),
        },
    }
}

/// SQLite-backed database for application state.
pub struct Database {
    conn: Connection,
    /// Unique ID for this friring instance (used in audit trail).
    instance_id: String,
    /// Last known data_version for external change detection.
    last_data_version: i64,
}

impl Database {
    /// Open or create a database at the given path. Runs schema migrations.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        schema::initialize(&conn)?;

        let last_data_version = conn.query_row("PRAGMA data_version", [], |row| row.get(0))?;

        let db = Self {
            conn,
            instance_id: Uuid::new_v4().to_string(),
            last_data_version,
        };
        // Best-effort retention; opening the DB must not fail over old breadcrumbs.
        if let Err(e) = db.prune_audit_log() {
            tracing::warn!("Failed to prune audit log: {e}");
        }
        if let Err(e) = db.prune_old_messages() {
            tracing::warn!("Failed to prune session messages: {e}");
        }
        Ok(db)
    }

    /// Get a reference to the underlying connection (for metadata queries).
    pub fn conn_ref(&self) -> &Connection {
        &self.conn
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::initialize(&conn)?;

        Ok(Self {
            conn,
            instance_id: Uuid::new_v4().to_string(),
            last_data_version: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory() {
        let db = Database::open_in_memory();
        assert!(db.is_ok());
    }

    #[test]
    fn open_file_based() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(temp.path());
        assert!(db.is_ok());
    }

    #[test]
    fn open_creates_parent_dirs() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sub").join("dir").join("friring.db");

        let db = Database::open(&path);
        assert!(db.is_ok());
        assert!(path.exists());
    }

    #[test]
    fn instance_id_is_unique() {
        let db1 = Database::open_in_memory().unwrap();
        let db2 = Database::open_in_memory().unwrap();
        assert_ne!(db1.instance_id, db2.instance_id);
    }
}
