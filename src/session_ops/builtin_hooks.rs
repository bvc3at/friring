//! The built-in **hooks** extension: wires each coding agent's lifecycle hooks
//! to `friring-cli session signal` so sessions report `working`/`blocked`/`done`
//! back to friring (see the hooks-driven `SessionStatus`). For **remote**
//! sessions the same hook file is shipped with its commands rewritten to a tmux
//! pane user option (`rewrite_hook_signals_for_remote`) — the local TUI
//! receives those over its control-mode subscription.
//!
//! Unlike user extensions (which are fetched from a source on demand, ADR-20),
//! this one ships **embedded** in the binary and is **auto-activated by default**
//! so the default agent has its hook pre-configured with zero setup. It is
//! delivered through the ordinary extension machinery: the embedded assets are
//! materialized into a stable local dir, then [`install_extension`] installs them
//! from there — so all the install/heal/uninstall logic is shared.
//!
//! Opt out with `friring-cli extension deactivate hooks`, which records an
//! opt-out flag so startup self-heal won't resurrect it.

use std::path::PathBuf;

use crate::storage::Database;

use super::install_extension;

/// The extension name (matches `extensions/hooks/extension.toml`).
pub const HOOKS_EXTENSION_NAME: &str = "hooks";

const MANIFEST: &str = include_str!("../../extensions/hooks/extension.toml");
const CLAUDE_SETTINGS: &str = include_str!("../../extensions/hooks/claude.json");
const OPENCODE_PLUGIN: &str = include_str!("../../extensions/hooks/opencode-status.js");
const ANTIGRAVITY_HOOKS: &str = include_str!("../../extensions/hooks/antigravity-hooks.json");
const CODEX_HOOKS: &str = include_str!("../../extensions/hooks/codex-hooks.json");
const VIBE_HOOKS: &str = include_str!("../../extensions/hooks/vibe-hooks.toml");
const COPILOT_HOOKS: &str = include_str!("../../extensions/hooks/copilot-hooks.json");

/// Marker prefix of every friring-managed hook command; the state word
/// (`working`/`blocked`/`done`/`idle`) follows it directly.
const SIGNAL_MARKER: &str = "friring-cli session signal --state ";

/// Rewrite friring-managed hook commands for a **remote (real-tmux) host**:
/// `friring-cli session signal --state <s>` →
/// `tmux set-option -p @friring_state <s>`.
///
/// `friring-cli` can't signal from a remote host (it isn't installed there,
/// and it would write the host's own DB — never the one the local TUI reads).
/// A tmux **pane user option** can: inside a pane `set-option -p` needs no
/// socket, pane id, or identity (`$TMUX`/`$TMUX_PANE` are in the pane env),
/// and the local TUI's control-mode connection receives changes through its
/// [`crate::session::REMOTE_HOOK_SUBSCRIPTION`] format subscription. Applied
/// by the spawn-time materialization (`adapt_agent_args_for_remote`) to every
/// config file it ships. Prefix-replace keeps the state word and whatever
/// trails it (`|| true`, `;; esac; true`) intact; the replacement contains no
/// `"`/`\`, so a byte-level replace on JSON text is safe. Idempotent, and a
/// no-op for marker-free content.
pub(crate) fn rewrite_hook_signals_for_remote(contents: &str) -> String {
    contents.replace(
        SIGNAL_MARKER,
        &format!(
            "tmux set-option -p {} ",
            crate::session::REMOTE_HOOK_STATE_OPTION
        ),
    )
}

/// The hooks extension's home, under this build's resolved config dir
/// (`~/.config/friring/hooks` for a release build, `~/.config/friring-dev/hooks`
/// for a dev build) — so dev and release installs stay isolated and the injected
/// `--settings` path always points inside the same tree the binary uses.
fn hooks_home() -> Option<String> {
    crate::paths::config_file()
        .and_then(|p| p.parent().map(|d| d.join("hooks")))
        .map(|p| p.to_string_lossy().into_owned())
}

