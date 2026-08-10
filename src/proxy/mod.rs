//! The sandbox egress proxy: a domain-filtering HTTP `CONNECT` and SOCKS5
//! proxy that Friring runs on the host for as long as a sandboxed agent is
//! alive.
//!
//! # Why a proxy at all
//!
//! Sandbox backends have wildly different network primitives — seatbelt can
//! express "all" or "localhost" and nothing in between, bubblewrap unshares the
//! network namespace, containers take `--network none` — and none of them can
//! filter by *name*. So every backend does the one thing it can do well (deny
//! direct egress at the kernel level) and points the sandbox at this proxy,
//! which is the single place a domain allowlist is enforced. Because the kernel
//! blocks everything else, a process that ignores the proxy environment
//! variables gets no network at all rather than an escape route. See ADR-27 in
//! `docs/SANDBOX.md`.
//!
//! # Using it
//!
//! ```no_run
//! use friring::proxy::{NetworkMode, Policy, Proxy, ProxyConfig};
//!
//! # async fn start() -> anyhow::Result<()> {
//! let policy = Policy::new(NetworkMode::Allowlist).with_allow(["api.anthropic.com"])?;
//! let (proxy, mut denials) = Proxy::start(ProxyConfig::new(policy)).await?;
//!
//! // Hand these to the sandbox as HTTP_PROXY / HTTPS_PROXY and ALL_PROXY.
//! // A unix-socket-only proxy has no URL of its own: the sandbox points at a
//! // `relay` instead, through `http_proxy_url_at`.
//! let _http = proxy.http_proxy_url();
//! let _socks = proxy.socks5_proxy_url();
//!
//! // Every refusal arrives here, for the TUI to notify or prompt on.
//! while let Some(denial) = denials.recv().await {
//!     eprintln!("blocked {}:{} — {}", denial.host, denial.port, denial.reason);
//! }
//! proxy.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! Both protocols share one listener: a SOCKS5 greeting starts with the byte
//! `0x05` and an HTTP request line with a method letter, so the first byte
//! selects the handler.
//!
//! # How a sandbox reaches it
//!
//! Two transports, because backends differ in what they can reach:
//!
//! - **TCP loopback**, for a sandbox that still shares the host's network
//!   stack (macOS seatbelt). The sandbox dials `127.0.0.1:<port>` directly.
//! - **A unix socket**, for a sandbox in its own network namespace (`bwrap
//!   --unshare-net`, a container on `--network none`). Host loopback does not
//!   exist in there, but a bind mount carries a socket across, and [`relay`]
//!   runs inside the namespace to give the agent's clients the TCP endpoint
//!   they insist on.
//!
//! Both apply the identical policy and demand the identical token.
//!
//! # What this does not do
//!
//! - **No TLS interception.** A `CONNECT` tunnel is opaque, so the allow
//!   decision trusts the hostname the *client* supplied. A client that opens a
//!   tunnel to an allowed host and then sends a different SNI or `Host` header
//!   — domain fronting — is not stopped by this proxy. Closing that gap means
//!   terminating TLS with a generated CA inside the sandbox, which this version
//!   deliberately does not do; the UI is expected to say so rather than imply
//!   containment it does not have.
//! - **The method restriction covers plaintext HTTP only**, for the same
//!   reason. See [`MethodPolicy::ReadOnly`].
//! - **No DNS filtering.** Names are resolved by the proxy after the decision,
//!   so a denied host is never even looked up, but a sandbox with its own DNS
//!   path can still resolve names — it just cannot connect anywhere with them.
//!
//! # Robustness
//!
//! The proxy is a network service inside a long-lived TUI, so every phase is
//! bounded: the request head is capped and read under a handshake timeout, the
//! upstream connect has its own timeout, an established tunnel is torn down
//! after an idle period, and a semaphore caps concurrent connections. Each
//! connection is one task in a [`tokio::task::JoinSet`]; a task that panics is
//! contained by the runtime and logged, and shutting the proxy down aborts and
//! awaits every one of them.

