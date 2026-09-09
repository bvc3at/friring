#!/usr/bin/env node
// friring's bridge-backed Team execution for oh-my-codex.
//
// One Node 20 ES module, no dependencies and no build step. Six subcommands:
//
//   leader     launch `omx` as the orchestrator, after checking the pins
//   worker     launch `codex` as a bridge child, after checking it exists
//   plan       turn OMX's approved Team DAG handoff into a friring run plan
//   run        create one bridge child per ready node and drive them
//   integrate  merge the branches friring verified, in order
//   evidence   write the checkpoint artifacts an OMX goal needs
//
// # Two rules hold everywhere in this file
//
// **No string ever becomes a command line.** Every external program — `omx`,
// `codex`, `git`, `friring-cli` — is started with an argv array and
// `shell: false`. The two one-line wrappers in `bin/` are the only shell in the
// extension, and each is a single `exec`.
//
// **Every JSON input is validated field by field before use.** `pins.json`, a
// Team DAG handoff, a plan, and every `friring-cli … --json` answer: parsed
// with `JSON.parse`, then checked against a closed shape. An unknown field or a
// wrong type refuses with the field named. A DAG comes out of a planning
// artifact an agent wrote; a bridge answer comes from friring, but a client that
// trusted its shape would fail confusingly the first time it changed.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

/** Where this program lives — the extension home is its parent. */
export const LIB_DIR = dirname(fileURLToPath(import.meta.url));
export const EXTENSION_HOME = dirname(LIB_DIR);

/** Exit status for a refusal. Distinct from a tool's own non-zero exit. */
const REFUSED = 2;

/**
 * Codex's own escape hatch for automation whose hook sources are already
 * vetted, passed by both agents here.
 *
 * Codex will not run a hook until its recorded hash is accepted at a "Hooks
 * need review" prompt, and that hash is keyed on the `hooks.json` **path**.
 * Neither agent has a stable one: the leader runs under `--madmax`, which mints
 * a `~/.omx-runs/run-<stamp>` state directory per launch, and a worker's private
 * `CODEX_HOME` is seeded fresh by friring on every launch from a `config.toml`
 * whose trust entries name the family's path. So the prompt is not a first-run
 * question that an operator can answer once — it is every launch, and friring
 * refuses to type into a pane showing it (`agent::tmux::MODAL_MARKERS`), which
 * leaves the agent unreachable. The hooks it asks about are the ones
 * `omx setup` and this extension's own install wrote.
 */
export const HOOK_TRUST_FLAG = '--dangerously-bypass-hook-trust';

/** How long `run` waits for the whole fan-out before giving up. */
const RUN_TIMEOUT_MS = 6 * 60 * 60 * 1000;

/** How long `run` waits between `status` polls. */
const POLL_MS = 5000;

/**
 * How many consecutive polls a node may be told "no capacity" while the run as
 * a whole makes no progress.
 *
 * `fanout_exhausted` and `quota` mean "not now": a slot frees as soon as a
 * sibling finishes, so retrying is right. What makes retrying *forever* wrong is
 * that the capacity may be held by something this run cannot see — another
 * leader's children under the same owner cap, or a sibling stuck in a state
 * only a person clears. This run's own state map cannot tell the difference, so
 * it counts instead: while anything at all changes the counter resets, and when
 * nothing has changed for this many polls the node is reported `blocked` with
 * what it was refused for. Six hours of silent polling and a `blocked` a person
 * can act on are the two ends of that choice.
 *
 * 60 polls is five minutes at [`POLL_MS`] — long enough for an ordinary worker
 * to finish and free its slot, short enough to be an answer rather than a
 * timeout.
 */
const MAX_CAPACITY_POLLS = 60;

/**
 * How long a `create` waits for friring's deferred answer.
 *
 * `friring-cli`'s own default is 120s, well under what the host allows itself.
 * A create has **two** blocking steps, not one — the worktree checkout and the
 * gated window spawn — each bounded by the host's 600s ceiling, plus 15s for the
 * egress acknowledgement and 60s for the readiness gate: 1275s in the worst
 * case. Giving up first is not a retry: friring has already cut the worktree by
 * then, so the next `create` on that branch is refused for a reason the plan
 * does not explain and the node is stuck until a person removes it. 1800s is
 * that ceiling with room, and a run that genuinely needs it has other problems.
 */
export const CREATE_TIMEOUT_SECS = 1800;

/** The one bridge protocol version this program speaks. */
export const BRIDGE_PROTOCOL = 1;

/** A refusal that carries its own message and nothing else. */
export class Refusal extends Error {}

/** Refuse with `message`. */
function refuse(message) {
  throw new Refusal(message);
}

// ── Pins ─────────────────────────────────────────────────────────────────

/**
 * Read `pins.json` and validate it structurally.
 *
 * The pins are what makes this extension specific to one OMX release: the
 * version its argv contract was read from, and the digest of every skill card
 * and role prompt whose text the routing depends on. Reading them structurally
 * rather than trusting the file is the same rule every other input follows.
 */
export function readPins(home = EXTENSION_HOME) {
  const path = join(home, 'pins.json');
  const pins = parseJsonFile(path);
  requireObject(pins, 'pins.json');
  requireNumber(pins.schema_version, 'pins.schema_version');
  if (pins.schema_version !== 1) refuse(`pins.json schema_version must be 1, not ${pins.schema_version}`);
  requireString(pins.omx_version, 'pins.omx_version');
  requireString(pins.omx_commit, 'pins.omx_commit');
  requireNumber(pins.node_min_major, 'pins.node_min_major');
  requireDigestMap(pins.skills, 'pins.skills');
  requireDigestMap(pins.role_prompts, 'pins.role_prompts');
  return pins;
}

function requireDigestMap(value, label) {
  requireObject(value, label);
  for (const [key, digest] of Object.entries(value)) {
    requireString(digest, `${label}.${key}`);
    if (!/^[0-9a-f]{64}$/.test(digest)) refuse(`${label}.${key} is not a sha256 digest`);
  }
}

