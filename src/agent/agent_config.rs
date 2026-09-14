//! Loading and seeding of the agent-definition config file.
//!
//! Agents are defined declaratively in `~/.config/friring/agents.toml`. On
//! first run (or whenever the file is missing) the built-in definitions are
//! written out so users have a working starting point they can edit. If the
//! file exists but cannot be read or parsed, we fall back to the built-ins
//! rather than failing to start.

use std::path::PathBuf;

use crate::session::{AgentDef, AgentRegistry};

/// Built-in agent definitions, also used to seed `agents.toml` on first run.
///
/// Kept deliberately small per agent: just the command, plus resume/fork/
/// session-id groups. `claude` pins a friring-generated id (`--session-id`) so
/// it can resume/fork by that exact id. The other built-ins can't pin or report
/// their session id, so they use `resume_latest = true` with id-less,
/// cwd-scoped flags (`codex resume --last`, `opencode --continue`, …): the agent
/// resolves "the last session in this directory" itself. Agents without any
/// resume group simply start fresh on restart. No model is passed — each agent
/// uses its own default config. Bake extra flags (including a model) into
/// `args` if you want them.
pub const BUILTIN_AGENTS_TOML: &str = r#"# Friring coding-agent definitions.
#
# Each [[agents]] entry describes how to launch one coding-agent CLI. The
# `*_args` groups are appended only when their value is present, with {id}
# and {name} substituted. `args` is always passed — put any extra flags
# (e.g. a model) there. Add your own [[agents]] entries to support any CLI.
#
# Unknown keys are reported on startup (and fail `friring-cli config
# validate`) but don't break the load — your agents stay in effect.

config_version = 1
default = "claude"

# claude also takes the friring session name (`-n {name}`) when a conversation
# is *created* (fresh spawn or fork), so it shows up under the same name in
# claude's own /resume picker. Resume deliberately omits {name}: a restart
# never renames a conversation the agent already owns (e.g. after an in-agent
# /rename).
[[agents]]
name = "claude"
command = "claude"
resume_args = ["--resume", "{id}"]
fork_args = ["--resume", "{id}", "--fork-session", "-n", "{name}"]
new_session_args = ["--session-id", "{id}", "-n", "{name}"]

# What this CLI needs to survive a sandbox profile (docs/SANDBOX.md). Applied
# only while a profile is active; edit it if your CLI's flags differ.
#
# `auth = "auto"` is host-passthrough under a policy sandbox (the real login,
# Keychain included, is right where it was) and, inside a container, a token
# from friring's own keychain entry if you stored one — otherwise one login per
# profile, done in the pane. `seed_file_supported` is deliberately absent: this
# vendor's refresh token is single-use, so a copy and the original invalidate
# each other (ADR-28).
[agents.sandbox]
auth = "auto"
config_dir_env = "CLAUDE_CONFIG_DIR"
state_dir = "~/.claude"
credential_file = "~/.claude/.credentials.json"
secret_env = ["ANTHROPIC_API_KEY"]
login_fallback = "/login"
writeback = true
state_rw = ["~/.claude", "~/.claude.json"]
bypass = ["--dangerously-skip-permissions"]
# Projected into a container's synthetic home at the same `~`-relative path,
# after a lint pass that drops anything naming the host filesystem. Instructions,
# skills and commands are prose and cross whole; a hook or MCP server pointing at
# a host binary does not, and no credential ever does (ADR-28).
copy_in = [
  "~/.claude/CLAUDE.md",
  "~/.claude/skills",
  "~/.claude/commands",
  "~/.claude/agents",
  "~/.claude/settings.json",
]

# Where this CLI keeps a conversation, so friring can tell a resume that will
# reach one from a resume that would silently start a new conversation with the
# same id. The transcript is `projects/<encoded cwd>/<id>.jsonl`, so the file
# NAME is the id friring resumes by.
[agents.transcript]
dir = "projects"
suffix = ".jsonl"
name_has_id = true

# codex can't pin or report its session id, so resume/fork target the most
# recent session in the launch directory. friring keeps that directory stable
# across restart (same cwd) and single-repo fork (child reuses the parent cwd).
[[agents]]
name = "codex"
command = "codex"
resume_args = ["resume", "--last"]
fork_args = ["fork", "--last"]
resume_latest = true

