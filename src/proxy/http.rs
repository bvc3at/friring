//! The HTTP half of the proxy: `CONNECT` tunnels and plaintext forwarding.
//!
//! `CONNECT` is what carries every `https://` request an agent makes. The
//! proxy reads the `host:port` the client asks for, decides on it, and — if
//! allowed — becomes a pipe. Nothing inside the tunnel is inspected, so the
//! decision rests entirely on the name the client supplied (see the module
//! docs on domain fronting).
//!
//! Plaintext forwarding handles absolute-form requests
//! (`GET http://host/path HTTP/1.1`), the shape a client emits when `HTTP_PROXY`
//! is set and the URL is not TLS. It is the only traffic whose method the proxy
//! can see, and therefore the only traffic
//! [`MethodPolicy::ReadOnly`](super::MethodPolicy::ReadOnly) can act on.

use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::auth;
use super::host::{self, CanonicalHost};
use super::policy::{Decision, DenyReason};
use super::stream::Client;
use super::{Protocol, Shared};

/// Ceiling on a request head. Generous for real headers (cookies and bearer
/// tokens are the bulky ones) and small enough that a client dribbling bytes
/// forever cannot grow the proxy's memory.
const MAX_HEAD_BYTES: usize = 16 * 1024;

/// Headers that describe *this* hop and must not be passed on. The list is
/// deliberately missing `transfer-encoding`: the body is spliced through
/// untouched, so its framing headers have to survive or the stream desynchronises.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
];

/// Serve one client connection that begins with an HTTP request line.
///
/// Every refusal is answered before the socket closes — a proxy that hangs up
/// silently leaves the agent guessing at a DNS failure instead of reading the
/// reason.
pub(super) async fn serve(mut client: Client, shared: Arc<Shared>) -> io::Result<()> {
    let limits = shared.limits();
    let head = match timeout(limits.handshake_timeout, read_head(&mut client)).await {
        Ok(Ok(head)) => head,
        Ok(Err(HeadError::Incomplete)) => return Ok(()),
        Ok(Err(HeadError::Io(error))) => return Err(error),
        Ok(Err(HeadError::TooLarge)) => {
            return respond(&mut client, 431, "Request Header Fields Too Large", &[], "").await;
        }
        Err(_elapsed) => {
            return respond(&mut client, 408, "Request Timeout", &[], "").await;
        }
    };

    let Some(request) = RequestHead::parse(&head) else {
        return refuse(&mut client, 400, "Bad Request", "malformed request").await;
    };
    let Some(route) = Route::resolve(&request) else {
        return refuse(
            &mut client,
            400,
            "Bad Request",
            "unsupported request target",
        )
        .await;
    };
    // The host as the client spelled it: what a denial reports, and all there
    // is to report when it turns out not to canonicalise.
    let (asked, port) = (route.host(), route.port());

    // Authentication precedes policy so a caller without the token learns
    // nothing about what the allowlist contains.
    let presented = request.header("proxy-authorization");
    if !presented.is_some_and(|value| auth::header_presents_token(value, shared.token())) {
        shared.report(Protocol::Http, asked, port, DenyReason::Unauthorized);
        let challenge = ["Proxy-Authenticate: Basic realm=\"friring\""];
        let body = format!("friring proxy: {}\n", DenyReason::Unauthorized);
        return respond(
            &mut client,
            407,
            "Proxy Authentication Required",
            &challenge,
            &body,
        )
        .await;
    }

    // One canonicalisation, at the edge, before anything is compared — and the
    // result is what gets dialled below.
    let host = match CanonicalHost::parse(asked) {
        Ok(host) => host,
        Err(fault) => {
            let reason = DenyReason::UnsupportedHost(fault.detail());
            shared.report(Protocol::Http, asked, port, reason.clone());
            return refuse(&mut client, 403, "Forbidden", &reason.to_string()).await;
        }
    };
    if let Decision::Deny(reason) = shared.decide(&host, port) {
        shared.report(Protocol::Http, asked, port, reason.clone());
        return refuse(&mut client, 403, "Forbidden", &reason.to_string()).await;
    }
    // A tunnel's method is unknowable, so the restriction only applies here.
    if matches!(route, Route::Forward { .. }) {
        if let Decision::Deny(reason) = shared.decide_method(&request.method) {
            shared.report(Protocol::Http, asked, port, reason.clone());
            return refuse(&mut client, 403, "Forbidden", &reason.to_string()).await;
        }
    }

    let permitted = |address| shared.permits_address(address, port);
    let dial = host::connect(&host, port, &permitted);
    let mut upstream = match timeout(limits.connect_timeout, dial).await {
        Ok(Ok(upstream)) => upstream,
        // The name was allowed and its address was not: a policy refusal that
        // only the resolver could reach, so it is reported and answered like
        // one rather than dressed up as an upstream failure.
        Ok(Err(host::DialError::HostLocal)) => {
            let reason = DenyReason::HostLocal;
            shared.report(Protocol::Http, asked, port, reason.clone());
            return refuse(&mut client, 403, "Forbidden", &reason.to_string()).await;
        }
        Ok(Err(host::DialError::Io(error))) => {
            let detail = format!("cannot reach {host}:{port}: {error}");
            return refuse(&mut client, 502, "Bad Gateway", &detail).await;
        }
        Err(_elapsed) => {
            let detail = format!("timed out connecting to {host}:{port}");
            return refuse(&mut client, 504, "Gateway Timeout", &detail).await;
        }
    };
    let _ = upstream.set_nodelay(true);

    match &route {
        Route::Connect { .. } => {
            client
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await?;
        }
        Route::Forward { path, .. } => {
            upstream
                .write_all(&request.forwarded_head(path, &host.to_string(), port))
                .await?;
        }
    }
    finish(client, upstream, &request.body_prefix, &shared).await
}

