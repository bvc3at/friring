//! The two `omx` ship gates, observed against the **installed** Codex CLI.
//!
//! Both seed modes are defined in `docs/SANDBOX.md` ("Private agent state,
//! always"), and until this file existed neither had been run: the
//! `copy-rewrite` seed was asserted on the rewriting and the `link-rw` seed on
//! the shared inode, and neither on what the vendor binary then *does* with
//! them.
//!
//! - **Relocated hooks.** A worker's `hooks.json` is `copy-rewrite`d so every
//!   mention of the family's `CODEX_HOME` becomes the child's private one. That
//!   the rewriting happens is a unit test; that codex then *fires* a hook from
//!   the new location is a property of codex, and if it did not hold the answer
//!   would be a finding — never a fallback to shared state, which would defeat
//!   the isolation ADR-31 rests on.
//! - **Credential writeback.** `link-rw` gives the child a hard link to the
//!   family's `auth.json` so a refreshed token lands in the one file (ADR-28).
//!   That only holds if codex writes that file **in place** rather than by
//!   writing a new one and renaming it over the name, which would break the
//!   link and leave the worker a private copy of a rotating credential.
//!
//! Everything runs in a temporary directory with `HOME`, `CODEX_HOME`,
//! `XDG_CONFIG_HOME` and `XDG_DATA_HOME` all pointed into it, and against a
//! local model stub. Nothing here reads the developer's own Codex state, and
//! nothing reaches a network: the one turn is answered by
//! `scripts/dev/agent-e2e/stub/openai-stub.mjs` on loopback, and the key the
//! credential probe writes is a literal that is not a key.
//!
//! Skipped, loudly, when `codex` or `node` is not installed — the CI runners
//! have neither, and a skip is the honest result there.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use friring::sandbox::child_state::SeedPlan;
use friring::session::{AgentSandboxDef, ChildSeedAllow, ChildStateSeed, SeedMode};

/// The tool this file needs, or the reason it is being skipped.
fn tool(name: &str) -> Option<PathBuf> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!("codex_private_state: '{name}' is not installed; skipping");
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// The seed declaration the `omx` extension ships for its worker, cut down to
/// the two entries these gates are about.
fn omx_worker_agent() -> AgentSandboxDef {
    AgentSandboxDef {
        config_dir_env: Some("CODEX_HOME".to_string()),
        state_dir: Some("~/.codex".to_string()),
        credential_file: Some("~/.codex/auth.json".to_string()),
        writeback: true,
        child_state_seed: vec![
            ChildStateSeed {
                src: "auth.json".to_string(),
                mode: SeedMode::LinkRw,
                required: true,
            },
            ChildStateSeed {
                src: "config.toml".to_string(),
                mode: SeedMode::Copy,
                required: true,
            },
            ChildStateSeed {
                src: "hooks.json".to_string(),
                mode: SeedMode::CopyRewrite,
                required: true,
            },
            ChildStateSeed {
                src: "hooks".to_string(),
                mode: SeedMode::Copy,
                required: true,
            },
        ],
        ..AgentSandboxDef::default()
    }
}

/// What a profile authorizes, matching the declaration above entry for entry.
fn authorized() -> Vec<ChildSeedAllow> {
    vec![
        ChildSeedAllow {
            path: "auth.json".to_string(),
            mode: SeedMode::LinkRw,
        },
        ChildSeedAllow {
            path: "config.toml".to_string(),
            mode: SeedMode::Copy,
        },
        ChildSeedAllow {
            path: "hooks.json".to_string(),
            mode: SeedMode::CopyRewrite,
        },
        ChildSeedAllow {
            path: "hooks".to_string(),
            mode: SeedMode::Copy,
        },
    ]
}

/// A local model stub, listening on loopback, answering one turn.
struct Stub {
    child: std::process::Child,
    url: String,
}

