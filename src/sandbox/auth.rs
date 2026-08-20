//! How a sandboxed agent gets its credentials — the strategies of
//! `docs/SANDBOX.md` §Credentials, resolved against the boundary in front of
//! them.
//!
//! Two facts decide everything here, and both come from ADR-28:
//!
//! - **Refresh tokens are single-use and rotating.** Copying a credential file
//!   into N sandboxes creates N consumers of one token: the first refresh wins,
//!   the rest get `invalid_grant`, and some agents then delete their own
//!   credential file. Copy-per-sandbox is broken by construction, so nothing
//!   here does it by default and the one strategy that copies at all refuses a
//!   second copy outright.
//! - **A keychain does not cross a VM boundary.** A policy backend shares the
//!   host's kernel, so the real credential store — Keychain included — keeps
//!   working with nothing copied. A place cannot reach it at all, which is what
//!   the other three strategies exist for.
//!
//! ## Resolution
//!
//! ```text
//! policy backend    →  host-passthrough, always, and nothing else attempted
//! place, auto       →  env-token     (a token in friring's own keychain entry, if one is there)
//!                   →  volume-login  (one login per profile, done inside the pane)
//! place, seed-file  →  seed-file     (never chosen by `auto`: it copies)
//! ```
//!
//! friring is agent-neutral: what an agent needs is [`AgentSandboxDef`] —
//! declared registry data — and nothing in this module names an agent.
//!
//! ## Nothing here fails a launch
//!
//! A credential problem degrades to **signing in inside the pane**, with the
//! reason in front of the user, rather than refusing the spawn. Refusing would
//! route the launch through the profile's `allow_unsandboxed_fallback` switch,
//! and answering "this token is missing" by running the agent *outside* the
//! boundary is the one outcome worse than an agent that asks you to log in. A
//! strategy friring will not carry out is refused — the copy does not happen —
//! and the refusal becomes the sentence beside the login prompt.
//!
//! The only errors are friring's own I/O: a state directory under friring's
//! data directory that cannot be created is a mount that will not work.

pub mod keychain;

use std::collections::BTreeMap;
use std::fmt;

pub use keychain::{Secret, SecretKey, SecretStore, SystemKeychain};

use crate::sandbox::backend::SandboxResult;
use crate::sandbox::dirs;
use crate::session::{AgentSandboxDef, SandboxAuth};

/// Where the boundary is, which is what decides whether the host's own
/// credential store is reachable at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary<'a> {
    /// A policy backend: a rule applied to a host process, so the real
    /// credential store is right where it always was, subject to the profile's
    /// path policy.
    Policy,
    /// A place: its own filesystem, its own home, and no route to the host's
    /// keychain.
    Place {
        /// The profile's synthetic home **on the host** — friring's own
        /// directory, per profile, never a bind of the host's agent
        /// configuration (ADR-28).
        home_dir: &'a str,
        /// Where that directory is mounted *inside* the place.
        inside_home: &'a str,
    },
}

/// What a launch actually does about credentials, once the request has met the
/// boundary.
///
/// [`SandboxAuth`] is the *request* (registry data, possibly `auto`); this is
/// the verdict, and it is always one of four concrete things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStrategy {
    /// The agent reads the host's own store. Policy backends only.
    HostPassthrough,
    /// A long-lived token from friring's keychain entry, injected as
    /// environment.
    EnvToken,
    /// The profile's own persistent state directory, signed into once inside a
    /// pane and shared by every session of that profile.
    VolumeLogin,
    /// A credential file copied in once, and only ever into one place.
    SeedFile,
}

impl fmt::Display for CredentialStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::HostPassthrough => "host-passthrough",
            Self::EnvToken => "env-token",
            Self::VolumeLogin => "volume-login",
            Self::SeedFile => "seed-file",
        })
    }
}

/// Whether the agent can authenticate inside this boundary, and what the user
/// does when it cannot.
///
/// The legible half of the feature: a place with no credential must read as
/// "sign in here, like this" rather than as an agent that mysteriously fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    /// A credential is in place. `source` is where it came from, in the words a
    /// user would use.
    Ready { source: String },
    /// friring cannot tell: the agent declares nothing it could look at, so its
    /// state simply lives in the profile's sandbox home like everything else.
    Unknown { how: String },
    /// There is no credential in this boundary. `reason` is why, `how` is what
    /// to do about it — inside this pane.
    Required { reason: String, how: String },
}

impl LoginState {
    /// Whether the agent will start signed out.
    pub fn needs_login(&self) -> bool {
        matches!(self, Self::Required { .. })
    }

    /// What the user does about it, when there is something to do.
    pub fn how(&self) -> Option<&str> {
        match self {
            Self::Ready { .. } => None,
            Self::Unknown { how } | Self::Required { how, .. } => Some(how),
        }
    }
}

/// The state directory a place keeps for one profile.
///
/// Per profile and therefore shared by every session using it, which is the
/// whole point: one login, one credential file, one writer. Sessions sharing it
/// are readers of a single rotating token rather than holders of N copies of it
/// — the distinction ADR-28 turns on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    /// Where it lives on the host: inside the profile's synthetic home, which
    /// friring created and which is already mounted.
    pub host_path: String,
    /// The path the agent sees, which is what
    /// [`config_dir_env`](crate::session::AgentSandboxDef::config_dir_env) is
    /// set to.
    pub inside_path: String,
}

/// What one launch does about credentials.
///
/// The environment is split in two on purpose. [`env`](Self::env) is ordinary
/// configuration and may go anywhere; the secret half may only travel over a
/// channel that is not a command line, and is reachable exclusively through
/// [`secret_env`](Self::secret_env).
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialPlan {
    /// What this launch does.
    pub strategy: CredentialStrategy,
    /// Non-secret environment: the relocated state directory, and nothing that
    /// would be a problem in a log.
    pub env: BTreeMap<String, String>,
    /// The token half — private, because reading it is a decision.
    secrets: BTreeMap<String, Secret>,
    /// The persistent state directory, when the boundary has one.
    pub state_dir: Option<StateDir>,
    pub login: LoginState,
    /// One line for the session's `Sandbox:` row and the launch's log line.
    /// Never contains a credential — only variable names, paths and reasons.
    pub note: String,
}

impl CredentialPlan {
    /// The names of the variables carrying a credential. The diagnostic half:
    /// which tokens a launch was given, without the values.
    pub fn secret_names(&self) -> Vec<&str> {
        self.secrets.keys().map(String::as_str).collect()
    }

