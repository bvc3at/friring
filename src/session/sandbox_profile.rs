//! Sandbox profiles — the pure-data description of *how* a session is isolated.
//!
//! Two types carry the feature's whole vocabulary:
//!
//! - [`SandboxProfile`] is what the user edits and what storage persists: a
//!   name, a backend choice, per-path read/write intent, egress rules, and the
//!   place-only knobs (limits, image). Paths are kept **exactly as written**.
//! - [`SandboxPolicy`] is what a backend consumes at launch: one concrete
//!   backend (never [`SandboxBackendKind::Auto`]), `~` already expanded against
//!   the home directory of the machine the agent will actually run on, the
//!   read-only / read-write sets flattened and ordered, the egress rules
//!   parsed, and the environment to inject.
//!
//! Kept here in `session` (the dependency sink) so the sandbox module, storage,
//! the CLI and the UI all share one definition without crossing the
//! module-isolation rules. Nothing here touches the filesystem, spawns a
//! process or probes a host — availability, argv generation and instance
//! lifecycle belong to the sandbox module. `docs/SANDBOX.md` is the design
//! contract this models; ADR-26 there explains the policy/place split that
//! [`SandboxShape`] encodes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The backend-name prefix a place-backed session is registered (and
/// persisted in `backend_type`) under: a profile named `dev` becomes
/// `sandbox:dev`. Mirrors `ssh:<host>` / `wsl:<distro>`, which already drive
/// restore and reattach; policy-backed sessions keep their existing backend
/// name because their tmux window is outside the sandbox.
pub const SANDBOX_BACKEND_PREFIX: &str = "sandbox:";

/// Whether a backend name refers to a sandbox place (`sandbox:<profile>`).
pub fn is_sandbox_backend(backend_name: &str) -> bool {
    backend_name.starts_with(SANDBOX_BACKEND_PREFIX)
}

/// The profile name inside a `sandbox:<profile>` backend name, or `None` for
/// any other backend.
pub fn sandbox_backend_profile(backend_name: &str) -> Option<&str> {
    backend_name.strip_prefix(SANDBOX_BACKEND_PREFIX)
}

/// Maximum length of a profile name. Matches the cap on every other friring
/// name that reaches a filesystem path.
const MAX_NAME_LEN: usize = 64;

/// Where tmux runs relative to the isolation boundary — the one structural
/// difference between backends (ADR-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxShape {
    /// A kernel policy applied to a process tree: the agent's argv is wrapped
    /// (`sandbox-exec -f … claude …`) and the tmux window stays *outside*, so
    /// discovery, reattach and scrollback are unchanged.
    Policy,
    /// An environment that outlives a command — a container, a VM, a WSL
    /// distro. tmux runs *inside* and is reached through a transport, so every
    /// session sharing the profile shares (and dies with) the place.
    Place,
}

impl SandboxShape {
    /// Storage / UI string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Policy => "policy",
            Self::Place => "place",
        }
    }

    /// Whether memory / CPU caps mean anything. Only a place has a resource
    /// boundary to cap; a policy backend's process is an ordinary host process.
    pub fn supports_limits(self) -> bool {
        matches!(self, Self::Place)
    }

    /// Whether the backend is built from an image or a containerfile.
    pub fn supports_image(self) -> bool {
        matches!(self, Self::Place)
    }

    /// Whether [`ReadScope`] applies. A place has a synthetic filesystem, so
    /// there is no host home to widen or narrow reads over.
    pub fn supports_read_scope(self) -> bool {
        matches!(self, Self::Policy)
    }
}

impl fmt::Display for SandboxShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The isolation technology a profile uses.
///
/// [`Auto`](Self::Auto) is resolved per host by the sandbox module's ladder
/// (see `docs/SANDBOX.md`), which is why it has no [`shape`](Self::shape): it
/// can land on either shape depending on what the host offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxBackendKind {
    /// Pick the first available rung of the host's ladder. The session-creation
    /// step shows what it resolved to, so the choice is never invisible.
    #[default]
    Auto,
    /// macOS `sandbox-exec` with a generated SBPL profile.
    Seatbelt,
    /// Apple's `container` CLI — one lightweight VM per container.
    AppleContainer,
    /// Bubblewrap on Linux and inside WSL2.
    Bwrap,
    Docker,
    Podman,
    /// One cloned WSL distro per profile, reached by the existing
    /// `wsl.exe -d <distro>` transport.
    WslDistro,
}

impl SandboxBackendKind {
    /// Every backend in the order the UI selector offers them, `auto` first.
    pub const ALL: &'static [Self] = &[
        Self::Auto,
        Self::Seatbelt,
        Self::AppleContainer,
        Self::Bwrap,
        Self::Docker,
        Self::Podman,
        Self::WslDistro,
    ];

    /// Storage value and UI label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Seatbelt => "seatbelt",
            Self::AppleContainer => "apple-container",
            Self::Bwrap => "bwrap",
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::WslDistro => "wsl-distro",
        }
    }

    /// Which shape this backend is, or `None` for [`Auto`](Self::Auto), whose
    /// shape is only known once a host has been probed. Callers that need a
    /// definite answer resolve the backend first — [`SandboxProfile::resolve`]
    /// refuses to build a policy from `auto` for exactly this reason.
    pub fn shape(self) -> Option<SandboxShape> {
        match self {
            Self::Auto => None,
            Self::Seatbelt | Self::Bwrap => Some(SandboxShape::Policy),
            Self::AppleContainer | Self::Docker | Self::Podman | Self::WslDistro => {
                Some(SandboxShape::Place)
            }
        }
    }
}

impl fmt::Display for SandboxBackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SandboxBackendKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|b| b.as_str() == key)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|b| b.as_str()).collect();
                format!(
                    "Unknown sandbox backend '{s}' (expected {})",
                    names.join(", ")
                )
            })
    }
}

/// Whether a sandboxed path is readable only, or writable too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PathMode {
    /// Readable, not writable. The default, because widening a boundary should
    /// be a deliberate keystroke.
    #[default]
    #[serde(rename = "ro")]
    ReadOnly,
    /// Readable and writable.
    #[serde(rename = "rw")]
    ReadWrite,
}

impl PathMode {
    /// Both modes in the order the `‹ ro | rw ›` selector cycles them.
    pub const ALL: &'static [Self] = &[Self::ReadOnly, Self::ReadWrite];

    /// Storage value and UI label (`ro` / `rw`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }

    pub fn is_writable(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

impl fmt::Display for PathMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PathMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ro" => Ok(Self::ReadOnly),
            "rw" => Ok(Self::ReadWrite),
            other => Err(format!("Unknown path mode '{other}' (expected ro or rw)")),
        }
    }
}

/// One directory (or file) the sandbox can see, with its intent.
///
/// `path` is stored **exactly as the user wrote it**, `~` included, and is
/// expanded only at launch by [`expand_tilde`]. Storing the expanded form would
/// pin the profile to one machine's `$HOME`: the same profile is used over the
/// SSH and WSL transports, where the home directory is a different string (and
/// on a different OS), and inside a place, whose home is synthetic.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxPath {
    /// As written — `~/dev/friring`, `/srv/repos`, `C:\src\app`.
    pub path: String,
    #[serde(default)]
    pub mode: PathMode,
}

impl SandboxPath {
    /// A read-only path.
    pub fn read_only(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            mode: PathMode::ReadOnly,
        }
    }

    /// A read-write path — what the session-creation step pre-fills the chosen
    /// repositories with, since an agent has to be able to write its workspace.
    pub fn workspace(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            mode: PathMode::ReadWrite,
        }
    }

    /// The stored path with surrounding whitespace and trailing separators
    /// removed, so `~/dev` and `~/dev/` are recognised as one path. Root
    /// (`/`, `C:\`) keeps its separator.
    pub fn normalized(&self) -> String {
        normalize_path(&self.path)
    }

    /// The launch-ready absolute path: [`normalized`](Self::normalized) with
    /// `~` expanded against `home` (the home directory of the machine the agent
    /// runs on, which is not necessarily this one).
    pub fn expanded(&self, home: &str) -> String {
        normalize_path(&expand_tilde(&self.path, home))
    }
}

/// Expand a leading `~` in `raw` against `home`.
///
/// Only the current user's home is expanded: `~`, `~/x` and (on Windows
/// spellings) `~\x`. `~other/x` is returned untouched, because resolving
/// another user's home needs a password database this layer has no business
/// reading. Every other string passes through unchanged, so absolute and
/// relative paths, and paths that merely contain a tilde, are safe.
///
/// `home` is passed in rather than read from the environment on purpose: a
/// profile is expanded against the *target's* home, which over the SSH/WSL
/// transports and inside a place is not this process's `$HOME`.
pub fn expand_tilde(raw: &str, home: &str) -> String {
    let raw = raw.trim();
    let home = home.trim_end_matches(['/', '\\']);
    if raw == "~" {
        return home.to_string();
    }
    match raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        // Keep the separator the user typed: a Windows host and a Unix host
        // both accept the one they wrote.
        Some(rest) => format!("{home}{}{rest}", &raw[1..2]),
        None => raw.to_string(),
    }
}

