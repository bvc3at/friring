//! OSC 52 clipboard-copy extraction from pane output streams.
//!
//! Programs running *inside* a session pane set the clipboard by writing
//! `ESC ] 52 ; <selection> ; <base64> (BEL | ESC \)` — Claude Code's `/copy`,
//! nvim's OSC 52 clipboard provider, tmux inside the pane, etc. In a normal
//! terminal that escape reaches the emulator; under friring it reaches the
//! vt100 parser, which ignores it — and its `unhandled_osc` callback can't
//! recover the copy either, because the underlying vte parser truncates OSC
//! payloads at 1 KiB, silently corrupting any real copy. So the raw `%output`
//! byte stream is scanned *before* the parser by [`Osc52Scanner`], and every
//! completed payload is routed through the app's clipboard stack (which picks
//! native / `tmux load-buffer` / outward OSC 52 — see `app::clipboard`).
//!
//! The scanner is a small state machine fed arbitrary chunks: sequences split
//! across read boundaries reassemble, non-52 OSCs are consumed without being
//! buffered, and the stream itself is never modified (the parser still sees
//! every byte; it ignores OSC 52 harmlessly).
//!
//! The **tmux-passthrough-wrapped** form is recognized too: a program that
//! sees `$TMUX` set (every friring pane) may wrap the escape as
//! `ESC P tmux ; <inner with every ESC doubled> ESC \` for tmux to unwrap —
//! Claude Code's `/copy` does exactly this. tmux's *default*
//! `allow-passthrough off` silently discards that DCS, so the copy died in
//! the inner tmux long before any terminal; the raw `%output` tap still
//! carries it, and the doubled introducer (`ESC ESC ]`) and doubled inner ST
//! (`ESC ESC \`) parse here by letting consecutive ESCs stand for one.
//!
//! Only 7-bit `ESC ]` introducers are recognized — every real emitter uses
//! them; the 8-bit C1 form (`0x9d`) would collide with UTF-8 continuation
//! bytes without full decoding.

/// Cap on a buffered `<selection>;<base64>` payload. A sequence that exceeds
/// it is consumed and dropped whole — a truncated copy would be worse than a
/// failed one. 8 MiB of base64 is ~6 MiB of text, far beyond any real copy,
/// while bounding what a runaway pane can make us buffer.
const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// The OSC prefix that selects sequence 52, matched byte-wise so a chunk
/// boundary can fall inside it.
const PREFIX: &[u8; 3] = b"52;";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// Outside any sequence of interest.
    #[default]
    Ground,
    /// Saw `ESC`; a `]` opens an OSC.
    Esc,
    /// Inside an OSC, still matching [`PREFIX`]; `usize` bytes matched so far.
    Prefix(usize),
    /// Inside an OSC 52, buffering the payload.
    Payload,
    /// Inside some other (or oversized) OSC — consume to the terminator
    /// without buffering.
    Discard,
    /// Saw `ESC` inside an OSC 52 payload; `\` (ST) completes it.
    PayloadEsc,
    /// Saw `ESC` inside a discarded OSC; `\` (ST) ends it.
    DiscardEsc,
}

/// Incremental OSC 52 extractor. Feed the pane byte stream through
/// [`scan`](Self::scan) in any chunking; each returned `String` is one
/// completed clipboard write, in stream order.
#[derive(Debug, Default)]
pub struct Osc52Scanner {
    state: State,
    payload: Vec<u8>,
}

impl Osc52Scanner {
    /// Scan the next chunk of the stream, returning any clipboard copies whose
    /// sequences completed within it.
    pub fn scan(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut copies = Vec::new();
        let mut rest = chunk;
        while !rest.is_empty() {
            // Fast path: in Ground (the overwhelmingly common state) jump
            // straight to the next ESC instead of stepping per byte.
            if self.state == State::Ground {
                let Some(esc) = rest.iter().position(|&b| b == ESC) else {
                    return copies;
                };
                self.state = State::Esc;
                rest = &rest[esc + 1..];
                continue;
            }
            self.step(rest[0], &mut copies);
            rest = &rest[1..];
        }
        copies
    }

