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
    /// `example.com`, `*.example.com` or `.example.com` — the name itself and
    /// every subdomain of it. The three spellings are one pattern; see
    /// [`HostRule`] for why the wildcard does not narrow.
    Domain(String),
    /// `127.0.0.1`, `[::1]` — compared as a parsed address, never by suffix.
    Address(IpAddr),
}

impl HostPattern {
    fn matches(&self, host: &str) -> bool {
        let host = normalise_host(host);
        match self {
            Self::Any => true,
            Self::Address(ip) => host.parse::<IpAddr>().is_ok_and(|h| h == *ip),
            Self::Domain(domain) => covers(host, domain),
        }
    }
}

/// Whether `host` is `domain` itself or sits under it on a label boundary.
///
/// The boundary is the whole point: `evilgithub.com` merely *ends with* the
/// text of `github.com`, and reading that as a match is the classic allowlist
/// bypass, so whatever precedes the suffix must be terminated by a `.`.
///
/// A candidate carrying an empty leading label (`.github.com`) is covered. That
/// is the deny-safe reading — a rule refusing `github.com` must not be dodged
/// by a spelling some resolvers accept — and it grants nothing extra, because
/// no such name is registrable.
///
/// This must stay behaviourally identical to `session::DomainRule::matches_host`,
/// which the two modules cannot share because the proxy is a leaf in the
/// architecture allowlist. `tests/egress_matcher_conformance.rs` runs both over
/// one table and fails when either side drifts.
fn covers(host: &str, domain: &str) -> bool {
    if host.eq_ignore_ascii_case(domain) {
        return true;
    }
    let (host_len, domain_len) = (host.len(), domain.len());
    // The `.` separator has to fit, so the host is strictly the longer string.
    host_len > domain_len
        && host.as_bytes()[host_len - domain_len - 1] == b'.'
        // Safe to slice: the preceding byte is ASCII `.`, so this is a char
        // boundary.
        && host[host_len - domain_len..].eq_ignore_ascii_case(domain)
}

/// Strip the presentation noise a request host can carry: the root label's
/// trailing dot (`github.com.`) and the brackets around an IPv6 literal.
fn normalise_host(host: &str) -> &str {
    let host = host.trim().trim_end_matches('.');
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// One entry of an allow or deny list: a host pattern with an optional port.
///
/// Written as `host`, `host:port`, or — for an IPv6 literal with a port —
/// `[addr]:port`. A rule without a port matches every port.
///
/// # Matching
///
/// The request host is compared case-insensitively after a trailing root dot
/// and IPv6 brackets are stripped:
///
/// | Rule | Matches | Does not match |
/// |---|---|---|
/// | `github.com` | `github.com`, `api.github.com`, `a.b.github.com` | `evilgithub.com`, `github.com.evil.net`, `github.co` |
/// | `*.github.com`, `.github.com` | exactly what `github.com` matches | exactly what `github.com` does not |
/// | `github.com:443` | `api.github.com` port 443 | `github.com` port 8080 |
/// | `127.0.0.1` | `127.0.0.1` | `evil.127.0.0.1`, `localhost` |
/// | `*` | every host | — |
///
/// A bare name therefore covers itself **and** its subtree, which is what a
/// user writing `github.com` means. An address rule is exact: suffix logic on
/// digits would be nonsense, and `localhost` is a *name*, matched as one, so it
/// does not cover `127.0.0.1` unless listed too.
///
/// **`*.` and `.` are spellings, not narrowings.** `*.github.com` covers the
/// apex as well, because the deny direction settles the question: a user who
/// denies `*.github.com` means "no github.com traffic", and a matcher that let
/// the apex through would be a silent hole. Reading the same spelling two ways
/// depending on which list it sits in would be worse still.
///
/// The accepted grammar is deliberately the one a sandbox profile's
/// `network_allow` / `network_deny` accepts (`session::DomainRule`), minus `*`,
/// which only this side has: ASCII labels of letters, digits, `-` and `_`, no
/// empty label, none edged with `-`, 63 bytes per label and 253 overall. An
/// international name must be written in punycode, because that is what a
/// request host is spelled as on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRule {
    pattern: HostPattern,
    port: Option<u16>,
}

