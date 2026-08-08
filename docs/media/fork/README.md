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

## The pacing gate here

These clips are gated, on their own profile. CI's `demo-pacing` job runs
`check-pacing.mjs --profile=agent` over this directory in a second step, and
the recorder prints the same profile when it records, so what you see while
recording is what CI will enforce.

The profile relaxes **only** the two metrics real agent latency explains, and
relaxes them to a measured ceiling rather than switching them off:

| Metric | Shipped clips | Here | Why |
|---|---|---|---|
| Held frame | 1.0s | 3.0s | The worst held frame across these seven is 0.68–2.04s, and every one is a CLI booting or answering. 3.0s still catches the class of stall the budget exists to kill (the pre-`Wait` audit found 3.81s). |
| Opening hold | 0.75s | 2.5s | Not an agent-latency allowance — see below. |
| **Size (10MB)** | gated | **gated** | A gif GitHub refuses to render is not excused by anything. Two of these are embedded in `README.md`. |
| **Blank final frame** | gated | **gated** | A leaked recorder teardown is perfectly well-paced, so this is the only check that sees it. It caught one clip in this very batch. |

The opening cap is the one worth being explicit about, because it is **not**
relaxed for the "the agent is slow" reason. An agent booting shows the viewer an
empty pane, which nothing excuses; the recorder's `SCENARIO_DEMO_PREROLL` keeps
that boot off camera instead. The cap is higher than the default because of the
recorder's own filmed floor (~1.7s of asciinema attach, settle poll and node
startup) plus the prompt being typed, which `freezedetect` cannot tell from a
hold. The six clean openings measure 0.20–2.02s against that floor; the
blank-pane defect this metric caught measured 2.93–3.54s. 2.5s is the line
between them.

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
