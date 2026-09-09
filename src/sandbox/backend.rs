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
/// `<root>/.git/config` is deliberately *not* protected in an ordinary
/// **checkout**, despite being a comparable channel (`core.pager`,
/// `core.fsmonitor` and aliases all name commands): ordinary work inside a
/// sandbox writes it — `git config`, `git remote add` — and a profile that
/// breaks `git` gets turned off, which protects nothing. It *is* protected when
/// the writable root is a bare git directory; see [`PROTECTED_IN_GIT_DIR`].
pub const PROTECTED_IN_WRITABLE_ROOT: &str = ".git/hooks";

/// What is taken back inside a writable root that **is a git directory** —
/// `<repo>/.git`, the shape a profile shares with bridge children so they can
/// reach the object and ref stores a linked worktree makes them share
/// (`docs/SANDBOX.md` §What a shared git directory does and does not give away).
///
/// Every entry is a way to run a command on the **host**, or the state of a
/// worktree that is not the writer's:
///
/// - `hooks` — run by whichever git touches the repository next, including the
///   operator's, outside the boundary. The real hooks of a git directory are at
///   `<root>/hooks`, one level up from where a checkout keeps them.
/// - `config` and `config.worktree` — `core.fsmonitor` and `core.pager` name
///   programs git executes, `core.hooksPath` moves the hooks somewhere writable,
///   and aliases run anything. Unlike a checkout's own config this one is the
///   *shared* repository's, so writing it reaches every sibling and the
///   operator. Nothing a child legitimately does writes it.
/// - `commondir` — git honours one in **any** git directory, including a main
///   checkout's, and it decides which directory `config` is then read from. A
///   writable one is the config protection above with an extra step.
/// - `HEAD` and `index` — the **main** worktree's, i.e. the leader's staging
///   area and checked-out branch. A child works in a linked worktree, whose
///   `HEAD` and `index` are per-worktree and stay writable.
///
/// A profile that grants a bare `.git` as a writable root to a session that
/// commits in the *main* worktree therefore breaks that session's commits. That
/// shape has no reason to exist — grant the checkout, which encloses its git
/// directory — and the alternative is a shared grant that silently lets one
/// child rewrite what another is committing.
pub const PROTECTED_IN_GIT_DIR: [&str; 6] = [
    "hooks",
    "config",
    "config.worktree",
    "commondir",
    "HEAD",
    "index",
];

/// What is taken back inside a writable root that is **one worktree's
/// metadata** — `<repo>/.git/worktrees/<id>`, which ADR-31 grants a bridge child
/// so it can commit at all.
///
/// All three are redirects, and a redirect is how a writable metadata directory
/// becomes host command execution: `commondir` decides which directory git
/// reads `config` (and so `core.fsmonitor`) from, `gitdir` decides which
/// worktree the entry belongs to, and `config.worktree` is read directly when
/// the shared config enables it. friring itself runs `git` in a child's worktree
/// to reach a verdict, so a child that could point any of them at a file it
/// wrote would be running commands as the host.
///
/// The rest of the directory — `HEAD`, `index`, `logs`, `refs` — is the child's
/// own and stays writable, because a child that cannot write them cannot commit.
pub const PROTECTED_IN_WORKTREE_METADATA: [&str; 3] = ["gitdir", "commondir", "config.worktree"];

/// Protected names that git does not always create — so friring creates them
/// **empty** before a launch, and every backend has something to take back.
///
/// Only seatbelt denies by pathname, which covers a path that does not exist
/// yet. bwrap binds and a container mounts, and neither can protect a name with
/// nothing behind it: `--ro-bind-try` skips a missing source, and a place mounts
/// only what exists, because an engine asked to bind a missing one invents a
/// root-owned file inside somebody's repository. Left absent,
/// `config.worktree` is therefore a name a child can *create* — and a repository
/// with `extensions.worktreeConfig` already enabled (`git sparse-checkout` turns
/// it on) reads `core.fsmonitor` out of what the child wrote, the next time the
/// host runs git in that worktree.
///
/// Every name here has to be inert when empty, which is why it is one name and
/// not three: an empty `commondir` would tell git the common directory is `""`.
pub const PROTECTED_CREATED_IF_ABSENT: [&str; 1] = ["config.worktree"];