impl HostRule {
    /// Whether this rule covers `host` on `port`.
    pub fn matches(&self, host: &str, port: u16) -> bool {
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
        let host = normalise_host(host);
        let pattern = if host == "*" {
            HostPattern::Any
        } else {
            // Normalise the spelling, then classify. Stripping `*.` before the
            // address check is what keeps `*.127.0.0.1` and `127.0.0.1` one
            // rule instead of an address and a suffix pattern over digits.
            let bare = host
                .strip_prefix("*.")
                .or_else(|| host.strip_prefix('.'))
                .unwrap_or(host);
            match bare.parse::<IpAddr>() {
                Ok(ip) => HostPattern::Address(ip),
                Err(_) => HostPattern::Domain(validated_domain(bare, entry)?),
            }
        };
        Ok(Self { pattern, port })
    }
}

impl fmt::Display for HostRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.pattern {
            HostPattern::Any => f.write_str("*")?,
            HostPattern::Domain(domain) => f.write_str(domain)?,
            // Re-bracket IPv6 so the rendering parses back to the same rule.
            HostPattern::Address(ip @ IpAddr::V6(_)) if self.port.is_some() => write!(f, "[{ip}]")?,
            HostPattern::Address(ip) => write!(f, "{ip}")?,
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

/// The DNS limits, which double as the reason a longer rule is a typo rather
/// than a host: nothing on the wire can be spelled that way, so the rule would
/// never match — and a deny rule that never matches is a hole.
const MAX_HOST_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;

/// Reject the shapes that would silently never match — an empty label, a stray
/// wildcard, a URL pasted in where a host belongs, an international name in
/// U-label form.
///
/// The grammar is deliberately the one `session::DomainRule` enforces on a
/// stored profile, so a spelling the profile editor refuses is also one this
/// proxy refuses to load rather than accepting as a rule that can match
/// nothing. Punctuation is rejected outright as well as being useless: a rule
/// is echoed back in denial text and in the UI.
fn validated_domain(domain: &str, entry: &str) -> Result<String> {
    let domain = normalise_host(domain);
    if domain.is_empty() {
        bail!("host rule `{entry}` has no host");
    }
    if domain.len() > MAX_HOST_LEN {
        bail!("host rule `{entry}` is longer than {MAX_HOST_LEN} characters");
    }
    for label in domain.split('.') {
        if label.is_empty() {
            bail!("host rule `{entry}` has an empty label");
        }
        if label.len() > MAX_LABEL_LEN {
            bail!("host rule `{entry}` has a label longer than {MAX_LABEL_LEN} characters");
        }
        if label.starts_with('-') || label.ends_with('-') {
            bail!("host rule `{entry}` has a label edged with `-`");
        }
        if let Some(bad) = label
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
        {
            bail!(
                "host rule `{entry}` contains `{}`: write a bare ASCII host, e.g. \
                 `api.github.com:443` (an international name in punycode)",
                bad.escape_debug()
            );
        }
    }
    Ok(domain.to_ascii_lowercase())
}

/// Split `host[:port]`, handling the bracketed IPv6 form and refusing the
/// ambiguous bare one (`::1:443` could be either).
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
        return Ok((inside, port));
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
    /// spelling — `*.github.com` reads back as `github.com`, which is what the
    /// rule means and what the profile's own renderer prints for it.
    DeniedByRule(String),
    /// [`NetworkMode::Allowlist`] and nothing in the allow list matched. This
    /// is the reason that drives the first-use domain prompt.
    NotAllowlisted,
    /// The request line carried a method [`MethodPolicy::ReadOnly`] withholds.
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
    pub fn decide(&self, host: &str, port: u16) -> Decision {
        if self.mode == NetworkMode::None {
            return Decision::Deny(DenyReason::NetworkDisabled);
        }
        if let Some(rule) = self.deny.iter().find(|rule| rule.matches(host, port)) {
            return Decision::Deny(DenyReason::DeniedByRule(rule.to_string()));
        }
        match self.mode {
            NetworkMode::Full => Decision::Allow,
            _ if self.allow.iter().any(|rule| rule.matches(host, port)) => Decision::Allow,
            _ => Decision::Deny(DenyReason::NotAllowlisted),
        }
    }

