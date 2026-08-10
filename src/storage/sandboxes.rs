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

use std::fmt;
use std::str::FromStr;

use rusqlite::{params, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::session::{
    NetworkMode, ReadScope, SandboxBackendKind, SandboxProfile, SANDBOX_BACKEND_PREFIX,
};
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
    /// Backend-defined lifecycle state. Free text on purpose: the vocabulary
    /// belongs to whichever backend wrote the row, and a place backend friring
    /// gains later must not need a migration to describe itself. The container
    /// backend writes `running` — the only state it ever hands back, because
    /// `ensure` returns when the place is up or not at all.
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

/// Decode an enum column, falling back to the type's default. Used for
/// [`SandboxInstance::engine`] only: an instance row is a *record of* a place
/// that already exists, never a policy anything is launched under, so an engine
/// name friring does not know costs the manager view a label and nothing else.
/// Profile columns go through [`RowDecoder`], which refuses to guess.
fn enum_from_db<T: FromStr + Default>(raw: &str) -> T {
    T::from_str(raw).unwrap_or_default()
}

/// Longest stored value an [`UndecodedColumn`] quotes back. A corrupt JSON
/// column has no length limit and the message built from it is a one-line
/// toast.
const MAX_QUOTED_VALUE: usize = 48;

/// A `sandbox_profiles` column whose stored value friring could not decode, and
/// the text it refused.
///
/// Recorded rather than raised as a read error: a corrupt row still has to
/// reach the list modal so it can be repaired or deleted, whereas a failed read
/// would take every *other* profile down with it. What must not happen is
/// **launching** it — see [`StoredSandboxProfile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndecodedColumn {
    /// The column, spelled as the schema spells it.
    pub column: &'static str,
    /// The stored text, truncated: the message built from it is a one-line
    /// toast and a corrupt JSON column has no length limit.
    pub value: String,
}

impl fmt::Display for UndecodedColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} = '{}'", self.column, self.value)
    }
}

/// Decodes one profile row, collecting the columns it could not read instead of
/// silently substituting a value for them.
#[derive(Debug, Default)]
struct RowDecoder {
    undecoded: Vec<UndecodedColumn>,
}

impl RowDecoder {
    /// Decode an enum column, or record it and fall back to `safe`.
    ///
    /// `safe` is the **narrowest** option the column has, not the type's
    /// `Default`: a policy friring cannot read must not decode to the wider
    /// grant, and a blind re-save from the editor must repair the row towards
    /// closed rather than towards open.
    fn enum_col<T: FromStr>(&mut self, column: &'static str, raw: &str, safe: T) -> T {
        match T::from_str(raw) {
            Ok(value) => value,
            Err(_) => {
                self.record(column, raw);
                safe
            }
        }
    }

    /// Decode a JSON list column. An empty column is an empty list — that is
    /// how a `NOT NULL` TEXT column spells "nothing here" — but anything else
    /// that does not parse is recorded, because an empty `network_deny` is a
    /// *wider* profile than whatever the row was trying to say.
    fn list_col<T: DeserializeOwned>(
        &mut self,
        column: &'static str,
        raw: Option<String>,
    ) -> Vec<T> {
        let Some(raw) = raw.filter(|s| !s.is_empty()) else {
            return Vec::new();
        };
        match serde_json::from_str(&raw) {
            Ok(list) => list,
            Err(_) => {
                self.record(column, &raw);
                Vec::new()
            }
        }
    }

    /// Decode an INTEGER limit column. `NULL` is "uncapped"; a value that is
    /// not a `u32` is recorded, because "uncapped" is the wider reading of a
    /// number friring could not use.
    fn limit_col(&mut self, column: &'static str, raw: Option<i64>) -> Option<u32> {
        let raw = raw?;
        match u32::try_from(raw) {
            Ok(value) => Some(value),
            Err(_) => {
                self.record(column, &raw.to_string());
                None
            }
        }
    }

