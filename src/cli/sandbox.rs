//! `friring-cli sandbox` — the sandbox feature's headless surface.
//!
//! Two kinds of command live here, and the difference is the database.
//!
//! **Inside the boundary.** [`Action::Relay`] runs *in* a sandbox, where
//! ADR-29 keeps the database out on purpose. A sandbox in its own network
//! namespace has no route to the host's loopback, so it reaches the egress
//! proxy through a bind-mounted unix socket — and no mainstream HTTP or SOCKS
//! client can dial one, because `HTTP_PROXY` and `ALL_PROXY` take a host and a
//! port. The relay closes that gap by offering a TCP endpoint inside the
//! namespace and forwarding each connection to the socket (`docs/SANDBOX.md`
//! §Reaching the proxy).
//!
//! ```text
//! agent  →  127.0.0.1:PORT   (the sandbox's own loopback)
//!        →  friring-cli sandbox relay
//!        →  /…/proxy.sock    (bind-mounted from the host)
//!        →  friring proxy    →  policy  →  upstream
//! ```
//!
//! Two properties are the point of running it there rather than teaching the
//! proxy to do it: the relay **holds no credential and makes no policy
//! decision** — the proxy still demands its token at the far end, and the
//! allowlist is still applied outside the boundary — and it never parses a
//! byte, so `CONNECT` and SOCKS5 both cross unchanged.
//!
//! **Outside the boundary.** Everything else — listing and inspecting
//! profiles, removing one, reclaiming places, moving profiles between machines
//! as TOML, and storing the `env-token` value — is host-side management that
//! reads and writes friring's own database.
//!
//! # Dispatched before the database
//!
//! `friring-cli`'s `main` calls [`run_before_database`] immediately after
//! parsing: it answers `Some` for the relay and `None` for every host-side
//! command, so the relay never opens a database (which would either create a
//! stray one inside the boundary or fail and leave the sandbox with no egress)
//! and the management commands take the ordinary [`crate::cli::run`] path with
//! the database already open. [`run`] refuses the relay for the same reason
//! rather than serving it with a database in hand.
//!
//! # Nothing here prints a secret
//!
//! `sandbox token` stores a value that must never reach a command line — argv
//! is world-readable through `/proc/<pid>/cmdline` on Linux — so the value is
//! taken from stdin or from a no-echo prompt, is held in
//! [`Secret`](crate::sandbox::auth::keychain::Secret) (which has no `Display`),
//! and is never rendered, logged or quoted back in an error. `token list`
//! answers *whether* an entry exists and never what it holds, and a profile
//! export carries no credential because a profile holds none.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cli::output::{self, CommandOutput};
use crate::session::{AgentDef, SandboxBackendKind, SandboxProfile, SandboxShape};
use crate::storage::sandboxes::StoredSandboxProfile;
use crate::storage::Database;

