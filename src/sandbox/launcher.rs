//! The argv that starts the egress relay beside the agent, *inside* the
//! boundary — and, for a bridge child, holds the agent at the door until the
//! host says the row exists (ADR-33).
//!
//! Shared by every backend whose sandbox has its own network namespace: a
//! `--unshare-net` bubblewrap sandbox and a `--network none` container reach the
//! friring proxy the same way (ADR-27), through a unix socket carried across the
//! boundary by a mount and a relay that offers a TCP endpoint on the sandbox's
//! *own* loopback. Only the mount differs, so the composition lives here rather
//! than once per backend.
//!
//! **No shell.** What runs in the pane is `friring-cli sandbox launch`, a
//! subcommand of friring's own CLI dispatched before the database opens. Every
//! value reaches it as its own argv element, so nothing is quoted, re-split or
//! re-parsed anywhere on the path from a profile to a running agent. The helper
//! itself is [`crate::cli::early`]; this module only composes
//! the command line, so the sandbox layer stays free of `crate::cli`.

/// A POSIX shell, for the place backends' image *probe* — `sh -c 'command -v
/// <program>'` inside a container is the only way to ask an image whether it
/// carries the agent, and the answer decides whether a session opens on a pane
/// that would die at once.
///
/// Not on any launch path: what starts an agent is [`launch_argv`], which never
/// composes a command string. An absolute path, and one every image already has
/// — an image without `/bin/sh` could not run tmux either.
pub const SHELL: &str = "/bin/sh";

/// The subcommand name, so the composition here and the dispatch in
/// `cli::early` cannot drift apart.
pub const LAUNCH_SUBCOMMAND: [&str; 2] = ["sandbox", "launch"];

/// How long a gated launch waits for its release file before giving up.
///
/// The backstop for a host that died between spawning the pane and committing
/// the child's row: the helper exits, the pane dies under `remain-on-exit`, and
/// recovery finds a window with no live agent rather than an agent running from
/// a half-built session. Generous, because the wait covers a worktree checkout
/// and a proxy bind on a loaded machine.
pub const GATE_TIMEOUT_SECS: u64 = 300;

/// What a launch asks the helper to do, beyond exec'ing the agent.
///
/// Both halves are optional and independent: a seatbelt launch needs no relay
/// (it reaches the proxy socket directly, `seatbelt.rs`), and an ordinary
/// session needs no gate. A launch with neither still goes through the helper,
/// which then only strips the environment and execs — one path, so the exec
/// semantics every backend relies on are the same for every launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchHelper<'a> {
    /// The gate to wait on, as `(directory inside the boundary, key)`.
    pub gate: Option<(&'a str, &'a str)>,
    /// The relay to start first: `(listen address, socket path inside)`.
    pub relay: Option<(&'a str, &'a str)>,
    /// Variables to remove from the environment before exec'ing the agent.
    pub unset: &'a [&'a str],
}

/// `<relay program> sandbox launch …` up to and including `--`, ready to have
/// the agent's own argv appended.
///
/// `program` is friring's own CLI, resolved the way the relay already is
/// ([`crate::sandbox::bwrap::local_relay_program`], or the place's own copy):
/// the binary that applies the gate must be *this* friring's, not a name an
/// inherited `PATH` resolves.
pub fn launch_argv(program: &str, helper: &LaunchHelper<'_>) -> Vec<String> {
    let mut argv: Vec<String> = vec![program.to_string()];
    argv.extend(LAUNCH_SUBCOMMAND.iter().map(|t| (*t).to_string()));
    if let Some((dir, key)) = helper.gate {
        argv.extend(
            ["--gate", dir, "--key", key, "--timeout"]
                .iter()
                .map(|t| (*t).to_string()),
        );
        argv.push(GATE_TIMEOUT_SECS.to_string());
    }
    if let Some((listen, socket)) = helper.relay {
        argv.extend(
            ["--relay-listen", listen, "--relay-socket", socket]
                .iter()
                .map(|t| (*t).to_string()),
        );
    }
    for var in helper.unset {
        argv.push("--unset".to_string());
        argv.push((*var).to_string());
    }
    argv.push("--".to_string());
    argv
}

/// The multiplexer variables every sandboxed launch drops.
///
/// Defence in depth, not the enforcement: the kernel policy denies the socket
/// itself ([`crate::sandbox::dirs::multiplexer_socket_denies`]). What this buys
/// is that a tool inside the boundary does not *find* an address for the server
/// outside it and report a confusing refusal instead of simply having no
/// multiplexer.
pub fn mux_env_to_unset() -> &'static [&'static str] {
    crate::session::MUX_NESTING_ENV
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_helper_is_the_subcommand_and_the_separator() {
        assert_eq!(
            launch_argv("/usr/local/bin/friring-cli", &LaunchHelper::default()),
            ["/usr/local/bin/friring-cli", "sandbox", "launch", "--"]
        );
    }

    #[test]
    fn the_relay_flags_carry_the_address_and_the_socket() {
        let argv = launch_argv(
            "/cli",
            &LaunchHelper {
                relay: Some(("127.0.0.1:8118", "/s/p.sock")),
                ..LaunchHelper::default()
            },
        );
        assert_eq!(
            argv,
            [
                "/cli",
                "sandbox",
                "launch",
                "--relay-listen",
                "127.0.0.1:8118",
                "--relay-socket",
                "/s/p.sock",
                "--",
            ]
        );
    }

    #[test]
    fn the_gate_flags_carry_the_directory_the_key_and_the_timeout() {
        let argv = launch_argv(
            "/cli",
            &LaunchHelper {
                gate: Some(("/gates/child", "abc123")),
                ..LaunchHelper::default()
            },
        );
        assert_eq!(
            argv,
            [
                "/cli",
                "sandbox",
                "launch",
                "--gate",
                "/gates/child",
                "--key",
                "abc123",
                "--timeout",
                &GATE_TIMEOUT_SECS.to_string(),
                "--",
            ]
        );
    }

    /// One element per variable: a helper that folded them into one comma-joined
    /// argument would be re-parsing a string, which is exactly what this
    /// composition exists to avoid.
    #[test]
    fn each_unset_variable_is_its_own_argv_element() {
        let argv = launch_argv(
            "/cli",
            &LaunchHelper {
                unset: &["TMUX", "TMUX_PANE"],
                ..LaunchHelper::default()
            },
        );
        assert_eq!(
            argv,
            [
                "/cli",
                "sandbox",
                "launch",
                "--unset",
                "TMUX",
                "--unset",
                "TMUX_PANE",
                "--"
            ]
        );
    }

    #[test]
    fn the_stripped_variables_are_the_multiplexer_nesting_set() {
        assert!(mux_env_to_unset().contains(&"TMUX"));
        assert!(mux_env_to_unset().contains(&"PSMUX_TARGET_SESSION"));
    }
}
