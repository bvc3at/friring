//! Bubblewrap: the Linux and WSL2 policy backend.
//!
//! [`build_argv`] is pure — a launch and a "does this path exist" predicate in,
//! a command line out — so the whole mount plan is assertable without running
//! anything. [`BwrapBackend`] adds the probe and passes the real predicate.
//!
//! Two things about bwrap decide the shape of everything below:
//!
//! - **Operations apply in order onto the new root**, and a later mount
//!   overrides an earlier one. So the broad root goes first and every override
//!   — a writable workspace, a read-only `.git/hooks` inside it, a hidden
//!   secret — follows in ancestor-before-descendant order.
//! - **A mount point cannot be created under a read-only mount.** Once `/` is
//!   bound read-only, `--tmpfs ~/.ssh` fails with `EROFS` unless the directory
//!   is already there. That is why hiding a secret is conditional on it
//!   existing: a path that is not there needs no hiding, and trying anyway
//!   would fail the launch instead of tightening it.

use std::sync::{Arc, OnceLock};

use crate::sandbox::backend::{
    Argv, Availability, Caps, InnerSandboxVerdict, ProxyEndpoint, SandboxBackend, SandboxError,
    SandboxLaunch, SandboxResult, PROTECTED_IN_WRITABLE_ROOT,
};
use crate::sandbox::dirs;
use crate::sandbox::probe::{detect_platform, HostPlatform, LocalProbeHost, ProbeHost};
use crate::sandbox::secrets::{secrets_for, SecretKind, SecretPlatform};
use crate::session::{NetworkMode, ReadScope, SandboxBackendKind, SandboxShape};

/// The name looked up on `PATH`, because distributions disagree about where
/// bubblewrap lives (and a setuid install lives elsewhere again).
///
/// It is a lookup key and **never** what gets executed: the probe resolves it to
/// an absolute path once, refuses one a sandboxed agent could rewrite, and the
/// launch runs that. Seatbelt makes the same promise by naming
/// [`SANDBOX_EXEC`](crate::sandbox::seatbelt::SANDBOX_EXEC) outright — the
/// user's environment must not choose what applies the policy, and a bare name
/// re-resolved at launch through the tmux server's `PATH` lets it.
pub const BWRAP: &str = "bwrap";

/// Overlays (`--overlay`, `--tmp-overlay`) — the copy-on-write workspace mode —
/// arrived in this version.
const OVERLAY_SINCE: (u32, u32) = (0, 11);

/// Directories bound into a [`ReadScope::Workspace`] sandbox so a binary can
/// load and run at all.
///
/// Every one is `--ro-bind-try`: `/lib64` is absent on aarch64, `/nix` exists
/// only on NixOS, and a missing one must not fail the launch.
const SYSTEM_RO_BINDS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/opt", "/nix",
];

/// Control-socket trees an empty tmpfs is laid over, even though the read scope
/// says the host is readable.
///
/// A read-only bind is not a barrier to a *socket*: `connect(2)` on a pathname
/// unix socket needs nothing but the path, and `--unshare-net` isolates the
/// network namespace, not the filesystem. Under `/run` — and its
/// `/run/user/$UID` runtime directory — live rootless docker/podman, the user
/// systemd bus, `gpg-agent` and dbus, several of which are arbitrary host
/// command execution. Seatbelt's `(deny default)` already refuses unix-domain
/// sockets, so masking these is what stops bwrap granting strictly *more* than
/// seatbelt for the same profile.
///
/// This is deliberately its own category rather than an addition to the
/// credentials deny list: those entries hide *secrets a read grants*, these hide
/// *endpoints a connect reaches*, and conflating them would lose the reason
/// either exists. A profile that explicitly lists a path under one of these
/// keeps it — the mask is a default, and the profile's own paths are bound
/// after it (most specific wins).
const MASKED_SOCKET_DIRS: &[&str] = &["/run", "/var/run"];

/// A parsed `bwrap --version`, and what the version implies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BwrapDetails {
    pub availability: Availability,
    /// `(major, minor)`, or `None` when `--version` could not be read.
    pub version: Option<(u32, u32)>,
    /// The absolute path the probe resolved and vetted, or `None` when the
    /// backend is unavailable. This — never [`BWRAP`] — is what a launch runs.
    pub program: Option<String>,
}

impl BwrapDetails {
    /// Whether unprivileged overlays are available, which is what the optional
    /// copy-on-write workspace mode needs.
    ///
    /// Version-only: overlays are also unavailable when bwrap is installed
    /// setuid, which this does not detect — a setuid install is rare, and the
    /// failure surfaces as bwrap's own error rather than as a wrong answer
    /// here.
    pub fn supports_overlay(&self) -> bool {
        self.version.is_some_and(|v| v >= OVERLAY_SINCE)
    }
}

/// Parse the `major.minor` out of `bubblewrap 0.11.0`.
pub fn parse_version(output: &str) -> Option<(u32, u32)> {
    let number = output.split_whitespace().find_map(|token| {
        let head = token.trim_start_matches('v');
        head.chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
            .then_some(head)
    })?;
    let mut parts = number.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor))
}