[agents.sandbox]
auth = "auto"
config_dir_env = "CODEX_HOME"
state_dir = "~/.codex"
credential_file = "~/.codex/auth.json"
secret_env = ["OPENAI_API_KEY"]
login_fallback = "codex login"
writeback = true
state_rw = ["~/.codex"]
bypass = ["--dangerously-bypass-approvals-and-sandbox"]
copy_in = ["~/.codex/AGENTS.md", "~/.codex/prompts"]

# Rollouts are `sessions/<yyyy>/<mm>/<dd>/rollout-*.jsonl`, and the id in that
# name is codex's own rather than friring's — so `name_has_id` is false and the
# check is "is there a conversation for `resume --last` to resolve to at all?".
# Exact for a bridge child, whose CODEX_HOME is private and holds only its own.
[agents.transcript]
dir = "sessions"
suffix = ".jsonl"

# friring's highest-precedence layer inside the boundary, merged over whatever
# projected to the same path. A place is always a fresh home, so the workspace
# trust this agent would otherwise prompt for on first run is pre-seeded here —
# once per path the profile granted.
[[agents.sandbox.enforced]]
path = "~/.codex/config.toml"
format = "toml"
per_path = """
[projects."{path}"]
trust_level = "trusted"
"""

# antigravity (the `agy` CLI, the Gemini CLI successor) resumes the latest
# session in the launch directory via `--continue`; it has no fork (Ctrl+F falls
# back to a fresh session).
[[agents]]
name = "antigravity"
command = "agy"
resume_args = ["--continue"]
resume_latest = true

# `--continue` resumes the last session in the cwd; add `--fork` to branch it.
[[agents]]
name = "opencode"
command = "opencode"
resume_args = ["--continue"]
fork_args = ["--continue", "--fork"]
resume_latest = true

# aider restores the chat-history file (.aider.chat.history.md) in the cwd; it
# has no separate session id and no fork.
[[agents]]
name = "aider"
command = "aider"
resume_args = ["--restore-chat-history"]
resume_latest = true

# GitHub Copilot CLI (the `copilot` command) resumes the most recent session in
# the launch directory via `--continue`; it can't pin or report a session id and
# has no fork (Ctrl+F falls back to a fresh session).
[[agents]]
name = "copilot"
command = "copilot"
resume_args = ["--continue"]
resume_latest = true

[[agents]]
name = "vibe"
command = "vibe"

