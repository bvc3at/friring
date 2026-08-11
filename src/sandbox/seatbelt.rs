//! macOS `sandbox-exec`: a generated SBPL profile wrapped around the agent.
//!
//! [`render_profile`] is a pure function from a launch to profile text, so the
//! whole security surface is assertable in a unit test and nothing has to run a
//! sandbox to check it. [`SeatbeltBackend`] adds only the two side effects:
//! writing that text to a per-session file, and prefixing the argv.
//!
//! Three properties of SBPL shape everything here:
//!
//! - **The last matching rule wins.** That is what orders the whole profile.
//!   The profile's own paths are emitted in one ancestor-before-descendant pass
//!   so the *most specific* rule is the last one to match — `repo` read-only
//!   with `repo/work` read-write means what the user wrote, and means the same
//!   thing bwrap's mount order means. The denies that must hold whatever a path
//!   rule said — the secrets list, friring's database — come after all of them,
//!   and nothing is emitted afterwards that could re-open one.
//! - **There is no host predicate.** Network filters accept `*` or `localhost`
//!   with a port, and nothing else, so a domain allowlist can only mean "deny
//!   all outbound except the loopback proxy" (ADR-27).
//! - **Nesting fails hard.** Under any profile containing a deny rule — which
//!   every profile here does, starting with `(deny default)` — an inner
//!   `sandbox_apply` returns `Operation not permitted`, so an agent's own
//!   seatbelt sandbox must be off. See [`InnerSandboxVerdict::Denied`].

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::sandbox::backend::{
    Argv, Availability, Caps, Egress, InnerSandboxVerdict, ProxyEndpoint, ProxyTransport,
    SandboxBackend, SandboxError, SandboxLaunch, SandboxResult, PROTECTED_IN_WRITABLE_ROOT,
};
use crate::sandbox::dirs::{self, sanitize_component, write_private};
use crate::sandbox::probe::{detect_platform, HostPlatform, LocalProbeHost, ProbeHost};
use crate::sandbox::secrets::{secrets_for, SecretPlatform};
use crate::session::{NetworkMode, ReadScope, SandboxBackendKind, SandboxShape};

/// Absolute path of the system binary. Not looked up on `PATH`: the whole point
/// of the wrapper is that the user's environment cannot choose what applies the
/// policy. [`crate::sandbox::bwrap::BWRAP`] keeps the same promise the only way
/// it can, by resolving and vetting one path at probe time.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The session's launch directory.
pub const PARAM_WORKSPACE: &str = "WORKSPACE";
/// The per-session status-file directory (ADR-29).
pub const PARAM_SIGNAL_DIR: &str = "SIGNAL_DIR";
/// The writable temp directory.
pub const PARAM_TMP_DIR: &str = "TMP_DIR";
/// The egress proxy: `localhost:<port>`, or a unix socket path.
pub const PARAM_PROXY: &str = "PROXY";
/// [`PROTECTED_IN_WRITABLE_ROOT`] under the session's workspace. Its own
/// parameter because SBPL filters take a plain string and cannot join one to a
/// suffix, and baking the joined path into the text would put a per-session
/// value back into a profile that is otherwise identical for every session.
pub const PARAM_WORKSPACE_PROTECTED: &str = "WORKSPACE_PROTECTED";

/// Directories a binary must read before it can run at all, for the
/// [`ReadScope::Workspace`] profiles that do not open the whole filesystem.
///
/// `/private/var/db` carries the dyld shared cache and the timezone database;
/// `/private/var/select` is where `/usr/bin/awk` and friends point. Without the
/// set, `dyld` fails before `main` and the pane dies with an unhelpful message.
const SYSTEM_READ_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/opt",
    "/private/etc",
    "/private/var/db",
    "/private/var/select",
    "/dev",
];

/// Device nodes and terminals every interactive process needs.
///
/// The agent is a TUI on the tmux pane's pseudo-terminal: without read, write
/// and `ioctl` on `/dev/ttys*` it renders nothing and receives no keystrokes,
/// which looks exactly like a hung sandbox.
const DEVICE_FILTERS: &[&str] = &[
    "(literal \"/dev/null\")",
    "(literal \"/dev/zero\")",
    "(literal \"/dev/random\")",
    "(literal \"/dev/urandom\")",
    "(literal \"/dev/tty\")",
    "(literal \"/dev/ptmx\")",
    "(literal \"/dev/dtracehelper\")",
    "(regex #\"^/dev/ttys[0-9]+$\")",
    "(regex #\"^/dev/fd/[0-9]+$\")",
];

/// Mach services a sandboxed process may look up.
///
/// The first five are the reason host-passthrough credentials work at all:
/// macOS keeps Claude Code's OAuth in the login Keychain, and a process reaches
/// a keychain item by asking `securityd` over Mach — never by reading a file.
/// Allow these and a sandboxed agent keeps the credentials it already has, with
/// no copying and nothing to expire (ADR-28); deny them and the agent is logged
/// out inside the boundary while the *file* it would fall back to does not
/// exist. The keychain database itself stays denied by the secrets list, so the
/// agent can use items through `securityd`'s per-item prompts but cannot copy
/// the store.
///
/// The last two are not about credentials: without `opendirectoryd.libinfo` a
/// `getpwuid` fails and most CLIs cannot resolve `$HOME` or the user name, and
/// `notification_center` is `notify(3)`, which libsystem itself calls.
const MACH_SERVICES: &[(&str, &str)] = &[
    ("com.apple.SecurityServer", "keychain item access"),
    ("com.apple.securityd", "the modern name of the same daemon"),
    ("com.apple.trustd", "certificate trust evaluation for TLS"),
    ("com.apple.ocspd", "certificate revocation checks"),
    (
        "com.apple.cfprefsd.daemon",
        "CFPreferences, which the Security framework reads on every call",
    ),
    (
        "com.apple.system.opendirectoryd.libinfo",
        "getpwuid: the user name and home directory",
    ),
    (
        "com.apple.system.notification_center",
        "notify(3), called from inside libsystem",
    ),
];

/// Mach services needed to resolve a host name.
///
/// Only emitted for [`NetworkMode::Full`]. A proxied sandbox deliberately does
/// **not** get them: the proxy resolves on the sandbox's behalf, so denying
/// `mDNSResponder` also closes DNS as an exfiltration channel.
const DNS_SERVICES: &[&str] = &["com.apple.dnssd.service", "com.apple.mDNSResponder"];

