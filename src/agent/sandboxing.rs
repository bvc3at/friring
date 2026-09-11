//! Applying a session's sandbox profile to the invocation it is about to run.
//!
//! `docs/SANDBOX.md` §Launch integration: a policy backend is a *decorator* on
//! the composed invocation — argv in, longer argv out — applied at the one seam
//! both the TUI and the headless paths pass through, and **before** per-transport
//! composition (transports quote and fold differently; the Windows multiplexer
//! path collapses everything into a single token, so wrapping at the shell-string
//! level would not survive).
//!
//! Everything with a policy in it lives in [`crate::sandbox`]; this module is
//! only the glue that turns a [`SessionConfig`] into the
//! [`SandboxLaunch`] that module wants, and the
//! answer back into the three things a spawn needs: a command, its arguments,
//! and the extra environment.

use std::collections::HashMap;

use crate::sandbox::{SandboxHost, SandboxLaunch};
use crate::session::{AgentDef, SessionConfig};

/// Re-exported because `session_ops` may not reference [`crate::sandbox`] at
/// all (`tests/architecture_rules.rs`), and the headless launch paths have to
/// name the handle they hold across a kill and a spawn.
pub use crate::sandbox::PendingEgress;

/// What a launch is *for*, when it is not an ordinary session.
///
/// Carried into [`apply`] rather than inferred, because every refusal below
/// depends on it and inferring one would mean guessing at a boundary. `None` is
/// an ordinary session, which is the overwhelmingly common case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BridgeLaunch {
    /// The narrowing this child runs under (ADR-31).
    pub overlay: crate::session::SandboxOverlay,
    /// The gate this child waits on and the key that opens it (ADR-33).
    pub gate: Option<(String, String)>,
    /// The child's own bridge directory, for `FRIRING_BRIDGE_DIR`.
    pub bridge_dir: Option<String>,
    /// The private state directory the agent is pointed at, and the plan that
    /// filled it.
    pub state_dir: Option<String>,
}

/// Whether a launch may carry the bridge at all, and why not when it may not.
///
/// Every one of these is a **refusal**, never a degraded launch, and every one
/// is `integrity: true` so a profile's `allow_unsandboxed_fallback` cannot
/// answer it. The switch means "this host cannot apply this profile, run on the
/// host instead"; a bridge-required agent started on the host has no boundary
/// *and* a channel nobody is serving, which is strictly worse than not starting.
pub fn bridge_refusal(
    def: Option<&AgentDef>,
    config: &SessionConfig,
    caps: Option<&crate::sandbox::Caps>,
    from_tui: bool,
) -> Option<Refusal> {
    let required: Vec<crate::session::BridgeCapability> = def
        .and_then(|d| d.sandbox.as_ref())
        .map(|s| s.bridge_requires.clone())
        .unwrap_or_default();
    if required.is_empty() {
        return None;
    }
    let integrity = |reason: String| {
        Some(Refusal {
            reason,
            integrity: true,
        })
    };
    let Some(profile) = config.sandbox.as_ref() else {
        return integrity(format!(
            "agent '{}' requires the orchestration bridge, which is granted by a sandbox \
             profile — and this session has none. A bridge-required agent is never started \
             as a plain session",
            config.agent
        ));
    };
    let missing: Vec<String> = required
        .iter()
        .filter(|cap| !profile.bridge_grants.contains(cap))
        .map(|cap| cap.to_string())
        .collect();
    if !missing.is_empty() {
        return integrity(format!(
            "grant_missing: agent '{}' requires the bridge capabilities [{}], which sandbox \
             profile '{}' does not grant",
            config.agent,
            missing.join(", "),
            profile.name
        ));
    }
    if caps.is_some_and(|caps| !caps.bridge) {
        return integrity(format!(
            "sandbox profile '{}' resolves to a backend that cannot carry the orchestration \
             bridge: the bridge is a directory friring mints on the host and exposes inside \
             the boundary, and a place or a remote host has neither at that path",
            profile.name
        ));
    }
    if config
        .backend
        .as_deref()
        .is_some_and(crate::session::is_remote_backend)
    {
        return integrity(format!(
            "agent '{}' requires the orchestration bridge, and this session runs on a remote \
             host: friring builds the boundary — and mints the bridge directory — on the \
             machine it runs on",
            config.agent
        ));
    }
    if !from_tui {
        return integrity(format!(
            "bridge_requires_tui: agent '{}' requires the orchestration bridge, which is \
             served by a running friring TUI. A headless `friring-cli session create` exits \
             after spawning, so nothing would own this session's egress proxy or answer its \
             requests. Create it from the TUI instead",
            config.agent
        ));
    }
    None
}

/// A launch with its sandbox profile applied.
#[derive(Clone, PartialEq, Eq)]
pub struct SandboxedInvocation {
    /// The wrapper program (`sandbox-exec`, `bwrap`, …).
    pub command: String,
    /// The wrapper's arguments, ending in the agent's own command line.
    pub args: Vec<String>,
    /// Environment the policy and the agent's declaration ask for, merged into
    /// the tmux window's environment by the caller. A policy backend applies
    /// nothing in argv — a policy is a rule on a process, so the wrapped agent
    /// inherits the window — which makes this the only channel inward.
    pub env: HashMap<String, String>,
    /// The credential half of that environment, kept **out** of
    /// [`env`](Self::env) so a caller has to decide about it rather than
    /// inherit it.
    ///
    /// A value here may only travel over a channel that is not a command line:
    /// the control-mode `new-window` the TUI and every off-host spawn use puts
    /// it in a command sent over the tmux socket, where `friring-cli`'s local
    /// one-shot spawn would put it in a `tmux -e KEY=VALUE` argument any local
    /// user can read out of `/proc/<pid>/cmdline` (`docs/SANDBOX.md` §Failure
    /// modes). A caller holding only that channel refuses the launch.
    ///
    /// Empty for every policy backend: the host's own credential store is
    /// already reachable there, so nothing is injected (ADR-28).
    pub secret_env: Vec<(String, String)>,
    /// The whole composition, for the log line: `sandbox: dev (seatbelt) ·
    /// inner agent sandbox: off — Friring is the boundary`.
    pub label: String,
    /// The same composition minus the profile name: the payload of the
    /// [`Applied`](crate::session::SandboxState::Applied) half of
    /// [`SessionInfo::sandbox_state`](crate::session::SessionInfo::sandbox_state)
    /// — the info panel already labels the row with the profile.
    pub state: String,
    /// The **place** this invocation runs in, for a place backend (ADR-26).
    /// `None` for a policy backend, whose boundary is the wrapped process and
    /// whose tmux window is on the host.
    ///
    /// The caller spawns through this instead of the session's own backend: it
    /// is the transport, and its [`name`](crate::agent::SessionBackend::name)
    /// is the `sandbox:<profile>` that lands in `backend_type` and drives
    /// restore.
    pub place: Option<crate::agent::transport::Place>,
    /// The place row to record in `sandbox_instances`, so garbage collection
    /// can find a container this launch created. `None` for a policy backend,
    /// which creates nothing that outlives the process.
    pub instance: Option<crate::sandbox::SandboxInstance>,
    /// Where this launch's egress proxy listens and the credential it demands,
    /// for the launch path to persist (`sessions.egress_*`).
    ///
    /// A restart has to rebind the **same** pair or the agent that is still
    /// running holds proxy URLs naming a port and a credential nothing answers
    /// on. Carried on the invocation rather than looked up afterwards because
    /// this is the one place that knows both, and its `state` is
    /// [`Preparing`](crate::session::EgressState::Preparing) until the launch
    /// commits.
    ///
    /// The token never reaches [`Debug`], which is why the record has one of its
    /// own.
    pub egress: crate::session::EgressRecord,
    /// What the user has to type in the pane to sign this agent in, when the
    /// boundary starts it signed out — for
    /// [`SessionInfo::sandbox_login`](crate::session::SessionInfo::sandbox_login).
    ///
    /// `None` whenever there is nothing to do, which is every policy launch and
    /// every place that already has a credential. It is an *instruction*, never
    /// a credential: the agent's own login command, plus how to store a token.
    pub login: Option<String>,
}

impl std::fmt::Debug for SandboxedInvocation {
    /// Hand-written, for the reason
    /// [`CredentialPlan`](crate::sandbox::CredentialPlan)'s is: a derived
    /// `Debug` would put a vendor token in every `{:?}` — an `expect`, a
    /// tracing field, a failing assertion's own message. The variable *names*
    /// are the diagnostic; the values are the secret.
    ///
    /// [`env`](Self::env)'s proxy variables are held back for the same reason,
    /// and that is not belt-and-braces: [`secret_env`](Self::secret_env) is the
    /// channel for an *agent's* token, and the egress proxy's credential does
    /// not travel there at all — it is inside the proxy URLs friring composes
    /// into `env` (see [`ProxyGrant`](crate::sandbox::ProxyGrant), whose own
    /// `Debug` withholds them). Printing this struct's `env` whole would undo
    /// that one field further out, which is how a live credential for the
    /// boundary a session is running behind ends up in a log somebody pastes
    /// into a bug report.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: std::collections::BTreeMap<&str, &str> = self
            .env
            .iter()
            .map(|(key, value)| {
                let credential = crate::sandbox::egress::HTTP_PROXY_VARS
                    .iter()
                    .chain(crate::sandbox::egress::SOCKS_PROXY_VARS)
                    .any(|name| name == key);
                let shown = if credential { "<proxy url>" } else { value };
                (key.as_str(), shown)
            })
            .collect();
        f.debug_struct("SandboxedInvocation")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env", &env)
            .field(
                "secret_env",
                &self.secret_env.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("label", &self.label)
            .field("state", &self.state)
            .field("place", &self.place)
            .field("instance", &self.instance)
            .field("egress", &self.egress)
            .field("login", &self.login)
            .finish()
    }
}

/// What [`apply`] decided about one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxDecision {
    /// The session carries no profile. The overwhelmingly common case.
    Unsandboxed,
    /// The profile applied.
    Wrapped(Box<SandboxedInvocation>),
    /// The profile could **not** be applied and its
    /// `allow_unsandboxed_fallback` switch permits launching anyway. The reason
    /// belongs in front of the user: a session the user believes is sandboxed
    /// and is not is the worst of the three outcomes.
    Skipped { reason: String },
}

/// Apply `config.sandbox` to an already-composed `(command, args)`.
///
/// `def` is the agent being launched — its `[agents.<name>.sandbox]` block
/// contributes the flags that turn the agent's *own* sandbox off (nesting is
/// denied outright under seatbelt) and the state directories it must keep
/// writable, and its credential family decides whose credential files stay
/// readable. `None` is an agent friring could not resolve a definition for; the
/// profile still applies, the agent just gets no help.
///
/// A filtered profile binds an egress proxy here, because argv has to name the
/// port or the socket. That instance is **provisional**: the session keeps
/// whatever it is already using until the launch this composed for is running.
/// Every caller must therefore take [`pending_egress`] before it can fail and
/// [`commit`](PendingEgress::commit) it once the pane exists — the boundary is
/// released, and the session's own left alone, on every path that does not.
///
/// # Errors
///
/// The profile named a backend that is unavailable here, resolved to a policy
/// this build cannot express, could not write its generated profile, could not
/// be given the egress proxy its network mode is enforced by, or asks for a
/// boundary friring will not grant — a read-write path reaching the database or
/// the host's tmux socket, a path that is not valid UTF-8 — and the profile
/// does not permit running unsandboxed. Failing the spawn is deliberate:
/// quietly launching an agent outside the boundary the user asked for is a
/// security regression, not a degraded mode.
pub fn apply(
    def: Option<&AgentDef>,
    config: &SessionConfig,
    command: &str,
    args: &[String],
) -> Result<SandboxDecision, String> {
    apply_for(def, config, command, args, None, true)
}

/// [`apply`], told what this launch is for.
///
/// `bridge` is the child narrowing when this launch is a bridge child's, and
/// `from_tui` is whether a running TUI will own the session's egress proxy and
/// answer its bridge requests. Both default the ordinary way in [`apply`]; the
/// headless path passes `from_tui = false`, which is what refuses a
/// bridge-required agent there rather than starting one whose channel nobody
/// serves.
///
/// # Errors
///
/// As [`apply`], plus every [`bridge_refusal`] — each of which is an integrity
/// refusal a profile's `allow_unsandboxed_fallback` may not answer.
pub fn apply_for(
    def: Option<&AgentDef>,
    config: &SessionConfig,
    command: &str,
    args: &[String],
    bridge: Option<&BridgeLaunch>,
    from_tui: bool,
) -> Result<SandboxDecision, String> {
    // Asked before anything is minted or bound: a refusal here costs nothing,
    // and a bridge-required agent must never reach a launch path that would
    // start it without one.
    let caps = config.sandbox.as_ref().and_then(|profile| {
        with_host(|host| {
            host.select(profile.backend)
                .backend()
                .ok()
                .and_then(|backend| host.backend(backend).map(|b| b.capabilities()))
        })
    });
    if let Some(refusal) = bridge_refusal(def, config, caps.as_ref(), from_tui) {
        return Err(refusal.reason);
    }
    let Some(profile) = config.sandbox.as_ref() else {
        // A session whose profile was cleared keeps nothing running — but not
        // before this launch happens, because until then the agent still
        // running is the one that boundary belongs to. Free for the
        // overwhelmingly common case: no supervisor is started to say so.
        PendingEgress::clearing(&session_key(config)).park();
        return Ok(SandboxDecision::Unsandboxed);
    };
    // A bridge launch has no fallback, whatever the profile says: an agent that
    // can leave its boundary by breaking it is not sandboxed, and the bridge is
    // the one channel that would then be answered for a process running on the
    // host. `SandboxPolicy::narrow` already refuses it for a child; this is the
    // same rule for the launch the profile itself asks for, and it covers the
    // composition failures `bridge_refusal` cannot see — no backend probe
    // resolved, so `caps` is `None` and the capability check above is vacuous.
    let bridge_launch = bridge.is_some()
        || def
            .and_then(|d| d.sandbox.as_ref())
            .is_some_and(|s| !s.bridge_requires.is_empty());
    let fallback = profile.allow_unsandboxed_fallback && !bridge_launch;
    let home = match crate::paths::home_dir() {
        Some(home) => match representable("the home directory", &home) {
            Ok(home) => home,
            Err(reason) if fallback => return Ok(SandboxDecision::Skipped { reason }),
            Err(reason) => return Err(reason),
        },
        None if fallback => {
            return Ok(SandboxDecision::Skipped {
                reason: NO_HOME.to_string(),
            })
        }
        None => return Err(NO_HOME.to_string()),
    };
    match with_host(|host| build(host, &home, def, config, command, args, bridge)) {
        Ok(invocation) => Ok(SandboxDecision::Wrapped(Box::new(invocation))),
        // The switch's meaning is "this host cannot apply this profile" — not
        // "the boundary's own state is wrong, so run outside it". A launch
        // refused because something inside a sandbox interfered with what
        // friring builds the next boundary out of is failed, whatever the
        // profile says, because otherwise the way for an agent to leave its
        // sandbox is to break it (`SandboxError::Tampered`).
        Err(refusal) if fallback && !refusal.integrity => Ok(SandboxDecision::Skipped {
            reason: refusal.reason,
        }),
        Err(refusal) => Err(refusal.reason),
    }
}