/// Sandbox management, plus the one command friring composes for the inside of
/// a boundary it built.
#[derive(Subcommand, Debug)]
pub enum Action {
    /// Forward a TCP port inside the sandbox to the egress proxy's unix socket.
    ///
    /// Not for interactive use: friring composes this itself, inside a boundary
    /// it built. It is also the one subcommand that never opens the database.
    Relay {
        /// Address to listen on, inside the sandbox's own network namespace.
        /// Port `0` binds an ephemeral port and prints the result.
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: SocketAddr,
        /// The proxy's unix socket, at its path inside the sandbox.
        #[arg(long)]
        socket: PathBuf,
    },
    /// List sandbox profiles, the backend each resolves to here, and their
    /// live places.
    List {
        /// One row per live place instead of one row per profile.
        #[arg(long)]
        instances: bool,
    },
    /// Show one profile in full: paths, egress rules, limits and its places.
    Show {
        /// Profile name (case-insensitive).
        name: String,
    },
    /// Delete a profile. Sessions that reference it keep the reference and
    /// refuse to relaunch, so deleting one that is in use needs `--force`.
    Rm {
        /// Profile name (case-insensitive).
        name: String,
        /// Delete even while sessions still reference it.
        #[arg(long)]
        force: bool,
    },
    /// Reclaim the places nothing needs any more — the background pass, run now.
    ///
    /// Only containers friring created are ever named, and a place a live
    /// session could be in is never taken.
    Prune {
        /// Restrict the pass to one profile's places.
        #[arg(long)]
        profile: Option<String>,
        /// Report what would be reclaimed without removing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write profiles out as TOML, for another machine or for version control.
    ///
    /// The document is the human rendering, so a redirected stdout gets JSON
    /// like every other command (`--text` for the plain document, or `--output`
    /// to write the file).
    Export {
        /// Profile to export. Omit to export every profile.
        name: Option<String>,
        /// Write to this file instead of stdout (refuses to overwrite).
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
    /// Read profiles from a TOML file written by `sandbox export`.
    ///
    /// Every profile is validated — including the path refusals a launch makes
    /// — before any of them is written, so an unsafe or malformed entry stores
    /// nothing at all.
    Import {
        /// The TOML file to read.
        path: PathBuf,
        /// Overwrite profiles that already exist under the same name.
        #[arg(long)]
        replace: bool,
    },
    /// Manage the long-lived tokens `env-token` injects into a place.
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
}

/// `sandbox token` — friring's own keychain entry, and nothing else's.
#[derive(Subcommand, Debug)]
pub enum TokenAction {
    /// Store a token for an agent. The value is read from stdin, or prompted
    /// for without echo — never taken as an argument, because argv is readable
    /// by other processes.
    Set {
        /// Agent name from `agents.toml` (its credential family is used).
        agent: String,
        /// Variable to store it under. Omit when the agent declares exactly one.
        variable: Option<String>,
    },
    /// Forget a stored token.
    Rm {
        /// Agent name from `agents.toml` (its credential family is used).
        agent: String,
        /// Variable the token is stored under. Omit when the agent declares
        /// exactly one.
        variable: Option<String>,
    },
    /// List the token variables agents declare and whether friring holds one —
    /// never the value.
    List,
}

/// Refusal shown if the relay ever reaches the database-bearing path.
///
/// Unreachable while `main` calls [`run_before_database`] first, and a refusal
/// rather than a working relay if that call is ever removed: a relay served
/// from here has a database open in a process the sandbox talks to, which is
/// the one thing ADR-29 forbids. Failing closed costs the sandbox its egress
/// and says exactly why.
const RELAY_OFF_THE_EARLY_PATH: &str =
    "`sandbox relay` runs inside a sandbox and must be dispatched before the database is \
     opened (ADR-29); friring-cli's main no longer does that, so the relay was refused \
     rather than served with a database open";

/// Run the one sandbox command that must not open the database, or answer
/// `None` for a command that needs one.
///
/// Called by `friring-cli`'s `main` immediately after parsing and before the
/// settings/database block. `None` means "this is host-side management, take
/// the normal path" — which is where the output format, the JSON rendering and
/// the exit code come from.
pub fn run_before_database(command: &crate::cli::Command) -> Option<Result<(), String>> {
    database_free(command).map(|(listen, socket)| relay(listen, socket))
}

/// The decision behind [`run_before_database`], separated from carrying it out
/// so it can be asserted on: the relay serves until its sandbox is torn down,
/// and a test that called the runner would never come back.
fn database_free(command: &crate::cli::Command) -> Option<(SocketAddr, &Path)> {
    match command {
        crate::cli::Command::Sandbox {
            action: Action::Relay { listen, socket },
        } => Some((*listen, socket.as_path())),
        _ => None,
    }
}

/// Run a host-side sandbox command against `db`.
///
/// # Errors
///
/// The profile does not exist, a file could not be read or written, an import
/// carried something a launch would refuse, or a credential store would not
/// answer. Never carries a token: see the module docs.
pub fn run(action: Action, db: &Database) -> Result<CommandOutput, String> {
    match action {
        Action::Relay { .. } => Err(RELAY_OFF_THE_EARLY_PATH.to_string()),
        Action::List { instances } => {
            list(db, crate::sandbox::SandboxHost::local_shared(), instances)
        }
        Action::Show { name } => show(db, crate::sandbox::SandboxHost::local_shared(), &name),
        Action::Rm { name, force } => remove(db, &name, force),
        Action::Prune { profile, dry_run } => prune(
            db,
            crate::sandbox::SandboxHost::local_shared(),
            profile.as_deref(),
            dry_run,
        ),
        Action::Export { name, output } => export(db, name.as_deref(), output.as_deref()),
        Action::Import { path, replace } => import(db, &path, replace),
        Action::Token { action } => token(
            action,
            crate::agent::agent_config::load_or_seed().agents,
            crate::sandbox::auth::keychain::system_store(),
        ),
    }
}

#[cfg(unix)]
fn relay(listen: SocketAddr, socket: &std::path::Path) -> Result<(), String> {
    // A runtime of its own, and the smallest one: this process exists to move
    // bytes between two sockets, and it is started by a sandbox launch that has
    // no runtime to inherit.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| format!("cannot start the relay runtime: {e}"))?;

    runtime.block_on(async move {
        let config = crate::proxy::RelayConfig::new(listen, socket);
        let mut relay = crate::proxy::Relay::start(config)
            .await
            .map_err(|e| format!("{e:#}"))?;
        // The bound address, so `--listen 127.0.0.1:0` is discoverable by
        // whoever asked for it. Inside a sandbox launch this goes to
        // `/dev/null`: the pane belongs to the agent's own display.
        println!("{}", relay.addr());
        tokio::select! {
            () = relay.wait() => {}
            result = tokio::signal::ctrl_c() => {
                if let Err(e) = result {
                    return Err(format!("cannot listen for Ctrl+C: {e}"));
                }
            }
        }
        Ok(())
    })
}

/// Unix sockets are what the relay exists to bridge to, so there is nothing to
/// bridge on a platform without them: a Windows host reaches a sandbox through
/// WSL or a container, and the relay runs *inside* that, where sockets exist.
#[cfg(not(unix))]
fn relay(_listen: SocketAddr, socket: &std::path::Path) -> Result<(), String> {
    Err(format!(
        "the sandbox relay needs unix sockets (asked for `{}`); on Windows a sandbox is reached \
         through WSL or a container, and the relay runs inside it",
        socket.display()
    ))
}

// ── Profiles and places ─────────────────────────────────────────────────

/// `sandbox list`.
fn list(
    db: &Database,
    host: &crate::sandbox::SandboxHost,
    instances: bool,
) -> Result<CommandOutput, String> {
    let stored = db
        .list_sandbox_profiles()
        .map_err(|e| format!("Failed to list sandbox profiles: {e}"))?;
    let mut rows = Vec::with_capacity(stored.len());
    for profile in &stored {
        let places = places_of(db, &profile.profile.name)?;
        rows.push((profile, places));
    }

    if instances {
        let listed: Vec<Value> = rows
            .iter()
            .flat_map(|(profile, places)| {
                places
                    .iter()
                    .map(move |place| place_json(&profile.profile.name, place))
            })
            .collect();
        let table = output::table(
            &["PROFILE", "ENGINE", "ID", "STATE"],
            &rows
                .iter()
                .flat_map(|(profile, places)| {
                    places.iter().map(move |place| {
                        vec![
                            profile.profile.name.clone(),
                            place.engine.to_string(),
                            place.external_id.clone(),
                            place.state.clone(),
                        ]
                    })
                })
                .collect::<Vec<_>>(),
        );
        let human = match listed.is_empty() {
            true => "No sandbox places recorded".to_string(),
            false => table,
        };
        return Ok(CommandOutput::new(Value::Array(listed), human));
    }

    let json: Vec<Value> = rows
        .iter()
        .map(|(profile, places)| profile_json(host, profile, places))
        .collect();
    let human = match rows.is_empty() {
        true => "No sandbox profiles".to_string(),
        false => output::table(
            &["NAME", "BACKEND", "PATHS", "NETWORK", "PLACES", "NOTES"],
            &rows
                .iter()
                .map(|(profile, places)| {
                    let p = &profile.profile;
                    vec![
                        p.name.clone(),
                        backend_label(host, p.backend),
                        p.paths.len().to_string(),
                        p.network_mode.to_string(),
                        places_label(places),
                        output::dash(notes(host, profile).as_deref()),
                    ]
                })
                .collect::<Vec<_>>(),
        ),
    };
    Ok(CommandOutput::new(Value::Array(json), human))
}

/// `sandbox show`.
fn show(
    db: &Database,
    host: &crate::sandbox::SandboxHost,
    name: &str,
) -> Result<CommandOutput, String> {
    let stored = load_profile(db, name)?;
    let places = places_of(db, &stored.profile.name)?;
    let p = &stored.profile;
    let paths: Vec<String> = p
        .paths
        .iter()
        .map(|path| format!("{} ({})", path.path, path.mode))
        .collect();
    let mut pairs = vec![("name", p.name.clone())];
    // Second line, not a footnote: every value below a damaged row is partly
    // friring's own narrowest substitution, and a reader who missed that would
    // read the substitutions as the profile.
    if !stored.is_intact() {
        pairs.push((
            "UNREADABLE",
            format!(
                "friring could not decode {} — the values below are its own narrowest \
                 substitutions, and this profile will not launch until it is re-saved",
                stored.undecoded_columns().join(", ")
            ),
        ));
    }
    pairs.extend([
        ("backend", backend_label(host, p.backend)),
        ("paths", join_or_dash(&paths)),
        ("network", p.network_mode.to_string()),
        ("allow", join_or_dash(&p.network_allow)),
        ("deny", join_or_dash(&p.network_deny)),
        ("prompt new domains", p.prompt_new_domains.to_string()),
        ("read scope", p.read_scope.as_str().to_string()),
        (
            "memory",
            p.memory_mb.map_or("-".to_string(), |mb| format!("{mb} MB")),
        ),
        (
            "cpus",
            p.cpus.map_or("-".to_string(), |cpus| cpus.to_string()),
        ),
        ("image", output::dash(p.image.as_deref())),
        ("containerfile", output::dash(p.containerfile.as_deref())),
        (
            "unsandboxed fallback",
            p.allow_unsandboxed_fallback.to_string(),
        ),
        (
            "places",
            join_or_dash(
                &places
                    .iter()
                    .map(|place| {
                        format!("{} {} ({})", place.engine, place.external_id, place.state)
                    })
                    .collect::<Vec<_>>(),
            ),
        ),
        (
            "sessions",
            db.count_sessions_using_sandbox_profile(&p.name)
                .unwrap_or(0)
                .to_string(),
        ),
    ]);
    if let Some(reason) = unavailable_reason(host, p.backend) {
        pairs.push(("unavailable here", reason));
    }
    let human = output::kv(&pairs);
    Ok(CommandOutput::new(
        profile_json(host, &stored, &places),
        human,
    ))
}

/// `sandbox rm`.
///
/// Mirrors the profile list's delete (`crate::app::sandbox`): the sessions that
/// name a deleted profile keep the reference on purpose — clearing it would
/// silently relaunch those agents on the host — so this refuses rather than
/// stranding them unasked, and `--force` is the deliberate answer.
fn remove(db: &Database, name: &str, force: bool) -> Result<CommandOutput, String> {
    let stored = load_profile(db, name)?;
    let name = stored.profile.name;
    let in_use = db
        .count_sessions_using_sandbox_profile(&name)
        .map_err(|e| format!("Failed to count sessions using '{name}': {e}"))?;
    if in_use > 0 && !force {
        return Err(format!(
            "Sandbox profile '{name}' is still referenced by {in_use} session(s), which will \
             refuse to relaunch without it. Re-run with --force to delete it anyway"
        ));
    }
    if !db
        .delete_sandbox_profile(&name)
        .map_err(|e| format!("Failed to delete sandbox profile '{name}': {e}"))?
    {
        return Err(format!("Sandbox profile '{name}' not found"));
    }
    // The profile's tree — its synthetic home, the login inside it, and the
    // marker that keeps one credential to one boundary — goes with the profile,
    // but only once nothing is running out of it. A tree removed under a live
    // place takes `$HOME` away from an agent mid-turn and unlinks the egress
    // sockets its siblings talk through; the reclaiming pass collects it after
    // the container instead. Same rule, same order as the TUI's delete.
    let reclaimed = in_use == 0;
    if reclaimed {
        crate::sandbox::dirs::cleanup_place(&name);
        crate::sandbox::auth::release_seeds(&name);
    }
    let summary = match in_use {
        0 => format!("Sandbox profile '{name}' deleted"),
        n => format!(
            "Sandbox profile '{name}' deleted — {n} session(s) still reference it and will \
             refuse to relaunch; its sandbox home and login are kept until they stop"
        ),
    };
    Ok(CommandOutput::from_summary(json!({
        "name": name,
        "deleted": true,
        "sessions_affected": in_use,
        "tree_removed": reclaimed,
        "summary": summary,
    })))
}

/// `sandbox prune` — the reclaiming pass, run now.
///
/// The decision is [`gc_plan`](crate::sandbox::container::gc_plan), the same
/// pure function the TUI's background pass uses, so "would this reap a place a
/// session is in?" has one answer rather than two. What differs is the
/// protection: a `friring-cli` drives no session, so it cannot know which
/// container a live session is running in and protects **every** container of
/// every profile with a live session by name — the conservative half of the
/// TUI's rule, applied always.
fn prune(
    db: &Database,
    host: &crate::sandbox::SandboxHost,
    only: Option<&str>,
    dry_run: bool,
) -> Result<CommandOutput, String> {
    let only = only.map(|name| name.trim().to_ascii_lowercase());
    let protected = protected_profiles(db)?;
    let stored = db
        .list_sandbox_profiles()
        .map_err(|e| format!("Failed to list sandbox profiles: {e}"))?;
    // A row friring could not decode is repairable rather than runnable, and
    // guessing at its spec here would compare a place against a policy nobody
    // wrote — so it is left out, which keeps its place alive.
    let profiles: Vec<SandboxProfile> = stored
        .into_iter()
        .filter(StoredSandboxProfile::is_intact)
        .map(|stored| stored.profile)
        .filter(|profile| profile.backend.shape() != Some(SandboxShape::Policy))
        .collect();

    let mut removed: Vec<Value> = Vec::new();
    let mut forgotten: Vec<Value> = Vec::new();
    let mut adopted: Vec<Value> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for engine in crate::sandbox::PLACE_KINDS.iter().copied() {
        let Some(backend) = host.place(engine) else {
            continue;
        };
        // An engine that will not answer is holding nothing *as far as this
        // pass can tell*, which is not the same as holding nothing: forgetting
        // its rows would lose the ids of containers nothing else can find. One
        // that is not installed at all is different — it created nothing, so it
        // is skipped silently rather than reported as an engine that failed.
        let live = match crate::sandbox::live_places_here(backend) {
            Ok(Some(live)) => live,
            Ok(None) => continue,
            Err(e) => {
                skipped.push(format!("{engine}: {e}"));
                continue;
            }
        };
        let rows = db
            .list_sandbox_instances_for_engine(engine)
            .map_err(|e| format!("Failed to list {engine} sandbox instances: {e}"))?;
        let records: Vec<crate::sandbox::container::InstanceRecord> = rows
            .iter()
            .map(|row| crate::sandbox::container::InstanceRecord {
                profile: row.profile.clone(),
                external_id: row.external_id.clone(),
                last_used_at: row.last_used_at,
            })
            .collect();
        let mut current = BTreeMap::new();
        let mut unplannable: BTreeSet<String> = BTreeSet::new();
        for profile in &profiles {
            match backend.current_spec(profile) {
                Some(spec) => {
                    current.insert(profile.name.clone(), spec);
                }
                // No opinion about this profile, which would otherwise read as
                // "the profile is gone" and reclaim its places.
                None => {
                    unplannable.insert(profile.name.to_ascii_lowercase());
                }
            }
        }
        let in_use: Vec<String> = live
            .iter()
            .filter(|container| {
                container.profile.as_ref().is_some_and(|profile| {
                    let key = profile.to_ascii_lowercase();
                    protected.contains(&key) || unplannable.contains(&key)
                })
            })
            .map(|container| container.id.clone())
            .collect();

        let mut plan = crate::sandbox::container::gc_plan(crate::sandbox::container::GcInput {
            records: &records,
            live: &live,
            current: &current,
            in_use: &in_use,
            now: crate::sync::current_time_millis(),
            // Idleness never reclaims a place: it is the environment a session
            // lives in, not a cache, and "unused for a while" is
            // indistinguishable from "the user is on holiday".
            idle_after_ms: None,
        });
        if let Some(only) = &only {
            let owner = |id: &str| -> Option<String> {
                live.iter()
                    .find(|container| container.id == id)
                    .and_then(|container| container.profile.clone())
                    .or_else(|| {
                        records
                            .iter()
                            .find(|record| record.external_id == id)
                            .map(|record| record.profile.clone())
                    })
            };
            let mine = |id: &String| {
                owner(id).is_some_and(|profile| profile.to_ascii_lowercase() == *only)
            };
            plan.remove.retain(&mine);
            plan.forget.retain(&mine);
            plan.adopt.retain(|container| {
                container
                    .profile
                    .as_ref()
                    .is_some_and(|profile| profile.to_ascii_lowercase() == *only)
            });
        }

        if dry_run {
            removed.extend(
                plan.remove
                    .iter()
                    .map(|id| json!({ "engine": engine.as_str(), "id": id })),
            );
            forgotten.extend(
                plan.forget
                    .iter()
                    .map(|id| json!({ "engine": engine.as_str(), "id": id })),
            );
            adopted.extend(
                plan.adopt
                    .iter()
                    .map(|container| json!({ "engine": engine.as_str(), "id": container.id })),
            );
            continue;
        }

        failures.extend(backend.reap(&plan));
        // What was actually reclaimed is decided by asking the engine again
        // rather than by assuming the removals worked: a row forgotten for a
        // container that is still there loses the only id anything has for it,
        // which is the leak `sandbox_instances` exists to prevent. Same rule as
        // the TUI's pass (`crate::app::sandbox`'s place job).
        match backend.live_places() {
            Ok(after) => {
                let gone = |id: &String| !after.iter().any(|container| container.id == *id);
                plan.remove.retain(gone);
                plan.forget.retain(gone);
            }
            // The engine reaped and then would not say what it still holds, so
            // nothing here can be called gone. Every row stays, which costs a
            // stale row the next pass clears and keeps every id findable.
            Err(e) => {
                failures.push(format!("{engine}: {e}"));
                plan.remove.clear();
                plan.forget.clear();
            }
        }
        removed.extend(
            plan.remove
                .iter()
                .map(|id| json!({ "engine": engine.as_str(), "id": id })),
        );
        for id in &plan.forget {
            match db.delete_sandbox_instance(engine, id) {
                Ok(_) => forgotten.push(json!({ "engine": engine.as_str(), "id": id })),
                Err(e) => failures.push(format!("{id}: the record could not be forgotten: {e}")),
            }
        }
        for container in plan.adopt {
            let Some(profile) = container.profile else {
                continue;
            };
            let row = crate::storage::sandboxes::SandboxInstance::new(
                profile,
                engine,
                container.id,
                crate::sandbox::container::INSTANCE_STATE_RUNNING,
            );
            match db.upsert_sandbox_instance(&row) {
                Ok(()) => adopted.push(json!({ "engine": engine.as_str(), "id": row.external_id })),
                Err(e) => failures.push(format!("{}: could not be recorded: {e}", row.external_id)),
            }
        }
    }

    let verb = if dry_run {
        "would reclaim"
    } else {
        "reclaimed"
    };
    let mut summary = format!(
        "{verb} {} place(s), forgot {} record(s), adopted {}",
        removed.len(),
        forgotten.len(),
        adopted.len()
    );
    for skip in &skipped {
        summary.push_str(&format!("; skipped {skip}"));
    }
    let json = json!({
        "dry_run": dry_run,
        "removed": removed,
        "forgotten": forgotten,
        "adopted": adopted,
        "skipped": skipped,
        "failures": failures,
        "summary": summary,
    });
    let human = std::iter::once(summary.clone())
        .chain(failures.iter().map(|f| format!("failed: {f}")))
        .collect::<Vec<_>>()
        .join("\n");
    match failures.is_empty() {
        true => Ok(CommandOutput::new(json, human)),
        false => Ok(CommandOutput::failed(
            json,
            human,
            format!("{} place(s) could not be reclaimed", failures.len()),
        )),
    }
}

/// The profiles whose places this pass must leave alone: every profile a live
/// session is registered under.
fn protected_profiles(db: &Database) -> Result<BTreeSet<String>, String> {
    Ok(db
        .list_active_sessions()
        .map_err(|e| format!("Failed to read sessions: {e}"))?
        .into_iter()
        .filter_map(|session| {
            crate::session::sandbox_backend_profile(&session.backend_type)
                .map(str::to_ascii_lowercase)
        })
        .collect())
}

/// One profile, or a refusal naming it. Trimmed and case-insensitive, like
/// every other lookup by profile name.
fn load_profile(db: &Database, name: &str) -> Result<StoredSandboxProfile, String> {
    db.get_sandbox_profile(name)
        .map_err(|e| format!("Failed to read sandbox profile '{name}': {e}"))?
        .ok_or_else(|| format!("Sandbox profile '{}' not found", name.trim()))
}

fn places_of(
    db: &Database,
    profile: &str,
) -> Result<Vec<crate::storage::sandboxes::SandboxInstance>, String> {
    db.list_sandbox_instances_for_profile(profile)
        .map_err(|e| format!("Failed to list places for '{profile}': {e}"))
}

/// The backend column: the profile's own choice, and what `auto` resolves to
/// here. Mirrors the profile list's row (`crate::app::sandbox`), which the CLI
/// cannot call into.
fn backend_label(host: &crate::sandbox::SandboxHost, requested: SandboxBackendKind) -> String {
    match (requested, resolved_backend(host, requested)) {
        (SandboxBackendKind::Auto, Some(resolved)) => format!("auto → {resolved}"),
        _ => requested.to_string(),
    }
}

/// What `requested` resolves to on this host, or `None` when nothing on the
/// ladder is available. Only `auto` needs resolving — a pinned backend is
/// already the answer, and a pin never falls back.
fn resolved_backend(
    host: &crate::sandbox::SandboxHost,
    requested: SandboxBackendKind,
) -> Option<SandboxBackendKind> {
    match requested {
        SandboxBackendKind::Auto => host.select(requested).chosen,
        explicit => Some(explicit),
    }
}

/// The row's warning column: a profile that did not decode, or a backend this
/// host cannot offer. `None` when there is nothing to say.
fn notes(host: &crate::sandbox::SandboxHost, stored: &StoredSandboxProfile) -> Option<String> {
    if !stored.is_intact() {
        return Some(format!(
            "unreadable {} — will not launch until repaired",
            stored.undecoded_columns().join(", ")
        ));
    }
    unavailable_reason(host, stored.profile.backend)
}

/// Why the backend a profile would run on cannot be used here, or `None` when
/// it can. A pinned backend reports the probe's own sentence; an exhausted
/// `auto` ladder reports every rung's.
fn unavailable_reason(
    host: &crate::sandbox::SandboxHost,
    requested: SandboxBackendKind,
) -> Option<String> {
    match requested {
        SandboxBackendKind::Auto => match host.select(requested).chosen {
            Some(_) => None,
            None => Some(
                host.select(requested)
                    .rejection_summary()
                    .replace('\n', "; "),
            ),
        },
        explicit => {
            let availability = host.probe(explicit);
            (!availability.is_available()).then(|| availability.message())
        }
    }
}

fn places_label(places: &[crate::storage::sandboxes::SandboxInstance]) -> String {
    match places {
        [] => "-".to_string(),
        [one] => one.state.clone(),
        many => format!("{} places", many.len()),
    }
}

fn join_or_dash(items: &[String]) -> String {
    match items.is_empty() {
        true => "-".to_string(),
        false => items.join(", "),
    }
}

fn place_json(profile: &str, place: &crate::storage::sandboxes::SandboxInstance) -> Value {
    json!({
        "profile": profile,
        "engine": place.engine.as_str(),
        "id": place.external_id,
        "state": place.state,
        "created_at": place.created_at,
        "last_used_at": place.last_used_at,
    })
}

fn profile_json(
    host: &crate::sandbox::SandboxHost,
    stored: &StoredSandboxProfile,
    places: &[crate::storage::sandboxes::SandboxInstance],
) -> Value {
    let p = &stored.profile;
    let resolved = resolved_backend(host, p.backend);
    json!({
        "name": p.name,
        "backend": p.backend.as_str(),
        "resolved_backend": resolved.map(SandboxBackendKind::as_str),
        "shape": resolved
            .and_then(SandboxBackendKind::shape)
            .map(SandboxShape::as_str),
        "paths": p.paths.iter().map(|path| json!({
            "path": path.path,
            "mode": path.mode.as_str(),
        })).collect::<Vec<_>>(),
        "network_mode": p.network_mode.as_str(),
        "network_allow": p.network_allow,
        "network_deny": p.network_deny,
        "prompt_new_domains": p.prompt_new_domains,
        "read_scope": p.read_scope.as_str(),
        "memory_mb": p.memory_mb,
        "cpus": p.cpus,
        "image": p.image,
        "containerfile": p.containerfile,
        "allow_unsandboxed_fallback": p.allow_unsandboxed_fallback,
        "undecoded": stored.undecoded_columns(),
        "unavailable": unavailable_reason(host, p.backend),
        "places": places.iter().map(|place| place_json(&p.name, place)).collect::<Vec<_>>(),
    })
}

// ── Export and import ───────────────────────────────────────────────────

/// The header every exported document carries, so a file found later says what
/// it is and what reads it.
const EXPORT_HEADER: &str = "# friring sandbox profiles\n\
                             # Import with: friring-cli sandbox import <file>\n";

/// A document of profiles, in the one shape export writes.
#[derive(Debug, Deserialize)]
struct ProfileBundle {
    #[serde(default, rename = "profile")]
    profiles: Vec<SandboxProfile>,
}

/// `sandbox export`.
///
/// A profile carries no credential — the `env-token` value lives in the OS
/// keychain and nothing else does (ADR-28) — so an export is safe to commit.
/// What it must not carry is friring's own bookkeeping: `created_at` and
/// `updated_at` belong to the row, not to the recipe, and importing them
/// somewhere else would claim a history that machine does not have.
fn export(
    db: &Database,
    name: Option<&str>,
    output_path: Option<&Path>,
) -> Result<CommandOutput, String> {
    let stored = match name {
        Some(name) => vec![load_profile(db, name)?],
        None => db
            .list_sandbox_profiles()
            .map_err(|e| format!("Failed to list sandbox profiles: {e}"))?,
    };
    if stored.is_empty() {
        return Err("There are no sandbox profiles to export".to_string());
    }
    // A row friring could not decode holds substituted values in the columns it
    // could not read, so exporting it would write friring's guesses out as if
    // they were the user's policy — and importing that file elsewhere would
    // make the guess permanent.
    let (intact, damaged): (Vec<_>, Vec<_>) = stored
        .into_iter()
        .partition(StoredSandboxProfile::is_intact);
    let profiles: Vec<SandboxProfile> = intact.into_iter().map(|s| s.profile).collect();
    let skipped: Vec<String> = damaged
        .iter()
        .map(|s| format!("{} ({})", s.profile.name, s.undecoded_columns().join(", ")))
        .collect();

    let document = render_bundle(&profiles);
    let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
    let written = match output_path {
        Some(path) => {
            if path.exists() {
                return Err(format!(
                    "'{}' already exists; export refuses to overwrite it",
                    path.display()
                ));
            }
            std::fs::write(path, &document)
                .map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
            Some(path.display().to_string())
        }
        None => None,
    };
    let human = match &written {
        Some(path) => format!("Exported {} profile(s) to {path}", names.len()),
        None => document.clone(),
    };
    let json = json!({
        "profiles": names,
        "skipped": skipped,
        "path": written,
        "toml": document,
    });
    match skipped.is_empty() {
        true => Ok(CommandOutput::new(json, human)),
        // Loud rather than silent: a script that exported "everything" and got
        // fewer profiles than it has must not read as success.
        false => Ok(CommandOutput::failed(
            json,
            format!("{human}\nskipped unreadable: {}", skipped.join(", ")),
            format!(
                "{} profile(s) could not be exported until they are repaired",
                skipped.len()
            ),
        )),
    }
}

/// Render profiles as the `[[profile]]` document import reads back.
///
/// Written with `toml_edit` rather than `toml::to_string` so the shape is
/// deliberate: paths are inline tables, which keeps every key of a profile in
/// one block and lets a human read the boundary top to bottom.
fn render_bundle(profiles: &[SandboxProfile]) -> String {
    let mut doc = toml_edit::DocumentMut::new();
    let mut entries = toml_edit::ArrayOfTables::new();
    for profile in profiles {
        entries.push(profile_table(profile));
    }
    doc.insert("profile", toml_edit::Item::ArrayOfTables(entries));
    format!("{EXPORT_HEADER}\n{doc}")
}

fn profile_table(profile: &SandboxProfile) -> toml_edit::Table {
    use toml_edit::{value, Array, InlineTable, Item, Value as TomlValue};

    let mut table = toml_edit::Table::new();
    table.insert("name", value(profile.name.trim()));
    table.insert("backend", value(profile.backend.as_str()));

    let mut paths = Array::new();
    for path in &profile.paths {
        let mut entry = InlineTable::new();
        entry.insert("path", TomlValue::from(path.path.clone()));
        entry.insert("mode", TomlValue::from(path.mode.as_str()));
        paths.push(TomlValue::InlineTable(entry));
    }
    // One path stays inline; several are broken over lines, because the path
    // list is the boundary and a reviewer has to be able to read it down the
    // page rather than across it.
    if profile.paths.len() > 1 {
        for entry in paths.iter_mut() {
            entry.decor_mut().set_prefix("\n    ");
        }
        paths.set_trailing_comma(true);
        paths.set_trailing("\n");
    }
    table.insert("paths", Item::Value(TomlValue::Array(paths)));

    table.insert("network_mode", value(profile.network_mode.as_str()));
    table.insert("network_allow", string_array(&profile.network_allow));
    table.insert("network_deny", string_array(&profile.network_deny));
    table.insert("prompt_new_domains", value(profile.prompt_new_domains));
    table.insert("read_scope", value(profile.read_scope.as_str()));
    if let Some(memory_mb) = profile.memory_mb {
        table.insert("memory_mb", value(i64::from(memory_mb)));
    }
    if let Some(cpus) = profile.cpus {
        table.insert("cpus", value(i64::from(cpus)));
    }
    if let Some(image) = &profile.image {
        table.insert("image", value(image.as_str()));
    }
    if let Some(containerfile) = &profile.containerfile {
        table.insert("containerfile", value(containerfile.as_str()));
    }
    table.insert(
        "allow_unsandboxed_fallback",
        value(profile.allow_unsandboxed_fallback),
    );
    table
}

fn string_array(items: &[String]) -> toml_edit::Item {
    let mut array = toml_edit::Array::new();
    for item in items {
        array.push(item.as_str());
    }
    toml_edit::Item::Value(toml_edit::Value::Array(array))
}

/// `sandbox import`.
///
/// **Everything is validated before anything is written.** A profile that a
/// launch would refuse — read-write roots enclosing the data directory
/// (ADR-29), a path reaching friring's own sandbox state or a container
/// engine's control socket — is refused *here*, in the same words, rather than
/// stored and discovered at the first launch that picks it. So is a name
/// collision, unless `--replace` says otherwise, and so is a key friring does
/// not know: an ignored `network_alow` would be a boundary quietly wider than
/// the document says.
fn import(db: &Database, path: &Path, replace: bool) -> Result<CommandOutput, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let profiles = parse_bundle(&text, &path.display().to_string())?;
    let existing = db
        .list_sandbox_profile_names()
        .map_err(|e| format!("Failed to list sandbox profiles: {e}"))?;

    let mut replaced: Vec<String> = Vec::new();
    let mut created: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for profile in &profiles {
        let name = profile.name.trim().to_string();
        let key = name.to_ascii_lowercase();
        if !seen.insert(key.clone()) {
            return Err(format!(
                "'{}' names the profile '{name}' twice; nothing was imported",
                path.display()
            ));
        }
        let collides = existing
            .iter()
            .any(|other| other.trim().to_ascii_lowercase() == key);
        // `validate_unique` is what the editor's save runs. Under `--replace`
        // the colliding name is the one being replaced, so it is excluded — the
        // same exemption the editor makes for the profile it is editing.
        let others: Vec<String> = match replace {
            true => existing
                .iter()
                .filter(|other| other.trim().to_ascii_lowercase() != key)
                .cloned()
                .collect(),
            false => existing.clone(),
        };
        profile
            .validate_unique(&others)
            .map_err(|e| format!("{e} — nothing was imported"))?;
        if let Some(refusal) = launch_path_refusal(profile) {
            return Err(format!(
                "sandbox profile '{name}' cannot be imported: {refusal} — nothing was imported"
            ));
        }
        match collides {
            true => replaced.push(name),
            false => created.push(name),
        }
    }

    // One transaction: everything above validated the whole document, so a
    // write that fails part-way must not leave a boundary set nobody authored.
    db.upsert_sandbox_profiles(&profiles)
        .map_err(|e| format!("Failed to save the imported profiles: {e}"))?;

    let summary = format!(
        "Imported {} profile(s) from {} ({} new, {} replaced)",
        profiles.len(),
        path.display(),
        created.len(),
        replaced.len()
    );
    Ok(CommandOutput::from_summary(json!({
        "imported": profiles.iter().map(|p| p.name.trim()).collect::<Vec<_>>(),
        "created": created,
        "replaced": replaced,
        "summary": summary,
    })))
}

/// Read a `[[profile]]` document, refusing anything friring would have to
/// guess about.
fn parse_bundle(text: &str, label: &str) -> Result<Vec<SandboxProfile>, String> {
    let (bundle, unknown) =
        crate::agent::agent_config::parse_toml_reporting_unknown::<ProfileBundle>(text, label)
            .map_err(|e| {
                format!(
                    "'{label}' is not a sandbox profile document: {}",
                    crate::agent::agent_config::compact_toml_error(&e.to_string())
                )
            })?;
    // Reported rather than ignored, and refused rather than reported: a key
    // friring does not recognise is either a typo that drops part of a boundary
    // or a knob a newer friring understands and this one cannot honour. Both
    // read as "this document does not mean here what it means where it was
    // written".
    if let Some(field) = unknown.first() {
        return Err(format!(
            "{field} — friring will not import a profile it cannot fully read, because the \
             ignored value may be part of the boundary"
        ));
    }
    if bundle.profiles.is_empty() {
        return Err(format!(
            "'{label}' carries no [[profile]] entries — export writes them with \
             `friring-cli sandbox export`"
        ));
    }
    Ok(bundle.profiles)
}

/// The path refusals a launch makes, applied at the import instead.
///
/// The same three checks the profile editor runs on save
/// (`crate::app::modals`'s `writable_roots_refusal`), in the same order and
/// with each check's own sentence: a read-write root enclosing the data
/// directory reaches the database (ADR-29) or a tmux socket directory, and a
/// path in **either** mode may reach neither friring's own sandbox state nor a
/// container engine's control socket. `None` when the profile is grantable.
fn launch_path_refusal(profile: &SandboxProfile) -> Option<String> {
    let home = crate::paths::home_dir()
        .as_deref()
        .and_then(Path::to_str)
        .unwrap_or_default()
        .to_string();
    let declared: Vec<String> = profile.paths.iter().map(|p| p.expanded(&home)).collect();
    let writable: Vec<String> = profile
        .paths
        .iter()
        .filter(|p| p.mode.is_writable())
        .map(|p| p.expanded(&home))
        .collect();
    let db = crate::paths::database_file();
    crate::sandbox::check_writable_roots(&writable, db.as_deref().and_then(Path::to_str))
        .err()
        .or_else(|| crate::sandbox::check_declared_paths(&declared).err())
        .or_else(|| crate::sandbox::check_engine_socket_paths(&declared, Some(&home)).err())
}

// ── Tokens ──────────────────────────────────────────────────────────────

/// The most a token may be. A value this long is a file somebody piped in by
/// accident, not a credential, and storing it would put megabytes into the
/// user's keychain under friring's name.
const MAX_TOKEN_BYTES: usize = 8 * 1024;

/// `sandbox token`.
///
/// `agents` and `store` are injected so the whole command is testable against
/// fabricated agents and a stub keychain: **no test may read a real credential
/// store**, and none of these paths may either unless a user asked for it.
fn token(
    action: TokenAction,
    agents: Vec<AgentDef>,
    store: &dyn crate::sandbox::auth::keychain::SecretStore,
) -> Result<CommandOutput, String> {
    match action {
        TokenAction::Set { agent, variable } => {
            set_token(&agent, variable.as_deref(), &agents, store)
        }
        TokenAction::Rm { agent, variable } => {
            remove_token(&agent, variable.as_deref(), &agents, store)
        }
        TokenAction::List => list_tokens(&agents, store),
    }
}

/// The credential family an agent name belongs to: its `hook_schema` when it
/// declares one (a rebranded claude reaches claude's entry rather than a
/// second, empty one), otherwise the registry name.
///
/// A name no agent in the registry carries is taken as a family in its own
/// right, so a token can be stored before the agent is declared — with the
/// second half of the answer saying which case this was.
fn resolve_family<'a>(name: &str, agents: &'a [AgentDef]) -> (String, Option<&'a AgentDef>) {
    let def = agents.iter().find(|def| def.name == name).or_else(|| {
        agents
            .iter()
            .find(|def| def.name.eq_ignore_ascii_case(name))
    });
    match def {
        Some(def) => (
            def.hook_schema.as_deref().unwrap_or(&def.name).to_string(),
            Some(def),
        ),
        None => (name.trim().to_string(), None),
    }
}

