#!/usr/bin/env node
// Local stub of the Anthropic Messages API for Friring's real-agent e2e
// harness (scripts/dev/agent-e2e/). A real Claude Code binary is pointed here
// via ANTHROPIC_BASE_URL; the stub answers from hand-curated *semantic*
// fixtures instead of recorded cassettes, because tool-use loops make raw
// record/replay brittle: request bodies grow cumulatively and embed
// machine-specific tool results (see docs/E2E.md).
//
// Zero npm dependencies on purpose — node's http module only — so the Rust
// dependency graph (cargo-deny) is untouched and the same sidecar serves both
// the bats suite and VHS demo recordings.
//
// Usage:
//   node anthropic-stub.mjs --fixtures f.json --journal j.jsonl \
//     [--port 0] [--port-file p] [--raw-dir d]
//
// Fixture file shape (all match fields optional; omitted = matches anything):
//   {
//     "responses": [{
//       "name": "write-tool-call",           // journal key; assertions grep it
//       "match": {
//         "modelContains": "opus",           // substring of body.model
//         "promptContains": "hello.txt",     // substring of the last user msg
//         "anyUserContains": "…",            // substring of any user msg
//         "hasToolResult": false,            // last user msg carries tool_result
//         "toolResultFor": "toolu_e2e_1"     // tool_result for this pinned id
//       },
//       "ambient": false,   // true = background traffic (e.g. haiku side
//                           // calls); journaled but excluded from the
//                           // harness's "every fixture consumed" assertion
//       "maxUses": 0,       // >0 = stop matching after N uses (loop guard)
//       "delayMs": 0,       // pause between SSE deltas (demo pacing)
//       "reply": {
//         "text": "…",                        // text block
//         "toolUse": {"id": "toolu_e2e_1", "name": "Write", "input": {…}},
//         "stopReason": "end_turn"            // default; tool_use if toolUse
//       }
//     }],
//     "default": {"reply": {"text": "…"}}     // optional catch-all
//   }
//
// First matching response wins (top to bottom). A request no fixture matches
// gets a 400 and an UNMATCHED journal entry — strict offline mode: the suite
// fails on surprise calls rather than improvising an answer.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const argv = process.argv.slice(2);
function arg(name, dflt) {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : dflt;
}
const PORT = Number(arg('port', '0'));
const PORT_FILE = arg('port-file', '');
const JOURNAL = arg('journal', '');
const RAW_DIR = arg('raw-dir', '');
const FIXTURES = arg('fixtures', '');

if (!JOURNAL || !FIXTURES) {
  console.error('anthropic-stub: --fixtures and --journal are required');
  process.exit(2);
}
if (RAW_DIR) fs.mkdirSync(RAW_DIR, { recursive: true });
const fixtures = JSON.parse(fs.readFileSync(FIXTURES, 'utf8'));
const useCounts = new Map();

let requestSeq = 0;
let messageSeq = 0;
function journal(entry) {
  fs.appendFileSync(JOURNAL, JSON.stringify(entry) + '\n');
}

function textOfContent(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content))
    return content
      .filter((b) => b && b.type === 'text')
      .map((b) => b.text)
      .join('\n');
  return '';
}

// Claude Code sometimes appends trailing system-role entries, so "the user
// turn" is the last message with role=user, not messages[last].
function summarize(body) {
  const msgs = Array.isArray(body.messages) ? body.messages : [];
  const lastUser = [...msgs].reverse().find((m) => m.role === 'user') || {};
  const lastUserContent = Array.isArray(lastUser.content) ? lastUser.content : [];
  const toolResults = lastUserContent.filter((b) => b && b.type === 'tool_result');
  return {
    model: body.model || '',
    stream: !!body.stream,
    nMessages: msgs.length,
    lastUserText: textOfContent(lastUser.content),
    allUserText: msgs
      .filter((m) => m.role === 'user')
      .map((m) => textOfContent(m.content))
      .join('\n'),
    hasToolResult: toolResults.length > 0,
    toolResultIds: toolResults.map((b) => b.tool_use_id),
    nTools: Array.isArray(body.tools) ? body.tools.length : 0,
  };
}

function matches(m, s) {
  if (!m) return true;
  if (m.modelContains && !s.model.includes(m.modelContains)) return false;
  if (m.promptContains && !s.lastUserText.includes(m.promptContains)) return false;
  if (m.anyUserContains && !s.allUserText.includes(m.anyUserContains)) return false;
  if (typeof m.hasToolResult === 'boolean' && s.hasToolResult !== m.hasToolResult)
    return false;
  if (m.toolResultFor && !s.toolResultIds.includes(m.toolResultFor)) return false;
  return true;
}

function pick(s) {
  for (const r of fixtures.responses || []) {
    const name = r.name || 'unnamed';
    if (r.maxUses > 0 && (useCounts.get(name) || 0) >= r.maxUses) continue;
    if (matches(r.match, s)) {
      useCounts.set(name, (useCounts.get(name) || 0) + 1);
      return { name, ambient: !!r.ambient, delayMs: r.delayMs || 0, reply: r.reply };
    }
  }
  if (fixtures.default) {
    useCounts.set('default', (useCounts.get('default') || 0) + 1);
    return { name: 'default', ambient: false, delayMs: 0, reply: fixtures.default.reply };
  }
  return null;
}

