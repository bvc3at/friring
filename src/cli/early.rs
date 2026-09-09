//! The commands `friring-cli` serves **before** it opens the database.
//!
//! ADR-29 keeps friring's SQLite file out of every sandbox: automations stored
//! in it are shell commands the *host* runs, so a database handle inside a
//! boundary is host command execution. Three commands run in there, and this is
//! the one place that says so:
//!
//! - `sandbox relay` — the TCP-to-unix-socket pipe a namespaced sandbox reaches
//!   the egress proxy through (ADR-27);
//! - `sandbox launch` — the gated, shell-free process that becomes the agent
//!   (ADR-33);
//! - `bridge …` — a sandboxed agent's own request queue (ADR-30), added in a
//!   later stage.
//!
//! Opening a database from any of them would either create a stray one inside
//! the boundary or fail and leave the sandbox with no egress at all. So `main`
//! asks [`run_before_database`] first, and every host-side command answers
//! `None` and takes the ordinary path with a database already open.
//!
//! Nothing here reads a configuration file, a profile or a registry either. The
//! whole input is argv, which the host composed.
//!
//! # `config paths` is here for a different reason
//!
//! It is not an in-boundary command; it runs on the host. It is dispatched here
//! because *when* it answers is the whole point: it reports which config dir,
//! data dir and database file this process resolved, and a report printed after
//! the open would be describing a file the process had already created or
//! migrated. A harness that wants to prove its isolation before letting a binary
//! touch storage needs the answer strictly earlier than the open, so this is the
//! only place it can be served from.
//!
//! Resolving a path is not opening one, and the module's source check below
//! still holds: nothing here names the storage layer — a rule that covers the
//! prose as well as the code, so the check needs no exception for a comment.

use std::io::Read as _;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::cli::sandbox::Action;
use crate::cli::{Cli, Command};

/// Exit status of a launch whose gate never opened.
///
/// `75` is `EX_TEMPFAIL`: the host did not get to this launch in time, which is
/// a condition a retry could clear, rather than a broken command line. The pane
/// dies with it under `remain-on-exit` and recovery finds a window with no live
/// agent — never an agent running from a session row that was never committed.
pub const GATE_TIMEOUT_EXIT: i32 = 75;

/// How often the gate is polled.
const GATE_POLL: Duration = Duration::from_millis(100);

/// The largest release file that will be read at all.
///
/// A gate key is a short opaque token; anything larger is not one, and reading
/// it would be reading whatever else ended up at that path.
const MAX_GATE_KEY_BYTES: usize = 4096;

/// Run a command that must not open the database, or answer `None`.
///
/// Called by `friring-cli`'s `main` immediately after parsing and before the
/// settings/database block. `None` means "this is host-side management, take the
/// normal path".
///
/// Takes the whole [`Cli`] rather than its command because `config paths`
/// renders in the same formats every other command does, and the format flags
/// are global.
pub fn run_before_database(cli: &Cli) -> Option<Result<(), String>> {
    match &cli.command {
        // Prints, then returns: the only early command that produces output the
        // ordinary dispatcher would otherwise have rendered.
        Command::Config {
            action: crate::cli::config::Action::Paths,
        } => {
            let format = crate::cli::output::Format::resolve(cli.json, cli.text, cli.pretty);
            println!("{}", format.render(&crate::cli::config::paths_output()));
            Some(Ok(()))
        }
        Command::Sandbox {
            action: Action::Relay { listen, socket },
        } => Some(crate::cli::sandbox::relay(*listen, socket)),
        Command::Sandbox {
            action:
                Action::Launch {
                    gate,
                    key,
                    timeout,
                    relay_listen,
                    relay_socket,
                    unset,
                    agent,
                },
        } => Some(sandbox_launch(&LaunchRequest {
            gate: gate.as_deref().zip(key.as_deref()),
            timeout: Duration::from_secs(*timeout),
            relay: relay_listen
                .as_ref()
                .zip(relay_socket.as_deref())
                .map(|(listen, socket)| (listen.to_string(), socket)),
            unset,
            agent,
        })),
        // Every bridge verb runs inside a boundary. The client writes a file,
        // waits for a file and prints it; every decision is made by the running
        // friring on the other side of the queue, where the authority is.
        // Cloned because this takes the command by reference — `main` needs it
        // afterwards for the ordinary path — and the client consumes its action
        // to move the request body out.
        Command::Bridge { action } => Some(crate::cli::bridge::run(action.clone())),
        _ => None,
    }
}

