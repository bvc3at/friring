//! The bridge harnesses ask whether this host can carry the bridge the way
//! friring asks it, and a dedicated CI gate cannot answer by skipping.
//!
//! Both halves are regressions rather than theory. `scripts/dev/bridge-e2e.sh`
//! and its two siblings gated on `command -v bwrap` — a *packaging* question —
//! while `scripts/dev/sandbox-probes/bwrap.sh` asked the *capability* question
//! and skipped where the kernel refused an unprivileged user namespace. On a
//! runner that refuses one, the probe reported success for making no
//! assertions and the harnesses ran anyway, `auto` fell through the ladder to a
//! backend `Caps::bridge` refuses, and the launch was rejected. Neither the
//! green probe nor the harness's own output said the bridge had not been
//! reached.
//!
//! Asserted on the scripts' source because that is where the drift lives, and
//! on the helper's behaviour because a comment is not a contract.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every script that needs a bridge-carrying backend, and the name it passes.
const GATED: [&str; 4] = [
    "scripts/dev/bridge-e2e.sh",
    "scripts/dev/codex-park-e2e.sh",
    "scripts/dev/omx-team-e2e.sh",
    "scripts/dev/sandbox-probes/bwrap.sh",
];

#[test]
fn every_bridge_harness_asks_the_capability_question_through_the_shared_gate() {
    for script in GATED {
        let source = read(script);
        assert!(
            source.contains("scripts/dev/lib/bridge-backend.sh"),
            "{script} does not source the shared capability gate"
        );
        assert!(
            source.contains("bridge_backend_or_skip"),
            "{script} sources the gate but never calls it"
        );
        // The exact shape that made a runner without user namespaces look
        // ready. `command -v bwrap` is fine *inside* the shared gate, which
        // follows it with a real `unshare`; it is not fine as the whole test.
        assert!(
            !source.contains("command -v bwrap"),
            "{script} still gates on bwrap being installed, which says nothing \
             about whether this kernel will grant it a namespace"
        );
    }
}

#[test]
fn the_shared_gate_creates_a_namespace_rather_than_looking_for_a_binary() {
    let gate = read("scripts/dev/lib/bridge-backend.sh");
    assert!(
        gate.contains("--unshare-all"),
        "the gate does not actually try to create a namespace"
    );
    // The failure's own words are carried out, because the same non-zero exit
    // covers a refused namespace, an unmakeable mount and a missing `true`.
    assert!(
        gate.contains("probe_err=$({ bwrap"),
        "the gate discards the probe's stderr, so a failure cannot say why"
    );
    assert!(
        gate.contains("${probe_err:-(no stderr)}"),
        "a probe that failed silently would report nothing at all"
    );
}

