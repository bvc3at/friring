// Shared core of the model-API stubs (anthropic / openai / gemini dialects).
// Everything here is dialect-AGNOSTIC: CLI args, the semantic fixture matcher,
// the journal, and the HTTP server skeleton (raw-body capture, /health, the
// catch-all "other" journaling). A dialect stub contributes only what actually
// differs per wire API: summarizing a request body into the shared match
// shape, and encoding a fixture reply into that API's (SSE) response format.
//
// The fixture schema and its matching semantics (docs/E2E.md) are shared
// across dialects on purpose — one fixtures.json vocabulary drives every
// agent, so scenarios and demo content never care which wire API a CLI
// speaks. Zero npm dependencies (node http only), same as always: the Rust
// dependency graph (cargo-deny) must stay untouched.
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

export function parseArgs(stubName) {
  const argv = process.argv.slice(2);
  const arg = (name, dflt) => {
    const i = argv.indexOf(`--${name}`);
    return i >= 0 ? argv[i + 1] : dflt;
  };
  const opts = {
    PORT: Number(arg('port', '0')),
    PORT_FILE: arg('port-file', ''),
    JOURNAL: arg('journal', ''),
    RAW_DIR: arg('raw-dir', ''),
    FIXTURES: arg('fixtures', ''),
  };
  if (!opts.JOURNAL || !opts.FIXTURES) {
    console.error(`${stubName}: --fixtures and --journal are required`);
    process.exit(2);
  }
  if (opts.RAW_DIR) fs.mkdirSync(opts.RAW_DIR, { recursive: true });
  return opts;
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function effectiveText(reply) {
  let text = reply.text || '';
  if (reply.flood) text = `${reply.flood.line}\n`.repeat(reply.flood.count) + text;
  return text;
}

// Chunk a reply text the way the real APIs stream it: word granularity for a
// natural typing cadence under delayMs, fixed 1KiB chunks for flood texts
// (the render path under test cares about bytes and cadence, and
// word-splitting 100KB would drown the stream in per-event overhead).
export function chunkText(text) {
  return text.length > 4096 ? text.match(/[\s\S]{1,1024}/g) : text.split(/(?<= )/);
}

function matches(m, s) {
  if (!m) return true;
  if (m.modelContains && !s.model.includes(m.modelContains)) return false;
  if (m.promptContains && !s.lastUserText.includes(m.promptContains)) return false;
  if (m.anyUserContains && !s.allUserText.includes(m.anyUserContains)) return false;
  // Distinguishes same-model ambient traffic whose user text echoes the
  // primary turn (e.g. opencode's title-generation call): the system prompt
  // is the only stable difference.
  if (m.systemContains && !(s.systemText || '').includes(m.systemContains)) return false;
  if (typeof m.hasToolResult === 'boolean' && s.hasToolResult !== m.hasToolResult)
    return false;
  if (m.toolResultFor && !s.toolResultIds.includes(m.toolResultFor)) return false;
  return true;
}

// The stub's whole runtime state: loaded fixtures + use counts + journal +
// sequence counters. `pick(summary)` implements first-match-wins with the
// maxUses loop guard; an unmatched call returns null (the dialect stub then
// fails OPEN with a marker reply and the harness fails CLOSED at assert time
// on the UNMATCHED journal entry — see anthropic-stub.mjs for the rationale).
export function createStub(stubName) {
  const opts = parseArgs(stubName);
  const fixtures = JSON.parse(fs.readFileSync(opts.FIXTURES, 'utf8'));
  const useCounts = new Map();
  let requestSeq = 0;
  let messageSeq = 0;
  return {
    opts,
    // The parsed fixture file, for dialect routes beyond `responses` (e.g. the
    // anthropic stub's account-usage endpoint reads a top-level `usage` key).
    fixtures,
    journal(entry) {
      fs.appendFileSync(opts.JOURNAL, JSON.stringify(entry) + '\n');
    },
    nextRequestSeq: () => ++requestSeq,
    requestCount: () => requestSeq,
    nextMessageId: (prefix) =>
      `${prefix}${String(++messageSeq).padStart(4, '0')}`,
    pick(s) {
      for (const r of fixtures.responses || []) {
        const name = r.name || 'unnamed';
        if (r.maxUses > 0 && (useCounts.get(name) || 0) >= r.maxUses) continue;
        if (matches(r.match, s)) {
          useCounts.set(name, (useCounts.get(name) || 0) + 1);
          return { name, ambient: !!r.ambient, delayMs: r.delayMs || 0, reply: r.reply };
        }
      }
      return null;
    },
    unmatchedReply(seq) {
      return {
        name: 'UNMATCHED',
        ambient: false,
        delayMs: 0,
        reply: { text: `[stub: unmatched request #${seq}]` },
      };
    },
  };
}

// HTTP skeleton shared by every dialect: buffers the body, saves it raw,
// answers /health, hands dialect endpoints to `route`, and journals + 200s
// anything the dialect doesn't claim (so a harmless telemetry/config probe
// can't fail a scenario, but conformance drift stays visible in artifacts).
// `route(ctx)` returns true if it handled the request; ctx = {req, res, raw,
// url, base} with `base` the pre-built journal entry stem.
export function serveStub(stubName, stub, route) {
  const server = http.createServer((req, res) => {
    const seq = stub.nextRequestSeq();
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', async () => {
      const raw = Buffer.concat(chunks).toString('utf8');
      if (stub.opts.RAW_DIR && raw)
        fs.writeFileSync(
          path.join(stub.opts.RAW_DIR, `${String(seq).padStart(3, '0')}.json`),
          raw
        );
      const url = req.url || '';
      const base = { seq, ts: new Date().toISOString(), method: req.method, url };

      if (req.method === 'GET' && url.startsWith('/health')) {
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ ok: true, requests: stub.requestCount() }));
        return;
      }
      if (await route({ req, res, raw, url, base })) return;
      stub.journal({ ...base, kind: 'other', bodyPreview: raw.slice(0, 200) });
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end('{}');
    });
  });

  server.listen(stub.opts.PORT, '127.0.0.1', () => {
    const port = server.address().port;
    if (stub.opts.PORT_FILE) fs.writeFileSync(stub.opts.PORT_FILE, String(port));
    console.log(`${stubName} listening on 127.0.0.1:${port}`);
  });

  for (const sig of ['SIGTERM', 'SIGINT']) {
    process.on(sig, () => {
      server.close();
      process.exit(0);
    });
  }
}