    fn record(&mut self, column: &'static str, value: &str) {
        let mut quoted: String = value.chars().take(MAX_QUOTED_VALUE).collect();
        if value.chars().nth(MAX_QUOTED_VALUE).is_some() {
            quoted.push('\u{2026}');
        }
        self.undecoded.push(UndecodedColumn {
            column,
            value: quoted,
        });
    }
}

/// A profile as it came out of the database, with every column friring could
/// not decode recorded beside it.
///
/// Reading stays lenient so a corrupt row is repairable, but leniency has to
/// pick *a* value and the value it picks cannot be trusted — an unreadable
/// `read_scope` or `network_deny` would otherwise decode to the more permissive
/// option. [`undecoded`](Self::undecoded) is what makes the leniency safe: the
/// row still lists and still opens in the editor, and every launch path refuses
/// it (`docs/SANDBOX.md` §Failure modes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSandboxProfile {
    /// The decoded profile. Every column named in
    /// [`undecoded`](Self::undecoded) holds the narrowest value that column
    /// allows rather than what the row stored.
    pub profile: SandboxProfile,
    /// The columns friring could not read, in column order. Empty for a row
    /// that decoded completely.
    pub undecoded: Vec<UndecodedColumn>,
}

impl StoredSandboxProfile {
    /// A profile that decoded completely — what an in-memory profile, or one
    /// the editor just built, always is.
    pub fn intact(profile: SandboxProfile) -> Self {
        Self {
            profile,
            undecoded: Vec::new(),
        }
    }

    /// Whether every column decoded. Only an intact profile may be launched.
    pub fn is_intact(&self) -> bool {
        self.undecoded.is_empty()
    }

    /// The refusal a launch shows, naming each column that failed to decode and
    /// the value it refused; `None` for an intact profile.
    ///
    /// Launching is the one thing a corrupt profile must not do: its policy is
    /// partly unknown, and the parts friring substituted are the parts a user
    /// would least want guessed.
    pub fn launch_refusal(&self) -> Option<String> {
        if self.is_intact() {
            return None;
        }
        let columns: Vec<String> = self.undecoded.iter().map(ToString::to_string).collect();
        Some(format!(
            "Sandbox profile '{}' cannot be launched: friring could not decode {} — \
             open it in the profile list (Alt+S) and save it to repair",
            self.profile.name,
            columns.join(", ")
        ))
    }

    /// Just the failed column names, for a list row that has no width for their
    /// values.
    pub fn undecoded_columns(&self) -> Vec<&'static str> {
        self.undecoded.iter().map(|c| c.column).collect()
    }
}

/// Column list for profile SELECTs (keep in sync with [`map_profile`]).
const COLS: &str = "name, backend, paths, network_mode, network_allow, network_deny, \
    prompt_new_domains, read_scope, memory_mb, cpus, image, containerfile, \
    allow_unsandboxed_fallback, created_at, updated_at";

/// Column list for instance SELECTs (keep in sync with [`map_instance`]).
const INSTANCE_COLS: &str = "profile, engine, external_id, state, created_at, last_used_at";

