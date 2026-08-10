//! The SOCKS5 half of the proxy (RFC 1928, with RFC 1929 authentication).
//!
//! SOCKS is what an agent's tooling reaches for when it is not speaking HTTP —
//! `ALL_PROXY`, `git`'s `ssh -o ProxyCommand`, and language runtimes that
//! ignore `HTTPS_PROXY`. The proxy accepts only the username/password method,
//! so an unauthenticated client is refused during negotiation, before it can
//! name a destination.
//!
//! Only `CONNECT` is implemented. `BIND` and `UDP ASSOCIATE` exist to let a
//! *remote* peer open a channel back to the client, which is the opposite of
//! what a sandbox is for; both are answered with "command not supported".
//!
//! Domain names are resolved by the proxy, not the client (`socks5h://`
//! semantics), because a policy that only ever saw a resolved address could
//! not enforce a domain allowlist at all.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::time::timeout;

use super::auth;
use super::host::{self, CanonicalHost};
use super::policy::{Decision, DenyReason};
use super::stream::Client;
use super::{Protocol, Shared};

/// The only protocol version this proxy speaks, and the byte that identifies a
/// SOCKS greeting to [`super::sniff`].
pub(super) const VERSION: u8 = 0x05;

const AUTH_USERNAME_PASSWORD: u8 = 0x02;
const AUTH_NO_ACCEPTABLE_METHOD: u8 = 0xFF;
/// RFC 1929's sub-negotiation carries its own version number, unrelated to
/// SOCKS's.
const AUTH_SUBNEGOTIATION_VERSION: u8 = 0x01;
const AUTH_FAILURE: u8 = 0x01;

const COMMAND_CONNECT: u8 = 0x01;

const ADDRESS_IPV4: u8 = 0x01;
const ADDRESS_DOMAIN: u8 = 0x03;
const ADDRESS_IPV6: u8 = 0x04;

// Reply codes, RFC 1928 §6.
const REPLY_SUCCESS: u8 = 0x00;
const REPLY_GENERAL_FAILURE: u8 = 0x01;
/// "Connection not allowed by ruleset" — the policy denial.
const REPLY_NOT_ALLOWED: u8 = 0x02;
const REPLY_HOST_UNREACHABLE: u8 = 0x04;
const REPLY_CONNECTION_REFUSED: u8 = 0x05;
const REPLY_TTL_EXPIRED: u8 = 0x06;
const REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const REPLY_ADDRESS_NOT_SUPPORTED: u8 = 0x08;

/// Where a client asked to be connected.
struct Target {
    host: String,
    port: u16,
}

/// Serve one client connection that begins with a SOCKS5 greeting.
pub(super) async fn serve(mut client: Client, shared: Arc<Shared>) -> io::Result<()> {
    let limits = shared.limits();
    let target = match timeout(limits.handshake_timeout, handshake(&mut client, &shared)).await {
        Ok(Ok(Some(target))) => target,
        // Refused during negotiation: the client already has its reply.
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(error)) => return Err(error),
        // A stalled handshake has no protocol-legal reply to send.
        Err(_elapsed) => return Ok(()),
    };

    // The host as the client spelled it — a SOCKS5 domain field carries
    // whatever bytes the client chose, so it is canonicalised at this edge
    // before any rule sees it, and the canonical form is what gets dialled. An
    // address field is already an address and needs none of this; a *domain*
    // field spelling one (`127.1`) is exactly the dodge this closes.
    let (asked, port) = (target.host.as_str(), target.port);
    let host = match CanonicalHost::parse(asked) {
        Ok(host) => host,
        Err(fault) => {
            shared.report(
                Protocol::Socks5,
                asked,
                port,
                DenyReason::UnsupportedHost(fault.detail()),
            );
            return refuse(&mut client, REPLY_NOT_ALLOWED).await;
        }
    };
    if let Decision::Deny(reason) = shared.decide(&host, port) {
        shared.report(Protocol::Socks5, asked, port, reason);
        return refuse(&mut client, REPLY_NOT_ALLOWED).await;
    }

    let upstream = match timeout(limits.connect_timeout, host::connect(&host, port)).await {
        Ok(Ok(upstream)) => upstream,
        Ok(Err(error)) => return refuse(&mut client, reply_code_for(&error)).await,
        Err(_elapsed) => return refuse(&mut client, REPLY_TTL_EXPIRED).await,
    };
    let _ = upstream.set_nodelay(true);

    // The bound address is informational; a client that cannot read it back
    // still gets a well-formed reply.
    reply(&mut client, REPLY_SUCCESS, upstream.local_addr().ok()).await?;
    let moved = super::tunnel::splice(client, upstream, limits.idle_timeout).await?;
    tracing::debug!(
        sent = moved.sent,
        received = moved.received,
        "proxy tunnel closed"
    );
    Ok(())
}

