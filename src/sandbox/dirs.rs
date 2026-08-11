//! The directories friring mints for a sandbox, and the host locations no
//! sandbox may be handed.
//!
//! Two halves, both boundary-critical.
//!
//! **What friring mints** lives under `<data dir>/sandbox`: a per-session
//! scratch directory the agent writes ([`create_session_scratch`]) and the
//! generated seatbelt profiles ([`profile_dir`]). Neither may be the host temp
//! root. Granting a sandbox `/tmp` grants it `/tmp/tmux-$UID/…`, where friring's
//! own tmux server listens — and a network namespace does not stop `connect(2)`
//! on a pathname unix socket, so a writable host temp root is a complete escape:
//! the agent drives the host tmux server and runs commands outside the boundary.
//! The same grant covers wherever the generated `.sb` profiles are written, so a
//! sandbox could swap the policy that constrains it.
//!
//! **What friring refuses to hand over** is [`check_writable_roots`]: a
//! read-write root that encloses the data directory reaches the database, which
//! ADR-29 keeps outside every boundary because automations stored in it are
//! shell commands the *host* runs; one that reaches a tmux socket directory is
//! the escape above. Both are refused at launch rather than trimmed, because a
//! boundary that silently grants less than it was asked for is as surprising as
//! one that grants more. [`grants_engine_socket`] is the third of those
//! locations and belongs to the place backends: a sandbox holding a container
//! engine's control socket can start a privileged container of its own.
//!
//! Every one of those comparisons is made against a path **as the kernel
//! resolves it** ([`canonical`], [`canonical_source`], [`reaches`]). A guard
//! that compares literal strings misses `/private/tmp` on macOS — where `/tmp`,
//! and therefore friring's own tmux socket directory, actually lives — and
//! misses anything reached through a symlink a sandboxed agent planted. The one
//! place that is loosened is friring's own tree ([`place_mount_source`]), which
//! is judged only from [`place_root`] down: everything above it is unreachable
//! from any sandbox, and a machine whose data directory sits behind a link would
//! otherwise be refused its own egress directory.

use std::path::{Path, PathBuf};

use crate::sandbox::backend::{SandboxError, SandboxResult};

/// Everything friring generates for a sandbox, under the data directory.
///
/// `paths::log_directory` *is* the data directory (`PathKind::LogDir` resolves
/// to `<data>/`), which is why it anchors this. `None` when friring cannot
/// resolve a home to hang it off — the callers all fail the launch, because
/// there is nowhere left that is both writable by friring and unreachable from
/// inside a sandbox.
pub fn sandbox_root() -> Option<PathBuf> {
    data_dir().map(|data| data.join("sandbox"))
}

/// friring's data directory: the tree ADR-29 keeps out of every sandbox.
pub fn data_dir() -> Option<PathBuf> {
    crate::paths::log_directory()
}

/// Where the generated seatbelt profiles are written.
///
/// Deliberately *not* under the host temp directory: the profile file is the
/// policy, so a sandbox that can write it can rewrite what constrains it. Here
/// it sits inside the data directory, which [`check_writable_roots`] refuses to
/// let any profile enclose.
pub fn profile_dir() -> Option<PathBuf> {
    sandbox_root().map(|root| root.join("profiles"))
}

/// Parent of every per-session scratch directory.
pub fn scratch_root() -> Option<PathBuf> {
    sandbox_root().map(|root| root.join("tmp"))
}

/// Where the one-copy-per-credential-family markers live (ADR-28).
///
/// Inside the data directory rather than in a place's own tree, because the
/// record that refuses a *second* copy is worthless if the sandbox holding the
/// first one can delete it.
pub fn seeds_root() -> Option<PathBuf> {
    sandbox_root().map(|root| root.join("seeds"))
}

/// Parent of every place's own tree — one directory per profile, because a
/// place backend's environment is per profile and outlives any single session.
///
/// Short on purpose (`pl`, not `places`): everything under here is a candidate
/// prefix for a unix socket path, and `sun_path` is 104 bytes including the
/// NUL on the tightest supported platform.
pub fn place_root() -> Option<PathBuf> {
    sandbox_root().map(|root| root.join("pl"))
}

/// One profile's place tree, whether or not it exists yet.
pub fn place_dir(profile: &str) -> Option<PathBuf> {
    place_root().map(|root| root.join(sanitize_component(profile)))
}

/// The synthetic home a profile's place gives the agent.
///
/// A place gets a **per-profile home, never a bind of the host's** agent
/// configuration (ADR-28): the host's credentials are rotating single-use
/// tokens, and a writable host agent configuration is an escape channel through
/// hooks the *host* agent later runs. This directory is friring's, created by
/// friring, and mounted at a fixed path inside — the one deliberate exception to
/// identical absolute paths, because `$HOME` is not a path an agent keys project
/// state by, and the host's own home path may not even exist inside a place.
pub fn place_home_dir(profile: &str) -> Option<PathBuf> {
    place_dir(profile).map(|dir| dir.join("home"))
}

/// The per-session directory a place-backed launch keeps its egress socket in,
/// mounted into the place at **exactly this path** so one string names the
/// socket on both sides of the boundary.
///
/// Keyed by a hash of the session rather than by the session id itself: a place
/// mounts one directory per profile and the socket path underneath it has to fit
/// `sun_path`, which a 36-character UUID plus the data directory does not
/// reliably leave room for. The hash is [`digest`], so the same session gets the
/// same directory across a relaunch and a friring restart.
pub fn place_session_dir(profile: &str, session_key: &str) -> Option<PathBuf> {
    place_dir(profile).map(|dir| dir.join(digest(session_key)))
}

/// Create (or adopt) the per-profile place tree: the directory mounted into the
/// place for egress sockets, and the synthetic home. Both `0700`, both refusing
/// a symlink, exactly like the per-session scratch.
///
/// Returns `(place directory, home directory)`. Called before a place is
/// created, because a bind mount's source has to exist first — an engine that
/// creates a missing source does it as root, and a directory the sandbox's user
/// cannot write is a place whose agent dies on first launch.
pub fn create_place_dirs(profile: &str) -> SandboxResult<(PathBuf, PathBuf)> {
    let dir = place_dir(profile).ok_or_else(no_data_dir)?;
    let home = place_home_dir(profile).ok_or_else(no_data_dir)?;
    create_private_dir(&dir)?;
    create_private_dir(&home)?;
    Ok((dir, home))
}

