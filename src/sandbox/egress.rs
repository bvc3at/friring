//! The egress boundary: the filtering proxy a sandboxed session reaches the
//! network through, and the lifetime friring keeps it for.
//!
//! ADR-27 in `docs/SANDBOX.md`: every backend denies *direct* egress at the
//! kernel level, and the domain list is enforced by a friring-owned proxy
//! outside the boundary. [`crate::proxy`] is that proxy — a leaf that knows
//! nothing about sessions. This module is the half that does: which transport a
//! backend can reach, when an instance starts and stops, and what environment
//! the agent needs in order to use it.
//!
//! # One proxy per session
//!
//! A policy is per *profile*, but an instance is per **session**, because the
//! other two things it owns are per launch: the bearer token and — for a
//! namespaced backend — the unix socket in that launch's scratch directory. So
//! a refusal names the session that provoked it, a first-use answer reaches the
//! session that was asked, an instance dies when its session does, and a
//! relaunch rotates one session's credential without touching anybody else's.
//! Duplicating the policy costs nothing, and one rule then holds on every
//! backend.
//!
//! An instance is bound *before* the agent launches, so nothing can race a
//! listener that is not bound yet — but it belongs to no session until the
//! launch it was composed for has a pane. Until that moment the session keeps
//! whatever it is already using, and a launch that fails anywhere in between
//! releases what it prepared rather than leaving a live credential behind:
//! [`prepare`] and [`PendingEgress`] are the two halves of that. Committing is
//! what replaces the previous instance, which every relaunch does, because
//! `Ctrl+R` re-derives the whole wrapper from the database and a profile edited
//! in between has to take effect. The last one is stopped where the session's
//! scratch directory is dropped, so a socket cannot outlive its session.
//!
//! # A place is the trust domain, not a session in it
//!
//! What an instance per session does **not** buy is isolation between the
//! sessions of a *place*. A place is created once per profile and shared by
//! that profile's sessions (ADR-26), and in there they run under one uid, in
//! one pid namespace, over one filesystem — so a sibling reads the proxy URL,
//! token and all, out of `/proc/<pid>/environ`, reaches every session's socket
//! in the place tree that is mounted for all of them, and dials any relay port
//! on the loopback they share. A first-use grant is theirs too, by that route
//! and by the profile the answer is written back to.
//!
//! Per-session isolation is therefore real for a policy backend, and for a
//! place holding one session; between siblings **in** a place it is not, and
//! nothing here, in the UI or in `docs/SANDBOX.md` may say otherwise. Two
//! agents that have to be kept apart get a profile each, which gives them a
//! place each — the lever the profile editor and list state where a profile is
//! chosen.
//!
//! # Where the instances live
//!
//! On a dedicated thread with its own single-threaded tokio runtime, addressed
//! by commands over a channel. Both halves of that are forced. The launch path is
//! synchronous and runs *inside* the TUI's own runtime — where
//! `Runtime::block_on` panics — as well as inside `friring-cli`, where there is
//! no runtime at all; and a proxy has to outlive the call that started it.
//!
//! What it does **not** outlive is the process. A sandboxed session created by
//! the short-lived `friring-cli` therefore starts with no way out: the agent
//! keeps running under tmux, its proxy dies with the CLI, and the kernel policy
//! denies everything else. That fails closed, and a relaunch from a running
//! friring restores egress.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use tokio::sync::mpsc;

use crate::proxy::{DenialEvent, HostRule, Policy, Proxy, ProxyBind, ProxyConfig};
use crate::sandbox::backend::{ProxyEndpoint, ProxyTransport, SandboxError, SandboxResult};
use crate::session::{NetworkMode, SandboxPolicy};

/// File name of the unix socket inside a session's scratch directory.
pub const PROXY_SOCKET_NAME: &str = "proxy.sock";

/// The name a launch takes while [`PROXY_SOCKET_NAME`] is still held by the
/// instance the session is using.
///
/// A relaunch binds its socket while the previous one is still serving the
/// agent that is running *now*, and two listeners cannot share a path —
/// [`Proxy::start`] refuses one that something is serving, which is what keeps
/// it from unlinking a live sibling. Two names are enough because only one
/// launch of a session is ever being prepared at a time; which of them a launch
/// gets is the supervisor's decision, since it is the only place that knows
/// what the session already holds.
pub const PROXY_SOCKET_ALT_NAME: &str = "proxy-b.sock";

/// The port the in-sandbox relay listens on, inside the sandbox's **own**
/// loopback.
///
/// A fixed port is safe precisely because the backends that need a relay are
/// the ones with their own network namespace: every sandbox has a private
/// `127.0.0.1`, so two sessions cannot collide, and nothing on the host is
/// bound here at all. The one collision left is an agent that binds this port
/// itself, which costs that sandbox its egress and nothing else.
pub const RELAY_PORT: u16 = 8118;

/// Longest unix socket path any supported platform accepts.
///
/// `sun_path` is 104 bytes on macOS and the BSDs and 108 on Linux, in both
/// cases including the terminating NUL. The smaller limit is used everywhere so
/// a path that works on one host works on all of them, and so the failure is a
/// sentence naming the fix rather than `bind: Invalid argument`.
const MAX_SOCKET_PATH: usize = 103;

/// `HTTP_PROXY` and its spellings. Both cases are set: plenty of tools read
/// only the lowercase one, and plenty only the uppercase.
pub const HTTP_PROXY_VARS: &[&str] = &["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"];

/// `ALL_PROXY` and its lowercase spelling — the SOCKS5 endpoint.
pub const SOCKS_PROXY_VARS: &[&str] = &["ALL_PROXY", "all_proxy"];

/// `NO_PROXY` and its lowercase spelling.
pub const NO_PROXY_VARS: &[&str] = &["NO_PROXY", "no_proxy"];

/// Destinations the agent must reach *without* the proxy: its own loopback.
///
/// A sandboxed agent's local traffic — a dev server it started, a language
/// server, a test fixture — has no business being tunnelled through a filter
/// that would refuse it for not being in a domain allowlist. For a namespaced
/// sandbox this is a boundary rule rather than a convenience: `127.0.0.1` means
/// something different on each side, so a tunnelled local request would be
/// dialled by the proxy on the *host's* loopback, where the services are not
/// the agent's own.
///
/// The consequence under a filtered mode is that local traffic is decided by
/// the kernel policy rather than by the allowlist, and both backends refuse it:
/// seatbelt opens the proxy port and nothing else, and a namespaced sandbox has
/// only what it started itself.
///
/// This is a convention the agent's own HTTP client honours, so it is a
/// *convenience* and never the boundary. What makes the boundary hold is that
/// the proxy refuses a host-local destination itself
/// ([`crate::proxy::host_is_local`]): a client that ignores `NO_PROXY` and
/// tunnels `127.0.0.1` reaches friring's refusal rather than the host's own
/// services. Reaching one on purpose takes the literal address in the profile's
/// allow list.
pub const NO_PROXY_VALUE: &str = "localhost,127.0.0.1,::1";

/// Whether this policy can only be honoured with the proxy running.
///
/// - [`NetworkMode::Allowlist`] always: the allowed domains are reachable
///   *only* through the proxy, since the kernel policy blocks direct egress.
/// - [`NetworkMode::Full`] **when it carries denies**: no kernel policy has a
///   host-name predicate, so a deny list under `full` is enforceable only by
///   proxying everything. `full` with no denies is the one mode that means
///   what it says without help.
/// - [`NetworkMode::None`] never: there is nothing to reach.
pub fn proxy_required(policy: &SandboxPolicy) -> bool {
    match policy.network {
        NetworkMode::None => false,
        NetworkMode::Allowlist => true,
        NetworkMode::Full => !policy.deny.is_empty(),
    }
}

