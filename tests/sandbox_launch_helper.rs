//! `friring-cli sandbox launch`, run as the real binary (ADR-33).
//!
//! The unit tests in `cli::early` cover the gate predicate; these cover the
//! things only a process can show: that the helper **execs in place** rather
//! than forking, that a gate that never opens exits `75` instead of starting the
//! agent, that the multiplexer variables are gone from the agent's own
//! environment, and — on Linux — that no relay survives the helper.
//!
//! Nothing here starts an agent, opens a socket to anything, or touches a
//! session: the agent's place is taken by `/usr/bin/true`, `printenv` and a
//! two-line script, and every fixture lives in a directory this test made.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The binary under test, built by cargo for this integration test.
const HELPER: &str = env!("CARGO_BIN_EXE_friring-cli");

/// A directory of this test's own, removed and recreated so a rerun starts
/// clean.
fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("friring-launch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a fixture directory");
    dir
}

/// Write the release file the way the host does — staged elsewhere and renamed
/// into place, so a reader never sees a half-written key.
fn release(gate: &Path, key: &str) {
    let staged = gate.with_extension("staged");
    std::fs::write(&staged, key).expect("a staged release file");
    std::fs::rename(staged, gate.join("release")).expect("the rename into the gate");
}

fn launch(args: &[&str]) -> Command {
    let mut command = Command::new(HELPER);
    command.args(["sandbox", "launch"]).args(args);
    command
}

#[test]
fn an_ungated_launch_becomes_the_agent() {
    let output = launch(&["--", "/usr/bin/true"])
        .output()
        .expect("the helper runs");
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn a_released_gate_lets_the_agent_run() {
    let gate = fixture("released");
    release(&gate, "k-1");
    let output = launch(&[
        "--gate",
        &gate.display().to_string(),
        "--key",
        "k-1",
        "--timeout",
        "5",
        "--",
        "/usr/bin/true",
    ])
    .output()
    .expect("the helper runs");
    assert!(output.status.success(), "{output:?}");
}

/// The backstop the whole gate design rests on: a host that died before it
/// committed the child's row leaves a pane that exits, never an agent running
/// from a session that does not exist.
#[test]
fn a_gate_that_never_opens_exits_temporarily_failed() {
    let gate = fixture("never-opens");
    let output = launch(&[
        "--gate",
        &gate.display().to_string(),
        "--key",
        "k-1",
        "--timeout",
        "1",
        "--",
        "/usr/bin/false",
    ])
    .output()
    .expect("the helper runs");
    assert_eq!(output.status.code(), Some(75), "{output:?}");
    // `/usr/bin/false` would also be a non-zero exit, so the message is what
    // distinguishes "the gate timed out" from "the agent failed".
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("did not open"), "{said}");
}

/// A key from an earlier launch of the same child must not open this one.
#[test]
fn a_stale_key_does_not_open_the_gate() {
    let gate = fixture("stale-key");
    release(&gate, "k-0");
    let output = launch(&[
        "--gate",
        &gate.display().to_string(),
        "--key",
        "k-1",
        "--timeout",
        "1",
        "--",
        "/usr/bin/true",
    ])
    .output()
    .expect("the helper runs");
    assert_eq!(output.status.code(), Some(75), "{output:?}");
}

/// A symlink at the release path points somewhere the agent may be able to
/// write; the helper refuses to follow it, so the gate stays shut.
#[test]
fn a_symlinked_release_does_not_open_the_gate() {
    let gate = fixture("symlinked");
    let elsewhere = gate.join("elsewhere");
    std::fs::write(&elsewhere, "k-1").expect("a file to link at");
    std::os::unix::fs::symlink(&elsewhere, gate.join("release")).expect("the link");
    let output = launch(&[
        "--gate",
        &gate.display().to_string(),
        "--key",
        "k-1",
        "--timeout",
        "1",
        "--",
        "/usr/bin/true",
    ])
    .output()
    .expect("the helper runs");
    assert_eq!(output.status.code(), Some(75), "{output:?}");
}

/// The variables tmux sets in the pane point at friring's **own** server, so
/// the agent must not inherit them. Asserted through the agent itself:
/// `printenv` is what the helper exec'd, and its output is the environment the
/// agent really got.
#[test]
fn the_named_variables_are_gone_from_the_agents_environment() {
    let output = launch(&[
        "--unset",
        "TMUX",
        "--unset",
        "TMUX_PANE",
        "--",
        "/usr/bin/env",
    ])
    .env("TMUX", "/tmp/tmux-501/friring,123,0")
    .env("TMUX_PANE", "%7")
    .env("FRIRING_LAUNCH_PROBE", "kept")
    .output()
    .expect("the helper runs");
    assert!(output.status.success(), "{output:?}");
    let environment = String::from_utf8_lossy(&output.stdout);
    assert!(!environment.contains("TMUX="), "{environment}");
    assert!(!environment.contains("TMUX_PANE="), "{environment}");
    // Only the named ones: an unrelated variable the launch set survives.
    assert!(
        environment.contains("FRIRING_LAUNCH_PROBE=kept"),
        "{environment}"
    );
}

/// `execvp`, not a spawn — which is what keeps the pane's process the agent and,
/// under `--unshare-pid`, keeps the agent as pid 1 so the namespace teardown
/// still takes the relay with it. The exec'd program prints its own pid, and it
/// is the pid cargo's `Child` was handed.
#[test]
fn the_helper_execs_in_place() {
    let dir = fixture("exec-in-place");
    let script = dir.join("print-pid.sh");
    std::fs::write(&script, "#!/bin/sh\necho $$\n").expect("the probe script");
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
    std::fs::set_permissions(&script, permissions).expect("an executable probe");

    let child = launch(&["--", &script.display().to_string()])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("the helper starts");
    let spawned = child.id();
    let output = child.wait_with_output().expect("the helper finishes");
    let reported: u32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("the probe prints its pid");
    assert_eq!(
        reported, spawned,
        "the helper forked instead of exec'ing: a pane whose process is not the agent"
    );
}

/// A relay is a direct child of the helper and dies with it. Linux-only,
/// because that is where `PR_SET_PDEATHSIG` gives the guarantee outside a pid
/// namespace — and where a bubblewrap sandbox is the shape being modelled.
/// The socket path is unique to this test and nothing ever binds it.
#[cfg(target_os = "linux")]
#[test]
fn no_relay_survives_the_helper() {
    let dir = fixture("relay-teardown");
    let socket = dir.join("proxy.sock");
    let socket = socket.display().to_string();
    assert!(!relay_running(&socket), "the fixture socket must be unused");

    // A gate that never opens: the helper starts the relay, waits, gives up.
    let output = launch(&[
        "--gate",
        &dir.display().to_string(),
        "--key",
        "k-1",
        "--timeout",
        "1",
        "--relay-listen",
        "127.0.0.1:0",
        "--relay-socket",
        &socket,
        "--",
        "/usr/bin/true",
    ])
    .output()
    .expect("the helper runs");
    assert_eq!(output.status.code(), Some(75), "{output:?}");

    // The relay may take a moment to notice its parent is gone.
    for _ in 0..50 {
        if !relay_running(&socket) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("a relay for '{socket}' outlived the helper that started it");
}

/// Whether any process names this exact socket path — the unique fixture path,
/// so no other test's or session's relay can be mistaken for it.
#[cfg(target_os = "linux")]
fn relay_running(socket: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", socket])
        .output()
        .is_ok_and(|out| out.status.success())
}