/// Trim a path and drop trailing separators, keeping a bare root intact.
fn normalize_path(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed.trim_end_matches(['/', '\\']);
    if stripped.is_empty() || stripped.ends_with(':') {
        // `/`, `\` and `C:` are as short as a path gets; stripping further
        // would turn a root into nothing.
        trimmed.to_string()
    } else {
        stripped.to_string()
    }
}

/// Whether `child` is `parent` or lives underneath it.
///
/// Compares the two as strings with separators unified, and **case
/// sensitively** even on Windows: over-matching would silently widen a
/// boundary, while under-matching only costs a profile its place in a UI
/// filter.
fn path_contains(parent: &str, child: &str) -> bool {
    let parent = normalize_path(parent).replace('\\', "/");
    let child = normalize_path(child).replace('\\', "/");
    if parent == child {
        return true;
    }
    let parent_prefix = if parent.ends_with('/') {
        parent
    } else {
        format!("{parent}/")
    };
    child.starts_with(&parent_prefix)
}

/// How much of the network a sandbox may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// No egress at all, and no proxy to ask.
    None,
    /// No *direct* egress; the friring proxy outside the boundary forwards what
    /// the allow list permits (ADR-27). The default: a listed domain is the
    /// only way out, so a new profile starts closed.
    ///
    /// At the kernel level this is configured identically to
    /// [`None`](Self::None) — every backend blocks direct egress either way —
    /// so a backend that has not implemented the proxy yet can treat the two
    /// alike without granting anything.
    #[default]
    Allowlist,
    /// Unrestricted egress, minus any [`network_deny`](SandboxProfile::network_deny)
    /// entries.
    Full,
}

impl NetworkMode {
    /// Every mode in selector order.
    pub const ALL: &'static [Self] = &[Self::None, Self::Allowlist, Self::Full];

    /// Storage value and UI label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Allowlist => "allowlist",
            Self::Full => "full",
        }
    }
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NetworkMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|m| m.as_str() == key)
            .ok_or_else(|| format!("Unknown network mode '{s}' (expected none, allowlist, full)"))
    }
}

/// How much of the *host* filesystem a policy backend exposes beyond the
/// profile's own paths. Meaningless for place backends, which have no host
/// filesystem — [`SandboxProfile::resolve`] narrows it to
/// [`Workspace`](Self::Workspace) there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadScope {
    /// Only the profile's paths are readable.
    Workspace,
    /// The host home is readable except a deny list (SSH keys, cloud
    /// credentials, other agents' credential files), writes still confined to
    /// the profile's read-write paths.
    ///
    /// The default because host passthrough is the default credential story for
    /// policy backends: an agent that cannot read its own configuration
    /// directory fails on first launch, and copying that directory in is
    /// exactly what ADR-28 forbids.
    #[default]
    HostMinusSecrets,
}

impl ReadScope {
    /// Both scopes in selector order.
    pub const ALL: &'static [Self] = &[Self::Workspace, Self::HostMinusSecrets];

    /// Storage value and UI label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::HostMinusSecrets => "host-minus-secrets",
        }
    }
}

impl fmt::Display for ReadScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ReadScope {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let key = s.trim().to_ascii_lowercase();
        Self::ALL
            .iter()
            .copied()
            .find(|r| r.as_str() == key)
            .ok_or_else(|| {
                format!("Unknown read scope '{s}' (expected workspace, host-minus-secrets)")
            })
    }
}

/// One `host[:port]` entry of an allow or deny list.
///
/// **A bare host is exactly that host; `*.host` is that host and its
/// subdomains.** `github.com` matches `github.com` and nothing else;
/// `*.github.com` — or `.github.com`, one spelling of the other — matches
/// `github.com`, `api.github.com` and `a.b.github.com`.
///
/// Bare-is-exact is the reading the rest of the feature promises: the first-use
/// prompt grants "that host on that port only", and a grammar under which a
/// bare name quietly carried the subtree would make every such grant wider than
/// the question asked. A subtree is therefore something a user spells.
///
/// **The wildcard covers the apex.** The deny direction settles it: someone who
/// denies `*.github.com` means "no github.com traffic", and sparing the apex
/// would be a silent hole. Reading one spelling two ways depending on which
/// list it sits in would be worse still.
///
/// The subtree match is on **label boundaries**: the candidate must equal the
/// rule or end with `.` + the rule, so `*.github.com` never covers
/// `evilgithub.com`, `github.com.evil.net` or `github.co`. Getting that
/// boundary wrong is the classic allowlist bypass, so it is tested in both
/// directions.
///
/// Rule and candidate are both reduced to one canonical spelling first (see
/// `canonical_host`): case folded, the root dot and IPv6 brackets dropped,
/// and every address spelling read as the address it denotes.
///
/// An address rule (`127.0.0.1`, `[::1]`) is exact whatever prefix it was
/// written with, compared as a parsed address — so `::1` and `::0001` are one
/// host, `127.1`, `2130706433` and `::ffff:127.0.0.1` are all `127.0.0.1` (they
/// reach one endpoint, so a deny they walked past would be no deny), and
/// `evil.127.0.0.1` is still not "under" `127.0.0.1`. An address has no
/// subtree to grant, so `*.127.0.0.1` is the address rule rather than an error:
/// it is what the writer can only have meant, and the wider reading of the two.
///
/// A rule without a port matches every port; a rule with one matches only that
/// port, so `github.com:443` permits HTTPS and nothing else.
///
/// Hosts are ASCII: an international name must be written in punycode
/// (`xn--bcher-kva.example`), which is how it is spelled on the wire.
/// Canonicalising a U-label correctly needs Unicode tables this crate does not
/// carry, so one is refused rather than guessed at — in a stored rule here, and
/// in a request the proxy refuses outright.
///
/// The egress proxy enforces this vocabulary at connection time with its own
/// matcher (`proxy::HostRule`) — the two cannot share a type, because the proxy
/// is a leaf in the architecture allowlist. `tests/egress_matcher_conformance.rs`
/// runs both over one table of stored spellings and fails when either drifts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainRule {
    /// The canonical **pattern**: a lower-cased host for an exact rule, `*.`
    /// and that host for a subtree, or an address in the one spelling every
    /// spelling of it reduces to. Never a trailing dot, never `.host` (which
    /// canonicalises to `*.host`).
    ///
    /// The prefix lives in the pattern rather than in a flag beside it so a
    /// rule has one source of truth — the same text the profile stores and the
    /// same text the proxy is handed. A separate flag could contradict the
    /// host, and [`matches_host`](Self::matches_host) would have to pick a
    /// winner.
    pub host: String,
    /// Port this rule is scoped to, or `None` for any port.
    pub port: Option<u16>,
}

impl DomainRule {
    /// Parse a `host[:port]` entry, rejecting the shapes users reach for that
    /// would otherwise match nothing (a URL, a path, a bare wildcard).
    /// IPv6 literals must be bracketed (`[::1]`, `[::1]:443`).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let entry = raw.trim();
        if entry.is_empty() {
            return Err("Domain cannot be empty".to_string());
        }
        if let Some((scheme, _)) = entry.split_once("://") {
            return Err(format!(
                "Domain '{entry}' includes a scheme; write the host alone \
                 (drop '{scheme}://')"
            ));
        }
        if entry.contains('/') {
            return Err(format!(
                "Domain '{entry}' includes a path; the allow list matches hosts, not URLs"
            ));
        }
        if entry.contains(char::is_whitespace) || entry.contains('@') {
            return Err(format!("Domain '{entry}' is not a valid host name"));
        }

