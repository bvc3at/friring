#!/usr/bin/env node
// Local stub of the OpenAI-compatible wire APIs for Friring's real-agent e2e
// harness. One "openai" dialect covers the whole OpenAI-compatible family —
// both endpoints are served, because the CLIs split across them:
//
//   POST {base}/responses          codex >= 0.144 (wire_api = "responses";
//                                  the chat wire is REMOVED there — it hard-
//                                  errors at startup)
//   POST {base}/chat/completions   opencode (@ai-sdk/openai-compatible)
//
// Same CLI surface, fixture schema and journal shape as anthropic-stub.mjs
// (shared via stub-core.mjs); only the wire encoding differs. Replies are
// TEXT-ONLY on this dialect: `reply.toolUse` is an anthropic-stub feature —
// the tool-use loop is conformance-tested against Claude Code, while this
// dialect exists to render stubbed conversations (demos, text turns).
//
// Conformance notes (probed against codex-cli 0.144.4):
// - codex sends the full transcript in `input`; the LAST input item with
//   role "user" is the prompt (an earlier user item carries
//   <environment_context> — never match on position, only on role order).
// - The minimal accepted SSE sequence is response.created →
//   response.output_item.added → response.output_text.delta* →
//   response.output_item.done → response.completed (usage drives the
//   "tokens used" line, so it is fixed for run-to-run stability).
import { createStub, serveStub, effectiveText, chunkText, sleep } from './stub-core.mjs';

const stub = createStub('openai-stub');

// Fixed usage numbers: assertions and demo captures must not vary run-to-run.
const USAGE = { input_tokens: 100, output_tokens: 50, total_tokens: 150 };

function textOfContent(content) {
  if (typeof content === 'string') return content;
  if (Array.isArray(content))
    return content
      .filter((b) => b && typeof b.text === 'string')
      .map((b) => b.text)
      .join('\n');
  return '';
}

// Responses API: body.input is a flat item list (messages + tool calls/outputs);
// the system text lives in `instructions` plus any developer-role items.
function summarizeResponses(body) {
  const items = Array.isArray(body.input) ? body.input : [];
  const userTexts = items
    .filter((i) => i && i.role === 'user')
    .map((i) => textOfContent(i.content));
  const toolOutputs = items.filter((i) => i && i.type === 'function_call_output');
  return {
    model: body.model || '',
    stream: body.stream !== false,
    nMessages: items.length,
    lastUserText: userTexts[userTexts.length - 1] || '',
    allUserText: userTexts.join('\n'),
    systemText: [
      typeof body.instructions === 'string' ? body.instructions : '',
      ...items.filter((i) => i && i.role === 'developer').map((i) => textOfContent(i.content)),
    ].join('\n'),
    hasToolResult: toolOutputs.length > 0,
    toolResultIds: toolOutputs.map((i) => i.call_id).filter(Boolean),
    nTools: Array.isArray(body.tools) ? body.tools.length : 0,
  };
}

// Chat Completions: body.messages, role "tool" carries tool results.
function summarizeChat(body) {
  const msgs = Array.isArray(body.messages) ? body.messages : [];
  const userTexts = msgs
    .filter((m) => m && m.role === 'user')
    .map((m) => textOfContent(m.content));
  const toolMsgs = msgs.filter((m) => m && m.role === 'tool');
  return {
    model: body.model || '',
    stream: body.stream !== false,
    nMessages: msgs.length,
    lastUserText: userTexts[userTexts.length - 1] || '',
    allUserText: userTexts.join('\n'),
    systemText: msgs
      .filter((m) => m && (m.role === 'system' || m.role === 'developer'))
      .map((m) => textOfContent(m.content))
      .join('\n'),
    hasToolResult: msgs.length > 0 && msgs[msgs.length - 1].role === 'tool',
    toolResultIds: toolMsgs.map((m) => m.tool_call_id).filter(Boolean),
    nTools: Array.isArray(body.tools) ? body.tools.length : 0,
  };
}

function sseWrite(res, event, data) {
  // The Responses dialect names its events; chat chunks are plain data lines.
  if (event) res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
  else res.write(`data: ${JSON.stringify(data)}\n\n`);
}

