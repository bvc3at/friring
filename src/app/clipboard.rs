//! Clipboard fallbacks for display-less environments.
//!
//! `arboard` needs a display-server connection (X11/Wayland/AppKit/Win32), so
//! it is unavailable exactly where friring often runs: inside tmux over SSH,
//! or in a WSL distro without WSLg. Two fallbacks cover that gap, tried in the
//! order that actually works:
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

/// Set the outer terminal's clipboard through the tmux server friring is
/// attached to (`$TMUX`), via `tmux load-buffer -w -`.
///
/// Works under tmux's default `set-clipboard external`, where a raw
/// application OSC 52 is dropped: tmux is the one issuing the terminal escape,
/// which `external` allows. `Ok(())` means the `tmux` process exited
/// successfully (the buffer was set and the terminal clipboard *attempted* —
/// tmux still needs the outer terminal's `Ms` capability to reach the system
/// clipboard, but the tmux paste buffer is set regardless).
pub(crate) fn tmux_copy(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-w", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tmux stdin unavailable")
        })?;
        stdin.write_all(text.as_bytes())?;
    } // drop stdin → EOF so tmux stops reading
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "tmux load-buffer exited with {status}"
        )))
    }
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