    /// Whether this plan carries a value that may **only** travel over a
    /// channel that is not a command line.
    ///
    /// The TUI composes a window's environment into a control-mode command over
    /// the tmux socket, which never reaches a process table. `friring-cli` has
    /// no control connection and passes each variable as a `-e KEY=VALUE`
    /// argument instead — where another local user can read it out of
    /// `/proc/<pid>/cmdline` (`docs/SANDBOX.md` §Failure modes). A caller with
    /// only the argv channel must therefore refuse rather than inject.
    pub fn needs_private_env_channel(&self) -> bool {
        !self.secrets.is_empty()
    }

    /// The token half, as `(name, value)` pairs.
    ///
    /// **The one place a credential leaves this type.** Route it into the tmux
    /// window over the control connection and nowhere else: not into argv, not
    /// into a log line, not into a message. Everything else about a plan —
    /// `Debug`, [`note`](Self::note), [`secret_names`](Self::secret_names) —
    /// deliberately withholds it.
    pub fn secret_env(&self) -> Vec<(&str, &str)> {
        self.secrets
            .iter()
            .map(|(name, secret)| (name.as_str(), secret.expose()))
            .collect()
    }
}

impl fmt::Debug for CredentialPlan {
    /// Hand-written, because this is a type that holds credentials: a derived
    /// `Debug` would put a vendor token in every `{:?}`, every `expect` and
    /// every tracing field that mentions a plan. The variable *names* are the
    /// diagnostic; the values are the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialPlan")
            .field("strategy", &self.strategy)
            .field("env", &self.env)
            .field("secrets", &self.secret_names())
            .field("state_dir", &self.state_dir)
            .field("login", &self.login)
            .field("note", &self.note)
            .finish()
    }
}

/// Everything resolution needs that the profile does not carry.
pub struct CredentialInput<'a> {
    /// The profile whose boundary this is. Names the place's own state, and is
    /// what a second profile's seed attempt is refused against.
    pub profile: &'a str,
    pub boundary: Boundary<'a>,
    /// The agent's **credential family** — its registry name, or its
    /// `hook_schema` when it is a rebrand of a built-in. The same identity the
    /// `host-minus-secrets` deny list uses, so a rebranded claude reaches
    /// claude's entry and not a second, empty one.
    pub family: Option<&'a str>,
    /// `[agents.<name>.sandbox]`, or `None` for an agent that declares nothing
    /// — which still launches, and still gets a legible login state.
    pub declaration: Option<&'a AgentSandboxDef>,
    /// Home directory of the machine the agent's *host* state lives on. Only
    /// `seed-file` reads anything under it, and only the file the declaration
    /// names.
    pub host_home: &'a str,
    /// Where friring keeps the tokens it injects.
    pub store: &'a dyn SecretStore,
}

/// Decide — and materialise — one launch's credential strategy.
///
/// Materialising is deliberately part of it: the strategies differ in what has
/// to exist before the agent starts (a state directory the place mounts, a file
/// copied in once), and a "plan" whose directory nobody created is a mount that
/// fails at launch. Everything created here is friring's own, `0700`/`0600`,
/// under the data directory.
///
/// # Errors
///
/// Only friring's own I/O: a state directory that cannot be created. A missing
/// token, an unreadable store, a refused copy and an agent that declares
/// nothing all resolve to a plan whose [`LoginState`] says what to do.
pub fn prepare(input: &CredentialInput<'_>) -> SandboxResult<CredentialPlan> {
    let Boundary::Place {
        home_dir,
        inside_home,
    } = input.boundary
    else {
        return Ok(host_passthrough(input));
    };

    let declaration = input.declaration;
    let mut requested = declaration.map_or(SandboxAuth::Auto, |d| d.auth);
    let mut degraded: Vec<String> = Vec::new();

    // Saying so beats silence: an agent declaring the policy-backend strategy
    // is not misconfigured, it is being run somewhere that strategy cannot
    // exist. What it gets instead is the same ladder an undeclared agent gets,
    // rather than nothing — the request named a store, and there is another one
    // here.
    if requested == SandboxAuth::HostPassthrough {
        degraded.push(
            "this agent declares host-passthrough, which a place cannot give it — the host's \
             credential store is outside the boundary"
                .to_string(),
        );
        requested = SandboxAuth::Auto;
    }

    // The state directory is relocated whatever the credential strategy turns
    // out to be: it is where the agent writes, not only where a login lands.
    let state_dir = relocate_state(declaration, input.host_home, home_dir, inside_home)?;
    let mut env = BTreeMap::new();
    if let (Some(state), Some(var)) = (
        &state_dir,
        declaration.and_then(|d| d.config_dir_env.as_deref()),
    ) {
        // The name comes from `agents.toml` and ends up on a `tmux setenv`, so
        // it is held to what an environment variable may be called rather than
        // trusted. Skipping it loses the relocation, which the agent's own
        // `$HOME`-relative default already covers; accepting it would put a
        // hand-edited registry's text into the launch's environment channel.
        if keychain::is_env_name(var) {
            env.insert(var.to_string(), state.inside_path.clone());
        } else {
            degraded.push(format!(
                "'{var}' is declared as this agent's state-directory variable but is not a usable \
                 environment variable name, so it was not set"
            ));
        }
    }

    if requested == SandboxAuth::SeedFile {
        match seed(input, home_dir) {
            Ok(seeded) => {
                return Ok(CredentialPlan {
                    strategy: CredentialStrategy::SeedFile,
                    note: note(
                        CredentialStrategy::SeedFile,
                        &format!(
                            "seeded once from '{}' into this profile's sandbox home",
                            seeded.source
                        ),
                        &degraded,
                    ),
                    env,
                    secrets: BTreeMap::new(),
                    state_dir,
                    login: LoginState::Ready {
                        source: format!("the copy seeded from '{}'", seeded.source),
                    },
                })
            }
            Err(reason) => degraded.push(reason),
        }
    }

    if matches!(requested, SandboxAuth::Auto | SandboxAuth::EnvToken) {
        match token(input) {
            Ok(Some(found)) => {
                let mut secrets = BTreeMap::new();
                secrets.insert(found.key.variable().to_string(), found.secret);
                return Ok(CredentialPlan {
                    strategy: CredentialStrategy::EnvToken,
                    note: note(
                        CredentialStrategy::EnvToken,
                        &format!(
                            "{} injected from {}",
                            found.key.variable(),
                            input.store.label()
                        ),
                        &degraded,
                    ),
                    env,
                    secrets,
                    state_dir,
                    login: LoginState::Ready {
                        source: format!("{}, as {}", input.store.label(), found.key.variable()),
                    },
                });
            }
            Ok(None) => {}
            Err(reason) => degraded.push(reason),
        }
    }

    Ok(volume_login(input, state_dir, env, degraded, home_dir))
}

