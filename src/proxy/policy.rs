//! The egress policy: which `host:port` a sandboxed agent may reach, and the
//! reason a request was refused.
//!
//! The vocabulary mirrors the sandbox profile columns (`network_mode`,
//! `network_allow`, `network_deny`) so a profile row maps onto a [`Policy`]
//! without a translation layer.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use super::host::{self, CanonicalHost, HostFault};

/// How much network a sandbox gets.
///
/// The serde representation is the lowercase word stored in the profile's
/// `network_mode` column.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// Refuse every request. The default, so a policy that was never
    /// configured denies rather than leaks.
    #[default]
    None,
    /// Allow only what the allow list matches.
    Allowlist,
    /// Allow anything the deny list does not match.
    Full,
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = match self {
            Self::None => "none",
            Self::Allowlist => "allowlist",
            Self::Full => "full",
        };
        f.write_str(word)
    }
}

/// Which HTTP methods the proxy forwards.
///
/// This is an exfiltration brake, not a boundary: see [`MethodPolicy::ReadOnly`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MethodPolicy {
    /// Forward whatever the client asks for.
    #[default]
    Any,
    /// Forward only `GET`, `HEAD` and `OPTIONS`.
    ///
    /// **This covers plaintext HTTP only.** A `CONNECT` tunnel — which is
    /// every `https://` request — is opaque bytes to a proxy that does not
    /// terminate TLS, so the method inside it is unknowable and unrestricted.
    /// The setting is worth having because plaintext HTTP is where an
    /// accidental `POST` of workspace contents is cheapest, but it must never
    /// be described to a user as "the agent cannot upload".
    ReadOnly,
}

impl MethodPolicy {
    /// Whether `method` (case-sensitive, as it appears on the request line)
    /// may be forwarded.
    fn permits(self, method: &str) -> bool {
        match self {
            Self::Any => true,
            Self::ReadOnly => matches!(method, "GET" | "HEAD" | "OPTIONS"),
        }
    }
}

/// The host half of a [`HostRule`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostPattern {
    /// `*` — every host. The proxy's own vocabulary: a sandbox profile cannot
    /// store this spelling, so it only ever arrives from a caller that built
    /// the policy in code.
    Any,
    /// `example.com` — that name and nothing else. See [`HostRule`] for why a
    /// bare name does not carry its subtree.
    Domain(String),
    /// `*.example.com` or `.example.com` — that name **and** every subdomain of
    /// it. The two spellings are one pattern.
    Subtree(String),
    /// `127.0.0.1`, `[::1]` — compared as a parsed address, never by suffix.
    Address(IpAddr),
}

impl HostPattern {
    /// Both sides are already canonical here — one spelling per host, decided
    /// by [`CanonicalHost`] before any rule was consulted.
    fn matches(&self, host: &CanonicalHost) -> bool {
        match (self, host) {
            (Self::Any, _) => true,
            (Self::Address(rule), CanonicalHost::Address(host)) => rule == host,
            (Self::Domain(rule), CanonicalHost::Domain(host)) => host == rule,
            (Self::Subtree(rule), CanonicalHost::Domain(host)) => covers(host, rule),
            // A name rule never covers an address and an address rule never
            // covers a name: `localhost` is a name, matched as one.
            (Self::Address(_), CanonicalHost::Domain(_))
            | (Self::Domain(_) | Self::Subtree(_), CanonicalHost::Address(_)) => false,
        }
    }
}

