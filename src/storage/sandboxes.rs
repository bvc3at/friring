//! Persistence for [`SandboxProfile`]s and the places they create.
//!
//! Sandbox profiles are a UI-edited collection, so they live in SQLite
//! following the automations pattern rather than in a TOML file
//! (`docs/SANDBOX.md`, "Data model"). The list-shaped fields — `paths`,
//! `network_allow`, `network_deny` — are JSON TEXT columns, the enums are
//! stored as their `as_str()` spelling, and the timestamps are unix millis
//! stamped here, exactly like `automations`.
//!
//! The one structural difference from automations is identity: a profile is
//! keyed by its **name**, because that same name is the `sandbox:<name>`
//! backend name and the value `sessions.sandbox_profile` carries. Changing it
//! is therefore not an `UPDATE … SET name` a caller may issue on its own — see
//! [`Database::rename_sandbox_profile`], which rewrites the referencing rows in
//! the same transaction, and [`Database::upsert_sandbox_profile`], which
//! deliberately refuses to move a profile's identity.

use std::str::FromStr;

use rusqlite::{params, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::session::{SandboxBackendKind, SandboxProfile, SANDBOX_BACKEND_PREFIX};
use crate::sync::current_time_millis;

use super::Database;

/// One place a profile has created: a container, a lightweight VM, a cloned WSL
/// distro. Policy backends never produce one — their sandbox is a kernel policy
/// over a process tree, with nothing that outlives the launch (ADR-26).
///
/// A row is a *record of* the real object, never the object itself. Dropping it
/// does not stop a container, and dropping one whose place is still running
/// leaks that place: nothing else knows the id. Tear the place down first, then
/// forget the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInstance {
    /// The profile that owns this place. Follows a
    /// [`rename_sandbox_profile`](Database::rename_sandbox_profile) and is
    /// removed by a [`delete_sandbox_profile`](Database::delete_sandbox_profile).
    pub profile: String,
    /// Which place backend created it. Never a policy backend, and never
    /// [`SandboxBackendKind::Auto`] — the ladder has resolved by the time a
    /// place exists.
    pub engine: SandboxBackendKind,
    /// The engine's own handle: a container id, a distro name. Unique per
    /// engine, and the key this row is stored under.
    pub external_id: String,
    /// Backend-defined lifecycle state (`running`, `stopped`, …). Free text on
    /// purpose: the vocabulary belongs to whichever backend wrote the row, and
    /// storage has no business narrowing it before a backend exists to define
    /// it.
    pub state: String,
    /// Unix millis, set on insert.
    pub created_at: u64,
    /// Unix millis, refreshed on every upsert — what garbage collection sorts
    /// by when deciding which idle place to reclaim.
    pub last_used_at: u64,
}

impl SandboxInstance {
    /// A record of a place that was just created or re-discovered. The
    /// timestamps are filled in by
    /// [`upsert_sandbox_instance`](Database::upsert_sandbox_instance), which
    /// owns the clock.
    pub fn new(
        profile: impl Into<String>,
        engine: SandboxBackendKind,
        external_id: impl Into<String>,
        state: impl Into<String>,
    ) -> Self {
        Self {
            profile: profile.into(),
            engine,
            external_id: external_id.into(),
            state: state.into(),
            created_at: 0,
            last_used_at: 0,
        }
    }
}

