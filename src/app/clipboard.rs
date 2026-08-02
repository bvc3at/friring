//! Clipboard fallbacks for environments where the native clipboard can't
//! reach the user.
//!
//! That gap has two shapes. On Linux, `arboard` needs a display-server
//! connection (X11/Wayland), so it is *unavailable* over SSH with no forwarded
//! display, under a display-less tmux, or in a WSL distro without WSLg. (With
//! `ssh -X` / a forwarded `WAYLAND_DISPLAY` the connection *does* reach a
//! display server, and the native clipboard follows it back to the user — see
//! [`native_clipboard_is_remote`].) On macOS (and Windows) the native
//! clipboard API is reachable even from an SSH login — there `arboard` is
//! *available but wrong*: the write lands on the **host's** clipboard, a
//! machine the user isn't looking at, while reporting success
//! ([`native_clipboard_is_remote`] detects this). Two fallbacks cover both
//! shapes, tried in the order that actually works:
//!
//! 1. **`tmux load-buffer -w`** when friring is itself running inside a tmux
//!    client (`$TMUX` set — the common `tmux -> friring` setup). This is the
//!    primary fallback: tmux's default `set-clipboard external` *ignores* an
//!    application's own OSC 52 (the manual: "ignore attempts by applications to
//!    set tmux buffers"), so writing the escape to our stdout is silently
//!    dropped. `load-buffer -w` instead has tmux *itself* set the outer
//!    terminal's clipboard — which `external` permits — and returns an exit
//!    status, so success is real rather than fire-and-forget. Requires tmux
//!    ≥ 3.2 for `-w`, which friring already mandates.
//! 2. **Raw OSC 52** (`ESC ] 52 ; c ; <base64> BEL`) when *not* inside tmux —
//!    e.g. a direct SSH session to an OSC-52-capable terminal. Here nothing
//!    strips the escape, so it reaches the terminal. This path is
//!    fire-and-forget: a terminal without OSC 52 support ignores it silently.
//!
//! Write-only: terminals refuse OSC 52 *reads* for security, so paste has no
//! equivalent fallback — the terminal's own paste keystroke reaches friring as
//! a bracketed paste instead (see `App::handle_paste`).

use std::io::Write;
use std::net::IpAddr;
use std::process::{Command, Stdio};

/// What to tell the user when friring has no clipboard it can *read* for them.
///
/// The terminal emulator's own paste chord still works — it arrives as a
/// bracketed paste (`App::handle_paste`), never touching this module — so this
/// is a hint, not a failure. Both chords are named because the machine that
/// owns the keyboard is not necessarily the one friring runs on: over SSH a
/// `cfg!(target_os)` here would describe the *host*, which is exactly the
/// machine the user isn't typing at.
pub(crate) const PASTE_UNAVAILABLE_HINT: &str = "paste with your terminal's key \
                                                 (Ctrl+Shift+V / Cmd+V)";

/// True when the native clipboard belongs to a different machine than the one
/// whose screen the user is watching, so even a *successful* native write
/// would land where the user isn't.
///
/// The concrete case is a macOS host reached over SSH: NSPasteboard accepts
/// writes from an SSH login, so `arboard` "succeeds" onto the SSH host's
/// clipboard and the terminal-routed fallbacks (which do reach the user)
/// never get a chance. Detected from the launch environment: an SSH session
/// (`SSH_TTY`/`SSH_CONNECTION`) whose display-server clipboard, if any, does
/// not follow the connection back to the user — but *not* a loopback SSH
/// (`ssh localhost`), where host and user are the same machine and native is
/// correct after all.
pub(crate) fn native_clipboard_is_remote() -> bool {
    native_targets_wrong_machine(
        std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some(),
        ssh_connection_is_loopback(),
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some(),
        cfg!(any(target_os = "macos", target_os = "windows")),
    )
}

