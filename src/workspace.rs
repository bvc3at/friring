//! Per-session multi-repo **symlink workspaces**.
//!
//! When a session spans more than one directory (multiple repos, or a repo plus
//! an extra access dir), the agent process can only be launched in a single
//! `cwd`. Rather than teach every agent CLI a different `--add-dir`-style flag
//! (many have none), friring builds one workspace directory full of symlinks —
//! one per member dir — and launches the agent there. The agent then sees every
//! repo as a subdirectory, with no per-agent configuration.
//!
//! ```text
//! ~/.local/share/friring/workspaces/<agent_session_id>/
//!     webapp  -> …/worktrees/<hash>/feat-x   (symlink)
//!     infra   -> /home/me/repos/infra        (symlink)
//! ```
//!
//! The directory only ever contains symlinks, so tearing it down (or rebuilding
//! it) never touches the underlying repositories. The path is derived from the
//! session's stable `agent_session_id`, so it is rebuilt idempotently on every
//! launch and needs no separate persistence.
//!
//! The new-session wizard can override the location with a user-chosen
//! directory ([`ensure_workspace_at`]); that choice *is* persisted
//! (`SessionInfo::workspace_dir`) because it can no longer be derived from the
//! id. A custom directory sits outside the friring-owned workspaces root, so
//! every destructive step there is gated on the directory containing nothing
//! but symlinks — friring never deletes real user files.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use crate::paths;

/// Resolve the workspace directory for a session id, ensuring it stays a single
/// segment under the workspaces root (defensive — the id is a UUID in practice).
fn workspace_dir(id: &str) -> io::Result<PathBuf> {
    let base = paths::workspaces_directory().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not resolve workspaces directory",
        )
    })?;
    let segment = paths::sanitize_workspace_segment(id);
    if segment.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty workspace id",
        ));
    }
    Ok(base.join(segment))
}

/// (Re)build the symlink workspace for `id` from `members` and return its path.
///
/// Idempotent: any existing workspace dir is removed first (it holds only
/// symlinks, so targets are untouched) and recreated from scratch, so adding or
/// removing a member between launches is reflected. Each member is symlinked
/// under a sanitized, de-duplicated name (collisions get a `-2`, `-3`, … suffix).
pub fn ensure_workspace(id: &str, members: &[(String, PathBuf)]) -> io::Result<PathBuf> {
    let dir = workspace_dir(id)?;
    remove_dir_under_root(&dir)?;
    populate_workspace(&dir, members)?;
    Ok(dir)
}

/// (Re)build the symlink workspace at a **user-chosen** `dir` (the wizard's
/// optional workspace-dir field). Same idempotent rebuild semantics as
/// [`ensure_workspace`], but the path lies outside the friring-owned workspaces
/// root, so the pre-rebuild teardown refuses a directory containing anything
/// but symlinks instead of trusting the location.
pub fn ensure_workspace_at(dir: &Path, members: &[(String, PathBuf)]) -> io::Result<PathBuf> {
    remove_workspace_at(dir)?;
    populate_workspace(dir, members)?;
    Ok(dir.to_path_buf())
}

/// Create `dir` and fill it with one member symlink per entry, names
/// de-duplicated with a `-2`, `-3`, … suffix on collision.
fn populate_workspace(dir: &Path, members: &[(String, PathBuf)]) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut used: HashSet<String> = HashSet::new();
    for (name, target) in members {
        let link_name = paths::unique_link_name(name, &mut used);
        let link_path = dir.join(&link_name);
        symlink(target, &link_path)?;
    }
    Ok(())
}

/// Remove a **user-chosen** workspace directory, refusing when it holds
/// anything but symlinks (then it is not a friring-built workspace — or a real
/// file was added since — and deleting it could destroy user data). A missing
/// directory is not an error.
pub fn remove_workspace_at(dir: &Path) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        if !is_workspace_link(&entry)? {
            return Err(io::Error::other(format!(
                "refusing to remove {}: {} is not a workspace link",
                dir.display(),
                entry.file_name().to_string_lossy()
            )));
        }
    }
    // Only workspace links verified above; `remove_dir_all` unlinks them
    // without following, so the member repos are untouched.
    std::fs::remove_dir_all(dir)
}

/// Resolve the wizard's raw workspace-dir input to an absolute directory:
/// empty → `None` (default id-derived workspace); `~`-prefixed or absolute →
/// that path; a bare name / relative path → under the workspaces root. `..`
/// components are rejected — the input names a fresh directory, it never
/// navigates.
pub fn resolve_custom_workspace_dir(raw: &str) -> Result<Option<PathBuf>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let expanded = paths::expand_tilde(raw);
    if expanded
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Workspace dir cannot contain '..'".to_string());
    }
    let dir = if expanded.is_absolute() {
        expanded
    } else {
        // A non-absolute path that still carries a Windows prefix or root
        // (drive-relative `C:foo`, rootless `\foo`) would make `base.join`
        // discard the workspaces root and escape the sandbox, so reject it —
        // the relative branch is for plain names/segments only. (No-op on
        // Unix, where such inputs are ordinary relative segments.)
        if expanded.components().any(|c| {
            matches!(
                c,
                std::path::Component::Prefix(_) | std::path::Component::RootDir
            )
        }) {
            return Err("Workspace dir must be a plain name or an absolute path".to_string());
        }
        let base = paths::workspaces_directory()
            .ok_or_else(|| "Could not resolve the workspaces directory".to_string())?;
        base.join(expanded)
    };
    Ok(Some(dir))
}