/// Build the bubblewrap command line for one launch.
///
/// `program` is the absolute path the probe resolved and vetted — see
/// [`BWRAP`]. `exists` answers whether a path is present **on the host the
/// sandbox runs on**; it is injected so the mount plan is a pure function of its
/// inputs and a test never has to consult the developer's own filesystem. The
/// secrets list, the socket masks and the database mask consult it — everything
/// else either must exist (a path the user listed, which should fail loudly) or
/// is bound with `-try`.
pub fn build_argv(
    program: &str,
    launch: &SandboxLaunch<'_>,
    exists: &dyn Fn(&str) -> bool,
) -> SandboxResult<Argv> {
    let policy = launch.policy;
    let mut argv: Vec<String> = vec![program.to_string()];

    // The sandbox dies with the pane that owns it, gets its own pid/ipc/uts
    // namespaces, and keeps the controlling terminal. `--new-session` is
    // deliberately absent: it calls setsid(), which detaches the agent from the
    // tmux pane's terminal and leaves a TUI with no window size, no job control
    // and no keystrokes.
    //
    // `--unshare-user` is never passed explicitly either: bwrap does it itself
    // when it is not installed setuid, and forcing it breaks a setuid install.
    push(&mut argv, &["--die-with-parent", "--unshare-pid"]);
    push(&mut argv, &["--unshare-ipc", "--unshare-uts"]);
    // A hostname the prompt shows, so being inside a sandbox is visible.
    push(&mut argv, &["--hostname", &hostname(&policy.profile)]);
    if policy.network != NetworkMode::Full {
        // Both `none` and `allowlist` mean "no direct egress" (ADR-27). The
        // allowlist's way *out* is the proxy socket bound in below.
        push(&mut argv, &["--unshare-net"]);
    }

    match policy.read_scope {
        ReadScope::HostMinusSecrets => push(&mut argv, &["--ro-bind", "/", "/"]),
        ReadScope::Workspace => {
            for path in SYSTEM_RO_BINDS {
                push(&mut argv, &["--ro-bind-try", path, path]);
            }
        }
    }
    // After the root, so they are not shadowed by it. `/tmp` stays a private
    // tmpfs and is never re-bound from the host: writable (tools that ignore
    // `TMPDIR` still work), invisible to the host (nothing outside reads an
    // agent's scratch files), and — the part that matters — not the directory
    // friring's own tmux server listens in. The scratch the agent is *given*
    // is a per-session directory friring mints elsewhere (see
    // [`crate::sandbox::dirs`]).
    push(&mut argv, &["--proc", "/proc"]);
    push(&mut argv, &["--dev", "/dev"]);
    push(&mut argv, &["--tmpfs", "/tmp"]);
    if policy.read_scope == ReadScope::HostMinusSecrets {
        // Only this scope binds the host root, so only this scope has socket
        // trees to take back. Emitted before the profile's own paths, so a path
        // the user listed inside one still wins.
        for dir in masked_socket_dirs() {
            if exists(&dir) {
                push(&mut argv, &["--tmpfs", &dir]);
            }
        }
    }

    // One sorted pass over both sets, so a read-only path nested in a writable
    // one is bound *after* its ancestor and wins.
    for (path, writable) in mount_plan(launch) {
        let flag = if writable { "--bind" } else { "--ro-bind" };
        push(&mut argv, &[flag, &path, &path]);
    }

    for root in launch.writable_paths() {
        // `-try`: most writable roots are not repositories, and a missing
        // source must not fail the launch.
        let path = format!("{root}/{PROTECTED_IN_WRITABLE_ROOT}");
        push(&mut argv, &["--ro-bind-try", &path, &path]);
    }

    if policy.read_scope == ReadScope::HostMinusSecrets {
        for secret in secrets_for(SecretPlatform::Linux, launch.agent) {
            let path = secret.resolved(launch.home);
            if !exists(&path) {
                continue;
            }
            match secret.kind {
                // An empty tmpfs over the directory: present, and empty.
                SecretKind::Dir => push(&mut argv, &["--tmpfs", &path]),
                // A file cannot be tmpfs'd; /dev/null reads as an empty file.
                SecretKind::File => push(&mut argv, &["--ro-bind", "/dev/null", &path]),
            }
        }
    }

    if let Some(db) = launch.friring_db {
        // ADR-29: the database never enters a sandbox.
        //
        // Whether a mask can be *created* decides how far this can go. Under a
        // read-only root a mount point cannot be made, so only what is already
        // there can be covered — the `exists` half. Where an ancestor is
        // writable the mount point can be made, and then the mask must be
        // unconditional: the `-wal` a launch does not see is exactly the file
        // SQLite creates afterwards, and a `-wal` written from inside is
        // replayed by the host on next open, which is the ADR-29 escape by a
        // slower route. `SandboxLaunch::validate` refuses such a profile
        // outright; this stays because `build_argv` is a pure function anyone
        // may call, and a mount plan that leans on someone else's earlier check
        // is the shape that produced this hole.
        let parent_is_writable = std::path::Path::new(db)
            .parent()
            .map(|p| p.display().to_string())
            .is_some_and(|parent| {
                launch
                    .writable_paths()
                    .iter()
                    .any(|root| dirs::encloses(root, &parent))
            });
        for file in [db.to_string(), format!("{db}-wal"), format!("{db}-shm")] {
            if parent_is_writable || exists(&file) {
                push(&mut argv, &["--ro-bind", "/dev/null", &file]);
            }
        }
    }

    if let Some((host_path, inside_path)) = proxy_mount(launch)? {
        push(&mut argv, &["--bind", &host_path, &inside_path]);
    }

    if let Some(workspace) = launch.workspace {
        // Explicit rather than inherited: bwrap keeps the caller's working
        // directory, and a cwd that is not mapped inside fails the launch.
        push(&mut argv, &["--chdir", workspace]);
    }

    // Ends bwrap's own option parsing, so an agent flag is never read as one.
    argv.push("--".to_string());
    Ok(argv)
}

