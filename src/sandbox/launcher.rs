//! The two-line shell that starts the egress relay beside the agent, *inside*
//! the boundary.
//!
//! Shared by every backend whose sandbox has its own network namespace: a
//! `--unshare-net` bubblewrap sandbox and a `--network none` container reach the
//! friring proxy the same way (ADR-27), through a unix socket carried across the
//! boundary by a mount and a relay that offers a TCP endpoint on the sandbox's
//! *own* loopback. Only the mount differs, so the launcher lives here rather
//! than once per backend — two copies of an argument-counting shell script is
//! exactly the kind of drift that ships a sandbox executing a socket path.

/// The shell that starts the relay before becoming the agent.
///
/// An absolute path, and one every sandbox already has: `/bin` is in
/// bubblewrap's system binds, the host read scope binds the whole root, and a
/// place image without `/bin/sh` could not run tmux either.
pub const SHELL: &str = "/bin/sh";

/// `$0` for that shell, so a `ps` inside the sandbox says what the process is.
pub const RELAY_LAUNCHER_NAME: &str = "friring-sandbox-launcher";

/// Start the relay, then become the agent.
///
/// Every value arrives as a positional parameter, so nothing here is quoted or
/// re-parsed: `$1` is friring's own CLI, `$2` the address to offer inside the
/// namespace, `$3` the socket the proxy is listening on, and everything after
/// them is the agent's argv exactly as the launch composed it.
///
/// Both of the relay's streams go to `/dev/null`: the pane belongs to the
/// agent's TUI, and a line written across it corrupts the display. A relay that
/// fails to start surfaces as the agent's own connection error instead.
///
/// `exec` matters twice — the agent replaces the shell as pid 1 of the
/// sandbox's pid namespace, so the pane's process *is* the agent, and when it
/// exits the namespace dies and takes the relay with it.
pub const RELAY_LAUNCHER: &str = "\"$1\" sandbox relay --listen \"$2\" --socket \"$3\" \
                                  >/dev/null 2>&1 &\nshift 3\nexec \"$@\"\n";

/// The launcher and its three positional parameters, ready to have the agent's
/// own argv appended.
///
/// The order is fixed by [`RELAY_LAUNCHER`]'s `shift 3`, which is why the two
/// backends build it here instead of each spelling the sequence out.
pub fn relay_launcher_argv(relay: &str, listen: &str, socket: &str) -> Vec<String> {
    [
        SHELL,
        "-c",
        RELAY_LAUNCHER,
        RELAY_LAUNCHER_NAME,
        relay,
        listen,
        socket,
    ]
    .iter()
    .map(|token| (*token).to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_carries_three_positionals_before_the_agent() {
        let argv = relay_launcher_argv("/usr/local/bin/friring-cli", "127.0.0.1:8118", "/s/p.sock");
        assert_eq!(
            argv,
            [
                SHELL,
                "-c",
                RELAY_LAUNCHER,
                RELAY_LAUNCHER_NAME,
                "/usr/local/bin/friring-cli",
                "127.0.0.1:8118",
                "/s/p.sock",
            ]
        );
    }
}