/// Env-free core of [`native_clipboard_is_remote`].
///
/// `ssh`: the process lives in an SSH session, so the user's screen is on the
/// client side of the connection. `ssh_loopback`: that SSH session terminates
/// on this same host (`ssh localhost` — the connection's server address is a
/// loopback IP), so "host" and "the user's machine" coincide and native is the
/// right target despite the SSH login. `display_env`: `DISPLAY`/`WAYLAND_DISPLAY`
/// is set — on an X11/Wayland platform that display was forwarded through SSH
/// (`ssh -X`), so the native clipboard follows it back to the user and is the
/// right target after all. `host_owned_clipboard`: platforms (macOS/Windows)
/// whose clipboard API always addresses the local host regardless of any
/// display variable.
fn native_targets_wrong_machine(
    ssh: bool,
    ssh_loopback: bool,
    display_env: bool,
    host_owned_clipboard: bool,
) -> bool {
    ssh && !ssh_loopback && (host_owned_clipboard || !display_env)
}

/// True when `$SSH_CONNECTION` reports a loopback server address — i.e. the SSH
/// session terminates on this very host (`ssh localhost` / `ssh ::1`), so the
/// native clipboard is the user's own after all.
///
/// `SSH_CONNECTION` is `"<client-ip> <client-port> <server-ip> <server-port>"`;
/// the server address (3rd field) is the host friring runs on. Absent or
/// unparseable → `false` (treat as a real remote, the safe default: routing a
/// genuinely-remote copy through the tmux/OSC 52 fallback still usually works,
/// whereas trusting native would silently lose it). A non-loopback LAN address
/// pointing back at the same machine (`ssh 192.168.x.x` to self) is not
/// detected here — rare, and it degrades gracefully to the fallback path.
fn ssh_connection_is_loopback() -> bool {
    std::env::var("SSH_CONNECTION")
        .ok()
        .and_then(|conn| conn.split_whitespace().nth(2).map(str::to_owned))
        .and_then(|server_ip| server_ip.parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback())
}

/// Set the outer terminal's clipboard through the tmux server friring is
/// attached to (`$TMUX`), via `tmux load-buffer -w -`.
///
/// Works under tmux's default `set-clipboard external`, where a raw
/// application OSC 52 is dropped: tmux is the one issuing the terminal escape,
/// which `external` allows. `Ok(())` means the `tmux` process exited
/// successfully (the buffer was set and the terminal clipboard *attempted* —
/// tmux still needs the outer terminal's `Ms` capability to reach the system
/// clipboard, but the tmux paste buffer is set regardless). On failure the
/// error carries tmux's own stderr (e.g. `no current client`, or an
/// `unknown flag` on a tmux too old for `-w`) so the status-bar message is
/// actionable rather than a bare exit code.
pub(crate) fn tmux_copy(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-w", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tmux stdin unavailable")
        })?;
        stdin.write_all(text.as_bytes())?;
    } // drop stdin → EOF so tmux stops reading
    let output = child.wait_with_output()?;
    if output.status.success() {
        return Ok(());
    }
    // Prefer tmux's stderr (the real reason); fall back to the exit status when
    // it wrote nothing. The `clipboard_error` wrapper already names the stage,
    // so keep this to the bare detail.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    Err(std::io::Error::other(if detail.is_empty() {
        output.status.to_string()
    } else {
        detail.to_string()
    }))
}

/// Most text one OSC 52 sequence may carry.
///
/// Terminals cap the length of a single escape sequence and **discard the
/// whole thing** past that cap (tmux's `input_osc_52` is the explicit case: it
/// sets `INPUT_DISCARD` rather than truncating) — but a terminal that aborts
/// mid-sequence stops *interpreting* while the rest of the base64 keeps
/// arriving, and prints it as text over whatever ratatui last painted. So an
/// oversized copy is either silent loss or a corrupted screen; refusing it up
/// front is the only outcome the user can act on.
///
/// The de-facto ceiling is a 100,000-byte total sequence. base64 costs 4 bytes
/// per 3, and the `ESC ] 5 2 ; c ;` + `BEL` framing costs 8, leaving
/// `((100_000 - 8) / 4) * 3` bytes of payload.
pub(crate) const OSC52_MAX_BYTES: usize = 74_994;

