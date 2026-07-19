//! Clipboard fallbacks for environments where the native clipboard can't
//! reach the user.
//!
//! That gap has two shapes. On Linux, `arboard` needs a display-server
//! connection (X11/Wayland), so it is simply *unavailable* over SSH, under a
//! display-less tmux, or in a WSL distro without WSLg. On macOS (and Windows)
//! the native clipboard API is reachable even from an SSH login — there
//! `arboard` is *available but wrong*: the write lands on the **host's**
//! clipboard, a machine the user isn't looking at, while reporting success
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
use std::process::{Command, Stdio};

/// True when the native clipboard belongs to a different machine than the one
/// whose screen the user is watching, so even a *successful* native write
/// would land where the user isn't.
///
/// The concrete case is a macOS host reached over SSH: NSPasteboard accepts
/// writes from an SSH login, so `arboard` "succeeds" onto the SSH host's
/// clipboard and the terminal-routed fallbacks (which do reach the user)
/// never get a chance. Detected from the launch environment: an SSH session
/// (`SSH_TTY`/`SSH_CONNECTION`) whose display-server clipboard, if any, does
/// not follow the connection back to the user.
pub(crate) fn native_clipboard_is_remote() -> bool {
    native_targets_wrong_machine(
        std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some(),
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some(),
        cfg!(any(target_os = "macos", target_os = "windows")),
    )
}

/// Env-free core of [`native_clipboard_is_remote`].
///
/// `ssh`: the process lives in an SSH session, so the user's screen is on the
/// client side of the connection. `display_env`: `DISPLAY`/`WAYLAND_DISPLAY`
/// is set — on an X11/Wayland platform that display was forwarded through SSH
/// (`ssh -X`), so the native clipboard follows it back to the user and is the
/// right target after all. `host_owned_clipboard`: platforms (macOS/Windows)
/// whose clipboard API always addresses the local host regardless of any
/// display variable.
fn native_targets_wrong_machine(ssh: bool, display_env: bool, host_owned_clipboard: bool) -> bool {
    ssh && (host_owned_clipboard || !display_env)
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

/// Copy `text` to the terminal's clipboard via OSC 52
/// (`ESC ] 52 ; c ; <base64> BEL`), written directly to stdout.
///
/// Best-effort fire-and-forget: `Ok` means the sequence reached stdout, not
/// that the terminal honoured it. Only meaningful when **not** behind tmux
/// (see the module docs); inside tmux use [`tmux_copy`] instead. Safe to emit
/// while ratatui owns the screen — the sequence paints nothing and moves no
/// cursor.
pub(crate) fn osc52_copy(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(&osc52_sequence(text))?;
    out.flush()
}

fn osc52_sequence(text: &str) -> Vec<u8> {
    let mut seq = b"\x1b]52;c;".to_vec();
    seq.extend_from_slice(base64(text.as_bytes()).as_bytes());
    seq.push(0x07);
    seq
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_is_trusted_locally_and_distrusted_over_ssh() {
        // No SSH → native is the user's clipboard, whatever the platform.
        assert!(!native_targets_wrong_machine(false, false, true));
        assert!(!native_targets_wrong_machine(false, true, false));
        // SSH with no display env → nothing routes native back to the user.
        assert!(native_targets_wrong_machine(true, false, false));
        assert!(native_targets_wrong_machine(true, false, true));
    }

    #[test]
    fn forwarded_display_reroutes_native_only_on_x11_platforms() {
        // ssh -X on Linux: the X clipboard follows the display to the user.
        assert!(!native_targets_wrong_machine(true, true, false));
        // macOS/Windows: DISPLAY can't reroute NSPasteboard/Win32 — still the
        // host's clipboard.
        assert!(native_targets_wrong_machine(true, true, true));
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
        assert_eq!(osc52_sequence("hi"), b"\x1b]52;c;aGk=\x07".to_vec());
    }
}
