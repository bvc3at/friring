//! One canonical spelling for every host, settled before any rule is consulted.
//!
//! A host arrives in whatever form the sandboxed client felt like writing:
//! `GitHub.COM.`, `[::ffff:127.0.0.1]`, `127.1`, `2130706433`. Several of those
//! are read by the resolver — and therefore by the *connection* — as something
//! other than their text, so a matcher comparing text alone can be asked about
//! one host and hand its verdict to another. `127.1` is `127.0.0.1` to
//! `getaddrinfo(3)` and to no string comparison at all, which under a deny list
//! is a bypass: the rule names the address, the request names a spelling of it,
//! and the two never meet.
//!
//! Every host is therefore canonicalised **once, here**, before it is compared
//! and before it is dialled. [`connect`] dials the canonical form, so what the
//! policy decided on is what the socket goes to — an address never returns to
//! the resolver to be read a second, possibly different, way. A rule goes
//! through the same function at parse time, which is what makes a rule and a
//! request comparable at all.
//!
//! A host that cannot be canonicalised is an [`Err`], never a guess, and every
//! caller turns that into a refusal: under a deny list a host nobody can
//! compare is exactly the one that must not travel.
//!
//! # The IDNA subset, and what is left out
//!
//! **A host containing any non-ASCII byte is refused.** Canonicalising an
//! international name correctly means UTS-46: case folding, NFC normalisation,
//! and tables of disallowed and deviation characters. Only the punycode
//! *encoding* half of that is table-free; the mapping half needs Unicode data
//! this crate does not carry.
//!
//! Encoding without mapping would be worse than refusing. `café.example`
//! written with a combining accent and the same name written pre-composed
//! encode to *different* A-labels, so a deny rule on the punycode form would
//! still be dodged — while the feature looked closed. A U-label is refused with
//! the punycode form named as the fix, in the request path and in the rule
//! parser alike, and `xn--` A-labels (what actually travels on the wire) are
//! ordinary ASCII here and always worked.
//!
//! A UTS-46 dependency would buy exactly one thing: rules and requests written
//! in Unicode, mapped to the same A-label. Nothing else in this module needs
//! it.
//!
//! This must stay behaviourally identical to the profile-side canonicaliser in
//! `session::sandbox_profile`, which the two modules cannot share because the
//! proxy is a leaf in the architecture allowlist.
//! `tests/egress_matcher_conformance.rs` runs both over one table of spellings
//! and fails when either side drifts.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::net::TcpStream;

/// The DNS limits (RFC 1035 §2.3.4), which double as the reason a longer
/// string is a typo rather than a host: nothing on the wire can be spelled that
/// way, so a rule written like that would match nothing — and a deny rule that
/// matches nothing is a hole.
const MAX_HOST_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;

/// A host reduced to the one spelling both a rule and a request are compared
/// in: a parsed address, or a lowercase ASCII name.
///
/// The two variants never match each other. `localhost` is a *name*, so it does
/// not cover `127.0.0.1` unless that is listed too, and `127.0.0.1` is an
/// address, so it has no subtree for `evil.127.0.0.1` to sit in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CanonicalHost {
    /// Compared as a parsed address, so `::1`, `::0001` and `[::1]` are one
    /// host and `127.1` is another spelling of `127.0.0.1`.
    Address(IpAddr),
    /// Lowercase, no trailing root dot, no brackets, ASCII only.
    Domain(String),
}