mod auth;
mod host;
mod http;
mod policy;
#[cfg(unix)]
pub mod relay;
mod socks5;
mod stream;
#[cfg(test)]
mod tests;
mod tunnel;

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

pub use auth::PROXY_USERNAME;
pub use policy::{Decision, DenyReason, HostRule, MethodPolicy, NetworkMode, Policy};
#[cfg(unix)]
pub use relay::{Relay, RelayConfig};
use stream::{Client, Listener};

/// Which protocol a refused request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP `CONNECT` or a forwarded plaintext request.
    Http,
    /// SOCKS5.
    Socks5,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Http => "http",
            Self::Socks5 => "socks5",
        })
    }
}

/// One refused request, for the TUI to turn into a notification or a
/// "allow this domain?" prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialEvent {
    /// Which listener the request arrived on.
    pub protocol: Protocol,
    /// The host as the client wrote it — a name, or an address literal.
    ///
    /// Empty when the refusal happened before a destination was named, which
    /// is the case for a SOCKS5 client that fails authentication: the protocol
    /// authenticates first and asks second.
    pub host: String,
    /// The requested port, or `0` alongside an empty [`DenialEvent::host`].
    pub port: u16,
    /// Why it was refused. [`DenyReason::NotAllowlisted`] is the one worth
    /// prompting on; the rest are decisions the user already made.
    pub reason: DenyReason,
}

/// A unix-socket listener: where to put the socket, and who may connect.
#[derive(Debug, Clone)]
pub struct UnixBind {
    /// Filesystem path for the socket. It is bind-mounted into the sandbox at
    /// this same path, so it must be somewhere Friring owns — a per-instance
    /// directory under the data directory, not a shared temporary directory.
    pub path: PathBuf,
    /// Mode applied to the socket file after binding.
    ///
    /// `0o600` by default: the socket is a credential-bearing endpoint, and
    /// while the token is still required, there is no reason for it to be
    /// connectable by every account on the host. A backend whose sandbox runs
    /// as a *different* uid (containers usually do) has to widen this
    /// deliberately, and should keep the parent directory private instead.
    pub mode: u32,
}

impl UnixBind {
    /// A socket at `path`, connectable only by the owning account.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mode: 0o600,
        }
    }

    /// Widen (or narrow) who may connect. See [`UnixBind::mode`].
    pub fn with_mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }
}

/// Which listeners a proxy instance offers.
///
/// At least one is required. Which one a backend needs is decided by its
/// network primitive: a sandbox sharing the host's network stack dials
/// loopback, while one in its own network namespace can only reach a
/// bind-mounted socket. Offering both at once is legitimate — a single Friring
/// proxy can serve a seatbelt sandbox and a bwrap sandbox at the same time.
#[derive(Debug, Clone, Default)]
pub struct ProxyBind {
    /// TCP address, if any. Loopback only in practice; binding anything
    /// routable would put a token-authenticated proxy on the network.
    pub tcp: Option<SocketAddr>,
    /// Unix socket, if any. Unix and WSL only; binding one elsewhere is an
    /// error rather than a silent no-op.
    pub unix: Option<UnixBind>,
}

impl ProxyBind {
    /// An ephemeral loopback port — what a seatbelt sandbox reaches directly.
    pub fn tcp(addr: SocketAddr) -> Self {
        Self {
            tcp: Some(addr),
            unix: None,
        }
    }

    /// A unix socket only — what a network-namespaced sandbox reaches through
    /// [`relay`], with no host port opened at all.
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self {
            tcp: None,
            unix: Some(UnixBind::new(path)),
        }
    }

    /// Both transports on one proxy instance.
    pub fn both(addr: SocketAddr, path: impl Into<PathBuf>) -> Self {
        Self {
            tcp: Some(addr),
            unix: Some(UnixBind::new(path)),
        }
    }
}

