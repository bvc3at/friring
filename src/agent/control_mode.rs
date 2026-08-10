//! Shared tmux control mode I/O infrastructure.
//!
//! This module contains the transport-agnostic control mode parsing and I/O types
//! shared by the single `TmuxBackend` across its local and SSH (`TmuxTransport`)
//! transports. The tmux control mode protocol is identical over either.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

/// Per-pane output channel capacity. Sized large enough to buffer heavy output
/// bursts; chunks are dropped (not blocked) when full to keep the reader thread alive.
pub const PANE_CHANNEL_CAPACITY: usize = 4096;

/// Maps pane IDs to sync senders for multi-instance output broadcast.
pub type PaneSendersMap = HashMap<String, Vec<SyncSender<Vec<u8>>>>;
pub type PaneSendersMapShared = Arc<Mutex<PaneSendersMap>>;

/// Response from a tmux control mode command.
pub struct CommandResponse {
    pub lines: Vec<String>,
    pub is_error: bool,
}

/// Parsed notification from the tmux control mode protocol.
#[derive(Debug, PartialEq)]
pub enum Notification {
    Output {
        pane_id: String,
        data: Vec<u8>,
    },
    Begin,
    End,
    Error,
    Pause {
        pane_id: String,
    },
    /// A `refresh-client -B` format subscription reported a changed value
    /// (tmux >= 3.2). Carries the remote hook state for
    /// [`crate::session::REMOTE_HOOK_SUBSCRIPTION`].
    SubscriptionChanged {
        name: String,
        pane_id: String,
        value: String,
    },
    Other(String),
}

/// Per-pane reader that receives output via an mpsc channel.
///
/// Implements `Read` so it plugs directly into the existing `Session::reader_loop`.
pub struct ControlModeReader {
    receiver: std::sync::mpsc::Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    pos: usize,
}

impl ControlModeReader {
    pub fn new(receiver: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            receiver,
            buffer: Vec::new(),
            pos: 0,
        }
    }
}

impl Read for ControlModeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Drain leftover buffered data first.
        if self.pos < self.buffer.len() {
            let remaining = &self.buffer[self.pos..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            if self.pos == self.buffer.len() {
                self.buffer.clear();
                self.pos = 0;
            }
            return Ok(n);
        }

        // Block until the next chunk arrives.
        match self.receiver.recv() {
            Ok(data) => {
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                if n < data.len() {
                    self.buffer = data;
                    self.pos = n;
                }
                Ok(n)
            }
            Err(_) => Ok(0), // Channel closed → EOF.
        }
    }
}

/// Max input bytes encoded into a single `send-keys -H` command. Each byte
/// becomes 3 chars (` XX`), so the command line stays ≈ `prefix + 3·512` ≈ 1.6
/// KB — well under tmux's per-command line limit (which would truncate a longer
/// line). `send_keys_commands` splits larger writes across multiple commands.
const SEND_KEYS_CHUNK_BYTES: usize = 512;

/// Split `buf` into the ordered `send-keys` command lines for `pane_id`.
///
/// Two encodings: real tmux uses the byte-exact `send-keys -H` hex flag (each
/// byte → two hex digits), chunked at `SEND_KEYS_CHUNK_BYTES` so no single
/// control-mode line gets over-long (tmux truncates those); the raw bytes —
/// including the bracketed-paste markers — span the chunks and the receiving
/// pane reassembles them. psmux (the native-Windows tmux clone) does **not**
/// implement `-H` — given `-H 62` it injects the literal text "62" instead of
/// byte 0x62, so the whole `-H` path is silently broken there — so
/// `psmux = true` selects the key-name/literal encoding it does support
/// (`psmux_send_keys_commands`).
pub fn send_keys_commands(pane_id: &str, buf: &[u8], psmux: bool) -> Vec<String> {
    if psmux {
        return psmux_send_keys_commands(pane_id, buf);
    }
    buf.chunks(SEND_KEYS_CHUNK_BYTES)
        .map(|chunk| format_send_keys(pane_id, chunk))
        .collect()
}

/// Build the psmux-compatible `send-keys` command line(s) for `buf`.
///
/// psmux supports `send-keys -l` (literal text) and key-names (`Enter`, `Tab`,
/// `Escape`, `BSpace`, `C-<letter>`, …) but not tmux's `-H` hex flag. Encode the
/// exact byte stream with those primitives: contiguous printable/UTF-8 runs go
/// out as one `-l` literal command, each control byte as its key-name. Because
/// every key-name injects exactly the byte it stands for, multi-byte sequences
/// round-trip — an arrow key (`\x1b[A`) becomes `Escape` then literal `[A`,
/// which the pane's PTY receives back as `\x1b[A`.
fn psmux_send_keys_commands(pane_id: &str, buf: &[u8]) -> Vec<String> {
    let mut cmds = Vec::new();
    let mut literal: Vec<u8> = Vec::new();
    for &b in buf {
        match psmux_key_name(b) {
            Some(name) => {
                flush_psmux_literal(pane_id, &mut literal, &mut cmds);
                cmds.push(format!("send-keys -t {pane_id} {name}\n"));
            }
            None => literal.push(b),
        }
    }
    flush_psmux_literal(pane_id, &mut literal, &mut cmds);
    cmds
}

/// Map a control byte to the psmux key-name that injects exactly that byte, or
/// `None` for a printable / UTF-8 byte (which joins an `-l` literal run).
fn psmux_key_name(b: u8) -> Option<String> {
    Some(match b {
        b'\r' => "Enter".to_string(),
        b'\t' => "Tab".to_string(),
        0x1b => "Escape".to_string(),
        0x7f => "BSpace".to_string(),
        // Ctrl+letter: 0x01..=0x1a → C-a..C-z (covers e.g. LF 0x0a → C-j).
        0x01..=0x1a => format!("C-{}", (b'a' + b - 1) as char),
        _ => return None,
    })
}

/// Emit the pending printable run as one or more `send-keys -l -N 1` commands
/// and clear it. Long runs are split at `SEND_KEYS_CHUNK_BYTES` (on char
/// boundaries) so no control-mode line gets over-long.
///
/// The `-N 1` is load-bearing, not a stray repeat count. psmux's control-mode
/// reader runs every line through a send-coalescing pass
/// (`coalesce_send_commands` in psmux) that decodes each send's bytes and
/// re-emits them re-quoted with the POSIX `'\''` escape — which psmux's own
/// tokenizer cannot read back, so any `'` in the text arrived in the pane as
/// `\` (`it's` was typed as `it\s`), regardless of how the client framed it.
/// The decoder bails on a `-N` flag, letting the original line reach the
/// direct send-keys handler, whose single parse handles the double-quote
/// framing of [`psmux_quote`] correctly. Verified against psmux 3.3.6.
fn flush_psmux_literal(pane_id: &str, literal: &mut Vec<u8>, cmds: &mut Vec<String>) {
    if literal.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(literal).into_owned();
    let emit = |chunk: &str, cmds: &mut Vec<String>| {
        cmds.push(format!(
            "send-keys -t {pane_id} -l -N 1 {}\n",
            psmux_quote(chunk)
        ));
    };
    let mut chunk = String::new();
    for ch in text.chars() {
        if !chunk.is_empty() && chunk.len() + ch.len_utf8() > SEND_KEYS_CHUNK_BYTES {
            emit(&chunk, cmds);
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        emit(&chunk, cmds);
    }
    literal.clear();
}

/// Double-quote `s` for a psmux `send-keys -l` argument. Always quotes (even a
/// bare word) so a leading `-` is never read as a flag. Double quotes — not
/// POSIX single quotes — because psmux's tokenizer has no working escape for a
/// `'` inside `'…'`, but inside `"…"` it passes `'` through untouched and
/// reads exactly two escapes: `\"` (literal quote) and `\\` (literal
/// backslash); any other backslash stays literal, so both are escaped here. A
/// literal run never contains a newline (LF and CR map to key-names), but the
/// control-mode line is `\n`-delimited, so newlines are replaced defensively.
/// Also the argument encoding for any other psmux control-mode line (e.g.
/// `new-window -c/-n` in [`super::tmux`]) — same tokenizer, so
/// [`shell_escape`]'s POSIX `'\''` idiom would arrive mangled there too.
pub(crate) fn psmux_quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', " ")
    )
}

