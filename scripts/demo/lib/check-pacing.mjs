#!/usr/bin/env node
// Hold a rendered demo gif to a pacing budget.
//
// The demos are autoplaying, silent, scrubber-less and looping. Nobody chose to
// watch one, so the thing that loses a viewer is not length — it is a frame that
// sits there. Published retention work is blunt about the asymmetry: viewers
// abandon a video they are WAITING on roughly 3x faster than one they are
// watching, and abandonment is near zero for the first ~2s of playback and then
// climbs steadily. A held frame reads as "this gif is broken", and the opening
// frame is where that judgement gets made.
//
// So this measures stalls, not runtime. A clip may be as long as it earns.
//
// ---------------------------------------------------------------------------
// Method: a gif frame's own delay IS how long that frame is held.
//
// agg emits one frame per screen change and merges identical repaints into it
// (record.sh relies on the same property), so the delay stored in each Graphic
// Control Extension is exactly the dwell on that image. That makes the headline
// metric exact: no decoding, no pixel threshold to calibrate, no false positive
// on a clip that happens to be mostly text.
//
// The pixel-difference alternative (ffmpeg freezedetect) was tried and is NOT
// used to gate. On a lossless gif it reproduces this file's numbers closely, but
// it cannot separate a stall from typing: one typed glyph changes ~240 pixels of
// a 2.08Mpx frame, and a stall in which a clock digit ticked changes ~290 — the
// same magnitude. Only the RATE of change distinguishes them, which freezedetect
// does not model, so it reports multi-second "freezes" over passages that are
// visibly someone typing. Frame delay has no such blind spot: a frame boundary
// means the screen changed, full stop.
//
// The one exception is the OPENING hold, which is measured with freezedetect
// after all — see openingHold() for why frame delay cannot see it and why
// freezedetect's blind spot does not apply there.
//
// Usage: check-pacing.mjs <file.gif> [...] [--json]
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

// The budget. `pause` is the one that matters; the rest are guard rails.
const BUDGET = {
  // A beat the viewer spends reading. Anything past this is the demo waiting on
  // itself — which, after `Wait` landed in drive-tape.mjs, no beat needs to do.
  pauseTarget: 0.5,
  pauseMax: 1.0,
  // The first frame is the README preview and the whole of a scroller's
  // impression. It is not a place to settle; the recorder polls for the attach
  // repaint precisely so this can be short.
  //
  // 0.5s is the target, but the CAP is 0.75s and that gap is not slack — it is
  // measured. Even with nothing scripted, the opening carries the attach-settle
  // poll plus node's own startup before drive-tape.mjs can send its first key,
  // and asciinema is filming through all of it. Across takes that floor lands
  // between 0.35s and 0.57s, so gating on 0.5 would fail runs that are as fast
  // as the recorder can go.
  openingTarget: 0.5,
  opening: 0.75,
  // GitHub refuses to render an image over 10MB, and a gif that fails to paint
  // is the "waiting" case that costs the most.
  bytesWarn: 5 * 1024 * 1024,
  bytesMax: 10 * 1024 * 1024,
};

// Per-frame delays, in seconds, from the gif's Graphic Control Extensions.
// GCE layout: 21 F9 04 <flags> <delay-lo> <delay-hi> <transparent-idx> 00.
//
// The delays are read by walking the stream's real block boundaries rather than
// by scanning for that byte pattern: LZW image data is arbitrary bytes, so
// `21 F9 04` occurs inside a compressed sub-block often enough to matter at
// these file sizes, and a phantom GCE takes its delay from two unrelated bytes
// — almost always well over the cap, i.e. a "held frame" that never happened
// rejecting a good take.
function frameDelays(buf) {
  const delays = [];
  // A sub-block chain: a length byte, that many bytes of payload, until 0.
  const skipSubBlocks = (p) => {
    while (buf[p]) p += 1 + buf[p];
    return p + 1;
  };
  const colorTableBytes = (packed) => (packed & 0x80 ? 3 * 2 ** ((packed & 0x07) + 1) : 0);

  // Header, then the logical screen descriptor (packed flags at byte 10) and
  // the global colour table it may declare.
  let p = 6 + 7 + colorTableBytes(buf[10]);
  for (;;) {
    const block = buf[p++];
    if (block === 0x3b) break; // trailer
    if (block === 0x21) {
      const label = buf[p++];
      // p now sits on the block-size byte, so the delay is at +2 / +3.
      if (label === 0xf9) delays.push((buf[p + 2] | (buf[p + 3] << 8)) / 100);
      p = skipSubBlocks(p);
    } else if (block === 0x2c) {
      p += 8; // image position and size
      const packed = buf[p++];
      p += colorTableBytes(packed) + 1; // local colour table, LZW min code size
      p = skipSubBlocks(p);
    } else {
      // Truncated, or not a gif. Fail loudly rather than report the frames that
      // happened to parse before the confusion.
      throw new Error(`unexpected gif block 0x${(block ?? 0).toString(16)} at byte ${p - 1}`);
    }
  }
  return delays;
}