    fn step(&mut self, byte: u8, copies: &mut Vec<String>) {
        use State::*;
        self.state = match self.state {
            Ground => {
                if byte == ESC {
                    Esc
                } else {
                    Ground
                }
            }
            Esc => match byte {
                b']' => {
                    self.payload.clear();
                    Prefix(0)
                }
                ESC => Esc,
                _ => Ground,
            },
            Prefix(matched) => match byte {
                // The OSC ended (or broke) before `52;` was complete — some
                // shorter-numbered sequence like `ESC ] 5 BEL`.
                BEL => Ground,
                ESC => DiscardEsc,
                b if b == PREFIX[matched] => {
                    if matched + 1 == PREFIX.len() {
                        Payload
                    } else {
                        Prefix(matched + 1)
                    }
                }
                // Any other control byte aborts an OSC (mirrors terminal
                // parsers); any other printable diverges to a non-52 OSC.
                b if b < 0x20 => Ground,
                _ => Discard,
            },
            Payload => match byte {
                BEL => {
                    self.complete(copies);
                    Ground
                }
                ESC => PayloadEsc,
                b if b < 0x20 => {
                    // A stray control byte inside an OSC aborts it.
                    self.payload.clear();
                    Ground
                }
                b => {
                    if self.payload.len() >= MAX_PAYLOAD {
                        tracing::debug!(
                            "dropping oversized OSC 52 payload (> {MAX_PAYLOAD} bytes)"
                        );
                        self.payload.clear();
                        Discard
                    } else {
                        self.payload.push(b);
                        Payload
                    }
                }
            },
            Discard => match byte {
                BEL => Ground,
                ESC => DiscardEsc,
                b if b < 0x20 => Ground,
                _ => Discard,
            },
            PayloadEsc => match byte {
                b'\\' => {
                    self.complete(copies);
                    Ground
                }
                // Consecutive ESCs stay pending: a tmux-passthrough wrap (see
                // the module docs) doubles the ESC of an inner ST, so a
                // wrapped terminator arrives as `ESC ESC \`.
                ESC => PayloadEsc,
                _ => {
                    // The ESC aborted the OSC and starts a new sequence.
                    self.payload.clear();
                    Self::reprocess_after_abort(byte)
                }
            },
            DiscardEsc => match byte {
                b'\\' => Ground,
                ESC => DiscardEsc,
                _ => Self::reprocess_after_abort(byte),
            },
        };
    }

    /// State for the byte after an `ESC` that aborted an OSC (anything but the
    /// `\` of ST, and not another ESC): that ESC begins a *new* escape
    /// sequence containing `byte`.
    fn reprocess_after_abort(byte: u8) -> State {
        match byte {
            b']' => State::Prefix(0),
            _ => State::Ground,
        }
    }

    /// A full OSC 52 payload (`<selection>;<base64>`) terminated cleanly:
    /// decode and emit it. The selection field is ignored — whichever
    /// selection (`c`, `p`, `s0`…) the program addressed, friring has exactly
    /// one clipboard to route it to. A `?` payload is a clipboard *read*
    /// request; answering one would leak the clipboard to whatever runs in the
    /// pane, so it is dropped (terminals refuse these too). An
    /// empty/undecodable payload ("clear the clipboard" in xterm terms) is
    /// dropped rather than clobbering the user's clipboard with nothing.
    fn complete(&mut self, copies: &mut Vec<String>) {
        let payload = std::mem::take(&mut self.payload);
        let Some(sep) = payload.iter().position(|&b| b == b';') else {
            return;
        };
        let data = &payload[sep + 1..];
        if data == b"?" {
            return;
        }
        let Some(bytes) = base64_decode(data) else {
            return;
        };
        if bytes.is_empty() {
            return;
        }
        copies.push(String::from_utf8_lossy(&bytes).into_owned());
    }
}