/// Create (or adopt) one place-backed session's egress directory, `0700`.
///
/// Idempotent for the same reason [`create_session_scratch`] is: a relaunch of a
/// session that crashed has to reuse what is there rather than fail.
pub fn create_place_session_dir(profile: &str, session_key: &str) -> SandboxResult<PathBuf> {
    let dir = place_session_dir(profile, session_key).ok_or_else(no_data_dir)?;
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Drop one session's directory inside whichever place it ran in.
///
/// Teardown holds a session key and not a profile: the place outlives the
/// session, and an edited profile may have moved the session into a different
/// one since. So the directory is found by its digest under every place rather
/// than by a profile the caller would have to have kept. Best effort — what is
/// left behind costs disk, and the next launch of the same session adopts it.
pub fn cleanup_place_session(session_key: &str) {
    let Some(root) = place_root() else {
        return;
    };
    let digest = digest(session_key);
    let Ok(places) = std::fs::read_dir(root) else {
        return;
    };
    for place in places.flatten() {
        let _ = std::fs::remove_dir_all(place.path().join(&digest));
    }
}

/// Drop one profile's whole place tree — its synthetic home and every session
/// directory under it.
///
/// Best effort, and *not* something a session teardown does: the tree belongs to
/// the profile, so it outlives every session in it and goes when the place does.
///
/// Never while a container of that profile is running. The tree is bind-mounted
/// into it, so removing it takes `$HOME` away from a live agent mid-turn,
/// unlinks the egress sockets its siblings are talking through, and destroys the
/// login — see [`reclaim_orphan_places`], which is where a tree whose profile is
/// gone is actually collected.
pub fn cleanup_place(profile: &str) {
    if let Some(dir) = place_dir(profile) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Drop the place trees that belong to no profile any more, keeping the ones
/// `keep` names.
///
/// The collecting half of [`cleanup_place`]. A profile is deleted while its
/// sessions are still running in its container, and the tree has to outlive that
/// delete — so something later has to notice that the profile is gone *and* that
/// nothing is running in it. That is this, driven by the same pass that reclaims
/// the containers, after it has reclaimed them.
///
/// Both arguments are profile **names**; the directory they own is
/// [`place_dir`]'s, so the comparison is made on the same sanitised component
/// that minted it. Anything under the root that no name accounts for is a tree
/// whose profile was deleted, or one a crashed run left behind.
///
/// Best effort, like every other reclaim: what will not go costs disk, and the
/// next pass tries again.
pub fn reclaim_orphan_places(known: &[String], keep: &[String]) {
    let Some(root) = place_root() else {
        return;
    };
    let accounted: Vec<String> = known
        .iter()
        .chain(keep)
        .map(|name| sanitize_component(name))
        .collect();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if accounted.contains(&name) {
            continue;
        }
        // `remove_dir_all` follows no symlink at its argument (it errors on
        // one), and this root is inside the data directory that no sandbox may
        // be handed, so there is nothing here to redirect it.
        let _ = std::fs::remove_dir_all(root.join(&name));
    }
}

/// A stable 64-bit digest of `value`, as 16 lowercase hex digits.
///
/// FNV-1a, written out rather than taken from `DefaultHasher`: the standard
/// library's hasher is explicitly allowed to change between releases, and two of
/// the things this names — a place's session directory and the label that
/// decides whether an existing container still matches its profile — have to
/// mean the same thing to the friring that created them and the one that finds
/// them later. Not a security primitive: nothing here resists a collision an
/// attacker chooses, it only has to be stable and cheap.
pub fn digest(value: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// Where `program` sits if it is somewhere a sandboxed agent could rewrite, and
/// `None` when it is not.
///
/// The rule every backend applies to the binary that *is* its boundary: bwrap
/// applies the policy, and a container engine's CLI is what asks the daemon for
/// the isolation. A copy under the home directory (`~/.local/bin` is on most
/// users' `PATH` and inside the default read scope's writable set) or in a
/// world-writable scratch directory is a boundary the sandboxed agent chooses,
/// so a backend that resolves one refuses rather than falling back to the next
/// `PATH` entry — which would still be running whatever an attacker arranged to
/// be found.
pub fn rewritable_root(program: &str, home: Option<&str>) -> Option<String> {
    let mut roots: Vec<String> = ["/tmp", "/var/tmp", "/dev/shm"]
        .iter()
        .map(|d| (*d).to_string())
        .collect();
    roots.extend(home.map(str::to_string));
    roots.extend(sandbox_root().map(|p| p.display().to_string()));
    roots.into_iter().find(|root| encloses(root, program))
}

/// The scratch directory one session's agent may write, whether or not it
/// exists yet.
pub fn session_scratch_dir(session_key: &str) -> Option<PathBuf> {
    scratch_root().map(|root| root.join(sanitize_component(session_key)))
}

/// Create (or adopt) the per-session scratch directory, `0700`.
///
/// Idempotent on purpose: a session that crashed leaves its directory behind and
/// the next launch of the same session must reuse it rather than fail. What it
/// will not do is adopt something that is not a directory friring owns — a
/// symlink there would redirect every scratch write to wherever it points.
pub fn create_session_scratch(session_key: &str) -> SandboxResult<PathBuf> {
    let dir = session_scratch_dir(session_key).ok_or_else(no_data_dir)?;
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Drop everything one session left behind: its scratch directory and the
/// seatbelt profile generated for it.
///
/// Best effort — this runs when a session ends, and a failure to clean up must
/// never be the thing that reports an error. Nothing here follows a symlink:
/// `remove_dir_all` refuses one, and unlinking never traverses the final
/// component.
pub fn cleanup_session(session_key: &str) {
    let key = sanitize_component(session_key);
    if let Some(dir) = scratch_root().map(|root| root.join(&key)) {
        let _ = std::fs::remove_dir_all(dir);
    }
    let Some(profiles) = profile_dir() else {
        return;
    };
    let suffix = format!("-{key}.sb");
    let Ok(entries) = std::fs::read_dir(profiles) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().ends_with(&suffix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// What `path` resolves to on the filesystem friring runs on, or `None` when it
/// cannot be resolved at all (absent, unreadable, or an answer that is not
/// UTF-8).
///
/// Not a guard on its own — it is what the guards compare *with*. A boundary
/// check that compares the strings a user wrote is a check the kernel does not
/// make: it resolves every component, so `/private/tmp/tmux-501` and
/// `/tmp/tmux-501` are one directory on macOS and a literal comparison sees two.
pub fn canonical(path: &str) -> Option<String> {
    std::fs::canonicalize(path)
        .ok()
        .and_then(|resolved| resolved.to_str().map(str::to_string))
}

/// A bind-mount source as the kernel will resolve it, or the reason friring
/// will not name it in a mount.
///
/// **Fail closed**, and deliberately stricter than "resolve it and mount that".
/// The string friring checks is the string the engine hands over, and the kernel
/// resolves it *again* when it sets the bind up — so a source that travels
/// through a symlink is a source whose meaning the sandbox can change after the
/// check. That is reachable without any user error: a place's plan adds
/// `<writable root>/.git/hooks` wherever it exists, so an agent inside a place
/// can plant that path as a link to friring's tmux socket directory and have the
/// next ensure mount it. A **read-only** bind would be enough — a read-only
/// superblock does not take write permission away from a socket inode, so
/// `connect(2)` still succeeds and the agent drives the host's tmux server.
///
/// So a source is refused when any component of it is a symlink, when a
/// component cannot be inspected, or when the whole path resolves anywhere but
/// to itself. The refusal names the component, because that is what the user has
/// to act on. A legitimately symlinked path is refused too: friring cannot tell
/// one from the other, and the half of that trade that costs a message is the
/// safe half.
///
/// The one answer that is not a refusal is a path that **is not on this
/// filesystem at all**. A place's host is not necessarily friring's own — a
/// remote engine is probed over ssh — and there the launch's own `exists`
/// predicate is the only authority there is, so this says nothing rather than
/// refusing a path it cannot see. Nothing local slips through that way: a path
/// that is genuinely here and genuinely missing has already been refused for not
/// existing, a dangling link is a symlink before it is a missing file, and a
/// component friring is not allowed to look at is refused rather than skipped —
/// a rootful engine's daemon resolves the same string as root and is not stopped
/// by the bits that stopped this.
pub fn canonical_source(path: &str) -> Result<String, String> {
    if !walk_components(Path::new(""), Path::new(path))? {
        return Ok(path.to_string());
    }
    match std::fs::canonicalize(path) {
        Ok(resolved) => match resolved.to_str() {
            Some(resolved) if normalize(resolved) == normalize(path) => Ok(resolved.to_string()),
            Some(resolved) => Err(format!("'{path}' resolves to '{resolved}'")),
            None => Err(format!(
                "'{path}' resolves to a path that is not valid UTF-8, which friring cannot name \
                 exactly"
            )),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_string()),
        Err(error) => Err(format!("'{path}' cannot be resolved ({error})")),
    }
}

/// The resolver a place's mounts are checked with: [`canonical_source`], except
/// under friring's own place tree, where [`canonical_source_below`] applies.
pub fn place_mount_source(path: &str) -> Result<String, String> {
    match place_root() {
        Some(root) => canonical_source_below(path, &root.display().to_string()),
        None => canonical_source(path),
    }
}

/// [`canonical_source`] for a path friring mints itself, checked only where a
/// sandbox could have interfered.
///
/// The strict rule is right for a path a *profile* names and wrong for one
/// friring builds, because it judges the whole ancestor chain: `/var` is a
/// symlink to `/private/var` on macOS, and any host whose home or
/// `$XDG_DATA_HOME` sits behind a link has one too — so a place would refuse to
/// start over its own egress directory, and no user could act on the message.
///
/// The part above `trusted` is not a path any sandbox can reach: friring's place
/// tree hangs off the data directory, which no mount may carry at all (ADR-29),
/// so a symlink there is one the machine's owner put there. The part *below* it
/// is another matter — the place directory is mounted read-write, so the
/// synthetic home inside it is exactly what an agent can replace with a link of
/// its own — and that is checked as strictly as any profile path.
///
/// A path that is not under `trusted` gets the strict rule; there is nothing to
/// vouch for it.
pub fn canonical_source_below(path: &str, trusted: &str) -> Result<String, String> {
    let Some(rest) = strip_ancestor(path, trusted) else {
        return canonical_source(path);
    };
    if Path::new(&rest)
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "'{path}' does not name a plain directory under '{trusted}'"
        ));
    }
    let base = PathBuf::from(normalize(trusted));
    if !walk_components(&base, Path::new(&rest))? {
        return Ok(path.to_string());
    }
    // The trusted prefix as the kernel resolves it, so what comes back is still
    // the sharpest form the protected-directory comparisons can be made against.
    let resolved = canonical(trusted).unwrap_or_else(|| normalize(trusted));
    Ok(format!("{}/{rest}", resolved.trim_end_matches('/')))
}

/// Walk each component of `rest` under `base`, refusing the first that is a
/// symlink or that cannot be inspected.
///
/// `Ok(false)` means the walk ran off the end of what exists on this filesystem
/// — not a refusal, for the reason [`canonical_source`] gives. The walk is
/// component by component rather than a comparison against [`canonical`],
/// because the answer is the message: "'/repo/.git/hooks' is a symlink to
/// '/tmp'" tells the user what to do, and "the path resolves elsewhere" does
/// not.
fn walk_components(base: &Path, rest: &Path) -> Result<bool, String> {
    let mut walked = base.to_path_buf();
    for component in rest.components() {
        walked.push(component);
        match std::fs::symlink_metadata(&walked) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let named = walked.display();
                return Err(match std::fs::read_link(&walked) {
                    Ok(target) => format!("'{named}' is a symlink to '{}'", target.display()),
                    Err(_) => format!("'{named}' is a symlink"),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "'{}' cannot be inspected ({error}), so friring cannot tell where it leads",
                    walked.display()
                ))
            }
        }
    }
    Ok(true)
}