        let (host, port) = split_host_port(entry)?;
        let fault = |fault: HostFault| format!("Domain '{entry}' {}", fault.detail());
        let (bare, subtree) = split_subtree(host).map_err(fault)?;
        let host = match canonical_host(bare).map_err(fault)? {
            // An address has no subtree, so the prefix is dropped rather than
            // stored — one rule per address, however it was written.
            CanonicalHost::Address(address) => address.to_string(),
            CanonicalHost::Domain(domain) if subtree => format!("{SUBTREE_PREFIX}{domain}"),
            CanonicalHost::Domain(domain) => domain,
        };
        Ok(Self { host, port })
    }

    /// The exact-host rule for one **request** host, scoped to `port`.
    ///
    /// This is what a first-use prompt grants and what a refused destination is
    /// remembered under. A request host is never a pattern: `*.` and a leading
    /// `.` are *rule* grammar, and a client that spells its destination
    /// `.github.com` is asking for `github.com` — which is what the resolver
    /// makes of it, and what the proxy dials. Reading it as a rule instead
    /// would answer a prompt that offered one host by storing its whole
    /// subtree.
    ///
    /// Unlike [`parse`](Self::parse) this takes the host alone, so an
    /// unbracketed IPv6 literal (how a SOCKS5 client names one) needs no
    /// brackets added first.
    ///
    /// # Errors
    ///
    /// The host cannot be canonicalised — a U-label, or a shape no request host
    /// can carry.
    pub fn exact(host: &str, port: Option<u16>) -> Result<Self, String> {
        let host = match canonical_host(host)
            .map_err(|fault| format!("Host '{host}' {}", fault.detail()))?
        {
            CanonicalHost::Address(address) => address.to_string(),
            CanonicalHost::Domain(domain) => domain,
        };
        Ok(Self { host, port })
    }

    /// Whether this rule covers `host` on `port`.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        if self.port.is_some_and(|p| p != port) {
            return false;
        }
        self.matches_host(host)
    }

    /// Whether this rule covers `host` on any port — the port-blind half of
    /// [`matches`](Self::matches), for UI questions like "is this domain
    /// covered at all?".
    ///
    /// The pattern is re-read here rather than only at [`parse`](Self::parse),
    /// because [`host`](Self::host) is a public field a caller (or a
    /// deserialised profile) can fill with any spelling, and a rule whose
    /// semantics depended on how it was built would be a rule that means one
    /// thing in the editor and another at launch. A rule that does not
    /// canonicalise covers nothing — [`SandboxProfile::validate`] is what keeps
    /// one from being stored.
    pub fn matches_host(&self, host: &str) -> bool {
        let Ok((pattern, subtree)) = split_subtree(self.host.trim()) else {
            return false;
        };
        let (Ok(rule), Ok(candidate)) = (canonical_host(pattern), canonical_host(host)) else {
            return false;
        };
        match (rule, candidate) {
            // An address rule is exact, and exact on the *parsed* address:
            // suffix logic over digits would put `evil.127.0.0.1` under
            // `127.0.0.1`, and a textual comparison would let `::0001` slip
            // past a deny rule on `::1`. A subtree prefix changes nothing —
            // there is nothing under an address.
            (CanonicalHost::Address(rule), CanonicalHost::Address(candidate)) => rule == candidate,
            (CanonicalHost::Domain(rule), CanonicalHost::Domain(candidate)) => {
                candidate == rule
                    || (subtree
                        && candidate
                            .strip_suffix(&rule)
                            .is_some_and(|prefix| prefix.ends_with('.')))
            }
            // A name rule never covers an address and an address rule never
            // covers a name: `localhost` is a name, matched as one.
            _ => false,
        }
    }
}

impl fmt::Display for DomainRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Re-bracket an IPv6 literal whether or not it carries a port, so what
        // is printed parses back to the same rule instead of tripping the
        // "several ':'" rejection.
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(port) => write!(f, "{host}:{port}"),
            None => f.write_str(&host),
        }
    }
}

impl FromStr for DomainRule {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// The prefix that widens a rule from one host to that host and its
/// subdomains. `.host` is the same pattern spelled without the star and
/// canonicalises to this form, so a stored rule has one spelling.
const SUBTREE_PREFIX: &str = "*.";

/// Split a pattern into the host it names and whether it covers that host's
/// subtree.
///
/// A second leading dot is not a third spelling of the same thing: it is an
/// empty label, which no name has and no resolver answers for, so `..github.com`
/// is refused here rather than folded away by [`canonical_host`] — whose
/// leading-dot rule exists for *request* hosts, where the resolvers that accept
/// the spelling resolve the apex.
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

/// Split `host[:port]`, handling the bracketed IPv6 form. An unbracketed entry
/// with several colons is rejected rather than guessed at.
///
/// The brackets stay on the host: they are address syntax, and
/// [`canonical_host`] is where that is enforced rather than here.
fn split_host_port(entry: &str) -> Result<(&str, Option<u16>), String> {
    if let Some(rest) = entry.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return Err(format!("Domain '{entry}' is missing its closing ']'"));
        };
        let port = match tail {
            "" => None,
            // The `:` is required: reading `[::1]443` as port 443 invents a
            // scoping the writer did not spell, and the proxy's parser refuses
            // that shape outright.
            _ => {
                let digits = tail.strip_prefix(':').ok_or_else(|| {
                    format!("Domain '{entry}' has trailing text after ']': '{tail}'")
                })?;
                Some(parse_port(digits, entry)?)
            }
        };
        return Ok((&entry[..host.len() + 2], port));
    }
    match entry.split_once(':') {
        None => Ok((entry, None)),
        Some((host, port)) if !port.contains(':') => Ok((host, Some(parse_port(port, entry)?))),
        Some(_) => Err(format!(
            "Domain '{entry}' has several ':'; bracket an IPv6 literal as [::1]:443"
        )),
    }
}

fn parse_port(raw: &str, entry: &str) -> Result<u16, String> {
    raw.parse::<u16>()
        .map_err(|_| format!("Domain '{entry}' has an invalid port '{raw}' (expected 1-65535)"))
        .and_then(|p| {
            if p == 0 {
                Err(format!("Domain '{entry}' has port 0"))
            } else {
                Ok(p)
            }
        })
}

/// The DNS limits (RFC 1035 §2.3.4), which double as the reason a longer
/// string is a typo rather than a host: nothing on the wire can be spelled that
/// way, so the rule would match nothing — and a deny rule that matches nothing
/// is a hole.
const MAX_HOST_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;

/// A host reduced to the one spelling a rule and a candidate are compared in.
///
/// The two variants never match each other, which is what keeps `localhost` a
/// name and `127.0.0.1` an address.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CanonicalHost {
    /// Compared as a parsed address, so every spelling reaching one endpoint is
    /// one host.
    Address(IpAddr),
    /// Lowercase, no trailing root dot, no brackets, ASCII only.
    Domain(String),
}

/// Canonicalise one host, as a rule stores it or as a request carries it.
///
/// A host is read the way the *resolver* reads it, because that is what the
/// connection will do: `127.1`, `2130706433` and `::ffff:127.0.0.1` are all
/// `127.0.0.1`, so a deny rule on the address cannot be walked past by
/// spelling it differently. Case, the root dot and IPv6 brackets are
/// presentation and go the same way.
///
/// **A host containing any non-ASCII byte is refused.** Canonicalising an
/// international name correctly means UTS-46 — case folding, NFC
/// normalisation, and tables of disallowed and deviation characters — and only
/// the punycode *encoding* half of that is table-free. Encoding without mapping
/// would be worse than refusing: `café.example` with a combining accent and the
/// same name pre-composed encode to different A-labels, so a deny rule on the
/// punycode form would still be dodged while looking closed. The A-label is
/// what travels on the wire, so that is what a rule is written as.
///
/// This must stay behaviourally identical to the proxy's `CanonicalHost`, which
/// the two modules cannot share because the proxy is a leaf in the architecture
/// allowlist. `tests/egress_matcher_conformance.rs` runs both over one table of
/// spellings and fails when either side drifts.
fn canonical_host(raw: &str) -> Result<CanonicalHost, HostFault> {
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
    // An empty *leading* label is presentation on a request host: the resolvers
    // that accept `.github.com` resolve the apex, so that is what it is compared
    // (and dialled) as. On a *rule* the same spelling means the subtree and is
    // taken off by `split_subtree` before this, so the two readings never meet.
    let host = host.strip_prefix('.').unwrap_or(host);
    if host.is_empty() {
        return Err(HostFault::Empty);
    }
    // Address readings come first, exactly as they do in the resolver.
    if let Some(address) = parse_address(host) {
        return Ok(CanonicalHost::Address(address));
    }
    if bracketed {
        // Brackets are address syntax. `[github.com]` is not a host any client
        // writes, and reading it as a name would be a second spelling of one
        // rule.
        return Err(HostFault::Brackets);
    }
    validate_domain(host)?;
    Ok(CanonicalHost::Domain(host.to_ascii_lowercase()))
}

/// Why a host could not be canonicalised. [`HostFault::detail`] is a clause
/// written to follow `Domain '<entry>'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostFault {
    Empty,
    TooLong,
    LabelTooLong,
    EmptyLabel,
    HyphenEdge,
    /// A U-label — see [`canonical_host`] on the IDNA subset.
    International,
    Character,
    Brackets,
}

impl HostFault {
    fn detail(self) -> &'static str {
        match self {
            Self::Empty => "has no host",
            Self::TooLong => "is too long (max 253 characters)",
            Self::LabelTooLong => "has a label longer than 63 characters",
            Self::EmptyLabel => "has an empty label",
            Self::HyphenEdge => "has a label edged with '-'",
            Self::International => {
                "is an international name and must be written in punycode, e.g. \
                 'xn--bcher-kva.example'"
            }
            Self::Character => "has an invalid character",
            Self::Brackets => "has brackets around something that is not an address",
        }
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
/// written as a rule can never be *allowed* either. `_` stays legal because
/// service names use it.
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

/// What the egress filter decided about one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressDecision {
    /// Permitted: [`NetworkMode::Full`], or an allow rule matched.
    Allow,
    /// Refused by a deny rule, by [`NetworkMode::None`], or because the host
    /// has no single spelling to compare. Final — denies beat allows, so this
    /// never becomes a prompt.
    Denied,
    /// Under [`NetworkMode::Allowlist`] and matched by nothing. Refused, but
    /// this is the outcome
    /// [`prompt_new_domains`](SandboxProfile::prompt_new_domains) turns into a
    /// question whose answer is written back to the profile.
    Unlisted,
}

