# Documentation

Friring's guidance is split between a lean, always-loaded brief and detailed
topic docs read on demand:

- **[`AGENTS.md`](../AGENTS.md)** (repo root; Claude Code reads it through the
  `CLAUDE.md` symlink) — the always-loaded brief: what the project is, the
  essential commands, the architecture rules you must not break, the per-change
  conventions, and a router to everything below. It is loaded every turn, so it
  is kept short.
- **`docs/*`** (this directory) — the full topic references, both operational
  ("how it works") and decisional ("why X over Y"). These are **not** loaded
  every turn; an agent reads the one relevant to its task (the `AGENTS.md`
  router says which).
- **[`FORK.md`](../FORK.md)** — what this fork changes versus upstream, and the
  fork-only features.

## Documents

| Document | Purpose | Update when… |
|---|---|---|
| [CONSTITUTION.md](CONSTITUTION.md) | Core principles / non-negotiable invariants | Adding/removing an enforced invariant |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Architecture decisions, module layout, event loop | Changing a technology or structural pattern |
| [FEATURES.md](FEATURES.md) | Feature-level design + behavior | Altering a feature, keybinding, lifecycle, layout, or UX |
| [CONFIG.md](CONFIG.md) | Every config file / env var / DB setting | Adding/changing a config file, env var, or DB setting |
| [DEVELOPMENT.md](DEVELOPMENT.md) | Build, test, sandbox, e2e harnesses, demos | Changing the dev/test workflow or tooling |
| [PERFORMANCE.md](PERFORMANCE.md) | Render/tick performance + how to measure | Touching the render loop or a perf optimization |
| [CLI.md](CLI.md) | The headless `friring-cli` surface | Adding/changing a CLI subcommand or flag |
| [RELEASING.md](RELEASING.md) | Release automation, versioning, installers, packaging | Changing the release/packaging pipeline |

## Keeping docs current

**Rule**: if a code change invalidates or extends a documented decision, update
the relevant doc in the *same* change.

- Keep **`AGENTS.md`** lean — commands, the architecture allowlist, per-change
  conventions, and the router. Don't grow it with subsystem walkthroughs.
- Put topic detail (operational **and** decisional) in the matching `docs/`
  file, so it loads only when an agent is working on that topic.
- Record any **divergence from upstream** in [`FORK.md`](../FORK.md).