/// The exact `--settings` value the hooks extension injects into `claude`
/// (`<hooks_home>/claude.json`) — constructed identically to the manifest's
/// `["--settings", "{home}/claude.json"]` after `{home}` substitution, so it
/// **byte-matches** the flag the CC daemon captures and replays when it
/// backgrounds a session. The Claude Code activity scan uses it to attribute a
/// detached background worker back to this friring instance (see
/// `app::cc_activity`).
pub(crate) fn hooks_settings_path() -> Option<String> {
    hooks_home().map(|h| format!("{h}/claude.json"))
}

/// Point a **local claude** launch at a **per-session** `--settings` file —
/// `<hooks_home>/sessions/<agent_session_id>.json`, a symlink to the shared
/// `claude.json` — instead of the shared file directly.
///
/// Why: when Claude Code backgrounds a session as a detached daemon worker it
/// **replays** the origin session's `--settings` flag. A shared path can only be
/// disambiguated back to a friring session by a cwd heuristic (ambiguous for two
/// non-worktree sessions on one repo); a per-session path **names the exact
/// session**, so the activity view (`app::cc_activity`) attributes the worker's
/// workflow precisely. The symlink means zero content upkeep — it tracks the
/// shared `claude.json` the heal pass keeps current.
///
/// Best-effort and inert when it can't apply: returns `args` unchanged for an
/// agent that doesn't carry our shared hooks `--settings`, on a non-unix host
/// (no symlinks; the CC daemon is unix-only anyway), or on any fs error — the
/// scan then falls back to the shared-path + cwd match.
pub(crate) fn rewrite_settings_for_session(
    agent_session_id: &str,
    args: Vec<String>,
) -> Vec<String> {
    let Some(shared) = hooks_settings_path() else {
        return args;
    };
    // Only rewrite launches that actually carry our shared hooks --settings.
    if !args_carry_settings(&args, &shared) {
        return args;
    }
    let Some(per) = ensure_per_session_symlink(agent_session_id) else {
        return args;
    };
    rewrite_settings_value(args, &shared, &per)
}

/// Whether `args` contains a `--settings` flag (split or `=`-joined form) whose
/// value equals `shared`.
fn args_carry_settings(args: &[String], shared: &str) -> bool {
    let joined = format!("--settings={shared}");
    args.iter().enumerate().any(|(i, a)| {
        a == &joined || (a == "--settings" && args.get(i + 1).map(String::as_str) == Some(shared))
    })
}

/// Create (idempotently) the per-session settings symlink
/// `<hooks_home>/sessions/<id>.json → ../claude.json` and return its path.
/// `None` on non-unix or any fs error (caller then keeps the shared path).
fn ensure_per_session_symlink(agent_session_id: &str) -> Option<String> {
    #[cfg(not(unix))]
    {
        let _ = agent_session_id;
        None
    }
    #[cfg(unix)]
    {
        let dir = std::path::Path::new(&hooks_home()?).join("sessions");
        std::fs::create_dir_all(&dir).ok()?;
        let link = dir.join(format!("{agent_session_id}.json"));
        // Reuse an existing entry (respawn/restore keep the same id); the target
        // is relative so it resolves from the link's own dir regardless of cwd.
        if std::fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink("../claude.json", &link).ok()?;
        }
        Some(link.to_string_lossy().into_owned())
    }
}

/// Replace the value of the `--settings` flag that currently equals `shared`
/// with `per` (both `--settings X` and `--settings=X` forms). Only the matching
/// flag is touched, so a user's own unrelated `--settings` is left alone.
fn rewrite_settings_value(mut args: Vec<String>, shared: &str, per: &str) -> Vec<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--settings" {
            if args.get(i + 1).map(String::as_str) == Some(shared) {
                args[i + 1] = per.to_string();
                break;
            }
            i += 2;
            continue;
        }
        if args[i].strip_prefix("--settings=") == Some(shared) {
            args[i] = format!("--settings={per}");
            break;
        }
        i += 1;
    }
    args
}

