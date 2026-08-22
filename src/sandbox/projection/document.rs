//! Structured configuration documents, and the two edits the lint pass makes to
//! them.
//!
//! An agent's settings file is where its configuration stops being inert
//! content and starts naming things on the host: a hook command, a status-line
//! command, a credential helper, an MCP server's binary, a plugin directory. So
//! a projected document is not copied — it is parsed, every string leaf in it is
//! examined, and each host reference is either **rewritten** to where it lands
//! inside the boundary or the entry carrying it is **dropped**.
//!
//! Both formats agents read settings from are handled through one value tree
//! (`serde_json::Value`), so the walk, the drop rule and the merge are written
//! once. TOML crosses into that tree and back through serde, which loses
//! comments and orders keys — the projected copy is friring's rendering of the
//! user's configuration, not a byte copy of it, and it says so wherever it is
//! reported.
//!
//! **The drop rule is structural, never schema-driven** — friring bakes in no
//! agent knowledge. Removing only the offending *string* would leave a hook with
//! no command or an MCP server with no binary, which is a broken agent rather
//! than a projected one. So the smallest whole entry around it goes:
//!
//! | The string is… | What is removed |
//! |---|---|
//! | an array element (`plugins.repos[1]`) | that element |
//! | a member of a nested object (`mcpServers.docs.command`) | that object's own slot (`mcpServers.docs`) |
//! | a member of the root (`awsAuthRefresh`) | that member |

use serde::Deserialize as _;
use serde_json::Value;

use crate::session::SettingsFormat;

/// One step down a document.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Key(String),
    Index(usize),
}

/// Where a string sits, as a reader of the file would name it:
/// `hooks.PreToolUse[0].hooks[0].command`.
pub fn render_path(path: &[Step]) -> String {
    let mut out = String::new();
    for step in path {
        match step {
            Step::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Step::Index(index) => out.push_str(&format!("[{index}]")),
        }
    }
    if out.is_empty() {
        "<document>".to_string()
    } else {
        out
    }
}

/// Parse `text` in `format` into the shared value tree.
///
/// # Errors
///
/// The text is not a document of that format, or (TOML only) carries a value
/// serde cannot represent.
pub fn parse(text: &str, format: SettingsFormat) -> Result<Value, String> {
    match format {
        SettingsFormat::Json => serde_json::from_str(text).map_err(|e| e.to_string()),
        SettingsFormat::Toml => {
            let value: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
            serde_json::to_value(value).map_err(|e| e.to_string())
        }
    }
}

/// Render a value tree back out in `format`.
///
/// # Errors
///
/// The tree cannot be written in that format — a TOML document has no way to
/// spell a `null`, for one, and refusing beats writing a file the agent then
/// fails to parse on startup.
pub fn render(value: &Value, format: SettingsFormat) -> Result<String, String> {
    match format {
        SettingsFormat::Json => serde_json::to_string_pretty(value)
            .map(|mut text| {
                text.push('\n');
                text
            })
            .map_err(|e| e.to_string()),
        SettingsFormat::Toml => {
            let value = toml::Value::deserialize(value.clone()).map_err(|e| e.to_string())?;
            toml::to_string(&value).map_err(|e| e.to_string())
        }
    }
}

/// Which format a projected file is read as, from its name.
///
/// Deliberately narrow: anything else is *content* (instructions, skills,
/// commands) and is projected verbatim. A document friring cannot parse is one
/// it cannot lint, and guessing at a format would mean editing a file by
/// pattern-matching its text.
pub fn format_of(file_name: &str) -> Option<SettingsFormat> {
    match file_name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("json") => Some(SettingsFormat::Json),
        Some("toml") => Some(SettingsFormat::Toml),
        _ => None,
    }
}

/// Every string leaf in `value`, with the path that names it.
///
/// Owned rather than borrowed because the caller edits the tree while it
/// decides, and a borrow of the thing being edited is exactly what it cannot
/// hold.
pub fn strings(value: &Value) -> Vec<(Vec<Step>, String)> {
    let mut out = Vec::new();
    walk(value, &mut Vec::new(), &mut out);
    out
}

fn walk(value: &Value, at: &mut Vec<Step>, out: &mut Vec<(Vec<Step>, String)>) {
    match value {
        Value::String(text) => out.push((at.clone(), text.clone())),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                at.push(Step::Index(index));
                walk(item, at, out);
                at.pop();
            }
        }
        Value::Object(members) => {
            for (key, member) in members {
                at.push(Step::Key(key.clone()));
                walk(member, at, out);
                at.pop();
            }
        }
        _ => {}
    }
}

