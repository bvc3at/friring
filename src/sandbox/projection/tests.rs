//! Everything here fabricates its own home under the test temporary directory
//! and populates it with synthetic configuration and fake tokens. No test reads
//! the machine owner's real agent state, opens a credential store or a keychain,
//! or makes a network request — `docs/SANDBOX.md` §Testing and privacy is a
//! constraint on the tests as much as on the feature.

use std::path::{Path, PathBuf};

use super::*;
use crate::session::{AgentSandboxDef, EnforcedSettings, SandboxAuth, SettingsFormat};
/// Read only by the cases below that project a place's hooks. Those — and every
/// case that classifies this fabricated *unix* home — are unix-only, because
/// what they feed is a place's synthetic home and a native Windows host has no
/// place at all (`crate::sandbox::select::NATIVE_WINDOWS`).
#[cfg(unix)]
use crate::session::{REMOTE_HOOK_STATE_OPTION, STATUS_SIGNAL_MARKER};

/// `$HOME` inside the place, matching what the container backend mounts the
/// synthetic home at.
const INSIDE: &str = "/home/agent";

/// A fabricated host: a home with synthetic agent configuration in it, a friring
/// config directory, a place to project into, and a data directory the
/// projection must never reach.
struct Host {
    _root: tempfile::TempDir,
    home: String,
    place_home: PathBuf,
    managed_root: String,
    db: String,
    granted: Vec<String>,
}

impl Host {
    fn new() -> Self {
        // `/tmp` is where tmux keeps its sockets on Linux, and the platform
        // temporary directory is under it there — so a fabricated home would be
        // classified host-only for a reason none of these tests is about.
        // Pointing the socket root at a path nothing else uses keeps the
        // fixtures classifiable; `tmux_socket_root` reads it on every call.
        std::env::set_var("TMUX_TMPDIR", "/friring-projection-tests-sockets");
        let root = tempfile::tempdir().expect("a temporary directory");
        let home = root.path().join("home");
        let place_home = root.path().join("place/home");
        let managed_root = home.join(".config/friring");
        for dir in [&home, &place_home, &managed_root] {
            std::fs::create_dir_all(dir).expect("a fabricated directory");
        }
        Self {
            home: home.display().to_string(),
            place_home,
            managed_root: managed_root.display().to_string(),
            db: home
                .join(".local/share/friring/friring.db")
                .display()
                .to_string(),
            granted: vec![root.path().join("repo").display().to_string()],
            _root: root,
        }
    }

    fn path(&self, home_relative: &str) -> PathBuf {
        Path::new(&self.home).join(home_relative)
    }

    /// Write a synthetic file into the fabricated home.
    fn write(&self, home_relative: &str, contents: &str) -> PathBuf {
        let path = self.path(home_relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("a fabricated directory");
        std::fs::write(&path, contents).expect("a fabricated file");
        path
    }

    #[cfg(unix)]
    fn dir(&self, home_relative: &str) -> PathBuf {
        let path = self.path(home_relative);
        std::fs::create_dir_all(&path).expect("a fabricated directory");
        path
    }

    fn input<'a>(
        &'a self,
        agent: &'a AgentSandboxDef,
        managed: &'a [String],
    ) -> ProjectionInput<'a> {
        ProjectionInput {
            profile: "dev",
            agent: Some(agent),
            granted: &self.granted,
            home: &self.home,
            inside_home: INSIDE,
            platform: SecretPlatform::Linux,
            friring_db: Some(&self.db),
            managed_root: &self.managed_root,
            managed,
        }
    }

    /// What landed inside the place, as text.
    fn projected(&self, home_relative: &str) -> Option<String> {
        std::fs::read_to_string(self.place_home.join(home_relative)).ok()
    }
}

fn agent(copy_in: &[&str]) -> AgentSandboxDef {
    AgentSandboxDef {
        auth: SandboxAuth::VolumeLogin,
        copy_in: copy_in.iter().map(|entry| (*entry).to_string()).collect(),
        credential_file: Some("~/.claude/.credentials.json".into()),
        ..Default::default()
    }
}

fn finding<'p>(projection: &'p ProjectionPlan, needle: &str) -> &'p Finding {
    projection
        .findings
        .iter()
        .find(|f| f.entry.contains(needle))
        .unwrap_or_else(|| {
            panic!(
                "nothing classified for {needle}: {:?}",
                projection
                    .findings
                    .iter()
                    .map(|f| &f.entry)
                    .collect::<Vec<_>>()
            )
        })
}

// ---- The safe subset ----------------------------------------------------

