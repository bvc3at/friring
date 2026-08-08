//! The in-sandbox relay: a TCP endpoint inside the sandbox that forwards to
//! the proxy's unix socket.
//!
//! # Why this exists
//!
//! A sandbox created with `bwrap --unshare-net`, or a container on
//! `--network none`, has its own network namespace. Its `127.0.0.1` is *its*
//! loopback, not the host's, so no TCP address reaches a proxy running on the
//! host — there is no route, by construction, which is exactly the property
//! that makes the sandbox worth having.
//!
//! A unix socket crosses that boundary, because it is a filesystem object and
//! a bind mount carries it in. But no mainstream HTTP or SOCKS client can
//! *dial* a proxy over a unix socket: `HTTP_PROXY` and `ALL_PROXY` take a host
//! and a port. So something inside the namespace has to offer a TCP endpoint
//! and pass the bytes along:
//!
//! ```text
//! agent  →  127.0.0.1:PORT (inside the sandbox's own netns)
//!        →  friring-cli sandbox relay
//!        →  /path/to/proxy.sock  (bind-mounted from the host)
//!        →  friring proxy  →  policy  →  upstream
//! ```
//!
//! This is the same shape as the `socat` pair such setups usually reach for,
//! with the same protocol-agnostic behaviour — the relay never parses a byte,
//! so HTTP `CONNECT` and SOCKS5 both pass through unchanged, and the proxy's
//! token is still required at the far end. The relay holds no credential and
//! makes no policy decision: giving it either would put both inside the
//! boundary they exist to constrain.
//!
//! # Running it
//!
//! ```no_run
//! use std::path::PathBuf;
//! use friring::proxy::{Relay, RelayConfig};
//!
//! # async fn run() -> anyhow::Result<()> {
//! let mut relay = Relay::start(RelayConfig::new(
//!     "127.0.0.1:8118".parse()?,
//!     PathBuf::from("/run/friring/proxy.sock"),
//! ))
//! .await?;
//! println!("{}", relay.addr());
//! relay.wait().await;
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

use super::tunnel;

/// How to start a relay.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Address to listen on *inside* the sandbox. Loopback: the namespace has
    /// nothing else worth binding, and a routable bind would offer the proxy
    /// to whatever else shares the namespace.
    pub listen: SocketAddr,
    /// The proxy's unix socket, at its path inside the sandbox.
    pub socket: PathBuf,
    /// Ceiling on simultaneous connections, mirroring the proxy's own cap so
    /// the relay cannot be the component that exhausts descriptors.
    pub max_connections: usize,
    /// How long to wait for the unix socket to accept a connection.
    pub connect_timeout: Duration,
    /// How long a forwarded connection may go without a byte in either
    /// direction before it is torn down.
    pub idle_timeout: Duration,
}

impl RelayConfig {
    /// Default bounds, forwarding `listen` to `socket`.
    pub fn new(listen: SocketAddr, socket: impl Into<PathBuf>) -> Self {
        Self {
            listen,
            socket: socket.into(),
            max_connections: 64,
            connect_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(300),
        }
    }
}

/// A running relay.
#[derive(Debug)]
pub struct Relay {
    addr: SocketAddr,
    active: Arc<AtomicUsize>,
    shutdown: watch::Sender<bool>,
    accept: Option<JoinHandle<()>>,
}

impl Relay {
    /// Bind the TCP listener and start forwarding.
    ///
    /// The unix socket is *not* required to exist yet: the host proxy may
    /// still be starting, and a relay that refused to start over that would
    /// turn a race into a failed session. A connection that arrives before the
    /// socket is there is closed, and the next one tries again.
    ///
    /// # Errors
    ///
    /// If the TCP address cannot be bound.
    pub async fn start(config: RelayConfig) -> Result<Self> {
        let listener = TcpListener::bind(config.listen)
            .await
            .with_context(|| format!("binding the sandbox relay to {}", config.listen))?;
        let addr = listener
            .local_addr()
            .context("reading the sandbox relay's bound address")?;
        let active = Arc::new(AtomicUsize::new(0));
        let (shutdown, stop) = watch::channel(false);
        let accept = tokio::spawn(accept_loop(
            listener,
            config.clone(),
            Arc::clone(&active),
            stop,
        ));
        tracing::info!(%addr, socket = %config.socket.display(), "sandbox relay listening");
        Ok(Self {
            addr,
            active,
            shutdown,
            accept: Some(accept),
        })
    }

    /// The bound address, with the port the OS assigned.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Connections currently being forwarded.
    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Run until the relay stops, which normally means until the process is
    /// signalled. This is what a `friring-cli` entry point awaits.
    pub async fn wait(&mut self) {
        if let Some(accept) = self.accept.as_mut() {
            let _ = accept.await;
        }
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

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

/// Decrements the live-connection count however its task ends.
struct ActiveConnection(Arc<AtomicUsize>);

impl ActiveConnection {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::Relaxed);
        Self(active)
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn accept_loop(
    listener: TcpListener,
    config: RelayConfig,
    active: Arc<AtomicUsize>,
    mut stop: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(config.max_connections.max(1)));
    let mut connections = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            _ = stop.changed() => break,
            Some(finished) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = finished {
                    tracing::error!(%error, "sandbox relay connection task failed");
                }
                continue;
            }
            accepted = listener.accept() => accepted,
        };
        let (client, _peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::warn!(%error, "sandbox relay accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = client.set_nodelay(true);
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            tracing::warn!("sandbox relay connection cap reached; dropping a connection");
            continue;
        };
        connections.spawn(forward(client, config.clone(), Arc::clone(&active), permit));
    }
    connections.shutdown().await;
    tracing::debug!("sandbox relay stopped");
}

/// Forward one connection to the unix socket, holding its permit and
/// live-count entry for exactly as long as it runs.
async fn forward(
    client: TcpStream,
    config: RelayConfig,
    active: Arc<AtomicUsize>,
    permit: OwnedSemaphorePermit,
) {
    let _permit = permit;
    let _active = ActiveConnection::new(active);
    let upstream = match connect(&config.socket, config.connect_timeout).await {
        Ok(upstream) => upstream,
        Err(error) => {
            // Nothing protocol-aware can be said here: the relay does not know
            // whether the client is speaking HTTP or SOCKS, so the honest
            // answer is to close and let the client's own error surface.
            tracing::warn!(
                socket = %config.socket.display(),
                %error,
                "sandbox relay could not reach the proxy socket"
            );
            return;
        }
    };
    match tunnel::splice(client, upstream, config.idle_timeout).await {
        Ok(moved) => {
            tracing::debug!(
                sent = moved.sent,
                received = moved.received,
                "relayed connection closed"
            );
        }
        Err(error) => tracing::debug!(%error, "relayed connection ended"),
    }
}

async fn connect(socket: &Path, within: Duration) -> std::io::Result<UnixStream> {
    match timeout(within, UnixStream::connect(socket)).await {
        Ok(connected) => connected,
        Err(_elapsed) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out connecting to the proxy socket",
        )),
    }
}