/// Whether `host` is `domain` itself or sits under it on a label boundary.
///
/// The boundary is the whole point: `evilgithub.com` merely *ends with* the
/// text of `github.com`, and reading that as a match is the classic allowlist
/// bypass, so whatever precedes the suffix must be terminated by a `.`.
///
/// Both arguments are canonical, so this is a byte comparison: case was folded
/// and the root dot dropped where the host was canonicalised, and doing either
/// again here is how the two would drift apart.
///
/// This must stay behaviourally identical to `session::DomainRule::matches_host`,
/// which the two modules cannot share because the proxy is a leaf in the
/// architecture allowlist. `tests/egress_matcher_conformance.rs` runs both over
/// one table and fails when either side drifts.
fn covers(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// The prefix that widens a rule from one host to that host and its
/// subdomains. `.host` is the same pattern spelled without the star and reads
/// back as this form.
const SUBTREE_PREFIX: &str = "*.";

/// Split a written pattern into the host it names and whether it covers that
/// host's subtree.
///
/// A second leading dot is not a third spelling: it is an empty label, which no
/// name has and no resolver answers for, so `..github.com` is refused here
/// rather than folded away by [`CanonicalHost::parse`] — whose leading-dot rule
/// is for *request* hosts, where the resolvers that accept the spelling resolve
/// the apex.
fn split_subtree(host: &str) -> Result<(&str, bool), HostFault> {
    let Some(rest) = host
        .strip_prefix(SUBTREE_PREFIX)
        .or_else(|| host.strip_prefix('.'))
    else {
        return Ok((host, false));
    };
    if rest.starts_with('.') {
        return Err(HostFault::EmptyLabel);
    }
    Ok((rest, true))
}

/// One entry of an allow or deny list: a host pattern with an optional port.
///
/// Written as `host`, `host:port`, or — for an IPv6 literal with a port —
/// `[addr]:port`. A rule without a port matches every port.
///
/// # Matching
///
/// Rule and request host are both reduced to one canonical spelling first (see
/// the `host` module): case folded, the root dot and IPv6 brackets dropped, and
/// every address spelling read as the address it denotes.
///
/// | Rule | Matches | Does not match |
/// |---|---|---|
/// | `github.com` | `github.com` | `api.github.com`, `evilgithub.com`, `github.co` |
/// | `*.github.com`, `.github.com` | `github.com`, `api.github.com`, `a.b.github.com` | `evilgithub.com`, `github.com.evil.net`, `github.co` |
/// | `github.com:443` | `github.com` port 443 | `github.com` port 8080 |
/// | `127.0.0.1` | `127.0.0.1`, `127.1`, `2130706433`, `::ffff:127.0.0.1` | `evil.127.0.0.1`, `127.0.0.1.evil.net`, `localhost` |
/// | `*` | every host that canonicalises | — |
///
/// **A bare name is exactly that name; the subtree is a wildcard the writer
/// spells.** The narrow reading is the one the rest of the feature promises —
/// a first-use prompt grants a bare, port-scoped rule and tells the user it
/// covers "that host on that port only" — and a grammar in which a bare name
/// quietly carried the subtree would make every such grant wider than the
/// question asked.
///
/// **`*.` and `.` are one spelling of the subtree, and it covers the apex.**
/// The deny direction settles that: a user who denies `*.github.com` means "no
/// github.com traffic", and a matcher that let the apex through would be a
/// silent hole. Reading the same spelling two ways depending on which list it
/// sits in would be worse still.
///
/// An address rule is exact whatever prefix it was written with: suffix logic
/// on digits would be nonsense, there is no subtree under an address, and
/// `localhost` is a *name*, matched as one, so it does not cover `127.0.0.1`
/// unless listed too. An address rule does cover every *spelling* of its
/// address, because those all reach one endpoint — a deny on `127.0.0.1` that
/// `127.1` walked past would be no deny at all.
///
/// The accepted grammar is deliberately the one a sandbox profile's
/// `network_allow` / `network_deny` accepts (`session::DomainRule`), minus `*`,
/// which only this side has: ASCII labels of letters, digits, `-` and `_`, no
/// empty label, none edged with `-`, 63 bytes per label and 253 overall. An
/// international name must be written in punycode, because that is what a
/// request host is spelled as on the wire and because canonicalising a U-label
/// correctly needs Unicode tables this crate does not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRule {
    pattern: HostPattern,
    port: Option<u16>,
}

impl HostRule {
    /// Whether this rule covers `host` on `port`.
    ///
    /// A host that does not canonicalise matches nothing — but that is not how
    /// it is refused: [`Policy::decide`] turns it into
    /// [`DenyReason::UnsupportedHost`] before any rule is consulted, so the
    /// answer never depends on which list it was compared against.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        CanonicalHost::parse(host).is_ok_and(|host| self.matches_canonical(&host, port))
    }

    /// [`HostRule::matches`] against a host already canonicalised at the edge,
    /// which is how the request path avoids canonicalising once per rule.
    pub(super) fn matches_canonical(&self, host: &CanonicalHost, port: u16) -> bool {
        self.port.map_or(true, |scoped| scoped == port) && self.pattern.matches(host)
    }

    /// The port this rule is scoped to, or `None` when it covers every port.
    pub fn port(&self) -> Option<u16> {
        self.port
    }
}

impl FromStr for HostRule {
    type Err = anyhow::Error;

    fn from_str(entry: &str) -> Result<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            bail!("empty host rule");
        }
        let (host, port) = split_host_port(entry)?;
        // `*` is this side's own vocabulary rather than a host, so it is read
        // before canonicalisation — which would refuse it.
        if host.trim() == "*" {
            return Ok(Self {
                pattern: HostPattern::Any,
                port,
            });
        }
        // Taking the prefix off before canonicalising is what keeps
        // `*.127.0.0.1` and `127.0.0.1` one rule instead of an address and a
        // suffix pattern over digits.
        let (bare, subtree) = match split_subtree(host) {
            Ok(split) => split,
            Err(fault) => bail!("host rule `{entry}` {}", fault.detail()),
        };
        let pattern = match CanonicalHost::parse(bare) {
            // An address has no subtree, so the prefix is dropped rather than
            // honoured — one rule per address, however it was written.
            Ok(CanonicalHost::Address(address)) => HostPattern::Address(address),
            Ok(CanonicalHost::Domain(domain)) if subtree => HostPattern::Subtree(domain),
            Ok(CanonicalHost::Domain(domain)) => HostPattern::Domain(domain),
            Err(fault) => bail!("host rule `{entry}` {}", fault.detail()),
        };
        Ok(Self { pattern, port })
    }
}

