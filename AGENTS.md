# Friring

Repository guide for coding agents. This is the **always-loaded** brief —
Claude Code reads it through the `CLAUDE.md` symlink, other agents read it as
`AGENTS.md`. Keep it lean: deep detail lives in `docs/` (and `FORK.md`) and is
read **on demand** — see [Where to read](#where-to-read). Don't paste subsystem
walkthroughs back into this file; add them to the relevant `docs/` file so this
brief stays short enough to load every turn.

## Project

Friring is a multi-session coding-agent TUI orchestrator built with Rust. It
runs multiple coding-agent CLI instances (Claude Code, Codex, Antigravity,
opencode, aider, … — any CLI you define) inside persistent tmux sessions,
rendered as terminal panels via ratatui + tui-term. Sessions survive
crashes/restarts because tmux keeps the processes alive.

Each session picks **which agent** to run from a declarative registry
(`~/.config/friring/agents.toml`). Friring is agent-neutral: it knows nothing
about any agent's model, permissions, prompts, or tools — only how to launch the
CLI with the right `command + args`.

## Fork (Friring)

This repository is **Friring**, a personal fork of
[Thurbox](https://github.com/Thurbeen/thurbox) (`Thurbeen/thurbox`). See
[`FORK.md`](FORK.md) for the full story and the running list of divergences.

Two rules matter when working here:

- **The plumbing is renamed to `friring` (July 2026).** The binaries
  (`friring`/`friring-cli`), the crate, config dir (`~/.config/friring`), data
  dir + DB (`~/.local/share/friring/friring.db`), tmux socket (`-L friring`),
  and `FRIRING_*` env vars are all `friring` now. So is the fork's **own
  distribution** (August 2026): `cd.yml` releases `friring-*` binaries,
  `scripts/install.*` and `HomebrewFormula/friring.rb` (this repo doubles as its
  own brew tap) fetch them from `bvc3at/friring`, and self-update /
  version-check query the same repo. What still says `thurbox` is deliberate:
  upstream **attribution** (`LICENSE`, provenance, the website/Sonar badges) and
  upstream-owned surfaces — `pages.yml`, `website/`, upstream extension
  payloads, the `min_thurbox_version` manifest key, and the `tb-`/`tbs-` tmux
  window prefixes.
  Upstream merges now carry rename conflicts; resolve them toward `friring` for
  this app's own identifiers and distribution, leaving the attribution names as
  upstream. Upstream's AUR/Chocolatey/winget manifests were deleted here, so
  merges touching them conflict as delete/modify — keep them deleted.
- **Log every divergence in [`FORK.md`](FORK.md).** Whenever a change makes this
  fork behave differently from upstream (a new feature, a changed default, a
  guarded workflow), add a bullet under its "Differences from upstream" section
  in the *same* change. `FORK.md` is the single place that tracks what differs.

## Commands

```bash
just build                           # build friring + friring-cli
just test                            # cargo nextest run --all
just lint                            # fmt-check + clippy + deny + rumdl + shellcheck
cargo nextest run -E 'test(name)'    # run a single test by name
cargo check --all                    # type check (bare cargo still works)

cargo fmt --all                      # format (rustfmt, 100-col max)
cargo clippy --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features   # docs (CI gates)
rumdl check .                        # markdown lint (.rumdl.toml)
cargo test --test architecture_rules # module-boundary enforcement (below)
```

Full dev environment, the isolated sandbox (`scripts/dev/sandbox.sh`), live
mode (`just dev-live` — the dev build against your **real** sessions), the e2e
harnesses, and demo recording live in **`docs/DEVELOPMENT.md`**.

## Architecture (do not break)

The app is The Elm Architecture: `Event → Message → update(model, msg) →
view(model) → Frame`. Module dependencies are an **enforced allowlist**
(`tests/architecture_rules.rs` — a new module fails the test until it declares
what it may reference):

```text
session  ← pure data types, no crate-internal references
agent    ← session (+ paths/shell utils; NEVER ui, git, app)
ui       ← session + app model/view state (+ fuzzy/paths; NEVER agent or git)
app      ← coordinator, imports all modules
```

`ui → app` is the TEA `view(model)` coupling (ui renders app-owned state, never
triggers side effects); `session_ops` and `cli` may reach `crate::agent::…` via
fully-qualified paths only (never `use`). Module responsibilities, the event
loop, and every ADR are in **`docs/ARCHITECTURE.md`**.

Key facts:

- MSRV 1.75, Edition 2021; async runtime tokio (multi-threaded).
- Session backend `TmuxBackend` over a `TmuxTransport` (local `tmux -L friring`,
  or `ssh <dest> tmux …` / `wsl.exe …` for remote hosts). Requires tmux ≥ 3.2.
- Output read in `spawn_blocking`, parsed by `vt100::Parser`, rendered by
  `tui_term`; input written via mpsc.
- State in SQLite `~/.local/share/friring/friring.db` (`XDG_DATA_HOME`
  respected); agents in `~/.config/friring/agents.toml`, hosts in `hosts.toml`.

## Every change

- **Conventional Commits** (enforced by cocogitto). Types: `feat`, `fix`,
  `perf`, `refactor`, `docs`, `style`, `test`, `chore`, `ci`, `build`, `revert`.
  Scopes: `api`, `cli`, `ui`, `git`, `core`, `docs`, `deps`, `config`, `mcp`,
  `fork` (fork-specific: FORK.md, migration, upstream divergences), and
  `review` (the code-review subsystem).
  Use `cog commit feat "message"` (or `… fix "message" scope`).
- **Comments earn their tokens** — a redundant or wrong comment makes agents
  *less* accurate.
  - *Why, not what*: explain rationale, tradeoffs, constraints, invariants the
    code can't show; never restate what the code plainly does.
  - *Accuracy is non-negotiable*: a stale comment is worse than none. When you
    touch code, fix or delete the comments around it.
  - *Keep* design rationale, cross-refs (`see fn_x`, `mirrors Y`), and
    `ADR-*` / `schema vNN` anchors. *Cut* restatements and obvious labels.
  - *Doc comments* (`///`/`//!`) are the public contract — don't delete an
    intra-doc link (`` [`Item`] ``) or a ``` ``` ``` example without re-running
    `cargo doc` (CI fails on a broken link/example).
  - **No `TODO`/`FIXME`/`HACK` markers and no commented-out code** — track work
    in issues, delete dead code.
- **Keep docs current.** If a change invalidates or extends a documented
  decision, update the relevant `docs/` file (and `FORK.md` for a divergence)
  in the *same* change.

## Where to read

Detail is read on demand — jump to the doc for what you're touching:

| When you're working on… | Read |
|---|---|
| Build, test, the dev sandbox, e2e harnesses, demos, pre-commit hooks | `docs/DEVELOPMENT.md` |
| Module boundaries, backends, the TEA event loop, or any ADR | `docs/ARCHITECTURE.md` |
| Core principles / non-negotiable invariants | `docs/CONSTITUTION.md` |
| Any config file (agents / hosts / settings / themes / keybindings), env var, or DB setting | `docs/CONFIG.md` |
| A user-facing feature — sessions, code review, automations, tasks, global search, notifications, status, remote/WSL, extensions, keybindings | `docs/FEATURES.md` |
| Render-loop performance, perf counters, redraw throttling | `docs/PERFORMANCE.md` |
| The headless CLI (`friring-cli`) | `docs/CLI.md` |
| Real-agent e2e tests, the model stub, scenario-driven demos | `docs/E2E.md` |
| Cutting a release, versioning, installers, packaging | `docs/RELEASING.md` |
| What this fork changes vs upstream, and fork-only features (e.g. the F9 activity view, conversation import) | `FORK.md` |
