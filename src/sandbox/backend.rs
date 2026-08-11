//! The seam every isolation technology plugs into.
//!
//! [`SandboxBackend`] is deliberately small: probe the host, declare what the
//! UI may offer, and then do exactly one of two things — wrap an argv (policy
//! backends) or ensure an environment exists (place backends). ADR-26 in
//! `docs/SANDBOX.md` explains why those are the only two shapes.

use std::fmt;

use crate::session::{
    NetworkMode, ReadScope, SandboxBackendKind, SandboxInstance, SandboxPolicy, SandboxProfile,
    SandboxShape,
};

/// A command line as the launch path passes it around: program first, then its
/// arguments. Policy backends take one and return a longer one.
pub type Argv = Vec<String>;

/// The one path that stays read-only *inside* a writable root, in every
/// backend that can express it (`docs/SANDBOX.md` §Inner agent sandboxes).
///
/// Hook scripts are run by whichever git touches the repository next —
/// including the **host's**, outside the boundary — so a writable hooks
/// directory is arbitrary host command execution. Protecting it costs nothing
/// when the root is not a repository: denying writes to a path that does not
/// exist is a no-op.
///
/// `.git/config` is deliberately *not* protected despite being a comparable
/// channel (`core.pager`, `core.fsmonitor` and aliases all name commands):
/// ordinary work inside a sandbox writes it — `git config`, `git remote add` —
/// and a profile that breaks `git` gets turned off, which protects nothing.
pub const PROTECTED_IN_WRITABLE_ROOT: &str = ".git/hooks";

/// Result of any sandbox operation.
pub type SandboxResult<T> = std::result::Result<T, SandboxError>;

/// Why a sandbox operation could not be carried out.
///
/// Cloneable and comparable so tests can assert on an exact failure and the UI
/// can stash one in model state; the I/O variant therefore carries a rendered
/// message rather than a [`std::io::Error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// The backend cannot run here. `reason` is [`Availability`]'s message, so
    /// the failure a user sees at launch is the same sentence the backend
    /// picker showed.
    Unavailable {
        backend: SandboxBackendKind,
        reason: String,
    },
    /// A policy operation was asked of a place backend, or the reverse. A
    /// programming error rather than a user-facing condition.
    WrongShape {
        backend: SandboxBackendKind,
        shape: SandboxShape,
    },
    /// The backend exists in the design but not in this build stage yet (the
    /// place backends land in P3 — see `docs/SANDBOX.md` §Delivery phases).
    NotInThisStage {
        backend: SandboxBackendKind,
        detail: String,
    },
    /// The policy asks for something this backend cannot express.
    Unsupported {
        backend: SandboxBackendKind,
        detail: String,
    },
    /// The launch would grant more than the profile's own words justify, or
    /// less than they promise. Raised by [`SandboxLaunch::validate`] before any
    /// policy is generated: a boundary that quietly differs from what the user
    /// wrote is worse than one that refuses and says why.
    Refused { profile: String, detail: String },
    /// The boundary's **own state** is not what friring left it as: a path
    /// friring mints turned out to be a symlink, a mount source travels through
    /// one, or the socket a filtered launch needs is held by something else.
    ///
    /// A separate variant because it is the one refusal that must **not** route
    /// through a profile's `allow_unsandboxed_fallback`. That switch means "this
    /// host cannot apply this profile, run on the host instead" — a backend that
    /// is not installed, a policy this build cannot express. This means
    /// something that was *inside* a boundary changed the state friring builds
    /// the next one out of, and turning that into "so run outside the boundary"
    /// would let a sandboxed agent choose to be unsandboxed. See
    /// `docs/SANDBOX.md` §Failure modes.
    ///
    /// `subject` is whatever the refusal it came from named: the profile for a
    /// [`Refused`](Self::Refused), the path for an [`Io`](Self::Io).
    Tampered { subject: String, detail: String },
    /// Writing or reading a generated artefact failed.
    Io { path: String, detail: String },
}

impl SandboxError {
    /// Re-label a refusal as evidence that something inside a boundary
    /// interfered with friring's own state.
    ///
    /// Written this way round so the refusal's sentence is composed once, where
    /// the check is, and the *classification* is applied by whichever branch
    /// knows it is looking at interference rather than at a profile a user
    /// should edit. Anything that is not a [`Refused`](Self::Refused) is
    /// returned unchanged: an unavailable backend does not become tampering by
    /// passing through here.
    #[must_use]
    pub fn tampered(self) -> Self {
        match self {
            Self::Refused { profile, detail } => Self::Tampered {
                subject: profile,
                detail,
            },
            Self::Io { path, detail } => Self::Tampered {
                subject: path,
                detail,
            },
            other => other,
        }
    }