/// Whether `path` is, or contains, `protected` — judged against `protected`
/// both as written and as the kernel resolves it.
///
/// The second half is not belt and braces. `/tmp` is a symlink to `/private/tmp`
/// on macOS, so a canonical path naming the resolved spelling reaches the tmux
/// socket directory while a comparison against the literal one says it does not.
pub fn reaches(path: &str, protected: &str) -> bool {
    encloses(path, protected)
        || canonical(protected).is_some_and(|resolved| encloses(path, &resolved))
}

/// Whether `path` and `tree` overlap at all — either enclosing the other, in
/// either spelling.
///
/// For the trees whose *contents* are the danger rather than one named file:
/// naming `/run/user/1000` hands over the engine socket inside it, and naming
/// `/run/user` hands over every user's.
fn overlaps(path: &str, tree: &str) -> bool {
    let resolved = canonical(tree);
    encloses(path, tree)
        || encloses(tree, path)
        || resolved.is_some_and(|resolved| encloses(path, &resolved) || encloses(&resolved, path))
}

/// The host's temp root — what the sandbox used to be handed, and what it must
/// never be handed again.
pub fn host_temp_root() -> PathBuf {
    std::env::temp_dir()
}

/// A directory of a test's own, **resolved**.
///
/// Shared by every test that plants a symlink, because they all need the same
/// fixture: the platform temp root is itself symlinked on macOS (`/var` →
/// `/private/var`), so a tree built on the unresolved spelling is refused by the
/// very check the test is exercising, for a reason the test is not about.
#[cfg(test)]
pub(crate) fn test_temp_base(name: &str) -> PathBuf {
    let base = host_temp_root().join(format!("friring-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("a fabricated directory");
    std::fs::canonicalize(&base).expect("a resolvable directory")
}

/// Where tmux keeps its per-user socket directories: `$TMUX_TMPDIR` when set,
/// `/tmp` otherwise — the rule tmux itself applies, so friring's own server
/// socket is under here.
///
/// The `tmux-<uid>` leaf is deliberately not computed. Treating the whole root
/// as the thing to protect needs no uid lookup and is a superset, and the one
/// case a superset would over-refuse — a read-write path *under* the root that
/// is not a socket directory — is settled by `grants_tmux_sockets`, which looks
/// for the `tmux-` prefix instead.
pub fn tmux_socket_root() -> PathBuf {
    match std::env::var_os("TMUX_TMPDIR").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("/tmp"),
    }
}

/// Whether a read-write root would hand a sandbox a tmux server socket
/// directory — either by enclosing the whole root, or by naming (or reaching
/// into) one of its `tmux-<uid>` children.
fn grants_tmux_sockets(root: &str, socket_root: &str) -> bool {
    if encloses(root, socket_root) {
        return true;
    }
    let Some(rest) = strip_ancestor(root, socket_root) else {
        return false;
    };
    rest.split('/')
        .next()
        .is_some_and(|first| first.starts_with("tmux-"))
}

/// Which spelling of the tmux socket directory `path` would hand over, or
/// `None`.
pub fn grants_tmux_socket_tree(path: &str) -> Option<String> {
    grants_socket_tree_under(path, &tmux_socket_root().display().to_string())
}

/// [`grants_tmux_sockets`] against `socket_root` as written *and* as the kernel
/// resolves it: on macOS the root is `/tmp`, which is a symlink to
/// `/private/tmp`, so a path naming the resolved spelling grants friring's own
/// tmux socket while a literal comparison misses it entirely.
///
/// Split from [`grants_tmux_socket_tree`] so the rule can be stated against a
/// root a test planted, rather than against one the whole process shares
/// through `$TMUX_TMPDIR`.
fn grants_socket_tree_under(path: &str, socket_root: &str) -> Option<String> {
    let resolved = canonical(socket_root).filter(|resolved| resolved != socket_root);
    [Some(socket_root.to_string()), resolved]
        .into_iter()
        .flatten()
        .find(|root| grants_tmux_sockets(path, root))
}

/// Which container engine control socket `path` would carry across a boundary,
/// or `None`.
///
/// **A sandbox holding the engine socket owns the host.** The socket is the
/// whole of docker's and podman's authorisation: anything that can speak to it
/// can start a container with `--privileged` and the host's root filesystem
/// bind-mounted into it, as root, outside every boundary friring set. So this is
/// refused outright rather than offered as an escape hatch the way an explicit
/// loopback address is — that hatch shares one service the user named, and no
/// profile can coherently intend to grant the ability to create privileged
/// containers.
///
/// `home` is the home directory on the host the engine runs on, for the
/// per-user sockets Docker Desktop and `podman machine` keep there. `None` only
/// narrows what can be found.
pub fn grants_engine_socket(path: &str, home: Option<&str>) -> Option<String> {
    engine_socket_files()
        .into_iter()
        .find(|socket| reaches(path, socket))
        .or_else(|| {
            engine_socket_trees(home)
                .into_iter()
                .find(|tree| overlaps(path, tree))
        })
}

/// The engine sockets that live at one nameable path.
///
/// The per-user ones are absent on purpose: their paths carry a uid or a machine
/// name, so they are covered as trees by [`engine_socket_trees`] instead.
/// `DOCKER_HOST` and `CONTAINER_HOST` are read because they are how a host that
/// puts its socket somewhere else entirely (Colima, Rancher Desktop, a
/// `podman machine`) says so — and that spelling is the one this friring's own
/// engine is talking to.
fn engine_socket_files() -> Vec<String> {
    let mut out: Vec<String> = [
        "/var/run/docker.sock",
        "/run/docker.sock",
        "/var/run/podman/podman.sock",
        "/run/podman/podman.sock",
    ]
    .iter()
    .map(|path| (*path).to_string())
    .collect();
    for var in ["DOCKER_HOST", "CONTAINER_HOST"] {
        let declared = std::env::var(var)
            .ok()
            .and_then(|value| unix_socket_path(&value));
        out.extend(declared);
    }
    out
}

/// The trees a rootless or desktop engine keeps a socket in, where the path
/// carries a uid or a machine name friring cannot enumerate.
///
/// Judged in both directions ([`overlaps`]), so `/run/user` and
/// `/run/user/1000/podman` are both refused. `$XDG_RUNTIME_DIR` is the same tree
/// under whatever name this host gave it — bubblewrap masks it wholesale for
/// exactly these endpoints (see `MASKED_SOCKET_DIRS`), and a place cannot mask,
/// only refuse.
fn engine_socket_trees(home: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = vec!["/run/user".to_string(), "/var/run/user".to_string()];
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|dir| dir.starts_with('/'));
    out.extend(runtime_dir);
    if let Some(home) = home.map(|home| home.trim_end_matches('/')) {
        // Docker Desktop binds `~/.docker/run/docker.sock`; `podman machine`
        // names its socket after the machine, under this tree.
        out.push(format!("{home}/.docker"));
        out.push(format!("{home}/.local/share/containers/podman/machine"));
    }
    out
}