/// Whether `command` is served before the database opens.
///
/// Split out so `cli::run` can refuse the same set with an explanation rather
/// than serving one with a database in hand, and so a test can assert the
/// routing without running a command that never returns.
pub fn is_database_free(command: &Command) -> bool {
    matches!(
        command,
        Command::Sandbox {
            action: Action::Relay { .. } | Action::Launch { .. },
        } | Command::Bridge { .. }
            | Command::Config {
                action: crate::cli::config::Action::Paths,
            }
    )
}

/// One gated launch, as [`sandbox_launch`] wants it.
struct LaunchRequest<'a> {
    /// The gate directory and the key its release file must hold.
    gate: Option<(&'a Path, &'a str)>,
    /// How long to wait for that file.
    timeout: Duration,
    /// The egress relay to start first: `(listen address, socket path)`.
    relay: Option<(String, &'a Path)>,
    /// Variables to remove before exec'ing the agent.
    unset: &'a [String],
    /// The agent's own argv.
    agent: &'a [String],
}

/// Start the relay, drop the named variables, wait for the gate, become the
/// agent.
///
/// # The order matters
///
/// The relay starts **first** so a gated child's agent finds its egress already
/// listening the instant it execs. It is a direct child of this process and is
/// never double-forked, re-parented or `setsid`'d, which is what makes the
/// lifetime invariant hold: under bwrap this process is pid 1 of the
/// `--unshare-pid` namespace, the agent replaces it *as pid 1*, and when the
/// agent exits the namespace teardown takes the relay with it. Every exit of
/// this process — a gate timeout, a failed `execvp`, a relay that would not
/// start — is an exit of pid 1 and tears the relay down the same way. Outside a
/// pid namespace there is no such teardown, so on Linux the relay child also
/// asks the kernel for `SIGTERM` when its parent dies.
///
/// # Errors
///
/// The relay could not be started, the environment could not be cleaned, the
/// gate never opened (which exits [`GATE_TIMEOUT_EXIT`] rather than returning),
/// or the agent could not be exec'd.
fn sandbox_launch(request: &LaunchRequest<'_>) -> Result<(), String> {
    if let Some((listen, socket)) = &request.relay {
        start_relay(listen, socket)?;
    }
    for var in request.unset {
        std::env::remove_var(var);
    }
    if let Some((dir, key)) = request.gate {
        if !wait_for_gate(dir, key, request.timeout) {
            eprintln!(
                "friring: this launch's gate at '{}' did not open within {}s; the session it \
                 belongs to was never committed",
                dir.display(),
                request.timeout.as_secs()
            );
            std::process::exit(GATE_TIMEOUT_EXIT);
        }
    }
    exec_agent(request.agent)
}

/// Start `friring-cli sandbox relay` as a direct child, both streams on
/// `/dev/null`.
///
/// The pane belongs to the agent's TUI, and a line written across it corrupts
/// the display — a relay that fails to start surfaces as the agent's own
/// connection error instead. The program is this very binary, by its own
/// `current_exe`: resolving a name through `PATH` inside a boundary would let
/// the environment choose what runs.
fn start_relay(listen: &str, socket: &Path) -> Result<(), String> {
    let program = std::env::current_exe()
        .map_err(|e| format!("cannot locate friring-cli to start the egress relay: {e}"))?;
    let mut command = std::process::Command::new(program);
    command
        .args(["sandbox", "relay", "--listen", listen, "--socket"])
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    parent_death_signal(&mut command);
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot start the egress relay: {e}"))
}

