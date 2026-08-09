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
use crate::sandbox::probe::{detect_platform, HostPlatform, LocalProbeHost, ProbeHost};
use crate::sandbox::secrets::{secrets_for, SecretKind, SecretPlatform};
use crate::session::{NetworkMode, ReadScope, SandboxBackendKind, SandboxShape};

/// The bubblewrap binary, resolved on `PATH` because distributions disagree
/// about where it lives (and a setuid install lives elsewhere again).
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

/// A parsed `bwrap --version`, and what the version implies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BwrapDetails {
    pub availability: Availability,
    /// `(major, minor)`, or `None` when `--version` could not be read.
    pub version: Option<(u32, u32)>,
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
/// `exists` answers whether a path is present **on the host the sandbox runs
/// on**; it is injected so the mount plan is a pure function of its inputs and
/// a test never has to consult the developer's own filesystem. Only the secrets
/// list consults it — everything else either must exist (a path the user
/// listed, which should fail loudly) or is bound with `-try`.
pub fn build_argv(
    launch: &SandboxLaunch<'_>,
    exists: &dyn Fn(&str) -> bool,
) -> SandboxResult<Argv> {
    let policy = launch.policy;
    let mut argv: Vec<String> = vec![BWRAP.to_string()];

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
    // After the root, so they are not shadowed by it. `/tmp` is a private
    // tmpfs: writable (tools that ignore `TMPDIR` still work) and invisible to
    // the host (nothing outside reads an agent's scratch files).
    push(&mut argv, &["--proc", "/proc"]);
    push(&mut argv, &["--dev", "/dev"]);
    push(&mut argv, &["--tmpfs", "/tmp"]);

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
        // ADR-29: the database never enters a sandbox. Bound only when it is
        // there, for the same mount-point reason as the secrets.
        for file in [db.to_string(), format!("{db}-wal"), format!("{db}-shm")] {
            if exists(&file) {
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
        let platform = detect_platform(self.host.as_ref());
        if !matches!(platform, HostPlatform::Linux | HostPlatform::WslDistro) {
            return BwrapDetails {
                availability: Availability::unavailable(format!(
                    "bubblewrap needs Linux; this host is {}",
                    platform.label()
                )),
                version: None,
            };
        }
        if !self.host.which(BWRAP) {
            return BwrapDetails {
                availability: Availability::needs_fix(
                    "bubblewrap (bwrap) is not installed",
                    "install it: apt install bubblewrap / dnf install bubblewrap / \
                     pacman -S bubblewrap",
                ),
                version: None,
            };
        }
        let version = self
            .host
            .run(BWRAP, &["--version"])
            .ok()
            .filter(|o| o.ok())
            .and_then(|o| parse_version(o.trimmed()));

        // The decisive question is not what a sysctl says but whether a
        // namespace can actually be created, so ask bwrap. It costs one fork of
        // `true` and is the only check that cannot be wrong.
        let attempt = self.host.run(BWRAP, &["--ro-bind", "/", "/", "true"]);
        let availability = match attempt {
            Ok(output) if output.ok() => Availability::available(match version {
                Some((major, minor)) => format!("bubblewrap {major}.{minor}"),
                None => "bubblewrap".to_string(),
            }),
            Ok(output) => self.diagnose(&output.stderr),
            Err(detail) => Availability::unavailable(detail),
        };
        BwrapDetails {
            availability,
            version,
        }
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
        let mut out = build_argv(launch, &|path| self.host.path_exists(path))?;
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
        let argv = build_argv(&launch, &nothing).unwrap();

        assert_eq!(argv[0], "bwrap");
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
        let argv = build_argv(&launch, &nothing).unwrap();
        let at = index_of(&argv, "--hostname");
        assert_eq!(argv[at + 1], "friring-dev");
        // sethostname accepts a narrow charset, so the profile name is filtered.
        assert_eq!(hostname("dev.box_1"), "friring-dev-box-1");
    }

    #[test]
    fn host_minus_secrets_binds_the_whole_root_read_only_first() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(&launch, &nothing).unwrap();
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
        let argv = build_argv(&launch, &nothing).unwrap();

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
        let argv = build_argv(&launch, &nothing).unwrap();
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
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_workspace("/home/u/work/repo")
            .with_signal_dir("/home/u/.local/share/friring/signals/s1")
            .with_tmp_dir("/tmp/friring-s1");
        let argv = build_argv(&launch, &nothing).unwrap();

        for path in [
            "/home/u/work/repo",
            "/home/u/.local/share/friring/signals/s1",
            "/tmp/friring-s1",
        ] {
            assert!(has_mount(&argv, "--bind", path, path), "missing {path}");
        }
        // The private /tmp is created before anything is bound inside it.
        assert!(index_of(&argv, "--tmpfs") < index_of(&argv, "/tmp/friring-s1"));
        let chdir = index_of(&argv, "--chdir");
        assert_eq!(argv[chdir + 1], "/home/u/work/repo");
    }

    #[test]
    fn secrets_are_hidden_only_where_they_exist() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_agent("claude");
        let present = |p: &str| matches!(p, "/home/u/.ssh" | "/home/u/.netrc");
        let argv = build_argv(&launch, &present).unwrap();

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
        let argv = build_argv(&launch, &|_| true).unwrap();
        assert!(!argv.iter().any(|a| a == "/home/u/.ssh"));
    }

    #[test]
    fn the_database_is_replaced_by_dev_null_when_it_is_there() {
        let policy = policy(vec![SandboxPath::workspace("~/.local/share/friring")]);
        let db = "/home/u/.local/share/friring/friring.db";
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_friring_db(db);
        let argv = build_argv(&launch, &|p| p == db).unwrap();
        assert!(has_mount(&argv, "--ro-bind", "/dev/null", db));
        // ADR-29 wins over the writable data directory it sits inside.
        assert!(index_of(&argv, db) > index_of(&argv, "/home/u/.local/share/friring"));
    }

    #[test]
    fn git_hooks_stay_read_only_inside_a_writable_root() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(&launch, &nothing).unwrap();
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
            let argv = build_argv(&launch, &nothing).unwrap();
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
        let argv = build_argv(&launch, &nothing).unwrap();
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
        let argv = build_argv(&socket, &nothing).unwrap();
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
        let err = build_argv(&loopback, &nothing).unwrap_err();
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

    #[test]
    fn a_blocked_user_namespace_reports_the_setting_that_would_fix_it() {
        let apparmor = StubHost::new()
            .with_command("uname -s", ProbeOutput::success("Linux\n"))
            .with_file("/proc/sys/kernel/osrelease", "6.8.0-generic\n")
            .with_binary("bwrap")
            .with_command(
                "bwrap --version",
                ProbeOutput::success("bubblewrap 0.9.0\n"),
            )
            .with_command(
                "bwrap --ro-bind / / true",
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
                "bwrap --version",
                ProbeOutput::success("bubblewrap 0.8.0\n"),
            )
            .with_command(
                "bwrap --ro-bind / / true",
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
                "bwrap --version",
                ProbeOutput::success("bubblewrap 0.11.0\n"),
            )
            .with_command(
                "bwrap --ro-bind / / true",
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