/// The path inside a `unix://…` engine address, or `None` for one that names
/// anything else (a TCP or SSH endpoint is not a path a mount could carry).
fn unix_socket_path(value: &str) -> Option<String> {
    let path = value.trim().strip_prefix("unix://")?;
    // `unix:///var/run/docker.sock` is the ordinary spelling (empty host, then
    // an absolute path); anything relative is not a path a bind mount can name.
    path.starts_with('/').then(|| path.to_string())
}

/// Refuse a set of read-write roots that reaches something no sandbox may
/// write. Returns the sentence to show the user, which names the location.
///
/// `db` is the launch's database path, used for its *directory*: the friring
/// that owns a session is not necessarily the one on the host where the agent
/// runs, so the launch input is the authoritative spelling of "the data
/// directory" and the local one is only a fallback.
///
/// Trimming the offending root instead was considered and rejected: a profile
/// that says "my home is writable" and silently is not produces a boundary
/// nobody can reason about. Refusing names the directory and the fix.
pub fn check_writable_roots(writable: &[String], db: Option<&str>) -> Result<(), String> {
    let protected_data = protected_data_dirs(db);
    for root in writable {
        if let Some(socket_root) = grants_tmux_socket_tree(root) {
            return Err(format!(
                "the read-write path '{root}' reaches the tmux socket directory under \
                 '{socket_root}'. A sandbox that can write friring's own tmux socket can run \
                 commands in any pane, outside the boundary — list the directories the agent \
                 needs instead"
            ));
        }
        for protected in &protected_data {
            if encloses(root, protected) {
                return Err(format!(
                    "the read-write path '{root}' encloses friring's data directory \
                     '{protected}'. The database there carries automation commands the host \
                     executes, so reaching it is host command execution (ADR-29) — list the \
                     directories the agent needs instead of an ancestor of the data directory"
                ));
            }
        }
    }
    Ok(())
}

/// Refuse a **profile-declared** path that reaches the state friring keeps for
/// its *other* sandboxes, whatever mode it was declared in.
///
/// [`check_writable_roots`] asks whether a path is an *ancestor* of the data
/// directory, which is the right question for the database and the wrong one for
/// everything friring mints beside it. `<data>/sandbox` encloses no data
/// directory and passes that check, and it holds:
///
/// - every *other* profile's synthetic home, and therefore the credential its
///   `volume-login` signed in with and the copy a `seed-file` made (ADR-28's
///   one-consumer rule is only worth as much as the boundary around the copy);
/// - the markers that enforce that rule, which a sandbox that could clear them
///   could make friring re-seed a credential family somewhere else;
/// - the generated seatbelt profiles — a sandbox that can write one rewrites the
///   policy that constrains it;
/// - every other session's egress socket and status-signal file.
///
/// So a profile may not name any of it. Judged in **both** directions, because
/// the danger is the contents: naming `<data>/sandbox` hands over all of it, and
/// naming `<data>/sandbox/pl/other` hands over exactly one other profile's
/// login. Read-only is no defence for any of it — a credential is disclosed by
/// being readable, and a unix socket takes `connect(2)` through a read-only
/// bind.
///
/// The launch's *own* minted directories are not declared paths and never reach
/// here: [`crate::sandbox::SandboxLaunch::writable_paths`] folds them in
/// afterwards, so this is given `rw_paths`/`ro_paths` rather than that.
pub fn check_declared_paths(declared: &[String]) -> Result<(), String> {
    let protected: Vec<(&str, PathBuf)> = [
        ("sandbox state", sandbox_root()),
        ("status-signal", crate::paths::signals_directory()),
    ]
    .into_iter()
    .filter_map(|(what, dir)| dir.map(|dir| (what, dir)))
    .collect();
    for path in declared {
        for (what, dir) in &protected {
            let dir = dir.display().to_string();
            if overlaps(path, &dir) {
                return Err(format!(
                    "the path '{path}' reaches friring's own {what} directory '{dir}'. That tree \
                     holds the other profiles' sandbox logins, the markers that keep one \
                     credential to one boundary, the generated sandbox policies and the other \
                     sessions' egress sockets — reading it is enough to take any of them, so no \
                     profile may name it in either mode. List the directories the agent needs \
                     instead"
                ));
            }
        }
    }
    Ok(())
}

/// Refuse a set of paths that reaches a container engine's control socket, in
/// **either** mode.
///
/// Separate from [`check_writable_roots`] because read-only is not a defence
/// here and is for the tmux and data directories it covers: a read-only bind
/// does not take write permission off a socket inode, so `connect(2)` still
/// succeeds. Separate from the place backends' own mount check because the
/// *policy* backends need it too — bwrap masks `/run` and `/var/run` only under
/// `host-minus-secrets`, and a path the profile lists explicitly wins that mask.
///
/// `home` is the home directory on the host the engine runs on, for the
/// per-user sockets Docker Desktop and `podman machine` keep there.
pub fn check_engine_socket_paths(paths: &[String], home: Option<&str>) -> Result<(), String> {
    for path in paths {
        if let Some(socket) = grants_engine_socket(path, home) {
            return Err(format!(
                "the path '{path}' reaches the container engine's control socket '{socket}'. \
                 Anything that can speak to it can start a privileged container with the host's \
                 filesystem in it, which is the whole host — list the directories the agent needs \
                 instead"
            ));
        }
    }
    Ok(())
}