/// Ask the kernel to `SIGTERM` the relay when this process dies.
///
/// A backstop for the one case the pid namespace does not cover: a seatbelt
/// launch, or any launch outside `--unshare-pid`, where nothing tears the
/// namespace down because there is no namespace. Linux only —
/// `PR_SET_PDEATHSIG` has no portable equivalent, and macOS launches never start
/// a relay at all (a seatbelt sandbox reaches the proxy socket directly).
#[cfg(target_os = "linux")]
fn parent_death_signal(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: `prctl` is async-signal-safe and touches no memory this process
    // owns. It runs between `fork` and `exec` in the child, where only such
    // calls are legal.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn parent_death_signal(_command: &mut std::process::Command) {}

/// Poll `<dir>/release` until it holds `key`, or until `timeout` runs out.
///
/// The release primitive is deliberately the existence of a **regular file** in
/// a directory the policy grants read-only:
///
/// - Seatbelt's `(deny default)` plus a `file-read*` allow means `open` for
///   writing, `rename`, `link` and `unlink` in that directory are all denied.
/// - A bubblewrap read-only bind returns `EROFS` for creating, renaming or
///   unlinking a regular file or a directory.
///
/// A FIFO or a socket would not do: a read-only bind stops neither `connect(2)`
/// nor a FIFO opened for writing, so a process inside the boundary could
/// release its own gate. That is why anything but a regular file at the path is
/// ignored rather than read, and why the open never follows a symlink.
fn wait_for_gate(dir: &Path, key: &str, timeout: Duration) -> bool {
    let path = dir.join(crate::sandbox::dirs::GATE_RELEASE_NAME);
    let deadline = Instant::now() + timeout;
    loop {
        if read_release(&path).is_some_and(|held| held == key) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(GATE_POLL);
    }
}

/// The release file's content, or `None` when there is nothing there this will
/// accept.
///
/// Three flags, each load-bearing:
///
/// - `O_NOFOLLOW` refuses a symlink at the final component, so a link planted at
///   the release path cannot point the read at a file the agent *can* write.
/// - `O_NONBLOCK` is what makes rejecting a FIFO possible at all: opening one
///   for reading blocks until a writer appears, so without it a FIFO at the
///   release path would hang this process forever rather than being ignored.
///   Harmless on a regular file.
/// - The regular-file check is made on the open **descriptor** rather than on
///   the path, so nothing can be swapped underneath between the two.
///
/// Plus a bounded read: a gate key is a short opaque token, and anything larger
/// is not one.
#[cfg(unix)]
fn read_release(path: &Path) -> Option<String> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut held = String::new();
    file.take(MAX_GATE_KEY_BYTES as u64)
        .read_to_string(&mut held)
        .ok()?;
    Some(held.trim().to_string())
}

#[cfg(not(unix))]
fn read_release(path: &Path) -> Option<String> {
    // No sandbox exists on a native Windows host (`docs/SANDBOX.md`), so no
    // gate is ever composed there. The plain read keeps the function total.
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .map(|held| held.trim().to_string())
}

/// Replace this process with the agent.
///
/// `execvp` rather than a spawn, and that is the whole point: the pane's process
/// stays the agent, `--unshare-pid`'s pid 1 stays the agent, and there is no
/// helper left to reap, signal or leak. A failure returns rather than exiting,
/// so the caller reports it the way it reports every other refusal.
#[cfg(unix)]
fn exec_agent(argv: &[String]) -> Result<(), String> {
    use std::os::unix::process::CommandExt as _;

    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "no agent command was given to run".to_string())?;
    // `exec` only ever returns an error; on success this process is gone.
    Err(format!(
        "cannot run '{program}': {}",
        std::process::Command::new(program).args(args).exec()
    ))
}

