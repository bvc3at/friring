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
//!
//! Each side canonicalises a host before comparing it — case, the root dot,
//! IPv6 brackets, and every spelling `getaddrinfo(3)` reads as an address
//! (`127.1`, `2130706433`, `::ffff:127.0.0.1`). Two canonicalisers are two more
//! things that can drift, so every spelling below is measured on both.

use friring::proxy::{Decision, DenyReason, HostRule, NetworkMode as ProxyNetworkMode, Policy};
use friring::session::{
    DomainRule, EgressDecision, NetworkMode, SandboxBackendKind, SandboxPath, SandboxPolicy,
    SandboxProfile,
};

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
    // Spellings no host can carry are not a way in: an unmatched rule is the
    // only honest answer, and the proxy refuses the request outright.
    ("github.com..", 443, false),
    ("git hub.com", 443, false),
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

/// Every spelling of the IPv4 loopback address, and the near misses.
///
/// `getaddrinfo(3)` reads all of the true rows as `127.0.0.1` and connects
/// there, so a matcher that compared text would hand a deny rule on the address
/// a host it thinks is a different one. The false rows are the other half of
/// that: an address rule must not grow a subtree, and folding must not turn a
/// *name* that merely contains the digits into the address.
const LOOPBACK: &[Probe] = &[
    ("127.0.0.1", 80, true),
    ("127.0.0.1.", 80, true),
    ("[127.0.0.1]", 80, true),
    // `inet_aton(3)`'s short forms: the last part fills the octets the leading
    // ones did not name.
    ("127.1", 80, true),
    ("127.0.1", 80, true),
    ("2130706433", 80, true),
    // Hex and octal parts, including the octal that `127.000.000.001` is.
    ("0x7f.1", 80, true),
    ("0x7f.0.0.1", 80, true),
    ("0x7f000001", 80, true),
    ("0177.0.0.1", 80, true),
    ("127.000.000.001", 80, true),
    // IPv4-mapped IPv6 reaches the same endpoint, in either spelling.
    ("::ffff:127.0.0.1", 80, true),
    ("[::ffff:127.0.0.1]", 80, true),
    ("::ffff:7f00:1", 80, true),
    // A different address is a different host, however similar it reads.
    ("127.0.0.2", 80, false),
    ("2130706434", 80, false),
    ("127.0.0.256", 80, false),
    // The deprecated IPv4-*compatible* form is a distinct IPv6 destination no
    // mainstream stack translates — and `::1` sits in that range, so folding it
    // would read loopback as `0.0.0.1`.
    ("::127.0.0.1", 80, false),
    ("::1", 80, false),
    // A name is a name: `localhost` resolves here, and is still not this rule.
    ("localhost", 80, false),
    // Normalising the digits must not widen the rule into a domain suffix.
    ("127.0.0.1.evil.net", 80, false),
    ("127.1.evil.net", 80, false),
    ("2130706433.evil.net", 80, false),
    ("evil.127.0.0.1", 80, false),
    ("10.127.0.0.1", 80, false),
    // A leading zero is octal, so this is 8.0.0.1 and not 10.0.0.1 either.
    ("010.0.0.1", 80, false),
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
            ("xn--bcher-kva.example.evil.net", 443, false),
            // One encoded character apart is a different name, not a near miss.
            ("xn--bcher-kvb.example", 443, false),
            // The U-label the A-label encodes is *not* quietly matched: neither
            // matcher canonicalises Unicode, so it is refused rather than
            // guessed at, and the proxy denies the request outright.
            ("b\u{fc}cher.example", 443, false),
        ],
    },
    // The ASCII name the punycode prefix reads like is a different name.
    Case {
        rule: "bcher.example",
        display: "bcher.example",
        probes: &[
            ("bcher.example", 443, true),
            ("xn--bcher-kva.example", 443, false),
            ("kva.example", 443, false),
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
    // An address rule is exact, and exact on the address rather than its text:
    // every spelling that reaches the endpoint is the same rule, and no
    // pseudo-subdomain sits under it.
    Case {
        rule: "127.0.0.1",
        display: "127.0.0.1",
        probes: LOOPBACK,
    },
    // …and a rule written in any of those spellings is that same rule, so a
    // deny list written `127.1` denies `127.0.0.1` and vice versa.
    Case {
        rule: "127.1",
        display: "127.0.0.1",
        probes: LOOPBACK,
    },
    Case {
        rule: "2130706433",
        display: "127.0.0.1",
        probes: LOOPBACK,
    },
    Case {
        rule: "0x7f.1",
        display: "127.0.0.1",
        probes: LOOPBACK,
    },
    Case {
        rule: "127.000.000.001",
        display: "127.0.0.1",
        probes: LOOPBACK,
    },
    Case {
        rule: "[::ffff:127.0.0.1]",
        display: "127.0.0.1",
        probes: LOOPBACK,
    },
    Case {
        rule: "[::1]",
        display: "[::1]",
        probes: &[
            ("::1", 80, true),
            ("[::1]", 80, true),
            ("::0001", 80, true),
            ("0:0:0:0:0:0:0:1", 80, true),
            ("::2", 80, false),
            ("127.0.0.1", 80, false),
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
    Case {
        rule: "127.1:8080",
        display: "127.0.0.1:8080",
        probes: &[
            ("127.0.0.1", 8080, true),
            ("2130706433", 8080, true),
            ("127.0.0.1", 443, false),
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

/// What both sides render is what both sides parse. A rule that printed one way
/// and read back another would drift the moment a denial quoted it into a
/// profile.
#[test]
fn every_rendering_parses_back_to_the_same_rule() {
    for case in CASES {
        let profile_side = DomainRule::parse(case.rule).expect("a storable rule");
        assert_eq!(
            DomainRule::parse(&profile_side.to_string()).expect("re-parses"),
            profile_side,
            "profile matcher: `{}` does not round-trip",
            case.rule
        );
        let proxy_side = case.rule.parse::<HostRule>().expect("a loadable rule");
        assert_eq!(
            case.display.parse::<HostRule>().expect("re-parses"),
            proxy_side,
            "proxy matcher: `{}` does not round-trip",
            case.rule
        );
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
    // Canonicalising Unicode correctly needs UTS-46 tables neither side
    // carries, and guessing would encode two spellings of one name to two
    // different A-labels.
    ("b\u{fc}cher.example", true),
    ("caf\u{e9}.example", true),
    // A rule is echoed into denial text and into the TUI.
    ("github.com\u{1b}[2J", true),
    ("[::1", true),
    ("[::1]443", true),
    ("[::::]", true),
    // Exactly one root dot is presentation; a second is an empty label, which
    // no name has and no resolver answers for.
    ("github.com..", true),
    ("..github.com", true),
    // Brackets are address syntax. Reading them as decoration would give one
    // rule a second spelling.
    ("[github.com]", true),
    // A zone identifier scopes an address to one interface, which is not
    // something a destination rule can mean.
    ("fe80::1%eth0", true),
    ("[fe80::1%eth0]", true),
    ("127.0.0.1:80:90", true),
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

/// A profile with one deny rule and everything else allowed — the shape that
/// makes a deny enforceable, and therefore the shape a spelling would be used
/// to dodge.
fn full_access_denying(entry: &str) -> (Policy, SandboxPolicy) {
    let proxy = Policy::new(ProxyNetworkMode::Full)
        .with_deny([entry])
        .expect("a loadable deny rule");

    let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
    profile.network_mode = NetworkMode::Full;
    profile.network_deny = vec![entry.to_string()];
    let sandbox = profile
        .resolve(SandboxBackendKind::Seatbelt, "/home/u")
        .expect("a valid profile");
    (proxy, sandbox)
}

/// Under `full` a deny rule is the only thing standing between the agent and a
/// host, so a spelling that slips past one is the whole gap. Both enforcement
/// paths — the proxy that decides at connection time and the policy the UI and
/// CLI preview — have to refuse every spelling of the denied address.
#[test]
fn a_denied_address_cannot_be_reached_by_spelling_it_differently() {
    let (proxy, sandbox) = full_access_denying("127.0.0.1");
    for spelling in [
        "127.0.0.1",
        "127.1",
        "2130706433",
        "0x7f.0.0.1",
        "0177.0.0.1",
        "127.000.000.001",
        "::ffff:127.0.0.1",
        "[::ffff:127.0.0.1]",
        "127.0.0.1.",
    ] {
        assert_eq!(
            proxy.decide(spelling, 80),
            Decision::Deny(DenyReason::DeniedByRule("127.0.0.1".into())),
            "proxy let `{spelling}` past a deny on 127.0.0.1"
        );
        assert_eq!(
            sandbox.decide_egress(spelling, 80),
            EgressDecision::Denied,
            "profile let `{spelling}` past a deny on 127.0.0.1"
        );
    }
    // The other direction: the deny must not have grown into a suffix rule over
    // the digits, or a name a CDN could register would be unreachable.
    for allowed in ["127.0.0.1.evil.net", "evil.127.0.0.1", "127.0.0.2"] {
        assert!(
            proxy.decide(allowed, 80).is_allowed(),
            "proxy read `{allowed}` as the denied address"
        );
        assert!(
            sandbox.decide_egress(allowed, 80).is_allowed(),
            "profile read `{allowed}` as the denied address"
        );
    }
}

/// A host neither side can canonicalise is refused **before** any rule is
/// consulted, in every mode — including `full`, where nothing else would stop
/// it. Guessing at what a U-label denotes would compare a rule against one name
/// and dial another; refusing is the only answer that cannot be wrong.
///
/// The proxy says so with its own reason rather than
/// [`DenyReason::NotAllowlisted`], which would raise the first-use prompt and
/// offer to store a rule the profile validator then refuses.
#[test]
fn a_host_that_does_not_canonicalise_is_refused_in_every_mode() {
    let (proxy, sandbox) = full_access_denying("xn--bcher-kva.example");
    let over_long_label = format!("{}.xn--bcher-kva.example", "a".repeat(64));
    for spelling in [
        "b\u{fc}cher.example",
        "www.b\u{fc}cher.example",
        "xn--bcher-kva.example..",
        "[xn--bcher-kva.example]",
        over_long_label.as_str(),
    ] {
        let decision = proxy.decide(spelling, 443);
        assert!(
            matches!(decision, Decision::Deny(DenyReason::UnsupportedHost(_))),
            "proxy did not refuse `{spelling}`: {decision:?}"
        );
        assert_eq!(
            sandbox.decide_egress(spelling, 443),
            EgressDecision::Denied,
            "profile did not refuse `{spelling}`"
        );
        // Not a question to put to the user: the answer would be a rule that
        // cannot be stored.
        assert!(!sandbox.should_prompt(sandbox.decide_egress(spelling, 443)));
    }
    // The punycode form of the same name is ordinary ASCII, and the deny rule
    // catches it exactly as written.
    assert_eq!(
        proxy.decide("www.xn--bcher-kva.example", 443),
        Decision::Deny(DenyReason::DeniedByRule("xn--bcher-kva.example".into()))
    );
    assert_eq!(
        sandbox.decide_egress("www.xn--bcher-kva.example", 443),
        EgressDecision::Denied
    );
    // And an unrelated host is still reachable under `full`, so the refusal is
    // the spelling and not the mode.
    assert!(proxy.decide("example.test", 443).is_allowed());
    assert!(sandbox.decide_egress("example.test", 443).is_allowed());
}

/// The same host, under an allowlist. This direction always failed closed —
/// an unmatched host is denied — but it must fail closed for the *stated*
/// reason, or the TUI prompts to allow a domain that can never be stored.
#[test]
fn an_uncanonicalisable_host_never_becomes_a_first_use_prompt() {
    let policy = Policy::new(ProxyNetworkMode::Allowlist)
        .with_allow(["xn--bcher-kva.example"])
        .expect("a loadable allow rule");
    assert!(policy.decide("xn--bcher-kva.example", 443).is_allowed());
    assert!(matches!(
        policy.decide("b\u{fc}cher.example", 443),
        Decision::Deny(DenyReason::UnsupportedHost(_))
    ));
    // An ordinary unlisted host is still the prompt-worthy reason.
    assert_eq!(
        policy.decide("example.test", 443),
        Decision::Deny(DenyReason::NotAllowlisted)
    );

    let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
    profile.network_mode = NetworkMode::Allowlist;
    profile.network_allow = vec!["xn--bcher-kva.example".to_string()];
    let sandbox = profile
        .resolve(SandboxBackendKind::Seatbelt, "/home/u")
        .expect("a valid profile");
    assert_eq!(
        sandbox.decide_egress("b\u{fc}cher.example", 443),
        EgressDecision::Denied
    );
    assert_eq!(
        sandbox.decide_egress("example.test", 443),
        EgressDecision::Unlisted
    );
    assert!(sandbox.should_prompt(EgressDecision::Unlisted));
}