/// One path a rule applies to, in both the spelling the profile uses and the
/// value it will have.
///
/// Per-session paths ride in as `sandbox-exec -D` parameters so the generated
/// text is identical for every session of a profile; the value is kept
/// alongside so nesting can still be decided at render time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PathSlot {
    /// The argument to `subpath`: a quoted literal, or `(param "…")`.
    expr: String,
    value: String,
    /// Whether [`expr`](Self::expr) is the path itself rather than a parameter
    /// standing in for it. Only a literal can have a suffix appended to it in
    /// the profile text.
    is_literal: bool,
}

impl PathSlot {
    fn literal(value: &str) -> Self {
        Self {
            expr: format!("{value:?}"),
            value: value.to_string(),
            is_literal: true,
        }
    }

    fn param(name: &str, value: &str) -> Self {
        Self {
            expr: format!("(param {name:?})"),
            value: value.to_string(),
            is_literal: false,
        }
    }
}

/// The `-D NAME=value` pairs a launch supplies, in a stable order.
///
/// Only the per-session facts are parameters. The profile's own paths are
/// per-profile, not per-session, so they are written into the text where they
/// are easier to read and to audit.
pub fn params(launch: &SandboxLaunch<'_>) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for (name, value) in [
        (PARAM_WORKSPACE, launch.workspace),
        (PARAM_SIGNAL_DIR, launch.signal_dir),
        (PARAM_TMP_DIR, launch.tmp_dir),
    ] {
        if let Some(value) = value {
            out.push((name, value.to_string()));
        }
    }
    if let Some(workspace) = launch.workspace {
        out.push((
            PARAM_WORKSPACE_PROTECTED,
            format!("{workspace}/{PROTECTED_IN_WRITABLE_ROOT}"),
        ));
    }
    if let Some(proxy) = proxy_value(launch) {
        out.push((PARAM_PROXY, proxy));
    }
    out
}

/// The proxy parameter's value, or `None` when this launch has no hole to
/// punch — the mode grants nothing, or everything, or no proxy is running.
///
/// A seatbelt sandbox shares the host's filesystem, so the socket spelling is
/// the socket's *host* path: there is no boundary for it to be carried across.
fn proxy_value(launch: &SandboxLaunch<'_>) -> Option<String> {
    match launch.egress() {
        Egress::Proxied(ProxyEndpoint::Loopback { port }) => Some(format!("localhost:{port}")),
        Egress::Proxied(ProxyEndpoint::UnixSocket { host_path, .. }) => Some(host_path.clone()),
        Egress::Open | Egress::Closed => None,
    }
}

/// Generate the SBPL profile for one launch.
///
/// Ordered: default deny, then the allows a process needs to exist, then the
/// read scope, then the profile's paths, and finally every deny that must not
/// be re-opened. Reading the output top to bottom is reading the policy in the
/// order the kernel evaluates it.
pub fn render_profile(launch: &SandboxLaunch<'_>) -> String {
    let policy = launch.policy;
    let (readable, writable) = path_slots(launch);
    let mut out: Vec<String> = Vec::new();

    out.push("(version 1)".to_string());
    out.push(format!(
        ";; friring sandbox — profile {:?}, backend seatbelt.",
        policy.profile
    ));
    out.push(";; Generated per launch. Edit the profile in friring, never this file.".to_string());
    out.push(String::new());
    out.push("(deny default)".to_string());

    section(
        &mut out,
        "process",
        &[
            "Nothing runs without these: the agent forks tools, execs them, signals",
            "itself, and reads its own process info and the machine's sysctls.",
        ],
    );
    out.push("(allow process-fork)".to_string());
    out.push("(allow process-exec)".to_string());
    out.push("(allow process-info* (target self))".to_string());
    out.push("(allow signal (target self))".to_string());
    out.push("(allow sysctl-read)".to_string());
    out.push(";; Resolving any allowed path needs metadata on each of its".to_string());
    out.push(";; ancestors, so a global stat is the price of subpath rules working".to_string());
    out.push(";; at all. It exposes existence and timestamps, never contents.".to_string());
    out.push("(allow file-read-metadata)".to_string());

    section(
        &mut out,
        "terminal",
        &["The pane's pseudo-terminal and the device nodes every process opens."],
    );
    out.push(format!(
        "(allow file-read* file-write* file-ioctl\n    {})",
        DEVICE_FILTERS.join("\n    ")
    ));

    section(
        &mut out,
        "mach services",
        &[
            "The Keychain is reachable from inside a sandbox, which is what makes",
            "host-passthrough credentials work with no copying (ADR-28).",
        ],
    );
    let services: Vec<String> = MACH_SERVICES
        .iter()
        .map(|(name, why)| format!("(global-name {name:?})   ;; {why}"))
        .collect();
    // The closing paren gets its own line: each filter carries a trailing `;;`
    // comment, which runs to the end of the line and would comment it out.
    out.push(format!(
        "(allow mach-lookup\n    {}\n)",
        services.join("\n    ")
    ));

    render_reads(&mut out, launch, &readable);
    render_network(&mut out, launch);
    render_path_rules(&mut out, &readable, &writable);
    render_protected_subdirectories(&mut out, launch, &writable);
    render_rename_boundaries(&mut out, launch, &writable);
    render_secrets(&mut out, launch);
    render_other_sandboxes(&mut out);
    render_database(&mut out, launch);

    out.push(String::new());
    out.join("\n")
}

/// Split the launch's paths into the two rule sets, with the per-session ones
/// as parameters and everything sorted so an ancestor precedes its descendants.
fn path_slots(launch: &SandboxLaunch<'_>) -> (Vec<PathSlot>, Vec<PathSlot>) {
    let mut writable: Vec<PathSlot> = launch
        .policy
        .rw_paths
        .iter()
        .map(|p| PathSlot::literal(p))
        .collect();
    for (name, value) in [
        (PARAM_WORKSPACE, launch.workspace),
        (PARAM_SIGNAL_DIR, launch.signal_dir),
        (PARAM_TMP_DIR, launch.tmp_dir),
    ] {
        // A session path that the profile already lists needs no second rule;
        // the parameter is still passed, and an unreferenced one is harmless.
        if let Some(value) = value.filter(|v| writable.iter().all(|s| s.value != *v)) {
            writable.push(PathSlot::param(name, value));
        }
    }
    writable.sort_by(|a, b| a.value.cmp(&b.value));

    let mut readable: Vec<PathSlot> = launch
        .policy
        .ro_paths
        .iter()
        .filter(|p| writable.iter().all(|s| s.value != **p))
        .map(|p| PathSlot::literal(p))
        .collect();
    readable.sort_by(|a, b| a.value.cmp(&b.value));
    (readable, writable)
}

