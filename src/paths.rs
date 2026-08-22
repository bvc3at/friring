//! Centralized path resolution for application data files.
//!
//! This module provides a unified interface for resolving paths to:
//! - Config files (`~/.config/friring[-dev]/config.toml`)
//! - SQLite database (`~/.local/share/friring[-dev]/friring.db`)
//! - Log directories (`~/.local/share/friring[-dev]/`)
//!
//! Dev builds (`0.0.0-dev`) use `friring-dev` subdirectories to avoid
//! interfering with an installed release binary.
//!
//! ## Production Behavior
//!
//! By default, uses XDG Base Directory Specification:
//! - Prefers `$XDG_CONFIG_HOME` for config, fallback to `$HOME/.config`
//! - Prefers `$XDG_DATA_HOME` for data, fallback to `$HOME/.local/share`
//!
//! ## Testing Behavior
//!
//! Tests can override path resolution using `TestPathGuard`:
//! ```ignore
//! #[test]
//! fn test_with_custom_paths() {
//!     let temp_dir = tempfile::TempDir::new().unwrap();
//!     let _guard = TestPathGuard::new(temp_dir.path());
//!
//!     // All paths now resolve under temp_dir
//!     let config = config_file().unwrap();
//!     assert_eq!(config, temp_dir.path().join("config.toml"));
//! }
//! ```

use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// Env var pinning the resolved config app dir for a child process (an agent
/// whose hook calls `friring-cli`), so it targets the same config the spawning
/// friring uses regardless of XDG/binary-flavor/tmux-server-env drift. Injected
/// at spawn ([`crate::session_ops`]); consumed by `config_app_dir`.
pub const CONFIG_DIR_OVERRIDE_ENV: &str = "FRIRING_CONFIG_DIR";
/// Data counterpart of [`CONFIG_DIR_OVERRIDE_ENV`] (`FRIRING_DATA_DIR`).
pub const DATA_DIR_OVERRIDE_ENV: &str = "FRIRING_DATA_DIR";
/// Env var naming the directory an agent's hooks write their metrics samples
/// into ([`metrics_directory`]).
///
/// Named here rather than spelled at each use because it is one of the three
/// variables that carry a **host path**: they are injected only for a launch
/// that runs on this machine, and taken back out of one that turns out to run
/// inside a sandbox place (`crate::agent::backend::HOST_PATH_ENV`).
pub const METRICS_DIR_ENV: &str = "FRIRING_METRICS_DIR";

/// Env var naming the file a **sandboxed** agent's hooks append their state to
/// ([`session_signal_file`]).
///
/// Set only where a boundary is actually applied, and only by the launch that
/// applies it, because its presence is what the shipped hook payloads branch
/// on: unset means "call `friring-cli session signal`", which is what every
/// unsandboxed session keeps doing. It is *not* injected with the other
/// `FRIRING_*` identity variables for that reason.
///
/// The whole path travels, rather than the directory plus a name the payloads
/// would have to spell themselves: the hook and this module then cannot disagree
/// about which file the channel is, and a rename here can never leave a shipped
/// payload writing somewhere nothing reads.
pub const SIGNAL_FILE_ENV: &str = "FRIRING_SIGNAL_FILE";

/// The file inside [`session_signal_dir`] the hook appends a state word to.
///
/// One fixed name, so the poll opens exactly one path per session rather than
/// listing a directory whose entries an agent chooses.
pub const SIGNAL_FILE_NAME: &str = "status";

/// Largest status file friring will read, in bytes.
///
/// The channel carries a word per hook event (8 bytes at most), and the poll
/// takes the file every ~100 ms, so this leaves room for hundreds of events
/// between two sweeps and none at all for a file worth streaming. A larger one
/// is dropped rather than truncated: the writer is inside the boundary, so a
/// file this size is not an agent reporting its state.
pub const MAX_SIGNAL_BYTES: u64 = 4096;

/// Returns "friring-dev" for dev builds, "friring" for release builds.
#[cfg_attr(test, allow(dead_code))] // only used by the non-test XDG fallback
fn app_dir_name() -> &'static str {
    if cfg!(dev_build) {
        "friring-dev"
    } else {
        "friring"
    }
}

/// The user's home directory: `$HOME` on Unix, `%USERPROFILE%` on Windows.
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).map(PathBuf::from)
}

/// Whether `exe` resolves on `PATH`. A minimal lookup that avoids pulling in a
/// `which` crate for a one-off probe (used to detect optional helper binaries
/// like `wsl.exe` / `powershell.exe`); cheap PATH scan, no process spawn.
pub fn which_on_path(exe: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(exe).exists())
}

/// Base directory for config files. `$XDG_CONFIG_HOME` wins on every platform
/// (some users set it on Windows too); otherwise `%APPDATA%` on Windows,
/// `$HOME/.config` on Unix.
#[cfg_attr(test, allow(dead_code))] // only used by the non-test XDG fallback
fn config_base() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(x));
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        home_dir().map(|h| h.join(".config"))
    }
}

/// Base directory for data files. `$XDG_DATA_HOME` wins on every platform;
/// otherwise `%LOCALAPPDATA%` on Windows, `$HOME/.local/share` on Unix.
#[cfg_attr(test, allow(dead_code))] // only used by the non-test XDG fallback
fn data_base() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(x));
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        home_dir().map(|h| h.join(".local").join("share"))
    }
}

/// Per-process temp sandbox for the XDG fallback in **test builds only**.
///
/// The unit-test harness (`cargo test`/`nextest`) frequently runs *inside* a
/// live friring session (the dev shell is itself an agent session), whose env
/// carries `FRIRING_CONFIG_DIR`/`FRIRING_DATA_DIR` pointing at the developer's
/// **real** config/data dirs (injected so an agent's `friring-cli` hook targets
/// the same DB — see `session_ops::inject_friring_env`). Honoring those in tests
/// — or falling through to the real `$HOME/.config/friring` — let any unguarded
/// test that writes config (settings save, hooks install, keybindings) clobber
/// the user's live settings. So in test builds the XDG fallback ignores the
/// override env entirely and resolves under a `<pid>`-scoped temp dir instead;
/// `TestPathGuard`/`set_test_dir` (the `Override` strategy) still wins where a
/// test wants a specific base.
#[cfg(test)]
fn test_sandbox_base() -> PathBuf {
    std::env::temp_dir().join(format!("friring-unittest-{}", std::process::id()))
}

/// Resolved friring config app dir. A `FRIRING_CONFIG_DIR` env override (the
/// already-resolved dir, incl. the `friring`/`friring-dev` segment) wins — this
/// is how the TUI pins child processes (agent hooks calling `friring-cli`) to
/// the *same* config it uses, immune to a stale tmux-server env or which
/// `friring-cli` binary is on PATH. Otherwise `<config_base>/<app>`. In test
/// builds the env override is ignored in favor of a temp sandbox — see
/// [`test_sandbox_base`].
#[cfg(not(test))]
fn config_app_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os(CONFIG_DIR_OVERRIDE_ENV).filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(x));
    }
    Some(config_base()?.join(app_dir_name()))
}

/// Test build: pin the config dir to a temp sandbox, ignoring the inherited
/// `FRIRING_CONFIG_DIR` — see [`test_sandbox_base`].
#[cfg(test)]
fn config_app_dir() -> Option<PathBuf> {
    Some(test_sandbox_base().join("config"))
}

/// Resolved friring data app dir; see [`config_app_dir`] (`FRIRING_DATA_DIR`).
#[cfg(not(test))]
fn data_app_dir() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os(DATA_DIR_OVERRIDE_ENV).filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(x));
    }
    Some(data_base()?.join(app_dir_name()))
}

/// Test build: pin the data dir to a temp sandbox; see [`config_app_dir`].
#[cfg(test)]
fn data_app_dir() -> Option<PathBuf> {
    Some(test_sandbox_base().join("data"))
}

/// `<config_app_dir>/<filename>`.
fn xdg_config_subpath(filename: &str) -> Option<PathBuf> {
    Some(config_app_dir()?.join(filename))
}