/// Per-pane writer that sends input via control-mode `send-keys` through the
/// shared control stdin. `psmux` selects the key-name/literal encoding when the
/// backend is psmux instead of tmux's `-H` hex (see [`send_keys_commands`]).
pub struct ControlModeWriter {
    pub stdin: Arc<Mutex<std::process::ChildStdin>>,
    pub pane_id: String,
    pub psmux: bool,
}

impl Write for ControlModeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|e| std::io::Error::other(format!("stdin lock: {e}")))?;
        for cmd in send_keys_commands(&self.pane_id, buf, self.psmux) {
            stdin.write_all(cmd.as_bytes())?;
        }
        stdin.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Decode tmux control mode octal escapes in `%output` data.
///
/// Scans for `\` followed by exactly 3 octal digits (0-7). Emits the decoded byte.
/// All other characters pass through unchanged.
pub fn decode_octal(input: &str) -> Vec<u8> {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let d0 = bytes[i + 1];
            let d1 = bytes[i + 2];
            let d2 = bytes[i + 3];
            if is_octal(d0) && is_octal(d1) && is_octal(d2) {
                let val = (d0 - b'0') as u16 * 64 + (d1 - b'0') as u16 * 8 + (d2 - b'0') as u16;
                result.push(val as u8);
                i += 4;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }

    result
}

fn is_octal(b: u8) -> bool {
    (b'0'..=b'7').contains(&b)
}

/// Parse a line from tmux control mode into a notification.
pub fn parse_notification(line: &str) -> Notification {
    if let Some(rest) = line.strip_prefix("%output ") {
        // Format: %output %<pane_id> <octal-encoded data>
        if let Some(space_idx) = rest.find(' ') {
            let pane_id = rest[..space_idx].to_string();
            let data = decode_octal(&rest[space_idx + 1..]);
            return Notification::Output { pane_id, data };
        }
    }

    if let Some(rest) = line.strip_prefix("%extended-output ") {
        // Format: %extended-output %<pane_id> <age> : <octal-encoded data>
        // The " : " separator divides metadata from payload.
        if let Some(colon_idx) = rest.find(" : ") {
            let meta = &rest[..colon_idx];
            let data = decode_octal(&rest[colon_idx + 3..]);
            // meta is "%<pane_id> <age>" — extract pane_id.
            if let Some(space_idx) = meta.find(' ') {
                let pane_id = meta[..space_idx].to_string();
                return Notification::Output { pane_id, data };
            }
        }
    }

    if line.starts_with("%begin ") {
        return Notification::Begin;
    }

    if line.starts_with("%end ") {
        return Notification::End;
    }

    if line.starts_with("%error ") {
        return Notification::Error;
    }

    if let Some(rest) = line.strip_prefix("%pause ") {
        // Format: %pause %<pane_id>
        return Notification::Pause {
            pane_id: rest.trim().to_string(),
        };
    }

    if let Some(n) = parse_subscription_changed(line) {
        return n;
    }

    Notification::Other(line.to_string())
}

/// Parse a `%subscription-changed` notification (tmux >= 3.2 format
/// subscriptions, armed via `refresh-client -B`).
///
/// Wire format (tmux man page): `%subscription-changed name session-id
/// window-id window-index pane-id ... : value` — "any arguments after pane-id
/// up until a single ':' are for future use and should be ignored". Parsed
/// positionally (name = token 0, pane id = token 4, validated) with the value
/// being everything after the first ` : ` separator past the pane token,
/// verbatim — it may legally be empty or contain spaces/colons. Any shape
/// violation returns `None` (→ `Notification::Other`); wire data never
/// panics.
fn parse_subscription_changed(line: &str) -> Option<Notification> {
    let rest = line.strip_prefix("%subscription-changed ")?;
    let mut tokens = rest.splitn(6, ' ');
    let name = tokens.next()?.to_string();
    // session-id, window-id, window-index — positional, unused.
    for _ in 0..3 {
        tokens.next()?;
    }
    let pane_id = tokens.next()?.to_string();
    if !is_valid_pane_id(&pane_id) {
        return None;
    }
    // Whatever follows the pane id: `[future-use tokens ]: value`.
    let tail = tokens.next().unwrap_or("");
    let value = if let Some(v) = tail.strip_prefix(": ") {
        v.to_string()
    } else if tail == ":" {
        String::new()
    } else if let Some(idx) = tail.find(" : ") {
        tail[idx + 3..].to_string()
    } else if tail.ends_with(" :") {
        // Future-use tokens then an empty value ("a b :").
        String::new()
    } else {
        return None;
    };
    Some(Notification::SubscriptionChanged {
        name,
        pane_id,
        value,
    })
}

