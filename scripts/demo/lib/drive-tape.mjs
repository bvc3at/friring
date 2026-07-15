#!/usr/bin/env node
// Drive a demo tape against a running friring TUI, for the cast recorder.
//
// The .tape files stay the single, human-readable description of each demo
// (they are as much documentation as script). This reads one and replays its
// beats as tmux keystrokes into the recorded session, instead of handing it to
// VHS. See scripts/demo/record.sh for why the recorder changed; docs/E2E.md
// and docs/DEVELOPMENT.md for the wider picture.
//
// Only the subset the tapes actually use is interpreted; anything else is a
// hard error rather than a silent no-op, so a tape can never half-run and
// quietly record the wrong thing:
//
//   Output <path>          collected (record.sh reads it via --print-outputs)
//   Set <key> <value>      collected (Width/Height/FontSize size the render)
//   Hide … Show            SKIPPED — the block only exists to launch the TUI
//                          off-camera, which the recorder now does itself
//                          before recording starts
//   Type "<text>"          typed character-by-character (see TYPING_SPEED_MS)
//   Sleep <n>s|<n>ms       real sleep — this is the demo's pacing
//   Enter/Tab/Space/…      a single key
//   Ctrl+<X>               a chord
//
// Usage:
//   drive-tape.mjs <tape> --socket <name> --session <name> [--print-outputs]
//                         [--print-set <Key>] [--print-duration]
import fs from 'node:fs';
import { spawnSync } from 'node:child_process';

const argv = process.argv.slice(2);
const tapePath = argv[0];
const arg = (name, dflt) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : dflt;
};
const has = (name) => argv.includes(`--${name}`);

if (!tapePath || !fs.existsSync(tapePath)) {
  console.error('usage: drive-tape.mjs <tape> --socket <s> --session <n>');
  process.exit(2);
}

// VHS types one character at a time; a whole line pasted at once reads as a
// glitch rather than someone using the tool. VHS's own default is 50ms.
const TYPING_SPEED_MS = Number(process.env.DEMO_TYPING_SPEED_MS || 50);

// VHS key name -> tmux send-keys key name. Only the ones the tapes use.
const KEYS = {
  Enter: 'Enter',
  Tab: 'Tab',
  Space: 'Space',
  Escape: 'Escape',
  Backspace: 'BSpace',
  Delete: 'DC',
  Up: 'Up',
  Down: 'Down',
  Left: 'Left',
  Right: 'Right',
  PageUp: 'PageUp',
  PageDown: 'PageDown',
};

function parse(src) {
  const outputs = [];
  const set = {};
  const steps = [];
  let hidden = false;

  src.split('\n').forEach((raw, i) => {
    const line = raw.trim();
    if (!line || line.startsWith('#')) return;
    const lineNo = i + 1;

    let m;
    if ((m = line.match(/^Output\s+(.+)$/))) {
      outputs.push(m[1].trim());
      return;
    }
    if ((m = line.match(/^Set\s+(\w+)\s+(.+)$/))) {
      set[m[1]] = m[2].trim().replace(/^"(.*)"$/, '$1');
      return;
    }
    // The Hide block launches the TUI off-camera. The recorder boots the TUI
    // itself (it has to: recording attaches to an already-running session), so
    // everything in here is already accounted for.
    if (line === 'Hide') {
      hidden = true;
      return;
    }
    if (line === 'Show') {
      hidden = false;
      return;
    }
    if (hidden) return;

    if ((m = line.match(/^Type\s+(?:`([^`]*)`|"((?:[^"\\]|\\.)*)")$/))) {
      const text = m[1] !== undefined ? m[1] : m[2].replace(/\\(.)/g, '$1');
      steps.push({ kind: 'type', text });
      return;
    }
    if ((m = line.match(/^Sleep\s+([\d.]+)(ms|s)?$/))) {
      const n = Number(m[1]);
      steps.push({ kind: 'sleep', ms: m[2] === 'ms' ? n : n * 1000 });
      return;
    }
    if ((m = line.match(/^Ctrl\+(\w)$/i))) {
      steps.push({ kind: 'key', key: `C-${m[1].toLowerCase()}` });
      return;
    }
    // `Enter 1` / `Down 3` — VHS's repeat-count form.
    if ((m = line.match(/^(\w+)(?:\s+(\d+))?$/)) && KEYS[m[1]]) {
      const n = m[2] ? Number(m[2]) : 1;
      for (let k = 0; k < n; k++) steps.push({ kind: 'key', key: KEYS[m[1]] });
      return;
    }
    throw new Error(`${tapePath}:${lineNo}: unsupported tape line: ${line}`);
  });
  return { outputs, set, steps };
}

const tape = parse(fs.readFileSync(tapePath, 'utf8'));

if (has('print-outputs')) {
  console.log(tape.outputs.join('\n'));
  process.exit(0);
}
if (has('print-set')) {
  const k = arg('print-set');
  if (tape.set[k] !== undefined) console.log(tape.set[k]);
  process.exit(0);
}

// Seconds this tape is scripted to take. record.sh checks the rendered clip
// against it: the recording is a real-time capture of real processes, so a
// clip that runs wildly long (a stall) or short (a truncated cast) is broken
// media that must not ship quietly.
if (has('print-duration')) {
  const ms = tape.steps.reduce(
    (t, s) =>
      t +
      (s.kind === 'sleep' ? s.ms : s.kind === 'type' ? s.text.length * TYPING_SPEED_MS : 0),
    0
  );
  console.log((ms / 1000).toFixed(2));
  process.exit(0);
}

const socket = arg('socket');
const session = arg('session');
if (!socket || !session) {
  console.error('drive-tape.mjs: --socket and --session are required');
  process.exit(2);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function tmux(args) {
  const r = spawnSync('tmux', ['-L', socket, ...args], { stdio: 'inherit' });
  if (r.status !== 0) throw new Error(`tmux ${args.join(' ')} failed`);
}
const sendKey = (key) => tmux(['send-keys', '-t', session, key]);
const sendLiteral = (text) => tmux(['send-keys', '-t', session, '-l', '--', text]);

for (const step of tape.steps) {
  if (step.kind === 'sleep') {
    await sleep(step.ms);
  } else if (step.kind === 'key') {
    sendKey(step.key);
  } else if (step.kind === 'type') {
    for (const ch of step.text) {
      sendLiteral(ch);
      await sleep(TYPING_SPEED_MS);
    }
  }
}