/// How to start a proxy instance.
///
/// The defaults are the intended configuration: loopback, an ephemeral port, a
/// freshly minted token, and a policy that denies everything until one is
/// supplied.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Which listeners to open. Defaults to an ephemeral loopback port.
    pub bind: ProxyBind,
    /// The egress policy, replaceable later through
    /// [`Proxy::update_policy`].
    pub policy: Policy,
    /// The credential the sandbox must present. `None` mints a fresh one,
    /// which is what every caller should do; supplying one exists for tests
    /// and for restoring a proxy across a Friring restart.
    pub token: Option<String>,
    /// Ceiling on simultaneous connections. Excess connections are dropped
    /// rather than queued, so a runaway client cannot exhaust the host's file
    /// descriptors through Friring.
    pub max_connections: usize,
    /// How long a client has to complete its request head or SOCKS handshake.
    pub handshake_timeout: Duration,
    /// How long to wait for the upstream TCP connection.
    pub connect_timeout: Duration,
    /// How long an established tunnel may go without a byte in either
    /// direction before it is torn down.
    pub idle_timeout: Duration,
    /// Denial events buffered before the oldest are dropped. Denials are
    /// logged as well, so a slow consumer loses notifications, never the
    /// record.
    pub event_capacity: usize,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            bind: ProxyBind::tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
            policy: Policy::default(),
            token: None,
            max_connections: 64,
            handshake_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(300),
            event_capacity: 128,
        }
    }
}

impl ProxyConfig {
    /// Default configuration enforcing `policy`.
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }
}

/// Per-connection bounds, snapshotted at start so a running proxy has no
/// configuration to re-read.
struct Limits {
    handshake_timeout: Duration,
    connect_timeout: Duration,
    idle_timeout: Duration,
}

/// State every connection task shares with the handle that owns it.
struct Shared {
    policy: RwLock<Policy>,
    token: String,
    events: mpsc::Sender<DenialEvent>,
    limits: Limits,
    active: AtomicUsize,
}

impl Shared {
    /// A poison-tolerant read: the critical sections here are a policy lookup
    /// and a clone, so a poisoned lock means an unrelated panic, not corrupt
    /// state. Refusing to serve at that point would fail closed on the *proxy*
    /// while the sandbox keeps running, which is the worse failure.
    fn read_policy(&self) -> RwLockReadGuard<'_, Policy> {
        self.policy.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_policy(&self) -> RwLockWriteGuard<'_, Policy> {
        self.policy.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The host is canonical already: both protocol handlers canonicalise at
    /// their edge, so the policy is asked about the same spelling the socket
    /// will be opened to.
    fn decide(&self, host: &host::CanonicalHost, port: u16) -> Decision {
        self.read_policy().decide_host(host, port)
    }

    fn decide_method(&self, method: &str) -> Decision {
        self.read_policy().decide_method(method)
    }

    fn token(&self) -> &str {
        &self.token
    }

    fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Record a refusal: always to the log, and to the event channel when the
    /// consumer is keeping up.
    fn report(&self, protocol: Protocol, host: &str, port: u16, reason: DenyReason) {
        let host = displayable_host(host);
        tracing::info!(%protocol, host, port, %reason, "sandbox proxy refused a request");
        let event = DenialEvent {
            protocol,
            host,
            port,
            reason,
        };
        if self.events.try_send(event).is_err() {
            // Never block a connection on a slow or departed consumer; the log
            // line above is the durable record.
            tracing::debug!("sandbox proxy denial event dropped");
        }
    }
}

/// Longest host recorded on an event: the DNS name limit, which no legitimate
/// request exceeds.
const MAX_REPORTED_HOST: usize = 255;

/// Make a client-supplied host safe to render.
///
/// The host on a denial is chosen by the *sandboxed* process and ends up in a
/// TUI notification and a log line, so it must not be able to carry escape
/// sequences into the host's terminal or an unbounded string into a modal.
/// Well-formed hosts pass through untouched, which keeps the string usable as
/// the answer to a "remember this domain" prompt.
fn displayable_host(host: &str) -> String {
    host.chars()
        .take(MAX_REPORTED_HOST)
        .map(|character| {
            if character.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                character
            }
        })
        .collect()
}

/// Decrements the live-connection count however its task ends — including an
/// abort during shutdown, which no explicit decrement would survive.
struct ActiveConnection(Arc<Shared>);

