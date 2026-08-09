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
//! one that grants more.

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

/// The host's temp root — what the sandbox used to be handed, and what it must
/// never be handed again.
pub fn host_temp_root() -> PathBuf {
    std::env::temp_dir()
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
    let socket_root = tmux_socket_root().display().to_string();
    let protected_data = data_directories(db);
    for root in writable {
        if grants_tmux_sockets(root, &socket_root) {
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

/// The data directories to protect: the launch's own (derived from the database
/// path it was given) and this machine's, de-duplicated.
fn data_directories(db: Option<&str>) -> Vec<String> {
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
    if cleaned.is_empty() {
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
pub fn create_private_dir(path: &Path) -> SandboxResult<()> {
    let io_err = |detail: String| SandboxError::Io {
        path: path.display().to_string(),
        detail,
    };

    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io_err(
                "is a symlink; friring will not write a sandbox directory through one".to_string(),
            ))
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(io_err("exists and is not a directory".to_string()))
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
pub fn write_private(path: &Path, contents: &str) -> SandboxResult<()> {
    use std::io::Write as _;

    let io_err = |detail: String| SandboxError::Io {
        path: path.display().to_string(),
        detail,
    };

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_private_dir(parent)?;
    }
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
        let dir = session_scratch_dir("../escape").unwrap();
        assert_eq!(dir.parent().unwrap(), scratch_root().unwrap());
    }
}