/// Materialize the embedded hooks-extension assets into a stable local dir under
/// the data directory and return it, so [`install_extension`] can treat it as a
/// local source. Rewritten on every call so the assets track the binary.
fn materialize_source() -> Result<PathBuf, String> {
    let base = crate::paths::builtin_extensions_directory()
        .ok_or("cannot resolve builtin-extensions dir")?;
    let dir = base.join(HOOKS_EXTENSION_NAME);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let writes = [
        ("extension.toml", MANIFEST),
        ("claude.json", CLAUDE_SETTINGS),
        ("opencode-status.js", OPENCODE_PLUGIN),
        ("antigravity-hooks.json", ANTIGRAVITY_HOOKS),
        ("codex-hooks.json", CODEX_HOOKS),
        ("vibe-hooks.toml", VIBE_HOOKS),
        ("copilot-hooks.json", COPILOT_HOOKS),
    ];
    for (name, contents) in writes {
        let path = dir.join(name);
        // Skip the write when unchanged — this runs on every startup + 60s tick.
        if std::fs::read_to_string(&path).is_ok_and(|c| c == contents) {
            continue;
        }
        std::fs::write(&path, contents).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(dir)
}

/// Ensure the built-in hooks extension is installed + active, unless the user
/// opted out. Idempotent — safe to call at every TUI startup / automation tick;
/// it re-applies the agent patches, payload + external files, and re-stamps the
/// manifest so an upgrade refreshes the wiring. Returns human-readable status
/// lines (empty when there's nothing to report).
pub fn ensure_builtin_hooks_extension(db: &Database) -> Vec<String> {
    if db.builtin_hooks_opted_out().unwrap_or(false) {
        return Vec::new();
    }
    let dir = match materialize_source() {
        Ok(d) => d,
        Err(e) => return vec![format!("hooks extension: {e}")],
    };
    // Home lives under *this build's* config dir (`friring` vs `friring-dev`), so
    // a dev build patches its dev `agents.toml` with a `--settings` path inside
    // the dev tree — never the release config. Manifest `home` is ignored.
    let Some(home) = hooks_home() else {
        return vec!["hooks extension: cannot resolve config dir".into()];
    };

    // Migrate a stale install whose home points elsewhere (e.g. an earlier build
    // that used the release path): tear down its patches/files before reinstalling
    // under the correct home, so claude doesn't end up with two `--settings`.
    if let Some(existing) = crate::agent::extension_config::load_manifest(HOOKS_EXTENSION_NAME) {
        if existing.home.as_deref() != Some(home.as_str()) {
            let _ = super::uninstall_extension(db, HOOKS_EXTENSION_NAME, false);
        }
    }

    match install_extension(db, &dir.to_string_lossy(), Some(&home), false) {
        Ok(report) => {
            let mut msgs = Vec::new();
            if !report.agents_patched.is_empty() {
                msgs.push(format!(
                    "hooks: wired agent hooks for {}",
                    report.agents_patched.join(", ")
                ));
            }
            if !report.external_files_written.is_empty() {
                // One per agent whose own config dir we drop a file into
                // (opencode's plugin, vibe's hooks.toml) — only those present.
                msgs.push(format!(
                    "hooks: installed {} agent hook file(s)",
                    report.external_files_written.len()
                ));
            }
            msgs
        }
        Err(e) => vec![format!("hooks extension: {e}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `command` string in a claude-shaped hooks payload, flattened out of
    /// the `hooks.<Event>[].hooks[]` nesting.
    fn codex_hook_commands(payload: &serde_json::Value) -> Vec<&str> {
        payload["hooks"]
            .as_object()
            .expect("hooks object")
            .values()
            .filter_map(serde_json::Value::as_array)
            .flatten()
            .filter_map(|matcher| matcher["hooks"].as_array())
            .flatten()
            .filter_map(|hook| hook["command"].as_str())
            .collect()
    }

    // --- rewrite_hook_signals_for_remote tests ---

    #[test]
    fn remote_rewrite_replaces_every_signal_command() {
        let rewritten = rewrite_hook_signals_for_remote(CLAUDE_SETTINGS);
        // No local CLI reference survives, every state maps to the pane option.
        assert!(!rewritten.contains("friring-cli"));
        for state in ["idle", "working", "blocked", "done"] {
            assert!(
                rewritten.contains(&format!("tmux set-option -p @friring_state {state}")),
                "missing rewritten {state} command"
            );
        }
        // The surrounding hook shape (`|| true`, the blocked `case`) survives
        // the prefix replace, and the result is still valid JSON with all five
        // hook events.
        assert!(rewritten.contains("tmux set-option -p @friring_state idle || true"));
        assert!(rewritten.contains("tmux set-option -p @friring_state blocked ;;"));
        let json: serde_json::Value = serde_json::from_str(&rewritten).expect("still valid JSON");
        let hooks = json.get("hooks").and_then(|h| h.as_object()).unwrap();
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "Notification",
            "Stop",
        ] {
            assert!(hooks.contains_key(event), "missing hook event {event}");
        }
    }

    #[test]
    fn remote_rewrite_is_idempotent_and_passes_through() {
        let once = rewrite_hook_signals_for_remote(CLAUDE_SETTINGS);
        assert_eq!(rewrite_hook_signals_for_remote(&once), once);
        let unrelated = "default = \"claude\"\n[[agents]]\nname = \"claude\"\n";
        assert_eq!(rewrite_hook_signals_for_remote(unrelated), unrelated);
    }

    #[test]
    fn signal_marker_matches_shipped_hook_commands() {
        // Guard: a future edit to the hook asset that drifts from the marker
        // (e.g. reordering flags) would silently break the remote rewrite.
        // Every `session signal` occurrence in the claude asset must carry the
        // exact marker prefix.
        let occurrences = CLAUDE_SETTINGS.matches("session signal").count();
        assert_eq!(
            CLAUDE_SETTINGS.matches(SIGNAL_MARKER).count(),
            occurrences,
            "a `session signal` command in claude.json doesn't match SIGNAL_MARKER"
        );
        assert_eq!(
            occurrences, 5,
            "claude.json hook count changed — review the rewrite"
        );
    }

    #[test]
    fn embedded_assets_are_present() {
        assert!(MANIFEST.contains("name = \"hooks\""));
        assert!(CLAUDE_SETTINGS.contains("session signal --state working"));
        // The opencode plugin must carry the managed marker so uninstall can
        // safely remove it (see `is_user_modified`).
        assert!(OPENCODE_PLUGIN.contains("friring `extension install`"));
        // codex's hooks.json reports the full idle/working/done range.
        assert!(CODEX_HOOKS.contains("session signal --state idle"));
        // The vibe payload carries the signal marker (prune) and the managed
        // marker (external-file uninstall, see `is_user_modified`).
        assert!(VIBE_HOOKS.contains("friring-cli session signal"));
        assert!(VIBE_HOOKS.contains("friring `extension install`"));
        // The copilot payload carries the signal command and the managed marker
        // (external-file uninstall, see `is_user_modified`).
        assert!(COPILOT_HOOKS.contains("friring-cli session signal"));
        assert!(COPILOT_HOOKS.contains("friring `extension install`"));
    }

    #[test]
    fn embedded_manifest_parses_with_codex_vibe_and_antigravity_wiring() {
        // Parse the embedded manifest exactly as the installer does — this guards
        // the codex + antigravity config_merges (and the vibe external file) from
        // silently breaking the build.
        let def: crate::session::ExtensionDef =
            toml::from_str(MANIFEST).expect("embedded manifest parses");

        // codex now JSON-merges a claude-shaped hooks.json into ~/.codex/hooks.json
        // (idle/working/done) rather than the old `-c notify=…` agent patch.
        let codex = def
            .config_merges
            .iter()
            .find(|m| m.path.contains(".codex"))
            .expect("codex config merge present");
        assert_eq!(codex.source_path(), "codex-hooks.json");
        assert_eq!(codex.requires_dir.as_deref(), Some("~/.codex"));
        assert!(
            def.agent_patches.iter().all(|p| p.name != "codex"),
            "codex should no longer be wired via an agent patch"
        );

        // The codex payload is valid JSON, claude-shaped, and carries the marker.
        let codex_payload: serde_json::Value =
            serde_json::from_str(CODEX_HOOKS).expect("codex payload is valid JSON");
        assert!(codex_payload["hooks"]["SessionStart"].is_array());
        assert!(codex_payload["hooks"]["Stop"].is_array());
        assert!(CODEX_HOOKS.contains("friring-cli session signal"));

        // The event → state mapping is what the TUI shows for a codex session;
        // the e2e scenario can only observe the transitions loosely (a `working`
        // turn can be over before the poller looks), so pin it here.
        for (event, state) in [
            ("SessionStart", "idle"),
            ("UserPromptSubmit", "working"),
            ("PreToolUse", "working"),
            ("Stop", "done"),
        ] {
            let command = codex_payload["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("codex {event} hook has a command string"));
            assert!(
                command.contains(&format!("--state {state}")),
                "codex {event} hook must signal {state}: {command}"
            );
        }

        // codex parses hook stdout strictly: anything that isn't empty or a JSON
        // object it accepts is rejected after the command has already run, so
        // codex paints "hook returned invalid <event> JSON output" on every
        // event. `friring-cli` renders JSON whenever stdout isn't a TTY — which
        // a hook's piped stdout always is — so every codex command must discard
        // its output. Verified against codex-cli 0.145.0; asserted end-to-end by
        // the codex-text-turn e2e scenario.
        for command in codex_hook_commands(&codex_payload) {
            assert!(
                command.contains(">/dev/null 2>&1"),
                "codex hook command must silence its output: {command}"
            );
        }

        // vibe drops a managed hooks.toml into ~/.vibe/ (guarded by requires_dir).
        let vibe = def
            .external_files
            .iter()
            .find(|f| f.path.contains(".vibe"))
            .expect("vibe external file present");
        assert_eq!(vibe.source_path(), "vibe-hooks.toml");
        assert_eq!(vibe.requires_dir.as_deref(), Some("~/.vibe"));

        // The vibe payload is valid TOML with at least one hook entry, so a
        // typo can't ship a file vibe would reject.
        let vibe_payload: toml::Value =
            toml::from_str(VIBE_HOOKS).expect("vibe payload is valid TOML");
        assert!(
            vibe_payload["hooks"]
                .as_array()
                .is_some_and(|h| !h.is_empty()),
            "vibe payload should declare hook entries"
        );

        // antigravity (agy) shares gemini's ~/.gemini/settings.json for hooks.
        let antigravity = def
            .config_merges
            .iter()
            .find(|m| m.path.contains(".gemini"))
            .expect("antigravity config merge present");
        assert_eq!(antigravity.source_path(), "antigravity-hooks.json");
        assert_eq!(antigravity.requires_dir.as_deref(), Some("~/.gemini"));

        // The antigravity payload is valid JSON and carries the prune marker.
        let payload: serde_json::Value =
            serde_json::from_str(ANTIGRAVITY_HOOKS).expect("antigravity payload is valid JSON");
        // agy 1.0.9 adopted claude's hook schema; guard against a regression back
        // to the gemini-era `BeforeTool`/`AfterAgent` names (which agy never fires,
        // so working/done would silently stop reporting).
        for event in ["SessionStart", "PreToolUse", "Notification", "Stop"] {
            assert!(
                payload["hooks"][event].is_array(),
                "antigravity hook event {event} missing"
            );
        }
        assert!(payload["hooks"]["BeforeTool"].is_null());
        assert!(payload["hooks"]["AfterAgent"].is_null());
        assert!(ANTIGRAVITY_HOOKS.contains("friring-cli session signal"));

        // copilot drops a managed standalone file into ~/.copilot/hooks/ (guarded
        // by requires_dir; the hooks/ subdir is created on write).
        let copilot = def
            .external_files
            .iter()
            .find(|f| f.path.contains(".copilot"))
            .expect("copilot external file present");
        assert_eq!(copilot.source_path(), "copilot-hooks.json");
        assert_eq!(copilot.requires_dir.as_deref(), Some("~/.copilot"));

        // The copilot payload is valid JSON using copilot's own event schema, so a
        // typo can't ship a file copilot would reject.
        let copilot_payload: serde_json::Value =
            serde_json::from_str(COPILOT_HOOKS).expect("copilot payload is valid JSON");
        for event in [
            "sessionStart",
            "userPromptSubmitted",
            "preToolUse",
            "notification",
            "agentStop",
        ] {
            assert!(
                copilot_payload["hooks"][event].is_array(),
                "copilot hook event {event} missing"
            );
        }
    }

    #[test]
    fn hooks_home_derives_from_build_config_dir() {
        // Home must track the resolved config dir (so a dev build lands under
        // `friring-dev`, not the release tree) — never a hardcoded path.
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let home = hooks_home().expect("home resolves");
        let expected = crate::paths::config_file()
            .unwrap()
            .parent()
            .unwrap()
            .join("hooks");
        assert_eq!(std::path::Path::new(&home), expected);
    }

    #[test]
    fn opt_out_skips_install() {
        let db = Database::open_in_memory().unwrap();
        db.set_builtin_hooks_optout(true).unwrap();
        // With opt-out set, ensure is a no-op (no install attempted).
        assert!(ensure_builtin_hooks_extension(&db).is_empty());
    }

    // --- per-session --settings rewrite (Phase 2: exact daemon attribution) ---

    #[test]
    fn rewrite_settings_value_handles_both_forms_and_leaves_others() {
        let per = "/h/sessions/ID.json";
        // Split form.
        let split = ["--session-id", "x", "--settings", "/h/claude.json"].map(String::from);
        assert_eq!(
            rewrite_settings_value(split.to_vec(), "/h/claude.json", per),
            ["--session-id", "x", "--settings", per].map(String::from)
        );
        // `=`-joined form.
        let joined = ["--settings=/h/claude.json", "/seed"].map(String::from);
        assert_eq!(
            rewrite_settings_value(joined.to_vec(), "/h/claude.json", per),
            [format!("--settings={per}"), "/seed".into()]
        );
        // A user's unrelated --settings (different value) is untouched.
        let other = ["--settings", "/user/own.json"].map(String::from);
        assert_eq!(
            rewrite_settings_value(other.to_vec(), "/h/claude.json", per),
            other
        );
    }

    #[test]
    fn args_carry_settings_detects_shared_only() {
        let shared = "/h/claude.json";
        assert!(args_carry_settings(
            &["--settings", shared].map(String::from),
            shared
        ));
        assert!(args_carry_settings(
            &[format!("--settings={shared}")],
            shared
        ));
        assert!(!args_carry_settings(
            &["--settings", "/other.json"].map(String::from),
            shared
        ));
        assert!(!args_carry_settings(
            &["--model", "opus"].map(String::from),
            shared
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_settings_for_session_creates_symlink_and_repoints() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::paths::TestPathGuard::new(tmp.path());
        let shared = hooks_settings_path().expect("shared path");
        // Give the shared claude.json a real target so the symlink resolves.
        let home = std::path::Path::new(&shared).parent().unwrap();
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(&shared, "{}").unwrap();

        let sid = "11111111-2222-3333-4444-555555555555";
        let args = vec![
            "--session-id".into(),
            sid.into(),
            "--settings".into(),
            shared.clone(),
        ];
        let out = rewrite_settings_for_session(sid, args);

        let per = format!("{}/sessions/{sid}.json", home.display());
        assert_eq!(out, vec!["--session-id", sid, "--settings", &per]);
        // The symlink exists and resolves to the shared file.
        let link = std::path::Path::new(&per);
        assert!(std::fs::symlink_metadata(link).is_ok());
        assert_eq!(std::fs::read_to_string(link).unwrap(), "{}");

        // Idempotent: a second call reuses the same symlink and path.
        let out2 = rewrite_settings_for_session(sid, vec!["--settings".into(), shared.clone()]);
        assert_eq!(out2, vec!["--settings".to_string(), per]);
    }

    #[test]
    fn rewrite_settings_for_session_noop_without_hook_arg() {
        // An agent whose args don't carry our shared --settings is unchanged (no
        // symlink created) — no TestPathGuard needed since it returns early.
        let args = vec!["--model".to_string(), "opus".to_string()];
        assert_eq!(rewrite_settings_for_session("some-id", args.clone()), args);
    }
}