/// The data directories to protect: the launch's own (derived from the database
/// path it was given) and this machine's, de-duplicated.
///
/// Public because ADR-29's rule is not only about the *writable* set. A policy
/// backend can only be handed a path by making it writable, so
/// [`check_writable_roots`] is the whole check there; a place backend mounts
/// read-only paths too, and a read-only bind of the data directory still carries
/// the automation commands the host executes across the boundary.
pub fn protected_data_dirs(db: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let from_db = db
        .and_then(|db| Path::new(db).parent().map(Path::to_path_buf))
        .filter(|p| !p.as_os_str().is_empty());
    for dir in [from_db, data_dir()].into_iter().flatten() {
        let dir = dir.display().to_string();
        if !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

/// Whether `parent` is `child` or an ancestor of it.
///
/// Mirrors the session layer's own containment rule: separators unified, and
/// case-sensitive even on Windows, because over-matching silently widens a
/// boundary while under-matching only costs a UI filter.
pub fn encloses(parent: &str, child: &str) -> bool {
    let parent = normalize(parent);
    let child = normalize(child);
    parent == child || strip_ancestor(&child, &parent).is_some()
}

/// Which read-write root would let a sandbox replace the program that applies
/// the boundary, and the spelling of the program that root contains.
///
/// The rule bubblewrap applies to itself and every place backend applies to the
/// engine it drives: a program at a user-writable prefix (`/usr/local/bin`,
/// `/opt/homebrew/bin`) plus a profile granting that prefix read-write lets the
/// sandbox replace what the host runs next. Shared rather than restated per
/// backend, because a second copy is a second thing to forget when the rule
/// changes.
///
/// Both spellings are compared, because replacing the symlink a program is
/// reached through redirects the host's next launch exactly as replacing the
/// binary does: a program on `PATH` at a system prefix pointing into a prefix
/// the profile makes writable is the obvious bypass of a check that only read
/// the name. `resolved` is the program as the kernel resolves it, or `None`
/// where that could not be asked.
#[must_use]
pub fn program_in_writable_root<'a>(
    rw_paths: &'a [String],
    program: &str,
    resolved: Option<&str>,
) -> Option<(&'a str, String)> {
    [
        Some(program),
        resolved.filter(|resolved| *resolved != program),
    ]
    .into_iter()
    .flatten()
    .find_map(|path| {
        rw_paths
            .iter()
            .find(|root| encloses(root, path))
            .map(|root| (root.as_str(), path.to_string()))
    })
}

/// `child` with `parent`'s prefix removed, or `None` when `parent` is not a
/// strict ancestor of `child`.
fn strip_ancestor(child: &str, parent: &str) -> Option<String> {
    let parent = normalize(parent);
    let child = normalize(child);
    let prefix = if parent.ends_with('/') {
        parent
    } else {
        format!("{parent}/")
    };
    child.strip_prefix(&prefix).map(str::to_string)
}

fn normalize(raw: &str) -> String {
    let unified = raw.trim().replace('\\', "/");
    let trimmed = unified.trim_end_matches('/');
    if trimmed.is_empty() {
        unified
    } else {
        trimmed.to_string()
    }
}

/// Every strict ancestor of an absolute `path`, root first — the directories a
/// rename could move in order to reach it (see `render_rename_boundaries`).
///
/// A relative or drive-lettered path has no ancestors here: the one caller is
/// seatbelt, whose paths are always absolute Unix ones.
pub fn ancestors_of(path: &str) -> Vec<String> {
    let normalized = normalize(path);
    let Some(rest) = normalized.strip_prefix('/').filter(|r| !r.is_empty()) else {
        return Vec::new();
    };
    let components: Vec<&str> = rest.split('/').filter(|c| !c.is_empty()).collect();
    let mut out = vec!["/".to_string()];
    let mut current = String::new();
    for part in components.iter().take(components.len().saturating_sub(1)) {
        current.push('/');
        current.push_str(part);
        out.push(current.clone());
    }
    out
}

/// Reduce one path component to something that cannot become a path.
///
/// Both values this is applied to are already restricted (a profile name's
/// charset forbids a separator, a session key is a UUID); the file name is the
/// one place either would become a path, so the filter lives next to the code
/// that needs it.
pub fn sanitize_component(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    // `.` and `..` survive the charset filter and are the two components that
    // are not names at all: joining `..` onto the scratch root would put a
    // sandbox's writable directory *above* it. Neither value can be one today —
    // a profile name is validated and a session key is a UUID — which is exactly
    // why the guard belongs here rather than in each caller's head.
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "sandbox".to_string()
    } else {
        cleaned
    }
}

/// Create `path` and its parents `0700`, adopting an existing directory.
///
/// Refuses a symlink outright: friring writes a policy file and an agent's
/// scratch through this, and following a link would put both wherever the link
/// points. An adopted directory has its mode re-asserted, because
/// `DirBuilder::mode` only applies to the components it actually creates.
///
/// Only the **final** component is judged, because the parents are friring's own
/// tree under the data directory — a place no sandbox may be handed (ADR-29), so
/// a link among them is one the machine's owner planted. For a path whose
/// ancestors a sandbox *can* write — anything under a place's synthetic home —
/// use [`create_private_dir_under`], which judges every component below the part
/// friring vouches for.
pub fn create_private_dir(path: &Path) -> SandboxResult<()> {
    let io_err = |detail: String| SandboxError::Io {
        path: path.display().to_string(),
        detail,
    };

    match std::fs::symlink_metadata(path) {
        // Interference, not a profile a user should edit: a place mounts its own
        // tree read-write, so an agent inside can replace the per-session
        // directory under it with a link. Classifying it keeps the refusal out
        // of `allow_unsandboxed_fallback`, which would otherwise answer "the
        // sandbox broke its own scratch directory" by starting the agent on the
        // host (see `SandboxError::Tampered`).
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io_err(
                "is a symlink; friring will not write a sandbox directory through one".to_string(),
            )
            .tampered())
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(io_err("exists and is not a directory".to_string()).tampered())
        }
        _ => {}
    }

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| io_err(e.to_string()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err(e.to_string()))?;
    }
    Ok(())
}

/// Create `base/rel` `0700`, judging **every component of `rel`** rather than
/// only the last, and answer with the path that was made.
///
/// `base` is a directory friring owns and a sandbox cannot reach; `rel` names
/// its way down through one a sandbox *can* — a place's synthetic home is
/// bind-mounted read-write at `/home/agent`, and the sessions already running in
/// that place are what makes every directory on the way down attacker-chosen.
/// [`create_private_dir`]'s recursive `DirBuilder` is wrong here for the reason
/// the projection writer states about its own walk: `mkdir(2)` never creates or
/// follows a symlink, but a *recursive* create resolves the components above the
/// one it makes, and so does the `chmod` that follows it. A live agent that
/// replaces `~/.config` with a link to the host's own `~/.config` would
/// otherwise have friring create — and `0700` — a directory outside the
/// boundary, and a `seed-file` copy land its credential there.
///
/// The mode is applied by `mkdir(2)` itself rather than by a later `chmod`, so
/// there is no window in which the directory exists world-readable and no second
/// path resolution to race. An adopted directory is checked, never re-chmodded:
/// friring did not make it, so the only safe thing to know about it is that it
/// is not a link.
///
/// # Errors
///
/// `rel` is empty or carries anything but plain names, `base` cannot be created,
/// or a component of `rel` is a symlink or is not a directory.
pub fn create_private_dir_under(base: &Path, rel: &str) -> SandboxResult<PathBuf> {
    create_private_dir(base)?;
    let mut at = base.to_path_buf();
    let names: Vec<&str> = rel.split('/').filter(|part| !part.is_empty()).collect();
    if names.is_empty() || names.iter().any(|part| *part == "." || *part == "..") {
        return Err(SandboxError::Io {
            path: base.join(rel).display().to_string(),
            detail: format!("'{rel}' does not name a plain directory under this home"),
        });
    }
    for name in names {
        at.push(name);
        let io_err = |detail: String| SandboxError::Io {
            path: at.display().to_string(),
            detail,
        };
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(&at) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match std::fs::symlink_metadata(&at) {
                    // Interference rather than a profile a user should edit:
                    // nothing but a process inside the place puts a link here,
                    // so this refusal never routes through
                    // `allow_unsandboxed_fallback` (see `SandboxError`).
                    Ok(meta) if meta.file_type().is_symlink() => return Err(io_err(
                        "is a symlink inside the sandbox's own home; friring will not create a \
                             directory through one, because the sandbox chose where it points"
                            .to_string(),
                    )
                    .tampered()),
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => {
                        return Err(io_err("exists and is not a directory".to_string()).tampered())
                    }
                    Err(e) => return Err(io_err(e.to_string())),
                }
            }
            Err(e) => return Err(io_err(e.to_string())),
        }
    }
    Ok(at)
}

