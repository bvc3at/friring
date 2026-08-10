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
//! other two things it owns are per boundary: the bearer token, which is that
//! sandbox's credential and must not be held by a sibling, and — for a
//! namespaced backend — the unix socket, which lives in the per-session scratch
//! directory. Sharing one instance across the sessions of a profile would put
//! every sibling's way out in the hands of whichever agent is compromised
//! first, and would make one session's "allow this domain?" answer silently
//! widen another's boundary. Duplicating the policy costs nothing.
//!
//! An instance is started *before* the agent launches, so nothing can race a
//! listener that is not bound yet; it is replaced on every relaunch, because
//! `Ctrl+R` re-derives the whole wrapper from the database and a profile edited
//! in between has to take effect; and it is stopped where the session's scratch
//! directory is dropped, so a socket cannot outlive its session.
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
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use tokio::sync::mpsc;

use crate::proxy::{DenialEvent, HostRule, Proxy, ProxyBind, ProxyConfig};
use crate::sandbox::backend::{ProxyEndpoint, ProxyTransport, SandboxError, SandboxResult};
use crate::session::{NetworkMode, SandboxPolicy};

/// File name of the unix socket inside a session's scratch directory.
pub const PROXY_SOCKET_NAME: &str = "proxy.sock";

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
/// only what it started itself. That is the tighter reading, and the one that
/// does not turn "allow 127.0.0.1" in a profile into a route to the host's own
/// loopback services.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyGrant {
    /// The one hole the backend's kernel policy must leave open.
    pub endpoint: ProxyEndpoint,
    /// The environment the agent needs in order to use it. Carries the
    /// instance's token inside the proxy URLs, so it is never logged.
    pub env: BTreeMap<String, String>,
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

