//! ADR-29 as a property of the **binary**, not of a module.
//!
//! A bridge verb runs inside a sandbox where friring's database is denied on
//! purpose. `cli::early` has two unit tests for it — the verb classification and
//! a scan of its own source — but neither exercises `main`, and the guarantee is
//! about what the process does: a client inside a boundary must fail on its
//! *queue*, never on a database it was never supposed to look for.
//!
//! The check is deterministic because `FRIRING_DATA_DIR` wins outright in a
//! non-test build, and a database open under a path that cannot hold one exits
//! `2` with a message of its own. So a run that exits `1` naming
//! `FRIRING_BRIDGE_DIR` is proof that nothing between `main` and the verb went
//! looking for storage.

use std::process::Command;

/// The binary under test, built by cargo for this integration test.
const CLI: &str = env!("CARGO_BIN_EXE_friring-cli");

/// A directory of this test's own, removed and recreated so a rerun starts
/// clean.
fn fixture(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("friring-bridge-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a fixture directory");
    dir
}

/// `friring-cli` with a data directory no database can be opened under, and no
/// inherited XDG or bridge environment to fall back on.
fn cli(data_dir: &std::path::Path) -> Command {
    let mut command = Command::new(CLI);
    command
        .env("FRIRING_DATA_DIR", data_dir)
        .env_remove("XDG_DATA_HOME")
        .env_remove("FRIRING_BRIDGE_DIR");
    command
}

#[test]
fn a_bridge_verb_fails_on_its_queue_and_never_on_the_database() {
    let dir = fixture("no-db");
    // A *file* where the data directory would be: every path under it is
    // unopenable, so any attempt to reach storage fails loudly rather than
    // quietly creating a stray database inside the boundary.
    let blocker = dir.join("not-a-directory");
    std::fs::write(&blocker, "").expect("the blocking file");
    let data_dir = blocker.join("data");

    let output = cli(&data_dir)
        .args(["bridge", "status"])
        .output()
        .expect("the cli runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "a bridge verb must fail as a client, not as a host: {stderr}"
    );
    assert!(
        stderr.contains("FRIRING_BRIDGE_DIR"),
        "the refusal must name the queue this process has no grant for: {stderr}"
    );
    assert!(
        !stderr.contains("failed to open database"),
        "a bridge verb opened the database (ADR-29): {stderr}"
    );
}

#[test]
fn a_bridge_verb_with_a_queue_fails_on_the_queue_alone() {
    let dir = fixture("queue-only");
    let blocker = dir.join("not-a-directory");
    std::fs::write(&blocker, "").expect("the blocking file");
    let queue = dir.join("queue");
    std::fs::create_dir_all(queue.join("req")).expect("a request directory");
    std::fs::create_dir_all(queue.join("res")).expect("a response directory");

    let output = cli(&blocker.join("data"))
        .env("FRIRING_BRIDGE_DIR", &queue)
        .args(["bridge", "status", "--timeout", "1"])
        .output()
        .expect("the cli runs");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Nothing answers a queue with no broker, so the only failure available is
    // the wait — which is exactly the point: the verb got all the way to its
    // channel without a database handle existing anywhere behind it.
    assert_eq!(
        output.status.code(),
        Some(1),
        "an unanswered queue is a client failure: {stderr}"
    );
    assert!(
        !stderr.contains("failed to open database"),
        "a bridge verb opened the database (ADR-29): {stderr}"
    );
}