fn push(argv: &mut Vec<String>, tokens: &[&str]) {
    argv.extend(tokens.iter().map(|t| (*t).to_string()));
}

/// Every path to mount, sorted so an ancestor precedes its descendants.
fn mount_plan(launch: &SandboxLaunch<'_>) -> Vec<(String, bool)> {
    let mut plan: Vec<(String, bool)> = launch
        .writable_paths()
        .into_iter()
        .map(|p| (p, true))
        .chain(launch.readable_paths().into_iter().map(|p| (p, false)))
        .collect();
    plan.sort_by(|a, b| a.0.cmp(&b.0));
    plan
}

/// The proxy socket to bind in, or `None` when this launch has no way out to
/// offer.
///
/// A `--unshare-net` sandbox has its own empty network stack, so a proxy on
/// *host* loopback is unreachable from inside — the endpoint has to be a socket
/// that can be mounted across the boundary. Saying so is the seam P2 builds
/// against; silently ignoring a loopback endpoint would produce a sandbox that
/// looks proxied and has no network at all.
fn proxy_mount(launch: &SandboxLaunch<'_>) -> SandboxResult<Option<(String, String)>> {
    if launch.policy.network != NetworkMode::Allowlist {
        return Ok(None);
    }
    match launch.proxy.as_ref() {
        None => Ok(None),
        Some(ProxyEndpoint::UnixSocket {
            host_path,
            inside_path,
        }) => Ok(Some((host_path.clone(), inside_path.clone()))),
        Some(ProxyEndpoint::Loopback { .. }) => Err(SandboxError::Unsupported {
            backend: SandboxBackendKind::Bwrap,
            detail: "a --unshare-net sandbox has no route to host loopback; the egress proxy \
                     must expose a unix socket for this backend"
                .to_string(),
        }),
    }
}

/// Every control-socket tree to cover, with the ones this host puts somewhere
/// non-standard folded in and anything already covered dropped.
///
/// The tmux socket root is here for friring's *own* server: `--tmpfs /tmp`
/// covers the default location, but `$TMUX_TMPDIR` moves it, and a sandbox that
/// can reach that socket can run a command in any pane on the host.
fn masked_socket_dirs() -> Vec<String> {
    // `/tmp` seeds the coverage test rather than the output: the caller has
    // already made it a private tmpfs by the time this is consulted.
    let mut covered: Vec<String> = vec!["/tmp".to_string()];
    let mut out: Vec<String> = Vec::new();
    let host_specific = [
        std::env::var_os("XDG_RUNTIME_DIR").map(|v| v.to_string_lossy().into_owned()),
        Some(dirs::tmux_socket_root().display().to_string()),
    ];
    for candidate in MASKED_SOCKET_DIRS
        .iter()
        .map(|d| (*d).to_string())
        .chain(host_specific.into_iter().flatten())
    {
        if !candidate.starts_with('/') {
            continue;
        }
        if covered.iter().any(|c| dirs::encloses(c, &candidate)) {
            continue;
        }
        covered.push(candidate.clone());
        out.push(candidate);
    }
    out
}

/// A hostname that says where you are, reduced to what `sethostname` accepts.
fn hostname(profile: &str) -> String {
    let cleaned: String = profile
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(48)
        .collect();
    format!("friring-{}", cleaned.trim_matches('-'))
}

/// Bubblewrap.
pub struct BwrapBackend {
    host: Arc<dyn ProbeHost>,
    details: OnceLock<BwrapDetails>,
}

impl BwrapBackend {
    pub fn new(host: Arc<dyn ProbeHost>) -> Self {
        Self {
            host,
            details: OnceLock::new(),
        }
    }

    /// The local machine.
    pub fn local() -> Self {
        Self::new(Arc::new(LocalProbeHost))
    }

    /// The probe's full answer, including the version behind
    /// [`BwrapDetails::supports_overlay`]. Cached with the availability.
    pub fn details(&self) -> &BwrapDetails {
        self.details.get_or_init(|| self.run_probe())
    }

