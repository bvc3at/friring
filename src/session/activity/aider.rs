//! Aider activity provider — normalizes a repo's `.aider.chat.history.md`
//! transcript into [`ActivityEvent`]s.
//!
//! Aider has **no** structured session log (unlike Claude Code / Vibe): the
//! only readable retrospective is a human-readable, append-only markdown
//! transcript at `<git_root|cwd>/.aider.chat.history.md`, written by
//! `io.append_chat_history()`. It is a line-oriented format with load-bearing
//! prefixes that aider's own reader round-trips
//! (`utils.split_chat_history_markdown`), so they are safe to parse:
//!
//! - `# aider chat started at <ts>` — per-session header (session start only;
//!   individual turns are **not** timestamped, so events carry no `ts_ms`).
//! - `#### ` — a user input line: raw slash-commands (`/run`, `!`, `/test`,
//!   `/lint`, `/add`, `/web`, …) or a typed prompt.
//! - `> ` — a tool/status blockquote (`Applied edit to …`, `Added … to the
//!   chat.`, `Scraping …`, `Main model: …`, `Tokens: … received. …`).
//! - anything else — the assistant's free markdown (fenced SEARCH/REPLACE edit
//!   blocks live here); a fallback bucket we don't mine, since `> Applied edit
//!   to` is the authoritative edit marker.
//!
//! Because a model's reply can contain a fenced code block whose lines start
//! with `> ` or `#### `, [`AiderScan`] tracks ``` fences and treats fenced
//! content as assistant text — a guard aider's own naive parser lacks.
//!
//! Coverage is partial by construction (see the per-category anchors below):
//! commands, edits, context-adds ("reads"), and web fetches are recoverable;
//! command **output**, implicit file reads, and web bodies are **not** in this
//! file (they only exist in the opt-in `.aider.llm.history`), so `result_head`
//! is never populated and `ok` is set only from the explicit "Applied edit to"
//! success marker.
//!
//! Verified against aider 0.86.2 / 0.86.3.dev (July 2026). Defensive like every
//! provider: unknown shapes are skipped, never errors.

use super::{head, ActionKind, ActivityEvent, ActivityMeta};

/// Cap for title text taken from the first typed prompt.
const NOTE_MAX: usize = 160;

/// Streaming scanner over one `.aider.chat.history.md`. Same contract as
/// [`super::claude::ClaudeScan`]: feed line-aligned chunks in file order; read
/// [`events`](Self::events) / [`meta`](Self::meta) between feeds. The transcript
/// is per-repo and shared by every aider run in it (aider has no on-disk session
/// id), so the accumulated stream spans all sessions recorded in the file.
#[derive(Debug, Clone, Default)]
pub struct AiderScan {
    pub events: Vec<ActivityEvent>,
    pub meta: ActivityMeta,
    /// Inside a ``` fenced block — its lines are assistant content, so the
    /// `> ` / `#### ` prefixes there are literal text, not markers.
    in_fence: bool,
}

impl AiderScan {
    /// Ingest the next line-aligned chunk of the transcript.
    pub fn ingest(&mut self, chunk: &str) {
        for raw in chunk.lines() {
            if raw.trim_start().starts_with("```") {
                self.in_fence = !self.in_fence;
                continue;
            }
            if self.in_fence {
                continue;
            }
            // Trailing two-space markdown hard-breaks (and any `\r`) are noise.
            let line = raw.trim_end();
            if let Some(body) = line.strip_prefix("#### ") {
                self.ingest_user(body.trim_end());
            } else if let Some(body) = line.strip_prefix("> ") {
                self.ingest_tool(body.trim_end());
            }
            // `# …` headers and un-prefixed assistant markdown record no action.
        }
    }

    /// A `#### ` user line. Shell commands become [`ActionKind::Command`];
    /// other slash-commands are bookkeeping or captured from their `> `
    /// confirmation lines (so not emitted here); a plain typed prompt seeds the
    /// title once.
    fn ingest_user(&mut self, body: &str) {
        if body.is_empty() {
            return;
        }
        if let Some(cmd) = shell_command(body) {
            self.emit(ActionKind::Command, cmd, None, None);
            return;
        }
        if body.starts_with('/') || body.starts_with('!') {
            return;
        }
        if self.meta.title.is_none() {
            self.meta.title = Some(head(body, NOTE_MAX));
        }
    }