impl EgressDecision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// One live **place**: a container, a VM, a distro clone.
///
/// The pure record of one, which is why it lives here rather than in
/// [`crate::sandbox`]: the backend that created it, the launch path that
/// reports it and the storage layer that persists it all name it, and only one
/// of those three may reference the others. `storage` keeps its own row type
/// with the timestamps it owns.
///
/// Policy backends never produce one — their boundary is a kernel policy over a
/// process tree, with nothing that outlives the launch (ADR-26).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInstance {
    /// The profile the place was built for.
    pub profile: String,
    /// Which engine created it. Never a policy backend, and never
    /// [`SandboxBackendKind::Auto`] — the ladder has resolved by the time a
    /// place exists.
    pub engine: SandboxBackendKind,
    /// The engine's own handle: a container id, a distro name.
    pub external_id: String,
    /// Backend-defined lifecycle state. Free text on purpose: the vocabulary
    /// belongs to whichever backend wrote it, so a place backend friring gains
    /// later needs no migration to describe itself.
    pub state: String,
}

/// A named, user-edited isolation recipe.
///
/// Identity is [`name`](Self::name): it is the storage primary key, the label
/// shown in every screen, and the `sandbox:<name>` backend name a place-backed
/// session is registered under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxProfile {
    /// Unique, and restricted to a portable character set because it becomes
    /// part of a backend name, a container / distro name and a directory.
    pub name: String,
    pub backend: SandboxBackendKind,
    /// What the sandbox can see, in the order the user listed it. Never empty
    /// in a valid profile.
    pub paths: Vec<SandboxPath>,
    pub network_mode: NetworkMode,
    /// `host[:port]` entries permitted under [`NetworkMode::Allowlist`], stored
    /// as written and parsed into [`DomainRule`]s at launch.
    pub network_allow: Vec<String>,
    /// `host[:port]` entries refused in every mode. Denies beat allows.
    pub network_deny: Vec<String>,
    /// Ask on first use of an unlisted domain and remember the answer, instead
    /// of refusing silently.
    pub prompt_new_domains: bool,
    /// Policy backends only; narrowed to [`ReadScope::Workspace`] for a place.
    pub read_scope: ReadScope,
    /// Place backends only. `None` = uncapped.
    pub memory_mb: Option<u32>,
    /// Place backends only. `None` = uncapped.
    pub cpus: Option<u32>,
    /// Place backends: the image to run. Mutually exclusive with
    /// [`containerfile`](Self::containerfile).
    pub image: Option<String>,
    /// Place backends: build source, an alternative to [`image`](Self::image).
    pub containerfile: Option<String>,
    /// Whether the agent may run a specific command outside the boundary. Off
    /// by default: the escape hatch exists, but visibly and per profile.
    pub allow_unsandboxed_fallback: bool,
    /// Unix millis, set by storage on insert.
    pub created_at: u64,
    /// Unix millis, set by storage on every save.
    pub updated_at: u64,
}

impl Default for SandboxProfile {
    /// The editor's starting state: no name and no paths yet — both of which
    /// [`validate`](Self::validate) rejects, so a half-filled form cannot be
    /// saved — with every other knob at its safe setting (deny-by-default
    /// egress, read-only paths, no escape hatch). Use
    /// [`SandboxProfile::new`] for a profile that is valid on the spot.
    fn default() -> Self {
        Self {
            name: String::new(),
            backend: SandboxBackendKind::Auto,
            paths: Vec::new(),
            network_mode: NetworkMode::Allowlist,
            network_allow: Vec::new(),
            network_deny: Vec::new(),
            prompt_new_domains: true,
            read_scope: ReadScope::HostMinusSecrets,
            memory_mb: None,
            cpus: None,
            image: None,
            containerfile: None,
            allow_unsandboxed_fallback: false,
            created_at: 0,
            updated_at: 0,
        }
    }
}

impl SandboxProfile {
    /// A named profile over `paths`, everything else defaulted.
    pub fn new(name: impl Into<String>, paths: Vec<SandboxPath>) -> Self {
        Self {
            name: name.into(),
            paths,
            ..Self::default()
        }
    }

    /// The backend name a place-backed session using this profile registers
    /// under (`sandbox:<name>`). Policy-backed sessions keep the backend name
    /// they would have had without a sandbox.
    pub fn backend_name(&self) -> String {
        format!("{SANDBOX_BACKEND_PREFIX}{}", self.name)
    }

    /// Whether `dir` is inside one of the profile's paths, once `~` is expanded
    /// against `home`. Drives the session-creation step, which offers the
    /// profiles that cover the directories the user picked and warns about the
    /// ones that do not.
    pub fn covers(&self, dir: &str, home: &str) -> bool {
        self.paths
            .iter()
            .any(|p| path_contains(&p.expanded(home), dir))
    }

    /// Everything wrong with this profile on its own terms, as one sentence for
    /// the error toast — the profile editor has no inline error widget.
    ///
    /// Name uniqueness needs the other profiles, so it lives in
    /// [`validate_unique`](Self::validate_unique).
    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.name)?;
        self.validate_paths()?;
        self.validate_domains()?;
        self.validate_backend_fields()
    }

    /// [`validate`](Self::validate) plus a name-collision check against
    /// `others` — the names of every *other* stored profile. Exclude the
    /// profile being edited, so re-saving one under its own name is not a
    /// collision.
    pub fn validate_unique(&self, others: &[String]) -> Result<(), String> {
        self.validate()?;
        let name = self.name.trim().to_ascii_lowercase();
        // Case-insensitive: a place backend turns the name into a container or
        // distro name, and two profiles differing only in case would collide
        // there while looking distinct here.
        if others.iter().any(|o| o.trim().to_ascii_lowercase() == name) {
            return Err(format!(
                "A sandbox profile named '{}' already exists",
                self.name.trim()
            ));
        }
        Ok(())
    }

    fn validate_paths(&self) -> Result<(), String> {
        if self.paths.is_empty() {
            return Err("Add at least one path the sandbox can see".to_string());
        }
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for p in &self.paths {
            let normalized = p.normalized();
            if normalized.is_empty() {
                return Err("Path cannot be empty".to_string());
            }
            // A control character does not survive the trip to the backend:
            // shell escaping rewrites newlines, and an SBPL/bwrap argument
            // built from the rewritten string names a *different* directory —
            // either failing the launch or authorising the wrong path.
            if let Some(bad) = normalized.chars().find(|c| c.is_control()) {
                return Err(format!(
                    "Path '{}' contains a control character ({}) and cannot be passed to \
                     the sandbox intact",
                    normalized.escape_debug(),
                    bad.escape_debug()
                ));
            }
            if !seen.insert(normalized.clone()) {
                return Err(format!("Path '{normalized}' is listed twice"));
            }
        }
        Ok(())
    }

    fn validate_domains(&self) -> Result<(), String> {
        for entry in self.network_allow.iter().chain(&self.network_deny) {
            DomainRule::parse(entry)?;
        }
        Ok(())
    }

    /// Reject knobs the chosen backend cannot honour. `auto` is exempt: its
    /// shape is only known after a host probe, and the session-creation step
    /// reports an unavailable capability once it has resolved one.
    fn validate_backend_fields(&self) -> Result<(), String> {
        if self.memory_mb == Some(0) || self.cpus == Some(0) {
            return Err("Memory and CPU limits must be greater than 0".to_string());
        }
        if self.image.is_some() && self.containerfile.is_some() {
            return Err("Set an image or a containerfile, not both".to_string());
        }
        let Some(shape) = self.backend.shape() else {
            return Ok(());
        };
        if !shape.supports_limits() && (self.memory_mb.is_some() || self.cpus.is_some()) {
            return Err(format!(
                "Backend '{}' cannot cap memory or CPU; it applies a policy to a host \
                 process. Use a container backend or clear the limits",
                self.backend
            ));
        }
        if !shape.supports_image() && (self.image.is_some() || self.containerfile.is_some()) {
            return Err(format!(
                "Backend '{}' runs the host toolchain and has no image; clear the image \
                 and containerfile",
                self.backend
            ));
        }
        Ok(())
    }

    /// Freeze this profile into the launch-ready [`SandboxPolicy`] a backend
    /// consumes: `backend` resolved (the ladder ran already), `~` expanded
    /// against `home`, paths ordered and de-duplicated, egress rules parsed.
    ///
    /// Fails on an invalid profile and on [`SandboxBackendKind::Auto`] — a
    /// policy names the backend that will actually run, so leaving `auto` in it
    /// would push the ladder down into every backend.
    pub fn resolve(
        &self,
        backend: SandboxBackendKind,
        home: &str,
    ) -> Result<SandboxPolicy, String> {
        self.validate()?;
        let Some(shape) = backend.shape() else {
            return Err(
                "Resolve 'auto' to a concrete backend before building a sandbox policy".to_string(),
            );
        };

        // Sorted sets: an ancestor always precedes its descendants
        // lexicographically, which is the order bind-mount backends need for a
        // nested override (a read-only `.git/hooks` inside a writable
        // workspace) to win.
        let mut rw: BTreeSet<String> = BTreeSet::new();
        let mut ro: BTreeSet<String> = BTreeSet::new();
        for p in &self.paths {
            let expanded = p.expanded(home);
            if p.mode.is_writable() {
                rw.insert(expanded);
            } else {
                ro.insert(expanded);
            }
        }
        // A path listed both ways is already a validation error; if one ever
        // reaches here, the wider grant must not be silently downgraded to a
        // read-only bind the backend would then fail to write through.
        ro.retain(|p| !rw.contains(p));

        let parse_all = |entries: &[String]| -> Result<Vec<DomainRule>, String> {
            entries.iter().map(|e| DomainRule::parse(e)).collect()
        };

        Ok(SandboxPolicy {
            profile: self.name.trim().to_string(),
            backend,
            shape,
            ro_paths: ro.into_iter().collect(),
            rw_paths: rw.into_iter().collect(),
            read_scope: if shape.supports_read_scope() {
                self.read_scope
            } else {
                ReadScope::Workspace
            },
            network: self.network_mode,
            allow: parse_all(&self.network_allow)?,
            deny: parse_all(&self.network_deny)?,
            prompt_new_domains: self.prompt_new_domains,
            memory_mb: shape.supports_limits().then_some(self.memory_mb).flatten(),
            cpus: shape.supports_limits().then_some(self.cpus).flatten(),
            image: shape.supports_image().then(|| self.image.clone()).flatten(),
            containerfile: shape
                .supports_image()
                .then(|| self.containerfile.clone())
                .flatten(),
            allow_unsandboxed_fallback: self.allow_unsandboxed_fallback,
            env: BTreeMap::new(),
        })
    }
}

