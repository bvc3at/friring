//! Splicing an approved connection: bidirectional copy with fixed buffers and
//! an idle timeout.
//!
//! Once a request is allowed the proxy stops interpreting bytes and becomes a
//! pipe. Two properties matter for a long-lived orchestrator: the pipe must
//! cost a bounded amount of memory no matter how much traffic crosses it, and
//! it must end — on either peer closing, on either peer erroring, or on going
//! quiet for longer than the sandbox's idle timeout.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::time::Instant;

/// Bytes moved per read. Two of these (one per direction) is the entire
/// memory cost of a spliced connection, which is what keeps a large download
/// from being buffered anywhere in Friring.
const COPY_BUFFER: usize = 16 * 1024;

/// How much crossed the tunnel, for the debug log that closes a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Transferred {
    /// Bytes the sandbox sent upstream.
    pub(super) sent: u64,
    /// Bytes the upstream sent back.
    pub(super) received: u64,
}

/// Copy between `client` and `upstream` until both directions finish.
///
/// Generic over both sides because the pairing differs by caller: the proxy
/// splices a sandbox connection (TCP or unix) to an upstream TCP socket, and
/// the relay splices a sandbox TCP connection to the proxy's unix socket.
///
/// A direction that reaches EOF shuts down the *other* socket's write half, so
/// a peer doing a half-close still receives the rest of the response instead of
/// hanging until a timeout. An error in either direction ends the whole
/// tunnel: the futures are dropped together, which closes both sockets and
/// leaves nothing running behind a client that vanished mid-transfer.
///
/// # Errors
///
/// The first I/O error from either direction, or [`io::ErrorKind::TimedOut`]
/// when no byte crossed in either direction for `idle_timeout`. The timeout is
/// on *inactivity*, not on total duration: a slow download and a long-polling
/// request are both legitimate and must not be cut off mid-stream.
pub(super) async fn splice<C, U>(
    client: C,
    upstream: U,
    idle_timeout: Duration,
) -> io::Result<Transferred>
where
    C: AsyncRead + AsyncWrite,
    U: AsyncRead + AsyncWrite,
{
    let (from_client, to_client) = tokio::io::split(client);
    let (from_upstream, to_upstream) = tokio::io::split(upstream);
    let started = Instant::now();
    let last_activity = AtomicU64::new(0);
    let copy = async {
        tokio::try_join!(
            pump(from_client, to_upstream, &last_activity, started),
            pump(from_upstream, to_client, &last_activity, started),
        )
    };
    tokio::select! {
        result = copy => {
            let (sent, received) = result?;
            Ok(Transferred { sent, received })
        }
        () = watch_for_idle(&last_activity, started, idle_timeout) => {
            Err(io::Error::new(io::ErrorKind::TimedOut, "proxy tunnel idle timeout"))
        }
    }
}

/// Copy one direction, recording activity for the shared idle watchdog.
async fn pump<R, W>(
    mut reader: R,
    mut writer: W,
    last_activity: &AtomicU64,
    started: Instant,
) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Heap-allocated so the spawned connection future stays small.
    let mut buffer = vec![0u8; COPY_BUFFER];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            // Propagate the half-close instead of letting the peer wait: a
            // shutdown error here only means the peer is already gone.
            let _ = writer.shutdown().await;
            return Ok(total);
        }
        writer.write_all(&buffer[..read]).await?;
        total += read as u64;
        last_activity.store(elapsed_millis(started), Ordering::Relaxed);
    }
}

/// Resolve once both directions have been quiet for `idle_timeout`.
///
/// Polling a shared timestamp rather than arming a timer per read keeps the
/// hot path to one relaxed store, and the coarseness is irrelevant to a
/// timeout measured in minutes.
async fn watch_for_idle(last_activity: &AtomicU64, started: Instant, idle_timeout: Duration) {
    let interval = (idle_timeout / 4).max(Duration::from_millis(20));
    loop {
        tokio::time::sleep(interval).await;
        let quiet_since = Duration::from_millis(last_activity.load(Ordering::Relaxed));
        if started.elapsed().saturating_sub(quiet_since) >= idle_timeout {
            return;
        }
    }
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};

    /// A connected loopback pair, standing in for the client and upstream
    /// sockets without any test needing a real network.
    async fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("bound address");
        let connect = tokio::spawn(async move { TcpStream::connect(addr).await });
        let (accepted, _) = listener.accept().await.expect("accept");
        let dialled = connect.await.expect("join").expect("connect");
        (dialled, accepted)
    }

    #[tokio::test]
    async fn bytes_cross_in_both_directions() {
        let (client, client_peer) = socket_pair().await;
        let (upstream_peer, upstream) = socket_pair().await;
        let tunnel = tokio::spawn(splice(client_peer, upstream_peer, Duration::from_secs(5)));

        let (mut client, mut upstream) = (client, upstream);
        client.write_all(b"ping").await.expect("write up");
        let mut received = [0u8; 4];
        upstream.read_exact(&mut received).await.expect("read up");
        assert_eq!(&received, b"ping");

        upstream.write_all(b"pong").await.expect("write down");
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.expect("read down");
        assert_eq!(&echoed, b"pong");

        drop(client);
        drop(upstream);
        let moved = tunnel.await.expect("join").expect("clean end");
        assert_eq!(
            moved,
            Transferred {
                sent: 4,
                received: 4
            }
        );
    }

    /// A client that closes its write half must still receive the response —
    /// the shape of every `curl` request that sends no body.
    #[tokio::test]
    async fn half_close_upstream_still_delivers_the_reply() {
        let (client, client_peer) = socket_pair().await;
        let (upstream_peer, upstream) = socket_pair().await;
        let tunnel = tokio::spawn(splice(client_peer, upstream_peer, Duration::from_secs(5)));

        let (mut client, mut upstream) = (client, upstream);
        client.write_all(b"request").await.expect("write");
        client.shutdown().await.expect("half close");

        let mut request = Vec::new();
        upstream
            .read_to_end(&mut request)
            .await
            .expect("read to eof");
        assert_eq!(request, b"request");

        upstream.write_all(b"reply").await.expect("reply");
        drop(upstream);
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.expect("read reply");
        assert_eq!(reply, b"reply");
        tunnel.await.expect("join").expect("clean end");
    }

    #[tokio::test]
    async fn a_silent_tunnel_is_torn_down() {
        let (_client, client_peer) = socket_pair().await;
        let (upstream_peer, _upstream) = socket_pair().await;
        let error = splice(client_peer, upstream_peer, Duration::from_millis(60))
            .await
            .expect_err("idle tunnels end");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