// ── Validation helpers ───────────────────────────────────────────────────

function requireObject(value, label) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    refuse(`${label} must be an object`);
  }
}

function requireString(value, label) {
  if (typeof value !== 'string' || value.trim() === '') refuse(`${label} must be a non-empty string`);
  return value;
}

function requireNumber(value, label) {
  if (typeof value !== 'number' || !Number.isFinite(value)) refuse(`${label} must be a number`);
  return value;
}

function optionalString(value, label) {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== 'string') refuse(`${label} must be a string`);
  return value;
}

function optionalStringArray(value, label) {
  if (value === undefined || value === null) return undefined;
  if (!Array.isArray(value) || value.some((v) => typeof v !== 'string')) {
    refuse(`${label} must be an array of strings`);
  }
  return value;
}

function optionalBoolean(value, label) {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== 'boolean') refuse(`${label} must be a boolean`);
  return value;
}

/** Read and parse one JSON file, or refuse naming it. */
export function parseJsonFile(path) {
  let text;
  try {
    text = readFileSync(path, 'utf-8');
  } catch (error) {
    refuse(`cannot read ${path}: ${error.message}`);
  }
  try {
    return JSON.parse(text);
  } catch (error) {
    refuse(`${path} is not valid JSON: ${error.message}`);
  }
  return undefined;
}

/** SHA-256 of a file, lowercase hex, or `null` when it is not there. */
export function digestOf(path) {
  try {
    return createHash('sha256').update(readFileSync(path)).digest('hex');
  } catch {
    return null;
  }
}

// ── Running external programs ────────────────────────────────────────────

/**
 * Run one program, argv-only.
 *
 * `shell: false` is not the default this relies on — it is stated, because the
 * single most consequential property of this file is that no string it holds
 * ever reaches an interpreter. `argv` is an array; an element containing a
 * space, a semicolon or a quote is one argument, not a command line.
 *
 * `spawn` is injected so a test can watch exactly what would have been run.
 */
export function runArgv(program, argv, options = {}, spawn = spawnSync) {
  if (typeof program !== 'string' || program === '') refuse('a program name is required');
  if (!Array.isArray(argv) || argv.some((a) => typeof a !== 'string')) {
    refuse(`${program} arguments must be an array of strings`);
  }
  return spawn(program, argv, {
    shell: false,
    encoding: 'utf-8',
    ...options,
  });
}

/** Run a program and return its trimmed stdout, or refuse. */
function capture(program, argv, spawn = spawnSync) {
  const result = runArgv(program, argv, { stdio: ['ignore', 'pipe', 'pipe'] }, spawn);
  if (result.error) refuse(`cannot run ${program}: ${result.error.message}`);
  if (result.status !== 0) {
    const stderr = (result.stderr || '').trim().split('\n')[0] || '';
    refuse(`${program} ${argv.join(' ')} failed: ${stderr}`);
  }
  return (result.stdout || '').trim();
}

// ── leader ───────────────────────────────────────────────────────────────

/**
 * The argv this program hands `omx`, given the argv friring handed the wrapper.
 *
 * friring emits the session-selection group **before** the static args, so a
 * fresh launch arrives as `["--direct"]` and a restart as
 * `["resume", "--last", "--direct"]`. OMX's own `resolveCliInvocation` reads its
 * first argument: a leading `--flag` is a launch, and a leading `resume` is a
 * resume whose remaining arguments are its own. Both forms are therefore
 * already in the order OMX expects, and this passes them through in order.
 *
 * What it refuses is a shape OMX would read as a *different command*: a first
 * argument that is neither a flag nor `resume` would select whichever
 * subcommand it happens to name.
 */
export function leaderArgv(forwarded) {
  const argv = [...forwarded];
  const first = argv[0];
  if (first !== undefined && first !== 'resume' && !first.startsWith('--')) {
    refuse(
      `omx-leader was launched with '${first}' first, which omx would read as a subcommand ` +
        'rather than a launch. Expected a flag, or `resume`',
    );
  }
  // A launch with no policy flag would take OMX's default, which is the tmux
  // one; `--direct` is what keeps the agent in the pane friring opened.
  if (!argv.includes('--direct')) argv.push('--direct');
  // Codex's hook-trust escape hatch, forwarded through OMX, and not optional
  // here. Codex asks to review a hook whose recorded hash does not match, and
  // the hash is keyed on the `hooks.json` **path** — which under `--madmax`
  // (this agent's bypass flag) is a `~/.omx-runs/run-<stamp>` directory OMX
  // mints per launch, so no answer ever carries to the next one. The question
  // renders `› 1.`, a `MODAL_MARKERS` entry, so friring refuses to type into
  // that pane and the leader is unreachable for as long as it stands there.
  // The hooks being asked about are the ones `omx setup` and this extension's
  // own install wrote a moment earlier, which is the case the flag exists for.
  if (!argv.includes(HOOK_TRUST_FLAG)) argv.push(HOOK_TRUST_FLAG);
  return argv;
}

/**
 * Launch OMX as the leader.
 *
 * Two checks before anything runs, and both are refusals:
 *
 * - `omx --version` must equal the pinned one. This extension's routing, its
 *   argv contract and its skill digests were all read from one release.
 * - `TMUX` must be unset. friring strips it from every sandboxed launch
 *   (`MUX_NESTING_ENV`), and OMX's native Team mode requires it — so an inherited
 *   `TMUX` would mean the boundary did not strip what it should have.
 */