/// A tmux pane id is `%<digits>`. Pane ids are interpolated unquoted into
/// control-mode commands (`send-keys -t`, `kill-pane -t`, …), so anything
/// else must be rejected where ids enter the system (spawn/adopt/discover).
pub fn is_valid_pane_id(s: &str) -> bool {
    s.strip_prefix('%')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Format a `send-keys -H` command for a pane.
///
/// Each byte is encoded as two hex digits.
pub fn format_send_keys(pane_id: &str, bytes: &[u8]) -> String {
    use std::fmt::Write;
    // "send-keys -t %NN -H" + " XX" per byte + "\n"
    let mut cmd = String::with_capacity(20 + pane_id.len() + bytes.len() * 3 + 1);
    write!(cmd, "send-keys -t {pane_id} -H").unwrap();
    for &b in bytes {
        write!(cmd, " {b:02x}").unwrap();
    }
    cmd.push('\n');
    cmd
}

/// Shell-escape a string for safe inclusion in a tmux control mode command.
///
/// Tmux control mode is line-delimited — each `\n` starts a new command.
/// Literal newlines in arguments (e.g. `--append-system-prompt`) would split
/// the command and corrupt the protocol, so they are replaced with spaces.
pub fn shell_escape(s: &str) -> String {
    // Strip the protocol-breaking newlines, then apply standard POSIX
    // single-quote escaping (shared with the SSH/git paths).
    crate::shell::posix_quote(&s.replace('\n', " "))
}

/// What a withheld value is replaced by in diagnostic text.
///
/// Distinctive on purpose: a log line or a pasted bug report should show *that*
/// something was withheld, rather than reading like a truncated value.
pub const REDACTED: &str = "<redacted>";

/// Name fragments that make the value they label a presumed credential.
///
/// Matched case-insensitively as substrings, so `HTTPS_PROXY`,
/// `ANTHROPIC_API_KEY`, `GH_TOKEN` and `--auth-header` are all covered without
/// this list naming any of them. It errs wide deliberately — see
/// [`redact_secrets`].
const SECRET_NAME_FRAGMENTS: &[&str] = &[
    "auth",
    "credential",
    "key",
    "passwd",
    "password",
    "proxy",
    "secret",
    "token",
];

/// Rewrite a control-mode command line so it is safe to log or show in a toast.
///
/// A sandboxed launch carries the egress boundary's bearer token in the command
/// itself — `new-window … -e 'HTTP_PROXY=http://friring:<token>@127.0.0.1:<port>'`
/// — and the failure paths that report a command quote it verbatim. Log files
/// persist, get shared and get pasted into bug reports, and that token together
/// with the port it names is a working local egress channel for any other
/// process on the machine, so the text is rewritten before it is reported.
///
/// The policy is by **shape**, never by variable name: this is shared plumbing
/// that every subsystem's launch flows through, so a credential can arrive from
/// an agent's registry environment, an extension's, or a user's own argv, and
/// special-casing the one variable that motivated this would miss all of them.
/// Four shapes are treated as secret:
///
/// - an assignment whose name reads like a credential (`SECRET_NAME_FRAGMENTS`)
///   → `NAME=<redacted>`, so *which* keys were set is still legible;
/// - a URL carrying userinfo (`scheme://user:pass@host:port`) →
///   `scheme://<redacted>`, host and port included, since they are the other
///   half of a usable endpoint;
/// - the word after one that *labels* a credential — a flag (`--api-key sk-…`)
///   or a qualified name (psmux's `Set-Item Env:GH_TOKEN '…'`);
/// - anything matching the above *inside* a quoted compound word, found by
///   re-running the policy on the unquoted content. A remote launch nests the
///   whole command in `/bin/sh -lc '…'` and a psmux one nests it in a
///   PowerShell string; quoting must not be a way to smuggle a value past this.
///
/// Everything else survives verbatim — the tmux verb, the target session, the
/// window name, the flags, the paths, the names of the environment entries. A
/// blanket `<redacted>` would be safe and useless; a stalled spawn still has to
/// be diagnosable from its error. Where the two pull against each other,
/// redaction wins: over-redacting a diagnostic costs a detail, under-redacting
/// one leaks a credential, so values that are merely credential-*shaped*
/// (`NO_PROXY`, a hostname behind `--auth-server`) go too.
///
/// The result is **not** a runnable command: quoting around a replaced value is
/// dropped, and nothing can be re-run from a redacted line anyway.
pub fn redact_secrets(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut copied = 0;
    let mut after_label = false;
    for word in word_ranges(command) {
        let raw = &command[word.clone()];
        let bare = unquote(raw);
        let replacement = if after_label {
            Some(REDACTED.to_string())
        } else if let Some(assignment) = redact_assignment(&bare) {
            Some(assignment)
        } else if bare != *raw {
            // A quoted compound: strip one layer and apply the whole policy to
            // what it was hiding. Each step removes at least one character, so
            // the recursion is bounded by the word's length.
            let inner = redact_secrets(&bare);
            (inner != bare).then(|| requote(raw, &inner))
        } else {
            redact_userinfo_urls(raw)
        };
        if let Some(replacement) = replacement {
            out.push_str(&command[copied..word.start]);
            out.push_str(&replacement);
            copied = word.end;
        }
        after_label = labels_a_secret(&bare);
    }
    out.push_str(&command[copied..]);
    out
}

/// Byte ranges of the whitespace-separated words of `s`, with a quoted run
/// (`'…'` or `"…"`) counting as part of the word it sits in — so an escaped
/// argument stays one word however much whitespace it contains.
fn word_ranges(s: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() => {
                if let Some(begin) = start.take() {
                    ranges.push(begin..i);
                }
                continue;
            }
            None => {}
        }
        start.get_or_insert(i);
    }
    if let Some(begin) = start {
        ranges.push(begin..s.len());
    }
    ranges
}

/// The value a quoted word stands for, so the policy matches whatever framing
/// the caller used.
///
/// Approximate by design — it reads POSIX `'…'`, double quotes and a
/// backslash escape, which covers every quoting style this crate emits
/// ([`shell_escape`], [`psmux_quote`]) and is only ever used to *decide* on a
/// redaction, never to rebuild a command.
fn unquote(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut quote: Option<char> = None;
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => out.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '\\' => out.extend(chars.next()),
            None => out.push(c),
        }
    }
    out
}

/// Put `inner` back inside the framing `raw` had, so a redacted compound still
/// reads as one argument.
fn requote(raw: &str, inner: &str) -> String {
    let mut chars = raw.chars();
    match (chars.next(), chars.next_back()) {
        (Some(q), Some(last)) if (q == '\'' || q == '"') && q == last => format!("{q}{inner}{q}"),
        _ => inner.to_string(),
    }
}

/// `NAME=<redacted>` when `word` assigns a value to a credential-shaped name.
fn redact_assignment(word: &str) -> Option<String> {
    let (name, value) = word.split_once('=')?;
    (!value.is_empty() && is_secret_name(name)).then(|| format!("{name}={REDACTED}"))
}

/// Whether `word` names a credential whose value is the *next* word.
///
/// Only a flag (`--api-key`) or a qualified name (`Env:GH_TOKEN`) counts. A
/// bare word does not: a window named `tb-api-keys` would otherwise blank the
/// flag that follows it, and tmux's own option names (`extended-keys`) would
/// blank their values.
fn labels_a_secret(word: &str) -> bool {
    if word.contains('=') {
        return false;
    }
    if let Some(flag) = word.strip_prefix('-') {
        return is_secret_name(flag);
    }
    match word.rsplit_once(':') {
        Some((prefix, name)) => !prefix.is_empty() && is_secret_name(name),
        None => false,
    }
}

/// Whether `name` reads as the name of a credential rather than of anything
/// else. Leading dashes are ignored so a flag and an environment key are judged
/// the same way; anything that is not plausibly a name (a URL, a tmux format, a
/// path) is rejected before the fragments are consulted.
fn is_secret_name(name: &str) -> bool {
    let name = name.trim_start_matches('-');
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    SECRET_NAME_FRAGMENTS.iter().any(|f| lower.contains(f))
}

/// Replace every `scheme://user[:pass]@host[:port]` in `word` with
/// `scheme://<redacted>`, or `None` if it holds no such URL.
///
/// Userinfo is the giveaway that a URL is credential-bearing, and the authority
/// goes with it rather than only the `user:pass` part: an endpoint's host and
/// port are what make a leaked token spendable. The scheme is kept because it
/// is not a secret and it is what tells an HTTP proxy from a SOCKS one.
fn redact_userinfo_urls(word: &str) -> Option<String> {
    let mut out = String::new();
    let mut copied = 0;
    let mut cursor = 0;
    let mut found = false;
    while let Some(offset) = word[cursor..].find("://") {
        let separator = cursor + offset;
        let authority = separator + 3;
        let end = authority
            + word[authority..]
                .find(ends_authority)
                .unwrap_or(word.len() - authority);
        if word[authority..end].contains('@') && has_scheme(word, separator) {
            // Everything up to the `://` — any leading text plus the scheme —
            // is not secret and is kept; the authority after it is dropped.
            out.push_str(&word[copied..separator]);
            out.push_str("://");
            out.push_str(REDACTED);
            copied = end;
            found = true;
        }
        cursor = end.max(separator + 3);
    }
    found.then(|| {
        out.push_str(&word[copied..]);
        out
    })
}

