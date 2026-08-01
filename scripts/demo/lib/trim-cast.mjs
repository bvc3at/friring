#!/usr/bin/env node
// Cut the tmux-client detach tail off a recorded demo cast, in place.
//
// The recording is an attached tmux client (see record.sh): when the recorder
// detaches it to stop filming, the client leaves the alternate screen, resets
// the terminal and prints "[detached]" on the primary screen — all of which
// asciinema dutifully records, so every clip would otherwise end on a black
// teardown frame held for the full last-frame duration. None of that is demo
// content. The client emits leave-alt-screen (ESC [?1049l) exactly once, at
// detach — the TUI inside the pane never toggles it on the outer terminal
// (tmux redraws pane content absolutely) — so everything from that event on
// is the tail. Drop it; agg then holds the final live TUI frame instead.
//
// A cast with no such event did not end with the client detaching cleanly —
// that is not a stream this trim understands, so fail closed rather than
// ship whatever it is (same rule as every other recorder check).
//
// Usage: trim-cast.mjs <file.cast>
import fs from 'node:fs';

const castPath = process.argv[2];
if (!castPath || !fs.existsSync(castPath)) {
  console.error('usage: trim-cast.mjs <file.cast>');
  process.exit(2);
}

const LEAVE_ALT_SCREEN = '[?1049l';

// Escape sequences are built from char codes rather than written literally: a
// literal ESC (and especially a NUL inside a control-range character class)
// puts raw control bytes in this file, which makes git treat the source as
// binary and stops it diffing.
const ESC = String.fromCharCode(27);
const BEL = String.fromCharCode(7);
const STRIP = [
  new RegExp(`${ESC}\\][^${BEL}${ESC}]*(?:${BEL}|${ESC}\\\\)`, 'g'), // OSC … BEL/ST
  new RegExp(`${ESC}\\[[0-9;?]*[ -/]*[@-~]`, 'g'), // CSI
  new RegExp(`${ESC}[@-Z\\\\-_]`, 'g'), // two-character escapes
];

// Does this event draw anything, or is it pure terminal control?
//
// The teardown is not guaranteed to arrive as ONE event. The client's writes
// are chunked by the pty, so the screen-clear and the leave-alt-screen that
// follows it can land in SEPARATE events — and cutting at the leave-alt-screen
// alone then keeps the clear, leaving a blank final frame that agg dutifully
// holds for the whole `--last-frame-duration`. That is how a clip ends on an
// empty screen, and it is intermittent because it depends on how the bytes
// happened to be split: nine clips recorded in one batch were clean and the
// tenth ended on a blank screen. Strip the escape sequences and see whether any
// glyph is left — a real TUI paint always draws something, a clear never does.
function hasPrintable(s) {
  let t = s;
  for (const re of STRIP) t = t.replace(re, '');
  for (const ch of t) {
    const c = ch.codePointAt(0);
    // Anything past space that is not DEL is ink on the screen.
    if (c > 0x20 && c !== 0x7f) return true;
  }
  return false;
}

// asciicast v2: a JSON header line, then one JSON event array per line.
const lines = fs.readFileSync(castPath, 'utf8').split('\n');
let cut = -1;
for (let i = 1; i < lines.length; i++) {
  if (!lines[i]) continue;
  const ev = JSON.parse(lines[i]);
  if (ev[1] === 'o' && ev[2].includes(LEAVE_ALT_SCREEN)) cut = i;
}
if (cut < 2) {
  // < 2: never found, or the cast is nothing but the teardown.
  console.error(`trim-cast.mjs: no exit tail found in ${castPath} — not a clean recording`);
  process.exit(1);
}
// Then walk back over any content-free events immediately before it: those are
// the rest of the same teardown, split across writes. Bounded, so a genuinely
// odd cast can never trim the demo itself away — the tail is a handful of
// writes, never dozens.
for (let n = 0; cut > 2 && n < 20; n++) {
  const prev = lines[cut - 1];
  if (!prev) {
    cut--;
    continue;
  }
  const ev = JSON.parse(prev);
  if (ev[1] !== 'o' || hasPrintable(ev[2])) break;
  cut--;
}
fs.writeFileSync(castPath, lines.slice(0, cut).join('\n') + '\n');