impl CanonicalHost {
    /// Canonicalise one host as a client wrote it, or as a rule stores it.
    ///
    /// # Errors
    ///
    /// [`HostFault`] for anything that cannot be reduced to a single spelling:
    /// an international name (see the module docs), a shape no request host can
    /// carry, or one over the DNS limits.
    pub(super) fn parse(raw: &str) -> Result<Self, HostFault> {
        let host = raw.trim();
        // Checked over the whole string, and first, so an international name is
        // told to use punycode rather than reported as a stray character.
        if !host.is_ascii() {
            return Err(HostFault::International);
        }
        let (host, bracketed) = match host.strip_prefix('[') {
            Some(inside) => (inside.strip_suffix(']').ok_or(HostFault::Brackets)?, true),
            None => (host, false),
        };
        // The root label is presentation: `github.com.` and `github.com` are one
        // name. Exactly one dot goes — a second would leave an empty label, which
        // no name has and no resolver answers for.
        let host = host.strip_suffix('.').unwrap_or(host);
        // An empty *leading* label is the deny-safe reading of `.github.com`:
        // the resolvers that accept that spelling resolve the apex, so a rule on
        // the apex has to reach it. It grants nothing extra, because no such
        // name is registrable.
        let host = host.strip_prefix('.').unwrap_or(host);
        if host.is_empty() {
            return Err(HostFault::Empty);
        }
        // Address readings come first, exactly as they do in the resolver: a
        // string `inet_aton(3)` accepts is an address there, so treating it as a
        // name here would compare one thing and connect to another.
        if let Some(address) = parse_address(host) {
            return Ok(Self::Address(address));
        }
        if bracketed {
            // Brackets are address syntax. `[github.com]` is not a host any
            // client writes, and reading it as a name would be a second
            // spelling of one rule.
            return Err(HostFault::Brackets);
        }
        validate_domain(host)?;
        Ok(Self::Domain(host.to_ascii_lowercase()))
    }
}

impl fmt::Display for CanonicalHost {
    /// The spelling to put back in front of a user — an authority, so an IPv6
    /// literal is bracketed and reads correctly beside a `:port`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Address(IpAddr::V6(address)) => write!(f, "[{address}]"),
            Self::Address(address) => write!(f, "{address}"),
            Self::Domain(name) => f.write_str(name),
        }
    }
}

/// Why a host could not be canonicalised.
///
/// [`HostFault::detail`] is Friring's own text and never the client's bytes: a
/// refusal is quoted into the agent's transcript, into a log line and into a
/// TUI notification, and a host chosen by a sandboxed process must not be able
/// to ride any of those into the user's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostFault {
    Empty,
    TooLong,
    LabelTooLong,
    EmptyLabel,
    HyphenEdge,
    /// A U-label — see the module docs on the IDNA subset.
    International,
    Character,
    Brackets,
}

impl HostFault {
    /// The clause naming this fault, written to follow a subject — "host rule
    /// x …" when a rule is parsed, "the requested host …" on a denial.
    pub(super) fn detail(self) -> &'static str {
        match self {
            Self::Empty => "has no host",
            Self::TooLong => "is longer than 253 characters",
            Self::LabelTooLong => "has a label longer than 63 characters",
            Self::EmptyLabel => "has an empty label",
            Self::HyphenEdge => "has a label edged with `-`",
            Self::International => {
                "is an international name and must be written in punycode, e.g. \
                 `xn--bcher-kva.example`"
            }
            Self::Character => "contains a character a host name cannot carry",
            Self::Brackets => "has brackets around something that is not an address",
        }
    }
}

/// Dial exactly the host the policy decided on.
///
/// An address is dialled *as* an address, so the resolver never gets a second
/// look at a spelling like `127.1` after the decision was taken on
/// `127.0.0.1`. A name is resolved here and nowhere else, which is the
/// `socks5h` half of the contract: the proxy sees the name, the client never
/// resolves one behind its back.
pub(super) async fn connect(host: &CanonicalHost, port: u16) -> io::Result<TcpStream> {
    match host {
        CanonicalHost::Address(address) => {
            TcpStream::connect(SocketAddr::new(*address, port)).await
        }
        CanonicalHost::Domain(name) => TcpStream::connect((name.as_str(), port)).await,
    }
}

/// Every address spelling that reaches the same endpoint, read as that endpoint.
fn parse_address(host: &str) -> Option<IpAddr> {
    if let Ok(address) = host.parse::<IpAddr>() {
        return Some(fold_mapped(address));
    }
    parse_legacy_ipv4(host).map(IpAddr::V4)
}

/// Fold an IPv4-mapped IPv6 address onto the IPv4 address it *is*:
/// `::ffff:127.0.0.1` and `::ffff:7f00:1` reach the same endpoint as
/// `127.0.0.1`, so a deny rule on one has to catch the others.
///
/// The deprecated IPv4-**compatible** form (`::127.0.0.1`) is deliberately not
/// folded: it is a distinct IPv6 destination that no mainstream stack
/// translates, and `::1` itself sits inside that range — folding it would read
/// loopback as `0.0.0.1`.
fn fold_mapped(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address => address,
    }
}