/// The address the relay offers *inside* a namespaced sandbox.
pub fn relay_addr() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, RELAY_PORT))
}

/// A running proxy, as the launch that asked for it sees it.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyGrant {
    /// The one hole the backend's kernel policy must leave open.
    pub endpoint: ProxyEndpoint,
    /// The environment the agent needs in order to use it. Carries the
    /// instance's token inside the proxy URLs, which is why `Debug` prints the
    /// variable names and not their values.
    pub env: BTreeMap<String, String>,
}

impl fmt::Debug for ProxyGrant {
    /// Hand-written, because this is the type that actually *holds* the
    /// credential: the proxy URLs in [`ProxyGrant::env`] carry the instance's
    /// token, and a derived `Debug` puts them in every log line, panic message
    /// and `expect` failure that mentions a grant. The variable names are the
    /// diagnostic — which spellings a launch was given — and the values are the
    /// secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyGrant")
            .field("endpoint", &self.endpoint)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// One refusal, tagged with the session whose sandbox provoked it.
///
/// [`crate::proxy::DenyReason::NotAllowlisted`] is the one worth prompting on:
/// every other reason is an answer the user already gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDenial {
    /// The launch key the proxy was started under — friring's session id.
    pub session_key: String,
    pub event: DenialEvent,
}

/// A bound instance and the grant that names it, before either belongs to the
/// session. Returned by [`prepare`].
#[derive(Debug)]
pub struct Prepared {
    /// Where the sandbox reaches the proxy, and the environment that lets it —
    /// what the launch is composed against.
    pub grant: ProxyGrant,
    /// The instance itself, provisional until the launch commits it.
    pub pending: PendingEgress,
}

/// One launch's claim on the instance [`prepare`] bound for it.
///
/// A proxy is composed into an invocation long before that invocation is
/// running: argv has to name the port or the socket, so the listener exists
/// first. Everything between the two can fail — a wrapper that will not
/// compose, a pane that will not spawn, a row that will not persist — and none
/// of those failures may cost the session the boundary it is *already* using,
/// nor leave a listener with no session behind it.
///
/// So the instance is nobody's until this handle says otherwise:
///
/// - [`commit`](Self::commit) hands it to the session, retiring whatever it
///   replaced. Only the code that has seen the pane exist calls it.
/// - Dropping it shuts the new instance down — listener, token and socket —
///   and leaves the session's own untouched.
///
/// The launch path holds one for the whole of its fallible stretch, so *every*
/// early return releases it without having to remember to.
#[derive(Debug)]
pub struct PendingEgress {
    /// The session this launch is for. Kept whatever the outcome, because
    /// [`park`](Self::park) is keyed on it.
    key: String,
    outcome: Outcome,
}

/// What committing a launch does to the session's egress instance.
#[derive(Debug)]
enum Outcome {
    /// Nothing: this launch neither bound an instance nor asked for the
    /// session's to go.
    Keep,
    /// Take over. The identifier is which pending instance is this launch's, so
    /// a commit or a release names the one it prepared rather than whatever is
    /// pending now.
    Take(u64),
    /// Stop the instance the session is still using, and put nothing in its
    /// place: this launch needs no proxy at all.
    Clear,
}

impl PendingEgress {
    /// A handle with nothing to commit: this launch prepared no instance and
    /// wants none of the session's stopped.
    fn none(session_key: &str) -> Self {
        Self {
            key: session_key.to_string(),
            outcome: Outcome::Keep,
        }
    }

    /// A launch that needs no proxy — its profile was removed, or its network
    /// mode is one the kernel enforces on its own.
    ///
    /// Committing stops whatever the session is still using, so a profile
    /// edited from `allowlist` to `full` does not leave the previous launch's
    /// listener behind. Deferring that to the commit is the same rule as
    /// everywhere else here: until the relaunch has a pane, the agent running
    /// now is still the one the boundary belongs to.
    pub fn clearing(session_key: &str) -> Self {
        Self {
            key: session_key.to_string(),
            outcome: Outcome::Clear,
        }
    }

    /// Whether this launch has an instance waiting on it.
    pub fn is_pending(&self) -> bool {
        matches!(self.outcome, Outcome::Take(_))
    }

    /// Apply this launch's outcome to the session: an instance it bound becomes
    /// the one [`stop`] stops and [`allow_domain`] answers, and the one it
    /// replaces is shut down.
    ///
    /// Call this only once the launch has succeeded. Committing a launch that
    /// then fails is the bug this type exists to prevent: it would retire a
    /// healthy session's proxy in favour of one nothing is using.
    pub fn commit(mut self) {
        // Taken, so the drop that follows this call has nothing left to release.
        let outcome = std::mem::replace(&mut self.outcome, Outcome::Keep);
        let key = std::mem::take(&mut self.key);
        match outcome {
            Outcome::Keep => (),
            Outcome::Take(id) => {
                if let Some(supervisor) = SUPERVISOR.get() {
                    supervisor.send(Command::Commit { key, id });
                }
            }
            // Not `stop`, which would also release a parked handle: nothing is
            // parked under this key any more, because this handle *was* what
            // was parked.
            Outcome::Clear => {
                if let Some(supervisor) = SUPERVISOR.get() {
                    supervisor.send(Command::Stop { key });
                }
            }
        }
    }

    /// Leave the instance for the launch path to [`claim`], keyed by session.
    ///
    /// Composition and launch are two calls apart, and the invocation travels
    /// between them through a layer that must not own a boundary, so the
    /// handle waits here in between. Parking a second one for the same session
    /// releases the first: a composition nobody launched has no claim on a
    /// listener.
    pub fn park(self) {
        let displaced = parked()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.key.clone(), self);
        drop(displaced);
    }
}

impl Drop for PendingEgress {
    /// Releasing is the default, because failing is: only an outcome that was
    /// asked for by name survives this.
    fn drop(&mut self) {
        let Outcome::Take(id) = self.outcome else {
            return;
        };
        if let Some(supervisor) = SUPERVISOR.get() {
            supervisor.send(Command::Discard {
                key: std::mem::take(&mut self.key),
                id,
            });
        }
    }
}

/// Instances that are bound and waiting for the launch that composed them.
fn parked() -> &'static Mutex<HashMap<String, PendingEgress>> {
    static PARKED: OnceLock<Mutex<HashMap<String, PendingEgress>>> = OnceLock::new();
    PARKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take the instance a composition [`parked`](PendingEgress::park) for this
/// session, so the launch about to happen owns it.
///
/// Always answers: a session whose composition prepared nothing gets a handle
/// with nothing to commit, which every launch path can treat identically.
pub fn claim(session_key: &str) -> PendingEgress {
    parked()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(session_key)
        .unwrap_or_else(|| PendingEgress::none(session_key))
}

