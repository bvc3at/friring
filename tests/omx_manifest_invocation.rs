//! What friring hands the `omx` extension's two agents, and what they hand on.
//!
//! The extension is bound to one oh-my-codex release through its argv contract,
//! and the contract has two halves that are checked in two different places:
//!
//! 1. **friring's half** — `AgentDef::build_args` emits the session-selection
//!    group *before* the static `args`, so a leader restart is `resume --last
//!    --direct`, which is the order the pinned `omx` accepts. That is asserted
//!    here, against the manifest as `ensure_agents_registered` parses it.
//! 2. **The wrapper's half** — the command friring runs is a one-line `sh`
//!    wrapper, so the program always receives its subcommand first and friring's
//!    argv after it. Asserted here too, by running each wrapper with a `node`
//!    shim first on `PATH` that records its argv.
//!
//! The program's own half — what it hands `omx` and `codex` — is
//! `node --test extensions/omx/lib`, behind `just omx-test`.

use std::path::{Path, PathBuf};

use friring::session::{AgentDef, ExtensionDef};

/// The manifest, parsed the way the installer parses it, with `{home}`
/// resolved to `home`.
fn manifest(home: &Path) -> ExtensionDef {
    let path = repo_root().join("extensions/omx/extension.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    // The same parser the installer uses, so an unknown key here fails the same
    // way it would there.
    let def: ExtensionDef =
        friring::agent::extension_config::parse_manifest_text(&text, "extension.toml")
            .map(|(def, _warnings)| def)
            .unwrap_or_else(|e| panic!("extensions/omx/{e}"));
    def.resolved_for_home(&home.display().to_string(), None)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn agent<'a>(def: &'a ExtensionDef, name: &str) -> &'a AgentDef {
    def.agents
        .iter()
        .find(|a| a.name == name)
        .unwrap_or_else(|| panic!("the manifest declares no agent '{name}'"))
}

/// Both agents' commands are the wrappers under `bin/`, with `{home}` resolved.
///
/// Never `node` directly: the wrapper is what guarantees the program receives
/// its subcommand first whatever friring emits.
#[test]
fn both_agents_run_the_wrapper_under_the_resolved_home() {
    let home = PathBuf::from("/opt/friring-omx");
    let def = manifest(&home);
    assert_eq!(
        agent(&def, "omx-leader").command,
        "/opt/friring-omx/bin/omx-leader"
    );
    assert_eq!(
        agent(&def, "omx-worker-codex").command,
        "/opt/friring-omx/bin/omx-worker-codex"
    );
    // The routing overlay is a path under the same home, and it fails closed:
    // OMX throws when the file named here is missing.
    let env = &agent(&def, "omx-leader")
        .sandbox
        .as_ref()
        .expect("the leader declares a sandbox block")
        .env;
    assert_eq!(
        env.get("OMX_RALPH_APPEND_INSTRUCTIONS_FILE")
            .map(String::as_str),
        Some("/opt/friring-omx/leader/APPENDIX.md")
    );
}

/// The argv friring emits, fresh and on a restart.
///
/// `resolveCliInvocation` in the pinned oh-my-codex reads its **first**
/// argument: a leading `--flag` is a launch and a leading `resume` is a resume.
/// Both forms below are already in that order, which is the whole reason the
/// program passes them through rather than reordering.
#[test]
fn friring_emits_the_argv_the_pinned_omx_accepts() {
    let def = manifest(Path::new("/opt/friring-omx"));
    let leader = agent(&def, "omx-leader");
    assert_eq!(leader.build_args(None, None, None, None), ["--direct"]);
    assert_eq!(
        leader.build_args(Some("ignored"), None, None, None),
        ["resume", "--last", "--direct"]
    );
    assert!(
        leader.resumes_latest(),
        "a leader resumes the last conversation in its worktree"
    );

    let worker = agent(&def, "omx-worker-codex");
    assert!(worker.build_args(None, None, None, None).is_empty());
    assert_eq!(
        worker.build_args(Some("ignored"), None, None, None),
        ["resume", "--last"]
    );
    // Unambiguous only because a worker's `CODEX_HOME` is private: the last
    // conversation in it is its own, and there is no other.
    assert!(worker.resumes_latest());
    assert_eq!(
        worker.sandbox.as_ref().unwrap().config_dir_env.as_deref(),
        Some("CODEX_HOME")
    );
}