/// `<data_app_dir>/<segments...>`.
fn xdg_data_subpath(segments: &[&str]) -> Option<PathBuf> {
    let mut p = data_app_dir()?;
    for seg in segments {
        p.push(seg);
    }
    Some(p)
}

/// Categories of application paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Config file: `~/.config/friring/config.toml` (legacy, used for migration only)
    Config,
    /// Log directory: `~/.local/share/friring/`
    LogDir,
    /// SQLite database: `~/.local/share/friring/friring.db`
    Database,
    /// Agent metrics files: `~/.local/share/friring/metrics/`
    MetricsDir,
    /// Embedded built-in extensions materialized for install:
    /// `~/.local/share/friring/builtin-extensions/`
    BuiltinExtensionsDir,
    /// Git worktrees: `~/.local/share/friring/worktrees/`
    WorktreesDir,
    /// Per-session multi-repo symlink workspaces:
    /// `~/.local/share/friring/workspaces/`
    WorkspacesDir,
    /// Per-session sandbox status-signal directories:
    /// `~/.local/share/friring/signals/`
    SignalsDir,
    /// User keybindings JSON file: `~/.config/friring/keybindings.json`
    KeybindingsFile,
}

/// Path resolution strategy (thread-local).
#[derive(Debug, PartialEq)]
enum PathStrategy {
    /// Production: Use XDG Base Directory Specification.
    Xdg,
    /// Testing: Use custom base directory for all paths.
    Override(PathBuf),
}

thread_local! {
    static PATH_STRATEGY: RefCell<PathStrategy> = const { RefCell::new(PathStrategy::Xdg) };
}

/// Resolve a path based on the current strategy.
///
/// # Returns
///
/// - `Some(path)` - Successfully resolved path
/// - `None` - Could not resolve path (e.g., HOME not set in XDG mode)
pub fn resolve(kind: PathKind) -> Option<PathBuf> {
    PATH_STRATEGY.with(|strategy| {
        let s = strategy.borrow();
        match *s {
            PathStrategy::Xdg => resolve_xdg(kind),
            PathStrategy::Override(ref base) => Some(resolve_override(base, kind)),
        }
    })
}

/// Resolve a path using XDG Base Directory Specification.
fn resolve_xdg(kind: PathKind) -> Option<PathBuf> {
    match kind {
        PathKind::Config => xdg_config_subpath("config.toml"),
        PathKind::Database => xdg_data_subpath(&["friring.db"]),
        PathKind::LogDir => xdg_data_subpath(&[]),
        PathKind::MetricsDir => xdg_data_subpath(&["metrics"]),
        PathKind::BuiltinExtensionsDir => xdg_data_subpath(&["builtin-extensions"]),
        PathKind::WorktreesDir => xdg_data_subpath(&["worktrees"]),
        PathKind::WorkspacesDir => xdg_data_subpath(&["workspaces"]),
        PathKind::SignalsDir => xdg_data_subpath(&["signals"]),
        PathKind::KeybindingsFile => xdg_config_subpath("keybindings.json"),
    }
}

/// Resolve a path using a custom base directory (for testing).
fn resolve_override(base: &Path, kind: PathKind) -> PathBuf {
    match kind {
        PathKind::Config => base.join("config.toml"),
        PathKind::LogDir => base.to_path_buf(),
        PathKind::Database => base.join("friring.db"),
        PathKind::MetricsDir => base.join("metrics"),
        PathKind::BuiltinExtensionsDir => base.join("builtin-extensions"),
        PathKind::WorktreesDir => base.join("worktrees"),
        PathKind::WorkspacesDir => base.join("workspaces"),
        PathKind::SignalsDir => base.join("signals"),
        PathKind::KeybindingsFile => base.join("keybindings.json"),
    }
}

/// Resolve the config file path.
///
/// Returns: `$XDG_CONFIG_HOME/friring/config.toml` or `$HOME/.config/friring/config.toml`
pub fn config_file() -> Option<PathBuf> {
    resolve(PathKind::Config)
}

/// Resolve the log directory path.
///
/// Returns: `$XDG_DATA_HOME/friring/` or `$HOME/.local/share/friring/`
pub fn log_directory() -> Option<PathBuf> {
    resolve(PathKind::LogDir)
}

/// Resolve the database file path.
///
/// Returns: `$XDG_DATA_HOME/friring/friring.db` or `$HOME/.local/share/friring/friring.db`
pub fn database_file() -> Option<PathBuf> {
    resolve(PathKind::Database)
}

/// Validate that `name` is a safe single-segment identifier — non-empty,
/// no dot-prefix, no slashes / backslashes / `..`, max 64 chars. Used by
/// `session_ops::spawn` to guard names that become on-disk paths.
pub fn validate_safe_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Name cannot be empty".into());
    }
    if name.len() > 64 {
        return Err("Name too long (max 64 characters)".into());
    }
    if name.starts_with('.') {
        return Err("Name cannot start with '.'".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("Name contains invalid characters".into());
    }
    Ok(())
}

/// Resolve the agent metrics directory path.
///
/// Returns: `$XDG_DATA_HOME/friring/metrics/` or `$HOME/.local/share/friring/metrics/`
pub fn metrics_directory() -> Option<PathBuf> {
    resolve(PathKind::MetricsDir)
}

/// The statusline metrics file for one agent conversation.
///
/// Keyed by `agent_session_id` (the id friring pins into the agent and exports
/// as `FRIRING_SESSION_ID`), not by [`SessionId`](crate::session::SessionId) —
/// the writer is the agent's own statusline, which only knows its conversation
/// id. Shared by the TUI's per-tick poll and `friring-cli session metrics` so
/// both look in exactly one place.
pub fn session_metrics_file(agent_session_id: &str) -> Option<PathBuf> {
    Some(metrics_directory()?.join(format!("{agent_session_id}.json")))
}

/// Directory where embedded built-in extensions are materialized so the
/// extension installer can treat them as a local source.
///
/// Returns: `$XDG_DATA_HOME/friring/builtin-extensions/` or
/// `$HOME/.local/share/friring/builtin-extensions/`
pub fn builtin_extensions_directory() -> Option<PathBuf> {
    resolve(PathKind::BuiltinExtensionsDir)
}

/// Resolve the worktrees directory path.
///
/// Returns: `$XDG_DATA_HOME/friring/worktrees/` or `$HOME/.local/share/friring/worktrees/`
pub fn worktrees_directory() -> Option<PathBuf> {
    resolve(PathKind::WorktreesDir)
}

/// Resolve the multi-repo workspaces directory path.
///
/// Returns: `$XDG_DATA_HOME/friring/workspaces/` or
/// `$HOME/.local/share/friring/workspaces/`
pub fn workspaces_directory() -> Option<PathBuf> {
    resolve(PathKind::WorkspacesDir)
}

/// The **default** multi-repo symlink workspace for one agent conversation:
/// `<workspaces>/<sanitized agent_session_id>`.
///
/// Pure derivation — nothing is created or read. The single definition of that
/// path: [`crate::workspace::workspace_path`] builds it here, and
/// `friring-cli session activity` derives the same launch cwd for a session it
/// cannot ask a running app about. `None` when no workspaces root resolves, or
/// when the id sanitizes to nothing (defensive — it is a UUID in practice).
pub fn session_workspace_dir(agent_session_id: &str) -> Option<PathBuf> {
    let segment = sanitize_workspace_segment(agent_session_id);
    if segment.is_empty() {
        return None;
    }
    Some(workspaces_directory()?.join(segment))
}

/// Resolve the sandbox status-signal root: `<data>/signals/`.
///
/// Host-only, and deliberately **not** under the sandbox's own tree: a launch
/// grants one directory beneath this root and nothing else, so the root itself
/// stays a place only friring writes — which is what makes
/// [`take_session_signal`]'s staging step safe.
pub fn signals_directory() -> Option<PathBuf> {
    resolve(PathKind::SignalsDir)
}

/// The status-signal directory of one session, whether or not it exists yet:
/// `<data>/signals/<key>`.
///
/// `session_key` is the same key the rest of a sandboxed launch is filed under
/// (friring's session id in practice), reduced to a single path segment.
pub fn session_signal_dir(session_key: &str) -> Option<PathBuf> {
    Some(signals_directory()?.join(sanitize_signal_key(session_key)))
}