/// Reject a profile name that would break a backend name, a container name or
/// a directory. Deliberately narrower than the rules those each impose, so one
/// check covers all of them.
fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Name cannot be empty".to_string());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!("Name too long (max {MAX_NAME_LEN} characters)"));
    }
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return Err("Name must start with a letter or a digit".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("Name may only contain letters, digits, '-', '_' and '.'".to_string());
    }
    Ok(())
}

/// A profile frozen for one launch: one concrete backend, absolute paths, and
/// the environment to inject.
///
/// This is the value every backend reads — the policy backends turn it into
/// argv (an SBPL profile, a `bwrap` command line), the place backends turn it
/// into mounts and a network configuration, and the egress proxy answers
/// [`decide_egress`](Self::decide_egress) with it. Pure data, so it crosses
/// between the sandbox module, storage and the CLI freely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    /// The profile this came from, for labels and for naming instances.
    pub profile: String,
    /// Never [`SandboxBackendKind::Auto`].
    pub backend: SandboxBackendKind,
    /// [`backend`](Self::backend)'s shape, resolved once so callers need not
    /// unwrap an `Option`.
    pub shape: SandboxShape,
    /// Read-only paths, absolute, sorted, de-duplicated.
    pub ro_paths: Vec<String>,
    /// Read-write paths, absolute, sorted, de-duplicated, disjoint from
    /// [`ro_paths`](Self::ro_paths).
    pub rw_paths: Vec<String>,
    /// Narrowed to [`ReadScope::Workspace`] for a place backend.
    pub read_scope: ReadScope,
    pub network: NetworkMode,
    /// Parsed [`network_allow`](SandboxProfile::network_allow).
    pub allow: Vec<DomainRule>,
    /// Parsed [`network_deny`](SandboxProfile::network_deny).
    pub deny: Vec<DomainRule>,
    pub prompt_new_domains: bool,
    /// `None` for a policy backend, which cannot cap resources.
    pub memory_mb: Option<u32>,
    /// `None` for a policy backend, which cannot cap resources.
    pub cpus: Option<u32>,
    /// `None` for a policy backend, which has no image.
    pub image: Option<String>,
    /// `None` for a policy backend, which has no image.
    pub containerfile: Option<String>,
    pub allow_unsandboxed_fallback: bool,
    /// Environment to inject inside the boundary — proxy variables, the agent's
    /// relocated config directory, and the session identity that has to be
    /// forwarded inward because tmux sets it *outside*. Ordered, so generated
    /// argv is byte-stable across runs.
    pub env: BTreeMap<String, String>,
}