impl ActiveConnection {
    fn new(shared: Arc<Shared>) -> Self {
        shared.active.fetch_add(1, Ordering::Relaxed);
        Self(shared)
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A running proxy instance.
///
/// Dropping the handle stops the listeners and removes the unix socket.
/// [`Proxy::shutdown`] does the same and waits for every connection task to
/// finish, which is what a caller tearing down a sandbox wants.
pub struct Proxy {
    tcp_addr: Option<SocketAddr>,
    unix_path: Option<PathBuf>,
    shared: Arc<Shared>,
    shutdown: watch::Sender<bool>,
    accept: Option<JoinHandle<()>>,
}

impl Proxy {
    /// Bind the configured listeners and start serving.
    ///
    /// Returns the handle and the stream of [`DenialEvent`]s. Binding happens
    /// before this returns, so [`Proxy::tcp_addr`] and [`Proxy::unix_path`]
    /// are immediately usable to build the sandbox's environment.
    ///
    /// # Errors
    ///
    /// If no listener was configured, if an address or socket path cannot be
    /// bound, or if a unix socket was asked for on a platform without them.
    pub async fn start(config: ProxyConfig) -> Result<(Self, mpsc::Receiver<DenialEvent>)> {
        let tcp = match config.bind.tcp {
            Some(addr) => Some(
                TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("binding the sandbox proxy to {addr}"))?,
            ),
            None => None,
        };
        let tcp_addr = match &tcp {
            Some(listener) => Some(
                listener
                    .local_addr()
                    .context("reading the sandbox proxy's bound address")?,
            ),
            None => None,
        };
        let unix = match &config.bind.unix {
            Some(bind) => Some(bind_unix_socket(bind)?),
            None => None,
        };
        let unix_path = config.bind.unix.as_ref().map(|bind| bind.path.clone());
        if tcp.is_none() && unix.is_none() {
            anyhow::bail!("the sandbox proxy needs at least one listener: a TCP address, a unix socket, or both");
        }

        let (events, receiver) = mpsc::channel(config.event_capacity.max(1));
        let shared = Arc::new(Shared {
            policy: RwLock::new(config.policy),
            token: config.token.unwrap_or_else(auth::generate_token),
            events,
            limits: Limits {
                handshake_timeout: config.handshake_timeout,
                connect_timeout: config.connect_timeout,
                idle_timeout: config.idle_timeout,
            },
            active: AtomicUsize::new(0),
        });
        let (shutdown, stop) = watch::channel(false);
        let accept = tokio::spawn(accept_loop(
            tcp.map(Listener::Tcp),
            unix,
            Arc::clone(&shared),
            stop,
            config.max_connections.max(1),
        ));
        tracing::info!(
            tcp = ?tcp_addr,
            unix = ?unix_path,
            "sandbox egress proxy listening"
        );
        Ok((
            Self {
                tcp_addr,
                unix_path,
                shared,
                shutdown,
                accept: Some(accept),
            },
            receiver,
        ))
    }

    /// The bound TCP address, with the port the OS assigned, or `None` for a
    /// unix-socket-only proxy.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_addr
    }

    /// The bound unix socket path, or `None` for a TCP-only proxy.
    pub fn unix_path(&self) -> Option<&Path> {
        self.unix_path.as_deref()
    }

    /// The credential a client must present. Never log this.
    pub fn token(&self) -> &str {
        self.shared.token()
    }

    /// The value for `HTTP_PROXY` / `HTTPS_PROXY` inside a sandbox that can
    /// reach the host's loopback — seatbelt. `None` when no TCP listener is
    /// bound, in which case the sandbox reaches a [`relay`] and the URL comes
    /// from [`Proxy::http_proxy_url_at`].
    ///
    /// The credential rides in the URL because that is the only channel every
    /// HTTP client understands; each derives the `Proxy-Authorization: Basic`
    /// header from it on its own.
    pub fn http_proxy_url(&self) -> Option<String> {
        self.tcp_addr.map(|addr| self.http_proxy_url_at(addr))
    }

