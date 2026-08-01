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
//   Wait /<re>/ [<timeout>]          until the pane matches — waiting on the APP
//   Wait Stable [<quiet>] [<timeout>]  until the pane stops changing
//   Enter/Tab/Space/…      a single key
//   Ctrl+<X>               a chord
//
// `Sleep` and `Wait` look interchangeable and are not. Sleep is viewer pacing:
// time deliberately spent on a frame the viewer is meant to read. Wait is the
// app being slow: a session booting, an agent painting its first screen. Only
// Sleep belongs in a demo's timing budget — a Sleep long enough to cover a
// boot on a slow machine films as a frozen screen on a fast one, which is how
// the media accumulated multi-second dead holds that no beat asked for. Wait
// ends the instant the pane proves the app is ready, so the clip carries the
// real latency and nothing more.
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

const WAIT_POLL_MS = 50;
// Default settle window for `Wait Stable` — the DEFAULT, not the answer.
//
// "The screen stopped changing" is a proxy for "the app is ready", and the two
// come apart on any beat that happens in stages: repaint, pause, repaint again.
// If the pause between stages outlasts this window the wait resolves on the
// intermediate screen. No value is safe for every beat — a modal closing settles
// in well under 250ms, while the gap between a session appearing and its agent
// CLI writing its first byte has measured 3.5s — which is why the window is a
// per-beat argument (`Wait Stable 800ms`) and why a marker (`Wait /<re>/`) is
// the better tool whenever the beat has one. 250ms suits the common case: a
// modal or picker that repaints once and is done.
const WAIT_STABLE_QUIET_MS = 250;
const WAIT_TIMEOUT_MS = 10_000;

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
    // `Wait Stable [<quiet>] [<timeout>]` — the FIRST number is the settle
    // window, not the deadline, because that is the knob a beat actually needs
    // tuning (see waitForStable). `Wait /re/ [<timeout>]` takes a deadline,
    // because a marker is exact and the only question is how long to allow.
    const dur = (v, u) => Number(v) * (u === 'ms' ? 1 : 1000);
    if ((m = line.match(/^Wait\s+Stable(?:\s+([\d.]+)(ms|s)?)?(?:\s+([\d.]+)(ms|s)?)?$/))) {
      steps.push({
        kind: 'stable',
        quietMs: m[1] === undefined ? WAIT_STABLE_QUIET_MS : dur(m[1], m[2]),
        timeoutMs: m[3] === undefined ? WAIT_TIMEOUT_MS : dur(m[3], m[4]),
        lineNo,
      });
      return;
    }
    if ((m = line.match(/^Wait\s+\/((?:[^/\\]|\\.)+)\/(?:\s+([\d.]+)(ms|s)?)?$/))) {
      steps.push({
        kind: 'match',
        re: m[1],
        timeoutMs: m[2] === undefined ? WAIT_TIMEOUT_MS : dur(m[2], m[3]),
        lineNo,
      });
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
//
// `Wait` counts as zero. Its duration is whatever the app takes, which is the
// very thing this number cannot predict — folding in a guess would just move
// the guess from the tape to here. The recorder's slack absorbs the real wait,
// and check-pacing.mjs is what actually polices the rendered result.
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

// The pane as text. Styling is not captured, so a blinking cursor or a repaint
// of identical content reads as "unchanged" — which is exactly the notion of
// stability `Wait Stable` wants. A failed capture, on the other hand, must not
// be swallowed: tmux prints nothing on stdout when the pane is gone, and an
// empty string that never changes again is precisely what `Wait Stable` calls
// settled — a dead session would satisfy the wait instead of failing the take.
function capturePane() {
  const r = spawnSync('tmux', ['-L', socket, 'capture-pane', '-p', '-t', session], {
    encoding: 'utf8',
  });
  if (r.error || r.status !== 0) throw new Error(`tmux capture-pane -t ${session} failed`);
  return r.stdout ?? '';
}

// A wait that never resolves means the beat it was guarding never happened, so
// every later keystroke lands somewhere unintended and the clip films the wrong
// thing. Fail closed, like every other recorder check.
function waitTimedOut(step, what) {
  throw new Error(
    `${tapePath}:${step.lineNo}: Wait ${what} did not resolve within ${step.timeoutMs}ms`
  );
}

async function waitForMatch(step) {
  const re = new RegExp(step.re);
  for (let waited = 0; waited <= step.timeoutMs; waited += WAIT_POLL_MS) {
    if (re.test(capturePane())) return waited;
    await sleep(WAIT_POLL_MS);
  }
  return waitTimedOut(step, `/${step.re}/`);
}

// Two phases, and the first one is the point: wait for the screen to START
// responding, then for it to stop. Quiet alone is not readiness — the gap
// between a keystroke being sent and the app reacting is itself quiet, so a
// single-phase version resolves inside that gap and calls the OLD screen
// settled. That is not a theoretical failure: it filmed a file-open beat before
// the file had painted, and it would have let the agent-boot beats film an
// empty pane.
//
// `baseline` is the pane captured BEFORE the beat this wait guards, because
// send-keys and capture-pane are two separate process spawns: a repaint that
// completes inside that window is already on screen by the time a self-captured
// baseline is taken, so the change phase would see nothing and reject a take
// that was in fact correct — just fast. Falls back to capturing here so a tape
// may still open on `Wait Stable`.
async function waitForStable(step, baseline) {
  const before = baseline ?? capturePane();
  let waited = 0;
  let changed = false;
  while (waited < step.timeoutMs) {
    await sleep(WAIT_POLL_MS);
    waited += WAIT_POLL_MS;
    if (capturePane() !== before) {
      changed = true;
      break;
    }
  }
  // Deliberately NOT lenient. An earlier version gave up after a grace period
  // and carried on, which quietly filmed the grace as a held frame and let the
  // following keys land on a screen that was not ready — the exact failure this
  // directive exists to prevent, reintroduced as its fallback. If the beat
  // changes nothing the tape is wrong (or the change is colour-only, which
  // capture-pane cannot see: use a plain Sleep for those). Say so.
  if (!changed) return waitTimedOut(step, 'Stable (nothing on screen changed)');
  let last = capturePane();
  let quiet = 0;
  while (waited < step.timeoutMs) {
    await sleep(WAIT_POLL_MS);
    waited += WAIT_POLL_MS;
    const now = capturePane();
    quiet = now === last ? quiet + WAIT_POLL_MS : 0;
    last = now;
    if (quiet >= step.quietMs) return waited;
  }
  return waitTimedOut(step, 'Stable');
}

// What each Wait actually cost. This is the number that tells you whether a
// beat is slow because the demo asked it to be or because the app is.
const waits = [];
// Waits that were still going when the tape moved on — see the audit below.
const early = [];

let stableBaseline = null;
// Set when a Wait has just resolved: the screen it settled on, so the NEXT
// sleep can check whether it stayed settled.
let audit = null;

for (let i = 0; i < tape.steps.length; i++) {
  const step = tape.steps[i];
  // See waitForStable: the "before" screen has to be read before the beat runs.
  if (tape.steps[i + 1]?.kind === 'stable') stableBaseline = capturePane();
  if (step.kind === 'sleep') {
    await sleep(step.ms);
    // Audit the preceding Wait, for free. No key was sent during that sleep, so
    // anything that moved on screen was the app still working — which means the
    // Wait resolved on an intermediate screen, not on readiness. Quiet is only
    // ever a proxy: a beat that repaints, pauses longer than the settle window,
    // then repaints again (a modal closing, then an agent CLI painting) will
    // satisfy it early, and the clip films the half-done state. This cannot
    // prove a Wait was right, but it catches the case that matters at zero cost
    // in runtime. A warning, not an error: on the multi-session view the live
    // agent panes repaint on their own, so movement here is not always a defect.
    if (audit) {
      if (capturePane() !== audit.pane) early.push(audit);
      audit = null;
    }
  } else if (step.kind === 'key') {
    audit = null;
    sendKey(step.key);
  } else if (step.kind === 'type') {
    audit = null;
    for (const ch of step.text) {
      sendLiteral(ch);
      await sleep(TYPING_SPEED_MS);
    }
  } else if (step.kind === 'match') {
    waits.push([step.lineNo, `/${step.re}/`, await waitForMatch(step)]);
    audit = { lineNo: step.lineNo, what: `/${step.re}/`, pane: capturePane() };
  } else if (step.kind === 'stable') {
    const base = stableBaseline;
    stableBaseline = null;
    waits.push([step.lineNo, 'Stable', await waitForStable(step, base)]);
    audit = { lineNo: step.lineNo, what: `Stable ${step.quietMs}ms`, pane: capturePane() };
  }
}

if (waits.length) {
  const total = waits.reduce((t, [, , ms]) => t + ms, 0);
  console.error(
    `${tapePath}: ${waits.length} Wait(s), ${(total / 1000).toFixed(2)}s total — ` +
      waits.map(([ln, what, ms]) => `L${ln} ${what} ${ms}ms`).join(', ')
  );
}
for (const e of early) {
  console.error(
    `${tapePath}:${e.lineNo}: note: the screen was still changing after ` +
      `\`Wait ${e.what}\` resolved, so the next beat filmed a screen the app had ` +
      `not finished with. Intended when the tape deliberately stops short of a ` +
      `later stage (not filming an agent boot, say); otherwise widen the settle ` +
      `window (\`Wait Stable <quiet>\`) or wait on a marker (\`Wait /<re>/\`).`
  );
}
