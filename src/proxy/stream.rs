//! The two transports a sandbox reaches the proxy over, behind one type.
//!
//! Which transport a backend can use is decided by its network primitive, not
//! by preference:
//!
//! - **seatbelt** shares the host's network stack and merely denies non-loopback
//!   traffic by policy, so the sandbox dials host `127.0.0.1` directly.
//! - **bwrap `--unshare-net`** and a container on `--network none` get a *new*
//!   network namespace whose loopback is their own. Host `127.0.0.1` is
//!   unreachable from inside, and no TCP address would help. A unix socket is a
//!   filesystem object, so a bind mount carries it across the namespace
//!   boundary — which is why the proxy listens on one, and why
//!   [`super::relay`] exists to give the sandbox a TCP endpoint that forwards
//!   to it.
//!
//! Both transports run the identical handlers and the identical policy; only
//! the plumbing differs.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

/// A connection from a sandbox, on whichever transport it arrived.
pub(super) struct Client {
    transport: Transport,
    /// The byte consumed to identify the protocol, replayed on the next read.
    ///
    /// `TcpStream` can `peek`, but `UnixStream` cannot, so the sniff has to
    /// consume. Holding the byte here — rather than passing it to each handler
    /// — is what keeps the HTTP and SOCKS5 code unaware that a sniff happened.
    pushback: Option<u8>,
}

enum Transport {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

impl Client {
    /// Wrap an accepted TCP connection.
    pub(super) fn tcp(stream: TcpStream) -> Self {
        Self {
            transport: Transport::Tcp(stream),
            pushback: None,
        }
    }

    /// Wrap an accepted unix-socket connection.
    #[cfg(unix)]
    pub(super) fn unix(stream: UnixStream) -> Self {
        Self {
            transport: Transport::Unix(stream),
            pushback: None,
        }
    }

    /// Read the first byte and hold it for replay.
    ///
    /// `Ok(None)` means the peer hung up without sending anything, which is
    /// what a port scanner and a health check both look like.
    pub(super) async fn sniff(&mut self, within: std::time::Duration) -> io::Result<Option<u8>> {
        let mut first = [0u8; 1];
        let read = tokio::time::timeout(within, self.read(&mut first))
            .await
            .map_err(|_elapsed| {
                io::Error::new(io::ErrorKind::TimedOut, "proxy handshake timeout")
            })??;
        if read == 0 {
            return Ok(None);
        }
        self.pushback = Some(first[0]);
        Ok(Some(first[0]))
    }
}

impl AsyncRead for Client {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(byte) = this.pushback.take() {
            if buf.remaining() > 0 {
                buf.put_slice(&[byte]);
                return Poll::Ready(Ok(()));
            }
            this.pushback = Some(byte);
        }
        match &mut this.transport {
            Transport::Tcp(stream) => Pin::new(stream).poll_read(cx, buf),
            #[cfg(unix)]
            Transport::Unix(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Client {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.get_mut().transport {
            Transport::Tcp(stream) => Pin::new(stream).poll_write(cx, buf),
            #[cfg(unix)]
            Transport::Unix(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().transport {
            Transport::Tcp(stream) => Pin::new(stream).poll_flush(cx),
            #[cfg(unix)]
            Transport::Unix(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().transport {
            Transport::Tcp(stream) => Pin::new(stream).poll_shutdown(cx),
            #[cfg(unix)]
            Transport::Unix(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// A bound listener, on whichever transport it was configured for.
pub(super) enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
}

impl Listener {
    /// Accept the next connection.
    pub(super) async fn accept(&self) -> io::Result<Client> {
        match self {
            Self::Tcp(listener) => {
                let (stream, _peer) = listener.accept().await?;
                // Proxied traffic is request/response and latency-sensitive;
                // Nagle would delay every small write by up to 40 ms.
                let _ = stream.set_nodelay(true);
                Ok(Client::tcp(stream))
            }
            #[cfg(unix)]
            Self::Unix(listener) => {
                let (stream, _peer) = listener.accept().await?;
                Ok(Client::unix(stream))
            }
        }
    }
}

/// Accept from an optional listener, never resolving when there is none.
///
/// Lets the accept loop hold one `select!` arm per transport without a
/// `#[cfg]` in the loop itself: an unconfigured (or unsupported) transport is
/// simply a branch that never fires.
pub(super) async fn accept_next(listener: &Option<Listener>) -> io::Result<Client> {
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;

    /// The sniffed byte has to reach the handler, or every request would lose
    /// its first character.
    #[tokio::test]
    async fn the_sniffed_byte_is_replayed_to_the_handler() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("bound address");
        tokio::spawn(async move {
            let mut stream = TcpStream::connect(addr).await.expect("connect");
            stream.write_all(b"CONNECT host:443").await.expect("write");
        });
        let listener = Listener::Tcp(listener);
        let mut client = listener.accept().await.expect("accept");

        assert_eq!(
            client
                .sniff(std::time::Duration::from_secs(5))
                .await
                .expect("sniff"),
            Some(b'C')
        );
        let mut whole = Vec::new();
        client.read_to_end(&mut whole).await.expect("read the rest");
        assert_eq!(whole, b"CONNECT host:443");
    }

    #[tokio::test]
    async fn a_peer_that_sends_nothing_sniffs_as_a_closed_connection() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("bound address");
        tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.expect("connect");
            drop(stream);
        });
        let listener = Listener::Tcp(listener);
        let mut client = listener.accept().await.expect("accept");
        assert_eq!(
            client
                .sniff(std::time::Duration::from_secs(5))
                .await
                .expect("sniff"),
            None
        );
    }
}