/// Every path that has to be taken back inside one writable root.
///
/// Three shapes, because a writable root is not always a checkout: an ordinary
/// checkout ([`PROTECTED_IN_WRITABLE_ROOT`]), a git directory
/// ([`PROTECTED_IN_GIT_DIR`]) and one worktree's metadata directory
/// ([`PROTECTED_IN_WORKTREE_METADATA`]). Decided on the path's **shape** rather
/// than by looking at the filesystem, so a profile reads the same wherever it is
/// generated; costs nothing when a root is none of them, because denying writes
/// to a path that does not exist is a no-op.
pub fn protected_paths_in(root: &str) -> Vec<String> {
    let mut out = vec![format!("{root}/{PROTECTED_IN_WRITABLE_ROOT}")];
    out.extend(
        protected_names_in(root)
            .iter()
            .map(|name| format!("{root}/{name}")),
    );
    out
}

/// The names a root's shape takes back inside it, beyond
/// [`PROTECTED_IN_WRITABLE_ROOT`] — and the answer
/// [`app::bridge_spawn::ensure_protected_placeholders`] acts on directly.
///
/// Read as **path components** rather than by splitting on `'/'`. A root
/// arrives spelled the way its host writes it, and where that is `\` a
/// `/`-split found no component it recognised and answered "no shape" — so
/// every git directory looked like an ordinary one and nothing was taken back.
/// Native Windows is offered no sandbox backend at all
/// ([`crate::sandbox::select::NATIVE_WINDOWS`]), so nothing reached that
/// answer; it is fixed rather than documented because the next backend would
/// inherit it silently.
///
/// [`app::bridge_spawn::ensure_protected_placeholders`]: crate::app
pub fn protected_names_in(root: &str) -> &'static [&'static str] {
    fn named(path: Option<&std::path::Path>) -> Option<&str> {
        path.and_then(std::path::Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
    }
    let path = std::path::Path::new(root);
    let parent = path.parent();
    if named(Some(path)) == Some(".git") {
        &PROTECTED_IN_GIT_DIR
    } else if named(parent) == Some("worktrees")
        && named(parent.and_then(std::path::Path::parent)) == Some(".git")
    {
        &PROTECTED_IN_WORKTREE_METADATA
    } else {
        &[]
    }
}

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
    /// Whether this backend can carry the orchestration bridge (ADR-30).
    ///
    /// The bridge is a directory friring mints on the host and exposes inside
    /// the boundary, and its **authority** is that friring exposed exactly one
    /// per session. A backend that cannot give a session a private directory
    /// friring writes — every place backend, every remote host — cannot carry
    /// that, so a bridge-required agent is refused there rather than launched
    /// with a channel nobody is serving.
    ///
    /// Declared per backend rather than inferred from the shape, so a backend
    /// that grows the capability says so where its other primitives are.
    pub bridge: bool,
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
    /// Per-session bridge queue (ADR-30's file channel). Writable, and present
    /// exactly when the session's profile grants a bridge capability.
    ///
    /// Writable because the whole channel is the agent writing a request file
    /// and reading an answer; which directory a request arrives in *is* the
    /// caller's identity, so exposing one session's is exposing its authority
    /// and no other's.
    pub bridge_dir: Option<&'a str>,
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
    /// The launch gate this session waits on, exposed **read-only** (ADR-33).
    ///
    /// `None` for an ordinary launch, which starts its agent immediately. A
    /// gated launch runs `friring-cli sandbox launch` in the pane instead, and
    /// that helper polls this directory for the release file the host renames
    /// in. Read-only is the whole proof: the helper only has to *see* a regular
    /// file, and a policy that grants reads and nothing else means no process
    /// inside the boundary can create, rename or unlink one.
    pub gate_dir: Option<&'a str>,
    /// The content the release file must hold for this launch to proceed.
    ///
    /// Not a credential — the gate directory is unwritable from inside, so
    /// nothing in the boundary could forge one anyway. What it buys is that a
    /// release file left behind by an earlier launch of the same child cannot
    /// open this one: the host mints a fresh key per launch, so a stale file is
    /// a mismatch rather than a gate that is already open.
    pub gate_key: Option<&'a str>,
    /// The **child narrowing** this launch runs under, when it is a bridge
    /// child's (ADR-31).
    ///
    /// Two halves the backends need beyond the policy itself: the paths to deny
    /// after **every** allow, whatever encloses them, and the exact seed targets
    /// to re-grant as the last word for those paths alone. A parent profile that
    /// grants the whole home directory encloses the family's state directory, so
    /// the subtraction cannot be expressed as an absent grant — it has to be a
    /// deny that comes after.
    ///
    /// `None` for every launch that is not a bridge child's, which is every
    /// ordinary session.
    pub narrowing: Option<&'a crate::session::SandboxOverlay>,
    /// friring's own CLI, which every policy launch runs inside the boundary as
    /// its launch helper (ADR-33).
    ///
    /// Set by the backend from its own resolution, so [`render_profile`] and
    /// [`build_argv`] stay pure functions of the launch. It has to be *readable*
    /// inside the boundary or the helper cannot start at all, which is why it is
    /// a launch fact rather than something either renderer looks up.
    ///
    /// [`render_profile`]: crate::sandbox::seatbelt::render_profile
    /// [`build_argv`]: crate::sandbox::bwrap::build_argv
    pub helper_program: Option<&'a str>,
    /// The **agent's** own program, when it is an absolute path.
    ///
    /// Read into the narrow scope for exactly the reason
    /// [`helper_program`](Self::helper_program) is: `workspace` grants "the
    /// profile's own paths, plus the system directories a binary needs in order
    /// to load and run", and an agent whose program lives outside both is a pane
    /// that dies before the agent starts. An extension that ships its agent as a
    /// script under its own home is the ordinary case — both of this fork's
    /// bridge-backed extensions do — and no operator should have to add it to
    /// every profile by hand.
    ///
    /// `None` for a bare command name, which resolves on `PATH` under the system
    /// directories the scope already allows, and for a place backend, whose
    /// filesystem is the place's own.
    pub agent_program: Option<&'a str>,
    /// The multiplexer sockets on this host, for the closed deny set every
    /// policy launch renders
    /// ([`crate::sandbox::dirs::multiplexer_socket_denies`]).
    ///
    /// `None` only where friring could not work them out, which leaves the deny
    /// set empty rather than guessed — the launch paths always supply them.
    pub host_mux: Option<&'a crate::session::HostMuxSockets>,
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
            bridge_dir: None,
            tmp_dir: None,
            friring_db: None,
            gate_dir: None,
            gate_key: None,
            narrowing: None,
            helper_program: None,
            agent_program: None,
            host_mux: None,
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

    /// The per-session bridge queue — see [`bridge_dir`](Self::bridge_dir).
    pub fn with_bridge_dir(mut self, dir: &'a str) -> Self {
        self.bridge_dir = Some(dir);
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

    /// The read-only launch gate and the key that opens it — see
    /// [`gate_dir`](Self::gate_dir).
    pub fn with_gate(mut self, dir: &'a str, key: &'a str) -> Self {
        self.gate_dir = Some(dir);
        self.gate_key = Some(key);
        self
    }

    /// The gate as the launch helper wants it, or `None` for an ungated launch.
    ///
    /// Both halves or neither: a directory with no key would let a stale release
    /// file from a previous launch open this one, and a key with no directory
    /// names nothing to poll.
    pub fn gate(&self) -> Option<(&'a str, &'a str)> {
        self.gate_dir.zip(self.gate_key)
    }

    /// The child narrowing — see [`narrowing`](Self::narrowing).
    pub fn with_narrowing(mut self, overlay: &'a crate::session::SandboxOverlay) -> Self {
        self.narrowing = Some(overlay);
        self
    }

    /// The paths this launch denies after every allow, and the seed targets it
    /// re-grants after those.
    ///
    /// `(subtract, seeds)`. Empty for every launch that is not a bridge child's.
    pub fn subtract(
        &self,
    ) -> (
        Vec<crate::session::SubtractPath>,
        Vec<crate::session::SeedGrant>,
    ) {
        match self.narrowing {
            Some(overlay) => (overlay.subtract.clone(), overlay.seed.clone()),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// A bridge child's **own** directories, which the subtract set denies
    /// wholesale and every backend must therefore re-grant after it.
    ///
    /// Returns `(read-write, read-only)`. The subtract set denies friring's
    /// trees — `<data>/worktrees`, `<data>/signals`, `<data>/sandbox`,
    /// `<data>/gates` — because that is what covers every sibling, including one
    /// created after this child launched. The child's own worktree, scratch,
    /// signal directory, bridge queue and private state live inside those trees,
    /// so a renderer that emitted the denies and stopped would have built a
    /// boundary in which the child cannot read its own gate, write its own
    /// workspace, report its own status or reach its own queue.
    ///
    /// The gate is the read-only half and must stay there: a writable gate is a
    /// gate the child can open for itself (ADR-33).
    ///
    /// Empty for a launch that is not a child, which is what makes calling this
    /// unconditionally safe.
    pub fn child_own_paths(&self) -> (Vec<String>, Vec<String>) {
        let Some(overlay) = self.narrowing else {
            return (Vec::new(), Vec::new());
        };
        let mut rw: Vec<String> = Vec::new();
        rw.extend(overlay.worktree.clone());
        rw.extend(overlay.own_dirs.iter().cloned());
        rw.extend(overlay.state_dir.clone());
        rw.sort();
        rw.dedup();
        let ro: Vec<String> = overlay.gate_dir.clone().into_iter().collect();
        (rw, ro)
    }

    /// friring's own CLI — see [`helper_program`](Self::helper_program).
    pub fn with_helper_program(mut self, program: &'a str) -> Self {
        self.helper_program = Some(program);
        self
    }

    /// The agent's own program — see [`agent_program`](Self::agent_program).
    ///
    /// A relative or bare command is ignored: it resolves on `PATH`, under the
    /// system directories every scope already allows, and turning it into a
    /// policy literal would grant a path relative to whatever the launch's cwd
    /// happens to be.
    pub fn with_agent_program(mut self, program: &'a str) -> Self {
        if program.starts_with('/') {
            self.agent_program = Some(program);
        }
        self
    }

    /// The host's multiplexer sockets — see [`host_mux`](Self::host_mux).
    pub fn with_host_mux(mut self, host: &'a crate::session::HostMuxSockets) -> Self {
        self.host_mux = Some(host);
        self
    }

    /// The closed multiplexer deny set for this launch, empty when friring could
    /// not work the host's sockets out.
    pub fn multiplexer_denies(&self) -> Vec<crate::sandbox::dirs::SocketDeny> {
        self.host_mux
            .map(crate::sandbox::dirs::multiplexer_socket_denies)
            .unwrap_or_default()
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
        for extra in [
            self.workspace,
            self.signal_dir,
            self.bridge_dir,
            self.tmp_dir,
        ]
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
    ///   of it. A **bridge child** is the one launch whose minted directories
    ///   are also policy paths, because [`crate::session::SandboxPolicy::narrow`]
    ///   has to put them there for the boundary to grant them; those exact paths
    ///   are taken back out here, since friring minted them for this child and
    ///   no profile declared them. Only the exact paths — anything else under
    ///   those trees is still refused, which is what keeps a child out of its
    ///   siblings'.
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
        let (own_rw, own_ro) = self.child_own_paths();
        let declared: Vec<String> = self
            .policy
            .rw_paths
            .iter()
            .chain(self.policy.ro_paths.iter())
            .filter(|path| !own_rw.contains(*path) && !own_ro.contains(*path))
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

    /// The profile's read-only paths, minus any the launch made writable, plus
    /// the launch gate.
    ///
    /// Mirrors [`SandboxPolicy`]'s own rule that the two sets stay disjoint and
    /// the wider grant wins. The gate is folded in here and **never** into
    /// [`writable_paths`](Self::writable_paths): a writable gate is a gate the
    /// agent releases itself.
    pub fn readable_paths(&self) -> Vec<String> {
        let writable = self.writable_paths();
        let mut out: Vec<String> = self
            .policy
            .ro_paths
            .iter()
            .map(String::as_str)
            .chain(self.gate_dir)
            .filter(|p| !writable.iter().any(|w| w == p))
            .map(str::to_string)
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
    /// for a place backend that implements neither — `wsl-distro`, the one that
    /// is still to come.
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

    /// The protected sets, spelled out here rather than read from the constants
    /// the renderers use.
    ///
    /// Every backend test iterates those constants, which is what keeps three
    /// renderers agreeing — and means a name **removed** from one would delete
    /// the assertions about it everywhere at once, silently. This is the one
    /// place the required names are stated independently, so a removal has to
    /// be argued for here.
    #[test]
    fn the_protected_sets_are_what_they_say_they_are() {
        assert_eq!(PROTECTED_IN_WRITABLE_ROOT, ".git/hooks");
        for name in [
            "hooks",
            "config",
            "config.worktree",
            "commondir",
            "HEAD",
            "index",
        ] {
            assert!(
                PROTECTED_IN_GIT_DIR.contains(&name),
                "a shared git directory no longer protects '{name}'"
            );
        }
        for name in ["gitdir", "commondir", "config.worktree"] {
            assert!(
                PROTECTED_IN_WORKTREE_METADATA.contains(&name),
                "a child's own git metadata no longer protects '{name}'"
            );
        }
        // And what a commit writes is in neither: a child that cannot write its
        // own `HEAD`, `index` or refs cannot commit, which friring then reads as
        // a dirty worktree and never merges.
        for name in ["objects", "refs", "logs", "packed-refs"] {
            assert!(!PROTECTED_IN_GIT_DIR.contains(&name));
        }
        for name in ["HEAD", "index", "logs", "refs"] {
            assert!(!PROTECTED_IN_WORKTREE_METADATA.contains(&name));
        }
    }

    /// Which shape a writable root is read as, on its path alone.
    #[test]
    fn a_roots_shape_decides_what_is_taken_back_inside_it() {
        let git_dir = protected_paths_in("/repo/.git");
        assert!(git_dir.contains(&"/repo/.git/config".to_string()));
        assert!(git_dir.contains(&"/repo/.git/hooks".to_string()));

        let metadata = protected_paths_in("/repo/.git/worktrees/child");
        assert!(metadata.contains(&"/repo/.git/worktrees/child/gitdir".to_string()));
        assert!(!metadata.contains(&"/repo/.git/worktrees/child/HEAD".to_string()));

        // An ordinary checkout keeps the narrow rule, so `git config` inside a
        // sandboxed session still works.
        let checkout = protected_paths_in("/repo");
        assert_eq!(checkout, vec!["/repo/.git/hooks".to_string()]);

        // The metadata *root* is neither: it is subtracted wholesale instead.
        assert_eq!(
            protected_paths_in("/repo/.git/worktrees"),
            vec!["/repo/.git/worktrees/.git/hooks".to_string()]
        );

        // A trailing slash must not change the reading.
        assert_eq!(
            protected_paths_in("/repo/.git/").len(),
            PROTECTED_IN_GIT_DIR.len() + 1
        );
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
                bridge: true,
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

    /// The one carve-out in `check_declared_paths`, from both sides.
    ///
    /// A bridge child is the single launch whose minted directories are also
    /// *policy* paths — `narrow` has to put them there for the boundary to grant
    /// them — so `validate` takes exactly those back out. Exactly: a sibling's
    /// directory under the same roots is what the protection is for, and a
    /// carve-out that widened to the trees rather than to the paths would hand a
    /// child every other child's gate and private state.
    #[test]
    fn a_childs_own_minted_directories_pass_validate_and_a_siblings_do_not() {
        let overlay = crate::session::SandboxOverlay {
            worktree: Some("/home/u/dev/app/child".into()),
            own_dirs: vec![
                crate::sandbox::dirs::session_scratch_dir("child")
                    .unwrap()
                    .display()
                    .to_string(),
                crate::paths::signals_directory()
                    .unwrap()
                    .join("child")
                    .display()
                    .to_string(),
            ],
            gate_dir: Some(
                crate::sandbox::dirs::gate_root()
                    .unwrap()
                    .join("child")
                    .display()
                    .to_string(),
            ),
            state_dir: Some(
                crate::sandbox::dirs::sandbox_root()
                    .unwrap()
                    .join("state")
                    .join("child")
                    .display()
                    .to_string(),
            ),
            ..crate::session::SandboxOverlay::default()
        };
        let narrowed = policy().narrow(&overlay).unwrap();
        SandboxLaunch::new(&narrowed, "/home/u", "child")
            .with_narrowing(&overlay)
            .with_proxy(endpoint())
            .validate()
            .expect("a child may reach the directories friring minted for it");

        // The same launch, plus one path under those roots that friring did not
        // mint for *this* child. It is not in `child_own_paths`, so nothing
        // exempts it and the refusal names it.
        let sibling = crate::sandbox::dirs::gate_root()
            .unwrap()
            .join("some-other-child")
            .display()
            .to_string();
        let mut with_sibling = narrowed.clone();
        with_sibling.ro_paths.push(sibling.clone());
        let err = SandboxLaunch::new(&with_sibling, "/home/u", "child")
            .with_narrowing(&overlay)
            .with_proxy(endpoint())
            .validate()
            .expect_err("a sibling's gate is not this child's own");
        let text = err.to_string();
        assert!(text.contains(&sibling), "{text}");
        assert!(text.contains("launch gate"), "{text}");
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