/// The variables an agent declares as token carriers, or an empty list.
fn declared_variables(def: Option<&AgentDef>) -> Vec<String> {
    def.and_then(|def| def.sandbox.as_ref())
        .map(|sandbox| sandbox.secret_env.clone())
        .unwrap_or_default()
}

/// Which variable this command is about.
///
/// Explicit when given. Otherwise the agent's single declared one, because an
/// agent that accepts exactly one token should not need it typed out — and a
/// refusal naming the choices when there are several, since guessing would
/// store a token nothing injects.
fn resolve_variable(
    given: Option<&str>,
    agent: &str,
    declared: &[String],
) -> Result<String, String> {
    if let Some(variable) = given {
        return Ok(variable.trim().to_string());
    }
    match declared {
        [only] => Ok(only.clone()),
        [] => Err(format!(
            "'{agent}' declares no token variables, so friring would not inject one. Add \
             `secret_env = [\"…\"]` under [agents.{agent}.sandbox] in agents.toml, or name the \
             variable explicitly"
        )),
        many => Err(format!(
            "'{agent}' declares several token variables ({}); name the one to store",
            many.join(", ")
        )),
    }
}

/// Build the keychain key for `family`/`variable`, refusing a spelling that
/// could be read as a flag or a path by the platform tool.
fn secret_key(
    family: &str,
    variable: &str,
) -> Result<crate::sandbox::auth::keychain::SecretKey, String> {
    crate::sandbox::auth::keychain::SecretKey::new(family, variable).ok_or_else(|| {
        format!(
            "'{family}/{variable}' cannot be a keychain entry: a credential family may hold \
             letters, digits, '-', '_' and '.', and a variable must be a usable environment \
             variable name"
        )
    })
}