/// [`write_private`] for a file under a directory a sandbox can write:
/// `base/rel`, with every directory component of `rel` judged by
/// [`create_private_dir_under`].
///
/// # Errors
///
/// As [`create_private_dir_under`] for the directories, then as
/// [`write_private`] for the file itself.
pub fn write_private_under(base: &Path, rel: &str, contents: &str) -> SandboxResult<PathBuf> {
    let (dirs, name) = match rel.rsplit_once('/') {
        Some((dirs, name)) => (dirs, name),
        None => ("", rel),
    };
    if name.is_empty() || name == "." || name == ".." {
        return Err(SandboxError::Io {
            path: base.join(rel).display().to_string(),
            detail: format!("'{rel}' does not name a file under this home"),
        });
    }
    let parent = if dirs.is_empty() {
        create_private_dir(base)?;
        base.to_path_buf()
    } else {
        create_private_dir_under(base, dirs)?
    };
    let path = parent.join(name);
    write_private_leaf(&path, contents)?;
    Ok(path)
}

/// Write `contents` to `path` with `O_NOFOLLOW` semantics and mode `0600`.
///
/// Two things, both because the file is a security policy a sandboxed process
/// must never be able to redirect:
///
/// - An existing symlink at the final component is **refused**, not followed —
///   the `O_NOFOLLOW` guarantee, expressed as a `lstat` because the flag's value
///   is not in the standard library and `libc` is a Linux-only dependency here.
/// - The bytes go to a fresh sibling opened `O_EXCL` and are renamed into place,
///   so a link planted between the check and the write is *replaced* rather than
///   written through. `rename(2)` never follows a symlink at its destination.
///
/// The directories above `path` are created by [`create_private_dir`], so this
/// is for a file whose parents friring owns. For one under a sandbox-writable
/// home, use [`write_private_under`].
pub fn write_private(path: &Path, contents: &str) -> SandboxResult<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_private_dir(parent)?;
    }
    write_private_leaf(path, contents)
}

/// The final component of [`write_private`] — the `lstat`, the `O_EXCL` staging
/// file and the rename — with no opinion about the directories above it.
fn write_private_leaf(path: &Path, contents: &str) -> SandboxResult<()> {
    use std::io::Write as _;

    let io_err = |detail: String| SandboxError::Io {
        path: path.display().to_string(),
        detail,
    };

    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io_err(
                "is a symlink; friring will not write a sandbox profile through one".to_string(),
            ))
        }
        Ok(meta) if !meta.is_file() => {
            return Err(io_err("exists and is not a regular file".to_string()))
        }
        _ => {}
    }

    let staging = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let staged_err = |detail: std::io::Error| SandboxError::Io {
        path: staging.display().to_string(),
        detail: detail.to_string(),
    };
    let mut file = options.open(&staging).map_err(staged_err)?;
    file.write_all(contents.as_bytes()).map_err(staged_err)?;
    drop(file);
    std::fs::rename(&staging, path).map_err(|e| io_err(e.to_string()))
}

