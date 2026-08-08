//! End-to-end tests: a real proxy in front of a local test server, driven over
//! real loopback sockets.
//!
//! Nothing here reaches the network or reads any credential. Every upstream is
//! an in-process server on an ephemeral loopback port, every token is minted by
//! the proxy under test, and hosts that must be *denied* are refused before a
//! name is ever resolved — which is also what makes those tests hermetic.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
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
async fn read_response_head(stream: &mut TcpStream) -> String {
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

async fn read_body(stream: &mut TcpStream) -> String {
    let mut body = Vec::new();
    stream
        .read_to_end(&mut body)
        .await
        .expect("reading the body");
    String::from_utf8(body).expect("a UTF-8 body")
}

async fn send_request(proxy: &Proxy, request: &str) -> (TcpStream, String) {
    let mut stream = TcpStream::connect(proxy.addr())
        .await
        .expect("dialling the proxy");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("writing the request");
    let head = read_response_head(&mut stream).await;
    (stream, head)
}

async fn send_connect(
    proxy: &Proxy,
    target: &str,
    credential: Option<&str>,
) -> (TcpStream, String) {
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some(credential) = credential {
        request.push_str(&format!("Proxy-Authorization: {credential}\r\n"));
    }
    request.push_str("\r\n");
    send_request(proxy, &request).await
}

/// Bounce a byte string off the echo server through an open tunnel.
async fn assert_tunnels(stream: &mut TcpStream, payload: &[u8]) {
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

async fn socks_greet(proxy: &Proxy, methods: &[u8]) -> (TcpStream, [u8; 2]) {
    let mut stream = TcpStream::connect(proxy.addr())
        .await
        .expect("dialling the proxy");
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
    (stream, chosen)
}

async fn socks_authenticate(stream: &mut TcpStream, password: &str) -> [u8; 2] {
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

async fn socks_request(stream: &mut TcpStream, host: &str, port: u16) -> [u8; 10] {
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
    let url = proxy.http_proxy_url();
    assert!(url.starts_with(&format!("http://{PROXY_USERNAME}:")));
    assert!(url.ends_with(&proxy.addr().to_string()));
    assert!(proxy.socks5_proxy_url().starts_with("socks5h://"));

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

#[tokio::test]
async fn network_mode_full_allows_a_host_no_rule_mentions() {
    let echo = spawn_echo_server().await;
    let (proxy, _denials) = start(Policy::new(NetworkMode::Full)).await;
    let (mut tunnel, head) = send_connect(&proxy, &echo.to_string(), Some(&bearer(&proxy))).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_tunnels(&mut tunnel, b"open").await;
    proxy.shutdown().await;
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
    let mut extra = TcpStream::connect(proxy.addr())
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
    let addr = proxy.addr();
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
    let mut stream = TcpStream::connect(proxy.addr())
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