/// The policy-backend answer, which is the same one every time.
///
/// Nothing is attempted here — not a keychain lookup, not a copy, not a
/// relocation. The agent's real state directory is already visible subject to
/// the path policy, and relocating it would strand the login the user already
/// has (`docs/SANDBOX.md` §Credentials, and the `config_dir_env` note on
/// [`crate::sandbox::agent::apply_agent_requirements`]).
fn host_passthrough(input: &CredentialInput<'_>) -> CredentialPlan {
    let requested = input.declaration.map_or(SandboxAuth::Auto, |d| d.auth);
    let mut degraded = Vec::new();
    if !matches!(requested, SandboxAuth::Auto | SandboxAuth::HostPassthrough) {
        degraded.push(format!(
            "this agent asks for '{}', which a policy backend has no need of: the real credential \
             store is already reachable here",
            auth_name(requested)
        ));
    }
    CredentialPlan {
        strategy: CredentialStrategy::HostPassthrough,
        env: BTreeMap::new(),
        secrets: BTreeMap::new(),
        state_dir: None,
        login: LoginState::Ready {
            source: "the host's own credential store, subject to this profile's path policy"
                .to_string(),
        },
        note: note(
            CredentialStrategy::HostPassthrough,
            "the host's own store (keychain included), nothing copied",
            &degraded,
        ),
    }
}

/// The floor a place always lands on: the profile's own persistent state, and a
/// login done once inside a pane.
///
/// Not a fallback in the apologetic sense — it is the strategy ADR-28 actually
/// recommends for a subscription login, because the sessions of one profile
/// share one credential rather than holding copies of it.
fn volume_login(
    input: &CredentialInput<'_>,
    state_dir: Option<StateDir>,
    env: BTreeMap<String, String>,
    degraded: Vec<String>,
    home_dir: &str,
) -> CredentialPlan {
    let shared = "kept in this profile's sandbox home and shared by every session using it";
    let login = match signed_in(input, home_dir) {
        Some(true) => LoginState::Ready {
            source: format!("a login {shared}"),
        },
        Some(false) => LoginState::Required {
            reason: reason_for_login(input, &degraded),
            how: how_to_sign_in(input),
        },
        None => LoginState::Unknown {
            how: how_to_sign_in(input),
        },
    };
    let summary = match &login {
        LoginState::Ready { .. } => format!("already signed in — the login is {shared}"),
        LoginState::Unknown { how } => {
            format!("this agent declares none, so its state is {shared} — {how}")
        }
        LoginState::Required { how, .. } => format!("none in this sandbox yet — {how}"),
    };
    CredentialPlan {
        strategy: CredentialStrategy::VolumeLogin,
        note: note(CredentialStrategy::VolumeLogin, &summary, &degraded),
        env,
        secrets: BTreeMap::new(),
        state_dir,
        login,
    }
}

/// Why there is no credential, in one sentence, with whatever was refused on
/// the way here.
fn reason_for_login(input: &CredentialInput<'_>, degraded: &[String]) -> String {
    let profile = input.profile;
    let mut reason =
        format!("the sandbox for profile '{profile}' holds no login for this agent yet");
    for note in degraded {
        reason.push_str("; ");
        reason.push_str(note);
    }
    reason
}

/// What the user does about it — the agent's own words, plus the token route
/// when the agent declares one.
fn how_to_sign_in(input: &CredentialInput<'_>) -> String {
    let declared = input
        .declaration
        .and_then(|d| d.login_fallback.as_deref())
        .filter(|text| !text.trim().is_empty());
    let mut how = match declared {
        Some(text) => format!("sign in inside this pane: {text}"),
        None => "sign in inside this pane with this agent's own login command".to_string(),
    };
    if let Some(key) = first_secret_key(input) {
        how.push_str(&format!(
            ". A long-lived token works too, once: {}",
            input.store.how_to_store(&key)
        ));
    }
    how
}

/// The entry a token would live under, for the "or supply a token" hint.
fn first_secret_key(input: &CredentialInput<'_>) -> Option<SecretKey> {
    let family = input.family?;
    input
        .declaration?
        .secret_env
        .iter()
        .find_map(|name| SecretKey::new(family, name))
}

/// Whether this boundary already holds a login: `Some(true)`/`Some(false)` when
/// the declaration names the credential file friring can look for, `None` when
/// it does not.
///
/// Only ever `exists` — friring never opens a credential file to inspect it
/// (ADR-28), and does not need to: the question is whether one is there at all.
///
/// Deliberately answers from **that file alone**. "The state directory has
/// something in it" would be a guess in any case, and since config projection
/// writes the user's own settings into exactly that directory it would be a
/// guess that is wrong on every first launch — an agent reported as signed in
/// because friring had just put a `settings.json` beside where its login would
/// go. An indicator that lies is worse than one that says it cannot tell.
fn signed_in(input: &CredentialInput<'_>, home_dir: &str) -> Option<bool> {
    let tail = input
        .declaration?
        .credential_file
        .as_deref()
        .and_then(|raw| home_relative(raw, input.host_home))?;
    Some(std::path::Path::new(&join(home_dir, &tail)).exists())
}

/// Create the profile's state directory inside its synthetic home, and answer
/// with both spellings of it.
///
/// `0700` and friring's own, like every other directory a launch mints — and
/// created here rather than by the engine, because a bind mount's source has to
/// exist first and an engine that invents one does it as root.
///
/// Every component below the home is judged
/// ([`dirs::create_private_dir_under`]): the home is bind-mounted read-write
/// into a place that a *sibling* session may already be running in, so a live
/// agent can replace a directory on the way down with a link to somewhere on the
/// host and have friring create the tail of the declaration there.
fn relocate_state(
    declaration: Option<&AgentSandboxDef>,
    host_home: &str,
    home_dir: &str,
    inside_home: &str,
) -> SandboxResult<Option<StateDir>> {
    let Some(tail) = declaration
        .and_then(|d| d.state_dir.as_deref())
        .and_then(|raw| home_relative(raw, host_home))
    else {
        return Ok(None);
    };
    let host_path = dirs::create_private_dir_under(std::path::Path::new(home_dir), &tail)?
        .display()
        .to_string();
    Ok(Some(StateDir {
        host_path,
        inside_path: join(inside_home, &tail),
    }))
}

/// What a `seed-file` copy produced.
struct Seeded {
    /// The host file it came from, for the note. A path, never a content.
    source: String,
}