    /// A `> ` tool/status blockquote. Each category is anchored on its single
    /// most reliable marker to avoid double-counting the same action from both
    /// its command line and its confirmation.
    fn ingest_tool(&mut self, body: &str) {
        if let Some(path) = body.strip_prefix("Applied edit to ") {
            let path = path.trim();
            if !path.is_empty() {
                self.emit(ActionKind::Edit, path.to_string(), None, Some(true));
            }
        } else if let Some(url) = body.strip_prefix("Scraping ") {
            let url = url.strip_suffix("...").unwrap_or(url).trim();
            if !url.is_empty() {
                self.emit(ActionKind::WebFetch, url.to_string(), None, None);
            }
        } else if let Some(rest) = body.strip_prefix("Added ") {
            if let Some(file) = added_file(rest) {
                self.emit(ActionKind::Read, file, None, None);
            }
        } else if let Some(model) = body.strip_prefix("Main model: ") {
            self.meta.model = Some(model_name(model));
        } else if let Some(rest) = body.strip_prefix("Tokens: ") {
            if let Some(n) = received_tokens(rest) {
                let total = self.meta.output_tokens.unwrap_or(0);
                self.meta.output_tokens = Some(total + n);
            }
        }
    }

    fn emit(&mut self, kind: ActionKind, detail: String, note: Option<String>, ok: Option<bool>) {
        self.events.push(ActivityEvent {
            ts_ms: None,
            kind,
            detail,
            note: note.filter(|n| !n.trim().is_empty()),
            result_head: None,
            ok,
            origin: None,
            minor: false,
            dur_ms: None,
        });
    }
}