export function leader(forwarded, env = process.env, spawn = spawnSync, home = EXTENSION_HOME) {
  const pins = readPins(home);
  if (env.TMUX) {
    refuse(
      'TMUX is set in this pane. friring strips it from every sandboxed launch, so its presence ' +
        'means this leader is not running inside the boundary it should be',
    );
  }
  const version = capture('omx', ['--version'], spawn);
  // The version token, not a substring of the line: `includes` would accept
  // `10.21.0`, `0.21.0-beta.1` and `0.21.01` as the pinned `0.21.0`, which is
  // exactly the different-release case this refusal exists to catch. Output
  // with no version token at all is refused too — an omx that cannot say what
  // it is has not been shown to be the pinned one.
  const token = /\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.]+)?/.exec(version);
  if (!token || token[0] !== pins.omx_version) {
    refuse(
      `this extension is pinned to oh-my-codex ${pins.omx_version} and 'omx --version' says ` +
        `'${version}'. Install the pinned release, or install an extension built for this one`,
    );
  }
  const argv = leaderArgv(forwarded);
  const result = runArgv('omx', argv, { stdio: 'inherit' }, spawn);
  if (result.error) refuse(`cannot run omx: ${result.error.message}`);
  return result.status ?? 0;
}

// ── worker ───────────────────────────────────────────────────────────────

/**
 * The argv this program hands `codex`, given what friring handed the wrapper.
 *
 * `-c model_instructions_file=<home>/worker/AGENTS.md` comes first, because a
 * worker's instructions are the extension's and not the repository's, and
 * friring's own argv follows in its original order — which for a restart is
 * `resume --last`, exactly where the pinned codex accepts it.
 */
export function workerArgv(forwarded, home = EXTENSION_HOME) {
  const instructions = join(home, 'worker', 'AGENTS.md');
  // The same hook-trust escape hatch the leader needs, for the same reason and
  // one that is structural for a child: friring seeds a worker's private
  // `CODEX_HOME` fresh on every launch, and the `config.toml` it copies in
  // carries trust hashes keyed on the *family's* `hooks.json` path. They can
  // never match, so a worker would open on "Hooks need review" and never fire
  // the status hook S9 waits for. Global, so it comes before a `resume`
  // subcommand rather than after it.
  return [HOOK_TRUST_FLAG, '-c', `model_instructions_file=${instructions}`, ...forwarded];
}

/** Launch codex as a bridge child. */
export function worker(forwarded, env = process.env, spawn = spawnSync, home = EXTENSION_HOME) {
  if (env.TMUX) {
    refuse(
      'TMUX is set in this pane. friring strips it from every sandboxed launch, so its presence ' +
        'means this worker is not running inside the boundary it should be',
    );
  }
  // A worker must be running from the private state directory friring seeded,
  // and `CODEX_HOME` is how it was pointed at one. Its absence means the launch
  // did not narrow, which is the one thing a worker must not run without.
  if (!env.CODEX_HOME) {
    refuse(
      'CODEX_HOME is not set. A bridge child runs from a private agent state directory friring ' +
        'seeds and points it at; without one this worker would read and write its family’s state',
    );
  }
  const argv = workerArgv(forwarded, home);
  const result = runArgv('codex', argv, { stdio: 'inherit' }, spawn);
  if (result.error) refuse(`cannot run codex: ${result.error.message}`);
  return result.status ?? 0;
}

// ── plan ─────────────────────────────────────────────────────────────────

/**
 * Validate one Team DAG handoff, structurally, against OMX's v1 shape.
 *
 * A DAG is written by an agent into a planning artifact, so every field is
 * checked and a bad one is named. Reproduced here rather than imported because
 * this program has no dependencies and OMX is TypeScript that would need a
 * build; the shape is `TeamDagHandoff` in `src/team/dag-schema.ts` at the pinned
 * commit, and the tests pin it.
 */
export function parseTeamDag(value) {
  requireObject(value, 'Team DAG handoff');
  if (value.schema_version !== 1) refuse('Team DAG handoff schema_version must be 1');
  if (!Array.isArray(value.nodes) || value.nodes.length === 0) {
    refuse('Team DAG handoff nodes must be a non-empty array');
  }
  const seen = new Set();
  const nodes = value.nodes.map((raw, index) => {
    requireObject(raw, `node ${index + 1}`);
    const id = requireString(raw.id, `node ${index + 1} id`);
    if (seen.has(id)) refuse(`duplicate node id: ${id}`);
    seen.add(id);
    return {
      id,
      subject: requireString(raw.subject, `node ${id} subject`),
      description: requireString(raw.description, `node ${id} description`),
      role: optionalString(raw.role, `node ${id} role`),
      lane: optionalString(raw.lane, `node ${id} lane`),
      filePaths: optionalStringArray(raw.filePaths, `node ${id} filePaths`) ?? [],
      domains: optionalStringArray(raw.domains, `node ${id} domains`) ?? [],
      depends_on: optionalStringArray(raw.depends_on, `node ${id} depends_on`) ?? [],
      requires_code_change: optionalBoolean(raw.requires_code_change, `node ${id} requires_code_change`),
      acceptance: optionalStringArray(raw.acceptance, `node ${id} acceptance`) ?? [],
    };
  });
  for (const node of nodes) {
    for (const dep of node.depends_on) {
      if (!seen.has(dep)) refuse(`node ${node.id} depends on unknown node: ${dep}`);
    }
  }
  assertAcyclic(nodes);
  return {
    schema_version: 1,
    plan_slug: optionalString(value.plan_slug, 'plan_slug'),
    source_prd: optionalString(value.source_prd, 'source_prd'),
    nodes,
  };
}

function assertAcyclic(nodes) {
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const visiting = new Set();
  const visited = new Set();
  const visit = (id) => {
    if (visited.has(id)) return;
    if (visiting.has(id)) refuse(`cycle detected at node: ${id}`);
    visiting.add(id);
    for (const dep of byId.get(id)?.depends_on ?? []) visit(dep);
    visiting.delete(id);
    visited.add(id);
  };
  for (const node of nodes) visit(node.id);
  return true;
}

/**
 * Serialize the writers that would collide.
 *
 * Two nodes with **no dependency edge between them** whose `filePaths` overlap
 * would be two workers editing one file in two worktrees, and whichever merged
 * second would conflict. So an edge is added, in input order, and the later node
 * waits. A node that changes code and names *no* files is treated as touching
 * everything: it cannot be reasoned about, so it runs alone among the
 * code-changing nodes.
 *
 * `strict` refuses instead of serializing — for an operator who would rather fix
 * the plan than have friring quietly change its shape.
 */
