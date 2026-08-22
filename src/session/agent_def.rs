//! Declarative coding-agent definitions.
//!
//! A [`AgentDef`] describes how to launch one coding-agent CLI (claude, codex,
//! antigravity, opencode, aider, …) as data: the command name plus a set of
//! argument-group templates. Definitions are loaded from
//! `~/.config/friring/agents.toml` (see [`crate::agent::agent_config`]) and
//! seeded with built-ins on first run, so users can register custom agents
//! without recompiling.
//!
//! This module is pure data + pure logic (no filesystem, no local imports
//! beyond serde/std) to satisfy the `session/` architecture rule. The TOML
//! loading and the `AgentProvider` bridge live in `crate::agent`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Placeholder substituted with a session id in resume/fork/new-session groups.
const ID_PLACEHOLDER: &str = "{id}";

/// Placeholder substituted with the friring session name in resume/fork/
/// new-session groups, for agents whose CLI can name a session at launch
/// (e.g. claude's `-n {name}`). A group token referencing `{name}` when the
/// launch has no name is dropped together with its preceding flag, so an
/// unnamed launch never emits a dangling `-n`.
const NAME_PLACEHOLDER: &str = "{name}";

/// One coding-agent CLI definition.
///
/// Each `*_args` group is appended to the final argument list **only** when its
/// driving value is present (the session is being resumed/forked, etc.), with
/// `{id}` and `{name}` substituted token-by-token. This avoids any "unresolved
/// placeholder" heuristics: a group with no value is simply omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDef {
    /// Display + lookup name (e.g. `"claude"`). Unique within a registry.
    pub name: String,
    /// CLI executable to run (e.g. `"claude"`, `"opencode"`).
    pub command: String,
    /// Static arguments always passed, before any templated group. Bake a
    /// model or any other flag here if you want one (e.g. `["--model", "opus"]`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Emitted when resuming a known session; `{id}` is substituted.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Emitted when forking from a parent conversation; `{id}` is substituted.
    #[serde(default)]
    pub fork_args: Vec<String>,
    /// Emitted on a fresh spawn to pin a session id; `{id}` is substituted.
    #[serde(default)]
    pub new_session_args: Vec<String>,
    /// When true, this agent resumes its most-recent session in the launch
    /// directory using id-less `resume_args` (no `{id}` token). friring cannot
    /// pin or read back the agent's real session id for these CLIs, so restart
    /// relies on the agent's own "last session in this directory" resolution
    /// (e.g. `codex resume --last`, `opencode --continue`). Agents that pin ids
    /// (claude) leave this `false` and resume by a friring-known id instead.
    #[serde(default)]
    pub resume_latest: bool,
    /// Names the hook *family* this CLI understands, letting the built-in
    /// `hooks` extension wire status hooks for a **custom** agent as if it were
    /// that built-in. A rebranded-claude agent sets `hook_schema = "claude"` so
    /// the same `--settings` patch (and remote rewrite) that targets the
    /// built-in `claude` also targets it. `None` = wire only if this agent's own
    /// `name` matches a built-in family. friring bakes in no agent knowledge —
    /// the *user* asserts the family here. See [`crate::agent::extension_config`]
    /// (`apply_agent_patches`) and `extensions/hooks/`.
    #[serde(default)]
    pub hook_schema: Option<String>,
    /// What this CLI needs in order to survive being sandboxed
    /// (`[agents.<name>.sandbox]`). Absent for an agent nobody has sandboxed
    /// yet: it still launches, it just gets no help — which the editor says
    /// rather than papering over. friring bakes in no agent knowledge; the
    /// *user* declares the flags and directories here.
    #[serde(default)]
    pub sandbox: Option<AgentSandboxDef>,
}

/// How an agent gets its credentials inside a sandbox (`docs/SANDBOX.md`
/// §Credentials, in resolution order).
///
/// A *request*, not a verdict: what a launch actually does is
/// [`crate::sandbox::auth::CredentialStrategy`], which resolves this
/// against the boundary's shape. A policy backend is always host passthrough,
/// and a place cannot be one — so a declaration naming a strategy its boundary
/// cannot give it degrades to one it can, with the reason in front of the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxAuth {
    /// Let friring pick: host passthrough under a policy backend, and the
    /// strongest strategy the agent declares elsewhere in a place.
    #[default]
    Auto,
    /// The agent sees the real credential store, subject to path policy. Policy
    /// backends only, and the reason they are the default.
    HostPassthrough,
    /// A long-lived token friring holds in its own keychain entry and injects.
    EnvToken,
    /// A per-profile volume holding the agent's state, with one login per
    /// profile.
    VolumeLogin,
    /// Copy a credential file in once and honour write-back. Opt-in, and only
    /// for agents whose vendor documents it.
    SeedFile,
}

