// `node --test extensions/omx/lib` — run by `just omx-test`.
//
// What these pin, in one sentence each: the argv contract this extension reads
// from one oh-my-codex release, the shape of a Team DAG handoff, the writer
// serialization that keeps two workers out of one file, the rule that only
// friring's own verdict integrates a branch, and the property everything else
// rests on — that no string here ever becomes a command line.

import assert from 'node:assert/strict';
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';

import {
  CREATE_TIMEOUT_SECS,
  EXTENSION_HOME,
  MIN_NODE_MAJOR,
  Refusal,
  branchFor,
  checkNodeVersion,
  bridge,
  digestOf,
  evidence,
  extractHandoff,
  integrate,
  leader,
  HOOK_TRUST_FLAG,
  leaderArgv,
  parseTeamDag,
  plan,
  planPath,
  readActiveGoal,
  readPins,
  readPlan,
  renderEvidence,
  rolePromptFor,
  run,
  runArgv,
  serializeWriters,
  worker,
  workerArgv,
} from './friring-omx.mjs';

/** A `spawnSync` stand-in that records every call and answers from a script. */
function spy(answers = {}) {
  const calls = [];
  const spawn = (program, argv, options) => {
    calls.push({ program, argv, options });
    const key = `${program} ${argv[0] ?? ''}`;
    const answer = answers[key] ?? answers[program];
    if (typeof answer === 'function') return answer(program, argv, options);
    return answer ?? { status: 0, stdout: '', stderr: '' };
  };
  spawn.calls = calls;
  return spawn;
}

/** A bridge answer envelope. */
function ok(data) {
  return { status: 0, stdout: JSON.stringify({ protocol: 1, key: 'k', ok: true, data }), stderr: '' };
}

/** A bridge refusal envelope, carrying the machine-readable code. */
function refusal(error, message) {
  return {
    status: 0,
    stdout: JSON.stringify({ protocol: 1, key: 'k', ok: false, error, message }),
    stderr: '',
  };
}

function temp() {
  return mkdtempSync(join(tmpdir(), 'friring-omx-'));
}

// ── The argv contract ────────────────────────────────────────────────────

test('leaderArgv passes friring argv through in the order omx accepts', () => {
  const T = HOOK_TRUST_FLAG;
  // A fresh launch: friring emits the static args alone.
  assert.deepEqual(leaderArgv(['--direct']), ['--direct', T]);
  // A restart: the session-selection group comes first, which is exactly where
  // `resolveCliInvocation` expects `resume`.
  assert.deepEqual(leaderArgv(['resume', '--last', '--direct']), ['resume', '--last', '--direct', T]);
  // Nothing at all still launches directly rather than into OMX's tmux policy.
  assert.deepEqual(leaderArgv([]), ['--direct', T]);
  // An unknown flag passes through, after which `--direct` is still ensured.
  assert.deepEqual(leaderArgv(['--verbose']), ['--verbose', '--direct', T]);
  assert.deepEqual(leaderArgv(['resume', '--last']), ['resume', '--last', '--direct', T]);
  // Never twice, so an operator who adds it by hand does not get a duplicate.
  assert.deepEqual(leaderArgv(['--direct', T]), ['--direct', T]);
});

test('both agents carry the hook-trust flag, because neither can be pre-trusted', () => {
  // The leader's `hooks.json` path is a per-launch `--madmax` run directory and
  // a worker's private CODEX_HOME is seeded fresh every launch, so the recorded
  // hash can never match. Without this the pane opens on a numbered list
  // friring will not type into, and the agent is unreachable.
  assert.ok(leaderArgv(['--direct']).includes(HOOK_TRUST_FLAG));
  assert.ok(workerArgv([], '/opt/omx').includes(HOOK_TRUST_FLAG));
});

test('a first argument omx would read as a subcommand is refused', () => {
  // `omx team --direct` would run native Team, not a launch. A silent
  // wrong-command launch is exactly what this refusal turns into a stop.
  assert.throws(() => leaderArgv(['team', '--direct']), Refusal);
  assert.throws(() => leaderArgv(['exec']), Refusal);
});

test('workerArgv puts the extension instructions before friring argv', () => {
  const argv = workerArgv(['resume', '--last'], '/opt/omx');
  assert.deepEqual(argv, [
    HOOK_TRUST_FLAG,
    '-c',
    'model_instructions_file=/opt/omx/worker/AGENTS.md',
    'resume',
    '--last',
  ]);
});

// ── The two launch refusals ──────────────────────────────────────────────

test('the leader refuses an omx that is not the pinned one', () => {
  // Every one of these *contains* the pin or is contained by it, so a substring
  // check would launch a release this extension was not built against.
  for (const version of ['0.20.4', '10.21.0', '0.21.0-beta.1', '0.21.01', 'omx (unknown)']) {
    const spawn = spy({ 'omx --version': { status: 0, stdout: `${version}\n`, stderr: '' } });
    assert.throws(
      () => leader(['--direct'], {}, spawn),
      /pinned to oh-my-codex 0\.21\.0/,
      `'${version}' must not satisfy the pin`,
    );
  }

  // …and the release it names does launch.
  const pinned = spy({ 'omx --version': { status: 0, stdout: 'omx 0.21.0\n', stderr: '' } });
  assert.equal(leader(['--direct'], {}, pinned), 0);
  assert.equal(pinned.calls.at(-1).program, 'omx');
});

test('the leader refuses when TMUX survived into the pane', () => {
  // friring strips it from every sandboxed launch, so its presence means the
  // boundary did not apply.
  const spawn = spy();
  assert.throws(() => leader(['--direct'], { TMUX: '/tmp/s,1,0' }, spawn), /TMUX is set/);
  assert.equal(spawn.calls.length, 0, 'nothing runs before the check');
});

test('a worker refuses without the private state directory friring seeds', () => {
  const spawn = spy();
  assert.throws(() => worker([], {}, spawn), /CODEX_HOME is not set/);
  assert.throws(() => worker([], { TMUX: 'x', CODEX_HOME: '/p' }, spawn), /TMUX is set/);
  assert.equal(spawn.calls.length, 0);
});