function buildContentBlocks(reply) {
  const blocks = [];
  if (reply.text) blocks.push({ type: 'text', text: reply.text });
  if (reply.toolUse)
    blocks.push({
      type: 'tool_use',
      id: reply.toolUse.id,
      name: reply.toolUse.name,
      input: reply.toolUse.input || {},
    });
  return blocks;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function sseWrite(res, event, data) {
  res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

// Fixed usage numbers: assertions and demo captures must not vary run-to-run.
const USAGE_START = { input_tokens: 100, output_tokens: 1 };
const USAGE_DELTA = { output_tokens: 50 };

async function respondStream(res, model, picked) {
  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  });
  const { reply, delayMs } = picked;
  const blocks = buildContentBlocks(reply);
  const stopReason = reply.toolUse ? 'tool_use' : reply.stopReason || 'end_turn';
  const msgId = `msg_e2e_${String(++messageSeq).padStart(4, '0')}`;
  sseWrite(res, 'message_start', {
    type: 'message_start',
    message: {
      id: msgId,
      type: 'message',
      role: 'assistant',
      model,
      content: [],
      stop_reason: null,
      stop_sequence: null,
      usage: USAGE_START,
    },
  });
  for (let index = 0; index < blocks.length; index++) {
    const block = blocks[index];
    if (block.type === 'text') {
      sseWrite(res, 'content_block_start', {
        type: 'content_block_start',
        index,
        content_block: { type: 'text', text: '' },
      });
      // Chunked like the real API so the TUI exercises streaming render; word
      // granularity gives demos a natural typing cadence under delayMs.
      for (const piece of block.text.split(/(?<= )/)) {
        sseWrite(res, 'content_block_delta', {
          type: 'content_block_delta',
          index,
          delta: { type: 'text_delta', text: piece },
        });
        if (delayMs) await sleep(delayMs);
      }
    } else {
      sseWrite(res, 'content_block_start', {
        type: 'content_block_start',
        index,
        content_block: { type: 'tool_use', id: block.id, name: block.name, input: {} },
      });
      sseWrite(res, 'content_block_delta', {
        type: 'content_block_delta',
        index,
        delta: { type: 'input_json_delta', partial_json: JSON.stringify(block.input) },
      });
    }
    sseWrite(res, 'content_block_stop', { type: 'content_block_stop', index });
  }
  sseWrite(res, 'message_delta', {
    type: 'message_delta',
    delta: { stop_reason: stopReason, stop_sequence: null },
    usage: USAGE_DELTA,
  });
  sseWrite(res, 'message_stop', { type: 'message_stop' });
  res.end();
}

function respondJson(res, model, picked) {
  const { reply } = picked;
  const blocks = buildContentBlocks(reply);
  const stopReason = reply.toolUse ? 'tool_use' : reply.stopReason || 'end_turn';
  res.writeHead(200, { 'content-type': 'application/json' });
  res.end(
    JSON.stringify({
      id: `msg_e2e_${String(++messageSeq).padStart(4, '0')}`,
      type: 'message',
      role: 'assistant',
      model,
      content: blocks,
      stop_reason: stopReason,
      stop_sequence: null,
      usage: { input_tokens: 100, output_tokens: 50 },
    })
  );
}

const server = http.createServer((req, res) => {
  const seq = ++requestSeq;
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', async () => {
    const raw = Buffer.concat(chunks).toString('utf8');
    if (RAW_DIR && raw)
      fs.writeFileSync(path.join(RAW_DIR, `${String(seq).padStart(3, '0')}.json`), raw);
    const url = req.url || '';
    const base = { seq, ts: new Date().toISOString(), method: req.method, url };

    if (req.method === 'GET' && url.startsWith('/health')) {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ ok: true, requests: requestSeq }));
      return;
    }
    if (url.startsWith('/v1/messages/count_tokens')) {
      journal({ ...base, kind: 'count_tokens' });
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ input_tokens: 100 }));
      return;
    }
    if (req.method === 'POST' && url.startsWith('/v1/messages')) {
      let body;
      try {
        body = JSON.parse(raw);
      } catch (e) {
        journal({ ...base, kind: 'bad_json', error: String(e) });
        res.writeHead(400, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ type: 'error', error: { type: 'invalid_request_error', message: 'stub: unparseable body' } }));
        return;
      }
      const s = summarize(body);
      const picked = pick(s);
      journal({
        ...base,
        kind: 'messages',
        model: s.model,
        stream: s.stream,
        nMessages: s.nMessages,
        nTools: s.nTools,
        hasToolResult: s.hasToolResult,
        toolResultIds: s.toolResultIds,
        lastUserText: s.lastUserText.slice(0, 300),
        matched: picked ? picked.name : 'UNMATCHED',
        ambient: picked ? picked.ambient : false,
      });
      // Fail-open at response time, fail-closed at assert time: an unmatched
      // call gets a benign marker reply so the agent stays alive (a 400 here
      // would stall the pane before the first wait and leave nothing to
      // debug), while the UNMATCHED journal entry makes the post-run
      // invariant fail the scenario.
      const effective =
        picked ||
        {
          name: 'UNMATCHED',
          ambient: false,
          delayMs: 0,
          reply: { text: `[stub: unmatched request #${seq}]` },
        };
      if (s.stream) await respondStream(res, body.model, effective);
      else respondJson(res, body.model, effective);
      return;
    }
    // Anything else (HEAD / connectivity probe, unknown endpoints): journaled
    // so conformance drift in a new pinned binary shows up in artifacts, but
    // answered 200 so a harmless probe can't fail a scenario.
    journal({ ...base, kind: 'other', bodyPreview: raw.slice(0, 200) });
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end('{}');
  });
});

server.listen(PORT, '127.0.0.1', () => {
  const port = server.address().port;
  if (PORT_FILE) fs.writeFileSync(PORT_FILE, String(port));
  console.log(`anthropic-stub listening on 127.0.0.1:${port}`);
});

for (const sig of ['SIGTERM', 'SIGINT']) {
  process.on(sig, () => {
    server.close();
    process.exit(0);
  });
}