/// The `[agents.<name>.sandbox]` block.
///
/// Every field is optional: an agent that declares nothing still runs, it just
/// gets no help. Lives here rather than in `crate::sandbox` because `session` is
/// the dependency sink — [`AgentDef`] can only embed a type defined alongside
/// it. `crate::sandbox::agent` is what turns a declaration plus the chosen
/// backend into the extra argv and extra writable paths a launch applies.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AgentSandboxDef {
    /// Environment variable that relocates the agent's state into the sandbox.
    /// Meaningful for a place, whose home is synthetic; a policy backend leaves
    /// it alone, because the real state directory is already visible and
    /// relocating it would strand the agent's existing login.
    #[serde(default)]
    pub config_dir_env: Option<String>,
    #[serde(default)]
    pub auth: SandboxAuth,
    /// The one directory [`config_dir_env`](Self::config_dir_env) names — the
    /// agent's own state, where its login lands. Written home-relative (`~/.x`),
    /// because a place relocates it under that place's synthetic home and the
    /// host's home path does not exist in there.
    ///
    /// Distinct from [`state_rw`](Self::state_rw), which is a *policy* input
    /// (paths to keep writable on the host): this is the single directory a
    /// place persists per profile, and the one an agent signs into once.
    #[serde(default)]
    pub state_dir: Option<String>,
    /// The vendor's credential file, home-relative — the file `seed-file`
    /// copies and the file whose presence says the sandbox has a login already.
    ///
    /// friring never reads its contents to inspect them (ADR-28); it is named
    /// here so a strategy can copy it whole and so "is this sandbox signed in?"
    /// can be answered without opening anything.
    #[serde(default)]
    pub credential_file: Option<String>,
    /// Whether the vendor documents copying
    /// [`credential_file`](Self::credential_file) into another environment.
    ///
    /// The gate on `seed-file`, and deliberately a separate assertion from
    /// asking for that strategy: a **rotating single-use refresh token can
    /// never be declared here**, because the copy and the original invalidate
    /// each other on the first refresh (ADR-28). Left `false`, `seed-file` is
    /// refused and the launch falls back to signing in inside the pane.
    #[serde(default)]
    pub seed_file_supported: bool,
    /// Directories the agent writes and must keep. Added to the policy's
    /// writable set: an agent that cannot write its own state directory dies on
    /// first launch under an otherwise correct profile.
    #[serde(default)]
    pub state_rw: Vec<String>,
    /// Configuration safe to project into a place, subject to the lint pass.
    ///
    /// Written `~`-anchored (`"~/.claude/skills"`) and projected to the
    /// *identical* home-relative path inside the boundary, because a place gives
    /// the agent a synthetic `$HOME` and nothing else about the layout has to
    /// change. Each entry is a file or a directory tree;
    /// [`crate::sandbox::projection`] classifies every member before any of it
    /// crosses, and a credential file never does (ADR-28).
    #[serde(default)]
    pub copy_in: Vec<String>,
    /// The highest-precedence configuration layer friring writes inside the
    /// boundary — what must hold there whatever a repository-level file says,
    /// including the workspace trust several agents otherwise prompt for on
    /// first run in a fresh home. See
    /// [`EnforcedSettings`](super::agent_projection::EnforcedSettings).
    #[serde(default)]
    pub enforced: Vec<super::agent_projection::EnforcedSettings>,
    /// Static environment applied whenever a sandbox is active.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Names of the environment variables that carry a long-lived token this
    /// agent accepts (`ANTHROPIC_API_KEY`, …) — **names only**. The values live
    /// in friring's own OS keychain entry and never in the registry, the
    /// database or a config file, and they are what `env-token` injects.
    #[serde(default)]
    pub secret_env: Vec<String>,
    /// Flags that mean "the outer boundary is the sandbox" — the agent's own
    /// sandbox off. Applied only when a profile is active.
    #[serde(default)]
    pub bypass: Vec<String>,
    /// Whether a credential the agent refreshes has to survive the sandbox it
    /// was refreshed in.
    ///
    /// It does so by living in the profile's own persistent state directory
    /// rather than by being written back to the host: two writers of one
    /// rotating token is the failure ADR-28 exists to prevent, and the host is
    /// one of them.
    #[serde(default)]
    pub writeback: bool,
    /// What the user has to do inside the pane when this agent has no
    /// credential in the sandbox — the agent's own words for it (`/login`,
    /// `codex login`). friring composes the sentence around it, so a place with
    /// no credential reads as "sign in here, like this" rather than as a broken
    /// session.
    #[serde(default)]
    pub login_fallback: Option<String>,
}