// ── No string becomes a command line ─────────────────────────────────────

test('every external program is started with an argv array and shell false', () => {
  const spawn = spy({ 'omx --version': { status: 0, stdout: '0.21.0', stderr: '' } });
  leader(['--direct'], {}, spawn);
  assert.ok(spawn.calls.length >= 2);
  for (const call of spawn.calls) {
    assert.equal(call.options.shell, false, `${call.program} must not use a shell`);
    assert.ok(Array.isArray(call.argv));
  }
});

test('an argument carrying shell syntax arrives as one element', () => {
  const spawn = spy();
  const hostile = "; rm -rf / # 'and a quote' and a space";
  runArgv('git', ['-C', '/repo', 'merge', hostile], {}, spawn);
  assert.deepEqual(spawn.calls[0].argv, ['-C', '/repo', 'merge', hostile]);
  assert.equal(spawn.calls[0].argv.length, 4, 'nothing was split');
});

test('a non-string argument is refused rather than coerced', () => {
  assert.throws(() => runArgv('git', ['ok', 42], {}, spy()), Refusal);
});

// ── The Team DAG handoff ─────────────────────────────────────────────────

const DAG = {
  schema_version: 1,
  plan_slug: 'widgets',
  nodes: [
    { id: 'a', subject: 'A', description: 'do a', filePaths: ['src/a.rs'] },
    { id: 'b', subject: 'B', description: 'do b', filePaths: ['src/b.rs'], depends_on: ['a'] },
  ],
};

test('a well-formed handoff parses to the shape OMX defines', () => {
  const dag = parseTeamDag(structuredClone(DAG));
  assert.equal(dag.nodes.length, 2);
  assert.deepEqual(dag.nodes[1].depends_on, ['a']);
  // Absent optional arrays normalize to empty rather than undefined.
  assert.deepEqual(dag.nodes[0].depends_on, []);
  assert.deepEqual(dag.nodes[0].acceptance, []);
});

test('a malformed handoff is refused with the field named', () => {
  const cases = [
    [{ schema_version: 2, nodes: DAG.nodes }, /schema_version must be 1/],
    [{ schema_version: 1, nodes: [] }, /non-empty array/],
    [{ schema_version: 1, nodes: [{ subject: 'A', description: 'x' }] }, /node 1 id/],
    [{ schema_version: 1, nodes: [{ id: 'a', description: 'x' }] }, /node a subject/],
    [
      { schema_version: 1, nodes: [{ id: 'a', subject: 'A', description: 'x', filePaths: 'no' }] },
      /node a filePaths/,
    ],
    [
      {
        schema_version: 1,
        nodes: [
          { id: 'a', subject: 'A', description: 'x' },
          { id: 'a', subject: 'B', description: 'y' },
        ],
      },
      /duplicate node id: a/,
    ],
    [
      {
        schema_version: 1,
        nodes: [{ id: 'a', subject: 'A', description: 'x', depends_on: ['ghost'] }],
      },
      /depends on unknown node: ghost/,
    ],
    [
      {
        schema_version: 1,
        nodes: [
          { id: 'a', subject: 'A', description: 'x', depends_on: ['b'] },
          { id: 'b', subject: 'B', description: 'y', depends_on: ['a'] },
        ],
      },
      /cycle detected/,
    ],
  ];
  for (const [value, pattern] of cases) {
    assert.throws(() => parseTeamDag(value), pattern, JSON.stringify(value));
  }
});

test('the handoff is found after its heading, in the first fenced block', () => {
  const prd = ['# PRD', 'prose', '## Team DAG Handoff', '```json', '{"a":1}', '```'].join('\n');
  assert.equal(extractHandoff(prd), '{"a":1}');
  assert.equal(extractHandoff('# PRD\nno handoff here'), null);
});

// ── Writer serialization ─────────────────────────────────────────────────

test('two unrelated nodes writing one file are serialized in input order', () => {
  const { nodes, serialized } = serializeWriters(
    parseTeamDag({
      schema_version: 1,
      nodes: [
        { id: 'a', subject: 'A', description: 'x', filePaths: ['src/shared.rs'] },
        { id: 'b', subject: 'B', description: 'y', filePaths: ['src/shared.rs'] },
      ],
    }).nodes,
  );
  assert.deepEqual(nodes[1].depends_on, ['a'], 'the later node waits');
  assert.deepEqual(serialized, [{ node: 'b', waits_for: 'a', files: ['src/shared.rs'] }]);
});

test('nodes already ordered by a dependency are left alone', () => {
  const { serialized } = serializeWriters(parseTeamDag(structuredClone({
    schema_version: 1,
    nodes: [
      { id: 'a', subject: 'A', description: 'x', filePaths: ['src/s.rs'] },
      { id: 'b', subject: 'B', description: 'y', filePaths: ['src/s.rs'], depends_on: ['a'] },
    ],
  })).nodes);
  assert.deepEqual(serialized, []);
});

test('a code-changing node that names no files runs alone', () => {
  // It cannot be reasoned about, so it is treated as touching everything.
  const { nodes } = serializeWriters(
    parseTeamDag({
      schema_version: 1,
      nodes: [
        { id: 'wide', subject: 'W', description: 'x', requires_code_change: true },
        { id: 'narrow', subject: 'N', description: 'y', filePaths: ['src/n.rs'] },
      ],
    }).nodes,
  );
  assert.deepEqual(nodes[1].depends_on, ['wide']);
});

test('strict refuses instead of reordering the plan', () => {
  assert.throws(
    () =>
      serializeWriters(
        parseTeamDag({
          schema_version: 1,
          nodes: [
            { id: 'a', subject: 'A', description: 'x', filePaths: ['src/s.rs'] },
            { id: 'b', subject: 'B', description: 'y', filePaths: ['src/s.rs'] },
          ],
        }).nodes,
        { strict: true },
      ),
    /would write the same files/,
  );
});

