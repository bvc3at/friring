---
name: friring-team
description: Run an approved Team DAG as friring bridge children — one sandboxed worker per node, host-verified results, ordered integration.
---

<!-- Managed by friring `extension install` — edits are overwritten; remove this line to take ownership. -->

# friring-team

Team execution for a session running under friring's orchestration bridge. Each
node of the approved Team DAG becomes a friring **bridge child**: its own
session, sandbox, git worktree and branch, and its own private Codex state.

Native `$team` and `omx team` are unavailable here — see the session appendix.

## Use it

Run the phases in order. Each is one command; each prints JSON you can read.

```sh
# 1. Turn the approved Team DAG handoff into a run plan.
node {home}/lib/friring-omx.mjs plan <repo-root>

# 2. Create a child per ready node and drive them to terminal states.
node {home}/lib/friring-omx.mjs run <repo-root> <plan-slug>

# 3. Merge the branches friring verified, in plan order.
node {home}/lib/friring-omx.mjs integrate <repo-root> <plan-slug>

# 4. Write the checkpoint artifacts.
node {home}/lib/friring-omx.mjs evidence <repo-root> <plan-slug>
```

`plan` reads `.omx/plans/team-dag-<slug>.json`, or the fenced `Team DAG Handoff`
block in the latest approved PRD. It validates the handoff field by field and
refuses a malformed one naming the field. It then **serializes writers**: two
nodes with no dependency between them whose `filePaths` overlap would be two
workers editing one file in two worktrees, so the later one waits. Pass
`--strict` to have it refuse instead of reordering.

## What comes back

`run` polls `friring-cli bridge status` until every node is terminal. A node is
started only when every node it depends on reached `done`; a dependency that
ended any other way stops that branch, because the work it was going to build on
is not there.

`integrate` merges **only** what friring verified: state `done`, worktree not
dirty, and the branch still at the head friring recorded after it stopped the
pane. Anything else is skipped with the reason named. A conflict is reported and
left exactly as git left it — resolve it yourself.

## Answering a worker

A worker that is stuck sends `blocked` mail. Read your inbox
(`friring-cli bridge inbox --claim --json`), decide, and answer:

```sh
friring-cli bridge send --to <child-id> --kind answer --body '<your answer>'
```

A worker that says it needs a person moves to `blocked` and appears in your
`status`. That is a decision for you, not something to wait out.

## The rules that do not bend

- A worker's own summary is not evidence. friring's verdict is.
- A dirty worktree is never merged, whatever the worker claimed.
- A worker friring could not stop lands in `stop_failed` and is never merged;
  that one needs a person.