/// Parse the pre-CIDR spellings `inet_aton(3)` accepts and every mainstream
/// resolver still honours: `127.1`, `2130706433`, `0x7f.0.0.1`, `0177.0.0.1`,
/// `127.000.000.001`.
///
/// One to four parts; each decimal, octal (a leading `0`) or hex (`0x`); every
/// part but the last is one octet and the last fills what remains. Anything
/// else is `None` and goes on to be read as a name — which is also what the
/// resolver does with it.
fn parse_legacy_ipv4(host: &str) -> Option<Ipv4Addr> {
    let mut parts = [0u32; 4];
    let mut count = 0usize;
    for part in host.split('.') {
        let slot = parts.get_mut(count)?;
        *slot = parse_ipv4_part(part)?;
        count += 1;
    }
    let (last, leading) = parts[..count].split_last()?;
    if leading.iter().any(|octet| *octet > 0xff) {
        return None;
    }
    // The last part carries every octet the leading ones did not name, so
    // `127.1` is `127.0.0.1` and `2130706433` is the whole address.
    let remaining_bits = 32 - 8 * u32::try_from(leading.len()).ok()?;
    if u64::from(*last) > (1u64 << remaining_bits) - 1 {
        return None;
    }
    let mut value = *last;
    for (index, octet) in leading.iter().enumerate() {
        value |= octet << (24 - 8 * index);
    }
    Some(Ipv4Addr::from(value))
}

/// One part of a legacy IPv4 literal, in the radix its prefix names.
fn parse_ipv4_part(part: &str) -> Option<u32> {
    let (digits, radix) = match part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
        Some(hex) => (hex, 16),
        // A leading zero is octal, which is why `010.0.0.1` is `8.0.0.1` and
        // stripping zeros would be wrong. A lone `0` is just zero.
        None if part.len() > 1 && part.starts_with('0') => (&part[1..], 8),
        None => (part, 10),
    };
    if digits.is_empty() || !digits.chars().all(|digit| digit.is_digit(radix)) {
        return None;
    }
    u32::from_str_radix(digits, radix).ok()
}

