//! What an agent's configuration becomes when friring materialises it somewhere
//! the agent's own machine is not.
//!
//! Two things live here, both pure data or pure text, because `session` is the
//! dependency sink and both halves of friring need them:
//!
//! - [`EnforcedSettings`], the `[[agents.<name>.sandbox.enforced]]` declaration
//!   — the highest-precedence configuration layer friring writes *inside* a
//!   boundary, so a repository-level file cannot override the orchestrator's
//!   intent. It is a template rather than a schema, for the same reason
//!   [`AgentDef`](crate::session::AgentDef)'s argument groups are: friring bakes
//!   in no agent knowledge, so the *user* writes the document their agent reads
//!   and friring only fills in what it alone knows — the paths the boundary
//!   granted.
//! - [`rewrite_status_signals_for_tmux`], which turns a friring-managed hook
//!   command into one that reports through a tmux pane option. An agent that is
//!   not on this filesystem cannot run `friring-cli session signal` — the binary
//!   need not be there, and the database it writes is what ADR-29 keeps outside
//!   every boundary — but `tmux set-option -p` needs no socket, no pane id and
//!   no identity, and the local control-mode connection receives the change
//!   through its [`REMOTE_HOOK_SUBSCRIPTION`](crate::session::REMOTE_HOOK_SUBSCRIPTION)
//!   subscription. A remote host and a sandbox place are the same problem.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Prefix of every friring-managed hook command; the state word
/// (`idle`/`working`/`blocked`/`done`) follows it directly.
///
/// The bundled hook payloads (`extensions/hooks/`) keep this spelling verbatim,
/// which is what lets every consumer find friring's own commands inside an
/// agent's config file without parsing that agent's schema.
pub const STATUS_SIGNAL_MARKER: &str = "friring-cli session signal --state ";

/// Rewrite friring-managed hook commands to report through a tmux pane option:
/// `friring-cli session signal --state <s>` →
/// `tmux set-option -p @friring_state <s>`.
///
/// Prefix-replace keeps the state word and everything trailing it (`|| true`,
/// `;; esac; true`, the `if [ -n "$FRIRING_SIGNAL_FILE" ]` branch the file
/// channel added) intact, and the replacement contains no `"` or `\`, so a
/// byte-level replace over JSON or TOML text stays valid in either. Idempotent,
/// and a no-op for content carrying no marker.
pub fn rewrite_status_signals_for_tmux(contents: &str) -> String {
    contents.replace(
        STATUS_SIGNAL_MARKER,
        &format!("tmux set-option -p {} ", super::REMOTE_HOOK_STATE_OPTION),
    )
}

/// The document format friring renders an enforced-settings layer in.
///
/// Only the two formats coding agents actually read their settings from. The
/// format decides how the rendered text is validated before it is written: a
/// template that does not parse is a registry typo, and finding it here beats
/// killing the agent's pane on startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SettingsFormat {
    #[default]
    Json,
    Toml,
}

impl fmt::Display for SettingsFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Json => "json",
            Self::Toml => "toml",
        })
    }
}

/// One configuration file friring writes inside the boundary, over whatever the
/// user's own configuration projected to the same path.
///
/// ```toml
/// [[agents.codex.sandbox.enforced]]
/// path = "~/.codex/config.toml"
/// format = "toml"
/// per_path = '''
/// [projects."{path}"]
/// trust_level = "trusted"
/// '''
///
/// [[agents.<name>.sandbox.enforced]]
/// path = "~/.my-agent/settings.json"
/// format = "json"
/// content = '''
/// { "trustedWorkspaces": {workspaces} }
/// '''
/// ```
///
/// Both keys above are the *vendor's*, which is why they are declarations
/// rather than code: friring bakes in no agent knowledge, and a key friring
/// guessed would be a silent no-op the user could not see. Only the codex block
/// ships seeded, because it is the one this repository's design notes record.
///
/// The two placeholders are everything friring contributes, and both name the
/// same thing — the paths the profile granted, which is the only fact the
/// registry cannot know: `{workspaces}` expands to a list of them (`["/a",
/// "/b"]`, valid in both formats), and `{path}` expands to one of them, once per
/// path, in a `per_path` block repeated for each. Both are escaped for a quoted
/// string, so the template author writes the quotes.
///
/// Pre-seeded workspace trust is what this exists for: several agents prompt on
/// first run in a fresh home, and a place always *is* a fresh home.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnforcedSettings {
    /// Where the file lands **inside** the boundary, written `~`-anchored: a
    /// place's only writable surface is its synthetic home, so an absolute path
    /// would name something in the image that friring cannot write.
    pub path: String,
    #[serde(default)]
    pub format: SettingsFormat,
    /// Written once, at the top. `{workspaces}` expands to the granted paths.
    #[serde(default)]
    pub content: Option<String>,
    /// Repeated once per granted path, with `{path}` substituted.
    #[serde(default)]
    pub per_path: Option<String>,
}