export function serializeWriters(nodes, { strict = false } = {}) {
  const order = new Map(nodes.map((node, index) => [node.id, index]));
  const reaches = transitiveDependencies(nodes);
  const added = [];
  const out = nodes.map((node) => ({ ...node, depends_on: [...node.depends_on] }));
  const byId = new Map(out.map((node) => [node.id, node]));
  const changesCode = (node) => node.requires_code_change !== false && (node.filePaths.length > 0 || node.requires_code_change === true);
  const unscoped = out.filter((node) => node.requires_code_change === true && node.filePaths.length === 0);

  for (let i = 0; i < out.length; i += 1) {
    for (let j = i + 1; j < out.length; j += 1) {
      const a = out[i];
      const b = out[j];
      if (reaches.get(a.id).has(b.id) || reaches.get(b.id).has(a.id)) continue;
      const overlap = a.filePaths.filter((p) => b.filePaths.includes(p));
      const collides = overlap.length > 0
        || (unscoped.includes(a) && changesCode(b))
        || (unscoped.includes(b) && changesCode(a));
      if (!collides) continue;
      if (strict) {
        refuse(
          `nodes ${a.id} and ${b.id} would write the same files (${overlap.join(', ') || 'unscoped code change'}) ` +
            'with no dependency between them; add an edge or narrow filePaths',
        );
      }
      const [first, second] = order.get(a.id) <= order.get(b.id) ? [a, b] : [b, a];
      if (!byId.get(second.id).depends_on.includes(first.id)) {
        byId.get(second.id).depends_on.push(first.id);
        added.push({ node: second.id, waits_for: first.id, files: overlap });
        // Recompute so a later pair sees the edge this one added.
        for (const [id, set] of transitiveDependencies(out)) reaches.set(id, set);
      }
    }
  }
  assertAcyclic(out);
  return { nodes: out, serialized: added };
}

/** For each node, every node it transitively depends on. */
function transitiveDependencies(nodes) {
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const memo = new Map();
  const walk = (id, stack = new Set()) => {
    if (memo.has(id)) return memo.get(id);
    if (stack.has(id)) return new Set();
    stack.add(id);
    const out = new Set();
    for (const dep of byId.get(id)?.depends_on ?? []) {
      out.add(dep);
      for (const inner of walk(dep, stack)) out.add(inner);
    }
    stack.delete(id);
    memo.set(id, out);
    return out;
  };
  for (const node of nodes) walk(node.id);
  return memo;
}

/** Where a repository's friring run plan for one slug lives. */
export function planPath(repoRoot, slug) {
  return join(repoRoot, '.omx', 'friring', `team-${slug}`, 'plan.json');
}

/** Find the latest approved PRD and its Team DAG handoff. */
export function readTeamDag(repoRoot) {
  const plansDir = join(repoRoot, '.omx', 'plans');
  if (!existsSync(plansDir)) refuse(`${plansDir} does not exist: run OMX planning first`);
  const files = readdirSync(plansDir).sort((a, b) => a.localeCompare(b));
  const prd = files.filter((f) => /^prd-.*\.md$/i.test(f)).at(-1);
  if (!prd) refuse(`no approved PRD (prd-*.md) in ${plansDir}`);
  const slug = prd.replace(/^prd-/i, '').replace(/\.md$/i, '');
  const testSpec = files.some((f) => new RegExp(`^test-?spec-${escapeRegExp(slug)}\\.md$`, 'i').test(f));
  if (!testSpec) {
    refuse(`prd-${slug}.md has no matching test spec in ${plansDir}; OMX planning is incomplete`);
  }
  const sidecar = join(plansDir, `team-dag-${slug}.json`);
  if (existsSync(sidecar)) {
    return { slug, source: sidecar, dag: parseTeamDag(parseJsonFile(sidecar)) };
  }
  const prdPath = join(plansDir, prd);
  const fenced = extractHandoff(readFileSync(prdPath, 'utf-8'));
  if (!fenced) refuse(`no Team DAG handoff in ${sidecar} or in ${prdPath}`);
  let parsed;
  try {
    parsed = JSON.parse(fenced);
  } catch (error) {
    refuse(`the Team DAG Handoff block in ${prdPath} is not valid JSON: ${error.message}`);
  }
  return { slug, source: prdPath, dag: parseTeamDag(parsed) };
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** The first fenced block after a `Team DAG Handoff` heading. */
export function extractHandoff(content) {
  const heading = content.toLowerCase().indexOf('team dag handoff');
  if (heading < 0) return null;
  const fence = /```(?:json)?\s*\n([\s\S]*?)\n```/i.exec(content.slice(heading));
  return fence ? fence[1].trim() : null;
}

/**
 * Refuse a plan whose nodes do not each get their own branch.
 *
 * `cleanRefComponent` keeps an already-legal id verbatim, so a sanitized id can
 * still land on a name some *other* node already owns literally — `foo.lock`
 * becomes `foo-0ddf73bf`, which is exactly what a node literally called
 * `foo-0ddf73bf` gets. `parseTeamDag` dedupes node **ids**, nothing dedupes the
 * branches they generate, and friring refuses a `create` whose worktree already
 * exists — so the second node would fail for a reason its plan gives no hint of.
 * Naming both ids here is the only place that can explain it.
 */
function assertDistinctBranches(planSlug, nodes) {
  const owner = new Map();
  for (const node of nodes) {
    const branch = branchFor(planSlug, node.id);
    // Case-folded, because the collision that matters is between two
    // **worktree directories** and the default macOS filesystem is
    // case-insensitive: `foo` and `Foo` are two branches git will happily make
    // and one directory friring would put both children in. Refusing at plan
    // time is the whole point of this check — catching it at fan-out instead
    // means a worktree is already cut.
    const key = branch.toLowerCase();
    const first = owner.get(key);
    if (first !== undefined) {
      refuse(`nodes '${first}' and '${node.id}' both resolve to branch ${branch}; rename one`);
    }
    owner.set(key, node.id);
  }
}

/** Build and write the run plan. */
export function plan(repoRoot, { strict = false, write = true } = {}) {
  const { slug, source, dag } = readTeamDag(repoRoot);
  const { nodes, serialized } = serializeWriters(dag.nodes, { strict });
  assertDistinctBranches(dag.plan_slug ?? slug, nodes);
  const document = {
    schema_version: 1,
    plan_slug: dag.plan_slug ?? slug,
    source,
    nodes,
    serialized,
  };
  if (write) {
    const path = planPath(repoRoot, slug);
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, `${JSON.stringify(document, null, 2)}\n`);
    document.path = path;
  }
  return document;
}

