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
//!
//! Version 0.11 adds unprivileged overlays, which back the optional
//! copy-on-write workspace: a writable root becomes an overlayfs whose lower
//! layer is the real directory and whose upper layer is a directory friring
//! mints — see [`OverlayWorkspace`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::sandbox::backend::{
    Argv, Availability, Caps, Egress, InnerSandboxVerdict, ProxyEndpoint, ProxyTransport,
    SandboxBackend, SandboxError, SandboxLaunch, SandboxResult, PROTECTED_IN_WRITABLE_ROOT,
};
use crate::sandbox::dirs;
use crate::sandbox::egress::relay_addr;
use crate::sandbox::launcher::relay_launcher_argv;
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
    /// Whether this bwrap can give a launch a [copy-on-write
    /// workspace](OverlayWorkspace), and — when it cannot — the reason a
    /// refusal quotes and the profile editor shows in place of the option.
    ///
    /// Probed by *doing* it rather than inferred, because two of the three
    /// answers are invisible to a version check: bubblewrap installed setuid
    /// cannot mount an unprivileged overlay at all, and a kernel older than the
    /// unprivileged-overlayfs work refuses one from a user namespace. Both would
    /// otherwise surface as a dead pane on the first launch that asked for the
    /// feature.
    pub overlay: Availability,
}

impl BwrapDetails {
    /// Whether unprivileged overlays are available, which is what the optional
    /// copy-on-write workspace mode needs. The reason behind a `false` is
    /// [`overlay`](Self::overlay).
    pub fn supports_overlay(&self) -> bool {
        self.overlay.is_available()
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

/// One writable root the sandbox sees through an overlay instead of directly.
///
/// The copy-on-write workspace of `docs/SANDBOX.md` §`bwrap`: the real
/// directory is the overlay's **lower** layer and stays untouched, every write
/// inside the boundary lands in [`upper`](Self::upper), and the merged view is
/// mounted back at [`root`](Self::root) — identical absolute paths, so a git
/// linked worktree and an agent's per-project state still resolve.
///
/// The upper layer is a plain directory rather than a tmpfs on purpose: what
/// makes the mode useful is being able to read what the agent wrote after the
/// fact, and to throw it away deliberately rather than on process exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayWorkspace {
    /// The path the sandbox sees, which is also the lower layer. Must be one of
    /// the launch's own read-write paths — the overlay narrows a grant the
    /// profile already made, and never invents one.
    pub root: String,
    /// Where writes land. friring's own directory, minted `0700`.
    pub upper: String,
    /// overlayfs's own scratch, which the kernel requires to be an empty
    /// directory on [`upper`](Self::upper)'s filesystem.
    pub work: String,
}

/// Where the copy-on-write layers live: `<data dir>/sandbox/overlay`.
///
/// Deliberately **not** under the session's scratch directory, which is the
/// obvious home for a per-launch artefact and the wrong one. The scratch is
/// bind-mounted read-write into the sandbox by design, so a layer directory
/// under it is a path the agent can replace with a symlink between two launches
/// — and the `upperdir` friring hands to `--overlay` is where *every* write
/// inside the boundary lands, so a link to `/` would put the next launch's
/// writes on the host root. Under here the tree is reachable from inside only
/// through the overlay the agent is already writing through.
///
/// `None` when friring cannot resolve its data directory, which refuses the
/// launch rather than falling back to somewhere writable.
fn overlay_root() -> Option<PathBuf> {
    dirs::sandbox_root().map(|root| root.join("overlay"))
}

/// Mint (or adopt) the layer directories for one launch's copy-on-write roots.
///
/// Adopted rather than recreated, like the per-session scratch: a relaunch of a
/// crashed session must find the work its agent had already done, and wiping the
/// upper layer on every start would make "discardable" mean "discarded without
/// being asked". Every component is created by
/// [`dirs::create_private_dir_under`], which refuses a symlink and classifies
/// one as interference rather than as a profile a user should edit.
///
/// # Errors
///
/// friring has no data directory, or a layer directory exists as something
/// other than a directory friring owns.
pub fn overlay_workspaces(
    session_key: &str,
    roots: &[String],
) -> SandboxResult<Vec<OverlayWorkspace>> {
    let base = overlay_root().ok_or_else(|| SandboxError::Io {
        path: "<data dir>/sandbox/overlay".to_string(),
        detail: "friring could not resolve its data directory, so it has nowhere outside every \
                 sandbox to keep a copy-on-write layer"
            .to_string(),
    })?;
    let key = dirs::sanitize_component(session_key);
    let mut out = Vec::with_capacity(roots.len());
    for root in roots {
        // Keyed on a digest of the path rather than on the path: a layer
        // directory is named once and looked up again on every relaunch, and a
        // sanitised absolute path collides (`/a/b` and `/a-b`) where a digest
        // does not.
        let leaf = format!("{key}/{}", dirs::digest(root));
        let upper = dirs::create_private_dir_under(&base, &format!("{leaf}/upper"))?;
        let work = dirs::create_private_dir_under(&base, &format!("{leaf}/work"))?;
        out.push(OverlayWorkspace {
            root: root.clone(),
            upper: representable(&upper)?,
            work: representable(&work)?,
        });
    }
    Ok(out)
}

/// Drop every copy-on-write layer one session left behind.
///
/// Session teardown's half of [`overlay_workspaces`], kept beside it rather than
/// folded into [`dirs::cleanup_session`] because the layers deliberately do not
/// live in the tree that function owns. Best effort: what will not go costs
/// disk, and the next launch of the same session adopts it.
pub fn cleanup_overlays(session_key: &str) {
    let key = dirs::sanitize_component(session_key);
    if let Some(dir) = overlay_root().map(|root| root.join(&key)) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A minted path the argv can name exactly, or a refusal — a lossy conversion
/// would hand overlayfs a different directory than the one friring created.
fn representable(path: &Path) -> SandboxResult<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| SandboxError::Io {
            path: path.display().to_string(),
            detail:
                "is not valid UTF-8, and an overlay layer built from an approximation of it would \
                 name a different directory"
                    .to_string(),
        })
}

