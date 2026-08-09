//! The seam every isolation technology plugs into.
//!
//! [`SandboxBackend`] is deliberately small: probe the host, declare what the
//! UI may offer, and then do exactly one of two things — wrap an argv (policy
//! backends) or ensure an environment exists (place backends). ADR-26 in
//! `docs/SANDBOX.md` explains why those are the only two shapes.

use std::fmt;

use crate::session::{
    NetworkMode, ReadScope, SandboxBackendKind, SandboxPolicy, SandboxProfile, SandboxShape,
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
    /// Writing or reading a generated artefact failed.
    Io { path: String, detail: String },
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
}

/// Where the friring egress proxy listens, for the one hole a sandbox with
/// [`NetworkMode::Allowlist`] is allowed to keep (ADR-27).
///
/// Both spellings exist because the backends differ in what they can reach: a
/// seatbelt process still shares the host's network stack and can dial
/// loopback, while a `--unshare-net` bwrap sandbox has its own empty stack and
/// can only be handed a socket. P2 builds the proxy; P1 passes `None` and every
/// backend then treats [`NetworkMode::Allowlist`] exactly like
/// [`NetworkMode::None`], which grants nothing.
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

/// One live place: a container, a VM, a distro clone.
///
/// Minimal on purpose — P3 owns instance lifecycle and the `sandbox_instances`
/// table, and may well move this type into `storage` when it adds the state and
/// timestamp columns. It exists here so [`SandboxBackend::ensure`] has a
/// signature to be declared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInstance {
    /// The profile the place was built for.
    pub profile: String,
    /// Which engine created it.
    pub engine: SandboxBackendKind,
    /// The engine's own handle: a container id, a distro name.
    pub external_id: String,
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
    /// Temp directory the agent may write. A CLI that cannot write a temp file
    /// dies on startup, so this is effectively required in practice.
    pub tmp_dir: Option<&'a str>,
    /// Absolute path of friring's SQLite database, to be denied explicitly
    /// (ADR-29). Passed in rather than resolved here because the database of
    /// the friring that owns the session is not necessarily the one on the host
    /// where the agent runs.
    pub friring_db: Option<&'a str>,
    /// The egress proxy, once P2 runs one.
    pub proxy: Option<ProxyEndpoint>,
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

    /// The writable temp directory.
    pub fn with_tmp_dir(mut self, dir: &'a str) -> Self {
        self.tmp_dir = Some(dir);
        self
    }

    /// The database path to deny (ADR-29).
    pub fn with_friring_db(mut self, db: &'a str) -> Self {
        self.friring_db = Some(db);
        self
    }

    /// The egress proxy P2 starts alongside the session.
    pub fn with_proxy(mut self, proxy: ProxyEndpoint) -> Self {
        self.proxy = Some(proxy);
        self
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

    /// The error for calling the half this backend does not implement: a shape
    /// mismatch for a backend of the other shape, and "not built yet" for a
    /// place backend, whose half lands in P3.
    fn wrong_half(&self) -> SandboxError {
        let kind = self.kind();
        match kind.shape() {
            Some(SandboxShape::Place) => SandboxError::NotInThisStage {
                backend: kind,
                detail: "place backends (containers, VMs, distro clones) land with the sandbox \
                         transport; this build has the policy backends only"
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
            }
        }
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