/** Read a plan back, validated the same way it was written. */
export function readPlan(repoRoot, slug) {
  const document = parseJsonFile(planPath(repoRoot, slug));
  requireObject(document, 'plan.json');
  if (document.schema_version !== 1) refuse('plan.json schema_version must be 1');
  requireString(document.plan_slug, 'plan.plan_slug');
  const dag = parseTeamDag({ schema_version: 1, nodes: document.nodes });
  // Re-checked rather than trusted: plan.json is an editable file, and `run`
  // reaches this function directly, so a hand-edited node id must not get as far
  // as a `create` that lands in a sibling's worktree.
  assertDistinctBranches(document.plan_slug, dag.nodes);
  return { ...document, nodes: dag.nodes };
}

// ── The bridge client ────────────────────────────────────────────────────

/**
 * Call one bridge verb through `friring-cli`, argv-only, and validate the
 * answer.
 *
 * The answer comes from friring, but a client that assumed its shape would fail
 * confusingly the first time it changed — and `ok` is what decides whether a
 * branch is merged, so it is read from the field and never inferred.
 */
export function bridge(verb, args = [], spawn = spawnSync) {
  const result = runArgv(
    'friring-cli',
    ['bridge', verb, ...args],
    { stdio: ['ignore', 'pipe', 'pipe'] },
    spawn,
  );
  if (result.error) refuse(`cannot run friring-cli: ${result.error.message}`);
  const text = (result.stdout || '').trim();
  if (text === '') {
    const stderr = (result.stderr || '').trim().split('\n')[0] || 'no output';
    refuse(`friring-cli bridge ${verb} printed nothing: ${stderr}`);
  }
  let answer;
  try {
    answer = JSON.parse(text);
  } catch (error) {
    refuse(`friring-cli bridge ${verb} did not print JSON: ${error.message}`);
  }
  requireObject(answer, `bridge ${verb} answer`);
  if (answer.protocol !== BRIDGE_PROTOCOL) {
    refuse(
      `this extension speaks bridge protocol ${BRIDGE_PROTOCOL} and friring answered ` +
        `${answer.protocol}`,
    );
  }
  if (typeof answer.ok !== 'boolean') refuse(`bridge ${verb} answer has no 'ok' field`);
  return answer;
}

/** The task body one worker is given: its node, its role prompt, the protocol. */
export function taskBody(node, rolePrompt, home = EXTENSION_HOME) {
  const acceptance = node.acceptance.length
    ? node.acceptance.map((line) => `- ${line}`).join('\n')
    : '- (none stated in the plan)';
  const files = node.filePaths.length ? node.filePaths.map((p) => `- ${p}`).join('\n') : '- (unscoped)';
  return [
    `# ${node.subject}`,
    '',
    node.description,
    '',
    '## Files this node owns',
    files,
    '',
    '## Acceptance',
    acceptance,
    '',
    '## Role',
    rolePrompt || `(no role prompt for '${node.role ?? 'executor'}')`,
    '',
    '## Worker protocol',
    readFileSync(join(home, 'worker', 'AGENTS.md'), 'utf-8'),
  ].join('\n');
}

/** The pinned role prompt for a node, read from the operator's `~/.codex`. */
export function rolePromptFor(node, pins, codexHome = join(homedir(), '.codex')) {
  const role = node.role ?? 'executor';
  const expected = pins.role_prompts[role];
  if (!expected) return null;
  const path = join(codexHome, 'prompts', `${role}.md`);
  const actual = digestOf(path);
  if (actual === null) refuse(`${path} is missing; run 'omx setup --scope user'`);
  if (actual !== expected) {
    refuse(
      `${path} does not match the digest this extension is pinned to. Reinstall the pinned ` +
        'oh-my-codex, or install an extension built for the one you have',
    );
  }
  return readFileSync(path, 'utf-8');
}

// ── run ──────────────────────────────────────────────────────────────────

/**
 * Create one bridge child per ready node and drive the fan-out to terminal
 * states.
 *
 * A node is *ready* when every node it depends on is `done`. A dependency that
 * reached any other terminal state stops that branch of the graph: friring
 * verified that its work is not there, and starting a node that assumed it would
 * be is how a fan-out produces confident nonsense.
 *
 * `now` and `sleep` are injected so a test drives the loop without waiting, and
 * `codexHome` so one can put a pinned role prompt somewhere other than the
 * operator's own `~/.codex`.
 */