async function respondResponses(res, model, picked) {
  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  });
  const { reply, delayMs } = picked;
  const text = effectiveText(reply);
  const respId = stub.nextMessageId('resp_e2e_');
  const itemId = stub.nextMessageId('msg_e2e_');
  sseWrite(res, 'response.created', {
    type: 'response.created',
    response: { id: respId, status: 'in_progress' },
  });
  sseWrite(res, 'response.output_item.added', {
    type: 'response.output_item.added',
    output_index: 0,
    item: { type: 'message', id: itemId, status: 'in_progress', role: 'assistant', content: [] },
  });
  for (const piece of chunkText(text)) {
    sseWrite(res, 'response.output_text.delta', {
      type: 'response.output_text.delta',
      item_id: itemId,
      output_index: 0,
      content_index: 0,
      delta: piece,
    });
    if (delayMs) await sleep(delayMs);
  }
  const doneItem = {
    type: 'message',
    id: itemId,
    status: 'completed',
    role: 'assistant',
    content: [{ type: 'output_text', text, annotations: [] }],
  };
  sseWrite(res, 'response.output_item.done', {
    type: 'response.output_item.done',
    output_index: 0,
    item: doneItem,
  });
  sseWrite(res, 'response.completed', {
    type: 'response.completed',
    response: {
      id: respId,
      status: 'completed',
      output: [doneItem],
      usage: {
        input_tokens: USAGE.input_tokens,
        input_tokens_details: { cached_tokens: 0 },
        output_tokens: USAGE.output_tokens,
        output_tokens_details: { reasoning_tokens: 0 },
        total_tokens: USAGE.total_tokens,
      },
    },
  });
  res.end();
}

async function respondChat(res, model, picked, stream) {
  const { reply, delayMs } = picked;
  const text = effectiveText(reply);
  const id = stub.nextMessageId('chatcmpl_e2e_');
  if (!stream) {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(
      JSON.stringify({
        id,
        object: 'chat.completion',
        created: 0,
        model,
        choices: [
          {
            index: 0,
            message: { role: 'assistant', content: text },
            finish_reason: 'stop',
          },
        ],
        usage: {
          prompt_tokens: USAGE.input_tokens,
          completion_tokens: USAGE.output_tokens,
          total_tokens: USAGE.total_tokens,
        },
      })
    );
    return;
  }
  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  });
  const chunk = (delta, finish) => ({
    id,
    object: 'chat.completion.chunk',
    created: 0,
    model,
    choices: [{ index: 0, delta, finish_reason: finish || null }],
  });
  sseWrite(res, null, chunk({ role: 'assistant', content: '' }));
  for (const piece of chunkText(text)) {
    sseWrite(res, null, chunk({ content: piece }));
    if (delayMs) await sleep(delayMs);
  }
  const last = chunk({}, 'stop');
  last.usage = {
    prompt_tokens: USAGE.input_tokens,
    completion_tokens: USAGE.output_tokens,
    total_tokens: USAGE.total_tokens,
  };
  sseWrite(res, null, last);
  res.write('data: [DONE]\n\n');
  res.end();
}

serveStub('openai-stub', stub, async ({ req, res, raw, url, base }) => {
  const isResponses = req.method === 'POST' && /\/responses(\?|$)/.test(url);
  const isChat = req.method === 'POST' && /\/chat\/completions(\?|$)/.test(url);
  if (!isResponses && !isChat) return false;

  let body;
  try {
    body = JSON.parse(raw);
  } catch (e) {
    stub.journal({ ...base, kind: 'bad_json', error: String(e) });
    res.writeHead(400, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ error: { message: 'stub: unparseable body' } }));
    return true;
  }
  const s = isResponses ? summarizeResponses(body) : summarizeChat(body);
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
  // Fail-open at response time, fail-closed at assert time (see
  // anthropic-stub.mjs: the UNMATCHED journal entry fails the scenario).
  const effective = picked || stub.unmatchedReply(base.seq);
  if (isResponses) await respondResponses(res, body.model, effective);
  else await respondChat(res, body.model, effective, s.stream);
  return true;
});