    fn run_probe(&self) -> BwrapDetails {
        let unavailable = |availability| BwrapDetails {
            availability,
            version: None,
            program: None,
        };
        let platform = detect_platform(self.host.as_ref());
        if !matches!(platform, HostPlatform::Linux | HostPlatform::WslDistro) {
            return unavailable(Availability::unavailable(format!(
                "bubblewrap needs Linux; this host is {}",
                platform.label()
            )));
        }
        let Some(program) = self.host.which(BWRAP) else {
            return unavailable(Availability::needs_fix(
                "bubblewrap (bwrap) is not installed",
                "install it: apt install bubblewrap / dnf install bubblewrap / \
                 pacman -S bubblewrap",
            ));
        };
        if let Some(location) = self.rewritable_location(&program) {
            // The wrapper is the boundary: a bwrap the sandboxed agent can
            // overwrite is a boundary the sandboxed agent chooses. Refusing is
            // the only safe answer — falling back to the next `PATH` entry would
            // still be running whatever an attacker arranged to be found.
            return unavailable(Availability::needs_fix(
                format!(
                    "bubblewrap resolves to '{program}', inside {location} — a sandboxed agent \
                     could replace it and the next launch would run unwrapped"
                ),
                "install bubblewrap system-wide (apt/dnf/pacman) and take the writable copy off \
                 PATH",
            ));
        }
        let version = self
            .host
            .run(&program, &["--version"])
            .ok()
            .filter(|o| o.ok())
            .and_then(|o| parse_version(o.trimmed()));

        // The decisive question is not what a sysctl says but whether a
        // namespace can actually be created, so ask bwrap. It costs one fork of
        // `true` and is the only check that cannot be wrong.
        let attempt = self.host.run(&program, &["--ro-bind", "/", "/", "true"]);
        let availability = match attempt {
            Ok(output) if output.ok() => Availability::available(match version {
                Some((major, minor)) => format!("bubblewrap {major}.{minor}"),
                None => "bubblewrap".to_string(),
            }),
            Ok(output) => self.diagnose(&output.stderr),
            Err(detail) => Availability::unavailable(detail),
        };
        BwrapDetails {
            program: availability.is_available().then_some(program),
            availability,
            version,
        }
    }

    /// Whether `program` sits somewhere a sandboxed agent can write, and where.
    ///
    /// The home directory is the one that matters — `~/.local/bin` is on most
    /// users' `PATH` and inside the default read scope's writable set — with the
    /// shared scratch directories alongside it because they are writable by
    /// anyone. friring's own sandbox tree is included for completeness; it lives
    /// under the data directory, which no profile may make writable.
    fn rewritable_location(&self, program: &str) -> Option<String> {
        let home = self.host.home();
        let mut roots: Vec<String> = ["/tmp", "/var/tmp", "/dev/shm"]
            .iter()
            .map(|d| (*d).to_string())
            .collect();
        roots.extend(home.clone());
        roots.extend(dirs::sandbox_root().map(|p| p.display().to_string()));
        roots
            .into_iter()
            .find(|root| dirs::encloses(root, program))
            .map(|root| {
                if home.as_deref() == Some(root.as_str()) {
                    format!("the home directory ('{root}')")
                } else {
                    format!("'{root}'")
                }
            })
    }

    /// Turn a failed namespace creation into the setting that would fix it.
    ///
    /// The order matters: on Ubuntu 23.10 through 24.04 the AppArmor
    /// restriction is the cause and the two older sysctls are untouched, so
    /// checking it first is what makes the message actionable rather than
    /// merely true. Ubuntu 25.04 and newer ship a `bwrap-userns-restrict`
    /// profile, so bwrap works there with the same sysctl set.
    fn diagnose(&self, stderr: &str) -> Availability {
        let sysctl = |path: &str| {
            self.host
                .read_file(path)
                .map(|v| v.trim().to_string())
                .unwrap_or_default()
        };
        if sysctl("/proc/sys/kernel/apparmor_restrict_unprivileged_userns") == "1" {
            return Availability::needs_fix(
                "unprivileged user namespaces are restricted by AppArmor",
                "install the bwrap-userns-restrict AppArmor profile, or run: sudo sysctl -w \
                 kernel.apparmor_restrict_unprivileged_userns=0",
            );
        }
        if sysctl("/proc/sys/kernel/unprivileged_userns_clone") == "0" {
            return Availability::needs_fix(
                "unprivileged user namespaces are disabled",
                "sudo sysctl -w kernel.unprivileged_userns_clone=1",
            );
        }
        if sysctl("/proc/sys/user/max_user_namespaces") == "0" {
            return Availability::needs_fix(
                "this host allows no user namespaces",
                "sudo sysctl -w user.max_user_namespaces=15000",
            );
        }
        let reason = stderr
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("bwrap could not create a sandbox");
        Availability::unavailable(reason.trim().to_string())
    }
}

impl SandboxBackend for BwrapBackend {
    fn kind(&self) -> SandboxBackendKind {
        SandboxBackendKind::Bwrap
    }

    fn probe(&self) -> Availability {
        self.details().availability.clone()
    }

    fn capabilities(&self) -> Caps {
        Caps {
            shape: SandboxShape::Policy,
            limits: false,
            network_modes: NetworkMode::ALL,
            read_scopes: ReadScope::ALL,
            persistent: false,
            host_credentials: true,
            // A nested bwrap can work, but it re-isolates what is already
            // isolated and needs user namespaces the outer sandbox may not
            // grant, so friring turns the inner one off rather than debugging
            // two boundaries.
            inner_agent_sandbox: InnerSandboxVerdict::Redundant,
        }
    }