impl EnforcedSettings {
    /// Why this declaration cannot be applied, or `None` when it can.
    ///
    /// Checked before anything is rendered so the reason names the declaration
    /// rather than the document that came out of it.
    pub fn invalid(&self) -> Option<String> {
        let path = self.path.trim();
        if path.is_empty() {
            return Some("declares no 'path'".to_string());
        }
        if !path.starts_with("~/") {
            return Some(format!(
                "'{path}' is not written '~/…'; a boundary's only writable surface is its \
                 synthetic home, so an absolute path would name a file in the image friring \
                 cannot write"
            ));
        }
        // The next three mirror `sandbox::projection::unsafe_relative`, which
        // re-checks this path when the document is written. The rules are
        // restated rather than shared because `session` may not reference
        // `sandbox`; keeping them in step is what stops a declaration passing
        // here and failing the whole launch there instead of being reported as
        // one host-only finding.
        if path.contains('\\') {
            return Some(format!(
                "'{path}' is not a POSIX path; a '\\' names a different file inside the boundary \
                 than the declaration reads as"
            ));
        }
        if path.contains('\0') {
            return Some(format!("'{path}' contains a NUL byte"));
        }
        if path
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == ".." || part == ".")
        {
            return Some(format!("'{path}' carries an empty, '.' or '..' component"));
        }
        if self.content.is_none() && self.per_path.is_none() {
            return Some(format!(
                "'{path}' declares neither 'content' nor 'per_path', so there is nothing to write"
            ));
        }
        if self.format == SettingsFormat::Json && self.per_path.is_some() {
            return Some(format!(
                "'{path}' is json and declares 'per_path'; two JSON documents cannot be \
                 concatenated — put the granted paths in 'content' with the '{{workspaces}}' \
                 placeholder"
            ));
        }
        None
    }

    /// The document this declaration renders to for `workspaces`.
    ///
    /// # Errors
    ///
    /// The declaration is [`invalid`](Self::invalid). Nothing else can fail:
    /// substitution is textual, and whether the result *parses* is the caller's
    /// check, because only the caller knows what to do about it.
    pub fn render(&self, workspaces: &[String]) -> Result<String, String> {
        if let Some(reason) = self.invalid() {
            return Err(reason);
        }
        let list = format!(
            "[{}]",
            workspaces
                .iter()
                .map(|path| format!("\"{}\"", escape_quoted(path)))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut out = String::new();
        if let Some(content) = &self.content {
            push_block(&mut out, &content.replace("{workspaces}", &list));
        }
        if let Some(per_path) = &self.per_path {
            for path in workspaces {
                push_block(&mut out, &per_path.replace("{path}", &escape_quoted(path)));
            }
        }
        Ok(out)
    }

    /// The path this lands at inside the boundary, relative to the synthetic
    /// home (`~/.claude/settings.json` → `.claude/settings.json`).
    ///
    /// `None` when the declaration is [`invalid`](Self::invalid).
    pub fn home_relative(&self) -> Option<&str> {
        if self.invalid().is_some() {
            return None;
        }
        self.path.trim().strip_prefix("~/")
    }
}