impl SandboxPolicy {
    /// Add or replace one environment entry, returning the value it displaced.
    pub fn insert_env(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Option<String> {
        self.env.insert(key.into(), value.into())
    }

    /// Whether the sandbox has any way out at all. Policy and place backends
    /// alike block direct egress, so this is really "is the proxy offered".
    pub fn has_egress(&self) -> bool {
        !matches!(self.network, NetworkMode::None)
    }

    /// The verdict on one outbound connection.
    ///
    /// Denies are checked first and in every mode, so a deny entry narrows
    /// [`NetworkMode::Full`] as well as an allow list. Under
    /// [`NetworkMode::Allowlist`] anything unmatched is
    /// [`EgressDecision::Unlisted`] rather than
    /// [`Denied`](EgressDecision::Denied), which is what
    /// [`should_prompt`](Self::should_prompt) keys off.
    ///
    /// A host that does not canonicalise is refused before any rule is
    /// consulted, in every mode — it is exactly the host a deny rule would be
    /// dodged with, and no rule can honestly be compared against it. The proxy
    /// answers the same request with `DenyReason::UnsupportedHost`; this is the
    /// same verdict, reached where the UI and the CLI can preview it.
    pub fn decide_egress(&self, host: &str, port: u16) -> EgressDecision {
        if canonical_host(host).is_err() {
            return EgressDecision::Denied;
        }
        if self.deny.iter().any(|r| r.matches(host, port)) {
            return EgressDecision::Denied;
        }
        match self.network {
            NetworkMode::None => EgressDecision::Denied,
            NetworkMode::Full => EgressDecision::Allow,
            NetworkMode::Allowlist => {
                if self.allow.iter().any(|r| r.matches(host, port)) {
                    EgressDecision::Allow
                } else {
                    EgressDecision::Unlisted
                }
            }
        }
    }

    /// Whether a denial should raise the "allow this domain?" modal instead of
    /// failing silently. Only an unlisted domain is ever asked about: an
    /// explicit deny is the user's own earlier answer.
    pub fn should_prompt(&self, decision: EgressDecision) -> bool {
        self.prompt_new_domains && decision == EgressDecision::Unlisted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> SandboxProfile {
        SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")])
    }

    #[test]
    fn backend_kind_round_trips_through_string() {
        for &b in SandboxBackendKind::ALL {
            assert_eq!(SandboxBackendKind::from_str(b.as_str()).unwrap(), b);
            assert_eq!(b.to_string(), b.as_str());
            // Storage and hand-edited imports are case- and space-tolerant.
            let loose = format!("  {}  ", b.as_str().to_ascii_uppercase());
            assert_eq!(SandboxBackendKind::from_str(&loose).unwrap(), b);
            // serde agrees with as_str, so a JSON export and a TEXT column
            // never disagree about what a backend is called.
            let json = serde_json::to_string(&b).unwrap();
            assert_eq!(json, format!("\"{}\"", b.as_str()));
        }
        assert!(SandboxBackendKind::from_str("firejail").is_err());
    }

    #[test]
    fn other_enums_round_trip_through_string() {
        for &m in NetworkMode::ALL {
            assert_eq!(NetworkMode::from_str(m.as_str()).unwrap(), m);
            assert_eq!(
                serde_json::to_string(&m).unwrap(),
                format!("\"{}\"", m.as_str())
            );
        }
        for &r in ReadScope::ALL {
            assert_eq!(ReadScope::from_str(r.as_str()).unwrap(), r);
            assert_eq!(
                serde_json::to_string(&r).unwrap(),
                format!("\"{}\"", r.as_str())
            );
        }
        for &p in PathMode::ALL {
            assert_eq!(PathMode::from_str(p.as_str()).unwrap(), p);
            assert_eq!(
                serde_json::to_string(&p).unwrap(),
                format!("\"{}\"", p.as_str())
            );
        }
        assert!(NetworkMode::from_str("some").is_err());
        assert!(ReadScope::from_str("host").is_err());
        assert!(PathMode::from_str("rwx").is_err());
    }

    #[test]
    fn shape_splits_policy_from_place() {
        assert_eq!(SandboxBackendKind::Auto.shape(), None);
        assert_eq!(
            SandboxBackendKind::Seatbelt.shape(),
            Some(SandboxShape::Policy)
        );
        assert_eq!(
            SandboxBackendKind::Bwrap.shape(),
            Some(SandboxShape::Policy)
        );
        for b in [
            SandboxBackendKind::Docker,
            SandboxBackendKind::Podman,
            SandboxBackendKind::AppleContainer,
            SandboxBackendKind::WslDistro,
        ] {
            assert_eq!(b.shape(), Some(SandboxShape::Place));
        }
        assert!(SandboxShape::Place.supports_limits());
        assert!(!SandboxShape::Policy.supports_limits());
        assert!(SandboxShape::Policy.supports_read_scope());
        assert!(!SandboxShape::Place.supports_read_scope());
    }

    #[test]
    fn backend_name_prefixes_with_sandbox() {
        assert_eq!(profile().backend_name(), "sandbox:dev");
        assert!(is_sandbox_backend("sandbox:dev"));
        assert!(!is_sandbox_backend("ssh:devbox"));
        assert_eq!(sandbox_backend_profile("sandbox:dev"), Some("dev"));
        assert_eq!(sandbox_backend_profile("local-tmux"), None);
    }

    #[test]
    fn domain_rule_parses_host_and_port() {
        let bare = DomainRule::parse("api.anthropic.com").unwrap();
        assert_eq!(bare.host, "api.anthropic.com");
        assert_eq!(bare.port, None);
        assert_eq!(bare.to_string(), "api.anthropic.com");

        let scoped = DomainRule::parse("GitHub.com:443").unwrap();
        assert_eq!(scoped.host, "github.com");
        assert_eq!(scoped.port, Some(443));
        assert_eq!(scoped.to_string(), "github.com:443");

        // The FQDN root dot is presentation; the two wildcard spellings are one
        // pattern, and a *different* one from the bare host.
        assert_eq!(DomainRule::parse("github.com.").unwrap().host, "github.com");
        for spelling in ["*.github.com", ".github.com"] {
            let rule = DomainRule::parse(spelling).unwrap();
            assert_eq!(rule.host, "*.github.com", "'{spelling}'");
            // What it renders as is what it parses back to, so a subtree rule
            // survives the trip through the profile and on to the proxy.
            assert_eq!(rule.to_string(), "*.github.com");
            assert_eq!(DomainRule::parse(&rule.to_string()).unwrap(), rule);
        }

        assert_eq!(DomainRule::parse("[::1]").unwrap().host, "::1");
        let v6 = DomainRule::parse("[::1]:8080").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", Some(8080)));
        // Rendering an IPv6 rule re-brackets it, so what the UI prints is what
        // the parser accepts back.
        assert_eq!(v6.to_string(), "[::1]:8080");
        assert_eq!(DomainRule::parse(&v6.to_string()).unwrap(), v6);
    }

    /// A bare host is that host and nothing else. The subtree is a wildcard the
    /// user spells, because the first-use prompt grants a bare rule and
    /// promises "that host only" — a bare name that carried the subtree would
    /// make every one of those grants wider than the question. The egress
    /// proxy's matcher reads it the same way; see
    /// `tests/egress_matcher_conformance.rs`.
    #[test]
    fn a_bare_host_is_exact_and_the_wildcard_is_the_subtree() {
        let bare = DomainRule::parse("github.com").unwrap();
        assert!(bare.matches("github.com", 443));
        assert!(!bare.matches("api.github.com", 443));
        assert!(!bare.matches("deep.api.github.com", 443));

        // `*.x` and `.x` are one pattern, and it covers the apex: a deny on
        // `*.github.com` that spared `github.com` would be a silent hole.
        for spelling in ["*.github.com", ".github.com"] {
            let rule = DomainRule::parse(spelling).unwrap();
            assert_ne!(rule, bare, "'{spelling}' is wider than the bare host");
            assert!(
                rule.matches("github.com", 443),
                "'{spelling}' covers the apex"
            );
            assert!(rule.matches("api.github.com", 443), "'{spelling}'");
            assert!(rule.matches("deep.api.github.com", 443), "'{spelling}'");
            // The label boundary holds for the wildcard too.
            assert!(!rule.matches("evilgithub.com", 443), "'{spelling}'");
            assert!(!rule.matches("github.com.evil.net", 443), "'{spelling}'");
            assert!(!rule.matches("github.co", 443), "'{spelling}'");
        }
    }

    /// The rule a first-use prompt stores: whatever spelling the *sandbox* chose
    /// for its destination, the grant is one host on one port. A client that
    /// wrote `.github.com` — which resolves to the apex — must not be answered
    /// with the wildcard rule that same text would mean in a profile.
    #[test]
    fn an_exact_rule_is_never_widened_by_the_clients_spelling() {
        for spelling in ["github.com", "GitHub.COM", "github.com.", ".github.com"] {
            let rule = DomainRule::exact(spelling, Some(443)).unwrap();
            assert_eq!(rule.to_string(), "github.com:443", "'{spelling}'");
            assert!(!rule.matches("api.github.com", 443), "'{spelling}'");
        }
        // A SOCKS5 client names an IPv6 destination unbracketed and a CONNECT
        // line brackets it; both are the address, in the one spelling a profile
        // stores.
        for spelling in ["2001:db8::1", "[2001:db8::1]", "[2001:DB8:0:0:0:0:0:1]"] {
            let rule = DomainRule::exact(spelling, Some(443)).unwrap();
            assert_eq!(rule.to_string(), "[2001:db8::1]:443", "'{spelling}'");
        }
        assert_eq!(DomainRule::exact("127.1", None).unwrap().host, "127.0.0.1");
        // A host with no canonical spelling has no rule either.
        for bad in ["b\u{fc}cher.example", "", "*.github.com", "github..com"] {
            assert!(DomainRule::exact(bad, Some(443)).is_err(), "'{bad}'");
        }
    }

    /// Address rules are exact, on the parsed address: no pseudo-subdomain, and
    /// no alternative spelling that slips past a deny entry.
    #[test]
    fn address_rules_match_the_address_not_a_suffix() {
        let v4 = DomainRule::parse("127.0.0.1").unwrap();
        assert!(v4.matches("127.0.0.1", 80));
        assert!(v4.matches("127.0.0.1.", 80));
        assert!(!v4.matches("evil.127.0.0.1", 80));
        assert!(!v4.matches("10.127.0.0.1", 80));
        assert!(!v4.matches("localhost", 80));

        let v6 = DomainRule::parse("[::1]").unwrap();
        assert!(v6.matches("::1", 80));
        assert!(v6.matches("[::1]", 80));
        assert!(v6.matches("::0001", 80));
        assert!(!v6.matches("::2", 80));
    }

    /// One rule per *address*, not per spelling: every row here is what
    /// `getaddrinfo(3)` reads as `127.0.0.1` and connects to, so a deny rule
    /// any of them walked past would be no deny at all. The proxy's matcher is
    /// held to the same table by `tests/egress_matcher_conformance.rs`.
    #[test]
    fn one_address_rule_covers_every_spelling_of_that_address() {
        let loopback = DomainRule::parse("127.0.0.1").unwrap();
        for spelling in [
            "127.1",
            "2130706433",
            "0x7f.0.0.1",
            "0177.0.0.1",
            "127.000.000.001",
            "::ffff:127.0.0.1",
            "::ffff:7f00:1",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(loopback.matches(spelling, 80), "'{spelling}'");
        }
        // …and a rule written that way is the same rule, so the two lists
        // cannot disagree about which address the user meant. An IPv6 literal
        // is bracketed here as it is anywhere else a rule is stored: this side
        // reads several ':' as a mis-written port rather than guessing.
        for spelling in [
            "127.1",
            "2130706433",
            "0x7f.0.0.1",
            "127.000.000.001",
            "[::ffff:127.0.0.1]",
        ] {
            assert_eq!(
                DomainRule::parse(spelling).unwrap(),
                loopback,
                "'{spelling}'"
            );
        }
        // Folding must not reach past the address: a *name* carrying the digits
        // is still a name, a leading zero is octal (so `010.` is 8), and the
        // deprecated IPv4-compatible form is a different destination — `::1`
        // itself lives in that range.
        for spelling in [
            "127.0.0.1.evil.net",
            "127.1.evil.net",
            "010.0.0.1",
            "::127.0.0.1",
            "::1",
        ] {
            assert!(!loopback.matches(spelling, 80), "'{spelling}'");
        }
    }

    /// A rule whose `host` was filled in directly — by a caller, or by
    /// deserialising a profile — means what the same text would have meant had
    /// it gone through `parse`. Anything else is a rule that reads one way in
    /// the editor and another at launch.
    #[test]
    fn an_unparsed_rule_still_matches_by_the_canonical_reading() {
        let hand_built = DomainRule {
            host: "127.1".to_string(),
            port: None,
        };
        assert!(hand_built.matches("127.0.0.1", 80));
        assert!(!hand_built.matches("127.1.evil.net", 80));
        // Both wildcard spellings are read as the subtree even unparsed. A
        // deny rule reaching here as `.github.com` — from an older profile, or
        // a caller that filled the field itself — must not be narrowed to the
        // apex, which is the direction that opens a hole.
        for spelling in ["*.github.com", ".github.com"] {
            let subtree = DomainRule {
                host: spelling.to_string(),
                port: None,
            };
            assert!(subtree.matches("api.github.com", 443), "'{spelling}'");
            assert!(subtree.matches("github.com", 443), "'{spelling}'");
            assert!(!subtree.matches("evilgithub.com", 443), "'{spelling}'");
        }
        // A host no spelling can canonicalise covers nothing; `validate` is
        // what keeps such a rule from being stored in the first place.
        let unstorable = DomainRule {
            host: "b\u{fc}cher.example".to_string(),
            port: None,
        };
        assert!(!unstorable.matches("b\u{fc}cher.example", 443));
        // An empty label is not a second wildcard spelling, unparsed either.
        let empty_label = DomainRule {
            host: "..github.com".to_string(),
            port: None,
        };
        assert!(!empty_label.matches("github.com", 443));
        assert!(!empty_label.matches("api.github.com", 443));
    }

    /// A host that does not canonicalise is refused before any rule is
    /// consulted, in every mode — the same verdict the proxy reaches with
    /// `DenyReason::UnsupportedHost`, and never a prompt.
    #[test]
    fn egress_refuses_a_host_that_does_not_canonicalise() {
        let mut p = profile();
        p.network_mode = NetworkMode::Full;
        p.network_deny = vec!["*.xn--bcher-kva.example".into()];
        let policy = p.resolve(SandboxBackendKind::Seatbelt, "/home/u").unwrap();

        for host in [
            "b\u{fc}cher.example",
            "www.b\u{fc}cher.example",
            "example.test..",
            "[example.test]",
        ] {
            let decision = policy.decide_egress(host, 443);
            assert_eq!(decision, EgressDecision::Denied, "'{host}'");
            assert!(!policy.should_prompt(decision), "'{host}'");
        }
        // The punycode form is ordinary ASCII and is caught by the rule itself,
        // and an unrelated host is still reachable under `full`.
        assert_eq!(
            policy.decide_egress("www.xn--bcher-kva.example", 443),
            EgressDecision::Denied
        );
        assert!(policy.decide_egress("example.test", 443).is_allowed());
    }

    #[test]
    fn domain_rule_rejects_unmatched_shapes() {
        for bad in [
            "",
            "   ",
            "https://github.com",
            "github.com/repo",
            "git hub.com",
            "user@github.com",
            "github.com:notaport",
            "github.com:0",
            "github.com:99999",
            "github..com",
            "-github.com",
            "github.com-",
            "::1:443",
            "[::1",
            // A port needs its ':': reading this as 443 invents a scoping.
            "[::1]443",
            // Hex-ish but not an address, so it could never be a request host.
            "[::::]",
            "*",
            // An international name is stored in punycode, because that is how
            // a request host is spelled — and because canonicalising Unicode
            // correctly needs UTS-46 tables this crate does not carry, so the
            // alternative is guessing.
            "b\u{fc}cher.example",
            // Exactly one root dot is presentation, and exactly one leading dot
            // is the subtree prefix; a second of either is an empty label.
            "github.com..",
            "..github.com",
            "*..github.com",
            // A prefix with nothing to widen, and a pattern that is all prefix.
            "*.",
            "*.*.github.com",
            // Brackets are address syntax, not decoration.
            "[github.com]",
            // A zone identifier scopes an address to one interface, which is
            // not something a destination rule can mean.
            "fe80::1%eth0",
            "[fe80::1%eth0]",
        ] {
            assert!(
                DomainRule::parse(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn domain_matching_respects_label_boundaries() {
        let rule = DomainRule::parse("*.github.com").unwrap();
        // A subtree rule covers the host itself and its subdomains…
        assert!(rule.matches("github.com", 443));
        assert!(rule.matches("api.github.com", 443));
        assert!(rule.matches("deep.api.github.com", 443));
        assert!(rule.matches("GitHub.COM", 443));
        assert!(rule.matches("api.github.com.", 443));
        // …and nothing that merely ends with the same characters, or that
        // parks the rule in its own left-hand labels.
        assert!(!rule.matches("evilgithub.com", 443));
        assert!(!rule.matches("notgithub.com", 443));
        assert!(!rule.matches("github.com.evil.net", 443));
        assert!(!rule.matches("github.co", 443));
        assert!(!rule.matches("com", 443));
    }

    #[test]
    fn domain_matching_scopes_ports() {
        let any = DomainRule::parse("github.com").unwrap();
        assert!(any.matches("github.com", 443));
        assert!(any.matches("github.com", 80));

        let https = DomainRule::parse("*.github.com:443").unwrap();
        assert!(https.matches("github.com", 443));
        assert!(!https.matches("github.com", 80));
        // The port-blind question ignores the scope, not the pattern.
        assert!(https.matches_host("api.github.com"));
        assert!(!https.matches_host("evilgithub.com"));
        assert!(!DomainRule::parse("github.com:443")
            .unwrap()
            .matches_host("api.github.com"));
    }

    #[test]
    fn egress_denies_beat_allows_in_every_mode() {
        let mut p = profile();
        p.network_mode = NetworkMode::Allowlist;
        p.network_allow = vec!["*.github.com".into(), "api.anthropic.com".into()];
        p.network_deny = vec!["gist.github.com".into()];
        let policy = p.resolve(SandboxBackendKind::Seatbelt, "/home/u").unwrap();

        assert_eq!(
            policy.decide_egress("api.github.com", 443),
            EgressDecision::Allow
        );
        assert_eq!(
            policy.decide_egress("gist.github.com", 443),
            EgressDecision::Denied
        );
        assert_eq!(
            policy.decide_egress("example.com", 443),
            EgressDecision::Unlisted
        );
        assert!(policy.decide_egress("api.github.com", 443).is_allowed());
        // Only the unlisted case is a question; an explicit deny is settled.
        assert!(policy.should_prompt(EgressDecision::Unlisted));
        assert!(!policy.should_prompt(EgressDecision::Denied));

        // A deny narrows `full` too, and `none` refuses everything.
        p.network_mode = NetworkMode::Full;
        let full = p.resolve(SandboxBackendKind::Seatbelt, "/home/u").unwrap();
        assert_eq!(
            full.decide_egress("example.com", 443),
            EgressDecision::Allow
        );
        assert_eq!(
            full.decide_egress("gist.github.com", 443),
            EgressDecision::Denied
        );
        assert!(full.has_egress());

        p.network_mode = NetworkMode::None;
        let off = p.resolve(SandboxBackendKind::Seatbelt, "/home/u").unwrap();
        assert_eq!(off.decide_egress("github.com", 443), EgressDecision::Denied);
        assert!(!off.has_egress());
        assert!(!off.should_prompt(off.decide_egress("github.com", 443)));
    }

    #[test]
    fn tilde_expands_against_the_given_home() {
        assert_eq!(expand_tilde("~", "/home/u"), "/home/u");
        assert_eq!(expand_tilde("~/dev/app", "/home/u"), "/home/u/dev/app");
        assert_eq!(expand_tilde("~/dev", "/home/u/"), "/home/u/dev");
        assert_eq!(expand_tilde(" ~/dev ", "/home/u"), "/home/u/dev");
        assert_eq!(expand_tilde("~\\dev", "C:\\Users\\u"), "C:\\Users\\u\\dev");
        // Untouched: absolute paths, another user's home, an inner tilde.
        assert_eq!(expand_tilde("/srv/repos", "/home/u"), "/srv/repos");
        assert_eq!(expand_tilde("~other/dev", "/home/u"), "~other/dev");
        assert_eq!(expand_tilde("/tmp/~x", "/home/u"), "/tmp/~x");
    }

    #[test]
    fn path_normalization_drops_trailing_separators_but_keeps_roots() {
        assert_eq!(SandboxPath::read_only("~/dev/ ").normalized(), "~/dev");
        assert_eq!(SandboxPath::read_only("~/dev/").normalized(), "~/dev");
        assert_eq!(SandboxPath::read_only("/").normalized(), "/");
        assert_eq!(SandboxPath::read_only("C:\\").normalized(), "C:\\");
        assert_eq!(
            SandboxPath::workspace("~/dev/app/").expanded("/home/u"),
            "/home/u/dev/app"
        );
    }

    #[test]
    fn covers_matches_paths_and_their_descendants() {
        let p = SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("/srv/shared"),
            ],
        );
        assert!(p.covers("/home/u/dev/app", "/home/u"));
        assert!(p.covers("/home/u/dev/app/src", "/home/u"));
        assert!(p.covers("/srv/shared", "/home/u"));
        assert!(!p.covers("/home/u/dev/other", "/home/u"));
        // A sibling that merely shares a prefix is not covered.
        assert!(!p.covers("/home/u/dev/apple", "/home/u"));
        // Same profile, different home: the stored `~` is what makes this work.
        assert!(p.covers("/Users/u/dev/app", "/Users/u"));
    }

    #[test]
    fn default_profile_is_the_blank_editor_state() {
        let d = SandboxProfile::default();
        assert_eq!(d.backend, SandboxBackendKind::Auto);
        assert_eq!(d.network_mode, NetworkMode::Allowlist);
        assert_eq!(d.read_scope, ReadScope::HostMinusSecrets);
        assert!(d.prompt_new_domains);
        assert!(!d.allow_unsandboxed_fallback);
        assert!(d.memory_mb.is_none() && d.cpus.is_none());
        assert!(d.image.is_none() && d.containerfile.is_none());
        // Blank on purpose, and unsaveable until the user fills it in.
        assert_eq!(d.validate().unwrap_err(), "Name cannot be empty");
        assert_eq!(
            SandboxProfile::new("dev", Vec::new())
                .validate()
                .unwrap_err(),
            "Add at least one path the sandbox can see"
        );
        // Everything the default sets is valid, so naming it and adding one
        // path is all a new profile needs.
        profile().validate().unwrap();
    }

    #[test]
    fn validation_rejects_bad_names() {
        let mut p = profile();
        for (name, expected) in [
            ("", "Name cannot be empty"),
            ("   ", "Name cannot be empty"),
            ("-dev", "Name must start with a letter or a digit"),
            (".dev", "Name must start with a letter or a digit"),
            (
                "dev box",
                "Name may only contain letters, digits, '-', '_' and '.'",
            ),
            // A ':' would split the `sandbox:<profile>` backend name.
            (
                "dev:box",
                "Name may only contain letters, digits, '-', '_' and '.'",
            ),
            (
                "dev/box",
                "Name may only contain letters, digits, '-', '_' and '.'",
            ),
        ] {
            p.name = name.to_string();
            assert_eq!(p.validate().unwrap_err(), expected, "name '{name}'");
        }
        p.name = "d".repeat(65);
        assert_eq!(
            p.validate().unwrap_err(),
            "Name too long (max 64 characters)"
        );

        for good in ["dev", "dev-box", "dev_box", "dev.box", "9lives"] {
            p.name = good.to_string();
            p.validate().unwrap();
        }
    }

    #[test]
    fn validation_rejects_duplicate_and_empty_paths() {
        let mut p = profile();
        // Trailing separators do not make a second path.
        p.paths = vec![
            SandboxPath::workspace("~/dev/app"),
            SandboxPath::read_only("~/dev/app/"),
        ];
        assert_eq!(
            p.validate().unwrap_err(),
            "Path '~/dev/app' is listed twice"
        );

        p.paths = vec![SandboxPath::workspace("  ")];
        assert_eq!(p.validate().unwrap_err(), "Path cannot be empty");
    }

    #[test]
    fn validation_rejects_paths_with_a_control_character() {
        let mut p = profile();
        p.paths = vec![SandboxPath::workspace("~/dev/app\nsecrets")];
        let err = p.validate().unwrap_err();
        assert!(err.contains("control character"), "{err}");

        // Quoting handles the merely awkward, so those still validate.
        p.paths = vec![SandboxPath::workspace("~/dev/my app/it's \"here\"")];
        p.validate().unwrap();
    }

    #[test]
    fn validation_rejects_malformed_domains_in_either_list() {
        let mut p = profile();
        p.network_allow = vec!["github.com".into(), "https://evil".into()];
        assert!(p.validate().unwrap_err().contains("includes a scheme"));

        p.network_allow.clear();
        p.network_deny = vec!["github.com:port".into()];
        assert!(p.validate().unwrap_err().contains("invalid port"));
    }

    /// Both scopes are storable, in either list, and both survive the trip
    /// through [`SandboxProfile::resolve`] — the entries are text until a
    /// launch parses them, so a profile that validated with a wildcard and
    /// resolved without one would enforce something else.
    #[test]
    fn validation_takes_both_scopes_and_resolve_keeps_them_apart() {
        let mut p = profile();
        p.network_allow = vec!["github.com".into(), "*.pypi.org".into()];
        p.network_deny = vec![".gist.github.com".into()];
        p.validate().unwrap();

        let policy = p.resolve(SandboxBackendKind::Seatbelt, "/home/u").unwrap();
        let allow: Vec<String> = policy.allow.iter().map(ToString::to_string).collect();
        assert_eq!(allow, ["github.com", "*.pypi.org"]);
        // The dotted spelling is stored as written and read as the subtree.
        let deny: Vec<String> = policy.deny.iter().map(ToString::to_string).collect();
        assert_eq!(deny, ["*.gist.github.com"]);
        assert!(policy.decide_egress("files.pypi.org", 443).is_allowed());
        assert_eq!(
            policy.decide_egress("api.github.com", 443),
            EgressDecision::Unlisted
        );
        assert_eq!(
            policy.decide_egress("api.gist.github.com", 443),
            EgressDecision::Denied
        );

        // A prefix the grammar has no reading for is a typo, not a third scope.
        p.network_allow = vec!["*..github.com".into()];
        assert!(p.validate().unwrap_err().contains("empty label"));
    }

    #[test]
    fn validation_rejects_place_only_fields_on_a_policy_backend() {
        let mut p = profile();
        p.backend = SandboxBackendKind::Seatbelt;
        p.memory_mb = Some(2048);
        assert!(p.validate().unwrap_err().contains("cannot cap memory"));

        p.memory_mb = None;
        p.image = Some("ghcr.io/example/dev:latest".into());
        assert!(p.validate().unwrap_err().contains("has no image"));

        // The same fields are fine on a place backend, and `auto` is exempt
        // because its shape is only known after a host probe.
        p.backend = SandboxBackendKind::Docker;
        p.memory_mb = Some(2048);
        p.validate().unwrap();
        p.backend = SandboxBackendKind::Auto;
        p.validate().unwrap();

        p.backend = SandboxBackendKind::Docker;
        p.containerfile = Some("Containerfile".into());
        assert_eq!(
            p.validate().unwrap_err(),
            "Set an image or a containerfile, not both"
        );

        p.containerfile = None;
        p.memory_mb = Some(0);
        assert_eq!(
            p.validate().unwrap_err(),
            "Memory and CPU limits must be greater than 0"
        );

        // Each limit is checked on its own: a valid memory cap must not let a
        // zero CPU count through.
        p.memory_mb = Some(2048);
        p.cpus = Some(0);
        assert_eq!(
            p.validate().unwrap_err(),
            "Memory and CPU limits must be greater than 0"
        );
    }

    #[test]
    fn validate_unique_ignores_the_profile_being_edited() {
        let p = profile();
        p.validate_unique(&["other".to_string()]).unwrap();
        // Case-insensitive: both names would be one container name.
        let err = p.validate_unique(&["DEV".to_string()]).unwrap_err();
        assert_eq!(err, "A sandbox profile named 'dev' already exists");
    }

    #[test]
    fn resolve_expands_orders_and_splits_the_path_sets() {
        let mut p = SandboxProfile::new(
            "dev",
            vec![
                SandboxPath::workspace("~/dev/app"),
                SandboxPath::read_only("~/dev/app/.git/hooks"),
                SandboxPath::read_only("/srv/shared/"),
            ],
        );
        p.read_scope = ReadScope::HostMinusSecrets;
        let policy = p.resolve(SandboxBackendKind::Bwrap, "/home/u").unwrap();

        assert_eq!(policy.profile, "dev");
        assert_eq!(policy.backend, SandboxBackendKind::Bwrap);
        assert_eq!(policy.shape, SandboxShape::Policy);
        assert_eq!(policy.rw_paths, ["/home/u/dev/app"]);
        // Sorted, so the writable ancestor is bound before the read-only
        // descendant that has to override it.
        assert_eq!(
            policy.ro_paths,
            ["/home/u/dev/app/.git/hooks", "/srv/shared"]
        );
        assert!(policy.env.is_empty());
        // A policy backend keeps the host read scope and gains no place fields.
        assert_eq!(policy.read_scope, ReadScope::HostMinusSecrets);
        assert!(policy.memory_mb.is_none() && policy.image.is_none());
    }

    #[test]
    fn resolve_narrows_fields_a_place_backend_cannot_use() {
        let mut p = profile();
        p.read_scope = ReadScope::HostMinusSecrets;
        p.memory_mb = Some(4096);
        p.cpus = Some(2);
        p.image = Some("ghcr.io/example/dev:latest".into());
        let place = p.resolve(SandboxBackendKind::Docker, "/home/u").unwrap();
        // A place has no host filesystem to read past the workspace.
        assert_eq!(place.read_scope, ReadScope::Workspace);
        assert_eq!(place.memory_mb, Some(4096));
        assert_eq!(place.cpus, Some(2));
        assert_eq!(place.image.as_deref(), Some("ghcr.io/example/dev:latest"));
    }

    #[test]
    fn resolve_refuses_auto_and_invalid_profiles() {
        let p = profile();
        assert_eq!(
            p.resolve(SandboxBackendKind::Auto, "/home/u").unwrap_err(),
            "Resolve 'auto' to a concrete backend before building a sandbox policy"
        );
        let blank = SandboxProfile::default();
        assert!(blank.resolve(SandboxBackendKind::Bwrap, "/home/u").is_err());
    }

    #[test]
    fn policy_env_is_ordered_and_replaceable() {
        let mut policy = profile()
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        policy.insert_env("HTTPS_PROXY", "http://127.0.0.1:1080");
        policy.insert_env("FRIRING_SESSION_ID", "abc");
        let replaced = policy.insert_env("HTTPS_PROXY", "http://127.0.0.1:2080");
        assert_eq!(replaced.as_deref(), Some("http://127.0.0.1:1080"));
        let keys: Vec<&str> = policy.env.keys().map(String::as_str).collect();
        assert_eq!(keys, ["FRIRING_SESSION_ID", "HTTPS_PROXY"]);
    }

    #[test]
    fn profile_survives_a_json_round_trip_with_missing_fields() {
        let mut p = profile();
        p.network_allow = vec!["github.com:443".into()];
        p.created_at = 17;
        p.updated_at = 42;
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<SandboxProfile>(&json).unwrap(), p);

        // An import that omits everything but the essentials inherits the
        // struct's defaults, not each field type's.
        let partial: SandboxProfile =
            serde_json::from_str(r#"{"name":"dev","paths":[{"path":"~/dev/app","mode":"rw"}]}"#)
                .unwrap();
        assert_eq!(partial, profile());
        assert!(partial.prompt_new_domains);
    }
}