fn at_mut<'v>(value: &'v mut Value, path: &[Step]) -> Option<&'v mut Value> {
    let mut cursor = value;
    for step in path {
        cursor = match (step, cursor) {
            (Step::Key(key), Value::Object(members)) => members.get_mut(key)?,
            (Step::Index(index), Value::Array(items)) => items.get_mut(*index)?,
            _ => return None,
        };
    }
    Some(cursor)
}

/// Replace the string at `path`. A no-op when the path no longer resolves,
/// which is how a rewrite behind an already-dropped entry ends.
pub fn set_string(value: &mut Value, path: &[Step], text: String) {
    if let Some(slot) = at_mut(value, path) {
        *slot = Value::String(text);
    }
}

/// The entry [`drop_entry`] would remove for a string at `path` — the smallest
/// whole entry around it (see the table in the module docs).
pub fn entry_of(path: &[Step]) -> &[Step] {
    match path.last() {
        None => path,
        Some(Step::Index(_)) => path,
        Some(Step::Key(_)) if path.len() >= 2 => &path[..path.len() - 1],
        Some(Step::Key(_)) => path,
    }
}

/// Remove the entry around `path`. Idempotent, and a no-op when the path no
/// longer resolves — dropping a parent first is expected, not an error.
pub fn drop_entry(value: &mut Value, path: &[Step]) {
    let entry = entry_of(path);
    let Some((last, parent)) = entry.split_last() else {
        return;
    };
    match (last, at_mut(value, parent)) {
        (Step::Key(key), Some(Value::Object(members))) => {
            members.remove(key);
        }
        (Step::Index(index), Some(Value::Array(items))) if *index < items.len() => {
            items.remove(*index);
        }
        _ => {}
    }
}

