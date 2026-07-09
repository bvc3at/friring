# Friring — a fork of Thurbox

**Friring** is a personal, opinionated fork of
[Thurbox](https://github.com/Thurbeen/thurbox) by Thurbeen. This document
describes what Friring is and how it differs from upstream Thurbox.

## The name

Thurbox's `thur-` reads as *Thursday*; Friring bumps it a day to *Friday*
(`fri-`), and a **ring** is a boxing arena. So Friring is *the ring where the
agents fight* — an arena for agents — or, read another way, *free agents in a
safe ring*.

## Relationship to upstream

- **Upstream:** [`Thurbeen/thurbox`](https://github.com/Thurbeen/thurbox)
- **This fork:** `bvc3at/friring`

Friring exists to land a few opinionated features on top of Thurbox. Some may be
contributed back to upstream over time; conversely, upstream's own improvements
are merged down into Friring as they land. The original commit history is kept
intact as a tribute to the upstream author.

## Why it still carries the Thurbox name

Almost everything here still uses the upstream name on purpose. The binary
(`thurbox` / `thurbox-cli`), the config and data dirs (`~/.config/thurbox`,
`~/.local/share/thurbox`), the tmux socket (`tmux -L thurbox`), the `THURBOX_*`
env vars, and the crate keep their original names, and every install command,
badge, package, and link points at `Thurbeen/thurbox`. Friring is a *branding*
layer over the upstream binary — it publishes no releases, packages, or website
of its own, so installing it installs upstream Thurbox. Keeping the functional
identifiers identical also lets Friring stay drop-in compatible with an existing
Thurbox install and easy to keep in sync with upstream. Only the human-facing
project *name* is rebranded, in `README.md` and `CLAUDE.md`.

## Differences from upstream

### Features

#### Claude Code activity view (F9)

*The first Friring feature — currently on the `feature/cc-workflows-view`
branch, pending merge into `main`.*

A native central-pane view (**F9**, gated by a `[features] cc_activity` flag)
that shows a live + historical **tree of a Claude Code session's workflows and
Task subagents**, plus each one's transcript (thinking / tool calls / output) —
live-tailed while running, browsable when finished. Claude + local sessions only
(v1).

- **Data source.** Reads Claude Code's on-disk JSONL under
  `~/.claude/projects/*/…/subagents/` (undocumented and version-specific;
  `$CLAUDE_CONFIG_DIR` honored) via an off-thread, mtime-gated scan that is never
  persisted.
- **Daemon-worker attribution.** Unions a session's own subagents tree with
  trees written by a background/daemon worker it launched, attributed via the
  `--settings` flag the daemon replays, so daemon-dispatched workflows are
  surfaced instead of showing an empty tab. In-flight background runs get a live
  overview from the worker's job state.
- **Find-in-transcript (`/`).** Incremental find with in-place match
  highlighting, mirroring the code-review / file-viewer find.

### Documentation / branding

- `README.md` and `CLAUDE.md` prose call the project **Friring** (the binary,
  URLs, install commands, and packaging are unchanged and still say `thurbox`).
- A fork notice at the top of `README.md` explains the fork, the name, and that
  all links intentionally point upstream.

### CI / automation

Some upstream workflows target infrastructure the fork doesn't have, so they are
guarded to run only on the canonical `Thurbeen/thurbox` repo and stay dormant
here (while remaining merge-safe). All build / test / lint jobs run normally on
the fork.

- `.github/workflows/pages.yml` (GitHub Pages) — dormant; the fork has no Pages
  site.
- `.github/workflows/cd.yml` (Release) — dormant; the fork does not cut its own
  releases.
- `.github/workflows/ci.yml` — the `sonarqube` job is dormant; SonarQube is not
  set up for the fork at the moment.