/// Characters that cannot appear in a URL authority, so the first one ends it.
fn ends_authority(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '/' | '?' | '#' | '\'' | '"' | '`' | ';' | ',' | '<' | '>' | '|' | '\\' | ')' | ']'
        )
}

/// Whether a scheme precedes the `://` at `separator` — otherwise the `://` is
/// incidental text and the run after it is not a URL authority.
fn has_scheme(word: &str, separator: usize) -> bool {
    let mut start = separator;
    for (i, c) in word[..separator].char_indices().rev() {
        if c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.') {
            start = i;
        } else {
            break;
        }
    }
    word[start..separator].starts_with(|c: char| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use super::*;

    // --- is_valid_pane_id tests ---

    #[test]
    fn pane_id_accepts_percent_digits() {
        assert!(is_valid_pane_id("%0"));
        assert!(is_valid_pane_id("%42"));
        assert!(is_valid_pane_id("%123456"));
    }

    #[test]
    fn pane_id_rejects_everything_else() {
        assert!(!is_valid_pane_id(""));
        assert!(!is_valid_pane_id("%"));
        assert!(!is_valid_pane_id("42"));
        assert!(!is_valid_pane_id("%4a"));
        assert!(!is_valid_pane_id("% 42"));
        assert!(!is_valid_pane_id("%-1"));
        assert!(!is_valid_pane_id("%42; kill-server"));
        assert!(!is_valid_pane_id("%42\nkill-server"));
    }

    // --- shell_escape tests ---

    #[test]
    fn shell_escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "hello");
    }

    #[test]
    fn shell_escape_path() {
        assert_eq!(shell_escape("/home/user/repos/app"), "/home/user/repos/app");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_escape_flag_value() {
        assert_eq!(shell_escape("--permission-mode"), "--permission-mode");
    }

    #[test]
    fn shell_escape_tool_pattern() {
        assert_eq!(shell_escape("Read Bash(git:*)"), "'Read Bash(git:*)'");
    }

    #[test]
    fn shell_escape_allows_equals_comma() {
        assert_eq!(shell_escape("key=val,other"), "key=val,other");
    }

    #[test]
    fn shell_escape_replaces_newlines() {
        // Newlines in tmux control mode commands would split the command,
        // corrupting the protocol. They must be replaced with spaces.
        assert_eq!(
            shell_escape("line one\nline two\nline three"),
            "'line one line two line three'"
        );
    }

    #[test]
    fn shell_escape_newline_only() {
        assert_eq!(shell_escape("\n"), "' '");
    }

    // --- redact_secrets tests ---
    //
    // The command line of a sandboxed launch carries the egress proxy's bearer
    // token. These pin the two halves of the bargain: no credential survives
    // into diagnostic text, and everything that is not one does.

    /// The exact shape the egress boundary produces, quoted the way
    /// `new-window -e` quotes it.
    #[test]
    fn redact_drops_a_proxy_url_but_keeps_its_key() {
        assert_eq!(
            redact_secrets("-e 'HTTP_PROXY=http://friring:s3cr3t@127.0.0.1:54321'"),
            "-e HTTP_PROXY=<redacted>"
        );
        assert_eq!(
            redact_secrets("-e 'ALL_PROXY=socks5h://friring:s3cr3t@127.0.0.1:54321'"),
            "-e ALL_PROXY=<redacted>"
        );
    }

    /// Neither the token nor the port it names may reach a log: together they
    /// are a usable local egress channel for anything else on the machine.
    #[test]
    fn redact_removes_token_and_port_together() {
        let out = redact_secrets("-e HTTP_PROXY=http://friring:s3cr3t@127.0.0.1:54321");
        assert!(!out.contains("s3cr3t"), "{out}");
        assert!(!out.contains("54321"), "{out}");
    }

    /// A credential-bearing URL is redacted wherever it appears, not only in an
    /// assignment — and the scheme stays, since it tells an HTTP proxy from a
    /// SOCKS one and is not itself a secret.
    #[test]
    fn redact_handles_a_bare_userinfo_url() {
        assert_eq!(
            redact_secrets("curl socks5h://friring:s3cr3t@127.0.0.1:9"),
            "curl socks5h://<redacted>"
        );
        assert_eq!(
            redact_secrets("git clone https://x-token:ghp_abc@github.com/o/r.git"),
            "git clone https://<redacted>/o/r.git"
        );
    }

    /// A URL with no userinfo is not credential-bearing and stays whole — most
    /// of them are the allowlist entries the diagnostic is about.
    #[test]
    fn redact_keeps_a_url_without_userinfo() {
        let cmd = "new-window -n tb-x claude --url https://api.anthropic.com/v1";
        assert_eq!(redact_secrets(cmd), cmd);
    }

    /// The point of redacting the value rather than the line: a stalled spawn
    /// still names its verb, its target, its window, its directory, and which
    /// environment keys were set.
    #[test]
    fn redact_keeps_the_command_readable() {
        let out = redact_secrets(
            "new-window -t friring -n tb-work -P -F '#{pane_id}' -c /w \
             -e 'HTTP_PROXY=http://friring:s3cr3t@127.0.0.1:9' -e FRIRING_SESSION_ID=abc claude",
        );
        assert_eq!(
            out,
            "new-window -t friring -n tb-work -P -F '#{pane_id}' -c /w \
             -e HTTP_PROXY=<redacted> -e FRIRING_SESSION_ID=abc claude"
        );
    }

    /// Every tmux command friring actually sends outside a sandboxed launch —
    /// control-mode lines and the one-shot argvs alike — is reported verbatim.
    /// The redactor must not turn ordinary tmux traffic into a puzzle.
    /// `extended-keys` is the trap: it contains a secret fragment but is an
    /// option name, and its value must survive.
    #[test]
    fn redact_leaves_ordinary_tmux_commands_alone() {
        for cmd in [
            "refresh-client",
            "refresh-client -f pause-after=5",
            "refresh-client -A '%1:continue'",
            "refresh-client -B 'friring-status:%*:#{@friring_agent_state}'",
            "set-option -s extended-keys on",
            "set-option -s extended-keys-format csi-u",
            "set-option -s default-command /bin/zsh",
            "set-option -t friring history-limit 5000",
            "new-session -d -s friring -x 80 -y 24",
            "has-session -t friring",
            "capture-pane -e -p -J -S -5000 -t %1",
            "list-windows -t friring -F '#{pane_id}|#{window_name}|#{pane_dead}'",
            "display-message -t %1 -p '#{pane_dead}'",
            "resize-pane -t %1 -x 80 -y 24",
            "kill-pane -t %1",
            "send-keys -t %1 -H 41 42 43",
            "",
        ] {
            assert_eq!(redact_secrets(cmd), cmd, "rewrote: {cmd}");
        }
    }

    /// The two rules cover for each other: a name the fragments don't know
    /// still loses a userinfo URL, and the surviving path keeps enough of the
    /// remote to be recognisable.
    #[test]
    fn redact_catches_a_credential_under_an_unremarkable_name() {
        assert_eq!(
            redact_secrets("-e 'GIT_REMOTE=https://u:ghp_abc@github.com/o/r.git'"),
            "-e 'GIT_REMOTE=https://<redacted>/o/r.git'"
        );
    }

    /// The seatbelt profile parameter naming the port the proxy listens on.
    /// The token is elsewhere, but the port is the other half of the endpoint.
    #[test]
    fn redact_covers_the_sandbox_proxy_parameter() {
        assert_eq!(
            redact_secrets("sandbox-exec -f /d/p.sb -D PROXY=localhost:54321 claude"),
            "sandbox-exec -f /d/p.sb -D PROXY=<redacted> claude"
        );
    }

    /// A flag names its value, so the word after a credential-shaped flag goes
    /// even when the value itself looks innocuous.
    #[test]
    fn redact_covers_a_value_behind_a_credential_flag() {
        assert_eq!(
            redact_secrets("claude --api-key sk-live-01 --model opus"),
            "claude --api-key <redacted> --model opus"
        );
    }

    /// Quoting must not be a way to smuggle a value past the policy: the
    /// remote path nests the whole launch in `/bin/sh -lc '…'`.
    #[test]
    fn redact_reaches_inside_a_quoted_compound() {
        assert_eq!(
            redact_secrets("/bin/sh -lc 'exec sandbox-exec -D PROXY=localhost:54321 claude'"),
            "/bin/sh -lc 'exec sandbox-exec -D PROXY=<redacted> claude'"
        );
    }

    /// The psmux shape: env folded into one double-quoted PowerShell token,
    /// where the key is a `Set-Item Env:NAME` label rather than an assignment.
    #[test]
    fn redact_reaches_inside_the_psmux_window_command() {
        let out = redact_secrets(
            "new-window -n tb-x \"Set-Item Env:GH_TOKEN 'ghp_abc'; \
             Set-Item Env:TERM 'xterm'; & 'claude'\"",
        );
        assert!(!out.contains("ghp_abc"), "{out}");
        assert!(out.contains("Env:GH_TOKEN <redacted>"), "{out}");
        assert!(out.contains("Set-Item Env:TERM 'xterm'"), "{out}");
        assert!(out.contains("tb-x"), "{out}");
    }

    /// Over-redacting a diagnostic costs a detail; under-redacting one leaks a
    /// credential. Values that are only credential-*shaped* go too.
    #[test]
    fn redact_errs_wide_on_credential_shaped_names() {
        assert_eq!(
            redact_secrets("-e NO_PROXY=localhost,127.0.0.1,::1"),
            "-e NO_PROXY=<redacted>"
        );
        assert_eq!(
            redact_secrets("-e ANTHROPIC_API_KEY=sk-ant-01"),
            "-e ANTHROPIC_API_KEY=<redacted>"
        );
        assert_eq!(
            redact_secrets("-e aws_secret_access_key=AKIA"),
            "-e aws_secret_access_key=<redacted>"
        );
    }

    /// Redacting twice must not eat the marker or the command around it — the
    /// text passes through this on more than one path.
    #[test]
    fn redact_is_idempotent() {
        for cmd in [
            "-e 'HTTP_PROXY=http://friring:s3cr3t@127.0.0.1:9' -e TERM=xterm",
            "/bin/sh -lc 'exec claude --api-key sk-1'",
            "socks5h://u:p@h:1",
        ] {
            let once = redact_secrets(cmd);
            assert_eq!(redact_secrets(&once), once, "not idempotent: {cmd}");
        }
    }

    /// An empty value carries nothing to leak, and blanking it would hide that
    /// the key was set to nothing at all — a real cause of a broken launch.
    #[test]
    fn redact_keeps_an_empty_assignment_visible() {
        assert_eq!(redact_secrets("-e HTTP_PROXY="), "-e HTTP_PROXY=");
    }

    // --- decode_octal tests ---

    #[test]
    fn decode_octal_esc() {
        assert_eq!(decode_octal("\\033"), vec![27]);
    }

    #[test]
    fn decode_octal_backslash() {
        assert_eq!(decode_octal("\\134"), vec![b'\\']);
    }

    #[test]
    fn decode_octal_newline() {
        assert_eq!(decode_octal("\\012"), vec![b'\n']);
    }

    #[test]
    fn decode_octal_passthrough() {
        assert_eq!(decode_octal("hello"), b"hello");
    }

    #[test]
    fn decode_octal_incomplete() {
        assert_eq!(decode_octal("\\01"), b"\\01");
    }

    #[test]
    fn decode_octal_non_octal_digits() {
        assert_eq!(decode_octal("\\089"), b"\\089");
    }

    #[test]
    fn decode_octal_mixed() {
        assert_eq!(
            decode_octal("A\\033[1mB"),
            vec![b'A', 27, b'[', b'1', b'm', b'B']
        );
    }

    #[test]
    fn decode_octal_consecutive() {
        assert_eq!(decode_octal("\\033\\033"), vec![27, 27]);
    }

    #[test]
    fn decode_octal_empty() {
        assert_eq!(decode_octal(""), b"");
    }

    #[test]
    fn decode_octal_trailing_backslash() {
        assert_eq!(decode_octal("a\\"), b"a\\");
    }

    #[test]
    fn decode_octal_max_value() {
        assert_eq!(decode_octal("\\377"), vec![0xFF]);
    }

    #[test]
    fn decode_octal_overflow_wraps() {
        assert_eq!(decode_octal("\\400"), vec![0u8]);
    }

    // --- parse_notification tests ---

    #[test]
    fn parse_output_notification() {
        let n = parse_notification("%output %42 hello\\033[1m");
        assert_eq!(
            n,
            Notification::Output {
                pane_id: "%42".to_string(),
                data: vec![b'h', b'e', b'l', b'l', b'o', 27, b'[', b'1', b'm'],
            }
        );
    }

    #[test]
    fn parse_extended_output_notification() {
        let n = parse_notification("%extended-output %2 0 : \\033[?2026hA\\033[?2026l");
        assert_eq!(
            n,
            Notification::Output {
                pane_id: "%2".to_string(),
                data: vec![
                    27, b'[', b'?', b'2', b'0', b'2', b'6', b'h', b'A', 27, b'[', b'?', b'2', b'0',
                    b'2', b'6', b'l'
                ],
            }
        );
    }

    #[test]
    fn parse_begin_notification() {
        assert_eq!(
            parse_notification("%begin 1234567890 7 0"),
            Notification::Begin
        );
    }

    #[test]
    fn parse_end_notification() {
        assert_eq!(parse_notification("%end 1234567890 7 0"), Notification::End);
    }

    #[test]
    fn parse_error_notification() {
        assert_eq!(
            parse_notification("%error 1234567890 3 0"),
            Notification::Error
        );
    }

    #[test]
    fn parse_pause_notification() {
        assert_eq!(
            parse_notification("%pause %42"),
            Notification::Pause {
                pane_id: "%42".to_string()
            }
        );
    }

    #[test]
    fn parse_other_notification() {
        assert_eq!(
            parse_notification("some random line"),
            Notification::Other("some random line".to_string())
        );
    }

    #[test]
    fn parse_output_no_data() {
        assert_eq!(
            parse_notification("%output %42"),
            Notification::Other("%output %42".to_string())
        );
    }

    #[test]
    fn parse_extended_output_no_colon_separator() {
        assert_eq!(
            parse_notification("%extended-output %2 0 data"),
            Notification::Other("%extended-output %2 0 data".to_string())
        );
    }

    #[test]
    fn parse_output_empty_data() {
        let n = parse_notification("%output %42 ");
        assert_eq!(
            n,
            Notification::Output {
                pane_id: "%42".to_string(),
                data: vec![],
            }
        );
    }

    #[test]
    fn parse_pause_notification_with_leading_percent() {
        assert_eq!(
            parse_notification("%pause %123"),
            Notification::Pause {
                pane_id: "%123".to_string()
            }
        );
    }

    #[test]
    fn parse_extended_output_missing_pane_space() {
        assert_eq!(
            parse_notification("%extended-output %2 : data"),
            Notification::Other("%extended-output %2 : data".to_string())
        );
    }

    // --- %subscription-changed parsing tests ---
    // Wire shape (tmux man page): `%subscription-changed name session-id
    // window-id window-index pane-id ... : value` — args between pane-id and
    // the ':' are documented future-use; the value may be empty or contain
    // spaces/colons.

    #[test]
    fn subscription_changed_parses_canonical_line() {
        assert_eq!(
            parse_notification("%subscription-changed friring-status $1 @5 2 %7 : done"),
            Notification::SubscriptionChanged {
                name: "friring-status".into(),
                pane_id: "%7".into(),
                value: "done".into(),
            }
        );
    }

    #[test]
    fn subscription_changed_ignores_future_use_args() {
        assert_eq!(
            parse_notification(
                "%subscription-changed friring-status $1 @5 2 %7 extra stuff : working"
            ),
            Notification::SubscriptionChanged {
                name: "friring-status".into(),
                pane_id: "%7".into(),
                value: "working".into(),
            }
        );
    }

    #[test]
    fn subscription_changed_empty_value_variants() {
        for line in [
            "%subscription-changed s $1 @5 2 %7 :",
            "%subscription-changed s $1 @5 2 %7 : ",
            "%subscription-changed s $1 @5 2 %7 future :",
        ] {
            match parse_notification(line) {
                Notification::SubscriptionChanged { value, .. } => {
                    assert_eq!(value, "", "line: {line}")
                }
                other => panic!("expected SubscriptionChanged for {line}, got {other:?}"),
            }
        }
    }

    #[test]
    fn subscription_changed_value_keeps_spaces_and_colons() {
        assert_eq!(
            parse_notification("%subscription-changed s $1 @5 2 %7 : a b : c"),
            Notification::SubscriptionChanged {
                name: "s".into(),
                pane_id: "%7".into(),
                value: "a b : c".into(),
            }
        );
    }

    #[test]
    fn subscription_changed_malformed_falls_to_other() {
        // Invalid pane token / missing separator / truncated — wire data must
        // never panic, it degrades to Other.
        for line in [
            "%subscription-changed s $1 @5 2 pane7 : done",
            "%subscription-changed s $1 @5 2 %7 done",
            "%subscription-changed s $1 @5",
            "%subscription-changed",
        ] {
            assert!(
                matches!(parse_notification(line), Notification::Other(_)),
                "line: {line}"
            );
        }
    }

    // --- format_send_keys tests ---

    #[test]
    fn format_send_keys_single_byte() {
        assert_eq!(format_send_keys("%42", b"A"), "send-keys -t %42 -H 41\n");
    }

    #[test]
    fn format_send_keys_multi_byte() {
        assert_eq!(
            format_send_keys("%42", b"ABC"),
            "send-keys -t %42 -H 41 42 43\n"
        );
    }

    #[test]
    fn format_send_keys_empty() {
        assert_eq!(format_send_keys("%42", &[]), "send-keys -t %42 -H\n");
    }

    #[test]
    fn format_send_keys_escape_sequence() {
        assert_eq!(
            format_send_keys("%1", &[0x1b, b'[', b'A']),
            "send-keys -t %1 -H 1b 5b 41\n"
        );
    }

    // --- send_keys_commands chunking tests ---

    #[test]
    fn send_keys_commands_short_input_is_one_command() {
        let cmds = send_keys_commands("%1", b"ABC", false);
        assert_eq!(cmds, vec!["send-keys -t %1 -H 41 42 43\n".to_string()]);
    }

    #[test]
    fn send_keys_commands_empty_input_is_no_commands() {
        assert!(send_keys_commands("%1", &[], false).is_empty());
        assert!(send_keys_commands("%1", &[], true).is_empty());
    }

    /// A large paste is split into multiple bounded `send-keys` commands whose
    /// concatenated bytes equal the original input — the property that keeps a
    /// big paste from being truncated by tmux's per-command line limit.
    #[test]
    fn send_keys_commands_chunks_large_input_losslessly() {
        // 5 KB of bracketed-paste-wrapped content, like `send_paste_to_session`.
        let mut input = b"\x1b[200~".to_vec();
        input.extend((0..5000u32).map(|i| (i % 256) as u8));
        input.extend_from_slice(b"\x1b[201~");

        let cmds = send_keys_commands("%1", &input, false);

        assert!(
            cmds.len() > 1,
            "expected the large input to span multiple commands, got {}",
            cmds.len()
        );

        // Parse each `send-keys -t %1 -H XX XX …\n` back into its bytes.
        let decode = |cmd: &str| -> Vec<u8> {
            cmd.trim_end()
                .strip_prefix("send-keys -t %1 -H")
                .expect("send-keys prefix")
                .split_whitespace()
                .map(|h| u8::from_str_radix(h, 16).expect("hex byte"))
                .collect()
        };

        let mut reassembled = Vec::new();
        for cmd in &cmds {
            let bytes = decode(cmd);
            assert!(
                bytes.len() <= SEND_KEYS_CHUNK_BYTES,
                "chunk encodes {} bytes, exceeds bound {SEND_KEYS_CHUNK_BYTES}",
                bytes.len()
            );
            reassembled.extend(bytes);
        }
        assert_eq!(reassembled, input);
    }

    // --- psmux send-keys encoding tests ---
    //
    // Regression: psmux has no `send-keys -H`, so on Windows the hex path
    // injected the literal text "62" when the user typed `b` (0x62), and Enter /
    // Backspace did nothing. The psmux encoding must use `-l` literals + key-names.

    #[test]
    fn psmux_printable_char_uses_literal_not_hex() {
        // Typing `b` must inject `b`, not the literal text "62".
        assert_eq!(
            send_keys_commands("%1", b"b", true),
            vec!["send-keys -t %1 -l -N 1 \"b\"\n".to_string()]
        );
    }

    #[test]
    fn psmux_printable_run_is_one_literal_command() {
        assert_eq!(
            send_keys_commands("%1", b"hello world", true),
            vec!["send-keys -t %1 -l -N 1 \"hello world\"\n".to_string()]
        );
    }

    #[test]
    fn psmux_enter_backspace_tab_escape_use_key_names() {
        assert_eq!(
            send_keys_commands("%1", b"\r", true),
            vec!["send-keys -t %1 Enter\n".to_string()]
        );
        assert_eq!(
            send_keys_commands("%1", &[0x7f], true),
            vec!["send-keys -t %1 BSpace\n".to_string()]
        );
        assert_eq!(
            send_keys_commands("%1", b"\t", true),
            vec!["send-keys -t %1 Tab\n".to_string()]
        );
        assert_eq!(
            send_keys_commands("%1", &[0x1b], true),
            vec!["send-keys -t %1 Escape\n".to_string()]
        );
    }

    #[test]
    fn psmux_ctrl_letters_map_to_c_prefix() {
        assert_eq!(
            send_keys_commands("%1", &[0x03], true), // Ctrl+C
            vec!["send-keys -t %1 C-c\n".to_string()]
        );
        assert_eq!(
            send_keys_commands("%1", &[0x01], true), // Ctrl+A
            vec!["send-keys -t %1 C-a\n".to_string()]
        );
        assert_eq!(
            send_keys_commands("%1", &[0x1a], true), // Ctrl+Z
            vec!["send-keys -t %1 C-z\n".to_string()]
        );
        assert_eq!(
            send_keys_commands("%1", &[0x0a], true), // LF → Ctrl+J
            vec!["send-keys -t %1 C-j\n".to_string()]
        );
    }

    #[test]
    fn psmux_arrow_sequence_splits_escape_then_literal() {
        // An arrow key arrives as `\x1b[A`; psmux reconstructs the same bytes
        // from `Escape` + literal `[A`.
        assert_eq!(
            send_keys_commands("%1", b"\x1b[A", true),
            vec![
                "send-keys -t %1 Escape\n".to_string(),
                "send-keys -t %1 -l -N 1 \"[A\"\n".to_string(),
            ]
        );
    }

    #[test]
    fn psmux_literal_single_quote_survives() {
        // Regression: psmux's send-coalescing re-quoted literals with the
        // POSIX `'\''` escape its own parser can't read back, so `it's` was
        // typed into the pane as `it\s`. The `-N 1` opts out of coalescing and
        // the double-quote framing passes `'` through untouched.
        assert_eq!(
            send_keys_commands("%1", b"it's", true),
            vec!["send-keys -t %1 -l -N 1 \"it's\"\n".to_string()]
        );
    }

    #[test]
    fn psmux_literal_escapes_backslash_and_double_quote() {
        // psmux's double-quote tokenizer reads exactly `\"` and `\\`; both
        // must be escaped so Windows paths and quoted text round-trip.
        assert_eq!(
            send_keys_commands("%1", br#"say "hi" C:\p"#, true),
            vec!["send-keys -t %1 -l -N 1 \"say \\\"hi\\\" C:\\\\p\"\n".to_string()]
        );
    }

    #[test]
    fn psmux_mixed_text_then_enter() {
        // The common "type a command and submit" path.
        assert_eq!(
            send_keys_commands("%1", b"ls\r", true),
            vec![
                "send-keys -t %1 -l -N 1 \"ls\"\n".to_string(),
                "send-keys -t %1 Enter\n".to_string(),
            ]
        );
    }

    #[test]
    fn psmux_bracketed_paste_splits_markers_from_text() {
        // A paste arrives wrapped in `\x1b[200~ … \x1b[201~`; the ESC bytes
        // become `Escape`, the rest stays literal — reconstructing the wrapper.
        assert_eq!(
            send_keys_commands("%1", b"\x1b[200~hi\x1b[201~", true),
            vec![
                "send-keys -t %1 Escape\n".to_string(),
                "send-keys -t %1 -l -N 1 \"[200~hi\"\n".to_string(),
                "send-keys -t %1 Escape\n".to_string(),
                "send-keys -t %1 -l -N 1 \"[201~\"\n".to_string(),
            ]
        );
    }

    #[test]
    fn psmux_utf8_char_goes_to_literal() {
        assert_eq!(
            send_keys_commands("%1", "é".as_bytes(), true),
            vec!["send-keys -t %1 -l -N 1 \"é\"\n".to_string()]
        );
    }

    #[test]
    fn psmux_long_run_splits_on_char_boundary() {
        let input = "é".repeat(400); // 800 bytes, each char 2 bytes
        let cmds = send_keys_commands("%1", input.as_bytes(), true);
        assert!(cmds.len() > 1, "expected a long run to span >1 command");
        // Reassemble the quoted literals back into the original text.
        let mut text = String::new();
        for cmd in &cmds {
            let inner = cmd
                .trim_end()
                .strip_prefix("send-keys -t %1 -l -N 1 \"")
                .and_then(|s| s.strip_suffix('"'))
                .expect("literal command shape");
            text.push_str(inner);
        }
        assert_eq!(text, input);
    }

    // --- ControlModeReader tests ---

    #[test]
    fn control_mode_reader_data_delivery() {
        let (tx, rx) = sync_channel(16);
        let mut reader = ControlModeReader::new(rx);

        tx.send(b"hello".to_vec()).unwrap();
        let mut buf = [0u8; 16];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn control_mode_reader_eof_on_sender_drop() {
        let (tx, rx) = sync_channel(16);
        let mut reader = ControlModeReader::new(rx);

        drop(tx);
        let mut buf = [0u8; 16];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn control_mode_reader_partial_reads() {
        let (tx, rx) = sync_channel(16);
        let mut reader = ControlModeReader::new(rx);

        tx.send(b"hello world".to_vec()).unwrap();

        let mut buf = [0u8; 5];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");

        let n = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b" worl");

        let n = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"d");
    }

    #[test]
    fn control_mode_reader_multiple_sends() {
        let (tx, rx) = sync_channel(16);
        let mut reader = ControlModeReader::new(rx);

        tx.send(b"aaa".to_vec()).unwrap();
        tx.send(b"bbb".to_vec()).unwrap();

        let mut buf = [0u8; 16];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"aaa");

        let n = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"bbb");
    }

    #[test]
    fn control_mode_writer_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ControlModeWriter>();
    }

    #[test]
    fn control_mode_reader_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ControlModeReader>();
    }

    #[test]
    fn control_mode_reader_exact_size_buffer() {
        let (tx, rx) = sync_channel(16);
        let mut reader = ControlModeReader::new(rx);

        tx.send(b"abc".to_vec()).unwrap();
        let mut buf = [0u8; 3];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(&buf[..n], b"abc");
    }

    #[test]
    fn try_send_drops_when_channel_full() {
        let (tx, _rx) = sync_channel::<Vec<u8>>(1);

        tx.send(b"first".to_vec()).unwrap();

        match tx.try_send(b"second".to_vec()) {
            Err(std::sync::mpsc::TrySendError::Full(_)) => {}
            other => panic!("Expected TrySendError::Full, got: {other:?}"),
        }
    }

    // Compile-time check: channel capacity must be large enough to buffer heavy output.
    const _: () = assert!(PANE_CHANNEL_CAPACITY >= 1024);
}