/// Encode a list column (`paths`, `network_allow`, `network_deny`) as JSON.
/// These are plain data types whose serialization cannot fail; an empty list is
/// the neutral value if one ever did, because the columns are `NOT NULL`.
fn list_to_json<T: Serialize>(items: &[T]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

/// Decode a JSON list column. `NULL`/empty/malformed → an empty list, never an
/// error: a profile that lost its paths still has to appear in the list modal so
/// the user can repair or delete it, whereas a failed row read would take every
/// *other* profile down with it. [`SandboxProfile::validate`] is what refuses to
/// save the result.
fn list_from_json<T: DeserializeOwned>(raw: Option<String>) -> Vec<T> {
    raw.filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Decode an enum column, falling back to the type's default for the same
/// reason [`list_from_json`] returns an empty list — a hand-edited or imported
/// value friring does not know must not hide the whole collection. Every one of
/// these defaults is also the default a fresh profile is created with.
fn enum_from_db<T: FromStr + Default>(raw: &str) -> T {
    T::from_str(raw).unwrap_or_default()
}

/// Column list for profile SELECTs (keep in sync with [`map_profile`]).
const COLS: &str = "name, backend, paths, network_mode, network_allow, network_deny, \
    prompt_new_domains, read_scope, memory_mb, cpus, image, containerfile, \
    allow_unsandboxed_fallback, created_at, updated_at";

/// Column list for instance SELECTs (keep in sync with [`map_instance`]).
const INSTANCE_COLS: &str = "profile, engine, external_id, state, created_at, last_used_at";

fn map_profile(row: &rusqlite::Row) -> rusqlite::Result<SandboxProfile> {
    Ok(SandboxProfile {
        name: row.get(0)?,
        backend: enum_from_db(&row.get::<_, String>(1)?),
        paths: list_from_json(row.get(2)?),
        network_mode: enum_from_db(&row.get::<_, String>(3)?),
        network_allow: list_from_json(row.get(4)?),
        network_deny: list_from_json(row.get(5)?),
        prompt_new_domains: row.get::<_, i64>(6)? != 0,
        read_scope: enum_from_db(&row.get::<_, String>(7)?),
        // Stored as INTEGER; a negative or over-wide value is meaningless as a
        // limit, so it decodes to "uncapped" rather than failing the read.
        memory_mb: row
            .get::<_, Option<i64>>(8)?
            .and_then(|v| u32::try_from(v).ok()),
        cpus: row
            .get::<_, Option<i64>>(9)?
            .and_then(|v| u32::try_from(v).ok()),
        image: row.get(10)?,
        containerfile: row.get(11)?,
        allow_unsandboxed_fallback: row.get::<_, i64>(12)? != 0,
        created_at: row.get::<_, i64>(13)? as u64,
        updated_at: row.get::<_, i64>(14)? as u64,
    })
}

fn map_instance(row: &rusqlite::Row) -> rusqlite::Result<SandboxInstance> {
    Ok(SandboxInstance {
        profile: row.get(0)?,
        engine: enum_from_db(&row.get::<_, String>(1)?),
        external_id: row.get(2)?,
        state: row.get(3)?,
        created_at: row.get::<_, i64>(4)? as u64,
        last_used_at: row.get::<_, i64>(5)? as u64,
    })
}

impl Database {
    /// Every sandbox profile, in the case-insensitive alphabetical order the
    /// list modal shows (the `name` column collates `NOCASE`).
    pub fn list_sandbox_profiles(&self) -> rusqlite::Result<Vec<SandboxProfile>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM sandbox_profiles ORDER BY name"
        ))?;
        let rows = stmt.query_map([], map_profile)?;
        rows.collect()
    }

    /// The names of every profile, for
    /// [`SandboxProfile::validate_unique`](crate::session::SandboxProfile::validate_unique)
    /// and for the session-creation step's filter, without paying for the JSON
    /// columns.
    pub fn list_sandbox_profile_names(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM sandbox_profiles ORDER BY name")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect()
    }

    /// Fetch one profile by name. Matching is case-insensitive, because the
    /// name becomes a container or distro name where two spellings would be one
    /// object; it is also trimmed, because the name arrives from a text field
    /// and a stray space must not read as "no such profile".
    pub fn get_sandbox_profile(&self, name: &str) -> rusqlite::Result<Option<SandboxProfile>> {
        self.conn
            .query_row(
                &format!("SELECT {COLS} FROM sandbox_profiles WHERE name = ?1"),
                params![name.trim()],
                map_profile,
            )
            .optional()
    }

    /// Insert a profile, or overwrite the one already stored under its name.
    ///
    /// Storage owns the timestamps: `created_at` is stamped on insert and kept
    /// afterwards, `updated_at` on every save. The profile's own timestamp
    /// fields are ignored on the way in and filled in on the way out.
    ///
    /// **This never moves a profile's identity.** A name that differs only in
    /// case updates the stored row and leaves its spelling alone; a genuinely
    /// different name inserts a *second* profile and leaves the first — the
    /// editor must call [`rename_sandbox_profile`](Self::rename_sandbox_profile)
    /// for a renamed profile, since only that path rewrites the sessions and
    /// instances pointing at the old name.
    pub fn upsert_sandbox_profile(&self, profile: &SandboxProfile) -> rusqlite::Result<()> {
        let now = current_time_millis() as i64;
        self.conn.execute(
            "INSERT INTO sandbox_profiles
                (name, backend, paths, network_mode, network_allow, network_deny,
                 prompt_new_domains, read_scope, memory_mb, cpus, image,
                 containerfile, allow_unsandboxed_fallback, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)
             ON CONFLICT(name) DO UPDATE SET
                 backend = excluded.backend,
                 paths = excluded.paths,
                 network_mode = excluded.network_mode,
                 network_allow = excluded.network_allow,
                 network_deny = excluded.network_deny,
                 prompt_new_domains = excluded.prompt_new_domains,
                 read_scope = excluded.read_scope,
                 memory_mb = excluded.memory_mb,
                 cpus = excluded.cpus,
                 image = excluded.image,
                 containerfile = excluded.containerfile,
                 allow_unsandboxed_fallback = excluded.allow_unsandboxed_fallback,
                 updated_at = excluded.updated_at",
            params![
                profile.name.trim(),
                profile.backend.as_str(),
                list_to_json(&profile.paths),
                profile.network_mode.as_str(),
                list_to_json(&profile.network_allow),
                list_to_json(&profile.network_deny),
                profile.prompt_new_domains as i64,
                profile.read_scope.as_str(),
                profile.memory_mb.map(i64::from),
                profile.cpus.map(i64::from),
                profile.image,
                profile.containerfile,
                profile.allow_unsandboxed_fallback as i64,
                now,
            ],
        )?;
        Ok(())
    }

    /// Rename a profile and every reference to it: its instance records (by
    /// `ON UPDATE CASCADE`) and the sessions that name it, both in
    /// `sandbox_profile` and in a place-backed session's
    /// `sandbox:<profile>` `backend_type`. One transaction, because a half-done
    /// rename leaves sessions pointing at a profile that no longer exists.
    ///
    /// Returns whether a profile was found under `from`. A `to` that collides
    /// with another profile surfaces as a constraint error — check
    /// [`SandboxProfile::validate_unique`](crate::session::SandboxProfile::validate_unique)
    /// against [`list_sandbox_profile_names`](Self::list_sandbox_profile_names)
    /// first for a message worth showing.
    pub fn rename_sandbox_profile(&self, from: &str, to: &str) -> rusqlite::Result<bool> {
        let from = from.trim();
        let to = to.trim();
        let tx = self.conn.unchecked_transaction()?;
        let renamed = tx.execute(
            "UPDATE sandbox_profiles SET name = ?2, updated_at = ?3 WHERE name = ?1",
            params![from, to, current_time_millis() as i64],
        )?;
        if renamed == 0 {
            return Ok(false);
        }
        // `COLLATE NOCASE` on the session side too: a session records the
        // spelling it was created with, which need not match the profile's.
        tx.execute(
            "UPDATE sessions SET sandbox_profile = ?2 \
             WHERE sandbox_profile = ?1 COLLATE NOCASE",
            params![from, to],
        )?;
        tx.execute(
            "UPDATE sessions SET backend_type = ?2 WHERE backend_type = ?1 COLLATE NOCASE",
            params![
                format!("{SANDBOX_BACKEND_PREFIX}{from}"),
                format!("{SANDBOX_BACKEND_PREFIX}{to}"),
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Delete a profile and forget the instance records of its places. Returns
    /// whether the profile existed.
    ///
    /// Two things this deliberately does **not** do. It does not stop or remove
    /// the places themselves — the caller tears those down first, or they leak
    /// (see [`SandboxInstance`]). And it does not clear `sandbox_profile` on the
    /// sessions that name it: a session left pointing at a missing profile fails
    /// loudly at launch, where silently clearing it would spawn that agent
    /// **unsandboxed** on the host. Ask
    /// [`count_sessions_using_sandbox_profile`](Self::count_sessions_using_sandbox_profile)
    /// before offering the delete.
    pub fn delete_sandbox_profile(&self, name: &str) -> rusqlite::Result<bool> {
        let name = name.trim();
        let tx = self.conn.unchecked_transaction()?;
        // The foreign key would cascade this anyway; doing it explicitly keeps
        // the pair atomic on a connection where `foreign_keys` is off, and
        // mirrors `delete_automation` clearing its run history first.
        tx.execute(
            "DELETE FROM sandbox_instances WHERE profile = ?1",
            params![name],
        )?;
        let deleted = tx.execute(
            "DELETE FROM sandbox_profiles WHERE name = ?1",
            params![name],
        )?;
        tx.commit()?;
        Ok(deleted > 0)
    }

    /// How many live sessions would be left without a sandbox if this profile
    /// were deleted — the number the delete confirmation names. Counts both the
    /// `sandbox_profile` link every sandboxed session carries and the
    /// `sandbox:<profile>` `backend_type` a place-backed one is registered
    /// under. Soft-deleted sessions are excluded: they are not running, and a
    /// restore re-checks the profile anyway.
    pub fn count_sessions_using_sandbox_profile(&self, name: &str) -> rusqlite::Result<usize> {
        let name = name.trim();
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM sessions \
             WHERE deleted_at IS NULL \
               AND (sandbox_profile = ?1 COLLATE NOCASE \
                    OR backend_type = ?2 COLLATE NOCASE)",
            params![name, format!("{SANDBOX_BACKEND_PREFIX}{name}")],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Record a place, or refresh the one already known under
    /// `(engine, external_id)`: its state and `last_used_at` move, its
    /// `created_at` stays.
    ///
    /// Fails with a foreign-key error if no profile is stored under
    /// [`profile`](SandboxInstance::profile) — a place with no recipe to
    /// describe it is not something garbage collection could ever reason about.
    pub fn upsert_sandbox_instance(&self, instance: &SandboxInstance) -> rusqlite::Result<()> {
        let now = current_time_millis() as i64;
        self.conn.execute(
            "INSERT INTO sandbox_instances
                (profile, engine, external_id, state, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(engine, external_id) DO UPDATE SET
                 profile = excluded.profile,
                 state = excluded.state,
                 last_used_at = excluded.last_used_at",
            params![
                instance.profile.trim(),
                instance.engine.as_str(),
                instance.external_id,
                instance.state,
                now,
            ],
        )?;
        Ok(())
    }

    /// Every recorded place, ordered by profile then engine then id — stable,
    /// so the manager view does not reshuffle when a state changes.
    pub fn list_sandbox_instances(&self) -> rusqlite::Result<Vec<SandboxInstance>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {INSTANCE_COLS} FROM sandbox_instances \
             ORDER BY profile, engine, external_id"
        ))?;
        let rows = stmt.query_map([], map_instance)?;
        rows.collect()
    }

    /// The places recorded for one profile, in the same order.
    pub fn list_sandbox_instances_for_profile(
        &self,
        profile: &str,
    ) -> rusqlite::Result<Vec<SandboxInstance>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {INSTANCE_COLS} FROM sandbox_instances WHERE profile = ?1 \
             ORDER BY engine, external_id"
        ))?;
        let rows = stmt.query_map(params![profile.trim()], map_instance)?;
        rows.collect()
    }

    /// Forget one place. Returns whether a record existed; removing the place
    /// itself is the caller's job.
    pub fn delete_sandbox_instance(
        &self,
        engine: SandboxBackendKind,
        external_id: &str,
    ) -> rusqlite::Result<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM sandbox_instances WHERE engine = ?1 AND external_id = ?2",
            params![engine.as_str(), external_id],
        )?;
        Ok(deleted > 0)
    }

    /// Forget every place recorded for a profile — what the list modal's prune
    /// action records once the places are gone. Returns how many rows went.
    pub fn delete_sandbox_instances_for_profile(&self, profile: &str) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM sandbox_instances WHERE profile = ?1",
            params![profile.trim()],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{NetworkMode, PathMode, ReadScope, SandboxPath, SandboxShape};

    fn profile(name: &str) -> SandboxProfile {
        SandboxProfile::new(
            name,
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("~/dev/app/.git/hooks"),
                SandboxPath::read_only("/srv/shared"),
            ],
        )
    }

    /// A session row referencing `profile`, written straight to the table:
    /// `sessions` belongs to `storage::sessions`, and these tests only care
    /// that the two link columns move when a profile is renamed.
    fn seed_session(db: &Database, id: &str, backend_type: &str, sandbox_profile: Option<&str>) {
        db.conn
            .execute(
                "INSERT INTO sessions (id, name, backend_type, sandbox_profile, \
                 created_at, updated_at) VALUES (?1, ?1, ?2, ?3, 0, 0)",
                params![id, backend_type, sandbox_profile],
            )
            .unwrap();
    }

    #[test]
    fn create_get_and_list_round_trip() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.list_sandbox_profiles().unwrap().is_empty());

        db.upsert_sandbox_profile(&profile("dev")).unwrap();

        let got = db.get_sandbox_profile("dev").unwrap().unwrap();
        assert_eq!(got.name, "dev");
        assert_eq!(got.paths.len(), 3);
        assert_eq!(db.list_sandbox_profiles().unwrap().len(), 1);
        assert_eq!(db.list_sandbox_profile_names().unwrap(), ["dev"]);
        assert!(db.get_sandbox_profile("nope").unwrap().is_none());
    }

    #[test]
    fn every_field_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let mut p = profile("place");
        p.backend = SandboxBackendKind::Podman;
        p.network_mode = NetworkMode::Allowlist;
        p.network_allow = vec!["api.anthropic.com".into(), "github.com:443".into()];
        p.network_deny = vec!["gist.github.com".into()];
        p.prompt_new_domains = false;
        p.read_scope = ReadScope::Workspace;
        p.memory_mb = Some(4096);
        p.cpus = Some(2);
        p.image = Some("ghcr.io/example/dev:latest".into());
        p.allow_unsandboxed_fallback = true;
        db.upsert_sandbox_profile(&p).unwrap();

        let got = db.get_sandbox_profile("place").unwrap().unwrap();
        assert_eq!(got.backend, SandboxBackendKind::Podman);
        assert_eq!(got.network_mode, NetworkMode::Allowlist);
        assert_eq!(got.network_allow, p.network_allow);
        assert_eq!(got.network_deny, p.network_deny);
        assert!(!got.prompt_new_domains);
        assert_eq!(got.read_scope, ReadScope::Workspace);
        assert_eq!(got.memory_mb, Some(4096));
        assert_eq!(got.cpus, Some(2));
        assert_eq!(got.image.as_deref(), Some("ghcr.io/example/dev:latest"));
        assert_eq!(got.containerfile, None);
        assert!(got.allow_unsandboxed_fallback);
        // Everything but the storage-owned timestamps survives untouched.
        assert_eq!(
            SandboxProfile {
                created_at: 0,
                updated_at: 0,
                ..got.clone()
            },
            p
        );
        // …and the stored profile still resolves, which is the only thing the
        // launch path asks of it.
        let policy = got.resolve(SandboxBackendKind::Podman, "/home/u").unwrap();
        assert_eq!(policy.shape, SandboxShape::Place);
    }

    #[test]
    fn paths_keep_their_order_and_modes() {
        // The JSON column is the only thing preserving per-path intent, and the
        // order is what the editor's add/remove sub-list shows back.
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();

        let got = db.get_sandbox_profile("dev").unwrap().unwrap();
        let listed: Vec<(&str, PathMode)> = got
            .paths
            .iter()
            .map(|p| (p.path.as_str(), p.mode))
            .collect();
        assert_eq!(
            listed,
            [
                ("~/dev/app", PathMode::ReadWrite),
                ("~/dev/app/.git/hooks", PathMode::ReadOnly),
                ("/srv/shared", PathMode::ReadOnly),
            ]
        );
        // Stored as written: `~` survives, so the profile still means the right
        // directory on a host whose home is spelled differently.
        assert!(got.covers("/Users/u/dev/app/src", "/Users/u"));
    }

    #[test]
    fn upsert_replaces_the_row_and_keeps_created_at() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        let first = db.get_sandbox_profile("dev").unwrap().unwrap();
        assert!(first.created_at > 0);
        assert_eq!(first.created_at, first.updated_at);

        let mut edited = first.clone();
        edited.paths = vec![SandboxPath::workspace("~/dev/other")];
        edited.network_mode = NetworkMode::None;
        // A stale timestamp on the way in must not travel back into the row.
        edited.created_at = 1;
        edited.updated_at = 1;
        db.upsert_sandbox_profile(&edited).unwrap();

        let got = db.get_sandbox_profile("dev").unwrap().unwrap();
        assert_eq!(db.list_sandbox_profiles().unwrap().len(), 1);
        assert_eq!(got.paths.len(), 1);
        assert_eq!(got.network_mode, NetworkMode::None);
        assert_eq!(got.created_at, first.created_at);
        assert!(got.updated_at >= first.updated_at);
    }

    #[test]
    fn names_are_unique_case_insensitively() {
        // Two profiles differing only in case would be one container name, so
        // the key collates NOCASE and the second save edits the first row.
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();

        let mut shouty = profile("DEV");
        shouty.network_mode = NetworkMode::Full;
        db.upsert_sandbox_profile(&shouty).unwrap();

        let all = db.list_sandbox_profiles().unwrap();
        assert_eq!(all.len(), 1);
        // The edit landed, and the stored spelling stayed put — moving identity
        // is `rename_sandbox_profile`'s job alone.
        assert_eq!(all[0].network_mode, NetworkMode::Full);
        assert_eq!(all[0].name, "dev");
        assert!(db.get_sandbox_profile("DeV").unwrap().is_some());
    }

    #[test]
    fn lookups_tolerate_the_spelling_a_text_field_produces() {
        // A profile name reaches storage from the editor's text field and from
        // a session's stored copy of it, so surrounding space and a different
        // case must still resolve to the one profile they mean.
        let db = Database::open_in_memory().unwrap();
        let mut padded = profile("dev");
        padded.name = "  dev  ".to_string();
        db.upsert_sandbox_profile(&padded).unwrap();

        assert_eq!(db.list_sandbox_profile_names().unwrap(), ["dev"]);
        assert!(db.get_sandbox_profile(" DEV ").unwrap().is_some());
        assert_eq!(db.count_sessions_using_sandbox_profile(" dev ").unwrap(), 0);
        assert!(db.delete_sandbox_profile(" Dev ").unwrap());
    }

    #[test]
    fn list_is_ordered_case_insensitively_by_name() {
        let db = Database::open_in_memory().unwrap();
        for name in ["zeta", "Beta", "alpha"] {
            db.upsert_sandbox_profile(&profile(name)).unwrap();
        }
        assert_eq!(
            db.list_sandbox_profile_names().unwrap(),
            ["alpha", "Beta", "zeta"]
        );
    }

    #[test]
    fn rename_moves_the_profile_its_instances_and_its_sessions() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        db.upsert_sandbox_instance(&SandboxInstance::new(
            "dev",
            SandboxBackendKind::Docker,
            "ctr-1",
            "running",
        ))
        .unwrap();
        // A policy-backed session (link column only) and a place-backed one
        // (link column plus the `sandbox:<profile>` backend name).
        seed_session(&db, "s-policy", "local-tmux", Some("dev"));
        seed_session(&db, "s-place", "sandbox:dev", Some("dev"));
        seed_session(&db, "s-plain", "local-tmux", None);

        assert!(db.rename_sandbox_profile("dev", "dev2").unwrap());

        assert!(db.get_sandbox_profile("dev").unwrap().is_none());
        assert_eq!(
            db.get_sandbox_profile("dev2").unwrap().unwrap().paths.len(),
            3
        );
        // The instance followed by ON UPDATE CASCADE…
        let instances = db.list_sandbox_instances_for_profile("dev2").unwrap();
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].external_id, "ctr-1");
        // …and so did both session references.
        assert_eq!(db.count_sessions_using_sandbox_profile("dev").unwrap(), 0);
        assert_eq!(db.count_sessions_using_sandbox_profile("dev2").unwrap(), 2);
        let backend_of = |id: &str| -> String {
            db.conn
                .query_row(
                    "SELECT backend_type FROM sessions WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .unwrap()
        };
        // Only the place-backed session's backend name carries the profile; the
        // rename must not invent one for the others.
        assert_eq!(backend_of("s-place"), "sandbox:dev2");
        assert_eq!(backend_of("s-policy"), "local-tmux");
        assert_eq!(backend_of("s-plain"), "local-tmux");
    }

    #[test]
    fn rename_reports_a_missing_profile_and_changes_nothing() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        seed_session(&db, "s1", "sandbox:dev", Some("dev"));

        assert!(!db.rename_sandbox_profile("ghost", "other").unwrap());

        assert!(db.get_sandbox_profile("other").unwrap().is_none());
        assert_eq!(db.count_sessions_using_sandbox_profile("dev").unwrap(), 1);
    }

    #[test]
    fn rename_onto_an_existing_name_is_refused_by_the_key() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        db.upsert_sandbox_profile(&profile("prod")).unwrap();

        assert!(db.rename_sandbox_profile("dev", "PROD").is_err());
        // The failed transaction rolled back: both profiles are still there.
        assert_eq!(db.list_sandbox_profile_names().unwrap(), ["dev", "prod"]);
    }

    #[test]
    fn delete_removes_the_profile_and_its_instance_records() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        db.upsert_sandbox_profile(&profile("keep")).unwrap();
        for (profile_name, id) in [("dev", "ctr-1"), ("dev", "ctr-2"), ("keep", "ctr-3")] {
            db.upsert_sandbox_instance(&SandboxInstance::new(
                profile_name,
                SandboxBackendKind::Docker,
                id,
                "running",
            ))
            .unwrap();
        }

        assert!(db.delete_sandbox_profile("dev").unwrap());

        assert!(db.get_sandbox_profile("dev").unwrap().is_none());
        assert!(db
            .list_sandbox_instances_for_profile("dev")
            .unwrap()
            .is_empty());
        // Only its own records went.
        let left = db.list_sandbox_instances().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].external_id, "ctr-3");
        // Deleting again is a no-op that says so.
        assert!(!db.delete_sandbox_profile("dev").unwrap());
    }

    #[test]
    fn delete_leaves_a_sessions_reference_dangling_on_purpose() {
        // Clearing the link would silently spawn that agent on the host next
        // launch; a dangling name fails loudly instead, which is the whole
        // point of not touching it.
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        seed_session(&db, "s1", "sandbox:dev", Some("dev"));
        assert_eq!(db.count_sessions_using_sandbox_profile("dev").unwrap(), 1);

        assert!(db.delete_sandbox_profile("dev").unwrap());

        let (backend, link): (String, Option<String>) = db
            .conn
            .query_row(
                "SELECT backend_type, sandbox_profile FROM sessions WHERE id = 's1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(backend, "sandbox:dev");
        assert_eq!(link.as_deref(), Some("dev"));
    }

    #[test]
    fn deleting_a_profile_cascades_to_instances_at_the_schema_level() {
        // `delete_sandbox_profile` clears the records itself; this proves the
        // foreign key is really enforced, so no other write path can strand an
        // instance row under a profile that no longer exists.
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        db.upsert_sandbox_instance(&SandboxInstance::new(
            "dev",
            SandboxBackendKind::Docker,
            "ctr-1",
            "running",
        ))
        .unwrap();

        db.conn
            .execute("DELETE FROM sandbox_profiles WHERE name = 'dev'", [])
            .unwrap();

        assert!(db.list_sandbox_instances().unwrap().is_empty());
    }

    #[test]
    fn an_instance_needs_a_profile_to_belong_to() {
        let db = Database::open_in_memory().unwrap();
        let orphan = SandboxInstance::new("ghost", SandboxBackendKind::Docker, "ctr-1", "running");
        assert!(db.upsert_sandbox_instance(&orphan).is_err());
    }

    #[test]
    fn instances_round_trip_and_refresh_in_place() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        db.upsert_sandbox_instance(&SandboxInstance::new(
            "dev",
            SandboxBackendKind::Docker,
            "ctr-1",
            "created",
        ))
        .unwrap();
        let first = db.list_sandbox_instances().unwrap().remove(0);
        assert_eq!(first.profile, "dev");
        assert_eq!(first.engine, SandboxBackendKind::Docker);
        assert_eq!(first.state, "created");
        assert!(first.created_at > 0);

        // The same place reported again moves its state, not its identity.
        db.upsert_sandbox_instance(&SandboxInstance::new(
            "dev",
            SandboxBackendKind::Docker,
            "ctr-1",
            "running",
        ))
        .unwrap();
        let all = db.list_sandbox_instances().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].state, "running");
        assert_eq!(all[0].created_at, first.created_at);
        assert!(all[0].last_used_at >= first.last_used_at);
    }

    #[test]
    fn the_same_id_on_another_engine_is_another_place() {
        // A docker container and a WSL distro can share a name; the key is the
        // pair, so one never silently overwrites the other into a leak.
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        for engine in [SandboxBackendKind::Docker, SandboxBackendKind::WslDistro] {
            db.upsert_sandbox_instance(&SandboxInstance::new("dev", engine, "friring-dev", "up"))
                .unwrap();
        }

        let all = db.list_sandbox_instances().unwrap();
        assert_eq!(all.len(), 2);
        // Stable ordering: profile, then engine, then id.
        assert_eq!(
            all.iter().map(|i| i.engine).collect::<Vec<_>>(),
            [SandboxBackendKind::Docker, SandboxBackendKind::WslDistro]
        );
    }

    #[test]
    fn instances_can_be_forgotten_one_at_a_time_or_per_profile() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        for id in ["ctr-1", "ctr-2"] {
            db.upsert_sandbox_instance(&SandboxInstance::new(
                "dev",
                SandboxBackendKind::Podman,
                id,
                "exited",
            ))
            .unwrap();
        }

        assert!(db
            .delete_sandbox_instance(SandboxBackendKind::Podman, "ctr-1")
            .unwrap());
        assert!(!db
            .delete_sandbox_instance(SandboxBackendKind::Podman, "ctr-1")
            .unwrap());
        assert_eq!(db.list_sandbox_instances().unwrap().len(), 1);

        assert_eq!(db.delete_sandbox_instances_for_profile("DEV").unwrap(), 1);
        assert!(db.list_sandbox_instances().unwrap().is_empty());
        // The profile itself is untouched by a prune.
        assert!(db.get_sandbox_profile("dev").unwrap().is_some());
    }

    #[test]
    fn a_row_friring_cannot_parse_still_lists() {
        // Hand-edited or imported junk in the enum and JSON columns decodes to
        // the defaults a fresh profile has, so the list modal can still show
        // (and the editor repair) the row instead of the whole collection
        // vanishing behind one bad value.
        let db = Database::open_in_memory().unwrap();
        db.conn
            .execute(
                "INSERT INTO sandbox_profiles
                    (name, backend, paths, network_mode, network_allow, network_deny,
                     prompt_new_domains, read_scope, memory_mb, cpus,
                     allow_unsandboxed_fallback, created_at, updated_at)
                 VALUES ('broken', 'firejail', 'not json', 'sometimes', '', '[oops',
                         1, 'everything', -1, 0, 0, 1, 1)",
                [],
            )
            .unwrap();

        let got = db.get_sandbox_profile("broken").unwrap().unwrap();
        assert_eq!(got.backend, SandboxBackendKind::Auto);
        assert_eq!(got.network_mode, NetworkMode::Allowlist);
        assert_eq!(got.read_scope, ReadScope::HostMinusSecrets);
        assert!(got.paths.is_empty());
        assert!(got.network_allow.is_empty() && got.network_deny.is_empty());
        // A negative limit is no limit, not a panic.
        assert_eq!(got.memory_mb, None);
        assert_eq!(got.cpus, Some(0));
        // Unsaveable until repaired, which is exactly the intended nudge.
        assert!(got.validate().is_err());
        assert_eq!(db.list_sandbox_profiles().unwrap().len(), 1);
    }

    #[test]
    fn profiles_survive_a_reopen() {
        // The collection is only useful if it outlives the process; exercise the
        // real file-backed open path, in a temp dir that is never a data dir.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("friring.db");
        {
            let db = Database::open(&path).unwrap();
            db.upsert_sandbox_profile(&profile("dev")).unwrap();
            db.upsert_sandbox_instance(&SandboxInstance::new(
                "dev",
                SandboxBackendKind::Docker,
                "ctr-1",
                "running",
            ))
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        let got = db.get_sandbox_profile("dev").unwrap().unwrap();
        assert_eq!(got.paths.len(), 3);
        assert_eq!(got.network_mode, NetworkMode::Allowlist);
        assert_eq!(db.list_sandbox_instances().unwrap().len(), 1);
    }
}