/// Build the bubblewrap command line for one launch.
///
/// [`build_argv_with`] with no copy-on-write workspaces.
pub fn build_argv(
    program: &str,
    launch: &SandboxLaunch<'_>,
    relay: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
) -> SandboxResult<Argv> {
    build_argv_with(program, launch, relay, exists, &[])
}

/// Build the bubblewrap command line for one launch.
///
/// `program` is the absolute path the probe resolved and vetted — see
/// [`BWRAP`]. `relay` is friring's own CLI, which runs *inside* the namespace
/// to give the agent's clients a TCP endpoint onto the proxy's socket; it is a
/// launch input for the same reason the database path is (the friring that owns
/// the session is the one whose binary belongs in there), and `None` is only
/// legal for a launch with no socket to relay to. `exists` answers whether a
/// path is present **on the host the sandbox runs on**; it is injected so the
/// mount plan is a pure function of its inputs and a test never has to consult
/// the developer's own filesystem. The secrets list, the socket masks, the
/// database mask and the relay binary consult it — everything else either must
/// exist (a path the user listed, which should fail loudly) or is bound with
/// `-try`.
///
/// `overlays` turns writable roots into [copy-on-write
/// workspaces](OverlayWorkspace). They are emitted in the *same* sorted pass as
/// every other mount, so the precedence rule is unchanged — a read-only path
/// nested inside a copy-on-write root is still bound after it and still wins —
/// and everything friring takes back (the `.git/hooks` bind, the secret masks,
/// the database masks) is emitted after that pass, so an overlay can never widen
/// what a mask denies.
///
/// # Errors
///
/// An overlay names a path this launch does not grant read-write, or two
/// overlays stack. Plus everything the launch's own inputs refuse: a filtered
/// mode with no proxy, a loopback endpoint this backend cannot reach, a missing
/// relay.
pub fn build_argv_with(
    program: &str,
    launch: &SandboxLaunch<'_>,
    relay: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
    overlays: &[OverlayWorkspace],
) -> SandboxResult<Argv> {
    let policy = launch.policy;
    check_overlays(launch, overlays)?;
    let socket = proxy_socket(launch)?;
    let relay = match &socket {
        Some(_) => Some(relay_program(launch, relay, exists)?),
        None => None,
    };
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
    if !matches!(launch.egress(), Egress::Open) {
        // Every mode but unrestricted `full` means "no direct egress" (ADR-27)
        // — including a `full` that carries denies, whose exceptions only the
        // proxy can enforce. The way *out*, when there is one, is the socket
        // bound in below and the relay that fronts it.
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
    } else if let Some(relay) = relay {
        // The narrow scope builds a root out of the system directories instead
        // of binding the host's, so friring's own CLI is not in there unless it
        // lives under one of them. Before the profile's own paths, like every
        // other default, so a path the user listed still wins.
        push(&mut argv, &["--ro-bind", relay, relay]);
    }

    // One sorted pass over both sets, so a read-only path nested in a writable
    // one is bound *after* its ancestor and wins.
    for (path, writable) in mount_plan(launch) {
        match overlays.iter().find(|ws| ws.root == path) {
            // `--overlay-src` is consumed by the option that follows it, so the
            // pair is emitted together and never separated by another mount.
            Some(ws) => push(
                &mut argv,
                &[
                    "--overlay-src",
                    &ws.root,
                    "--overlay",
                    &ws.upper,
                    &ws.work,
                    &ws.root,
                ],
            ),
            None => {
                let flag = if writable { "--bind" } else { "--ro-bind" };
                push(&mut argv, &[flag, &path, &path]);
            }
        }
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
        // The state friring keeps for its *other* sandboxes: the logins their
        // places hold, the markers that keep one credential to one boundary
        // (ADR-28), and the generated policies that constrain other sessions.
        // The host read scope binds the host root, so without these it carries
        // all of it — and every one of them is taken by being read. Masked after
        // the profile's own paths rather than before, unlike the socket trees:
        // a profile naming one of these is refused outright
        // (`dirs::check_declared_paths`), so there is no listed path to lose to
        // the mask, and there is no ordering left in which the scope can win.
        //
        // Not `<data>/sandbox` whole: this launch's own scratch is under
        // `<data>/sandbox/tmp` and the agent needs it to start.
        for dir in [dirs::place_root(), dirs::profile_dir(), dirs::seeds_root()]
            .into_iter()
            .flatten()
            .map(|dir| dir.display().to_string())
        {
            if exists(&dir) {
                push(&mut argv, &["--tmpfs", &dir]);
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

    if let Some((host_path, inside_path)) = &socket {
        // Read-write, and not by oversight: `connect(2)` on a unix socket needs
        // write permission on it, so a `--ro-bind` here would leave the sandbox
        // looking proxied with no way to dial the proxy. What the mount buys
        // instead is that the socket becomes a *mount point*: the scratch
        // directory around it is writable by design, and unlinking a mount
        // point is `EBUSY`, so the agent can neither delete its own way out nor
        // replace it with a socket of its own.
        push(
            &mut argv,
            &["--bind", host_path.as_str(), inside_path.as_str()],
        );
    }

    if let Some(workspace) = launch.workspace {
        // Explicit rather than inherited: bwrap keeps the caller's working
        // directory, and a cwd that is not mapped inside fails the launch.
        push(&mut argv, &["--chdir", workspace]);
    }

    // Ends bwrap's own option parsing, so an agent flag is never read as one.
    argv.push("--".to_string());

    if let (Some((_, inside_path)), Some(relay)) = (&socket, relay) {
        // The relay is started *inside* the namespace, because that is the only
        // place the sandbox's own loopback exists. It holds no token and makes
        // no decision — the proxy still demands its credential at the far end —
        // so what runs in here is a pipe, not a policy.
        let listen = relay_addr().to_string();
        argv.extend(relay_launcher_argv(relay, &listen, inside_path));
    }
    Ok(argv)
}

fn push(argv: &mut Vec<String>, tokens: &[&str]) {
    argv.extend(tokens.iter().map(|t| (*t).to_string()));
}

/// Refuse a set of copy-on-write workspaces that would mean more than the
/// profile says.
///
/// Two shapes, both fail-closed:
///
/// - **An overlay narrows a grant; it never makes one.** A root the launch does
///   not already grant read-write would otherwise become writable *and*
///   invisible to every check that was made against the writable set — the
///   database masks, the tmux-socket refusal, the engine-socket refusal all read
///   [`SandboxLaunch::writable_paths`], and a path that is not in there has been
///   judged by none of them.
/// - **Overlays do not stack.** A copy-on-write root inside another one is an
///   overlayfs whose lower layer is a directory that is itself an overlay by the
///   time the sandbox looks at it; the merged view is defensible on paper and
///   not something friring can state precisely, so it is refused rather than
///   composed.
fn check_overlays(launch: &SandboxLaunch<'_>, overlays: &[OverlayWorkspace]) -> SandboxResult<()> {
    let refuse = |detail: String| SandboxError::Refused {
        profile: launch.policy.profile.clone(),
        detail,
    };
    let writable = launch.writable_paths();
    for ws in overlays {
        if !writable.contains(&ws.root) {
            return Err(refuse(format!(
                "'{}' is a copy-on-write workspace of a path this launch does not grant \
                 read-write, and an overlay narrows a grant rather than making one",
                ws.root
            )));
        }
        if let Some(outer) = overlays
            .iter()
            .find(|other| other.root != ws.root && dirs::encloses(&other.root, &ws.root))
        {
            return Err(refuse(format!(
                "'{}' and '{}' are both copy-on-write workspaces, one inside the other, and \
                 friring will not stack one overlay's writes on another's",
                outer.root, ws.root
            )));
        }
    }
    Ok(())
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

/// The proxy socket to bind in, as `(host path, path inside)`, or `None` when
/// this launch has no way out to offer.
///
/// A `--unshare-net` sandbox has its own empty network stack, so a proxy on
/// *host* loopback is unreachable from inside — the endpoint has to be a socket
/// that can be mounted across the boundary. A loopback endpoint is refused with
/// that reason rather than ignored: a sandbox that looks proxied and has no
/// network at all is the failure nobody diagnoses.
fn proxy_socket(launch: &SandboxLaunch<'_>) -> SandboxResult<Option<(String, String)>> {
    match launch.egress() {
        Egress::Open | Egress::Closed => Ok(None),
        Egress::Proxied(ProxyEndpoint::UnixSocket {
            host_path,
            inside_path,
        }) => Ok(Some((host_path.clone(), inside_path.clone()))),
        Egress::Proxied(ProxyEndpoint::Loopback { .. }) => Err(SandboxError::Unsupported {
            backend: SandboxBackendKind::Bwrap,
            detail: "a --unshare-net sandbox has no route to host loopback; the egress proxy \
                     must expose a unix socket for this backend"
                .to_string(),
        }),
    }
}

/// friring's own CLI, which the sandbox runs as its relay, checked to be there.
///
/// Refusing beats launching without it: the relay is the whole of the sandbox's
/// egress, so a missing binary means an agent that believes it is proxied and
/// reaches nothing. The launch fails with the fix, and a profile's
/// `allow_unsandboxed_fallback` decides what happens next.
fn relay_program<'a>(
    launch: &SandboxLaunch<'_>,
    relay: Option<&'a str>,
    exists: &dyn Fn(&str) -> bool,
) -> SandboxResult<&'a str> {
    let refuse = |detail: String| SandboxError::Refused {
        profile: launch.policy.profile.clone(),
        detail,
    };
    let relay = relay.ok_or_else(|| {
        refuse(
            "friring could not locate its own 'friring-cli', which a bubblewrap sandbox runs \
             inside its namespace to reach the egress proxy's socket. Install friring-cli \
             beside friring"
                .to_string(),
        )
    })?;
    if !exists(relay) {
        return Err(refuse(format!(
            "'{relay}' does not exist, and a bubblewrap sandbox runs it inside its namespace to \
             reach the egress proxy's socket. Install friring-cli beside friring"
        )));
    }
    Ok(relay)
}

/// Where this friring's `friring-cli` is, for the in-sandbox relay.
///
/// Derived from the running binary rather than from `PATH`: the relay must be
/// *this* friring's CLI, and a name resolved through the tmux server's
/// environment is one the user's environment chooses. `None` when the running
/// binary has no sibling CLI, which refuses a launch that needs one rather than
/// guessing.
pub fn local_relay_program() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = exe.file_name()?.to_str()?;
    if name.starts_with("friring-cli") {
        return Some(exe);
    }
    let sibling = exe.parent()?.join("friring-cli");
    sibling.exists().then_some(sibling)
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
        let unavailable = |availability: Availability| BwrapDetails {
            overlay: Availability::unavailable(format!(
                "bubblewrap is unavailable here: {}",
                availability.message()
            )),
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
        let overlay = if availability.is_available() {
            self.probe_overlay(&program, version)
        } else {
            Availability::unavailable(format!(
                "bubblewrap is unavailable here: {}",
                availability.message()
            ))
        };
        BwrapDetails {
            program: availability.is_available().then_some(program),
            availability,
            version,
            overlay,
        }
    }

    /// Whether this bwrap can mount an unprivileged overlay, asked by mounting
    /// one.
    ///
    /// The version is the cheap half and is checked first, because a bwrap that
    /// has never heard of `--overlay-src` answers "unknown option" and the fix
    /// is an upgrade rather than anything about this host. Past that bar the
    /// only honest answer is an attempt: **a setuid bubblewrap cannot mount an
    /// overlay at all**, and neither can a kernel that refuses overlayfs from a
    /// user namespace, and nothing about either shows up in `--version`. The
    /// attempt costs one fork of `true`, mounts nothing that outlives it and
    /// writes nothing — the same shape as the user-namespace probe above it.
    fn probe_overlay(&self, program: &str, version: Option<(u32, u32)>) -> Availability {
        let (major, minor) = OVERLAY_SINCE;
        match version {
            Some(found) if found >= OVERLAY_SINCE => {}
            Some((found_major, found_minor)) => {
                return Availability::needs_fix(
                    format!(
                        "bubblewrap {found_major}.{found_minor} has no unprivileged overlays, \
                         which a copy-on-write workspace is made of"
                    ),
                    format!("upgrade bubblewrap to {major}.{minor} or newer"),
                )
            }
            None => {
                return Availability::unavailable(
                    "friring could not read this bubblewrap's version, so it will not assume the \
                     unprivileged overlays a copy-on-write workspace is made of",
                )
            }
        }
        // `/usr` is the lower layer and the mount point: it is on every host
        // this backend runs on, and an overlay over it is discarded with the
        // namespace the moment `true` exits.
        let attempt = self.host.run(
            program,
            &[
                "--ro-bind",
                "/",
                "/",
                "--overlay-src",
                "/usr",
                "--tmp-overlay",
                "/usr",
                "true",
            ],
        );
        match attempt {
            Ok(output) if output.ok() => Availability::available("unprivileged overlays"),
            Ok(output) => {
                let reason = output
                    .stderr
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("bubblewrap could not mount an unprivileged overlay");
                // A setuid install is the one cause with a fix the user can act
                // on, and bwrap says so itself ("Unable to create overlay
                // filesystem in setuid mode"), so it is named rather than left
                // as a quoted error nobody can place.
                if reason.to_ascii_lowercase().contains("setuid") {
                    return Availability::needs_fix(
                        format!(
                            "'{program}' is installed setuid, and an unprivileged overlay — which \
                             a copy-on-write workspace is made of — cannot be mounted in that mode"
                        ),
                        "install a bubblewrap that relies on unprivileged user namespaces instead \
                         of the setuid bit, or take the copy-on-write workspace off the profile",
                    );
                }
                Availability::unavailable(reason.to_string())
            }
            Err(detail) => Availability::unavailable(detail),
        }
    }

    /// Whether `program` sits somewhere a sandboxed agent can write, phrased for
    /// the probe message. The rule itself is
    /// [`dirs::rewritable_root`](crate::sandbox::dirs::rewritable_root), which
    /// the container engines apply to their own CLI for the same reason.
    fn rewritable_location(&self, program: &str) -> Option<String> {
        let home = self.host.home();
        dirs::rewritable_root(program, home.as_deref()).map(|root| {
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
            // `--unshare-net` leaves the sandbox its own loopback and no route
            // to the host's, so only a bind-mounted socket crosses.
            proxy_transport: ProxyTransport::UnixSocket,
        }
    }

    fn wrap(&self, argv: Argv, launch: &SandboxLaunch<'_>) -> SandboxResult<Argv> {
        self.wrap_copy_on_write(argv, launch, &[])
    }
}

impl BwrapBackend {
    /// [`wrap`](SandboxBackend::wrap), with `cow_roots` served through
    /// [copy-on-write workspaces](OverlayWorkspace) instead of written to
    /// directly.
    ///
    /// The seam the profile's own capability plugs into: each root must be one
    /// the profile already granted read-write, and the layers are minted under
    /// friring's data directory rather than anywhere the sandbox can reach.
    ///
    /// **A host whose bubblewrap cannot mount an overlay refuses the launch**
    /// rather than binding the root directly. Degrading silently would be the
    /// one failure this mode exists to prevent: the user asked for writes that
    /// do not touch the real directory, and a plain read-write bind writes
    /// straight into it.
    ///
    /// # Errors
    ///
    /// Everything [`wrap`](SandboxBackend::wrap) refuses, plus: this bwrap has
    /// no unprivileged overlays (with [`BwrapDetails::overlay`]'s reason), a
    /// root the launch does not grant read-write, stacked overlays, or a layer
    /// directory friring could not mint.
    pub fn wrap_copy_on_write(
        &self,
        argv: Argv,
        launch: &SandboxLaunch<'_>,
        cow_roots: &[String],
    ) -> SandboxResult<Argv> {
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
        // Asked before a directory is minted, and refused rather than degraded:
        // a copy-on-write workspace that quietly became an ordinary read-write
        // bind would write straight into the repository the user asked to keep
        // untouched.
        if !cow_roots.is_empty() && !details.supports_overlay() {
            return Err(SandboxError::Unsupported {
                backend: SandboxBackendKind::Bwrap,
                detail: format!(
                    "this profile asks for a copy-on-write workspace, which needs an unprivileged \
                     overlay: {}",
                    details.overlay.message()
                ),
            });
        }
        let overlays = overlay_workspaces(launch.session_key, cow_roots)?;
        let relay = local_relay_program().map(|p| p.display().to_string());
        let mut out = build_argv_with(
            program,
            launch,
            relay.as_deref(),
            &|path| self.host.path_exists(path),
            &overlays,
        )?;
        out.extend(argv);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::launcher::{RELAY_LAUNCHER, RELAY_LAUNCHER_NAME, SHELL};
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

    /// The same profile with no network at all, for the tests about argv shape
    /// rather than about egress: a filtered mode needs a running proxy, which
    /// `wrap` is right to refuse a launch without.
    fn closed_policy(paths: Vec<SandboxPath>) -> SandboxPolicy {
        let mut profile = SandboxProfile::new("dev", paths);
        profile.network_mode = NetworkMode::None;
        profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap()
    }

    /// Nothing exists — the default for a test that does not care.
    fn nothing(_: &str) -> bool {
        false
    }

    /// What the probe would have resolved: a system-installed bubblewrap.
    const PROGRAM: &str = "/usr/bin/bwrap";

    /// The command line [`BwrapBackend::probe_overlay`] asks the host, spelled
    /// once so a test scripting an answer cannot drift from the probe.
    const OVERLAY_PROBE: &str = "/usr/bin/bwrap --ro-bind / / --overlay-src /usr --tmp-overlay \
                                 /usr true";

    /// A host whose bubblewrap mounts an unprivileged overlay happily.
    fn with_overlays(host: StubHost) -> StubHost {
        host.with_command(OVERLAY_PROBE, ProbeOutput::success(""))
    }

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
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();

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
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();
        let at = index_of(&argv, "--hostname");
        assert_eq!(argv[at + 1], "friring-dev");
        // sethostname accepts a narrow charset, so the profile name is filtered.
        assert_eq!(hostname("dev.box_1"), "friring-dev-box-1");
    }

    #[test]
    fn host_minus_secrets_binds_the_whole_root_read_only_first() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();
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
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();

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
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();
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
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();

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
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();

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

    /// The host read scope binds the host root, which carries the state friring
    /// keeps for its **other** sandboxes: their places' logins (ADR-28), the
    /// markers that keep one credential to one boundary, and the generated
    /// policies constraining other sessions. Reading any of them is taking it,
    /// so they are covered rather than left readable.
    #[test]
    fn the_other_sandboxes_state_is_masked_under_the_host_read_scope() {
        let host_scope = workspace_policy();
        let scratch = dirs::session_scratch_dir("s1")
            .unwrap()
            .display()
            .to_string();
        let launch = SandboxLaunch::new(&host_scope, "/home/u", "s1").with_tmp_dir(&scratch);
        let argv = build_argv(PROGRAM, &launch, None, &|_| true).unwrap();
        for dir in [dirs::place_root(), dirs::profile_dir(), dirs::seeds_root()]
            .into_iter()
            .flatten()
            .map(|d| d.display().to_string())
        {
            assert!(has_flag(&argv, "--tmpfs", &dir), "missing mask for {dir}");
            // After the profile's own paths: a profile naming one of these is
            // refused outright, so there is no listed path to lose to the mask
            // and no ordering left in which the scope can win.
            assert!(index_of(&argv, &dir) > index_of(&argv, "--ro-bind"));
            assert!(
                !dirs::encloses(&dir, &scratch),
                "{dir} must not cover this launch's own scratch {scratch}"
            );
        }
        // The launch's own scratch is still writable, in the same tree.
        assert!(has_flag(&argv, "--bind", &scratch), "{argv:?}");

        // The workspace scope binds no host root, so it has nothing to take
        // back here either.
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.read_scope = ReadScope::Workspace;
        let narrow = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let argv = build_argv(
            PROGRAM,
            &SandboxLaunch::new(&narrow, "/home/u", "s1"),
            None,
            &|_| true,
        )
        .unwrap();
        for dir in [dirs::place_root(), dirs::profile_dir(), dirs::seeds_root()]
            .into_iter()
            .flatten()
            .map(|d| d.display().to_string())
        {
            assert!(!has_flag(&argv, "--tmpfs", &dir), "{dir} needs no mask");
        }
    }

    /// A read-only bind is no barrier to `connect(2)`, and `--unshare-net`
    /// isolates the network namespace rather than the filesystem — so the
    /// control-socket trees are covered rather than merely read-only.
    #[test]
    fn control_socket_trees_are_masked_under_the_host_read_scope() {
        let host_scope = workspace_policy();
        let launch = SandboxLaunch::new(&host_scope, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, None, &|_| true).unwrap();
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
        let argv = build_argv(PROGRAM, &launch, None, &|_| true).unwrap();
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
        let argv = build_argv(PROGRAM, &launch, None, &|_| true).unwrap();
        assert!(!has_flag(&argv, "--tmpfs", "/run"));
    }

    #[test]
    fn secrets_are_hidden_only_where_they_exist() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_agent("claude");
        let present = |p: &str| matches!(p, "/home/u/.ssh" | "/home/u/.netrc");
        let argv = build_argv(PROGRAM, &launch, None, &present).unwrap();

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
        let argv = build_argv(PROGRAM, &launch, None, &|_| true).unwrap();
        assert!(!argv.iter().any(|a| a == "/home/u/.ssh"));
    }

    #[test]
    fn the_database_is_replaced_by_dev_null_when_it_is_there() {
        let policy = policy(vec![SandboxPath::workspace("~/.local/share/friring")]);
        let db = "/home/u/.local/share/friring/friring.db";
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_friring_db(db);
        let argv = build_argv(PROGRAM, &launch, None, &|p| p == db).unwrap();
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
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();
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
        let argv = build_argv(PROGRAM, &launch, None, &|p| p == db).unwrap();
        assert!(has_mount(&argv, "--ro-bind", "/dev/null", db));
        assert!(!argv.iter().any(|a| a == &format!("{db}-wal")));
    }

    #[test]
    fn git_hooks_stay_read_only_inside_a_writable_root() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();
        let hooks = "/home/u/dev/app/.git/hooks";
        assert!(has_mount(&argv, "--ro-bind-try", hooks, hooks));
    }

    /// Only a `full` profile with nothing to take back keeps the host's network
    /// stack. A `full` that carries denies is proxied like an allowlist — those
    /// exceptions exist nowhere else — and both of the closed modes unshare.
    #[test]
    fn network_modes_decide_whether_the_stack_is_shared() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        for mode in [NetworkMode::None, NetworkMode::Allowlist] {
            profile.network_mode = mode;
            let policy = profile
                .resolve(SandboxBackendKind::Bwrap, "/home/u")
                .unwrap();
            let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
            let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();
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
        let argv = build_argv(PROGRAM, &launch, None, &nothing).unwrap();
        assert!(!argv.contains(&"--unshare-net".to_string()));

        profile.network_deny = vec!["evil.example".into()];
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_proxy(socket_endpoint());
        let argv = build_argv(PROGRAM, &launch, Some(RELAY), &|_| true).unwrap();
        assert!(
            argv.contains(&"--unshare-net".to_string()),
            "'full' with denies must not keep a direct route around them"
        );
    }

    /// The socket a launch is handed, at its own path on both sides.
    fn socket_endpoint() -> ProxyEndpoint {
        let socket = format!("{}/proxy.sock", scratch());
        ProxyEndpoint::UnixSocket {
            host_path: socket.clone(),
            inside_path: socket,
        }
    }

    /// friring's own CLI, as the launch that composed it resolved it.
    const RELAY: &str = "/usr/local/bin/friring-cli";

    /// A namespaced sandbox reaches the proxy through a socket and the relay
    /// that fronts it — never through host loopback, which does not exist in
    /// there at all.
    #[test]
    fn the_proxy_socket_is_bound_in_and_fronted_by_the_relay() {
        let policy = workspace_policy();
        let socket = format!("{}/proxy.sock", scratch());
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_proxy(socket_endpoint());
        let argv = build_argv(PROGRAM, &launch, Some(RELAY), &|_| true).unwrap();

        assert!(has_mount(&argv, "--bind", &socket, &socket));
        // The namespace still has no route out except that socket.
        assert!(argv.contains(&"--unshare-net".to_string()));

        // The relay is started inside the namespace, before the agent, with
        // every value as its own argument: nothing here is re-parsed by a
        // shell, so a path with a space or a quote in it survives.
        let listen = relay_addr().to_string();
        let end = index_of(&argv, "--");
        assert_eq!(
            &argv[end + 1..],
            [
                SHELL,
                "-c",
                RELAY_LAUNCHER,
                RELAY_LAUNCHER_NAME,
                RELAY,
                listen.as_str(),
                socket.as_str(),
            ]
        );
        // The relay listens on the sandbox's *own* loopback.
        assert!(relay_addr().ip().is_loopback());

        let loopback = SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_proxy(ProxyEndpoint::Loopback { port: 8123 });
        let err = build_argv(PROGRAM, &loopback, Some(RELAY), &|_| true).unwrap_err();
        assert!(err.to_string().contains("no route to host loopback"));
    }

    /// The launcher's `shift` has to land exactly on the agent, so this **runs**
    /// the script instead of reading it, over the positionals `build_argv`
    /// actually emitted. A miscount leaves the sandbox executing a socket path
    /// or losing the agent's first argument, and both are silent in an argv
    /// assertion.
    ///
    /// Nothing starts and nothing leaves the machine: the relay's place is taken
    /// by a path that does not exist (its failure goes to `/dev/null`, which is
    /// the design), and the agent's place by `echo`.
    #[cfg(unix)]
    #[test]
    fn the_launcher_shifts_past_the_relay_and_becomes_the_agent() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_proxy(socket_endpoint());
        let argv = build_argv(PROGRAM, &launch, Some(RELAY), &|_| true).unwrap();
        let launcher = &argv[index_of(&argv, "--") + 1..];

        let output = std::process::Command::new(&launcher[0])
            .args(&launcher[1..])
            // What `wrap` appends: the agent's own argv, unchanged.
            .args(["/bin/echo", "the-agent", "--resume=abc"])
            .output()
            .expect("the launcher runs");

        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "the-agent --resume=abc",
            "the launcher exec'd the wrong thing: {output:?}"
        );
    }

    /// Without the relay there is no egress at all, so a launch that cannot
    /// find friring's CLI is refused rather than started blind. `exists` is the
    /// second half of the same question: a path that was resolved once and has
    /// since been moved is no better than none.
    #[test]
    fn a_launch_that_needs_a_relay_and_has_none_is_refused() {
        let policy = workspace_policy();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_proxy(socket_endpoint());
        for relay in [None, Some(RELAY)] {
            let err = build_argv(PROGRAM, &launch, relay, &nothing).unwrap_err();
            assert!(matches!(err, SandboxError::Refused { .. }), "{err}");
            assert!(err.to_string().contains("friring-cli"), "{err}");
        }
        // A launch with nothing to relay to needs none.
        let closed = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = build_argv(PROGRAM, &closed, None, &nothing).unwrap();
        assert!(!argv.contains(&SHELL.to_string()));
    }

    /// The narrow read scope builds its root out of the system directories, so
    /// friring's CLI has to be bound in explicitly — before the profile's own
    /// paths, so a path the user listed still wins.
    #[test]
    fn the_workspace_scope_binds_the_relay_binary_in() {
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.read_scope = ReadScope::Workspace;
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1").with_proxy(socket_endpoint());
        let argv = build_argv(PROGRAM, &launch, Some(RELAY), &|_| true).unwrap();
        assert!(has_mount(&argv, "--ro-bind", RELAY, RELAY));
        assert!(index_of(&argv, RELAY) < index_of(&argv, "/home/u/dev/app"));

        // The host read scope already carries it, at its own path, so binding
        // it again would only narrow what the profile granted.
        let host_scope = workspace_policy();
        let launch = SandboxLaunch::new(&host_scope, "/home/u", "s1").with_proxy(socket_endpoint());
        let argv = build_argv(PROGRAM, &launch, Some(RELAY), &|_| true).unwrap();
        assert!(!has_mount(&argv, "--ro-bind", RELAY, RELAY));
    }

    #[test]
    fn the_agent_argv_is_appended_after_the_separator() {
        // `none`: this is about argv order, and a proxied launch would put the
        // relay's launcher between the separator and the agent.
        let mut profile = SandboxProfile::new("dev", vec![SandboxPath::workspace("~/dev/app")]);
        profile.network_mode = NetworkMode::None;
        let policy = profile
            .resolve(SandboxBackendKind::Bwrap, "/home/u")
            .unwrap();
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
        let modern = BwrapBackend::new(Arc::new(with_overlays(StubHost::linux_with_bwrap(
            "0.11.0",
        ))));
        assert_eq!(modern.probe().message(), "bubblewrap 0.11");
        assert!(modern.details().supports_overlay());

        let old = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.8.0")));
        assert!(old.probe().is_available());
        assert!(
            !old.details().supports_overlay(),
            "copy-on-write workspaces need 0.11"
        );
        // The version is the cheap half of the answer, so it is the one quoted:
        // nothing about this host would change it.
        let message = old.details().overlay.message();
        assert!(message.contains("bubblewrap 0.8"), "{message}");
        assert!(message.contains("upgrade bubblewrap to 0.11"), "{message}");
    }

    /// A setuid bubblewrap is a version check's blind spot: it reports 0.11 and
    /// then refuses every overlay it is asked for. So the probe *mounts* one,
    /// and the profile editor and the launch both get bwrap's own reason.
    #[test]
    fn a_setuid_bwrap_reports_overlays_as_unavailable_with_the_reason() {
        let setuid = StubHost::linux_with_bwrap("0.11.0").with_command(
            OVERLAY_PROBE,
            ProbeOutput::failure(
                1,
                "bwrap: Unable to create overlay filesystem in setuid mode\n",
            ),
        );
        let backend = BwrapBackend::new(Arc::new(setuid));
        // The backend itself is fine — only the copy-on-write mode is not.
        assert!(backend.probe().is_available());
        assert!(!backend.details().supports_overlay());
        let message = backend.details().overlay.message();
        assert!(message.contains("installed setuid"), "{message}");
        assert!(message.contains("/usr/bin/bwrap"), "{message}");

        // A kernel that refuses an unprivileged overlay says so in its own
        // words rather than being reported as a setuid install.
        let old_kernel = StubHost::linux_with_bwrap("0.11.0").with_command(
            OVERLAY_PROBE,
            ProbeOutput::failure(1, "bwrap: Can't mount overlayfs: Operation not permitted\n"),
        );
        assert_eq!(
            BwrapBackend::new(Arc::new(old_kernel))
                .details()
                .overlay
                .message(),
            "bwrap: Can't mount overlayfs: Operation not permitted"
        );
    }

    /// The refusal that keeps the mode honest: a host that cannot overlay must
    /// not quietly bind the root read-write instead, because the whole point of
    /// the mode is that the real directory is not written to.
    #[test]
    fn a_host_without_overlays_refuses_a_copy_on_write_launch() {
        let backend = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.8.0")));
        let policy = closed_policy(vec![SandboxPath::workspace("~/dev/app")]);
        let launch = SandboxLaunch::new(&policy, "/home/u", "cow-refused");
        let err = backend
            .wrap_copy_on_write(
                vec!["claude".into()],
                &launch,
                &["/home/u/dev/app".to_string()],
            )
            .unwrap_err();
        assert!(matches!(err, SandboxError::Unsupported { .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains("copy-on-write"), "{text}");
        assert!(text.contains("upgrade bubblewrap"), "{text}");
        // Nothing was minted for a launch that was never composed.
        assert!(!overlay_root()
            .expect("a data directory")
            .join("cow-refused")
            .exists());
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
        let policy = closed_policy(vec![SandboxPath::workspace("~/dev/app")]);
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv = system.wrap(vec!["claude".into()], &launch).unwrap();
        assert_eq!(argv[0], PROGRAM);
    }

    #[test]
    fn a_profile_that_hands_over_bwrap_itself_is_refused_at_launch() {
        // The probe vetted the binary against the host; this profile is what
        // decides whether the agent can rewrite it.
        let system = BwrapBackend::new(Arc::new(StubHost::linux_with_bwrap("0.11.0")));
        let policy = closed_policy(vec![SandboxPath::workspace("/usr/bin")]);
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

    /// A copy-on-write root, as a launch that never touched the disk sees one.
    fn workspace_at(root: &str) -> OverlayWorkspace {
        OverlayWorkspace {
            root: root.to_string(),
            upper: format!("/data/sandbox/overlay/s1/{}/upper", dirs::digest(root)),
            work: format!("/data/sandbox/overlay/s1/{}/work", dirs::digest(root)),
        }
    }

    /// The overlay replaces the writable bind at exactly the same point in the
    /// ordering, so the merged view lands at the root's own path — identical
    /// absolute paths — and everything emitted after it still overrides it.
    #[test]
    fn a_copy_on_write_root_is_overlaid_at_its_own_path() {
        let policy = closed_policy(vec![SandboxPath::workspace("~/dev/app")]);
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let ws = workspace_at("/home/u/dev/app");
        let argv =
            build_argv_with(PROGRAM, &launch, None, &nothing, std::slice::from_ref(&ws)).unwrap();

        // `--overlay-src` is consumed by the option that follows it, so the two
        // are adjacent: a mount emitted between them would take the lower layer.
        let at = index_of(&argv, "--overlay-src");
        assert_eq!(
            &argv[at..at + 6],
            [
                "--overlay-src",
                &ws.root,
                "--overlay",
                &ws.upper,
                &ws.work,
                &ws.root,
            ]
        );
        // And the root is not *also* bound read-write, which would put the
        // agent's writes straight into the real directory.
        assert!(!has_mount(&argv, "--bind", &ws.root, &ws.root));
    }

    /// The precedence rule is the mount order, and an overlay is emitted in the
    /// same sorted pass as every other mount — so a read-only path nested in a
    /// copy-on-write root still wins, and so does a read-write one.
    #[test]
    fn nesting_inside_a_copy_on_write_root_still_resolves_most_specific_first() {
        let policy = closed_policy(vec![
            SandboxPath::workspace("/repo"),
            SandboxPath::read_only("/repo/vendor"),
            SandboxPath::workspace("/repo/vendor/cache"),
        ]);
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let argv =
            build_argv_with(PROGRAM, &launch, None, &nothing, &[workspace_at("/repo")]).unwrap();

        let overlay = index_of(&argv, "--overlay");
        let vendor = index_of(&argv, "/repo/vendor");
        let cache = index_of(&argv, "/repo/vendor/cache");
        assert!(overlay < vendor && vendor < cache, "{argv:?}");
        // The nested pair are ordinary binds of the *real* directories, so a
        // read-only descendant is still read-only and a read-write one still
        // writes through to the host.
        assert!(has_mount(
            &argv,
            "--ro-bind",
            "/repo/vendor",
            "/repo/vendor"
        ));
        assert!(has_mount(
            &argv,
            "--bind",
            "/repo/vendor/cache",
            "/repo/vendor/cache"
        ));
    }

    /// ADR-29 and the secret masks are emitted after the mount pass, so they
    /// still win over an overlay — a database sidecar written inside the
    /// boundary would otherwise land in the upper layer and be replayed by the
    /// host on next open.
    #[test]
    fn the_database_and_secret_masks_still_win_over_an_overlay() {
        let db = "/home/u/.local/share/friring/friring.db";
        let policy = policy(vec![SandboxPath::workspace("~/work")]);
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1")
            .with_agent("claude")
            .with_friring_db(db);
        let present = |p: &str| p == "/home/u/.ssh" || p == db;
        let argv = build_argv_with(
            PROGRAM,
            &launch,
            None,
            &present,
            &[workspace_at("/home/u/work")],
        )
        .unwrap();

        for file in [db, "/home/u/.ssh"] {
            assert!(
                index_of(&argv, file) > index_of(&argv, "--overlay"),
                "{file} must be taken back after the overlay"
            );
        }
        assert!(has_mount(&argv, "--ro-bind", "/dev/null", db));
        assert!(has_flag(&argv, "--tmpfs", "/home/u/.ssh"));
        // `.git/hooks` is the same rule: bound read-only over the merged view,
        // because the *host's* git runs whatever is in the real directory.
        let hooks = "/home/u/work/.git/hooks";
        assert!(has_mount(&argv, "--ro-bind-try", hooks, hooks));
        assert!(index_of(&argv, hooks) > index_of(&argv, "--overlay"));
    }

    /// An overlay narrows a grant the profile already made. One naming a path
    /// the launch does not grant read-write would be a grant that no check ran
    /// against — the database, tmux-socket and engine-socket refusals all read
    /// the writable set.
    #[test]
    fn an_overlay_may_not_name_a_path_the_launch_does_not_grant() {
        let policy = closed_policy(vec![SandboxPath::workspace("~/dev/app")]);
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let err = build_argv_with(
            PROGRAM,
            &launch,
            None,
            &nothing,
            &[workspace_at("/home/u/secrets")],
        )
        .unwrap_err();
        assert!(matches!(err, SandboxError::Refused { .. }), "{err}");
        assert!(err.to_string().contains("narrows a grant"), "{err}");

        // Nor may two of them stack: the inner overlay's lower layer would be
        // the outer overlay's merged view, which friring will not reason about.
        let policy = closed_policy(vec![
            SandboxPath::workspace("/repo"),
            SandboxPath::workspace("/repo/sub"),
        ]);
        let launch = SandboxLaunch::new(&policy, "/home/u", "s1");
        let err = build_argv_with(
            PROGRAM,
            &launch,
            None,
            &nothing,
            &[workspace_at("/repo"), workspace_at("/repo/sub")],
        )
        .unwrap_err();
        assert!(err.to_string().contains("stack"), "{err}");
    }

    /// The layers are minted outside every sandbox's reach, adopted across a
    /// relaunch, and dropped with the session.
    ///
    /// The escape this placement closes: the per-session scratch directory is
    /// bind-mounted read-write into the sandbox, so a layer under it is a path
    /// the agent can replace with a symlink — and an `upperdir` pointing at `/`
    /// would put the next launch's every write on the host root.
    #[test]
    fn overlay_layers_live_outside_the_sandbox_and_survive_a_relaunch() {
        let root = "/home/u/dev/app".to_string();
        let first = overlay_workspaces("layers-test", std::slice::from_ref(&root)).unwrap();
        assert_eq!(first.len(), 1);
        let layers = &first[0];
        assert!(std::path::Path::new(&layers.upper).is_dir());
        assert!(std::path::Path::new(&layers.work).is_dir());

        // Neither layer is under the directory the sandbox is handed, and both
        // are inside the tree friring keeps for itself.
        let scratch = dirs::session_scratch_dir("layers-test")
            .unwrap()
            .display()
            .to_string();
        let tree = dirs::sandbox_root().unwrap().display().to_string();
        for path in [&layers.upper, &layers.work] {
            assert!(
                !dirs::encloses(&scratch, path),
                "{path} is inside {scratch}"
            );
            assert!(dirs::encloses(&tree, path), "{path} is outside {tree}");
        }
        // And no profile could ever name them: they are under the sandbox tree
        // that `check_declared_paths` refuses in either mode.
        assert!(dirs::check_declared_paths(std::slice::from_ref(&layers.upper)).is_err());

        // A relaunch adopts what is there rather than wiping the agent's work.
        std::fs::write(std::path::Path::new(&layers.upper).join("kept"), "x").unwrap();
        let again = overlay_workspaces("layers-test", std::slice::from_ref(&root)).unwrap();
        assert_eq!(again, first);
        assert!(std::path::Path::new(&layers.upper).join("kept").exists());

        cleanup_overlays("layers-test");
        assert!(!std::path::Path::new(&layers.upper).exists());
    }

    /// The layer directories are friring's own, and a link where one belongs is
    /// interference rather than a profile a user should edit — so it refuses the
    /// launch outright instead of routing through `allow_unsandboxed_fallback`.
    #[cfg(unix)]
    #[test]
    fn a_layer_directory_replaced_by_a_link_refuses_the_launch_as_tampering() {
        let base = overlay_root().expect("a data directory");
        let key = "layers-tampered";
        dirs::create_private_dir(&base.join(key).join(dirs::digest("/repo"))).unwrap();
        let planted = base.join(key).join(dirs::digest("/repo")).join("upper");
        let _ = std::fs::remove_dir_all(&planted);
        std::os::unix::fs::symlink(dirs::host_temp_root(), &planted).unwrap();

        let err = overlay_workspaces(key, &["/repo".to_string()]).unwrap_err();
        assert!(err.is_tampering(), "{err}");
        cleanup_overlays(key);
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