test('two nodes that share no files run in parallel', () => {
  const { serialized } = serializeWriters(parseTeamDag(structuredClone(DAG)).nodes);
  assert.deepEqual(serialized, []);
});

// ── plan on a fixture repository ─────────────────────────────────────────

function fixtureRepo({ sidecar = true } = {}) {
  const repo = temp();
  const plans = join(repo, '.omx', 'plans');
  mkdirSync(plans, { recursive: true });
  writeFileSync(join(plans, 'prd-widgets.md'), sidecar
    ? '# PRD\n'
    : '# PRD\n\n## Team DAG Handoff\n\n```json\n' + JSON.stringify(DAG) + '\n```\n');
  writeFileSync(join(plans, 'test-spec-widgets.md'), '# spec\n');
  if (sidecar) writeFileSync(join(plans, 'team-dag-widgets.json'), JSON.stringify(DAG));
  return repo;
}

test('plan reads the sidecar, or the fenced block, and writes a run plan', () => {
  for (const sidecar of [true, false]) {
    const repo = fixtureRepo({ sidecar });
    const document = plan(repo);
    assert.equal(document.plan_slug, 'widgets');
    assert.equal(document.nodes.length, 2);
    assert.ok(document.path.endsWith(join('.omx', 'friring', 'team-widgets', 'plan.json')));
    const written = JSON.parse(readFileSync(document.path, 'utf-8'));
    assert.equal(written.schema_version, 1);
  }
});

test('planning with no matching test spec is incomplete, as OMX treats it', () => {
  const repo = temp();
  const plans = join(repo, '.omx', 'plans');
  mkdirSync(plans, { recursive: true });
  writeFileSync(join(plans, 'prd-widgets.md'), '# PRD\n');
  assert.throws(() => plan(repo), /no matching test spec/);
});

test('a branch name carries nothing a shell or a ref would object to', () => {
  // friring runs `git check-ref-format --branch` on this name before it cuts a
  // worktree, so a name that carries `..`, a dotted edge, a `.lock` suffix or an
  // empty component is not an odd branch — it is a refused create.
  const hostile = [
    ['my slug; rm -rf /', 'node/../..'],
    ['a..b', 'a..b'],
    ['..', 'index.lock'],
    ['...', '...'],
    ['-lead-', '.hidden.'],
  ];
  for (const [slug, nodeId] of hostile) {
    const branch = branchFor(slug, nodeId);
    assert.match(branch, /^omx\/[A-Za-z0-9._/-]+$/, branch);
    assert.ok(!branch.includes(' '), branch);
    assert.ok(!branch.includes(';'), branch);
    assert.ok(!branch.includes('..'), `'${branch}' carries a '..' git refuses`);
    const parts = branch.split('/');
    assert.equal(parts.length, 3, branch);
    for (const part of parts) {
      assert.notEqual(part, '', `'${branch}' has an empty component`);
      assert.ok(!part.startsWith('.'), `'${branch}' has a component starting with '.'`);
      assert.ok(!part.endsWith('.'), `'${branch}' has a component ending with '.'`);
      assert.ok(!part.endsWith('.lock'), `'${branch}' has a component ending with '.lock'`);
      assert.ok(!part.startsWith('-'), `'${branch}' has a component starting with '-'`);
    }
  }
});

test('two distinct node ids never land on one branch', () => {
  // Every rule that makes a name git-legal is many-to-one on its own — strip a
  // `.lock`, collapse a `..`, replace an empty component — so sanitizing alone
  // would put two nodes in one worktree. friring refuses a `create` whose
  // worktree already exists, so the second node would fail for a reason its plan
  // gives no hint of.
  const colliding = [
    ['foo.lock', 'foo'],
    ['a..b', 'a.b'],
    ['...', ''],
    ['.hidden.', 'hidden'],
    ['x/y', 'x-y'],
  ];
  for (const [left, right] of colliding) {
    assert.notEqual(
      branchFor('plan', left),
      branchFor('plan', right),
      `'${left}' and '${right}' collided`,
    );
  }
  // An id that needs no cleaning is left exactly as it is — the disambiguator
  // must not make every branch unreadable.
  assert.equal(branchFor('widgets', 'api-layer'), 'omx/widgets/api-layer');
  // …and it is stable, so a re-run of one plan names the same branches.
  assert.equal(branchFor('a..b', 'x/y'), branchFor('a..b', 'x/y'));
});

test('a plan whose nodes share one branch is refused, naming both nodes', () => {
  // Keeping an already-legal id verbatim is what makes a clean plan readable,
  // and it is also the one way two ids still meet: `x/y` sanitizes to
  // `x-y-<digest of 'x/y'>`, which some other node can spell literally. Only a
  // whole-plan check sees that, so it is checked over the plan, not per name.
  const twin = branchFor('widgets', 'x/y').split('/').at(-1);
  assert.equal(branchFor('widgets', twin), branchFor('widgets', 'x/y'));
  const dag = {
    schema_version: 1,
    plan_slug: 'widgets',
    nodes: [
      { id: 'x/y', subject: 'X', description: 'do x', filePaths: ['src/x.rs'] },
      { id: twin, subject: 'T', description: 'do t', filePaths: ['src/t.rs'] },
    ],
  };
  const repo = temp();
  const plans = join(repo, '.omx', 'plans');
  mkdirSync(plans, { recursive: true });
  writeFileSync(join(plans, 'prd-widgets.md'), '# PRD\n');
  writeFileSync(join(plans, 'test-spec-widgets.md'), '# spec\n');
  writeFileSync(join(plans, 'team-dag-widgets.json'), JSON.stringify(dag));
  assert.throws(() => plan(repo), /both resolve to branch/);
  assert.throws(() => plan(repo), new RegExp(`'x/y'`));
  assert.throws(() => plan(repo), new RegExp(`'${twin}'`));
  // A hand-edited plan.json cannot slip past `plan` straight into `run`.
  const planFile = planPath(repo, 'widgets');
  mkdirSync(dirname(planFile), { recursive: true });
  writeFileSync(
    planFile,
    JSON.stringify({ schema_version: 1, plan_slug: 'widgets', nodes: dag.nodes, serialized: [] }),
  );
  assert.throws(() => readPlan(repo, 'widgets'), /both resolve to branch/);
});