export async function run(
  repoRoot,
  slug,
  {
    spawn = spawnSync,
    sleep = defaultSleep,
    now = () => Date.now(),
    home = EXTENSION_HOME,
    codexHome = join(homedir(), '.codex'),
    pins,
  } = {},
) {
  const document = readPlan(repoRoot, slug);
  const resolved = pins ?? readPins(home);
  const nodes = document.nodes;
  const state = new Map(nodes.map((node) => [node.id, { node, child: null, state: 'pending' }]));
  const deadline = now() + RUN_TIMEOUT_MS;
  const mail = [];
  // What the whole run looked like on the previous pass. Any difference at all
  // is progress, and progress is what resets every node's capacity counter —
  // see MAX_CAPACITY_POLLS.
  let previous = null;

  for (;;) {
    refreshStates(state, spawn);
    const shape = runShape(state);
    if (shape !== previous) {
      previous = shape;
      for (const entry of state.values()) entry.capacityPolls = 0;
    }
    for (const entry of state.values()) {
      if (entry.child || entry.state !== 'pending') continue;
      const deps = entry.node.depends_on.map((id) => state.get(id));
      if (deps.some((d) => d.state !== 'done')) {
        if (deps.some((d) => isTerminal(d.state) && d.state !== 'done')) {
          entry.state = 'skipped';
        }
        continue;
      }
      const prompt = rolePromptFor(entry.node, resolved, codexHome);
      const answer = bridge(
        'create',
        [
          '--repo-root', repoRoot,
          '--branch', branchFor(slug, entry.node.id),
          '--agent', 'omx-worker-codex',
          '--task-kind', 'team-node',
          '--task-body', taskBody(entry.node, prompt, home),
          '--role-hint', entry.node.role ?? 'executor',
          '--timeout', String(CREATE_TIMEOUT_SECS),
        ],
        spawn,
      );
      if (!answer.ok) {
        // `fanout_exhausted` (the owner's live-child cap) and `quota` (friring's
        // concurrent-lifecycle-job cap) both mean "not now", not "not ever": a
        // slot frees as soon as a sibling finishes. The node stays `pending` so
        // the next pass offers it again. Bounded, though: the capacity may be
        // held by children this run cannot see, and polling to the six-hour
        // deadline is the same as never answering. Every other code is a
        // decision that will not change, so it stays terminal.
        if (answer.error === 'fanout_exhausted' || answer.error === 'quota') {
          entry.capacityPolls = (entry.capacityPolls ?? 0) + 1;
          if (entry.capacityPolls >= MAX_CAPACITY_POLLS) {
            entry.state = 'blocked';
            entry.error =
              `friring refused this node ${answer.error} for ` +
              `${MAX_CAPACITY_POLLS} consecutive polls with nothing in this run changing. ` +
              'The child slots are held by something this run does not own — check the ' +
              "leader's own children with 'friring-cli bridge status', stop whichever are " +
              'stuck, and run this plan again.';
          }
          continue;
        }
        entry.state = 'refused';
        entry.error = answer.message ?? answer.error;
        continue;
      }
      entry.child = requireString(answer.data?.child_id, 'create answer child_id');
      entry.state = 'starting';
      entry.capacityPolls = 0;
    }
    // The leader's own mail, every pass. A worker that asks a question sits in
    // `blocked` until somebody answers, and nothing in this loop answers — so
    // draining is how the question reaches a person instead of expiring against
    // the run timeout. Claimed rather than peeked: a message read twice would be
    // printed on every pass.
    for (const message of drainInbox(spawn)) mail.push(message);

    // A child that needs a person stops the run *now*. `blocked` is a question,
    // `dirty` is a worktree friring will not integrate, `stop_failed` is a pane
    // it could not kill, and `stalled` is a child that stopped answering
    // friring's nudges — core sets that from the nudge counter and never clears
    // it (`mirror_child_states` mirrors only ready/working/blocked, and
    // `ChildState::is_resumable` lists stalled), so only an operator's resume or
    // stop moves it. None of them changes on its own, and waiting six hours to
    // say so is the same as not saying it.
    const held = [...state.values()].filter((e) => NEEDS_OPERATOR.includes(e.state));
    if (held.length > 0) {
      return finish(state, mail, held);
    }
    if ([...state.values()].every((e) => isTerminal(e.state))) break;
    if (now() >= deadline) refuse('the team run did not finish within its time limit');
    await sleep(POLL_MS);
  }
  return finish(state, mail, []);
}

/** Shape one `run` outcome, whatever ended it. */
function finish(state, mail, held) {
  return {
    nodes: [...state.values()].map((e) => ({
      id: e.node.id,
      child: e.child,
      state: e.state,
      error: e.error,
    })),
    mail,
    // Non-empty exactly when a person has to look. The caller exits non-zero on
    // it rather than reporting a fan-out that is merely stuck. The `error` is
    // carried because for a capacity block it is the only place that says what
    // to do about it.
    needs_operator: held.map((e) => ({
      id: e.node.id,
      child: e.child,
      state: e.state,
      error: e.error,
    })),
  };
}

/**
 * A stable string of what every node is doing, for change detection.
 *
 * Compared rather than diffed: the question is only "did anything at all move",
 * and any movement is enough to say the run is not wedged.
 */
function runShape(state) {
  return [...state.values()].map((e) => `${e.node.id}=${e.state}:${e.child ?? ''}`).join('|');
}

/**
 * Claim the leader's mail and return it as plain objects.
 *
 * A refusal is not fatal: the mailbox is a courtesy to the operator, and losing
 * it must not stop a fan-out that is otherwise fine.
 */
function drainInbox(spawn) {
  const answer = bridge('inbox', ['--claim', '--limit', '20'], spawn);
  if (!answer.ok) return [];
  const messages = Array.isArray(answer.data?.messages) ? answer.data.messages : [];
  return messages.map((m) => ({
    id: m.id ?? null,
    from: m.from ?? null,
    kind: typeof m.kind === 'string' ? m.kind : 'unknown',
    body: m.body ?? null,
  }));
}

/** The states that do not change without a person. */
const NEEDS_OPERATOR = ['blocked', 'dirty', 'stalled', 'stop_failed'];

/** Terminal for the purposes of the fan-out loop. */
function isTerminal(state) {
  return ['done', 'failed', 'stopped', 'unusable', 'refused', 'skipped'].includes(state);
}

/** Update every started node's state from one `status` call. */
function refreshStates(state, spawn) {
  const started = [...state.values()].filter((e) => e.child && !isTerminal(e.state));
  if (started.length === 0) return;
  const answer = bridge('status', [], spawn);
  if (!answer.ok) refuse(`friring refused a status call: ${answer.message ?? answer.error}`);
  const children = Array.isArray(answer.data?.children) ? answer.data.children : [];
  for (const entry of started) {
    const row = children.find((c) => c.id === entry.child);
    if (row && typeof row.state === 'string') entry.state = row.state;
  }
}