/// Copy the vendor credential file into this profile's sandbox home — once,
/// ever, anywhere.
///
/// Every refusal here is an `Err` whose text becomes the sentence beside the
/// login prompt: the copy does not happen and the launch continues without it.
/// The gates, in order:
///
/// 1. The declaration must **assert vendor support**. A rotating single-use
///    refresh token can never be declared, because the copy and the original
///    invalidate each other on the first refresh.
/// 2. The credential file must be named, home-relative, and actually there.
/// 3. **No other profile may already hold a copy.** That is the ADR-28 rule
///    with teeth: a second place would be a second consumer of one credential.
///
/// The marker that enforces (3) lives under friring's data directory rather
/// than inside the place's own tree — the tree is mounted read-write into the
/// container, so a marker in there would be the sandboxed agent's to delete.
fn seed(input: &CredentialInput<'_>, home_dir: &str) -> Result<Seeded, String> {
    let declaration = input
        .declaration
        .ok_or("this agent declares nothing, so there is no credential file to seed")?;
    if !declaration.seed_file_supported {
        return Err(
            "'seed-file' is refused: this agent's declaration does not assert that its vendor \
             supports copying the credential file (`seed_file_supported`), and copying a rotating \
             token invalidates it for whoever refreshes second (ADR-28)"
                .to_string(),
        );
    }
    let family = input.family.ok_or(
        "friring could not tell which credential family this agent belongs to, so it \
                cannot tell whether the credential is already seeded somewhere else",
    )?;
    let raw = declaration
        .credential_file
        .as_deref()
        .ok_or("'seed-file' is refused: this agent declares no `credential_file` to copy")?;
    let tail = home_relative(raw, input.host_home).ok_or_else(|| {
        format!(
            "'seed-file' is refused: '{raw}' is not inside the home directory, and a place's \
             credential lands under its own synthetic home"
        )
    })?;

    let source = join(input.host_home, &tail);
    if !std::path::Path::new(&source).exists() {
        return Err(format!(
            "'seed-file' has nothing to copy: '{source}' does not exist on this host"
        ));
    }

    let marker = marker_path(family)?;
    let fresh_claim = claim(&marker, input.profile)?;

    // `writeback` is what decides whether the copy or the host file is the
    // durable one. Set, the sandbox's copy is never overwritten again: the agent
    // refreshes it in there, and a relaunch that re-copied would put back a
    // credential the vendor has already retired. Unset, the host file is the
    // source of truth and every launch takes it afresh — which is the only
    // honest reading for a credential the agent does not rotate.
    let target = join(home_dir, &tail);
    if !declaration.writeback || !std::path::Path::new(&target).exists() {
        // Read and write whole, and never look at it: the contents are a
        // credential, so nothing here parses, logs or reports them. A file that
        // is not UTF-8 is refused rather than mangled through a lossy copy.
        let copied = std::fs::read_to_string(&source)
            .map_err(|e| format!("'seed-file' could not read '{source}': {e}. Nothing was copied"))
            .and_then(|contents| {
                // Under the home, component by component: the credential's own
                // directory is one a sibling agent in this place can replace
                // with a link, and following it would copy the credential onto
                // the host outside the boundary (`dirs::write_private_under`).
                dirs::write_private_under(std::path::Path::new(home_dir), &tail, &contents)
                    .map(|_| ())
                    .map_err(|e| format!("'seed-file' could not write the copy: {e}"))
            });
        if let Err(reason) = copied {
            // Nothing was copied, so a claim this call made must not outlive the
            // attempt — it would hold the family's one permitted copy for a
            // profile that has none.
            if fresh_claim {
                let _ = std::fs::remove_file(&marker);
            }
            return Err(reason);
        }
    }
    Ok(Seeded { source })
}

/// Take the family's marker for `profile`, answering whether *this* call created
/// it.
///
/// The claim is made **before** the copy, and by creating the marker
/// exclusively, because that is the only step that can decide between two
/// friring instances launching different profiles at the same moment: reading
/// the marker first and writing it after the copy would let both pass the holder
/// check and both copy one rotating credential, which is the second consumer
/// ADR-28 exists to prevent.
///
/// A marker already naming `profile` is that profile re-launching, not a second
/// copy, so it continues; any other holder is the refusal.
fn claim(marker: &std::path::Path, profile: &str) -> Result<bool, String> {
    fn recording(e: impl std::fmt::Display) -> String {
        format!("'seed-file' could not record which profile holds the copy: {e}")
    }
    if let Some(parent) = marker.parent() {
        dirs::create_private_dir(parent).map_err(recording)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(marker) {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(profile.as_bytes()).map_err(recording)?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let holder = std::fs::read_to_string(marker)
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
            match holder {
                Some(holder) if holder != profile => Err(format!(
                    "'seed-file' is refused: profile '{holder}' already holds a copy of this \
                     agent's credential, and a second copy would make two consumers of one token \
                     — the first refresh invalidates the other (ADR-28). Use 'env-token' or \
                     'volume-login' for profile '{profile}', or stop seeding it into '{holder}'"
                )),
                Some(_) => Ok(false),
                // A marker left empty or unreadable by an interrupted claim
                // names nobody, so it is taken over rather than left to refuse
                // every profile forever.
                None => {
                    std::fs::write(marker, profile).map_err(recording)?;
                    Ok(false)
                }
            }
        }
        Err(e) => Err(recording(e)),
    }
}

/// Which profile holds the one permitted copy of a family's credential.
///
/// Under `<data dir>/sandbox/seeds/`, which is inside the tree ADR-29 keeps out
/// of every boundary — so no sandbox can rewrite the record that stops a second
/// copy being made.
fn marker_path(family: &str) -> Result<std::path::PathBuf, String> {
    let root = dirs::seeds_root().ok_or(
        "friring cannot resolve its data directory, so it has nowhere to record which \
                profile holds a seeded credential",
    )?;
    Ok(root.join(dirs::sanitize_component(family)))
}