    /// The value for `ALL_PROXY`, for a sandbox that can reach host loopback.
    ///
    /// `socks5h` rather than `socks5`: the `h` asks the client to send the
    /// *hostname* and let the proxy resolve it, which is what the allowlist
    /// needs to see. With plain `socks5` the client resolves first and the
    /// proxy is handed an address it cannot match against a domain rule — the
    /// allowlist then fails open or closed depending on the rule, and either
    /// way is not the policy the user wrote.
    pub fn socks5_proxy_url(&self) -> Option<String> {
        self.tcp_addr.map(|addr| self.socks5_proxy_url_at(addr))
    }

    /// `HTTP_PROXY` / `HTTPS_PROXY` pointing at `endpoint` rather than at this
    /// proxy's own address — the sandbox-side address of a [`relay`], which is
    /// inside the sandbox's own network namespace and forwards to this proxy's
    /// unix socket. The token is this proxy's, since the relay only moves bytes.
    pub fn http_proxy_url_at(&self, endpoint: SocketAddr) -> String {
        format!("http://{PROXY_USERNAME}:{}@{endpoint}", self.token())
    }

    /// `ALL_PROXY` pointing at a [`relay`]'s address. See
    /// [`Proxy::socks5_proxy_url`] for why this is `socks5h`.
    pub fn socks5_proxy_url_at(&self, endpoint: SocketAddr) -> String {
        format!("socks5h://{PROXY_USERNAME}:{}@{endpoint}", self.token())
    }

    /// A snapshot of the policy in force.
    pub fn policy(&self) -> Policy {
        self.shared.read_policy().clone()
    }

    /// Replace the policy. Takes effect on the next request; connections
    /// already spliced are not interrupted, since a decision that was correct
    /// when made stays made.
    pub fn set_policy(&self, policy: Policy) {
        *self.shared.write_policy() = policy;
    }

    /// Edit the policy in place — how a "remember this domain" answer reaches
    /// a running proxy without a restart.
    ///
    /// ```no_run
    /// # use friring::proxy::{HostRule, Proxy};
    /// # fn remember(proxy: &Proxy, answer: HostRule) {
    /// proxy.update_policy(|policy| policy.allow_rule(answer));
    /// # }
    /// ```
    pub fn update_policy<F: FnOnce(&mut Policy)>(&self, edit: F) {
        edit(&mut self.shared.write_policy());
    }

    /// Connections currently being served. Zero once every client has gone
    /// away, which is what makes a leaked task observable.
    pub fn active_connections(&self) -> usize {
        self.shared.active.load(Ordering::Relaxed)
    }

    /// Stop listening, abort every live connection, wait for the tasks to
    /// finish, and remove the unix socket.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(accept) = self.accept.take() {
            let _ = accept.await;
        }
        self.remove_socket();
    }

    /// Unlink the unix socket. Nothing removes it for us — a listener's socket
    /// file outlives the process that bound it, and a leftover file is what
    /// makes the *next* start fail.
    fn remove_socket(&self) {
        if let Some(path) = &self.unix_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // A dropped handle must not leave a listener bound or a socket file
        // behind; the accept loop sees the signal and tears its connections
        // down on its own.
        let _ = self.shutdown.send(true);
        self.remove_socket();
    }
}

impl fmt::Debug for Proxy {
    /// Deliberately hand-written: the token must never appear here. `Debug`
    /// output reaches logs, panic messages and `expect` failures, none of
    /// which should carry a sandbox's credential.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("tcp_addr", &self.tcp_addr)
            .field("unix_path", &self.unix_path)
            .field("active_connections", &self.active_connections())
            .finish_non_exhaustive()
    }
}