/**
 * The branch a node's worktree is cut on.
 *
 * The name has to survive `git check-ref-format --branch`, which friring runs
 * before it cuts the worktree (`check_child_branch_name`) — so a node id like
 * `node/../..` must not turn into a component carrying `..`, a leading or
 * trailing `.`, a `.lock` suffix, or nothing at all. Each of those is a refused
 * create rather than a slightly odd branch name.
 */
export function branchFor(slug, nodeId) {
  return `omx/${cleanRefComponent(slug)}/${cleanRefComponent(nodeId)}`;
}

/**
 * One node id or plan slug, as a branch-name component git will accept —
 * **without merging two distinct inputs onto one name**.
 *
 * Sanitizing alone cannot have both properties. `git check-ref-format` refuses
 * `..`, a leading or trailing `.` or `-`, an empty component and a `.lock`
 * suffix, and every rule that removes one of those is many-to-one: `foo.lock`
 * and `foo` collapse together, so do `a..b` and `a.b`, and so do `...` and `""`.
 * A collision is not cosmetic here — two nodes would resolve to one worktree,
 * and friring refuses a `create` whose worktree already exists, so the second
 * node fails for a reason its plan gives no hint of.
 *
 * So the sanitized form is kept for a person to read, and a short digest of the
 * **original** is appended whenever sanitizing changed anything. Distinct inputs
 * then stay distinct by construction, and an input that needed no cleaning is
 * left exactly as it was.
 */
function cleanRefComponent(value) {
  const safe = String(value)
    .replace(/[^A-Za-z0-9._-]+/g, '-')
    .replace(/\.{2,}/g, '.')
    .replace(/^[-.]+|[-.]+$/g, '')
    .replace(/\.lock$/, '')
    .replace(/[-.]+$/, '');
  // An input that needed no cleaning is left exactly as it was — the
  // disambiguator must not make every branch unreadable. An input that
  // sanitizes to nothing is *not* that case, even when it was empty to begin
  // with: an empty component is not a branch name.
  if (safe !== '' && safe === String(value)) return safe;
  // Eight hex characters: enough that two ids in one plan will not collide, and
  // short enough that the readable half still dominates the name.
  const tag = createHash('sha256').update(String(value)).digest('hex').slice(0, 8);
  return safe === '' ? tag : `${safe}-${tag}`;
}

function defaultSleep(ms) {
  return new Promise((done) => {
    // Referenced on purpose: every bridge call in the run loop is `spawnSync`,
    // so between two of them this timer is the only handle keeping the process
    // alive. Unref'd, node drains the event loop and exits at the first poll —
    // before any child reaches a terminal state and before the outcome is
    // written.
    setTimeout(done, ms);
  });
}

// ── integrate ────────────────────────────────────────────────────────────

/**
 * Merge the branches friring verified, one at a time, in plan order.
 *
 * The gate is the **host's** verdict and nothing else: state `done` (which
 * implies `outcome = completed`), `dirty` false, and `git rev-parse <branch>`
 * equal to the head friring recorded after it stopped the pane. A worker's own
 * summary is not consulted, and a branch that moved after the verdict is
 * refused rather than merged — the verified head is the one that was verified.
 *
 * Conflicts are **reported, never resolved**. A merge this could finish by
 * choosing sides is one nobody reviewed.
 */
export function integrate(repoRoot, slug, { spawn = spawnSync } = {}) {
  const document = readPlan(repoRoot, slug);
  const answer = bridge('status', [], spawn);
  if (!answer.ok) refuse(`friring refused a status call: ${answer.message ?? answer.error}`);
  const children = Array.isArray(answer.data?.children) ? answer.data.children : [];
  const merged = [];
  const skipped = [];
  for (const node of document.nodes) {
    const branch = branchFor(slug, node.id);
    const child = children.find((c) => c.result && c.result.branch === branch);
    if (!child) {
      skipped.push({ node: node.id, branch, reason: 'friring has no verified result for it' });
      continue;
    }
    if (child.state !== 'done') {
      skipped.push({ node: node.id, branch, reason: `friring's verdict is '${child.state}'` });
      continue;
    }
    if (child.result.dirty) {
      skipped.push({ node: node.id, branch, reason: 'its worktree was dirty when friring stopped it' });
      continue;
    }
    const head = capture('git', ['-C', repoRoot, 'rev-parse', branch], spawn);
    if (head !== child.result.head) {
      skipped.push({
        node: node.id,
        branch,
        reason: `it has moved since friring verified it (${head} vs ${child.result.head})`,
      });
      continue;
    }
    const result = runArgv(
      'git',
      ['-C', repoRoot, 'merge', '--no-ff', '-m', `omx: ${node.id} (${node.subject})`, branch],
      { stdio: ['ignore', 'pipe', 'pipe'] },
      spawn,
    );
    if (result.status !== 0) {
      // Left exactly as git left it: a conflict is somebody's to look at.
      return {
        merged,
        skipped,
        conflict: {
          node: node.id,
          branch,
          detail: (result.stdout || result.stderr || '').trim(),
        },
      };
    }
    merged.push({ node: node.id, branch, head });
  }
  return { merged, skipped, conflict: null };
}

// ── evidence ─────────────────────────────────────────────────────────────

/**
 * Write the checkpoint artifacts an OMX goal needs.
 *
 * Two files, and every field in them is **host-known**: the goal id from OMX's
 * own ledger, the verification commands the operator ran, and per node the
 * branch, head and outcome friring recorded after it stopped the pane. A
 * worker's summary is not evidence of anything, so it is not here.
 */