/// The status file itself: `<data>/signals/<key>/status`.
///
/// The value of [`SIGNAL_FILE_ENV`], and the one path a sandboxed agent's hooks
/// are told about. The directory around it is what a launch grants read-write —
/// see [`create_session_signal_dir`].
pub fn session_signal_file(session_key: &str) -> Option<PathBuf> {
    Some(session_signal_dir(session_key)?.join(SIGNAL_FILE_NAME))
}

/// Where [`take_session_signal`] moves a status file before reading it.
///
/// A sibling of the session's directory rather than a child of it, because the
/// whole point is to land somewhere the agent cannot reach: it holds the one
/// directory, not this root.
///
/// One name per session rather than per process, so a friring that died
/// mid-take leaves at most one file and the next take overwrites it. Two live
/// instances watching one session can therefore take from under each other —
/// which costs a duplicate or a dropped report of a state they both derive the
/// same way, and never a wrong one.
fn signal_staging_path(session_key: &str) -> Option<PathBuf> {
    Some(signals_directory()?.join(format!("{}.taken", sanitize_signal_key(session_key))))
}

/// Reduce a session key to one path segment that cannot become a path.
///
/// Every character outside `[A-Za-z0-9._-]` becomes `-`, so no separator, drive
/// letter or NUL survives; a name made only of dots (`..`) is replaced outright,
/// because those *are* legal segments and `join`ing one would walk upwards.
/// Keys are UUIDs in practice — this is the guard that keeps that from being
/// load-bearing.
fn sanitize_signal_key(raw: &str) -> String {
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
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        "session".to_string()
    } else {
        cleaned
    }
}

/// Create `path` and its parents `0700`, adopting a directory that is already
/// there.
///
/// The shared primitive for every directory friring mints on a sandbox's
/// behalf. Two properties, both because what lands inside is either a security
/// policy or a channel out of a boundary:
///
/// - A **symlink** at the final component is refused rather than followed, so
///   nothing planted there can redirect the writes that follow it elsewhere.
/// - An adopted directory has its mode **re-asserted**: `DirBuilder::mode`
///   applies only to the components it actually creates, so a directory left
///   behind by an older, laxer build would otherwise keep its old permissions.
pub fn create_private_dir(path: &Path) -> Result<(), String> {
    let io_err = |detail: String| format!("{}: {detail}", path.display());

    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io_err(
                "is a symlink; friring will not write a private directory through one".to_string(),
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

/// The two paths a sandboxed launch needs: what to expose, and what to name.
///
/// Separate fields rather than one path plus a `join` at the call site, so the
/// launch never spells [`SIGNAL_FILE_NAME`] itself and cannot drift from the
/// poll that reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalChannel {
    /// The per-session directory to expose read-write, and **only** this.
    pub dir: PathBuf,
    /// The file inside it, for [`SIGNAL_FILE_ENV`].
    pub file: PathBuf,
}

/// Mint the status-signal channel for one sandboxed launch.
///
/// The directory is `0700` and is the **only** thing a launch exposes read-write
/// for this purpose — see `docs/SANDBOX.md` §Status signals and ADR-29, which is
/// why the database is not.
///
/// It starts empty: whatever the previous run of this session left behind is
/// dropped here, so a `done` written before a restart cannot be replayed as the
/// new run's first report. The *directory* is adopted rather than recreated —
/// the launch is about to hand its path to a sandbox, and a fresh inode would
/// leave an already-running agent writing into an unlinked one.
///
/// # Errors
///
/// No data directory resolves, or something that is not a directory friring
/// owns — a symlink above all — sits where the directory belongs. Both fail the
/// launch: a signal directory friring cannot vouch for is worse than none,
/// because the poll would act on whatever it collected.
pub fn create_session_signal_dir(session_key: &str) -> Result<SignalChannel, String> {
    let root = signals_directory().ok_or(NO_SIGNAL_ROOT)?;
    create_private_dir(&root)?;
    let dir = session_signal_dir(session_key).ok_or(NO_SIGNAL_ROOT)?;
    create_private_dir(&dir)?;
    let file = dir.join(SIGNAL_FILE_NAME);
    remove_anything_at(&file);
    if let Some(staged) = signal_staging_path(session_key) {
        remove_anything_at(&staged);
    }
    Ok(SignalChannel { dir, file })
}

/// Drop one session's signal directory and anything staged out of it.
///
/// Best effort, and called where the rest of a session's per-launch state is
/// dropped: failing to clean up must never be the thing that reports an error.
/// Nothing here follows a symlink — `remove_dir_all` refuses one, and unlinking
/// never traverses the final component.
pub fn remove_session_signal_dir(session_key: &str) {
    if let Some(dir) = session_signal_dir(session_key) {
        remove_anything_at(&dir);
    }
    if let Some(staged) = signal_staging_path(session_key) {
        remove_anything_at(&staged);
    }
}

/// Take one session's status file and return its text, or `None` when there is
/// nothing to take or nothing friring will read.
///
/// The take is a `rename(2)` out of the session's directory into the signals
/// root, and that single syscall is what makes the rest of this safe. It is
/// atomic, it never follows a symlink, and it never opens anything — so once it
/// returns, the object friring is about to inspect sits in a directory no
/// sandbox was granted, and the agent cannot swap it for something else between
/// the check and the read. Everything after the rename is therefore a decision
/// about a fixed inode rather than a race:
///
/// - **Not a regular file** — a symlink, a FIFO, a directory, a device node —
///   is dropped unread. The FIFO is the one that matters: opening one blocks
///   until a writer appears, and this runs on the render loop, so reading it in
///   place would let an agent freeze the whole TUI with `mkfifo`.
/// - **Too large** is dropped, checked before the open *and* enforced during the
///   read: a descriptor the agent still holds keeps writing to the same inode
///   after the rename, so the size at `stat` time is not a bound on what an
///   unbounded read would return.
/// - **Not UTF-8** is dropped rather than lossily converted, so no byte sequence
///   is reshaped into something that might parse.
///
/// Taking rather than peeking is also what keeps the channel honest in time:
/// each write is delivered once, and a file left behind by a crashed run is
/// consumed instead of re-reported forever.
///
/// The returned text is still hostile — it is the agent's own bytes. Only
/// `session::status_signal::parse_status_signal` decides what it means, and it
/// answers with an enum.
pub fn take_session_signal(session_key: &str) -> Option<String> {
    let inbox = session_signal_file(session_key)?;
    let staged = signal_staging_path(session_key)?;
    // A leftover from a run that died mid-take would make the rename fail (or,
    // if it is a directory, keep failing forever).
    remove_anything_at(&staged);
    std::fs::rename(&inbox, &staged).ok()?;
    let text = read_taken_signal(&staged);
    remove_anything_at(&staged);
    text
}

/// Read a status file that has already been taken out of the agent's reach.
fn read_taken_signal(staged: &Path) -> Option<String> {
    use std::io::Read as _;

    let meta = std::fs::symlink_metadata(staged).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_SIGNAL_BYTES {
        return None;
    }
    let file = std::fs::File::open(staged).ok()?;
    let mut bytes = Vec::new();
    // One byte past the cap, so a file that grew under an open descriptor is
    // detected rather than silently truncated into something that parses.
    file.take(MAX_SIGNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_SIGNAL_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Unlink whatever is at `path`, whichever kind of thing it turned out to be.
///
/// `remove_file` covers every non-directory — a regular file, a FIFO, a socket,
/// and a symlink, which it unlinks itself rather than following;
/// `remove_dir_all` covers the directory an agent planted instead, and refuses
/// to descend through a symlink. Best effort: every caller is either cleaning up
/// or about to overwrite.
fn remove_anything_at(path: &Path) {
    if std::fs::remove_file(path).is_ok() {
        return;
    }
    let _ = std::fs::remove_dir_all(path);
}

/// The failure both signal-directory callers share.
const NO_SIGNAL_ROOT: &str = "friring cannot resolve its data directory, so it has nowhere to \
                              keep a sandboxed session's status signals";

/// Resolve the user keybindings file path.
///
/// Returns: `$XDG_CONFIG_HOME/friring/keybindings.json` or
/// `$HOME/.config/friring/keybindings.json`.
pub fn keybindings_file() -> Option<PathBuf> {
    resolve(PathKind::KeybindingsFile)
}

/// Resolve the Claude Code config root:
/// `config_dir_override` → `$CLAUDE_CONFIG_DIR` → `~/.claude`.
fn claude_config_root(config_dir_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = config_dir_override {
        Some(p.to_path_buf())
    } else if let Some(env) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(PathBuf::from(env))
    } else {
        home_dir().map(|h| h.join(".claude"))
    }
}

/// The Claude Code `projects/` directory under the resolved config root. Each
/// session lives at `projects/<slug>/<agent_session_id>{.jsonl,/}`, with its
/// subagents/workflows under `projects/<slug>/<agent_session_id>/subagents/`.
/// Root resolution: `config_dir_override` → `$CLAUDE_CONFIG_DIR` → `~/.claude`.
pub fn claude_projects_dir(config_dir_override: Option<&Path>) -> Option<PathBuf> {
    claude_config_root(config_dir_override).map(|r| r.join("projects"))
}

/// The Claude Code daemon roster (`<root>/daemon/roster.json`): the live registry
/// of background/detached workers. Same root resolution as
/// [`claude_projects_dir`]. Used by the activity scan to attribute a background
/// worker's `subagents/` tree back to the friring session that launched it.
pub fn claude_daemon_roster(config_dir_override: Option<&Path>) -> Option<PathBuf> {
    claude_config_root(config_dir_override).map(|r| r.join("daemon").join("roster.json"))
}

/// The Claude Code jobs directory (`<root>/jobs`): per-background-job state
/// (`<short>/state.json`) that **persists after a run settles**, carrying the
/// live agent grid + status. Same root resolution as [`claude_projects_dir`].
pub fn claude_jobs_dir(config_dir_override: Option<&Path>) -> Option<PathBuf> {
    claude_config_root(config_dir_override).map(|r| r.join("jobs"))
}

/// Claude Code's `projects/<slug>` directory name for a working directory:
/// every non-ASCII-alphanumeric byte becomes `-` (so `/a/b.c` → `-a-b-c`).
///
/// For *finding* an existing session dir friring never computes a slug — it
/// scans `projects/*/` (see `claude_transcript_exists`) precisely because this
/// rule is undocumented and version-specific. Computing it is only needed when
/// *creating* the destination dir for a conversation import, where there is
/// nothing to scan yet. `claude` resolves its cwd via `getcwd`, which returns
/// the physical path, so callers must canonicalize first or the slugs diverge
/// on any symlinked component.
///
/// The rule is **per UTF-8 byte**, not per `char`: a multi-byte character
/// yields one `-` per byte (verified against Claude Code v2.1.206 — `café`
/// slugs to `caf--` because `é` is two bytes). A `chars()`-based rule would
/// undercount and stage the transcript into a dir `--resume` never reads.
pub fn claude_project_slug(canonical_cwd: &Path) -> String {
    canonical_cwd
        .to_string_lossy()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '-'
            }
        })
        .collect()
}

