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
//! Both protocols share one loopback port: a SOCKS5 greeting starts with the
//! byte `0x05` and an HTTP request line with a method letter, so the first byte
//! selects the handler without being consumed.
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
//! - **No unix-socket listener.** The design sketch allowed for one, but no
//!   mainstream HTTP or SOCKS client can dial a proxy over a unix socket, so a
//!   sandbox could not actually use it. Loopback plus the per-instance token is
//!   the shipped baseline; a unix socket becomes worth adding when a backend
//!   appears that cannot reach host loopback at all.
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
mod http;
mod policy;
mod socks5;
#[cfg(test)]
mod tests;
mod tunnel;

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

pub use auth::PROXY_USERNAME;
pub use policy::{Decision, DenyReason, HostRule, MethodPolicy, NetworkMode, Policy};

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

/// How to start a proxy instance.
///
/// The defaults are the intended configuration: loopback, an ephemeral port, a
/// freshly minted token, and a policy that denies everything until one is
/// supplied.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Address to listen on. Defaults to `127.0.0.1:0` — an ephemeral port on
    /// loopback, which is the only interface a sandbox needs and the only one
    /// that keeps the listener off the network.
    pub bind: SocketAddr,
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
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
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

    fn decide(&self, host: &str, port: u16) -> Decision {
        self.read_policy().decide(host, port)
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
/// Dropping the handle stops the listener. [`Proxy::shutdown`] does the same
/// and waits for every connection task to finish, which is what a caller
/// tearing down a sandbox wants.
pub struct Proxy {
    addr: SocketAddr,
    shared: Arc<Shared>,
    shutdown: watch::Sender<bool>,
    accept: Option<JoinHandle<()>>,
}

impl Proxy {
    /// Bind the listener and start serving.
    ///
    /// Returns the handle and the stream of [`DenialEvent`]s. Binding happens
    /// before this returns, so [`Proxy::addr`] is immediately usable to build
    /// the sandbox's environment.
    ///
    /// # Errors
    ///
    /// If the address cannot be bound.
    pub async fn start(config: ProxyConfig) -> Result<(Self, mpsc::Receiver<DenialEvent>)> {
        let listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("binding the sandbox proxy to {}", config.bind))?;
        let addr = listener
            .local_addr()
            .context("reading the sandbox proxy's bound address")?;
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
            listener,
            Arc::clone(&shared),
            stop,
            config.max_connections.max(1),
        ));
        tracing::info!(%addr, "sandbox egress proxy listening");
        Ok((
            Self {
                addr,
                shared,
                shutdown,
                accept: Some(accept),
            },
            receiver,
        ))
    }

    /// The bound address, with the port the OS assigned.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The credential a client must present. Never log this.
    pub fn token(&self) -> &str {
        self.shared.token()
    }

    /// The value for `HTTP_PROXY` / `HTTPS_PROXY` inside the sandbox.
    ///
    /// The credential rides in the URL because that is the only channel every
    /// HTTP client understands; each derives the `Proxy-Authorization: Basic`
    /// header from it on its own.
    pub fn http_proxy_url(&self) -> String {
        format!("http://{PROXY_USERNAME}:{}@{}", self.token(), self.addr)
    }

    /// The value for `ALL_PROXY`.
    ///
    /// `socks5h` rather than `socks5`: the `h` asks the client to send the
    /// *hostname* and let the proxy resolve it, which is what the allowlist
    /// needs to see. With plain `socks5` the client resolves first and the
    /// proxy is handed an address it cannot match against a domain rule.
    pub fn socks5_proxy_url(&self) -> String {
        format!("socks5h://{PROXY_USERNAME}:{}@{}", self.token(), self.addr)
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

    /// Stop listening, abort every live connection, and wait for the tasks to
    /// finish.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(accept) = self.accept.take() {
            let _ = accept.await;
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // A dropped handle must not leave a listener bound; the accept loop
        // sees this and tears its connections down on its own.
        let _ = self.shutdown.send(true);
    }
}

/// Accept connections until told to stop, then abort what is still running.
async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    mut stop: watch::Receiver<bool>,
    max_connections: usize,
) {
    let permits = Arc::new(Semaphore::new(max_connections));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
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
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        // Per-connection failures (a descriptor limit, a peer
                        // gone between SYN and accept) must not end the proxy.
                        tracing::warn!(%error, "sandbox proxy accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    tracing::warn!(%peer, "sandbox proxy connection cap reached; dropping");
                    continue;
                };
                let shared = Arc::clone(&shared);
                connections.spawn(serve(stream, shared, permit));
            }
        }
    }
    connections.shutdown().await;
    tracing::debug!("sandbox egress proxy stopped");
}

/// Serve one accepted connection, holding its permit and live-count entry for
/// exactly as long as it runs.
async fn serve(stream: TcpStream, shared: Arc<Shared>, permit: OwnedSemaphorePermit) {
    let _permit = permit;
    let _active = ActiveConnection::new(Arc::clone(&shared));
    if let Err(error) = dispatch(stream, shared).await {
        // Client-side resets are the normal end of a tunnel, so this is debug,
        // not warn.
        tracing::debug!(%error, "sandbox proxy connection ended");
    }
}

/// Route a connection to the handler its first byte identifies.
async fn dispatch(stream: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    match sniff(&stream, shared.limits().handshake_timeout).await? {
        Some(socks5::VERSION) => socks5::serve(stream, shared).await,
        // Anything else is treated as HTTP, which answers a malformed request
        // with `400` instead of a silent close.
        Some(_) => http::serve(stream, shared).await,
        None => Ok(()),
    }
}

/// Peek the first byte without consuming it, so the chosen handler still sees
/// a complete stream. `None` means the client hung up first.
async fn sniff(stream: &TcpStream, within: Duration) -> io::Result<Option<u8>> {
    let mut first = [0u8; 1];
    let read = timeout(within, stream.peek(&mut first))
        .await
        .map_err(|_elapsed| io::Error::new(io::ErrorKind::TimedOut, "proxy handshake timeout"))??;
    Ok((read > 0).then_some(first[0]))
}