# ──────────────────────────────────────────────────────────────────────────
# Add your own agent (uncomment and edit)
# ──────────────────────────────────────────────────────────────────────────
#
# Any CLI works — friring only needs `command` plus the optional `*_args`
# groups below. The agent uses its OWN default config; friring never passes a
# model or permissions of its own.
#
# [[agents]]
# name = "my-agent"             # shown in the new-session agent picker
# command = "my-agent-cli"      # the executable on your PATH
# args = []                     # ALWAYS passed (see "Pin a model" below)
# resume_args = []              # appended on restart/resume, with {id}/{name} substituted
# fork_args = []                # appended on Ctrl+F fork, with {id}/{name} substituted
# new_session_args = []         # appended on a fresh spawn, with {id}/{name} substituted
# resume_latest = false         # true ⇒ resume "the last session in this dir"
#                               #   (id-less flags); leave false to pin by {id}
# hook_schema = "claude"        # OPTIONAL: name the hook FAMILY this CLI speaks
#                               #   so the built-in `hooks` extension wires its
#                               #   status hooks as if this were that built-in.
#                               #   A rebranded-claude CLI sets "claude" to get
#                               #   claude's --settings hook wiring under its own
#                               #   name. Omit if the agent has no known family.
# activity_provider = "claude-code"
#                               # OPTIONAL: name the TRANSCRIPT FORMAT this CLI
#                               #   writes, so the F9 activity view and
#                               #   `friring-cli session activity` can read it.
#                               #   Omitted, the format is guessed from the
#                               #   command's basename — so a wrapper script or
#                               #   a rebranded binary under any other name
#                               #   reports nothing until you declare one here.
#                               #   Independent of hook_schema: that is the
#                               #   hooks this CLI speaks, this is the records
#                               #   it leaves. One of:
#                               #     claude-code  vibe    qwen-code  cursor-agent
#                               #     gemini-cli   crush   copilot    aider
#                               #     goose        opencode codex     cline
#
# [agents.transcript]           # OPTIONAL: where this CLI stores a conversation,
# dir = "sessions"              #   relative to `state_dir` below, searched
# suffix = ".jsonl"             #   recursively. friring uses it to check that a
# name_has_id = false           #   conversation still EXISTS before resuming it:
#                               #   without it a "resume" can silently start a
#                               #   new one. Set name_has_id = true when the file
#                               #   is named for the {id} friring resumes by.
#                               #   REQUIRED for an agent used as a bridge child
#                               #   with `resume_latest = true`: a child's resume
#                               #   is refused rather than started blank.
#
# [agents.sandbox]              # OPTIONAL: what this CLI needs inside a sandbox
# auth = "auto"                 #   auto | host-passthrough | env-token
#                               #   | volume-login | seed-file
#                               #   (see docs/SANDBOX.md §Credentials)
# state_rw = ["~/.my-agent"]    #   directories it writes and must keep — an
#                               #   agent that can't write its state dies at launch
# bypass = ["--no-sandbox"]     #   flags turning its OWN sandbox off, applied
#                               #   only while a friring profile is active
# env = { X = "1" }             #   static env applied whenever a sandbox is active
#
# --- credentials in a container (nothing below applies to a policy sandbox,
#     which just uses your real login) -------------------------------------
# config_dir_env = "MY_AGENT_HOME"          # env var naming its state directory
# state_dir = "~/.my-agent"                 # the directory that var points at;
#                                           #   kept per profile inside the
#                                           #   sandbox and signed into ONCE
# secret_env = ["MY_AGENT_TOKEN"]           # variable NAMES only — the value
#                                           #   lives in your OS keychain, under
#                                           #   service "dev.friring.sandbox"
# login_fallback = "/login"                 # what to type in the pane when the
#                                           #   sandbox has no credential yet
# writeback = true                          # a refreshed credential must survive
# credential_file = "~/.my-agent/auth.json" # the vendor's credential file
# seed_file_supported = false               # true ONLY if the vendor documents
#                                           #   copying that file elsewhere. A
#                                           #   rotating OAuth token never can:
#                                           #   the copy and the original
#                                           #   invalidate each other (ADR-28),
#                                           #   and friring refuses a second copy
#
# --- configuration in a container (a policy sandbox already sees the real
#     home, so none of this applies to one) -------------------------------
# copy_in = ["~/.my-agent/skills"]          # config safe to project, `~`-anchored:
#                                           #   it lands at the same home-relative
#                                           #   path inside. Linted first — a hook
#                                           #   or MCP server naming a host path is
#                                           #   dropped, a credential never crosses
#
# [[agents.<name>.sandbox.enforced]]        # friring's highest-precedence layer
# path = "~/.my-agent/config.toml"          #   `~`-anchored: a place's only
# format = "toml"                           #   writable surface is its own home
# per_path = "[projects.\"{path}\"]\ntrust_level = \"trusted\""
#                                           # `content` is written once
#                                           #   ({workspaces} → the granted paths
#                                           #   as a list); `per_path` repeats once
#                                           #   per granted path ({path} → that
#                                           #   path). JSON takes `content` only —
#                                           #   two JSON documents cannot be
#                                           #   concatenated
#
# {id} is a friring-generated UUID. Only agents that accept it at creation
# (like claude's `--session-id {id}`) can resume/fork by that exact id; for
# everything else use `resume_latest = true` with id-less, cwd-scoped flags
# (e.g. `["resume", "--last"]`). Omit every resume group to start fresh on
# restart.
#
# {name} is the friring session name, for agents whose CLI can name a session
# at launch (claude's `-n {name}`). A launch without a name drops the {name}
# token together with its preceding flag, so the pair vanishes cleanly. Omit
# {name} everywhere for agents with no naming flag.
#
# ──────────────────────────────────────────────────────────────────────────
# Pin a model (or any flag) — put it in `args`, which is always passed
# ──────────────────────────────────────────────────────────────────────────
#
# friring is model-neutral; to force a model, bake the flag into `args`. E.g.
# a claude variant pinned to Opus, kept alongside the default `claude` entry:
#
# [[agents]]
# name = "claude-opus"
# command = "claude"
# args = ["--model", "opus"]    # always-on flag
# resume_args = ["--resume", "{id}"]
# fork_args = ["--resume", "{id}", "--fork-session", "-n", "{name}"]
# new_session_args = ["--session-id", "{id}", "-n", "{name}"]
#
# Set `default = "claude-opus"` at the top of this file to make it the default.
"#;