fn render_reads(out: &mut Vec<String>, launch: &SandboxLaunch<'_>, readable: &[PathSlot]) {
    match launch.policy.read_scope {
        ReadScope::HostMinusSecrets => {
            section(
                out,
                "reads: host-minus-secrets",
                &[
                    "The host filesystem is readable, and the secrets list at the end",
                    "of this profile takes back the credentials that are not this",
                    "agent's own. An agent that cannot read its own configuration",
                    "directory fails on first launch, and copying that directory in is",
                    "exactly what ADR-28 forbids.",
                ],
            );
            out.push("(allow file-read*)".to_string());
        }
        ReadScope::Workspace => {
            section(
                out,
                "reads: workspace",
                &[
                    "Only the profile's own paths, plus the system directories a",
                    "binary needs in order to load and run.",
                ],
            );
            let filters: Vec<String> = SYSTEM_READ_PATHS
                .iter()
                .map(|p| format!("(subpath {p:?})"))
                .chain(std::iter::once("(literal \"/\")".to_string()))
                .collect();
            out.push(format!(
                "(allow file-read*\n    {})",
                filters.join("\n    ")
            ));
            for slot in readable {
                out.push(format!("(allow file-read* (subpath {}))", slot.expr));
            }
        }
    }
}

/// The profile's own paths, as one ancestor-before-descendant pass.
///
/// **Most specific wins.** A lexicographic sort puts a path before everything
/// nested inside it, and the last matching SBPL rule is the one that applies, so
/// emitting the whole set in that order makes the innermost rule decide:
/// `repo` read-only with `repo/work` read-write leaves `repo/work` writable, and
/// `repo` read-write with `repo/vendor` read-only leaves `vendor` read-only.
/// That is what the user wrote, and — decisively — it is also what bwrap's mount
/// order does with the same profile, so the two backends cannot disagree about
/// one path (`docs/SANDBOX.md` §The two sandbox shapes).
///
/// Read rules are not here: `render_reads` grants them per scope, and nothing in
/// this section takes a read back. Only writes are contested.
fn render_path_rules(out: &mut Vec<String>, readable: &[PathSlot], writable: &[PathSlot]) {
    section(
        out,
        "paths",
        &[
            "Ancestor before descendant, so the most specific rule is the last",
            "one to match — which is the one SBPL applies. A read-only path",
            "nested in a writable one loses the ancestor's write grant, and a",
            "writable path nested in a read-only one keeps its own.",
        ],
    );
    let mut merged: Vec<(&PathSlot, bool)> = writable
        .iter()
        .map(|slot| (slot, true))
        .chain(readable.iter().map(|slot| (slot, false)))
        .collect();
    merged.sort_by(|a, b| a.0.value.cmp(&b.0.value));
    if merged.is_empty() {
        out.push(";; This profile lists no paths at all.".to_string());
        return;
    }
    for (slot, is_writable) in merged {
        if is_writable {
            out.push(format!(
                "(allow file-read* file-write* (subpath {}))",
                slot.expr
            ));
        } else {
            out.push(format!("(deny file-write* (subpath {}))", slot.expr));
        }
    }
}

fn render_network(out: &mut Vec<String>, launch: &SandboxLaunch<'_>) {
    let mode = launch.policy.network;
    match launch.egress() {
        Egress::Open => {
            section(
                out,
                "network: full",
                &[
                    "Unrestricted egress, which `full` means only while it carries no",
                    "denies: SBPL has no host predicate, so a deny list is enforceable",
                    "only by routing everything through the proxy instead — which is",
                    "what a `full` profile with denies renders as (see below).",
                ],
            );
            out.push("(allow network*)".to_string());
            out.push("(allow system-socket)".to_string());
            let services: Vec<String> = DNS_SERVICES
                .iter()
                .map(|s| format!("(global-name {s:?})"))
                .collect();
            out.push(format!(
                "(allow mach-lookup\n    {})   ;; host-name resolution",
                services.join("\n    ")
            ));
        }
        Egress::Proxied(ProxyEndpoint::Loopback { .. }) => {
            section(
                out,
                &format!("network: {mode} (proxied)"),
                &[
                    "One hole, to the friring proxy on loopback, which enforces the",
                    "domain rules outside the boundary (ADR-27). A process that",
                    "ignores the proxy environment gets no network rather than an",
                    "escape route. DNS is deliberately absent: the proxy resolves,",
                    "so name lookups cannot become an exfiltration channel.",
                    "`localhost` here covers ::1 as well as 127.0.0.1 and SBPL has",
                    "no way to say which, so the proxy holds the port on both.",
                ],
            );
            out.push(format!(
                "(allow network-outbound (remote ip (param {PARAM_PROXY:?})))"
            ));
        }
        Egress::Proxied(ProxyEndpoint::UnixSocket { .. }) => {
            section(
                out,
                &format!("network: {mode} (proxied)"),
                &[
                    "One hole, to the friring proxy's unix socket (ADR-27). A seatbelt",
                    "sandbox shares the host's filesystem, so it connects to the socket",
                    "directly and needs no relay.",
                ],
            );
            out.push(format!(
                "(allow network-outbound (literal (param {PARAM_PROXY:?})))"
            ));
        }
        Egress::Closed if mode == NetworkMode::None => {
            section(
                out,
                "network: none",
                &[
                    "No rule at all: `(deny default)` already refuses every socket.",
                    "That includes unix-domain sockets, which SBPL also models as",
                    "network operations — so an ssh-agent handoff does not survive a",
                    "`none` profile, by design.",
                ],
            );
        }
        Egress::Closed => {
            section(
                out,
                &format!("network: {mode} (no proxy running)"),
                &[
                    "Configured identically to `none`: this mode is enforced by the",
                    "friring proxy, and without one there is nothing to open a hole to.",
                    "`SandboxLaunch::validate` refuses such a launch, so this is what a",
                    "profile generated outside that path renders as — closed.",
                ],
            );
        }
    }
}

/// `.git/hooks` inside every writable root, after the path rules so no grant —
/// however specific — re-opens it.
fn render_protected_subdirectories(
    out: &mut Vec<String>,
    launch: &SandboxLaunch<'_>,
    writable: &[PathSlot],
) {
    section(
        out,
        "protected inside every writable root",
        &[
            "Hook scripts are run by whichever git touches the repository next,",
            "including the host's, outside the boundary. A no-op where the root",
            "is not a repository.",
        ],
    );
    // Only the profile's own roots and the session's workspace: the signal and
    // scratch directories friring mints are never repositories.
    for slot in writable.iter().filter(|s| s.is_literal) {
        out.push(format!(
            "(deny file-write* (subpath {:?}))",
            format!("{}/{PROTECTED_IN_WRITABLE_ROOT}", slot.value)
        ));
    }
    if launch.workspace.is_some() {
        out.push(format!(
            "(deny file-write* (subpath (param {PARAM_WORKSPACE_PROTECTED:?})))"
        ));
    }
}

