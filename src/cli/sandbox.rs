//! `friring-cli sandbox` — the part of the sandbox feature that runs *inside*
//! the boundary.
//!
//! Today that is one command. A sandbox in its own network namespace has no
//! route to the host's loopback, so it reaches the egress proxy through a
//! bind-mounted unix socket — and no mainstream HTTP or SOCKS client can dial
//! one, because `HTTP_PROXY` and `ALL_PROXY` take a host and a port. The relay
//! closes that gap by offering a TCP endpoint inside the namespace and
//! forwarding each connection to the socket (`docs/SANDBOX.md` §Reaching the
//! proxy).
//!
//! ```text
//! agent  →  127.0.0.1:PORT   (the sandbox's own loopback)
//!        →  friring-cli sandbox relay
//!        →  /…/proxy.sock    (bind-mounted from the host)
//!        →  friring proxy    →  policy  →  upstream
//! ```
//!
//! Two properties are the point of running it here rather than teaching the
//! proxy to do it: the relay **holds no credential and makes no policy
//! decision** — the proxy still demands its token at the far end, and the
//! allowlist is still applied outside the boundary — and it never parses a
//! byte, so `CONNECT` and SOCKS5 both cross unchanged.
//!
//! # Dispatched before the database
//!
//! This command is the one `friring-cli` subcommand that must **not** open the
//! database: it runs inside a sandbox, where ADR-29 keeps the database out on
//! purpose, so opening one would either create a stray database inside the
//! boundary or fail and leave the sandbox with no egress. `friring-cli`'s
//! `main` therefore matches it immediately after parsing, before the
//! settings/database block, and [`run`] returns nothing renderable because it
//! does not finish until the sandbox does.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Subcommand;

/// Sandbox-internal commands. Not for interactive use: friring composes these
/// itself, inside a boundary it built.
#[derive(Subcommand, Debug)]
pub enum Action {
    /// Forward a TCP port inside the sandbox to the egress proxy's unix socket.
    Relay {
        /// Address to listen on, inside the sandbox's own network namespace.
        /// Port `0` binds an ephemeral port and prints the result.
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: SocketAddr,
        /// The proxy's unix socket, at its path inside the sandbox.
        #[arg(long)]
        socket: PathBuf,
    },
}

/// Run a sandbox-internal command.
///
/// Long-running by nature: the relay serves until the sandbox is torn down.
/// Under bubblewrap that needs no signal handling of its own — the agent is pid
/// 1 of the sandbox's pid namespace and `--die-with-parent` collapses the
/// namespace when it goes — but `Ctrl+C` is honoured so the command is usable
/// on its own while debugging a profile.
///
/// # Errors
///
/// The runtime could not be built, or the address could not be bound.
pub fn run(action: &Action) -> Result<(), String> {
    match action {
        Action::Relay { listen, socket } => relay(*listen, socket),
    }
}

#[cfg(unix)]
fn relay(listen: SocketAddr, socket: &std::path::Path) -> Result<(), String> {
    // A runtime of its own, and the smallest one: this process exists to move
    // bytes between two sockets, and it is started by a sandbox launch that has
    // no runtime to inherit.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|e| format!("cannot start the relay runtime: {e}"))?;

    runtime.block_on(async move {
        let config = crate::proxy::RelayConfig::new(listen, socket);
        let mut relay = crate::proxy::Relay::start(config)
            .await
            .map_err(|e| format!("{e:#}"))?;
        // The bound address, so `--listen 127.0.0.1:0` is discoverable by
        // whoever asked for it. Inside a sandbox launch this goes to
        // `/dev/null`: the pane belongs to the agent's own display.
        println!("{}", relay.addr());
        tokio::select! {
            () = relay.wait() => {}
            result = tokio::signal::ctrl_c() => {
                if let Err(e) = result {
                    return Err(format!("cannot listen for Ctrl+C: {e}"));
                }
            }
        }
        Ok(())
    })
}

/// Unix sockets are what the relay exists to bridge to, so there is nothing to
/// bridge on a platform without them: a Windows host reaches a sandbox through
/// WSL or a container, and the relay runs *inside* that, where sockets exist.
#[cfg(not(unix))]
fn relay(_listen: SocketAddr, socket: &std::path::Path) -> Result<(), String> {
    Err(format!(
        "the sandbox relay needs unix sockets (asked for `{}`); on Windows a sandbox is reached \
         through WSL or a container, and the relay runs inside it",
        socket.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use clap::Parser as _;

    #[test]
    fn the_relay_defaults_to_an_ephemeral_loopback_port() {
        let cli = Cli::parse_from(["friring-cli", "sandbox", "relay", "--socket", "/s/p.sock"]);
        let Command::Sandbox {
            action: Action::Relay { listen, socket },
        } = cli.command
        else {
            panic!("expected the relay action");
        };
        // Loopback: inside a namespaced sandbox there is nothing else worth
        // binding, and a routable bind would offer the proxy to whatever else
        // shares the namespace.
        assert!(listen.ip().is_loopback());
        assert_eq!(listen.port(), 0);
        assert_eq!(socket, PathBuf::from("/s/p.sock"));
    }

    /// The socket is not optional: a relay with nowhere to forward to would
    /// accept the agent's connections and drop every one of them.
    #[test]
    fn the_relay_needs_a_socket_to_forward_to() {
        assert!(Cli::try_parse_from(["friring-cli", "sandbox", "relay"]).is_err());
    }
}