/// Path to the agent-definition config file:
/// `~/.config/friring/agents.toml` (sibling of `config.toml`).
pub fn agents_config_path() -> Option<PathBuf> {
    crate::paths::config_file().map(|p| p.with_file_name("agents.toml"))
}

/// Parse the built-in definitions. Infallible in practice (the const is a
/// valid document); falls back to an empty registry if that ever changes.
pub fn builtin_registry() -> AgentRegistry {
    toml::from_str(BUILTIN_AGENTS_TOML).unwrap_or(AgentRegistry {
        config_version: None,
        default: String::new(),
        agents: Vec::new(),
    })
}

/// Load the agent registry, seeding the config file with built-ins when it is
/// absent. Any read/parse error degrades gracefully to the built-in registry
/// so the TUI always starts with at least the bundled agents; the warnings are
/// logged here (headless callers) — the TUI uses
/// [`load_or_seed_with_warnings`] to surface them in the status bar too.
pub fn load_or_seed() -> AgentRegistry {
    let (registry, warnings) = load_or_seed_with_warnings();
    for w in &warnings {
        tracing::warn!("{w}");
    }
    registry
}

/// [`load_or_seed`], also returning user-facing warnings for anything that
/// silently degraded (parse error → built-ins, seed failure, …).
pub fn load_or_seed_with_warnings() -> (AgentRegistry, Vec<String>) {
    let Some(path) = agents_config_path() else {
        return (
            builtin_registry(),
            vec!["Could not resolve agents.toml path; using built-in agents".into()],
        );
    };

    if !path.exists() {
        return seed_agents_toml(&path);
    }

    match std::fs::read_to_string(&path) {
        Ok(contents) => parse_agents_toml(&contents),
        Err(e) => (
            builtin_registry(),
            vec![format!("Failed to read agents.toml: {e}")],
        ),
    }
}

/// Write the bundled agents.toml on first run, degrading to the built-in
/// registry (with a warning) if the dir or file can't be created.
fn seed_agents_toml(path: &std::path::Path) -> (AgentRegistry, Vec<String>) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return (
                builtin_registry(),
                vec![format!("Failed to create config dir for agents.toml: {e}")],
            );
        }
    }
    if let Err(e) = std::fs::write(path, BUILTIN_AGENTS_TOML) {
        return (
            builtin_registry(),
            vec![format!("Failed to seed agents.toml: {e}")],
        );
    }
    tracing::info!(path = %path.display(), "Seeded agents.toml with built-in agents");
    (builtin_registry(), Vec::new())
}

/// Top-level keys read directly by the resilient parser; anything else at the
/// document root is reported as an unknown field (typo / stale key).
const KNOWN_TOP_LEVEL_KEYS: [&str; 3] = ["config_version", "default", "agents"];

/// Parse agents.toml contents **resiliently**, entry by entry: one malformed
/// `[[agents]]` block is skipped (with a warning naming it) instead of
/// discarding every agent the user defined. We fall back to the built-in
/// registry only when the document is syntactically broken (unrecoverable) or
/// yields no usable agents at all.
///
/// This is deliberately more forgiving than `friring-cli config validate`,
/// which still strict-parses the whole document — `validate` is the diagnostic
/// that tells you to fix the file, while the TUI degrades gracefully so a
/// single typo never strands you on the built-ins.
fn parse_agents_toml(contents: &str) -> (AgentRegistry, Vec<String>) {
    // A genuine syntax error can't be recovered per entry — fall back to built-ins.
    let table: toml::Table = match contents.parse() {
        Ok(table) => table,
        Err(e) => {
            return (
                builtin_registry(),
                vec![format!(
                    "agents.toml: {}; using built-in agents",
                    compact_toml_error(&e.to_string())
                )],
            );
        }
    };

    let mut warnings = Vec::new();
    for key in table.keys() {
        if !KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            warnings.push(format!("agents.toml: unknown field `{key}` (ignored)"));
        }
    }

    let config_version = table
        .get("config_version")
        .and_then(toml::Value::as_integer)
        .map(|v| v as u32);
    let default = table
        .get("default")
        .and_then(toml::Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut agents = Vec::new();
    match table.get("agents") {
        Some(toml::Value::Array(entries)) => {
            for (index, entry) in entries.iter().enumerate() {
                if let Some(agent) = deserialize_agent(entry, index, &mut warnings) {
                    agents.push(agent);
                }
            }
        }
        Some(_) => warnings.push("agents.toml: `agents` must be an array of tables".into()),
        None => {}
    }

    if agents.is_empty() {
        warnings.push("agents.toml has no usable agents; using built-in agents".into());
        return (builtin_registry(), warnings);
    }

    (
        AgentRegistry {
            config_version,
            default,
            agents,
        },
        warnings,
    )
}