/// Which keychain entry one `token` command is about.
///
/// Resolved — and every refusal raised — **before** a value is read, so a token
/// typed into a prompt is never one the user has to rotate because friring
/// said no afterwards.
struct TokenEntry {
    family: String,
    variable: String,
    key: crate::sandbox::auth::keychain::SecretKey,
}

/// Resolve the entry `agent`/`variable` names.
///
/// `require_declared` is the difference between storing and forgetting: a token
/// stored under a name the agent does not declare is one no launch would ever
/// inject, while a name it *stopped* declaring is exactly the entry a user
/// wants removed.
fn token_entry(
    agent: &str,
    variable: Option<&str>,
    agents: &[AgentDef],
    require_declared: bool,
) -> Result<TokenEntry, String> {
    let (family, def) = resolve_family(agent, agents);
    let declared = declared_variables(def);
    let variable = resolve_variable(variable, agent, &declared)?;
    if require_declared && def.is_some() && !declared.contains(&variable) {
        return Err(format!(
            "'{agent}' does not declare '{variable}' as a token variable, so friring would \
             never inject it. It declares {}. Add it to `secret_env` under \
             [agents.{agent}.sandbox] in agents.toml first",
            match declared.is_empty() {
                true => "none".to_string(),
                false => declared.join(", "),
            }
        ));
    }
    let key = secret_key(&family, &variable)?;
    Ok(TokenEntry {
        family,
        variable,
        key,
    })
}