/// Hand the connection over to the tunnel, first flushing whatever the client
/// pipelined behind its request head (a TLS `ClientHello` usually arrives in
/// the same packet as the `CONNECT`).
async fn finish(
    client: Client,
    mut upstream: TcpStream,
    body_prefix: &[u8],
    shared: &Shared,
) -> io::Result<()> {
    if !body_prefix.is_empty() {
        upstream.write_all(body_prefix).await?;
    }
    let moved = super::tunnel::splice(client, upstream, shared.limits().idle_timeout).await?;
    tracing::debug!(
        sent = moved.sent,
        received = moved.received,
        "proxy tunnel closed"
    );
    Ok(())
}

/// What the client asked the proxy to do.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// `CONNECT host:port` — an opaque tunnel.
    Connect { host: String, port: u16 },
    /// An absolute-form request the proxy forwards itself.
    Forward {
        host: String,
        port: u16,
        /// Origin-form target for the rewritten request line.
        path: String,
    },
}

impl Route {
    fn resolve(request: &RequestHead) -> Option<Self> {
        if request.method.eq_ignore_ascii_case("CONNECT") {
            // RFC 9110 requires the port; default it rather than fail, since a
            // client that omits it always means TLS.
            let (host, port) = split_authority(&request.target, 443)?;
            return Some(Self::Connect { host, port });
        }
        let (authority, path) = split_absolute_uri(&request.target)
            // Origin-form reaches a proxy only from a misconfigured client, but
            // the `Host` header still says where it meant to go.
            .or_else(|| {
                let host = request
                    .target
                    .starts_with('/')
                    .then(|| request.header("host"))??;
                Some((host.to_string(), request.target.clone()))
            })?;
        let (host, port) = split_authority(&authority, 80)?;
        Some(Self::Forward { host, port, path })
    }

    fn host(&self) -> &str {
        match self {
            Self::Connect { host, .. } | Self::Forward { host, .. } => host,
        }
    }

    fn port(&self) -> u16 {
        match self {
            Self::Connect { port, .. } | Self::Forward { port, .. } => *port,
        }
    }
}

/// A parsed request head plus any bytes that arrived behind it.
#[derive(Debug)]
struct RequestHead {
    method: String,
    target: String,
    /// Header names lowercased; values trimmed.
    headers: Vec<(String, String)>,
    /// Bytes read past the blank line — the start of the body or of the
    /// tunnelled stream, which must be forwarded rather than dropped.
    body_prefix: Vec<u8>,
}