/// Bind the egress proxy one launch will be composed against.
///
/// `transport` comes from the chosen backend's
/// [`Caps::proxy_transport`](crate::sandbox::Caps::proxy_transport), and
/// `scratch` is the per-session directory friring minted for the launch — the
/// only place a unix socket may live, because it is private to friring and
/// dropped with the session. Private to the *session* only where the boundary
/// is: a place's scratch directories sit in the one tree that place mounts, so
/// in there the socket is private to the place, which is the trust domain there
/// (see this module's own docs).
///
/// The instance is listening when this returns and enforcing the policy, but it
/// is not the session's: whatever that session is already using keeps running
/// until [`PendingEgress::commit`], and is left alone entirely if the launch
/// fails instead. Replacing rather than reusing is deliberate — the profile may
/// have been edited between launches, and a fresh instance means a fresh token,
/// so a process left over from the previous launch cannot keep using the
/// tunnel.
///
/// The scratch directory is writable by the sandbox, which is the point of it —
/// and the reason the socket cannot be hijacked from in there is worth stating.
/// While a launch is live the socket is a *mount point* inside the sandbox, so
/// unlinking it is `EBUSY`. Between launches the agent could plant something at
/// the path, and the next start refuses to touch it: [`Proxy::start`] replaces
/// only a socket that nothing is serving, and bails on anything else. Both ends
/// of that are fail-closed — the worst an agent achieves is refusing its own
/// next launch.
///
/// Friring does connect to that path, once, and only to find out whether
/// something is already serving it: a socket file outlives a crash, and
/// unlinking one blindly could take a live sibling's listener out from under a
/// running agent. The probe `lstat`s first and refuses a symlink or a non-socket
/// without touching it, sends and reads nothing, and treats a peer that answers
/// as a reason to *abort the launch* rather than to proceed. So a socket an
/// agent plants is never a channel to friring — the most it can do is be
/// answered once and stop its own next launch.
///
/// # Errors
///
/// The policy carries a rule the proxy will not load, a socket path is longer
/// than any platform's `sun_path`, or the listener could not be bound. Every
/// one of them refuses the launch: a sandbox that silently has no filtered way
/// out is a session the user believes is proxied and is not.
pub fn prepare(
    session_key: &str,
    policy: &SandboxPolicy,
    transport: ProxyTransport,
    scratch: &Path,
) -> SandboxResult<Prepared> {
    prepare_at(session_key, policy, transport, scratch, relay_addr())
}

/// [`prepare`] for a sandbox whose relay does **not** listen on
/// [`relay_addr`].
///
/// A namespaced policy sandbox gets a private `127.0.0.1`, so one fixed port is
/// enough and [`prepare`] is the whole story. A **place** is created once per
/// profile and shared by that profile's sessions, so they share one loopback and
/// a fixed port would collide: each takes its own port out of a span and passes
/// it here, and the address is what the proxy environment is composed against.
/// Getting this wrong is silent — the second session's agent would dial a port
/// nothing listens on and fail closed, with the profile still claiming a
/// filtered network.
///
/// A port of its own is addressing, never separation: the loopback is the
/// place's, so any session in it can dial any of these ports, and holds the
/// token to be let through (this module's own docs).
///
/// `relay` is where the *sandbox* reaches its relay, never where friring binds
/// anything: the proxy still listens on the unix socket in `scratch`.
///
/// # Errors
///
/// As [`prepare`].
pub fn prepare_at(
    session_key: &str,
    policy: &SandboxPolicy,
    transport: ProxyTransport,
    scratch: &Path,
    relay: SocketAddr,
) -> SandboxResult<Prepared> {
    let refuse = |detail: String| SandboxError::Refused {
        profile: policy.profile.clone(),
        detail,
    };
    let rules = proxy_policy(policy).map_err(|error| {
        refuse(format!(
            "the egress proxy refused this profile's domain rules: {error:#}"
        ))
    })?;

    let bind = match transport {
        ProxyTransport::Loopback => StartBind::Loopback,
        ProxyTransport::UnixSocket => {
            let (primary, alternate) = socket_paths(scratch).map_err(&refuse)?;
            StartBind::UnixSocket {
                primary,
                alternate,
                relay,
            }
        }
    };

    let bound = supervisor()
        .start(session_key, rules, bind)
        .map_err(|error| {
            // Never a fallback to running on the host. The reachable way to make
            // this fail is an agent inside a place binding its own listener at
            // the socket path its next launch needs — the scratch directory is
            // sandbox-writable by design and, inside a place, shared with every
            // sibling session — and a profile with
            // `allow_unsandboxed_fallback` on would answer that by starting the
            // agent outside the boundary. So it is classified as interference
            // (see `SandboxError::Tampered`), which also covers the honest
            // reading of a bind that simply fails: a filtered profile whose
            // proxy is not listening must refuse, never run unfiltered.
            refuse(format!(
                "the egress proxy could not start, so this sandbox would have no filtered way \
                 out: {error}"
            ))
            .tampered()
        })?;
    // A listener is bound from here on, so every remaining way out of this
    // function has to release it. The handle is built before the first of them.
    let pending = PendingEgress {
        key: session_key.to_string(),
        outcome: Outcome::Take(bound.id),
    };

    let endpoint = match (bound.unix, bound.tcp) {
        (Some(socket), _) => ProxyEndpoint::UnixSocket {
            // Bind-mounted at its own path, so every rule and every log line
            // names one string on both sides of the boundary.
            host_path: socket.clone(),
            inside_path: socket,
        },
        (None, Some(addr)) => ProxyEndpoint::Loopback { port: addr.port() },
        (None, None) => {
            return Err(refuse(
                "the egress proxy started without a listener to hand the sandbox".to_string(),
            ))
        }
    };
    Ok(Prepared {
        grant: ProxyGrant {
            endpoint,
            env: bound.env,
        },
        pending,
    })
}

/// [`prepare`] a proxy and give it to the session in one step, for a caller
/// with no launch to guard: nothing is composed against this grant, so there is
/// no window in which committing it could turn out to be wrong.
///
/// # Errors
///
/// As [`prepare`].
pub fn establish(
    session_key: &str,
    policy: &SandboxPolicy,
    transport: ProxyTransport,
    scratch: &Path,
) -> SandboxResult<ProxyGrant> {
    let prepared = prepare(session_key, policy, transport, scratch)?;
    prepared.pending.commit();
    Ok(prepared.grant)
}

/// Stop the proxy a session was given, if it has one — and release a prepared
/// instance nobody claimed, since a session being torn down will not launch it.
///
/// Never starts the supervisor: a friring that has sandboxed nothing has
/// nothing to stop, and spawning a thread to say so would cost every session
/// the feature is not used for.
pub fn stop(session_key: &str) {
    drop(claim(session_key));
    let Some(supervisor) = SUPERVISOR.get() else {
        return;
    };
    supervisor.send(Command::Stop {
        key: session_key.to_string(),
    });
}

/// Stop every running instance and wait for them, for a friring shutting down
/// while sandboxed sessions are alive.
pub fn shutdown_all() {
    parked()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    let Some(supervisor) = SUPERVISOR.get() else {
        return;
    };
    let (reply, answer) = std::sync::mpsc::channel();
    supervisor.send(Command::StopAll { reply });
    let _ = answer.recv();
}

/// Apply a first-use answer to a running proxy, without restarting it.
///
/// The caller persists the same rule to the profile; this is what makes it take
/// effect for the session that asked, immediately.
///
/// # Errors
///
/// The rule is not one the proxy accepts, or that session has no proxy running
/// — a stale prompt, answered after the session was torn down.
pub fn allow_domain(session_key: &str, rule: &str) -> Result<(), String> {
    let parsed: HostRule = rule
        .parse()
        .map_err(|error: anyhow::Error| format!("{error:#}"))?;
    let supervisor = SUPERVISOR
        .get()
        .ok_or_else(|| "no sandbox egress proxy is running".to_string())?;
    let (reply, answer) = std::sync::mpsc::channel();
    supervisor.send(Command::Allow {
        key: session_key.to_string(),
        rule: Box::new(parsed),
        reply,
    });
    match answer.recv() {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "session '{session_key}' has no egress proxy running; its sandbox was torn down"
        )),
        Err(_) => Err("the sandbox egress supervisor is not running".to_string()),
    }
}