/// Pin every pathname the denies below are written in.
///
/// SBPL matches by pathname, and a deny that names a path is only as good as the
/// path staying where it is. With a read-write rule covering `~`, a sandbox can
/// rename `~/.local/share` — an ordinary write, authorised by the subtree grant
/// — and reach the database through a name no `(literal …)` deny matches. So the
/// unlink of each writable anchor *and* of every directory on the way to a
/// protected path is denied, which is what keeps those names from moving.
///
/// bwrap needs no counterpart: a directory containing a mount point cannot be
/// renamed, so its masks pin themselves.
///
/// The ancestors are taken from the protected paths rather than from the
/// writable roots, which is a superset of what is strictly needed. It is also
/// stable: deriving them from the roots would write the session's own workspace
/// into a profile text that is otherwise identical for every session.
fn render_rename_boundaries(
    out: &mut Vec<String>,
    launch: &SandboxLaunch<'_>,
    writable: &[PathSlot],
) {
    section(
        out,
        "rename boundaries",
        &[
            "SBPL denies by pathname, so a protected path must not be reachable",
            "under a second name. Neither a writable anchor nor any directory",
            "leading to a secret or to friring's database can be unlinked, and a",
            "rename is an unlink of its source.",
        ],
    );
    for slot in writable {
        out.push(format!("(deny file-write-unlink (literal {}))", slot.expr));
    }
    for path in protected_ancestors(launch) {
        if writable.iter().any(|slot| slot.value == path) {
            continue;
        }
        out.push(format!("(deny file-write-unlink (literal {path:?}))"));
    }
}

/// Every directory a rename could move in order to bring a protected path out
/// from under the denies that name it. Sorted and de-duplicated.
fn protected_ancestors(launch: &SandboxLaunch<'_>) -> Vec<String> {
    let mut targets: Vec<String> = secrets_for(SecretPlatform::MacOs, launch.agent)
        .iter()
        .map(|secret| secret.resolved(launch.home))
        .collect();
    if let Some(db) = launch.friring_db {
        targets.push(db.to_string());
    }
    let mut out: Vec<String> = Vec::new();
    for target in targets {
        for ancestor in dirs::ancestors_of(&target) {
            if !out.contains(&ancestor) {
                out.push(ancestor);
            }
        }
    }
    out.sort();
    out
}

fn render_secrets(out: &mut Vec<String>, launch: &SandboxLaunch<'_>) {
    let secrets = secrets_for(SecretPlatform::MacOs, launch.agent);
    if secrets.is_empty() {
        return;
    }
    section(
        out,
        "secrets",
        &[
            "Credentials that are not this agent's own. Last, so nothing above can",
            "re-open them.",
        ],
    );
    for secret in secrets {
        let path = secret.resolved(launch.home);
        out.push(format!(";; {}", secret.why));
        out.push(format!("(deny file-read* file-write* (subpath {path:?}))"));
    }
}

/// Deny the state friring keeps for its **other** sandboxes.
///
/// `host-minus-secrets` makes the host filesystem readable and takes back a
/// named list — which covered the host's own credential files and not the ones
/// a *sandbox* holds. Those live beside the database, and every one of them is
/// taken by being read: another profile's synthetic home holds the credential
/// its `volume-login` signed in with and the copy a `seed-file` made (ADR-28),
/// the seed markers are what keep one credential to one boundary, and a
/// generated `.sb` file is the policy constraining some other session.
///
/// Deliberately **not** the whole of `<data>/sandbox`: this launch's own scratch
/// directory lives under `<data>/sandbox/tmp`, and denying that would take away
/// the temp directory the agent needs to start. `<data>/sandbox/pl` is a place's
/// tree, which no policy launch is ever given — a place is a container, and its
/// paths are mounts rather than SBPL rules.
///
/// Denied last with the database, so no path grant can re-open it. A profile
/// that *names* one of these is refused before it gets here
/// ([`crate::sandbox::dirs::check_declared_paths`]); this is the scope, which
/// names nothing and grants everything.
fn render_other_sandboxes(out: &mut Vec<String>) {
    let denied: Vec<String> = [dirs::place_root(), dirs::profile_dir(), dirs::seeds_root()]
        .into_iter()
        .flatten()
        .map(|dir| format!("(subpath {:?})", dir.display().to_string()))
        .collect();
    if denied.is_empty() {
        return;
    }
    section(
        out,
        "friring's other sandboxes",
        &[
            "The logins the other profiles' places hold, the markers that keep one",
            "credential to one boundary (ADR-28), and the generated policies that",
            "constrain other sessions. Reading any of them is taking it, so the",
            "host read scope takes them back here rather than trusting the list of",
            "host credential paths to cover them.",
        ],
    );
    out.push(format!(
        "(deny file-read* file-write*\n    {})",
        denied.join("\n    ")
    ));
}

fn render_database(out: &mut Vec<String>, launch: &SandboxLaunch<'_>) {
    let Some(db) = launch.friring_db else {
        return;
    };
    section(
        out,
        "friring's database",
        &[
            "ADR-29: the database never enters a sandbox. Automations stored in it",
            "carry shell commands the *host* executes, so write access from inside",
            "is arbitrary host command execution. Denied last, so a read-write path",
            "covering the data directory cannot re-open it.",
        ],
    );
    let files: Vec<String> = [db.to_string(), format!("{db}-wal"), format!("{db}-shm")]
        .iter()
        .map(|f| format!("(literal {f:?})"))
        .collect();
    out.push(format!(
        "(deny file-read* file-write*\n    {})",
        files.join("\n    ")
    ));
}

/// A blank line, a banner, and the section's rationale as comments.
fn section(out: &mut Vec<String>, title: &str, why: &[&str]) {
    out.push(String::new());
    out.push(format!(";; --- {title} ---"));
    for line in why {
        out.push(format!(";; {line}"));
    }
}

/// `sandbox-exec` with a generated profile.
pub struct SeatbeltBackend {
    host: Arc<dyn ProbeHost>,
    /// `None` when friring cannot resolve a data directory, which is the only
    /// place a generated profile may live — see [`default_profile_dir`]. A
    /// launch then fails rather than falling back somewhere reachable.
    profile_dir: Option<PathBuf>,
    availability: OnceLock<Availability>,
}

