//! Picking up the status a **sandboxed** agent reported through its file
//! channel, and filing it exactly where every other channel files one.
//!
//! `docs/SANDBOX.md` §Status signals and ADR-29. An unsandboxed agent's hook
//! runs `friring-cli session signal`, which writes `sessions.hook_state`
//! directly; a remote agent's sets a tmux pane user option that
//! [`App::drain_remote_hook_events`](super::App::drain_remote_hook_events)
//! turns into the same write. Neither works from inside a boundary — the binary
//! need not be there, and database write access from inside *is* the escape,
//! because the automations stored in that database are shell commands the host
//! runs. So a sandboxed agent appends a word to a file, and this module is the
//! third writer of that one column.
//!
//! It is deliberately the *thinnest* of the three. Everything specific to
//! reading a file an agent controls lives on the other side of two calls:
//! [`crate::paths::take_session_signal`] takes the file out of the sandbox's
//! reach and refuses anything friring will not read, and
//! [`crate::session::parse_status_signal`] turns its text into a
//! [`SignalState`] or into nothing. What arrives here is already an enum, so
//! the only value this module can put in the database is one of four
//! `&'static str`s — the property that keeps a status report from becoming a
//! host command.
//!
//! Failure is per session and never fatal: a file that will not parse is
//! dropped and the next one is read normally, because the poll it would
//! otherwise stall is the same poll every session's status is derived from.

use crate::session::{parse_status_signal, SessionId, SignalState};
use crate::storage::Database;

use super::App;

/// Ticks between two sweeps of the signal directories.
///
/// Ten ticks is ~100 ms, the same budget the persisted hook columns are
/// re-read on, and for the same reason: a status that shows a tenth of a second
/// late is under any perceptible threshold, while a filesystem call per
/// sandboxed session per 10 ms tick is not free. Offset by half a period so the
/// two sweeps land on different ticks rather than stacking their I/O onto one.
const SIGNAL_POLL_TICKS: u64 = 10;

impl App {
    /// Read every sandboxed session's status file and persist what it says.
    ///
    /// Only sessions carrying a sandbox profile are looked at: a signal
    /// directory is minted by the launch that applies a boundary, so for anyone
    /// else there is nothing to find and the syscall would be pure waste. That
    /// also means an unsandboxed session's status keeps travelling exactly the
    /// way it did before this channel existed.
    pub(super) fn drain_sandbox_status_signals(&mut self) {
        if self.metrics.tick_count % SIGNAL_POLL_TICKS != SIGNAL_POLL_TICKS / 2 {
            return;
        }
        let sandboxed: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|s| s.info.sandbox_profile.is_some())
            .map(|s| s.info.id)
            .collect();
        if sandboxed.is_empty() {
            return;
        }
        if !apply_status_signals(&self.db, &sandboxed).is_empty() {
            // Our own connection's write does not move `data_version`, so the
            // version gate would not notice it — force the reload that makes
            // this tick's derivation see the row.
            self.invalidate_hook_state_cache();
        }
    }
}