impl fmt::Display for HostRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.pattern {
            HostPattern::Any => f.write_str("*")?,
            HostPattern::Domain(domain) => f.write_str(domain)?,
            // The prefix is part of what the rule *means*, so it is printed:
            // a denial is quoted back into a profile, and dropping the star
            // there would narrow the rule the user wrote.
            HostPattern::Subtree(domain) => write!(f, "{SUBTREE_PREFIX}{domain}")?,
            // Re-bracket IPv6, with or without a port: a rendering is quoted
            // into a denial and from there into a profile, and `::1` is a
            // spelling `session::DomainRule` refuses (it cannot tell the
            // colons apart from a port).
            HostPattern::Address(ip @ IpAddr::V6(_)) => write!(f, "[{ip}]")?,
            HostPattern::Address(ip) => write!(f, "{ip}")?,
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

/// Split `host[:port]`, handling the bracketed IPv6 form and refusing the
/// ambiguous bare one (`::1:443` could be either).
///
/// The brackets stay on the host: they are address syntax, and
/// [`CanonicalHost`] is where that is enforced rather than here.
fn split_host_port(entry: &str) -> Result<(&str, Option<u16>)> {
    if let Some(rest) = entry.strip_prefix('[') {
        let (inside, after) = rest
            .split_once(']')
            .with_context(|| format!("host rule `{entry}` opens `[` without a closing `]`"))?;
        let port = match after {
            "" => None,
            suffix => {
                let digits = suffix.strip_prefix(':').with_context(|| {
                    format!("host rule `{entry}` has trailing text after `]`: `{suffix}`")
                })?;
                Some(parse_port(digits, entry)?)
            }
        };
        return Ok((&entry[..inside.len() + 2], port));
    }
    // An unbracketed IPv6 literal is all colons and carries no port — `::1:443`
    // is one address, not `::1` on port 443. Testing it first is what decides
    // that reading, and it is the reading the brackets exist to override.
    if entry.parse::<IpAddr>().is_ok() {
        return Ok((entry, None));
    }
    match entry.rsplit_once(':') {
        Some((host, digits)) => {
            // Multiple colons and not an address: the writer meant a port on an
            // IPv6 host and left the brackets off.
            if host.contains(':') {
                bail!("host rule `{entry}` needs brackets around the address: `[{host}]:{digits}`");
            }
            Ok((host, Some(parse_port(digits, entry)?)))
        }
        None => Ok((entry, None)),
    }
}

fn parse_port(digits: &str, entry: &str) -> Result<u16> {
    digits
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .with_context(|| format!("host rule `{entry}` has an invalid port `{digits}`"))
}

/// Why the proxy refused a request.
///
/// Every variant reaches the user twice: as the body of the HTTP `403` the
/// agent sees, and as a [`crate::proxy::DenialEvent`] the TUI turns into a
/// notification or a "allow this domain?" prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// The sandbox is running with [`NetworkMode::None`].
    NetworkDisabled,
    /// The host matched a deny-list entry, quoted back in its canonical
    /// spelling — `.github.com` reads back as `*.github.com`, the one spelling
    /// of the subtree that the profile's own renderer also prints.
    DeniedByRule(String),
    /// [`NetworkMode::Allowlist`] and nothing in the allow list matched. This
    /// is the reason that drives the first-use domain prompt.
    NotAllowlisted,
    /// The client named a host that has no single spelling to compare — a
    /// U-label, or a shape no host can carry (see the `host` module).
    ///
    /// Refused in **every** mode, [`NetworkMode::Full`] included: a host nobody
    /// can canonicalise is precisely the one a deny rule would be dodged with,
    /// and guessing at what it denotes is how a rule ends up compared against a
    /// different destination than the one dialled. Distinct from
    /// [`DenyReason::NotAllowlisted`] on purpose — it must never raise the
    /// first-use prompt, whose answer would be a rule the profile validator
    /// then refuses to store.
    ///
    /// The payload is Friring's own explanation, never the client's bytes: a
    /// denial is rendered in the user's terminal.
    UnsupportedHost(&'static str),
    /// The destination is the *host's* own loopback, its unspecified address or
    /// a link-local one, and no address rule in the allow list names it.
    ///
    /// The proxy dials on friring's network stack, not the sandbox's, so
    /// forwarding one of these would give the boundary back the route it exists
    /// to remove — see [`crate::proxy::host_is_local`]. Refused in **every**
    /// mode, [`NetworkMode::Full`] included, because `full` describes the
    /// network the sandbox may reach and these addresses are not on it.
    ///
    /// Distinct from [`DenyReason::NotAllowlisted`] on purpose: it must never
    /// raise the first-use prompt, whose answer would be the user granting the
    /// host's own services to a sandbox on the strength of a question that
    /// looked like it was about the agent's dev server.
    HostLocal,
    /// The request line carried a method [`MethodPolicy::ReadOnly`] withholds,
    /// quoted back with anything a terminal would act on replaced.
    MethodNotAllowed(String),
    /// Missing or wrong proxy credentials — something other than the sandbox
    /// this proxy was started for.
    Unauthorized,
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NetworkDisabled => f.write_str("network access is disabled for this sandbox"),
            Self::DeniedByRule(rule) => write!(f, "host matches the deny rule `{rule}`"),
            Self::NotAllowlisted => f.write_str("host is not in the sandbox allowlist"),
            Self::UnsupportedHost(detail) => write!(f, "the requested host {detail}"),
            Self::HostLocal => f.write_str(
                "the destination is local to the machine running friring, which is outside \
                 this sandbox; add the address itself to the allowlist if that is intended",
            ),
            Self::MethodNotAllowed(method) => {
                write!(
                    f,
                    "method {method} is not allowed: this sandbox is read-only over HTTP"
                )
            }
            Self::Unauthorized => f.write_str("missing or invalid proxy credentials"),
        }
    }
}

