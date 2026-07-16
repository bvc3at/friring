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

const LEAVE_ALT_SCREEN = '[?1049l';

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
fs.writeFileSync(castPath, lines.slice(0, cut).join('\n') + '\n');