/// Take each session's status file, and persist the states that changed.
///
/// Returns what was written, newest value per session, so a caller (and a test)
/// can tell a quiet sweep from one that moved something.
///
/// A file repeating the state already on record is dropped rather than
/// re-stamped: `state_at` is what decides whether a `done` has been
/// acknowledged, so re-writing an unchanged `done` would resurrect it as unseen
/// and fire its notification again. This is the same rule the remote
/// pane-option channel applies, for the same reason.
///
/// **What "on record" means is the database, not this process's cache.** friring
/// supports several instances against one database (ADR-7b), and the bridge
/// depends on this dedupe in a way nothing else does: S8 clears a relaunching
/// child's hook row precisely so that only its *new* agent's report can satisfy
/// S9. Asked of a cache, a peer instance that had not reloaded since the clear
/// would see the previous life's state, call the first report a repeat, drop
/// it — and the relaunch would die at its readiness timeout with a healthy agent
/// running. The row is read only for a session that actually produced a signal
/// this sweep, so the cost is one indexed lookup per state change rather than
/// per tick.
pub(crate) fn apply_status_signals(
    db: &Database,
    sandboxed: &[SessionId],
) -> Vec<(SessionId, SignalState)> {
    let mut applied = Vec::new();
    for &id in sandboxed {
        // One take per session per sweep: the file holds every event since the
        // last one, and only the newest is a state anyone still wants.
        let Some(state) = crate::paths::take_session_signal(&id.to_string())
            .as_deref()
            .and_then(parse_status_signal)
        else {
            continue;
        };
        // A row friring cannot read is not evidence of a repeat, so the report
        // is written: a state written twice costs a redundant notification,
        // where one dropped costs a launch.
        let on_record = db.hook_state_of(id).ok().flatten();
        if on_record.as_deref() == Some(state.as_str()) {
            continue;
        }
        // `state.as_str()` and nothing else: the file's own bytes never reach
        // the database, a command, or a format string used as one.
        if db.set_hook_state(id, state.as_str()).is_ok() {
            applied.push((id, state));
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{create_session_signal_dir, session_signal_file, TestPathGuard};

    /// A database and a fabricated data directory, with one sandboxed session
    /// row to signal about. Nothing here reads the developer's own state: every
    /// path resolves under the temp directory the guard pins.
    fn fixture(name: &str) -> (tempfile::TempDir, TestPathGuard, Database, SessionId) {
        let tmp = tempfile::TempDir::new().unwrap();
        let guard = TestPathGuard::new(tmp.path().join(name));
        let db = Database::open_in_memory().expect("in-memory database");
        let mut row = stored_session("sandboxed");
        row.sandbox_profile = Some("dev".to_string());
        let id = row.id;
        db.upsert_session(&row).unwrap();
        (tmp, guard, db, id)
    }

    /// The persisted shape of a session, with no boundary asked for.
    fn stored_session(name: &str) -> crate::sync::SharedSession {
        crate::sync::SharedSession {
            id: SessionId::default(),
            name: name.to_string(),
            agent: "claude".to_string(),
            backend_id: "friring:@0".to_string(),
            backend_type: "tmux".to_string(),
            agent_session_id: None,
            cwd: None,
            additional_dirs: Vec::new(),
            workspace_dir: None,
            worktrees: Vec::new(),
            shell_backend_id: None,
            sandbox_profile: None,
            sandbox_enforcement: crate::session::SandboxEnforcement::default(),
            parent_session_id: None,
            display_order: None,
            tombstone: false,
            tombstone_at: None,
            mux: crate::session::MuxIdentity::default(),
            egress: crate::session::EgressRecord::default(),
            sandbox_overlay: None,
        }
    }

    /// The persisted hook rows, for asserting what a sweep wrote.
    fn hooks(db: &Database) -> std::collections::HashMap<SessionId, crate::storage::HookRow> {
        db.load_hook_states().unwrap()
    }

    /// Plant what an agent's hook would have appended.
    fn write_signal(id: SessionId, contents: &[u8]) {
        create_session_signal_dir(&id.to_string()).unwrap();
        std::fs::write(session_signal_file(&id.to_string()).unwrap(), contents).unwrap();
    }

    #[test]
    fn a_well_formed_file_drives_the_status_transition() {
        let (_tmp, _guard, db, id) = fixture("transition");
        create_session_signal_dir(&id.to_string()).unwrap();
        assert!(apply_status_signals(&db, &[id]).is_empty());

        for state in [
            SignalState::Idle,
            SignalState::Working,
            SignalState::Blocked,
            SignalState::Done,
        ] {
            write_signal(id, format!("{}\n", state.as_str()).as_bytes());
            assert_eq!(apply_status_signals(&db, &[id]), [(id, state)]);
            assert_eq!(
                hooks(&db)[&id].state.as_deref(),
                Some(state.as_str()),
                "{state:?} did not reach the database"
            );
        }

        // Several events between two sweeps: the newest is the state, and the
        // ones before it are history.
        write_signal(id, b"working\nblocked\nworking\n");
        assert_eq!(
            apply_status_signals(&db, &[id]),
            [(id, SignalState::Working)]
        );

        // A file repeating what is already on record is dropped: re-stamping a
        // `done` would resurrect it as unseen and re-fire its notification.
        write_signal(id, b"working\n");
        assert!(apply_status_signals(&db, &[id]).is_empty());
    }

    /// The dedupe asks the **database**, so a hook row another instance cleared
    /// is seen as cleared.
    ///
    /// S8 clears a relaunching child's hook row precisely so only its new
    /// agent's report can satisfy S9. Any friring on the same database may be
    /// the one that picks that report up (ADR-7b), and a peer that dedupes
    /// against its own cache would compare the first report of the new agent
    /// against the *previous life's* state, call it a repeat and drop it. The
    /// relaunch then dies at its readiness timeout with a healthy agent running.
    #[test]
    fn a_report_after_a_cleared_row_is_written_even_when_it_repeats_the_old_state() {
        let (_tmp, _guard, db, id) = fixture("relaunch");
        create_session_signal_dir(&id.to_string()).unwrap();

        write_signal(id, b"working\n");
        assert_eq!(
            apply_status_signals(&db, &[id]),
            [(id, SignalState::Working)]
        );

        // What S8 does at the gate release of a relaunch: clear the row, then
        // take the moment S9 will compare every later stamp against.
        db.clear_hook_state(id).unwrap();
        let opened_at = crate::sync::current_time_millis() as i64;

        // The new agent's first report happens to say the same word its previous
        // life ended on. It is the *new* one, and it must land.
        write_signal(id, b"working\n");
        assert_eq!(
            apply_status_signals(&db, &[id]),
            [(id, SignalState::Working)],
            "a report after a cleared row was dropped as a repeat"
        );

        // S9's own predicate, not a stricter one. `state_at` is milliseconds, so
        // two writes inside one tick are equal and a `>` here would fail on a
        // fast machine while the thing it is checking held perfectly. The clear
        // above set the column to NULL, so a dropped report leaves `None` and
        // this still catches it.
        let stamped = hooks(&db)[&id]
            .state_at
            .expect("the report must leave a stamp; the clear left none");
        assert!(
            stamped >= opened_at,
            "the report was written without a stamp this launch could claim, \
             which is the whole of S9's proof"
        );
    }

    /// Every shape of hostile file, one after another through the *same* poll:
    /// each is refused, none writes anything, and the well-formed file after
    /// them still lands. The sequence is the point — a channel that rejects
    /// safely once but cannot be used again has still been taken down.
    #[test]
    fn hostile_files_are_refused_without_wedging_the_poll() {
        let (_tmp, _guard, db, id) = fixture("hostile");
        create_session_signal_dir(&id.to_string()).unwrap();
        let status = session_signal_file(&id.to_string()).unwrap();
        let refuse = |what: &str| {
            assert!(
                apply_status_signals(&db, &[id]).is_empty(),
                "the poll accepted {what}"
            );
            assert_eq!(hooks(&db)[&id].state, None, "{what} wrote a state");
        };

        std::fs::write(&status, b"").unwrap();
        refuse("an empty file");
        std::fs::write(&status, b"wor").unwrap();
        refuse("a truncated write");
        // A state word with something riding along — the one that must never
        // reach a shell, a query, or a format string used as either.
        std::fs::write(&status, b"done; touch pwned\n").unwrap();
        refuse("a state word with a command after it");
        std::fs::write(&status, b"$(id)\n").unwrap();
        refuse("a command substitution");
        std::fs::write(&status, b"'; DROP TABLE sessions; --\n").unwrap();
        refuse("a SQL fragment");
        std::fs::write(&status, b"done\n\xff\xfe").unwrap();
        refuse("invalid UTF-8");
        std::fs::write(&status, "working\n".repeat(2000)).unwrap();
        refuse("a file past the size cap");
        std::fs::create_dir(&status).unwrap();
        refuse("a directory where the file goes");

        #[cfg(unix)]
        {
            let outside = _tmp.path().join("host-file");
            std::fs::write(&outside, "done\n").unwrap();
            std::os::unix::fs::symlink(&outside, &status).unwrap();
            refuse("a symlink pointing out of the boundary");
            assert_eq!(
                std::fs::read_to_string(&outside).unwrap(),
                "done\n",
                "the file a symlink pointed at was disturbed"
            );
        }

        // The channel still works after every one of them.
        write_signal(id, b"done\n");
        assert_eq!(apply_status_signals(&db, &[id]), [(id, SignalState::Done)]);
    }

    /// A session with no profile has no signal directory, so nothing about it
    /// changes — including when a file is planted where its directory would be.
    #[test]
    fn an_unsandboxed_session_is_left_alone() {
        let (_tmp, _guard, db, sandboxed) = fixture("unsandboxed");
        let plain = stored_session("plain");
        assert_eq!(plain.sandbox_profile, None);
        db.upsert_session(&plain).unwrap();

        // Even with a file sitting exactly where the poll would look, a session
        // the poll is never handed is never read.
        write_signal(plain.id, b"done\n");
        assert!(apply_status_signals(&db, &[sandboxed]).is_empty());
        assert_eq!(hooks(&db)[&plain.id].state, None);
        // The file is still there: nothing consumed it either.
        let planted = session_signal_file(&plain.id.to_string()).unwrap();
        assert!(planted.exists());
    }

    /// The two ends of the channel, tied together by the shipped payload.
    ///
    /// `/bin/sh` runs the exact hook command the hooks extension installs, with
    /// the environment a sandboxed launch exports, and the poll reads what it
    /// wrote. A payload that appends somewhere else, quotes the path wrongly, or
    /// spells a state friring does not accept fails here rather than in front of
    /// a user; so does a poll that looks in the wrong place.
    ///
    /// The claude payload is the one asserted because it is the only one that
    /// carries every event, including the `Notification` command that reads its
    /// stdin. Nothing here runs an agent, reaches the network, or touches the
    /// user's own configuration: the "agent" is `sh`, the settings file is the
    /// repository's own asset, and every path is under the test temp directory.
    #[cfg(unix)]
    #[test]
    fn the_shipped_hook_payload_writes_what_the_poll_reads() {
        use std::io::Write as _;

        const CLAUDE_HOOKS: &str = include_str!("../../extensions/hooks/claude.json");
        let payload: serde_json::Value = serde_json::from_str(CLAUDE_HOOKS).unwrap();
        let command = |event: &str| {
            payload["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("{event} has no command"))
                .to_string()
        };

        let (_tmp, _guard, db, id) = fixture("payload");
        create_session_signal_dir(&id.to_string()).unwrap();
        let file = session_signal_file(&id.to_string()).unwrap();

        // Ordered so every step is a real transition — two `working` events in
        // a row would be deduped, and prove nothing about the second hook.
        for (event, expected) in [
            ("SessionStart", SignalState::Idle),
            ("UserPromptSubmit", SignalState::Working),
            // The permission payload claude pipes in, which the command greps.
            ("Notification", SignalState::Blocked),
            ("PreToolUse", SignalState::Working),
            ("Stop", SignalState::Done),
        ] {
            let mut child = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command(event))
                .env(crate::paths::SIGNAL_FILE_ENV, &file)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("/bin/sh");
            // Four of the five payloads never read their stdin — only
            // `Notification` pipes it through `cat` — so the hook is entitled to
            // have printed its line and exited before this write lands, which
            // closes the pipe under it. `BrokenPipe` is that shell finishing
            // early and nothing else: a `Notification` payload that stopped
            // consuming stdin would fail the state assertion below, which is
            // where that claim is actually made.
            let write = child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(br#"{"message":"Claude needs your permission to use Bash"}"#);
            if let Err(e) = write {
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::BrokenPipe,
                    "{event}: the hook's stdin refused the payload: {e}"
                );
            }
            assert!(child.wait().unwrap().success(), "{event} hook failed");

            // The assertion runs the whole way through: file → poll → database.
            assert_eq!(
                apply_status_signals(&db, &[id]),
                [(id, expected)],
                "the {event} hook did not report {expected:?}"
            );
            assert!(!file.exists(), "the {event} signal was not consumed");
        }

        // And with the variable unset — every unsandboxed session — the very
        // same command calls the CLI instead and writes no file at all.
        let bin = _tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let record = _tmp.path().join("cli-calls");
        let stub = bin.join("friring-cli");
        std::fs::write(&stub, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$RECORD\"\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command("Stop"))
            .env_remove(crate::paths::SIGNAL_FILE_ENV)
            .env("RECORD", &record)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .status()
            .expect("/bin/sh");
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            "session signal --state done\n"
        );
        assert!(!file.exists(), "an unsandboxed hook wrote a signal file");
    }
}