impl SeatbeltBackend {
    /// A backend probing `host` and writing its generated profiles under
    /// `profile_dir`.
    pub fn new(host: Arc<dyn ProbeHost>, profile_dir: Option<PathBuf>) -> Self {
        Self {
            host,
            profile_dir,
            availability: OnceLock::new(),
        }
    }

    /// The local machine, with profiles under the data directory.
    pub fn local() -> Self {
        Self::new(Arc::new(LocalProbeHost), default_profile_dir())
    }

    /// Where this launch's profile is written.
    ///
    /// Both components are sanitised even though a profile name is already
    /// restricted and a session key is a UUID: the file name is the one place
    /// where either value would become a path, and a defence that costs a
    /// character filter is worth keeping local to the code that needs it.
    pub fn profile_path(&self, launch: &SandboxLaunch<'_>) -> SandboxResult<PathBuf> {
        let dir = self.profile_dir.as_ref().ok_or_else(|| SandboxError::Io {
            path: "<data directory>".to_string(),
            detail: "friring cannot resolve its data directory, so it has nowhere outside every \
                     sandbox-writable tree to write the generated profile"
                .to_string(),
        })?;
        Ok(dir.join(format!(
            "{}-{}.sb",
            sanitize_component(&launch.policy.profile),
            sanitize_component(launch.session_key)
        )))
    }
}

/// `<data dir>/sandbox/profiles`, created `0700`.
///
/// Deliberately not under the host temp directory. The generated file *is* the
/// policy, so a sandbox that can write it decides its own boundary — and the
/// temp root was, until this moved, inside the writable set of every launch.
/// Under the data directory it is somewhere no profile may make writable
/// ([`crate::sandbox::dirs::check_writable_roots`]).
pub fn default_profile_dir() -> Option<PathBuf> {
    dirs::profile_dir()
}

impl SandboxBackend for SeatbeltBackend {
    fn kind(&self) -> SandboxBackendKind {
        SandboxBackendKind::Seatbelt
    }

    fn probe(&self) -> Availability {
        self.availability
            .get_or_init(|| {
                let platform = detect_platform(self.host.as_ref());
                let HostPlatform::MacOs { major, .. } = platform else {
                    return Availability::unavailable(format!(
                        "seatbelt is macOS-only; this host is {}",
                        platform.label()
                    ));
                };
                if !self.host.path_exists(SANDBOX_EXEC) {
                    // Nothing to install: it ships with the OS, so its absence
                    // means the system is not one friring can reason about.
                    return Availability::unavailable(format!("{SANDBOX_EXEC} is missing"));
                }
                Availability::available(format!("sandbox-exec (macOS {major})"))
            })
            .clone()
    }

    fn capabilities(&self) -> Caps {
        Caps {
            shape: SandboxShape::Policy,
            limits: false,
            network_modes: NetworkMode::ALL,
            read_scopes: ReadScope::ALL,
            persistent: false,
            host_credentials: true,
            inner_agent_sandbox: InnerSandboxVerdict::Denied,
            // A seatbelt process keeps the host's network stack, so host
            // loopback is reachable and the profile opens exactly that port.
            proxy_transport: ProxyTransport::Loopback,
        }
    }

    fn wrap(&self, argv: Argv, launch: &SandboxLaunch<'_>) -> SandboxResult<Argv> {
        if launch.policy.backend != SandboxBackendKind::Seatbelt {
            return Err(SandboxError::Unsupported {
                backend: SandboxBackendKind::Seatbelt,
                detail: format!(
                    "policy was resolved for '{}'; resolve it for seatbelt first",
                    launch.policy.backend
                ),
            });
        }
        launch.validate()?;
        let path = self.profile_path(launch)?;
        write_private(&path, &render_profile(launch))?;

        let mut out: Vec<String> = vec![
            SANDBOX_EXEC.to_string(),
            "-f".to_string(),
            path.display().to_string(),
        ];
        for (name, value) in params(launch) {
            out.push("-D".to_string());
            out.push(format!("{name}={value}"));
        }
        // `--` ends sandbox-exec's own option parsing, so an agent whose first
        // argument starts with `-` is not mistaken for a flag.
        out.push("--".to_string());
        out.extend(argv);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::probe::{ProbeOutput, StubHost};
    use crate::session::{SandboxPath, SandboxPolicy, SandboxProfile};

    fn policy(paths: Vec<SandboxPath>) -> SandboxPolicy {
        SandboxProfile::new("dev", paths)
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap()
    }

    fn workspace_policy() -> SandboxPolicy {
        policy(vec![
            SandboxPath::workspace("~/dev/app"),
            SandboxPath::read_only("/srv/shared"),
        ])
    }

    fn backend() -> SeatbeltBackend {
        SeatbeltBackend::new(Arc::new(StubHost::macos(26, true)), default_profile_dir())
    }

    #[test]
    fn profile_denies_by_default_and_ends_with_its_denies() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let text = render_profile(&launch);

        assert!(text.starts_with("(version 1)\n"));
        assert!(text.contains("\n(deny default)"));
        // The last rule wins in SBPL, so every allow must precede every deny
        // that has to hold.
        let first_deny = text.find("(deny file-read*").unwrap();
        let last_allow = text.rfind("(allow ").unwrap();
        assert!(
            last_allow < first_deny,
            "an allow after the secrets denies would re-open them"
        );
    }

