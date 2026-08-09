//! One egress vocabulary, two matchers — held to one table.
//!
//! A sandbox profile stores its allow and deny lists as text
//! (`sandbox_profiles.network_allow` / `network_deny`). Two independent
//! matchers read that same text: [`friring::session::DomainRule`], which the
//! profile validator and the UI consult, and [`friring::proxy::HostRule`],
//! which the egress proxy enforces at connection time. They deliberately do
//! **not** share a type: `tests/architecture_rules.rs` makes `proxy` a leaf
//! with no crate-internal references, which is what lets it be tested without a
//! session, a database or a sandbox backend.
//!
//! Two implementations of one rule drift, and these two once did: the profile
//! side read `*.github.com` as `github.com`, while the proxy side read the same
//! spelling as "subdomains only, never the apex" — so a deny list written with
//! a wildcard would have let the apex straight through.
//!
//! **This file is the alignment mechanism.** It is the only place both types
//! are visible at once, so a change to either matcher that is not made to both
//! fails here. Add a spelling by adding a row; change a verdict by changing it
//! in both matchers.

use friring::proxy::HostRule;
use friring::session::DomainRule;

/// A candidate `host:port` and the verdict both matchers must reach on it.
type Probe = (&'static str, u16, bool);

/// The probes every spelling of `github.com` must answer identically. Kept in
/// one constant so `x`, `*.x` and `.x` are measured by literally one standard.
const GITHUB: &[Probe] = &[
    // Apex, subdomain, deep subdomain.
    ("github.com", 443, true),
    ("api.github.com", 443, true),
    ("a.b.api.github.com", 443, true),
    // Case is folded on both sides, and a root dot is presentation.
    ("GitHub.COM", 443, true),
    ("API.GitHub.Com", 8080, true),
    ("github.com.", 443, true),
    ("api.github.com.", 443, true),
    // An empty leading label: resolvers that accept it resolve the apex, so a
    // deny rule has to reach it.
    (".github.com", 443, true),
    // The three classic allowlist bypasses: sibling prefix, suffix injection,
    // truncation.
    ("evilgithub.com", 443, false),
    ("xgithub.com", 443, false),
    ("github.com.evil.net", 443, false),
    ("github.co", 443, false),
    // Neither a left-hand fragment nor a bare parent is covered.
    ("ithub.com", 443, false),
    ("com", 443, false),
    ("", 443, false),
];

/// `GITHUB` narrowed to one port. Every spelling scoped with `:443` must agree
/// on these.
const GITHUB_443: &[Probe] = &[
    ("github.com", 443, true),
    ("api.github.com", 443, true),
    ("GitHub.COM.", 443, true),
    ("github.com", 80, false),
    ("api.github.com", 8080, false),
    ("evilgithub.com", 443, false),
];

/// One stored spelling and what both matchers must make of it.
struct Case {
    /// The entry exactly as it sits in a profile's allow or deny list.
    rule: &'static str,
    /// What both matchers must render it as. A denial quotes the rule back —
    /// to the agent in the proxy's `403`, to the user in the TUI event — so the
    /// two must also *say* the same thing about a rule they agree on.
    display: &'static str,
    probes: &'static [Probe],
}

const CASES: &[Case] = &[
    // `*.x` and `.x` are spellings of `x`, apex included. This is the finding
    // the file exists for.
    Case {
        rule: "github.com",
        display: "github.com",
        probes: GITHUB,
    },
    Case {
        rule: "*.github.com",
        display: "github.com",
        probes: GITHUB,
    },
    Case {
        rule: ".github.com",
        display: "github.com",
        probes: GITHUB,
    },
    // Storage keeps what the user typed, so an upper-case or fully-qualified
    // spelling reaches both parsers unchanged.
    Case {
        rule: "GitHub.COM.",
        display: "github.com",
        probes: GITHUB,
    },
    Case {
        rule: "github.com:443",
        display: "github.com:443",
        probes: GITHUB_443,
    },
    Case {
        rule: "*.github.com:443",
        display: "github.com:443",
        probes: GITHUB_443,
    },
    // A rule that is itself a subdomain covers its own subtree and not its
    // parent.
    Case {
        rule: "API.GitHub.COM",
        display: "api.github.com",
        probes: &[
            ("api.github.com", 443, true),
            ("deep.api.github.com", 443, true),
            ("API.GITHUB.COM", 443, true),
            ("github.com", 443, false),
            ("xapi.github.com", 443, false),
        ],
    },
    // Punycode is the only way an international name is spelled on the wire,
    // and both parsers take the A-label as ordinary ASCII.
    Case {
        rule: "xn--bcher-kva.example",
        display: "xn--bcher-kva.example",
        probes: &[
            ("xn--bcher-kva.example", 443, true),
            ("www.xn--bcher-kva.example", 443, true),
            ("XN--BCHER-KVA.EXAMPLE", 443, true),
            ("evilxn--bcher-kva.example", 443, false),
        ],
    },
    // `_` is legal in a stored host: service names use it.
    Case {
        rule: "_acme-challenge.example.test",
        display: "_acme-challenge.example.test",
        probes: &[
            ("_acme-challenge.example.test", 443, true),
            ("example.test", 443, false),
        ],
    },
    // A single-label name is a name, not the address it resolves to.
    Case {
        rule: "localhost",
        display: "localhost",
        probes: &[
            ("localhost", 80, true),
            ("127.0.0.1", 80, false),
            ("notlocalhost", 80, false),
        ],
    },
    // An address rule is exact, and exact on the *parsed* address: no
    // pseudo-subdomain, and no alternative spelling that dodges a deny entry.
    Case {
        rule: "127.0.0.1",
        display: "127.0.0.1",
        probes: &[
            ("127.0.0.1", 80, true),
            ("127.0.0.1.", 80, true),
            ("evil.127.0.0.1", 80, false),
            ("10.127.0.0.1", 80, false),
            ("localhost", 80, false),
            ("127.000.000.001", 80, false),
        ],
    },
    Case {
        rule: "[::1]",
        display: "::1",
        probes: &[
            ("::1", 80, true),
            ("[::1]", 80, true),
            ("::0001", 80, true),
            ("0:0:0:0:0:0:0:1", 80, true),
            ("::2", 80, false),
        ],
    },
    Case {
        rule: "[::1]:8080",
        display: "[::1]:8080",
        probes: &[
            ("::1", 8080, true),
            ("[::1]", 8080, true),
            ("::1", 443, false),
        ],
    },
];

#[test]
fn both_matchers_agree_on_every_stored_spelling() {
    for case in CASES {
        let profile_side = DomainRule::parse(case.rule)
            .unwrap_or_else(|e| panic!("profile matcher rejected `{}`: {e}", case.rule));
        let proxy_side = case
            .rule
            .parse::<HostRule>()
            .unwrap_or_else(|e| panic!("proxy matcher rejected `{}`: {e}", case.rule));

        assert_eq!(
            profile_side.to_string(),
            case.display,
            "profile matcher renders `{}` differently",
            case.rule
        );
        assert_eq!(
            proxy_side.to_string(),
            case.display,
            "proxy matcher renders `{}` differently",
            case.rule
        );

        for &(host, port, expected) in case.probes {
            assert_eq!(
                profile_side.matches(host, port),
                expected,
                "profile matcher: rule `{}` vs {host}:{port}",
                case.rule
            );
            assert_eq!(
                proxy_side.matches(host, port),
                expected,
                "proxy matcher: rule `{}` vs {host}:{port}",
                case.rule
            );
        }
    }
}

/// Spellings the profile validator refuses, paired with whether the proxy's
/// parser refuses them too.
///
/// The verdict is recorded per side rather than asserted symmetric because two
/// of them are not, on purpose: `*` is the proxy's own "every host", which a
/// profile has no way to say (`network_mode = "full"` is how a user says it),
/// and an unbracketed IPv6 literal is read by the proxy as one address instead
/// of a host and a port. Both stay unreachable from a stored profile precisely
/// because the profile side refuses them — which is what the first assertion
/// below pins.
const NOT_STORABLE: &[(&str, bool)] = &[
    ("", true),
    ("   ", true),
    ("https://github.com", true),
    ("github.com/repo", true),
    ("git hub.com", true),
    ("user@github.com", true),
    ("github.com:notaport", true),
    ("github.com:0", true),
    ("github.com:99999", true),
    ("github..com", true),
    (".", true),
    ("-github.com", true),
    ("github.com-", true),
    ("api.*.com", true),
    (".*", true),
    // A U-label: store the punycode form, which is what a request host is.
    ("b\u{fc}cher.example", true),
    // A rule is echoed into denial text and into the TUI.
    ("github.com\u{1b}[2J", true),
    ("[::1", true),
    ("[::1]443", true),
    ("[::::]", true),
    ("*", false),
    ("::1:443", false),
];

#[test]
fn spellings_a_profile_cannot_store() {
    for &(entry, proxy_refuses_too) in NOT_STORABLE {
        assert!(
            DomainRule::parse(entry).is_err(),
            "profile matcher must refuse `{entry}`"
        );
        assert_eq!(
            entry.parse::<HostRule>().is_err(),
            proxy_refuses_too,
            "proxy matcher's verdict on `{entry}` changed"
        );
    }
}

/// The DNS length limits, which need runtime strings and so cannot sit in the
/// table above. A name no request host can be spelled as is a typo, and a typo
/// in a deny list is a rule that silently matches nothing.
#[test]
fn over_long_names_are_refused_by_both() {
    let long_label = format!("{}.example", "a".repeat(64));
    let long_host = ["averyverylonglabelindeed"; 12].join(".");
    for entry in [long_label, long_host] {
        let bytes = entry.len();
        assert!(
            DomainRule::parse(&entry).is_err(),
            "profile matcher must refuse a {bytes}-byte name"
        );
        assert!(
            entry.parse::<HostRule>().is_err(),
            "proxy matcher must refuse a {bytes}-byte name"
        );
    }
}