test('an ordinary plan gives every node its own branch', () => {
  const document = plan(fixtureRepo());
  const branches = document.nodes.map((node) => branchFor(document.plan_slug, node.id));
  assert.equal(new Set(branches).size, branches.length);
});

// ── The bridge client ────────────────────────────────────────────────────

test('a bridge answer is validated before it is believed', () => {
  assert.throws(
    () => bridge('status', [], spy({ 'friring-cli bridge': { status: 0, stdout: 'not json' } })),
    /did not print JSON/,
  );
  assert.throws(
    () =>
      bridge('status', [], spy({
        'friring-cli bridge': { status: 0, stdout: JSON.stringify({ protocol: 99, ok: true }) },
      })),
    /protocol 1 and friring answered 99/,
  );
  assert.throws(
    () =>
      bridge('status', [], spy({
        'friring-cli bridge': { status: 0, stdout: JSON.stringify({ protocol: 1 }) },
      })),
    /no 'ok' field/,
  );
});

// ── integrate ────────────────────────────────────────────────────────────

/** A `status` answer with one child per node, scripted. */
function statusFor(children) {
  return (program, argv) => {
    if (argv[1] === 'status') return ok({ session: {}, children });
    return ok({});
  };
}

test('only what friring verified is merged', () => {
  const repo = fixtureRepo();
  plan(repo);
  const children = [
    {
      id: 'c1',
      state: 'done',
      result: { branch: branchFor('widgets', 'a'), head: 'aaa', dirty: false, ahead_of_base: 1 },
    },
    {
      id: 'c2',
      state: 'dirty',
      result: { branch: branchFor('widgets', 'b'), head: 'bbb', dirty: true, ahead_of_base: 1 },
    },
  ];
  const spawn = spy({
    'friring-cli bridge': statusFor(children),
    git: (program, argv) => {
      if (argv.includes('rev-parse')) return { status: 0, stdout: 'aaa\n', stderr: '' };
      return { status: 0, stdout: '', stderr: '' };
    },
  });
  const outcome = integrate(repo, 'widgets', { spawn });
  assert.deepEqual(outcome.merged.map((m) => m.node), ['a']);
  assert.equal(outcome.skipped.length, 1);
  assert.match(outcome.skipped[0].reason, /dirty/);
  assert.equal(outcome.conflict, null);
  // Every git call is an argv array.
  for (const call of spawn.calls.filter((c) => c.program === 'git')) {
    assert.ok(Array.isArray(call.argv));
    assert.equal(call.options.shell, false);
  }
});

test('a branch that moved since friring verified it is not merged', () => {
  const repo = fixtureRepo();
  plan(repo);
  const spawn = spy({
    'friring-cli bridge': statusFor([
      {
        id: 'c1',
        state: 'done',
        result: { branch: branchFor('widgets', 'a'), head: 'aaa', dirty: false, ahead_of_base: 1 },
      },
    ]),
    git: { status: 0, stdout: 'zzz\n', stderr: '' },
  });
  const outcome = integrate(repo, 'widgets', { spawn });
  assert.deepEqual(outcome.merged, []);
  assert.match(outcome.skipped[0].reason, /has moved since friring verified it/);
});

test('a conflict is reported and left exactly as git left it', () => {
  const repo = fixtureRepo();
  plan(repo);
  const spawn = spy({
    'friring-cli bridge': statusFor([
      {
        id: 'c1',
        state: 'done',
        result: { branch: branchFor('widgets', 'a'), head: 'aaa', dirty: false, ahead_of_base: 1 },
      },
    ]),
    git: (program, argv) => {
      if (argv.includes('rev-parse')) return { status: 0, stdout: 'aaa\n', stderr: '' };
      return { status: 1, stdout: 'CONFLICT (content): src/a.rs', stderr: '' };
    },
  });
  const outcome = integrate(repo, 'widgets', { spawn });
  assert.equal(outcome.conflict.node, 'a');
  assert.match(outcome.conflict.detail, /CONFLICT/);
  // No `git merge --abort`, no `-X ours`: nobody resolved anything.
  const resolving = spawn.calls.filter(
    (c) => c.program === 'git' && (c.argv.includes('--abort') || c.argv.some((a) => a.startsWith('-X'))),
  );
  assert.deepEqual(resolving, []);
});

// ── evidence ─────────────────────────────────────────────────────────────

test('evidence names the goal and carries only the host verdict', () => {
  const repo = fixtureRepo();
  plan(repo);
  mkdirSync(join(repo, '.omx', 'ultragoal'), { recursive: true });
  writeFileSync(
    join(repo, '.omx', 'ultragoal', 'goals.json'),
    JSON.stringify({ goals: [{ id: 'goal-7', status: 'in_progress' }] }),
  );
  const spawn = spy({
    'friring-cli bridge': statusFor([
      {
        id: 'c1',
        state: 'done',
        result: { branch: branchFor('widgets', 'a'), head: 'aaa', dirty: false, ahead_of_base: 2 },
        last_report: { summary: 'I did great work', child_authored: true },
      },
    ]),
  });
  const document = evidence(repo, 'widgets', {
    spawn,
    verification: [{ command: 'cargo test', outcome: 'passed' }],
  });
  assert.equal(document.goal_id, 'goal-7');
  assert.ok(document.goals_path.endsWith(join('.omx', 'ultragoal', 'goals.json')));
  const text = readFileSync(document.textPath, 'utf-8');
  assert.match(text, /goal: goal-7/);
  assert.match(text, /cargo test -> passed/);
  assert.match(text, /a \[done\].*head=aaa.*ahead=2/);
  // A worker's own words are not evidence, so they are not in it.
  assert.ok(!text.includes('I did great work'), text);
  assert.ok(!JSON.stringify(document).includes('I did great work'));
});