/// Start (or replace) the egress proxy for one session.
///
/// `transport` comes from the chosen backend's
/// [`Caps::proxy_transport`](crate::sandbox::Caps::proxy_transport), and
/// `scratch` is the per-session directory friring minted for the launch — the
/// only place a unix socket may live, because it is private to friring, private
/// to the session, and dropped with it.
///
/// Replacing rather than reusing is deliberate: the profile may have been
/// edited between launches, and a fresh instance means a fresh token, so a
/// process left over from the previous launch cannot keep using the tunnel.
///
/// The scratch directory is writable by the sandbox, which is the point of it —
/// and the reason the socket cannot be hijacked from in there is worth stating.
/// While a launch is live the socket is a *mount point* inside the sandbox, so
/// unlinking it is `EBUSY`. Between launches the agent could plant something at
/// the path, and the next start refuses to touch it: [`Proxy::start`] replaces
/// only a socket that nothing is serving, and bails on anything else. Both ends
/// of that are fail-closed — the worst an agent achieves is refusing its own
/// next launch, and friring never connects to the socket itself.
///
/// # Errors
///
/// The policy carries a rule the proxy will not load, the socket path is longer
/// than any platform's `sun_path`, or the listener could not be bound. Every
/// one of them refuses the launch: a sandbox that silently has no filtered way
/// out is a session the user believes is proxied and is not.
pub fn establish(
    session_key: &str,
    policy: &SandboxPolicy,
    transport: ProxyTransport,
    scratch: &Path,
) -> SandboxResult<ProxyGrant> {
    let refuse = |detail: String| SandboxError::Refused {
        profile: policy.profile.clone(),
        detail,
    };
    let rules = proxy_policy(policy).map_err(|error| {
        refuse(format!(
            "the egress proxy refused this profile's domain rules: {error:#}"
        ))
    })?;

    let (bind, client, socket) = match transport {
        ProxyTransport::Loopback => (
            ProxyBind::tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            None,
            None,
        ),
        ProxyTransport::UnixSocket => {
            let socket = socket_path(scratch).map_err(&refuse)?;
            (
                ProxyBind::unix(&socket),
                // The agent dials the relay inside its own namespace; the
                // socket is what the relay forwards to.
                Some(relay_addr()),
                Some(socket),
            )
        }
    };

    let bound = supervisor()
        .start(
            session_key,
            ProxyConfig {
                bind,
                ..ProxyConfig::new(rules)
            },
            client,
        )
        .map_err(|error| {
            refuse(format!(
                "the egress proxy could not start, so this sandbox would have no filtered way \
                 out: {error}"
            ))
        })?;

    let endpoint = match (socket, bound.tcp) {
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
    Ok(ProxyGrant {
        endpoint,
        env: bound.env,
    })
}

/// Stop the proxy a session was given, if it has one.
///
/// Never starts the supervisor: a friring that has sandboxed nothing has
/// nothing to stop, and spawning a thread to say so would cost every session
/// the feature is not used for.
pub fn stop(session_key: &str) {
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

/// The socket path for a launch whose scratch directory is `scratch`.
fn socket_path(scratch: &Path) -> Result<String, String> {
    let socket = scratch.join(PROXY_SOCKET_NAME);
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
/// supervisor except inside those URLs.
#[derive(Debug)]
struct Bound {
    tcp: Option<SocketAddr>,
    env: BTreeMap<String, String>,
}

/// One instruction for the supervisor thread.
enum Command {
    Start {
        key: String,
        config: Box<ProxyConfig>,
        /// Where the *sandbox* dials, when that is not the proxy's own address
        /// — the relay's, for a namespaced backend.
        client: Option<SocketAddr>,
        reply: std::sync::mpsc::Sender<Result<Bound, String>>,
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

    fn start(
        &self,
        key: &str,
        config: ProxyConfig,
        client: Option<SocketAddr>,
    ) -> Result<Bound, String> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.send(Command::Start {
            key: key.to_string(),
            config: Box::new(config),
            client,
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
/// Serialising the commands is what makes replacement safe: a `Start` for a key
/// that already has an instance shuts the old one down — and unlinks its socket
/// — strictly before the new one binds the same path.
async fn serve(mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut proxies: HashMap<String, Proxy> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            Command::Start {
                key,
                config,
                client,
                reply,
            } => {
                if let Some(previous) = proxies.remove(&key) {
                    previous.shutdown().await;
                }
                let started = match Proxy::start(*config).await {
                    Ok(started) => started,
                    Err(error) => {
                        let _ = reply.send(Err(format!("{error:#}")));
                        continue;
                    }
                };
                let (proxy, denials) = started;
                let endpoint = client.or_else(|| proxy.tcp_addr());
                let bound = Bound {
                    tcp: proxy.tcp_addr(),
                    env: match endpoint {
                        Some(addr) => proxy_env(
                            &proxy.http_proxy_url_at(addr),
                            &proxy.socks5_proxy_url_at(addr),
                        ),
                        None => BTreeMap::new(),
                    },
                };
                tokio::spawn(collect_denials(key.clone(), denials));
                proxies.insert(key, proxy);
                let _ = reply.send(Ok(bound));
            }
            Command::Stop { key } => {
                if let Some(proxy) = proxies.remove(&key) {
                    proxy.shutdown().await;
                }
            }
            Command::StopAll { reply } => {
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
        // The wildcard is a spelling, so it arrives as the bare rule and still
        // covers the apex; the port scope survives.
        let allow: Vec<String> = rules
            .allow_rules()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(allow, ["github.com", "api.anthropic.com:443"]);
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
        let token = http
            .strip_prefix("http://friring:")
            .and_then(|rest| rest.split_once('@'))
            .map(|(token, _)| token)
            .unwrap_or_else(|| panic!("no credential in {http}"));
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
        assert_ne!(again.env.get("HTTP_PROXY"), grant.env.get("HTTP_PROXY"));
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
        let error = socket_path(&deep).unwrap_err();
        assert!(error.contains("at most 103"), "{error}");
        assert!(error.contains("XDG_DATA_HOME"), "{error}");
        // A realistic one fits.
        socket_path(Path::new(
            "/home/u/.local/share/friring/sandbox/tmp/session",
        ))
        .unwrap();
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