/// Take the refusals recorded since the last call, oldest first.
///
/// Polled rather than pushed so the consumer needs no async context: the TUI
/// drains this on its own tick, exactly as it drains every other background
/// signal.
pub fn take_denials() -> Vec<SessionDenial> {
    let mut buffer = denials().lock().unwrap_or_else(PoisonError::into_inner);
    buffer.drain(..).collect()
}

/// The proxy's own policy vocabulary, built from the session profile's.
///
/// The two matchers deliberately do not share a type (the proxy is a leaf in
/// the architecture allowlist), so the rules cross as their canonical strings —
/// which `tests/egress_matcher_conformance.rs` holds both sides to.
fn proxy_policy(policy: &SandboxPolicy) -> anyhow::Result<crate::proxy::Policy> {
    let mode = match policy.network {
        NetworkMode::None => crate::proxy::NetworkMode::None,
        NetworkMode::Allowlist => crate::proxy::NetworkMode::Allowlist,
        NetworkMode::Full => crate::proxy::NetworkMode::Full,
    };
    crate::proxy::Policy::new(mode)
        .with_allow(policy.allow.iter().map(ToString::to_string))?
        .with_deny(policy.deny.iter().map(ToString::to_string))
}

/// Both socket paths a launch in `scratch` may be given, canonical first.
///
/// Both are checked against `sun_path` here rather than one at a time: a
/// directory that fits only the shorter name would launch once and then refuse
/// the relaunch that has to take the other, which is a failure the user would
/// meet at the worst possible moment. See [`PROXY_SOCKET_ALT_NAME`].
fn socket_paths(scratch: &Path) -> Result<(String, String), String> {
    Ok((
        socket_path(scratch, PROXY_SOCKET_NAME)?,
        socket_path(scratch, PROXY_SOCKET_ALT_NAME)?,
    ))
}

/// One socket path for a launch whose scratch directory is `scratch`.
fn socket_path(scratch: &Path, name: &str) -> Result<String, String> {
    let socket = scratch.join(name);
    let path = socket.to_str().ok_or_else(|| {
        format!(
            "the sandbox scratch directory ('{}') is not valid UTF-8, and the proxy socket inside \
             it could not be named exactly",
            scratch.display()
        )
    })?;
    if path.len() > MAX_SOCKET_PATH {
        return Err(format!(
            "the egress proxy's socket path ('{path}') is {} bytes, and a unix socket accepts at \
             most {MAX_SOCKET_PATH}. Point XDG_DATA_HOME (or FRIRING_DATA_DIR) at a shorter \
             directory",
            path.len()
        ));
    }
    Ok(path.to_string())
}

/// The proxy environment for a sandbox dialling `http` / `socks`.
///
/// `ALL_PROXY` is **`socks5h://`**, never `socks5://`. The `h` is what keeps
/// name resolution on the proxy's side; with plain `socks5` the client resolves
/// the name itself and hands the proxy an address, which no domain rule can
/// match — the allowlist would then be enforced against IP literals nobody
/// wrote, silently permitting or refusing the wrong things instead of failing
/// loudly. Both URLs come from [`Proxy`] itself for that reason: the scheme is
/// its decision, not a string built twice.
fn proxy_env(http: &str, socks: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in HTTP_PROXY_VARS {
        env.insert((*name).to_string(), http.to_string());
    }
    for name in SOCKS_PROXY_VARS {
        env.insert((*name).to_string(), socks.to_string());
    }
    for name in NO_PROXY_VARS {
        env.insert((*name).to_string(), NO_PROXY_VALUE.to_string());
    }
    env
}

/// What one [`Command::Start`] hands back: where the sandbox reaches the proxy,
/// and the environment that lets it. The token itself never leaves the
/// supervisor except inside those URLs — which is why `Debug` is hand-written
/// here too, on the same reasoning as [`ProxyGrant`]'s.
struct Bound {
    /// Which pending instance this is. Carried by the launch's
    /// [`PendingEgress`] so a commit or a release names the instance that
    /// launch prepared, never a later one.
    id: u64,
    tcp: Option<SocketAddr>,
    /// The socket path the supervisor chose. See [`StartBind::UnixSocket`].
    unix: Option<String>,
    env: BTreeMap<String, String>,
}

impl fmt::Debug for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bound")
            .field("id", &self.id)
            .field("tcp", &self.tcp)
            .field("unix", &self.unix)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Which listener one instance opens.
#[derive(Debug)]
enum StartBind {
    /// An ephemeral host loopback port — what a sandbox that still shares the
    /// host's network stack dials. Two of them never collide, so there is
    /// nothing to choose between.
    Loopback,
    /// A unix socket in the session's scratch directory, bound at the first of
    /// these paths the session's *running* instance is not already using. See
    /// [`PROXY_SOCKET_ALT_NAME`] for why there are two.
    ///
    /// `relay` is the address the sandbox's own relay offers — [`relay_addr`]
    /// for a namespaced policy sandbox, a per-session port for a place (see
    /// [`prepare_at`]). It decides nothing about the listener; it is what the
    /// proxy environment names.
    UnixSocket {
        primary: String,
        alternate: String,
        relay: SocketAddr,
    },
}

/// One instruction for the supervisor thread.
enum Command {
    Start {
        key: String,
        /// Boxed to keep every variant the size of the smallest one.
        policy: Box<Policy>,
        bind: StartBind,
        reply: std::sync::mpsc::Sender<Result<Bound, String>>,
    },
    /// Give a prepared instance to its session, retiring the one it replaces.
    Commit {
        key: String,
        id: u64,
    },
    /// Shut down a prepared instance the launch that asked for it will not use.
    Discard {
        key: String,
        id: u64,
    },
    Stop {
        key: String,
    },
    StopAll {
        reply: std::sync::mpsc::Sender<()>,
    },
    Allow {
        key: String,
        /// Boxed to keep every variant the size of the smallest one.
        rule: Box<HostRule>,
        reply: std::sync::mpsc::Sender<bool>,
    },
    /// What a running instance is enforcing right now.
    ///
    /// Test-only, and the reason is worth stating: [`Command::Allow`]'s reply
    /// says only that an instance was *found*, so without this the whole
    /// `update_policy` call could be deleted and every other assertion here
    /// would still pass. The wire-level half of the same claim — a live policy
    /// change turning a refusal into a tunnel — is
    /// `proxy::tests::a_policy_update_takes_effect_without_a_restart`.
    #[cfg(test)]
    Rules {
        key: String,
        reply: std::sync::mpsc::Sender<Option<Vec<String>>>,
    },
}

/// The handle to the thread that owns every running proxy.
struct Supervisor {
    commands: mpsc::UnboundedSender<Command>,
}

static SUPERVISOR: OnceLock<Supervisor> = OnceLock::new();

fn supervisor() -> &'static Supervisor {
    SUPERVISOR.get_or_init(Supervisor::spawn)
}

impl Supervisor {
    /// Start the thread and its runtime. A thread that cannot be spawned leaves
    /// a sender with no receiver, so every later command fails and every launch
    /// that needed a proxy is refused — the fail-closed direction.
    fn spawn() -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let started = std::thread::Builder::new()
            .name("friring-egress".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(%error, "sandbox egress runtime could not be built");
                        return;
                    }
                };
                runtime.block_on(serve(receiver));
            });
        if let Err(error) = started {
            tracing::error!(%error, "sandbox egress supervisor thread could not be started");
        }
        Self { commands }
    }

    /// Queue a command, dropping it when the supervisor is gone. Every caller
    /// that needs an answer waits on its own reply channel, which reports the
    /// same failure.
    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    fn start(&self, key: &str, policy: Policy, bind: StartBind) -> Result<Bound, String> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.send(Command::Start {
            key: key.to_string(),
            policy: Box::new(policy),
            bind,
            reply,
        });
        // A plain blocking receive, not `tokio::sync`: this runs on whatever
        // thread composed the launch, which is a runtime worker inside the TUI
        // — where tokio's own blocking helpers panic on purpose.
        answer
            .recv()
            .map_err(|_| "the sandbox egress supervisor is not running".to_string())?
    }
}