test('evidence without a goal ledger says so rather than inventing one', () => {
  const repo = fixtureRepo();
  plan(repo);
  const spawn = spy({ 'friring-cli bridge': statusFor([]) });
  const document = evidence(repo, 'widgets', { spawn });
  assert.equal(document.goal_id, null);
  assert.match(renderEvidence(document), /goal: \(none recorded\)/);
});

test('the active goal is the unfinished one, or the last recorded', () => {
  const dir = temp();
  const path = join(dir, 'goals.json');
  writeFileSync(path, JSON.stringify([{ id: 'g1', status: 'completed' }, { id: 'g2' }]));
  assert.equal(readActiveGoal(path).id, 'g2');
  writeFileSync(path, JSON.stringify([{ id: 'g1', status: 'completed' }]));
  assert.equal(readActiveGoal(path).id, 'g1');
  assert.equal(readActiveGoal(join(dir, 'absent.json')), null);
});

// ── The manifest and the wrappers ────────────────────────────────────────

test('every pinned role prompt is the real file in the pinned oh-my-codex release', (t) => {
  // The pins and the manifest agreeing with each other proves only that they
  // were copied from the same place. This asks the *release*: an unpacked
  // oh-my-codex 0.21.0 tree. A pin that drifted makes its `file-digest` gate in
  // the manifest unsatisfiable, and the operator sees a refused install with no
  // way to tell whether their tree or friring's pins are wrong.
  //
  // **Role prompts only.** `omx setup` copies `prompts/*.md` verbatim, so the
  // release file is what lands at `~/.codex/prompts/<role>.md` and is therefore
  // what the gate can be pinned to. A *skill* is not: see the test below.
  //
  // Skipped when the source is not unpacked, which it is not on a CI runner:
  // point OMX_SOURCE_DIR at a checkout to run it.
  const source = process.env.OMX_SOURCE_DIR;
  if (!source || !existsSync(join(source, 'prompts'))) {
    // The runner's own skip, not a `return`: a returned test is reported as a
    // pass, so a run with no fixture would say it checked the pins.
    t.skip('set OMX_SOURCE_DIR to an unpacked oh-my-codex tree to run this');
    return;
  }
  const pins = readPins(EXTENSION_HOME);
  const checked = [];
  for (const [role, digest] of Object.entries(pins.role_prompts)) {
    const path = join(source, 'prompts', `${role}.md`);
    assert.ok(existsSync(path), `the release has no prompts/${role}.md`);
    assert.equal(digestOf(path), digest, `prompts/${role}.md drifted`);
    checked.push(role);
  }
  assert.ok(checked.length >= 17, `only ${checked.length} role prompts were checked`);
});

test('every pinned skill digest is what `omx setup` actually installs', (t) => {
  // A skill is **not** installed verbatim. `omx setup` rewrites the frontmatter
  // `description` of each SKILL.md it installs, prefixing `[OMX] ` and quoting
  // the value, so the file at `~/.codex/skills/<name>/SKILL.md` differs from the
  // release's copy of it for every skill whose description did not already carry
  // the prefix — 7 of the 8 gated here, against oh-my-codex 0.21.0.
  //
  // Pinning those to the release file is the shape of bug this whole extension
  // keeps producing: it reads correctly, it passes a lint, and the operator's
  // install is refused. The gate is on what the operator *has*, so that is what
  // the pin has to be, and this is the only thing that can check it. The rewrite
  // is deterministic — two setups into two fresh homes produce identical files —
  // and idempotent, which is why `autopilot` (already prefixed upstream) is
  // unchanged.
  //
  // Point OMX_CODEX_HOME at a `CODEX_HOME` a real `omx setup --scope user
  // --install-mode legacy` produced. `docs/DEVELOPMENT.md` has the disposable
  // fixture recipe; nothing here may run against an operator's own `~/.codex`.
  const installed = process.env.OMX_CODEX_HOME;
  if (!installed || !existsSync(join(installed, 'skills'))) {
    t.skip('set OMX_CODEX_HOME to a CODEX_HOME an `omx setup` produced');
    return;
  }
  const pins = readPins(EXTENSION_HOME);
  const checked = [];
  for (const [name, digest] of Object.entries(pins.skills)) {
    const path = join(installed, 'skills', name, 'SKILL.md');
    // Absent rather than different is the `--install-mode plugin` case, where
    // skills land under `~/.codex/plugins/` and this tree does not exist at all.
    assert.ok(existsSync(path), `no skills/${name}/SKILL.md — was setup run in legacy mode?`);
    assert.equal(digestOf(path), digest, `skills/${name}/SKILL.md drifted`);
    checked.push(name);
  }
  assert.ok(checked.length >= 8, `only ${checked.length} skills were checked`);
});

test('the pins are the digests the manifest gates on', () => {
  const pins = readPins(EXTENSION_HOME);
  assert.equal(pins.omx_version, '0.21.0');
  // Path **paired with** digest, not two independent substring searches:
  // permuting two `sha256` values between blocks leaves every `includes` check
  // true while the shipped extension refuses to install against the pinned
  // files.
  const manifest = readFileSync(join(EXTENSION_HOME, 'extension.toml'), 'utf-8');
  const gated = new Map();
  for (const block of manifest.split('[[requires]]')) {
    if (!/kind\s*=\s*"file-digest"/.test(block)) continue;
    const path = block.match(/path\s*=\s*"([^"]+)"/);
    const sha = block.match(/sha256\s*=\s*"([0-9a-f]{64})"/);
    assert.ok(path && sha, `a file-digest block is missing a path or a digest: ${block}`);
    gated.set(path[1], sha[1]);
  }
  assert.ok(gated.size > 0, 'the manifest gates on no digests at all');

  for (const [name, digest] of Object.entries(pins.skills)) {
    assert.equal(
      gated.get(`~/.codex/skills/${name}/SKILL.md`),
      digest,
      `the manifest gates skills/${name} on a different digest than the pins carry`,
    );
  }
  for (const [role, digest] of Object.entries(pins.role_prompts)) {
    assert.equal(
      gated.get(`~/.codex/prompts/${role}.md`),
      digest,
      `the manifest gates prompts/${role}.md on a different digest than the pins carry`,
    );
  }
});

