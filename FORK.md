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

## Renamed to friring (July 2026)

Friring began as a pure *branding* layer: only the human-facing name was
rebranded, while every functional identifier kept the upstream `thurbox` name.
As of July 2026 the plumbing is renamed too. The app's own identifiers are now
`friring` — the `friring` / `friring-cli` binaries, the crate, the config dir
(`~/.config/friring`), the data dir and DB
(`~/.local/share/friring/friring.db`), the tmux socket (`tmux -L friring`), and
the `FRIRING_*` env vars.

What still says `thurbox` is deliberate, and splits in two:

- **Upstream attribution** — the repo URLs, badges, `LICENSE`, and provenance
  notes point at [`Thurbeen/thurbox`](https://github.com/Thurbeen/thurbox) and
  stay as-is (this is a fork, and the credit is upstream's).
- **Upstream distribution machinery** — Friring publishes no releases,
  packages, or website of its own, so everything that fetches or ships an
  upstream artifact keeps the upstream name: `packaging/` registry manifests,
  `scripts/install.*`, the `cd.yml` / `pages.yml` workflows, `website/`, the
  self-update / version-check code, the `min_thurbox_version` extension-manifest
  key (a wire format shared with upstream), and the `tb-` / `tbs-` tmux window
  prefixes (brand-neutral, kept for live-window compatibility).

The tradeoff the branding-only approach used to avoid is now real: upstream
merges carry rename conflicts on the renamed identifiers, and an existing
`thurbox` install needs a one-time [migration](#migration).

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
  with `-`). `agent_session_id` is what friring injects as `FRIRING_SESSION_ID`;
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

#### Import an existing Claude Code conversation (`i` in the session list)

*The counterpart to the F9 activity view — adopt conversations Friring didn't
start.*

Upstream Thurbox resumes only sessions it spawned (it pins `agent_session_id`
at launch); a raw `claude` run elsewhere, or one in a pre-existing worktree,
could not be adopted. Pressing **`i`** in the session list (gated by the same
`[features] cc_activity` flag — both features read Claude Code's undocumented
on-disk layout) opens a picker of every conversation on disk, and launching one
creates a normal session that `--resume`s it **in a directory you choose**
(default: the conversation's original cwd). Claude + local sessions only (v1).

- **Browse.** An off-thread one-shot scan lists every top-level
  `~/.claude/projects/*/<uuid>.jsonl` (`$CLAUDE_CONFIG_DIR` honored): title
  (Claude Code's `summary` line if present, else the first typed prompt — meta
  lines like slash-command envelopes are skipped), original cwd, git branch,
  last-active age. Fuzzy search (`/`), newest first. Conversations already
  tracked by a live session are excluded (importing one would race the running
  agent on its own transcript); duplicate ids across project dirs (earlier
  imports) collapse to the newest copy.
- **Pick a directory.** `Enter` moves to a working-directory input prefilled
  with the original cwd (fish-style Tab completion, mirrors the repo picker).
  This is the "resume without cd-ing to the original directory" gap: point an
  old conversation at a fresh worktree.
- **Transcript staging.** `claude --resume <id>` only finds transcripts under
  the *current* directory's project slug (verified v2.1.206), so importing into
  a different directory copies the newest `<id>.jsonl` into
  `projects/<slug-of-destination>/` first. The original file is never touched;
  an existing same-or-newer destination copy is kept (re-importing never rolls
  a continued conversation back). This is the one place friring *computes* a
  slug (`paths::claude_project_slug`, every non-alphanumeric → `-`, destination
  canonicalized first because `claude` slugs its physical cwd) — finding
  existing dirs still scans instead. If a future Claude Code changes the rule,
  the failure is visible (`--resume` errors in the pane), not silent.
- **Spawn.** The session config pins **both** `resume_session_id` (selects the
  `--resume {id}` arg group) and `agent_session_id` (identity: `FRIRING_SESSION_ID`,
  the F9 activity scan, the DB row, later `Ctrl+R` restarts), so an imported
  session behaves exactly like one Friring started. The relaunch agent is the
  registry default when it resumes by id, else the first agent whose
  `resume_args` carry `{id}` (`AgentDef::resumes_by_id`); the agent picker is
  skipped. The session-name modal is prefilled from the conversation title.
- **Code shape.** Pure head-parsing (`parse_conversation_head`) in
  `session::cc_activity` beside the other defensive Claude Code parsers; scan +
  staging + modal state + key handlers in `app::cc_import`; renderer in
  `ui::conversation_picker_modal` (mirrors the repo picker's
  search/list/input/footer shape).
- **Follow-ups** (named, not silently dropped): remote (`ssh:`/`wsl:`) imports
  (scan the remote `~/.claude` and stage over the transport); importing
  conversations of *deleted* (tombstoned) sessions currently re-imports rather
  than restoring; surfacing other agents' conversation stores (codex/opencode)
  if they ever expose stable resume-by-id semantics.

#### Inline info-pane docking (`info_panel_position`)

Upstream's F2 info panel is always a dedicated column (needs ≥120 cols and
costs the terminal ~15% of its width). The fork adds a top-level
`info_panel_position` setting — `auto` (new default) / `column` / `inline` —
that can dock the pane **inline at the bottom of the sidebar** instead, below
the session list and automations pane, costing no terminal width and working
from 80 cols up. `auto` inlines whenever the full session list + automations
pane + full info content fit the sidebar and falls back to the column
otherwise; `column` is exactly the upstream behavior; `inline` forces the
sidebar dock even when the session list must shrink to its minimum. Applies
live (settings panel / file reload), F2 still toggles visibility, and a
tick-side drift check re-pushes PTY sizes when an `auto` flip moves the dock
(a content-driven layout change no resize event covers). Details in
`docs/CONFIG.md` + `docs/FEATURES.md` ("Info panel docking"); the **default
changed** from upstream's always-column to `auto`.

### Behavior fixes

- **Worktree branch pre-fill keeps `/`.** In the new-worktree flow, the branch
  name suggested from the session name upstream drops every char that isn't
  alphanumeric / space / `-` / `_`, so a git-flow style session name like
  `fix/branch-naming` was pre-filled as `fixbranch-naming`. The fork preserves
  `/` as a hierarchy separator (collapsing repeats, absorbing adjacent hyphens,
  trimming at the ends). Everything downstream already handled slash branches —
  the worktree directory flattens `/` to `-` and tmux window names sanitize
  separately (`session_name_to_branch` in `src/app/key_handlers.rs`).

### Performance

- **New-session dialog never blocks on git (ADR-P12).** Upstream's worktree
  flow runs `git fetch origin` + the branch listing synchronously in the key
  handler (a measured 1.8 s+ freeze on a slow remote), and only shows the agent
  picker after every `git worktree add` finished. The fork opens the branch
  selector instantly with an off-thread listing, runs the fetch concurrently
  (worktree creation waits on it off-thread, so worktrees still fork from fresh
  origin refs), overlaps the agent picker with the worktree creation, and moves
  backend readiness + repo-display-name resolution into the async spawn worker.
  See ADR-P12 in `docs/PERFORMANCE.md` for measurements and gates.

### Documentation / branding

- **Renamed the plumbing to `friring` (July 2026).** The app's own identifiers
  flipped from `thurbox` to `friring`: the `friring` / `friring-cli` binaries,
  the crate, `~/.config/friring`, `~/.local/share/friring/friring.db`, the
  `tmux -L friring` socket, and the `FRIRING_*` env vars. What deliberately
  still says `thurbox`: upstream **attribution** (repo URLs, `LICENSE`,
  provenance, badges) and the upstream **distribution machinery** the fork
  reuses rather than republishes — `packaging/` registry manifests,
  `scripts/install.*`, the `cd.yml` / `pages.yml` workflows, `website/`, the
  self-update / version-check code, the `min_thurbox_version` manifest key, and
  the `tb-` / `tbs-` tmux window prefixes. See [Migration](#migration); upstream
  merges now carry rename conflicts on the renamed identifiers.
- `README.md` and the agent-guide prose call the project **Friring**; the repo
  URLs, install commands, badges, and packaging still point at upstream (that's
  attribution and shared distribution, not a rename target).
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

## Migration

Upgrading an existing `thurbox` install to the renamed `friring`? The rename
changed where the app looks, so move your state across once:

- **Config** — copy the config dir:

  ```bash
  cp -r ~/.config/thurbox ~/.config/friring
  ```

- **Data + DB** — copy the data dir, then rename the database file:

  ```bash
  cp -r ~/.local/share/thurbox ~/.local/share/friring
  mv ~/.local/share/friring/thurbox.db ~/.local/share/friring/friring.db
  ```

- **Live sessions** keep running on the old tmux server (the socket name is
  fixed for a running server). Reattach and drain them on the old socket, then
  stop it once nothing is left:

  ```bash
  tmux -L thurbox attach        # drain in-flight sessions
  tmux -L thurbox kill-server   # once none are left running
  ```

  The same applies on remote hosts, and existing sessions' remote
  `~/.local/share/thurbox` worktrees stay where they are — leave them until
  those sessions are retired.

- **Env vars** — rename any `THURBOX_*` you set in shell rc files, agent
  wrappers, or hooks to `FRIRING_*`.

- **Automation units** — reinstall your systemd / launchd units under the new
  `friring` names and disable the old `thurbox` ones.

- **Dev sandbox** — profiles under `target/dev-sandbox/*/thurbox-*` are stale;
  recreate them (see `docs/DEVELOPMENT.md`).

- **Extensions & self-update** — extensions fetched at runtime from upstream
  still invoke `thurbox-cli`; use this repo's local copies instead. The
  self-update / version-check paths still track upstream **Thurbox** releases
  and aren't meaningful for a source-built `friring`.