/// Mistral Vibe's per-session log root: `$VIBE_HOME/logs/session` →
/// `~/.vibe/logs/session`. Each session is a
/// `<prefix>_<utc-ts>_<shortid>/` dir holding `meta.json` + `messages.jsonl`
/// (subagents nested under `agents/`). `home_override` is the test hook,
/// mirroring [`claude_projects_dir`]'s `config_dir_override`.
pub fn vibe_sessions_dir(home_override: Option<&Path>) -> Option<PathBuf> {
    let root = if let Some(p) = home_override {
        p.to_path_buf()
    } else if let Some(env) = std::env::var_os("VIBE_HOME") {
        PathBuf::from(env)
    } else {
        home_dir()?.join(".vibe")
    };
    Some(root.join("logs").join("session"))
}

/// Returns true if a Claude transcript file `<agent_session_id>.jsonl` exists
/// under `<root>/projects/*/`.
///
/// Used by restart paths to decide between `--resume` (transcript exists) and
/// `--session-id` (fresh start with same id).
pub fn claude_transcript_exists(
    agent_session_id: &str,
    config_dir_override: Option<&Path>,
) -> bool {
    let Some(projects) = claude_projects_dir(config_dir_override) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return false;
    };
    let target = format!("{agent_session_id}.jsonl");
    for entry in entries.flatten() {
        if entry.path().join(&target).is_file() {
            return true;
        }
    }
    false
}

/// Override path resolution for all paths to use a custom base directory.
///
/// This is primarily intended for testing. All paths will resolve under the given base:
/// - `config_file()` → `base/config.toml`
/// - `log_directory()` → `base/`
/// - `database_file()` → `base/friring.db`
///
/// # Note
///
/// This change is thread-local and affects only the current thread.
/// Use `reset_to_xdg()` or `TestPathGuard` to restore XDG behavior.
pub fn set_test_dir(base: impl Into<PathBuf>) {
    PATH_STRATEGY.with(|strategy| {
        *strategy.borrow_mut() = PathStrategy::Override(base.into());
    });
}

/// Reset path resolution back to XDG Base Directory Specification.
pub fn reset_to_xdg() {
    PATH_STRATEGY.with(|strategy| {
        *strategy.borrow_mut() = PathStrategy::Xdg;
    });
}

/// RAII guard for test path overrides.
///
/// Automatically resets to XDG behavior when dropped.
/// Simplifies test setup/teardown:
///
/// ```ignore
/// #[test]
/// fn test_with_override() {
///     let temp_dir = tempfile::TempDir::new().unwrap();
///     let _guard = TestPathGuard::new(temp_dir.path());
///
///     // Paths are overridden in this scope...
///     let config = config_file();
///
///     // Automatically reset on drop
/// }
/// ```
pub struct TestPathGuard;

impl TestPathGuard {
    /// Create a new test path guard with the given base directory.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        set_test_dir(base_dir);
        TestPathGuard
    }
}

impl Drop for TestPathGuard {
    fn drop(&mut self) {
        reset_to_xdg();
    }
}

/// Expand a leading `~` followed by a path separator to the user's home
/// directory. On Windows both separators are accepted (`~/` and `~\`); on Unix
/// only `~/` is (a backslash is a legal filename character there).
///
/// - `"~/foo"` → `"/home/user/foo"`
/// - `"~\\foo"` → `"C:\\Users\\user\\foo"` (Windows)
/// - `"~"` → `"/home/user"`
/// - `"/absolute/path"` → unchanged
/// - `"relative/path"` → unchanged
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = strip_tilde_prefix(path) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Strip a leading `~` + path separator, returning the remainder. Accepts `~/`
/// everywhere and `~\` on Windows (where `\` is a path separator).
fn strip_tilde_prefix(path: &str) -> Option<&str> {
    if let Some(rest) = path.strip_prefix("~/") {
        return Some(rest);
    }
    if cfg!(windows) {
        return path.strip_prefix("~\\");
    }
    None
}

/// Short display label for a repo/dir path: the final path component,
/// falling back to the full path when there is no file name (e.g. `/`).
///
/// - `/home/user/Repositories/friring` → `friring`
/// - `/home/user/Repositories/friring/` → `friring` (trailing slash ignored)
/// - `/` → `/`
pub fn display_path(path: &Path) -> String {
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.display().to_string(),
    }
}

/// Full path for display with the home directory shortened to `~`. Used where
/// a basename alone would be ambiguous (e.g. "import repos from ~/code").
pub fn display_path_tilde(path: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// Find the longest common prefix among a slice of strings.
fn longest_common_prefix(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    let first = &strings[0];
    let mut prefix_len = first.len();
    for s in &strings[1..] {
        prefix_len = prefix_len.min(s.len());
        for (i, (a, b)) in first.bytes().zip(s.bytes()).enumerate() {
            if a != b {
                prefix_len = prefix_len.min(i);
                break;
            }
        }
    }
    // A byte-wise LCP can land mid-char when strings diverge inside a
    // multibyte char — floor to a boundary or the slice below panics.
    while !first.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    first[..prefix_len].to_string()
}

/// Directory names directly under `parent` that start with `prefix`. Hidden
/// entries (`.`-prefixed) are included only when `prefix` itself is hidden.
/// Returns an empty vec when `parent` can't be read.
pub(crate) fn matching_dir_names(parent: &Path, prefix: &str) -> Vec<String> {
    let show_hidden = prefix.starts_with('.');
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            name.starts_with(prefix).then_some(name)
        })
        .collect()
}