/// Give up every seed `profile` holds, because the profile is gone.
///
/// The marker is what refuses a second copy of one credential family
/// (ADR-28), and a marker naming a profile that no longer exists would refuse
/// forever — the boundary it protected was deleted along with the copy inside
/// it. Best-effort and idempotent: nothing here is worth failing a deletion
/// over, and a marker that survives only costs the *next* profile a `seed-file`
/// it can still get as `env-token` or `volume-login`.
///
/// Deliberately keyed on the *contents*, never on the file name: the marker is
/// named for the credential family, so "which ones did this profile hold?" can
/// only be answered by reading them.
pub fn release_seeds(profile: &str) {
    let Some(root) = dirs::sandbox_root().map(|root| root.join("seeds")) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if std::fs::read_to_string(&path).is_ok_and(|held| held.trim() == profile) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The token this agent declares, if friring holds one.
///
/// `Ok(None)` is "the agent declares no token, or there is none stored" — the
/// ordinary case, and the reason `volume-login` is the floor. `Err` is a store
/// that is there and would not answer, which the user can act on.
fn token(input: &CredentialInput<'_>) -> Result<Option<FoundToken>, String> {
    let declaration = input.declaration.filter(|d| !d.secret_env.is_empty());
    let Some(declaration) = declaration else {
        return Ok(None);
    };
    let family = input.family.ok_or(
        "this agent accepts a token, but friring could not tell which credential family it \
         belongs to, so it has no keychain entry to look in",
    )?;
    for name in &declaration.secret_env {
        let Some(key) = SecretKey::new(family, name) else {
            return Err(format!(
                "'{name}' is declared as a token variable but is not a usable environment \
                 variable name, so friring will not look for it"
            ));
        };
        match input.store.get(&key)? {
            Some(secret) if !secret.is_empty() => {
                // A stored value is a token or it is nothing. One carrying a
                // control character cannot survive the environment channel it
                // travels on — a newline in a `tmux setenv` ends the command —
                // so it is refused here rather than truncated into a credential
                // that silently does not work. The reason names the variable
                // and never the value.
                if secret.expose().chars().any(char::is_control) {
                    return Err(format!(
                        "the stored token for '{}' contains a control character, which no token \
                         does and which could not be carried into the sandbox intact",
                        key.variable()
                    ));
                }
                return Ok(Some(FoundToken { key, secret }));
            }
            _ => continue,
        }
    }
    Ok(None)
}

/// One stored token and the entry it came from.
struct FoundToken {
    key: SecretKey,
    secret: Secret,
}

/// The one line a launch shows: what the strategy is, in one clause, plus
/// anything that was refused on the way to it.
fn note(strategy: CredentialStrategy, summary: &str, degraded: &[String]) -> String {
    let mut note = format!("credentials ({strategy}): {summary}");
    for reason in degraded {
        note.push_str(" · ");
        note.push_str(reason);
    }
    note
}

/// The registry spelling of a requested strategy, for a message about it.
fn auth_name(auth: SandboxAuth) -> &'static str {
    match auth {
        SandboxAuth::Auto => "auto",
        SandboxAuth::HostPassthrough => "host-passthrough",
        SandboxAuth::EnvToken => "env-token",
        SandboxAuth::VolumeLogin => "volume-login",
        SandboxAuth::SeedFile => "seed-file",
    }
}

/// `raw` as a path relative to `home`, or `None` when it does not name
/// something under one.
///
/// Accepts both spellings a declaration can use — `~/.claude`, and the expanded
/// `/home/u/.claude` — because a registry written for one machine is read on
/// another. A component that is `.` or `..` is refused rather than normalised:
/// the result is joined onto a directory friring owns, and a traversal out of
/// it would put an agent's state somewhere nobody granted.
fn home_relative(raw: &str, home: &str) -> Option<String> {
    let trimmed = raw.trim();
    match home_relative_to_tilde(trimmed) {
        Some(tail) => Some(tail),
        None => {
            let home = home.trim().trim_end_matches('/');
            // An empty home would make the prefix `/`, and then *every*
            // absolute path would read as living under it — `/etc/shadow`
            // included. A launch always has one; this is the guard that keeps a
            // caller that does not from widening what a declaration can name.
            if home.is_empty() {
                return None;
            }
            let rest = trimmed.strip_prefix(&format!("{home}/"))?;
            clean_tail(rest)
        }
    }
}

/// The `~/…` half of [`home_relative`], which is the spelling a declaration
/// should use and the only one that means the same thing on every host.
fn home_relative_to_tilde(raw: &str) -> Option<String> {
    clean_tail(raw.trim().strip_prefix("~/")?)
}

/// A path tail with nothing in it that could climb out of the directory it is
/// joined onto.
fn clean_tail(rest: &str) -> Option<String> {
    let rest = rest.trim_matches('/');
    if rest.is_empty()
        || rest
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(rest.to_string())
}