/// Run a snippet against the gate, returning `(exit status, stderr)`.
fn gate(require: bool, snippet: &str) -> (i32, String) {
    let path = repo_root().join("scripts/dev/lib/bridge-backend.sh");
    let script = format!(". {}\n{snippet}\n", path.display());
    let out = Command::new("bash")
        .arg("-c")
        .arg(script)
        .env(
            "FRIRING_E2E_REQUIRE_BRIDGE",
            if require { "1" } else { "0" },
        )
        .output()
        .expect("bash runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_skip_is_a_skip_by_default_and_a_failure_when_the_gate_is_required() {
    let snippet = r#"bridge_require_or_skip "probe" "no namespaces here""#;

    let (status, stderr) = gate(false, snippet);
    assert_eq!(status, 0, "a developer's machine gets an honest skip");
    assert!(stderr.contains("skipping"), "{stderr}");

    let (status, stderr) = gate(true, snippet);
    assert_eq!(
        status, 1,
        "a dedicated gate reported success for assertions it never made"
    );
    assert!(stderr.contains("no namespaces here"), "{stderr}");
}

/// The skip paths that are *not* the capability gate — an absent tmux, a
/// missing vendor tool, a registry that could not be reached — route through
/// the same helper, so requiring the gate cannot be satisfied by exiting 0 a
/// few lines later.
#[test]
fn no_later_skip_path_can_exit_zero_when_the_gate_is_required() {
    for script in GATED {
        let source = read(script);
        for (number, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed != "exit 0" {
                continue;
            }
            // The one allowed unconditional exit: a platform that has no
            // backend to probe at all, decided before the gate is sourced.
            let preceding = source.lines().take(number).collect::<Vec<_>>().join("\n");
            assert!(
                preceding.contains("bubblewrap is Linux only"),
                "{script}:{} exits 0 outside the shared gate, so a required run \
                 could still report success without asserting anything",
                number + 1
            );
        }
    }
}

/// Reading the log for a refusal must not itself end the run.
///
/// `bridge-e2e.sh` runs under `set -euo pipefail`, where `x=$(grep …)` exits the
/// script when grep matches nothing — and matching nothing is the state of
/// every poll before the wizard is answered. Exercised as behaviour under those
/// options, in both directions, because the shape of this bug is invisible to
/// any assertion about the source.
#[test]
fn reading_the_log_for_a_refusal_survives_set_e_when_there_is_none() {
    let lib = repo_root().join("scripts/dev/lib/harness-log.sh");
    let run = |data_dir: &Path| {
        let script = format!(
            "set -euo pipefail\n. {}\nrefused=$(harness_spawn_refusal {} probe)\n\
             printf 'status=%s refused=[%s]\\n' \"$?\" \"$refused\"\n",
            lib.display(),
            data_dir.display()
        );
        let out = Command::new("bash")
            .arg("-c")
            .arg(script)
            .output()
            .expect("bash runs");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        )
    };

    let tmp = std::env::temp_dir().join(format!("friring-harness-log-{}", std::process::id()));
    let empty = tmp.join("empty");
    let refused = tmp.join("refused");
    let no_logs = tmp.join("no-logs");
    for dir in [&empty, &refused, &no_logs] {
        std::fs::create_dir_all(dir).expect("the fixture");
    }
    std::fs::write(empty.join("friring.log.2026-01-01"), "INFO all is well\n").unwrap();
    std::fs::write(
        refused.join("friring.log.2026-01-01"),
        "INFO starting\nERROR friring::app: Failed to spawn session: no backend carries it\n",
    )
    .unwrap();

    // No logs at all, and logs with no refusal: both are an ordinary poll.
    for (case, dir) in [("no logs", &no_logs), ("no refusal", &empty)] {
        let (status, out) = run(dir);
        assert_eq!(status, Some(0), "{case}: the poll killed the run: {out}");
        assert_eq!(out, "status=0 refused=[]", "{case}");
    }

    let (status, out) = run(&refused);
    assert_eq!(status, Some(0), "a refusal killed the run: {out}");
    assert!(
        out.contains("Failed to spawn session: no backend carries it"),
        "the refusal was not reported: {out}"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

/// Both boundary probes, and the network-mode loop each one makes its
/// assertions in.
const PROBES: [&str; 2] = [
    "scripts/dev/sandbox-probes/seatbelt.sh",
    "scripts/dev/sandbox-probes/bwrap.sh",
];

/// The body of `for mode in full allowlist none; do … done`.
fn mode_loop(source: &str, script: &str) -> String {
    let body = source
        .split_once("for mode in full allowlist none; do")
        .unwrap_or_else(|| panic!("{script} has no network-mode loop"))
        .1;
    body.split_once("\ndone")
        .unwrap_or_else(|| panic!("{script}'s mode loop never ends"))
        .0
        .to_string()
}

/// A network mode whose launches never start must not be counted as observed.
///
/// The regression: every `probe_denied` is an exit status, and friring refuses a
/// **filtered** profile to a one-shot before the command runs — so the whole
/// deny set passed under `network_mode = allowlist` for the reason nothing ran,
/// and the tally reported those six as boundary assertions. The fix is
/// structural rather than a comment: the mode's positive control is the gate on
/// its deny set, and a mode that cannot launch is recorded as not asked.
#[test]
fn no_mode_is_counted_without_a_launch_that_composed() {
    for script in PROBES {
        let source = read(script);
        let loop_body = mode_loop(&source, script);
        let launches = loop_body
            .find("probe_launches")
            .unwrap_or_else(|| panic!("{script} never asks whether the mode launches"));
        let denied = loop_body
            .find("probe_denied")
            .unwrap_or_else(|| panic!("{script}'s mode loop makes no deny assertion"));
        let allowed = loop_body
            .find("probe_allowed")
            .unwrap_or_else(|| panic!("{script}'s mode loop has no positive control"));
        assert!(
            launches < denied && allowed < denied,
            "{script} counts a deny assertion before anything established that a \
             launch in that mode starts at all"
        );
        assert!(
            loop_body.contains("probe_mode_unlaunchable"),
            "{script} skips a mode it cannot launch without deciding whether that \
             is a scope limit or a regression"
        );
        assert!(
            !loop_body.contains("probe_unexercised"),
            "{script} calls the neutral outcome directly, so a supported mode that \
             stopped launching would be recorded as merely not asked"
        );
    }
}

/// A mode nothing can launch in **by design** is a scope limit; a supported mode
/// that stops launching is a regression that takes its whole deny set with it.
///
/// Both leave the same empty transcript, so the outcome cannot be read off the
/// launch failing. `none` is the mode `bridge-conformance` itself runs under: if
/// it broke and this recorded "not asked", a required job would stay green with
/// a footnote where its Linux boundary checks used to be.
#[test]
fn a_supported_mode_that_stops_launching_is_a_failure_not_a_footnote() {
    let root = repo_root();
    let outcome = |mode: &str| {
        let script = format!(
            "set -uo pipefail\nREPO_ROOT={root}\nPROBE_NAME=probe\n\
             . {root}/scripts/dev/sandbox-probes/common.sh\n\
             probe_mode_unlaunchable {mode}\nprobe_summary\n",
            root = root.display()
        );
        let out = Command::new("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("bash runs");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };

    // The one a one-shot cannot launch in: neutral, and named in the tally.
    let (status, stdout) = outcome("allowlist");
    assert_eq!(status, Some(0), "a scope limit failed the probe: {stdout}");
    assert!(
        stdout.contains("0 passed, 0 failed, 1 not asked"),
        "the scope limit was not recorded apart from the passes: {stdout}"
    );

    // Every mode a one-shot is supposed to launch in.
    for mode in ["full", "none"] {
        let (status, stdout) = outcome(mode);
        assert_eq!(
            status,
            Some(1),
            "{mode} stopped launching and the probe stayed green: {stdout}"
        );
        assert!(
            stdout.contains("0 passed, 1 failed, 0 not asked"),
            "{mode} was recorded as not asked rather than as broken: {stdout}"
        );
        assert!(
            stdout.contains("would have passed for the reason nothing ran"),
            "{mode}'s failure does not say what it costs: {stdout}"
        );
    }
}

/// An assertion about a launch reads a file the launch wrote, never its stdout.
///
/// `friring-cli` prints its own one-line summary of the applied boundary after
/// the wrapped command's output, and with stdout redirected — every command
/// substitution — that line is JSON whatever format flags are passed. An
/// assertion matching *within* that stream accepts one good line among the
/// trailer, which is how a strict check first caught it.
#[test]
fn nothing_asserts_on_a_launchs_stdout() {
    let common = read("scripts/dev/sandbox-probes/common.sh");
    assert!(
        !common.contains("probe_run_raw"),
        "the raw runner is back; its output still carries friring's own summary"
    );
    let bwrap = read("scripts/dev/sandbox-probes/bwrap.sh");
    assert!(
        bwrap.contains("readlink /proc/self/ns/pid > '$NS_FILE'"),
        "the namespace assertion no longer reads the answer out of a file"
    );
    for script in PROBES {
        assert!(
            !read(script).contains("probe_run_raw"),
            "{script} still reads a launch's stdout"
        );
    }
}

/// What was not asked is part of the tally, not a footnote.
#[test]
fn the_summary_reports_what_it_could_not_ask() {
    let root = repo_root();
    let script = format!(
        "set -euo pipefail\nREPO_ROOT={root}\nPROBE_NAME=probe\n\
         . {root}/scripts/dev/sandbox-probes/common.sh\n\
         probe_ok 'something real'\n\
         probe_unexercised 'network_mode = allowlist' 'a one-shot cannot own the proxy'\n\
         probe_summary\n",
        root = root.display()
    );
    let out = Command::new("bash")
        .arg("-c")
        .arg(&script)
        .output()
        .expect("bash runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a mode that could not be asked is not a failure: {stdout}"
    );
    assert!(
        stdout.contains("probe: 1 passed, 0 failed, 1 not asked"),
        "the tally hides what went unexercised: {stdout}"
    );
    assert!(
        stdout.contains("network_mode = allowlist — a one-shot cannot own the proxy"),
        "the summary does not say which assertions were never made: {stdout}"
    );
}

#[test]
fn the_ci_jobs_that_exist_for_these_assertions_require_them() {
    let workflow = read(".github/workflows/ci.yml");
    assert_eq!(
        workflow
            .matches(r#"FRIRING_E2E_REQUIRE_BRIDGE: "1""#)
            .count(),
        3,
        "the seatbelt probe, the bwrap probe and the bridge e2e job each have to \
         require their own assertions"
    );
    for job in ["seatbelt-probe", "bwrap-probe", "bridge-e2e"] {
        assert!(
            workflow.contains(&format!("      - {job}")),
            "{job} is not in the aggregate gate, so a failure would not block"
        );
    }
    assert!(
        !workflow.contains("continue-on-error: true"),
        "a dedicated gate that cannot skip has no reason to be advisory"
    );
}

/// The runner helper changes kernel settings, so it runs only where a machine
/// is re-imaged for every job.
///
/// `CI` and `GITHUB_ACTIONS` do not establish that — both are equally true on a
/// **self-hosted** runner, which is somebody's real machine — so the helper is
/// given `runner.environment` and refuses anything but `github-hosted`. The
/// three cases below are a developer's shell, a self-hosted runner, and a
/// caller that thought the CI variables were enough.
#[test]
fn the_runner_helper_runs_only_on_a_github_hosted_runner() {
    let helper = repo_root().join("scripts/ci/allow-user-namespaces.sh");
    let refused = |env: &[(&str, &str)]| {
        let mut cmd = Command::new("bash");
        cmd.arg(&helper)
            .env_remove("GITHUB_ACTIONS")
            .env_remove("CI")
            .env_remove("RUNNER_ENVIRONMENT");
        for (key, value) in env {
            cmd.env(key, value);
        }
        let out = cmd.output().expect("bash runs");
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    for (case, env) in [
        ("a developer's shell", &[][..]),
        (
            "a self-hosted runner",
            &[
                ("CI", "true"),
                ("GITHUB_ACTIONS", "true"),
                ("RUNNER_ENVIRONMENT", "self-hosted"),
            ][..],
        ),
        (
            "the CI variables alone",
            &[("CI", "true"), ("GITHUB_ACTIONS", "true")][..],
        ),
    ] {
        let (status, stderr) = refused(env);
        assert_eq!(status, Some(2), "{case}: the helper ran anyway: {stderr}");
        assert!(stderr.contains("GitHub-hosted runner"), "{case}: {stderr}");
    }

    // And it refuses *before* it reaches anything that changes state.
    let source = read("scripts/ci/allow-user-namespaces.sh");
    let guard = source.find("RUNNER_ENVIRONMENT").expect("the guard");
    let first_change = source.find("sudo ").expect("something it changes");
    assert!(
        guard < first_change,
        "the helper can reach a `sudo` before deciding whether it may"
    );

    // The workflow has to actually pass it, or the guard refuses every run.
    let workflow = read(".github/workflows/ci.yml");
    assert_eq!(
        workflow
            .matches("RUNNER_ENVIRONMENT: ${{ runner.environment }}")
            .count(),
        2,
        "both namespace steps have to report the runner they are on"
    );
}

#[test]
fn the_path_filter_reaches_everything_these_gates_depend_on() {
    let workflow = read(".github/workflows/ci.yml");
    let filter = workflow
        .split_once("            sandbox_probe:")
        .expect("the sandbox_probe filter")
        .1
        .split_once("            omx:")
        .expect("the next filter")
        .0;
    for needed in [
        "scripts/dev/lib/**",
        "scripts/dev/bridge-e2e.sh",
        "scripts/dev/codex-park-e2e.sh",
        "scripts/dev/omx-team-e2e.sh",
        "scripts/dev/sandbox-probes/**",
        "scripts/ci/allow-user-namespaces.sh",
        "tests/harness_capability_gate.rs",
    ] {
        assert!(
            filter.contains(needed),
            "editing {needed} would not re-run the gates that depend on it"
        );
    }
    // And the file this test is in has to exist where the filter names it.
    assert!(
        Path::new(&repo_root().join("tests/harness_capability_gate.rs")).is_file(),
        "the filter names a test file that is not there"
    );
}