impl Stub {
    fn start(root: &Path) -> Option<Self> {
        let node = tool("node")?;
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let script = repo.join("scripts/dev/agent-e2e/stub/openai-stub.mjs");
        if !script.exists() {
            eprintln!("codex_private_state: no model stub at {}", script.display());
            return None;
        }
        let fixtures = root.join("fixtures.json");
        std::fs::write(
            &fixtures,
            r#"{"responses":[{"name":"turn","reply":{"text":"PROBE-TURN-DONE"}}]}"#,
        )
        .unwrap();
        let port_file = root.join("port");
        let mut child = Command::new(node)
            .arg(&script)
            .args(["--port", "0"])
            .arg("--port-file")
            .arg(&port_file)
            .arg("--journal")
            .arg(root.join("journal.ndjson"))
            .arg("--fixtures")
            .arg(&fixtures)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        for _ in 0..100 {
            if let Ok(port) = std::fs::read_to_string(&port_file) {
                let port = port.trim().to_string();
                if !port.is_empty() {
                    return Some(Self {
                        child,
                        url: format!("http://127.0.0.1:{port}/v1"),
                    });
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // The process is running even though it never announced a port, and
        // `Child`'s own `Drop` neither kills nor reaps it — dropping here would
        // leave a node listening for the rest of the session.
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("codex_private_state: the model stub did not start");
        None
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The family `CODEX_HOME` a worker is seeded from: a hooks file whose command
/// names a script **inside** that directory, which is the only shape the
/// relocated-hooks gate is about.
fn family_home(root: &Path, stub_url: &str) -> PathBuf {
    // `.codex` under the fixture's own home, because that is what the agent
    // declares (`state_dir = "~/.codex"`) and `SeedPlan::build` resolves.
    let family = root.join(".codex");
    std::fs::create_dir_all(family.join("hooks")).unwrap();
    std::fs::write(
        family.join("config.toml"),
        format!(
            "model = \"probe-model\"\n\
             model_provider = \"stub\"\n\
             approval_policy = \"never\"\n\
             sandbox_mode = \"danger-full-access\"\n\
             check_for_update_on_startup = false\n\
             \n\
             [model_providers.stub]\n\
             name = \"Stub\"\n\
             base_url = \"{stub_url}\"\n\
             wire_api = \"responses\"\n"
        ),
    )
    .unwrap();
    // Not a real key, and never sent anywhere: the stub reads no auth header.
    std::fs::write(
        family.join("auth.json"),
        "{\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"probe-placeholder\"}",
    )
    .unwrap();

    let note = family.join("hooks/note.sh");
    std::fs::write(
        &note,
        "#!/bin/sh\nprintf 'fired\\n' >> \"$(dirname \"$0\")/fired.log\"\n",
    )
    .unwrap();
    make_executable(&note);
    std::fs::write(
        family.join("hooks.json"),
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"sh {} >/dev/null 2>&1 || true","timeout":10}}]}}]}}}}"#,
            note.display()
        ),
    )
    .unwrap();
    family
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run `codex` with every state root pointed inside the fixture.
fn codex_in(codex: &Path, home: &Path, codex_home: &Path, cwd: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new(codex);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("CODEX_HOME", codex_home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::null());
    let out = cmd.output().expect("run codex");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// **Ship gate one.** A `copy-rewrite`d `hooks.json` fires from the child's own
/// private `CODEX_HOME`, and the family's script does not run.
///
/// The seeding is friring's own `SeedPlan`, not a hand-rolled copy, so what is
/// observed is the code a launch runs.
#[test]
fn a_relocated_hooks_file_fires_from_the_childs_own_state_directory() {
    let (Some(codex), Some(_node)) = (tool("codex"), tool("node")) else {
        return;
    };
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    // Not a skip. The prerequisite checks above already covered "not installed",
    // so a stub that will not start is a missing script, a failed spawn or a
    // timeout — none of which may quietly turn this ship gate into a pass.
    let stub = Stub::start(root).expect("the model stub started; codex and node are installed");

    let family = family_home(root, &stub.url);
    let child_dir = root.join("child-codex");
    std::fs::create_dir_all(&child_dir).unwrap();
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // `home` is the fixture root, so the agent's `~/.codex` resolves to the
    // family directory above with no patching: what runs is exactly the plan a
    // launch would build.
    let plan = SeedPlan::build(
        &omx_worker_agent(),
        &authorized(),
        &child_dir,
        &root.display().to_string(),
        &|path| path.exists(),
    )
    .expect("the omx worker's seeds are a valid plan");
    assert_eq!(plan.source_dir, family);
    plan.apply().expect("the seeding is carried out");

    // The rewriting itself, before codex is asked anything: the child's hooks
    // file must name the child's script and nothing of the family's.
    let rewritten = std::fs::read_to_string(child_dir.join("hooks.json")).unwrap();
    assert!(
        rewritten.contains(&child_dir.display().to_string()),
        "the rewrite did not repoint the hook at the child's directory: {rewritten}"
    );
    assert!(
        !rewritten.contains(&family.display().to_string()),
        "the rewrite left the family's directory in the child's hooks: {rewritten}"
    );
    make_executable(&child_dir.join("hooks/note.sh"));

    let transcript = codex_in(
        &codex,
        root,
        &child_dir,
        &ws,
        &[
            "exec",
            "--skip-git-repo-check",
            "--dangerously-bypass-hook-trust",
            "--dangerously-bypass-approvals-and-sandbox",
            "say the marker",
        ],
    );
    assert!(
        transcript.contains("PROBE-TURN-DONE"),
        "the stubbed turn did not run, so nothing was observed:\n{transcript}"
    );

    // The gate. The child's own script ran; the family's did not.
    assert!(
        child_dir.join("hooks/fired.log").exists(),
        "codex did not fire a hook from the relocated CODEX_HOME:\n{transcript}"
    );
    assert!(
        !family.join("hooks/fired.log").exists(),
        "codex fired the *family's* hook script from a child's private state"
    );
}

/// **Ship gate two.** codex writes `auth.json` **in place**, so the `link-rw`
/// seed keeps one credential file rather than leaving a worker a private copy.
///
/// Observed by making codex write the file for real — `codex login
/// --with-api-key`, which needs no network — through a hard link, and checking
/// the inode, the link count and what the *family's* path reads back.
///
/// What this does **not** observe is an OAuth *refresh*: inducing one needs a
/// real credential and an expired token, neither of which this may hold. Both
/// writes go through codex's own auth-file writer, and the write path is what
/// ADR-28's condition is about — but the refresh itself stays an unobserved
/// gap, recorded in `docs/SANDBOX.md` rather than converted into a pass.
#[test]
fn codex_refreshes_the_credential_in_place_through_a_link_rw_seed() {
    let Some(codex) = tool("codex") else { return };
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    let family = root.join("family-codex");
    std::fs::create_dir_all(&family).unwrap();
    std::fs::write(
        family.join("auth.json"),
        "{\"auth_mode\":\"apikey\",\"OPENAI_API_KEY\":\"family-placeholder\"}",
    )
    .unwrap();
    let child_dir = root.join("child-codex");
    std::fs::create_dir_all(&child_dir).unwrap();
    // The `link-rw` seed, made the way `SeedPlan::apply` makes it.
    std::fs::hard_link(family.join("auth.json"), child_dir.join("auth.json")).unwrap();

    let inode = |path: &Path| {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(path).unwrap().ino()
    };
    let links = |path: &Path| {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(path).unwrap().nlink()
    };
    let before = inode(&family.join("auth.json"));
    assert_eq!(before, inode(&child_dir.join("auth.json")));
    assert_eq!(links(&family.join("auth.json")), 2);

    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let mut cmd = Command::new(&codex);
    cmd.args(["login", "--with-api-key"])
        .current_dir(&ws)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("CODEX_HOME", &child_dir)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("run codex login");
    {
        use std::io::Write as _;
        // A literal that is not a key, written to a file nothing sends.
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"probe-not-a-real-key")
            .unwrap();
    }
    let out = child.wait_with_output().expect("codex login finishes");
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "codex login refused, so nothing was observed:\n{transcript}"
    );

    // The gate: one file, not two. A writer that renamed a new file over the
    // name would leave the child a fresh inode and the family the stale one.
    assert_eq!(
        inode(&child_dir.join("auth.json")),
        before,
        "codex replaced the credential file rather than writing it in place, so a worker \
         would hold a private copy of a rotating token (ADR-28)"
    );
    assert_eq!(
        links(&family.join("auth.json")),
        2,
        "the link between the family's credential and the child's was broken by the write"
    );
    let seen = std::fs::read_to_string(family.join("auth.json")).unwrap();
    assert!(
        seen.contains("probe-not-a-real-key"),
        "the family's path does not read back what the child wrote: {seen}"
    );
}