impl AgentDef {
    /// Build the CLI argument list for one launch.
    ///
    /// Session-selection precedence mirrors the historical Claude behaviour:
    /// fork wins over resume, which wins over a fresh `new_session` id. After
    /// the selection group come the static `args`. No model is ever passed —
    /// the agent uses its own default config (bake one into `args` if needed).
    ///
    /// `session_name` fills `{name}` tokens in the selected group. Whether a
    /// launch pushes the friring name into the agent is decided by the
    /// *templates*: the built-in claude entry references `{name}` only in its
    /// fork/new-session groups, so a resume never renames a conversation the
    /// agent already owns.
    pub fn build_args(
        &self,
        resume_id: Option<&str>,
        fork_id: Option<&str>,
        new_session_id: Option<&str>,
        session_name: Option<&str>,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();

        if let Some(id) = fork_id {
            out.extend(subst_group(&self.fork_args, id, session_name));
        } else if let Some(id) = resume_id {
            out.extend(subst_group(&self.resume_args, id, session_name));
        } else if let Some(id) = new_session_id {
            out.extend(subst_group(&self.new_session_args, id, session_name));
        }

        out.extend(self.args.iter().cloned());

        out
    }

    /// Whether a restart should trigger this agent's resume group via
    /// "latest session in the launch directory" semantics rather than a
    /// friring-known session id. True only when [`Self::resume_latest`] is set
    /// and there are `resume_args` to emit.
    pub fn resumes_latest(&self) -> bool {
        self.resume_latest && !self.resume_args.is_empty()
    }

    /// Whether this agent can resume a *specific* conversation by id: its
    /// `resume_args` carry the `{id}` placeholder and it doesn't rely on
    /// "latest in cwd" resolution. This is what the conversation-import flow
    /// needs — an id-less resume group would silently open the wrong session.
    pub fn resumes_by_id(&self) -> bool {
        !self.resume_latest && self.resume_args.iter().any(|t| t.contains(ID_PLACEHOLDER))
    }
}

/// Substitute `{id}` — and `{name}`, when the launch has a non-empty session
/// name — in every token of one selected arg group.
///
/// A `{name}` token with no name to fill it is dropped **together with** its
/// immediately preceding value-taking flag (same pair rule as the remote
/// config-path rewriting in `session_ops`), so `["-n", "{name}"]` vanishes as
/// a pair instead of leaving a dangling `-n` to eat the next arg. A
/// self-contained `--flag={name}` token drops alone.
fn subst_group(tokens: &[String], id: &str, name: Option<&str>) -> Vec<String> {
    let name = name.filter(|n| !n.is_empty());
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let token = token.replace(ID_PLACEHOLDER, id);
        if !token.contains(NAME_PLACEHOLDER) {
            out.push(token);
            continue;
        }
        match name {
            Some(n) => out.push(token.replace(NAME_PLACEHOLDER, n)),
            None => {
                // Only a bare *value* token (the value half of a `-flag {name}`
                // pair) pops the preceding value-taking flag. A token that is
                // itself a flag (`--name={name}`, starts with `-`) is
                // self-contained and drops alone — mirroring the flag vs
                // `--flag=value` split in `session_ops`' `rewrite_config_path_args`.
                // Keyed on the leading `-`, not on an embedded `=`: a value
                // token can legitimately contain `=` (e.g. `["--label",
                // "session={name}"]`), and that pair must still drop together.
                if !token.starts_with('-')
                    && out
                        .last()
                        .is_some_and(|prev| prev.starts_with('-') && !prev.contains('='))
                {
                    out.pop();
                }
            }
        }
    }
    out
}