/// Why an OSC 52 copy didn't happen.
///
/// [`TooLarge`](Osc52Error::TooLarge) is a property of the *text*, not a
/// transport failure: nothing was written, and no fallback would fare better.
/// It carries its own complete message so the caller doesn't prefix it with
/// why the native clipboard was skipped (see `App::clipboard_error`).
#[derive(Debug)]
pub(crate) enum Osc52Error {
    /// Text exceeds what one sequence can carry ([`OSC52_MAX_BYTES`]).
    TooLarge { bytes: usize },
    /// Writing the sequence to stdout failed.
    Write(std::io::Error),
}

impl std::fmt::Display for Osc52Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { bytes } => write!(
                f,
                "Too large to copy over OSC 52 ({bytes} bytes; limit {OSC52_MAX_BYTES})"
            ),
            Self::Write(e) => write!(f, "{e}"),
        }
    }
}

/// Copy `text` to the terminal's clipboard via OSC 52
/// (`ESC ] 52 ; c ; <base64> BEL`), written directly to stdout.
///
/// Best-effort fire-and-forget: `Ok` means the sequence reached stdout, not
/// that the terminal honoured it. Only meaningful when **not** behind tmux
/// (see the module docs); inside tmux use [`tmux_copy`] instead, which has no
/// such size limit — tmux reads the text over a pipe and sets the buffer
/// whatever its length. Safe to emit while ratatui owns the screen — the
/// sequence paints nothing and moves no cursor, provided it is short enough
/// that the terminal doesn't abandon it mid-flight ([`OSC52_MAX_BYTES`]).
pub(crate) fn osc52_copy(text: &str) -> Result<(), Osc52Error> {
    let seq = osc52_sequence(text)?;
    let mut out = std::io::stdout().lock();
    out.write_all(&seq).map_err(Osc52Error::Write)?;
    out.flush().map_err(Osc52Error::Write)
}

fn osc52_sequence(text: &str) -> Result<Vec<u8>, Osc52Error> {
    if text.len() > OSC52_MAX_BYTES {
        return Err(Osc52Error::TooLarge { bytes: text.len() });
    }
    let mut seq = b"\x1b]52;c;".to_vec();
    seq.extend_from_slice(base64(text.as_bytes()).as_bytes());
    seq.push(0x07);
    Ok(seq)
}