impl RequestHead {
    fn parse(raw: &[u8]) -> Option<Self> {
        let end = find_head_end(raw)?;
        let text = std::str::from_utf8(&raw[..end]).ok()?;
        let mut lines = text.split("\r\n");
        let mut request_line = lines.next()?.split(' ').filter(|part| !part.is_empty());
        let method = request_line.next()?.to_string();
        let target = request_line.next()?.to_string();
        // The version is present but unused: the proxy speaks 1.1 to upstream
        // regardless, and a 0.9-style request has no headers to authenticate with.
        request_line.next()?;
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let (name, value) = line.split_once(':')?;
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
        Some(Self {
            method,
            target,
            headers,
            body_prefix: raw[end..].to_vec(),
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    /// Rebuild the request for the upstream server: origin-form target,
    /// hop-by-hop headers dropped, and a `Host` that is the authority the
    /// policy authorised.
    ///
    /// The client's own `Host` is **replaced**, never forwarded. An absolute-form
    /// request carries the destination twice — in the URI the proxy decided on
    /// and in a header the proxy does not need — and letting the two disagree is
    /// domain fronting: `GET http://allowed.example/` with `Host: elsewhere.test`
    /// would be allowed against one name and served by the virtual host of
    /// another. Inside a `CONNECT` tunnel the proxy genuinely cannot see that
    /// mismatch; here it can, so it does not forward one.
    ///
    /// `Connection: close` is forced so each plaintext request opens its own
    /// proxy connection and is policed on its own. Reusing the connection would
    /// hand a client one policy check for an unbounded number of later
    /// requests, and the method restriction in particular would cover only the
    /// first of them. The cost is that plaintext `Upgrade` (`ws://`) does not
    /// survive; `wss://` is unaffected, since that is a `CONNECT` tunnel.
    fn forwarded_head(&self, path: &str, host: &str, port: u16) -> Vec<u8> {
        let mut head = format!("{} {} HTTP/1.1\r\n", self.method, path);
        for (name, value) in &self.headers {
            if !HOP_BY_HOP.contains(&name.as_str()) && name != "host" {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
        }
        let authority = if port == 80 {
            host.to_string()
        } else {
            format!("{host}:{port}")
        };
        head.push_str(&format!("host: {authority}\r\n"));
        head.push_str("connection: close\r\n\r\n");
        head.into_bytes()
    }
}

enum HeadError {
    /// The peer closed before sending a complete head.
    Incomplete,
    /// The head grew past [`MAX_HEAD_BYTES`].
    TooLarge,
    Io(io::Error),
}

/// Read until the blank line that ends the request head.
async fn read_head(client: &mut Client) -> Result<Vec<u8>, HeadError> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = client.read(&mut chunk).await.map_err(HeadError::Io)?;
        if read == 0 {
            return Err(HeadError::Incomplete);
        }
        head.extend_from_slice(&chunk[..read]);
        if find_head_end(&head).is_some() {
            return Ok(head);
        }
        if head.len() > MAX_HEAD_BYTES {
            return Err(HeadError::TooLarge);
        }
    }
}

/// Offset just past the `\r\n\r\n` terminating the head.
fn find_head_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 4)
}

/// Split an absolute-form target into `(authority, origin-form path)`.
///
/// Returns `None` for anything that is not `http://…`, including `https://…`:
/// the proxy cannot originate TLS, and a client asking it to has misread its
/// own configuration.
fn split_absolute_uri(target: &str) -> Option<(String, String)> {
    let rest = target
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .and_then(|_| target.get(7..))?;
    let split = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..split];
    // Userinfo belongs to the request, not to the host being authorised.
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let path = match &rest[split..] {
        "" => "/".to_string(),
        path => path.to_string(),
    };
    Some((authority.to_string(), path))
}

/// Split `host`, `host:port` or `[v6]:port` into its parts.
fn split_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (address, suffix) = rest.split_once(']')?;
        let port = match suffix {
            "" => default_port,
            suffix => suffix.strip_prefix(':')?.parse().ok()?,
        };
        return Some((address.to_string(), port));
    }
    match authority.rsplit_once(':') {
        // A bare IPv6 literal: colons everywhere and no port.
        Some((host, _)) if host.contains(':') => Some((authority.to_string(), default_port)),
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None => Some((authority.to_string(), default_port)),
    }
}

/// Answer with a status and a one-line explanation the agent's transcript will
/// show verbatim.
async fn refuse(client: &mut Client, status: u16, reason: &str, detail: &str) -> io::Result<()> {
    respond(
        client,
        status,
        reason,
        &[],
        &format!("friring proxy: {detail}\n"),
    )
    .await
}