test('the manifest ships two external files and patches nothing', () => {
  const manifest = readFileSync(join(EXTENSION_HOME, 'extension.toml'), 'utf-8');
  const external = manifest.match(/\[\[external_files\]\]/g) ?? [];
  assert.equal(external.length, 2, 'only the two skill cards leave the extension home');
  assert.ok(manifest.includes('on_conflict = "refuse"'));
  // An agent patch or a config merge would edit a file friring shares with
  // every other extension; this one has no reason to.
  assert.ok(!manifest.includes('[[agent_patches]]'));
  assert.ok(!manifest.includes('[[config_merges]]'));
});

test('each wrapper is one exec line and nothing else', () => {
  for (const [name, sub] of [['omx-leader', 'leader'], ['omx-worker-codex', 'worker']]) {
    const text = readFileSync(join(EXTENSION_HOME, 'bin', name), 'utf-8');
    const lines = text.trim().split('\n');
    assert.equal(lines.length, 2, `${name} is a shebang and one line: ${text}`);
    assert.equal(lines[0], '#!/bin/sh');
    assert.equal(
      lines[1],
      `exec node "\${0%/*}/../lib/friring-omx.mjs" ${sub} "$@"`,
      `${name} execs the program with its subcommand first, resolving it with a ` +
        'shell parameter expansion rather than a `dirname` the boundary would have to allow',
    );
  }
});

test('the appendix carries the marker the routing test asserts', () => {
  const appendix = readFileSync(join(EXTENSION_HOME, 'leader', 'APPENDIX.md'), 'utf-8');
  assert.match(appendix, /friring-omx-appendix v1/);
  assert.match(appendix, /\$friring-team/);
  assert.match(appendix, /omx team/);
  // The digest is stable, so the e2e routing assertion can key on the file.
  assert.match(digestOf(join(EXTENSION_HOME, 'leader', 'APPENDIX.md')), /^[0-9a-f]{64}$/);
});


// ── The run loop ─────────────────────────────────────────────────────────

/** The `pins.json` shape `run` needs, without reading the real file. */
function runPins() {
  return {
    schema_version: 1,
    omx_version: '0.21.0',
    omx_commit: 'x'.repeat(40),
    node_min_major: 20,
    skills: {},
    role_prompts: {},
  };
}

test('run drains the leader inbox and stops for a child that needs a person', async () => {
  const repo = fixtureRepo();
  plan(repo);
  let pass = 0;
  let claimed = false;
  const spawn = spy({
    'friring-cli bridge': (_program, argv) => {
      const verb = argv[1];
      if (verb === 'create') return ok({ child_id: `child-${pass}` });
      if (verb === 'inbox') {
        // Claimed, so it is delivered once — which is the property being
        // asserted, not an artefact of the stub.
        if (claimed) return ok({ messages: [] });
        claimed = true;
        return ok({ messages: [{ id: 1, from: 'child-0', kind: 'blocked', body: 'which schema?' }] });
      }
      if (verb === 'status') {
        pass += 1;
        // The first child asks a question and stays there.
        return ok({ children: [{ id: 'child-0', state: 'blocked' }, { id: 'child-1', state: 'working' }] });
      }
      return ok({});
    },
  });

  const outcome = await run(repo, 'widgets', {
    spawn,
    sleep: async () => {},
    now: () => 0,
    pins: runPins(),
  });

  // It did not wait out the six-hour timer: `blocked` returned immediately.
  assert.equal(outcome.needs_operator.length, 1);
  assert.equal(outcome.needs_operator[0].state, 'blocked');
  // …and the question reached the caller rather than expiring in the mailbox.
  assert.equal(outcome.mail.length, 1);
  assert.equal(outcome.mail[0].kind, 'blocked');
  assert.equal(outcome.mail[0].body, 'which schema?');
  // The inbox was **claimed**, so the same message is not reprinted every pass.
  const inbox = spawn.calls.filter((c) => c.argv[1] === 'inbox');
  assert.ok(inbox.length > 0, 'the run loop never drained the inbox');
  assert.ok(inbox.every((c) => c.argv.includes('--claim')));
});

test('run reports every node when the fan-out finishes on its own', async () => {
  const repo = fixtureRepo();
  plan(repo);
  // One child id per node, keyed off the branch the create asked for: with a
  // single shared id nothing here could tell two children from one, which is
  // exactly what a fan-out has to get right.
  const created = [];
  const spawn = spy({
    'friring-cli bridge': (_program, argv) => {
      const verb = argv[1];
      if (verb === 'create') {
        const node = argv[argv.indexOf('--branch') + 1].split('/').pop();
        created.push(node);
        return ok({ child_id: `child-${node}` });
      }
      if (verb === 'inbox') return ok({ messages: [] });
      if (verb === 'status') {
        return ok({ children: created.map((node) => ({ id: `child-${node}`, state: 'done' })) });
      }
      return ok({});
    },
  });
  // A **pinned** role prompt, so the create path runs the digest check rather
  // than skipping it on an empty `role_prompts`.
  const role = codexHomeWithPrompt('executor', '# Executor\n\nOwn the files listed.\n');
  const outcome = await run(repo, 'widgets', {
    spawn,
    sleep: async () => {},
    now: () => 0,
    codexHome: role.home,
    pins: { ...runPins(), role_prompts: { executor: role.digest } },
  });

  assert.equal(outcome.needs_operator.length, 0);
  // Two creates, one per node, and the dependent one second.
  assert.equal(spawn.calls.filter((c) => c.argv[1] === 'create').length, 2);
  assert.deepEqual(created, ['a', 'b']);
  // Each node carries its *own* child, and both are done.
  assert.deepEqual(
    outcome.nodes.map((n) => [n.id, n.child, n.state]),
    [
      ['a', 'child-a', 'done'],
      ['b', 'child-b', 'done'],
    ],
  );
  // …and the verified prompt is what reached the worker, not a placeholder.
  for (const call of spawn.calls.filter((c) => c.argv[1] === 'create')) {
    const body = call.argv[call.argv.indexOf('--task-body') + 1];
    assert.match(body, /Own the files listed\./);
  }
});

