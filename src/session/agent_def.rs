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
                },
            ],
        };
        assert_eq!(reg.get("claude").unwrap().command, "claude");
        assert_eq!(reg.default_agent().unwrap().name, "codex");
        assert_eq!(reg.names(), vec!["claude", "codex"]);
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