/// Why a launch could not be composed, and whether the profile's
/// `allow_unsandboxed_fallback` is allowed to answer it.
///
/// `integrity` marks the refusals that must fail the launch outright: a mount
/// source that turned out to travel through a symlink, a directory friring mints
/// that is now a link, an egress socket something else is holding. Every one of
/// those is reachable from *inside* a boundary, so answering them by launching
/// on the host would hand a sandboxed agent a way out that it can take on
/// purpose. See [`crate::sandbox::SandboxError::Tampered`].
///
/// Most of `build` refuses with a plain `String`; `From` keeps those sites
/// unchanged and files them as ordinary refusals, which is what they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The sentence shown to whoever asked for the launch.
    pub reason: String,
    /// Set when the fallback may not answer this.
    pub integrity: bool,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl From<String> for Refusal {
    fn from(reason: String) -> Self {
        Self {
            reason,
            integrity: false,
        }
    }
}

impl From<crate::sandbox::SandboxError> for Refusal {
    fn from(error: crate::sandbox::SandboxError) -> Self {
        Self {
            integrity: error.is_tampering(),
            reason: error.to_string(),
        }
    }
}

/// Whether the supervisor holds this session's committed egress instance.
///
/// Re-exported here because `session_ops` may not reference
/// [`crate::sandbox`] at all (`tests/architecture_rules.rs`), and the headless
/// launch path has to ask the same question the TUI's does.
pub fn egress_acknowledged(session_key: &str) -> bool {
    crate::sandbox::egress::acknowledged(session_key)
}

/// Take the egress instance the composition for this session prepared, so the
/// launch that is about to happen owns it.
///
/// Always answers, so a launch path treats every session the same way: a
/// session with no profile — or one whose network mode the kernel enforces on
/// its own — gets a handle with nothing to commit. Call it immediately after
/// [`apply`], before anything that can fail, and
/// [`commit`](PendingEgress::commit) it once the agent's pane exists.
pub fn pending_egress(config: &SessionConfig) -> PendingEgress {
    crate::sandbox::egress::claim(&session_key(config))
}

/// friring's own CLI, which every policy launch runs inside the boundary as its
/// launch helper (ADR-33).
///
/// Resolved from the running binary, never from `PATH`, for the reason the relay
/// is: the program that applies a launch's last boundary steps must be *this*
/// friring's. `None` refuses the launch in whichever backend composes it.
///
/// A test binary lives in `target/debug/deps` with no `friring-cli` beside it,
/// so under `cfg(test)` this answers a fixed path instead. That keeps every
/// composition test about what it is about; the refusal itself is asserted
/// directly against the backends, where the launch is built by hand.
fn local_helper_program() -> Option<String> {
    #[cfg(test)]
    {
        Some(TEST_HELPER_PROGRAM.to_string())
    }
    #[cfg(not(test))]
    {
        crate::sandbox::bwrap::local_relay_program().map(|p| p.display().to_string())
    }
}

/// The launch helper a composition test sees — see [`local_helper_program`].
#[cfg(test)]
pub(crate) const TEST_HELPER_PROGRAM: &str = "/usr/local/bin/friring-cli";

/// Run `wrap` against the host friring itself runs on — or, in a test, the one
/// it installed.
///
/// Every path that resolves a backend goes through here rather than reaching
/// for [`SandboxHost::local_shared`] itself, and the reason is the one
/// [`TestSandboxHost`] gives: a path that does not asserts something different
/// depending on whether the machine running the test happens to have seatbelt
/// or bubblewrap. Egress restoration is such a path, in `app::sandbox`.
pub(crate) fn with_host<R>(wrap: impl FnOnce(&SandboxHost) -> R) -> R {
    #[cfg(test)]
    if let Some(host) = TEST_HOST.with(|installed| installed.borrow().clone()) {
        return wrap(&host);
    }
    wrap(SandboxHost::local_shared())
}

#[cfg(test)]
thread_local! {
    /// See [`TestSandboxHost`].
    static TEST_HOST: std::cell::RefCell<Option<std::sync::Arc<SandboxHost>>> =
        const { std::cell::RefCell::new(None) };
}

/// Compose against a fabricated host for as long as this guard lives — the
/// sandbox twin of [`crate::paths::TestPathGuard`], and thread-local for the
/// same reason.
///
/// [`apply`] resolves the backend from the machine friring runs on, so without
/// this every test of a *launch* would assert something different depending on
/// whether the host it ran on happened to have seatbelt or bubblewrap
/// installed. Nothing here executes a sandbox: the wrapped argv is composed and
/// handed to a stub.
#[cfg(test)]
pub(crate) struct TestSandboxHost;

#[cfg(test)]
impl TestSandboxHost {
    pub(crate) fn new(host: SandboxHost) -> Self {
        Self::install(std::sync::Arc::new(host))
    }

    /// Install an already-shared host, so a worker thread can be given the one
    /// the test installed.
    ///
    /// The override is thread-local — see [`crate::paths::test_dir_override`]
    /// for why — so a blocking task that composes a launch would otherwise
    /// resolve the *machine's* backends and make every such test depend on
    /// whether seatbelt or bubblewrap happened to be installed.
    pub(crate) fn install(host: std::sync::Arc<SandboxHost>) -> Self {
        TEST_HOST.with(|installed| *installed.borrow_mut() = Some(host));
        Self
    }

    /// The host installed on this thread, for handing to a worker.
    pub(crate) fn installed() -> Option<std::sync::Arc<SandboxHost>> {
        TEST_HOST.with(|installed| installed.borrow().clone())
    }

    /// A host offering seatbelt, whatever this machine is.
    ///
    /// The one shape that composes end to end inside a test process: the
    /// transport is host loopback, so no relay — and therefore no
    /// `friring-cli` beside the test binary — is involved. Named rather than
    /// built by the caller because `session_ops` may not reference
    /// [`crate::sandbox`] at all, and its launch paths need this too.
    pub(crate) fn seatbelt() -> Self {
        Self::new(SandboxHost::new(std::sync::Arc::new(
            crate::sandbox::probe::StubHost::macos(26, true),
        )))
    }

    // The place cases below are unix-only, and not for a fixture's sake: a
    // place mounts every path at exactly its host path, which is why a native
    // Windows friring is offered no backend at all
    // (`crate::sandbox::select::NATIVE_WINDOWS`). Inside WSL friring is a Linux
    // binary and they all run.

    /// A host offering rootless podman, so a launch resolves to a **place**.
    ///
    /// The other shape a launch path has to be tested against, and named here
    /// for the same reason [`Self::seatbelt`] is: `session_ops` may not
    /// reference [`crate::sandbox`], and one of the things a place changes is
    /// what its launch is allowed to put in the window's environment (ADR-29).
    /// Nothing is pulled, built or started — every engine command is answered by
    /// the injected probe host, and the place's own directories are friring's,
    /// under whatever data directory the test pinned.
    #[cfg(unix)]
    pub(crate) fn place(profile: &str, workspace: &str) -> Self {
        use crate::sandbox::probe::ProbeOutput;
        const PODMAN: &str = "/usr/bin/podman";
        let (place_dir, home_dir) = crate::sandbox::create_place_dirs(profile)
            .expect("a data directory to mint the place tree under");
        let stub = crate::sandbox::probe::StubHost::new()
            .with_home("/fabricated/home")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("podman")
            .with_command("id -u", ProbeOutput::success("1000\n"))
            .with_command("id -g", ProbeOutput::success("1000\n"))
            .with_command(
                &format!(
                    "{PODMAN} info --format {}",
                    "{{.Version.Version}}|{{.Host.Security.Rootless}}"
                ),
                ProbeOutput::success("5.2.2|true\n"),
            )
            .with_path(workspace)
            .with_path(&place_dir.display().to_string())
            .with_path(&home_dir.display().to_string())
            .with_command_prefix(
                &format!("{PODMAN} image inspect"),
                ProbeOutput::success("[{}]\n"),
            )
            .with_command_prefix(
                &format!("{PODMAN} run"),
                ProbeOutput::success(
                    "1f2e3d4c5b6a798807162534435261708192a3b4c5d6e7f8091a2b3c4d5e6f70\n",
                ),
            )
            // Answers both `command -v` questions a launch asks of a place: the
            // relay binary and the agent it is about to run.
            .with_command_prefix(
                &format!("{PODMAN} exec"),
                ProbeOutput::success("/usr/local/bin/friring-cli\n"),
            );
        Self::new(SandboxHost::new(std::sync::Arc::new(stub)))
    }
}

#[cfg(test)]
impl Drop for TestSandboxHost {
    fn drop(&mut self) {
        TEST_HOST.with(|installed| *installed.borrow_mut() = None);
    }
}

/// Every sandbox path is expanded against a home, and a profile stores `~`
/// un-expanded precisely so it can be expanded against a *different* one.
const NO_HOME: &str = "Cannot resolve a home directory to expand sandbox paths against";

/// A security-relevant path as the sandbox layer needs it: exactly, or not at
/// all.
///
/// Every rule a backend writes is a string. A lossy conversion turns a
/// non-UTF-8 byte into `U+FFFD`, and the rule then names a path that does not
/// exist while the real one stays visible — an ADR-29 deny that denies nothing,
/// a secrets deny that hides nothing. The launch is refused instead, in front of
/// the user, wherever a path of this kind cannot be spelled.
fn representable(what: &str, path: &std::path::Path) -> Result<String, String> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        format!(
            "Cannot apply a sandbox profile: {what} ('{}') is not valid UTF-8, and a sandbox rule \
             built from an approximation of it would name a different file",
            path.display()
        )
    })
}