fn map_profile(row: &rusqlite::Row) -> rusqlite::Result<StoredSandboxProfile> {
    let mut decoder = RowDecoder::default();
    let profile = SandboxProfile {
        name: row.get(0)?,
        // No backend is inherently the narrow one, so an unreadable pin falls
        // back to the ladder — and is refused at launch like every other
        // undecoded column, because a pin that silently became `auto` is the
        // "isolation technology the user did not choose" the design forbids.
        backend: decoder.enum_col(
            "backend",
            &row.get::<_, String>(1)?,
            SandboxBackendKind::Auto,
        ),
        paths: decoder.list_col("paths", row.get(2)?),
        network_mode: decoder.enum_col(
            "network_mode",
            &row.get::<_, String>(3)?,
            NetworkMode::None,
        ),
        network_allow: decoder.list_col("network_allow", row.get(4)?),
        network_deny: decoder.list_col("network_deny", row.get(5)?),
        prompt_new_domains: row.get::<_, i64>(6)? != 0,
        read_scope: decoder.enum_col(
            "read_scope",
            &row.get::<_, String>(7)?,
            ReadScope::Workspace,
        ),
        memory_mb: decoder.limit_col("memory_mb", row.get(8)?),
        cpus: decoder.limit_col("cpus", row.get(9)?),
        image: row.get(10)?,
        containerfile: row.get(11)?,
        allow_unsandboxed_fallback: row.get::<_, i64>(12)? != 0,
        created_at: row.get::<_, i64>(13)? as u64,
        updated_at: row.get::<_, i64>(14)? as u64,
    };
    Ok(StoredSandboxProfile {
        profile,
        undecoded: decoder.undecoded,
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
    ///
    /// A row friring could not fully decode is included, carrying its
    /// [`undecoded`](StoredSandboxProfile::undecoded) columns — the list is
    /// where a corrupt profile is repaired or deleted, so hiding it would
    /// strand it.
    pub fn list_sandbox_profiles(&self) -> rusqlite::Result<Vec<StoredSandboxProfile>> {
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
    ///
    /// Callers about to **launch** must check
    /// [`is_intact`](StoredSandboxProfile::is_intact) first: a row whose policy
    /// columns did not decode is repairable, not runnable.
    pub fn get_sandbox_profile(
        &self,
        name: &str,
    ) -> rusqlite::Result<Option<StoredSandboxProfile>> {
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

    /// Refresh an existing record's `state` and `last_used_at` — and **only** an
    /// existing one. Answers whether there was a row.
    ///
    /// The difference from
    /// [`upsert_sandbox_instance`](Self::upsert_sandbox_instance) is the whole
    /// point: a launch that reuses a place it did not create must not re-insert
    /// a row garbage collection has just deleted, which would resurrect a record
    /// of a container that is on its way out. Recording a *new* place is an
    /// upsert; saying "this one is still in use" is this.
    pub fn touch_sandbox_instance(
        &self,
        engine: SandboxBackendKind,
        external_id: &str,
        state: &str,
    ) -> rusqlite::Result<bool> {
        let now = current_time_millis() as i64;
        let updated = self.conn.execute(
            "UPDATE sandbox_instances SET state = ?3, last_used_at = ?4 \
             WHERE engine = ?1 AND external_id = ?2",
            params![engine.as_str(), external_id, state, now],
        )?;
        Ok(updated > 0)
    }

    /// The places recorded for one engine, oldest use first — the order garbage
    /// collection reclaims in, so the least recently used place goes first when
    /// a pass reclaims several.
    ///
    /// Per engine because that is what can be reconciled: the list of containers
    /// a `docker` can be asked for says nothing about a `podman` one, and a row
    /// whose engine is not running must not be read as a place that has
    /// vanished.
    pub fn list_sandbox_instances_for_engine(
        &self,
        engine: SandboxBackendKind,
    ) -> rusqlite::Result<Vec<SandboxInstance>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {INSTANCE_COLS} FROM sandbox_instances WHERE engine = ?1 \
             ORDER BY last_used_at, external_id"
        ))?;
        let rows = stmt.query_map(params![engine.as_str()], map_instance)?;
        rows.collect()
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

    /// Insert the hand-edited or imported row [`StoredSandboxProfile`] exists
    /// for: an unrecognised value in every enum column, a malformed JSON list,
    /// and an out-of-range limit.
    ///
    /// Test-only, and the only way to produce one —
    /// [`upsert_sandbox_profile`](Self::upsert_sandbox_profile) can only write
    /// values that decode, so nothing else in the crate can reach the state the
    /// launch refusal guards against.
    #[cfg(test)]
    pub(crate) fn insert_undecodable_sandbox_profile(&self, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO sandbox_profiles
                (name, backend, paths, network_mode, network_allow, network_deny,
                 prompt_new_domains, read_scope, memory_mb, cpus,
                 allow_unsandboxed_fallback, created_at, updated_at)
             VALUES (?1, 'firejail', 'not json', 'sometimes', '', '[oops',
                     1, 'everything', -1, 0, 0, 1, 1)",
            params![name],
        )?;
        Ok(())
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

        let got = db.get_sandbox_profile("dev").unwrap().unwrap().profile;
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

        let got = db.get_sandbox_profile("place").unwrap().unwrap().profile;
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

        let got = db.get_sandbox_profile("dev").unwrap().unwrap().profile;
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
        let first = db.get_sandbox_profile("dev").unwrap().unwrap().profile;
        assert!(first.created_at > 0);
        assert_eq!(first.created_at, first.updated_at);

        let mut edited = first.clone();
        edited.paths = vec![SandboxPath::workspace("~/dev/other")];
        edited.network_mode = NetworkMode::None;
        // A stale timestamp on the way in must not travel back into the row.
        edited.created_at = 1;
        edited.updated_at = 1;
        db.upsert_sandbox_profile(&edited).unwrap();

        let got = db.get_sandbox_profile("dev").unwrap().unwrap().profile;
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
        assert_eq!(all[0].profile.network_mode, NetworkMode::Full);
        assert_eq!(all[0].profile.name, "dev");
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
            db.get_sandbox_profile("dev2")
                .unwrap()
                .unwrap()
                .profile
                .paths
                .len(),
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

    /// The distinction garbage collection depends on: saying "still in use"
    /// must never *create* a record, or a launch that reuses a place would
    /// resurrect the row a collection pass had just deleted.
    #[test]
    fn touching_a_place_refreshes_a_record_and_never_invents_one() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        assert!(!db
            .touch_sandbox_instance(SandboxBackendKind::Podman, "ctr-1", "running")
            .unwrap());
        assert!(db.list_sandbox_instances().unwrap().is_empty());

        db.upsert_sandbox_instance(&SandboxInstance::new(
            "dev",
            SandboxBackendKind::Podman,
            "ctr-1",
            "created",
        ))
        .unwrap();
        let before = db.list_sandbox_instances().unwrap().remove(0);
        assert!(db
            .touch_sandbox_instance(SandboxBackendKind::Podman, "ctr-1", "running")
            .unwrap());
        let after = db.list_sandbox_instances().unwrap().remove(0);
        assert_eq!(after.state, "running");
        assert_eq!(after.created_at, before.created_at);
        assert!(after.last_used_at >= before.last_used_at);
    }

    /// Garbage collection reconciles one engine's rows against that engine's
    /// containers, so it asks for them least recently used first — and never
    /// sees another engine's, which it could not have asked about.
    #[test]
    fn instances_are_listed_per_engine_oldest_use_first() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_sandbox_profile(&profile("dev")).unwrap();
        for id in ["ctr-a", "ctr-b"] {
            db.upsert_sandbox_instance(&SandboxInstance::new(
                "dev",
                SandboxBackendKind::Podman,
                id,
                "running",
            ))
            .unwrap();
        }
        db.upsert_sandbox_instance(&SandboxInstance::new(
            "dev",
            SandboxBackendKind::Docker,
            "ctr-docker",
            "running",
        ))
        .unwrap();
        // `ctr-b` is used first and `ctr-a` last, so a list that came back
        // alphabetically rather than by use would read the other way round.
        db.touch_sandbox_instance(SandboxBackendKind::Podman, "ctr-b", "running")
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        db.touch_sandbox_instance(SandboxBackendKind::Podman, "ctr-a", "running")
            .unwrap();

        let podman = db
            .list_sandbox_instances_for_engine(SandboxBackendKind::Podman)
            .unwrap();
        assert_eq!(
            podman
                .iter()
                .map(|i| i.external_id.as_str())
                .collect::<Vec<_>>(),
            ["ctr-b", "ctr-a"]
        );
        let docker = db
            .list_sandbox_instances_for_engine(SandboxBackendKind::Docker)
            .unwrap();
        assert_eq!(docker.len(), 1);
        assert_eq!(docker[0].external_id, "ctr-docker");
        assert!(db
            .list_sandbox_instances_for_engine(SandboxBackendKind::WslDistro)
            .unwrap()
            .is_empty());
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
        // Junk in the enum and JSON columns must not hide the row: the list
        // modal is where it gets repaired or deleted, and a read error would
        // take every *other* profile down with it.
        let db = Database::open_in_memory().unwrap();
        db.insert_undecodable_sandbox_profile("broken").unwrap();

        let got = db.get_sandbox_profile("broken").unwrap().unwrap();
        assert!(got.profile.paths.is_empty());
        assert!(got.profile.network_allow.is_empty() && got.profile.network_deny.is_empty());
        // A negative limit is no limit, not a panic.
        assert_eq!(got.profile.memory_mb, None);
        assert_eq!(got.profile.cpus, Some(0));
        // Unsaveable until repaired, which is exactly the intended nudge.
        assert!(got.profile.validate().is_err());
        assert_eq!(db.list_sandbox_profiles().unwrap().len(), 1);
    }

    #[test]
    fn an_unreadable_column_decodes_to_the_narrower_option_and_is_recorded() {
        // The bug this pins: leniency has to pick a value, and picking the
        // type's default handed an unreadable `read_scope` the *wider* of its
        // two options and an unreadable `network_deny` an empty deny list.
        let db = Database::open_in_memory().unwrap();
        db.insert_undecodable_sandbox_profile("broken").unwrap();

        let got = db.get_sandbox_profile("broken").unwrap().unwrap();
        assert_eq!(got.profile.read_scope, ReadScope::Workspace);
        assert_eq!(got.profile.network_mode, NetworkMode::None);
        assert_eq!(got.profile.backend, SandboxBackendKind::Auto);

        assert!(!got.is_intact());
        assert_eq!(
            got.undecoded_columns(),
            [
                "backend",
                "paths",
                "network_mode",
                "network_deny",
                "read_scope",
                "memory_mb"
            ]
        );
        // An empty column is "nothing here", not a decode failure.
        assert!(!got.undecoded_columns().contains(&"network_allow"));

        // The refusal names the profile and every value it could not read, so
        // the toast says what to repair.
        let refusal = got.launch_refusal().expect("a corrupt row cannot launch");
        assert!(refusal.contains("'broken'"), "{refusal}");
        assert!(refusal.contains("read_scope = 'everything'"), "{refusal}");
        assert!(refusal.contains("network_deny = '[oops'"), "{refusal}");
    }

    #[test]
    fn an_intact_row_carries_no_decode_failures() {
        let db = Database::open_in_memory().unwrap();
        let mut p = profile("dev");
        p.network_deny = vec!["gist.github.com".into()];
        p.read_scope = ReadScope::HostMinusSecrets;
        db.upsert_sandbox_profile(&p).unwrap();

        let got = db.get_sandbox_profile("dev").unwrap().unwrap();
        assert!(got.is_intact());
        assert!(got.launch_refusal().is_none());
        // A value friring *can* read is never narrowed on the way out.
        assert_eq!(got.profile.read_scope, ReadScope::HostMinusSecrets);
    }

    #[test]
    fn a_quoted_value_cannot_run_away_with_the_message() {
        // The refusal is a one-line toast and the column it quotes is free
        // text, so an enormous stored value is truncated rather than pasted.
        let db = Database::open_in_memory().unwrap();
        let huge = "x".repeat(4096);
        db.conn
            .execute(
                "INSERT INTO sandbox_profiles
                    (name, backend, paths, network_mode, network_allow, network_deny,
                     prompt_new_domains, read_scope, allow_unsandboxed_fallback,
                     created_at, updated_at)
                 VALUES ('huge', 'auto', ?1, 'none', '[]', '[]', 0, 'workspace', 0, 1, 1)",
                params![huge],
            )
            .unwrap();

        let got = db.get_sandbox_profile("huge").unwrap().unwrap();
        assert_eq!(got.undecoded_columns(), ["paths"]);
        let quoted = &got.undecoded[0].value;
        assert_eq!(quoted.chars().count(), MAX_QUOTED_VALUE + 1);
        assert!(quoted.ends_with('\u{2026}'));
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
        let got = db.get_sandbox_profile("dev").unwrap().unwrap().profile;
        assert_eq!(got.paths.len(), 3);
        assert_eq!(got.network_mode, NetworkMode::Allowlist);
        assert_eq!(db.list_sandbox_instances().unwrap().len(), 1);
    }
}