/// Standard-alphabet base64 with `=` padding (RFC 4648). Hand-rolled: a few
/// lines beat a new dependency for the one call site above.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, b[0], b[1], b[2]]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Serializes every test — in this module or any other — that mutates the
/// process-global environment the clipboard routing reads (`SSH_*`,
/// `DISPLAY`/`WAYLAND_DISPLAY`).
///
/// One lock shared across all of them, deliberately: a `static` declared
/// *inside* a test function is a distinct instance per function, so
/// same-named per-test locks synchronize nothing and `cargo test`'s threads
/// race on the same variables. (nextest's process-per-test model hides that,
/// which is exactly why the lock has to be right rather than merely present.)
#[cfg(test)]
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Apply `vars` to the process environment (`None` unsets) under
/// [`ENV_LOCK`], restoring the previous values when the returned guard drops
/// — including on a panicking assertion, so one failing test can't leak an
/// SSH-looking environment into the next.
#[cfg(test)]
pub(crate) fn scoped_env(vars: &[(&'static str, Option<&str>)]) -> EnvGuard {
    let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved = vars
        .iter()
        .map(|(k, _)| (*k, std::env::var(k).ok()))
        .collect();
    for (k, v) in vars {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    EnvGuard { saved, _lock: lock }
}

/// Restores what [`scoped_env`] replaced. `saved` is declared before `_lock`
/// so the restore runs while the lock is still held.
#[cfg(test)]
pub(crate) struct EnvGuard {
    saved: Vec<(&'static str, Option<String>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.saved {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_is_trusted_locally_and_distrusted_over_ssh() {
        // No SSH → native is the user's clipboard, whatever the platform.
        assert!(!native_targets_wrong_machine(false, false, false, true));
        assert!(!native_targets_wrong_machine(false, false, true, false));
        // SSH with no display env → nothing routes native back to the user.
        assert!(native_targets_wrong_machine(true, false, false, false));
        assert!(native_targets_wrong_machine(true, false, false, true));
    }

    #[test]
    fn loopback_ssh_keeps_native_trusted() {
        // `ssh localhost`: host == the user's machine, so native is correct
        // even with no display env and a host-owned clipboard (macOS).
        assert!(!native_targets_wrong_machine(true, true, false, true));
        assert!(!native_targets_wrong_machine(true, true, false, false));
    }

    #[test]
    fn forwarded_display_reroutes_native_only_on_x11_platforms() {
        // ssh -X on Linux: the X clipboard follows the display to the user.
        assert!(!native_targets_wrong_machine(true, false, true, false));
        // macOS/Windows: DISPLAY can't reroute NSPasteboard/Win32 — still the
        // host's clipboard.
        assert!(native_targets_wrong_machine(true, false, true, true));
    }

    #[test]
    fn ssh_connection_loopback_parsing() {
        // Holds the shared lock and restores `SSH_CONNECTION` afterwards; the
        // cases below overwrite it in turn.
        let _env = scoped_env(&[("SSH_CONNECTION", None)]);

        let cases = [
            ("127.0.0.1 54321 127.0.0.1 22", true),
            ("::1 54321 ::1 22", true),
            ("10.0.0.120 62266 10.0.0.13 22", false),
            ("garbage", false),
        ];
        for (conn, expected) in cases {
            std::env::set_var("SSH_CONNECTION", conn);
            assert_eq!(
                ssh_connection_is_loopback(),
                expected,
                "SSH_CONNECTION={conn}"
            );
        }
        std::env::remove_var("SSH_CONNECTION");
        assert!(!ssh_connection_is_loopback(), "absent SSH_CONNECTION");
    }

    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_non_ascii_utf8() {
        assert_eq!(base64("é".as_bytes()), "w6k=");
    }

    #[test]
    fn sequence_wraps_payload_in_osc52() {
        assert_eq!(
            osc52_sequence("hi").unwrap(),
            b"\x1b]52;c;aGk=\x07".to_vec()
        );
    }

    #[test]
    fn sequence_refuses_oversized_text_before_writing_anything() {
        let over = "x".repeat(OSC52_MAX_BYTES + 1);
        let err = osc52_sequence(&over).expect_err("past the ceiling");
        assert!(
            matches!(err, Osc52Error::TooLarge { bytes } if bytes == OSC52_MAX_BYTES + 1),
            "got {err:?}"
        );
        // The whole point is that the user sees the size, not a corrupted TUI —
        // and the limit, so the overshoot is a number they can act on.
        assert!(err.to_string().contains(&(OSC52_MAX_BYTES + 1).to_string()));
        assert!(err.to_string().contains(&OSC52_MAX_BYTES.to_string()));
    }

    #[test]
    fn sequence_at_the_ceiling_fits_the_100k_budget() {
        // The ceiling is derived from a 100,000-byte total sequence; a payload
        // exactly at it must still be accepted and must not exceed that budget.
        let at = "x".repeat(OSC52_MAX_BYTES);
        let seq = osc52_sequence(&at).expect("exactly at the ceiling is allowed");
        assert_eq!(seq.len(), 100_000);
    }
}
