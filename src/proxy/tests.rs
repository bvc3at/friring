//! End-to-end tests: a real proxy in front of a local test server, driven over
//! real loopback sockets.
//!
//! Nothing here reaches the network or reads any credential. Every upstream is
//! an in-process server on an ephemeral loopback port, every token is minted by
//! the proxy under test, and hosts that must be *denied* are refused before a
//! name is ever resolved — which is also what makes those tests hermetic.

use std::net::Ipv6Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use super::*;

/// Short timeouts everywhere: a test that hangs should fail, not stall the
/// suite for the production defaults.
fn test_config(policy: Policy) -> ProxyConfig {
    ProxyConfig {
        handshake_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        ..ProxyConfig::new(policy)
    }
}

async fn start(policy: Policy) -> (Proxy, mpsc::Receiver<DenialEvent>) {
    Proxy::start(test_config(policy))
        .await
        .expect("the proxy binds a loopback port")
}

/// An allowlist covering only the loopback interface, which is where every
/// test server lives.
fn loopback_only() -> Policy {
    Policy::new(NetworkMode::Allowlist)
        .with_allow(["127.0.0.1", "localhost"])
        .expect("valid rules")
}

fn bearer(proxy: &Proxy) -> String {
    format!("Bearer {}", proxy.token())
}

fn tcp_addr(proxy: &Proxy) -> SocketAddr {
    proxy.tcp_addr().expect("a TCP listener is bound")
}

/// A loopback server that echoes whatever it is sent, standing in for an
/// upstream inside a `CONNECT` tunnel.
async fn spawn_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => {
                            if stream.write_all(&buffer[..read]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// A loopback HTTP server recording the request heads it is given, for the
/// plaintext-forwarding tests.
struct HttpServer {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<String>>>,
}

impl HttpServer {
    async fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("bound address");
        let received = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&received);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut byte).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) => head.push(byte[0]),
                        }
                    }
                    recorder
                        .lock()
                        .expect("recorder lock")
                        .push(String::from_utf8_lossy(&head).into_owned());
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nhi",
                        )
                        .await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { addr, received }
    }

    fn heads(&self) -> Vec<String> {
        self.received.lock().expect("recorder lock").clone()
    }
}

/// Read an HTTP response head one byte at a time, so nothing belonging to the
/// tunnel behind it is swallowed.
///
/// Every client helper below is generic over the transport: the same
/// assertions have to hold whether the sandbox reached the proxy over loopback
/// or over its unix socket, and writing them twice would let the two drift.
async fn read_response_head<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).await.expect("reading the response");
        assert_ne!(
            read,
            0,
            "the proxy closed without a response: {}",
            String::from_utf8_lossy(&head)
        );
        head.push(byte[0]);
    }
    String::from_utf8(head).expect("an ASCII response head")
}

async fn read_body<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut body = Vec::new();
    stream
        .read_to_end(&mut body)
        .await
        .expect("reading the body");
    String::from_utf8(body).expect("a UTF-8 body")
}

/// Write a request and read back the response head.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, request: &str) -> String {
    stream
        .write_all(request.as_bytes())
        .await
        .expect("writing the request");
    read_response_head(stream).await
}

fn connect_request(target: &str, credential: Option<&str>) -> String {
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(credential) = credential {
        request.push_str(&format!("Proxy-Authorization: {credential}\r\n"));
    }
    request.push_str("\r\n");
    request
}

async fn send_request(proxy: &Proxy, request: &str) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(tcp_addr(proxy))
        .await
        .expect("dialling the proxy");
    let head = exchange(&mut stream, request).await;
    (stream, head)
}

async fn send_connect(
    proxy: &Proxy,
    target: &str,
    credential: Option<&str>,
) -> (TcpStream, String) {
    send_request(proxy, &connect_request(target, credential)).await
}

/// Bounce a byte string off the echo server through an open tunnel.
async fn assert_tunnels<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, payload: &[u8]) {
    stream
        .write_all(payload)
        .await
        .expect("writing into the tunnel");
    let mut echoed = vec![0u8; payload.len()];
    stream
        .read_exact(&mut echoed)
        .await
        .expect("reading back out of the tunnel");
    assert_eq!(echoed, payload);
}

async fn next_denial(denials: &mut mpsc::Receiver<DenialEvent>) -> DenialEvent {
    tokio::time::timeout(Duration::from_secs(5), denials.recv())
        .await
        .expect("a denial event arrives")
        .expect("the event channel stays open")
}

async fn wait_until(limit: Duration, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while started.elapsed() < limit {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition still unmet after {limit:?}");
}

// SOCKS5 client steps, kept separate so a test can stop after any one of them.

const SOCKS_AUTH_USERNAME_PASSWORD: u8 = 0x02;

async fn socks_greet_on<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    methods: &[u8],
) -> [u8; 2] {
    let mut greeting = vec![0x05, u8::try_from(methods.len()).expect("few methods")];
    greeting.extend_from_slice(methods);
    stream
        .write_all(&greeting)
        .await
        .expect("writing the greeting");
    let mut chosen = [0u8; 2];
    stream
        .read_exact(&mut chosen)
        .await
        .expect("reading the method");
    chosen
}

async fn socks_greet(proxy: &Proxy, methods: &[u8]) -> (TcpStream, [u8; 2]) {
    let mut stream = TcpStream::connect(tcp_addr(proxy))
        .await
        .expect("dialling the proxy");
    let chosen = socks_greet_on(&mut stream, methods).await;
    (stream, chosen)
}

/// The full SOCKS5 client sequence on an already-connected stream, for the
/// transports that do not dial a TCP address.
async fn socks_tunnel_on<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    token: &str,
    host: &str,
    port: u16,
) -> [u8; 10] {
    let chosen = socks_greet_on(stream, &[SOCKS_AUTH_USERNAME_PASSWORD]).await;
    assert_eq!(chosen, [0x05, SOCKS_AUTH_USERNAME_PASSWORD]);
    assert_eq!(socks_authenticate(stream, token).await, [0x01, 0x00]);
    socks_request(stream, host, port).await
}

async fn socks_authenticate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    password: &str,
) -> [u8; 2] {
    let user = PROXY_USERNAME.as_bytes();
    let mut message = vec![0x01, u8::try_from(user.len()).expect("short username")];
    message.extend_from_slice(user);
    message.push(u8::try_from(password.len()).expect("short password"));
    message.extend_from_slice(password.as_bytes());
    stream
        .write_all(&message)
        .await
        .expect("writing credentials");
    let mut status = [0u8; 2];
    stream
        .read_exact(&mut status)
        .await
        .expect("reading the auth reply");
    status
}