    /// Whether this is a boundary-integrity failure, which no fallback may
    /// convert into an unsandboxed launch.
    #[must_use]
    pub fn is_tampering(&self) -> bool {
        matches!(self, Self::Tampered { .. })
    }
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { backend, reason } => {
                write!(f, "Sandbox backend '{backend}' is unavailable: {reason}")
            }
            Self::WrongShape { backend, shape } => write!(
                f,
                "Sandbox backend '{backend}' is a {shape} backend and does not support this \
                 operation"
            ),
            Self::NotInThisStage { backend, detail } => {
                write!(f, "Sandbox backend '{backend}' is not built yet: {detail}")
            }
            Self::Unsupported { backend, detail } => {
                write!(f, "Sandbox backend '{backend}' cannot do this: {detail}")
            }
            Self::Refused { profile, detail } => {
                write!(
                    f,
                    "Sandbox profile '{profile}' cannot be launched: {detail}"
                )
            }
            Self::Tampered { subject, detail } => write!(
                f,
                "Sandbox boundary '{subject}' is not what friring left it as, so this launch is \
                 refused rather than run without a sandbox: {detail}"
            ),
            Self::Io { path, detail } => write!(f, "Sandbox file '{path}': {detail}"),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Whether a backend can be used on one host, and — when it cannot — the
/// sentence the UI shows instead of hiding the option.
///
/// A missing backend is never a silently absent menu entry: `docs/SANDBOX.md`
/// §Failure modes requires the reason *and*, where one exists, the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// Usable. `detail` is a short note for the picker — a version, an edition,
    /// whatever distinguishes this host's copy. May be empty.
    Available { detail: String },
    /// Not usable here. `reason` states what is missing; `fix` is the command
    /// or setting that would change the answer, when there is one.
    Unavailable { reason: String, fix: Option<String> },
}

impl Availability {
    /// Usable, with a short version/edition note.
    pub fn available(detail: impl Into<String>) -> Self {
        Self::Available {
            detail: detail.into(),
        }
    }

    /// Unusable, with no action the user could take (wrong OS, wrong CPU).
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
            fix: None,
        }
    }

    /// Unusable, with the command or setting that would fix it.
    pub fn needs_fix(reason: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
            fix: Some(fix.into()),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    /// One line for the picker: the detail when available, otherwise the reason
    /// followed by the fix.
    pub fn message(&self) -> String {
        match self {
            Self::Available { detail } => detail.clone(),
            Self::Unavailable { reason, fix: None } => reason.clone(),
            Self::Unavailable {
                reason,
                fix: Some(fix),
            } => format!("{reason} — {fix}"),
        }
    }
}

impl fmt::Display for Availability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

/// What an agent's *own* sandbox does inside this boundary.
///
/// Both verdicts end with the inner sandbox off; they differ in what the UI can
/// honestly say about why, which is the difference between "we chose to" and
/// "the kernel refuses".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InnerSandboxVerdict {
    /// The kernel refuses the nested policy outright. Under seatbelt an inner
    /// `sandbox_apply` returns `Operation not permitted` as soon as the outer
    /// profile contains a deny rule, which every friring profile does.
    Denied,
    /// It would work, but it re-isolates what is already isolated and costs a
    /// second boundary's worth of failure modes, so friring turns it off.
    Redundant,
}

impl InnerSandboxVerdict {
    /// The clause the composition line ends with (`… inner agent sandbox: off —
    /// <reason>`).
    pub fn reason(self) -> &'static str {
        match self {
            Self::Denied => "nested sandbox policies are denied by the kernel",
            Self::Redundant => "Friring is the boundary",
        }
    }
}

/// What a backend can offer, for the profile editor and the creation step.
///
/// Static per backend: anything that depends on the host (a bwrap old enough to
/// lack overlays, a macOS too old for isolated container networks) belongs in
/// the [`Availability`] the probe returns, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    pub shape: SandboxShape,
    /// Whether `memory_mb` / `cpus` mean anything.
    pub limits: bool,
    /// Network modes this backend can actually enforce, in selector order.
    pub network_modes: &'static [NetworkMode],
    /// Read scopes this backend can express.
    pub read_scopes: &'static [ReadScope],
    /// Whether the environment outlives one command (a place), so several
    /// sessions share it and the manager view has something to list.
    pub persistent: bool,
    /// Whether the host credential store keeps working with no copying — the
    /// `host-passthrough` strategy of `docs/SANDBOX.md` §Credentials.
    pub host_credentials: bool,
    /// What happens to the agent's own sandbox inside this one.
    pub inner_agent_sandbox: InnerSandboxVerdict,
    /// Which way out to the egress proxy this backend's kernel primitive
    /// leaves open — the per-backend table in `docs/SANDBOX.md` §Reaching the
    /// proxy, declared where the primitive is rather than in a lookup beside
    /// it.
    pub proxy_transport: ProxyTransport,
}