/// A set of agent definitions plus the name of the default agent.
///
/// Unknown fields are tolerated but reported: the loader names every
/// unrecognized key in a startup warning (stale keys from older versions and
/// typos both surface without stranding the user on built-ins).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegistry {
    /// Config-format version, for future migrations. Currently `1`.
    #[serde(default)]
    pub config_version: Option<u32>,
    /// Name of the agent selected by default in the picker / headless spawns.
    #[serde(default)]
    pub default: String,
    /// All known agent definitions, in display order.
    #[serde(default)]
    pub agents: Vec<AgentDef>,
}

impl AgentRegistry {
    /// Look up an agent definition by name.
    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// The default agent definition: the one named by `default`, falling back
    /// to the first defined agent.
    pub fn default_agent(&self) -> Option<&AgentDef> {
        self.get(&self.default).or_else(|| self.agents.first())
    }

    /// The default agent's name, or empty string when the registry is empty.
    pub fn default_name(&self) -> String {
        self.default_agent()
            .map(|a| a.name.clone())
            .unwrap_or_default()
    }

    /// All agent names in display order.
    pub fn names(&self) -> Vec<&str> {
        self.agents.iter().map(|a| a.name.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> AgentDef {
        AgentDef {
            name: "claude".into(),
            command: "claude".into(),
            args: vec![],
            resume_args: vec!["--resume".into(), "{id}".into()],
            fork_args: vec![
                "--resume".into(),
                "{id}".into(),
                "--fork-session".into(),
                "-n".into(),
                "{name}".into(),
            ],
            new_session_args: vec![
                "--session-id".into(),
                "{id}".into(),
                "-n".into(),
                "{name}".into(),
            ],
            resume_latest: false,
            hook_schema: None,
            sandbox: None,
        }
    }

    #[test]
    fn fresh_session_pins_id() {
        let d = claude();
        let args = d.build_args(None, None, Some("new-id"), None);
        assert_eq!(args, vec!["--session-id", "new-id"]);
        // No model is ever passed.
        assert!(!args.iter().any(|a| a == "--model"));
    }

    #[test]
    fn fresh_session_with_name_appends_name_flag() {
        let d = claude();
        // A name with spaces stays a single argv token — no re-splitting.
        let args = d.build_args(None, None, Some("new-id"), Some("fix auth flow"));
        assert_eq!(args, vec!["--session-id", "new-id", "-n", "fix auth flow"]);
    }

    #[test]
    fn empty_name_drops_the_flag_pair_like_none() {
        let d = claude();
        let args = d.build_args(None, None, Some("new-id"), Some(""));
        assert_eq!(args, vec!["--session-id", "new-id"]);
    }

    #[test]
    fn equals_form_name_token_drops_alone() {
        // A self-contained `--flag={name}` token must not pop the (complete)
        // token before it when the launch has no name.
        let mut d = claude();
        d.new_session_args = vec!["--session-id={id}".into(), "--name={name}".into()];
        assert_eq!(
            d.build_args(None, None, Some("new-id"), None),
            vec!["--session-id=new-id"]
        );
        assert_eq!(
            d.build_args(None, None, Some("new-id"), Some("x")),
            vec!["--session-id=new-id", "--name=x"]
        );
    }

    #[test]
    fn equals_form_name_token_does_not_pop_an_unrelated_flag() {
        // A self-contained `--name={name}` token must drop alone even when the
        // token before it is a plain flag: `--verbose` is not the value slot of
        // `--name=…`, so a name-less launch must keep it.
        let mut d = claude();
        d.new_session_args = vec!["--verbose".into(), "--name={name}".into()];
        assert_eq!(
            d.build_args(None, None, Some("new-id"), None),
            vec!["--verbose"]
        );
    }

    #[test]
    fn pair_with_equals_in_value_token_drops_together() {
        // The value half of a `-flag {name}` pair can legitimately embed `=`
        // (`session={name}`); it is still a value token, not a self-contained
        // `--flag=value`, so a name-less launch drops it *and* its preceding
        // flag together. Keying the drop on an embedded `=` would strand the
        // flag next to an unrelated arg.
        let mut d = claude();
        d.new_session_args = vec![
            "--label".into(),
            "session={name}".into(),
            "--verbose".into(),
        ];
        assert_eq!(
            d.build_args(None, None, Some("new-id"), None),
            vec!["--verbose"]
        );
        assert_eq!(
            d.build_args(None, None, Some("new-id"), Some("s")),
            vec!["--label", "session=s", "--verbose"]
        );
    }

    #[test]
    fn bare_positional_name_token_drops_alone() {
        // A bare positional {name} (not a `-flag {name}` pair) drops by itself
        // when the launch has no name: the preceding non-flag token is not a
        // value-taking flag, so it survives. An empty name behaves like None.
        let mut d = claude();
        d.new_session_args = vec!["prefix".into(), "{name}".into(), "--mode".into()];
        assert_eq!(
            d.build_args(None, None, Some("new-id"), None),
            vec!["prefix", "--mode"]
        );
        assert_eq!(
            d.build_args(None, None, Some("new-id"), Some("")),
            d.build_args(None, None, Some("new-id"), None)
        );
    }

    #[test]
    fn resume_takes_precedence_over_new() {
        let d = claude();
        let args = d.build_args(Some("resume-id"), None, Some("new-id"), None);
        assert_eq!(args, vec!["--resume", "resume-id"]);
    }

    #[test]
    fn resume_never_pushes_the_name() {
        // The resume template carries no {name} token: a conversation the agent
        // already owns is never renamed, even though the launch has a name.
        let d = claude();
        let args = d.build_args(Some("resume-id"), None, None, Some("my session"));
        assert_eq!(args, vec!["--resume", "resume-id"]);
    }

    #[test]
    fn fork_takes_precedence_over_resume() {
        let d = claude();
        let args = d.build_args(Some("resume-id"), Some("fork-id"), Some("new-id"), None);
        assert_eq!(args, vec!["--resume", "fork-id", "--fork-session"]);
    }

    #[test]
    fn fork_with_name_names_the_forked_conversation() {
        let d = claude();
        let args = d.build_args(None, Some("fork-id"), None, Some("child"));
        assert_eq!(
            args,
            vec!["--resume", "fork-id", "--fork-session", "-n", "child"]
        );
    }

    #[test]
    fn static_args_only_when_no_session_group() {
        let d = AgentDef {
            name: "codex".into(),
            command: "codex".into(),
            args: vec!["--quiet".into()],
            resume_args: vec![],
            fork_args: vec![],
            new_session_args: vec![],
            resume_latest: false,
            hook_schema: None,
            sandbox: None,
        };
        let args = d.build_args(None, None, Some("ignored"), None);
        assert_eq!(args, vec!["--quiet"]);
    }

    #[test]
    fn idless_resume_and_fork_pass_tokens_verbatim() {
        // Mirrors the seeded codex definition: id-less resume/fork groups that
        // resolve "latest in cwd" inside the agent and ignore any supplied id.
        let d = AgentDef {
            name: "codex".into(),
            command: "codex".into(),
            args: vec![],
            resume_args: vec!["resume".into(), "--last".into()],
            fork_args: vec!["fork".into(), "--last".into()],
            new_session_args: vec![],
            resume_latest: true,
            hook_schema: None,
            sandbox: None,
        };
        // resume id present, but no {id} token -> tokens unchanged.
        assert_eq!(
            d.build_args(Some("ignored-uuid"), None, None, None),
            vec!["resume", "--last"]
        );
        // fork wins over resume, still id-less.
        assert_eq!(
            d.build_args(Some("ignored-uuid"), Some("also-ignored"), None, None),
            vec!["fork", "--last"]
        );
        assert!(d.resumes_latest());
    }

    #[test]
    fn resumes_by_id_requires_id_placeholder_without_resume_latest() {
        assert!(claude().resumes_by_id());
        let mut idless = claude();
        idless.resume_args = vec!["--continue".into()];
        assert!(!idless.resumes_by_id());
        let mut latest = claude();
        latest.resume_latest = true;
        assert!(!latest.resumes_by_id());
    }

    #[test]
    fn resumes_latest_requires_flag_and_resume_args() {
        let mut d = AgentDef {
            name: "x".into(),
            command: "x".into(),
            args: vec![],
            resume_args: vec![],
            fork_args: vec![],
            new_session_args: vec![],
            resume_latest: true,
            hook_schema: None,
            sandbox: None,
        };
        // Flag set but no resume_args -> nothing to emit, so not "resumes latest".
        assert!(!d.resumes_latest());
        d.resume_args = vec!["--continue".into()];
        assert!(d.resumes_latest());
        d.resume_latest = false;
        assert!(!d.resumes_latest());
    }

    #[test]
    fn registry_lookup_and_default() {
        let reg = AgentRegistry {
            config_version: None,
            default: "codex".into(),
            agents: vec![
                claude(),
                AgentDef {
                    name: "codex".into(),
                    command: "codex".into(),
                    args: vec![],
                    resume_args: vec![],
                    fork_args: vec![],
                    new_session_args: vec![],
                    resume_latest: false,
                    hook_schema: None,
                    sandbox: None,
                },
            ],
        };
        assert_eq!(reg.get("claude").unwrap().command, "claude");
        assert_eq!(reg.default_agent().unwrap().name, "codex");
        assert_eq!(reg.names(), vec!["claude", "codex"]);
    }

    #[test]
    fn sandbox_block_round_trips_and_is_optional() {
        let toml = r#"
name = "claude"
command = "claude"

[sandbox]
config_dir_env = "CLAUDE_CONFIG_DIR"
auth = "host-passthrough"
state_dir = "~/.claude"
credential_file = "~/.claude/.credentials.json"
seed_file_supported = false
secret_env = ["ANTHROPIC_API_KEY"]
login_fallback = "/login"
state_rw = ["~/.claude"]
bypass = ["--dangerously-skip-permissions"]
writeback = true
[sandbox.env]
DISABLE_AUTOUPDATER = "1"
"#;
        let def: AgentDef = toml::from_str(toml).unwrap();
        let sandbox = def.sandbox.expect("declared block parses");
        assert_eq!(sandbox.auth, SandboxAuth::HostPassthrough);
        assert_eq!(sandbox.config_dir_env.as_deref(), Some("CLAUDE_CONFIG_DIR"));
        assert_eq!(sandbox.state_dir.as_deref(), Some("~/.claude"));
        assert_eq!(
            sandbox.credential_file.as_deref(),
            Some("~/.claude/.credentials.json")
        );
        assert!(!sandbox.seed_file_supported);
        assert_eq!(sandbox.secret_env, ["ANTHROPIC_API_KEY"]);
        assert_eq!(sandbox.login_fallback.as_deref(), Some("/login"));
        assert_eq!(sandbox.state_rw, ["~/.claude"]);
        assert_eq!(sandbox.bypass, ["--dangerously-skip-permissions"]);
        assert_eq!(sandbox.env.get("DISABLE_AUTOUPDATER").unwrap(), "1");
        assert!(sandbox.writeback);
        assert!(sandbox.copy_in.is_empty());

        // An agents.toml written before sandboxing existed must load unchanged.
        let legacy: AgentDef = toml::from_str("name = \"x\"\ncommand = \"x\"\n").unwrap();
        assert_eq!(legacy.sandbox, None);

        // An empty block is valid and means "declare nothing".
        let bare: AgentDef = toml::from_str("name = \"x\"\ncommand = \"x\"\n[sandbox]\n").unwrap();
        assert_eq!(bare.sandbox, Some(AgentSandboxDef::default()));

        // …and so is one written before the credential fields existed: every
        // addition here is `#[serde(default)]`, so a registry that predates
        // this slice keeps loading and simply declares nothing about
        // credentials.
        let p3: AgentDef = toml::from_str(
            "name = \"x\"\ncommand = \"x\"\n[sandbox]\nauth = \"auto\"\nstate_rw = [\"~/.x\"]\n",
        )
        .unwrap();
        let p3 = p3.sandbox.expect("the older block still parses");
        assert_eq!(p3.state_dir, None);
        assert_eq!(p3.credential_file, None);
        assert!(!p3.seed_file_supported);
        assert!(p3.secret_env.is_empty());
    }

    #[test]
    fn registry_default_falls_back_to_first() {
        let reg = AgentRegistry {
            config_version: None,
            default: "missing".into(),
            agents: vec![claude()],
        };
        assert_eq!(reg.default_agent().unwrap().name, "claude");
    }
}
