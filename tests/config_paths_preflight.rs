//! `friring-cli config paths` as a property of the **binary**: it answers
//! before `Database::open`, and it answers with the paths this process resolved.
//!
//! A dev harness that redirects `HOME`, the `XDG_*` roots and the two
//! `FRIRING_*_DIR` overrides has, until this command existed, no way to check
//! that the binary agreed with it. It could only run something and hope. When
//! that assumption failed once — a friring reaching a data directory the harness
//! believed it had redirected away from — nothing in the harness could have
//! caught it, because the first thing that reveals the resolved database path is
//! the open itself, which is already too late.
//!
//! So the guarantee under test is a *timing* one and cannot be checked from
//! inside the library: it has to be the real process, with a real environment,
//! under a data directory where opening a database is impossible. A run that
//! prints the report and exits `0` there is proof the open never happened; the
//! contrast case — an ordinary command under the same environment exiting `2` —
//! is proof the impossibility is real and not an artifact of the fixture.

use std::path::Path;
use std::process::Command;

/// The binary under test, built by cargo for this integration test.
const CLI: &str = env!("CARGO_BIN_EXE_friring-cli");

/// A directory of this test's own, removed and recreated so a rerun starts
/// clean.
fn fixture(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "friring-config-paths-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a fixture directory");
    dir
}

/// `friring-cli` with **no** inherited path environment at all.
///
/// The test process runs under the operator's own environment, and this command
/// resolves directories from exactly those variables — so every one of them is
/// removed and only what a case sets is put back. Otherwise a passing assertion
/// could be describing the developer's real config directory.
fn cli() -> Command {
    let mut command = Command::new(CLI);
    for var in [
        "FRIRING_CONFIG_DIR",
        "FRIRING_DATA_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "HOME",
        // The Windows fallbacks. Removed on every platform so one list
        // describes the whole resolution chain rather than half of it.
        "APPDATA",
        "LOCALAPPDATA",
        "USERPROFILE",
    ] {
        command.env_remove(var);
    }
    command
}

/// Run `config paths --json` and parse the report, failing loudly on a non-zero
/// exit.
fn report(command: &mut Command) -> serde_json::Value {
    let output = command
        .args(["--json", "config", "paths"])
        .output()
        .expect("the cli runs");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "config paths must answer without a database: {stderr}"
    );
    serde_json::from_slice(&output.stdout).expect("a JSON report on stdout")
}

/// A data directory nothing can open a database under: a regular file stands
/// where its parent directory would be, so every path below it is unopenable.
fn unopenable(dir: &Path) -> std::path::PathBuf {
    let blocker = dir.join("not-a-directory");
    std::fs::write(&blocker, "").expect("the blocking file");
    blocker.join("data")
}

#[test]
fn the_paths_report_is_answered_before_the_database_is_opened() {
    let dir = fixture("no-db");
    let data_dir = unopenable(&dir);
    let config_dir = dir.join("config");

    let json = report(
        cli()
            .env("FRIRING_CONFIG_DIR", &config_dir)
            .env("FRIRING_DATA_DIR", &data_dir),
    );

    assert_eq!(json["data_dir"], data_dir.display().to_string());
    assert_eq!(json["config_dir"], config_dir.display().to_string());
    assert_eq!(json["database_opened"], false);
    assert_eq!(
        json["database"],
        data_dir.join("friring.db").display().to_string(),
        "the report must name the file that would be opened, not a directory"
    );

    // The contrast that makes the assertion above mean something: the same
    // environment, a command that does open the database, and a hard failure.
    let ordinary = cli()
        .env("FRIRING_CONFIG_DIR", &config_dir)
        .env("FRIRING_DATA_DIR", &data_dir)
        .args(["--json", "config", "show"])
        .output()
        .expect("the cli runs");
    assert_eq!(
        ordinary.status.code(),
        Some(2),
        "this fixture is only meaningful if a database truly cannot be opened under it"
    );
}

/// The explicit overrides win, and the report says so by name — which is what a
/// harness compares against the variables it set.
#[test]
fn an_explicit_override_is_reported_as_the_source() {
    let dir = fixture("explicit");
    let json = report(
        cli()
            .env("FRIRING_CONFIG_DIR", dir.join("cfg"))
            .env("FRIRING_DATA_DIR", dir.join("data"))
            // Set, and losing: an override that did not win would be reported
            // as `XDG_*`, which is the failure this whole surface exists to
            // make visible.
            .env("XDG_CONFIG_HOME", dir.join("xdg-cfg"))
            .env("XDG_DATA_HOME", dir.join("xdg-data"))
            .env("HOME", dir.join("home")),
    );

    assert_eq!(json["config_source"], "FRIRING_CONFIG_DIR");
    assert_eq!(json["data_source"], "FRIRING_DATA_DIR");
    assert_eq!(json["config_dir"], dir.join("cfg").display().to_string());
    assert_eq!(json["data_dir"], dir.join("data").display().to_string());
}

