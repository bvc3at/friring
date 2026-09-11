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
use crate::sandbox::launcher::{launch_argv, mux_env_to_unset, LaunchHelper};
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
/// The per-session bridge queue (ADR-30).
pub const PARAM_BRIDGE_DIR: &str = "BRIDGE_DIR";
/// The writable temp directory.
pub const PARAM_TMP_DIR: &str = "TMP_DIR";
/// The egress proxy: `localhost:<port>`, or a unix socket path.
pub const PARAM_PROXY: &str = "PROXY";
/// The read-only launch gate a gated child waits on (ADR-33).
pub const PARAM_GATE_DIR: &str = "GATE_DIR";
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
/// The first six are the reason host-passthrough credentials and native TLS
/// work at all:
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
    (
        "com.apple.trustd.agent",
        "per-user certificate-chain evaluation for native TLS",
    ),
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
        (PARAM_BRIDGE_DIR, launch.bridge_dir),
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
    if let Some(gate) = launch.gate_dir {
        out.push((PARAM_GATE_DIR, gate.to_string()));
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
    render_own_gate(&mut out, launch);
    render_database(&mut out, launch);
    render_child_subtract(&mut out, launch);
    render_multiplexer_denies(&mut out, launch);

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
        (PARAM_BRIDGE_DIR, launch.bridge_dir),
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
    // The launch gate, read-only and never anywhere else (ADR-33). A parameter
    // like the other per-session paths, and deliberately in the *readable* half:
    // `render_path_rules` turns a readable slot into `(deny file-write* …)`
    // emitted after every allow, so even a profile granting the whole home
    // directory read-write leaves the gate unwritable — which is the entire
    // proof that only the host can release one.
    if let Some(gate) = launch
        .gate_dir
        .filter(|g| writable.iter().all(|s| s.value != **g))
    {
        readable.push(PathSlot::param(PARAM_GATE_DIR, gate));
    }
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
            // Two programs are read into the set rather than left to the system
            // directories, for one reason: a narrow scope that cannot read them
            // opens on a pane that dies before the agent starts. friring's own
            // CLI is the launch helper every policy launch execs (ADR-33), and
            // the **agent's** program is what that helper then execs — an
            // extension that ships its agent as a script under its own home is
            // outside every system directory, and no operator should have to add
            // it to a profile by hand.
            let filters: Vec<String> = SYSTEM_READ_PATHS
                .iter()
                .map(|p| format!("(subpath {p:?})"))
                .chain(
                    launch
                        .helper_program
                        .into_iter()
                        .chain(launch.agent_program)
                        .map(|program| format!("(literal {program:?})")),
                )
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
            "including the host's, outside the boundary. A writable root that is",
            "a git directory, or one worktree's metadata, also keeps back the",
            "config and the redirects that name a program git executes. A no-op",
            "where the root is neither.",
        ],
    );
    // Only the profile's own roots and the session's workspace: the signal and
    // scratch directories friring mints are never repositories.
    for slot in writable.iter().filter(|s| s.is_literal) {
        for path in crate::sandbox::backend::protected_paths_in(&slot.value) {
            out.push(format!("(deny file-write* (subpath {path:?}))"));
        }
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

/// Re-grant **this** launch's gate directory, read-only, after the gate tree is
/// denied wholesale.
///
/// The deny above covers every session's gate, which is what stops one sandbox
/// reading another's release file and learning its key. This launch's own gate
/// is the exception it needs: the helper polls it for the file the host renames
/// in (ADR-33). Read-only and never writable, so the one thing that can open a
/// gate is still the host — `render_path_rules` already emits the write deny,
/// and this restores only the read the wholesale deny took back.
fn render_own_gate(out: &mut Vec<String>, launch: &SandboxLaunch<'_>) {
    if launch.gate_dir.is_none() {
        return;
    }
    section(
        out,
        "this launch's own gate",
        &[
            "Read-only, and after the deny above: a gated launch polls this",
            "directory for the release file the host renames in, and every other",
            "session's gate stays unreadable.",
        ],
    );
    out.push(format!(
        "(allow file-read* (subpath (param {PARAM_GATE_DIR:?})))"
    ));
    out.push(format!(
        "(deny file-write* (subpath (param {PARAM_GATE_DIR:?})))"
    ));
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
    let denied: Vec<String> = [
        dirs::place_root(),
        dirs::profile_dir(),
        dirs::seeds_root(),
        // The gate tree, in **both** modes. A release file carries the gate key
        // its launch helper compares against, so a session that can read another
        // session's gate learns it — and `check_declared_paths` already refuses
        // a profile that *names* this tree in either mode, saying "reading or
        // writing it is enough to take any of them". Under `host-minus-secrets`
        // the read was granted anyway, which a real-kernel probe found: the
        // named credential list does not cover friring's own trees. This
        // launch's *own* gate is re-granted read-only afterwards, by
        // `PARAM_GATE_DIR`.
        dirs::gate_root(),
    ]
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
            "credential to one boundary (ADR-28), the generated policies that",
            "constrain other sessions, and the launch gates whose release files",
            "carry the key a helper waits for. Reading any of them is taking it,",
            "so the host read scope takes them back here rather than trusting the",
            "list of host credential paths to cover them.",
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
    // What "the database" is comes from one place, so a deny here and a mount
    // refusal in `dirs` cannot end up meaning different sets of files.
    let files: Vec<String> = dirs::database_files(db)
        .iter()
        .map(|f| format!("(literal {f:?})"))
        .collect();
    out.push(format!(
        "(deny file-read* file-write*\n    {})",
        files.join("\n    ")
    ));
}

/// A bridge child's subtract set, denied after **every** allow, and then the
/// exact seed targets re-granted after those (ADR-31).
///
/// The ordering is the whole mechanism. A parent profile may grant the entire
/// home directory, and the family's state directory is inside it — so what a
/// child must not reach cannot be expressed as an absent grant. It has to be a
/// deny that comes *after* every allow, which SBPL's last-match-wins gives for
/// free.
///
/// Then the exact seed targets come after **that**, as the last rule for those
/// paths alone: a `symlink` seed is a link into a directory the subtract set has
/// just denied, so without the re-grant it would be a dead link. Read-only for a
/// `symlink`, read *and* write for `link-rw` — so write reaches exactly the
/// credential file the agent refreshes and never the state directory, the
/// transcripts, the history or the sessions around it. That one shared file is
/// ADR-28's condition: no second copy of a rotating token ever exists.
fn render_child_subtract(out: &mut Vec<String>, launch: &SandboxLaunch<'_>) {
    let (subtract, seeds) = launch.subtract();
    if launch.narrowing.is_none() {
        return;
    }
    if !subtract.is_empty() {
        section(
            out,
            "a bridge child's subtract set",
            &[
                "What this child may not reach, whatever encloses it: its",
                "family's agent state (transcripts, history, logs, sessions),",
                "its owner's and every sibling's control directories and",
                "worktrees, and friring's own trees. Denied after every allow,",
                "because a parent profile granting the whole home directory",
                "encloses all of it.",
            ],
        );
        for entry in subtract {
            // `subpath` for both: for a directory it is the tree, and for a
            // file it is the file itself — SBPL's `subpath` on a non-directory
            // matches exactly that path, so one form covers both and cannot
            // disagree with the kind the host recorded.
            out.push(format!(
                "(deny file-read* file-write* (subpath {:?}))",
                entry.path
            ));
        }
    }
    // The child's own directories, re-granted immediately after the denies.
    // They are inside the trees just denied wholesale, so without this the child
    // cannot read its own gate, write its own workspace, report its own status
    // or reach its own bridge queue — the boundary would deny the child itself.
    let (own_rw, own_ro) = launch.child_own_paths();
    if !own_rw.is_empty() || !own_ro.is_empty() {
        section(
            out,
            "a bridge child's own directories",
            &[
                "Inside the trees denied above, because that is how the deny",
                "covers every sibling. Re-granted here so the child reaches",
                "exactly its own: its worktree, scratch, signal file, bridge",
                "queue and private state, and its gate read-only — a writable",
                "gate is a gate the child can open for itself (ADR-33).",
            ],
        );
        for path in &own_rw {
            out.push(format!(
                "(allow file-read* file-write* (subpath {path:?}))   ;; the child's own"
            ));
        }
        for path in &own_ro {
            out.push(format!(
                "(allow file-read* (subpath {path:?}))   ;; the child's own gate"
            ));
        }
        // And what stays read-only *inside* one of the child's own directories,
        // emitted after the allow that just re-opened it. SBPL is
        // last-match-wins, so the same rule earlier in the profile
        // (`render_protected_subdirectories`) is overridden by the re-grant
        // above — which is how a child's own `.git/worktrees/<id>` came back
        // with its `gitdir` and `commondir` redirects writable, and a redirect a
        // child can write is a program the *host* runs when friring inspects
        // that worktree.
        for path in own_rw
            .iter()
            .flat_map(|path| crate::sandbox::backend::protected_paths_in(path))
        {
            out.push(format!("(deny file-write* (subpath {path:?}))"));
        }
    }
    if seeds.is_empty() {
        return;
    }
    section(
        out,
        "a bridge child's seed targets",
        &[
            "The exact files the subtract set above just denied and this child",
            "legitimately needs: read-only for a linked skill or prompt, read",
            "and write for the one credential the agent refreshes in place, so",
            "no second copy of a rotating token exists (ADR-28). Last, so this",
            "is the rule that applies to these paths and to no others.",
        ],
    );
    for grant in seeds {
        let verbs = if grant.is_writable() {
            "file-read* file-write*"
        } else {
            "file-read*"
        };
        // `subpath` rather than `literal`: a seed may be a directory (a skills
        // tree), and a literal would grant the directory itself and nothing in
        // it.
        out.push(format!(
            "(allow {verbs} (subpath {:?}))   ;; seed: {}",
            grant.target, grant.mode
        ));
    }
}

/// The host's multiplexer sockets, denied after **every** allow so nothing above
/// can re-open them (ADR-33).
///
/// SBPL models a unix-domain socket as a network operation, so `(allow
/// network*)` under an unrestricted `full` profile grants `connect(2)` on every
/// pathname socket on the host — friring's own tmux server included. A sandbox
/// that reaches it asks that server to run a command in a pane, outside the
/// boundary, as the user. The path-filter form is the one the backend already
/// uses for the proxy socket (`(allow network-outbound (literal (param
/// "PROXY")))`), so this is the same primitive pointed the other way.
///
/// Both `network-outbound` and `network-bind` are denied: dialling the socket is
/// the escape, and binding over it would let a sandbox impersonate the server to
/// something outside.
///
/// The set is [`dirs::multiplexer_socket_denies`] — closed, and every entry in
/// both its written and its resolved spelling, because SBPL matches by pathname
/// and `/tmp` is `/private/tmp` on macOS.
fn render_multiplexer_denies(out: &mut Vec<String>, launch: &SandboxLaunch<'_>) {
    let denies = launch.multiplexer_denies();
    if denies.is_empty() {
        return;
    }
    section(
        out,
        "the host's multiplexer sockets",
        &[
            "SBPL models a unix socket as a network operation, so an",
            "unrestricted `full` profile would otherwise grant connect(2) on",
            "friring's own tmux server — which runs commands in host panes,",
            "outside this boundary. Denied last, so no allow above re-opens it.",
            "A closed set: see dirs::multiplexer_socket_denies for what is",
            "deliberately *not* in it and why.",
        ],
    );
    for deny in denies {
        let filter = if deny.is_dir {
            format!("(subpath {:?})", deny.path)
        } else {
            format!("(literal {:?})", deny.path)
        };
        out.push(format!("(deny network-outbound {filter})"));
        out.push(format!("(deny network-bind {filter})"));
    }
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
            // The boundary is the wrapped process on this host's filesystem, so
            // a directory friring mints is a directory the sandbox reaches at
            // the same path.
            bridge: true,
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
        // Resolved before the profile is rendered, because the helper has to be
        // *readable* inside the boundary: under the narrow read scope the
        // profile grants only the system directories, and friring's own CLI is
        // as often as not somewhere else entirely.
        let program = launch_program(launch)?;
        let launch = &launch.clone().with_helper_program(&program);
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
        // friring's own launch helper runs first and `execvp`s the agent in
        // place (ADR-33). Seatbelt never starts a relay — a seatbelt sandbox
        // reaches the proxy socket directly — so the helper's jobs here are the
        // gate and stripping the multiplexer environment. Composed for every
        // launch, not only gated ones: tmux sets `$TMUX` in the pane itself, and
        // it points at friring's own server.
        out.extend(launch_argv(
            &program,
            &LaunchHelper {
                gate: launch.gate(),
                relay: None,
                unset: mux_env_to_unset(),
            },
        ));
        out.extend(argv);
        Ok(out)
    }
}

