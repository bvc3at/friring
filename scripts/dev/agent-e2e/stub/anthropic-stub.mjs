#!/usr/bin/env node
// Local stub of the Anthropic Messages API for Friring's real-agent e2e
// harness (scripts/dev/agent-e2e/). A real Claude Code binary is pointed here
// via ANTHROPIC_BASE_URL; the stub answers from hand-curated *semantic*
// fixtures instead of recorded cassettes, because tool-use loops make raw
// record/replay brittle: request bodies grow cumulatively and embed
// machine-specific tool results (see docs/E2E.md).
//
// The CLI, fixture schema, matcher and journal are shared with every other
// dialect via stub-core.mjs; only the wire encoding lives here. Zero npm
// dependencies on purpose (node's http only), so the Rust dependency graph
// (cargo-deny) is untouched and the same sidecar serves both the bats suite
// and VHS demo recordings.
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
//         "systemContains": "…",             // substring of the system prompt
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
//         "flood": {"line": "…", "count": 500}, // prepend line×count to text
//                                             // (perf scenarios: large output
//                                             // without megabytes of JSON)
//         "toolUse": {"id": "toolu_e2e_1", "name": "Write", "input": {…}},
//         "stopReason": "end_turn"            // default; tool_use if toolUse
//       }
//     }]
//   }
//
// First matching response wins (top to bottom). List `ambient` fixtures FIRST:
// a background side-model call (e.g. haiku title-gen) is model-keyed, so an
// ambient-first order catches it before a primary fixture whose prompt text it
// happens to echo can shadow it. A request no fixture matches fails OPEN here —
// a benign 200 marker reply keeps the agent alive and debuggable — and fails
// CLOSED at assert time: the UNMATCHED journal entry makes the harness's
// post-run invariant fail the scenario. (There is deliberately no catch-all
// default: it would answer surprise calls 200 as `matched:"default"`, silently
// disabling the strictness the UNMATCHED marker exists to enforce.)
import { createStub, serveStub, effectiveText, chunkText, sleep } from './stub-core.mjs';

const stub = createStub('anthropic-stub');

// Fixed usage numbers: assertions and demo captures must not vary run-to-run.
const USAGE_START = { input_tokens: 100, output_tokens: 1 };
const USAGE_DELTA = { output_tokens: 50 };

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
    systemText: textOfContent(body.system),
    hasToolResult: toolResults.length > 0,
    toolResultIds: toolResults.map((b) => b.tool_use_id),
    nTools: Array.isArray(body.tools) ? body.tools.length : 0,
  };
}

function buildContentBlocks(reply) {
  const blocks = [];
  const text = effectiveText(reply);
  if (text) blocks.push({ type: 'text', text });
  if (reply.toolUse)
    blocks.push({
      type: 'tool_use',
      id: reply.toolUse.id,
      name: reply.toolUse.name,
      input: reply.toolUse.input || {},
    });
  return blocks;
}

function sseWrite(res, event, data) {
  res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

async function respondStream(res, model, picked) {
  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  });
  const { reply, delayMs } = picked;
  const blocks = buildContentBlocks(reply);
  const stopReason = reply.toolUse ? 'tool_use' : reply.stopReason || 'end_turn';
  const msgId = stub.nextMessageId('msg_e2e_');
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
      for (const piece of chunkText(block.text)) {
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
      id: stub.nextMessageId('msg_e2e_'),
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

serveStub('anthropic-stub', stub, async ({ req, res, raw, url, base }) => {
  if (url.startsWith('/v1/messages/count_tokens')) {
    stub.journal({ ...base, kind: 'count_tokens' });
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ input_tokens: 100 }));
    return true;
  }
  if (!(req.method === 'POST' && url.startsWith('/v1/messages'))) return false;

  let body;
  try {
    body = JSON.parse(raw);
  } catch (e) {
    stub.journal({ ...base, kind: 'bad_json', error: String(e) });
    res.writeHead(400, { 'content-type': 'application/json' });
    res.end(
      JSON.stringify({
        type: 'error',
        error: { type: 'invalid_request_error', message: 'stub: unparseable body' },
      })
    );
    return true;
  }
  const s = summarize(body);
  const picked = stub.pick(s);
  stub.journal({
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
  // Fail-open at response time, fail-closed at assert time: an unmatched call
  // gets a benign marker reply so the agent stays alive (a 400 here would stall
  // the pane before the first wait and leave nothing to debug), while the
  // UNMATCHED journal entry makes the post-run invariant fail the scenario.
  const effective = picked || stub.unmatchedReply(base.seq);
  if (s.stream) await respondStream(res, body.model, effective);
  else respondJson(res, body.model, effective);
  return true;
});
