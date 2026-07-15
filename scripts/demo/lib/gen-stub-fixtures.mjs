#!/usr/bin/env node
// Turn the demo's scripted conversations (scripts/demo/demo-content.json) into
// the semantic fixtures the model stubs replay (scripts/dev/agent-e2e/stub/).
// record.sh pre-plays each session's user prompts through `friring-cli session
// send`; the stub answers them from these fixtures, so every agent pane shows
// its scripted (fictional-future) exchange with no real model or account.
//
// One stub per wire dialect, shared across agents that speak it:
//   anthropic-fixtures.json  <- claude sessions
//   openai-fixtures.json     <- codex (Responses) + opencode (Chat) sessions
//
// Matching is by prompt substring (stub-core.mjs). Each user prompt in
// demo-content.json is authored with a unique >=12-char substring, so the
// whole prompt is a safe, unambiguous match key. opencode also fires one
// ambient title-generation call per session (same model, "title generator"
// system prompt); it is emitted FIRST and keyed on systemContains so it can't
// be shadowed by a primary fixture whose text it echoes.
//
// Zero npm dependencies (node stdlib only), like the stubs themselves.
import fs from 'node:fs';

const [, , contentPath, outDir] = process.argv;
if (!contentPath || !outDir) {
  console.error('usage: gen-stub-fixtures.mjs <demo-content.json> <out-dir>');
  process.exit(2);
}
const content = JSON.parse(fs.readFileSync(contentPath, 'utf8'));
fs.mkdirSync(outDir, { recursive: true });

// Which wire dialect each agent speaks. antigravity is deliberately absent:
// agy forces real Google OAuth and can't be stubbed offline (see
// scripts/dev/agent-e2e/agents/antigravity/profile.sh), so it is demoed
// logged-out and has no scripted conversation to replay.
const DIALECT = { claude: 'anthropic', codex: 'openai', opencode: 'openai' };

// A short, wrap-safe substring of the reply's LAST line — record.sh polls the
// pane for it to know a pre-played turn has finished rendering.
//
// The last line, not the first: the agent TUIs draw on the alternate screen,
// which has no scrollback, and pre-play runs before any friring TUI attaches,
// so the panes are still at tmux's default 24 rows. A reply longer than the
// pane scrolls its opening line away unrecoverably, while its tail is always
// on screen — and the tail only appears once the whole reply has rendered,
// which is exactly the condition we're waiting for.
//
// Taken as a line PREFIX and kept short, so a soft-wrap (which breaks the end
// of a line, never its start) can't split it.
function marker(text) {
  const lines = text.split('\n').filter((l) => l.trim());
  const lastLine = lines[lines.length - 1] || text;
  const slice = lastLine.slice(0, 24);
  // Trim a trailing partial word so the marker is a clean prefix.
  return slice.replace(/\s+\S*$/, '') || slice;
}

const anthropic = [];
const openai = [];
const preplay = [];
let sawOpencode = false;

for (const session of content.sessions || []) {
  const dialect = DIALECT[session.agent];
  if (!dialect) continue; // unstubbable agent (antigravity) — no fixtures
  const bucket = dialect === 'anthropic' ? anthropic : openai;
  if (session.agent === 'opencode') sawOpencode = true;
  const turns = session.turns || [];
  turns.forEach((turn, i) => {
    bucket.push({
      name: `${session.session_name}-${i + 1}`,
      match: { promptContains: turn.user },
      reply: { text: turn.assistant },
    });
  });
  if (turns.length)
    preplay.push({
      session: session.session_name,
      agent: session.agent,
      turns: turns.map((t) => ({ prompt: t.user, marker: marker(t.assistant) })),
    });
}

// opencode's per-session title-gen call. Its reply becomes the visible session
// title, so keep it short and on-theme. Ambient + systemContains-keyed +
// listed first (see module header).
const openaiOut = [];
if (sawOpencode) {
  openaiOut.push({
    name: 'opencode-title',
    ambient: true,
    match: { systemContains: 'title generator' },
    reply: { text: 'ocean-current LB migration' },
  });
}
openaiOut.push(...openai);

fs.writeFileSync(
  `${outDir}/anthropic-fixtures.json`,
  JSON.stringify({ responses: anthropic }, null, 2) + '\n'
);
fs.writeFileSync(
  `${outDir}/openai-fixtures.json`,
  JSON.stringify({ responses: openaiOut }, null, 2) + '\n'
);
fs.writeFileSync(`${outDir}/preplay.json`, JSON.stringify(preplay, null, 2) + '\n');

console.log(
  `wrote ${anthropic.length} anthropic + ${openaiOut.length} openai fixture(s), ` +
    `${preplay.length} pre-play session(s) to ${outDir}`
);