/// Pre-flight check for a resolved custom workspace dir (run at wizard confirm
/// time, before anything spawns): the target must be missing, empty, or a
/// previous symlink-only workspace — the same rule [`ensure_workspace_at`]
/// enforces, surfaced early as a user-facing error.
pub fn validate_custom_workspace_dir(dir: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("Cannot access {}: {e}", dir.display())),
        Ok(meta) if !meta.is_dir() => {
            return Err(format!("{} exists and is not a directory", dir.display()));
        }
        Ok(_) => {}
    }
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
        let is_link =
            is_workspace_link(&entry).map_err(|e| format!("Cannot read {}: {e}", dir.display()))?;
        if !is_link {
            return Err(format!(
                "{} is not empty (only a previous workspace can be reused)",
                dir.display()
            ));
        }
    }
    Ok(())
}

/// Whether a workspace directory entry is a link friring created — safe to
/// unlink because it never holds real content. A Unix symlink; on Windows a
/// directory symlink **or** an NTFS junction (the `mklink /J` fallback in
/// [`symlink`], which is a reparse point but *not* a symlink), so the
/// symlink-only guards above accept a junction-built workspace for rebuild,
/// reuse, and removal instead of refusing it as "real content".
#[cfg(not(windows))]
fn is_workspace_link(entry: &std::fs::DirEntry) -> io::Result<bool> {
    Ok(entry.file_type()?.is_symlink())
}

#[cfg(windows)]
fn is_workspace_link(entry: &std::fs::DirEntry) -> io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    if entry.file_type()?.is_symlink() {
        return Ok(true);
    }
    // `DirEntry::metadata` does not traverse a reparse point, so these are the
    // junction's own attributes, not its target's.
    Ok((entry.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0)
}

/// The workspace directory path for `id` **without building or touching it** —
/// for callers that need where an existing workspace lives while an agent is
/// still running in it (e.g. the companion shell pane): the destructive
/// rebuild in [`ensure_workspace`] would delete the running agent's cwd inode
/// out from under it.
pub fn workspace_path(id: &str) -> io::Result<PathBuf> {
    workspace_dir(id)
}

/// Remove the workspace directory for `id`, if present. Only the symlinks are
/// removed; the directories they point at are untouched. A missing workspace is
/// not an error.
pub fn remove_workspace(id: &str) -> io::Result<()> {
    let dir = workspace_dir(id)?;
    remove_dir_under_root(&dir)
}