/// friring's own CLI, for the launch helper.
///
/// The launch's own answer when it carries one — the launch path resolves it
/// once and hands it down, which is what makes this composition testable without
/// a `friring-cli` beside the test binary. Otherwise resolved from the running
/// binary, never from `PATH`, for the reason
/// [`crate::sandbox::bwrap::local_relay_program`] is: the program that applies a
/// launch's last boundary steps must be *this* friring's, not one the user's
/// environment chose. A launch that has neither is refused — an ordinary
/// refusal, so a profile's `allow_unsandboxed_fallback` still answers it.
fn launch_program(launch: &SandboxLaunch<'_>) -> SandboxResult<String> {
    if let Some(program) = launch.helper_program {
        return Ok(program.to_string());
    }
    crate::sandbox::bwrap::local_relay_program()
        .map(|p| p.display().to_string())
        .ok_or_else(|| SandboxError::Refused {
            profile: launch.policy.profile.clone(),
            detail: "friring could not locate its own 'friring-cli', which every sandboxed \
                     launch runs inside the boundary to drop the host multiplexer's environment \
                     and — for a bridge child — to wait for its session row. Install friring-cli \
                     beside friring"
                .to_string(),
        })
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

    /// friring's own CLI, as the launch path resolved it. Injected so the
    /// composition is testable on a machine with no `friring-cli` beside the
    /// test binary.
    const HELPER: &str = "/usr/local/bin/friring-cli";

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

    /// The host's multiplexer sockets, as a launch on this machine sees them.
    fn host_mux() -> crate::session::HostMuxSockets {
        crate::session::HostMuxSockets {
            own_socket: PathBuf::from("/tmp/tmux-501/friring"),
            outer_socket: Some(PathBuf::from("/private/tmp/tmux-501/outer")),
            uid: 501,
        }
    }

    /// The whole deny set is in the text, in both spellings, under **every**
    /// combination of network mode and read scope — including unrestricted
    /// `full`, which is the mode that would otherwise grant `connect(2)` on
    /// friring's own tmux socket through `(allow network*)`.
    #[test]
    fn every_mode_denies_the_whole_multiplexer_set() {
        let host = host_mux();
        let expected = dirs::multiplexer_socket_denies(&host);
        // Stated as content rather than as a count. How many *spellings* a
        // socket root has is a property of the machine — `/private/tmp` is a
        // macOS root, and `canonical` resolves `/tmp` into it only there — so a
        // count makes this test assert something different per platform. What
        // it is actually about is that both sockets and a directory are in the
        // set the rules below are then checked against.
        for socket in [&host.own_socket, host.outer_socket.as_ref().unwrap()] {
            let named = socket.display().to_string();
            assert!(
                expected
                    .iter()
                    .any(|deny| !deny.is_dir && deny.path == named),
                "the fixture should deny the socket itself: {named} not in {expected:?}"
            );
        }
        assert!(
            expected.iter().any(|deny| deny.is_dir),
            "the fixture should deny at least one socket directory: {expected:?}"
        );
        for mode in [NetworkMode::Full, NetworkMode::None] {
            for scope in [ReadScope::HostMinusSecrets, ReadScope::Workspace] {
                let mut profile =
                    SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
                profile.network_mode = mode;
                profile.read_scope = scope;
                let policy = profile
                    .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
                    .unwrap();
                let launch = SandboxLaunch::new(&policy, "/Users/u", "s1").with_host_mux(&host);
                let text = render_profile(&launch);
                for deny in &expected {
                    let filter = if deny.is_dir {
                        format!("(subpath {:?})", deny.path)
                    } else {
                        format!("(literal {:?})", deny.path)
                    };
                    assert!(
                        text.contains(&format!("(deny network-outbound {filter})")),
                        "{mode}/{scope} must deny outbound to {}",
                        deny.path
                    );
                    assert!(
                        text.contains(&format!("(deny network-bind {filter})")),
                        "{mode}/{scope} must deny binding {}",
                        deny.path
                    );
                }
            }
        }
    }

    /// The set is **closed**: a blanket denial of the temp and runtime roots
    /// would take away the IPC a real agent needs — its own language server, a
    /// test harness's socket, a package-manager daemon — so those roots are
    /// never denied as wholes, only their exact `tmux-<uid>` children.
    #[test]
    fn the_deny_set_never_names_a_whole_temp_or_runtime_root() {
        let host = host_mux();
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1").with_host_mux(&host);
        let text = render_profile(&launch);
        for root in ["/tmp", "/private/tmp", "/run", "/var/run"] {
            for verb in ["network-outbound", "network-bind"] {
                assert!(
                    !text.contains(&format!("(deny {verb} (subpath {root:?}))")),
                    "denying all of {root} would break legitimate sandbox IPC"
                );
            }
        }
    }

    /// A launch with no gate carries no gate parameter and no gate rule — the
    /// generated text for an ordinary session is unchanged.
    #[test]
    fn an_ungated_launch_has_no_gate_rule() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        assert!(params(&launch).iter().all(|(n, _)| *n != PARAM_GATE_DIR));
        assert!(!render_profile(&launch).contains(PARAM_GATE_DIR));
    }

    /// The gate is readable and, whatever else the profile granted, never
    /// writable — the whole proof that only the host can release one.
    #[test]
    fn the_gate_is_readable_and_never_writable() {
        // A profile that hands the agent the entire home directory read-write,
        // which is the case a naive rule would get wrong.
        let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("~")])
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let gate = "/Users/u/.local/share/friring/gates/c1";
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1").with_gate(gate, "k-1");
        let text = render_profile(&launch);

        assert!(params(&launch)
            .iter()
            .any(|(n, v)| *n == PARAM_GATE_DIR && v == gate));
        let write_allow =
            format!("(allow file-read* file-write* (subpath (param {PARAM_GATE_DIR:?})))");
        assert!(!text.contains(&write_allow));
        let deny_write = format!("(deny file-write* (subpath (param {PARAM_GATE_DIR:?})))");
        assert!(text.contains(&deny_write), "{text}");
        // And it comes after the home grant, so the most specific rule wins.
        let home_allow = text.find("(allow file-read* file-write* (subpath \"/Users/u\"))");
        assert!(home_allow.is_some_and(|at| at < text.find(&deny_write).unwrap()));
    }

    /// A child's narrowing, as a launch carries it.
    /// A child's overlay whose own directories are **inside** the trees its
    /// subtract set denies wholesale, which is what a real one always looks
    /// like: `subtract_set` denies `<data>/{sandbox,signals,gates,worktrees}` in
    /// order to cover every sibling, and friring mints the child's own
    /// directories under exactly those roots.
    ///
    /// The fixture was once written with the two sets on different roots, so the
    /// interesting case — the child being denied itself — never arose.
    fn child_narrowing() -> crate::session::SandboxOverlay {
        const DATA: &str = "/Users/u/.local/share/friring";
        crate::session::SandboxOverlay {
            worktree: Some(format!("{DATA}/worktrees/repo/child")),
            own_dirs: vec![
                format!("{DATA}/sandbox/tmp/child"),
                format!("{DATA}/signals/child"),
                format!("{DATA}/signals/child/bridge"),
            ],
            gate_dir: Some(format!("{DATA}/gates/child")),
            state_dir: Some(format!("{DATA}/sandbox/state/child")),
            seed: vec![
                crate::session::SeedGrant {
                    target: "/Users/u/.codex/auth.json".into(),
                    mode: crate::session::SeedMode::LinkRw,
                },
                crate::session::SeedGrant {
                    target: "/Users/u/.codex/skills".into(),
                    mode: crate::session::SeedMode::Symlink,
                },
            ],
            subtract: vec![
                crate::session::SubtractPath {
                    path: "/Users/u/.codex".into(),
                    is_dir: true,
                },
                crate::session::SubtractPath {
                    path: format!("{DATA}/sandbox"),
                    is_dir: true,
                },
                crate::session::SubtractPath {
                    path: format!("{DATA}/signals"),
                    is_dir: true,
                },
                crate::session::SubtractPath {
                    path: format!("{DATA}/gates"),
                    is_dir: true,
                },
                crate::session::SubtractPath {
                    path: format!("{DATA}/worktrees"),
                    is_dir: true,
                },
            ],
            ..crate::session::SandboxOverlay::default()
        }
    }

    /// The re-grant of a child's own directories must not re-open what is
    /// protected **inside** one of them.
    ///
    /// SBPL is last-match-wins and the child's own directories are allowed after
    /// every deny — so the protected rules emitted with the profile's writable
    /// roots are undone for exactly the directory ADR-31 mints for this child.
    /// That is its `.git/worktrees/<id>`, whose `gitdir` and `commondir`
    /// redirects decide which repository git reads `config` from; friring runs
    /// `git` in a child's worktree to reach a verdict, so a redirect a child can
    /// write is a `core.fsmonitor` the **host** executes.
    ///
    /// A test that renders this without a narrowing cannot see it: the re-grant
    /// only exists for a bridge child, and this defect was written and shipped
    /// past exactly such a test.
    #[test]
    fn a_childs_own_git_metadata_keeps_its_redirects_after_the_re_grant() {
        const MINE: &str = "/Users/u/dev/app/.git/worktrees/child";
        let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("~")])
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let mut overlay = child_narrowing();
        overlay.own_dirs.push(MINE.to_string());
        let launch = SandboxLaunch::new(&policy, "/Users/u", "child")
            .with_narrowing(&overlay)
            .with_helper_program(HELPER);
        let text = render_profile(&launch);

        let allow =
            format!("(allow file-read* file-write* (subpath {MINE:?}))   ;; the child's own");
        let at_allow = text
            .rfind(&allow)
            .unwrap_or_else(|| panic!("the child's own git directory is not re-granted:\n{text}"));
        for name in crate::sandbox::backend::PROTECTED_IN_WORKTREE_METADATA {
            let deny = format!("(deny file-write* (subpath \"{MINE}/{name}\"))");
            let at_deny = text
                .rfind(&deny)
                .unwrap_or_else(|| panic!("'{name}' is never denied:\n{text}"));
            assert!(
                at_deny > at_allow,
                "the re-grant of the child's own directory re-opens '{name}'"
            );
        }
        // And what a commit writes in that same directory is still writable.
        for name in ["HEAD", "index"] {
            let deny = format!("(deny file-write* (subpath \"{MINE}/{name}\"))");
            assert!(!text.contains(&deny), "a child cannot commit:\n{text}");
        }
    }

    /// The subtract set denies friring's trees wholesale, and the child's own
    /// directories are inside them — so a renderer that stopped at the denies
    /// would build a boundary in which the child cannot read its own gate, write
    /// its own workspace, report its own status or reach its own bridge queue.
    #[test]
    fn a_childs_own_directories_are_re_granted_after_the_subtract_set() {
        let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("~")])
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let overlay = child_narrowing();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "child")
            .with_narrowing(&overlay)
            .with_helper_program(HELPER);
        let text = render_profile(&launch);

        let deny = |path: &str| format!("(deny file-read* file-write* (subpath {path:?}))");
        let allow_rw = |path: &str| {
            format!("(allow file-read* file-write* (subpath {path:?}))   ;; the child's own")
        };
        let allow_ro =
            |path: &str| format!("(allow file-read* (subpath {path:?}))   ;; the child's own gate");

        for own in overlay
            .worktree
            .iter()
            .chain(overlay.own_dirs.iter())
            .chain(overlay.state_dir.iter())
        {
            let grant = text
                .find(&allow_rw(own))
                .unwrap_or_else(|| panic!("no re-grant for the child's own '{own}':\n{text}"));
            // Last match wins in SBPL, so the position is the whole assertion:
            // a re-grant before the deny that covers it grants nothing.
            for entry in &overlay.subtract {
                if let Some(at) = text.find(&deny(&entry.path)) {
                    assert!(
                        grant > at || !crate::sandbox::dirs::encloses(&entry.path, own),
                        "'{own}' is re-granted before the deny of '{}'",
                        entry.path
                    );
                }
            }
        }

        let gate = overlay.gate_dir.as_deref().unwrap();
        let gate_at = text
            .find(&allow_ro(gate))
            .unwrap_or_else(|| panic!("no read-only re-grant for the gate:\n{text}"));
        assert!(
            gate_at
                > text
                    .find(&deny("/Users/u/.local/share/friring/gates"))
                    .unwrap(),
            "the gate is re-granted before the deny that covers it"
        );
        // A writable gate is a gate the child opens for itself (ADR-33).
        assert!(
            !text.contains(&allow_rw(gate)),
            "the gate was re-granted writable:\n{text}"
        );
    }

    /// A gate's release file carries the key its launch helper compares against,
    /// so one session reading another's gate learns it. `check_declared_paths`
    /// already refuses a profile that *names* the tree in either mode; the
    /// generated scope has to agree, and under `host-minus-secrets` it did not —
    /// a real-kernel probe found the read allowed.
    #[test]
    fn every_gate_but_this_launchs_own_is_unreadable() {
        let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("~")])
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1")
            .with_gate("/data/gates/s1", "k-1")
            .with_helper_program(HELPER);
        let text = render_profile(&launch);

        let root = dirs::gate_root().unwrap().display().to_string();
        let deny = format!("(subpath {root:?})");
        let deny_at = text
            .find(&deny)
            .unwrap_or_else(|| panic!("the gate tree is not denied:\n{text}"));

        // …and this launch's own gate re-granted after it, read-only. Last match
        // wins, so the position is the assertion.
        let allow = format!("(allow file-read* (subpath (param {PARAM_GATE_DIR:?})))");
        let allow_at = text
            .find(&allow)
            .unwrap_or_else(|| panic!("this launch's own gate is not re-granted:\n{text}"));
        assert!(allow_at > deny_at, "the re-grant comes before the deny");

        // Never writable: the one thing that can open a gate is the host.
        let write = format!("(deny file-write* (subpath (param {PARAM_GATE_DIR:?})))");
        assert!(text.rfind(&write).unwrap() > allow_at, "{text}");
        assert!(
            !text.contains(&format!(
                "(allow file-read* file-write* (subpath (param {PARAM_GATE_DIR:?})))"
            )),
            "the gate was granted writable"
        );
    }

    /// A parent profile granting the **whole home directory** is the case a
    /// naive rule gets wrong: the family's state, the owner's control
    /// directories and every sibling's are all inside it, so the subtraction has
    /// to come after every allow.
    #[test]
    fn a_childs_subtract_set_survives_a_home_wide_grant() {
        let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("~")])
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let overlay = child_narrowing();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "child")
            .with_narrowing(&overlay)
            .with_helper_program(HELPER);
        let text = render_profile(&launch);

        let home_allow = text
            .find(r#"(allow file-read* file-write* (subpath "/Users/u"))"#)
            .expect("the profile grants the whole home");
        for denied in [
            "/Users/u/.codex",
            "/Users/u/.local/share/friring/sandbox",
            "/Users/u/.local/share/friring/signals",
        ] {
            let rule = format!("(deny file-read* file-write* (subpath {denied:?}))");
            let at = text
                .find(&rule)
                .unwrap_or_else(|| panic!("{denied} must be denied:\n{text}"));
            assert!(at > home_allow, "{denied} must be denied after the grant");
        }
    }

    /// The seed targets come back after the subtract set, and the **only**
    /// writable one under the family's state is the credential file.
    #[test]
    fn only_the_credential_seed_is_writable_under_the_family_state() {
        let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("~")])
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let overlay = child_narrowing();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "child")
            .with_narrowing(&overlay)
            .with_helper_program(HELPER);
        let text = render_profile(&launch);

        let subtract = text
            .find(r#"(deny file-read* file-write* (subpath "/Users/u/.codex"))"#)
            .expect("the family's state is denied");
        let credential = r#"(allow file-read* file-write* (subpath "/Users/u/.codex/auth.json"))"#;
        let skills = r#"(allow file-read* (subpath "/Users/u/.codex/skills"))"#;
        for (rule, what) in [(credential, "the credential"), (skills, "the skills tree")] {
            let at = text
                .find(rule)
                .unwrap_or_else(|| panic!("{what} must be re-granted:\n{text}"));
            assert!(
                at > subtract,
                "{what} must be granted after the subtraction"
            );
        }

        // Every *write* allow in the whole profile, enumerated: the only one
        // under the family's state is the credential file.
        let writable_under_family: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("(allow file-read* file-write*"))
            .filter(|line| line.contains("/Users/u/.codex"))
            .collect();
        // Compared on the rule, not the whole line: a seed rule carries a
        // trailing `;; seed: <mode>` comment that says which mode granted it.
        let rules: Vec<&str> = writable_under_family
            .iter()
            .map(|line| line.split("   ;;").next().unwrap_or(line))
            .collect();
        assert_eq!(
            rules,
            [credential],
            "exactly one writable path under the family's state"
        );
    }

    /// An ordinary launch carries no subtract set at all: nothing about the
    /// generated policy changes for a session that is not a bridge child.
    #[test]
    fn an_ordinary_launch_has_no_subtract_section() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/Users/u", "s1");
        let text = render_profile(&launch);
        assert!(!text.contains("subtract set"), "{text}");
        assert!(!text.contains("seed targets"), "{text}");
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

    /// A writable root that **is** a git directory protects its own hooks — and
    /// the rest of what a shared git directory must not hand over.
    ///
    /// The shape a bridge child gets: a profile shares `<repo>/.git` so a linked
    /// worktree can reach the object and ref stores it makes siblings share, and
    /// there `<root>/.git/hooks` names nothing while the real hooks are one
    /// level up. Unprotected, a shared `.git` is arbitrary host command
    /// execution for every child, run by whichever git touches the repository
    /// next — including the operator's, outside the boundary. `config` is the
    /// same channel by another name (`core.fsmonitor`, `core.hooksPath`, an
    /// alias), and `HEAD`/`index` are the leader's own working state.
    #[test]
    fn a_writable_git_directory_protects_the_hooks_beside_it() {
        let mut profile = SandboxProfile::new(
            "shared-git",
            vec![SandboxPath::workspace("/Users/u/dev/app/.git")],
        );
        profile.read_scope = ReadScope::Workspace;
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let text = render_profile(&SandboxLaunch::new(&policy, "/Users/u", "s1"));
        for name in crate::sandbox::backend::PROTECTED_IN_GIT_DIR {
            let rule = format!(r#"(deny file-write* (subpath "/Users/u/dev/app/.git/{name}"))"#);
            assert!(
                text.contains(&rule),
                "a writable git directory left '{name}' writable:\n{text}"
            );
        }
        // The object and ref stores stay writable: a child that cannot write
        // them cannot commit, which is the whole reason the grant exists.
        assert!(!text.contains(r#"(deny file-write* (subpath "/Users/u/dev/app/.git/objects"))"#));
        assert!(!text.contains(r#"(deny file-write* (subpath "/Users/u/dev/app/.git/refs"))"#));
    }

    /// A writable root that is one worktree's **metadata** protects its
    /// redirects.
    ///
    /// ADR-31 grants a bridge child `<repo>/.git/worktrees/<id>` so it can
    /// commit. `gitdir` and `commondir` inside it decide which worktree the
    /// entry belongs to and which directory git reads `config` from — and
    /// friring runs `git` in a child's worktree to reach a verdict, so a child
    /// that could redirect either would be naming a `core.fsmonitor` program the
    /// host then executes.
    #[test]
    fn a_childs_git_metadata_directory_protects_its_redirects() {
        let mut profile = SandboxProfile::new(
            "child-metadata",
            vec![SandboxPath::workspace(
                "/Users/u/dev/app/.git/worktrees/child-a",
            )],
        );
        profile.read_scope = ReadScope::Workspace;
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let text = render_profile(&SandboxLaunch::new(&policy, "/Users/u", "s1"));
        for name in crate::sandbox::backend::PROTECTED_IN_WORKTREE_METADATA {
            let rule = format!(
                r#"(deny file-write* (subpath "/Users/u/dev/app/.git/worktrees/child-a/{name}"))"#
            );
            assert!(
                text.contains(&rule),
                "a child's git metadata directory left '{name}' writable:\n{text}"
            );
        }
        // Its own `HEAD` and `index` are exactly what a commit writes.
        let head = r#"(deny file-write* (subpath "/Users/u/dev/app/.git/worktrees/child-a/HEAD"))"#;
        let index =
            r#"(deny file-write* (subpath "/Users/u/dev/app/.git/worktrees/child-a/index"))"#;
        assert!(!text.contains(head), "a child cannot commit:\n{text}");
        assert!(!text.contains(index), "a child cannot commit:\n{text}");
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

    /// The **agent's own program** is readable under the narrow scope.
    ///
    /// An extension ships its agent as a script under its own home, which is
    /// under no system directory and in no profile path. Left out, the pane
    /// opens and the launch helper dies exec'ing a file it cannot read — which
    /// is the failure the helper's own literal already exists to prevent.
    #[test]
    fn the_workspace_scope_reads_the_agents_own_program() {
        const AGENT: &str = "/Users/u/.config/friring/extensions/x/bin/run.sh";
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("/srv/work")]);
        profile.read_scope = ReadScope::Workspace;
        let policy = profile
            .resolve(SandboxBackendKind::Seatbelt, "/Users/u")
            .unwrap();
        let text = render_profile(
            &SandboxLaunch::new(&policy, "/Users/u", "s1")
                .with_helper_program(HELPER)
                .with_agent_program(AGENT),
        );
        assert!(
            text.contains(&format!("(literal {AGENT:?})")),
            "the agent's own program is unreadable under its own profile:\n{text}"
        );

        // A bare command name resolves on PATH under the system directories the
        // scope already allows, and turning it into a literal would name a path
        // relative to whatever cwd the launch happened to have.
        let text = render_profile(
            &SandboxLaunch::new(&policy, "/Users/u", "s1")
                .with_helper_program(HELPER)
                .with_agent_program("codex"),
        );
        assert!(!text.contains("(literal \"codex\")"), "{text}");
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
            "com.apple.trustd.agent",
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
            .with_helper_program(HELPER)
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
        // `--` keeps an agent flag from being read as a sandbox-exec flag, and
        // friring's own helper runs before the agent so the multiplexer
        // environment is gone by the time the agent starts (ADR-33).
        let end = argv.iter().position(|a| a == "--").unwrap();
        assert_eq!(&argv[end + 1..end + 4], [HELPER, "sandbox", "launch"]);
        let handover = argv.iter().rposition(|a| a == "--").unwrap();
        assert_eq!(&argv[handover + 1..], ["claude", "--resume", "abc"]);

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