// How long the clip sits on its first image before anything moves.
//
// This is the one metric frame delay gets wrong. A static opening is not
// necessarily ONE gif frame: if a single character ticks inside it (a clock, a
// gauge), agg splits it into several, and each individual delay looks modest
// while the viewer sees 2.3s of nothing. Pixels are needed, and here they are
// safe to use — freezedetect's weakness is mistaking typing for a freeze, and
// nothing is being typed at t=0.
//
// Returns null when ffmpeg is not INSTALLED, so a quick local run still works;
// the recorder and CI both have it, and the caller says loudly that the metric
// went unmeasured.
//
// ffmpeg being absent and ffmpeg *failing* are deliberately not the same thing.
// A spawn error means the tool is missing — degrade and warn. A non-zero exit
// means ffmpeg is right there and could not read the clip: a truncated or
// corrupt gif, or a filter that no longer behaves as this code assumes. That is
// the very condition the gate exists to catch, so it must not quietly become a
// warning about a missing tool. Fail.
function openingHold(file) {
  const r = spawnSync(
    'ffmpeg',
    ['-hide_banner', '-i', file, '-vf', 'freezedetect=n=-60dB:d=0.2', '-map', '0:v', '-f', 'null', '-'],
    { encoding: 'utf8' }
  );
  // ENOENT is "not installed". Anything else (EACCES, a process limit) is a
  // problem with this machine rather than a reason to skip a budget.
  if (r.error) {
    if (r.error.code === 'ENOENT') return null;
    throw r.error;
  }
  if (r.status !== 0) {
    const tail = (r.stderr || '').trim().split('\n').slice(-3).join('\n  ');
    throw new Error(`${file}: ffmpeg could not analyse this clip (exit ${r.status})\n  ${tail}`);
  }
  const log = r.stderr || '';
  const start = /freeze_start: ([0-9.]+)/.exec(log);
  const dur = /freeze_duration: ([0-9.]+)/.exec(log);
  if (!start || !dur) return 0;
  // A first freeze that begins later than a frame or two in means the clip is
  // already moving at t=0 — there is no opening hold to report.
  if (Number(start[1]) > 0.3) return 0;
  return Number(start[1]) + Number(dur[1]);
}

function measure(file) {
  const buf = fs.readFileSync(file);
  const delays = frameDelays(buf);
  if (!delays.length) throw new Error(`${file}: no gif frames found — not a gif?`);

  const duration = delays.reduce((a, d) => a + d, 0);
  // Where each frame starts, so a violation can be pointed at.
  let t = 0;
  const frames = delays.map((d) => {
    const start = t;
    t += d;
    return { start, delay: d };
  });
  const worst = frames.reduce((a, f) => (f.delay > a.delay ? f : a), frames[0]);
  const overTarget = frames.filter((f) => f.delay > BUDGET.pauseTarget);

  return {
    file,
    bytes: buf.length,
    duration,
    frameCount: frames.length,
    // How much of the clip is new information rather than a held image.
    framesPerSecond: frames.length / duration,
    opening: openingHold(file),
    trailing: frames[frames.length - 1].delay,
    worstPause: worst.delay,
    worstPauseAt: worst.start,
    overTarget: overTarget.map((f) => ({ at: f.start, delay: f.delay })),
    // Time spent on frames held longer than a beat needs.
    stalledSeconds: overTarget.reduce((a, f) => a + f.delay, 0),
  };
}