/// How a sandbox reaches the friring egress proxy, which is decided by what its
/// own network isolation leaves reachable (ADR-27).
///
/// Not a preference: a backend that shares the host's network stack **can only**
/// be given a port, and one with its own namespace **can only** be given a
/// socket, because inside a fresh namespace `127.0.0.1` is that namespace's own
/// loopback and no address reaches the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyTransport {
    /// Host TCP loopback: the sandbox dials `127.0.0.1:<port>` directly
    /// (seatbelt, which shares the host's network stack).
    Loopback,
    /// A unix socket, bind-mounted across the boundary and reached through the
    /// relay friring runs inside the namespace (`bwrap --unshare-net`, and the
    /// containers that land with the place backends).
    UnixSocket,
}

/// Where the friring egress proxy listens, for the one hole a sandbox that is
/// filtered rather than cut off is allowed to keep (ADR-27).
///
/// Both spellings exist because the backends differ in what they can reach: a
/// seatbelt process still shares the host's network stack and can dial
/// loopback, while a `--unshare-net` bwrap sandbox has its own empty stack and
/// can only be handed a socket. Which one a backend takes is its
/// [`Caps::proxy_transport`]; a backend handed the other refuses the launch
/// rather than opening a hole that leads nowhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyEndpoint {
    /// A TCP listener on host loopback.
    Loopback { port: u16 },
    /// A unix socket on the host, to be exposed inside the boundary at
    /// `inside_path`.
    UnixSocket {
        host_path: String,
        inside_path: String,
    },
}

/// What one launch's network mode comes to, once it is known whether a proxy is
/// running for it.
///
/// The single value both policy backends read, so they cannot disagree about
/// what a profile means: the same three shapes turn into `(allow network*)` or
/// `--unshare-net` on their own terms. Every case that is not positively open
/// or positively proxied is [`Closed`](Self::Closed), which is what makes the
/// absence of a proxy a sandbox with no way out rather than one with an
/// unfiltered one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Egress<'a> {
    /// No way out at all: [`NetworkMode::None`], or a mode that needs the proxy
    /// with none running.
    Closed,
    /// Direct, unfiltered egress: [`NetworkMode::Full`] carrying no denies —
    /// the one mode no proxy is needed to keep honest.
    Open,
    /// Everything through the friring proxy at this endpoint, which enforces
    /// the domain rules outside the boundary.
    Proxied(&'a ProxyEndpoint),
}

/// What a launch into a **place** knows that a policy launch does not.
///
/// Carried on the launch rather than looked up by the backend, because it comes
/// from the place that was ensured for this session. A place backend that finds
/// this missing refuses the launch — an argv composed without it would run on
/// the host under a profile that says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceLaunch<'a> {
    /// The egress relay to start inside the place beside the agent. `None` when
    /// the profile's network mode is one the kernel enforces on its own
    /// (`none`, or `full` with no denies): there is no proxy, so there is
    /// nothing to relay to.
    pub relay: Option<PlaceRelay<'a>>,
}

/// The in-place half of the egress boundary — see
/// `docs/SANDBOX.md` §Reaching the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceRelay<'a> {
    /// Absolute path of friring's own CLI *inside* the place, resolved when the
    /// place was created rather than named on the command line: the relay is a
    /// binary of the **image's**, and the host's copy is the wrong architecture
    /// as often as not.
    pub program: &'a str,
    /// The loopback port the relay offers inside the place. Not a constant the
    /// way it is for a namespaced policy sandbox: a place is shared by the
    /// sessions of its profile, and they share its loopback.
    pub port: u16,
}

