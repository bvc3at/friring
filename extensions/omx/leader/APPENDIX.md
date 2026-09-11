# friring session appendix

<!-- friring-omx-appendix v1 -->

## Team execution in this session

This session runs under friring's orchestration bridge. Team work is carried out
by **`$friring-team`**, and by nothing else.

- `$team` and `omx team` are **unavailable here**. Native Team mode drives a tmux
  pane layout from inside the leader's own terminal; this session's pane is
  friring's, and `TMUX` is deliberately absent from it, so native Team refuses
  with `Team mode requires running inside tmux current leader pane`. Do not try
  to work around that — the refusal is the boundary working.
- Each Team node runs as a **friring bridge child**: its own session, its own
  sandbox, its own git worktree and branch, and its own private Codex state
  directory. A worker cannot read this session's transcripts, another worker's
  transcripts, or anything under this session's control directories.
- You never start, stop or inspect a worker directly. `$friring-team` does it
  through `friring-cli bridge`, which is the only channel out of this boundary.

## What counts as done

A worker's own report is **not** evidence. friring stops each worker's pane and
then reads its worktree itself — branch, head, whether it is dirty, how far ahead
of the base it is — and that verdict is what `$friring-team` integrates from.

- Only a node friring recorded as `done` is merged.
- A node whose worktree was dirty is never merged, whatever the worker said
  about it. Ask for a `resume` if the work is worth committing, or stop it.
- A merge conflict is reported and left exactly as git left it. Resolve it
  yourself, in this session, or say so and stop.

## Checkpoints

Before you report a goal complete, run `$friring-team evidence`. It writes
`evidence.json` and `evidence.txt` naming the active goal id, the goals file,
the verification commands you ran with their outcomes, and per node the branch,
head and outcome **friring** recorded. Quote that file; do not paraphrase a
worker.