/// Split a path-style input into the directory to list and the name prefix
/// being typed, tilde-expanded. A trailing path separator (`/` everywhere,
/// plus `\` on Windows — tilde expansion yields `C:\Users\me\`) means "list
/// this directory's contents" (empty prefix). `None` for an empty input or
/// one with no listable parent.
pub(crate) fn split_path_input(input: &str) -> Option<(PathBuf, String)> {
    if input.is_empty() {
        return None;
    }
    let expanded = expand_tilde(input);
    // A bare `~` is a whole-token home reference: list home's *contents*, like
    // `~/` does. Splitting the expanded home path (`/home/me`) at its last
    // separator would instead list home's *siblings* matching `me`.
    if input == "~" {
        return Some((expanded, String::new()));
    }
    let expanded_str = expanded.to_str().unwrap_or(input);
    // Split at the last separator textually. `Path::parent()`/`file_name()`
    // would normalize a trailing `.` component away, so "dir/." would list
    // dir's *parent* — breaking "type `.` to see hidden dirs".
    let sep = expanded_str
        .char_indices()
        .rev()
        .find(|(_, c)| std::path::is_separator(*c))?
        .0;
    let (parent, prefix) = expanded_str.split_at(sep + 1);
    Some((PathBuf::from(parent), prefix.to_string()))
}

/// Fish-style directory path completion.
///
/// Given a partial path input, returns the suffix to complete it.
/// Only considers directories. Hidden entries (starting with `.`) are
/// included only when the user's prefix starts with `.`.
///
/// # Examples
///
/// - Input `"/home/us"` with `/home/user/` existing → `Some("er/")`
/// - Input `"/home/user/"` → suggests first common prefix of children
/// - Input `"/nonexistent"` → `None`
pub fn complete_directory_path(input: &str) -> Option<String> {
    let (parent, prefix) = split_path_input(input)?;

    let matches = matching_dir_names(&parent, &prefix);

    if matches.is_empty() {
        return None;
    }

    let common = longest_common_prefix(&matches);
    let beyond_typed = &common[prefix.len()..];
    if beyond_typed.is_empty() && matches.len() > 1 {
        return None;
    }

    let completed = parent.join(&common);
    let suffix = if completed.is_dir() {
        format!("{beyond_typed}/")
    } else {
        beyond_typed.to_string()
    };

    if suffix.is_empty() {
        None
    } else {
        Some(suffix)
    }
}

/// Reduce a display name / session id to a safe single path segment for a
/// symlink-workspace link or directory name. Shared by the local
/// (`workspace`) and remote (`git`) workspace builders so their layouts match
/// by construction — neither may depend on the other.
pub(crate) fn sanitize_workspace_segment(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '-',
            c if c.is_whitespace() => '-',
            c => c,
        })
        .collect();
    cleaned.trim_matches(['.', '-']).to_string()
}

