---
name: friring-autopilot
description: One-command autonomous delivery under friring — deep interview, plan, goal, then Team execution as sandboxed bridge children.
---

<!-- Managed by friring `extension install` — edits are overwritten; remove this line to take ownership. -->

# friring-autopilot

The deterministic entry point for a session running under friring's
orchestration bridge. It is the native `$autopilot` lane with one substitution:
**`$friring-team` is the only lane engine**, because native Team mode needs a
tmux pane layout this boundary does not have.

## The lane

Run these in order, and do not skip one because the brief looks small:

1. **`$deep-interview <brief>`** — until the specification stops changing.
2. **`$ralplan`** — produce the PRD and the matching test spec under
   `.omx/plans/`, including the `Team DAG Handoff` block (or its
   `team-dag-<slug>.json` sidecar).
3. **`$ultragoal`** — record the goal and its acceptance in
   `.omx/ultragoal/goals.json`.
4. **`$friring-team`** — `plan`, `run`, `integrate`, `evidence`.
5. **`$ultraqa`** and **`$code-review`** on the integrated result, in this
   session. They are not worker nodes: they read the merged tree, which does not
   exist until step 4 finishes.

## Before you report the goal complete

Run `$friring-team` step 4 and quote `evidence.txt`. It names the active goal
id, the goals file, every verification command you ran with its outcome, and per
node the branch, head and outcome **friring** recorded after it stopped that
worker's pane.

A node friring did not record as `done` is not done. A worker's own summary is
not evidence of anything, and neither is a green test run in a worktree that was
never merged.

## What is unavailable here

`$team` and `omx team` refuse in this session, deliberately. So does anything
that assumes a tmux pane layout around the leader. If you find yourself reaching
for one, the answer is `$friring-team`.