/// Deep-merge `overlay` over `base`; `overlay` wins every collision.
///
/// What makes an enforced-settings layer the *highest-precedence* one: the
/// user's projected file is the base, friring's document is the overlay, and no
/// key the overlay names can be taken back by what was projected under it.
pub fn merge(base: Value, overlay: Value) -> Value {
    match (base, overlay) {
        (Value::Object(mut base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                let merged = match base.remove(&key) {
                    Some(existing) => merge(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (_, overlay) => overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_settings() -> Value {
        serde_json::json!({
            "statusLine": { "type": "command", "command": "/Users/u/bin/status.sh" },
            "mcpServers": {
                "docs": { "command": "/opt/homebrew/bin/uvx", "args": ["docs-mcp"] },
                "local": { "command": "npx", "args": ["-y", "server"] }
            },
            "hooks": {
                "PreToolUse": [
                    { "matcher": "", "hooks": [
                        { "type": "command", "command": "/Users/u/bin/audit.sh" },
                        { "type": "command", "command": "printf working" }
                    ] }
                ]
            },
            "plugins": { "repositories": ["/Users/u/plugins", "builtin"] },
            "model": "opus"
        })
    }

    fn path_of(document: &Value, needle: &str) -> Vec<Step> {
        strings(document)
            .into_iter()
            .find(|(_, text)| text == needle)
            .unwrap_or_else(|| panic!("{needle} is not in the document"))
            .0
    }

    #[test]
    fn every_string_is_found_and_named_the_way_a_reader_would() {
        let document = claude_settings();
        let named: Vec<String> = strings(&document)
            .iter()
            .map(|(path, _)| render_path(path))
            .collect();
        for expected in [
            "statusLine.command",
            "mcpServers.docs.command",
            "hooks.PreToolUse[0].hooks[0].command",
            "plugins.repositories[0]",
            "model",
        ] {
            assert!(named.contains(&expected.to_string()), "{named:?}");
        }
    }

    /// The whole point of the drop rule: what comes out is a document the agent
    /// can still read, with the offending entry gone rather than gutted.
    #[test]
    fn dropping_takes_the_smallest_whole_entry_around_the_string() {
        let mut document = claude_settings();
        for needle in [
            "/Users/u/bin/status.sh",
            "/opt/homebrew/bin/uvx",
            "/Users/u/bin/audit.sh",
            "/Users/u/plugins",
        ] {
            let path = path_of(&document, needle);
            drop_entry(&mut document, &path);
        }

        // A nested object's own slot goes, not just its `command`.
        assert!(document.get("statusLine").is_none());
        assert!(document["mcpServers"].get("docs").is_none());
        // …and the sibling that named no host path stays.
        assert_eq!(document["mcpServers"]["local"]["command"], "npx");
        // An array element goes alone, leaving the rest of the array.
        assert_eq!(
            document["plugins"]["repositories"],
            serde_json::json!(["builtin"])
        );
        // A hook entry goes as one object, leaving the hook that is fine.
        let hooks = &document["hooks"]["PreToolUse"][0]["hooks"];
        assert_eq!(hooks.as_array().map(Vec::len), Some(1));
        assert_eq!(hooks[0]["command"], "printf working");
        // Nothing else moved.
        assert_eq!(document["model"], "opus");

        // Idempotent: a second drop of a path that has gone changes nothing.
        let before = document.clone();
        drop_entry(
            &mut document,
            &[Step::Key("statusLine".into()), Step::Key("command".into())],
        );
        assert_eq!(document, before);
    }

    #[test]
    fn a_root_member_is_dropped_alone_rather_than_taking_the_document() {
        let mut document = serde_json::json!({ "helper": "/Users/u/bin/h.sh", "model": "opus" });
        let path = path_of(&document, "/Users/u/bin/h.sh");
        assert_eq!(entry_of(&path), path.as_slice());
        drop_entry(&mut document, &path);
        assert_eq!(document, serde_json::json!({ "model": "opus" }));
    }

    #[test]
    fn a_rewrite_replaces_exactly_the_string_it_names() {
        let mut document = claude_settings();
        let path = path_of(&document, "/Users/u/bin/status.sh");
        set_string(
            &mut document,
            &path,
            "/home/agent/bin/status.sh".to_string(),
        );
        assert_eq!(
            document["statusLine"]["command"],
            "/home/agent/bin/status.sh"
        );
        assert_eq!(document["statusLine"]["type"], "command");
    }

    #[test]
    fn both_formats_cross_into_one_tree_and_back() {
        let toml_text = "[projects.\"/repo\"]\ntrust_level = \"trusted\"\n";
        let tree = parse(toml_text, SettingsFormat::Toml).unwrap();
        assert_eq!(tree["projects"]["/repo"]["trust_level"], "trusted");
        let rendered = render(&tree, SettingsFormat::Toml).unwrap();
        assert_eq!(parse(&rendered, SettingsFormat::Toml).unwrap(), tree);

        let json = parse("{\"a\":{\"b\":1}}", SettingsFormat::Json).unwrap();
        let rendered = render(&json, SettingsFormat::Json).unwrap();
        assert_eq!(parse(&rendered, SettingsFormat::Json).unwrap(), json);

        assert!(parse("{ not json", SettingsFormat::Json).is_err());
        assert!(parse("= =", SettingsFormat::Toml).is_err());
        assert_eq!(format_of("settings.json"), Some(SettingsFormat::Json));
        assert_eq!(format_of("config.toml"), Some(SettingsFormat::Toml));
        assert_eq!(format_of("CLAUDE.md"), None);
        assert_eq!(format_of("Makefile"), None);
    }

    /// A TOML document mixing values and tables at one level is the shape a
    /// naive serializer emits in an order TOML cannot read back.
    #[test]
    fn a_toml_document_survives_values_beside_tables() {
        let text = "model = \"gpt-5\"\n[projects.\"/repo\"]\ntrust_level = \"trusted\"\n";
        let tree = parse(text, SettingsFormat::Toml).unwrap();
        let rendered = render(&tree, SettingsFormat::Toml).unwrap();
        let round_tripped = parse(&rendered, SettingsFormat::Toml).unwrap();
        assert_eq!(round_tripped, tree, "{rendered}");
    }

    #[test]
    fn the_overlay_wins_every_collision_and_keeps_what_it_does_not_name() {
        let base = serde_json::json!({
            "trust": { "asked": true, "folders": ["/a"] },
            "model": "opus"
        });
        let overlay = serde_json::json!({ "trust": { "asked": false } });
        let merged = merge(base, overlay);
        assert_eq!(
            merged,
            serde_json::json!({
                "trust": { "asked": false, "folders": ["/a"] },
                "model": "opus"
            })
        );
        // A scalar overlay replaces a whole subtree rather than merging into it.
        assert_eq!(
            merge(
                serde_json::json!({ "a": { "b": 1 } }),
                serde_json::json!({ "a": 2 })
            ),
            serde_json::json!({ "a": 2 })
        );
    }
}