/** A `~/.codex` stand-in holding one role prompt, and that prompt's text. */
function codexHomeWithPrompt(role, text) {
  const home = temp();
  mkdirSync(join(home, 'prompts'), { recursive: true });
  const path = join(home, 'prompts', `${role}.md`);
  writeFileSync(path, text);
  return { home, path, digest: digestOf(path) };
}

test('a role prompt is read only when it matches the digest it is pinned to', () => {
  // The whole point of the function: unpinned prompt text must not become a
  // worker's instructions.
  const text = '# Executor\n\nDo the work described below.\n';
  const { home, digest } = codexHomeWithPrompt('executor', text);
  const node = { id: 'a', role: 'executor' };

  assert.equal(rolePromptFor(node, { role_prompts: { executor: digest } }, home), text);

  assert.throws(
    () => rolePromptFor(node, { role_prompts: { executor: 'f'.repeat(64) } }, home),
    (error) => error instanceof Refusal && /digest/.test(error.message),
    'a prompt whose digest moved was handed to a worker anyway',
  );

  // A role the pins say nothing about has nothing to verify, so there is no
  // prompt rather than an unverified one.
  assert.equal(rolePromptFor(node, { role_prompts: {} }, home), null);
  // The role a node without one takes.
  assert.equal(rolePromptFor({ id: 'a' }, { role_prompts: { executor: digest } }, home), text);
});

test('a pinned role prompt that is not installed is refused by path', () => {
  const home = temp();
  assert.throws(
    () =>
      rolePromptFor(
        { id: 'a', role: 'executor' },
        { role_prompts: { executor: 'a'.repeat(64) } },
        home,
      ),
    (error) =>
      error instanceof Refusal && error.message.includes(join(home, 'prompts', 'executor.md')),
    'a missing prompt was not named',
  );
});

test('a fan-out that never finishes is refused at the run deadline', async () => {
  // Every other `run` test pins the clock at 0, so the deadline branch is never
  // evaluated by any of them: a broken or removed six-hour limit would leave the
  // suite green while a stuck fan-out polled forever. The clock answers 0 once —
  // when the deadline is computed — and past six hours on every later call.
  const repo = fixtureRepo();
  plan(repo);
  const spawn = spy({
    'friring-cli bridge': (_program, argv) => {
      const verb = argv[1];
      if (verb === 'create') return ok({ child_id: 'child-a' });
      if (verb === 'inbox') return ok({ messages: [] });
      // Never terminal and never operator-held, so only the deadline ends it.
      if (verb === 'status') return ok({ children: [{ id: 'child-a', state: 'working' }] });
      return ok({});
    },
  });
  let asked = 0;
  const now = () => (asked++ === 0 ? 0 : 7 * 60 * 60 * 1000);

  await assert.rejects(
    () => run(repo, 'widgets', { spawn, sleep: async () => {}, now, pins: runPins() }),
    (error) => {
      assert.ok(error instanceof Refusal, `not a Refusal: ${error}`);
      assert.match(error.message, /time limit/);
      return true;
    },
  );
});

test('a stalled child returns the run to its operator rather than being polled out', async () => {
  // `stalled` is set from friring's nudge counter and nothing clears it on its
  // own — `mirror_child_states` mirrors only ready/working/blocked, and a
  // stalled child is resumable, meaning a person resumes or stops it. Polling
  // one for six hours is the same as never reporting it.
  const repo = fixtureRepo();
  plan(repo);
  let slept = 0;
  const spawn = spy({
    'friring-cli bridge': (_program, argv) => {
      const verb = argv[1];
      if (verb === 'create') return ok({ child_id: 'child-a' });
      if (verb === 'inbox') return ok({ messages: [] });
      if (verb === 'status') return ok({ children: [{ id: 'child-a', state: 'stalled' }] });
      return ok({});
    },
  });

  const outcome = await run(repo, 'widgets', {
    spawn,
    sleep: async () => {
      slept += 1;
    },
    now: () => 0,
    pins: runPins(),
  });

  assert.deepEqual(
    outcome.needs_operator.map((n) => [n.id, n.state]),
    [['a', 'stalled']],
  );
  // Returned on the pass that saw it, not slept on.
  assert.equal(slept, 1, `the run slept ${slept} times over a stalled child`);
});

test('a create refused for capacity is offered again once a slot frees', async () => {
  // Four independent nodes against a profile that allows three live children —
  // the shipped `max_children`. Nothing tells a plan author to keep a fan-out
  // narrower than that, so the fourth node must wait for a slot rather than
  // being retired, taking everything downstream of it with it.
  const repo = fixtureRepo();
  writeFileSync(
    join(repo, '.omx', 'plans', 'team-dag-widgets.json'),
    JSON.stringify({
      schema_version: 1,
      plan_slug: 'widgets',
      nodes: ['a', 'b', 'c', 'd'].map((id) => ({
        id,
        subject: id.toUpperCase(),
        description: `do ${id}`,
        filePaths: [`src/${id}.rs`],
      })),
    }),
  );
  plan(repo);

  const children = new Map();
  let retired = false;
  const spawn = spy({
    'friring-cli bridge': (_program, argv) => {
      const verb = argv[1];
      if (verb === 'create') {
        const node = argv[argv.indexOf('--branch') + 1].split('/').pop();
        if ([...children.values()].filter((s) => s !== 'done').length >= 3) {
          return refusal('fanout_exhausted', 'this session already has 3 live children');
        }
        children.set(`child-${node}`, 'working');
        return ok({ child_id: `child-${node}` });
      }
      if (verb === 'inbox') return ok({ messages: [] });
      if (verb === 'status') {
        // The first status pass retires one child, which frees its slot.
        if (retired) for (const id of children.keys()) children.set(id, 'done');
        else children.set('child-a', 'done');
        retired = true;
        return ok({ children: [...children].map(([id, state]) => ({ id, state })) });
      }
      return ok({});
    },
  });

  const outcome = await run(repo, 'widgets', {
    spawn,
    sleep: async () => {},
    now: () => 0,
    pins: runPins(),
  });

  const fourth = outcome.nodes.find((n) => n.id === 'd');
  assert.equal(fourth.state, 'done', `the fourth node ended ${fourth.state}: ${fourth.error}`);
  assert.equal(fourth.child, 'child-d', 'the fourth node was never created');
  assert.deepEqual(outcome.nodes.filter((n) => n.state === 'refused'), []);
  // It really was refused once and offered again, rather than the cap never
  // biting: two creates went out for the same node.
  const forD = spawn.calls.filter(
    (c) => c.argv[1] === 'create' && c.argv.includes('omx/widgets/d'),
  );
  assert.equal(forD.length, 2, 'the fourth node was not retried after its refusal');
});