/// Run method selection, authentication and the connect request.
///
/// `Ok(None)` means the client was answered and refused; the caller closes.
async fn handshake(client: &mut Client, shared: &Shared) -> io::Result<Option<Target>> {
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    if greeting[0] != VERSION {
        return Ok(None);
    }
    let mut methods = vec![0u8; usize::from(greeting[1])];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&AUTH_USERNAME_PASSWORD) {
        // Anything that will not authenticate is not the sandbox this proxy
        // was started for.
        client
            .write_all(&[VERSION, AUTH_NO_ACCEPTABLE_METHOD])
            .await?;
        shared.report(Protocol::Socks5, "", 0, DenyReason::Unauthorized);
        return close(client).await;
    }
    client.write_all(&[VERSION, AUTH_USERNAME_PASSWORD]).await?;

    if !authenticate(client, shared).await? {
        client
            .write_all(&[AUTH_SUBNEGOTIATION_VERSION, AUTH_FAILURE])
            .await?;
        shared.report(Protocol::Socks5, "", 0, DenyReason::Unauthorized);
        return close(client).await;
    }
    client
        .write_all(&[AUTH_SUBNEGOTIATION_VERSION, REPLY_SUCCESS])
        .await?;

    let mut request = [0u8; 4];
    client.read_exact(&mut request).await?;
    if request[0] != VERSION {
        return close(client).await;
    }
    if request[1] != COMMAND_CONNECT {
        reply(client, REPLY_COMMAND_NOT_SUPPORTED, None).await?;
        return close(client).await;
    }
    let host = match request[3] {
        ADDRESS_IPV4 => {
            let mut octets = [0u8; 4];
            client.read_exact(&mut octets).await?;
            Ipv4Addr::from(octets).to_string()
        }
        ADDRESS_IPV6 => {
            let mut octets = [0u8; 16];
            client.read_exact(&mut octets).await?;
            Ipv6Addr::from(octets).to_string()
        }
        ADDRESS_DOMAIN => {
            let mut length = [0u8; 1];
            client.read_exact(&mut length).await?;
            let mut name = vec![0u8; usize::from(length[0])];
            client.read_exact(&mut name).await?;
            match String::from_utf8(name) {
                Ok(name) => name,
                Err(_) => {
                    reply(client, REPLY_GENERAL_FAILURE, None).await?;
                    return close(client).await;
                }
            }
        }
        _ => {
            reply(client, REPLY_ADDRESS_NOT_SUPPORTED, None).await?;
            return close(client).await;
        }
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port).await?;
    Ok(Some(Target {
        host,
        port: u16::from_be_bytes(port),
    }))
}

/// Read the RFC 1929 username/password pair and check the password against the
/// instance token. The username is read and discarded — the token is the
/// credential.
async fn authenticate(client: &mut Client, shared: &Shared) -> io::Result<bool> {
    let mut header = [0u8; 2];
    client.read_exact(&mut header).await?;
    if header[0] != AUTH_SUBNEGOTIATION_VERSION {
        return Ok(false);
    }
    let mut username = vec![0u8; usize::from(header[1])];
    client.read_exact(&mut username).await?;
    let mut length = [0u8; 1];
    client.read_exact(&mut length).await?;
    let mut password = vec![0u8; usize::from(length[0])];
    client.read_exact(&mut password).await?;
    // The token is hex; anything that is not valid UTF-8 cannot be it.
    Ok(std::str::from_utf8(&password)
        .is_ok_and(|password| auth::secret_eq(password, shared.token())))
}

/// Send a failure reply and close, so a refused client sees the code rather
/// than a reset.
async fn refuse(client: &mut Client, code: u8) -> io::Result<()> {
    reply(client, code, None).await?;
    client.shutdown().await
}

async fn close(client: &mut Client) -> io::Result<Option<Target>> {
    client.shutdown().await?;
    Ok(None)
}

/// Write a reply carrying the proxy's bound address, or the unspecified IPv4
/// address when there is none to report (every failure case).
async fn reply(client: &mut Client, code: u8, bound: Option<SocketAddr>) -> io::Result<()> {
    let mut message = vec![VERSION, code, 0x00];
    match bound {
        Some(SocketAddr::V4(bound)) => {
            message.push(ADDRESS_IPV4);
            message.extend_from_slice(&bound.ip().octets());
            message.extend_from_slice(&bound.port().to_be_bytes());
        }
        Some(SocketAddr::V6(bound)) => {
            message.push(ADDRESS_IPV6);
            message.extend_from_slice(&bound.ip().octets());
            message.extend_from_slice(&bound.port().to_be_bytes());
        }
        None => {
            message.push(ADDRESS_IPV4);
            message.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        }
    }
    client.write_all(&message).await
}

fn reply_code_for(error: &io::Error) -> u8 {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => REPLY_CONNECTION_REFUSED,
        io::ErrorKind::TimedOut => REPLY_HOST_UNREACHABLE,
        _ => REPLY_GENERAL_FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    async fn reply_bytes(code: u8, bound: Option<SocketAddr>) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("bound");
        let writer = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.expect("connect");
            let mut client = Client::tcp(stream);
            reply(&mut client, code, bound).await.expect("reply");
        });
        let (mut accepted, _) = listener.accept().await.expect("accept");
        let mut received = Vec::new();
        accepted.read_to_end(&mut received).await.expect("read");
        writer.await.expect("join");
        received
    }

    #[tokio::test]
    async fn a_success_reply_carries_the_bound_address() {
        let bound = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 4242));
        let bytes = reply_bytes(REPLY_SUCCESS, Some(bound)).await;
        assert_eq!(bytes, vec![5, 0, 0, 1, 127, 0, 0, 1, 0x10, 0x92]);
    }

    #[tokio::test]
    async fn a_denial_reply_is_the_ruleset_code_with_a_null_address() {
        let bytes = reply_bytes(REPLY_NOT_ALLOWED, None).await;
        assert_eq!(bytes, vec![5, 2, 0, 1, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn upstream_errors_map_onto_distinguishable_reply_codes() {
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert_eq!(reply_code_for(&refused), REPLY_CONNECTION_REFUSED);
        let timed_out = io::Error::from(io::ErrorKind::TimedOut);
        assert_eq!(reply_code_for(&timed_out), REPLY_HOST_UNREACHABLE);
        let other = io::Error::from(io::ErrorKind::Other);
        assert_eq!(reply_code_for(&other), REPLY_GENERAL_FAILURE);
    }
}