/// The failure every caller shares when there is no data directory to mint
/// sandbox state under.
fn no_data_dir() -> SandboxError {
    SandboxError::Io {
        path: "<data directory>".to_string(),
        detail: "friring cannot resolve its data directory, so it has nowhere outside every \
                 sandbox-writable tree to keep the sandbox's scratch and policy files"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friring_state_hangs_off_the_data_directory_and_grants_no_socket_tree() {
        // The escape this module exists to close: what a launch grants must
        // never *be* the host temp root or a tmux socket directory. (A unit-test
        // build pins the data directory under the temp root, which is why the
        // assertion is about reach rather than about the prefix.)
        let data = data_dir().unwrap().display().to_string();
        let root = sandbox_root().unwrap().display().to_string();
        assert!(encloses(&data, &root), "{root} must be under {data}");
        for dir in [profile_dir(), scratch_root(), session_scratch_dir("s1")]
            .into_iter()
            .flatten()
        {
            let dir = dir.display().to_string();
            assert!(encloses(&root, &dir), "{dir} must be under {root}");
            assert_ne!(dir, host_temp_root().display().to_string());
        }
        // The one directory a launch hands over passes the guard.
        let scratch = session_scratch_dir("s1").unwrap().display().to_string();
        check_writable_roots(&[scratch], None).unwrap();
    }

    #[test]
    fn a_session_scratch_directory_is_private_and_reusable() {
        let key = "scratch-lifecycle-test";
        let dir = create_session_scratch(key).unwrap();
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "the scratch directory must be private");
        }
        // A crashed run leaves the directory (and its contents) behind; the next
        // launch of the same session adopts it rather than failing.
        std::fs::write(dir.join("leftover"), "x").unwrap();
        let again = create_session_scratch(key).unwrap();
        assert_eq!(again, dir);
        assert!(dir.join("leftover").exists());

        cleanup_session(key);
        assert!(!dir.exists());
    }

    #[test]
    fn cleanup_takes_the_generated_profile_with_it() {
        let key = "cleanup-profile-test";
        let dir = create_session_scratch(key).unwrap();
        let profile = profile_dir().unwrap().join(format!("dev-{key}.sb"));
        write_private(&profile, "(version 1)").unwrap();
        let other = profile_dir().unwrap().join("dev-another-session.sb");
        write_private(&other, "(version 1)").unwrap();

        cleanup_session(key);
        assert!(!dir.exists());
        assert!(!profile.exists());
        // Another session's profile is untouched: the key is the whole suffix.
        assert!(other.exists());
        let _ = std::fs::remove_file(other);
    }

    #[test]
    fn a_symlink_at_the_profile_path_is_refused_rather_than_followed() {
        let base = host_temp_root().join(format!("friring-nofollow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        create_private_dir(&base).unwrap();
        let target = base.join("victim.txt");
        std::fs::write(&target, "host data").unwrap();
        let link = base.join("dev-s1.sb");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let err = write_private(&link, "(version 1)").unwrap_err();
            assert!(err.to_string().contains("is a symlink"), "{err}");
            // The file the link pointed at is intact: nothing was truncated.
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "host data");

            // A symlinked *directory* is refused for the same reason.
            let linked_dir = base.join("profiles");
            std::os::unix::fs::symlink(&base, &linked_dir).unwrap();
            let err = write_private(&linked_dir.join("dev-s2.sb"), "(version 1)").unwrap_err();
            assert!(err.to_string().contains("is a symlink"), "{err}");
        }

        // Writing where nothing is planted works, and the file is private.
        write_private(&base.join("dev-s3.sb"), "(version 1)").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(base.join("dev-s3.sb"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn writable_roots_that_reach_the_database_are_refused() {
        let db = "/home/u/.local/share/friring/friring.db";
        // The data directory itself, and every ancestor of it.
        for root in [
            "/home/u",
            "/home/u/.local",
            "/home/u/.local/share",
            "/home/u/.local/share/friring",
        ] {
            let err = check_writable_roots(&[root.to_string()], Some(db)).unwrap_err();
            assert!(
                err.contains("/home/u/.local/share/friring") && err.contains("ADR-29"),
                "{root}: {err}"
            );
        }
        // The filesystem root reaches everything, and is refused for whichever
        // reason is checked first.
        assert!(check_writable_roots(&["/".to_string()], Some(db)).is_err());
        // A sibling, and a directory inside the data directory, are both fine:
        // neither can reach the database.
        check_writable_roots(&["/home/u/dev/app".to_string()], Some(db)).unwrap();
        check_writable_roots(
            &["/home/u/.local/share/friring/sandbox/tmp/s1".to_string()],
            Some(db),
        )
        .unwrap();
        // A path that merely shares a prefix is not an ancestor.
        check_writable_roots(&["/home/us".to_string()], Some(db)).unwrap();
    }

    use super::test_temp_base as temp_base;

    /// Sets an environment variable for one test and puts back what was there.
    /// Both of the hosts described below are described *by their environment*,
    /// and every other test in the process reads the same variables.
    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(previous) => std::env::set_var(self.key, previous),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// The escape a place's mounts have to be checked against: the string
    /// friring vets is the string the kernel resolves *again* at mount time, so
    /// a source travelling through a symlink is a source the sandbox can
    /// redirect after the check.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_mount_source_is_refused_and_the_symlink_is_named() {
        let base = temp_base("canonical-source");
        let repo = base.join("repo/.git");
        let elsewhere = base.join("elsewhere");
        for dir in [&repo, &elsewhere] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let real = repo.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let hooks = repo.join("hooks");
        std::os::unix::fs::symlink(&elsewhere, &hooks).unwrap();

        // An ordinary directory resolves to itself and is named as written.
        let real = real.display().to_string();
        assert_eq!(canonical_source(&real).unwrap(), real);
        // A trailing separator is the same directory, not a different one.
        assert_eq!(canonical_source(&format!("{real}/")).unwrap(), real);

        // The planted link is refused, and the message names the link and where
        // it points — which is the only thing the user can act on.
        let hooks = hooks.display().to_string();
        let err = canonical_source(&hooks).unwrap_err();
        assert!(err.contains(&hooks), "{err}");
        assert!(err.contains(&elsewhere.display().to_string()), "{err}");

        // So is anything under it: the escape is the ancestor, not the leaf,
        // and the message still names the ancestor.
        let err = canonical_source(&format!("{hooks}/inner")).unwrap_err();
        assert!(err.contains(&hooks), "{err}");

        // A path that resolves elsewhere without a symlink of its own — `..`
        // walks out of the tree the profile named.
        let err = canonical_source(&format!("{real}/../..")).unwrap_err();
        assert!(err.contains("resolves to"), "{err}");

        // A component friring is not allowed to look at is refused, not walked
        // past: a rootful engine's daemon resolves the same string as root and
        // is not stopped by the bits that stopped this. (Running as root there
        // *is* no such component, and the link below it is refused instead —
        // which is why only the refusal is pinned, not the sentence.)
        let closed = base.join("closed");
        std::fs::create_dir_all(&closed).unwrap();
        let hidden = closed.join("link");
        std::os::unix::fs::symlink(&elsewhere, &hidden).unwrap();
        set_mode(&closed, 0o000);
        assert!(canonical_source(&hidden.display().to_string()).is_err());
        set_mode(&closed, 0o700);

        // A path that is not on this filesystem at all is a question for the
        // launch's own `exists`, which has already been asked — so this says
        // nothing rather than refusing a path it cannot see (the engine's host
        // is not necessarily friring's).
        let absent = base.join("nothing").display().to_string();
        assert_eq!(canonical_source(&absent).unwrap(), absent);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The exemption friring's own place tree gets, and its limit: a link the
    /// machine's owner put *above* the tree (`/var` → `/private/var` on macOS,
    /// a home behind a link on Linux) is not one any sandbox could have planted,
    /// and refusing over it would refuse a place its own egress directory. A
    /// link *inside* the tree is exactly what an agent with the place mounted
    /// read-write can plant, and gets the strict rule.
    #[cfg(unix)]
    #[test]
    fn frirings_own_place_tree_is_checked_where_a_sandbox_could_reach_it() {
        let base = temp_base("trusted-prefix");
        let real = base.join("data/sandbox/pl");
        let elsewhere = base.join("elsewhere");
        for dir in [&real.join("dev/home"), &elsewhere] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // How friring would name it: through a link somebody else made.
        let linked = base.join("link");
        std::os::unix::fs::symlink(base.join("data"), &linked).unwrap();
        let trusted = linked.join("sandbox/pl").display().to_string();
        let home = format!("{trusted}/dev/home");

        // Accepted, and reported as the kernel resolves it — so the checks that
        // compare it against the directories no place may be handed still have
        // the sharpest form of the path.
        assert_eq!(
            canonical_source_below(&home, &trusted).unwrap(),
            real.join("dev/home").display().to_string()
        );
        // The strict rule would have refused the same path outright.
        assert!(canonical_source(&home).is_err());

        // Inside the tree, nothing is exempt.
        std::fs::remove_dir(real.join("dev/home")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, real.join("dev/home")).unwrap();
        let err = canonical_source_below(&home, &trusted).unwrap_err();
        assert!(err.contains("is a symlink"), "{err}");
        assert!(err.contains(&elsewhere.display().to_string()), "{err}");

        // Nor may the part below the trusted root walk back out of it.
        let err = canonical_source_below(&format!("{trusted}/dev/.."), &trusted).unwrap_err();
        assert!(err.contains("plain directory"), "{err}");

        // And a path that is not under the tree at all has nothing vouching for
        // it, so it gets the strict rule.
        let err = canonical_source_below(&linked.display().to_string(), &trusted).unwrap_err();
        assert!(err.contains("is a symlink"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `/tmp` is a symlink to `/private/tmp` on macOS, so a guard that compares
    /// the literal socket root misses every path naming the resolved one — and
    /// that path is friring's own tmux server.
    #[cfg(unix)]
    #[test]
    fn the_tmux_socket_directory_is_refused_under_either_spelling() {
        let base = temp_base("socket-root");
        let real = base.join("run");
        std::fs::create_dir_all(&real).unwrap();
        let linked = base.join("sockets");
        std::os::unix::fs::symlink(&real, &linked).unwrap();

        let real = real.display().to_string();
        let linked = linked.display().to_string();
        // The root as tmux was told to spell it, and as the kernel resolves it.
        let grants = |path: &str| grants_socket_tree_under(path, &linked);
        assert!(grants(&linked).is_some());
        assert!(grants(&real).is_some(), "{real}");
        // A `tmux-<uid>` child is one server's socket directory, under either
        // spelling, and an ancestor carries every server's.
        assert!(grants(&format!("{real}/tmux-1000")).is_some());
        assert!(grants(&format!("{linked}/tmux-1000/default")).is_some());
        assert!(grants(&base.display().to_string()).is_some());
        // Something else under the same root is ordinary scratch space.
        assert!(grants(&format!("{real}/build-cache")).is_none());
        // And this host's own root is what the public rule reads.
        let root = tmux_socket_root().display().to_string();
        assert!(grants_tmux_socket_tree(&root).is_some(), "{root}");

        // The comparison behind it: a protected directory reached through a
        // symlink is still reached.
        assert!(reaches(&real, &linked));
        assert!(!reaches(&format!("{real}x"), &linked));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A sandbox holding the engine socket can start a privileged container
    /// with the host's root in it, so every spelling of one is refused — the
    /// rootless per-user paths included, where the uid is not friring's to
    /// enumerate.
    #[test]
    fn a_container_engine_control_socket_is_refused_wherever_it_lives() {
        // A runtime directory this host spells its own way: the standard
        // `/run/user/<uid>` is covered by name, and this is not.
        let _runtime = EnvGuard::set("XDG_RUNTIME_DIR", "/custom/xdg");
        let home = Some("/home/u");
        for path in [
            "/var/run/docker.sock",
            "/var/run",
            "/run",
            "/run/docker.sock",
            "/run/podman/podman.sock",
            "/var/run/podman",
            "/run/user",
            "/run/user/1000",
            "/run/user/1000/podman",
            "/run/user/1000/podman/podman.sock",
            "/var/run/user/1000",
            "/custom/xdg",
            "/custom/xdg/podman/podman.sock",
            "/home/u/.docker",
            "/home/u/.docker/run/docker.sock",
            "/home/u/.local/share/containers/podman/machine",
            "/home/u/.local/share/containers/podman/machine/qemu/podman.sock",
            "/",
        ] {
            assert!(
                grants_engine_socket(path, home).is_some(),
                "{path} must be refused"
            );
        }
        for path in [
            "/home/u/dev/app",
            "/srv/shared",
            "/run/systemd/resolve",
            "/var/lib/docker",
        ] {
            assert!(
                grants_engine_socket(path, home).is_none(),
                "{path} must be allowed"
            );
        }
        // Without a home the fixed locations still stand.
        assert!(grants_engine_socket("/var/run/docker.sock", None).is_some());
        assert!(grants_engine_socket("/home/u/.docker", None).is_none());
    }

    /// How a host that keeps its engine somewhere else entirely says so. Only a
    /// `unix://` address names something a mount could carry — a TCP or SSH
    /// endpoint is not a path, and inventing one from it would refuse a
    /// directory for no reason.
    #[test]
    fn only_a_unix_engine_address_names_a_path() {
        assert_eq!(
            unix_socket_path("unix:///opt/colima/docker.sock"),
            Some("/opt/colima/docker.sock".to_string())
        );
        assert_eq!(
            unix_socket_path("  unix:///run/podman/podman.sock  "),
            Some("/run/podman/podman.sock".to_string())
        );
        for other in [
            "ssh://u@host/run/podman.sock",
            "tcp://127.0.0.1:2375",
            "unix://relative.sock",
            "/var/run/docker.sock",
            "",
        ] {
            assert_eq!(unix_socket_path(other), None, "{other}");
        }
    }

    #[test]
    fn no_profile_may_name_frirings_own_sandbox_state_in_either_mode() {
        // The mirror image of the ancestor rule: `<data>/sandbox` encloses no
        // data directory, so `check_writable_roots` passes it — and it holds
        // every *other* profile's login, the seed markers, the generated
        // seatbelt policies and the other sessions' sockets.
        let root = sandbox_root().unwrap().display().to_string();
        let signals = crate::paths::signals_directory()
            .unwrap()
            .display()
            .to_string();
        for path in [
            root.clone(),
            format!("{root}/pl"),
            format!("{root}/pl/other/home"),
            format!("{root}/profiles"),
            signals.clone(),
            format!("{signals}/deadbeef"),
            data_dir().unwrap().display().to_string(),
        ] {
            let err = check_declared_paths(std::slice::from_ref(&path))
                .expect_err(&format!("'{path}' must not be grantable"));
            assert!(
                err.contains("friring's own"),
                "{path} must be refused: {err}"
            );
        }
        // A neighbour of the tree is not the tree.
        check_declared_paths(&[format!("{root}-elsewhere"), "/home/u/dev".to_string()]).unwrap();
    }

    #[test]
    fn a_directory_below_a_trusted_base_refuses_a_planted_link_at_every_component() {
        let base = test_temp_base("under-walk");
        // What a live agent in the shared place does: replace a directory on the
        // way down with a link to somewhere on the host.
        let outside = base.join("host-side");
        std::fs::create_dir_all(&outside).unwrap();
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, home.join(".config")).unwrap();

        #[cfg(unix)]
        {
            let err = create_private_dir_under(&home, ".config/opencode").unwrap_err();
            assert!(
                err.to_string().contains("is a symlink"),
                "the walk must refuse the link rather than create through it: {err}"
            );
            assert!(err.is_tampering(), "a planted link is interference: {err}");
            assert!(
                !outside.join("opencode").exists(),
                "nothing may be created on the other side of the link"
            );
            // The credential copy takes the same walk.
            let err =
                write_private_under(&home, ".config/auth.json", "not-a-real-token").unwrap_err();
            assert!(err.is_tampering(), "{err}");
            assert!(!outside.join("auth.json").exists());
        }

        // The ordinary case still works, and lands 0700 without a second chmod.
        let made = create_private_dir_under(&home, ".codex/sessions").unwrap();
        assert_eq!(made, home.join(".codex/sessions"));
        assert!(made.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for dir in [home.join(".codex"), made.clone()] {
                let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{}", dir.display());
            }
        }
        let written = write_private_under(&home, ".codex/auth.json", "not-a-real-token").unwrap();
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            "not-a-real-token"
        );
        // A tail that is not a plain name never reaches the filesystem.
        for bad in ["../escape/x", "", "./x"] {
            assert!(create_private_dir_under(&home, bad).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_place_tree_no_profile_accounts_for_is_reclaimed_and_the_rest_is_left() {
        let root = place_root().unwrap();
        for name in ["kept-by-profile", "kept-by-container", "orphan"] {
            std::fs::create_dir_all(root.join(name).join("home")).unwrap();
        }
        reclaim_orphan_places(
            &["kept-by-profile".to_string()],
            &["kept-by-container".to_string()],
        );
        assert!(
            root.join("kept-by-profile").is_dir(),
            "a live profile keeps its tree"
        );
        assert!(
            root.join("kept-by-container").is_dir(),
            "a deleted profile whose container is still running keeps its tree — that tree is \
             the container's own $HOME"
        );
        assert!(
            !root.join("orphan").exists(),
            "a tree no profile and no container accounts for is collected"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn writable_roots_that_reach_a_tmux_socket_are_refused() {
        let socket_root = tmux_socket_root().display().to_string();
        for root in [
            socket_root.clone(),
            format!("{socket_root}/tmux-1000"),
            format!("{socket_root}/tmux-1000/friring"),
            "/".to_string(),
        ] {
            let err = check_writable_roots(std::slice::from_ref(&root), None).unwrap_err();
            assert!(err.contains("tmux socket directory"), "{root}: {err}");
        }
        // Something else under the same root is ordinary scratch space.
        check_writable_roots(&[format!("{socket_root}/build-cache")], None).unwrap();
    }

    #[test]
    fn ancestors_run_from_the_root_down_to_the_parent() {
        assert_eq!(
            ancestors_of("/home/u/.local/share/friring/friring.db"),
            [
                "/",
                "/home",
                "/home/u",
                "/home/u/.local",
                "/home/u/.local/share",
                "/home/u/.local/share/friring",
            ]
        );
        assert_eq!(ancestors_of("/home"), ["/"]);
        assert!(ancestors_of("/").is_empty());
    }

    #[test]
    fn containment_compares_whole_components() {
        assert!(encloses("/a/b", "/a/b"));
        assert!(encloses("/a/b", "/a/b/c"));
        assert!(encloses("/", "/a"));
        assert!(!encloses("/a/b", "/a/bc"));
        assert!(!encloses("/a/b/c", "/a/b"));
        // A trailing separator is not a different path.
        assert!(encloses("/a/b/", "/a/b/c"));
    }

    #[test]
    fn a_session_key_cannot_become_a_path() {
        assert_eq!(sanitize_component("../../etc"), "..-..-etc");
        assert_eq!(sanitize_component(""), "sandbox");
        // The two components that are not names: `..` would put a sandbox's
        // writable directory above the root friring mints it under.
        assert_eq!(sanitize_component(".."), "sandbox");
        assert_eq!(sanitize_component("."), "sandbox");
        assert_eq!(sanitize_component("..."), "sandbox");
        let dir = session_scratch_dir("../escape").unwrap();
        assert_eq!(dir.parent().unwrap(), scratch_root().unwrap());
    }
}