    fn wrap(&self, argv: Argv, launch: &SandboxLaunch<'_>) -> SandboxResult<Argv> {
        if launch.policy.backend != SandboxBackendKind::Bwrap {
            return Err(SandboxError::Unsupported {
                backend: SandboxBackendKind::Bwrap,
                detail: format!(
                    "policy was resolved for '{}'; resolve it for bwrap first",
                    launch.policy.backend
                ),
            });
        }
        launch.validate()?;
        let details = self.details();
        let Some(program) = details.program.as_deref() else {
            return Err(SandboxError::Unavailable {
                backend: SandboxBackendKind::Bwrap,
                reason: details.availability.message(),
            });
        };
        // The probe vetted the binary against the *host*; this profile decides
        // what the agent can write, and a profile that hands it the directory
        // bubblewrap lives in hands it the boundary.
        if let Some(root) = launch
            .writable_paths()
            .into_iter()
            .find(|root| dirs::encloses(root, program))
        {
            return Err(SandboxError::Refused {
                profile: launch.policy.profile.clone(),
                detail: format!(
                    "the read-write path '{root}' contains bubblewrap itself ('{program}'), so \
                     the sandbox could replace the program that applies its own boundary"
                ),
            });
        }
        let mut out = build_argv(program, launch, &|path| self.host.path_exists(path))?;
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
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap()
    }

    fn workspace_policy() -> SandboxPolicy {
        policy(vec![
            SandboxPath::workspace("~/dev/app"),
            SandboxPath::read_only("/srv/shared"),
        ])
    }

    /// Nothing exists — the default for a test that does not care.
    fn nothing(_: &str) -> bool {
        false
    }

    /// What the probe would have resolved: a system-installed bubblewrap.
    const PROGRAM: &str = "/usr/bin/bwrap";

    /// The per-session scratch directory friring mints, as a launch sees it.
    /// Under the data directory — never the host temp root.
    fn scratch() -> String {
        crate::sandbox::dirs::session_scratch_dir("s1")
            .unwrap()
            .display()
            .to_string()
    }

    /// The index of `token` in `argv`, panicking with the whole command line so
    /// a failure reads as a diff rather than as `None`.
    fn index_of(argv: &[String], token: &str) -> usize {
        argv.iter()
            .position(|a| a == token)
            .unwrap_or_else(|| panic!("{token} missing from {argv:?}"))
    }

    /// Whether `argv` contains `flag src dst` in sequence.
    fn has_mount(argv: &[String], flag: &str, src: &str, dst: &str) -> bool {
        argv.windows(3)
            .any(|w| w[0] == flag && w[1] == src && w[2] == dst)
    }

    /// Whether `argv` contains `flag value` in sequence.
    fn has_flag(argv: &[String], flag: &str, value: &str) -> bool {
        argv.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn base_flags_isolate_without_stealing_the_terminal() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();

        // The absolute path the probe pinned, never the bare name.
        assert_eq!(argv[0], PROGRAM);
        for flag in ["--die-with-parent", "--unshare-pid", "--unshare-ipc"] {
            assert!(argv.contains(&flag.to_string()), "missing {flag}");
        }
        // setsid() would detach the agent from the pane's terminal.
        assert!(!argv.contains(&"--new-session".to_string()));
        // bwrap unshares the user namespace itself unless installed setuid.
        assert!(!argv.contains(&"--unshare-user".to_string()));
        assert!(has_flag(&argv, "--proc", "/proc"));
        assert!(has_flag(&argv, "--dev", "/dev"));
        assert!(has_flag(&argv, "--tmpfs", "/tmp"));
    }