/// The manifest declares what each agent needs from the bridge, and what a
/// worker needs seeded — the two halves ADR-31 keeps separate.
#[test]
fn the_manifest_declares_its_bridge_needs_and_its_seeds() {
    use friring::session::{BridgeCapability, SeedMode};

    let def = manifest(Path::new("/opt/friring-omx"));
    let leader = agent(&def, "omx-leader").sandbox.as_ref().unwrap();
    assert_eq!(
        leader.bridge_requires,
        [BridgeCapability::ChildLifecycle, BridgeCapability::Mailbox]
    );
    assert!(
        leader.child_state_seed.is_empty(),
        "a leader is nobody's child and needs no seed"
    );

    let worker = agent(&def, "omx-worker-codex").sandbox.as_ref().unwrap();
    assert_eq!(
        worker.bridge_requires,
        [BridgeCapability::Mailbox, BridgeCapability::Report],
        "a worker never asks for child-lifecycle: orchestration is one level deep"
    );
    let seeds: Vec<(&str, SeedMode)> = worker
        .child_state_seed
        .iter()
        .map(|s| (s.src.as_str(), s.mode))
        .collect();
    assert_eq!(
        seeds,
        [
            ("auth.json", SeedMode::LinkRw),
            ("config.toml", SeedMode::Copy),
            ("hooks.json", SeedMode::CopyRewrite),
            ("skills", SeedMode::Symlink),
            ("prompts", SeedMode::Symlink),
            ("AGENTS.md", SeedMode::Symlink),
        ]
    );
    // `link-rw` is the one mode that writes outside the child's own tree, and it
    // is legal only because the block declares that exact file as the
    // credential under `state_dir` with `writeback` (ADR-28).
    assert_eq!(
        worker.credential_file.as_deref(),
        Some("~/.codex/auth.json")
    );
    assert_eq!(worker.state_dir.as_deref(), Some("~/.codex"));
    assert!(worker.writeback);

    // A worker's transcripts are its own: the family's are in `state_rw`, which
    // puts them in every child's subtract set (ADR-31).
    for private in [
        "~/.codex/sessions",
        "~/.codex/history.jsonl",
        "~/.codex/log",
    ] {
        assert!(
            worker.state_rw.iter().any(|p| p == private),
            "{private} must be in state_rw so a child's subtract set denies it"
        );
    }
    assert!(
        !worker.state_rw.iter().any(|p| p == "~/.codex"),
        "never the whole state directory"
    );
}

/// The profile template authorizes exactly the seeds the worker declares, in
/// exactly those modes.
///
/// The agent declares and the **profile authorizes**: a mismatch in either
/// direction refuses every launch with `state_unrelocatable`, so the two lists
/// have to agree and this is where that is checked once.
#[test]
fn the_profile_template_authorizes_exactly_what_the_worker_declares() {
    let def = manifest(Path::new("/opt/friring-omx"));
    let worker = agent(&def, "omx-worker-codex").sandbox.as_ref().unwrap();

    let text = std::fs::read_to_string(repo_root().join("extensions/omx/profiles.toml"))
        .expect("the profile template");
    for seed in &worker.child_state_seed {
        let entry = format!("{{ path = \"{}\", mode = \"{}\" }}", seed.src, seed.mode);
        assert!(
            text.contains(&entry),
            "the profile template must authorize {entry}"
        );
    }
    // And nothing beyond them: a template that authorized more would hand every
    // worker a surface no agent asked for.
    let seed_block = text
        .split_once("child_seed_allow = [")
        .and_then(|(_, rest)| rest.split_once("\n]"))
        .map(|(block, _)| block)
        .expect("the template declares child_seed_allow");
    assert_eq!(
        seed_block.matches("{ path = ").count(),
        worker.child_state_seed.len(),
        "the template authorizes exactly the declared seeds"
    );
    // The three fields a leader needs, and the one that keeps a worker from
    // becoming one.
    assert!(text.contains("child_agents = [\"omx-worker-codex\"]"));
    assert!(text.contains("bridge_grants = [\"child-lifecycle\", \"mailbox\", \"report\"]"));
    assert!(text.contains("read_scope = \"workspace\""));
}

/// Every payload the manifest writes outside its own home carries the managed
/// marker.
///
/// `is_user_modified` decides ownership by searching a file's content for it,
/// and these destinations declare `on_conflict = "refuse"`. Without the marker
/// the first install writes the file and every install, update and self-heal
/// after it refuses — while uninstall, which prunes only marker-managed files,
/// leaves it behind.
#[test]
fn every_external_payload_carries_the_managed_marker() {
    const MARKER: &str = "friring `extension install`";
    let def = manifest(&PathBuf::from("/opt/friring-omx"));
    assert!(
        !def.external_files.is_empty(),
        "the manifest writes outside its home, so there is something to check"
    );
    for external in &def.external_files {
        // The installer's own default: a `source` omitted means the
        // destination's file name.
        let relative = external.source.clone().unwrap_or_else(|| {
            Path::new(&external.path)
                .file_name()
                .expect("an external file destination names a file")
                .to_string_lossy()
                .into_owned()
        });
        let path = repo_root().join("extensions/omx").join(&relative);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert!(
            text.contains(MARKER),
            "{} carries no managed marker, so a reinstall would refuse it",
            path.display()
        );
    }
}