/// The inert content — instructions, skills, commands — is what projection is
/// *for*: without it a place's agent starts with none of the user's setup.
#[test]
fn the_inert_subset_lands_in_the_synthetic_home() {
    let host = Host::new();
    host.write(".claude/CLAUDE.md", "# house style\nprefer small diffs\n");
    host.write(".claude/skills/review/SKILL.md", "review skill\n");
    host.write(".claude/commands/ship.md", "ship it\n");
    host.write(".claude/agents/critic.md", "be critical\n");

    let agent = agent(&[
        "~/.claude/CLAUDE.md",
        "~/.claude/skills",
        "~/.claude/commands",
    ]);
    let projection = plan(&host.input(&agent, &[]));
    assert_eq!(projection.apply(&host.place_home).unwrap(), 3);

    assert_eq!(
        host.projected(".claude/CLAUDE.md").as_deref(),
        Some("# house style\nprefer small diffs\n")
    );
    assert_eq!(
        host.projected(".claude/skills/review/SKILL.md").as_deref(),
        Some("review skill\n")
    );
    assert_eq!(
        host.projected(".claude/commands/ship.md").as_deref(),
        Some("ship it\n")
    );
    // Only what was declared: the agents directory was not, so it stays home.
    assert!(host.projected(".claude/agents/critic.md").is_none());
    assert!(projection
        .summary()
        .starts_with("config projected: 3 files"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(host.place_home.join(".claude/CLAUDE.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "projected content is friring's own");
    }

    // Idempotent, and it does not remove what the agent itself wrote beside it
    // — the same tree holds the login a place signs into once.
    std::fs::write(host.place_home.join(".claude/.credentials.json"), "{}").unwrap();
    assert_eq!(projection.apply(&host.place_home).unwrap(), 3);
    assert!(host.place_home.join(".claude/.credentials.json").exists());
}

/// An agent that declares nothing is projected nothing, and says so rather than
/// looking like a projection that silently found nothing.
#[test]
fn an_agent_that_declares_nothing_gets_nothing_and_says_so() {
    let host = Host::new();
    let silent = AgentSandboxDef::default();
    let projection = plan(&host.input(&silent, &[]));
    assert_eq!(projection.file_count(), 0);
    assert!(projection.findings.is_empty());
    assert_eq!(
        projection.summary(),
        "no config projected — this agent declares none"
    );

    // …and neither does a launch with no declaration at all.
    let mut input = host.input(&silent, &[]);
    input.agent = None;
    assert_eq!(plan(&input), ProjectionPlan::default());
}

// ---- Credentials (ADR-28) ----------------------------------------------

/// ADR-28 is absolute, and it covers the launching agent's *own* credential: a
/// rotating refresh token with two consumers invalidates itself on the first
/// refresh, which logs the user out of the real installation.
#[test]
fn a_credential_file_never_crosses() {
    let host = Host::new();
    host.write(".claude/CLAUDE.md", "content\n");
    host.write(".claude/.credentials.json", "{\"fake\":\"token-not-real\"}");
    host.write(".claude/skills/notes/api_token.txt", "fake-token-value");
    host.write(".ssh/id_ed25519", "fake key material");
    host.write(".codex/auth.json", "{\"fake\":\"other agent\"}");

    let agent = agent(&["~/.claude", "~/.ssh", "~/.codex"]);
    let projection = plan(&host.input(&agent, &[]));
    projection.apply(&host.place_home).unwrap();

    for never in [
        ".claude/.credentials.json",
        ".claude/skills/notes/api_token.txt",
        ".ssh/id_ed25519",
        ".codex/auth.json",
    ] {
        assert!(host.projected(never).is_none(), "{never} crossed");
    }
    // …and the inert file beside it still did.
    assert_eq!(
        host.projected(".claude/CLAUDE.md").as_deref(),
        Some("content\n")
    );

    assert!(finding(&projection, ".credentials.json")
        .reason
        .contains("ADR-28"));
    assert_eq!(
        finding(&projection, "api_token.txt").verdict,
        Verdict::HostOnly
    );
    assert!(finding(&projection, ".ssh").reason.contains("private keys"));
    // Nothing that crossed carries the fake token's text either — a credential
    // quoted inside a settings file would be the same disclosure.
    for file in &projection.files {
        let text = String::from_utf8_lossy(&file.contents);
        assert!(!text.contains("token-not-real"), "{}: {text}", file.rel);
    }
}

// ---- The lint pass ------------------------------------------------------

/// A settings document carrying every shape that names the host, with `HOME` and
/// `REPO` standing in for the fabricated paths.
#[cfg(unix)]
const SETTINGS: &str = r#"{
  "statusLine": { "type": "command", "command": "HOME/bin/status.sh" },
  "mcpServers": {
    "docs": { "command": "/opt/vendor/bin/uvx", "args": ["docs-mcp"] },
    "local": { "command": "npx", "args": ["-y", "server"] }
  },
  "hooks": {
    "PreToolUse": [
      { "matcher": "", "hooks": [
        { "type": "command", "command": "HOME/audit/run.sh --strict" },
        { "type": "command", "command": "printf working" }
      ] }
    ]
  },
  "plugins": { "repositories": ["HOME/plugins", "REPO/vendor-plugins"] },
  "model": "opus"
}"#;

/// The three awkward shapes, in one document: a hook naming a host script, an
/// MCP server naming a host binary, and a plugin directory outside the config
/// directory.
#[cfg(unix)]
#[test]
fn every_host_reference_is_classified_with_a_reason() {
    let host = Host::new();
    // A host script and a host plugin directory that really are on this host, so
    // the verdicts are about *where* they are rather than about existence.
    host.write("audit/run.sh", "#!/bin/sh\necho audit\n");
    host.write("bin/status.sh", "#!/bin/sh\necho status\n");
    host.dir("plugins");
    host.write(
        ".claude/settings.json",
        &SETTINGS
            .replace("HOME", &host.home)
            .replace("REPO", &host.granted[0]),
    );

    let agent = agent(&["~/.claude"]);
    let projection = plan(&host.input(&agent, &[]));
    projection.apply(&host.place_home).unwrap();

    // A hook naming a host script: no grant makes a host binary runnable in a
    // place, so it is host-only and the reason says why.
    let hook = finding(&projection, "hooks.PreToolUse[0].hooks[0]");
    assert_eq!(hook.verdict, Verdict::HostOnly);
    assert!(hook.reason.contains("image's binaries"), "{}", hook.reason);
    // An MCP server naming a host binary: the whole server entry goes.
    let mcp = finding(&projection, "mcpServers.docs");
    assert_eq!(mcp.verdict, Verdict::HostOnly);
    // A status-line command, same shape, one level up.
    assert_eq!(
        finding(&projection, "statusLine").verdict,
        Verdict::HostOnly
    );
    // A plugin *directory* outside the profile's paths is the one a read-only
    // grant would fix, and the finding says exactly that.
    let plugins = finding(&projection, "plugins.repositories[0]");
    assert_eq!(
        plugins.verdict,
        Verdict::NeedsMount {
            host_path: host.path("plugins").display().to_string()
        }
    );
    assert!(plugins.reason.contains("read-only"), "{}", plugins.reason);

    let text = host.projected(".claude/settings.json").unwrap();
    let projected: serde_json::Value = serde_json::from_str(&text).unwrap();
    // What could not cross is gone as a whole entry, so the document is still
    // one the agent can read…
    assert!(projected.get("statusLine").is_none(), "{projected}");
    assert!(projected["mcpServers"].get("docs").is_none(), "{projected}");
    assert_eq!(
        projected["hooks"]["PreToolUse"][0]["hooks"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        projected["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "printf working"
    );
    // …and everything that crosses on its own merits is untouched, including a
    // reference into a path the profile granted, which a place mounts at exactly
    // its host path.
    assert_eq!(projected["mcpServers"]["local"]["command"], "npx");
    assert_eq!(projected["model"], "opus");
    assert_eq!(
        projected["plugins"]["repositories"],
        serde_json::json!([format!("{}/vendor-plugins", host.granted[0])])
    );
    // Nothing that crossed still names a host path outside the boundary.
    assert!(!text.contains("/opt/vendor/bin/uvx"), "{text}");
    assert!(
        !text.contains(&host.path("bin/status.sh").display().to_string()),
        "{text}"
    );
    assert!(
        projection.summary().contains("need a read-only mount"),
        "{}",
        projection.summary()
    );
    assert!(projection.actionable().count() >= 4);
}

/// A reference to something that *is* projected is rewritten rather than
/// dropped: it exists inside the boundary, just at the synthetic home's path.
#[cfg(unix)]
#[test]
fn a_reference_to_projected_content_is_rewritten_to_where_it_lands() {
    let host = Host::new();
    host.write(".claude/hooks/pre.sh", "#!/bin/sh\necho pre\n");
    host.write(
        ".claude/settings.json",
        &format!(
            r#"{{ "hooks": {{ "PreToolUse": [ {{ "matcher": "", "hooks": [
                 {{ "type": "command", "command": "{}/.claude/hooks/pre.sh" }},
                 {{ "type": "command", "command": "~/.claude/hooks/pre.sh" }} ] }} ] }} }}"#,
            host.home
        ),
    );

    let agent = agent(&["~/.claude"]);
    let projection = plan(&host.input(&agent, &[]));
    projection.apply(&host.place_home).unwrap();

    let rewritten = finding(&projection, "hooks.PreToolUse[0].hooks[0]");
    assert_eq!(
        rewritten.verdict,
        Verdict::Rewritten {
            inside: format!("{INSIDE}/.claude/hooks/pre.sh")
        }
    );
    let projected: serde_json::Value =
        serde_json::from_str(&host.projected(".claude/settings.json").unwrap()).unwrap();
    let hooks = &projected["hooks"]["PreToolUse"][0]["hooks"];
    assert_eq!(hooks.as_array().map(Vec::len), Some(2), "{projected}");
    assert_eq!(
        hooks[0]["command"],
        format!("{INSIDE}/.claude/hooks/pre.sh")
    );
    // A `~`-anchored reference already means "the home the agent has", which
    // inside a place is the synthetic one — so it survives untouched.
    assert_eq!(hooks[1]["command"], "~/.claude/hooks/pre.sh");

    // The script it names crossed too, and kept its execute bit: a hook pointing
    // at a file that is there but not runnable is the same broken agent.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let source = host.path(".claude/hooks/pre.sh");
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        let projection = plan(&host.input(&agent, &[]));
        projection.apply(&host.place_home).unwrap();
        let mode = std::fs::metadata(host.place_home.join(".claude/hooks/pre.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}

/// Content is content: a markdown file that mentions a path is prose, and
/// friring rewriting the user's words would be worse than useless.
#[test]
fn instructions_are_copied_verbatim_rather_than_linted() {
    let host = Host::new();
    let prose = "Run `/opt/vendor/bin/uvx` from ~/plugins when reviewing.\n";
    host.write(".claude/CLAUDE.md", prose);
    let agent = agent(&["~/.claude"]);
    plan(&host.input(&agent, &[]))
        .apply(&host.place_home)
        .unwrap();
    assert_eq!(host.projected(".claude/CLAUDE.md").as_deref(), Some(prose));
}

// ---- Enforced settings --------------------------------------------------

#[cfg(unix)]
#[test]
fn enforced_settings_are_frirings_last_word_over_what_was_projected() {
    let host = Host::new();
    host.write(
        ".claude/settings.json",
        "{\"hasTrustDialogAccepted\": false, \"model\": \"opus\"}",
    );
    let mut agent = agent(&["~/.claude"]);
    agent.enforced = vec![EnforcedSettings {
        path: "~/.claude/settings.json".into(),
        format: SettingsFormat::Json,
        content: Some("{ \"hasTrustDialogAccepted\": true, \"trusted\": {workspaces} }".into()),
        per_path: None,
    }];

    let projection = plan(&host.input(&agent, &[]));
    projection.apply(&host.place_home).unwrap();
    let projected: serde_json::Value =
        serde_json::from_str(&host.projected(".claude/settings.json").unwrap()).unwrap();

    // friring's key wins, the user's other keys survive, and the paths the
    // boundary granted are pre-trusted so a fresh home does not prompt.
    assert_eq!(projected["hasTrustDialogAccepted"], true);
    assert_eq!(projected["model"], "opus");
    assert_eq!(projected["trusted"], serde_json::json!([host.granted[0]]));
    assert!(finding(&projection, "(enforced)")
        .reason
        .contains("pre-trusted"));
}

#[test]
fn a_per_path_layer_seeds_one_table_per_granted_path() {
    let host = Host::new();
    let mut agent = agent(&[]);
    agent.enforced = vec![EnforcedSettings {
        path: "~/.codex/config.toml".into(),
        format: SettingsFormat::Toml,
        content: None,
        per_path: Some("[projects.\"{path}\"]\ntrust_level = \"trusted\"".into()),
    }];

    plan(&host.input(&agent, &[]))
        .apply(&host.place_home)
        .unwrap();
    let projected: toml::Value =
        toml::from_str(&host.projected(".codex/config.toml").unwrap()).unwrap();
    assert_eq!(
        projected["projects"][&host.granted[0]]["trust_level"].as_str(),
        Some("trusted")
    );
}

/// A registry typo writes nothing rather than a document the agent then fails to
/// parse on startup — and the launch still happens, because enforced settings
/// are a layer, not the boundary.
#[test]
fn an_enforced_template_that_will_not_render_is_reported_not_written() {
    let host = Host::new();
    let mut agent = agent(&[]);
    agent.enforced = vec![
        EnforcedSettings {
            path: "~/.claude/settings.json".into(),
            format: SettingsFormat::Json,
            content: Some("{ this is not json".into()),
            per_path: None,
        },
        EnforcedSettings {
            path: "/etc/agent/managed.json".into(),
            format: SettingsFormat::Json,
            content: Some("{}".into()),
            per_path: None,
        },
    ];

    let projection = plan(&host.input(&agent, &[]));
    assert_eq!(projection.file_count(), 0);
    assert!(host.projected(".claude/settings.json").is_none());
    assert!(projection
        .findings
        .iter()
        .any(|f| f.reason.contains("does not render a json document")));
    assert!(projection
        .findings
        .iter()
        .any(|f| f.reason.contains("synthetic home")));
}

/// friring's own layer is held to the rule it applies to everybody else's: a
/// host path in it would be friring writing the very reference the lint pass
/// strips out of the user's file.
#[cfg(unix)]
#[test]
fn an_enforced_template_naming_a_host_path_is_refused() {
    let host = Host::new();
    let mut agent = agent(&[]);
    agent.enforced = vec![EnforcedSettings {
        path: "~/.claude/settings.json".into(),
        format: SettingsFormat::Json,
        content: Some(format!(
            "{{ \"statusLine\": {{ \"command\": \"{}/bin/x.sh\" }} }}",
            host.home
        )),
        per_path: None,
    }];
    let projection = plan(&host.input(&agent, &[]));
    assert_eq!(projection.file_count(), 0);
    assert!(finding(&projection, "(enforced)")
        .reason
        .contains("names a path the boundary does not reach"));
}

// ---- Status signals -----------------------------------------------------

/// The reason a place-backed session reported no status at all: friring's own
/// hook configuration was dropped from the launch rather than carried in.
#[cfg(unix)]
#[test]
fn frirings_hooks_cross_and_report_through_a_tmux_pane_option() {
    let host = Host::new();
    let payload = format!(
        "{{\"hooks\":{{\"Stop\":[{{\"hooks\":[{{\"type\":\"command\",\"command\":\"if [ -n \
         \\\"$FRIRING_SIGNAL_FILE\\\" ]; then printf 'done' >> \\\"$FRIRING_SIGNAL_FILE\\\" || \
         true; else {STATUS_SIGNAL_MARKER}done || true; fi\"}}]}}]}}}}"
    );
    let settings = host.write(".config/friring/hooks/claude.json", &payload);
    let managed = vec![settings.display().to_string()];

    let agent = agent(&[]);
    let projection = plan(&host.input(&agent, &managed));
    projection.apply(&host.place_home).unwrap();

    let inside = format!("{INSIDE}/.config/friring/hooks/claude.json");
    assert_eq!(projection.inside_path(&managed[0]), Some(inside.as_str()));
    let projected = host.projected(".config/friring/hooks/claude.json").unwrap();
    assert!(
        projected.contains(&format!(
            "tmux set-option -p {REMOTE_HOOK_STATE_OPTION} done"
        )),
        "{projected}"
    );
    assert!(
        !projected.contains("friring-cli session signal"),
        "{projected}"
    );
    // The shell around the command survives, so the unsandboxed branch is still
    // a branch and the hook is still one command.
    assert!(projected.contains("fi"), "{projected}");
    assert!(projection.reports_status());
    assert!(!projection.summary().contains("reports no state"));

    // …and a launch that carries no hook payload says so, because a session that
    // silently never leaves `idle` looks like a broken session.
    let silent = plan(&host.input(&agent, &[]));
    assert!(!silent.reports_status());
}

/// friring's own file is followed through the per-session symlink the local
/// claude launch points at — and only as far as friring's own config root.
#[test]
#[cfg(unix)]
fn a_managed_path_is_followed_only_inside_frirings_own_config_root() {
    let host = Host::new();
    host.write(".config/friring/hooks/claude.json", "{\"hooks\":{}}");
    let per_session = host.path(".config/friring/hooks/sessions/s1.json");
    std::fs::create_dir_all(per_session.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("../claude.json", &per_session).unwrap();

    let elsewhere = host.write("outside/settings.json", "{\"secret\": \"not-frirings\"}");
    let escaping = host.path(".config/friring/hooks/escape.json");
    std::os::unix::fs::symlink(&elsewhere, &escaping).unwrap();

    let agent = agent(&[]);
    let managed = vec![
        per_session.display().to_string(),
        escaping.display().to_string(),
    ];
    let projection = plan(&host.input(&agent, &managed));
    projection.apply(&host.place_home).unwrap();

    assert!(projection.inside_path(&managed[0]).is_some());
    assert_eq!(
        host.projected(".config/friring/hooks/sessions/s1.json")
            .as_deref(),
        Some("{\n  \"hooks\": {}\n}\n")
    );
    // The link out of friring's own tree is refused, and nothing it pointed at
    // crossed.
    assert_eq!(projection.inside_path(&managed[1]), None);
    assert!(finding(&projection, "escape.json")
        .reason
        .contains("does not resolve inside friring's own configuration directory"));
    for file in &projection.files {
        assert!(!String::from_utf8_lossy(&file.contents).contains("not-frirings"));
    }
}

// ---- ADR-29 -------------------------------------------------------------

/// The database never enters a sandbox, and projection is a shape that could
/// carry it in three different ways: as a declared entry, through a symlink, and
/// as a reference inside a document friring copies.
#[cfg(unix)]
#[test]
fn no_projection_shape_reaches_the_data_directory() {
    let host = Host::new();
    let data = host.path(".local/share/friring");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("friring.db"), "SQLite format 3\0not-a-real-db").unwrap();
    std::fs::write(data.join("friring.db-wal"), "wal").unwrap();

    host.write(".claude/CLAUDE.md", "content\n");
    host.write(
        ".claude/settings.json",
        &format!(
            "{{\"hooks\":{{\"Stop\":[{{\"hooks\":[{{\"type\":\"command\",\"command\":\"sqlite3 \
             {}/friring.db 'select 1'\"}}]}}]}},\"model\":\"opus\"}}",
            data.display()
        ),
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink(&data, host.path(".claude/state")).unwrap();

    let agent = agent(&["~/.claude", "~/.local/share/friring", "~/.local/share"]);
    let projection = plan(&host.input(&agent, &[]));
    projection.apply(&host.place_home).unwrap();

    // A declared entry that is (or encloses) the data directory is refused, and
    // the reason names the ADR rather than a filesystem error.
    for entry in ["~/.local/share/friring", "~/.local/share"] {
        let refused = finding(&projection, entry);
        assert_eq!(refused.verdict, Verdict::HostOnly, "{entry}");
        assert!(
            refused.reason.contains("ADR-29"),
            "{entry}: {}",
            refused.reason
        );
    }
    // A hook that would reach it is dropped, with the same reason.
    let hook = finding(&projection, "settings.json → hooks");
    assert_eq!(hook.verdict, Verdict::HostOnly);
    assert!(hook.reason.contains("ADR-29"), "{}", hook.reason);

    // The sweep: nothing this plan would write is *under* the data directory,
    // came *from* it, or names it.
    let protected = [
        data.display().to_string(),
        data.join("friring.db").display().to_string(),
        data.join("friring.db-wal").display().to_string(),
    ];
    for file in &projection.files {
        let landed = host.place_home.join(&file.rel).display().to_string();
        let text = String::from_utf8_lossy(&file.contents);
        for path in &protected {
            assert!(
                !dirs::encloses(path, &landed),
                "{} landed under {path}",
                file.rel
            );
            assert!(!text.contains(path.as_str()), "{} names {path}", file.rel);
        }
        assert!(
            !text.contains("SQLite format 3"),
            "{} carries the database",
            file.rel
        );
    }
    // Projection creates no mount of any kind: `NeedsMount` is a sentence for
    // the user, and widening the boundary stays a deliberate profile edit.
    assert!(!std::fs::read_dir(&host.place_home)
        .unwrap()
        .flatten()
        .any(|entry| entry.file_name() == ".local"));
    assert_eq!(
        host.projected(".claude/CLAUDE.md").as_deref(),
        Some("content\n")
    );
}

// ---- Hostile input ------------------------------------------------------

#[test]
fn a_declared_entry_that_is_not_home_relative_is_refused() {
    let host = Host::new();
    let agent = agent(&[
        "/etc",
        "~/../../etc/passwd",
        "~/./x",
        "~/a\\b",
        "not-anchored",
    ]);
    let projection = plan(&host.input(&agent, &[]));
    assert_eq!(projection.file_count(), 0);
    for entry in [
        "/etc",
        "~/../../etc/passwd",
        "~/./x",
        "~/a\\b",
        "not-anchored",
    ] {
        assert_eq!(
            finding(&projection, entry).verdict,
            Verdict::HostOnly,
            "{entry}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_source_tree_is_reported_rather_than_followed() {
    let host = Host::new();
    let outside = host.write("outside/private.txt", "host-only material");
    host.write(".claude/CLAUDE.md", "content\n");
    std::os::unix::fs::symlink(&outside, host.path(".claude/leak.txt")).unwrap();
    std::os::unix::fs::symlink(host.path("outside"), host.path(".claude/leakdir")).unwrap();

    let agent = agent(&["~/.claude"]);
    let projection = plan(&host.input(&agent, &[]));
    projection.apply(&host.place_home).unwrap();

    assert!(host.projected(".claude/leak.txt").is_none());
    assert!(host.projected(".claude/leakdir/private.txt").is_none());
    assert!(finding(&projection, "leak.txt").reason.contains("symlink"));
    for file in &projection.files {
        assert!(!String::from_utf8_lossy(&file.contents).contains("host-only material"));
    }
}

#[test]
fn an_enormous_file_is_refused_with_its_size() {
    let host = Host::new();
    host.write(".claude/CLAUDE.md", "content\n");
    host.write(
        ".claude/skills/huge.md",
        &"x".repeat(MAX_FILE_BYTES as usize + 1),
    );
    let agent = agent(&["~/.claude"]);
    let projection = plan(&host.input(&agent, &[]));
    projection.apply(&host.place_home).unwrap();

    assert!(host.projected(".claude/skills/huge.md").is_none());
    let refused = finding(&projection, "huge.md");
    assert_eq!(refused.verdict, Verdict::HostOnly);
    assert!(
        refused.reason.contains("is configuration"),
        "{}",
        refused.reason
    );
    assert_eq!(
        host.projected(".claude/CLAUDE.md").as_deref(),
        Some("content\n")
    );
    assert!(projection.byte_count() < MAX_FILE_BYTES);
}

#[test]
fn a_document_friring_cannot_read_is_refused_rather_than_copied_blind() {
    let host = Host::new();
    host.write(".claude/settings.json", "{ not json at all");
    let binary = host.path(".claude/other.json");
    std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
    std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x01]).unwrap();

    let agent = agent(&["~/.claude"]);
    let projection = plan(&host.input(&agent, &[]));
    projection.apply(&host.place_home).unwrap();

    // Withheld rather than emptied: an empty file at a settings path is not "no
    // settings", it is a settings file the agent fails to parse — which is the
    // pane dying on startup rather than an agent running with its defaults.
    for refused in [".claude/settings.json", ".claude/other.json"] {
        assert_eq!(host.projected(refused), None, "{refused}");
    }
    assert_eq!(projection.file_count(), 0);
    assert!(finding(&projection, "settings.json")
        .reason
        .contains("does not parse"));
    assert!(finding(&projection, "other.json")
        .reason
        .contains("not UTF-8"));
}

/// The escape this writer exists to close: the synthetic home is writable by the
/// sandbox, so the sandbox can replace a directory in it with a link to the
/// host's real agent configuration — and friring's next projection would write
/// straight through it, into the one directory ADR-28 keeps out of reach.
#[cfg(unix)]
#[test]
fn a_symlink_planted_in_the_sandboxs_own_home_refuses_the_write() {
    let host = Host::new();
    host.write(".claude/CLAUDE.md", "projected\n");
    // What the sandbox would aim the link at: the host's real agent directory,
    // whose contents are nothing like what is being projected — so writing
    // through the link is detectable rather than a coincidence.
    let real = host.write("real-agent-home/CLAUDE.md", "the host's own instructions\n");
    let agent = agent(&["~/.claude"]);
    let projection = plan(&host.input(&agent, &[]));

    // A directory component the sandbox replaced.
    std::os::unix::fs::symlink(
        host.path("real-agent-home"),
        host.place_home.join(".claude"),
    )
    .unwrap();
    let err = projection.apply(&host.place_home).unwrap_err();
    assert!(err.to_string().contains("symlink"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        "the host's own instructions\n",
        "the host's own agent configuration was written through"
    );

    // …and the final component, which is the same attack one level down.
    std::fs::remove_file(host.place_home.join(".claude")).unwrap();
    std::fs::create_dir_all(host.place_home.join(".claude")).unwrap();
    let victim = host.write("victim.md", "host material");
    std::os::unix::fs::symlink(&victim, host.place_home.join(".claude/CLAUDE.md")).unwrap();
    let err = projection.apply(&host.place_home).unwrap_err();
    assert!(err.to_string().contains("symlink"), "{err}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host material");

    // A home that is not a directory at all is refused before anything is
    // written, rather than reported one file at a time.
    let err = projection.apply(&host.path("victim.md")).unwrap_err();
    assert!(err.to_string().contains("not a directory"), "{err}");
}

/// A hard link is the other way to make one name mean two files. The staged
/// write replaces the *name*, so whatever else shares the inode is untouched.
#[cfg(unix)]
#[test]
fn a_hard_link_in_the_sandboxs_home_keeps_its_own_inode() {
    let host = Host::new();
    host.write(".claude/CLAUDE.md", "projected\n");
    let victim = host.write("repo-file.rs", "fn main() {}\n");
    std::fs::create_dir_all(host.place_home.join(".claude")).unwrap();
    std::fs::hard_link(&victim, host.place_home.join(".claude/CLAUDE.md")).unwrap();

    let agent = agent(&["~/.claude"]);
    plan(&host.input(&agent, &[]))
        .apply(&host.place_home)
        .unwrap();
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "fn main() {}\n");
    assert_eq!(
        host.projected(".claude/CLAUDE.md").as_deref(),
        Some("projected\n")
    );
}

#[cfg(unix)]
#[test]
fn something_that_is_not_a_file_is_not_configuration() {
    let host = Host::new();
    host.dir(".claude");
    let fifo = host.path(".claude/pipe");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !made {
        eprintln!("skipped: mkfifo is not available on this host");
        return;
    }
    let agent = agent(&["~/.claude"]);
    let projection = plan(&host.input(&agent, &[]));
    assert_eq!(projection.file_count(), 0);
    assert!(finding(&projection, "pipe")
        .reason
        .contains("not a regular file"));
}

// ---- Composition --------------------------------------------------------

/// The inner sandbox composition is legible rather than implicit, and it exists
/// only where a profile does: `compose_inner_sandbox` takes a resolved
/// `SandboxPolicy`, which is what a *profile* resolves to — an unsandboxed
/// launch never reaches this seam, so an agent's bypass flags never reach its
/// argv. (`agent::sandboxing::apply` is the other half of that claim: it answers
/// `Unsandboxed` before composing anything when a session carries no profile.)
#[test]
fn bypass_flags_come_with_a_profile_and_a_sentence() {
    use crate::sandbox::{compose_inner_sandbox, InnerSandboxVerdict};
    use crate::session::{SandboxBackendKind, SandboxPath, SandboxProfile};

    let policy = SandboxProfile::new("dev", vec![SandboxPath::workspace("/repo")])
        .resolve(SandboxBackendKind::Docker, "/home/u")
        .unwrap();
    let mut agent = agent(&[]);
    agent.bypass = vec!["--dangerously-skip-permissions".into()];

    let composed = compose_inner_sandbox(&policy, InnerSandboxVerdict::Redundant, Some(&agent));
    assert_eq!(composed.extra_args, ["--dangerously-skip-permissions"]);
    assert_eq!(
        composed.label,
        "sandbox: dev (docker) · inner agent sandbox: off — Friring is the boundary"
    );
    // No declaration, nothing applied — the agent-neutrality rule, and the one
    // shape where friring cannot turn an inner sandbox off at all.
    let silent = compose_inner_sandbox(&policy, InnerSandboxVerdict::Redundant, None);
    assert!(silent.extra_args.is_empty());
    assert!(silent.label.contains("declares no bypass flags"));
}

// ---- The classifier itself ---------------------------------------------

#[test]
fn a_windows_path_is_never_translated_into_a_posix_place() {
    let host = Host::new();
    let agent = agent(&[]);
    let input = host.input(&agent, &[]);
    let reach = Reach::new(&input, std::iter::empty());
    for token in [
        "C:\\Users\\u\\bin\\x.exe",
        "c:/Users/u",
        "\\\\server\\share",
    ] {
        assert!(
            matches!(reach.resolve(token), Reference::Forbidden(_)),
            "{token}"
        );
    }
    // …and a bare word is not a path at all, so an agent naming a binary on the
    // image's own PATH crosses untouched.
    assert_eq!(reach.resolve("npx"), Reference::Fine);
    assert_eq!(reach.resolve("docs-mcp"), Reference::Fine);
}

#[test]
fn a_path_is_found_inside_a_command_line_but_a_colon_never_splits_one() {
    let found = |text: &str| -> Vec<String> {
        path_tokens(text)
            .into_iter()
            .map(|(range, token)| {
                assert_eq!(&text[range], token, "the range must name the token");
                token.to_string()
            })
            .collect()
    };
    assert_eq!(
        found("/usr/bin/env python3 ~/s.py --flag=/opt/x"),
        ["/usr/bin/env", "~/s.py", "/opt/x"]
    );
    assert!(found("printf working").is_empty());
    // A colon is legal in a path, so splitting on one would turn a single
    // reference into fragments and classify none of them correctly.
    assert_eq!(found("/srv/a:b/c"), ["/srv/a:b/c"]);
    assert_eq!(found("$HOME/x ${HOME}/y"), ["$HOME/x", "${HOME}/y"]);
}

/// A prefix neighbour is classified on its own merits rather than swept along
/// with the path it happens to start with.
///
/// `~/.claude` is projected and `~/.claude-backup` is not, and the two differ by
/// a suffix — which is also why a rewrite splices the byte range it classified
/// instead of replacing every occurrence of the text.
#[cfg(unix)]
#[test]
fn a_prefix_neighbour_is_classified_on_its_own() {
    let host = Host::new();
    host.write(".claude/hooks/pre.sh", "#!/bin/sh\n");
    host.dir(".claude-backup");
    host.write(
        ".claude/settings.json",
        &format!(
            r#"{{ "plugins": {{ "repositories": ["{home}/.claude/hooks", "{home}/.claude-backup"] }} }}"#,
            home = host.home
        ),
    );
    let agent = agent(&["~/.claude"]);
    let projection = plan(&host.input(&agent, &[]));
    projection.apply(&host.place_home).unwrap();

    let text = host.projected(".claude/settings.json").unwrap();
    let projected: serde_json::Value = serde_json::from_str(&text).unwrap();
    // The projected sibling is rewritten; the un-projected one is dropped whole
    // rather than half-rewritten into a path that exists on neither side.
    assert_eq!(
        projected["plugins"]["repositories"],
        serde_json::json!([format!("{INSIDE}/.claude/hooks")]),
        "{text}"
    );
}

#[test]
fn a_name_that_says_credential_is_treated_as_one() {
    for name in [
        ".credentials.json",
        "auth.json",
        "api_token.txt",
        "id_ed25519",
        "server.pem",
        ".env",
        "SECRET",
    ] {
        assert!(looks_like_a_credential(name), "{name}");
    }
    for name in [
        "SKILL.md",
        "CLAUDE.md",
        "settings.json",
        "review.md",
        "config.toml",
    ] {
        assert!(!looks_like_a_credential(name), "{name}");
    }
}