/// The one grammar a stored rule and a request host share: ASCII labels of
/// letters, digits, `-` and `_`, none empty, none edged with `-`, 63 bytes per
/// label and 253 overall.
///
/// One grammar on both sides is the point: a spelling that could never be
/// written as a rule can never be *allowed* either, so it is refused rather
/// than compared. `_` stays legal because service names use it.
fn validate_domain(host: &str) -> Result<(), HostFault> {
    if host.len() > MAX_HOST_LEN {
        return Err(HostFault::TooLong);
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(HostFault::EmptyLabel);
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(HostFault::LabelTooLong);
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(HostFault::HyphenEdge);
        }
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        {
            return Err(HostFault::Character);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(raw: &str) -> CanonicalHost {
        CanonicalHost::parse(raw).unwrap_or_else(|fault| panic!("`{raw}` {}", fault.detail()))
    }

    fn address(raw: &str) -> IpAddr {
        match canonical(raw) {
            CanonicalHost::Address(address) => address,
            CanonicalHost::Domain(name) => panic!("`{raw}` read as the name `{name}`"),
        }
    }

    #[test]
    fn presentation_noise_collapses_onto_one_name() {
        for raw in [
            "github.com",
            "GitHub.COM",
            "  github.com  ",
            "github.com.",
            ".github.com",
            ".GitHub.com.",
        ] {
            assert_eq!(
                canonical(raw),
                CanonicalHost::Domain("github.com".into()),
                "`{raw}`"
            );
        }
    }

    /// The spellings `getaddrinfo(3)` resolves to loopback, which a text
    /// comparison would hand straight past a deny rule on the address.
    #[test]
    fn every_loopback_spelling_is_the_same_address() {
        let loopback: IpAddr = Ipv4Addr::LOCALHOST.into();
        for raw in [
            "127.0.0.1",
            "127.1",
            "127.0.1",
            "2130706433",
            "0x7f.1",
            "0x7f.0.0.1",
            "0x7f000001",
            "0177.0.0.1",
            "127.000.000.001",
            "127.0.0.1.",
            "::ffff:127.0.0.1",
            "[::ffff:127.0.0.1]",
            "::ffff:7f00:1",
        ] {
            assert_eq!(address(raw), loopback, "`{raw}`");
        }
    }

    /// A leading zero is octal, so trimming zeros rather than reading them
    /// would put `010.0.0.1` at the wrong address entirely.
    #[test]
    fn a_leading_zero_is_octal_not_decoration() {
        assert_eq!(address("010.0.0.1"), IpAddr::from([8, 0, 0, 1]));
        assert_eq!(address("0.0.0.0"), IpAddr::from([0, 0, 0, 0]));
        assert_eq!(address("00"), IpAddr::from([0, 0, 0, 0]));
        // `09` is not octal, so this is not an address at all — which is also
        // what the resolver concludes, and it then looks the name up.
        assert_eq!(
            canonical("127.09.0.1"),
            CanonicalHost::Domain("127.09.0.1".into())
        );
    }

    #[test]
    fn a_name_that_merely_starts_with_digits_stays_a_name() {
        for raw in [
            "127.0.0.1.evil.net",
            "1.2.3.4.5",
            "127.0.0.256",
            "0x7f.1.evil.net",
            "2130706433.evil.net",
        ] {
            assert_eq!(canonical(raw), CanonicalHost::Domain(raw.into()), "`{raw}`");
        }
    }

    #[test]
    fn ipv6_spellings_collapse_and_the_mapped_form_folds() {
        for raw in ["::1", "[::1]", "::0001", "0:0:0:0:0:0:0:1", "[::0001]"] {
            assert_eq!(
                address(raw),
                "::1".parse::<IpAddr>().expect("loopback"),
                "`{raw}`"
            );
        }
        // The deprecated IPv4-compatible form is a different destination, and
        // folding it would also read `::1` as `0.0.0.1`.
        assert_eq!(
            address("::127.0.0.1"),
            "::7f00:1".parse::<IpAddr>().expect("v4-compatible")
        );
        assert_ne!(address("::127.0.0.1"), address("127.0.0.1"));
    }

    #[test]
    fn a_u_label_is_refused_rather_than_guessed_at() {
        let fault = CanonicalHost::parse("b\u{fc}cher.example").expect_err("refused");
        assert_eq!(fault, HostFault::International);
        assert!(fault.detail().contains("punycode"));
        // The A-label is ordinary ASCII and always worked.
        assert_eq!(
            canonical("XN--BCHER-KVA.example"),
            CanonicalHost::Domain("xn--bcher-kva.example".into())
        );
    }

    #[test]
    fn shapes_no_host_can_carry_are_refused() {
        for (raw, expected) in [
            ("", HostFault::Empty),
            ("   ", HostFault::Empty),
            (".", HostFault::Empty),
            ("..", HostFault::Empty),
            ("github.com..", HostFault::EmptyLabel),
            ("..github.com", HostFault::EmptyLabel),
            ("github..com", HostFault::EmptyLabel),
            ("-github.com", HostFault::HyphenEdge),
            ("github.com-", HostFault::HyphenEdge),
            ("api.*.com", HostFault::Character),
            ("github.com/repo", HostFault::Character),
            ("fe80::1%eth0", HostFault::Character),
            ("github.com\u{1b}[2J", HostFault::Character),
            ("[github.com]", HostFault::Brackets),
            ("[::1", HostFault::Brackets),
            ("[::::]", HostFault::Brackets),
        ] {
            assert_eq!(CanonicalHost::parse(raw), Err(expected), "`{raw}`");
        }
        let long_label = format!("{}.example", "a".repeat(64));
        assert_eq!(
            CanonicalHost::parse(&long_label),
            Err(HostFault::LabelTooLong)
        );
        let long_host = ["averyverylonglabelindeed"; 12].join(".");
        assert_eq!(CanonicalHost::parse(&long_host), Err(HostFault::TooLong));
    }

    #[test]
    fn an_authority_renders_with_ipv6_bracketed() {
        assert_eq!(canonical("GitHub.com.").to_string(), "github.com");
        assert_eq!(canonical("127.1").to_string(), "127.0.0.1");
        assert_eq!(canonical("::0001").to_string(), "[::1]");
    }
}