    /// Decide whether a plaintext HTTP request using `method` may be
    /// forwarded. Never consulted for `CONNECT`, which carries no method the
    /// proxy can read (see [`MethodPolicy::ReadOnly`]).
    pub fn decide_method(&self, method: &str) -> Decision {
        if self.methods.permits(method) {
            Decision::Allow
        } else {
            Decision::Deny(DenyReason::MethodNotAllowed(method.to_string()))
        }
    }
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

    #[test]
    fn bare_name_covers_itself_and_its_subtree() {
        let github = rule("github.com");
        assert!(github.matches("github.com", 443));
        assert!(github.matches("api.github.com", 443));
        assert!(github.matches("a.b.github.com", 443));
    }

    /// The attack the label-boundary rule exists to stop: a registrable domain
    /// that merely *ends with* the allowed text.
    #[test]
    fn suffix_lookalikes_do_not_match() {
        let github = rule("github.com");
        assert!(!github.matches("evilgithub.com", 443));
        assert!(!github.matches("notgithub.com", 443));
        assert!(!github.matches("github.com.evil.net", 443));
        assert!(!github.matches("github.co", 443));
        assert!(!github.matches("ithub.com", 443));
    }

    #[test]
    fn matching_ignores_case_and_the_root_dot() {
        let github = rule("GitHub.COM");
        assert!(github.matches("API.GitHub.com", 443));
        assert!(github.matches("github.com.", 443));
    }

    /// `*.x` and `.x` are spellings of `x`, apex included. The deny direction
    /// decides it: refusing `*.github.com` has to refuse `github.com` too.
    /// `tests/egress_matcher_conformance.rs` holds the profile-side matcher to
    /// the same reading.
    #[test]
    fn a_wildcard_prefix_is_a_spelling_not_a_narrowing() {
        let bare = rule("github.com");
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
            assert_eq!(wildcard, bare, "`{text}` is `github.com`");
        }
    }

    /// An empty leading label is the deny-dodge the equality branch alone would
    /// miss: some resolvers accept `.github.com` for `github.com`.
    #[test]
    fn an_empty_leading_label_is_covered() {
        assert!(rule("github.com").matches(".github.com", 443));
        assert!(!rule("github.com").matches(".evilgithub.com", 443));
    }

    /// A request host reaches the proxy as an A-label, so a rule is written the
    /// same way. The U-label spelling is refused rather than accepted as a rule
    /// nothing can match.
    #[test]
    fn international_names_are_written_in_punycode() {
        let bucher = rule("xn--bcher-kva.example");
        assert!(bucher.matches("xn--bcher-kva.example", 443));
        assert!(bucher.matches("WWW.XN--BCHER-KVA.EXAMPLE", 443));
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

    #[test]
    fn port_scoping_narrows_a_rule() {
        let https_only = rule("github.com:443");
        assert!(https_only.matches("api.github.com", 443));
        assert!(!https_only.matches("api.github.com", 8080));
        assert!(rule("github.com").matches("github.com", 8080));
    }

    #[test]
    fn rules_round_trip_through_display() {
        for text in [
            "*",
            "github.com",
            "github.com:443",
            "[::1]:443",
            "127.0.0.1",
        ] {
            assert_eq!(rule(text).to_string(), text, "round-trip of `{text}`");
        }
        // Normalising forms collapse onto the canonical rendering — including
        // the wildcard, which is a spelling rather than a pattern of its own.
        assert_eq!(rule("*.github.com").to_string(), "github.com");
        assert_eq!(rule(".github.com").to_string(), "github.com");
        assert_eq!(rule("GitHub.com.").to_string(), "github.com");
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
            .with_deny(["evil.example"])
            .expect("valid rules");
        assert!(policy.decide("anything.example", 443).is_allowed());
        assert_eq!(
            policy.decide("api.evil.example", 443),
            Decision::Deny(DenyReason::DeniedByRule("evil.example".into()))
        );
    }

    #[test]
    fn deny_beats_allow_even_for_a_subdomain_of_an_allowed_host() {
        let policy = Policy::new(NetworkMode::Allowlist)
            .with_allow(["github.com"])
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

    #[test]
    fn allow_rule_extends_a_live_policy_once() {
        let mut policy = Policy::new(NetworkMode::Allowlist);
        policy.allow_rule(rule("pypi.org"));
        policy.allow_rule(rule("pypi.org"));
        assert!(policy.decide("files.pypi.org", 443).is_allowed());
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