/// Everything one launch knows that the profile does not.
///
/// A [`SandboxPolicy`] is per profile and is reused by every session that picks
/// it; this is the per-session half — where the agent is being started, which
/// paths friring minted for it, and whether a proxy is listening yet. Policy
/// backends turn the two into a wrapped argv.
#[derive(Debug, Clone)]
pub struct SandboxLaunch<'a> {
    /// The frozen profile. Already resolved to a concrete backend.
    pub policy: &'a SandboxPolicy,
    /// Home directory **of the machine the agent runs on** — the same value the
    /// policy's paths were expanded against, needed again for the secrets deny
    /// list, which is written relative to `~`.
    pub home: &'a str,
    /// Stable per-session key (friring's session id). Names the generated
    /// profile file, so two sessions of one profile never race on it.
    pub session_key: &'a str,
    /// Credential family of the agent being launched (its registry name, or its
    /// `hook_schema` when it is a rebrand). The one agent whose own credential
    /// file stays readable under [`ReadScope::HostMinusSecrets`] — every other
    /// agent's is denied, so a sandboxed claude cannot read codex's token.
    pub agent: Option<&'a str>,
    /// The directory the agent is launched in. Writable, and the `WORKSPACE`
    /// profile parameter.
    pub workspace: Option<&'a str>,
    /// Per-session status-file directory (ADR-29's file channel). Writable.
    pub signal_dir: Option<&'a str>,
    /// The per-session scratch directory friring minted for this launch
    /// ([`crate::sandbox::dirs::create_session_scratch`]). Writable, because a
    /// CLI that cannot write a temp file dies on startup.
    ///
    /// Never the host temp root. `/tmp` is where friring's own tmux server
    /// listens, and no backend's network isolation stops `connect(2)` on a
    /// pathname unix socket, so a read-write host temp root is a complete
    /// escape — see [`crate::sandbox::dirs`].
    pub tmp_dir: Option<&'a str>,
    /// Absolute path of friring's SQLite database, to be denied explicitly
    /// (ADR-29). Passed in rather than resolved here because the database of
    /// the friring that owns the session is not necessarily the one on the host
    /// where the agent runs.
    pub friring_db: Option<&'a str>,
    /// The egress proxy, once P2 runs one.
    pub proxy: Option<ProxyEndpoint>,
    /// The place this launch runs in, for a place backend. `None` for a policy
    /// backend, whose sandbox is the wrapped process itself.
    pub place: Option<PlaceLaunch<'a>>,
}

impl<'a> SandboxLaunch<'a> {
    /// The mandatory half: which policy, whose home, which session.
    pub fn new(policy: &'a SandboxPolicy, home: &'a str, session_key: &'a str) -> Self {
        Self {
            policy,
            home,
            session_key,
            agent: None,
            workspace: None,
            signal_dir: None,
            tmp_dir: None,
            friring_db: None,
            proxy: None,
            place: None,
        }
    }

    /// The agent's credential family — see [`agent`](Self::agent).
    pub fn with_agent(mut self, agent: &'a str) -> Self {
        self.agent = Some(agent);
        self
    }

    /// The launch directory.
    pub fn with_workspace(mut self, workspace: &'a str) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// The per-session status-file directory.
    pub fn with_signal_dir(mut self, dir: &'a str) -> Self {
        self.signal_dir = Some(dir);
        self
    }

    /// The per-session scratch directory — see [`tmp_dir`](Self::tmp_dir).
    pub fn with_tmp_dir(mut self, dir: &'a str) -> Self {
        self.tmp_dir = Some(dir);
        self
    }

    /// The database path to deny (ADR-29).
    pub fn with_friring_db(mut self, db: &'a str) -> Self {
        self.friring_db = Some(db);
        self
    }

    /// The egress proxy friring started alongside the session.
    pub fn with_proxy(mut self, proxy: ProxyEndpoint) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// The place this launch runs in — see [`PlaceLaunch`].
    pub fn with_place(mut self, place: PlaceLaunch<'a>) -> Self {
        self.place = Some(place);
        self
    }

    /// What this launch's network policy comes to — see [`Egress`].
    ///
    /// Fails closed by construction: a mode that can only be honoured with the
    /// proxy ([`crate::sandbox::egress::proxy_required`]) and no endpoint to
    /// point at is [`Egress::Closed`], never [`Egress::Open`].
    /// [`validate`](Self::validate) refuses that combination before a backend
    /// ever asks, so a caller sees it only by building a launch by hand.
    pub fn egress(&self) -> Egress<'_> {
        if !crate::sandbox::egress::proxy_required(self.policy) {
            return match self.policy.network {
                NetworkMode::Full => Egress::Open,
                _ => Egress::Closed,
            };
        }
        match self.proxy.as_ref() {
            Some(endpoint) => Egress::Proxied(endpoint),
            None => Egress::Closed,
        }
    }

