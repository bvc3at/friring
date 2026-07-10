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
project *name* is rebranded, in `README.md` and the agent brief (`AGENTS.md`).

## Differences from upstream

### Features

#### Claude Code activity view (F9)

*The first Friring feature — merged into `main` (#1).*

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

Implementation notes (this is a fork-only feature, so its detail lives here
rather than in `docs/FEATURES.md`):

- **Two data homes.** `SessionInfo.cc_activity` is a lightweight **index**
  (workflows + agents + standalone subagents; ids, agentType, state, mtimes — no
  transcript bodies), refreshed off the UI thread ~1 s per local session,
  mtime-signature-gated, and **never persisted** (a high-churn DB field would
  bump `PRAGMA data_version`). `App::cc_activities` is the **open-view** UI state
  (rows, selection, scroll, wrap, folds); the selected agent's transcript is
  parsed **on demand** and re-read on growth for live-tail.
- **Path resolution.** The dir is found by scanning `projects/*/` for the
  `<agent_session_id>/subagents` child (`paths::claude_projects_dir`), not by
  computing Claude Code's slug (which replaces `/`, `.`, and likely all non-alnum
  with `-`). `agent_session_id` is what thurbox injects as `THURBOX_SESSION_ID`;
  `$CLAUDE_CONFIG_DIR` is honored.
- **Surface & keys.** A side tree in the file-viewer column
  (`InputFocus::CcActivityTree`) folds workflows over their agents; a central
  transcript pane (`InputFocus::CcActivity`) shows assistant thinking / text /
  foldable `tool_use` + `tool_result`, or a workflow overview (phases + per-agent
  grid + logs, plus a background run's live pace / what it's blocked on). Keys
  mirror the code-review view (`j`/`k`, PageUp/Down, `Ctrl+D`/`U`, `g`/`G`, `w`
  wrap, `Left`/`Right` h-scroll, `/` find). Mutually exclusive with the
  code-review overlay.
- **Code shape.** Pure data + defensive parsers in `session::cc_activity` (arch
  rule `ui ← session`); the off-thread scan + view state + key handlers in
  `app::cc_activity`; the renderer in `ui::cc_activity` (reuses `focus_block` /
  `scrollbar` / theme). Parsing is isolated in one module because the Claude Code
  on-disk layout is undocumented and version-specific (verified against v2.1.201–
  2.1.204), degrading to a partial tree rather than an error.
- **Follow-ups** (named, not silently dropped): markdown rendering of
  thinking/text (blocked on `ui::markdown` not being width-aware); async parse of
  very large transcripts; parsing an in-process run's workflow `scripts/*.js` for
  live phase names; baking the session id into per-session hook commands so a
  daemon worker also reports `working`/`blocked`/`done` **status**; and remote
  (`ssh:`/`wsl:`) support. **Done since v1:** daemon-worker attribution + live
  overview, per-session `--settings` for exact attribution, find-in-transcript.

### Documentation / branding

- `README.md` and the agent guide prose call the project **Friring** (the
  binary, URLs, install commands, and packaging are unchanged and still say
  `thurbox`).
- A fork notice at the top of `README.md` explains the fork, the name, and that
  all links intentionally point upstream.
- **Agent-guide layout.** Upstream keeps one large `CLAUDE.md`. On the fork the
  always-loaded brief is a lean **`AGENTS.md`** (root) with **`CLAUDE.md` a
  symlink** to it, and the former monolith's detail was moved into on-demand
  `docs/` topic files (two new ones added: `docs/CLI.md`, `docs/RELEASING.md`).
  This keeps per-turn context small and makes the guide agent-neutral (Codex and
  others read `AGENTS.md`). Thin path-scoped Claude Code rules live in
  `.claude/rules/` (rust/shell/markdown/website), each loaded only when a
  matching file is edited.

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
  set up for the fork at the moment. The `changes` (paths-filter) job also grants
  `pull-requests: read`, which a **private** repo's default token lacks (public
  upstream doesn't need it).