/// Property/fuzz tests proving the tmux control-mode **transport** is byte
/// transparent: whatever the agent writes is exactly what comes out of
/// [`decode_octal`] + [`ControlModeReader`], regardless of how tmux escapes it
/// or how the byte stream is chunked. If these stay green, friring's transport
/// layer cannot be the source of glitched/stray characters in the rendered pane.
#[cfg(test)]
mod transport_proptests {
    use std::fmt::Write as _;
    use std::io::Read;
    use std::sync::mpsc::channel;

    use proptest::prelude::*;

    use super::{
        decode_octal, format_send_keys, parse_notification, ControlModeReader, Notification,
    };

    /// Reference encoder mirroring tmux's control-mode `%output` escaping:
    /// printable ASCII passes through, backslash becomes `\134`, and every other
    /// byte is emitted as a 3-digit `\ooo` octal escape. Because backslash is
    /// always escaped, a bare `\` never appears in the payload except as the
    /// start of a complete octal escape — exactly the input shape `decode_octal`
    /// is meant to invert.
    fn tmux_octal_encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len());
        for &b in bytes {
            if b == b'\\' {
                out.push_str("\\134");
            } else if (0x20..=0x7e).contains(&b) {
                out.push(b as char);
            } else {
                write!(out, "\\{b:03o}").unwrap();
            }
        }
        out
    }

    /// Split `bytes` into contiguous, non-empty chunks at the given (wrapped)
    /// offsets — models tmux emitting output across several `%output` lines.
    fn chunk_bytes(bytes: &[u8], split_points: &[usize]) -> Vec<Vec<u8>> {
        if bytes.is_empty() {
            return Vec::new();
        }
        let mut points: Vec<usize> = split_points
            .iter()
            .map(|&p| p % (bytes.len() + 1))
            .collect();
        points.push(0);
        points.push(bytes.len());
        points.sort_unstable();
        points.dedup();
        points
            .windows(2)
            .map(|w| bytes[w[0]..w[1]].to_vec())
            .filter(|c| !c.is_empty())
            .collect()
    }

    /// Read a `ControlModeReader` to EOF using a cycling sequence of buffer
    /// sizes, so reassembly is exercised across arbitrary read boundaries.
    fn drain_reader(reader: &mut ControlModeReader, buf_sizes: &[usize]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            let sz = buf_sizes[i % buf_sizes.len()].max(1);
            let mut buf = vec![0u8; sz];
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
            i += 1;
        }
        out
    }

    /// Parse our own `send-keys -t %1 -H XX XX …` command back into the bytes it
    /// encodes, to confirm the *input* (typed/pasted) path is lossless too.
    fn parse_send_keys_hex(cmd: &str) -> Vec<u8> {
        let cmd = cmd.strip_suffix('\n').expect("trailing newline");
        let rest = cmd
            .strip_prefix("send-keys -t %1 -H")
            .expect("send-keys prefix");
        rest.split_whitespace()
            .map(|h| u8::from_str_radix(h, 16).expect("hex byte"))
            .collect()
    }

    proptest! {
        /// `decode_octal` is the exact inverse of tmux's octal escaping for any
        /// byte sequence (all 256 values, escapes, and digit runs that merely
        /// look like octal).
        #[test]
        fn decode_octal_inverts_tmux_encoding(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let encoded = tmux_octal_encode(&bytes);
            prop_assert_eq!(decode_octal(&encoded), bytes);
        }

        /// `ControlModeReader` reassembles a chunked byte stream identically,
        /// regardless of how the stream is chunked or what read buffer sizes the
        /// consumer uses.
        #[test]
        fn control_mode_reader_reassembles_losslessly(
            chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..64), 0..32),
            buf_sizes in prop::collection::vec(1usize..40, 1..16),
        ) {
            let expected: Vec<u8> = chunks.iter().flatten().copied().collect();
            let (tx, rx) = channel();
            for c in &chunks {
                tx.send(c.clone()).unwrap();
            }
            drop(tx);
            let mut reader = ControlModeReader::new(rx);
            let got = drain_reader(&mut reader, &buf_sizes);
            prop_assert_eq!(got, expected);
        }

        /// The full transport — agent bytes → tmux octal `%output` lines →
        /// `parse_notification` → `decode_octal` → mpsc channel →
        /// `ControlModeReader` — is the identity function on the byte stream,
        /// for arbitrary bytes split across arbitrary `%output` boundaries and
        /// drained with arbitrary read sizes.
        #[test]
        fn full_transport_is_byte_identity(
            bytes in prop::collection::vec(any::<u8>(), 0..512),
            split_points in prop::collection::vec(any::<usize>(), 0..16),
            buf_sizes in prop::collection::vec(1usize..40, 1..16),
        ) {
            let chunks = chunk_bytes(&bytes, &split_points);
            let (tx, rx) = channel();
            for chunk in &chunks {
                let line = format!("%output %1 {}", tmux_octal_encode(chunk));
                match parse_notification(&line) {
                    Notification::Output { pane_id, data } => {
                        prop_assert_eq!(pane_id, "%1");
                        if !data.is_empty() {
                            tx.send(data).unwrap();
                        }
                    }
                    other => prop_assert!(false, "expected Output, got {:?}", other),
                }
            }
            drop(tx);
            let mut reader = ControlModeReader::new(rx);
            let got = drain_reader(&mut reader, &buf_sizes);
            prop_assert_eq!(got, bytes);
        }

        /// `format_send_keys` (the `send-keys -H` hex encoding used for every
        /// typed/pasted byte) round-trips losslessly.
        #[test]
        fn format_send_keys_round_trips(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
            let cmd = format_send_keys("%1", &bytes);
            prop_assert_eq!(parse_send_keys_hex(&cmd), bytes);
        }
    }
}