/// The shell command a `#### ` body ran, if any. Distinguishes commands the
/// **user** actually ran (`/run`, `!`, `/test`, `/lint`) from LLM-*suggested*
/// shell in fenced assistant blocks (which never reach this function). Bare
/// forms with no argument fall back to the command name.
fn shell_command(body: &str) -> Option<String> {
    if let Some(rest) = body.strip_prefix("/run ") {
        return non_empty(rest);
    }
    if let Some(rest) = body.strip_prefix('!') {
        return non_empty(rest);
    }
    if let Some(rest) = body.strip_prefix("/test") {
        return Some(word_or(rest, "test"));
    }
    if let Some(rest) = body.strip_prefix("/lint") {
        return Some(word_or(rest, "lint"));
    }
    None
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// `rest` trimmed, or `fallback` when the command carried no argument.
fn word_or(rest: &str, fallback: &str) -> String {
    let t = rest.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

/// The file path from a `> Added <path> to the chat[ (read-only)].` line, or
/// `None` for the unrelated `> Added N lines of output to the chat.` (command
/// output) confirmation.
fn added_file(rest: &str) -> Option<String> {
    let mid = rest
        .strip_suffix(" to the chat (read-only).")
        .or_else(|| rest.strip_suffix(" to the chat."))?;
    if mid.is_empty() || mid.ends_with("lines of output") {
        return None;
    }
    Some(mid.to_string())
}

/// The model id from a `Main model: <model> with <fmt> edit format …` line.
fn model_name(s: &str) -> String {
    s.split(" with ").next().unwrap_or(s).trim().to_string()
}

/// The received-token count from a `> Tokens: … <n> received. Cost: …` line —
/// the per-response output-token figure (aider prints it human-formatted, e.g.
/// `2.3k`).
fn received_tokens(rest: &str) -> Option<u64> {
    let idx = rest.find(" received")?;
    let tok = rest[..idx].split_whitespace().next_back()?;
    parse_human_number(tok)
}

/// Parse aider's human-formatted counts: `412`, `2.3k`, `23k`, `1.2M`.
fn parse_human_number(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        'k' | 'K' => (&s[..s.len() - 1], 1_000f64),
        'm' | 'M' => (&s[..s.len() - 1], 1_000_000f64),
        _ => (s, 1f64),
    };
    let v: f64 = num.trim().parse().ok()?;
    if v < 0.0 {
        return None;
    }
    Some((v * mult).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> AiderScan {
        let mut sc = AiderScan::default();
        sc.ingest(s);
        sc
    }

    #[test]
    fn classifies_commands_edits_reads_and_web() {
        // Verbatim marker shapes from the research samples (hard-break spaces
        // included on the `> ` lines).
        let md = concat!(
            "\n# aider chat started at 2026-07-12 03:16:28\n\n",
            "> /usr/local/bin/aider --model gpt-4o --message hi  \n",
            "> Aider v0.86.2  \n",
            "> Main model: gpt-4o with diff edit format  \n",
            "> Added app.py to the chat.  \n",
            "#### add a docstring to app.py\n",
            "#### /run pytest -q\n",
            "> Added 14 lines of output to the chat.  \n",
            "#### !ls -la\n",
            "#### /test\n",
            "#### /read-only src/config.py\n",
            "> Added src/config.py to the chat (read-only).  \n",
            "#### /web https://example.com/docs\n",
            "> Scraping https://example.com/docs...  \n",
            "> Applied edit to app.py  \n",
            "> Commit 1a2b3c4 docs: add docstring  \n",
            "> Tokens: 2.3k sent, 412 received. Cost: $0.01 message, $0.02 session.  \n",
        );
        let s = scan(md);
        let kinds: Vec<ActionKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ActionKind::Read,     // > Added app.py to the chat.
                ActionKind::Command,  // /run pytest -q
                ActionKind::Command,  // !ls -la
                ActionKind::Command,  // /test (bare)
                ActionKind::Read,     // > Added src/config.py (read-only)
                ActionKind::WebFetch, // > Scraping …
                ActionKind::Edit,     // > Applied edit to app.py
            ]
        );
        assert_eq!(s.events[0].detail, "app.py");
        assert_eq!(s.events[1].detail, "pytest -q");
        assert_eq!(s.events[2].detail, "ls -la");
        assert_eq!(s.events[3].detail, "test");
        assert_eq!(s.events[4].detail, "src/config.py");
        assert_eq!(s.events[5].detail, "https://example.com/docs");
        assert_eq!(s.events[6].detail, "app.py");
        assert_eq!(s.events[6].ok, Some(true));
        // Every event is untimestamped (no per-record timestamps on disk).
        assert!(s.events.iter().all(|e| e.ts_ms.is_none()));
        // Meta scraped from the session header block + usage line.
        assert_eq!(s.meta.title.as_deref(), Some("add a docstring to app.py"));
        assert_eq!(s.meta.model.as_deref(), Some("gpt-4o"));
        assert_eq!(s.meta.output_tokens, Some(412));
    }

    #[test]
    fn fenced_search_replace_block_is_not_mined_for_markers() {
        // The assistant's edit block embeds lines that look like markers; the
        // fence guard must keep them out of the event stream. Only the trailing
        // `> Applied edit to` (outside the fence) counts.
        let md = concat!(
            "Here is the edit:\n",
            "app.py\n",
            "```python\n",
            "#### this is not a user prompt\n",
            "> Applied edit to /decoy.py\n",
            "<<<<<<< SEARCH\n",
            "def foo():\n",
            "=======\n",
            "def foo():\n",
            "    \"\"\"Return foo.\"\"\"\n",
            ">>>>>>> REPLACE\n",
            "```\n",
            "> Applied edit to app.py  \n",
        );
        let s = scan(md);
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, ActionKind::Edit);
        assert_eq!(s.events[0].detail, "app.py");
        // The decoy prompt inside the fence never became the title.
        assert_eq!(s.meta.title, None);
    }

    #[test]
    fn output_confirmation_is_not_a_file_read() {
        let s = scan("> Added 14 lines of output to the chat.  \n");
        assert!(s.events.is_empty());
    }

    #[test]
    fn tokens_accumulate_across_turns_and_parse_human_units() {
        let s = scan(concat!(
            "> Tokens: 2.3k sent, 1.1k cache write, 500 cache hit, 412 received. Cost: $0.01.  \n",
            "> Tokens: 900 sent, 1.2k received. Cost: $0.02.  \n",
        ));
        assert_eq!(s.events.len(), 0);
        assert_eq!(s.meta.output_tokens, Some(412 + 1200));
    }

    #[test]
    fn human_number_parsing() {
        assert_eq!(parse_human_number("412"), Some(412));
        assert_eq!(parse_human_number("2.3k"), Some(2300));
        assert_eq!(parse_human_number("23k"), Some(23000));
        assert_eq!(parse_human_number("1.2M"), Some(1_200_000));
        assert_eq!(parse_human_number("nope"), None);
        assert_eq!(parse_human_number(""), None);
    }

    #[test]
    fn malformed_and_unknown_lines_are_skipped() {
        let s = scan(concat!(
            "\n",
            "# aider chat started at not-a-date\n",
            "> \n",
            "> Some future status line we don't parse\n",
            "#### \n",
            "#### /model gpt-4o\n", // bookkeeping slash-command → skipped
            "#### /drop app.py\n",  // bookkeeping → skipped
            "just some assistant prose\n", // un-prefixed → assistant bucket, ignored
            "> Applied edit to \n", // empty path → skipped
        ));
        assert!(s.events.is_empty());
        // No `#### ` typed prompt appeared (only slash-commands), so no title.
        assert_eq!(s.meta.title, None);
    }

    #[test]
    fn model_name_strips_edit_format_suffix() {
        assert_eq!(model_name("gpt-4o with diff edit format"), "gpt-4o");
        assert_eq!(
            model_name("claude-sonnet-4 with diff-fenced edit format, prompt cache"),
            "claude-sonnet-4"
        );
        assert_eq!(model_name("bare-model"), "bare-model");
    }
}