fn set_token(
    agent: &str,
    variable: Option<&str>,
    agents: &[AgentDef],
    store: &dyn crate::sandbox::auth::keychain::SecretStore,
) -> Result<CommandOutput, String> {
    let entry = token_entry(agent, variable, agents, true)?;
    // Before the value is read, not after: a store friring cannot write to
    // (macOS, where `security` takes a new item's value only on its command
    // line) would otherwise take a token off the user and then refuse it —
    // leaving them a secret they have to rotate for nothing.
    store.can_store(&entry.key)?;
    let secret = read_secret(&entry.key.account())?;
    store_token(&entry, store, &secret)
}

/// Write the value, and report the entry without a syllable of what is in it.
fn store_token(
    entry: &TokenEntry,
    store: &dyn crate::sandbox::auth::keychain::SecretStore,
    secret: &crate::sandbox::auth::keychain::Secret,
) -> Result<CommandOutput, String> {
    store.set(&entry.key, secret)?;
    let summary = format!(
        "Stored a token for {} in {}",
        entry.key.account(),
        store.label()
    );
    Ok(CommandOutput::from_summary(json!({
        "family": entry.family,
        "variable": entry.variable,
        "account": entry.key.account(),
        "store": store.label(),
        "stored": true,
        "summary": summary,
    })))
}

fn remove_token(
    agent: &str,
    variable: Option<&str>,
    agents: &[AgentDef],
    store: &dyn crate::sandbox::auth::keychain::SecretStore,
) -> Result<CommandOutput, String> {
    let entry = token_entry(agent, variable, agents, false)?;
    store.remove(&entry.key)?;
    let summary = format!("Removed {} from {}", entry.key.account(), store.label());
    Ok(CommandOutput::from_summary(json!({
        "family": entry.family,
        "variable": entry.variable,
        "account": entry.key.account(),
        "store": store.label(),
        "stored": false,
        "summary": summary,
    })))
}

/// `sandbox token list` — whether an entry exists, never what it holds.
///
/// Driven by the registry rather than by the keychain, because a credential
/// store cannot be enumerated by service without reading every entry in it, and
/// friring has no business doing that. So the answer is "of the tokens agents
/// declare, these are the ones friring holds".
fn list_tokens(
    agents: &[AgentDef],
    store: &dyn crate::sandbox::auth::keychain::SecretStore,
) -> Result<CommandOutput, String> {
    // Keyed by family, because two rebranded agents share one entry and would
    // otherwise be reported as two.
    let mut entries: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut unusable: Vec<Value> = Vec::new();
    for def in agents {
        let family = def.hook_schema.as_deref().unwrap_or(&def.name).to_string();
        for variable in declared_variables(Some(def)) {
            match crate::sandbox::auth::keychain::SecretKey::new(&family, &variable) {
                Some(_) => entries
                    .entry((family.clone(), variable))
                    .or_default()
                    .push(def.name.clone()),
                None => unusable.push(json!({
                    "agent": def.name,
                    "family": family,
                    "variable": variable,
                    "reason": "not a usable environment variable name, so friring will not \
                               look for it",
                })),
            }
        }
    }

    let mut rows: Vec<Value> = Vec::new();
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    for ((family, variable), mut agents) in entries {
        agents.sort();
        agents.dedup();
        let key = secret_key(&family, &variable)?;
        // Presence only. The value is deliberately dropped here rather than
        // carried anywhere it could be rendered.
        let stored = store.get(&key)?.is_some();
        rows.push(json!({
            "family": family,
            "variable": variable,
            "account": key.account(),
            "agents": agents,
            "stored": stored,
        }));
        table_rows.push(vec![
            family,
            variable,
            match stored {
                true => "yes".to_string(),
                false => "no".to_string(),
            },
            agents.join(", "),
        ]);
    }

    let human = match table_rows.is_empty() {
        true => format!(
            "No agent declares a token variable ({} is where friring would keep one)",
            store.label()
        ),
        false => output::table(&["FAMILY", "VARIABLE", "STORED", "AGENTS"], &table_rows),
    };
    Ok(CommandOutput::new(
        json!({
            "store": store.label(),
            "tokens": rows,
            "unusable": unusable,
        }),
        human,
    ))
}

/// Take a token from stdin, or prompt for one without echo.
///
/// **Never from argv.** A command line is on the host's process table —
/// `/proc/<pid>/cmdline` is world-readable on Linux by default — which is the
/// same rule that keeps the egress proxy's token out of the tmux command line
/// (`docs/SANDBOX.md` §Credentials).
fn read_secret(account: &str) -> Result<crate::sandbox::auth::keychain::Secret, String> {
    let raw = match std::io::stdin().is_terminal() {
        true => prompt_without_echo(&format!("Token for {account}: "))?,
        false => {
            let mut buffer = Vec::new();
            std::io::stdin()
                .take(MAX_TOKEN_BYTES as u64 + 1)
                .read_to_end(&mut buffer)
                .map_err(|e| format!("could not read the token from stdin: {e}"))?;
            String::from_utf8(buffer).map_err(|_| "the token is not valid UTF-8".to_string())?
        }
    };
    clean_token(&raw)
}

/// Hold a raw value to what can be carried in an environment variable, and
/// nothing else. The value is never quoted back in any of these messages.
fn clean_token(raw: &str) -> Result<crate::sandbox::auth::keychain::Secret, String> {
    // Only the line ending a terminal or a here-doc adds: a token never ends in
    // whitespace, and trimming the front would silently accept a leading space.
    let value = raw.trim_end_matches(['\n', '\r']);
    if value.len() > MAX_TOKEN_BYTES {
        return Err(format!(
            "that is {} bytes, and a token may be at most {MAX_TOKEN_BYTES} — did a file reach \
             stdin by accident?",
            value.len()
        ));
    }
    if value.trim().is_empty() {
        return Err(
            "no token was given; nothing was stored (use `sandbox token rm` to clear one)"
                .to_string(),
        );
    }
    if let Some(bad) = value.chars().find(|c| c.is_control()) {
        return Err(format!(
            "the token carries a control character ({}), which cannot be passed to the sandbox \
             in an environment variable",
            bad.escape_debug()
        ));
    }
    Ok(crate::sandbox::auth::keychain::Secret::new(value))
}

/// Read one line from the terminal with echo off.
///
/// Raw mode is the only portable way to stop the terminal printing what is
/// typed; the guard puts it back however this returns, so a `?` on the way out
/// cannot leave the user's shell in raw mode.
fn prompt_without_echo(prompt: &str) -> Result<String, String> {
    // stderr, so `friring-cli sandbox token set … 2>/dev/null` still prompts on
    // a terminal and a redirected stdout stays machine-readable.
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    crossterm::terminal::enable_raw_mode()
        .map_err(|e| format!("could not turn off terminal echo, so nothing was read: {e}"))?;
    let result = read_raw_line();
    let _ = crossterm::terminal::disable_raw_mode();
    eprintln!();
    result
}

