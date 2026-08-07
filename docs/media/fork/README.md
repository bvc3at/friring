# Fork-feature clips

Clips of features this fork adds or changes, recorded by the real-agent e2e
harness (`scripts/dev/agent-e2e/run.sh --demo <scenario>`) rather than by
`scripts/demo`. Each one is linked from the doc it illustrates — nothing here
is unreferenced, and a clip that stops being referenced should be deleted
rather than left to rot.

| Clip | Illustrates | Linked from |
|---|---|---|
| `claude-ghost-fleet.gif` | Lazy sessions & ghosts at fleet scale — 4 real claude trees frozen one by one, `Σ` falling to nothing, the info panel pricing each tree, one loaded back | `docs/FEATURES.md` · `README.md` · website |
| `claude-activity-view.gif` | The F9 agent-activity view — Overview dashboard, turn-grouped Timeline, a real 3-agent workflow | `docs/FEATURES.md` · `README.md` · website |
| `scripted-leader-key.gif` | The tmux-style leader key and its which-key overlay | `docs/FEATURES.md` · website |
| `claude-named-workspace.gif` | Named multi-repo workspace dir (`Ctrl+O` on the wizard's name step) | `FORK.md` |
| `claude-review-loop.gif` | Code review v2's structured handoff — a classified comment reaching a real agent | `FORK.md` |
| `claude-text-turn.gif` · `opencode-text-turn.gif` | The harness itself: one scenario description, two agents, two wire dialects | `docs/E2E.md` |

The three carrying an `.mp4` beside the gif are the ones the website plays;
`pages.yml` copies those into `website/assets/` at deploy time alongside
`docs/media/*.mp4`.

Each file is named for the scenario that produced it, so any of them can be
rebuilt from source:

```bash
just agent-demo claude-ghost-fleet     # -> target/agent-e2e/demos/
```

Recorded the same way the shipped `docs/media` clips are — asciinema captures
the TUI's terminal byte stream, agg renders it offline — but against loopback
stubs on a throwaway `HOME`/`XDG_*` under `/tmp`, so no account, token, real
conversation or real path is on camera. Every clip is themed `doom`, and the
panes talk to models that do not exist (`fable-67`, `gpt-6.2`,
`tempest-oss-140b`) about infrastructure nobody has yet — see `docs/E2E.md`
§ Demo mode.

## Why this directory sits outside the pacing gate

CI's `demo-pacing` job globs `docs/media/*.gif`, which does not descend here,
and that is deliberate rather than incidental. The budget it enforces
(`scripts/demo/lib/check-pacing.mjs`) is calibrated for tapes that film a
seeded TUI with no agent in the loop, where a held frame can only be the demo
waiting on itself. These clips film real CLIs booting, thinking and answering,
so a held frame is usually the app being honestly slow at the moment the
scenario came to film. Every clip here busts the 1.0s max-hold; the shipped ten
do not, and should keep having to prove it.

The recorder still runs the check and prints the numbers, so a clip that stalls
for a reason other than the agent is visible when it is recorded.

## Recorded but not shipped

The full set covers more scenarios than the six features above; the rest were
recorded, reviewed and left out. They are one `just agent-demo <scenario>` away
if a doc ever needs them: `claude-lineage` (fork lineage with both branches
continuing apart), `claude-unload-load` (the ghost lifecycle on one session),
`claude-restart-resume`, `scripted-global-search`, `scripted-wizard-backnav`,
`scripted-wizard-worktree`, `scripted-automation-fire`,
`scripted-extension-tasks`, `claude-tool-loop`.

`codex-text-turn` is the one scenario with no recording at all: it passes as a
test but its recording stalls waiting for codex's `›` ready glyph on a pane
that never paints. Not the seeded tmux.conf, not machine load, not a leaked
sandbox server — each was ruled out — so it needs a look of its own.