/// Without the overrides, the `XDG_*` roots decide — and the app-dir segment is
/// appended, which is the difference between the two flavors of a dev checkout.
#[test]
fn the_xdg_roots_decide_when_no_override_is_set() {
    let dir = fixture("xdg");
    let json = report(
        cli()
            .env("XDG_CONFIG_HOME", dir.join("xdg-cfg"))
            .env("XDG_DATA_HOME", dir.join("xdg-data"))
            .env("HOME", dir.join("home")),
    );

    assert_eq!(json["config_source"], "XDG_CONFIG_HOME");
    assert_eq!(json["data_source"], "XDG_DATA_HOME");
    let app = json["app_dir_name"].as_str().expect("an app dir name");
    assert!(
        app == "friring" || app == "friring-dev",
        "unexpected app dir segment {app}"
    );
    assert_eq!(
        json["data_dir"],
        dir.join("xdg-data").join(app).display().to_string()
    );
    assert_eq!(
        json["config_dir"],
        dir.join("xdg-cfg").join(app).display().to_string()
    );
}

/// A variable that is set but empty must not count as set — for either the
/// override or the XDG root.
///
/// This is where the report and the resolution could quietly disagree: an empty
/// `XDG_DATA_HOME` used to join onto nothing and produce a *relative* data
/// directory, which resolves against whatever the process's working directory
/// happens to be. A preflight comparing an absolute root against that would have
/// rejected it, which is right — but a run with no preflight would have written
/// somewhere nobody named. Both now fall through to the next link instead.
#[test]
#[cfg(not(windows))]
fn an_empty_variable_does_not_count_as_set() {
    let dir = fixture("empty");
    let home = dir.join("home");
    let json = report(
        cli()
            .env("FRIRING_DATA_DIR", "")
            .env("XDG_DATA_HOME", "")
            .env("FRIRING_CONFIG_DIR", "")
            .env("XDG_CONFIG_HOME", "")
            .env("HOME", &home),
    );

    assert_eq!(json["data_source"], "HOME");
    assert_eq!(json["config_source"], "HOME");
    let data = json["data_dir"].as_str().expect("a data dir");
    assert!(
        std::path::Path::new(data).is_absolute(),
        "an empty root produced a relative data dir: {data}"
    );
    assert!(data.starts_with(&home.display().to_string()), "got {data}");
}

/// On Windows the last resort is not one variable but two, and they decide
/// different directories: `%APPDATA%` the config dir, `%LOCALAPPDATA%` the data
/// dir. `%USERPROFILE%` decides neither, so naming it would point a harness at
/// a variable it could change without changing anything.
#[test]
#[cfg(windows)]
fn the_windows_fallbacks_are_named_separately() {
    let dir = fixture("windows-fallback");
    // Three distinct roots, so a report that named the wrong one would show up
    // as the wrong *path* too rather than only the wrong label.
    let json = report(
        cli()
            .env("APPDATA", dir.join("appdata"))
            .env("LOCALAPPDATA", dir.join("localappdata"))
            .env("USERPROFILE", dir.join("userprofile")),
    );

    assert_eq!(json["config_source"], "APPDATA");
    assert_eq!(json["data_source"], "LOCALAPPDATA");
    let app = json["app_dir_name"].as_str().expect("an app dir name");
    assert_eq!(
        json["config_dir"],
        dir.join("appdata").join(app).display().to_string()
    );
    assert_eq!(
        json["data_dir"],
        dir.join("localappdata").join(app).display().to_string()
    );
}

/// `HOME` alone is the last resort, and is reported as such: a harness that sees
/// this where it expected an override knows its environment did not reach the
/// process. This is the exact shape of the isolation failure that motivated the
/// command.
///
/// Unix only: the conventional base is `%APPDATA%`/`%LOCALAPPDATA%` on Windows,
/// so `HOME` decides nothing there.
#[test]
#[cfg(not(windows))]
fn home_is_the_last_resort_and_is_named() {
    let dir = fixture("home");
    let home = dir.join("home");
    let json = report(cli().env("HOME", &home));

    assert_eq!(json["config_source"], "HOME");
    assert_eq!(json["data_source"], "HOME");
    let app = json["app_dir_name"].as_str().expect("an app dir name");
    assert_eq!(
        json["data_dir"],
        home.join(".local")
            .join("share")
            .join(app)
            .display()
            .to_string()
    );
}
