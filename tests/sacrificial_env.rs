//! `scripts/dev/sacrificial-env.sh` as a property of the **wrapper**: it is the
//! outer ring, so the only thing that can check it is a run that deliberately
//! leaks.
//!
//! Every other invocation in this repository wraps a command that is expected to
//! leave the canaries alone, which means the detector's failure branch — the
//! canary set, the digest over it, and the mismatch exit — is never taken. All
//! three could regress together and every wrapped run would still print
//! "canaries intact". These two cases pin both branches: a command that touches
//! nothing passes, and a command that writes to the `$HOME` fallback path the
//! wrapper exists to catch fails.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output};

/// Run the wrapper around `sh -c <script>` and return its completed output.
fn wrapper(script: &str) -> Output {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/dev/sacrificial-env.sh");
    Command::new(&path)
        .args(["sh", "-c", script])
        .output()
        .unwrap_or_else(|e| panic!("{} runs: {e}", path.display()))
}

/// The throwaway root the wrapper announced, which a failing run deliberately
/// keeps as the artifact to debug — so a test that forced a failure has to
/// remove it itself.
fn announced_root(stdout: &str) -> PathBuf {
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("sacrificial-env: root "))
        .expect("the wrapper announces its root on stdout");
    PathBuf::from(line.trim())
}

#[test]
fn a_command_that_writes_nothing_leaves_the_canaries_intact() {
    let output = wrapper("true");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "a no-op command must pass:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("canaries intact"),
        "the wrapper must report the check it ran:\n{stdout}"
    );
}

#[test]
fn a_write_to_the_home_fallback_database_fails_the_run() {
    // The exact path a process that ignored both `FRIRING_DATA_DIR` and
    // `XDG_DATA_HOME` would land on, which is what the original incident hit.
    let output = wrapper(r#"printf leak >> "$HOME/.local/share/friring-dev/friring.db""#);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The evidence tree is kept on purpose; nothing else will clean it up.
    let root = announced_root(&stdout);

    let code = output.status.code();
    let intact = stdout.contains("canaries intact");
    let reported = stderr.contains("ISOLATION FAILED");

    let _ = std::fs::remove_dir_all(&root);

    assert_eq!(
        code,
        Some(1),
        "a leak must fail the run:\n{stdout}\n{stderr}"
    );
    assert!(!intact, "a leak must not be reported as intact:\n{stdout}");
    assert!(reported, "a leak must be named as one:\n{stderr}");
}