#[cfg(not(unix))]
fn exec_agent(argv: &[String]) -> Result<(), String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "no agent command was given to run".to_string())?;
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("cannot run '{program}': {e}"))?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The routing decision, without running a command that never returns.
    #[test]
    fn the_in_boundary_commands_are_served_before_the_database() {
        let relay = Command::Sandbox {
            action: Action::Relay {
                listen: "127.0.0.1:0".parse().unwrap(),
                socket: "/s/p.sock".into(),
            },
        };
        let launch = Command::Sandbox {
            action: Action::Launch {
                gate: None,
                key: None,
                timeout: 1,
                relay_listen: None,
                relay_socket: None,
                unset: Vec::new(),
                agent: vec!["/usr/bin/true".to_string()],
            },
        };
        let host_side = Command::Sandbox {
            action: Action::List { instances: false },
        };
        assert!(is_database_free(&relay));
        assert!(is_database_free(&launch));
        assert!(!is_database_free(&host_side));
        assert!(run_before_database(&cli_for(host_side)).is_none());
    }

    /// A whole `Cli` around one command, with every format flag off.
    fn cli_for(command: Command) -> Cli {
        use clap::Parser as _;
        let mut cli = Cli::parse_from(["friring-cli", "capabilities"]);
        cli.command = command;
        cli
    }

    /// `config paths` is answered early and every other `config` subcommand is
    /// not: the report's whole value is that it precedes the open.
    #[test]
    fn only_the_paths_report_of_config_runs_before_the_database() {
        use clap::Parser as _;

        let paths = Cli::parse_from(["friring-cli", "config", "paths"]);
        assert!(is_database_free(&paths.command));
        for argv in [
            vec!["friring-cli", "config", "show"],
            vec!["friring-cli", "config", "validate"],
        ] {
            let cli = Cli::parse_from(argv.clone());
            assert!(
                !is_database_free(&cli.command),
                "{argv:?} reads the database and must take the ordinary path"
            );
        }
    }

    /// Every bridge verb is served before the database opens, and none of the
    /// host-side commands is.
    #[test]
    fn every_bridge_verb_runs_before_the_database() {
        use clap::Parser as _;

        for argv in [
            vec!["friring-cli", "bridge", "status"],
            vec!["friring-cli", "bridge", "inbox", "--claim"],
            vec![
                "friring-cli",
                "bridge",
                "send",
                "--to",
                "owner",
                "--kind",
                "result",
                "--body",
                "{}",
            ],
            vec!["friring-cli", "bridge", "report", "--phase", "implementing"],
            vec![
                "friring-cli",
                "bridge",
                "create",
                "--repo-root",
                "/repo",
                "--branch",
                "feat/x",
                "--agent",
                "worker",
                "--task-body",
                "do the thing",
            ],
            vec!["friring-cli", "bridge", "stop", "c1"],
            vec!["friring-cli", "bridge", "resume", "c1"],
        ] {
            let cli = crate::cli::Cli::parse_from(argv.clone());
            assert!(
                is_database_free(&cli.command),
                "{argv:?} runs inside a boundary and must be dispatched early"
            );
        }
        // `capabilities` is host-side: it reads constants, not a boundary.
        let host_side = crate::cli::Cli::parse_from(["friring-cli", "capabilities"]);
        assert!(!is_database_free(&host_side.command));
    }

    /// ADR-29 as a property of this module's source: nothing here names the
    /// storage layer, so no code path from an in-boundary command can reach a
    /// database handle. The whole input is argv, which the host composed.
    ///
    /// A source check rather than a runtime one because the runtime version
    /// cannot exist: `sandbox relay` serves until its sandbox is torn down and
    /// `sandbox launch` execs, so a test that *ran* either would never return.
    #[test]
    fn the_early_path_never_names_the_storage_layer() {
        let whole = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli/early.rs"),
        )
        .expect("this module's own source");
        // The test module below names these in order to forbid them, so it is
        // cut before the check rather than made to spell them obliquely.
        let source = whole
            .split_once("#[cfg(test)]")
            .map_or(whole.as_str(), |(code, _)| code);
        for forbidden in ["crate::storage", "Database::", "database_file("] {
            assert!(
                !source.contains(forbidden),
                "cli::early must not reach the database, but names '{forbidden}' (ADR-29)"
            );
        }
    }

    #[cfg(unix)]
    mod gate {
        use super::*;

        fn temp_gate(name: &str) -> std::path::PathBuf {
            crate::sandbox::dirs::test_temp_base(name)
        }

        #[test]
        fn a_matching_release_file_opens_the_gate() {
            let dir = temp_gate("gate-open");
            std::fs::write(dir.join(crate::sandbox::dirs::GATE_RELEASE_NAME), "k-1\n").unwrap();
            assert!(wait_for_gate(&dir, "k-1", Duration::from_millis(50)));
        }

        /// A release file left by an earlier launch of the same child must not
        /// open this one, which is what the per-launch key is for.
        #[test]
        fn a_stale_key_does_not_open_the_gate() {
            let dir = temp_gate("gate-stale");
            std::fs::write(dir.join(crate::sandbox::dirs::GATE_RELEASE_NAME), "k-0").unwrap();
            assert!(!wait_for_gate(&dir, "k-1", Duration::from_millis(50)));
        }

        #[test]
        fn an_absent_release_file_times_out() {
            let dir = temp_gate("gate-absent");
            assert!(!wait_for_gate(&dir, "k-1", Duration::from_millis(50)));
        }

        /// A symlink pointing at a file that holds the key is still not a
        /// release: the open refuses to follow it, so the gate stays shut.
        #[test]
        fn a_symlinked_release_is_ignored() {
            let dir = temp_gate("gate-symlink");
            let real = dir.join("elsewhere");
            std::fs::write(&real, "k-1").unwrap();
            std::os::unix::fs::symlink(&real, dir.join(crate::sandbox::dirs::GATE_RELEASE_NAME))
                .unwrap();
            assert!(!wait_for_gate(&dir, "k-1", Duration::from_millis(50)));
        }

        /// A FIFO is the shape the gate proof rules out on purpose: a read-only
        /// bind does not stop one being opened for writing, so a gate that
        /// accepted one could be released from inside the boundary.
        #[test]
        fn a_fifo_at_the_release_path_is_ignored() {
            let dir = temp_gate("gate-fifo");
            let path = dir.join(crate::sandbox::dirs::GATE_RELEASE_NAME);
            let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: a NUL-terminated path this test owns, and a mode with no
            // bits outside the permission mask.
            let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
            assert_eq!(made, 0, "the fixture needs a FIFO");
            assert!(!wait_for_gate(&dir, "k-1", Duration::from_millis(50)));
        }
    }
}