    #[test]
    fn the_hostname_says_which_sandbox_you_are_in() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();
        let at = index_of(&argv, "--hostname");
        assert_eq!(argv[at + 1], "friring-dev");
        // sethostname accepts a narrow charset, so the profile name is filtered.
        assert_eq!(hostname("dev.box_1"), "friring-dev-box-1");
    }

    #[test]
    fn host_minus_secrets_binds_the_whole_root_read_only_first() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();
        assert!(has_mount(&argv, "--ro-bind", "/", "/"));
        // Everything that overrides the root must come after it.
        assert!(index_of(&argv, "/srv/shared") > index_of(&argv, "--ro-bind"));
    }

    #[test]
    fn workspace_scope_builds_a_root_instead_of_binding_one() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.read_scope = ReadScope::Workspace;
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();

        assert!(!has_mount(&argv, "--ro-bind", "/", "/"));
        assert!(has_mount(&argv, "--ro-bind-try", "/usr", "/usr"));
        // `-try`, because /lib64 is absent on aarch64 and must not fail a launch.
        assert!(has_mount(&argv, "--ro-bind-try", "/lib64", "/lib64"));
        assert!(has_mount(
            &argv,
            "--bind",
            "/home/u/dev/app",
            "/home/u/dev/app"
        ));
    }

    #[test]
    fn a_nested_read_only_path_is_bound_after_its_writable_ancestor() {
        let policy = policy(vec![
            SandboxPath::workspace("~/dev/app"),
            SandboxPath::read_only("~/dev/app/.git/hooks"),
        ]);
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();
        let parent = index_of(&argv, "/home/u/dev/app");
        let child = index_of(&argv, "/home/u/dev/app/.git/hooks");
        assert!(
            parent < child,
            "the nested mount must override, not precede"
        );
        assert!(has_mount(
            &argv,
            "--ro-bind",
            "/home/u/dev/app/.git/hooks",
            "/home/u/dev/app/.git/hooks"
        ));
    }

    #[test]
    fn session_paths_are_bound_writable_at_their_own_paths() {
        let policy = workspace_policy();
        let scratch = scratch();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_workspace("/home/u/work/repo")
            .with_signal_dir("/home/u/.local/share/friring/signals/s1")
            .with_tmp_dir(&scratch);
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();

        for path in [
            "/home/u/work/repo",
            "/home/u/.local/share/friring/signals/s1",
            &scratch,
        ] {
            assert!(has_mount(&argv, "--bind", path, path), "missing {path}");
        }
        let chdir = index_of(&argv, "--chdir");
        assert_eq!(argv[chdir + 1], "/home/u/work/repo");
    }

    /// The escape: the host `/tmp` holds friring's own tmux socket, and
    /// `--unshare-net` does nothing about a unix socket reached by path. `/tmp`
    /// is a private tmpfs and is never bound back over.
    #[test]
    fn the_host_temp_root_is_never_bound_into_the_sandbox() {
        let policy = workspace_policy();
        let scratch = scratch();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_tmp_dir(&scratch);
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();

        assert!(has_flag(&argv, "--tmpfs", "/tmp"));
        for flag in ["--bind", "--ro-bind", "--ro-bind-try"] {
            assert!(
                !has_mount(&argv, flag, "/tmp", "/tmp"),
                "{flag} put the host /tmp back over the private tmpfs"
            );
        }
        // The scratch the agent is given is friring's own per-session directory.
        assert!(has_mount(&argv, "--bind", &scratch, &scratch));
        assert_ne!(scratch, "/tmp");

        // And a launch that tried to hand over the directory tmux keeps its
        // sockets in is refused before a single mount is planned.
        let sockets = crate::sandbox::dirs::tmux_socket_root()
            .display()
            .to_string();
        assert!(SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_tmp_dir(&sockets)
            .validate()
            .is_err());
    }

    /// A read-only bind is no barrier to `connect(2)`, and `--unshare-net`
    /// isolates the network namespace rather than the filesystem — so the
    /// control-socket trees are covered rather than merely read-only.
    #[test]
    fn control_socket_trees_are_masked_under_the_host_read_scope() {
        let host_scope = workspace_policy();
        let launch = SandboxLaunch::new(&host_scope, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &|_| true).unwrap();
        for dir in ["/run", "/var/run"] {
            assert!(has_flag(&argv, "--tmpfs", dir), "missing mask for {dir}");
            // After the root bind, so the mask is not shadowed by it.
            assert!(index_of(&argv, dir) > index_of(&argv, "--ro-bind"));
        }

        // A path the profile lists inside a masked tree still wins: the mask is
        // a default, and the profile's own paths are bound after it.
        let listed = policy(vec![
            SandboxPath::workspace("~/dev/app"),
            SandboxPath::read_only("/run/systemd/resolve"),
        ]);
        let launch = SandboxLaunch::new(&listed, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &|_| true).unwrap();
        assert!(
            index_of(&argv, "/run/systemd/resolve") > index_of(&argv, "/run"),
            "an explicitly listed path must be bound after the mask"
        );

        // The workspace scope binds no host root, so it has nothing to take
        // back — and mounting a tmpfs there would only cost a launch.
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.read_scope = ReadScope::Workspace;
        let narrow = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&narrow, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &|_| true).unwrap();
        assert!(!has_flag(&argv, "--tmpfs", "/run"));
    }

    #[test]
    fn secrets_are_hidden_only_where_they_exist() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_agent("claude");
        let present = |p: &str| matches!(p, "/home/u/.ssh" | "/home/u/.netrc");
        let argv = build_argv(PROGRAM, &launch, &present).unwrap();

        // A directory is covered by an empty tmpfs, a file by /dev/null.
        assert!(has_flag(&argv, "--tmpfs", "/home/u/.ssh"));
        assert!(has_mount(&argv, "--ro-bind", "/dev/null", "/home/u/.netrc"));
        // A missing secret needs no hiding; mounting over it would fail the
        // launch because the mount point cannot be created under a ro root.
        assert!(!argv.iter().any(|a| a == "/home/u/.aws"));
        // The launching agent keeps its own credentials, and only its own.
        assert!(!argv.iter().any(|a| a.contains(".claude")));
    }

    #[test]
    fn workspace_scope_needs_no_secret_hiding() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.read_scope = ReadScope::Workspace;
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        // Nothing outside the listed paths is in the sandbox to begin with.
        let argv = build_argv(PROGRAM, &launch, &|_| true).unwrap();
        assert!(!argv.iter().any(|a| a == "/home/u/.ssh"));
    }

    #[test]
    fn the_database_is_replaced_by_dev_null_when_it_is_there() {
        let policy = policy(vec![SandboxPath::workspace("~/.local/share/friring")]);
        let db = "/home/u/.local/share/friring/friring.db";
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_friring_db(db);
        let argv = build_argv(PROGRAM, &launch, &|p| p == db).unwrap();
        assert!(has_mount(&argv, "--ro-bind", "/dev/null", db));
        // ADR-29 wins over the writable data directory it sits inside.
        assert!(index_of(&argv, db) > index_of(&argv, "/home/u/.local/share/friring"));
    }

    /// The sidecar that does not exist yet is the one that matters: SQLite
    /// creates the `-wal` on first write, and the host replays it on next open.
    /// Where an ancestor is writable the mount point can be created, so the mask
    /// cannot wait for the file to appear.
    #[test]
    fn database_sidecars_are_masked_before_they_exist_under_a_writable_ancestor() {
        let db = "/home/u/.local/share/friring/friring.db";
        let writable = policy(vec![SandboxPath::workspace("~/.local/share/friring")]);
        let launch = SandboxLaunch::new(&writable, "/home/u", "s1").with_friring_db(db);
        // Nothing exists yet — not even the database.
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();
        for file in [db, &format!("{db}-wal"), &format!("{db}-shm")] {
            assert!(
                has_mount(&argv, "--ro-bind", "/dev/null", file),
                "missing mask for {file}"
            );
        }

        // Under a read-only root the mount point cannot be created, so only what
        // is already there is masked — mounting over the rest would fail the
        // launch rather than tighten it.
        let read_only = policy(vec![SandboxPath::read_only("~/.local/share/friring")]);
        let launch = SandboxLaunch::new(&read_only, "/home/u", "s1").with_friring_db(db);
        let argv = build_argv(PROGRAM, &launch, &|p| p == db).unwrap();
        assert!(has_mount(&argv, "--ro-bind", "/dev/null", db));
        assert!(!argv.iter().any(|a| a == &format!("{db}-wal")));
    }

    #[test]
    fn git_hooks_stay_read_only_inside_a_writable_root() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();
        let hooks = "/home/u/dev/app/.git/hooks";
        assert!(has_mount(&argv, "--ro-bind-try", hooks, hooks));
    }

    #[test]
    fn network_modes_decide_whether_the_stack_is_shared() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        for mode in [NetworkMode::None, NetworkMode::Allowlist] {
            profile.network_mode = mode;
            let policy = profile
                .resolve(SandboxBackendKind::Bwrap, "/home/u")
                .unwrap();
            let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
            let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();
            assert!(
                argv.contains(&"--unshare-net".to_string()),
                "{mode} must have no direct egress"
            );
        }
        profile.network_mode = NetworkMode::Full;
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, &nothing).unwrap();
        assert!(!argv.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn the_proxy_seam_takes_a_socket_and_refuses_loopback() {
        let policy = workspace_policy();
        let socket =
            SandboxLaunch::new(&policy, "/home/u", "s1").with_proxy(ProxyEndpoint::UnixSocket {
                host_path: "/run/friring/proxy-s1.sock".into(),
                inside_path: "/run/friring-proxy.sock".into(),
            });
        let argv = build_argv(PROGRAM, &socket, &nothing).unwrap();
        assert!(has_mount(
            &argv,
            "--bind",
            "/run/friring/proxy-s1.sock",
            "/run/friring-proxy.sock"
        ));
        // The namespace still has no route out except that socket.
        assert!(argv.contains(&"--unshare-net".to_string()));

        let loopback = SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_proxy(ProxyEndpoint::Loopback { port: 8123 });
        let err = build_argv(PROGRAM, &loopback, &nothing).unwrap_err();
        assert!(err.to_string().contains("no route to host loopback"));
    }

    #[test]
    fn the_agent_argv_is_appended_after_the_separator() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let backend = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        let argv = backend
            .wrap(
                vec!["claude".into(), "--resume".into(), "abc".into()],
                &launch,
            )
            .unwrap();
        let end = index_of(&argv, "--");
        assert_eq!(&argv[end + 1..], ["claude", "--resume", "abc"]);
    }

    #[test]
    fn wrap_refuses_a_policy_resolved_for_another_backend() {
        let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")])
            .resolve(SandboxBackendKind::Seatbelt, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let backend = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        let err = backend.wrap(vec!["claude".into()], &launch).unwrap_err();
        assert!(err.to_string().contains("resolve it for bwrap first"));
    }

    #[test]
    fn version_parsing_survives_the_shapes_bwrap_prints() {
        assert_eq!(parse_version("bubblewrap 0.11.0"), Some((0, 11)));
        assert_eq!(parse_version("bubblewrap 0.8"), Some((0, 8)));
        assert_eq!(parse_version("bwrap v1.2.3"), Some((1, 2)));
        assert_eq!(parse_version("bubblewrap"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn probe_reports_the_version_and_overlay_support() {
        let modern = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        assert_eq!(modern.probe().message(), "bubblewrap 0.11");
        assert!(modern.details().supports_overlay());

        let old = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.8.0")));
        assert!(old.probe().is_available());
        assert!(
            !old.details().supports_overlay(),
            "copy-on-write workspaces need 0.11"
        );
    }

    #[test]
    fn probe_needs_linux_and_the_binary() {
        let mac = BwrapBackend::new(Arc::new(StubHost::macos(26, true)));
        assert_eq!(
            mac.probe().message(),
            "bubblewrap needs Linux; this host is macOS 26 (Apple Silicon)"
        );

        let bare = StubHost::new()
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n");
        let missing = BwrapBackend::new(Arc::new(bare));
        assert!(missing
            .probe()
            .message()
            .starts_with("bubblewrap (bwrap) is not installed"));
        assert!(missing.probe().message().contains("apt install bubblewrap"));
    }

    /// A bare `bwrap` on `PATH` is chosen by the environment, and the tmux
    /// server's environment is one a sandboxed agent with a writable home can
    /// arrange: plant `~/.local/bin/bwrap` and the next launch runs it,
    /// unwrapped, as the host user. So the probe resolves and vets one path, and
    /// that path is what both the probe and the launch run.
    #[test]
    fn a_bwrap_the_agent_could_rewrite_is_never_the_boundary() {
        let planted = StubHost::new()
            .with_home("/home/u")
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary_at("bwrap", "/home/u/.local/bin/bwrap");
        let backend = BwrapBackend::new(Arc::new(planted));
        let message = backend.probe().message();
        assert!(message.contains("/home/u/.local/bin/bwrap"), "{message}");
        assert!(message.contains("home directory"), "{message}");
        assert!(!backend.probe().is_available());
        assert!(backend.details().program.is_none());

        // A shared scratch directory is no better: anyone can write it.
        let shared = StubHost::new()
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary_at("bwrap", "/tmp/bwrap");
        assert!(BwrapBackend::new(Arc::new(shared))
            .probe()
            .message()
            .contains("'/tmp'"));

        // A system install is used by its absolute path, for the probe and the
        // launch alike.
        let system = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        assert_eq!(system.details().program.as_deref(), Some(PROGRAM));
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = system.wrap(vec!["claude".into()], &launch).unwrap();
        assert_eq!(argv[0], PROGRAM);
    }

    #[test]
    fn a_profile_that_hands_over_bwrap_itself_is_refused_at_launch() {
        // The probe vetted the binary against the host; this profile is what
        // decides whether the agent can rewrite it.
        let system = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        let policy = policy(vec![SandboxPath::workspace("/usr/bin")]);
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let err = system.wrap(vec!["claude".into()], &launch).unwrap_err();
        assert!(err.to_string().contains(PROGRAM), "{err}");
        assert!(matches!(err, SandboxError::Refused { .. }));
    }

    #[test]
    fn a_blocked_user_namespace_reports_the_setting_that_would_fix_it() {
        let apparmor = StubHost::new()
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("bwrap")
            .with_command(
                "/usr/bin/bwrap --version",
                ProbeOutput::success("bubblewrap 0.9.0\n"),
            )
            .with_command(
                "/usr/bin/bwrap --ro-bind / / true",
                ProbeOutput::failure(1, "bwrap: setting up uid map: Permission denied\n"),
            )
            .with_file(
                "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
                "1\n",
            );
        let backend = BwrapBackend::new(Arc::new(apparmor));
        let message = backend.probe().message();
        assert!(message.contains("restricted by AppArmor"));
        assert!(message.contains("bwrap-userns-restrict"));

        // A host with the older sysctl gets the older fix.
        let sysctl = StubHost::new()
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.1.0-generic\n")
            .with_binary("bwrap")
            .with_command(
                "/usr/bin/bwrap --version",
                ProbeOutput::success("bubblewrap 0.8.0\n"),
            )
            .with_command(
                "/usr/bin/bwrap --ro-bind / / true",
                ProbeOutput::failure(1, "bwrap: No permissions to create new namespace\n"),
            )
            .with_file("/proc/sys/kernel/unprivileged_userns_clone", "0\n");
        let backend = BwrapBackend::new(Arc::new(sysctl));
        assert!(backend
            .probe()
            .message()
            .contains("sudo sysctl -w kernel.unprivileged_userns_clone=1"));
    }

    #[test]
    fn an_unexplained_failure_surfaces_bwraps_own_words() {
        let odd = StubHost::new()
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("bwrap")
            .with_command(
                "/usr/bin/bwrap --version",
                ProbeOutput::success("bubblewrap 0.11.0\n"),
            )
            .with_command(
                "/usr/bin/bwrap --ro-bind / / true",
                ProbeOutput::failure(1, "\nbwrap: Can't mount proc on /newroot/proc\n"),
            );
        let backend = BwrapBackend::new(Arc::new(odd));
        assert_eq!(
            backend.probe().message(),
            "bwrap: Can't mount proc on /newroot/proc"
        );
    }

    #[test]
    fn capabilities_say_what_bwrap_cannot_do() {
        let backend = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        let caps = backend.capabilities();
        assert_eq!(caps.shape, SandboxShape::Policy);
        assert!(!caps.limits);
        assert!(caps.host_credentials);
        assert_eq!(caps.inner_agent_sandbox, InnerSandboxVerdict::Redundant);
    }
}