/// Property tests for [`redact_secrets`], the one function standing between a
/// sandboxed launch's bearer token and the log file. A unit test can only pin
/// the shapes someone thought of; these pin the invariant itself — a credential
/// placed anywhere in a command does not survive, whatever surrounds it.
#[cfg(test)]
mod redaction_proptests {
    use proptest::prelude::*;

    use super::redact_secrets;

    /// A token that cannot collide with anything else in a generated command,
    /// so "the output still contains it" can only mean it leaked.
    fn token() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_-]{8,32}".prop_map(|s| format!("tok-{s}"))
    }

    proptest! {
        /// Wherever the proxy grant is spelled — in either case, as either
        /// scheme, quoted or bare — the token does not reach the diagnostic,
        /// and what is left still identifies the launch.
        #[test]
        fn a_proxy_credential_never_survives_redaction(
            token in token(),
            key in prop::sample::select(vec!["HTTP_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]),
            scheme in prop::sample::select(vec!["http", "socks5h"]),
            port in 1024u16..65535,
            quoted in any::<bool>(),
        ) {
            let entry = format!("{key}={scheme}://friring:{token}@127.0.0.1:{port}");
            let entry = if quoted { format!("'{entry}'") } else { entry };
            let cmd = format!(
                "new-window -t friring -n tb-work -P -F '#{{pane_id}}' -c /w -e {entry} claude"
            );
            prop_assert!(cmd.contains(&token), "the launch itself must carry it");

            let out = redact_secrets(&cmd);
            prop_assert!(!out.contains(&token), "leaked: {}", out);
            prop_assert!(!out.contains(&port.to_string()), "leaked the port: {}", out);
            // Still a diagnostic: the verb, the window and the key survive.
            prop_assert!(out.contains("new-window"), "{}", out);
            prop_assert!(out.contains("tb-work"), "{}", out);
            prop_assert!(out.contains(key), "{}", out);
        }

        /// A credential nested inside the quoting a remote or psmux launch adds
        /// is no more visible than one at the top level.
        #[test]
        fn nesting_does_not_hide_a_credential(token in token(), depth in 1usize..4) {
            let mut inner = format!("claude --api-key {token}");
            for _ in 0..depth {
                inner = format!("/bin/sh -lc '{}'", inner.replace('\'', ""));
            }
            let out = redact_secrets(&inner);
            prop_assert!(!out.contains(&token), "leaked at depth {}: {}", depth, out);
        }

        /// Arbitrary text — a malformed command, unbalanced quotes, non-ASCII —
        /// must neither panic nor change meaning on a second pass. This runs
        /// over anything that reaches a failure path, including tmux's own
        /// error replies.
        #[test]
        fn redaction_is_total_and_stable(text in ".{0,200}") {
            let once = redact_secrets(&text);
            prop_assert_eq!(redact_secrets(&once), once);
        }
    }
}