    /// Count parentheses outside string literals and `;;` comments — the two
    /// places a paren does not mean what it looks like.
    fn paren_balance(profile: &str) -> i32 {
        let mut depth = 0;
        for line in profile.lines() {
            let mut in_string = false;
            let mut chars = line.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' if in_string => {
                        chars.next();
                    }
                    '"' => in_string = !in_string,
                    ';' if !in_string => break,
                    '(' if !in_string => depth += 1,
                    ')' if !in_string => depth -= 1,
                    _ => {}
                }
            }
        }
        depth
    }

    #[test]
    fn the_generated_profile_is_balanced_in_every_shape() {
        // A trailing `;;` comment runs to the end of its line, so a closing
        // paren written after one is silently commented out and sandbox-exec
        // rejects the whole profile.
        let mut profile = SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
            ],
        );
        for scope in [ReadScope::HostMinusSecrets, ReadScope::Workspace] {
            for network in NetworkMode::ALL {
                profile.read_scope = scope;
                profile.network_mode = *network;
                let policy = profile
                    .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
                    .unwrap();
                for proxy in [None, Some(ProxyEndpoint::Loopback { port: 8123 })] {
                    let mut launch = SandboxLaunch::new(&policy, "/Users/u", "s1")
                        .with_agent("claude")
                        .with_workspace("/Users/u/work/repo")
                        .with_signal_dir("/Users/u/.local/share/friring/signals/s1")
                        .with_tmp_dir("/tmp/friring-s1")
                        .with_friring_db("/Users/u/.local/share/friring/friring.db");
                    launch.proxy = proxy;
                    assert_eq!(
                        paren_balance(&render_profile(&launch)),
                        0,
                        "unbalanced profile for {scope} / {network}"
                    );
                }
            }
        }
    }

    #[test]
    fn read_only_and_read_write_paths_get_their_own_rules() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let text = render_profile(&launch);

        assert!(text.contains(r#"(allow file-read* file-write* (subpath "/Users/u/dev/app"))"#));
        // host-minus-secrets already allows every read, so a read-only path
        // needs no read rule — only the write boundary that keeps it read-only.
        assert!(text.contains("(allow file-read*)"));
        assert!(text.contains(r#"(deny file-write* (subpath "/srv/shared"))"#));
        assert!(
            text.contains(r#"(deny file-write-unlink (literal "/Users/u/dev/app"))"#),
            "the writable anchor must not be renamable"
        );
    }

    #[test]
    fn a_nested_read_only_path_is_denied_after_its_writable_ancestor() {
        let policy = policy(vec![
            SandboxPath::workspace("~/dev/app"),
            SandboxPath::read_only("~/dev/app/.git/hooks"),
        ]);
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let text = render_profile(&launch);
        let allow = text
            .find(r#"(allow file-read* file-write* (subpath "/Users/u/dev/app"))"#)
            .unwrap();
        let deny = text
            .find(r#"(deny file-write* (subpath "/Users/u/dev/app/.git/hooks"))"#)
            .unwrap();
        assert!(allow < deny, "the nested deny must win by coming last");
    }

    /// "`repo` read-only, `repo/work` read-write" means the descendant is
    /// writable. Anchoring only the read-only root made the descendant *not*
    /// writable here while bwrap's mount order made it writable — one profile,
    /// two answers, decided by the operating system.
    #[test]
    fn the_most_specific_path_rule_is_the_one_that_applies() {
        let nested_grant = policy(vec![
            SandboxPath::read_only("/repo"),
            SandboxPath::workspace("/repo/work"),
        ]);
        let text = render_profile(&SandboxLaunch::new(&nested_grant, "/Users/u", "s1"));
        let deny = text
            .find(r#"(deny file-write* (subpath "/repo"))"#)
            .unwrap();
        let allow = text
            .find(r#"(allow file-read* file-write* (subpath "/repo/work"))"#)
            .unwrap();
        assert!(deny < allow, "the descendant's grant must be the last word");

        // …and the reverse nesting keeps the descendant read-only.
        let nested_deny = policy(vec![
            SandboxPath::workspace("/repo"),
            SandboxPath::read_only("/repo/vendor"),
        ]);
        let text = render_profile(&SandboxLaunch::new(&nested_deny, "/Users/u", "s1"));
        let allow = text
            .find(r#"(allow file-read* file-write* (subpath "/repo"))"#)
            .unwrap();
        let deny = text
            .find(r#"(deny file-write* (subpath "/repo/vendor"))"#)
            .unwrap();
        assert!(allow < deny, "the descendant's deny must be the last word");
    }

    /// Most-specific-wins makes this backend more permissive than it was, so the
    /// two denies that must survive any grant are asserted against the most
    /// specific grant there is: the protected path itself, listed read-write.
    #[test]
    fn the_database_and_secret_denies_outlast_even_a_grant_naming_them() {
        let db = "/Users/u/.local/share/friring/friring.db";
        let policy = policy(vec![
            SandboxPath::workspace("~/.local/share/friring"),
            SandboxPath::workspace("~/.ssh"),
        ]);
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1")
            .with_agent("claude")
            .with_friring_db(db);
        let text = render_profile(&launch);

        let ssh_grant = text
            .find(r#"(allow file-read* file-write* (subpath "/Users/u/.ssh"))"#)
            .unwrap();
        let ssh_deny = text
            .find(r#"(deny file-read* file-write* (subpath "/Users/u/.ssh"))"#)
            .unwrap();
        assert!(ssh_grant < ssh_deny, "a secret must stay denied");

        let data_grant = text
            .find(r#"(allow file-read* file-write* (subpath "/Users/u/.local/share/friring"))"#)
            .unwrap();
        let db_deny = text.find(&format!(r#"(literal "{db}")"#)).unwrap();
        assert!(data_grant < db_deny, "ADR-29 must stay denied");
    }

    /// `host-minus-secrets` makes the host readable and takes back a *named*
    /// list, which named the host's own credential files and not the ones a
    /// sandbox holds. Another profile's synthetic home holds the login its
    /// `volume-login` signed in with, and reading it is taking it (ADR-28).
    #[test]
    fn the_host_read_scope_does_not_carry_the_other_sandboxes_logins_or_policies() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.read_scope = ReadScope::HostMinusSecrets;
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let scratch = dirs::session_scratch_dir("s1")
            .unwrap()
            .display()
            .to_string();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1")
            .with_agent("claude")
            .with_tmp_dir(&scratch);
        let text = render_profile(&launch);

        let host_read = text.find("(allow file-read*)").expect("the host scope");
        for dir in [dirs::place_root(), dirs::profile_dir(), dirs::seeds_root()]
            .into_iter()
            .flatten()
        {
            let rule = format!("(subpath {:?})", dir.display().to_string());
            let at = text
                .find(&rule)
                .unwrap_or_else(|| panic!("{rule} must be denied:\n{text}"));
            assert!(at > host_read, "{rule} must be taken back after the scope");
        }
        // …and the launch's own scratch, which lives in the same tree, is not
        // taken with them: an agent that cannot write a temp file dies on
        // startup, and this deny is emitted last so anything it covers is gone.
        for dir in [dirs::place_root(), dirs::profile_dir(), dirs::seeds_root()]
            .into_iter()
            .flatten()
        {
            let dir = dir.display().to_string();
            assert!(
                !dirs::encloses(&dir, &scratch),
                "{dir} must not cover this launch's own scratch {scratch}"
            );
        }
    }

    /// SBPL denies by pathname, so a deny is only as good as the path staying
    /// put: with `~` writable, renaming `~/.local/share` moves the database to a
    /// name no deny matches.
    #[test]
    fn every_directory_leading_to_a_protected_path_is_pinned_against_rename() {
        let db = "/Users/u/.local/share/friring/friring.db";
        let policy = policy(vec![SandboxPath::workspace("~")]);
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1")
            .with_agent("claude")
            .with_friring_db(db);
        let text = render_profile(&launch);

        for dir in [
            "/Users/u/.local",
            "/Users/u/.local/share",
            "/Users/u/.local/share/friring",
            "/Users/u/.codex",
            "/Users/u/Library",
        ] {
            assert!(
                text.contains(&format!(r#"(deny file-write-unlink (literal "{dir}"))"#)),
                "{dir} can still be renamed out from under its deny"
            );
        }
        // The writable anchor was already pinned, and is not pinned twice.
        assert_eq!(
            text.matches(r#"(deny file-write-unlink (literal "/Users/u"))"#)
                .count(),
            1
        );
        // Nothing to pin when the launch names no protected path outside home.
        let bare = SandboxLaunch::new(&policy, "/Users/u", "s1");
        assert!(!render_profile(&bare).contains(r#"(literal "/Users/u/.local/share/friring")"#));
    }

    #[test]
    fn every_writable_root_protects_its_git_hooks() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let text = render_profile(&launch);
        assert!(text.contains(r#"(deny file-write* (subpath "/Users/u/dev/app/.git/hooks"))"#));

        // The session's workspace is protected through its own parameter, so
        // the rule can name a path the profile text never spells out.
        let with_workspace =
            SandboxLaunch::new(&policy, "/Users/u", "s1").with_workspace("/Users/u/work/repo");
        let text = render_profile(&with_workspace);
        assert!(text.contains(r#"(deny file-write* (subpath (param "WORKSPACE_PROTECTED")))"#));
        assert!(params(&with_workspace).contains(&(
            "WORKSPACE_PROTECTED",
            "/Users/u/work/repo/.git/hooks".to_string()
        )));
    }

    #[test]
    fn workspace_read_scope_lists_paths_instead_of_the_whole_host() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::read_only("/srv/shared")]);
        profile.read_scope = ReadScope::Workspace;
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let text = render_profile(&launch);

        assert!(!text.contains("(allow file-read*)\n"));
        assert!(text.contains(r#"(allow file-read* (subpath "/srv/shared"))"#));
        // Without the system read set nothing would get past dyld.
        assert!(text.contains(r#"(subpath "/usr")"#));
        assert!(text.contains(r#"(subpath "/System")"#));
    }

    #[test]
    fn session_paths_are_parameters_not_baked_in() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1")
            .with_workspace("/Users/u/work/repo")
            .with_signal_dir("/Users/u/.local/share/friring/signals/s1")
            .with_tmp_dir("/tmp/friring-s1");
        let text = render_profile(&launch);

        assert!(text.contains(r#"(allow file-read* file-write* (subpath (param "WORKSPACE")))"#));
        assert!(text.contains(r#"(subpath (param "SIGNAL_DIR"))"#));
        assert!(text.contains(r#"(subpath (param "TMP_DIR"))"#));
        // The values live on the command line, not in the profile text.
        assert!(!text.contains("/Users/u/work/repo"));
        assert_eq!(
            params(&launch),
            [
                ("WORKSPACE", "/Users/u/work/repo".to_string()),
                (
                    "SIGNAL_DIR",
                    "/Users/u/.local/share/friring/signals/s1".to_string()
                ),
                ("TMP_DIR", "/tmp/friring-s1".to_string()),
                (
                    "WORKSPACE_PROTECTED",
                    "/Users/u/work/repo/.git/hooks".to_string()
                ),
            ]
        );
    }

    #[test]
    fn a_session_path_the_profile_already_lists_is_not_repeated() {
        let policy = workspace_policy();
        let launch =
            SandboxLaunch::new(&policy, "/Users/u", "s1").with_workspace("/Users/u/dev/app");
        let text = render_profile(&launch);
        assert!(!text.contains(r#"(allow file-read* file-write* (subpath (param "WORKSPACE")))"#));
        // The parameter is still passed; an unreferenced one is harmless, and
        // dropping it would make the argv depend on the profile's contents.
        assert!(params(&launch)
            .iter()
            .any(|(name, _)| *name == PARAM_WORKSPACE));
    }

    #[test]
    fn network_none_adds_no_rule_at_all() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_mode = NetworkMode::None;
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let text = render_profile(&launch);
        assert!(!text.contains("(allow network"));
        assert!(text.contains(";; --- network: none ---"));
    }

    #[test]
    fn network_full_opens_egress_and_name_resolution() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_mode = NetworkMode::Full;
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let text = render_profile(&launch);
        assert!(text.contains("(allow network*)"));
        assert!(text.contains("com.apple.mDNSResponder"));
    }

    /// A filtered mode with no proxy is refused at launch; the generator is
    /// pure and answers anyway, and its answer must be *closed*. Opening
    /// `network*` here — the shape `full` uses — would turn a missing proxy
    /// into unrestricted egress.
    #[test]
    fn a_filtered_mode_without_a_proxy_grants_exactly_what_none_does() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_deny = vec!["evil.example".into()];
        for mode in [NetworkMode::Allowlist, NetworkMode::Full] {
            profile.network_mode = mode;
            let policy = profile
                .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
                .unwrap();
            let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
            let text = render_profile(&launch);
            assert!(!text.contains("(allow network"), "{mode}: {text}");
            assert!(text.contains("no proxy running"), "{mode}");
            assert!(params(&launch).iter().all(|(n, _)| *n != PARAM_PROXY));
            // And the launch itself is refused rather than quietly started.
            assert!(launch.validate().is_err(), "{mode}");
        }
    }

    #[test]
    fn a_proxied_mode_opens_only_that_endpoint() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_deny = vec!["evil.example".into()];
        // `full` with denies is proxied exactly like `allowlist`: the denies
        // are enforceable only outside the boundary, so nothing else may leave.
        for mode in [NetworkMode::Allowlist, NetworkMode::Full] {
            profile.network_mode = mode;
            let policy = profile
                .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
                .unwrap();
            let launch = SandboxLaunch::new(&policy, "/Users/u", "s1")
                .with_proxy(ProxyEndpoint::Loopback { port: 8123 });
            let text = render_profile(&launch);
            assert!(
                text.contains(r#"(allow network-outbound (remote ip (param "PROXY")))"#),
                "{mode}: {text}"
            );
            // Not the `full` shape: unrestricted egress would ignore the rules
            // the proxy exists to apply.
            assert!(!text.contains("(allow network*)"), "{mode}: {text}");
            // No DNS: the proxy resolves, so name lookups cannot become a side
            // channel.
            assert!(!text.contains("mDNSResponder"), "{mode}");
            assert_eq!(
                params(&launch).last(),
                Some(&("PROXY", "localhost:8123".to_string()))
            );
        }

        let policy = workspace_policy();
        let socket =
            SandboxLaunch::new(&policy, "/Users/u", "s1").with_proxy(ProxyEndpoint::UnixSocket {
                host_path: "/Users/u/.local/share/friring/sandbox/tmp/s1/proxy.sock".into(),
                inside_path: "/Users/u/.local/share/friring/sandbox/tmp/s1/proxy.sock".into(),
            });
        assert!(render_profile(&socket)
            .contains(r#"(allow network-outbound (literal (param "PROXY")))"#));
    }

    #[test]
    fn the_keychain_lookups_are_present_and_the_database_is_not_readable() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let text = render_profile(&launch);
        for service in [
            "com.apple.SecurityServer",
            "com.apple.securityd",
            "com.apple.trustd",
            "com.apple.ocspd",
            "com.apple.cfprefsd.daemon",
        ] {
            assert!(text.contains(service), "missing mach service {service}");
        }
        // The Keychain *database* is still denied: items stay reachable through
        // securityd's per-item prompts, the store cannot be copied wholesale.
        assert!(text.contains(r#"(subpath "/Users/u/Library/Keychains")"#));
    }

    #[test]
    fn secrets_are_denied_except_the_launching_agents_own() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1").with_agent("claude");
        let text = render_profile(&launch);
        assert!(text.contains(r#"(deny file-read* file-write* (subpath "/Users/u/.ssh"))"#));
        assert!(text.contains(r#"(subpath "/Users/u/.codex/auth.json")"#));
        assert!(!text.contains(".claude/.credentials.json"));
        // The reason travels with the rule.
        assert!(text.contains(";; private keys and known_hosts"));
    }

    #[test]
    fn the_database_is_denied_last_even_under_a_writable_data_directory() {
        let policy = policy(vec![SandboxPath::workspace("~/.local/share/friring")]);
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1")
            .with_friring_db("/Users/u/.local/share/friring/friring.db");
        let text = render_profile(&launch);
        let allow = text
            .find(r#"(allow file-read* file-write* (subpath "/Users/u/.local/share/friring"))"#)
            .unwrap();
        let deny = text
            .find(r#"(literal "/Users/u/.local/share/friring/friring.db")"#)
            .unwrap();
        assert!(allow < deny, "ADR-29: the database must stay denied");
        assert!(text.contains(r#"(literal "/Users/u/.local/share/friring/friring.db-wal")"#));
        assert!(text.contains(r#"(literal "/Users/u/.local/share/friring/friring.db-shm")"#));
    }

    #[test]
    fn wrap_prefixes_sandbox_exec_with_the_profile_and_its_parameters() {
        let policy = workspace_policy();
        let backend = backend();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "wrap-test")
            .with_workspace("/Users/u/work/repo")
            .with_proxy(ProxyEndpoint::Loopback { port: 8123 });
        let argv = backend
            .wrap(
                vec!["claude".into(), "--resume".into(), "abc".into()],
                &launch,
            )
            .unwrap();

        assert_eq!(argv[0], SANDBOX_EXEC);
        assert_eq!(argv[1], "-f");
        assert!(argv[2].ends_with("dev-wrap-test.sb"));
        assert!(argv.contains(&"-D".to_string()));
        assert!(argv.contains(&"WORKSPACE=/Users/u/work/repo".to_string()));
        // `--` keeps an agent flag from being read as a sandbox-exec flag.
        let end = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[end + 1..], ["claude", "--resume", "abc"]);

        // The profile really was written, and only for this user.
        let written = std::fs::read_to_string(backend.profile_path(&launch).unwrap()).unwrap();
        assert!(written.starts_with("(version 1)"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(backend.profile_path(&launch).unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(backend.profile_path(&launch).unwrap());
    }

    #[test]
    fn wrap_refuses_a_policy_resolved_for_another_backend() {
        let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")])
            .resolve(SandboxBackendKind::Bwrap, "/Users/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let err = backend().wrap(vec!["claude".into()], &launch).unwrap_err();
        assert!(err.to_string().contains("resolve it for seatbelt first"));
    }

    #[test]
    fn probe_needs_macos_and_the_system_binary() {
        assert!(backend().probe().is_available());

        let linux = SeatbeltBackend::new(
            Arc::new(StubHost::linux_with_bwrap("0.11.0")),
            Some(PathBuf::from("/tmp")),
        );
        assert_eq!(
            linux.probe().message(),
            "seatbelt is macOS-only; this host is Linux"
        );

        // A macOS that answers `uname` but has no sandbox-exec names the binary
        // rather than claiming the OS is wrong.
        let stripped = StubHost::new()
            .with_command("uname -s", ProbeOutput::success("Darwin\n"))
            .with_command("uname -m", ProbeOutput::success("arm64\n"))
            .with_command("sw_vers -productVersion", ProbeOutput::success("26.1\n"));
        let backend = SeatbeltBackend::new(Arc::new(stripped), Some(PathBuf::from("/tmp")));
        assert!(backend.probe().message().contains("/usr/bin/sandbox-exec"));
    }

    #[test]
    fn capabilities_say_what_seatbelt_cannot_do() {
        let caps = backend().capabilities();
        assert_eq!(caps.shape, SandboxShape::Policy);
        assert!(!caps.limits, "a host process has no resource boundary");
        assert!(caps.host_credentials, "the Keychain stays reachable");
        assert!(!caps.persistent);
        assert_eq!(caps.inner_agent_sandbox, InnerSandboxVerdict::Denied);
    }

    #[test]
    fn a_hostile_profile_name_cannot_escape_the_profile_directory() {
        // Neither value can hold a separator today (the name charset forbids
        // it, the key is a UUID); the file name is where either would become a
        // path, so the filter lives next to the code that needs it.
        let policy = SandboxPolicy {
            profile: "../../etc/evil".to_string(),
            ..workspace_policy()
        };
        let launch = SandboxLaunch::new(&policy, "/Users/u", "../../s");
        let path = backend().profile_path(&launch).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "..-..-etc-evil-..-..-s.sb"
        );
        assert_eq!(
            Some(path.parent().unwrap()),
            backend().profile_dir.as_deref()
        );
    }
}