function violations(m) {
  const out = [];
  if (m.worstPause > BUDGET.pauseMax) {
    out.push(
      `held frame of ${m.worstPause.toFixed(2)}s at ${m.worstPauseAt.toFixed(2)}s ` +
        `(max ${BUDGET.pauseMax}s)`
    );
  }
  if (m.opening !== null && m.opening > BUDGET.opening) {
    out.push(
      `opens on ${m.opening.toFixed(2)}s of held frame ` +
        `(target ${BUDGET.openingTarget}s, max ${BUDGET.opening}s)`
    );
  }
  if (m.bytes > BUDGET.bytesMax) {
    out.push(
      `${(m.bytes / 1024 / 1024).toFixed(1)}MB exceeds GitHub's 10MB image limit`
    );
  }
  return out;
}

const args = process.argv.slice(2);
const asJson = args.includes('--json');
const files = args.filter((a) => !a.startsWith('--'));
if (!files.length) {
  console.error('usage: check-pacing.mjs <file.gif> [...] [--json]');
  process.exit(2);
}

// An unreadable clip is a failure, not a crash report: this runs as a gate, and
// a stack trace buries the one line naming the file that could not be measured.
let results;
try {
  results = files.map(measure);
} catch (e) {
  console.error(`error: ${e.message}`);
  process.exit(1);
}

if (asJson) {
  console.log(JSON.stringify({ budget: BUDGET, results }, null, 2));
} else {
  const w = Math.max(...results.map((r) => path.basename(r.file).length));
  console.log(
    `${'clip'.padEnd(w)}  ${'dur'.padStart(6)} ${'fps'.padStart(5)} ` +
      `${'open'.padStart(5)} ${'worst'.padStart(6)} ${'at'.padStart(6)} ${'size'.padStart(6)}`
  );
  for (const r of results) {
    console.log(
      `${path.basename(r.file).padEnd(w)}  ${r.duration.toFixed(2).padStart(6)} ` +
        `${r.framesPerSecond.toFixed(1).padStart(5)} ` +
        `${(r.opening === null ? 'n/a' : r.opening.toFixed(2)).padStart(5)} ` +
        `${r.worstPause.toFixed(2).padStart(6)} ${r.worstPauseAt.toFixed(2).padStart(6)} ` +
        `${(r.bytes / 1024 / 1024).toFixed(1) + 'M'}`.padStart(7)
    );
  }
}

// Never let a missing tool quietly turn a gate into a no-op: an unchecked
// budget that prints nothing is indistinguishable from a passing one.
if (results.some((r) => r.opening === null)) {
  console.error(
    '\nwarning: ffmpeg unavailable — opening-hold NOT checked on any clip.\n' +
      '  The held-frame and size budgets still applied. Install ffmpeg to check\n' +
      '  the opening (it is the one metric a gif\'s frame delays cannot express:\n' +
      '  a static opening split by one ticking character looks like several\n' +
      '  short frames).'
  );
}

let failed = 0;
for (const r of results) {
  const bad = violations(r);
  if (bad.length) {
    failed++;
    console.error(`\nerror: ${r.file}`);
    for (const b of bad) console.error(`  ${b}`);
    // The tape line to go fix is the one whose Sleep matches this timestamp.
    if (r.overTarget.length > 1) {
      const rest = r.overTarget
        .filter((f) => f.delay <= BUDGET.pauseMax)
        .map((f) => `${f.delay.toFixed(2)}s@${f.at.toFixed(1)}s`);
      if (rest.length) console.error(`  also over ${BUDGET.pauseTarget}s: ${rest.join(', ')}`);
    }
  } else if (r.bytes > BUDGET.bytesWarn) {
    console.error(
      `warning: ${r.file} is ${(r.bytes / 1024 / 1024).toFixed(1)}MB ` +
        `(GitHub's limit is 10MB)`
    );
  }
}
process.exit(failed ? 1 : 0);
