# Fork-feature tapes — review drop

One clip per fork-only feature, recorded by the real-agent e2e harness
(`scripts/dev/agent-e2e/run.sh --demo <scenario>`), staged here to be looked at
and **curated**, not to ship as-is. Nothing in `docs/` links these yet: the
shipped media stays `docs/media/*.gif`, which is what the `demo-pacing` CI job
gates. Whatever survives review moves up a directory and gets linked from the
doc it illustrates; the rest goes.

Each file is named for the scenario that produced it, so any clip can be
re-recorded from source:

```bash
just agent-demo claude-activity-view     # -> target/agent-e2e/demos/
```

Recorded against loopback stubs on a throwaway `HOME`/`XDG_*`, so no account,
token or real conversation is on camera — see `docs/E2E.md` § Demo mode.

| Clip | Feature |
|---|---|
| `claude-unload-load.gif` | Lazy sessions & ghosts — unload to a frozen frame, load back through `--resume` |
| `claude-activity-view.gif` | The F9 agent-activity view — Overview dashboard and turn-grouped Timeline |
| `claude-review-loop.gif` | Code review v2 — classified comment, structured handoff to the agent |
| `claude-fork.gif` | Session fork via `<leader> f`, parent linkage and sidebar nesting |
| `claude-named-workspace.gif` | Named multi-repo workspace dir (`Ctrl+O` on the name step) |
| `claude-restart-resume.gif` | Restart resumes the same conversation |
| `scripted-leader-key.gif` | The tmux-style leader key and its which-key overlay |
| `scripted-global-search.gif` | The centered global-search popup across every scope |
| `scripted-wizard-backnav.gif` | New-session wizard — always-type palette, path mode, `Esc` back-navigation |
| `scripted-wizard-worktree.gif` | Worktree flow with the type-to-filter base-branch selector |
| `scripted-automation-fire.gif` | Automations firing from both the TUI tick and the headless path |
| `scripted-extension-tasks.gif` | Extension lifecycle feeding the tasks panel |
| `claude-text-turn.gif` | Baseline: a real Claude Code turn through a Friring pane |
| `claude-tool-loop.gif` | Baseline: a real tool-use loop (`Write`) round-tripping through the stub |
| `codex-text-turn.gif` | Baseline: codex, proving the registry is agent-neutral |
| `opencode-text-turn.gif` | Baseline: opencode, same |