export function evidence(repoRoot, slug, { spawn = spawnSync, verification = [] } = {}) {
  const document = readPlan(repoRoot, slug);
  const answer = bridge('status', [], spawn);
  if (!answer.ok) refuse(`friring refused a status call: ${answer.message ?? answer.error}`);
  const children = Array.isArray(answer.data?.children) ? answer.data.children : [];
  const goalsPath = join(repoRoot, '.omx', 'ultragoal', 'goals.json');
  const goal = readActiveGoal(goalsPath);

  const nodes = document.nodes.map((node) => {
    const branch = branchFor(slug, node.id);
    const child = children.find((c) => c.result && c.result.branch === branch);
    return {
      id: node.id,
      subject: node.subject,
      branch,
      state: child?.state ?? 'unknown',
      head: child?.result?.head ?? null,
      dirty: child?.result?.dirty ?? null,
      ahead_of_base: child?.result?.ahead_of_base ?? null,
      verified_at: child?.result?.verified_at ?? null,
    };
  });

  const document_json = {
    schema_version: 1,
    plan_slug: document.plan_slug,
    goal_id: goal?.id ?? null,
    goals_path: goalsPath,
    verification,
    nodes,
  };
  const dir = dirname(planPath(repoRoot, slug));
  mkdirSync(dir, { recursive: true });
  const jsonPath = join(dir, 'evidence.json');
  const textPath = join(dir, 'evidence.txt');
  writeFileSync(jsonPath, `${JSON.stringify(document_json, null, 2)}\n`);
  writeFileSync(textPath, renderEvidence(document_json));
  return { ...document_json, jsonPath, textPath };
}

/** The active goal in OMX's ledger, or `null` when there is none. */
export function readActiveGoal(goalsPath) {
  if (!existsSync(goalsPath)) return null;
  const goals = parseJsonFile(goalsPath);
  const list = Array.isArray(goals) ? goals : goals?.goals;
  if (!Array.isArray(list)) return null;
  const active = list.find((g) => g && typeof g === 'object' && g.status !== 'completed');
  const chosen = active ?? list.at(-1);
  if (!chosen || typeof chosen.id !== 'string') return null;
  return { id: chosen.id, status: optionalString(chosen.status, 'goal.status') };
}

/** The human half of the evidence pair. */
export function renderEvidence(document) {
  const lines = [
    `goal: ${document.goal_id ?? '(none recorded)'}`,
    `goals: ${document.goals_path}`,
    `plan: ${document.plan_slug}`,
    '',
    'verification:',
  ];
  if (document.verification.length === 0) {
    lines.push('  (none recorded)');
  } else {
    for (const entry of document.verification) {
      lines.push(`  ${entry.command} -> ${entry.outcome}`);
    }
  }
  lines.push('', 'nodes:');
  for (const node of document.nodes) {
    lines.push(
      `  ${node.id} [${node.state}] ${node.branch} head=${node.head ?? '-'} ` +
        `dirty=${node.dirty ?? '-'} ahead=${node.ahead_of_base ?? '-'}`,
    );
  }
  return `${lines.join('\n')}\n`;
}

// ── Entry point ──────────────────────────────────────────────────────────

/** Dispatch one invocation. Returns the process exit status. */
/**
 * The oldest Node this program is written against.
 *
 * `node --test`, `structuredClone` and the `node:` import prefix used
 * throughout are all Node 20. The manifest's `tool-version` requirement is a
 * coarse first filter — a substring pattern cannot express "20 or newer" — so
 * the exact bound is checked here, where the version is a number rather than a
 * spelling. A leader that started under Node 18 would fail somewhere further in,
 * as a stack trace nobody can attribute.
 */
export const MIN_NODE_MAJOR = 20;

/** Refuse a Node too old to run this program, by number rather than by pattern. */
export function checkNodeVersion(version = process.versions.node) {
  const major = Number.parseInt(String(version).split('.')[0], 10);
  if (!Number.isFinite(major) || major < MIN_NODE_MAJOR) {
    refuse(
      `this program needs Node ${MIN_NODE_MAJOR} or newer; this is Node ${version}`,
    );
  }
}

export async function main(argv, env = process.env) {
  checkNodeVersion();
  const [sub, ...rest] = argv;
  switch (sub) {
    case 'leader':
      return leader(rest, env);
    case 'worker':
      return worker(rest, env);
    case 'plan': {
      const repo = rest[0] ?? process.cwd();
      const strict = rest.includes('--strict');
      const document = plan(repo, { strict });
      process.stdout.write(`${JSON.stringify(document, null, 2)}\n`);
      return 0;
    }
    case 'run': {
      const [repo, slug] = rest;
      const outcome = await run(requireString(repo, 'run <repo>'), requireString(slug, 'run <slug>'));
      process.stdout.write(`${JSON.stringify(outcome, null, 2)}\n`);
      // Non-zero when a person has to look, and not only when a node failed: a
      // `blocked` worker is a question waiting for an answer, and reporting the
      // run as merely unfinished would bury it.
      if (outcome.needs_operator.length > 0) return 1;
      return outcome.nodes.every((n) => n.state === 'done') ? 0 : 1;
    }
    case 'integrate': {
      const [repo, slug] = rest;
      const outcome = integrate(
        requireString(repo, 'integrate <repo>'),
        requireString(slug, 'integrate <slug>'),
      );
      process.stdout.write(`${JSON.stringify(outcome, null, 2)}\n`);
      return outcome.conflict ? 1 : 0;
    }
    case 'evidence': {
      const [repo, slug] = rest;
      const outcome = evidence(
        requireString(repo, 'evidence <repo>'),
        requireString(slug, 'evidence <slug>'),
      );
      process.stdout.write(`${outcome.textPath}\n`);
      return 0;
    }
    default:
      refuse(
        `unknown subcommand '${sub ?? ''}'. Expected leader, worker, plan, run, integrate or evidence`,
      );
      return REFUSED;
  }
}

// Only when run as a program, so `node --test` can import every export.
if (process.argv[1] && resolve(process.argv[1]) === resolve(fileURLToPath(import.meta.url))) {
  main(process.argv.slice(2))
    .then((status) => process.exit(status))
    .catch((error) => {
      const label = error instanceof Refusal ? 'refused' : 'failed';
      process.stderr.write(`friring-omx ${label}: ${error.message}\n`);
      process.exit(error instanceof Refusal ? REFUSED : 1);
    });
}

export { basename };