/// The raw-mode read itself: bytes until the line ends, with `Ctrl+C` and
/// backspace honoured because raw mode means the terminal no longer is.
fn read_raw_line() -> Result<String, String> {
    let mut stdin = std::io::stdin();
    let mut collected: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("could not read the token: {e}")),
        }
        match byte[0] {
            b'\r' | b'\n' => break,
            // Ctrl+C: raw mode swallows the signal, so answer it here.
            0x03 => return Err("cancelled; nothing was stored".to_string()),
            // Backspace / delete. Pops a whole byte, which is enough: a token is
            // ASCII, and a mistyped multi-byte paste is refused as non-UTF-8
            // rather than half-deleted.
            0x08 | 0x7f => {
                collected.pop();
            }
            b if b < 0x20 => {}
            b => {
                if collected.len() < MAX_TOKEN_BYTES {
                    collected.push(b);
                }
            }
        }
    }
    String::from_utf8(collected).map_err(|_| "the token is not valid UTF-8".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use crate::session::{AgentSandboxDef, PathMode, SandboxPath};
    use clap::Parser as _;

    /// A fabricated token. Nothing in this file's tests ever reaches a real
    /// credential store: the stub below is why `SecretStore` is a trait.
    const FAKE: &str = "fabricated-token-9c31ab";

    fn agent(name: &str, secret_env: &[&str]) -> AgentDef {
        AgentDef {
            name: name.to_string(),
            command: name.to_string(),
            args: Vec::new(),
            resume_args: Vec::new(),
            fork_args: Vec::new(),
            new_session_args: Vec::new(),
            resume_latest: false,
            hook_schema: None,
            sandbox: Some(AgentSandboxDef {
                secret_env: secret_env.iter().map(|s| (*s).to_string()).collect(),
                ..Default::default()
            }),
        }
    }

    fn profile(name: &str, paths: Vec<SandboxPath>) -> SandboxProfile {
        SandboxProfile::new(name, paths)
    }

    fn db_with(profiles: &[SandboxProfile]) -> Database {
        let db = Database::open_in_memory().unwrap();
        for profile in profiles {
            db.upsert_sandbox_profile(profile).unwrap();
        }
        db
    }

    /// A live session row, which is what protects a profile's places from a
    /// prune and what makes a delete refuse. Written through the ordinary
    /// upsert: `sessions` belongs to `storage::sessions`, and these tests only
    /// care about the two sandbox link columns.
    fn seed_session(db: &Database, name: &str, backend_type: &str, profile: Option<&str>) {
        db.upsert_session(&crate::sync::SharedSession {
            id: crate::session::SessionId::default(),
            name: name.to_string(),
            agent: "claude".to_string(),
            backend_id: String::new(),
            backend_type: backend_type.to_string(),
            agent_session_id: None,
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: Vec::new(),
            shell_backend_id: None,
            sandbox_profile: profile.map(str::to_string),
            sandbox_enforcement: Default::default(),
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
        })
        .unwrap();
    }

    // ── Dispatch ─────────────────────────────────────────────────────────

    #[test]
    fn the_relay_defaults_to_an_ephemeral_loopback_port() {
        let cli = Cli::parse_from(["friring-cli", "sandbox", "relay", "--socket", "/s/p.sock"]);
        let Command::Sandbox {
            action: Action::Relay { listen, socket },
        } = cli.command
        else {
            panic!("expected the relay action");
        };
        // Loopback: inside a namespaced sandbox there is nothing else worth
        // binding, and a routable bind would offer the proxy to whatever else
        // shares the namespace.
        assert!(listen.ip().is_loopback());
        assert_eq!(listen.port(), 0);
        assert_eq!(socket, PathBuf::from("/s/p.sock"));
    }

    /// The socket is not optional: a relay with nowhere to forward to would
    /// accept the agent's connections and drop every one of them.
    #[test]
    fn the_relay_needs_a_socket_to_forward_to() {
        assert!(Cli::try_parse_from(["friring-cli", "sandbox", "relay"]).is_err());
    }

    /// The relay is the one subcommand that must never open a database
    /// (ADR-29), and every host-side command must. This is the split
    /// `friring-cli`'s `main` dispatches on.
    #[test]
    fn only_the_relay_runs_before_the_database_is_opened() {
        let relay = Cli::parse_from(["friring-cli", "sandbox", "relay", "--socket", "/s/p.sock"]);
        let (listen, socket) =
            database_free(&relay.command).expect("the relay is dispatched before the database");
        assert!(listen.ip().is_loopback());
        assert_eq!(socket, Path::new("/s/p.sock"));

        for argv in [
            vec!["friring-cli", "sandbox", "list"],
            vec!["friring-cli", "sandbox", "show", "dev"],
            vec!["friring-cli", "sandbox", "rm", "dev"],
            vec!["friring-cli", "sandbox", "prune"],
            vec!["friring-cli", "sandbox", "export"],
            vec!["friring-cli", "sandbox", "import", "p.toml"],
            vec!["friring-cli", "sandbox", "token", "list"],
            vec!["friring-cli", "session", "list"],
        ] {
            let cli = Cli::parse_from(argv.clone());
            assert!(
                database_free(&cli.command).is_none(),
                "{argv:?} needs the database and must take the normal path"
            );
        }
    }

    /// The other half of that split, and the reason it fails closed: a relay
    /// reaching the database-bearing path is refused rather than served, so a
    /// `main` that stopped dispatching it early cannot quietly put a database in
    /// the process a sandbox is talking to.
    #[test]
    fn a_relay_that_reached_the_database_path_is_refused() {
        let db = Database::open_in_memory().unwrap();
        let error = run(
            Action::Relay {
                listen: "127.0.0.1:0".parse().unwrap(),
                socket: PathBuf::from("/s/p.sock"),
            },
            &db,
        )
        .unwrap_err();
        assert!(error.contains("ADR-29"), "{error}");
    }

    /// `main` is the only caller of the early dispatch, and the ordering is the
    /// invariant: a relay served after `Database::open` is the ADR-29 violation
    /// the split exists to prevent.
    #[test]
    fn the_binary_dispatches_the_relay_before_it_opens_a_database() {
        let source = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/friring-cli.rs"),
        )
        .expect("the friring-cli binary source");
        let dispatch = source
            .find("run_before_database")
            .expect("main must dispatch the database-free commands through run_before_database");
        let open = source
            .find("Database::open")
            .expect("main must open the database");
        assert!(
            dispatch < open,
            "the database-free dispatch has to come before the database is opened"
        );
    }

    // ── Profiles ─────────────────────────────────────────────────────────

    fn stub_host() -> crate::sandbox::SandboxHost {
        // A host with nothing installed: probing runs no engine and touches
        // nothing on the machine the test happens to run on.
        crate::sandbox::SandboxHost::new(std::sync::Arc::new(
            crate::sandbox::probe::StubHost::default(),
        ))
    }

    /// A host whose podman probes as available and then refuses to list what it
    /// holds — the difference between "not installed" and "would not answer".
    fn podman_that_will_not_list() -> crate::sandbox::SandboxHost {
        // Fully qualified, never `use`: `cli` reaches the sandbox layer by path
        // only, so every reach stays visible at the call site
        // (`tests/architecture_rules.rs`).
        type ProbeOutput = crate::sandbox::probe::ProbeOutput;
        let host = crate::sandbox::probe::StubHost::new()
            .with_home("/home/u")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("podman")
            .with_command("id -u", ProbeOutput::success("1000\n"))
            .with_command("id -g", ProbeOutput::success("1000\n"))
            .with_command(
                &format!(
                    "/usr/bin/podman info --format {}",
                    "{{.Version.Version}}|{{.Host.Security.Rootless}}"
                ),
                ProbeOutput::success("5.2.2|true\n"),
            )
            .with_command_prefix(
                "/usr/bin/podman ps",
                ProbeOutput::failure(125, "Cannot connect to the podman socket\n"),
            );
        crate::sandbox::SandboxHost::new(std::sync::Arc::new(host))
    }

    #[test]
    fn list_reports_each_profile_and_its_places() {
        let db = db_with(&[profile("dev", vec![SandboxPath::workspace("~/dev/app")])]);
        db.upsert_sandbox_instance(&crate::storage::sandboxes::SandboxInstance::new(
            "dev",
            SandboxBackendKind::Docker,
            "ctr-1",
            "running",
        ))
        .unwrap();

        let out = list(&db, &stub_host(), false).unwrap();
        let rows = out.json.as_array().expect("an array of profiles");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "dev");
        assert_eq!(rows[0]["network_mode"], "allowlist");
        assert_eq!(rows[0]["places"][0]["id"], "ctr-1");
        assert!(out.human.contains("dev"), "{}", out.human);

        // The same places, one row each.
        let out = list(&db, &stub_host(), true).unwrap();
        let places = out.json.as_array().expect("an array of places");
        assert_eq!(places.len(), 1);
        assert_eq!(places[0]["profile"], "dev");
        assert_eq!(places[0]["engine"], "docker");
    }

    /// An empty installation is a sentence, not an empty table.
    #[test]
    fn list_says_so_when_there_is_nothing() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(
            list(&db, &stub_host(), false).unwrap().human,
            "No sandbox profiles"
        );
        assert_eq!(
            list(&db, &stub_host(), true).unwrap().human,
            "No sandbox places recorded"
        );
    }

    /// A row friring could not decode still lists — the list is where it gets
    /// repaired — but says so instead of reporting friring's own substitutions
    /// as the profile.
    #[test]
    fn a_profile_that_did_not_decode_is_listed_with_its_damage() {
        let db = Database::open_in_memory().unwrap();
        db.insert_undecodable_sandbox_profile("broken").unwrap();

        let out = list(&db, &stub_host(), false).unwrap();
        assert_eq!(out.json[0]["name"], "broken");
        assert!(out.human.contains("unreadable"), "{}", out.human);

        // `show` says it at the top rather than as a footnote, because every
        // value under it is friring's own substitution.
        let out = show(&db, &stub_host(), "broken").unwrap();
        let second = out.human.lines().nth(1).unwrap_or_default();
        assert!(second.starts_with("UNREADABLE:"), "{}", out.human);
        assert!(second.contains("read_scope"), "{second}");
    }

    #[test]
    fn show_names_the_profile_or_says_it_is_not_there() {
        let mut p = profile(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
            ],
        );
        p.network_allow = vec!["api.anthropic.com:443".into()];
        let db = db_with(&[p]);

        let out = show(&db, &stub_host(), "DEV").unwrap();
        assert_eq!(out.json["name"], "dev");
        assert!(out.human.contains("~/dev/app (rw)"), "{}", out.human);
        assert!(out.human.contains("/srv/shared (ro)"), "{}", out.human);
        assert!(out.human.contains("api.anthropic.com:443"), "{}", out.human);

        let error = show(&db, &stub_host(), "ghost").unwrap_err();
        assert!(error.contains("'ghost' not found"), "{error}");
    }

    /// Deleting a profile out from under a live session would leave it unable
    /// to relaunch, so the CLI refuses until it is told to do it anyway — the
    /// headless half of the list modal's confirmation.
    #[test]
    fn rm_refuses_a_profile_sessions_still_reference_until_forced() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let db = db_with(&[profile("dev", vec![SandboxPath::workspace("~/dev/app")])]);
        seed_session(&db, "s1", "sandbox:dev", Some("dev"));

        let error = remove(&db, "dev", false).unwrap_err();
        assert!(error.contains("1 session(s)"), "{error}");
        assert!(error.contains("--force"), "{error}");
        assert!(db.get_sandbox_profile("dev").unwrap().is_some());

        let out = remove(&db, "dev", true).unwrap();
        assert_eq!(out.json["sessions_affected"], 1);
        // The tree is kept while a session is still in it: removing it would
        // take `$HOME` away from a live agent mid-turn.
        assert_eq!(out.json["tree_removed"], false);
        assert!(db.get_sandbox_profile("dev").unwrap().is_none());
    }

    #[test]
    fn rm_of_an_unused_profile_takes_its_tree_with_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let db = db_with(&[profile("dev", vec![SandboxPath::workspace("~/dev/app")])]);

        let out = remove(&db, "dev", false).unwrap();
        assert_eq!(out.json["tree_removed"], true);
        assert_eq!(out.json["sessions_affected"], 0);
        assert!(remove(&db, "dev", false).is_err(), "it is gone now");
    }

    /// A prune on a machine with no container engine reclaims nothing, and
    /// reports nothing it could not ask: an engine that is not installed created
    /// no place, so naming it would be noise on every machine that has one
    /// engine rather than all of them.
    #[test]
    fn prune_on_a_machine_with_no_engine_reclaims_and_reports_nothing() {
        let db = db_with(&[profile("dev", vec![SandboxPath::workspace("~/dev/app")])]);
        db.upsert_sandbox_instance(&crate::storage::sandboxes::SandboxInstance::new(
            "dev",
            SandboxBackendKind::Docker,
            "ctr-1",
            "running",
        ))
        .unwrap();

        let out = prune(&db, &stub_host(), None, false).unwrap();
        assert_eq!(out.json["removed"].as_array().unwrap().len(), 0);
        assert_eq!(out.json["forgotten"].as_array().unwrap().len(), 0);
        assert_eq!(out.json["skipped"].as_array().unwrap().len(), 0);
        // The record survived, because nothing could say the container had not.
        assert_eq!(db.list_sandbox_instances().unwrap().len(), 1);
    }

    /// The case that *is* worth reporting, and the reason the two are told
    /// apart: an engine friring can drive and that would not answer may be
    /// holding anything, and forgetting its rows would lose the only ids that
    /// can find those containers again.
    #[test]
    fn prune_skips_an_installed_engine_it_cannot_ask_rather_than_forgetting_its_rows() {
        let db = db_with(&[profile("dev", vec![SandboxPath::workspace("~/dev/app")])]);
        db.upsert_sandbox_instance(&crate::storage::sandboxes::SandboxInstance::new(
            "dev",
            SandboxBackendKind::Podman,
            "ctr-1",
            "running",
        ))
        .unwrap();

        let out = prune(&db, &podman_that_will_not_list(), None, false).unwrap();
        assert_eq!(out.json["removed"].as_array().unwrap().len(), 0);
        assert_eq!(out.json["forgotten"].as_array().unwrap().len(), 0);
        let skipped = out.json["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(
            skipped[0].as_str().unwrap().starts_with("podman:"),
            "{skipped:?}"
        );
        assert_eq!(db.list_sandbox_instances().unwrap().len(), 1);
    }

    /// The protection a headless prune applies: it drives no session, so it
    /// cannot know which container one is in and protects every place of every
    /// profile a live session names.
    #[test]
    fn every_place_of_a_profile_with_a_live_session_is_protected() {
        let db = db_with(&[profile("dev", vec![SandboxPath::workspace("~/dev/app")])]);
        seed_session(&db, "s1", "sandbox:Dev", Some("Dev"));
        seed_session(&db, "s2", "local-tmux", None);

        let protected = protected_profiles(&db).unwrap();
        // Case-insensitively, because the profile name is and the container's
        // label carries whatever spelling the launch had.
        assert!(protected.contains("dev"));
        assert_eq!(protected.len(), 1);
    }

    // ── Export and import ────────────────────────────────────────────────

    fn round_trip(profile: &SandboxProfile) -> SandboxProfile {
        let document = render_bundle(std::slice::from_ref(profile));
        let mut parsed = parse_bundle(&document, "test.toml").expect("the export parses back");
        assert_eq!(parsed.len(), 1);
        parsed.remove(0)
    }

    #[test]
    fn a_profile_survives_an_export_and_an_import() {
        let mut p = profile(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
            ],
        );
        p.backend = SandboxBackendKind::Podman;
        p.network_allow = vec!["api.anthropic.com".into(), "github.com:443".into()];
        p.network_deny = vec!["gist.github.com".into()];
        p.prompt_new_domains = false;
        p.memory_mb = Some(4096);
        p.cpus = Some(2);
        p.image = Some("ghcr.io/example/dev:latest".into());
        p.allow_unsandboxed_fallback = true;

        let back = round_trip(&p);
        assert_eq!(back, p);
        // Paths keep their order and their intent, which is the only thing
        // carrying per-path read/write.
        assert_eq!(back.paths[0].mode, PathMode::ReadWrite);
        assert_eq!(back.paths[1].path, "/srv/shared");
    }

    /// Storage owns the timestamps: exporting them would carry one machine's
    /// history to another, where the import ignores them anyway.
    #[test]
    fn an_export_carries_no_timestamps_and_no_secret() {
        let db = db_with(&[profile("dev", vec![SandboxPath::workspace("~/dev/app")])]);
        let out = export(&db, None, None).unwrap();
        let document = out.json["toml"].as_str().unwrap();
        assert!(!document.contains("created_at"), "{document}");
        assert!(!document.contains("updated_at"), "{document}");
        // A profile holds no credential — the `env-token` value lives in the OS
        // keychain (ADR-28) — so an export is safe to commit. Pinned, because a
        // future column could quietly change that.
        assert!(!document.contains(FAKE), "{document}");
        assert!(document.contains("[[profile]]"), "{document}");
    }

    #[test]
    fn export_writes_a_file_and_refuses_to_overwrite_one() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("profiles.toml");
        let db = db_with(&[profile("dev", vec![SandboxPath::workspace("~/dev/app")])]);

        let out = export(&db, None, Some(&path)).unwrap();
        assert!(out.human.contains("Exported 1 profile"), "{}", out.human);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("[[profile]]"));

        let error = export(&db, None, Some(&path)).unwrap_err();
        assert!(error.contains("refuses to overwrite"), "{error}");
    }

    /// An unreadable row is not exportable: its columns hold friring's
    /// substitutions, and writing those out would make the guess permanent
    /// wherever the file lands.
    #[test]
    fn export_skips_an_unreadable_row_loudly() {
        let db = db_with(&[profile("dev", vec![SandboxPath::workspace("~/dev/app")])]);
        db.insert_undecodable_sandbox_profile("broken").unwrap();

        let out = export(&db, None, None).unwrap();
        assert_eq!(out.json["profiles"].as_array().unwrap().len(), 1);
        assert!(out.failure.is_some(), "a partial export exits non-zero");
        assert!(out.human.contains("broken"), "{}", out.human);
    }

    #[test]
    fn import_stores_what_export_wrote() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let source = db_with(&[
            profile("dev", vec![SandboxPath::workspace("/srv/app")]),
            profile("lib", vec![SandboxPath::read_only("/srv/lib")]),
        ]);
        let path = temp.path().join("profiles.toml");
        export(&source, None, Some(&path)).unwrap();

        let target = Database::open_in_memory().unwrap();
        let out = import(&target, &path, false).unwrap();
        assert_eq!(out.json["created"].as_array().unwrap().len(), 2);
        assert_eq!(target.list_sandbox_profile_names().unwrap(), ["dev", "lib"]);
    }

    /// A profile a launch would refuse is refused at the import instead, in the
    /// same words — the whole point of validating here rather than at the first
    /// launch that picks it.
    #[test]
    fn a_profile_that_could_never_launch_is_refused_on_import() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let data_dir = crate::sandbox::dirs::data_dir().expect("a redirected data directory");
        let target = Database::open_in_memory().unwrap();

        for (label, path) in [
            // Read-write over the data directory: the database there carries
            // automation commands the host executes (ADR-29).
            (
                "the data directory",
                SandboxPath::workspace(data_dir.display().to_string()),
            ),
            // friring's own sandbox state, in either mode: it holds the other
            // profiles' logins and the other sessions' egress sockets, all of
            // which are taken by being readable.
            (
                "friring's sandbox state",
                SandboxPath::read_only(data_dir.join("sandbox").join("pl").display().to_string()),
            ),
        ] {
            let file = temp.path().join(format!("{}.toml", path.mode));
            let _ = std::fs::remove_file(&file);
            std::fs::write(&file, render_bundle(&[profile("bad", vec![path.clone()])])).unwrap();

            let error = import(&target, &file, false).unwrap_err();
            assert!(error.contains("nothing was imported"), "{label}: {error}");
            assert!(
                target.list_sandbox_profile_names().unwrap().is_empty(),
                "{label}: an unsafe profile was stored"
            );
        }
    }

    /// One bad entry stops the whole document: the alternative is a half-applied
    /// import nobody can reason about.
    #[test]
    fn nothing_is_written_when_one_entry_is_invalid() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let path = temp.path().join("profiles.toml");
        // The second profile has no paths at all, which the validator refuses.
        std::fs::write(
            &path,
            render_bundle(&[
                profile("good", vec![SandboxPath::workspace("/srv/app")]),
                profile("empty", vec![]),
            ]),
        )
        .unwrap();

        let db = Database::open_in_memory().unwrap();
        let error = import(&db, &path, false).unwrap_err();
        assert!(error.contains("nothing was imported"), "{error}");
        assert!(db.list_sandbox_profile_names().unwrap().is_empty());
    }

    #[test]
    fn import_refuses_a_name_that_is_already_taken_unless_replacing() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let path = temp.path().join("profiles.toml");
        let mut incoming = profile("dev", vec![SandboxPath::read_only("/srv/other")]);
        incoming.network_mode = crate::session::NetworkMode::None;
        std::fs::write(&path, render_bundle(&[incoming])).unwrap();

        let db = db_with(&[profile("DEV", vec![SandboxPath::workspace("/srv/app")])]);
        let error = import(&db, &path, false).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            db.get_sandbox_profile("dev")
                .unwrap()
                .unwrap()
                .profile
                .paths[0]
                .path,
            "/srv/app",
            "the stored profile was not touched"
        );

        let out = import(&db, &path, true).unwrap();
        assert_eq!(out.json["replaced"].as_array().unwrap().len(), 1);
        let stored = db.get_sandbox_profile("dev").unwrap().unwrap().profile;
        assert_eq!(stored.paths[0].path, "/srv/other");
        assert_eq!(stored.network_mode, crate::session::NetworkMode::None);
    }

    /// A key friring does not know is refused rather than dropped: a typo'd
    /// `network_alow` would import a boundary quietly wider than the document
    /// says.
    #[test]
    fn an_unknown_key_is_refused_rather_than_ignored() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());
        let path = temp.path().join("typo.toml");
        std::fs::write(
            &path,
            "[[profile]]\nname = \"dev\"\nnetwork_alow = [\"evil.example\"]\n\
             paths = [{ path = \"/srv/app\", mode = \"rw\" }]\n",
        )
        .unwrap();

        let db = Database::open_in_memory().unwrap();
        let error = import(&db, &path, false).unwrap_err();
        assert!(error.contains("network_alow"), "{error}");
        assert!(db.list_sandbox_profile_names().unwrap().is_empty());
    }

    #[test]
    fn a_document_with_no_profiles_says_so() {
        assert!(parse_bundle("", "empty.toml")
            .unwrap_err()
            .contains("no [[profile]] entries"));
        assert!(parse_bundle("nonsense = [", "bad.toml")
            .unwrap_err()
            .contains("not a sandbox profile document"));
    }

    // ── Tokens ───────────────────────────────────────────────────────────

    #[test]
    fn a_token_is_stored_under_the_agents_credential_family() {
        let store = crate::sandbox::auth::keychain::StubStore::new();
        let agents = vec![agent("claude", &["ANTHROPIC_API_KEY"])];
        let key = secret_key("claude", "ANTHROPIC_API_KEY").unwrap();

        // The value never comes from argv, so the command is exercised at the
        // seam below `read_secret`: everything above it decides *which* entry,
        // and the reading itself is `clean_token`'s own test.
        let entry = token_entry("claude", None, &agents, true).unwrap();
        let out = store_token(&entry, &store, &clean_token(&format!("{FAKE}\n")).unwrap()).unwrap();

        assert_eq!(out.json["account"], "claude/ANTHROPIC_API_KEY");
        // The trait is named in full rather than imported: `cli` may reach
        // `crate::sandbox` only through fully-qualified paths.
        let stored = crate::sandbox::auth::keychain::SecretStore::get(&store, &key);
        assert_eq!(stored.unwrap().unwrap().expose(), FAKE);
        // Not a syllable of it in anything rendered.
        assert!(!out.human.contains(FAKE), "{}", out.human);
        assert!(!out.json.to_string().contains(FAKE));
    }

    /// A rebranded agent shares the family it declares, so one stored token
    /// serves both rather than one of them silently having none.
    #[test]
    fn a_rebranded_agent_reaches_the_family_it_declares() {
        let mut fleet = agent("fleet", &["ANTHROPIC_API_KEY"]);
        fleet.hook_schema = Some("claude".into());
        let agents = vec![agent("claude", &["ANTHROPIC_API_KEY"]), fleet];

        let (family, def) = resolve_family("fleet", &agents);
        assert_eq!(family, "claude");
        assert!(def.is_some());

        // A name no agent carries is a family in its own right, so a token can
        // be stored before the registry declares the agent.
        let (family, def) = resolve_family("nobody", &agents);
        assert_eq!(family, "nobody");
        assert!(def.is_none());
    }

    /// Storing a token under a name the agent does not declare would store
    /// something nothing ever injects — and the refusal comes *before* the
    /// value is asked for, so a rejected token is never one the user has to
    /// rotate.
    #[test]
    fn a_variable_the_agent_does_not_declare_is_refused_before_anything_is_read() {
        let store = crate::sandbox::auth::keychain::StubStore::new();
        let agents = vec![agent("claude", &["ANTHROPIC_API_KEY"])];

        let error = set_token("claude", Some("SOME_OTHER_KEY"), &agents, &store).unwrap_err();
        assert!(
            error.contains("does not declare 'SOME_OTHER_KEY'"),
            "{error}"
        );
        assert!(error.contains("ANTHROPIC_API_KEY"), "{error}");

        // An agent that declares nothing is told what to add rather than left
        // with an entry no launch reads.
        let error = set_token("plain", None, &[agent("plain", &[])], &store).unwrap_err();
        assert!(error.contains("secret_env"), "{error}");

        // …and one that declares several will not be guessed at.
        let several = vec![agent("multi", &["A_KEY", "B_KEY"])];
        let error = set_token("multi", None, &several, &store).unwrap_err();
        assert!(error.contains("A_KEY, B_KEY"), "{error}");
    }

    /// A store friring cannot write to says so **before** the value is read.
    ///
    /// The macOS case: `security` takes a new item's value only on its command
    /// line, so friring refuses to write and prints the prompting command
    /// instead. Discovering that after the prompt would mean taking a token off
    /// the user and then refusing it — a secret they now have to rotate, for
    /// nothing. Nothing here reads a real keychain: the store is a stub, and the
    /// assertion is that the refusal arrives instead of a read of stdin.
    #[test]
    fn a_store_that_cannot_be_written_refuses_before_the_value_is_asked_for() {
        let store = crate::sandbox::auth::keychain::StubStore::new()
            .that_cannot_store("friring will not write to the macOS keychain itself: run …");
        let agents = vec![agent("claude", &["ANTHROPIC_API_KEY"])];

        let error = set_token("claude", None, &agents, &store).unwrap_err();
        assert!(error.contains("will not write"), "{error}");
    }

    /// A spelling the platform tool would read as a flag never becomes a
    /// keychain entry.
    #[test]
    fn an_unspellable_entry_is_refused() {
        for (family, variable) in [("claude", "-w"), ("-s", "KEY"), ("claude", "1KEY")] {
            let error = secret_key(family, variable).unwrap_err();
            assert!(error.contains("cannot be a keychain entry"), "{error}");
        }
    }

    #[test]
    fn a_token_can_be_removed_even_after_the_agent_stopped_declaring_it() {
        let store = crate::sandbox::auth::keychain::StubStore::new().with_token(
            "claude",
            "ANTHROPIC_API_KEY",
            FAKE,
        );
        let key = secret_key("claude", "ANTHROPIC_API_KEY").unwrap();
        // The registry no longer declares the variable — the entry must still be
        // reachable, or a stale token is stranded in the keychain.
        let agents = vec![agent("claude", &[])];

        let out = remove_token("claude", Some("ANTHROPIC_API_KEY"), &agents, &store).unwrap();
        assert_eq!(out.json["account"], "claude/ANTHROPIC_API_KEY");
        let stored = crate::sandbox::auth::keychain::SecretStore::get(&store, &key);
        assert_eq!(stored.unwrap(), None);
        assert!(!out.human.contains(FAKE));
    }

    /// A host with nowhere to keep a token cannot have removed one, and says so
    /// rather than reporting a revocation that never happened — the user's token
    /// is still wherever they actually put it.
    #[test]
    fn removing_a_token_where_there_is_no_store_is_an_error_not_a_summary() {
        let store = crate::sandbox::auth::keychain::Unavailable::new(
            "this host has no credential store friring can use",
            "install libsecret-tools and run a Secret Service such as gnome-keyring",
        );
        let agents = vec![agent("claude", &["ANTHROPIC_API_KEY"])];

        let error = remove_token("claude", None, &agents, &store).unwrap_err();
        assert!(error.contains("no credential store"), "{error}");
        assert!(error.contains("libsecret-tools"), "{error}");
    }

    /// The listing answers "is there one?" and never "what is it?".
    #[test]
    fn listing_says_whether_a_token_exists_and_never_what_it_is() {
        let store = crate::sandbox::auth::keychain::StubStore::new().with_token(
            "claude",
            "ANTHROPIC_API_KEY",
            FAKE,
        );
        let mut fleet = agent("fleet", &["ANTHROPIC_API_KEY"]);
        fleet.hook_schema = Some("claude".into());
        let agents = vec![
            agent("claude", &["ANTHROPIC_API_KEY"]),
            fleet,
            agent("codex", &["OPENAI_API_KEY"]),
            agent("plain", &[]),
        ];

        let out = list_tokens(&agents, &store).unwrap();
        let rendered = format!("{}\n{}", out.human, out.json);
        assert!(!rendered.contains(FAKE), "{rendered}");

        let tokens = out.json["tokens"].as_array().unwrap();
        assert_eq!(tokens.len(), 2, "one row per entry, not per agent");
        let claude = &tokens[0];
        assert_eq!(claude["account"], "claude/ANTHROPIC_API_KEY");
        assert_eq!(claude["stored"], true);
        // Both agents that share the family are named on the one entry.
        assert_eq!(claude["agents"], json!(["claude", "fleet"]));
        assert_eq!(tokens[1]["stored"], false);
        assert!(out.human.contains("yes"), "{}", out.human);
    }

    /// A declared name that could never be a keychain entry is reported rather
    /// than silently skipped: the launch refuses it too, and a user comparing
    /// the two lists would otherwise see nothing at all.
    #[test]
    fn a_declared_name_that_cannot_be_an_entry_is_reported() {
        let store = crate::sandbox::auth::keychain::StubStore::new();
        let out = list_tokens(&[agent("odd", &["not a variable"])], &store).unwrap();
        assert_eq!(out.json["tokens"].as_array().unwrap().len(), 0);
        assert_eq!(out.json["unusable"][0]["variable"], "not a variable");
    }

    /// A store that is there and will not answer is an error the user can act
    /// on ("unlock the keychain"), not a silent "no token".
    #[test]
    fn a_store_that_will_not_answer_is_reported() {
        let store = crate::sandbox::auth::keychain::StubStore::failing("the keychain is locked");
        let agents = vec![agent("claude", &["ANTHROPIC_API_KEY"])];
        assert_eq!(
            list_tokens(&agents, &store).unwrap_err(),
            "the keychain is locked"
        );
    }

    /// Everything a raw value has to survive before it becomes a credential —
    /// and none of these messages quotes it back.
    #[test]
    fn a_read_value_is_cleaned_and_held_to_what_an_environment_can_carry() {
        assert_eq!(clean_token("abc\n").unwrap().expose(), "abc");
        assert_eq!(clean_token("abc\r\n").unwrap().expose(), "abc");
        // A leading space is part of the value: trimming it would silently
        // store a different token than the one that was pasted.
        assert_eq!(clean_token(" abc").unwrap().expose(), " abc");

        for empty in ["", "\n", "   \n"] {
            assert!(clean_token(empty).unwrap_err().contains("no token"));
        }
        let error = clean_token("secret-half\nother-half").unwrap_err();
        assert!(error.contains("control character"), "{error}");
        assert!(!error.contains("half"), "the value must not be quoted back");
        let error = clean_token(&"x".repeat(MAX_TOKEN_BYTES + 1)).unwrap_err();
        assert!(error.contains("at most"), "{error}");
    }
}