async fn socks_request<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    host: &str,
    port: u16,
) -> [u8; 10] {
    let mut message = vec![0x05, 0x01, 0x00, 0x03];
    message.push(u8::try_from(host.len()).expect("short host"));
    message.extend_from_slice(host.as_bytes());
    message.extend_from_slice(&port.to_be_bytes());
    stream
        .write_all(&message)
        .await
        .expect("writing the request");
    let mut reply = [0u8; 10];
    stream
        .read_exact(&mut reply)
        .await
        .expect("reading the reply");
    reply
}

/// The full happy path, up to an open tunnel.
async fn socks_tunnel(proxy: &Proxy, host: &str, port: u16) -> (TcpStream, [u8; 10]) {
    let (mut stream, chosen) = socks_greet(proxy, &[SOCKS_AUTH_USERNAME_PASSWORD]).await;
    assert_eq!(chosen, [0x05, SOCKS_AUTH_USERNAME_PASSWORD]);
    assert_eq!(
        socks_authenticate(&mut stream, proxy.token()).await,
        [0x01, 0x00]
    );
    let reply = socks_request(&mut stream, host, port).await;
    (stream, reply)
}

#[tokio::test]
async fn an_allowed_connect_tunnels_bytes_both_ways() {
    let echo = spawn_echo_server().await;
    let (proxy, _denials) = start(loopback_only()).await;
    let (mut tunnel, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_tunnels(&mut tunnel, b"ping").await;
    assert_tunnels(&mut tunnel, b"a longer second write").await;
    proxy.shutdown().await;
}

/// The proxy's own URL is what a sandbox is configured with, so the credential
/// it publishes must be the one the proxy accepts.
#[tokio::test]
async fn the_published_basic_credential_is_accepted() {
    let echo = spawn_echo_server().await;
    let (proxy, _denials) = start(loopback_only()).await;
    let url = proxy.http_proxy_url().expect("a TCP proxy has a URL");
    assert!(url.starts_with(&format!("http://{PROXY_USERNAME}:")));
    assert!(url.ends_with(&tcp_addr(&proxy).to_string()));
    assert!(proxy
        .socks5_proxy_url()
        .expect("a TCP proxy has a URL")
        .starts_with("socks5h://"));

    // What a client derives from that URL: base64("friring:<token>").
    let pair = format!("{PROXY_USERNAME}:{}", proxy.token());
    let encoded = base64_for_test(pair.as_bytes());
    let credential = format!("Basic {encoded}");
    let (_tunnel, head) = send_connect(&proxy, &echo.to_string(), Some(&credential)).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    proxy.shutdown().await;
}

fn base64_for_test(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let mut group = [0u8; 3];
        group[..chunk.len()].copy_from_slice(chunk);
        let triple = u32::from_be_bytes([0, group[0], group[1], group[2]]);
        for shift in [18, 12, 6, 0] {
            out.push(ALPHABET[((triple >> shift) & 0x3f) as usize] as char);
        }
        let dropped = 3 - chunk.len();
        out.truncate(out.len() - dropped);
        out.extend(std::iter::repeat('=').take(dropped));
    }
    out
}