    /// Every writable path this launch grants: the profile's own read-write
    /// paths plus the per-session directories friring mints. Sorted and
    /// de-duplicated, so an ancestor always precedes its descendants — the
    /// order bind-mount backends need.
    pub fn writable_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = self.policy.rw_paths.clone();
        for extra in [self.workspace, self.signal_dir, self.tmp_dir]
            .into_iter()
            .flatten()
        {
            out.push(extra.to_string());
        }
        out.sort();
        out.dedup();
        out
    }

    /// Refuse a launch whose grants do not match what the profile says, before
    /// any backend generates a line of policy.
    ///
    /// Both checks fail closed, and both exist because the alternative is a
    /// boundary the user cannot reason about:
    ///
    /// - **Some read-write roots are escapes**, whatever the profile intended:
    ///   one enclosing friring's data directory reaches the database, which
    ///   ADR-29 keeps outside every boundary, and one reaching a tmux socket
    ///   directory drives the host's own multiplexer. See
    ///   [`crate::sandbox::dirs::check_writable_roots`]. Checked first: it is
    ///   the profile's own doing, and fixable by editing it.
    /// - **friring's own sandbox state is off limits in either mode.** The tree
    ///   beside the database holds the other profiles' logins, the generated
    ///   policies and the other sessions' sockets, and read-only is no defence
    ///   for any of them. See
    ///   [`crate::sandbox::dirs::check_declared_paths`], which is given the
    ///   profile's own paths rather than [`Self::writable_paths`] — this
    ///   launch's minted directories live in exactly that tree and are the point
    ///   of it.
    /// - **The container engine's control socket is off limits in either
    ///   mode**, and not only to the place backends. bwrap masks `/run` and
    ///   `/var/run` under `host-minus-secrets` alone, and a path the profile
    ///   lists explicitly wins that mask — so a policy profile naming
    ///   `/var/run/docker.sock` would get it, and read-only is no defence for a
    ///   socket: a read-only bind does not take write permission off the inode,
    ///   so `connect(2)` still succeeds. Anything that can speak to that socket
    ///   can start a privileged container with the host's filesystem in it. See
    ///   [`crate::sandbox::dirs::grants_engine_socket`].
    /// - **A filtered mode with no filter** is refused. No kernel policy has a
    ///   host-name predicate (`docs/SANDBOX.md` §Egress firewall), so an
    ///   allowlist — and a deny list under `full` — mean nothing without the
    ///   proxy that enforces them. Launching anyway would give an `allowlist`
    ///   sandbox no way out at all and a `full` one no denies, in both cases
    ///   silently; refusing routes it through the profile's own
    ///   `allow_unsandboxed_fallback` switch instead.
    pub fn validate(&self) -> SandboxResult<()> {
        let refuse = |detail: String| SandboxError::Refused {
            profile: self.policy.profile.clone(),
            detail,
        };
        crate::sandbox::dirs::check_writable_roots(&self.writable_paths(), self.friring_db)
            .map_err(&refuse)?;
        let declared: Vec<String> = self
            .policy
            .rw_paths
            .iter()
            .chain(self.policy.ro_paths.iter())
            .cloned()
            .collect();
        crate::sandbox::dirs::check_declared_paths(&declared).map_err(&refuse)?;
        crate::sandbox::dirs::check_engine_socket_paths(&declared, Some(self.home))
            .map_err(&refuse)?;
        if crate::sandbox::egress::proxy_required(self.policy) && self.proxy.is_none() {
            let denied: Vec<String> = self.policy.deny.iter().map(|r| r.to_string()).collect();
            let what = if self.policy.network == NetworkMode::Full {
                format!(
                    "the domain denies it carries ({}) are enforced by that proxy and by nothing \
                     else, so 'full' would run with them silently inert",
                    denied.join(", ")
                )
            } else {
                "an 'allowlist' sandbox reaches its allowed domains only through that proxy, so \
                 it would run with no way out at all"
                    .to_string()
            };
            return Err(refuse(format!(
                "network mode '{}' is enforced by the friring egress proxy, and none is running \
                 for this launch: {what}",
                self.policy.network
            )));
        }
        Ok(())
    }

    /// The profile's read-only paths, minus any the launch made writable.
    /// Mirrors [`SandboxPolicy`]'s own rule that the two sets stay disjoint and
    /// the wider grant wins.
    pub fn readable_paths(&self) -> Vec<String> {
        let writable = self.writable_paths();
        let mut out: Vec<String> = self
            .policy
            .ro_paths
            .iter()
            .filter(|p| !writable.contains(p))
            .cloned()
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// One isolation technology.
///
/// A backend implements [`wrap`](Self::wrap) **or** [`ensure`](Self::ensure),
/// never both: the default bodies fail with the shape mismatch, so forgetting
/// to implement the right half is a loud error rather than a silent no-op.
pub trait SandboxBackend {
    /// Which backend this is. Also decides which half of the trait is live.
    fn kind(&self) -> SandboxBackendKind;

    /// Is this usable on the host this backend was built for, and if not, why?
    ///
    /// Cheap and cached: implementations probe once and reuse the answer, so
    /// the UI may call this while painting. The host is injected when the
    /// backend is constructed (see [`crate::sandbox::probe::ProbeHost`]), which
    /// is also what makes the probe testable with no tool installed.
    fn probe(&self) -> Availability;

    /// What the UI may offer for this backend.
    fn capabilities(&self) -> Caps;

    /// Policy backends: argv in, wrapped argv out.
    ///
    /// Applied where the invocation is composed, *before* per-transport
    /// composition — the transports differ in how they quote and fold a command
    /// line, so wrapping at the shell-string level would not survive all of
    /// them (`docs/SANDBOX.md` §Launch integration).
    ///
    /// [`SandboxPolicy::env`] is **not** applied here. A policy backend is a
    /// rule applied to a process, not a new environment, so the wrapped process
    /// inherits the tmux window's environment; the launch path sets it there
    /// once for sandboxed and unsandboxed sessions alike.
    fn wrap(&self, argv: Argv, launch: &SandboxLaunch<'_>) -> SandboxResult<Argv> {
        let _ = (argv, launch);
        Err(self.wrong_half())
    }

    /// Place backends: make sure the environment exists and is running.
    fn ensure(&self, profile: &SandboxProfile) -> SandboxResult<SandboxInstance> {
        let _ = profile;
        Err(self.wrong_half())
    }

    /// The error for calling a half this backend does not implement: a shape
    /// mismatch for a policy backend asked to be a place, and "not built yet"
    /// for a place backend that implements neither — `apple-container` and
    /// `wsl-distro`, which land in P4.
    fn wrong_half(&self) -> SandboxError {
        let kind = self.kind();
        match kind.shape() {
            Some(SandboxShape::Place) => SandboxError::NotInThisStage {
                backend: kind,
                detail: "place backends (containers, VMs, distro clones) are reached through the \
                         sandbox transport, and this build has no implementation of that one"
                    .to_string(),
            },
            _ => SandboxError::WrongShape {
                backend: kind,
                shape: SandboxShape::Policy,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SandboxPath, SandboxProfile};

    fn policy() -> SandboxPolicy {
        SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
            ],
        )
        .resolve(SandboxBackendKind::Bwrap, "/home/u")
        .unwrap()
    }

    /// A place backend that implements neither half, to exercise the defaults.
    struct FakePlace;

    impl SandboxBackend for FakePlace {
        fn kind(&self) -> SandboxBackendKind {
            SandboxBackendKind::Docker
        }
        fn probe(&self) -> Availability {
            Availability::available("")
        }
        fn capabilities(&self) -> Caps {
            Caps {
                shape: SandboxShape::Place,
                limits: true,
                network_modes: NetworkMode::ALL,
                read_scopes: &[ReadScope::Workspace],
                persistent: true,
                host_credentials: false,
                inner_agent_sandbox: InnerSandboxVerdict::Redundant,
                proxy_transport: ProxyTransport::UnixSocket,
            }
        }
    }

    /// A proxy endpoint standing in for a running instance, so a test can build
    /// the launches a filtered mode now requires.
    fn endpoint() -> ProxyEndpoint {
        ProxyEndpoint::Loopback { port: 8123 }
    }

    #[test]
    fn availability_messages_carry_the_fix() {
        assert!(Availability::available("bubblewrap 0.11.0").is_available());
        assert_eq!(
            Availability::available("bubblewrap 0.11.0").message(),
            "bubblewrap 0.11.0"
        );
        let none = Availability::unavailable("seatbelt is macOS-only");
        assert!(!none.is_available());
        assert_eq!(none.message(), "seatbelt is macOS-only");
        let fixable = Availability::needs_fix(
            "unprivileged user namespaces are disabled",
            "sudo sysctl -w kernel.unprivileged_userns_clone=1",
        );
        assert_eq!(
            fixable.to_string(),
            "unprivileged user namespaces are disabled — sudo sysctl -w \
             kernel.unprivileged_userns_clone=1"
        );
    }

    #[test]
    fn a_place_backend_reports_its_half_as_unbuilt() {
        let policy = policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let err = FakePlace.wrap(vec!["claude".into()], &launch).unwrap_err();
        assert!(matches!(err, SandboxError::NotInThisStage { .. }));
        assert!(err.to_string().contains("place backends"));
        // The other half is equally unimplemented, but says so as "not built"
        // rather than pretending to have ensured anything.
        let profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        assert!(FakePlace.ensure(&profile).is_err());
    }

    #[test]
    fn launch_merges_session_paths_into_the_writable_set() {
        let policy = policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_workspace("/home/u/dev/app")
            .with_signal_dir("/home/u/.local/share/friring/signals/s1")
            .with_tmp_dir("/tmp/friring-s1");
        // The workspace duplicates a profile path and collapses into it; the
        // result is sorted so an ancestor precedes its descendants.
        assert_eq!(
            launch.writable_paths(),
            [
                "/home/u/.local/share/friring/signals/s1",
                "/home/u/dev/app",
                "/tmp/friring-s1",
            ]
        );
        assert_eq!(launch.readable_paths(), ["/srv/shared"]);
    }

    #[test]
    fn launch_never_leaves_a_path_in_both_sets() {
        // A read-only profile path that the session makes its workspace is
        // writable: the wider grant wins, exactly as `resolve` decides it.
        let policy = SandboxProfile::new("dev", vec![SandboxPath::read_only("/srv/shared")])
            .resolve(SandboxBackendKind::Seatbelt, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_workspace("/srv/shared");
        assert_eq!(launch.writable_paths(), ["/srv/shared"]);
        assert!(launch.readable_paths().is_empty());
    }

    /// The complete escape this whole shape exists to prevent: friring's tmux
    /// server listens on a pathname unix socket under the host temp root, and no
    /// backend's network isolation stops `connect(2)` on one. Nothing a launch
    /// mints may therefore be — or contain — either directory.
    #[test]
    fn no_profile_shape_makes_the_host_temp_root_or_a_tmux_socket_writable() {
        let scratch = crate::sandbox::dirs::session_scratch_dir("s1")
            .unwrap()
            .display()
            .to_string();
        let temp_root = crate::sandbox::dirs::host_temp_root().display().to_string();
        let socket_root = crate::sandbox::dirs::tmux_socket_root()
            .display()
            .to_string();
        let mut profile = SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
            ],
        );
        for scope in ReadScope::ALL {
            for network in NetworkMode::ALL {
                profile.read_scope = *scope;
                profile.network_mode = *network;
                for backend in [SandboxBackendKind::Seatbelt, SandboxBackendKind::Bwrap] {
                    let policy = profile.resolve(backend, "/home/u").unwrap();
                    let launch = SandboxLaunch::new(&policy, "/home/u", "s1")
                        .with_workspace("/home/u/dev/app")
                        .with_signal_dir("/home/u/.local/share/friring/signals/s1")
                        .with_tmp_dir(&scratch)
                        .with_friring_db("/home/u/.local/share/friring/friring.db")
                        .with_proxy(endpoint());
                    let writable = launch.writable_paths();
                    for forbidden in [&temp_root, &socket_root] {
                        assert!(
                            !writable.contains(forbidden),
                            "{backend}/{scope}/{network} granted {forbidden}: {writable:?}"
                        );
                    }
                    launch.validate().unwrap();
                }
            }
        }
    }

    #[test]
    fn a_writable_root_that_encloses_the_database_is_refused() {
        let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("~")])
            .resolve(SandboxBackendKind::Seatbelt, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_friring_db("/home/u/.local/share/friring/friring.db");
        let err = launch.validate().unwrap_err();
        assert!(matches!(err, SandboxError::Refused { .. }));
        let text = err.to_string();
        assert!(text.contains("/home/u/.local/share/friring"), "{text}");
        assert!(text.contains("ADR-29"), "{text}");
        // A profile that stays out of the data directory launches.
        let ok = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")])
            .resolve(SandboxBackendKind::Seatbelt, "/home/u")
            .unwrap();
        SandboxLaunch::new(&ok, "/home/u", "s1")
            .with_friring_db("/home/u/.local/share/friring/friring.db")
            .with_proxy(endpoint())
            .validate()
            .unwrap();
    }

    /// A **read-only** path may reach neither friring's own sandbox state nor a
    /// container engine's control socket — the two grants that are taken by
    /// being readable, so `check_writable_roots` is the wrong gate for both.
    #[test]
    fn a_read_only_path_may_not_reach_other_sandboxes_state_or_the_engine_socket() {
        let sandbox_state = crate::sandbox::dirs::sandbox_root()
            .unwrap()
            .join("pl")
            .display()
            .to_string();
        for (path, expected) in [
            // Another profile's synthetic home holds the login it signed in
            // with, and the seatbelt policy beside it is what constrains this
            // sandbox (ADR-28).
            (sandbox_state.as_str(), "friring's own"),
            // The socket is the whole of the engine's authorisation, and a
            // read-only bind does not take write permission off an inode.
            ("/var/run/docker.sock", "control socket"),
        ] {
            let policy = SandboxProfile::new(
                "dev",
                vec![
                    SandboxPath::workspace("~/dev/app"),
                    SandboxPath::read_only(path),
                ],
            )
            .resolve(SandboxBackendKind::Seatbelt, "/home/u")
            .unwrap();
            let err = SandboxLaunch::new(&policy, "/home/u", "s1")
                .with_proxy(endpoint())
                .validate()
                .expect_err(&format!("'{path}' must not be grantable read-only"));
            assert!(matches!(err, SandboxError::Refused { .. }), "{err}");
            let text = err.to_string();
            assert!(text.contains(expected), "{path}: {text}");
            assert!(text.contains(path), "{path}: {text}");
        }
        // The launch's *own* minted directories live in exactly that tree and
        // are the point of it, so they are not what this refuses.
        let scratch = crate::sandbox::dirs::session_scratch_dir("s1")
            .unwrap()
            .display()
            .to_string();
        SandboxLaunch::new(&policy(), "/home/u", "s1")
            .with_tmp_dir(&scratch)
            .with_proxy(endpoint())
            .validate()
            .unwrap();
    }

    /// A mode the kernel cannot express on its own is refused unless the proxy
    /// that *can* express it is running — and is honoured once it is.
    ///
    /// The hole this closes is the one P1 papered over by refusing `full` with
    /// denies outright: a rule the user wrote, carried by a launch, enforced by
    /// nothing. The answer is now the proxy rather than a refusal, and the
    /// refusal moved to the case where there is no proxy to enforce it with.
    #[test]
    fn a_filtered_mode_needs_the_proxy_that_enforces_it() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_mode = NetworkMode::Full;
        profile.network_deny = vec!["evil.example".into()];
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let err = SandboxLaunch::new(&policy, "/home/u", "s1")
            .validate()
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("evil.example"), "{text}");
        assert!(text.contains("egress proxy"), "{text}");
        // With one running, the denies are enforceable and `full` no longer
        // means unrestricted: everything goes through the proxy.
        let proxied = SandboxLaunch::new(&policy, "/home/u", "s1").with_proxy(endpoint());
        proxied.validate().unwrap();
        assert_eq!(proxied.egress(), Egress::Proxied(&endpoint()));

        // An allowlist is the same bargain, and says so in its own words.
        profile.network_mode = NetworkMode::Allowlist;
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let err = SandboxLaunch::new(&policy, "/home/u", "s1")
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("no way out at all"), "{err}");
        SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_proxy(endpoint())
            .validate()
            .unwrap();

        // `full` with nothing to take back is the one mode that needs no help,
        // and `none` has nothing to reach.
        profile.network_mode = NetworkMode::Full;
        profile.network_deny.clear();
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let open = SandboxLaunch::new(&policy, "/home/u", "s1");
        open.validate().unwrap();
        assert_eq!(open.egress(), Egress::Open);

        profile.network_mode = NetworkMode::None;
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let closed = SandboxLaunch::new(&policy, "/home/u", "s1");
        closed.validate().unwrap();
        assert_eq!(closed.egress(), Egress::Closed);
    }

    /// A launch built by hand, with a filtered mode and no endpoint, must read
    /// as *closed* rather than open — the direction that turns a missing proxy
    /// into a sandbox with no network instead of one with unfiltered network.
    #[test]
    fn a_filtered_mode_without_a_proxy_reads_as_closed() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        for (mode, deny) in [
            (NetworkMode::Allowlist, Vec::new()),
            (NetworkMode::Full, vec!["evil.example".to_string()]),
        ] {
            profile.network_mode = mode;
            profile.network_deny = deny;
            let policy = profile
                .resolve(SandboxBackendKind::Seatbelt, "/home/u")
                .unwrap();
            assert_eq!(
                SandboxLaunch::new(&policy, "/home/u", "s1").egress(),
                Egress::Closed,
                "{mode} without a proxy must not read as open"
            );
        }
    }

    #[test]
    fn inner_sandbox_verdicts_explain_themselves() {
        assert_eq!(
            InnerSandboxVerdict::Denied.reason(),
            "nested sandbox policies are denied by the kernel"
        );
        assert_eq!(
            InnerSandboxVerdict::Redundant.reason(),
            "Friring is the boundary"
        );
    }
}