test('capacity held by children this run does not own is reported, not polled out', async () => {
  // The fan-out cap is per **owner**, and a leader's own state map sees only the
  // children *this* run made. So a slot held by a sibling run — or by a child
  // stuck in a state nothing clears without a person — is invisible here, and
  // retrying it to the six-hour deadline is the same as never answering.
  const repo = fixtureRepo();
  plan(repo);
  const spawn = spy({
    'friring-cli bridge': (_program, argv) => {
      const verb = argv[1];
      // Refused every single time, and nothing else about the run ever moves.
      if (verb === 'create') return refusal('fanout_exhausted', 'this session has 3 live children');
      if (verb === 'inbox') return ok({ messages: [] });
      if (verb === 'status') return ok({ children: [] });
      return ok({});
    },
  });

  let slept = 0;
  const outcome = await run(repo, 'widgets', {
    spawn,
    sleep: async () => {
      slept += 1;
    },
    // A clock that never advances, so only the capacity bound can end this run.
    // If it were the run deadline that stopped it, `run` would have thrown.
    now: () => 0,
    pins: runPins(),
  });

  const node = outcome.nodes.find((n) => n.id === 'a');
  assert.equal(node.state, 'blocked', `the node ended ${node.state}`);
  assert.match(node.error, /friring-cli bridge status/);
  assert.deepEqual(
    outcome.needs_operator.map((n) => n.id),
    ['a'],
    'a capacity block must return the run to its operator',
  );
  assert.ok(
    outcome.needs_operator[0].error,
    'the operator was told the node is blocked without being told why',
  );
  // Bounded, and by the counter rather than by the clock: one poll short of the
  // cap plus the pass that trips it.
  assert.ok(slept < 100, `the run slept ${slept} times before answering`);
});

test('a create waits at least as long as the host may take to answer it', async () => {
  // friring answers `create` asynchronously: one blocking saga step is bounded
  // at 600s and the readiness gate adds 60s. `friring-cli`'s own default is
  // 120s, so without this flag the client gives up on a create that is still
  // running — and the worktree it already cut refuses the next attempt.
  const repo = fixtureRepo();
  plan(repo);
  const spawn = spy({
    'friring-cli bridge': (_program, argv) => {
      const verb = argv[1];
      if (verb === 'create') return ok({ child_id: 'child-a' });
      if (verb === 'inbox') return ok({ messages: [] });
      if (verb === 'status') return ok({ children: [{ id: 'child-a', state: 'done' }] });
      return ok({});
    },
  });
  await run(repo, 'widgets', { spawn, sleep: async () => {}, now: () => 0, pins: runPins() });

  const creates = spawn.calls.filter((c) => c.argv[1] === 'create');
  assert.ok(creates.length > 0, 'the run loop never created a child');
  for (const call of creates) {
    const at = call.argv.indexOf('--timeout');
    assert.notEqual(at, -1, 'a create carried no --timeout');
    assert.ok(Number(call.argv[at + 1]) >= HOST_CREATE_CEILING_SECS, call.argv[at + 1]);
  }
  assert.ok(CREATE_TIMEOUT_SECS >= HOST_CREATE_CEILING_SECS, String(CREATE_TIMEOUT_SECS));
});

/**
 * The host's own worst case for one `create`, from the constants in
 * `src/app/bridge_saga.rs` and `src/app/bridge_spawn.rs`: **two** blocking steps
 * at `BLOCKING_STEP_TIMEOUT` (the worktree checkout and the gated spawn), plus
 * `EGRESS_ACK_TIMEOUT` and `READY_TIMEOUT`. Written out rather than as one
 * number so a change to any of them is visible here.
 */
const HOST_CREATE_CEILING_SECS = 600 + 600 + 15 + 60;

test('two node ids that differ only in case are refused at plan time', () => {
  // The collision that matters is between two worktree *directories*, and the
  // default macOS filesystem is case-insensitive — so `foo` and `Foo` are two
  // branches git will make and one directory friring would put both children
  // in. Caught at plan time or not at all: by fan-out a worktree is cut.
  const repo = temp();
  const plans = join(repo, '.omx', 'plans');
  mkdirSync(plans, { recursive: true });
  const dag = {
    schema_version: 1,
    nodes: [
      { id: 'Api', subject: 'a', description: 'a' },
      { id: 'api', subject: 'b', description: 'b' },
    ],
  };
  writeFileSync(join(plans, 'prd-widgets.md'), '# PRD\n');
  writeFileSync(join(plans, 'test-spec-widgets.md'), '# spec\n');
  writeFileSync(join(plans, 'team-dag-widgets.json'), JSON.stringify(dag));
  assert.throws(() => plan(repo), /both resolve to branch/);
});

test('a Node older than this program is written for is refused by number', () => {
  // The manifest's `tool-version` pattern is a substring and cannot express
  // "20 or newer", so the exact bound lives here.
  assert.throws(() => checkNodeVersion('18.19.0'), Refusal);
  assert.throws(() => checkNodeVersion('2.5.0'), Refusal);
  checkNodeVersion(`${MIN_NODE_MAJOR}.0.0`);
  checkNodeVersion('24.1.0');
});