/// The whole call order `docs/SANDBOX.md` prescribes, in one place: select a
/// concrete backend, resolve the profile against it, fold in what the agent
/// declares, then wrap.
///
/// `host` and `home` are injected rather than resolved here so the composition
/// is testable on a machine that has no backend installed — and so a future
/// remote host can be wrapped by handing in its own pair.
fn build(
    host: &SandboxHost,
    home: &str,
    def: Option<&AgentDef>,
    config: &SessionConfig,
    command: &str,
    args: &[String],
    bridge: Option<&BridgeLaunch>,
) -> Result<SandboxedInvocation, Refusal> {
    let profile = config
        .sandbox
        .as_ref()
        .ok_or_else(|| "no sandbox profile on this session".to_string())?;

    let selection = host.select(profile.backend);
    let backend = selection
        .backend()
        .map_err(|e| format!("{e}\n{}", selection.rejection_summary()))?;
    let shape = backend.shape();

    // Both shapes are built on the machine friring runs on: a policy backend
    // generates its artefacts here (a `.sb` profile file, an argv naming local
    // paths), and a place is created by an engine here, with *this* machine's
    // paths mounted. Either one around a session whose worktrees and tmux are on
    // another host would be a boundary around the wrong filesystem.
    //
    // A place-backed session's own `sandbox:<profile>` backend never trips this
    // — it is not a remote backend, and it says where the *boundary* is rather
    // than where the session's machine is — so a relaunch composes normally.
    if config
        .backend
        .as_deref()
        .is_some_and(crate::session::is_remote_backend)
    {
        return Err(format!(
            "Sandbox profile '{}' cannot be applied to a session on a remote host: friring \
             builds the boundary on the machine it runs on",
            profile.name
        )
        .into());
    }

    let agent_sandbox = def.and_then(|d| d.sandbox.as_ref());
    let mut policy = profile.resolve(backend, home).map_err(|e| e.to_string())?;
    crate::sandbox::apply_agent_requirements(&mut policy, agent_sandbox, home);
    // A bridge child's policy is its owner's, narrowed (ADR-31) — and narrowed
    // **here**, at every launch, against the parent's profile as it stands now.
    // A relaunch of a child whose owner's profile was narrowed in between is
    // therefore narrowed to match, or refused; it is never wider than its owner.
    //
    // The agent's own `state_rw` host paths, which `apply_agent_requirements`
    // just folded into the writable set, are taken back out by the narrowing:
    // a bridge child's state is the private directory, not the family's.
    if let Some(bridge) = bridge {
        policy = policy
            .narrow(&bridge.overlay)
            .map_err(|violation| Refusal {
                reason: format!("overlay_violation: {violation}"),
                // An overlay that would widen is not a host that cannot apply a
                // profile: it is a request for a boundary friring will not build,
                // and running on the host instead would be wider still.
                integrity: true,
            })?;
    }

    let plan = host
        .inner_sandbox(backend, &policy, agent_sandbox)
        .ok_or_else(|| format!("Sandbox backend '{backend}' has no inner-sandbox verdict"))?;
    let transport = host
        .backend(backend)
        .ok_or_else(|| format!("Sandbox backend '{backend}' is not built into this friring"))?
        .capabilities()
        .proxy_transport;

    let session_key = session_key(config);
    let workspace = config
        .cwd
        .as_ref()
        .map(|p| representable("the session's working directory", p))
        .transpose()?;

    // A place exists before the launch does: the container has to be running
    // for the transport to reach its tmux, for the relay port to be one no
    // sibling session holds, and for the relay binary inside it to be resolved.
    // Ensuring is idempotent, so a relaunch adopts the same place.
    let place_backend = match shape {
        Some(crate::session::SandboxShape::Place) => Some(as_place(host, backend)?),
        _ => None,
    };
    let ensured = place_backend
        .map(|place| place.ensure_place(profile))
        .transpose()?;

    // A place runs the agent *inside* itself, so an image with no agent CLI is a
    // pane that dies the instant it opens — and the sign-in `volume-login` does
    // in that same pane goes with it. Checked per launch rather than folded into
    // `ensure_place`: a place is shared by every session of its profile, and
    // those sessions need not run the same agent. Before the scratch directory
    // is minted and the proxy is bound, so a refusal costs nothing.
    if let (Some(backend), Some(place)) = (place_backend, &ensured) {
        backend.ensure_agent_program(&policy, place, command)?;
    }

    // Never the host temp root: `/tmp` holds friring's own tmux socket, and a
    // read-write grant over it is a complete escape (ADR-29's sibling problem —
    // see `crate::sandbox::dirs`). friring mints a private per-session directory
    // instead, adopting one left behind by a crashed run. A place's lives under
    // that place's own tree, because the whole tree is what is mounted in.
    let tmp_dir = match &ensured {
        Some(_) => crate::sandbox::create_place_session_dir(&profile.name, &session_key),
        None => crate::sandbox::create_session_scratch(&session_key),
    }
    .map_err(Refusal::from)
    .and_then(|dir| representable("the sandbox scratch directory", &dir).map_err(Refusal::from))?;

    // The private directory is useful only when programs can discover it.
    // Override inherited host temp variables after applying the agent's static
    // environment so every tool lands in this launch's boundary-owned scratch.
    for key in ["TMPDIR", "TMP", "TEMP"] {
        policy.insert_env(key, tmp_dir.clone());
    }

    // The one channel out of a policy boundary (ADR-29): the agent's hooks
    // append a state word to a file here and the status poll takes it, because
    // the database `friring-cli session signal` writes is what a sandbox may
    // never reach. A place needs no such directory: its hooks are projected in
    // with every signal command rewritten to `tmux set-option -p`, which reaches
    // friring over the control-mode subscription it already holds — the same
    // channel an SSH host's hooks use.
    let signals = match &ensured {
        None => Some(
            crate::paths::create_session_signal_dir(&session_key)
                .map_err(|e| format!("Cannot apply a sandbox profile: {e}"))?,
        ),
        Some(_) => None,
    };
    let signal_dir = signals
        .as_ref()
        .map(|channel| representable("the sandbox signal directory", &channel.dir))
        .transpose()?;
    if let Some(channel) = &signals {
        let file = representable("the sandbox signal file", &channel.file)?;
        // Inserted on the *policy*, whose environment is the launch's last word,
        // so an agent that declares `FRIRING_SIGNAL_FILE` in the registry cannot
        // point the channel somewhere friring does not read.
        policy.insert_env(crate::paths::SIGNAL_FILE_ENV, file);
    }

    // The bridge channel. A **leader** is an ordinary session — the operator
    // creates it from the TUI, not the child saga — so minting only on the child
    // path would leave every leader holding capabilities it has no channel to
    // use. The rule is therefore the profile's, not the launch's: any
    // policy-sandboxed session whose profile grants a capability gets a queue,
    // which is the contract `docs/CONFIG.md` states.
    //
    // A child already carries its own directory on the `BridgeLaunch` (minted at
    // S3 with the rest of its private dirs, before this launch is composed), so
    // that one wins and the child path is untouched. A place gets none: no place
    // backend advertises the capability, and a host path inside a container names
    // nothing.
    let bridge_dir = match bridge.and_then(|b| b.bridge_dir.clone()) {
        Some(dir) => Some(dir),
        None if ensured.is_none() && !profile.bridge_grants.is_empty() => {
            let dir = crate::paths::create_session_bridge_dirs(&session_key)
                .map_err(|e| format!("Cannot apply a sandbox profile: {e}"))?;
            Some(representable("the bridge directory", &dir)?)
        }
        None => None,
    };
    // On the **policy**, whose environment is the launch's last word, so an
    // agent that declares either variable in the registry cannot point the
    // channel or the state somewhere friring does not own — the same rule the
    // signal file follows.
    if let Some(dir) = &bridge_dir {
        policy.insert_env(crate::session::bridge::BRIDGE_DIR_ENV, dir.clone());
    }
    // The private state directory that makes a child a child stays child-only:
    // a leader runs from the family's own state, which is what it is for.
    if let Some(bridge) = bridge {
        if let (Some(state), Some(variable)) = (
            &bridge.state_dir,
            agent_sandbox.and_then(|s| s.config_dir_env.as_deref()),
        ) {
            policy.insert_env(variable, state.clone());
        }
    }

    let database = crate::paths::database_file()
        .map(|p| representable("friring's database", &p))
        .transpose()?;
    // The credential family, not the registry name: a rebranded claude shares
    // claude's credential file, and denying it would log the agent out.
    let family = def.map(|d| d.hook_schema.as_deref().unwrap_or(&d.name));

    // The egress proxy, before anything is launched: a mode the kernel cannot
    // express on its own (an allowlist, or denies under `full`) is enforced
    // there and nowhere else, so the boundary is not composable until it is
    // listening. A failure here refuses the launch — `apply` turns that into
    // the profile's own `allow_unsandboxed_fallback` decision — rather than
    // starting an agent that believes it is filtered and is not.
    //
    // `pending` is what keeps the instance from being the session's before the
    // launch is: holding it here means every `?` below releases it, and the
    // session's current boundary — a healthy agent's way out — is untouched
    // until the launch path commits.
    let (proxy, relay_port, pending) = if crate::sandbox::egress::proxy_required(&policy) {
        // The key is the boundary's identity: the token, the socket and the
        // first-use answers are all per session, and two launches sharing one
        // key share all three — starting the second would replace the first's
        // instance mid-run, and one session's "allow this domain?" would widen
        // the other's. A launch with no id of its own has no boundary to be
        // told apart by, so it is refused rather than filed under the fallback.
        // Which is attribution and lifetime, not isolation between the sessions
        // of a *place*: they share a uid and a pid namespace, so the place is
        // the trust domain (`crate::sandbox::egress`).
        if session_key == UNIDENTIFIED_SESSION {
            return Err(
                "This launch has no session id, so its egress boundary could not be told \
                 apart from another's"
                    .to_string()
                    .into(),
            );
        }
        // A namespaced policy sandbox gets a private loopback, so its relay can
        // use one fixed port. A place is shared by its profile's sessions and so
        // is its loopback, so each takes a port of its own and keeps it across
        // relaunches — and the address the proxy environment names has to be
        // that one, or every session after the first would fail closed.
        let relay = match (&ensured, place_backend) {
            (Some(place), Some(backend)) => backend
                .relay_port(&place.instance.external_id, &session_key)
                .map(Some)
                .map_err(|e| e.to_string())?,
            _ => None,
        };
        let prepared = crate::sandbox::egress::prepare_at(
            &session_key,
            &policy,
            transport,
            std::path::Path::new(&tmp_dir),
            relay.map_or_else(crate::sandbox::egress::relay_addr, |port| {
                (std::net::Ipv4Addr::LOCALHOST, port).into()
            }),
        )?;
        for (key, value) in prepared.grant.env {
            policy.insert_env(key, value);
        }
        (
            Some((prepared.grant.endpoint, prepared.grant.token)),
            relay,
            prepared.pending,
        )
    } else {
        // A profile edited from `allowlist` to `full` or `none` must not leave
        // the previous launch's listener behind — once this launch is the one
        // running, and not before.
        (None, None, PendingEgress::clearing(&session_key))
    };

    // Credentials, once the boundary they have to survive exists: a policy
    // backend returns before any lookup — the host's own store, keychain
    // included, is already reachable subject to the path policy — and a place
    // gets whichever strategy resolves, a token from friring's own keychain
    // entry or the profile's persistent login (ADR-28). A credential problem
    // never fails a launch; it becomes a login the user is told how to do, and
    // refusing would answer "your token is missing" by running the agent
    // *outside* the boundary through `allow_unsandboxed_fallback`.
    //
    // Its non-secret half goes on the **policy**, whose environment is the
    // launch's last word, so a hand-edited registry cannot point an agent's
    // state directory somewhere friring did not create. The secret half never
    // touches the policy, argv or a log: it leaves on `secret_env` alone.
    let credentials = crate::sandbox::auth::prepare(&crate::sandbox::CredentialInput {
        profile: &profile.name,
        boundary: match &ensured {
            Some(ensured) => crate::sandbox::Boundary::Place {
                home_dir: &ensured.home_dir,
                inside_home: crate::sandbox::container::CONTAINER_HOME,
            },
            None => crate::sandbox::Boundary::Policy,
        },
        family,
        declaration: agent_sandbox,
        host_home: home,
        store: crate::sandbox::auth::keychain::system_store(),
    })
    .map_err(|e| e.to_string())?;
    for (key, value) in &credentials.env {
        policy.insert_env(key.clone(), value.clone());
    }

    // The host's own multiplexer sockets, so the generated policy denies them
    // (ADR-33). Every policy launch gets them: friring's tmux server runs
    // commands in host panes, and a sandbox that can dial its socket is outside
    // the boundary whatever the profile says.
    //
    // The launch helper is resolved here, once, for the same reason the relay is
    // a launch input rather than a backend lookup: it is *this* friring's CLI,
    // and a backend that resolved it itself could not be composed against a host
    // that has no such binary beside it. A policy backend refuses the launch
    // when it is missing.
    let helper = local_helper_program();
    let mut launch = SandboxLaunch::new(&policy, home, &session_key)
        .with_tmp_dir(&tmp_dir)
        .with_agent_program(command)
        .with_host_mux(crate::agent::tmux::host_mux_sockets());
    if let Some(helper) = helper.as_deref() {
        launch = launch.with_helper_program(helper);
    }
    if let Some(bridge) = bridge {
        launch = launch.with_narrowing(&bridge.overlay);
        if let Some((dir, key)) = &bridge.gate {
            launch = launch.with_gate(dir, key);
        }
    }
    // Recorded before the launch is composed against it, so the row a restart
    // rebinds from names the very endpoint the agent's own environment names.
    // `Preparing` until the supervisor acknowledges the commit: a listener that
    // is bound and not yet anybody's is not a boundary anything is using.
    let egress = crate::session::EgressRecord {
        endpoint: proxy.as_ref().map(|(endpoint, _)| {
            crate::sandbox::egress::PersistedEndpoint::of(endpoint).to_string()
        }),
        token: proxy.as_ref().map(|(_, token)| token.clone()),
        state: match &proxy {
            Some(_) => crate::session::EgressState::Preparing,
            None => crate::session::EgressState::None,
        },
    };
    if let Some((endpoint, _)) = proxy {
        launch = launch.with_proxy(endpoint);
    }
    if let Some(dir) = signal_dir.as_deref() {
        launch = launch.with_signal_dir(dir);
    }
    if let Some(dir) = bridge_dir.as_deref() {
        launch = launch.with_bridge_dir(dir);
    }
    if let Some(place) = &ensured {
        // A relay exactly when the launch is proxied. `ensure_place` refuses a
        // filtered profile whose image carries no `friring-cli`, so the two are
        // already consistent — refusing here rather than composing an empty
        // program keeps that an invariant of this function too.
        let relay = match relay_port {
            Some(port) => Some(crate::sandbox::PlaceRelay {
                program: place.relay_program.as_deref().ok_or_else(|| {
                    format!(
                        "Sandbox profile '{}' filters egress, and the place friring ensured for \
                         it reported no relay binary inside it",
                        profile.name
                    )
                })?,
                port,
            }),
            None => None,
        };
        launch = launch.with_place(crate::sandbox::PlaceLaunch { relay });
    }
    if let Some(workspace) = workspace.as_deref() {
        launch = launch.with_workspace(workspace);
    }
    if let Some(database) = database.as_deref() {
        launch = launch.with_friring_db(database);
    }
    if let Some(family) = family {
        launch = launch.with_agent(family);
    }

    // The agent's bypass flags belong to the *agent*, so they go on the agent's
    // own argv — inside the wrapper, after everything the backend contributes.
    let mut argv = Vec::with_capacity(1 + args.len() + plan.extra_args.len());
    argv.push(command.to_string());
    argv.extend(args.iter().cloned());
    argv.extend(plan.extra_args.iter().cloned());

    // A place has none of the host's filesystem it did not ask for, so the
    // user's agent configuration has to be carried in — including friring's own
    // hook payload, which an argument names by a host path (claude's
    // `--settings <config dir>/hooks/claude.json`) that points at nothing in
    // there. An agent handed a settings path that does not exist dies on
    // startup, so the argument follows the file: it is repointed where the
    // projection landed it, and dropped only where nothing crossed.
    let projected = match &ensured {
        Some(ensured) => {
            // No config directory means nothing can be *recognised* as
            // friring's, so nothing is rewritten or dropped. An empty root
            // would match every absolute path instead, which is the opposite
            // of the narrow scope this rewrite is allowed.
            let config_root = crate::agent::config_args::managed_root();
            let managed = config_root
                .as_deref()
                .map(|root| crate::agent::config_args::collect_config_paths(&argv, root))
                .unwrap_or_default();
            // The paths the place actually **mounts**: `profile.resolve`, not
            // `policy`, which has had the agent's `state_rw` host paths folded
            // into it and a place mounts none of those. Getting this wrong would
            // tell the user a host reference resolves inside when it does not.
            let mounted = profile.resolve(backend, home).map_err(|e| e.to_string())?;
            let granted: Vec<String> = mounted
                .rw_paths
                .iter()
                .chain(mounted.ro_paths.iter())
                .cloned()
                .collect();
            let projection = crate::sandbox::plan_projection(&crate::sandbox::ProjectionInput {
                profile: &profile.name,
                agent: agent_sandbox,
                granted: &granted,
                home,
                inside_home: crate::sandbox::container::CONTAINER_HOME,
                platform: crate::sandbox::SecretPlatform::of(host.platform()),
                friring_db: database.as_deref(),
                managed_root: config_root.as_deref().unwrap_or_default(),
                managed: &managed,
            });
            projection.apply(std::path::Path::new(&ensured.home_dir))?;
            if let Some(root) = config_root.as_deref() {
                argv = crate::agent::config_args::rewrite_config_path_args(argv, root, |p| {
                    projection.inside_path(p).map(str::to_string)
                });
            }
            Some(projection)
        }
        None => None,
    };

    // A wrap that fails takes `pending` down with the stack: nothing is going to
    // use the instance prepared for this launch, and the session's own — which
    // this composition has not touched — keeps serving whatever is running.
    let wrapped = host
        .wrap(backend, argv, &launch)
        .map_err(|e| e.to_string())?;
    let mut wrapped = wrapped.into_iter();
    let command = wrapped
        .next()
        .ok_or_else(|| format!("Sandbox backend '{backend}' produced an empty command line"))?;

    // The agent's declared environment first, the *policy's* last: an agent
    // that declares `HTTP_PROXY` in the registry must not shadow the boundary's
    // own, which would route it around the allowlist. `apply_agent_requirements`
    // has already folded the agent's declarations into the policy, so nothing
    // an agent asked for is lost by the precedence — only overruled where the
    // boundary has an answer of its own.
    let mut env: HashMap<String, String> = plan
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.extend(policy.env.iter().map(|(k, v)| (k.clone(), v.clone())));

    // The place's address for the transport, built from the engine path the
    // probe vetted rather than a bare name an inherited `PATH` could re-resolve.
    let place = match (place_backend, &ensured) {
        (Some(backend), Some(ensured)) => Some(
            crate::agent::transport::Place::new(
                backend.engine_program().map_err(|e| e.to_string())?,
                &ensured.instance.external_id,
                &profile.name,
            )
            .map_err(|e| format!("Cannot reach the sandbox place: {e:#}"))?,
        ),
        _ => None,
    };

    // Composed, so the instance survives this stack — but as the *launch's*,
    // not the session's. Whoever spawns the pane claims it with
    // `pending_egress` and commits it once there is something behind it.
    pending.park();

    let note = composition_note(&credentials, projected.as_ref());
    Ok(SandboxedInvocation {
        command,
        args: wrapped.collect(),
        env,
        secret_env: credentials
            .secret_env()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        state: format!("{backend} · inner agent sandbox: {}{note}", plan.state),
        label: format!("{}{note}", plan.label),
        place,
        instance: ensured.map(|ensured| ensured.instance),
        egress,
        // Only where there is something to do. An agent whose state friring
        // cannot inspect (`LoginState::Unknown`) is *not* a prompt: telling a
        // user to sign in every launch when they may already be signed in is
        // the indicator lying in the other direction.
        login: credentials
            .login
            .needs_login()
            .then(|| credentials.login.how().unwrap_or_default().to_string()),
    })
}

/// What a launch has to say for itself beyond the boundary it applied: how the
/// agent authenticates in there, and — for a place — what became of the user's
/// configuration.
///
/// Both are recoverable and neither is self-explanatory, so the composition
/// carries them wherever it is shown rather than leaving the user with an agent
/// that mysteriously has no skills, or a session that silently never leaves
/// `idle`. Never carries a credential: [`CredentialPlan::note`] is names, paths
/// and reasons only.
///
/// [`CredentialPlan::note`]: crate::sandbox::CredentialPlan::note
fn composition_note(
    credentials: &crate::sandbox::CredentialPlan,
    projected: Option<&crate::sandbox::ProjectionPlan>,
) -> String {
    let mut note = format!(" · {}", credentials.note);
    if let Some(projection) = projected {
        note.push_str(" · ");
        note.push_str(&projection.summary());
    }
    note
}

/// Drop the per-session state a sandboxed launch minted: its egress proxy, the
/// scratch directory the agent wrote, the copy-on-write layers it wrote through,
/// and the policy file generated for it.
///
/// Call this when a session ends or is deleted. Skipping the directories costs
/// disk rather than correctness — the next launch of the same session adopts
/// what is there, which is what makes a crashed run recoverable — but an
/// agent's writable scratch should not outlive the agent, and its way out
/// certainly should not: the proxy is stopped **first**, so the socket is
/// unlinked by the process that bound it rather than pulled out from under a
/// live listener. Harmless for a session that never had a profile.
pub fn cleanup(config: &SessionConfig) {
    cleanup_key(&session_key(config));
}

/// The same cleanup for a teardown path that holds a persisted session row
/// rather than the [`SessionConfig`] the launch was composed from.
///
/// A real spawn always pins `SessionConfig::session_id` (it is also
/// `FRIRING_SESSION`), so the friring session id *is* the launch key the
/// wrapped invocation was keyed on — and a session that never had one minted
/// nothing to drop. Exists so `session_ops`, which may not reference
/// [`crate::sandbox`], still reaches the key derivation that lives here.
pub fn cleanup_by_session_id(session_id: crate::session::SessionId) {
    cleanup_key(&session_id.to_string());
}