/// Bind a unix socket, clearing a stale one and applying the configured mode.
#[cfg(unix)]
fn bind_unix_socket(bind: &UnixBind) -> Result<Listener> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    clear_stale_socket(&bind.path)?;
    if let Some(parent) = bind.path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            // Only when we create it: a directory holding a credential-bearing
            // socket is ours alone. An existing directory's mode is the
            // caller's decision, not ours to rewrite.
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .with_context(|| format!("creating the socket directory `{}`", parent.display()))?;
        }
    }
    let listener = tokio::net::UnixListener::bind(&bind.path)
        .with_context(|| format!("binding the sandbox proxy to `{}`", bind.path.display()))?;
    // `bind` applies the umask, which is the user's setting rather than a
    // decision about this socket; set the mode explicitly instead.
    std::fs::set_permissions(&bind.path, std::fs::Permissions::from_mode(bind.mode))
        .with_context(|| format!("restricting `{}`", bind.path.display()))?;
    Ok(Listener::Unix(listener))
}

#[cfg(not(unix))]
fn bind_unix_socket(bind: &UnixBind) -> Result<Listener> {
    anyhow::bail!(
        "unix-socket listeners are not available on this platform (asked for `{}`); \
         sandboxes here are reached through WSL or a container, which have their own",
        bind.path.display()
    )
}

/// Remove a socket file left behind by a previous run, and refuse to touch
/// anything else.
///
/// A crashed Friring leaves its socket file in place, and the next start would
/// fail with "address already in use" forever. Deleting it blindly would be
/// worse — it could unlink a *live* proxy's socket — so liveness is tested
/// first, by connecting. The connect is synchronous, which is fine for a unix
/// socket: it either succeeds immediately or fails immediately.
#[cfg(unix)]
fn clear_stale_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt as _;

    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.file_type().is_socket() {
        anyhow::bail!(
            "`{}` already exists and is not a socket; refusing to replace it",
            path.display()
        );
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        anyhow::bail!("`{}` is already served by a running proxy", path.display());
    }
    std::fs::remove_file(path)
        .with_context(|| format!("removing the stale socket `{}`", path.display()))
}

/// Accept connections on every configured transport until told to stop, then
/// abort what is still running.
async fn accept_loop(
    tcp: Option<Listener>,
    unix: Option<Listener>,
    shared: Arc<Shared>,
    mut stop: watch::Receiver<bool>,
    max_connections: usize,
) {
    let permits = Arc::new(Semaphore::new(max_connections));
    let mut connections = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            _ = stop.changed() => break,
            // Reap finished tasks so a long-lived proxy does not accumulate
            // their handles. Guarded because an empty set yields `None`
            // immediately, which would spin the loop.
            Some(finished) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = finished {
                    // A panicking connection is contained by the runtime;
                    // logging it is what keeps it from being invisible.
                    tracing::error!(%error, "sandbox proxy connection task failed");
                }
                continue;
            }
            accepted = stream::accept_next(&tcp) => accepted,
            accepted = stream::accept_next(&unix) => accepted,
        };
        let client = match accepted {
            Ok(client) => client,
            Err(error) => {
                // Per-connection failures (a descriptor limit, a peer gone
                // between SYN and accept) must not end the proxy.
                tracing::warn!(%error, "sandbox proxy accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            tracing::warn!("sandbox proxy connection cap reached; dropping a connection");
            continue;
        };
        connections.spawn(serve(client, Arc::clone(&shared), permit));
    }
    connections.shutdown().await;
    tracing::debug!("sandbox egress proxy stopped");
}

/// Serve one accepted connection, holding its permit and live-count entry for
/// exactly as long as it runs.
async fn serve(client: Client, shared: Arc<Shared>, permit: OwnedSemaphorePermit) {
    let _permit = permit;
    let _active = ActiveConnection::new(Arc::clone(&shared));
    if let Err(error) = dispatch(client, shared).await {
        // Client-side resets are the normal end of a tunnel, so this is debug,
        // not warn.
        tracing::debug!(%error, "sandbox proxy connection ended");
    }
}

/// Route a connection to the handler its first byte identifies.
async fn dispatch(mut client: Client, shared: Arc<Shared>) -> io::Result<()> {
    match client.sniff(shared.limits().handshake_timeout).await? {
        Some(socks5::VERSION) => socks5::serve(client, shared).await,
        // Anything else is treated as HTTP, which answers a malformed request
        // with `400` instead of a silent close.
        Some(_) => http::serve(client, shared).await,
        None => Ok(()),
    }
}