/// Append `block` and make sure it ends a line, so two templates concatenate
/// into two TOML tables rather than one run-on line.
fn push_block(out: &mut String, block: &str) {
    out.push_str(block);
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

/// One path as the *content* of a quoted string in either format.
///
/// JSON strings and TOML basic strings share an escape vocabulary, and both
/// accept `\uXXXX` for anything else — so one function covers both and a path
/// friring substitutes can never end the string it was written into.
fn escape_quoted(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> EnforcedSettings {
        EnforcedSettings {
            path: "~/.claude/settings.json".into(),
            format: SettingsFormat::Json,
            content: Some("{ \"trustedFolders\": {workspaces} }".into()),
            per_path: None,
        }
    }

    fn codex() -> EnforcedSettings {
        EnforcedSettings {
            path: "~/.codex/config.toml".into(),
            format: SettingsFormat::Toml,
            content: None,
            per_path: Some("[projects.\"{path}\"]\ntrust_level = \"trusted\"".into()),
        }
    }

    #[test]
    fn the_granted_paths_are_the_only_thing_friring_fills_in() {
        let workspaces = ["/repo".to_string(), "/srv/shared".to_string()];
        assert_eq!(
            claude().render(&workspaces).unwrap(),
            "{ \"trustedFolders\": [\"/repo\", \"/srv/shared\"] }\n"
        );
        assert_eq!(
            codex().render(&workspaces).unwrap(),
            "[projects.\"/repo\"]\ntrust_level = \"trusted\"\n[projects.\"/srv/shared\"]\n\
             trust_level = \"trusted\"\n"
        );
        // No granted paths is a document with no entries, not a failure: a
        // profile can legitimately grant nothing but its own scratch.
        assert_eq!(codex().render(&[]).unwrap(), "");
        assert_eq!(
            claude().render(&[]).unwrap(),
            "{ \"trustedFolders\": [] }\n"
        );
    }

    /// A path is substituted into a string the template author quoted, so it can
    /// never end that string early — which would otherwise let a directory name
    /// write arbitrary settings.
    #[test]
    fn a_substituted_path_cannot_break_out_of_its_quotes() {
        let hostile = vec!["/repo\", \"evil\": true, \"x\": \"".to_string()];
        let rendered = claude().render(&hostile).unwrap();
        assert!(rendered.contains("\\\""), "{rendered}");
        let parsed: serde_json::Value =
            serde_json::from_str(&rendered).expect("still one document");
        assert!(parsed.get("evil").is_none(), "{parsed}");

        let rendered = codex().render(&["/a\\b\"c".to_string()]).unwrap();
        let parsed: toml::Value = toml::from_str(&rendered).expect("still valid TOML");
        assert!(parsed["projects"].get("/a\\b\"c").is_some(), "{parsed}");
    }

    #[test]
    fn a_declaration_that_cannot_be_applied_says_why_before_rendering() {
        let cases = [
            (
                EnforcedSettings {
                    path: "/etc/claude-code/managed-settings.json".into(),
                    ..claude()
                },
                "synthetic home",
            ),
            (
                EnforcedSettings {
                    path: "~/../../etc/passwd".into(),
                    ..claude()
                },
                "'..' component",
            ),
            // The four the projection writer refuses: caught here, so they are
            // one host-only finding rather than a failed launch.
            (
                EnforcedSettings {
                    path: "~/.codex//config.toml".into(),
                    ..claude()
                },
                "empty, '.' or '..' component",
            ),
            (
                EnforcedSettings {
                    path: "~/.codex/".into(),
                    ..claude()
                },
                "empty, '.' or '..' component",
            ),
            (
                EnforcedSettings {
                    path: "~/a\\b".into(),
                    ..claude()
                },
                "not a POSIX path",
            ),
            (
                EnforcedSettings {
                    path: "~/.claude/settings.json\0".into(),
                    ..claude()
                },
                "NUL byte",
            ),
            (
                EnforcedSettings {
                    content: None,
                    ..claude()
                },
                "nothing to write",
            ),
            (
                EnforcedSettings {
                    per_path: Some("x".into()),
                    ..claude()
                },
                "cannot be concatenated",
            ),
            (
                EnforcedSettings {
                    path: String::new(),
                    ..claude()
                },
                "no 'path'",
            ),
        ];
        for (declaration, needle) in cases {
            let reason = declaration.invalid().expect("refused");
            assert!(reason.contains(needle), "{reason}");
            assert_eq!(declaration.render(&[]).unwrap_err(), reason);
            assert!(declaration.home_relative().is_none());
        }
        assert_eq!(claude().home_relative(), Some(".claude/settings.json"));
    }

    #[test]
    fn the_status_rewrite_keeps_the_shell_around_the_command() {
        let payload = "if [ -n \"$FRIRING_SIGNAL_FILE\" ]; then printf 'done\\n' >> \
                       \"$FRIRING_SIGNAL_FILE\" || true; else friring-cli session signal --state \
                       done || true; fi";
        let rewritten = rewrite_status_signals_for_tmux(payload);
        assert!(rewritten.contains("tmux set-option -p @friring_state done || true; fi"));
        assert!(!rewritten.contains("friring-cli session signal"));
        // Idempotent, and inert for text carrying no marker.
        assert_eq!(rewrite_status_signals_for_tmux(&rewritten), rewritten);
        assert_eq!(
            rewrite_status_signals_for_tmux("nothing here"),
            "nothing here"
        );
    }

    /// The marker is a promise about text friring ships, so it is checked
    /// against that text rather than against itself.
    ///
    /// Every payload the bundled hooks extension carries has to spell the
    /// command exactly this way: a reordered flag would leave the rewrite
    /// silently inert, and a place-backed or remote session would report no
    /// state with nothing on screen saying why.
    #[test]
    fn every_shipped_hook_payload_spells_the_marker_the_same_way() {
        const PAYLOADS: &[(&str, &str)] = &[
            (
                "claude.json",
                include_str!("../../extensions/hooks/claude.json"),
            ),
            (
                "codex-hooks.json",
                include_str!("../../extensions/hooks/codex-hooks.json"),
            ),
            (
                "antigravity-hooks.json",
                include_str!("../../extensions/hooks/antigravity-hooks.json"),
            ),
            (
                "copilot-hooks.json",
                include_str!("../../extensions/hooks/copilot-hooks.json"),
            ),
            (
                "vibe-hooks.toml",
                include_str!("../../extensions/hooks/vibe-hooks.toml"),
            ),
        ];
        for (name, payload) in PAYLOADS {
            // Counted on the command's own shape (`session signal --state`)
            // rather than on the whole marker, so a drifted spelling — an
            // inserted flag, a reordered one — is a mismatch rather than a
            // payload that quietly stops being rewritten. Prose mentioning the
            // command without a state is not a command.
            let occurrences = payload.matches("session signal --state").count();
            assert!(occurrences > 0, "{name} carries no status hook at all");
            assert_eq!(
                payload.matches(STATUS_SIGNAL_MARKER).count(),
                occurrences,
                "a `session signal` command in {name} does not match the marker"
            );
            let rewritten = rewrite_status_signals_for_tmux(payload);
            assert!(
                !rewritten.contains("session signal --state"),
                "{name} keeps a command that cannot run outside this filesystem"
            );
            assert_eq!(
                rewritten
                    .matches(&format!(
                        "tmux set-option -p {}",
                        crate::session::REMOTE_HOOK_STATE_OPTION
                    ))
                    .count(),
                occurrences,
                "{name} lost a status report in the rewrite"
            );
        }
    }

    /// The registry spelling the seeded `agents.toml` uses, parsed exactly as
    /// the loader parses it.
    ///
    /// A projection declaration is only as good as the block a user can copy, so
    /// the block is here rather than only in a comment: a rename or a serde
    /// attribute that stops it parsing fails a test instead of silently
    /// projecting nothing.
    #[test]
    fn the_seeded_registry_spelling_parses_into_a_declaration() {
        let toml = r#"
name = "codex"
command = "codex"

[sandbox]
auth = "host-passthrough"
state_rw = ["~/.codex"]
copy_in = ["~/.codex/AGENTS.md", "~/.codex/prompts"]
bypass = ["--dangerously-bypass-approvals-and-sandbox"]

[[sandbox.enforced]]
path = "~/.codex/config.toml"
format = "toml"
per_path = """
[projects."{path}"]
trust_level = "trusted"
"""
"#;
        let def: crate::session::AgentDef = toml::from_str(toml).expect("the block parses");
        let sandbox = def.sandbox.expect("a declared block");
        assert_eq!(sandbox.copy_in, ["~/.codex/AGENTS.md", "~/.codex/prompts"]);
        let enforced = sandbox.enforced.first().expect("one enforced layer");
        assert_eq!(enforced.invalid(), None);
        assert_eq!(enforced.home_relative(), Some(".codex/config.toml"));
        let rendered = enforced.render(&["/repo".to_string()]).unwrap();
        let parsed: toml::Value = toml::from_str(&rendered).expect("a TOML document");
        assert_eq!(
            parsed["projects"]["/repo"]["trust_level"].as_str(),
            Some("trusted")
        );
    }

    #[test]
    fn the_declaration_round_trips_through_the_registry_format() {
        let toml = r#"
path = "~/.codex/config.toml"
format = "toml"
per_path = "[projects.\"{path}\"]\ntrust_level = \"trusted\""
"#;
        let parsed: EnforcedSettings = toml::from_str(toml).unwrap();
        assert_eq!(parsed, codex());
        assert_eq!(parsed.format, SettingsFormat::Toml);
        // `format` defaults to json, which is what most agents read.
        let bare: EnforcedSettings =
            toml::from_str("path = \"~/x.json\"\ncontent = \"{}\"").unwrap();
        assert_eq!(bare.format, SettingsFormat::Json);
        assert!(bare.invalid().is_none());
    }
}