/// Deserialize one `[[agents]]` entry, returning `None` (and pushing a warning
/// that names the entry) when it is malformed so the caller can skip it. Unknown
/// fields within a valid entry are reported but kept.
fn deserialize_agent(
    entry: &toml::Value,
    index: usize,
    warnings: &mut Vec<String>,
) -> Option<AgentDef> {
    // Label by name when present (the useful identifier), else by position.
    let label = entry
        .get("name")
        .and_then(toml::Value::as_str)
        .map(|n| format!("\"{n}\""))
        .unwrap_or_else(|| format!("#{index}"));

    let mut unknowns = Vec::new();
    let result: Result<AgentDef, _> =
        serde_ignored::deserialize(entry.clone(), |path| unknowns.push(path.to_string()));

    match result {
        Ok(agent) => {
            for field in unknowns {
                warnings.push(format!(
                    "agents.toml: agent {label}: unknown field `{field}` (ignored)"
                ));
            }
            Some(agent)
        }
        Err(e) => {
            warnings.push(format!(
                "agents.toml: skipped agent {label}: {}",
                compact_toml_error(&e.to_string())
            ));
            None
        }
    }
}

/// Parse a TOML config document leniently, reporting every unknown field by
/// path instead of failing on it. Stale keys from older friring versions and
/// typos both surface as warnings without stranding the user on defaults; a
/// real syntax/type error still fails the parse.
pub(crate) fn parse_toml_reporting_unknown<T: serde::de::DeserializeOwned>(
    contents: &str,
    file_label: &str,
) -> Result<(T, Vec<String>), toml::de::Error> {
    let mut warnings = Vec::new();
    let de = toml::de::Deserializer::parse(contents)?;
    let value = serde_ignored::deserialize(de, |path| {
        warnings.push(format!("{file_label}: unknown field `{path}` (ignored)"));
    })?;
    Ok((value, warnings))
}

