//! Agent arguments that name a **friring-managed config file**, and what to do
//! with them when the agent is not launched on this filesystem.
//!
//! An agent registry entry may point the agent at a file friring owns — claude's
//! `--settings <config dir>/hooks/claude.json` is the shipped example. That path
//! is real only where friring runs. Hand it to an agent somewhere else and the
//! agent does not degrade: it errors out and the pane dies instantly ("Settings
//! file not found"), which reads as "the session is broken", not as "that file
//! is not over there".
//!
//! Two callers need the same rewrite over the same token grammar, so it lives
//! here rather than in either of them:
//!
//! - a **remote** spawn translates the path onto the host and copies the file
//!   there, falling back to dropping it (`session_ops::spawn`);
//! - a **place**-backed launch drops it outright, because config projection into
//!   a sandbox is a later slice and ADR-28 forbids binding the host's agent
//!   configuration in ([`crate::agent::sandboxing`]).

/// True when `path` is `root` itself or a descendant — a plain `starts_with`
/// would also claim sibling directories sharing the prefix
/// (`…/friring-backup` under root `…/friring`).
pub(crate) fn path_under_root(path: &str, root: &str) -> bool {
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Every arg (or `--flag=value` value) under `config_root` is passed to `map`;
/// `Some(new)` substitutes the path, `None` drops the arg **and** its preceding
/// token when that token is a flag (so a `--settings <path>` pair vanishes
/// together).
///
/// Scope is deliberately narrow: only paths under the friring config directory
/// are touched, so an arbitrary path in the agent's own args — a repo path, a
/// user file — is never rewritten or dropped.
pub(crate) fn rewrite_config_path_args(
    args: Vec<String>,
    config_root: &str,
    mut map: impl FnMut(&str) -> Option<String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        if path_under_root(&arg, config_root) {
            match map(&arg) {
                Some(new) => out.push(new),
                None => {
                    // A `--flag=value` token is self-contained — never a
                    // dangling flag for the dropped path — so popping it would
                    // eat an unrelated (possibly already-rewritten) arg.
                    if out
                        .last()
                        .is_some_and(|prev| prev.starts_with('-') && !prev.contains('='))
                    {
                        out.pop();
                    }
                }
            }
            continue;
        }
        // `--flag=<path>` form: rewrite the value in place, or drop the whole
        // token (it is self-contained — nothing precedes it to pop).
        if let Some((flag, value)) = arg.split_once('=') {
            if flag.starts_with('-') && path_under_root(value, config_root) {
                if let Some(new) = map(value) {
                    out.push(format!("{flag}={new}"));
                }
                continue;
            }
        }
        out.push(arg);
    }
    out
}

/// `argv` with every friring-managed config path — and the flag introducing it —
/// removed, plus the paths that went.
///
/// The answer for a launch that has nowhere to put the file: a place sandbox
/// today, and any future "elsewhere" that cannot be written to. Returning what
/// was dropped rather than only logging it is what lets the caller say so in
/// front of the user; a session whose hooks silently vanished looks like a
/// session that never reports status, and there is nothing on screen connecting
/// the two.
///
/// Answers `(argv, [])` unchanged when friring cannot resolve its own config
/// directory — nothing can be recognised as managed, so nothing is dropped.
pub(crate) fn without_config_paths(argv: Vec<String>) -> (Vec<String>, Vec<String>) {
    let Some(config_root) = crate::paths::config_file()
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
    else {
        return (argv, Vec::new());
    };
    let mut dropped = Vec::new();
    let kept = rewrite_config_path_args(argv, &config_root, |path| {
        dropped.push(path.to_string());
        None
    });
    (kept, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_managed_path_takes_its_flag_with_it_and_leaves_everything_else() {
        let root = "/fabricated/config/friring";
        let args = vec![
            "--resume".to_string(),
            "abc".to_string(),
            "--settings".to_string(),
            format!("{root}/hooks/claude.json"),
            "--add-dir".to_string(),
            "/repo".to_string(),
        ];
        let out = rewrite_config_path_args(args, root, |_| None);
        assert_eq!(out, ["--resume", "abc", "--add-dir", "/repo"]);
    }

    #[test]
    fn a_flag_equals_value_token_is_dropped_whole() {
        let root = "/fabricated/config/friring";
        let args = vec![
            "--resume".to_string(),
            format!("--settings={root}/hooks/claude.json"),
            "--verbose".to_string(),
        ];
        let out = rewrite_config_path_args(args, root, |_| None);
        assert_eq!(out, ["--resume", "--verbose"]);
    }

    /// A sibling directory sharing the prefix is a different directory.
    #[test]
    fn a_prefix_neighbour_is_not_under_the_root() {
        assert!(path_under_root("/a/friring", "/a/friring"));
        assert!(path_under_root("/a/friring/hooks/x.json", "/a/friring"));
        assert!(!path_under_root("/a/friring-backup/x.json", "/a/friring"));
    }
}