/// Everything one launch key owns, in the order that keeps a live listener from
/// being pulled out from under itself.
fn cleanup_key(key: &str) {
    crate::sandbox::egress::stop(key);
    crate::sandbox::cleanup_session(key);
    // A place-backed session's writable directory is inside the place's tree
    // rather than the policy scratch root, because the whole tree is what is
    // mounted in — so both are dropped, and each is a no-op for the shape that
    // did not use it.
    crate::sandbox::dirs::cleanup_place_session(key);
    // The copy-on-write layers are a third per-session tree, and the only one
    // that holds what the agent *wrote*: a bubblewrap launch with a
    // copy-on-write workspace lands every write in an upper layer under
    // `<data>/sandbox/overlay/<key>`, deliberately outside the two roots above
    // (a directory the sandbox can reach is an `upperdir` it can redirect). A
    // layer left behind here would outlive the session that made it — see
    // `crate::sandbox::bwrap::cleanup_overlays`.
    crate::sandbox::bwrap::cleanup_overlays(key);
    crate::paths::remove_session_signal_dir(key);
    // A place outlives its sessions, so nothing here removes one — but the
    // loopback port this session held inside it is the place's to hand out
    // again, and a place with a bounded span of them would otherwise run out
    // after enough sessions had come and gone.
    with_host(|host| {
        for kind in crate::sandbox::PLACE_KINDS.iter().copied() {
            if let Some(place) = host.place(kind) {
                place.release_relay_ports(key);
            }
        }
    });
}

/// Ensure `profile`'s place exists and answer with the transport that reaches
/// it, plus the instance to record.
///
/// The restore path's half of the launch composition: a session persisted with
/// `backend_type = sandbox:<profile>` has to be reached before it can be
/// adopted, and the place it names may be stopped (a host reboot) or gone (an
/// engine that was pruned). Ensuring is idempotent, so a place that is already
/// running costs one `inspect`.
///
/// Blocking, and deliberately so: this runs a container engine, which on a cold
/// place is seconds. Callers keep it off the render path.
///
/// # Errors
///
/// The engine is unavailable, the profile cannot be resolved against it, or the
/// place will not start — each with the backend's own actionable sentence.
pub fn open_place(
    profile: &crate::session::SandboxProfile,
) -> Result<
    (
        std::sync::Arc<dyn crate::agent::SessionBackend>,
        crate::sandbox::SandboxInstance,
    ),
    String,
> {
    with_host(|host| {
        let backend = host
            .select(profile.backend)
            .backend()
            .map_err(|e| e.to_string())?;
        let container = as_place(host, backend)?;
        let ensured = container.ensure_place(profile).map_err(|e| e.to_string())?;
        let place = crate::agent::transport::Place::new(
            container.engine_program().map_err(|e| e.to_string())?,
            &ensured.instance.external_id,
            &profile.name,
        )
        .map_err(|e| format!("Cannot reach the sandbox place: {e:#}"))?;
        Ok((
            crate::agent::backend::place_backend(&place),
            ensured.instance,
        ))
    })
}

/// **Every** place a profile's sessions may be running in right now.
///
/// Deliberately does not create one: the callers are teardown and delivery
/// paths, and starting a container in order to kill a pane inside it — or in
/// order to discover there is none — is the opposite of what they are for. It
/// asks each engine friring can drive for the containers *it* created and keeps
/// the ones carrying this profile's label, so a profile whose `auto` backend has
/// changed since the launch is still found.
///
/// **A profile can have more than one live place, and answering with just one is
/// how a caller acts on a stranger.** An edited profile asks for a new container
/// while the sessions already launched keep running in the old one — that is
/// what `gc_plan`'s in-use rule exists to protect — and `sessions` records no
/// container, so nothing here can say which of them a given session is in. The
/// pane id it *does* record is per tmux server and therefore per container, so
/// the same `%1` names a different session's pane in each. Callers must address
/// a place-backed window by its `tb-<session>` **name**, which is unique to the
/// session across every container of the profile, and try every place rather
/// than guessing at one.
///
/// **A container answers to two names, and only one of them survives a
/// rename.** The label is written at create and an engine cannot change it,
/// while renaming a profile rewrites the profile row, its instance rows and
/// every session's `sandbox:<profile>` in one transaction. So `recorded` — the
/// `sandbox_instances` ids friring holds for this profile *now* — is asked
/// beside the label. Matched on the label alone, a renamed profile's places all
/// read as nothing running: teardown reports "no pane to kill" and leaves the
/// agent working inside, automation delivery silently never fires, and the
/// manager view's confirmation under-counts what it is about to stop. The ids
/// have to be passed in because `agent` may not reach `storage`.
///
/// Exists here because `session_ops` may not reference [`crate::sandbox`] at
/// all, and the transport address is assembled from two things only this layer
/// holds: the engine path the probe vetted, and the engine's own container id.
pub fn running_places(profile: &str, recorded: &[String]) -> Vec<crate::agent::transport::Place> {
    with_host(|host| {
        let mut found = Vec::new();
        for kind in crate::sandbox::PLACE_KINDS.iter().copied() {
            let Some(backend) = host.place(kind) else {
                continue;
            };
            let Ok(engine) = backend.engine_program() else {
                continue;
            };
            let Ok(places) = backend.live_places() else {
                continue;
            };
            found.extend(
                places
                    .into_iter()
                    .filter(|place| {
                        place.owned
                            && (place
                                .profile
                                .as_deref()
                                .is_some_and(|name| name.eq_ignore_ascii_case(profile))
                                || recorded.contains(&place.id))
                    })
                    .filter_map(|place| {
                        crate::agent::transport::Place::new(engine, &place.id, profile).ok()
                    }),
            );
        }
        found
    })
}

/// The backend behind `kind` as a place, or the reason friring cannot launch
/// into one.
///
/// One lookup for every path that opens a place, so the sentence a user gets is
/// the same wherever the launch was composed from. The interesting case is a
/// kind whose place friring can *build* but not yet *run a session in*: the
/// refusal names what is missing and what to pick instead, because "never a
/// silently dead pane" applies to the shapes friring has not finished as much as
/// to the ones a host is missing.
fn as_place(
    host: &SandboxHost,
    kind: crate::session::SandboxBackendKind,
) -> Result<&dyn crate::sandbox::PlaceBackend, String> {
    if let Some(place) = host.place(kind) {
        return Ok(place);
    }
    if kind == crate::session::SandboxBackendKind::WslDistro {
        return Err(
            "A WSL distro is a place friring can create but cannot yet run a session in: \
             reaching it needs the `wsl:` transport rather than the container one, and the \
             projected hooks and credentials a place is launched with are not wired for a \
             filesystem inside the utility VM. Run friring inside the distro and pick the \
             'bwrap' backend, or pick 'docker'/'podman'"
                .to_string(),
        );
    }
    Err(format!(
        "Sandbox backend '{kind}' is a place this friring cannot create"
    ))
}

/// The key a launch that pinned neither id falls back to.
///
/// A constant, so two such launches collide — which for the scratch directory
/// is a shared writable directory and no worse, and for the egress boundary
/// would be a shared credential and a shared socket. `build` refuses a proxied
/// launch under this key rather than filing two boundaries under one name.
const UNIDENTIFIED_SESSION: &str = "session";