/// Collapse a (possibly multi-line) toml error to "<position>: <message>" for
/// compact status-bar display. toml errors render as a header line with the
/// position, a source snippet, then the message — keep the first and last
/// meaningful lines and drop the snippet in between.
pub(crate) fn compact_toml_error(s: &str) -> String {
    let lines: Vec<&str> = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('|') && !l.starts_with(char::is_numeric))
        .collect();
    match (lines.first(), lines.last()) {
        (Some(first), Some(last)) if first != last => format!("{first}: {last}"),
        (Some(first), _) => (*first).to_string(),
        _ => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_parses_and_has_claude_default() {
        let reg = builtin_registry();
        assert_eq!(reg.default, "claude");
        assert!(reg.get("claude").is_some());
        assert!(reg.get("codex").is_some());
        assert!(reg.get("antigravity").is_some());
        assert!(reg.get("opencode").is_some());
        assert!(reg.get("aider").is_some());
        assert!(reg.get("copilot").is_some());
        assert!(reg.get("vibe").is_some());

        // Claude pins a friring id and resumes/forks by it.
        let claude = reg.get("claude").unwrap();
        assert!(!claude.resume_args.is_empty());
        assert!(!claude.resume_latest);
        assert!(claude.resume_args.iter().any(|t| t.contains("{id}")));

        // codex/opencode resume + fork via id-less, cwd-scoped flags.
        let codex = reg.get("codex").unwrap();
        assert_eq!(codex.resume_args, ["resume", "--last"]);
        assert_eq!(codex.fork_args, ["fork", "--last"]);
        assert!(codex.resume_latest);
        let opencode = reg.get("opencode").unwrap();
        assert_eq!(opencode.fork_args, ["--continue", "--fork"]);
        assert!(opencode.resume_latest);

        // antigravity/aider/copilot resume their latest session but have no
        // fork group.
        for name in ["antigravity", "aider", "copilot"] {
            let a = reg.get(name).unwrap();
            assert!(a.resume_latest, "{name} should resume latest");
            assert!(!a.resume_args.is_empty(), "{name} needs resume_args");
            assert!(a.fork_args.is_empty(), "{name} has no fork");
        }

        // No non-claude resume/fork token may carry a {id} placeholder — these
        // agents can't be addressed by a friring-known id.
        for name in ["codex", "antigravity", "opencode", "aider", "copilot"] {
            let a = reg.get(name).unwrap();
            assert!(
                !a.resume_args
                    .iter()
                    .chain(&a.fork_args)
                    .any(|t| t.contains("{id}")),
                "{name} must use id-less resume/fork flags"
            );
        }
    }

    /// The seeded declarations are what the credential strategies read
    /// (`docs/SANDBOX.md` §Credentials): friring bakes in no agent knowledge, so
    /// everything a sandboxed launch needs about an agent has to be *here*, as
    /// data, and be internally consistent.
    #[test]
    fn the_seeded_sandbox_declarations_describe_their_credentials() {
        use crate::session::SandboxAuth;

        let reg = builtin_registry();
        for (name, config_dir_env, state_dir, credential_file, token) in [
            (
                "claude",
                "CLAUDE_CONFIG_DIR",
                "~/.claude",
                "~/.claude/.credentials.json",
                "ANTHROPIC_API_KEY",
            ),
            (
                "codex",
                "CODEX_HOME",
                "~/.codex",
                "~/.codex/auth.json",
                "OPENAI_API_KEY",
            ),
        ] {
            let sandbox = reg
                .get(name)
                .unwrap_or_else(|| panic!("{name} is a built-in"))
                .sandbox
                .as_ref()
                .unwrap_or_else(|| panic!("{name} declares a sandbox block"));
            // `auto`, not a strategy: the boundary decides, and a place cannot
            // give an agent the host's credential store however it is declared.
            assert_eq!(sandbox.auth, SandboxAuth::Auto, "{name}");
            assert_eq!(sandbox.config_dir_env.as_deref(), Some(config_dir_env));
            assert_eq!(sandbox.state_dir.as_deref(), Some(state_dir));
            assert_eq!(sandbox.credential_file.as_deref(), Some(credential_file));
            assert_eq!(sandbox.secret_env, [token], "{name}");
            // The state directory has to be the one the credential file lives
            // in, or a relocated agent would sign in somewhere friring does not
            // look for the login.
            assert!(credential_file.starts_with(state_dir), "{name}");
            // A place with no credential has to be able to say what to type.
            assert!(
                sandbox
                    .login_fallback
                    .as_deref()
                    .is_some_and(|text| !text.trim().is_empty()),
                "{name} needs a login fallback"
            );
            assert!(sandbox.writeback, "{name} refreshes its own credential");
        }

        // Nothing shipped may assert that its credential file is safe to copy:
        // both vendors here issue rotating single-use refresh tokens, and a copy
        // logs one of the two holders out on the first refresh (ADR-28). Turning
        // this on is a decision a user makes for an agent they know.
        for agent in &reg.agents {
            if let Some(sandbox) = &agent.sandbox {
                assert!(
                    !sandbox.seed_file_supported,
                    "{} must not ship claiming its credential is copyable",
                    agent.name
                );
            }
        }
    }

    /// What the seeded registry offers a *container*: the configuration to
    /// carry in, and the settings friring writes over it.
    ///
    /// Every entry has to be `~`-anchored (a place's only writable surface is
    /// its own synthetic home), none of them may be a credential (ADR-28 — a
    /// copy and the original invalidate each other), and an enforced template
    /// that will not render is a registry typo that writes nothing, which is
    /// worth catching here rather than in a container.
    #[test]
    fn the_seeded_projection_declarations_are_projectable() {
        let reg = builtin_registry();
        let mut projecting = 0;
        for agent in &reg.agents {
            let Some(sandbox) = &agent.sandbox else {
                continue;
            };
            for entry in &sandbox.copy_in {
                projecting += 1;
                assert!(entry.starts_with("~/"), "{}: '{entry}'", agent.name);
                assert!(!entry.contains(".."), "{}: '{entry}'", agent.name);
                // The one file that must never be listed is the one this agent
                // declares as its own credential.
                assert_ne!(
                    Some(entry.as_str()),
                    sandbox.credential_file.as_deref(),
                    "{} lists its own credential file for projection",
                    agent.name
                );
            }
            for enforced in &sandbox.enforced {
                assert_eq!(enforced.invalid(), None, "{}", agent.name);
                let rendered = enforced
                    .render(&["/fabricated/repo".to_string()])
                    .unwrap_or_else(|e| panic!("{} enforced template: {e}", agent.name));
                assert!(!rendered.trim().is_empty(), "{}", agent.name);
            }
        }
        assert!(
            projecting > 0,
            "no built-in declares anything to project, so a container would start empty"
        );
    }

    /// The seed must carry copy-pasteable examples (add-your-own-agent +
    /// pin-a-model) but keep them commented, so parsing still yields exactly
    /// the seven built-ins — a fresh install boots on pure defaults.
    #[test]
    fn seed_documents_examples_yet_stays_builtin_only() {
        for marker in [
            "Add your own agent",
            "Pin a model",
            "claude-opus",
            "[\"--model\", \"opus\"]",
        ] {
            assert!(
                BUILTIN_AGENTS_TOML.contains(marker),
                "agents.toml seed must document example '{marker}'"
            );
        }
        // Examples are commented, so the seed parses to just the built-ins.
        let reg = builtin_registry();
        assert_eq!(reg.default, "claude");
        assert_eq!(reg.agents.len(), 7, "examples must stay commented out");
        assert!(
            reg.get("claude-opus").is_none(),
            "example must not register"
        );
    }

    #[test]
    fn load_or_seed_writes_file_when_absent_then_reads_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());

        let path = agents_config_path().unwrap();
        assert!(!path.exists());

        let reg = load_or_seed();
        assert_eq!(reg.default, "claude");
        assert!(path.exists(), "agents.toml should have been seeded");

        // Second call reads the seeded file and yields the same registry.
        let reg2 = load_or_seed();
        assert_eq!(reg, reg2);
    }

    #[test]
    fn load_or_seed_falls_back_on_malformed_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());

        let path = agents_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "this is not = valid toml {{{").unwrap();

        let reg = load_or_seed();
        assert_eq!(reg.default, "claude");
    }

    #[test]
    fn load_or_seed_reports_unknown_field_but_keeps_agents() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());

        let path = agents_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Typo'd field: `resumeargs` instead of `resume_args`. The user's
        // agents must stay in effect (stale keys from older friring versions
        // are common); the warning names the bad key.
        std::fs::write(
            &path,
            "default = \"mine\"\n[[agents]]\nname = \"mine\"\ncommand = \"x\"\nresumeargs = []\n",
        )
        .unwrap();

        let (reg, warnings) = load_or_seed_with_warnings();
        assert_eq!(reg.default, "mine", "user agents must stay in effect");
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("resumeargs"),
            "warning must name the unknown field: {}",
            warnings[0]
        );
    }

    #[test]
    fn one_malformed_entry_is_skipped_and_the_rest_survive() {
        // The real-world footgun: `args` given a bare string instead of an
        // array. Previously this failed the whole document and stranded the
        // user on the built-ins; now only the bad entry is dropped.
        let toml = r#"
default = "claude"

[[agents]]
name = "claude"
command = "claude"

[[agents]]
name = "claude-bypass"
command = "claude"
args = "--dangerously-skip-permissions"

[[agents]]
name = "shepherd"
command = "claude"
args = ["--model", "claude-haiku-4-5"]
"#;
        let (reg, warnings) = parse_agents_toml(toml);

        // The two valid agents load; the bad one is skipped.
        assert_eq!(reg.names(), vec!["claude", "shepherd"]);
        assert!(reg.get("claude-bypass").is_none());
        // And the warning names the skipped agent so it's actionable.
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(
            warnings[0].contains("claude-bypass") && warnings[0].contains("skipped"),
            "warning must name the skipped agent: {}",
            warnings[0]
        );
    }

    #[test]
    fn activity_provider_is_optional_explicit_and_orthogonal_to_hook_schema() {
        use crate::session::activity::ProviderKind;

        let toml = r#"
default = "ringwriter"

[[agents]]
name = "ringwriter"
command = "/opt/ring/bin/ringwriter"
activity_provider = "claude-code"

[[agents]]
name = "hooked"
command = "rebrand"
hook_schema = "claude"

[[agents]]
name = "legacy"
command = "claude"
"#;
        let (reg, warnings) = parse_agents_toml(toml);
        assert!(warnings.is_empty(), "got: {warnings:?}");

        // Declared: the provider is the entry's, whatever the command is named.
        let ring = reg.get("ringwriter").expect("declared entry loads");
        assert_eq!(ring.activity_provider, Some(ProviderKind::Claude));
        assert_eq!(
            ring.resolved_activity_provider(),
            Some(ProviderKind::Claude)
        );

        // hook_schema alone declares nothing about the transcript format.
        let hooked = reg.get("hooked").expect("entry loads");
        assert_eq!(hooked.activity_provider, None);
        assert_eq!(hooked.resolved_activity_provider(), None);

        // Omitted: the pre-existing basename inference, unchanged.
        let legacy = reg.get("legacy").expect("entry loads");
        assert_eq!(legacy.activity_provider, None);
        assert_eq!(
            legacy.resolved_activity_provider(),
            Some(ProviderKind::Claude)
        );
    }

    #[test]
    fn an_invalid_activity_provider_is_diagnosed_and_siblings_survive() {
        // Same contract every malformed entry gets: the bad one is named and
        // skipped, the rest of the file still loads. `friring-cli config
        // validate` strict-parses the same document and fails on it there.
        let toml = r#"
default = "claude"

[[agents]]
name = "claude"
command = "claude"

[[agents]]
name = "ringwriter"
command = "ringwriter"
activity_provider = "ringwriter-format"

[[agents]]
name = "shepherd"
command = "codex"
"#;
        let (reg, warnings) = parse_agents_toml(toml);

        assert_eq!(reg.names(), vec!["claude", "shepherd"]);
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(
            warnings[0].contains("ringwriter") && warnings[0].contains("skipped"),
            "warning must name the skipped agent: {}",
            warnings[0]
        );
        // The diagnostic tells the user what they could have written, rather
        // than only that they were wrong.
        assert!(
            warnings[0].contains("claude-code"),
            "warning must enumerate the valid providers: {}",
            warnings[0]
        );
    }

    #[test]
    fn malformed_entry_without_name_is_labeled_by_index() {
        let toml = r#"
[[agents]]
name = "ok"
command = "ok"

[[agents]]
command = "missing-name"
"#;
        let (reg, warnings) = parse_agents_toml(toml);
        assert_eq!(reg.names(), vec!["ok"]);
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
        assert!(
            warnings[0].contains("#1"),
            "nameless entry should be labeled by index: {}",
            warnings[0]
        );
    }

    #[test]
    fn all_entries_malformed_falls_back_to_builtins() {
        let toml = "[[agents]]\nargs = \"oops\"\n";
        let (reg, warnings) = parse_agents_toml(toml);
        assert_eq!(reg.default, "claude", "should fall back to built-ins");
        assert!(reg.get("codex").is_some());
        assert!(
            warnings.iter().any(|w| w.contains("no usable agents")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn agents_key_of_wrong_type_falls_back_to_builtins() {
        // `agents` must be an array of tables; a scalar is a type error that
        // leaves zero usable agents.
        let (reg, warnings) = parse_agents_toml("agents = 3\n");
        assert_eq!(reg.default, "claude");
        assert!(
            warnings.iter().any(|w| w.contains("array of tables")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn syntax_error_still_falls_back_to_builtins() {
        let (reg, warnings) = parse_agents_toml("this is not = valid toml {{{");
        assert_eq!(reg.default, "claude");
        assert!(warnings.iter().any(|w| w.contains("using built-in agents")));
    }

    #[test]
    fn unknown_top_level_key_is_reported_but_agents_survive() {
        let toml = "stray = true\n[[agents]]\nname = \"mine\"\ncommand = \"x\"\n";
        let (reg, warnings) = parse_agents_toml(toml);
        assert_eq!(reg.names(), vec!["mine"]);
        assert!(
            warnings.iter().any(|w| w.contains("stray")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn compact_toml_error_keeps_position_and_message() {
        // A type error (string field given an integer) still fails the parse.
        let err = toml::from_str::<AgentRegistry>("default = 1\n").unwrap_err();
        let compact = compact_toml_error(&err.to_string());
        assert!(compact.contains("string"), "got: {compact}");
        assert!(!compact.contains('\n'), "must be one line: {compact}");
    }

    #[test]
    fn load_or_seed_reads_custom_agent() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::paths::TestPathGuard::new(temp.path());

        let path = agents_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "default = \"mine\"\n[[agents]]\nname = \"mine\"\ncommand = \"my-agent\"\n",
        )
        .unwrap();

        let reg = load_or_seed();
        assert_eq!(reg.default, "mine");
        assert_eq!(reg.get("mine").unwrap().command, "my-agent");
    }
}