/// Sanitize `name` and make it unique within `used` by appending `-2`, `-3`,
/// …; an empty sanitized name falls back to `repo`. Same sharing rationale as
/// [`sanitize_workspace_segment`].
pub(crate) fn unique_link_name(name: &str, used: &mut std::collections::HashSet<String>) -> String {
    let sanitized = sanitize_workspace_segment(name);
    let base = if sanitized.is_empty() {
        "repo".to_string()
    } else {
        sanitized
    };
    if used.insert(base.clone()) {
        return base;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env var `home_dir()` reads on this platform: `USERPROFILE` on
    /// Windows, `HOME` elsewhere. Tests that exercise tilde expansion source the
    /// home directory from the same var so they pass on every target.
    const HOME_VAR: &str = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

    #[test]
    fn display_path_uses_basename() {
        assert_eq!(
            display_path(Path::new("/home/user/Repositories/friring")),
            "friring"
        );
    }

    #[test]
    fn which_on_path_finds_present_and_rejects_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        // `which_on_path` checks for the file verbatim (no `.exe` munging), so a
        // plain marker filename resolves identically on every platform.
        let marker = "tbx_which_probe_marker";
        std::fs::write(dir.path().join(marker), b"").unwrap();

        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", dir.path());
        let found = which_on_path(marker);
        let missing = which_on_path("tbx_which_probe_absent");
        match saved {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }

        assert!(found, "marker on PATH should be found");
        assert!(!missing, "a name not on PATH should not be found");
    }

    #[test]
    fn display_path_ignores_trailing_slash() {
        assert_eq!(
            display_path(Path::new("/home/user/Repositories/friring/")),
            "friring"
        );
    }

    #[test]
    fn display_path_falls_back_to_full_path_without_file_name() {
        assert_eq!(display_path(Path::new("/")), "/");
    }

    #[test]
    fn transcript_exists_detects_file_under_any_project_slug() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("projects").join("-some-slug");
        std::fs::create_dir_all(&proj).unwrap();
        let sid = "11111111-2222-3333-4444-555555555555";
        std::fs::write(proj.join(format!("{sid}.jsonl")), b"").unwrap();

        assert!(claude_transcript_exists(sid, Some(tmp.path())));
        assert!(!claude_transcript_exists("not-present", Some(tmp.path())));
    }

    #[test]
    fn transcript_exists_returns_false_when_projects_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!claude_transcript_exists("any-id", Some(tmp.path())));
    }

    #[test]
    fn default_strategy_is_xdg() {
        reset_to_xdg();
        PATH_STRATEGY.with(|s| {
            assert_eq!(*s.borrow(), PathStrategy::Xdg);
        });
    }

    #[test]
    fn test_build_ignores_config_and_data_dir_override_env() {
        // Regression: the unit-test harness often runs *inside* a live friring
        // session whose env carries FRIRING_CONFIG_DIR/FRIRING_DATA_DIR pointing
        // at the developer's real config/data. On the XDG strategy a test build
        // must ignore those and stay under the per-process temp sandbox, so an
        // unguarded config write can never clobber the user's live settings.
        reset_to_xdg();
        let saved_cfg = std::env::var_os(CONFIG_DIR_OVERRIDE_ENV);
        let saved_data = std::env::var_os(DATA_DIR_OVERRIDE_ENV);
        std::env::set_var(CONFIG_DIR_OVERRIDE_ENV, "/real/config/friring");
        std::env::set_var(DATA_DIR_OVERRIDE_ENV, "/real/data/friring");

        let cfg = config_file().unwrap();
        let db = database_file().unwrap();

        match saved_cfg {
            Some(v) => std::env::set_var(CONFIG_DIR_OVERRIDE_ENV, v),
            None => std::env::remove_var(CONFIG_DIR_OVERRIDE_ENV),
        }
        match saved_data {
            Some(v) => std::env::set_var(DATA_DIR_OVERRIDE_ENV, v),
            None => std::env::remove_var(DATA_DIR_OVERRIDE_ENV),
        }

        assert!(cfg.starts_with(test_sandbox_base()), "config: {cfg:?}");
        assert!(db.starts_with(test_sandbox_base()), "db: {db:?}");
        assert!(!cfg.starts_with("/real/config/friring"), "config: {cfg:?}");
        assert!(!db.starts_with("/real/data/friring"), "db: {db:?}");
    }

    #[test]
    fn override_isolates_paths() {
        let base = PathBuf::from("/test/base");
        set_test_dir(&base);

        assert_eq!(config_file(), Some(base.join("config.toml")));
        assert_eq!(log_directory(), Some(base.clone()));
        assert_eq!(database_file(), Some(base.join("friring.db")));

        reset_to_xdg();
    }

    #[test]
    fn guard_resets_on_drop() {
        let base = PathBuf::from("/test/base");
        {
            let _guard = TestPathGuard::new(&base);
            assert_eq!(config_file(), Some(base.join("config.toml")));
        }
        PATH_STRATEGY.with(|s| {
            assert_eq!(*s.borrow(), PathStrategy::Xdg);
        });
    }

    #[test]
    fn thread_local_isolation() {
        let base1 = PathBuf::from("/test/base1");
        set_test_dir(&base1);

        assert_eq!(config_file(), Some(base1.join("config.toml")));

        // A fresh thread starts with the Xdg default, unaffected by this one.
        let handle =
            std::thread::spawn(|| PATH_STRATEGY.with(|s| matches!(*s.borrow(), PathStrategy::Xdg)));

        assert!(handle.join().unwrap());

        assert_eq!(config_file(), Some(base1.join("config.toml")));

        reset_to_xdg();
    }

    #[test]
    fn all_path_kinds_resolve_in_override() {
        let base = PathBuf::from("/test/override");
        set_test_dir(&base);

        assert_eq!(resolve(PathKind::Config), Some(base.join("config.toml")));
        assert_eq!(resolve(PathKind::LogDir), Some(base.clone()));
        assert_eq!(resolve(PathKind::Database), Some(base.join("friring.db")));
        assert_eq!(resolve(PathKind::MetricsDir), Some(base.join("metrics")));
        assert_eq!(
            resolve(PathKind::WorktreesDir),
            Some(base.join("worktrees"))
        );
        assert_eq!(
            resolve(PathKind::KeybindingsFile),
            Some(base.join("keybindings.json"))
        );

        reset_to_xdg();
    }

    #[test]
    fn config_file_convenience() {
        let base = PathBuf::from("/custom");
        set_test_dir(&base);

        let path = config_file().unwrap();
        assert!(path.ends_with("config.toml"));

        reset_to_xdg();
    }

    #[test]
    fn log_directory_convenience() {
        let base = PathBuf::from("/custom");
        set_test_dir(&base);

        let path = log_directory().unwrap();
        assert_eq!(path, base);

        reset_to_xdg();
    }

    #[test]
    fn database_file_convenience() {
        let base = PathBuf::from("/custom");
        set_test_dir(&base);

        let path = database_file().unwrap();
        assert!(path.ends_with("friring.db"));

        reset_to_xdg();
    }

    #[test]
    fn set_test_dir_explicit() {
        reset_to_xdg();

        let base = PathBuf::from("/test/explicit");
        set_test_dir(&base);

        assert_eq!(config_file(), Some(base.join("config.toml")));

        reset_to_xdg();

        PATH_STRATEGY.with(|s| {
            assert_eq!(*s.borrow(), PathStrategy::Xdg);
        });
    }

    #[test]
    fn override_persists_across_calls() {
        let base = PathBuf::from("/persistent");
        set_test_dir(&base);

        for _ in 0..3 {
            assert_eq!(config_file(), Some(base.join("config.toml")));
        }

        reset_to_xdg();
    }

    #[test]
    fn multiple_guards_reset_correctly() {
        let base1 = PathBuf::from("/base1");
        let base2 = PathBuf::from("/base2");

        {
            let _guard1 = TestPathGuard::new(&base1);
            assert_eq!(config_file(), Some(base1.join("config.toml")));

            {
                let _guard2 = TestPathGuard::new(&base2);
                assert_eq!(config_file(), Some(base2.join("config.toml")));
            }

            PATH_STRATEGY.with(|s| matches!(*s.borrow(), PathStrategy::Xdg));
        }

        PATH_STRATEGY.with(|s| {
            assert_eq!(*s.borrow(), PathStrategy::Xdg);
        });
    }

    #[test]
    fn resolve_override_all_kinds() {
        let base = Path::new("/data");

        assert_eq!(
            resolve_override(base, PathKind::Config),
            PathBuf::from("/data/config.toml")
        );
        assert_eq!(
            resolve_override(base, PathKind::LogDir),
            PathBuf::from("/data")
        );
        assert_eq!(
            resolve_override(base, PathKind::Database),
            PathBuf::from("/data/friring.db")
        );
        assert_eq!(
            resolve_override(base, PathKind::MetricsDir),
            PathBuf::from("/data/metrics")
        );
        assert_eq!(
            resolve_override(base, PathKind::WorktreesDir),
            PathBuf::from("/data/worktrees")
        );
    }

    /// A fabricated data directory for the signal-channel tests. Nothing here
    /// touches a real home: every path resolves under the temp dir the guard
    /// pins, and the "agent" is this test writing a file. The base is returned
    /// too, for the one test that needs a second thread to resolve the same
    /// paths (the override is thread-local).
    fn signal_sandbox(name: &str) -> (tempfile::TempDir, PathBuf, TestPathGuard) {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = tmp.path().join(name);
        let guard = TestPathGuard::new(&base);
        (tmp, base, guard)
    }

    #[test]
    fn a_session_signal_directory_is_private_and_starts_empty() {
        let (_tmp, _base, _guard) = signal_sandbox("mint");
        let channel = create_session_signal_dir("s1").unwrap();
        let dir = channel.dir.clone();
        assert_eq!(dir, signals_directory().unwrap().join("s1"));
        // The path handed to the agent is the one the poll takes from.
        assert_eq!(channel.file, dir.join(SIGNAL_FILE_NAME));
        assert_eq!(session_signal_file("s1"), Some(channel.file));
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for probe in [signals_directory().unwrap(), dir.clone()] {
                let mode = std::fs::metadata(&probe).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "{} must be private", probe.display());
            }
        }

        // A status left behind by the previous run is dropped, not replayed:
        // the directory is adopted (the agent may already hold its path) but
        // the channel starts empty.
        std::fs::write(dir.join(SIGNAL_FILE_NAME), "done\n").unwrap();
        let again = create_session_signal_dir("s1").unwrap();
        assert_eq!(again.dir, dir);
        assert!(!dir.join(SIGNAL_FILE_NAME).exists());
        assert_eq!(take_session_signal("s1"), None);
    }

    #[test]
    fn a_well_formed_status_file_is_taken_exactly_once() {
        let (_tmp, _base, _guard) = signal_sandbox("take");
        let dir = create_session_signal_dir("s1").unwrap().dir;
        std::fs::write(dir.join(SIGNAL_FILE_NAME), "working\ndone\n").unwrap();

        assert_eq!(
            take_session_signal("s1").as_deref(),
            Some("working\ndone\n")
        );
        // Taken, not peeked: the same write is never delivered twice, so a file
        // a crashed run left behind cannot be re-reported forever.
        assert_eq!(take_session_signal("s1"), None);
        assert!(!dir.join(SIGNAL_FILE_NAME).exists());
        // And nothing is left staged in the root beside the session's dir.
        let leftovers: Vec<_> = std::fs::read_dir(signals_directory().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers, ["s1"]);
    }

    /// The file types an agent can leave in a directory it may write, none of
    /// which friring will read. A symlink would redirect the read outside the
    /// boundary; a FIFO would block the render loop that polls it until a
    /// writer appeared, which is a frozen TUI on demand.
    #[cfg(unix)]
    #[test]
    fn a_status_file_that_is_not_a_regular_file_is_dropped_unread() {
        let (_tmp, _base, _guard) = signal_sandbox("kinds");
        let dir = create_session_signal_dir("s1").unwrap().dir;
        let status = dir.join(SIGNAL_FILE_NAME);
        let outside = signals_directory().unwrap().join("host-secret");
        std::fs::write(&outside, "done\n").unwrap();

        std::os::unix::fs::symlink(&outside, &status).unwrap();
        assert_eq!(take_session_signal("s1"), None, "followed a symlink");
        // The file it pointed at is intact: taking never touches the target.
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "done\n");
        assert!(!status.exists(), "the symlink itself must be consumed");

        std::fs::create_dir(&status).unwrap();
        std::fs::write(status.join("done"), "done\n").unwrap();
        assert_eq!(take_session_signal("s1"), None, "read a directory");
        assert!(!status.exists());

        // A working file after all that: the channel is not wedged by any of it.
        std::fs::write(&status, "done\n").unwrap();
        assert_eq!(take_session_signal("s1").as_deref(), Some("done\n"));
    }

    /// A FIFO where the status file goes is the hostile case with teeth:
    /// `open(2)` on one blocks until a writer appears, and the poll that reads
    /// this runs on the render loop — so an agent could freeze the whole TUI
    /// with one `mkfifo`. The take must therefore decide on the *kind* of thing
    /// it moved, in a directory the agent cannot reach, before opening it.
    ///
    /// Run on a second thread with a deadline, so a regression fails the test
    /// instead of hanging it. The thread installs the same path override
    /// because it is thread-local.
    #[cfg(unix)]
    #[test]
    fn a_fifo_where_the_status_file_goes_cannot_block_the_poll() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (_tmp, base, _guard) = signal_sandbox("fifo");
        let dir = create_session_signal_dir("s1").unwrap().dir;
        let status = dir.join(SIGNAL_FILE_NAME);
        let made = std::process::Command::new("mkfifo")
            .arg(&status)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            // No `mkfifo` on this machine; the rest of the suite still covers
            // every kind of file that can be created without one.
            return;
        }

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _guard = TestPathGuard::new(base);
            let _ = tx.send(take_session_signal("s1"));
        });
        let taken = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("taking a FIFO blocked the poll");
        assert_eq!(taken, None);

        // And the channel is not wedged by it.
        std::fs::write(&status, "done\n").unwrap();
        assert_eq!(take_session_signal("s1").as_deref(), Some("done\n"));
    }

    #[test]
    fn an_oversized_or_non_utf8_status_file_is_dropped() {
        let (_tmp, _base, _guard) = signal_sandbox("caps");
        let dir = create_session_signal_dir("s1").unwrap().dir;
        let status = dir.join(SIGNAL_FILE_NAME);

        let huge = format!("{}done\n", "working\n".repeat(2000));
        assert!(huge.len() as u64 > MAX_SIGNAL_BYTES);
        std::fs::write(&status, &huge).unwrap();
        assert_eq!(take_session_signal("s1"), None, "read past the cap");

        std::fs::write(&status, b"done\n\xff\xfe").unwrap();
        assert_eq!(take_session_signal("s1"), None, "accepted invalid UTF-8");

        // Exactly at the cap still reads: the bound is a bound, not a margin.
        let tail = "\ndone\n";
        let mut at_cap = "x".repeat(MAX_SIGNAL_BYTES as usize - tail.len());
        at_cap.push_str(tail);
        assert_eq!(at_cap.len() as u64, MAX_SIGNAL_BYTES);
        std::fs::write(&status, &at_cap).unwrap();
        assert_eq!(take_session_signal("s1").as_deref(), Some(at_cap.as_str()));
    }

    /// The directory is minted for the launch and dropped with the session; a
    /// symlink planted where it belongs fails the mint rather than being
    /// written through.
    #[cfg(unix)]
    #[test]
    fn the_signal_directory_refuses_a_symlink_and_is_removed_on_cleanup() {
        let (tmp, _base, _guard) = signal_sandbox("lifecycle");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        create_private_dir(&signals_directory().unwrap()).unwrap();
        let dir = session_signal_dir("s1").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();

        let err = create_session_signal_dir("s1").unwrap_err();
        assert!(err.contains("is a symlink"), "{err}");
        assert!(elsewhere.exists(), "the link target must be untouched");

        // Cleanup takes the link itself, and then a real directory.
        remove_session_signal_dir("s1");
        assert!(!dir.exists());
        assert!(elsewhere.exists());

        let dir = create_session_signal_dir("s1").unwrap().dir;
        std::fs::write(dir.join(SIGNAL_FILE_NAME), "done\n").unwrap();
        remove_session_signal_dir("s1");
        assert!(!dir.exists());
        // Another session's channel is untouched by that.
        let other = create_session_signal_dir("s2").unwrap();
        assert!(other.dir.is_dir());
    }

    #[test]
    fn a_session_key_cannot_become_a_path() {
        let (_tmp, _base, _guard) = signal_sandbox("keys");
        let root = signals_directory().unwrap();
        for key in ["../escape", "..", ".", "a/b", "a\\b", "", "  "] {
            let dir = session_signal_dir(key).unwrap();
            assert_eq!(dir.parent(), Some(root.as_path()), "{key} escaped the root");
            assert!(
                dir.strip_prefix(&root)
                    .is_ok_and(|rest| rest.components().count() == 1),
                "{key} is not one segment"
            );
        }
        assert_eq!(sanitize_signal_key("../../etc"), "..-..-etc");
        assert_eq!(sanitize_signal_key(".."), "session");
        assert_eq!(sanitize_signal_key(""), "session");
    }

    #[test]
    fn metrics_directory_convenience() {
        let base = PathBuf::from("/custom");
        set_test_dir(&base);

        let path = metrics_directory().unwrap();
        assert!(path.ends_with("metrics"));

        reset_to_xdg();
    }

    #[test]
    fn worktrees_directory_convenience() {
        let base = PathBuf::from("/custom");
        set_test_dir(&base);

        let path = worktrees_directory().unwrap();
        assert!(path.ends_with("worktrees"));

        reset_to_xdg();
    }

    #[test]
    fn longest_common_prefix_empty() {
        assert_eq!(longest_common_prefix(&[]), "");
    }

    #[test]
    fn longest_common_prefix_single() {
        assert_eq!(longest_common_prefix(&["hello".to_string()]), "hello");
    }

    #[test]
    fn longest_common_prefix_multiple() {
        assert_eq!(
            longest_common_prefix(&[
                "foobar".to_string(),
                "foobaz".to_string(),
                "fooqux".to_string(),
            ]),
            "foo"
        );
    }

    #[test]
    fn longest_common_prefix_identical() {
        assert_eq!(
            longest_common_prefix(&["abc".to_string(), "abc".to_string()]),
            "abc"
        );
    }

    #[test]
    fn longest_common_prefix_no_common() {
        assert_eq!(
            longest_common_prefix(&["abc".to_string(), "xyz".to_string()]),
            ""
        );
    }

    #[test]
    fn longest_common_prefix_floors_multibyte_divergence() {
        // é (C3 A9) and ê (C3 AA) share their first byte — the byte-wise LCP
        // lands mid-char and must be floored, not panic on the slice.
        assert_eq!(
            longest_common_prefix(&["répo".to_string(), "rêpo".to_string()]),
            "r"
        );
    }

    #[test]
    fn display_path_tilde_shortens_home() {
        if let Some(home) = home_dir() {
            assert_eq!(display_path_tilde(&home.join("code")), "~/code");
            assert_eq!(display_path_tilde(&home), "~");
        }
        assert_eq!(display_path_tilde(Path::new("/opt/x")), "/opt/x");
    }

    #[test]
    fn split_path_input_empty_is_none() {
        assert_eq!(split_path_input(""), None);
    }

    #[test]
    fn split_path_input_trailing_sep_lists_that_dir() {
        let (parent, prefix) = split_path_input("/tmp/").unwrap();
        assert_eq!(parent, PathBuf::from("/tmp"));
        assert_eq!(prefix, "");
    }

    #[test]
    fn split_path_input_splits_parent_and_typed_prefix() {
        let (parent, prefix) = split_path_input("/tmp/fo").unwrap();
        assert_eq!(parent, PathBuf::from("/tmp"));
        assert_eq!(prefix, "fo");
    }

    #[test]
    fn split_path_input_expands_tilde() {
        if let Some(home) = home_dir() {
            let (parent, prefix) = split_path_input("~/co").unwrap();
            assert_eq!(parent, home);
            assert_eq!(prefix, "co");
        }
    }

    #[test]
    fn split_path_input_bare_tilde_lists_home_contents() {
        if let Some(home) = home_dir() {
            // A bare `~` lists home itself (empty prefix), not home's siblings.
            let (parent, prefix) = split_path_input("~").unwrap();
            assert_eq!(parent, home);
            assert_eq!(prefix, "");
        }
    }

    #[test]
    fn complete_directory_path_empty_input() {
        assert_eq!(complete_directory_path(""), None);
    }

    #[test]
    fn complete_directory_path_nonexistent() {
        assert_eq!(complete_directory_path("/nonexistent_dir_xyz_123"), None);
    }

    #[test]
    fn complete_directory_path_with_real_dir() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(temp.path().join("uniquedir")).unwrap();
        let input = format!("{}/uniqued", temp.path().display());
        assert_eq!(complete_directory_path(&input), Some("ir/".to_string()));
    }

    #[test]
    fn complete_directory_path_trailing_slash() {
        // A real directory with a trailing slash lists children — must not panic.
        let temp = tempfile::TempDir::new().unwrap();
        let result = complete_directory_path(&format!("{}/", temp.path().display()));
        assert!(result.is_some() || result.is_none());
    }

    #[test]
    fn complete_directory_path_exact_match() {
        let temp = tempfile::TempDir::new().unwrap();
        let inner = temp.path().join("exact");
        std::fs::create_dir(&inner).unwrap();
        let result = complete_directory_path(&inner.display().to_string());
        assert_eq!(result, Some("/".to_string()));
    }

    #[test]
    fn complete_directory_path_root() {
        // "/" is a valid directory — shouldn't panic.
        let result = complete_directory_path("/");
        assert!(result.is_some() || result.is_none());
    }

    #[test]
    fn complete_directory_path_with_tempdir() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();

        std::fs::create_dir(base.join("project_alpha")).unwrap();
        std::fs::create_dir(base.join("project_beta")).unwrap();
        std::fs::create_dir(base.join("other")).unwrap();

        // Two matches, no completion beyond the typed prefix → None.
        let input = format!("{}/project_", base.display());
        let result = complete_directory_path(&input);
        assert_eq!(result, None);

        let input = format!("{}/project_a", base.display());
        let result = complete_directory_path(&input);
        assert_eq!(result, Some("lpha/".to_string()));

        let input = format!("{}/oth", base.display());
        let result = complete_directory_path(&input);
        assert_eq!(result, Some("er/".to_string()));
    }

    #[test]
    fn complete_directory_path_skips_hidden_by_default() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();

        std::fs::create_dir(base.join(".hidden")).unwrap();
        std::fs::create_dir(base.join("visible")).unwrap();

        let input = format!("{}/", base.display());
        let result = complete_directory_path(&input);
        assert_eq!(result, Some("visible/".to_string()));
    }

    #[test]
    fn complete_directory_path_shows_hidden_with_dot_prefix() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();

        std::fs::create_dir(base.join(".hidden")).unwrap();
        std::fs::create_dir(base.join("visible")).unwrap();

        let input = format!("{}/.hid", base.display());
        let result = complete_directory_path(&input);
        assert_eq!(result, Some("den/".to_string()));
    }

    #[test]
    fn complete_directory_path_ignores_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();

        std::fs::write(base.join("readme.md"), "content").unwrap();
        std::fs::create_dir(base.join("src")).unwrap();

        let input = format!("{}/rea", base.display());
        let result = complete_directory_path(&input);
        assert_eq!(result, None);

        let input = format!("{}/sr", base.display());
        let result = complete_directory_path(&input);
        assert_eq!(result, Some("c/".to_string()));
    }

    #[test]
    fn longest_common_prefix_different_lengths() {
        assert_eq!(
            longest_common_prefix(&["ab".to_string(), "abcdef".to_string()]),
            "ab"
        );
    }

    #[test]
    fn expand_tilde_home() {
        let home = std::env::var(HOME_VAR).unwrap();
        assert_eq!(expand_tilde("~/foo"), PathBuf::from(&home).join("foo"));
    }

    #[test]
    fn expand_tilde_bare() {
        let home = std::env::var(HOME_VAR).unwrap();
        assert_eq!(expand_tilde("~"), PathBuf::from(&home));
    }

    #[test]
    fn expand_tilde_absolute() {
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    fn expand_tilde_relative() {
        assert_eq!(expand_tilde("rel/path"), PathBuf::from("rel/path"));
    }

    #[test]
    fn expand_tilde_no_home() {
        // Temporarily unset the home var — use a thread to avoid interfering with
        // other tests.
        let result = std::thread::spawn(|| {
            let orig = std::env::var_os(HOME_VAR);
            std::env::remove_var(HOME_VAR);
            let p = expand_tilde("~/foo");
            if let Some(home) = orig {
                std::env::set_var(HOME_VAR, home);
            }
            p
        })
        .join()
        .unwrap();
        assert_eq!(result, PathBuf::from("~/foo"));
    }

    #[test]
    fn expand_tilde_nested_path() {
        let home = std::env::var(HOME_VAR).unwrap();
        assert_eq!(
            expand_tilde("~/a/b/c"),
            PathBuf::from(&home).join("a").join("b").join("c")
        );
    }

    #[test]
    fn expand_tilde_empty() {
        assert_eq!(expand_tilde(""), PathBuf::from(""));
    }

    #[test]
    fn expand_tilde_other_user() {
        // ~otheruser is NOT expanded — only ~ and ~/ are handled.
        assert_eq!(expand_tilde("~otheruser"), PathBuf::from("~otheruser"));
    }

    #[cfg(windows)]
    #[test]
    fn expand_tilde_backslash_on_windows() {
        // On Windows `~\foo` uses the native separator and must expand too.
        let home = std::env::var(HOME_VAR).unwrap();
        assert_eq!(expand_tilde("~\\foo"), PathBuf::from(&home).join("foo"));
        assert_eq!(
            expand_tilde("~\\a\\b"),
            PathBuf::from(&home).join("a").join("b")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn expand_tilde_backslash_literal_on_unix() {
        // On Unix `\` is a legal filename character, not a separator, so
        // `~\foo` is left untouched.
        assert_eq!(expand_tilde("~\\foo"), PathBuf::from("~\\foo"));
    }

    #[test]
    fn complete_directory_path_tilde() {
        let temp = tempfile::TempDir::new().unwrap();
        let base = temp.path();
        std::fs::create_dir(base.join("mydir")).unwrap();

        let input = format!("{}/my", base.display());
        let result = complete_directory_path(&input);
        assert_eq!(result, Some("dir/".to_string()));
    }

    #[test]
    fn complete_directory_path_tilde_trailing_slash() {
        // "~/" completions are relative suffixes, never absolute.
        if let Some(s) = complete_directory_path("~/") {
            assert!(!s.starts_with('/'));
        }
    }

    #[test]
    fn sanitize_workspace_segment_strips_separators_and_dots() {
        // Slashes/backslashes/colons and whitespace → `-`; leading/trailing
        // `.`/`-` trimmed.
        assert_eq!(sanitize_workspace_segment("a/b\\c:d"), "a-b-c-d");
        assert_eq!(sanitize_workspace_segment("  .git  "), "git");
        assert_eq!(sanitize_workspace_segment("my repo"), "my-repo");
        assert_eq!(sanitize_workspace_segment("--.hidden.--"), "hidden");
        // A session-id UUID (the real workspace-dir input) is unchanged.
        assert_eq!(
            sanitize_workspace_segment("d5715d35-9599-4507-9901-ef33b9476358"),
            "d5715d35-9599-4507-9901-ef33b9476358"
        );
    }

    #[test]
    fn claude_project_slug_dashes_every_non_alphanumeric() {
        // Leading `/`, separators, dots, and existing dashes all become `-`
        // (so a dashed dir yields a double dash) — observed CC v2.1.206 rule.
        assert_eq!(
            claude_project_slug(Path::new("/mnt/shared/projects/friring")),
            "-mnt-shared-projects-friring"
        );
        assert_eq!(
            claude_project_slug(Path::new("/home/me/.claude/worktrees/x-y")),
            "-home-me--claude-worktrees-x-y"
        );
        // Non-ASCII is per-byte: `é` (2 UTF-8 bytes) → `--`, matching CC.
        assert_eq!(claude_project_slug(Path::new("/tmp/café")), "-tmp-caf--");
    }

    #[test]
    fn unique_link_name_dedups_with_dash_two_suffix() {
        // First collision is `-2`, then `-3`; an empty label falls back to
        // `repo`.
        let mut used = std::collections::HashSet::new();
        assert_eq!(unique_link_name("webapp", &mut used), "webapp");
        assert_eq!(unique_link_name("webapp", &mut used), "webapp-2");
        assert_eq!(unique_link_name("webapp", &mut used), "webapp-3");
        assert_eq!(unique_link_name("", &mut used), "repo");
        assert_eq!(unique_link_name("", &mut used), "repo-2");
    }
}