/// Own every running proxy, one command at a time.
///
/// Two maps, because an instance has two lives: `proxies` is what each session
/// is using, and `pending` is what a launch has prepared and not yet committed
/// — at most one per session, since a session composes one launch at a time.
/// Serialising the commands is what makes replacement safe: a `Commit` shuts the
/// instance it replaces down — and unlinks its socket — before the new one takes
/// its place, and nothing tears down a listener the session is still reaching.
async fn serve(mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut proxies: HashMap<String, Proxy> = HashMap::new();
    let mut pending: HashMap<String, (u64, Proxy)> = HashMap::new();
    let mut next_id: u64 = 0;
    while let Some(command) = commands.recv().await {
        match command {
            Command::Start {
                key,
                policy,
                bind,
                reply,
            } => {
                // A composition nobody launched has no claim on a listener —
                // and it may be holding the socket path this one needs.
                if let Some((_, superseded)) = pending.remove(&key) {
                    superseded.shutdown().await;
                }
                let (socket, relay) = match &bind {
                    StartBind::Loopback => (None, None),
                    StartBind::UnixSocket {
                        primary,
                        alternate,
                        relay,
                    } => {
                        let held = proxies.get(&key).and_then(|proxy| proxy.unix_path());
                        let path = if held == Some(Path::new(primary)) {
                            alternate.clone()
                        } else {
                            primary.clone()
                        };
                        (Some(path), Some(*relay))
                    }
                };
                let config = ProxyConfig {
                    bind: match &socket {
                        Some(path) => ProxyBind::unix(path),
                        None => ProxyBind::tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
                    },
                    ..ProxyConfig::new(*policy)
                };
                let started = match Proxy::start(config).await {
                    Ok(started) => started,
                    Err(error) => {
                        let _ = reply.send(Err(format!("{error:#}")));
                        continue;
                    }
                };
                let (proxy, denials) = started;
                // A socket-only instance is dialled through the relay inside
                // the sandbox's own namespace; the socket is what the relay
                // forwards to, and no HTTP client can name it. The relay's
                // address comes from the launch, because a place's sessions
                // share one loopback and cannot share one port.
                let endpoint = relay.or_else(|| proxy.tcp_addr());
                next_id += 1;
                let bound = Bound {
                    id: next_id,
                    tcp: proxy.tcp_addr(),
                    unix: socket,
                    env: match endpoint {
                        Some(addr) => proxy_env(
                            &proxy.http_proxy_url_at(addr),
                            &proxy.socks5_proxy_url_at(addr),
                        ),
                        None => BTreeMap::new(),
                    },
                };
                // Refusals are the *session's*, whether or not its launch has
                // committed yet: nothing else could explain them.
                tokio::spawn(collect_denials(key.clone(), denials));
                pending.insert(key, (next_id, proxy));
                let _ = reply.send(Ok(bound));
            }
            Command::Commit { key, id } => {
                // Only when it is still the instance that launch prepared: a
                // handle outlived by a newer composition must not adopt it.
                if pending.get(&key).is_some_and(|(held, _)| *held == id) {
                    if let Some((_, proxy)) = pending.remove(&key) {
                        if let Some(previous) = proxies.remove(&key) {
                            previous.shutdown().await;
                        }
                        proxies.insert(key, proxy);
                    }
                }
            }
            Command::Discard { key, id } => {
                if pending.get(&key).is_some_and(|(held, _)| *held == id) {
                    if let Some((_, proxy)) = pending.remove(&key) {
                        proxy.shutdown().await;
                    }
                }
            }
            Command::Stop { key } => {
                if let Some((_, proxy)) = pending.remove(&key) {
                    proxy.shutdown().await;
                }
                if let Some(proxy) = proxies.remove(&key) {
                    proxy.shutdown().await;
                }
            }
            Command::StopAll { reply } => {
                for (_, (_, proxy)) in pending.drain() {
                    proxy.shutdown().await;
                }
                for (_, proxy) in proxies.drain() {
                    proxy.shutdown().await;
                }
                let _ = reply.send(());
            }
            Command::Allow { key, rule, reply } => {
                let running = proxies.get(&key);
                if let Some(proxy) = running {
                    proxy.update_policy(|policy| policy.allow_rule(*rule));
                }
                let _ = reply.send(running.is_some());
            }
            #[cfg(test)]
            Command::Rules { key, reply } => {
                let rules = proxies.get(&key).map(|proxy| {
                    proxy
                        .policy()
                        .allow_rules()
                        .iter()
                        .map(ToString::to_string)
                        .collect()
                });
                let _ = reply.send(rules);
            }
        }
    }
    for (_, (_, proxy)) in pending.drain() {
        proxy.shutdown().await;
    }
    for (_, proxy) in proxies.drain() {
        proxy.shutdown().await;
    }
}

/// Buffer one session's refusals until the TUI drains them. Ends when the proxy
/// is dropped, which closes the channel.
async fn collect_denials(session_key: String, mut events: mpsc::Receiver<DenialEvent>) {
    while let Some(event) = events.recv().await {
        record_denial(SessionDenial {
            session_key: session_key.clone(),
            event,
        });
    }
}

/// How many refusals are kept for a consumer that is not draining them.
///
/// The proxy logs every denial as well, so an overflowing buffer costs
/// notifications, never the record.
const MAX_BUFFERED_DENIALS: usize = 256;

fn denials() -> &'static Mutex<VecDeque<SessionDenial>> {
    static DENIALS: OnceLock<Mutex<VecDeque<SessionDenial>>> = OnceLock::new();
    DENIALS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn record_denial(denial: SessionDenial) {
    let mut buffer = denials().lock().unwrap_or_else(PoisonError::into_inner);
    if buffer.len() >= MAX_BUFFERED_DENIALS {
        buffer.pop_front();
    }
    buffer.push_back(denial);
}

/// Put a refusal in the buffer [`take_denials`] drains, without standing up a
/// proxy and a sandbox to provoke one.
///
/// Test-only, and it exists so the TUI's own tick can be driven over the real
/// channel rather than over a hand-passed vector: dropping the drain from the
/// tick is otherwise a change no test notices.
#[cfg(test)]
pub(crate) fn record_denial_for_test(denial: SessionDenial) {
    record_denial(denial);
}

/// The allow rules a session's running proxy is enforcing, in the proxy's own
/// canonical spelling — `None` when nothing is running for that key.
#[cfg(test)]
pub(crate) fn running_allow_rules(session_key: &str) -> Option<Vec<String>> {
    let (reply, answer) = std::sync::mpsc::channel();
    SUPERVISOR.get()?.send(Command::Rules {
        key: session_key.to_string(),
        reply,
    });
    answer.recv().ok().flatten()
}