/// Remove `dir` (recursively) only when it really sits under the workspaces
/// root, so a bad id can never delete something outside it. `remove_dir_all`
/// unlinks symlink entries without following them, so member repos are safe.
fn remove_dir_under_root(dir: &Path) -> io::Result<()> {
    let Some(base) = paths::workspaces_directory() else {
        return Ok(());
    };
    if !dir.starts_with(&base) || dir == base {
        return Ok(());
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Create a directory link at `link` pointing to `target`.
///
/// Workspace members are always directories (a worktree checkout or a plain
/// repo dir), so the Unix path uses a plain symlink and the Windows path uses a
/// directory symlink.
#[cfg(not(windows))]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows directory link. A directory symlink is preferred, but it requires
/// privilege (Developer Mode or admin) on Windows; when that fails we fall back
/// to an NTFS **junction** (`mklink /J`), which needs no special privilege and
/// also links directories across volumes.
#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => Ok(()),
        Err(_) => {
            // `mklink` is a cmd.exe builtin, so it runs via `cmd /C`.
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!(
                    "mklink /J failed for {} -> {}",
                    link.display(),
                    target.display()
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::TestPathGuard;

    fn temp_base() -> PathBuf {
        // Unique-ish per test via the thread name; avoids Date/rand (forbidden).
        let mut p = std::env::temp_dir();
        let t = std::thread::current();
        let name = t.name().unwrap_or("ws").replace("::", "-");
        p.push(format!("friring-ws-test-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn ensure_creates_one_symlink_per_member() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        let repo_a = base.join("src-a");
        let repo_b = base.join("src-b");
        std::fs::create_dir_all(&repo_a).unwrap();
        std::fs::create_dir_all(&repo_b).unwrap();

        let ws = ensure_workspace(
            "sess-1",
            &[
                ("webapp".to_string(), repo_a.clone()),
                ("infra".to_string(), repo_b.clone()),
            ],
        )
        .unwrap();

        assert!(ws.ends_with("workspaces/sess-1"));
        assert_eq!(std::fs::read_link(ws.join("webapp")).unwrap(), repo_a);
        assert_eq!(std::fs::read_link(ws.join("infra")).unwrap(), repo_b);
    }

    #[test]
    fn ensure_is_idempotent_and_reflects_new_members() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        ensure_workspace("s", &[("a".into(), a.clone())]).unwrap();
        let ws =
            ensure_workspace("s", &[("a".into(), a.clone()), ("b".into(), b.clone())]).unwrap();

        assert!(ws.join("a").exists());
        assert!(ws.join("b").exists());
    }

    #[test]
    fn colliding_names_are_disambiguated() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        let a = base.join("one");
        let b = base.join("two");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let ws = ensure_workspace(
            "s",
            &[("repo".into(), a.clone()), ("repo".into(), b.clone())],
        )
        .unwrap();

        assert_eq!(std::fs::read_link(ws.join("repo")).unwrap(), a);
        assert_eq!(std::fs::read_link(ws.join("repo-2")).unwrap(), b);
    }

    #[test]
    fn remove_deletes_links_not_targets() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        let repo = base.join("keepme");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("file.txt"), b"data").unwrap();

        let ws = ensure_workspace("s", &[("repo".into(), repo.clone())]).unwrap();
        assert!(ws.exists());

        remove_workspace("s").unwrap();
        assert!(!ws.exists());
        assert!(repo.join("file.txt").exists());
    }

    #[test]
    fn remove_missing_workspace_is_ok() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        assert!(remove_workspace("never-made").is_ok());
    }

    #[test]
    fn ensure_at_builds_and_rebuilds_at_custom_path() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        let repo_a = base.join("src-a");
        let repo_b = base.join("src-b");
        std::fs::create_dir_all(&repo_a).unwrap();
        std::fs::create_dir_all(&repo_b).unwrap();
        let custom = base.join("my-workspace");

        let ws = ensure_workspace_at(&custom, &[("webapp".to_string(), repo_a.clone())]).unwrap();
        assert_eq!(ws, custom);
        assert_eq!(std::fs::read_link(custom.join("webapp")).unwrap(), repo_a);

        // Rebuild reflects the new member set (idempotent, like `ensure_workspace`).
        ensure_workspace_at(
            &custom,
            &[
                ("webapp".to_string(), repo_a.clone()),
                ("infra".to_string(), repo_b.clone()),
            ],
        )
        .unwrap();
        assert!(custom.join("webapp").exists());
        assert_eq!(std::fs::read_link(custom.join("infra")).unwrap(), repo_b);
    }

    #[test]
    fn ensure_at_refuses_dir_with_real_files() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let custom = base.join("precious");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("keep.txt"), b"data").unwrap();

        assert!(ensure_workspace_at(&custom, &[("repo".into(), repo)]).is_err());
        assert!(custom.join("keep.txt").exists());
    }

    #[test]
    fn remove_at_refuses_non_workspace_dir_and_ignores_missing() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        let custom = base.join("notaws");
        std::fs::create_dir_all(custom.join("subdir")).unwrap();

        assert!(remove_workspace_at(&custom).is_err());
        assert!(custom.join("subdir").exists());
        assert!(remove_workspace_at(&base.join("never-made")).is_ok());
    }

    #[test]
    fn resolve_custom_dir_maps_bare_name_under_root_and_keeps_absolute() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        let root = paths::workspaces_directory().unwrap();

        assert_eq!(resolve_custom_workspace_dir("  ").unwrap(), None);
        assert_eq!(
            resolve_custom_workspace_dir("acme").unwrap(),
            Some(root.join("acme"))
        );
        assert_eq!(
            resolve_custom_workspace_dir("client/acme").unwrap(),
            Some(root.join("client/acme"))
        );
        let abs = base.join("elsewhere");
        assert_eq!(
            resolve_custom_workspace_dir(abs.to_str().unwrap()).unwrap(),
            Some(abs)
        );
        assert!(resolve_custom_workspace_dir("../escape").is_err());
    }

    // Windows-only: a drive-relative (`C:foo`) or rootless (`\foo`) input is not
    // `is_absolute()` yet would make `base.join` discard the workspaces root, so
    // it must be refused rather than silently escaping the sandbox. On Unix these
    // are ordinary relative segments (no `Prefix`/`RootDir`), so there is nothing
    // to reject and the case cannot be exercised.
    #[cfg(windows)]
    #[test]
    fn resolve_custom_dir_rejects_drive_relative_or_rootless_on_windows() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        assert!(resolve_custom_workspace_dir("C:foo").is_err());
        assert!(resolve_custom_workspace_dir(r"\rooted").is_err());
    }

    #[test]
    fn validate_custom_dir_accepts_missing_empty_or_symlink_only() {
        let base = temp_base();
        let _g = TestPathGuard::new(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        assert!(validate_custom_workspace_dir(&base.join("missing")).is_ok());

        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(validate_custom_workspace_dir(&empty).is_ok());

        let prior = base.join("prior");
        ensure_workspace_at(&prior, &[("repo".into(), repo.clone())]).unwrap();
        assert!(validate_custom_workspace_dir(&prior).is_ok());

        let file = base.join("afile");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_custom_workspace_dir(&file).is_err());

        let full = base.join("full");
        std::fs::create_dir_all(full.join("real")).unwrap();
        assert!(validate_custom_workspace_dir(&full).is_err());
    }
}