/// The verdict on one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Connect upstream.
    Allow,
    /// Refuse, and tell the caller why.
    Deny(DenyReason),
}

impl Decision {
    /// Whether this decision permits the connection.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// The complete egress policy for one sandbox.
///
/// Evaluation order is fixed and denial-biased: mode [`NetworkMode::None`]
/// refuses outright, then the deny list, then the allow list. **Denies win**,
/// including under [`NetworkMode::Full`] — "allow everything" is still subject
/// to the explicit exceptions, which is the only reading under which a deny
/// list is worth writing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    mode: NetworkMode,
    allow: Vec<HostRule>,
    deny: Vec<HostRule>,
    methods: MethodPolicy,
}

impl Policy {
    /// An empty policy in `mode`. With [`NetworkMode::Allowlist`] and no allow
    /// entries this denies everything, like [`NetworkMode::None`] but with a
    /// reason the UI can act on.
    pub fn new(mode: NetworkMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Parse and append allow-list entries (see [`HostRule`] for the syntax).
    ///
    /// # Errors
    ///
    /// Returns the first entry that is not a valid rule, naming it — a typo in
    /// a profile must be visible rather than silently unmatched forever.
    pub fn with_allow<I, S>(mut self, entries: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.allow.extend(parse_rules(entries)?);
        Ok(self)
    }

    /// Parse and append deny-list entries (see [`HostRule`] for the syntax).
    ///
    /// # Errors
    ///
    /// As [`Policy::with_allow`].
    pub fn with_deny<I, S>(mut self, entries: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.deny.extend(parse_rules(entries)?);
        Ok(self)
    }

    /// Restrict which HTTP methods are forwarded.
    pub fn with_methods(mut self, methods: MethodPolicy) -> Self {
        self.methods = methods;
        self
    }

    /// Add one allow entry in place — the "remember this domain" answer to a
    /// first-use prompt, applied to a running proxy through
    /// [`crate::proxy::Proxy::update_policy`].
    pub fn allow_rule(&mut self, rule: HostRule) {
        if !self.allow.contains(&rule) {
            self.allow.push(rule);
        }
    }

    /// The configured network mode.
    pub fn mode(&self) -> NetworkMode {
        self.mode
    }

    /// The allow list, in evaluation order.
    pub fn allow_rules(&self) -> &[HostRule] {
        &self.allow
    }

    /// The deny list, in evaluation order.
    pub fn deny_rules(&self) -> &[HostRule] {
        &self.deny
    }

    /// The configured method restriction.
    pub fn methods(&self) -> MethodPolicy {
        self.methods
    }

    /// Decide whether `host:port` may be reached.
    ///
    /// `host` is canonicalised first, exactly once, and a host that cannot be
    /// is refused rather than compared — see [`DenyReason::UnsupportedHost`].
    /// The request path canonicalises at its edge and calls
    /// `decide_host`; this entry point exists for callers holding a
    /// host as text.
    pub fn decide(&self, host: &str, port: u16) -> Decision {
        match CanonicalHost::parse(host) {
            Ok(host) => self.decide_host(&host, port),
            Err(fault) => Decision::Deny(DenyReason::UnsupportedHost(fault.detail())),
        }
    }

    /// [`Policy::decide`] on a host already canonicalised at the edge, which is
    /// also the host that will be dialled — deciding on one spelling and
    /// connecting to another is the gap this whole path exists to close.
    pub(super) fn decide_host(&self, host: &CanonicalHost, port: u16) -> Decision {
        if self.mode == NetworkMode::None {
            return Decision::Deny(DenyReason::NetworkDisabled);
        }
        if let Some(rule) = self
            .deny
            .iter()
            .find(|rule| rule.matches_canonical(host, port))
        {
            return Decision::Deny(DenyReason::DeniedByRule(rule.to_string()));
        }
        // Before the mode, so `full` grants it no more than an allowlist does,
        // and after the denies, so a deny still wins and still names its rule.
        if let CanonicalHost::Address(address) = host {
            if host::is_host_local(*address) && !self.names_host_local(*address, port) {
                return Decision::Deny(DenyReason::HostLocal);
            }
        }
        match self.mode {
            NetworkMode::Full => Decision::Allow,
            _ if self
                .allow
                .iter()
                .any(|rule| rule.matches_canonical(host, port)) =>
            {
                Decision::Allow
            }
            _ => Decision::Deny(DenyReason::NotAllowlisted),
        }
    }

    /// Whether an allow rule names this host-local address *as an address*.
    ///
    /// The only way to reach one of them through the proxy, and deliberately
    /// the narrowest: the user wrote the literal address into the profile's
    /// allow list, so the grant is a sentence they typed rather than one the
    /// grammar handed them. Everything wider is refused —
    ///
    /// - `*` does not, because it means "the network" and this is not on it;
    /// - a **name** does not, however it resolves, because a name is a claim
    ///   somebody else controls, checked where the socket is opened;
    /// - a **deny** entry does not, because a deny list only ever narrows;
    /// - and [`NetworkMode::Full`] does not, because it consults no list.
    ///
    /// The port scope is honoured, so `127.0.0.1:11434` opens one local service
    /// rather than every one of them.
    fn names_host_local(&self, address: IpAddr, port: u16) -> bool {
        self.allow.iter().any(|rule| {
            matches!(rule.pattern, HostPattern::Address(named) if named == address)
                && rule.port.map_or(true, |scoped| scoped == port)
        })
    }

    /// Whether the proxy may open a socket to `address` for this sandbox —
    /// the `host::connect` vetting of what a name actually resolved to,
    /// answered by the same rule the decision used.
    pub(super) fn permits_address(&self, address: IpAddr, port: u16) -> bool {
        !host::is_host_local(address) || self.names_host_local(address, port)
    }

    /// Decide whether a plaintext HTTP request using `method` may be
    /// forwarded. Never consulted for `CONNECT`, which carries no method the
    /// proxy can read (see [`MethodPolicy::ReadOnly`]).
    pub fn decide_method(&self, method: &str) -> Decision {
        if self.methods.permits(method) {
            Decision::Allow
        } else {
            Decision::Deny(DenyReason::MethodNotAllowed(displayable_method(method)))
        }
    }
}

/// Longest method quoted back on a denial. Every real method is far shorter —
/// WebDAV's `VERSION-CONTROL` is the longest in any registry.
const MAX_REPORTED_METHOD: usize = 24;

/// Make a client-supplied method safe to quote back.
///
/// The method reaches the agent's transcript, a log line and a TUI notification
/// through [`DenyReason::MethodNotAllowed`], and it is chosen by the *sandboxed*
/// process — so, like the host on a denial, it must not be able to carry an
/// escape sequence into the user's terminal or an unbounded string into a
/// modal. RFC 9110 token characters survive unchanged.
fn displayable_method(method: &str) -> String {
    method
        .chars()
        .take(MAX_REPORTED_METHOD)
        .map(|character| {
            let token = character.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(character);
            if token {
                character
            } else {
                char::REPLACEMENT_CHARACTER
            }
        })
        .collect()
}

fn parse_rules<I, S>(entries: I) -> Result<Vec<HostRule>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    entries
        .into_iter()
        .map(|entry| entry.as_ref().parse::<HostRule>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(text: &str) -> HostRule {
        text.parse().expect("valid rule")
    }

    /// A bare name is that name and nothing else. The first-use prompt stores
    /// exactly this shape and promises "that host on that port only", so a bare
    /// rule that carried the subtree would make every prompt an over-grant.
    #[test]
    fn a_bare_name_is_exactly_that_name() {
        let github = rule("github.com");
        assert!(github.matches("github.com", 443));
        assert!(!github.matches("api.github.com", 443));
        assert!(!github.matches("a.b.github.com", 443));
        assert_ne!(github, rule("*.github.com"));
    }

    /// The attack the label-boundary rule exists to stop: a registrable domain
    /// that merely *ends with* the allowed text. Measured on the subtree rule,
    /// which is the only one that compares a suffix at all.
    #[test]
    fn suffix_lookalikes_do_not_match() {
        for text in ["github.com", "*.github.com"] {
            let github = rule(text);
            assert!(!github.matches("evilgithub.com", 443), "`{text}`");
            assert!(!github.matches("notgithub.com", 443), "`{text}`");
            assert!(!github.matches("github.com.evil.net", 443), "`{text}`");
            assert!(!github.matches("github.co", 443), "`{text}`");
            assert!(!github.matches("ithub.com", 443), "`{text}`");
        }
    }

    #[test]
    fn matching_ignores_case_and_the_root_dot() {
        let github = rule("*.GitHub.COM");
        assert!(github.matches("API.GitHub.com", 443));
        assert!(github.matches("github.com.", 443));
        assert!(rule("GitHub.COM").matches("github.com.", 443));
    }

    /// `*.x` and `.x` are one spelling of the subtree, apex included. The deny
    /// direction decides the apex: refusing `*.github.com` has to refuse
    /// `github.com` too. `tests/egress_matcher_conformance.rs` holds the
    /// profile-side matcher to the same reading.
    #[test]
    fn a_wildcard_prefix_is_the_subtree_and_covers_the_apex() {
        let starred = rule("*.github.com");
        for text in ["*.github.com", ".github.com"] {
            let wildcard = rule(text);
            for host in ["github.com", "api.github.com", "a.b.github.com"] {
                assert!(wildcard.matches(host, 443), "`{text}` must cover {host}");
            }
            for host in ["evilgithub.com", "github.com.evil.net", "github.co"] {
                assert!(
                    !wildcard.matches(host, 443),
                    "`{text}` must not cover {host}"
                );
            }
            assert_eq!(wildcard, starred, "`{text}` is `*.github.com`");
        }
    }

    /// An empty leading label is the deny-dodge the equality branch alone would
    /// miss: some resolvers accept `.github.com` for `github.com`, so a
    /// *request* spelled that way is the apex — even though the same text in a
    /// *rule* is the subtree.
    #[test]
    fn an_empty_leading_label_on_a_request_is_the_apex() {
        assert!(rule("github.com").matches(".github.com", 443));
        assert!(!rule("github.com").matches(".evilgithub.com", 443));
        assert!(!rule("github.com").matches(".api.github.com", 443));
        assert!(rule("*.github.com").matches(".api.github.com", 443));
    }

    /// A request host reaches the proxy as an A-label, so a rule is written the
    /// same way. The U-label spelling is refused rather than accepted as a rule
    /// nothing can match.
    #[test]
    fn international_names_are_written_in_punycode() {
        let bucher = rule("xn--bcher-kva.example");
        assert!(bucher.matches("xn--bcher-kva.example", 443));
        assert!(bucher.matches("XN--BCHER-KVA.EXAMPLE", 443));
        assert!(!bucher.matches("www.xn--bcher-kva.example", 443));
        assert!(rule("*.xn--bcher-kva.example").matches("WWW.XN--BCHER-KVA.EXAMPLE", 443));
        assert!(!bucher.matches("evilxn--bcher-kva.example", 443));
        assert!("b\u{fc}cher.example".parse::<HostRule>().is_err());
    }

    #[test]
    fn star_matches_every_host() {
        let any = rule("*");
        assert!(any.matches("github.com", 443));
        assert!(any.matches("127.0.0.1", 80));
    }

    #[test]
    fn addresses_match_exactly_never_by_suffix() {
        let loopback = rule("127.0.0.1");
        assert!(loopback.matches("127.0.0.1", 80));
        assert!(!loopback.matches("localhost", 80));
        assert!(!loopback.matches("10.127.0.0.1", 80));
        assert!(!loopback.matches("evil.127.0.0.1", 80));
        assert!(rule("::1").matches("[::1]", 80));
        assert!(rule("[::1]:443").matches("::1", 443));
        assert!(!rule("[::1]:443").matches("::1", 80));
        // The wildcard prefix normalises away before the address check, so it
        // cannot turn an address into a suffix pattern over digits.
        assert_eq!(rule("*.127.0.0.1"), loopback);
        assert!(!rule("*.127.0.0.1").matches("evil.127.0.0.1", 80));
    }

    /// An address rule is one rule per *address*, not per spelling: every row
    /// here is what `getaddrinfo(3)` reads as `127.0.0.1` and connects to, so a
    /// deny rule any of them walked past would be no deny at all.
    /// `tests/egress_matcher_conformance.rs` holds the profile-side matcher to
    /// the same table.
    #[test]
    fn one_address_rule_covers_every_spelling_of_that_address() {
        let loopback = rule("127.0.0.1");
        for spelling in [
            "127.1",
            "2130706433",
            "0x7f.0.0.1",
            "0177.0.0.1",
            "127.000.000.001",
            "::ffff:127.0.0.1",
            "::ffff:7f00:1",
        ] {
            assert!(loopback.matches(spelling, 80), "`{spelling}`");
            // …and a rule written that way is the same rule, so the two lists
            // cannot disagree about which one the user meant.
            assert_eq!(rule(spelling), loopback, "rule `{spelling}`");
        }
        // Folding must not reach past the address: a *name* carrying the digits
        // is still a name, and the deprecated IPv4-compatible form is a
        // different destination (`::1` lives in that range).
        for spelling in ["127.0.0.1.evil.net", "127.1.evil.net", "::127.0.0.1", "::1"] {
            assert!(!loopback.matches(spelling, 80), "`{spelling}`");
        }
    }

    /// The deny direction under `full`, where a rule is the only thing between
    /// the agent and the host: a spelling nobody can canonicalise is refused
    /// outright rather than compared, and with a reason of its own — reporting
    /// it as merely unlisted would raise the first-use prompt and offer to
    /// store a rule the profile validator refuses.
    #[test]
    fn a_host_that_does_not_canonicalise_is_refused_in_every_mode() {
        let policy = Policy::new(NetworkMode::Full)
            .with_deny(["xn--bcher-kva.example"])
            .expect("valid rules");
        for host in ["b\u{fc}cher.example", "evil.example..", "[evil.example]"] {
            assert!(
                matches!(
                    policy.decide(host, 443),
                    Decision::Deny(DenyReason::UnsupportedHost(_))
                ),
                "`{host}` was not refused"
            );
        }
        assert!(policy.decide("example.test", 443).is_allowed());
        assert_eq!(
            Policy::new(NetworkMode::Allowlist).decide("b\u{fc}cher.example", 443),
            Decision::Deny(DenyReason::UnsupportedHost(
                "is an international name and must be written in punycode, e.g. \
                 `xn--bcher-kva.example`"
            ))
        );
    }

    #[test]
    fn port_scoping_narrows_a_rule() {
        let https_only = rule("*.github.com:443");
        assert!(https_only.matches("api.github.com", 443));
        assert!(!https_only.matches("api.github.com", 8080));
        assert!(!rule("github.com:443").matches("api.github.com", 443));
        assert!(rule("github.com").matches("github.com", 8080));
    }

    #[test]
    fn rules_round_trip_through_display() {
        for text in [
            "*",
            "github.com",
            "*.github.com",
            "github.com:443",
            "*.github.com:443",
            "[::1]",
            "[::1]:443",
            "127.0.0.1",
        ] {
            assert_eq!(rule(text).to_string(), text, "round-trip of `{text}`");
        }
        // Normalising forms collapse onto the canonical rendering — including
        // the dotted spelling of the subtree, and every spelling of an address.
        // The star itself is *not* normalised away: it is what the rule means.
        assert_eq!(rule(".github.com").to_string(), "*.github.com");
        assert_eq!(rule("*.GitHub.com.").to_string(), "*.github.com");
        assert_eq!(rule("GitHub.com.").to_string(), "github.com");
        assert_eq!(rule("127.1").to_string(), "127.0.0.1");
        assert_eq!(rule("[::ffff:127.0.0.1]").to_string(), "127.0.0.1");
        assert_eq!(rule("::0001").to_string(), "[::1]");
    }

    #[test]
    fn malformed_rules_are_rejected_with_the_entry_named() {
        let long_label = format!("{}.com", "a".repeat(64));
        let long_host = ["averyverylonglabelindeed"; 12].join(".");
        let malformed = [
            "",
            "   ",
            "github.com:0",
            "github.com:70000",
            "github.com:https",
            "[::1",
            "[::1]443",
            "1:2:3:4:5:6:7:8:443",
            "https://github.com",
            "api.*.com",
            "git\thub.com",
            "github.com\u{1b}[2J",
            // Shapes no request host can ever be spelled as, so a rule written
            // this way would sit in a deny list matching nothing.
            "github..com",
            "-github.com",
            "github.com-",
            ".*",
            // One leading dot is the subtree prefix; a second is an empty
            // label, and a prefix with nothing after it names no host.
            "..github.com",
            "*..github.com",
            "*.",
            "*.*.github.com",
            "b\u{fc}cher.example",
            long_label.as_str(),
            long_host.as_str(),
        ];
        for text in malformed {
            let error = text.parse::<HostRule>().err().map(|e| e.to_string());
            let error = error.unwrap_or_else(|| panic!("`{text}` must not parse"));
            assert!(
                error.contains(text.trim()) || text.trim().is_empty(),
                "opaque: {error}"
            );
        }
        // An unbracketed address with a port cannot be told apart from a longer
        // address, so the error says which form to write.
        assert!("1:2:3:4:5:6:7:8:443"
            .parse::<HostRule>()
            .expect_err("needs brackets")
            .to_string()
            .contains("[1:2:3:4:5:6:7:8]:443"));
    }

    /// `::1:443` is a valid IPv6 *address*, not `::1` on port 443, and is read
    /// as one. Brackets are how a port is expressed.
    #[test]
    fn an_unbracketed_ipv6_literal_is_an_address_not_a_host_and_port() {
        let address = rule("::1:443");
        assert_eq!(address.port(), None);
        assert!(address.matches("::1:443", 80));
        assert!(!address.matches("::1", 443));
        assert_eq!(rule("[::1]:443").port(), Some(443));
    }

    #[test]
    fn mode_none_refuses_everything_it_was_handed() {
        let policy = Policy::new(NetworkMode::None)
            .with_allow(["github.com"])
            .expect("valid rules");
        assert_eq!(
            policy.decide("github.com", 443),
            Decision::Deny(DenyReason::NetworkDisabled)
        );
    }

    #[test]
    fn mode_full_allows_what_is_not_denied() {
        let policy = Policy::new(NetworkMode::Full)
            .with_deny(["*.evil.example"])
            .expect("valid rules");
        assert!(policy.decide("anything.example", 443).is_allowed());
        // The rule is quoted back as written, star included: the denial reaches
        // the user, and a subtree deny reported as a bare host would read as a
        // narrower rule than the one that fired.
        assert_eq!(
            policy.decide("api.evil.example", 443),
            Decision::Deny(DenyReason::DeniedByRule("*.evil.example".into()))
        );
        // A bare deny is one host, so the subdomain is left alone.
        let apex_only = Policy::new(NetworkMode::Full)
            .with_deny(["evil.example"])
            .expect("valid rules");
        assert!(apex_only.decide("api.evil.example", 443).is_allowed());
        assert!(!apex_only.decide("evil.example", 443).is_allowed());
    }

    #[test]
    fn deny_beats_allow_even_for_a_subdomain_of_an_allowed_host() {
        let policy = Policy::new(NetworkMode::Allowlist)
            .with_allow(["*.github.com"])
            .expect("valid rules")
            .with_deny(["gist.github.com"])
            .expect("valid rules");
        assert!(policy.decide("api.github.com", 443).is_allowed());
        assert_eq!(
            policy.decide("gist.github.com", 443),
            Decision::Deny(DenyReason::DeniedByRule("gist.github.com".into()))
        );
    }

    #[test]
    fn allowlist_without_a_match_reports_the_prompt_worthy_reason() {
        let policy = Policy::new(NetworkMode::Allowlist)
            .with_allow(["github.com"])
            .expect("valid rules");
        assert_eq!(
            policy.decide("pypi.org", 443),
            Decision::Deny(DenyReason::NotAllowlisted)
        );
    }

    #[test]
    fn an_empty_allowlist_denies() {
        assert_eq!(
            Policy::new(NetworkMode::Allowlist).decide("github.com", 443),
            Decision::Deny(DenyReason::NotAllowlisted)
        );
    }

    /// The live half of a first-use answer. The grant is exactly the rule the
    /// user was shown — the subtree is not thrown in — and answering twice
    /// leaves one entry.
    #[test]
    fn allow_rule_extends_a_live_policy_once() {
        let mut policy = Policy::new(NetworkMode::Allowlist);
        policy.allow_rule(rule("pypi.org:443"));
        policy.allow_rule(rule("pypi.org:443"));
        assert!(policy.decide("pypi.org", 443).is_allowed());
        assert!(!policy.decide("files.pypi.org", 443).is_allowed());
        assert!(!policy.decide("pypi.org", 80).is_allowed());
        assert_eq!(policy.allow_rules().len(), 1);
    }

    #[test]
    fn read_only_methods_withhold_writes() {
        let policy = Policy::new(NetworkMode::Full).with_methods(MethodPolicy::ReadOnly);
        for method in ["GET", "HEAD", "OPTIONS"] {
            assert!(policy.decide_method(method).is_allowed(), "{method}");
        }
        assert_eq!(
            policy.decide_method("POST"),
            Decision::Deny(DenyReason::MethodNotAllowed("POST".into()))
        );
        assert!(Policy::new(NetworkMode::Full)
            .decide_method("POST")
            .is_allowed());
    }

    /// The method on a denial is chosen by the *sandboxed* process and is
    /// rendered by the TUI, exactly like the host on one — so it must not carry
    /// an escape sequence out of the sandbox, nor an unbounded string into a
    /// modal.
    #[test]
    fn a_refused_method_cannot_smuggle_control_characters() {
        let policy = Policy::new(NetworkMode::Full).with_methods(MethodPolicy::ReadOnly);
        let Decision::Deny(DenyReason::MethodNotAllowed(quoted)) =
            policy.decide_method("PO\u{1b}[2JST")
        else {
            panic!("a method outside the read-only set must be refused");
        };
        // Every character outside an RFC 9110 token goes, not just the escape:
        // a method has no use for the rest, and `[` is half of the sequence.
        assert_eq!(quoted, "PO\u{fffd}\u{fffd}2JST");
        assert!(!quoted.chars().any(char::is_control));

        let Decision::Deny(DenyReason::MethodNotAllowed(capped)) =
            policy.decide_method(&"M".repeat(4096))
        else {
            panic!("an over-long method must be refused");
        };
        assert_eq!(capped.chars().count(), MAX_REPORTED_METHOD);
    }

    #[test]
    fn default_policy_denies() {
        let policy = Policy::default();
        assert_eq!(policy.mode(), NetworkMode::None);
        assert!(!policy.decide("github.com", 443).is_allowed());
    }

    #[test]
    fn modes_serialise_as_the_profile_column_words() {
        for (mode, word) in [
            (NetworkMode::None, "none"),
            (NetworkMode::Allowlist, "allowlist"),
            (NetworkMode::Full, "full"),
        ] {
            assert_eq!(
                serde_json::to_string(&mode).expect("serialises"),
                format!("\"{word}\"")
            );
            assert_eq!(mode.to_string(), word);
        }
    }
}