/// The wrapper reaches the program with its subcommand first and friring's argv
/// after it, on both a fresh launch and a restart.
///
/// Run for real, with a `node` shim first on `PATH` that records what it was
/// asked to run. Asserting on the wrapper's *text* would prove the file says the
/// right thing; this proves `sh` does the right thing with it.
#[cfg(unix)]
#[test]
fn each_wrapper_reaches_the_program_with_its_subcommand_first() {
    let temp = tempfile::tempdir().expect("a temp dir");
    let shim_dir = temp.path().join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let record = temp.path().join("argv.txt");
    let shim = shim_dir.join("node");
    std::fs::write(
        &shim,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n", record.display()),
    )
    .unwrap();
    set_executable(&shim);

    let home = repo_root().join("extensions/omx");
    for (wrapper, subcommand, argv) in [
        ("omx-leader", "leader", vec!["--direct"]),
        ("omx-leader", "leader", vec!["resume", "--last", "--direct"]),
        ("omx-worker-codex", "worker", vec![]),
        ("omx-worker-codex", "worker", vec!["resume", "--last"]),
    ] {
        let status = std::process::Command::new(home.join("bin").join(wrapper))
            .args(&argv)
            // The shim first, the real PATH after it: the wrapper is `sh`, and
            // stripping its environment would test something else.
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    shim_dir.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .status()
            .unwrap_or_else(|e| panic!("run {wrapper}: {e}"));
        assert!(status.success(), "{wrapper} {argv:?} exited {status}");

        let recorded: Vec<String> = std::fs::read_to_string(&record)
            .expect("the shim recorded an argv")
            .lines()
            .map(str::to_string)
            .collect();
        // The program is resolved **relative to the wrapper**, so the extension
        // works wherever its home is — and the path is absolute, so it is not
        // resolved against a working directory friring chose.
        assert_eq!(
            recorded[0],
            format!("{}/../lib/friring-omx.mjs", home.join("bin").display()),
            "the wrapper runs the program beside it: {recorded:?}"
        );
        assert_eq!(
            recorded[1], subcommand,
            "the subcommand comes first: {recorded:?}"
        );
        assert_eq!(
            &recorded[2..],
            argv.as_slice(),
            "friring's argv follows, intact and in order"
        );
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// The conformance extension parses too, and declares only what a bridge
/// contract demonstration needs.
///
/// It exists so the bridge can be proven with **no vendor agent involved**:
/// both its agents are `/bin/sh` scripts, so a green run demonstrates the
/// verbs, the authority rules and the quiesce protocol rather than an
/// integration with anything.
#[test]
fn the_conformance_extension_declares_only_the_bridge() {
    let path = repo_root().join("extensions/bridge-conformance/extension.toml");
    let text = std::fs::read_to_string(&path).expect("the conformance manifest");
    let def: ExtensionDef =
        friring::agent::extension_config::parse_manifest_text(&text, "extension.toml")
            .map(|(def, _)| def)
            .unwrap_or_else(|e| panic!("extensions/bridge-conformance/{e}"))
            .resolved_for_home("/opt/conformance", None);

    use friring::session::BridgeCapability;
    let leader = agent(&def, "conformance-leader");
    assert_eq!(leader.command, "/opt/conformance/bin/leader.sh");
    assert_eq!(
        leader.sandbox.as_ref().unwrap().bridge_requires,
        [
            BridgeCapability::ChildLifecycle,
            BridgeCapability::Mailbox,
            BridgeCapability::Report
        ]
    );
    let worker = agent(&def, "conformance-worker");
    assert_eq!(
        worker.sandbox.as_ref().unwrap().bridge_requires,
        [BridgeCapability::Mailbox, BridgeCapability::Report]
    );
    // No credential, no login, no network: the point is that nothing here is a
    // vendor integration, so what a run proves is the bridge.
    assert!(worker.sandbox.as_ref().unwrap().credential_file.is_none());
    assert!(
        def.external_files.is_empty(),
        "it writes nothing outside its home"
    );
    assert!(def.agent_patches.is_empty());
    assert!(def.config_merges.is_empty());
    // It gates on the two capabilities an extension author has to be able to
    // state, and on nothing that would tie it to a machine.
    assert_eq!(def.requires.len(), 2);
    for requirement in &def.requires {
        assert!(
            matches!(
                requirement,
                friring::session::Requirement::BinaryCapability { .. }
            ),
            "{requirement:?}"
        );
    }

    let profile =
        std::fs::read_to_string(repo_root().join("extensions/bridge-conformance/profiles.toml"))
            .expect("the conformance profile");
    assert!(
        profile.contains("network_mode = \"none\""),
        "the run is offline"
    );
    assert!(profile.contains("child_agents = [\"conformance-worker\"]"));
}