/// Join a tail onto a directory, with exactly one separator.
fn join(base: &str, tail: &str) -> String {
    format!("{}/{tail}", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::keychain::StubStore;
    use super::*;
    use crate::paths::TestPathGuard;

    /// A fabricated token. Nothing in this file reads a real credential store:
    /// the keychain is stubbed, and every home is a temporary directory this
    /// test created.
    const FAKE_TOKEN: &str = "sk-fabricated-6a1c9e2d";

    /// A fabricated host home with a synthetic credential file in it, plus the
    /// data directory friring mints its own state under.
    struct Fixture {
        _data: TestPathGuard,
        home: std::path::PathBuf,
        /// The same path as a string, so an input can borrow it rather than a
        /// temporary.
        home_str: String,
        place_home: std::path::PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let data = TestPathGuard::new(
                std::env::temp_dir().join(format!("fr-cred-{}-{name}", std::process::id())),
            );
            // Both fabricated: the host home is a temp directory with synthetic
            // files, and the place's home is friring's own per-profile
            // directory under the (also fabricated) data directory.
            let home =
                std::env::temp_dir().join(format!("fr-cred-home-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).unwrap();
            let (_, place_home) = dirs::create_place_dirs("dev").unwrap();
            Self {
                _data: data,
                home_str: home.display().to_string(),
                home,
                place_home,
            }
        }

        /// A second profile's place, for the copy-twice refusal.
        fn other_place(&self, profile: &str) -> std::path::PathBuf {
            dirs::create_place_dirs(profile).unwrap().1
        }

        fn place_home(&self) -> String {
            self.place_home.display().to_string()
        }

        /// Write a synthetic credential file into the fabricated host home.
        fn with_host_credential(&self, tail: &str, contents: &str) -> String {
            let path = self.home.join(tail);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path.display().to_string()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    const INSIDE_HOME: &str = "/home/agent";

    fn claude() -> AgentSandboxDef {
        AgentSandboxDef {
            config_dir_env: Some("CLAUDE_CONFIG_DIR".into()),
            state_dir: Some("~/.claude".into()),
            credential_file: Some("~/.claude/.credentials.json".into()),
            secret_env: vec!["ANTHROPIC_API_KEY".into()],
            login_fallback: Some("/login".into()),
            state_rw: vec!["~/.claude".into()],
            ..Default::default()
        }
    }

    fn input<'a>(
        fixture: &'a Fixture,
        place_home: &'a str,
        declaration: Option<&'a AgentSandboxDef>,
        store: &'a dyn SecretStore,
    ) -> CredentialInput<'a> {
        CredentialInput {
            profile: "dev",
            boundary: Boundary::Place {
                home_dir: place_home,
                inside_home: INSIDE_HOME,
            },
            family: Some("claude"),
            declaration,
            host_home: &fixture.home_str,
            store,
        }
    }

    /// A policy backend's answer is host passthrough, whatever the declaration
    /// asks for — and nothing else is even attempted, which the store's read
    /// counter proves.
    #[test]
    fn a_policy_backend_uses_the_host_store_and_looks_for_nothing_else() {
        let store = StubStore::new().with_token("claude", "ANTHROPIC_API_KEY", FAKE_TOKEN);
        for auth in [
            SandboxAuth::Auto,
            SandboxAuth::HostPassthrough,
            SandboxAuth::EnvToken,
            SandboxAuth::VolumeLogin,
            SandboxAuth::SeedFile,
        ] {
            let declaration = AgentSandboxDef { auth, ..claude() };
            let plan = prepare(&CredentialInput {
                profile: "dev",
                boundary: Boundary::Policy,
                family: Some("claude"),
                declaration: Some(&declaration),
                host_home: "/fabricated/home",
                store: &store,
            })
            .unwrap();
            assert_eq!(
                plan.strategy,
                CredentialStrategy::HostPassthrough,
                "{auth:?}"
            );
            assert!(!plan.needs_private_env_channel(), "{auth:?}");
            assert!(plan.env.is_empty(), "{auth:?}");
            assert!(plan.state_dir.is_none(), "{auth:?}");
            assert!(!plan.login.needs_login(), "{auth:?}");
        }
        assert_eq!(
            store.reads(),
            0,
            "a policy backend must not go looking for an injected token"
        );
    }

    /// A place with a stored token injects it — as environment, never as argv,
    /// and named in the note without its value.
    #[test]
    fn a_place_injects_the_declared_token_from_the_keychain() {
        let fixture = Fixture::new("token");
        let place_home = fixture.place_home();
        let store = StubStore::new().with_token("claude", "ANTHROPIC_API_KEY", FAKE_TOKEN);
        let declaration = claude();
        let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();

        assert_eq!(plan.strategy, CredentialStrategy::EnvToken);
        assert_eq!(plan.secret_names(), ["ANTHROPIC_API_KEY"]);
        assert!(plan.needs_private_env_channel());
        assert_eq!(plan.secret_env(), [("ANTHROPIC_API_KEY", FAKE_TOKEN)]);
        assert!(!plan.login.needs_login());
        // The state directory is still relocated: a token is about
        // authentication, not about where the agent writes.
        assert_eq!(
            plan.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/home/agent/.claude")
        );
    }

    /// The requirement with the sharpest edge: no rendered form of a plan may
    /// carry the token.
    #[test]
    fn the_token_never_appears_in_anything_rendered() {
        let fixture = Fixture::new("redaction");
        let place_home = fixture.place_home();
        let store = StubStore::new().with_token("claude", "ANTHROPIC_API_KEY", FAKE_TOKEN);
        let declaration = claude();
        let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();

        let mut rendered = vec![
            format!("{plan:?}"),
            plan.note.clone(),
            format!("{:?}", plan.login),
            format!("{:?}", plan.state_dir),
            format!("{:?}", plan.strategy),
            plan.secret_names().join(","),
        ];
        // Everything the non-secret half could reach: a caller merges this into
        // a window's environment and logs the keys.
        rendered.extend(plan.env.iter().map(|(k, v)| format!("{k}={v}")));
        rendered.push(format!("{:?}", plan.env));
        for text in &rendered {
            assert!(!text.contains(FAKE_TOKEN), "a token leaked into: {text}");
        }
        // …and the one deliberate way out still hands it over.
        assert_eq!(plan.secret_env(), [("ANTHROPIC_API_KEY", FAKE_TOKEN)]);
    }

    /// A place with nothing stored is the common first launch, and it has to
    /// read as an instruction rather than as a failure.
    #[test]
    fn a_place_with_no_credential_asks_the_user_to_sign_in() {
        let fixture = Fixture::new("nologin");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        let declaration = claude();
        let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();

        assert_eq!(plan.strategy, CredentialStrategy::VolumeLogin);
        assert!(plan.login.needs_login());
        let LoginState::Required { reason, how } = &plan.login else {
            panic!("expected a login prompt, got {:?}", plan.login);
        };
        assert!(reason.contains("dev"), "{reason}");
        assert!(how.contains("/login"), "{how}");
        assert!(how.contains("inside this pane"), "{how}");
        // The token route is offered too, because this agent declares one.
        assert!(how.contains("ANTHROPIC_API_KEY"), "{how}");
        assert!(
            plan.note.contains("sign in inside this pane"),
            "{}",
            plan.note
        );
        // Nothing is injected, and the state directory is still there to sign
        // into.
        assert!(!plan.needs_private_env_channel());
        assert_eq!(
            plan.state_dir.as_ref().map(|s| s.inside_path.as_str()),
            Some("/home/agent/.claude")
        );
    }

    /// Once the login exists in the profile's own home, the same launch reads
    /// as ready — and friring answered by looking, never by opening the file.
    #[test]
    fn a_login_already_in_the_profiles_home_reads_as_ready() {
        let fixture = Fixture::new("signedin");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        let declaration = claude();
        // Fabricated: this is friring's own per-profile directory, and the file
        // is a placeholder standing in for whatever the agent writes there.
        let credential = fixture.place_home.join(".claude/.credentials.json");
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "{\"fabricated\":true}").unwrap();

        let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();
        assert_eq!(plan.strategy, CredentialStrategy::VolumeLogin);
        assert!(!plan.login.needs_login());
        assert!(
            plan.note.contains("shared by every session"),
            "{}",
            plan.note
        );
    }

    /// An agent that declares nothing still launches, still gets its state kept
    /// per profile, and still says what to do.
    #[test]
    fn an_agent_that_declares_nothing_gets_a_legible_state() {
        let fixture = Fixture::new("undeclared");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        let plan = prepare(&input(&fixture, &place_home, None, &store)).unwrap();

        assert_eq!(plan.strategy, CredentialStrategy::VolumeLogin);
        assert!(matches!(plan.login, LoginState::Unknown { .. }));
        assert!(plan.state_dir.is_none());
        assert!(plan.env.is_empty());
        assert!(
            plan.login
                .how()
                .is_some_and(|how| how.contains("inside this pane")),
            "{:?}",
            plan.login
        );
        assert_eq!(store.reads(), 0, "there was nothing declared to look up");
    }

    /// The declaration that cannot be honoured says so instead of failing
    /// quietly: a place has no host credential store to pass through to.
    #[test]
    fn a_place_asked_for_host_passthrough_degrades_with_the_reason() {
        let fixture = Fixture::new("passthrough");
        let place_home = fixture.place_home();
        let store = StubStore::new().with_token("claude", "ANTHROPIC_API_KEY", FAKE_TOKEN);
        let declaration = AgentSandboxDef {
            auth: SandboxAuth::HostPassthrough,
            ..claude()
        };
        let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();
        // The declared strategy is impossible here, so the ladder runs anyway
        // and the reason travels with the result.
        assert_eq!(plan.strategy, CredentialStrategy::EnvToken);
        assert!(plan.note.contains("outside the boundary"), "{}", plan.note);
    }

    /// A store that is present and will not answer is a different sentence from
    /// a store with nothing in it, because the fix is different.
    #[test]
    fn a_store_that_will_not_answer_says_so_beside_the_login_prompt() {
        let fixture = Fixture::new("locked");
        let place_home = fixture.place_home();
        let store = StubStore::failing("the keychain is locked");
        let declaration = claude();
        let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();
        assert_eq!(plan.strategy, CredentialStrategy::VolumeLogin);
        assert!(
            plan.note.contains("the keychain is locked"),
            "{}",
            plan.note
        );
        assert!(plan.login.needs_login());
    }

    fn seeding_codex() -> AgentSandboxDef {
        AgentSandboxDef {
            auth: SandboxAuth::SeedFile,
            config_dir_env: Some("CODEX_HOME".into()),
            state_dir: Some("~/.codex".into()),
            credential_file: Some("~/.codex/auth.json".into()),
            seed_file_supported: true,
            writeback: true,
            login_fallback: Some("codex login".into()),
            ..Default::default()
        }
    }

    // A place's synthetic home is minted under this host's data directory and
    // mounted at a path inside it, which a native Windows host has no place to
    // do (`crate::sandbox::select::NATIVE_WINDOWS`).

    /// ADR-28 with teeth: one copy, in one place, ever. The second profile is
    /// refused with the reason and falls back to signing in.
    #[cfg(unix)]
    #[test]
    fn a_second_place_may_not_copy_the_same_credential() {
        let fixture = Fixture::new("seed-twice");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        let declaration = seeding_codex();
        // A synthetic credential file in the fabricated host home. Nothing here
        // is the machine owner's: the home is a temp directory this test made.
        let source = fixture.with_host_credential(".codex/auth.json", "{\"fabricated\":\"token\"}");

        let first = prepare(&CredentialInput {
            family: Some("codex"),
            ..input(&fixture, &place_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(first.strategy, CredentialStrategy::SeedFile);
        assert!(first.note.contains(&source), "{}", first.note);
        let copied = fixture.place_home.join(".codex/auth.json");
        assert_eq!(
            std::fs::read_to_string(&copied).unwrap(),
            "{\"fabricated\":\"token\"}"
        );

        // A second profile asking for the same copy is refused — and gets a
        // login prompt rather than a broken launch.
        let other_home = fixture.other_place("other");
        let other_home = other_home.display().to_string();
        let second = prepare(&CredentialInput {
            profile: "other",
            family: Some("codex"),
            ..input(&fixture, &other_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(second.strategy, CredentialStrategy::VolumeLogin);
        assert!(second.note.contains("ADR-28"), "{}", second.note);
        assert!(
            second.note.contains("already holds a copy"),
            "{}",
            second.note
        );
        assert!(second.login.needs_login());
        assert!(
            !std::path::Path::new(&other_home)
                .join(".codex/auth.json")
                .exists(),
            "the refused copy was made anyway"
        );

        // The profile that holds the copy keeps re-launching without a second
        // copy being made, and — because this agent declares `writeback` — with
        // what the sandbox refreshed left alone.
        std::fs::write(&copied, "{\"fabricated\":\"rotated\"}").unwrap();
        let again = prepare(&CredentialInput {
            family: Some("codex"),
            ..input(&fixture, &place_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(again.strategy, CredentialStrategy::SeedFile);
        assert_eq!(
            std::fs::read_to_string(&copied).unwrap(),
            "{\"fabricated\":\"rotated\"}",
            "a relaunch overwrote a credential the sandbox had refreshed"
        );
    }

    /// The claim is taken *before* the copy, so a marker with no copy behind it
    /// yet still refuses the second profile — which is what a second friring
    /// instance sees in the window where the first has decided to seed and not
    /// yet written anything (ADR-28).
    #[test]
    fn a_claim_with_no_copy_behind_it_yet_still_refuses_the_second_profile() {
        let fixture = Fixture::new("seed-claimed");
        let store = StubStore::new();
        let declaration = seeding_codex();
        fixture.with_host_credential(".codex/auth.json", "{\"fabricated\":\"token\"}");

        // What the other instance's claim looks like from here: the family is
        // held by 'dev' and nothing has been copied into any place.
        let marker = marker_path("codex").unwrap();
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "dev").unwrap();

        let other_home = fixture.other_place("other");
        let other_home = other_home.display().to_string();
        let refused = prepare(&CredentialInput {
            profile: "other",
            family: Some("codex"),
            ..input(&fixture, &other_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(refused.strategy, CredentialStrategy::VolumeLogin);
        assert!(
            refused.note.contains("already holds a copy"),
            "{}",
            refused.note
        );
        assert!(
            !std::path::Path::new(&other_home)
                .join(".codex/auth.json")
                .exists(),
            "the refused copy was made anyway"
        );
    }

    /// Deleting the profile that holds the one permitted copy gives the family
    /// back. The marker outliving its boundary would refuse every future
    /// profile a `seed-file` on behalf of a place that no longer exists.
    #[test]
    fn deleting_the_holding_profile_releases_the_credential_family() {
        let fixture = Fixture::new("seed-release");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        let declaration = seeding_codex();
        fixture.with_host_credential(".codex/auth.json", "{\"fabricated\":\"token\"}");

        let first = prepare(&CredentialInput {
            family: Some("codex"),
            ..input(&fixture, &place_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(first.strategy, CredentialStrategy::SeedFile);

        // A profile that never held it releases nothing…
        release_seeds("someone-else");
        let other_home = fixture.other_place("other");
        let other_home = other_home.display().to_string();
        let refused = prepare(&CredentialInput {
            profile: "other",
            family: Some("codex"),
            ..input(&fixture, &other_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(refused.strategy, CredentialStrategy::VolumeLogin);

        // …and the holder's own deletion hands it back.
        release_seeds("dev");
        let second = prepare(&CredentialInput {
            profile: "other",
            family: Some("codex"),
            ..input(&fixture, &other_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(second.strategy, CredentialStrategy::SeedFile);
    }

    /// The other reading of `writeback`, and the only honest one for a
    /// credential the agent does not rotate: the host file is the source of
    /// truth, so every launch takes it afresh.
    #[test]
    fn a_credential_without_writeback_is_re_copied_from_the_host() {
        let fixture = Fixture::new("seed-static");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        let declaration = AgentSandboxDef {
            writeback: false,
            ..seeding_codex()
        };
        fixture.with_host_credential(".codex/auth.json", "{\"fabricated\":\"host\"}");

        let plan = prepare(&CredentialInput {
            family: Some("codex"),
            ..input(&fixture, &place_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(plan.strategy, CredentialStrategy::SeedFile);
        let copied = fixture.place_home.join(".codex/auth.json");
        std::fs::write(&copied, "{\"fabricated\":\"stale\"}").unwrap();

        prepare(&CredentialInput {
            family: Some("codex"),
            ..input(&fixture, &place_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&copied).unwrap(),
            "{\"fabricated\":\"host\"}"
        );
    }

    /// The vendor gate: without the declaration asserting it, nothing is
    /// copied, whatever the profile asked for.
    #[test]
    fn seeding_without_declared_vendor_support_copies_nothing() {
        let fixture = Fixture::new("seed-ungated");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        let declaration = AgentSandboxDef {
            seed_file_supported: false,
            ..seeding_codex()
        };
        fixture.with_host_credential(".codex/auth.json", "{\"fabricated\":\"token\"}");

        let plan = prepare(&CredentialInput {
            family: Some("codex"),
            ..input(&fixture, &place_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(plan.strategy, CredentialStrategy::VolumeLogin);
        assert!(plan.note.contains("seed_file_supported"), "{}", plan.note);
        assert!(plan.login.needs_login());
        assert!(
            !fixture.place_home.join(".codex/auth.json").exists(),
            "an ungated seed copied the credential anyway"
        );
    }

    /// A seed with nothing to copy is a login prompt, not a crash.
    #[test]
    fn seeding_with_no_credential_on_the_host_asks_for_a_login() {
        let fixture = Fixture::new("seed-missing");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        let declaration = seeding_codex();
        let plan = prepare(&CredentialInput {
            family: Some("codex"),
            ..input(&fixture, &place_home, Some(&declaration), &store)
        })
        .unwrap();
        assert_eq!(plan.strategy, CredentialStrategy::VolumeLogin);
        assert!(plan.note.contains("nothing to copy"), "{}", plan.note);
        assert!(plan
            .login
            .how()
            .is_some_and(|how| how.contains("codex login")));
    }

    /// The state directory is friring's own, private, and inside the home the
    /// place already mounts — so nothing new has to be bound to make a login
    /// persist.
    #[cfg(unix)]
    #[test]
    fn the_state_directory_is_created_private_inside_the_profiles_home() {
        let fixture = Fixture::new("statedir");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        let declaration = claude();
        let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();
        let state = plan.state_dir.expect("a declared state directory");
        assert_eq!(state.host_path, format!("{place_home}/.claude"));
        assert_eq!(state.inside_path, "/home/agent/.claude");
        assert!(std::path::Path::new(&state.host_path).is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&state.host_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    /// A declared path that could climb out of the home it is joined onto is
    /// not relocated at all — the result is a directory friring owns, and a
    /// traversal would put an agent's state somewhere nobody granted.
    #[test]
    fn a_state_path_that_could_escape_the_home_is_not_relocated() {
        let fixture = Fixture::new("traversal");
        let place_home = fixture.place_home();
        let store = StubStore::new();
        for raw in ["~/../../etc", "~/", "~", "/etc/shadow", "relative/path"] {
            let declaration = AgentSandboxDef {
                state_dir: Some(raw.to_string()),
                config_dir_env: Some("CLAUDE_CONFIG_DIR".into()),
                ..Default::default()
            };
            let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();
            assert!(plan.state_dir.is_none(), "{raw} was relocated");
            assert!(plan.env.is_empty(), "{raw} still set the config variable");
        }
    }

    /// Everything a declaration contributes to the launch's environment is held
    /// to what an environment variable may be called: `agents.toml` is a
    /// hand-edited file, and these names reach a `tmux setenv` on the way in.
    #[test]
    fn a_variable_name_a_declaration_could_not_use_is_not_set() {
        let fixture = Fixture::new("envname");
        let place_home = fixture.place_home();
        let store = StubStore::new().with_token("claude", "ANTHROPIC_API_KEY", FAKE_TOKEN);
        let declaration = AgentSandboxDef {
            config_dir_env: Some("BAD NAME; echo".into()),
            secret_env: vec!["ALSO BAD".into()],
            ..claude()
        };
        let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();
        assert!(plan.env.is_empty(), "{:?}", plan.env);
        assert!(!plan.needs_private_env_channel());
        assert!(plan.note.contains("BAD NAME; echo"), "{}", plan.note);
        assert!(plan.note.contains("ALSO BAD"), "{}", plan.note);
    }

    /// A value that could not survive the channel it travels on is refused
    /// rather than truncated into a credential that silently does not work —
    /// and the refusal names the variable, never the value.
    #[test]
    fn a_stored_value_with_a_control_character_is_not_injected() {
        let fixture = Fixture::new("controlchar");
        let place_home = fixture.place_home();
        let poisoned = format!("{FAKE_TOKEN}\nsetenv -g x y");
        let store = StubStore::new().with_token("claude", "ANTHROPIC_API_KEY", &poisoned);
        let declaration = claude();
        let plan = prepare(&input(&fixture, &place_home, Some(&declaration), &store)).unwrap();

        assert_eq!(plan.strategy, CredentialStrategy::VolumeLogin);
        assert!(!plan.needs_private_env_channel());
        assert!(plan.note.contains("control character"), "{}", plan.note);
        assert!(!plan.note.contains(FAKE_TOKEN), "{}", plan.note);
        assert!(!format!("{plan:?}").contains(FAKE_TOKEN));
    }

    #[test]
    fn home_relative_accepts_both_spellings_and_refuses_the_rest() {
        assert_eq!(
            home_relative("~/.claude/.credentials.json", "/home/u").as_deref(),
            Some(".claude/.credentials.json")
        );
        assert_eq!(
            home_relative("/home/u/.codex/auth.json", "/home/u").as_deref(),
            Some(".codex/auth.json")
        );
        assert_eq!(
            home_relative("/home/u/.codex/auth.json", "/home/u/").as_deref(),
            Some(".codex/auth.json")
        );
        for raw in [
            "/etc/passwd",
            "~/..",
            "~/./x",
            "~/../u2/.ssh",
            "~",
            "~/",
            ".codex/auth.json",
            "/home/user2/.codex",
        ] {
            assert!(
                home_relative(raw, "/home/u").is_none(),
                "{raw} must not resolve under the home"
            );
        }
        // A home friring could not resolve makes every absolute path look like
        // it lives under one, which is the widening this guard exists for.
        for home in ["", "   ", "/"] {
            assert!(
                home_relative("/etc/shadow", home).is_none(),
                "an empty home accepted /etc/shadow"
            );
        }
    }
}