/// Standard-alphabet base64 (RFC 4648) decode; `=` padding optional (some
/// emitters omit it). `None` on any byte outside the alphabet, misplaced
/// padding, or an impossible length — an invalid payload is dropped, never
/// half-decoded. Hand-rolled like the encoder in `app::clipboard`: a few lines
/// beat a dependency, and the `agent` module can't reach `app` anyway.
fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u32> {
        Some(match b {
            b'A'..=b'Z' => u32::from(b - b'A'),
            b'a'..=b'z' => u32::from(b - b'a') + 26,
            b'0'..=b'9' => u32::from(b - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    // Padding may only be trailing, and only up to two `=`.
    let end = input.iter().position(|&b| b == b'=').unwrap_or(input.len());
    let data = &input[..end];
    let padding = &input[end..];
    if padding.len() > 2 || padding.iter().any(|&b| b != b'=') {
        return None;
    }
    if data.len() % 4 == 1 {
        return None; // 6 bits can't form a byte
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3 + 2);
    for chunk in data.chunks(4) {
        let mut acc = 0u32;
        for (i, &b) in chunk.iter().enumerate() {
            acc |= val(b)? << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if chunk.len() >= 3 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() == 4 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> Vec<String> {
        let mut scanner = Osc52Scanner::default();
        chunks.iter().flat_map(|c| scanner.scan(c)).collect()
    }

    #[test]
    fn bel_terminated_copy() {
        assert_eq!(scan_all(&[b"\x1b]52;c;aGVsbG8=\x07"]), ["hello"]);
    }

    #[test]
    fn st_terminated_copy() {
        assert_eq!(scan_all(&[b"\x1b]52;c;aGVsbG8=\x1b\\"]), ["hello"]);
    }

    #[test]
    fn copy_embedded_in_normal_output() {
        let stream = b"plain text\r\n\x1b[31mred\x1b[0m\x1b]52;c;aGk=\x07more";
        assert_eq!(scan_all(&[stream]), ["hi"]);
    }

    #[test]
    fn every_chunk_boundary_reassembles() {
        // The %output framing can split a sequence anywhere, including inside
        // the ESC ] introducer, the 52; prefix, the payload, the ST
        // terminator, and a tmux-passthrough wrap.
        let stream: &[u8] = b"out\x1b]52;c;aGVsbG8=\x1b\\rest\x1b]52;p;d29ybGQ=\x07\
                              \x1bPtmux;\x1b\x1b]52;c;IQ==\x1b\x1b\\\x1b\\";
        for cut in 0..stream.len() {
            let (a, b) = stream.split_at(cut);
            assert_eq!(
                scan_all(&[a, b]),
                ["hello", "world", "!"],
                "split at byte {cut}"
            );
        }
    }

    #[test]
    fn tmux_passthrough_wrapped_copy_is_extracted() {
        // Inside tmux, emitters wrap the OSC 52 in the DCS passthrough with
        // inner ESCs doubled (Claude Code's `/copy` shape: BEL-terminated
        // inner, `\x1bPtmux;` + doubled introducer + `\x1b\\`).
        assert_eq!(
            scan_all(&[b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\"]),
            ["hello"]
        );
        // ST-terminated inner (nvim's shape): the inner ST's ESC is doubled
        // by the wrap too.
        assert_eq!(
            scan_all(&[b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x1b\x1b\\\x1b\\"]),
            ["hello"]
        );
    }

    #[test]
    fn byte_at_a_time_reassembles() {
        let stream: &[u8] = b"\x1b]52;c;Zm9vYmFy\x07";
        let chunks: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(scan_all(&chunks), ["foobar"]);
    }

    #[test]
    fn query_is_never_answered_or_copied() {
        assert_eq!(scan_all(&[b"\x1b]52;c;?\x07"]), Vec::<String>::new());
    }

    #[test]
    fn selection_field_variants_all_route_to_the_clipboard() {
        assert_eq!(scan_all(&[b"\x1b]52;;aGk=\x07"]), ["hi"]);
        assert_eq!(scan_all(&[b"\x1b]52;pc0;aGk=\x07"]), ["hi"]);
    }

    #[test]
    fn missing_selection_separator_is_dropped() {
        assert_eq!(scan_all(&[b"\x1b]52;aGk=\x07"]), Vec::<String>::new());
    }

    #[test]
    fn other_osc_numbers_do_not_copy() {
        // Includes the prefix-shaped `520`: diverges after `52`.
        let stream = b"\x1b]2;title\x07\x1b]520;c;aGk=\x07\x1b]52;c;b2s=\x07";
        assert_eq!(scan_all(&[stream]), ["ok"]);
    }

    #[test]
    fn invalid_and_empty_base64_are_dropped() {
        assert_eq!(scan_all(&[b"\x1b]52;c;not*b64\x07"]), Vec::<String>::new());
        assert_eq!(scan_all(&[b"\x1b]52;c;\x07"]), Vec::<String>::new());
        // Misplaced padding.
        assert_eq!(scan_all(&[b"\x1b]52;c;aG=s\x07"]), Vec::<String>::new());
    }

    #[test]
    fn unpadded_base64_is_accepted() {
        assert_eq!(scan_all(&[b"\x1b]52;c;aGVsbG8\x07"]), ["hello"]);
    }

    #[test]
    fn interrupting_escape_aborts_and_recovers() {
        // A CSI barging into an unterminated OSC 52 aborts it; the stream
        // keeps scanning cleanly afterwards.
        let stream = b"\x1b]52;c;aGVs\x1b[31m\x1b]52;c;b2s=\x07";
        assert_eq!(scan_all(&[stream]), ["ok"]);
        // An OSC barging in starts over as a fresh sequence.
        let stream = b"\x1b]52;c;aGVs\x1b]52;c;b2s=\x07";
        assert_eq!(scan_all(&[stream]), ["ok"]);
    }

    #[test]
    fn stray_control_byte_aborts_the_sequence() {
        assert_eq!(
            scan_all(&[b"\x1b]52;c;aGk\ndGV4dA==\x07"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn oversized_payload_is_dropped_whole() {
        let mut stream = b"\x1b]52;c;".to_vec();
        stream.resize(stream.len() + MAX_PAYLOAD + 8, b'A');
        stream.push(BEL);
        stream.extend_from_slice(b"\x1b]52;c;b2s=\x07");
        assert_eq!(scan_all(&[&stream]), ["ok"]);
    }

    #[test]
    fn multiple_copies_in_one_chunk_stay_ordered() {
        assert_eq!(
            scan_all(&[b"\x1b]52;c;YQ==\x07\x1b]52;c;Yg==\x07"]),
            ["a", "b"]
        );
    }

    #[test]
    fn non_utf8_payload_is_lossy_not_dropped() {
        // 0xFF 0xFE is valid base64 output but not UTF-8 ("//4=").
        assert_eq!(scan_all(&[b"\x1b]52;c;//4=\x07"]), ["\u{FFFD}\u{FFFD}"]);
    }

    #[test]
    fn base64_decode_rfc4648_vectors() {
        // Mirrors the encoder vectors in `app::clipboard`.
        assert_eq!(base64_decode(b"").as_deref(), Some(&b""[..]));
        assert_eq!(base64_decode(b"Zg==").as_deref(), Some(&b"f"[..]));
        assert_eq!(base64_decode(b"Zm8=").as_deref(), Some(&b"fo"[..]));
        assert_eq!(base64_decode(b"Zm9v").as_deref(), Some(&b"foo"[..]));
        assert_eq!(base64_decode(b"Zm9vYg==").as_deref(), Some(&b"foob"[..]));
        assert_eq!(base64_decode(b"Zm9vYmE=").as_deref(), Some(&b"fooba"[..]));
        assert_eq!(base64_decode(b"Zm9vYmFy").as_deref(), Some(&b"foobar"[..]));
        assert_eq!(base64_decode(b"w6k=").as_deref(), Some("é".as_bytes()));
        assert_eq!(base64_decode(b"Zg"), Some(b"f".to_vec()));
        assert_eq!(base64_decode(b"Z"), None);
        assert_eq!(base64_decode(b"Zg==="), None);
        assert_eq!(base64_decode(b"Zg=a"), None);
    }
}