async fn respond(
    client: &mut Client,
    status: u16,
    reason: &str,
    extra_headers: &[&str],
    body: &str,
) -> io::Result<()> {
    let mut response = format!("HTTP/1.1 {status} {reason}\r\n");
    for header in extra_headers {
        response.push_str(header);
        response.push_str("\r\n");
    }
    response.push_str("content-type: text/plain; charset=utf-8\r\n");
    response.push_str(&format!("content-length: {}\r\n", body.len()));
    response.push_str("connection: close\r\n\r\n");
    response.push_str(body);
    client.write_all(response.as_bytes()).await?;
    client.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> RequestHead {
        RequestHead::parse(raw.as_bytes()).expect("parses")
    }

    #[test]
    fn connect_targets_carry_the_port_or_default_to_tls() {
        let request = parse("CONNECT api.github.com:443 HTTP/1.1\r\nHost: api.github.com\r\n\r\n");
        assert_eq!(
            Route::resolve(&request),
            Some(Route::Connect {
                host: "api.github.com".into(),
                port: 443,
            })
        );
        let bare = parse("CONNECT api.github.com HTTP/1.1\r\n\r\n");
        assert_eq!(Route::resolve(&bare).expect("resolves").port(), 443);
        let v6 = parse("CONNECT [::1]:8443 HTTP/1.1\r\n\r\n");
        let route = Route::resolve(&v6).expect("resolves");
        assert_eq!((route.host(), route.port()), ("::1", 8443));
    }

    #[test]
    fn absolute_form_requests_split_into_host_and_path() {
        let request =
            parse("GET http://example.test/a/b?c=d HTTP/1.1\r\nHost: example.test\r\n\r\n");
        assert_eq!(
            Route::resolve(&request),
            Some(Route::Forward {
                host: "example.test".into(),
                port: 80,
                path: "/a/b?c=d".into(),
            })
        );
        let rooted = parse("GET http://example.test:8080 HTTP/1.1\r\n\r\n");
        assert_eq!(
            Route::resolve(&rooted),
            Some(Route::Forward {
                host: "example.test".into(),
                port: 8080,
                path: "/".into(),
            })
        );
    }

    /// Userinfo in the URL must not become part of the host the policy sees —
    /// `http://github.com@evil.test/` goes to `evil.test`.
    #[test]
    fn userinfo_does_not_masquerade_as_the_host() {
        let request = parse("GET http://github.com@evil.test/x HTTP/1.1\r\n\r\n");
        assert_eq!(
            Route::resolve(&request).expect("resolves").host(),
            "evil.test"
        );
    }

    #[test]
    fn origin_form_falls_back_to_the_host_header() {
        let request = parse("GET /path HTTP/1.1\r\nHost: example.test:8080\r\n\r\n");
        assert_eq!(
            Route::resolve(&request),
            Some(Route::Forward {
                host: "example.test".into(),
                port: 8080,
                path: "/path".into(),
            })
        );
        // Neither an absolute URI nor a `Host`: nowhere to send it.
        assert_eq!(Route::resolve(&parse("GET /path HTTP/1.1\r\n\r\n")), None);
    }

    #[test]
    fn a_scheme_the_proxy_cannot_originate_is_refused() {
        assert_eq!(
            Route::resolve(&parse("GET https://example.test/ HTTP/1.1\r\n\r\n")),
            None
        );
    }

    #[test]
    fn bytes_behind_the_head_are_kept_for_the_tunnel() {
        let request = parse("CONNECT example.test:443 HTTP/1.1\r\n\r\n\x16\x03\x01hello");
        assert_eq!(request.body_prefix, b"\x16\x03\x01hello");
    }

    #[test]
    fn malformed_heads_do_not_parse() {
        for raw in [
            "GET\r\n\r\n",
            "GET /only-two-parts\r\n\r\n",
            "GET / HTTP/1.1\r\nnot-a-header\r\n\r\n",
            "",
        ] {
            assert!(
                RequestHead::parse(raw.as_bytes()).is_none(),
                "parsed `{raw}`"
            );
        }
    }

    #[test]
    fn forwarding_strips_this_hop_and_pins_the_connection() {
        let request = parse(concat!(
            "POST http://example.test/upload HTTP/1.1\r\n",
            "Host: example.test\r\n",
            "Proxy-Authorization: Bearer secret\r\n",
            "Proxy-Connection: keep-alive\r\n",
            "Connection: keep-alive\r\n",
            "Content-Length: 3\r\n",
            "\r\n",
        ));
        let head = request.forwarded_head("/upload", "example.test", 80);
        let head = String::from_utf8(head).expect("ascii");
        assert!(head.starts_with("POST /upload HTTP/1.1\r\n"));
        assert!(!head.to_ascii_lowercase().contains("secret"));
        assert!(!head.to_ascii_lowercase().contains("proxy-connection"));
        assert!(head.contains("content-length: 3\r\n"));
        assert!(head.contains("host: example.test\r\n"));
        assert!(head.ends_with("connection: close\r\n\r\n"));
        assert_eq!(head.matches("connection: ").count(), 1);
    }

    #[test]
    fn a_missing_host_header_is_synthesised_from_the_target() {
        let request = parse("GET http://example.test:8080/x HTTP/1.1\r\n\r\n");
        let head = request.forwarded_head("/x", "example.test", 8080);
        let head = String::from_utf8(head).expect("ascii");
        assert!(head.contains("host: example.test:8080\r\n"));
    }

    #[test]
    fn head_end_is_the_offset_past_the_blank_line() {
        assert_eq!(find_head_end(b"A\r\n\r\nBODY"), Some(5));
        assert_eq!(find_head_end(b"A\r\nB\r\n"), None);
    }
}