/// The scratch directory a test binds its socket in: short, private, and
/// nowhere near the user's own state.
///
/// Deliberately not the unit-test data directory: on macOS that path is long
/// enough on its own that `<data>/sandbox/tmp/<key>/proxy.sock` exceeds
/// `sun_path`, which is exactly what [`MAX_SOCKET_PATH`] refuses.
#[cfg(test)]
fn test_scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("frx{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the test scratch directory");
    dir
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::session::{SandboxBackendKind, SandboxPath, SandboxProfile};

    fn policy(mode: NetworkMode, allow: &[&str], deny: &[&str]) -> SandboxPolicy {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_mode = mode;
        profile.network_allow = allow.iter().map(|s| (*s).to_string()).collect();
        profile.network_deny = deny.iter().map(|s| (*s).to_string()).collect();
        profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .expect("a valid profile")
    }

    /// The rule that decides whether a launch needs a proxy at all — and the
    /// one P1 got wrong in the other direction, by refusing `full` with denies
    /// instead of proxying it.
    #[test]
    fn only_a_mode_the_kernel_cannot_express_needs_the_proxy() {
        assert!(proxy_required(&policy(NetworkMode::Allowlist, &[], &[])));
        assert!(proxy_required(&policy(
            NetworkMode::Full,
            &[],
            &["evil.example"]
        )));
        // `full` with nothing to take back means what it says on its own, and
        // `none` has nothing to reach.
        assert!(!proxy_required(&policy(NetworkMode::Full, &[], &[])));
        assert!(!proxy_required(&policy(NetworkMode::None, &[], &[])));
        // A deny list under `none` is still nothing to enforce.
        assert!(!proxy_required(&policy(
            NetworkMode::None,
            &[],
            &["evil.example"]
        )));
    }

    #[test]
    fn the_profiles_rules_cross_into_the_proxys_vocabulary() {
        let rules = proxy_policy(&policy(
            NetworkMode::Allowlist,
            &["*.github.com", "api.anthropic.com:443"],
            &["gist.github.com"],
        ))
        .expect("the proxy loads the profile's rules");
        assert_eq!(rules.mode(), crate::proxy::NetworkMode::Allowlist);
        // The wildcard is the subtree spelling, and it crosses as written; the
        // port scope survives too.
        let allow: Vec<String> = rules
            .allow_rules()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(allow, ["*.github.com", "api.anthropic.com:443"]);
        let deny: Vec<String> = rules.deny_rules().iter().map(ToString::to_string).collect();
        assert_eq!(deny, ["gist.github.com"]);
    }

    /// Every spelling is set, because tools disagree about which one they read
    /// — and `ALL_PROXY` carries the SOCKS scheme unchanged from the proxy, for
    /// the reason the grant tests below assert against a real instance.
    #[test]
    fn every_spelling_of_the_proxy_variables_is_set() {
        let env = proxy_env(
            "http://friring:tok@127.0.0.1:9",
            "socks5h://friring:tok@127.0.0.1:9",
        );
        for name in HTTP_PROXY_VARS {
            assert_eq!(
                env.get(*name).map(String::as_str),
                Some("http://friring:tok@127.0.0.1:9"),
                "{name} is unset"
            );
        }
        for name in SOCKS_PROXY_VARS {
            let value = env.get(*name).expect("ALL_PROXY is set");
            assert!(value.starts_with("socks5h://"), "{name} = {value}");
            assert!(
                !value.starts_with("socks5://"),
                "{name} must not let the client resolve: {value}"
            );
        }
        // The agent's own local traffic is not tunnelled through a filter that
        // would refuse it for not being in a domain allowlist.
        for name in NO_PROXY_VARS {
            assert_eq!(env.get(*name).map(String::as_str), Some(NO_PROXY_VALUE));
        }
    }

    /// The grant is the type that actually holds the credential, and `Debug`
    /// output is the cheapest way for one to escape: a `?grant` in a log line,
    /// a `{grant:?}` in an error, an `expect` on a result that carries one.
    /// The variable *names* are the diagnostic and survive; the URLs do not.
    #[test]
    fn a_grant_does_not_print_the_credential_it_carries() {
        let grant = ProxyGrant {
            endpoint: ProxyEndpoint::Loopback { port: 9 },
            env: proxy_env(
                "http://friring:s3cr3t@127.0.0.1:9",
                "socks5h://friring:s3cr3t@127.0.0.1:9",
            ),
        };
        let printed = format!("{grant:?}");
        assert!(!printed.contains("s3cr3t"), "{printed}");
        assert!(!printed.contains("friring:"), "{printed}");
        // Still worth reading: which endpoint, and which spellings were set.
        assert!(printed.contains("Loopback"), "{printed}");
        assert!(printed.contains("HTTP_PROXY"), "{printed}");
        assert!(printed.contains("NO_PROXY"), "{printed}");
    }

    /// The same guarantee for the config the supervisor builds one from: the
    /// token is handed in there, and a derived `Debug` would print it verbatim.
    #[test]
    fn a_proxy_config_does_not_print_the_token_it_was_handed() {
        let config = ProxyConfig {
            token: Some("s3cr3t".to_string()),
            ..ProxyConfig::new(Policy::new(crate::proxy::NetworkMode::Allowlist))
        };
        let printed = format!("{config:?}");
        assert!(!printed.contains("s3cr3t"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        // A config with no token says so, because "supplied or minted" is a
        // real difference between two launches.
        let minted = ProxyConfig {
            token: None,
            ..ProxyConfig::default()
        };
        assert!(format!("{minted:?}").contains("token: None"));
    }

    /// A real proxy, bound on an ephemeral loopback port, reached the way a
    /// seatbelt sandbox reaches it. No egress: nothing connects through it.
    #[test]
    fn a_loopback_grant_names_the_bound_port_and_carries_its_credential() {
        let policy = policy(NetworkMode::Allowlist, &["github.com"], &[]);
        let scratch = test_scratch("loopback");
        let grant = establish(
            "egress-loopback",
            &policy,
            ProxyTransport::Loopback,
            &scratch,
        )
        .expect("the proxy binds a loopback port");

        let ProxyEndpoint::Loopback { port } = grant.endpoint else {
            panic!("expected a loopback endpoint, got {:?}", grant.endpoint);
        };
        assert_ne!(port, 0, "the bound port, not the requested one");
        let http = grant.env.get("HTTP_PROXY").expect("HTTP_PROXY is set");
        assert!(http.ends_with(&format!("@127.0.0.1:{port}")), "{http}");
        // The credential rides in the URL, which is the only channel every HTTP
        // client understands — and the reason this value is never logged.
        fn token_of(url: &str) -> &str {
            url.strip_prefix("http://friring:")
                .and_then(|rest| rest.split_once('@'))
                .map(|(token, _)| token)
                .unwrap_or_else(|| panic!("no credential in {url}"))
        }
        let token = token_of(http);
        assert!(token.len() >= 16, "a guessable token: {token}");

        // `socks5h`, from the proxy's own renderer: with plain `socks5` the
        // client resolves the hostname and hands the proxy an address, so every
        // domain rule stops matching and the allowlist enforces nothing at all.
        let socks = grant.env.get("ALL_PROXY").expect("ALL_PROXY is set");
        assert!(socks.starts_with("socks5h://"), "{socks}");

        // Relaunching the same session replaces the instance rather than
        // stacking a second one, and mints a fresh token with it.
        let again = establish(
            "egress-loopback",
            &policy,
            ProxyTransport::Loopback,
            &scratch,
        )
        .expect("the replacement binds");
        assert_ne!(
            token_of(again.env.get("HTTP_PROXY").unwrap()),
            token_of(grant.env.get("HTTP_PROXY").unwrap()),
            "a relaunch must mint a fresh credential, not just a fresh port"
        );
        stop("egress-loopback");
    }

    /// The bwrap shape: a socket in the session's scratch directory, and an
    /// environment pointing at the relay inside the namespace rather than at
    /// the socket, which no HTTP client can dial.
    #[cfg(unix)]
    #[test]
    fn a_socket_grant_lives_in_the_scratch_directory_and_points_at_the_relay() {
        let policy = policy(NetworkMode::Allowlist, &["github.com"], &[]);
        let scratch = test_scratch("socket");
        let grant = establish(
            "egress-socket",
            &policy,
            ProxyTransport::UnixSocket,
            &scratch,
        )
        .expect("the proxy binds its socket");

        let ProxyEndpoint::UnixSocket {
            host_path,
            inside_path,
        } = &grant.endpoint
        else {
            panic!("expected a socket endpoint, got {:?}", grant.endpoint);
        };
        assert_eq!(host_path, inside_path, "one path on both sides");
        assert_eq!(
            Path::new(host_path),
            scratch.join(PROXY_SOCKET_NAME),
            "the socket belongs to the session, never the host temp root"
        );
        assert!(Path::new(host_path).exists(), "the socket is bound already");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(host_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "a credential-bearing endpoint");
        }

        // No client can dial a unix socket through HTTP_PROXY, so the
        // environment names the relay's address inside the sandbox.
        for name in HTTP_PROXY_VARS.iter().chain(SOCKS_PROXY_VARS) {
            let value = grant.env.get(*name).expect("proxy variable is set");
            assert!(
                value.ends_with(&relay_addr().to_string()),
                "{name} must point at the relay: {value}"
            );
            assert!(!value.contains(PROXY_SOCKET_NAME), "{name} = {value}");
        }
        // The relay moves bytes and reads none of them, so the scheme still has
        // to keep resolution on the *proxy's* side of the boundary.
        let socks = grant.env.get("ALL_PROXY").expect("ALL_PROXY is set");
        assert!(socks.starts_with("socks5h://"), "{socks}");

        stop("egress-socket");
    }

    /// A socket path no platform accepts is refused with the fix, rather than
    /// surfacing as `bind: Invalid argument` from inside a launch.
    #[test]
    fn an_over_long_socket_path_is_refused_with_the_fix() {
        let deep = PathBuf::from("/").join("d".repeat(MAX_SOCKET_PATH));
        let error = socket_paths(&deep).unwrap_err();
        assert!(error.contains("at most 103"), "{error}");
        assert!(error.contains("XDG_DATA_HOME"), "{error}");
        // A realistic one fits, and both names are checked: a directory that
        // only fits the canonical one would launch once and refuse the
        // relaunch that has to take the other.
        let (primary, alternate) = socket_paths(Path::new(
            "/home/u/.local/share/friring/sandbox/tmp/session",
        ))
        .unwrap();
        assert!(primary.ends_with(PROXY_SOCKET_NAME), "{primary}");
        assert!(alternate.ends_with(PROXY_SOCKET_ALT_NAME), "{alternate}");
        // Long enough that `<dir>/proxy.sock` is exactly the limit, which
        // leaves no room for the two extra bytes the other name costs.
        let snug =
            PathBuf::from("/").join("d".repeat(MAX_SOCKET_PATH - PROXY_SOCKET_NAME.len() - 2));
        socket_path(&snug, PROXY_SOCKET_NAME).expect("the canonical name fits");
        assert!(
            socket_paths(&snug).is_err(),
            "a directory only the shorter name fits is refused up front"
        );
    }

    /// A rule the proxy will not load refuses the launch instead of starting an
    /// instance that enforces a shorter list than the profile spells out.
    #[test]
    fn a_rule_the_proxy_refuses_refuses_the_launch() {
        let mut policy = policy(NetworkMode::Allowlist, &["github.com"], &[]);
        // Past the profile validator, which cannot run again here: a policy is
        // already-parsed data, so this is what a hand-built one could carry.
        policy.allow.push(crate::session::DomainRule {
            host: "not a host".to_string(),
            port: None,
        });
        let error = establish(
            "egress-bad-rule",
            &policy,
            ProxyTransport::Loopback,
            &test_scratch("bad-rule"),
        )
        .unwrap_err();
        assert!(matches!(error, SandboxError::Refused { .. }), "{error}");
        assert!(error.to_string().contains("domain rules"), "{error}");
    }

    /// Answering a first-use prompt reaches the running instance; answering one
    /// for a session that is gone says so rather than pretending.
    #[test]
    fn a_first_use_answer_applies_to_the_running_instance() {
        let policy = policy(NetworkMode::Allowlist, &[], &[]);
        establish(
            "egress-allow",
            &policy,
            ProxyTransport::Loopback,
            &test_scratch("allow"),
        )
        .expect("the proxy binds");

        assert_eq!(
            running_allow_rules("egress-allow").as_deref(),
            Some(&[][..]),
            "the profile allowed nothing to begin with"
        );

        allow_domain("egress-allow", "api.github.com:443").expect("the answer applies live");
        // The reply above says only that an instance was found — this is what
        // says the rule reached the policy it is enforcing. The wire-level half
        // (a refusal becoming a tunnel) is
        // `proxy::tests::a_policy_update_takes_effect_without_a_restart`.
        assert_eq!(
            running_allow_rules("egress-allow"),
            Some(vec!["api.github.com:443".to_string()])
        );

        // A rule the proxy will not load changes nothing.
        assert!(allow_domain("egress-allow", "https://nope").is_err());
        assert_eq!(
            running_allow_rules("egress-allow"),
            Some(vec!["api.github.com:443".to_string()])
        );

        stop("egress-allow");
        assert!(running_allow_rules("egress-allow").is_none());
        let stale = allow_domain("egress-allow", "api.github.com").unwrap_err();
        assert!(stale.contains("no egress proxy running"), "{stale}");
    }

    /// Wait for every command queued so far to have been handled.
    ///
    /// The supervisor answers one command at a time, so a reply to a later one
    /// is proof the earlier ones are done — which is what makes "this listener
    /// is gone" and "that one is still there" assertable without sleeping.
    fn settle(session_key: &str) {
        let _ = running_allow_rules(session_key);
    }

    fn is_listening(port: u16) -> bool {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    }

    /// The shape this module keeps: **an instance per session**, even for two
    /// sessions of one profile — which for a place means two instances inside
    /// one trust domain rather than two boundaries.
    ///
    /// What it buys is asserted here, because it is the whole justification
    /// left once the isolation claim is gone: a live grant lands on the session
    /// that was asked and nowhere else, and one session ending does not take a
    /// sibling's way out with it. What it does *not* buy — keeping a sibling in
    /// the same place away from that token — is unassertable here for the
    /// reason it is unfixable here: it is decided by the uid and the pid
    /// namespace the place gives them, not by this module.
    #[test]
    fn two_sessions_of_one_profile_get_an_instance_each() {
        let profile = policy(NetworkMode::Allowlist, &["github.com"], &[]);
        let first = establish(
            "egress-share-a",
            &profile,
            ProxyTransport::Loopback,
            &test_scratch("share-a"),
        )
        .expect("the first session's proxy binds");
        let second = establish(
            "egress-share-b",
            &profile,
            ProxyTransport::Loopback,
            &test_scratch("share-b"),
        )
        .expect("the second session's proxy binds");

        let (first_port, second_port) = (port_of(&first.endpoint), port_of(&second.endpoint));
        assert_ne!(first_port, second_port, "one listener served both sessions");
        assert_ne!(
            first.env.get("HTTP_PROXY"),
            second.env.get("HTTP_PROXY"),
            "one credential was handed to both sessions"
        );

        // A first-use answer applies to the instance of the session that was
        // asked. (A sibling *in a place* still reaches the widened one — by
        // taking its token, and at its own next launch through the profile the
        // answer is written back to.)
        allow_domain("egress-share-a", "api.github.com:443").expect("the answer applies live");
        let widened = running_allow_rules("egress-share-a").expect("the asked session is running");
        assert!(
            widened.iter().any(|rule| rule == "api.github.com:443"),
            "{widened:?}"
        );
        let untouched = running_allow_rules("egress-share-b").expect("the sibling is running");
        assert_eq!(untouched, vec!["github.com".to_string()]);

        // And a session ending takes its own instance only.
        stop("egress-share-a");
        settle("egress-share-b");
        assert!(!is_listening(first_port));
        assert!(
            is_listening(second_port),
            "tearing one session down cost the other its egress"
        );
        stop("egress-share-b");
        settle("egress-share-b");
        assert!(!is_listening(second_port));
    }

    fn port_of(endpoint: &ProxyEndpoint) -> u16 {
        match endpoint {
            ProxyEndpoint::Loopback { port } => *port,
            other => panic!("expected a loopback endpoint, got {other:?}"),
        }
    }

    /// The whole point of preparing rather than establishing: composing a
    /// launch must not take the boundary away from the agent that is running
    /// *now*, because the launch it is being composed for may never happen.
    #[test]
    fn a_prepared_instance_leaves_the_running_one_alone_until_it_is_committed() {
        let key = "egress-provisional";
        let scratch = test_scratch("provisional");
        let running = policy(NetworkMode::Allowlist, &["old.example"], &[]);
        let relaunch = policy(NetworkMode::Allowlist, &["new.example"], &[]);

        let old = establish(key, &running, ProxyTransport::Loopback, &scratch)
            .expect("the session's own proxy binds");
        let old_port = port_of(&old.endpoint);

        // A relaunch is composed: bound and enforcing, and nobody's.
        let prepared = prepare(key, &relaunch, ProxyTransport::Loopback, &scratch)
            .expect("the relaunch's proxy binds");
        let new_port = port_of(&prepared.grant.endpoint);
        assert_ne!(old_port, new_port, "a second listener, not the same one");
        assert!(is_listening(old_port), "the running agent lost its way out");
        assert_eq!(
            running_allow_rules(key),
            Some(vec!["old.example".to_string()]),
            "the session's boundary is still the one it is using"
        );

        // The launch failed: dropping the handle is what every error path does.
        drop(prepared);
        settle(key);
        assert!(
            !is_listening(new_port),
            "a launch that never happened left a listener behind"
        );
        assert!(
            is_listening(old_port),
            "a launch that never happened cost the running agent its egress"
        );
        assert_eq!(
            running_allow_rules(key),
            Some(vec!["old.example".to_string()])
        );

        // A relaunch that *does* happen replaces it, exactly once.
        let prepared = prepare(key, &relaunch, ProxyTransport::Loopback, &scratch)
            .expect("the relaunch's proxy binds");
        let new_port = port_of(&prepared.grant.endpoint);
        prepared.pending.commit();
        settle(key);
        assert!(
            !is_listening(old_port),
            "the replaced instance kept running"
        );
        assert!(is_listening(new_port));
        assert_eq!(
            running_allow_rules(key),
            Some(vec!["new.example".to_string()])
        );

        stop(key);
        settle(key);
        assert!(!is_listening(new_port));
    }

    /// A composition that is never claimed is not a leak either: the next one
    /// for the same session releases it, and so does tearing the session down.
    #[test]
    fn a_parked_instance_is_released_by_the_next_one_and_by_teardown() {
        let key = "egress-parked";
        let scratch = test_scratch("parked");
        let profile = policy(NetworkMode::Allowlist, &[], &[]);

        let first = prepare(key, &profile, ProxyTransport::Loopback, &scratch).unwrap();
        let first_port = port_of(&first.grant.endpoint);
        first.pending.park();

        let second = prepare(key, &profile, ProxyTransport::Loopback, &scratch).unwrap();
        let second_port = port_of(&second.grant.endpoint);
        second.pending.park();
        settle(key);
        assert!(
            !is_listening(first_port),
            "the superseded composition kept its listener"
        );
        assert!(is_listening(second_port));

        // Claiming hands ownership to the launch; nothing is parked afterwards.
        let claimed = claim(key);
        assert!(claimed.is_pending());
        assert!(!claim(key).is_pending(), "claimed twice");
        drop(claimed);
        settle(key);
        assert!(!is_listening(second_port));

        // And a teardown releases one nobody claimed at all.
        let third = prepare(key, &profile, ProxyTransport::Loopback, &scratch).unwrap();
        let third_port = port_of(&third.grant.endpoint);
        third.pending.park();
        stop(key);
        settle(key);
        assert!(!is_listening(third_port));
    }

    /// The bwrap shape of the same claim. Two live instances of one session
    /// cannot share a socket path — [`Proxy::start`] refuses a path something
    /// is serving, which is what stops it unlinking a live sibling — so the
    /// launch being prepared takes the other name.
    #[cfg(unix)]
    #[test]
    fn a_prepared_socket_takes_the_name_the_running_one_is_not_using() {
        let key = "egress-two-sockets";
        let scratch = test_scratch("two-sockets");
        let profile = policy(NetworkMode::Allowlist, &[], &[]);
        let canonical = scratch.join(PROXY_SOCKET_NAME);
        let alternate = scratch.join(PROXY_SOCKET_ALT_NAME);
        let _ = std::fs::remove_file(&canonical);
        let _ = std::fs::remove_file(&alternate);

        establish(key, &profile, ProxyTransport::UnixSocket, &scratch)
            .expect("the session's own proxy binds");
        assert!(
            canonical.exists(),
            "the first instance takes the plain name"
        );

        let prepared = prepare(key, &profile, ProxyTransport::UnixSocket, &scratch)
            .expect("the relaunch binds");
        assert!(
            alternate.exists(),
            "the relaunch must not need the path the running agent reaches"
        );
        assert!(
            canonical.exists(),
            "the running agent's socket was unlinked"
        );

        // Released: its socket goes with it, and the live one is untouched.
        drop(prepared);
        settle(key);
        assert!(!alternate.exists(), "a released instance left its socket");
        assert!(canonical.exists());

        // Committed: now the *old* socket is the one that goes, and the name it
        // frees is available to the launch after this one.
        let prepared = prepare(key, &profile, ProxyTransport::UnixSocket, &scratch)
            .expect("the relaunch binds");
        prepared.pending.commit();
        settle(key);
        assert!(!canonical.exists(), "the replaced instance left its socket");
        assert!(alternate.exists());

        let prepared = prepare(key, &profile, ProxyTransport::UnixSocket, &scratch).unwrap();
        assert!(canonical.exists(), "the freed name is reused");
        drop(prepared);
        stop(key);
        settle(key);
        assert!(!canonical.exists() && !alternate.exists());
    }

    #[test]
    fn buffered_denials_drop_the_oldest_rather_than_growing_without_bound() {
        use crate::proxy::{DenyReason, Protocol};

        let _ = take_denials();
        for index in 0..MAX_BUFFERED_DENIALS + 5 {
            record_denial(SessionDenial {
                session_key: "s1".to_string(),
                event: DenialEvent {
                    protocol: Protocol::Http,
                    host: format!("host-{index}.example"),
                    port: 443,
                    reason: DenyReason::NotAllowlisted,
                },
            });
        }
        let drained = take_denials();
        assert_eq!(drained.len(), MAX_BUFFERED_DENIALS);
        assert_eq!(drained[0].event.host, "host-5.example");
        assert!(take_denials().is_empty(), "draining empties the buffer");
    }
}