#[tokio::test]
async fn a_denied_host_gets_403_naming_the_reason() {
    let (proxy, mut denials) = start(loopback_only()).await;
    let (mut stream, head) = send_connect(&proxy, "blocked.test:443", Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 Forbidden"), "{head}");
    let body = read_body(&mut stream).await;
    assert!(body.contains("not in the sandbox allowlist"), "{body}");

    let denial = next_denial(&mut denials).await;
    assert_eq!(denial.protocol, Protocol::Http);
    assert_eq!(denial.host, "blocked.test");
    assert_eq!(denial.port, 443);
    assert_eq!(denial.reason, DenyReason::NotAllowlisted);
    proxy.shutdown().await;
}

/// The host on a denial is chosen by the sandboxed process and is rendered by
/// the TUI, so it must not be able to carry an escape sequence out of the
/// sandbox and into the host's terminal.
#[tokio::test]
async fn a_reported_host_cannot_smuggle_control_characters() {
    let (proxy, mut denials) = start(loopback_only()).await;
    let (_stream, head) =
        send_connect(&proxy, "\u{1b}[2Jevil.test:443", Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    let denial = next_denial(&mut denials).await;
    assert_eq!(denial.host, "\u{fffd}[2Jevil.test");
    assert!(!denial.host.chars().any(char::is_control));
    proxy.shutdown().await;
}

#[test]
fn long_hosts_are_capped_and_well_formed_ones_pass_through() {
    assert_eq!(displayable_host("api.github.com"), "api.github.com");
    assert_eq!(displayable_host(&"x".repeat(4096)).chars().count(), 255);
}

/// The negative case of subdomain matching, over the wire: a host that merely
/// ends with an allowed name is refused before it is even resolved.
#[tokio::test]
async fn a_lookalike_of_an_allowed_domain_is_refused() {
    let policy = Policy::new(NetworkMode::Allowlist)
        .with_allow(["example.test"])
        .expect("valid rules");
    let (proxy, mut denials) = start(policy).await;
    let (_stream, head) = send_connect(&proxy, "evilexample.test:443", Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert_eq!(next_denial(&mut denials).await.host, "evilexample.test");
    proxy.shutdown().await;
}

/// The deny-list gap, on the wire: `getaddrinfo(3)` reads `127.1` and
/// `2130706433` as `127.0.0.1` and would have connected there, so a spelling
/// that walked past the rule would be an unfiltered route to the denied host.
/// Both protocols canonicalise at their own edge, so both are checked.
#[tokio::test]
async fn an_address_spelled_differently_does_not_walk_past_a_deny_rule() {
    let policy = Policy::new(NetworkMode::Full)
        .with_deny(["127.0.0.1"])
        .expect("valid rules");
    let (proxy, mut denials) = start(policy).await;

    let (_stream, head) = send_connect(&proxy, "127.1:80", Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    let denial = next_denial(&mut denials).await;
    // The event carries the spelling the sandbox chose — that is what the user
    // needs to see — while the reason names the rule it actually hit.
    assert_eq!(denial.host, "127.1");
    assert_eq!(
        denial.reason,
        DenyReason::DeniedByRule("127.0.0.1".to_string())
    );

    let (_socks, reply) = socks_tunnel(&proxy, "2130706433", 80).await;
    assert_eq!(reply[1], 0x02, "connection not allowed by ruleset");
    let denial = next_denial(&mut denials).await;
    assert_eq!(denial.host, "2130706433");
    assert_eq!(
        denial.reason,
        DenyReason::DeniedByRule("127.0.0.1".to_string())
    );
    proxy.shutdown().await;
}

/// A host with no canonical spelling is refused with a reason of its own,
/// under `full` — where no rule would have stopped it — and the refusal names
/// the fix. Reporting it as merely unlisted would raise the first-use prompt
/// and offer to store a rule the profile validator refuses.
#[tokio::test]
async fn an_international_host_is_refused_with_the_punycode_fix_named() {
    let (proxy, mut denials) = start(Policy::new(NetworkMode::Full)).await;
    let (mut stream, head) =
        send_connect(&proxy, "b\u{fc}cher.example:443", Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    let body = read_body(&mut stream).await;
    assert!(body.contains("punycode"), "{body}");

    let denial = next_denial(&mut denials).await;
    assert_eq!(denial.host, "b\u{fc}cher.example");
    assert!(
        matches!(denial.reason, DenyReason::UnsupportedHost(_)),
        "{:?}",
        denial.reason
    );

    let (_socks, reply) = socks_tunnel(&proxy, "b\u{fc}cher.example", 443).await;
    assert_eq!(reply[1], 0x02, "connection not allowed by ruleset");
    assert!(matches!(
        next_denial(&mut denials).await.reason,
        DenyReason::UnsupportedHost(_)
    ));
    proxy.shutdown().await;
}

/// The other side of canonicalising: an allowed address stays reachable
/// whichever way it was spelled, and the bytes prove the proxy dialled the
/// address it decided on rather than handing the spelling back to the resolver.
#[tokio::test]
async fn an_allowlisted_address_is_reachable_by_any_spelling() {
    let echo = spawn_echo_server().await;
    let (proxy, _denials) = start(loopback_only()).await;
    for spelling in ["127.1", "2130706433", "0x7f.0.0.1"] {
        let target = format!("{spelling}:{}", echo.port());
        let (mut tunnel, head) = send_connect(&proxy, &target, Some(&bearer(&proxy))).await;
        assert!(head.starts_with("HTTP/1.1 200 "), "{spelling}: {head}");
        assert_tunnels(&mut tunnel, spelling.as_bytes()).await;
    }
    proxy.shutdown().await;
}

/// The positive case, over the wire: an allowlisted *name* is resolved by the
/// proxy and connected to. `localhost` is the one name that resolves without a
/// network.
#[tokio::test]
async fn an_allowlisted_name_is_resolved_and_connected() {
    let echo = spawn_echo_server().await;
    let (proxy, _denials) = start(loopback_only()).await;
    let target = format!("localhost:{}", echo.port());
    let (mut tunnel, head) = send_connect(&proxy, &target, Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_tunnels(&mut tunnel, b"named").await;
    proxy.shutdown().await;
}

#[tokio::test]
async fn http_without_the_token_is_refused_before_any_policy_check() {
    let (proxy, mut denials) = start(loopback_only()).await;
    for credential in [None, Some("Bearer wrong"), Some("Basic bm9wZTpub3Bl")] {
        // A host that *is* allowed: the refusal must still be about the token.
        let (mut stream, head) = send_connect(&proxy, "127.0.0.1:1", credential).await;
        assert!(
            head.starts_with("HTTP/1.1 407 Proxy Authentication Required"),
            "{credential:?} produced {head}"
        );
        assert!(head.contains("Proxy-Authenticate: Basic"), "{head}");
        let body = read_body(&mut stream).await;
        assert!(body.contains("proxy credentials"), "{body}");
        assert_eq!(
            next_denial(&mut denials).await.reason,
            DenyReason::Unauthorized
        );
    }
    proxy.shutdown().await;
}

#[tokio::test]
async fn network_mode_none_refuses_even_an_allowlisted_host() {
    let policy = Policy::new(NetworkMode::None)
        .with_allow(["127.0.0.1"])
        .expect("valid rules");
    let (proxy, mut denials) = start(policy).await;
    let (mut stream, head) = send_connect(&proxy, "127.0.0.1:1", Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert!(read_body(&mut stream)
        .await
        .contains("network access is disabled"));
    assert_eq!(
        next_denial(&mut denials).await.reason,
        DenyReason::NetworkDisabled
    );
    proxy.shutdown().await;
}

/// The seatbelt hole is one port, and the kernel cannot be told which family
/// it means.
///
/// `(allow network-outbound (remote ip "localhost:<port>"))` is a semantic
/// loopback predicate covering `::1` as well as `127.0.0.1`, and SBPL rejects a
/// literal address, so the profile cannot be narrowed. A listener on `[::1]:P`
/// does not conflict with one on `127.0.0.1:P`, so unless the proxy claims
/// both, the sandbox's single hole can reach whatever local service happens to
/// own the other one — with no token and no policy.
#[tokio::test]
async fn the_loopback_port_belongs_to_the_proxy_on_both_families() {
    let (proxy, _denials) = start(loopback_only()).await;
    let port = tcp_addr(&proxy).port();

    // Nothing else can take the companion: the proxy is holding it.
    let stolen = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await;
    assert!(
        stolen.is_err(),
        "[::1]:{port} was free for something other than the proxy"
    );

    // And it is served, not merely held — a sandbox that dials it is talking
    // to the proxy, so it meets the same token demand.
    let mut client = TcpStream::connect((Ipv6Addr::LOCALHOST, port))
        .await
        .expect("the companion accepts");
    client
        .write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n")
        .await
        .expect("write");
    let mut head = vec![0u8; 64];
    let read = client.read(&mut head).await.expect("read");
    let head = String::from_utf8_lossy(&head[..read]).into_owned();
    assert!(head.starts_with("HTTP/1.1 407 "), "{head}");
    proxy.shutdown().await;
}

/// `full` consults no allow list for an ordinary destination — asserted on the
/// decision, because every upstream a hermetic test can reach is on loopback
/// and loopback is the one thing `full` does *not* carry (below).
#[test]
fn network_mode_full_allows_a_host_no_rule_mentions() {
    let policy = Policy::new(NetworkMode::Full);
    assert_eq!(policy.decide("example.test", 443), Decision::Allow);
    assert_eq!(policy.decide("192.0.2.10", 443), Decision::Allow);
}

/// The same thing on the wire, with the host-local exception that is the only
/// reason an in-process upstream is reachable at all.
#[tokio::test]
async fn network_mode_full_tunnels_without_a_domain_rule() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(NetworkMode::Full)
        .with_allow(["127.0.0.1"])
        .expect("valid rules");
    let (proxy, _denials) = start(policy).await;
    let (mut tunnel, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_tunnels(&mut tunnel, b"open").await;
    proxy.shutdown().await;
}

/// The route `--unshare-net` exists to remove, offered back by the proxy.
///
/// A `full`-with-denies profile is exactly the shape P2 newly routes through
/// the proxy, and the sandbox it wraps has no network stack of its own — so a
/// `CONNECT 127.0.0.1:<port>` that the proxy honoured would reach a service on
/// the *host*: a rootless container API, a language server, an SSH forward.
/// Both protocols are driven, because both dial.
#[tokio::test]
async fn a_full_sandbox_is_not_lent_the_hosts_own_loopback() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(NetworkMode::Full)
        .with_deny(["telemetry.example"])
        .expect("valid rules");
    let (proxy, mut denials) = start(policy).await;

    let (mut stream, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert!(read_body(&mut stream)
        .await
        .contains("local to the machine"));
    assert_eq!(
        next_denial(&mut denials).await.reason,
        DenyReason::HostLocal
    );

    let (_socks, reply) = socks_tunnel(&proxy, "127.0.0.1", echo.port()).await;
    assert_eq!(reply[1], 0x02, "connection not allowed by ruleset");
    assert_eq!(
        next_denial(&mut denials).await.reason,
        DenyReason::HostLocal
    );
    proxy.shutdown().await;
}

/// The same refusal reached through a *name*, which is the half a decision
/// taken before the resolver answers cannot make: `localhost` is allowlisted
/// here, matches, and is still refused once its address is known. A name whose
/// record points inward — `127.0.0.1.nip.io`, a rebinding TTL on an allowed
/// domain — lands on this same check, without the test needing a resolver that
/// answers for one.
#[tokio::test]
async fn a_name_that_resolves_inward_is_refused_after_it_resolves() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(NetworkMode::Allowlist)
        .with_allow(["localhost"])
        .expect("valid rules");
    let (proxy, mut denials) = start(policy).await;
    let target = format!("localhost:{}", echo.port());

    let (_stream, head) = send_connect(&proxy, &target, Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    let denial = next_denial(&mut denials).await;
    assert_eq!(denial.host, "localhost");
    assert_eq!(denial.reason, DenyReason::HostLocal);
    proxy.shutdown().await;
}

/// The one way in, and its shape: the literal address, written by the user
/// into the allow list, and scoped to the port they wrote. A rule for another
/// port on the same address does not carry it.
#[tokio::test]
async fn an_address_rule_is_the_only_grant_of_a_local_service() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(NetworkMode::Allowlist)
        .with_allow([format!("127.0.0.1:{}", echo.port())])
        .expect("valid rules");
    let (proxy, mut denials) = start(policy).await;

    let (mut tunnel, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_tunnels(&mut tunnel, b"granted").await;

    let elsewhere = format!("127.0.0.1:{}", echo.port() + 1);
    let (_refused, head) = send_connect(&proxy, &elsewhere, Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert_eq!(
        next_denial(&mut denials).await.reason,
        DenyReason::HostLocal
    );
    proxy.shutdown().await;
}

/// `*` is the proxy's own "every host" rule, and a *deny* entry only ever
/// narrows. Neither is the user naming an address, so neither opens one.
#[test]
fn neither_a_wildcard_nor_a_deny_entry_opens_a_local_service() {
    let wildcard = Policy::new(NetworkMode::Allowlist)
        .with_allow(["*"])
        .expect("valid rules");
    assert_eq!(
        wildcard.decide("127.0.0.1", 2375),
        Decision::Deny(DenyReason::HostLocal)
    );
    let denied = Policy::new(NetworkMode::Full)
        .with_deny(["127.0.0.1"])
        .expect("valid rules");
    assert_eq!(
        denied.decide("127.0.0.1", 2375),
        Decision::Deny(DenyReason::DeniedByRule("127.0.0.1".into()))
    );
}

/// Every spelling of a host-local destination, and the addresses that are not
/// one. A sandbox picks the spelling, so `127.1` and `::ffff:127.0.0.1` have to
/// land where `127.0.0.1` does — and the user's own network is not refused,
/// because that is their network rather than their machine.
#[test]
fn every_spelling_of_a_local_destination_is_refused_and_no_others_are() {
    let policy = Policy::new(NetworkMode::Full);
    for host in [
        "127.0.0.1",
        "127.1",
        "2130706433",
        "0x7f.0.0.1",
        "[::1]",
        "[::ffff:127.0.0.1]",
        "0.0.0.0",
        "[::]",
        "169.254.169.254",
        "[fe80::1]",
    ] {
        assert_eq!(
            policy.decide(host, 80),
            Decision::Deny(DenyReason::HostLocal),
            "{host} reached the host's own stack"
        );
    }
    for host in [
        "10.0.0.1",
        "192.168.1.5",
        "172.16.0.1",
        "[fd00::1]",
        "8.8.8.8",
    ] {
        assert_eq!(policy.decide(host, 80), Decision::Allow, "{host}");
    }
}

#[tokio::test]
async fn a_deny_rule_beats_full_access() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(NetworkMode::Full)
        .with_deny(["127.0.0.1"])
        .expect("valid rules");
    let (proxy, mut denials) = start(policy).await;
    let (mut stream, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert!(read_body(&mut stream)
        .await
        .contains("deny rule `127.0.0.1`"));
    assert_eq!(
        next_denial(&mut denials).await.reason,
        DenyReason::DeniedByRule("127.0.0.1".into())
    );
    proxy.shutdown().await;
}

/// The "remember this domain" answer: a live proxy learns a new rule and the
/// next connection is allowed, with nothing restarted.
#[tokio::test]
async fn a_policy_update_takes_effect_without_a_restart() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(NetworkMode::Allowlist)
        .with_allow(["example.test"])
        .expect("valid rules");
    let (proxy, _denials) = start(policy).await;
    let target = echo.to_string();

    let (_refused, head) = send_connect(&proxy, &target, Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");

    proxy.update_policy(|policy| policy.allow_rule("127.0.0.1".parse().expect("valid rule")));
    assert_eq!(proxy.policy().allow_rules().len(), 2);

    let (mut tunnel, head) = send_connect(&proxy, &target, Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_tunnels(&mut tunnel, b"now allowed").await;

    // And a wholesale replacement closes it again.
    proxy.set_policy(Policy::new(NetworkMode::None));
    let (_refused, head) = send_connect(&proxy, &target, Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    proxy.shutdown().await;
}

#[tokio::test]
async fn a_plaintext_request_is_forwarded_without_this_hop_s_headers() {
    let server = HttpServer::spawn().await;
    let (proxy, _denials) = start(loopback_only()).await;
    let request = format!(
        "GET http://{}/hello HTTP/1.1\r\nHost: {}\r\nProxy-Authorization: {}\r\nX-Trace: keep\r\n\r\n",
        server.addr,
        server.addr,
        bearer(&proxy),
    );
    let (mut stream, head) = send_request(&proxy, &request).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    assert_eq!(read_body(&mut stream).await, "hi");

    let seen = server.heads();
    let forwarded = seen.first().expect("the upstream saw the request");
    assert!(
        forwarded.starts_with("GET /hello HTTP/1.1\r\n"),
        "{forwarded}"
    );
    assert!(forwarded.contains("x-trace: keep"), "{forwarded}");
    assert!(
        !forwarded
            .to_ascii_lowercase()
            .contains("proxy-authorization"),
        "the credential must not reach upstream: {forwarded}"
    );
    proxy.shutdown().await;
}

/// Domain fronting over plaintext: the policy authorises the authority in the
/// absolute-form URI, so a `Host` header naming somewhere else would have the
/// proxy dial the allowed origin and ask it to serve a different virtual host.
/// Inside a `CONNECT` tunnel the proxy cannot see that; here it can, so the
/// header it forwards is the authority it authorised.
#[tokio::test]
async fn a_plaintext_request_cannot_front_a_second_host_past_the_policy() {
    let server = HttpServer::spawn().await;
    let (proxy, _denials) = start(loopback_only()).await;
    let request = format!(
        "GET http://{}/hello HTTP/1.1\r\nHost: exfil.attacker.example\r\nProxy-Authorization: {}\r\n\r\n",
        server.addr,
        bearer(&proxy),
    );
    let (_stream, head) = send_request(&proxy, &request).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");

    let seen = server.heads();
    let forwarded = seen.first().expect("the upstream saw the request");
    assert!(
        !forwarded.to_ascii_lowercase().contains("attacker.example"),
        "the client's host header was forwarded: {forwarded}"
    );
    assert!(
        forwarded
            .to_ascii_lowercase()
            .contains(&format!("host: {}\r\n", server.addr)),
        "{forwarded}"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn the_method_restriction_stops_a_plaintext_write() {
    let server = HttpServer::spawn().await;
    let policy = loopback_only().with_methods(MethodPolicy::ReadOnly);
    let (proxy, mut denials) = start(policy).await;

    let post = format!(
        "POST http://{}/upload HTTP/1.1\r\nHost: {}\r\nProxy-Authorization: {}\r\ncontent-length: 0\r\n\r\n",
        server.addr,
        server.addr,
        bearer(&proxy),
    );
    let (mut stream, head) = send_request(&proxy, &post).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert!(read_body(&mut stream).await.contains("read-only"));
    assert_eq!(
        next_denial(&mut denials).await.reason,
        DenyReason::MethodNotAllowed("POST".into())
    );
    assert!(server.heads().is_empty(), "the upstream must never see it");

    let get = format!(
        "GET http://{}/read HTTP/1.1\r\nHost: {}\r\nProxy-Authorization: {}\r\n\r\n",
        server.addr,
        server.addr,
        bearer(&proxy),
    );
    let (_stream, head) = send_request(&proxy, &get).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    proxy.shutdown().await;
}

/// The documented limit of the method restriction: a `CONNECT` tunnel carries
/// methods the proxy cannot see, so it is unaffected. Asserted so the gap is
/// visible in the test suite rather than only in prose.
#[tokio::test]
async fn the_method_restriction_does_not_reach_inside_a_tunnel() {
    let echo = spawn_echo_server().await;
    let policy = loopback_only().with_methods(MethodPolicy::ReadOnly);
    let (proxy, _denials) = start(policy).await;
    let (mut tunnel, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_tunnels(&mut tunnel, b"POST /anything HTTP/1.1\r\n").await;
    proxy.shutdown().await;
}

#[tokio::test]
async fn socks5_tunnels_an_allowed_host() {
    let echo = spawn_echo_server().await;
    let (proxy, _denials) = start(loopback_only()).await;
    let (mut tunnel, reply) = socks_tunnel(&proxy, "127.0.0.1", echo.port()).await;
    assert_eq!(reply[0], 0x05);
    assert_eq!(reply[1], 0x00, "success reply");
    assert_eq!(reply[3], 0x01, "an IPv4 bound address");
    assert_tunnels(&mut tunnel, b"socks ping").await;
    proxy.shutdown().await;
}

#[tokio::test]
async fn socks5_denies_with_the_ruleset_reply() {
    let (proxy, mut denials) = start(loopback_only()).await;
    let (mut stream, reply) = socks_tunnel(&proxy, "blocked.test", 443).await;
    assert_eq!(reply[1], 0x02, "connection not allowed by ruleset");

    let denial = next_denial(&mut denials).await;
    assert_eq!(denial.protocol, Protocol::Socks5);
    assert_eq!((denial.host.as_str(), denial.port), ("blocked.test", 443));
    assert_eq!(denial.reason, DenyReason::NotAllowlisted);

    // The proxy closes a refused connection rather than leaving it open.
    let mut trailing = Vec::new();
    stream
        .read_to_end(&mut trailing)
        .await
        .expect("closed cleanly");
    assert!(trailing.is_empty());
    proxy.shutdown().await;
}

#[tokio::test]
async fn socks5_refuses_a_wrong_password() {
    let (proxy, mut denials) = start(loopback_only()).await;
    let (mut stream, chosen) = socks_greet(&proxy, &[SOCKS_AUTH_USERNAME_PASSWORD]).await;
    assert_eq!(chosen, [0x05, SOCKS_AUTH_USERNAME_PASSWORD]);
    assert_eq!(
        socks_authenticate(&mut stream, "not-the-token").await,
        [0x01, 0x01]
    );
    assert_eq!(
        next_denial(&mut denials).await.reason,
        DenyReason::Unauthorized
    );

    let mut trailing = Vec::new();
    stream
        .read_to_end(&mut trailing)
        .await
        .expect("closed cleanly");
    assert!(
        trailing.is_empty(),
        "a rejected client gets nothing further"
    );
    proxy.shutdown().await;
}

#[tokio::test]
async fn socks5_refuses_a_client_that_will_not_authenticate() {
    let (proxy, mut denials) = start(loopback_only()).await;
    // 0x00 is "no authentication required".
    let (mut stream, chosen) = socks_greet(&proxy, &[0x00]).await;
    assert_eq!(chosen, [0x05, 0xFF], "no acceptable method");
    assert_eq!(
        next_denial(&mut denials).await.reason,
        DenyReason::Unauthorized
    );

    let mut trailing = Vec::new();
    stream
        .read_to_end(&mut trailing)
        .await
        .expect("closed cleanly");
    assert!(trailing.is_empty());
    proxy.shutdown().await;
}

#[tokio::test]
async fn socks5_rejects_a_command_it_does_not_implement() {
    let (proxy, _denials) = start(Policy::new(NetworkMode::Full)).await;
    let (mut stream, _) = socks_greet(&proxy, &[SOCKS_AUTH_USERNAME_PASSWORD]).await;
    assert_eq!(
        socks_authenticate(&mut stream, proxy.token()).await,
        [0x01, 0x00]
    );
    // 0x02 is BIND.
    stream
        .write_all(&[0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0x00, 0x50])
        .await
        .expect("writing a BIND request");
    let mut reply = [0u8; 10];
    stream
        .read_exact(&mut reply)
        .await
        .expect("reading the reply");
    assert_eq!(reply[1], 0x07, "command not supported");
    proxy.shutdown().await;
}

/// Both protocols share one port, so the sniff must not mistake one for the
/// other — proven by driving each of them against the same running proxy.
#[tokio::test]
async fn one_port_serves_both_protocols() {
    let echo = spawn_echo_server().await;
    let (proxy, _denials) = start(loopback_only()).await;
    let (mut http_tunnel, head) =
        send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    let (mut socks_tunnel_stream, reply) = socks_tunnel(&proxy, "127.0.0.1", echo.port()).await;
    assert_eq!(reply[1], 0x00);
    assert_tunnels(&mut http_tunnel, b"over http").await;
    assert_tunnels(&mut socks_tunnel_stream, b"over socks").await;
    proxy.shutdown().await;
}

#[tokio::test]
async fn an_upstream_that_refuses_the_connection_becomes_a_bad_gateway() {
    // Bind and immediately drop a listener: the port is free, so connecting to
    // it is refused rather than routed anywhere.
    let closed = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = closed.local_addr().expect("bound address");
    drop(closed);
    let (proxy, _denials) = start(loopback_only()).await;
    let (mut stream, head) = send_connect(&proxy, &addr.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 502 Bad Gateway"), "{head}");
    assert!(read_body(&mut stream).await.contains("cannot reach"));
    proxy.shutdown().await;
}

#[tokio::test]
async fn a_client_that_vanishes_mid_tunnel_leaves_nothing_running() {
    let echo = spawn_echo_server().await;
    let (proxy, _denials) = start(loopback_only()).await;
    let (mut tunnel, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_tunnels(&mut tunnel, b"ping").await;
    assert_eq!(proxy.active_connections(), 1);

    drop(tunnel);
    wait_until(Duration::from_secs(5), || proxy.active_connections() == 0).await;
    proxy.shutdown().await;
}

#[tokio::test]
async fn connections_beyond_the_cap_are_dropped_not_queued() {
    let echo = spawn_echo_server().await;
    let config = ProxyConfig {
        max_connections: 1,
        ..test_config(loopback_only())
    };
    let (proxy, _denials) = Proxy::start(config).await.expect("the proxy binds");
    let (mut held, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");

    // Nothing is written on the extra connection: the proxy drops it at accept
    // time, and an unwritten socket closes with a clean EOF rather than a reset.
    let mut extra = TcpStream::connect(tcp_addr(&proxy))
        .await
        .expect("dialling the proxy");
    let mut response = Vec::new();
    extra
        .read_to_end(&mut response)
        .await
        .expect("the proxy closes it");
    assert!(
        response.is_empty(),
        "an over-cap client is dropped, not served"
    );

    // The connection under the cap is untouched by the one that was dropped.
    assert_tunnels(&mut held, b"still open").await;
    proxy.shutdown().await;
}

#[tokio::test]
async fn shutdown_releases_the_port_and_ends_live_tunnels() {
    let echo = spawn_echo_server().await;
    let (proxy, _denials) = start(loopback_only()).await;
    let addr = tcp_addr(&proxy);
    let (mut tunnel, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");

    proxy.shutdown().await;

    // The listener is gone: the port either refuses the connection outright or
    // accepts nothing and closes.
    let refused = match TcpStream::connect(addr).await {
        Err(_) => true,
        Ok(mut stream) => {
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response).await;
            response.is_empty()
        }
    };
    assert!(refused, "the port still answers after shutdown");

    // The live tunnel was aborted with it.
    let mut trailing = Vec::new();
    let _ = tunnel.read_to_end(&mut trailing).await;
    assert!(trailing.is_empty());
}

#[tokio::test]
async fn a_malformed_request_gets_a_400_rather_than_a_silent_close() {
    let (proxy, _denials) = start(loopback_only()).await;
    let (_stream, head) = send_request(&proxy, "GET\r\n\r\n").await;
    assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "{head}");
    proxy.shutdown().await;
}

#[tokio::test]
async fn an_oversized_request_head_is_capped() {
    let (proxy, _denials) = start(loopback_only()).await;
    let mut stream = TcpStream::connect(tcp_addr(&proxy))
        .await
        .expect("dialling the proxy");
    stream
        .write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\n")
        .await
        .expect("writing the request line");

    // Headers are dribbled and the socket read between writes: the proxy
    // answers a client that is still mid-header and closes, so the response
    // has to be collected before more bytes are pushed at a closed socket.
    let padding = "x".repeat(1000);
    let mut response = Vec::new();
    for index in 0..40 {
        let header = format!("X-Pad-{index}: {padding}\r\n");
        if stream.write_all(header.as_bytes()).await.is_err() {
            break;
        }
        let mut chunk = [0u8; 512];
        let read = tokio::time::timeout(Duration::from_millis(20), stream.read(&mut chunk)).await;
        if let Ok(Ok(read)) = read {
            response.extend_from_slice(&chunk[..read]);
            break;
        }
    }
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 431 "), "{response}");
    proxy.shutdown().await;
}

/// The transport a network-namespaced sandbox actually uses: a unix socket on
/// the host, reached from inside through [`super::relay`].
///
/// Every socket here lives under a fresh temporary directory that the test
/// owns and deletes; nothing reads or writes a real Friring data directory.
#[cfg(unix)]
mod unix_transport {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use tokio::net::UnixStream;

    use super::*;
    use crate::proxy::{Relay, RelayConfig};

    /// A private directory for one test's socket. Held by the caller: dropping
    /// it removes the directory.
    fn socket_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("a private temporary directory")
    }

    fn unix_config(policy: Policy, path: &Path) -> ProxyConfig {
        ProxyConfig {
            bind: ProxyBind::unix(path),
            ..test_config(policy)
        }
    }

    async fn start_unix(policy: Policy, path: &Path) -> (Proxy, mpsc::Receiver<DenialEvent>) {
        Proxy::start(unix_config(policy, path))
            .await
            .expect("the proxy binds its socket")
    }

    async fn dial(proxy: &Proxy) -> UnixStream {
        let path = proxy.unix_path().expect("a unix listener is bound");
        UnixStream::connect(path)
            .await
            .expect("dialling the proxy socket")
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("the socket exists")
            .permissions()
            .mode()
            & 0o777
    }

    #[tokio::test]
    async fn a_connect_over_the_socket_tunnels_bytes_both_ways() {
        let echo = spawn_echo_server().await;
        let dir = socket_dir();
        let (proxy, _denials) = start_unix(loopback_only(), &dir.path().join("p.sock")).await;
        let mut tunnel = dial(&proxy).await;
        let head = exchange(
            &mut tunnel,
            &connect_request(&echo.to_string(), Some(&bearer(&proxy))),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
        assert_tunnels(&mut tunnel, b"over a unix socket").await;
        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn socks5_works_over_the_socket_too() {
        let echo = spawn_echo_server().await;
        let dir = socket_dir();
        let (proxy, _denials) = start_unix(loopback_only(), &dir.path().join("p.sock")).await;
        let mut tunnel = dial(&proxy).await;
        let reply = socks_tunnel_on(&mut tunnel, proxy.token(), "127.0.0.1", echo.port()).await;
        assert_eq!(reply[1], 0x00, "success reply");
        assert_tunnels(&mut tunnel, b"socks over a unix socket").await;
        proxy.shutdown().await;
    }

    /// The socket is not a way around the token or the allowlist: reaching the
    /// proxy through the filesystem changes nothing about what it enforces.
    #[tokio::test]
    async fn the_socket_enforces_the_same_token_and_policy() {
        let dir = socket_dir();
        let (proxy, mut denials) = start_unix(loopback_only(), &dir.path().join("p.sock")).await;

        let mut unauthenticated = dial(&proxy).await;
        let head = exchange(
            &mut unauthenticated,
            &connect_request("127.0.0.1:1", Some("Bearer wrong")),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 407 "), "{head}");
        assert_eq!(
            next_denial(&mut denials).await.reason,
            DenyReason::Unauthorized
        );

        let mut denied = dial(&proxy).await;
        let head = exchange(
            &mut denied,
            &connect_request("blocked.test:443", Some(&bearer(&proxy))),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
        assert!(read_body(&mut denied)
            .await
            .contains("not in the sandbox allowlist"));
        assert_eq!(
            next_denial(&mut denials).await.reason,
            DenyReason::NotAllowlisted
        );

        // And SOCKS5's own authentication step, over the same transport.
        let mut socks = dial(&proxy).await;
        assert_eq!(
            socks_greet_on(&mut socks, &[SOCKS_AUTH_USERNAME_PASSWORD]).await,
            [0x05, SOCKS_AUTH_USERNAME_PASSWORD]
        );
        assert_eq!(
            socks_authenticate(&mut socks, "not-the-token").await,
            [0x01, 0x01]
        );
        proxy.shutdown().await;
    }

    /// The socket is a credential-bearing endpoint. Even with the token
    /// required, it has no business being connectable by every account on the
    /// host.
    #[tokio::test]
    async fn the_socket_is_not_world_connectable() {
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        assert_eq!(mode_of(&path), 0o600, "default mode");
        proxy.shutdown().await;

        // A backend whose sandbox runs as another uid has to widen it
        // deliberately, and gets exactly what it asked for.
        let config = ProxyConfig {
            bind: ProxyBind {
                tcp: None,
                unix: Some(UnixBind::new(&path).with_mode(0o660)),
            },
            ..test_config(loopback_only())
        };
        let (proxy, _denials) = Proxy::start(config).await.expect("the proxy binds");
        assert_eq!(mode_of(&path), 0o660);
        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn the_socket_is_removed_on_shutdown() {
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        assert!(path.exists());
        proxy.shutdown().await;
        assert!(!path.exists(), "shutdown leaves no socket behind");
    }

    #[tokio::test]
    async fn dropping_the_handle_also_removes_the_socket() {
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        assert!(path.exists());
        drop(proxy);
        assert!(!path.exists());
    }

    /// A crashed Friring leaves its socket file behind. The next start has to
    /// reclaim it, or the sandbox is wedged until someone deletes it by hand.
    #[tokio::test]
    async fn a_stale_socket_file_does_not_wedge_startup() {
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let abandoned = std::os::unix::net::UnixListener::bind(&path).expect("bind a stale socket");
        drop(abandoned);
        assert!(path.exists(), "a listener's socket file outlives it");

        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        assert_eq!(proxy.unix_path(), Some(path.as_path()));
        // And it serves: reclaiming the path is not just unlinking it.
        let mut tunnel = dial(&proxy).await;
        let head = exchange(
            &mut tunnel,
            &connect_request("blocked.test:443", Some(&bearer(&proxy))),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
        proxy.shutdown().await;
    }

    /// Reclaiming a stale socket must not become stealing a live one.
    #[tokio::test]
    async fn a_socket_a_running_proxy_is_serving_is_not_taken_over() {
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (first, _denials) = start_unix(loopback_only(), &path).await;

        let error = Proxy::start(unix_config(loopback_only(), &path))
            .await
            .expect_err("a live socket is not free");
        assert!(
            error
                .to_string()
                .contains("already served by a running proxy"),
            "{error}"
        );

        // The first proxy is untouched.
        let mut tunnel = dial(&first).await;
        let head = exchange(
            &mut tunnel,
            &connect_request("blocked.test:443", Some(&bearer(&first))),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
        first.shutdown().await;
    }

    #[tokio::test]
    async fn a_regular_file_at_the_socket_path_is_never_deleted() {
        let dir = socket_dir();
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"something the user cares about").expect("write the file");

        let error = Proxy::start(unix_config(loopback_only(), &path))
            .await
            .expect_err("a regular file is not a socket to reclaim");
        assert!(error.to_string().contains("not a socket"), "{error}");
        assert!(path.exists(), "the file must survive the refusal");
    }

    #[tokio::test]
    async fn one_proxy_can_serve_both_transports_at_once() {
        let echo = spawn_echo_server().await;
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let config = ProxyConfig {
            bind: ProxyBind::both("127.0.0.1:0".parse().expect("a loopback address"), &path),
            ..test_config(loopback_only())
        };
        let (proxy, _denials) = Proxy::start(config).await.expect("the proxy binds both");
        assert!(proxy.tcp_addr().is_some());
        assert_eq!(proxy.unix_path(), Some(path.as_path()));

        let (mut over_tcp, head) =
            send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
        let mut over_unix = dial(&proxy).await;
        let head = exchange(
            &mut over_unix,
            &connect_request(&echo.to_string(), Some(&bearer(&proxy))),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");

        assert_tunnels(&mut over_tcp, b"loopback").await;
        assert_tunnels(&mut over_unix, b"socket").await;
        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn a_proxy_with_no_listener_is_a_configuration_error() {
        let config = ProxyConfig {
            bind: ProxyBind {
                tcp: None,
                unix: None,
            },
            ..test_config(loopback_only())
        };
        let error = Proxy::start(config)
            .await
            .expect_err("a proxy nothing can reach is not a proxy");
        assert!(
            error.to_string().contains("at least one listener"),
            "{error}"
        );
    }

    // The relay: what makes the socket reachable from inside a network
    // namespace, where no host TCP address exists.

    async fn start_relay(socket: &Path) -> Relay {
        Relay::start(RelayConfig {
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(5),
            ..RelayConfig::new(
                "127.0.0.1:0".parse().expect("a loopback address"),
                socket.to_path_buf(),
            )
        })
        .await
        .expect("the relay binds")
    }

    /// The whole path an agent inside a `--unshare-net` sandbox takes: TCP to
    /// the relay, unix socket to the proxy, policy, upstream.
    #[tokio::test]
    async fn the_relay_carries_connect_through_to_the_proxy() {
        let echo = spawn_echo_server().await;
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        let relay = start_relay(&path).await;

        let mut tunnel = TcpStream::connect(relay.addr())
            .await
            .expect("dialling the relay");
        let head = exchange(
            &mut tunnel,
            &connect_request(&echo.to_string(), Some(&bearer(&proxy))),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
        assert_tunnels(&mut tunnel, b"through the relay").await;

        relay.shutdown().await;
        proxy.shutdown().await;
    }

    /// The relay moves bytes and nothing else, so SOCKS5 crosses it unchanged.
    #[tokio::test]
    async fn the_relay_carries_socks5_unchanged() {
        let echo = spawn_echo_server().await;
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        let relay = start_relay(&path).await;

        let mut tunnel = TcpStream::connect(relay.addr())
            .await
            .expect("dialling the relay");
        let reply = socks_tunnel_on(&mut tunnel, proxy.token(), "127.0.0.1", echo.port()).await;
        assert_eq!(reply[1], 0x00, "success reply");
        assert_tunnels(&mut tunnel, b"socks through the relay").await;

        relay.shutdown().await;
        proxy.shutdown().await;
    }

    /// The relay holds no credential and makes no decision: a denial still
    /// comes from the proxy, with its reason intact.
    #[tokio::test]
    async fn a_denial_survives_the_relay_with_its_reason() {
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, mut denials) = start_unix(loopback_only(), &path).await;
        let relay = start_relay(&path).await;

        let mut stream = TcpStream::connect(relay.addr())
            .await
            .expect("dialling the relay");
        let head = exchange(
            &mut stream,
            &connect_request("blocked.test:443", Some(&bearer(&proxy))),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
        assert!(read_body(&mut stream)
            .await
            .contains("not in the sandbox allowlist"));
        assert_eq!(next_denial(&mut denials).await.host, "blocked.test");

        // And the token is still required on the far side.
        let mut unauthenticated = TcpStream::connect(relay.addr())
            .await
            .expect("dialling the relay");
        let head = exchange(&mut unauthenticated, &connect_request("127.0.0.1:1", None)).await;
        assert!(head.starts_with("HTTP/1.1 407 "), "{head}");

        relay.shutdown().await;
        proxy.shutdown().await;
    }

    /// The environment a sandbox is handed: the relay's address inside the
    /// namespace, carrying the proxy's token.
    #[tokio::test]
    async fn the_sandbox_proxy_urls_point_at_the_relay() {
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        let relay = start_relay(&path).await;

        // A unix-only proxy has no URL of its own to hand out.
        assert_eq!(proxy.http_proxy_url(), None);
        assert_eq!(proxy.socks5_proxy_url(), None);

        let http = proxy.http_proxy_url_at(relay.addr());
        assert_eq!(
            http,
            format!("http://{PROXY_USERNAME}:{}@{}", proxy.token(), relay.addr())
        );
        assert!(proxy
            .socks5_proxy_url_at(relay.addr())
            .starts_with("socks5h://"));

        relay.shutdown().await;
        proxy.shutdown().await;
    }

    /// The relay starts before the host proxy is guaranteed to be up, so a
    /// missing socket must close the connection rather than hang or panic.
    #[tokio::test]
    async fn the_relay_closes_cleanly_when_the_socket_is_absent() {
        let dir = socket_dir();
        let relay = start_relay(&dir.path().join("never-created.sock")).await;

        let mut stream = TcpStream::connect(relay.addr())
            .await
            .expect("dialling the relay");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("the relay closes it");
        assert!(response.is_empty());

        wait_until(Duration::from_secs(5), || relay.active_connections() == 0).await;
        relay.shutdown().await;
    }

    #[tokio::test]
    async fn a_client_that_vanishes_leaves_no_relay_task_behind() {
        let echo = spawn_echo_server().await;
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        let relay = start_relay(&path).await;

        let mut tunnel = TcpStream::connect(relay.addr())
            .await
            .expect("dialling the relay");
        let head = exchange(
            &mut tunnel,
            &connect_request(&echo.to_string(), Some(&bearer(&proxy))),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
        assert_eq!(relay.active_connections(), 1);

        drop(tunnel);
        wait_until(Duration::from_secs(5), || relay.active_connections() == 0).await;
        wait_until(Duration::from_secs(5), || proxy.active_connections() == 0).await;

        relay.shutdown().await;
        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn the_relay_drops_connections_beyond_its_cap() {
        let echo = spawn_echo_server().await;
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        let relay = Relay::start(RelayConfig {
            max_connections: 1,
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(5),
            ..RelayConfig::new("127.0.0.1:0".parse().expect("a loopback address"), &path)
        })
        .await
        .expect("the relay binds");

        let mut held = TcpStream::connect(relay.addr())
            .await
            .expect("dialling the relay");
        let head = exchange(
            &mut held,
            &connect_request(&echo.to_string(), Some(&bearer(&proxy))),
        )
        .await;
        assert!(head.starts_with("HTTP/1.1 200 "), "{head}");

        let mut extra = TcpStream::connect(relay.addr())
            .await
            .expect("dialling the relay");
        let mut response = Vec::new();
        extra
            .read_to_end(&mut response)
            .await
            .expect("the relay closes it");
        assert!(response.is_empty(), "an over-cap client is dropped");

        assert_tunnels(&mut held, b"still open").await;
        relay.shutdown().await;
        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn relay_shutdown_releases_the_port() {
        let dir = socket_dir();
        let path = dir.path().join("p.sock");
        let (proxy, _denials) = start_unix(loopback_only(), &path).await;
        let relay = start_relay(&path).await;
        let addr = relay.addr();
        relay.shutdown().await;

        let released = match TcpStream::connect(addr).await {
            Err(_) => true,
            Ok(mut stream) => {
                let mut response = Vec::new();
                let _ = stream.read_to_end(&mut response).await;
                response.is_empty()
            }
        };
        assert!(released, "the relay port still answers after shutdown");
        proxy.shutdown().await;
    }

    /// Socket paths are length-limited by the OS (around 100 bytes), which is
    /// short enough that a deep data directory can cross it. The failure has to
    /// name the path rather than surface as a bare errno.
    #[tokio::test]
    async fn an_unbindable_socket_path_names_itself() {
        let dir = socket_dir();
        let path: PathBuf = dir.path().join("x".repeat(120));
        let error = Proxy::start(unix_config(loopback_only(), &path))
            .await
            .expect_err("an over-long socket path cannot bind");
        assert!(
            error.to_string().contains("binding the sandbox proxy"),
            "{error}"
        );
    }
}