/// Names the generated profile file and the scratch directory, so two sessions
/// of one profile never race on either. The friring session id when the caller
/// pinned one (it always does on a real spawn, because it is also
/// `FRIRING_SESSION`), otherwise the agent's own conversation id, and
/// [`UNIDENTIFIED_SESSION`] when there is neither.
fn session_key(config: &SessionConfig) -> String {
    config
        .session_id
        .map(|id| id.to_string())
        .or_else(|| config.agent_session_id.clone())
        .unwrap_or_else(|| UNIDENTIFIED_SESSION.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Bridge launch refusals (ADR-31) ──────────────────────────────────

    /// An agent that requires the bridge, for the refusal tests.
    fn bridge_agent() -> AgentDef {
        let mut def = agent_def();
        def.sandbox = Some(crate::session::AgentSandboxDef {
            bridge_requires: vec![
                crate::session::BridgeCapability::ChildLifecycle,
                crate::session::BridgeCapability::Mailbox,
            ],
            config_dir_env: Some("CODEX_HOME".to_string()),
            state_dir: Some("~/.codex".to_string()),
            ..def.sandbox.unwrap_or_default()
        });
        def
    }

    /// A profile granting everything the agent above requires.
    fn granting_profile() -> crate::session::SandboxProfile {
        let mut profile = crate::session::SandboxProfile::new(
            "orchestrator",
            vec![crate::session::SandboxPath::workspace("~/dev/app")],
        );
        profile.network_mode = crate::session::NetworkMode::None;
        profile.bridge_grants = vec![
            crate::session::BridgeCapability::ChildLifecycle,
            crate::session::BridgeCapability::Mailbox,
        ];
        profile
    }

    /// A **leader** is an ordinary session: the operator creates it from the
    /// TUI, and nothing about its launch goes through the child saga. So the
    /// queue has to be minted by the *profile's* grant rather than by the child
    /// path, or every leader holds capabilities with no channel to use them
    /// through — which is what `docs/CONFIG.md` promises and what both shipped
    /// extensions' leaders die on the first line without.
    #[test]
    fn a_session_whose_profile_grants_the_bridge_is_minted_a_queue() {
        let _guard = fabricated_data_dir("leader-queue");
        let _host = TestSandboxHost::new(stub_host());

        let mut config = config_with(Some(granting_profile()));
        config.agent_session_id = Some("leader-queue".into());
        let invocation = build(
            &stub_host(),
            "/fabricated/home",
            Some(&agent_def()),
            &config,
            "claude",
            &[],
            // No `BridgeLaunch`: this is a leader, not a child.
            None,
        )
        .expect("a granting profile composes");

        let dir = invocation
            .env
            .get(crate::session::bridge::BRIDGE_DIR_ENV)
            .cloned()
            .expect("a granting profile mints a bridge queue");
        assert_eq!(
            dir,
            crate::paths::session_bridge_dir("leader-queue")
                .unwrap()
                .display()
                .to_string()
        );
        // The channel is the agent writing a request and reading an answer, so
        // the directory has to be **writable** inside the boundary. Read off the
        // generated argv, which is the boundary the kernel is handed: the stub
        // host is bwrap, which spells a writable root `--bind src dst`. Merely
        // finding the path in the argv would accept a read-only mount — a leader
        // that can read answers and never submit a request.
        assert!(
            invocation
                .args
                .windows(3)
                .any(|w| w[0] == "--bind" && w[1] == dir && w[2] == dir),
            "the bridge queue is not bound read-write into the boundary: {:?}",
            invocation.args
        );
        assert!(
            !invocation
                .args
                .windows(3)
                .any(|w| (w[0] == "--ro-bind" || w[0] == "--ro-bind-try") && w[2] == dir),
            "the bridge queue is mounted read-only: {:?}",
            invocation.args
        );
        cleanup(&config);

        // And a profile that grants nothing gets none: the variable's presence
        // is what tells `friring-cli bridge` it has authority to spend.
        let mut plain = config_with(Some(closed_profile()));
        plain.agent_session_id = Some("leader-none".into());
        let invocation = build(
            &stub_host(),
            "/fabricated/home",
            Some(&agent_def()),
            &plain,
            "claude",
            &[],
            None,
        )
        .expect("an ordinary profile composes");
        assert!(
            !invocation
                .env
                .contains_key(crate::session::bridge::BRIDGE_DIR_ENV),
            "a profile granting nothing was still given a queue"
        );
        cleanup(&plain);
    }

    /// A refusal is read by a person, so it must not carry the indentation of
    /// the source it was wrapped in.
    ///
    /// A multi-line string literal without a `\` continuation keeps every
    /// continuation line's leading spaces, and rustfmt does not touch string
    /// contents — so the mistake is invisible in the source and obvious on a
    /// terminal. This asserts it over every refusal this function can produce.
    #[test]
    fn no_refusal_carries_the_indentation_it_was_wrapped_in() {
        let def = bridge_agent();
        let caps = crate::sandbox::backend::Caps {
            shape: crate::session::SandboxShape::Place,
            limits: true,
            network_modes: crate::session::NetworkMode::ALL,
            read_scopes: crate::session::ReadScope::ALL,
            persistent: true,
            host_credentials: false,
            inner_agent_sandbox: crate::sandbox::backend::InnerSandboxVerdict::Redundant,
            proxy_transport: crate::sandbox::backend::ProxyTransport::UnixSocket,
            bridge: false,
        };
        let mut short = config_with(Some(granting_profile()));
        short.sandbox.as_mut().unwrap().bridge_grants = Vec::new();
        let mut remote = config_with(Some(granting_profile()));
        remote.backend = Some("ssh:devbox".to_string());

        let refusals = [
            bridge_refusal(Some(&def), &config_with(None), None, true),
            bridge_refusal(Some(&def), &short, None, true),
            bridge_refusal(
                Some(&def),
                &config_with(Some(granting_profile())),
                Some(&caps),
                true,
            ),
            bridge_refusal(Some(&def), &remote, None, true),
            bridge_refusal(
                Some(&def),
                &config_with(Some(granting_profile())),
                None,
                false,
            ),
        ];
        for refusal in refusals {
            let reason = refusal.expect("each of these is a refusal").reason;
            assert!(
                !reason.contains("  "),
                "a refusal carries wrapped-source indentation: {reason:?}"
            );
        }
    }

    /// Every way a bridge-required agent must **not** start, each an integrity
    /// refusal a profile's `allow_unsandboxed_fallback` may not answer.
    ///
    /// The common failure this closes is subtle: a bridge-required agent that
    /// fell back to the host would have no boundary *and* a channel nobody is
    /// serving — strictly worse than not starting, and much harder to diagnose.
    #[test]
    fn a_bridge_required_agent_is_refused_wherever_it_cannot_be_served() {
        let def = bridge_agent();
        let with_bridge = crate::sandbox::Caps {
            bridge: true,
            ..crate::sandbox::backend::Caps {
                shape: crate::session::SandboxShape::Policy,
                limits: false,
                network_modes: crate::session::NetworkMode::ALL,
                read_scopes: crate::session::ReadScope::ALL,
                persistent: false,
                host_credentials: true,
                inner_agent_sandbox: crate::sandbox::backend::InnerSandboxVerdict::Redundant,
                proxy_transport: crate::sandbox::backend::ProxyTransport::Loopback,
                bridge: true,
            }
        };
        let without_bridge = crate::sandbox::Caps {
            bridge: false,
            ..with_bridge.clone()
        };

        // No profile at all: the bridge is granted by a profile, so an agent
        // that needs one is never started as a plain session.
        let plain = config_with(None);
        let refusal = bridge_refusal(Some(&def), &plain, Some(&with_bridge), true)
            .expect("a bridge agent with no profile is refused");
        assert!(refusal.integrity, "the fallback must not answer this");
        assert!(refusal.reason.contains("has none"), "{}", refusal.reason);

        // A profile that grants less than the agent needs.
        let mut profile = granting_profile();
        profile.bridge_grants = vec![crate::session::BridgeCapability::Mailbox];
        let short = config_with(Some(profile));
        let refusal = bridge_refusal(Some(&def), &short, Some(&with_bridge), true)
            .expect("a missing grant is refused");
        assert!(
            refusal.reason.contains("grant_missing"),
            "{}",
            refusal.reason
        );
        assert!(
            refusal.reason.contains("child-lifecycle"),
            "{}",
            refusal.reason
        );

        // A backend that cannot carry the channel.
        let placed = config_with(Some(granting_profile()));
        let refusal = bridge_refusal(Some(&def), &placed, Some(&without_bridge), true)
            .expect("a place cannot carry the bridge");
        assert!(
            refusal.reason.contains("cannot carry"),
            "{}",
            refusal.reason
        );

        // A remote session: friring mints the bridge directory on the machine it
        // runs on.
        let mut remote = config_with(Some(granting_profile()));
        remote.backend = Some("ssh:devbox".to_string());
        let refusal = bridge_refusal(Some(&def), &remote, Some(&with_bridge), true)
            .expect("a remote session cannot carry the bridge");
        assert!(refusal.reason.contains("remote"), "{}", refusal.reason);

        // The headless path: nothing would own the proxy or answer the requests.
        let headless = config_with(Some(granting_profile()));
        let refusal = bridge_refusal(Some(&def), &headless, Some(&with_bridge), false)
            .expect("a headless create is refused");
        assert!(
            refusal.reason.contains("bridge_requires_tui"),
            "{}",
            refusal.reason
        );

        // …and the one shape that is allowed: a granting profile, a capable
        // backend, from a running TUI.
        let ok = config_with(Some(granting_profile()));
        assert!(bridge_refusal(Some(&def), &ok, Some(&with_bridge), true).is_none());
    }

    /// An agent that requires nothing is unaffected by any of it — which is
    /// every agent in the registry today.
    #[test]
    fn an_agent_that_requires_nothing_is_never_refused_for_the_bridge() {
        let def = agent_def();
        let config = config_with(None);
        assert!(bridge_refusal(Some(&def), &config, None, false).is_none());
        assert!(bridge_refusal(None, &config, None, false).is_none());
    }

    use crate::session::{AgentSandboxDef, SandboxBackendKind, SandboxPath, SandboxProfile};

    fn agent_def() -> AgentDef {
        AgentDef {
            name: "claude".into(),
            command: "claude".into(),
            args: vec![],
            resume_args: vec![],
            fork_args: vec![],
            new_session_args: vec![],
            resume_latest: false,
            hook_schema: None,
            transcript: None,
            sandbox: Some(AgentSandboxDef {
                bypass: vec!["--dangerously-skip-permissions".into()],
                env: [("DISABLE_AUTOUPDATER".to_string(), "1".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            }),
        }
    }

    fn config_with(profile: Option<SandboxProfile>) -> SessionConfig {
        SessionConfig {
            agent: "claude".into(),
            sandbox: profile,
            ..Default::default()
        }
    }

    #[test]
    fn a_session_without_a_profile_is_left_alone() {
        let config = config_with(None);
        let decision = apply(Some(&agent_def()), &config, "claude", &["--foo".into()]).unwrap();
        assert_eq!(decision, SandboxDecision::Unsandboxed);
    }

    #[test]
    fn an_unavailable_backend_fails_the_launch_and_names_every_rung() {
        // `wsl-distro` is the one backend that is unavailable off Windows
        // whatever else the host has, so this exercises the platform probe
        // wherever the suite runs. The refusal a Windows host gets instead is
        // `a_windows_host_is_told_a_wsl_place_cannot_be_launched_into` below.
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.backend = SandboxBackendKind::WslDistro;
        let config = config_with(Some(profile));
        let err = apply(Some(&agent_def()), &config, "claude", &[]).unwrap_err();
        assert!(err.contains("wsl-distro"), "{err}");
    }

    #[test]
    fn the_escape_hatch_turns_that_failure_into_a_reported_skip() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.backend = SandboxBackendKind::WslDistro;
        profile.allow_unsandboxed_fallback = true;
        let config = config_with(Some(profile));
        let decision = apply(Some(&agent_def()), &config, "claude", &[]).unwrap();
        let SandboxDecision::Skipped { reason } = decision else {
            panic!("expected a skip, got {decision:?}");
        };
        assert!(reason.contains("wsl-distro"), "{reason}");
    }

    /// The refusal a *native Windows* host gets, all the way through the launch
    /// path, whatever its profile asks for — and it names WSL2, because that is
    /// where a Windows user's boundary actually comes from.
    ///
    /// The host here has WSL installed and a distro to clone, which is what
    /// makes the assertion mean something: this is not "nothing is installed",
    /// it is friring declining to call anything here a boundary. A place mounts
    /// every path at exactly its host path and no container engine can do that
    /// with `C:\…`, so the ladder offers nothing and a pin is not even probed
    /// ([`crate::sandbox::select::NATIVE_WINDOWS`]).
    ///
    /// Driven through [`apply`] rather than through the ladder alone, because
    /// what a user meets is the launch: the sentence has to survive the whole
    /// path, and the escape hatch has to answer it the same way.
    #[test]
    fn a_windows_host_is_refused_and_pointed_at_wsl2() {
        // A Windows host with a Store WSL and one WSL2 distro to clone: enough
        // for the backend to probe as *available*, which is what puts the
        // launch on the rung this refusal belongs to.
        const WSL_EXE: &str = "C:/Windows/System32/wsl.exe";
        let utf16 = |text: &str| {
            let mut bytes: Vec<u8> = vec![0xff, 0xfe];
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            crate::sandbox::probe::ProbeOutput::success(
                String::from_utf8_lossy(&bytes).into_owned(),
            )
        };
        let windows = crate::sandbox::probe::StubHost::new()
            .with_home("C:/Users/me")
            // No `uname`; `cmd.exe` is what settles the platform.
            .with_binary("cmd.exe")
            .with_binary_at("wsl.exe", WSL_EXE)
            .with_command(
                &format!("{WSL_EXE} --version"),
                utf16("WSL version: 2.3.26.0\nKernel version: 5.15.167.4-1\n"),
            )
            .with_command(
                &format!("{WSL_EXE} --list --verbose"),
                utf16(
                    "  NAME             STATE           VERSION\n* Ubuntu-24.04     Running    \
                       2\n",
                ),
            );
        let _host = TestSandboxHost::new(SandboxHost::new(std::sync::Arc::new(windows)));

        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.read_scope = crate::session::ReadScope::Workspace;

        // `auto` finds no rung, and every pin is refused without being probed —
        // including the one backend that would have answered "available" here.
        for backend in [
            SandboxBackendKind::Auto,
            SandboxBackendKind::WslDistro,
            SandboxBackendKind::Docker,
        ] {
            profile.backend = backend;
            let err = apply(
                Some(&agent_def()),
                &config_with(Some(profile.clone())),
                "claude",
                &[],
            )
            .expect_err("a native Windows host has no boundary to apply");
            assert!(err.contains("WSL2"), "{backend}: {err}");
            assert!(err.contains("exactly its host path"), "{backend}: {err}");
        }

        // A boundary this host cannot give is not the boundary's state being
        // wrong, so the escape hatch does answer it — with the same sentence,
        // and still without registering anything: every `wsl.exe` beyond the
        // probe is unscripted, so an ensure would have failed the stub instead.
        profile.backend = SandboxBackendKind::WslDistro;
        profile.allow_unsandboxed_fallback = true;
        let decision = apply(
            Some(&agent_def()),
            &config_with(Some(profile)),
            "claude",
            &[],
        )
        .unwrap();
        let SandboxDecision::Skipped { reason } = decision else {
            panic!("expected a skip, got {decision:?}");
        };
        assert!(reason.contains("WSL2"), "{reason}");
    }

    /// The escape hatch answers "this host cannot apply this profile". It must
    /// not answer "the boundary's own state is not what friring left it as" —
    /// otherwise an agent's way out of its sandbox is to break it.
    #[test]
    #[cfg(unix)]
    fn a_sandbox_that_breaks_its_own_scratch_directory_may_not_fall_back_onto_the_host() {
        let _paths = fabricated_data_dir("tampered-scratch");
        let _host = TestSandboxHost::new(stub_host());
        let scratch = crate::sandbox::dirs::session_scratch_dir("session").unwrap();
        std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&scratch);
        // What an agent inside a place does to the per-session directory under
        // the tree its own container mounts read-write.
        std::os::unix::fs::symlink("/tmp", &scratch).unwrap();

        let mut profile = closed_profile();
        profile.allow_unsandboxed_fallback = true;
        let config = config_with(Some(profile));
        let err = apply(Some(&agent_def()), &config, "claude", &[])
            .expect_err("a broken boundary must fail the launch, not skip the sandbox");
        assert!(err.contains("is a symlink"), "{err}");
        assert!(
            err.contains("not what friring left it as"),
            "the refusal says why the fallback did not apply: {err}"
        );

        // The same switch still answers an ordinary "not on this host".
        let _ = std::fs::remove_file(&scratch);
        let mut absent = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        absent.backend = SandboxBackendKind::WslDistro;
        absent.allow_unsandboxed_fallback = true;
        assert!(matches!(
            apply(
                Some(&agent_def()),
                &config_with(Some(absent)),
                "claude",
                &[]
            )
            .unwrap(),
            SandboxDecision::Skipped { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_remote_session_is_refused_rather_than_wrapped_locally() {
        let profile = closed_profile();
        let mut config = config_with(Some(profile));
        config.backend = Some("ssh:devbox".into());
        let err = apply(Some(&agent_def()), &config, "claude", &[]).unwrap_err();
        assert!(err.contains("remote host"), "{err}");
    }

    /// A host with bwrap and nothing else, so the whole composition runs on a
    /// machine that has no sandbox installed. Nothing here touches a real home:
    /// the profile's paths and the secrets deny list are both expanded against
    /// the fabricated `home` passed in.
    fn stub_host() -> SandboxHost {
        SandboxHost::new(std::sync::Arc::new(
            crate::sandbox::probe::StubHost::linux_with_bwrap("0.11.0"),
        ))
    }

    /// Pin friring's data directory somewhere short, private and fabricated,
    /// for the tests that compose a real launch.
    ///
    /// A bubblewrap launch binds `<data>/sandbox/tmp/<key>/proxy.sock`, and a
    /// unix socket path has to fit in `sun_path` (103 bytes) — which the
    /// default unit-test base, several directories under the platform temp
    /// directory, does not leave room for on macOS. Held for the test's
    /// lifetime: the override is thread-local and resets on drop.
    fn fabricated_data_dir(name: &str) -> crate::paths::TestPathGuard {
        let base = std::env::temp_dir().join(format!("frs{}-{name}", std::process::id()));
        crate::paths::TestPathGuard::new(base)
    }

    /// A profile with no egress at all.
    ///
    /// The tests about argv and environment *shape* use this so they compose
    /// one thing: the default `allowlist` starts a proxy and — under bwrap —
    /// a relay, which the egress tests below exercise deliberately.
    fn closed_profile() -> SandboxProfile {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_mode = crate::session::NetworkMode::None;
        profile
    }

    /// Where the stub host's bubblewrap lives. A launch runs the path the probe
    /// resolved, never the bare [`crate::sandbox::bwrap::BWRAP`] name.
    const STUB_BWRAP: &str = "/usr/bin/bwrap";

    /// Every bubblewrap flag whose next argument is a **host** path being handed
    /// across the boundary. `--tmpfs`, `--proc` and `--dev` are deliberately not
    /// here: their argument is a path *inside*, and what lands there is a fresh
    /// filesystem rather than anything of the host's.
    const BIND_FLAGS: [&str; 6] = [
        "--bind",
        "--bind-try",
        "--ro-bind",
        "--ro-bind-try",
        "--dev-bind",
        "--dev-bind-try",
    ];

    #[test]
    fn the_wrapper_surrounds_the_agent_and_appends_its_bypass_flags() {
        let profile = closed_profile();
        let mut config = config_with(Some(profile));
        config.cwd = Some("/fabricated/home/dev/app".into());
        let mut def = agent_def();
        def.sandbox
            .as_mut()
            .unwrap()
            .env
            .insert("TMPDIR".into(), "/host/temp".into());

        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            Some(&def),
            &config,
            "claude",
            &["--resume".into(), "abc".into()],
            None,
        )
        .unwrap();

        assert_eq!(wrapped.command, STUB_BWRAP);
        // The agent's own command line is last, after the *helper's* `--`, with
        // the bypass flags appended to *its* arguments rather than the
        // wrapper's. friring's own launch helper sits between the two, which is
        // what drops the host multiplexer's environment (ADR-33).
        let tail: Vec<&str> = wrapped
            .args
            .iter()
            .skip_while(|a| *a != "--")
            .map(String::as_str)
            .collect();
        assert_eq!(&tail[..4], ["--", TEST_HELPER_PROGRAM, "sandbox", "launch"]);
        let handover = tail.iter().rposition(|a| *a == "--").expect("a handover");
        assert_eq!(
            &tail[handover..],
            [
                "--",
                "claude",
                "--resume",
                "abc",
                "--dangerously-skip-permissions"
            ]
        );
        assert!(wrapped
            .state
            .starts_with("bwrap · inner agent sandbox: off"));
    }

    /// `docs/SANDBOX.md` §Launch integration: the session identity variables
    /// are set on the tmux window, *outside* a policy boundary, and inherited
    /// through it. Wrapping must therefore only ever **add** environment —
    /// dropping `FRIRING_SESSION` kills status reporting silently.
    #[test]
    fn wrapping_adds_environment_and_never_replaces_the_sessions_own() {
        let profile = closed_profile();
        let mut config = config_with(Some(profile));
        config
            .env
            .insert("FRIRING_SESSION".into(), "session-uuid".into());
        config
            .env
            .insert("FRIRING_SESSION_ID".into(), "agent-uuid".into());
        let def = agent_def();

        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            Some(&def),
            &config,
            "claude",
            &[],
            None,
        )
        .unwrap();

        // The wrap contributes the agent's declared environment and nothing
        // that would shadow an identity variable.
        assert_eq!(
            wrapped.env.get("DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        );
        assert!(!wrapped.env.contains_key("FRIRING_SESSION"));
        // Neither backend applies environment in argv (a policy is a rule on a
        // process, so the wrapped agent inherits the window) — so the identity
        // variables must not appear there either, or they would be duplicated
        // into a place the sandbox layer cannot keep in sync.
        assert!(!wrapped.args.iter().any(|a| a.contains("FRIRING_SESSION")));

        // What the caller does with the two halves.
        let mut env = config.env.clone();
        env.extend(wrapped.env);
        assert_eq!(
            env.get("FRIRING_SESSION").map(String::as_str),
            Some("session-uuid")
        );
        assert_eq!(
            env.get("FRIRING_SESSION_ID").map(String::as_str),
            Some("agent-uuid")
        );
        assert_eq!(
            env.get("DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        );
    }

    /// An agent with no `[agents.<name>.sandbox]` block still launches — it
    /// just gets no help, which is the agent-neutrality rule.
    #[test]
    fn an_agent_that_declares_nothing_is_still_wrapped() {
        let profile = closed_profile();
        let config = config_with(Some(profile));
        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &config,
            "aider",
            &[],
            None,
        )
        .unwrap();
        assert_eq!(wrapped.command, STUB_BWRAP);
        assert_eq!(wrapped.args.last().map(String::as_str), Some("aider"));
        assert!(wrapped.state.contains("declares no bypass flags"));
    }

    /// A lossy conversion would turn a non-UTF-8 byte into `U+FFFD`, and every
    /// rule built from the result would name a path that does not exist — an
    /// ADR-29 deny that denies nothing while the real database stays visible.
    /// Refusing is the only honest answer.
    #[cfg(unix)]
    #[test]
    fn a_path_that_cannot_be_spelled_exactly_refuses_the_launch() {
        use std::os::unix::ffi::OsStringExt as _;

        let profile = closed_profile();
        let mut config = config_with(Some(profile));
        config.cwd = Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(
            vec![b'/', b'w', 0xff, b'k'],
        )));
        let err = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
            None,
        )
        .unwrap_err();
        assert!(err.reason.contains("not valid UTF-8"), "{err}");
        assert!(err.reason.contains("working directory"), "{err}");
    }

    /// The scratch directory is friring's own, per session, and adopted rather
    /// than re-created when a crashed run left one behind. It is emphatically
    /// not the host temp root, which holds friring's tmux socket.
    #[test]
    fn the_launch_mints_its_own_scratch_directory() {
        let profile = closed_profile();
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some("scratch-mint-test".into());
        let def = agent_def();
        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            Some(&def),
            &config,
            "claude",
            &[],
            None,
        )
        .unwrap();

        let scratch = crate::sandbox::dirs::session_scratch_dir("scratch-mint-test").unwrap();
        assert!(
            scratch.is_dir(),
            "the scratch directory must exist by launch"
        );
        let scratch = scratch.display().to_string();
        for key in ["TMPDIR", "TMP", "TEMP"] {
            assert_eq!(
                wrapped.env.get(key).map(String::as_str),
                Some(scratch.as_str()),
                "{key} must name the launch's private scratch directory, even when the agent's \
                 static environment points elsewhere"
            );
        }
        assert!(
            wrapped.args.contains(&scratch),
            "the sandbox was not given its scratch directory: {:?}",
            wrapped.args
        );
        // What must never happen is that the host temp root is *bound* into the
        // boundary — not that its spelling never appears. bubblewrap covers the
        // sandbox's own `/tmp` with a `--tmpfs`, which is the opposite of
        // granting the host's, and on a host whose temp root is `/tmp` (every
        // Linux) the scratch directory above is legitimately a path under it.
        // So the assertion reads the source of every bind instead.
        let host_temp = std::env::temp_dir().display().to_string();
        let bound: Vec<&str> = wrapped
            .args
            .windows(2)
            .filter(|pair| BIND_FLAGS.contains(&pair[0].as_str()))
            .map(|pair| pair[1].as_str())
            .collect();
        assert!(
            !bound.contains(&host_temp.as_str()),
            "the host temp root must never be granted: {:?}",
            wrapped.args
        );

        crate::agent::sandboxing::cleanup(&config);
        assert!(!std::path::Path::new(&scratch).exists());
    }

    /// A macOS host, where the egress transport is host loopback and no relay
    /// is involved — the shape that composes end to end in a test process.
    fn mac_host() -> SandboxHost {
        SandboxHost::new(std::sync::Arc::new(crate::sandbox::probe::StubHost::macos(
            26, true,
        )))
    }

    /// The seatbelt port the kernel policy opens — which is the one the
    /// composed launch would dial.
    fn proxy_port(wrapped: &SandboxedInvocation) -> u16 {
        let param = wrapped
            .args
            .iter()
            .find(|a| a.starts_with("PROXY="))
            .unwrap_or_else(|| panic!("no proxy parameter in {:?}", wrapped.args));
        param
            .trim_start_matches("PROXY=localhost:")
            .parse()
            .unwrap_or_else(|e| panic!("{param}: {e}"))
    }

    /// Wait for every egress command queued so far to have been handled: the
    /// supervisor answers one at a time, so a reply to a later one is proof the
    /// earlier ones are done.
    fn settle(session_key: &str) -> Option<Vec<String>> {
        crate::sandbox::egress::running_allow_rules(session_key)
    }

    /// A boundary needs a name of its own, and a launch that has none is
    /// refused rather than filed under the shared fallback.
    ///
    /// Two such launches would share one key, and everything the key owns is
    /// per boundary: starting the second replaces the first's instance while
    /// its agent is still running, they hold one token and one socket, and one
    /// session's first-use answer widens the other's allowlist. The scratch
    /// directory tolerates the collision; the boundary must not.
    #[test]
    fn a_launch_with_no_session_id_gets_no_boundary_to_share() {
        let _guard = fabricated_data_dir("unidentified");
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_allow = vec!["api.anthropic.com".into()];
        let config = config_with(Some(profile));
        assert_eq!(session_key(&config), UNIDENTIFIED_SESSION);

        let refusal = build(
            &mac_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
            None,
        )
        .expect_err("an unidentifiable boundary must not be composed");
        assert!(refusal.reason.contains("no session id"), "{refusal}");
        assert_eq!(
            settle(UNIDENTIFIED_SESSION),
            None,
            "a refused launch left an instance behind"
        );

        // The same profile with an id composes: it is the anonymity that is
        // refused, not the profile.
        let mut identified = config.clone();
        identified.agent_session_id = Some("egress-identified".into());
        build(
            &mac_host(),
            "/fabricated/home",
            None,
            &identified,
            "claude",
            &[],
            None,
        )
        .expect("an identified launch composes");
        cleanup(&identified);
    }

    /// Composing is not launching. The instance is bound — argv has to name its
    /// port — but it belongs to nobody until a pane exists, so the launch path
    /// claims it and commits it, and a composition thrown away in between
    /// leaves nothing running.
    #[test]
    fn a_composition_leaves_its_boundary_for_the_launch_to_claim() {
        let _guard = fabricated_data_dir("claim");
        let key = "egress-claim";
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_allow = vec!["api.anthropic.com".into()];
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some(key.into());

        let wrapped = build(
            &mac_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
            None,
        )
        .unwrap();
        let port = proxy_port(&wrapped);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        assert_eq!(
            settle(key),
            None,
            "composing must not hand the session a boundary it is not running behind"
        );

        let pending = pending_egress(&config);
        assert!(pending.is_pending(), "the launch found nothing to claim");
        assert!(
            !pending_egress(&config).is_pending(),
            "a second claim must not take the same instance twice"
        );
        pending.commit();
        assert_eq!(
            settle(key),
            Some(vec!["api.anthropic.com".to_string()]),
            "the launch's boundary is the session's once it has a pane"
        );

        cleanup(&config);
    }

    /// The other half: a launch that never happens releases what it composed,
    /// rather than leaving a listener and a token with no session behind them.
    #[test]
    fn a_launch_that_never_happens_releases_the_boundary_it_composed() {
        let _guard = fabricated_data_dir("released");
        let key = "egress-released";
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_allow = vec!["api.anthropic.com".into()];
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some(key.into());

        let wrapped = build(
            &mac_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
            None,
        )
        .unwrap();
        let port = proxy_port(&wrapped);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());

        drop(pending_egress(&config));
        settle(key);
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
            "the proxy for a launch that never happened is still listening"
        );
        assert_eq!(settle(key), None);
    }

    /// Editing a profile down to a mode the kernel enforces on its own is a
    /// composition that needs no proxy — and it must not take the running
    /// agent's away either, for exactly the same reason: the launch that
    /// replaces that agent may never happen.
    #[test]
    fn a_profile_that_no_longer_needs_a_proxy_keeps_the_running_one_until_the_relaunch() {
        let _guard = fabricated_data_dir("cleared");
        let key = "egress-cleared";
        let compose = |profile: SandboxProfile| {
            let mut config = config_with(Some(profile));
            config.agent_session_id = Some(key.into());
            let wrapped = build(
                &mac_host(),
                "/fabricated/home",
                None,
                &config,
                "claude",
                &[],
                None,
            )
            .expect("the boundary composes");
            (config, wrapped)
        };
        let filtered = || {
            let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
            profile.network_allow = vec!["api.anthropic.com".into()];
            profile
        };
        let unfiltered = || {
            let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
            profile.network_mode = crate::session::NetworkMode::None;
            profile
        };

        // The instance the running agent was launched with.
        let (config, wrapped) = compose(filtered());
        let port = proxy_port(&wrapped);
        pending_egress(&config).commit();
        assert_eq!(settle(key), Some(vec!["api.anthropic.com".to_string()]));

        // The profile is edited, and a relaunch composed against it — twice,
        // once thrown away and once launched.
        let (config, _) = compose(unfiltered());
        assert_eq!(
            settle(key),
            Some(vec!["api.anthropic.com".to_string()]),
            "composing retired a boundary the running agent is still reaching"
        );
        drop(pending_egress(&config));
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok(),
            "a relaunch that never happened cost the running agent its egress"
        );

        let (config, _) = compose(unfiltered());
        pending_egress(&config).commit();
        assert_eq!(settle(key), None, "the relaunch needs no proxy");
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
            "the boundary the session no longer has is still listening"
        );
    }

    /// The whole of P2 in one launch: a filtered profile starts a proxy before
    /// the agent exists, the kernel policy opens exactly that port, and the
    /// agent is handed every spelling of the proxy environment.
    ///
    /// `ALL_PROXY` is asserted to be **`socks5h`**, which is the one detail
    /// that fails silently: with plain `socks5` the client resolves the
    /// hostname itself and hands the proxy an address, so every domain rule
    /// stops matching and the allowlist enforces nothing at all.
    #[test]
    fn a_filtered_profile_is_launched_with_a_proxy_and_the_environment_to_use_it() {
        let _guard = fabricated_data_dir("egress");
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_allow = vec!["api.anthropic.com".into()];
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some("egress-launch".into());
        let def = agent_def();

        let wrapped = build(
            &mac_host(),
            "/fabricated/home",
            Some(&def),
            &config,
            "claude",
            &[],
            None,
        )
        .unwrap();

        // The one hole the profile leaves open names the port the proxy is
        // already listening on: nothing can race a listener that is not bound.
        let port = proxy_port(&wrapped);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());

        let expected = format!("127.0.0.1:{port}");
        for name in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            let value = wrapped
                .env
                .get(name)
                .unwrap_or_else(|| panic!("{name} is unset: {:?}", wrapped.env));
            assert!(value.starts_with("http://"), "{name} = {value}");
            assert!(value.ends_with(&expected), "{name} = {value}");
        }
        for name in ["ALL_PROXY", "all_proxy"] {
            let value = wrapped.env.get(name).expect("ALL_PROXY is set");
            assert!(
                value.starts_with("socks5h://"),
                "{name} must keep resolution on the proxy's side: {value}"
            );
        }
        for name in ["NO_PROXY", "no_proxy"] {
            assert_eq!(
                wrapped.env.get(name).map(String::as_str),
                Some("localhost,127.0.0.1,::1"),
                "the agent's own local traffic must not be tunnelled"
            );
        }
        // The agent's declared environment still arrives; the boundary's
        // variables are simply the last word.
        assert_eq!(
            wrapped.env.get("DISABLE_AUTOUPDATER").map(String::as_str),
            Some("1")
        );

        // Teardown takes the proxy with the scratch directory, so no listener
        // outlives the session that was given it. The stop is queued rather
        // than waited on — teardown must not block a UI thread — so the
        // assertion is that it happens, not that it has already happened.
        cleanup(&config);
        let closed = (0..200).any(|_| {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            false
        });
        assert!(closed, "the proxy outlived its session");
    }

    /// Fail closed: a proxy that cannot start refuses the launch, so an
    /// `allowlist` sandbox is never started believing it is filtered. The
    /// profile's own escape hatch decides what happens next — and a fallback
    /// keeps the desired profile, which is what the next relaunch rebuilds
    /// from.
    #[test]
    fn a_proxy_that_cannot_start_refuses_the_launch() {
        // A data directory deep enough that the socket path cannot fit in
        // `sun_path`, which is a failure with no host and no network in it.
        let base =
            std::env::temp_dir().join(format!("frs{}-{}", std::process::id(), "d".repeat(120)));
        let _guard = crate::paths::TestPathGuard::new(&base);
        let mut config = config_with(Some(SandboxProfile::new(
            "dev",
            vec![SandboxPath::workspace("~/dev/app")],
        )));
        config.agent_session_id = Some("egress-refused".into());

        let err = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
            None,
        )
        .unwrap_err();
        assert!(err.reason.contains("egress proxy"), "{err}");
        assert!(err.reason.contains("at most"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A place-backed session's writable directory lives inside the place's
    /// tree, not in the policy scratch root — so teardown has to reach it there
    /// or the socket, and everything the agent wrote beside it, outlives the
    /// session forever.
    #[test]
    fn deleting_a_place_backed_session_drops_its_directory_inside_the_place() {
        let _guard = fabricated_data_dir("place-teardown");
        let key = "place-teardown-session";
        let minted = crate::sandbox::create_place_session_dir("dev", key).unwrap();
        std::fs::write(minted.join("agent-scratch"), "x").unwrap();
        assert!(minted.is_dir());

        cleanup_key(key);
        assert!(
            !minted.exists(),
            "{} outlived the session",
            minted.display()
        );
        // The place itself is the profile's and outlives every session in it.
        assert!(crate::sandbox::dirs::place_dir("dev").unwrap().is_dir());
    }

    /// A copy-on-write launch's layers are the third per-session tree, and the
    /// only one holding what the agent *wrote*. They deliberately live outside
    /// the two roots teardown already knew about — a directory the sandbox can
    /// reach is an `upperdir` it can redirect — so teardown has to reach them
    /// where they are, or every workspace the user asked to throw away stays on
    /// disk for the life of the machine.
    #[test]
    fn deleting_a_session_drops_the_copy_on_write_layers_it_wrote_into() {
        use crate::sandbox::bwrap::{cleanup_overlays, overlay_workspaces};
        let _guard = fabricated_data_dir("overlay-teardown");
        let roots = ["/repo".to_string()];
        let mine = overlay_workspaces("overlay-teardown-a", &roots).unwrap();
        let sibling = overlay_workspaces("overlay-teardown-b", &roots).unwrap();
        let upper = std::path::PathBuf::from(&mine[0].upper);
        let theirs = std::path::PathBuf::from(&sibling[0].upper);
        std::fs::write(upper.join("what-the-agent-wrote"), "x").unwrap();
        std::fs::write(theirs.join("what-the-agent-wrote"), "x").unwrap();

        cleanup_key("overlay-teardown-a");
        assert!(!upper.exists(), "{} outlived the session", upper.display());
        // Teardown holds one session's key, and takes exactly that session's
        // layers: the sibling is still running in its own.
        assert!(theirs.join("what-the-agent-wrote").exists());
        cleanup_overlays("overlay-teardown-b");
    }

    // ---- Place backends ---------------------------------------------------

    /// The container id the stub engine hands back for a freshly created place.
    const STUB_CONTAINER: &str = "1f2e3d4c5b6a798807162534435261708192a3b4c5d6e7f8091a2b3c4d5e6f70";

    /// A host with a working rootless podman and nothing else, scripted far
    /// enough to create a place and resolve the relay inside it.
    ///
    /// Nothing here starts, pulls or builds anything: every engine command goes
    /// through the injected probe host, and the paths are all fabricated or
    /// friring's own under the test's data directory.
    fn place_host() -> SandboxHost {
        place_host_answering_exec(crate::sandbox::probe::ProbeOutput::success(
            "/usr/local/bin/friring-cli\n",
        ))
    }

    /// [`place_host`], for a place whose kernel is friring's own.
    ///
    /// What a filtered profile needs, and what a plain [`StubHost`] cannot be:
    /// before it composes one, friring binds a listener outside the place and
    /// has the place dial it, because the proxy's socket only carries a
    /// *listener* where the two share a kernel (`ContainerBackend::
    /// check_proxy_reachable`). A stub answers a command line with canned text,
    /// so nothing ever dials and every filtered place is refused for a boundary
    /// that was never really asked. This host answers the dial by dialling —
    /// which is exactly what an `exec` into a container on this machine's own
    /// kernel does, and is the case being modelled.
    #[cfg(unix)]
    fn place_host_on_this_kernel() -> SandboxHost {
        use crate::sandbox::probe::{ProbeHost, ProbeOutput, StubHost};

        /// Delegates everything but the dial.
        struct OnThisKernel(StubHost);

        impl ProbeHost for OnThisKernel {
            fn which(&self, program: &str) -> Option<String> {
                self.0.which(program)
            }
            fn home(&self) -> Option<String> {
                self.0.home()
            }
            fn path_exists(&self, path: &str) -> bool {
                self.0.path_exists(path)
            }
            fn read_file(&self, path: &str) -> Option<String> {
                self.0.read_file(path)
            }
            fn run(&self, program: &str, args: &[&str]) -> Result<ProbeOutput, String> {
                // `exec <ctr> tmux -S <socket> …`: the dialer's own exit status
                // is never what friring reads, so this answers with the failure
                // a real one gives after the listener accepts and closes.
                let dialled = (args.first() == Some(&"exec"))
                    .then(|| args.windows(2).find(|w| w[0] == "-S").map(|w| w[1]))
                    .flatten();
                if let Some(socket) = dialled {
                    let _ = std::os::unix::net::UnixStream::connect(socket);
                    return Ok(ProbeOutput::failure(1, "protocol version mismatch\n"));
                }
                self.0.run(program, args)
            }
        }

        SandboxHost::new(std::sync::Arc::new(OnThisKernel(place_stub(
            ProbeOutput::success("/usr/local/bin/friring-cli\n"),
        ))))
    }

    /// [`place_host`] with the place's answer to `command -v` scripted.
    ///
    /// One `exec` answers both questions a launch asks of a place — where the
    /// relay binary is, and whether the agent it is about to run is in there —
    /// so a test of either scripts it here.
    fn place_host_answering_exec(exec: crate::sandbox::probe::ProbeOutput) -> SandboxHost {
        SandboxHost::new(std::sync::Arc::new(place_stub(exec)))
    }

    /// The scripted engine [`place_host_answering_exec`] is built from, before
    /// it becomes a host — so a test that needs to answer one question itself
    /// can wrap it rather than script the whole engine again.
    fn place_stub(exec: crate::sandbox::probe::ProbeOutput) -> crate::sandbox::probe::StubHost {
        use crate::sandbox::probe::ProbeOutput;
        const PODMAN: &str = "/usr/bin/podman";
        let place_dir = crate::sandbox::dirs::place_dir("dev").expect("a data directory");
        let home_dir = crate::sandbox::dirs::place_home_dir("dev").expect("a data directory");
        let stub = crate::sandbox::probe::StubHost::new()
            .with_home("/fabricated/home")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("podman")
            .with_command("id -u", ProbeOutput::success("1000\n"))
            .with_command("id -g", ProbeOutput::success("1000\n"))
            .with_command(
                &format!(
                    "{PODMAN} info --format {}",
                    "{{.Version.Version}}|{{.Host.Security.Rootless}}"
                ),
                ProbeOutput::success("5.2.2|true\n"),
            )
            .with_path("/fabricated/home/dev/app")
            .with_path(&place_dir.display().to_string())
            .with_path(&home_dir.display().to_string())
            .with_command_prefix(
                &format!("{PODMAN} image inspect"),
                ProbeOutput::success("[{}]\n"),
            )
            .with_command_prefix(
                &format!("{PODMAN} run"),
                ProbeOutput::success(format!("{STUB_CONTAINER}\n")),
            )
            .with_command_prefix(&format!("{PODMAN} exec"), exec);
        stub
    }

    /// A place runs the agent *inside* itself, so a launch whose agent is not in
    /// there is refused with the command that installs one — rather than opening
    /// a pane that dies the instant it appears, taking the sign-in that happens
    /// in that same pane with it.
    ///
    /// Asked per launch and not per place: a place is shared by every session of
    /// its profile, and those sessions need not run the same agent.
    #[test]
    #[cfg(unix)]
    fn a_place_without_the_agent_refuses_the_launch_rather_than_opening_a_dead_pane() {
        use crate::sandbox::probe::ProbeOutput;
        let _paths = short_data_dir("no-agent");
        // `command -v` says nothing at all when it finds nothing, and the
        // profile asks for no egress so the relay is never looked up.
        let host = place_host_answering_exec(ProbeOutput::success("\n"));
        let _host = TestSandboxHost::new(host);

        let config = config_with(Some(place_profile(|profile| {
            profile.network_mode = crate::session::NetworkMode::None;
        })));
        let err = apply(Some(&agent_def()), &config, "claude", &[])
            .expect_err("a place with no agent must not compose");
        assert!(err.contains("has no 'claude' on PATH"), "{err}");
        assert!(
            err.contains("install yours once into this profile's home"),
            "the refusal carries the fix, not just the problem: {err}"
        );
        // The same place with the agent in it composes.
        let _host = TestSandboxHost::new(place_host_answering_exec(ProbeOutput::success(
            "/home/agent/.npm-global/bin/claude\n",
        )));
        apply(Some(&agent_def()), &config, "claude", &[]).expect("an installed agent composes");
    }

    /// A profile can have **several live places at once** — an edited profile
    /// builds a new container while the sessions already launched keep running
    /// in the old one — so teardown and delivery must be given all of them.
    ///
    /// Answering with one is how a caller acts on a stranger: a pane id is per
    /// tmux server and therefore per container, so `%1` in the container a
    /// session is *not* in names a different session's agent.
    #[cfg(unix)]
    #[test]
    fn every_live_place_of_a_profile_is_answered_with_not_just_the_first() {
        use crate::sandbox::probe::ProbeOutput;
        const PODMAN: &str = "/usr/bin/podman";
        const SUPERSEDED: &str = "aa11bb22cc33dd44ee55ff6677889900aabbccddeeff00112233445566778899";
        const REBUILT: &str = "99887766554433221100ffeeddccbbaa00998877665544332211ffeeddccbbaa";

        let inspect = |id: &str, spec: &str| {
            (
                format!(
                    "{PODMAN} inspect --type container --format {} {id}",
                    crate::sandbox::container::INSPECT_FORMAT
                ),
                ProbeOutput::success(format!("{id}|running|1|dev|{spec}\n")),
            )
        };
        let (old_line, old_out) = inspect(SUPERSEDED, "specold");
        let (new_line, new_out) = inspect(REBUILT, "specnew");
        let stub = crate::sandbox::probe::StubHost::new()
            .with_home("/fabricated/home")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("podman")
            .with_command(
                &format!(
                    "{PODMAN} info --format {}",
                    "{{.Version.Version}}|{{.Host.Security.Rootless}}"
                ),
                ProbeOutput::success("5.2.2|true\n"),
            )
            .with_command_prefix(
                &format!("{PODMAN} ps"),
                ProbeOutput::success(format!("{SUPERSEDED}\n{REBUILT}\n")),
            )
            .with_command(&old_line, old_out)
            .with_command(&new_line, new_out);
        let _host = TestSandboxHost::new(SandboxHost::new(std::sync::Arc::new(stub)));

        let found: Vec<String> = running_places("dev", &[])
            .iter()
            .map(|place| place.container().to_string())
            .collect();
        assert_eq!(found, [SUPERSEDED, REBUILT], "both places, in engine order");
        // A profile with no place of its own gets none of somebody else's.
        assert!(running_places("other", &[]).is_empty());

        // …and a profile that was **renamed** still finds its own. An engine
        // cannot relabel a running container, so both of these still answer to
        // `dev` while the rows, the profile and every session say `dev2`.
        // Matched on the label alone this is "nothing is running": teardown
        // leaves the agent working inside and automation delivery never fires.
        let recorded = [SUPERSEDED.to_string(), REBUILT.to_string()];
        let after_rename: Vec<String> = running_places("dev2", &recorded)
            .iter()
            .map(|place| place.container().to_string())
            .collect();
        assert_eq!(after_rename, [SUPERSEDED, REBUILT]);
        // The ids are a second name for *these* containers, never a way to
        // reach one no row of this profile's names.
        assert!(running_places("other", &[]).is_empty());
    }

    /// A data directory short enough for a **place's** unix socket path.
    ///
    /// A place nests its per-session directory one level deeper than a policy
    /// sandbox's (`sandbox/pl/<profile>/<digest>/proxy.sock`), and macOS's
    /// per-user temp root is ~49 bytes before anything is appended — together
    /// they overrun `sun_path`'s 103. The real data directory
    /// (`~/.local/share/friring`) is nowhere near it; this is the test
    /// environment's problem, and the launch says so with the fix when it is
    /// anyone's.
    #[cfg(unix)]
    fn short_data_dir(name: &str) -> crate::paths::TestPathGuard {
        crate::paths::TestPathGuard::new(
            std::path::Path::new("/tmp").join(format!("fr{}{name}", std::process::id())),
        )
    }

    fn place_profile(mutate: impl FnOnce(&mut SandboxProfile)) -> SandboxProfile {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.backend = SandboxBackendKind::Podman;
        mutate(&mut profile);
        profile
    }

    /// The whole of a place launch in one composition: the place is ensured,
    /// the command composed for the *inside* of it, and the transport that
    /// reaches it handed back with the row to record.
    #[cfg(unix)]
    #[test]
    fn a_place_profile_composes_a_transport_and_an_in_place_command() {
        let _guard = fabricated_data_dir("place-compose");
        let profile = place_profile(|p| p.network_mode = crate::session::NetworkMode::None);
        let mut config = config_with(Some(profile));
        config.agent_session_id = Some("place-compose".into());
        config.cwd = Some("/fabricated/home/dev/app".into());

        let wrapped = build(
            &place_host(),
            "/fabricated/home",
            Some(&agent_def()),
            &config,
            "claude",
            &["--resume".into()],
            None,
        )
        .expect("a place composes");

        // Nothing the in-place command says names the engine: reaching the
        // place is the transport's business, and this runs inside it.
        assert_eq!(wrapped.command, "claude");
        assert_eq!(wrapped.args.first().map(String::as_str), Some("--resume"));
        assert!(!wrapped.args.iter().any(|a| a.contains("podman")));

        // The transport, built from the vetted engine path and the id the
        // engine minted, named the way `backend_type` will record it.
        let place = wrapped.place.expect("a place-backed launch has a place");
        assert_eq!(place.engine(), "/usr/bin/podman");
        assert_eq!(place.container(), STUB_CONTAINER);
        assert_eq!(place.backend_name(), "sandbox:dev");

        // …and the row garbage collection finds the container by.
        let instance = wrapped
            .instance
            .expect("a place launch records an instance");
        assert_eq!(instance.profile, "dev");
        assert_eq!(instance.engine, SandboxBackendKind::Podman);
        assert_eq!(instance.external_id, STUB_CONTAINER);
    }

    /// A policy launch composes no place, and that is what stops the two halves
    /// of ADR-26 from being confused for one another.
    #[test]
    fn a_policy_profile_composes_no_place() {
        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            None,
            &config_with(Some(closed_profile())),
            "claude",
            &[],
            None,
        )
        .unwrap();
        assert!(wrapped.place.is_none());
        assert!(wrapped.instance.is_none());
    }

    /// friring builds both shapes of boundary on the machine it runs on, so
    /// neither can wrap a session whose worktrees and tmux are on another host:
    /// a place created here would mount *this* machine's paths around an agent
    /// working over there.
    #[test]
    fn a_remote_session_is_refused_whichever_shape_the_profile_resolves_to() {
        let _guard = fabricated_data_dir("place-remote");
        for profile in [
            place_profile(|p| p.network_mode = crate::session::NetworkMode::None),
            closed_profile(),
        ] {
            let mut config = config_with(Some(profile));
            config.agent_session_id = Some("place-remote".into());
            config.backend = Some("ssh:devbox".into());
            let err = build(
                &place_host(),
                "/fabricated/home",
                None,
                &config,
                "claude",
                &[],
                None,
            )
            .expect_err("a boundary friring builds here cannot hold a session over there");
            assert!(err.reason.contains("remote host"), "{err}");
        }
    }

    /// …and a place-backed session's *own* backend is not a remote one, so the
    /// refusal above must not catch its every relaunch.
    #[cfg(unix)]
    #[test]
    fn a_places_own_backend_is_not_mistaken_for_a_remote_host() {
        let _guard = fabricated_data_dir("place-relaunch");
        let mut config = config_with(Some(place_profile(|p| {
            p.network_mode = crate::session::NetworkMode::None;
        })));
        config.agent_session_id = Some("place-relaunch".into());
        config.backend = Some("sandbox:dev".into());
        build(
            &place_host(),
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
            None,
        )
        .expect("a place-backed session relaunches into its place");
    }

    /// friring's own hook payload, written where a launch would find it: a
    /// fabricated config directory under the guard's root, never the machine
    /// owner's.
    #[cfg(unix)]
    fn fabricated_hook_payload() -> String {
        let path = crate::paths::config_file()
            .and_then(|p| p.parent().map(|d| d.join("hooks").join("claude.json")))
            .expect("a config directory");
        std::fs::create_dir_all(path.parent().expect("a hooks directory")).unwrap();
        std::fs::write(
            &path,
            "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\
             \"friring-cli session signal --state done || true\"}]}]}}",
        )
        .unwrap();
        path.display().to_string()
    }

    /// The whole of what P3b buys a place-backed session, end to end: friring's
    /// own hook payload crosses with its signal commands rewritten for a
    /// boundary that has neither `friring-cli` nor the database (ADR-29), the
    /// launch points the agent's own argument at where it landed instead of
    /// dropping it, and the composition says both what crossed and how the
    /// agent authenticates in there.
    ///
    /// The regression this pins: "the indicator lies". A session whose hooks
    /// silently vanished never leaves `idle` and nothing on screen connects the
    /// two.
    #[cfg(unix)]
    #[test]
    fn a_place_carries_frirings_hooks_and_reports_through_the_pane() {
        let _guard = fabricated_data_dir("place-config");
        let config_arg = fabricated_hook_payload();
        let mut config = config_with(Some(place_profile(|p| {
            p.network_mode = crate::session::NetworkMode::None;
        })));
        config.agent_session_id = Some("place-config".into());

        let wrapped = build(
            &place_host(),
            "/fabricated/home",
            Some(&agent_def()),
            &config,
            "claude",
            &["--settings".into(), config_arg.clone(), "--verbose".into()],
            None,
        )
        .unwrap();

        // The argument follows the file rather than naming a host path the
        // container does not have — an agent handed a settings path that does
        // not exist dies on startup.
        let inside = wrapped
            .args
            .iter()
            .find(|a| a.ends_with("claude.json"))
            .unwrap_or_else(|| panic!("the settings argument survived: {:?}", wrapped.args));
        assert!(
            inside.starts_with(crate::sandbox::container::CONTAINER_HOME),
            "{inside}"
        );
        assert_ne!(*inside, config_arg);
        assert!(wrapped.args.contains(&"--settings".to_string()));
        assert!(wrapped.args.contains(&"--verbose".to_string()));

        // And the file is really in the place's home, with the one rewrite that
        // makes a status signal reach friring from in there.
        let rel = inside
            .trim_start_matches(crate::sandbox::container::CONTAINER_HOME)
            .trim_start_matches('/');
        let landed = crate::sandbox::dirs::place_home_dir("dev")
            .expect("a data directory")
            .join(rel);
        let text = std::fs::read_to_string(&landed)
            .unwrap_or_else(|e| panic!("{} was not written: {e}", landed.display()));
        assert!(
            text.contains("tmux set-option -p @friring_state done"),
            "{text}"
        );
        assert!(!text.contains("friring-cli session signal"), "{text}");

        assert!(
            wrapped.state.contains("config projected"),
            "{}",
            wrapped.state
        );
        assert!(
            !wrapped.state.contains("reports no state"),
            "a place whose hooks crossed must not claim otherwise: {}",
            wrapped.state
        );
        // The agent has no login in there yet, and the row says what to type.
        assert!(
            wrapped.state.contains("credentials (volume-login)"),
            "{}",
            wrapped.state
        );

        // A policy sandbox keeps the host path: its filesystem is the host's
        // subject to a policy, so the file is right where the argument says —
        // and nothing is projected anywhere.
        let policy = build(
            &stub_host(),
            "/fabricated/home",
            Some(&agent_def()),
            &config_with(Some(closed_profile())),
            "claude",
            &["--settings".into(), config_arg.clone()],
            None,
        )
        .unwrap();
        assert!(policy.args.contains(&config_arg));
        assert!(
            !policy.state.contains("config projected"),
            "{}",
            policy.state
        );
        assert!(
            policy.state.contains("credentials (host-passthrough)"),
            "{}",
            policy.state
        );
    }

    /// An agent that declares a token, a state directory and configuration to
    /// project — the shape the seeded registry ships.
    fn declaring_agent() -> AgentDef {
        AgentDef {
            sandbox: Some(AgentSandboxDef {
                config_dir_env: Some("CLAUDE_CONFIG_DIR".into()),
                state_dir: Some("~/.claude".into()),
                credential_file: Some("~/.claude/.credentials.json".into()),
                secret_env: vec!["ANTHROPIC_API_KEY".into()],
                login_fallback: Some("/login".into()),
                copy_in: vec!["~/.claude/CLAUDE.md".into()],
                bypass: vec!["--dangerously-skip-permissions".into()],
                ..Default::default()
            }),
            ..agent_def()
        }
    }

    /// A fabricated `$HOME` with one instruction file in it, standing in for the
    /// user's agent configuration. Never the machine owner's: every path here is
    /// under the test's own temporary directory.
    ///
    /// Rooted at `dirs::test_temp_base` rather than at the platform temp root,
    /// because the projection refuses an entry reaching a tmux socket directory
    /// — and on Linux that root *is* the platform temp root, so a home built
    /// there would be classified host-only for a reason no test here is about.
    #[cfg(unix)]
    fn fabricated_agent_home(name: &str) -> String {
        let home = crate::sandbox::dirs::test_temp_base(name).join("home");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude/CLAUDE.md"),
            "# fabricated instructions\n",
        )
        .unwrap();
        home.display().to_string()
    }

    /// The token reaches the launch on the one channel that is not a command
    /// line, and appears in nothing else the launch produces.
    ///
    /// The store is a stub: no test may consult a real keychain, and this one
    /// holds a fabricated value in memory.
    #[cfg(unix)]
    #[test]
    fn a_stored_token_is_injected_off_the_command_line_and_named_nowhere_else() {
        const FABRICATED: &str = "sk-fabricated-not-a-real-token";
        let _guard = fabricated_data_dir("place-token");
        let _store = crate::sandbox::auth::keychain::TestSecretStore::install(
            crate::sandbox::auth::keychain::StubStore::new().with_token(
                "claude",
                "ANTHROPIC_API_KEY",
                FABRICATED,
            ),
        );
        let mut config = config_with(Some(place_profile(|p| {
            p.network_mode = crate::session::NetworkMode::None;
        })));
        config.agent_session_id = Some("place-token".into());

        let wrapped = build(
            &place_host(),
            "/fabricated/home",
            Some(&declaring_agent()),
            &config,
            "claude",
            &[],
            None,
        )
        .unwrap();

        assert_eq!(
            wrapped.secret_env,
            [("ANTHROPIC_API_KEY".to_string(), FABRICATED.to_string())]
        );
        // Everything else a launch hands out, and none of it carries the value.
        let rendered = format!(
            "{} {} {:?} {:?} {} {}",
            wrapped.command,
            wrapped.args.join(" "),
            wrapped.env,
            wrapped,
            wrapped.label,
            wrapped.state
        );
        assert!(
            !rendered.contains(FABRICATED),
            "a token leaked into: {rendered}"
        );
        assert!(!wrapped.env.contains_key("ANTHROPIC_API_KEY"));
        // The name is diagnostic and stays; the strategy is on the row.
        assert!(
            wrapped.state.contains("credentials (env-token)"),
            "{}",
            wrapped.state
        );
        assert!(
            wrapped.state.contains("ANTHROPIC_API_KEY"),
            "{}",
            wrapped.state
        );
        // …and the agent's state directory is relocated into the place's own
        // home, which is what makes the login survive a container rebuild.
        assert_eq!(
            wrapped.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/home/agent/.claude")
        );
    }

    /// Every relaunch of a place-backed session — restart, restore, a fork's
    /// first spawn — re-derives the whole boundary from the profile, so the
    /// projected configuration follows the host's and the login state follows
    /// the place's.
    ///
    /// The regression this pins is the one this feature keeps re-breaking:
    /// sandbox state silently dropped on a path that is not the first spawn. A
    /// session that logged in once must not be told to log in again, and a hook
    /// payload friring has since changed must not stay stale inside the place.
    #[cfg(unix)]
    #[test]
    fn a_relaunch_reprojects_the_config_and_keeps_the_one_login() {
        let _guard = fabricated_data_dir("place-relaunch");
        let config_arg = fabricated_hook_payload();
        let mut config = config_with(Some(place_profile(|p| {
            p.network_mode = crate::session::NetworkMode::None;
        })));
        config.agent_session_id = Some("place-relaunch".into());
        let launch = |args: &[String]| {
            build(
                &place_host(),
                "/fabricated/home",
                Some(&declaring_agent()),
                &config,
                "claude",
                args,
                None,
            )
            .unwrap()
        };

        let args = vec!["--settings".to_string(), config_arg.clone()];
        let first = launch(&args);
        assert!(first.login.is_some(), "a fresh place has no login in it");

        // The host's payload changes (a friring upgrade, an extension heal) and
        // the relaunch carries the new one rather than leaving the place on the
        // copy the first spawn made.
        std::fs::write(
            &config_arg,
            "{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\
             \"friring-cli session signal --state blocked || true\"}]}]}}",
        )
        .unwrap();
        let second = launch(&args);
        let inside = second
            .args
            .iter()
            .find(|a| a.ends_with("claude.json"))
            .expect("the settings argument survived");
        let landed = crate::sandbox::dirs::place_home_dir("dev")
            .expect("a data directory")
            .join(
                inside
                    .trim_start_matches(crate::sandbox::container::CONTAINER_HOME)
                    .trim_start_matches('/'),
            );
        assert!(std::fs::read_to_string(&landed)
            .unwrap()
            .contains("@friring_state blocked"));

        // A login done inside the pane lands in the profile's own home, and the
        // next relaunch stops asking for one. Fabricated: this is friring's own
        // per-profile directory under a temp data dir, never a real credential.
        let credential = crate::sandbox::dirs::place_home_dir("dev")
            .expect("a data directory")
            .join(".claude/.credentials.json");
        std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
        std::fs::write(&credential, "{\"fabricated\":true}").unwrap();
        let third = launch(&args);
        assert!(third.login.is_none(), "{:?}", third.login);
        assert!(third.state.contains("already signed in"), "{}", third.state);
    }

    /// A policy backend uses the host's own store, so nothing is looked up and
    /// nothing is injected — structurally, not by luck.
    #[test]
    fn a_policy_launch_injects_no_credential_and_consults_no_store() {
        let _guard = fabricated_data_dir("policy-creds");
        let _store = crate::sandbox::auth::keychain::TestSecretStore::install(
            crate::sandbox::auth::keychain::StubStore::new().with_token(
                "claude",
                "ANTHROPIC_API_KEY",
                "sk-fabricated-not-a-real-token",
            ),
        );
        let wrapped = build(
            &stub_host(),
            "/fabricated/home",
            Some(&declaring_agent()),
            &config_with(Some(closed_profile())),
            "claude",
            &[],
            None,
        )
        .unwrap();

        assert!(wrapped.secret_env.is_empty());
        assert!(!wrapped.env.contains_key("ANTHROPIC_API_KEY"));
        // Relocating the state directory under a policy backend would strand the
        // login the user already has — host passthrough means the real one.
        assert!(!wrapped.env.contains_key("CLAUDE_CONFIG_DIR"));
        assert!(
            wrapped.state.contains("credentials (host-passthrough)"),
            "{}",
            wrapped.state
        );
    }

    /// The user's own configuration crosses into the place's home, at the same
    /// home-relative path — which is the whole point of a synthetic `$HOME`.
    #[cfg(unix)]
    #[test]
    fn a_declared_config_entry_lands_in_the_places_home() {
        let _guard = fabricated_data_dir("place-copyin");
        let home = fabricated_agent_home("place-copyin");
        let mut config = config_with(Some(place_profile(|p| {
            p.network_mode = crate::session::NetworkMode::None;
        })));
        config.agent_session_id = Some("place-copyin".into());

        let wrapped = build(
            &place_host(),
            &home,
            Some(&declaring_agent()),
            &config,
            "claude",
            &[],
            None,
        )
        .unwrap();

        let landed = crate::sandbox::dirs::place_home_dir("dev")
            .expect("a data directory")
            .join(".claude/CLAUDE.md");
        assert_eq!(
            std::fs::read_to_string(&landed).unwrap(),
            "# fabricated instructions\n"
        );
        assert!(
            wrapped.state.contains("config projected: 1 files"),
            "{}",
            wrapped.state
        );
    }

    /// Sessions of one profile share a place and therefore its loopback, so the
    /// address each one's proxy environment names has to be that session's own
    /// relay port — the second session would otherwise be handed the first's
    /// and fail closed while the profile still claimed a filtered network.
    #[cfg(unix)]
    #[test]
    fn every_session_in_a_place_gets_its_own_proxy_address() {
        let _guard = short_data_dir("pe");
        let host = place_host_on_this_kernel();
        let compose = |key: &str| {
            let mut config = config_with(Some(place_profile(|p| {
                p.network_allow = vec!["api.anthropic.com".into()];
            })));
            config.agent_session_id = Some(key.to_string());
            let wrapped = build(
                &host,
                "/fabricated/home",
                None,
                &config,
                "claude",
                &[],
                None,
            )
            .expect("a filtered place composes");
            pending_egress(&config).commit();
            (config, wrapped)
        };

        let (first_config, first) = compose("place-egress-a");
        let (second_config, second) = compose("place-egress-b");
        let address = |wrapped: &SandboxedInvocation| {
            wrapped
                .env
                .get("HTTP_PROXY")
                .expect("a filtered launch is given the proxy environment")
                .rsplit_once(':')
                .map(|(_, port)| port.to_string())
                .expect("a proxy URL names a port")
        };
        assert_ne!(
            address(&first),
            address(&second),
            "two sessions in one place were handed one loopback port"
        );
        // …and each keeps its own across a relaunch, because the environment of
        // the launch that is still running names it.
        let (_, again) = compose("place-egress-a");
        assert_eq!(address(&first), address(&again));

        // The relay runs beside the agent, inside the place — started by
        // friring's own launch helper, which is the place's copy of the CLI.
        assert_eq!(first.command, "/usr/local/bin/friring-cli");
        assert_eq!(&first.args[..2], ["sandbox", "launch"]);

        cleanup(&first_config);
        cleanup(&second_config);
    }

    /// And the other half of that, at the level a user meets it: where the place
    /// cannot be shown to reach the proxy, the launch is refused rather than
    /// composed.
    ///
    /// The failure this closes is not an escape — a place on `--network none`
    /// whose relay forwards to nothing has *less* network than the profile
    /// promises, not more — but "silently no network, while the UI reports the
    /// allowlist applied" is a boundary lying about itself, and every one of
    /// those in this feature's history was a bug.
    #[cfg(unix)]
    #[test]
    fn a_filtered_place_that_cannot_reach_the_proxy_refuses_the_launch() {
        let _guard = short_data_dir("pu");
        // A plain stub engine: it answers the dial with text instead of dialling,
        // which is a place friring cannot settle either way.
        let host = place_host();
        let mut config = config_with(Some(place_profile(|p| {
            p.network_allow = vec!["api.anthropic.com".into()];
        })));
        config.agent_session_id = Some("place-unprovable".into());

        let err = build(
            &host,
            "/fabricated/home",
            None,
            &config,
            "claude",
            &[],
            None,
        )
        .expect_err("a place that cannot be shown to reach the proxy must not compose");
        assert!(
            err.reason.contains("could not prove this place can dial"),
            "{err}"
        );
        assert!(err.reason.contains("egress proxy"), "{err}");
        // A profile refusal, so the profile's own `allow_unsandboxed_fallback`
        // still decides what happens next — this is a host that cannot apply the
        // profile, not evidence of interference.
        assert!(!err.integrity, "{err}");
        cleanup(&config);
    }

    /// The `Debug` that exists to keep a token out of `{:?}` has to keep the
    /// **proxy's** credential out too.
    ///
    /// It does not travel in `secret_env` — that is the agent's own channel —
    /// but inside the proxy URLs friring composes into `env`, which is why
    /// `ProxyGrant`'s own `Debug` withholds them. Merging that env into this
    /// struct puts it back within reach of one `{:?}`: a tracing field, an
    /// `expect`, a failing assertion whose message goes into a log and then
    /// into a bug report.
    #[test]
    fn the_debug_that_hides_a_token_hides_the_proxy_credential_too() {
        const FAKE: &str = "totally-fake-proxy-token";
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/agent".to_string());
        for name in crate::sandbox::egress::HTTP_PROXY_VARS
            .iter()
            .chain(crate::sandbox::egress::SOCKS_PROXY_VARS)
        {
            env.insert(
                (*name).to_string(),
                format!("http://friring:{FAKE}@127.0.0.1:8118"),
            );
        }
        let wrapped = SandboxedInvocation {
            command: "/bin/sh".to_string(),
            args: vec!["claude".to_string()],
            env,
            secret_env: vec![("ANTHROPIC_API_KEY".to_string(), FAKE.to_string())],
            label: String::new(),
            state: String::new(),
            place: None,
            instance: None,
            // The egress record is the second place a credential lives on this
            // type, and its own `Debug` has to withhold it too.
            egress: crate::session::EgressRecord {
                endpoint: Some("tcp:8118".to_string()),
                token: Some(FAKE.to_string()),
                state: crate::session::EgressState::Preparing,
            },
            login: None,
        };

        let rendered = format!("{wrapped:?}");
        assert!(!rendered.contains(FAKE), "{rendered}");
        // The endpoint is not a credential and stays legible.
        assert!(rendered.contains("tcp:8118"), "{rendered}");
        // The names are the diagnostic and stay, and so does everything that is
        // not a credential.
        for name in ["HTTP_PROXY", "ALL_PROXY", "ANTHROPIC_API_KEY"] {
            assert!(rendered.contains(name), "{rendered}");
        }
        assert!(rendered.contains("/home/agent"), "{rendered}");
    }

    /// The one shape assertion that does not need a backend to be installed:
    /// the session key is what names the generated profile, and a spawn always
    /// pins the friring id before launch.
    #[test]
    fn the_session_key_prefers_the_friring_id() {
        let mut config = config_with(None);
        assert_eq!(session_key(&config), "session");
        config.agent_session_id = Some("agent-conversation".into());
        assert_eq!(session_key(&config), "agent-conversation");
        let id = crate::session::SessionId::default();
        config.session_id = Some(id);
        assert_eq!(session_key(&config), id.to_string());
    }

    /// Session teardown holds a stored row, not the config the launch was
    /// composed from, so the id-keyed cleanup has to reach exactly what a
    /// pinned launch minted. If the two key derivations ever drift, a deleted
    /// session leaves its agent's writable scratch on disk forever.
    #[test]
    fn a_delete_that_knows_only_the_session_id_still_reaches_the_scratch() {
        let id = crate::session::SessionId::default();
        let mut config = config_with(None);
        config.session_id = Some(id);

        let minted = crate::sandbox::create_session_scratch(&session_key(&config)).unwrap();
        std::fs::write(minted.join("agent-scratch"), "x").unwrap();

        cleanup_by_session_id(id);
        assert!(
            !minted.exists(),
            "{} outlived the session",
            minted.display()
        );
    }
}
