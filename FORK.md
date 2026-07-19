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
- **Upstream distribution machinery** — Friring cuts its **own** GitHub Releases
  (`friring-*` binaries via `cd.yml`; see [CI / automation](#ci--automation)),
  but reuses upstream's package-manager channels and website rather than
  republishing them. So everything that fetches or ships an *upstream* artifact
  keeps the upstream name: `packaging/` registry manifests, `scripts/install.*`,
  the `pages.yml` workflow, `website/`, the self-update / version-check code, the
  `min_thurbox_version` extension-manifest key (a wire format shared with
  upstream), and the `tb-` / `tbs-` tmux window prefixes (brand-neutral, kept for
  live-window compatibility).

The tradeoff the branding-only approach used to avoid is now real: upstream
merges carry rename conflicts on the renamed identifiers, and an existing
`thurbox` install needs a one-time [migration](#migration).

## Differences from upstream

### Features

#### Agent activity view (F9)

*The first Friring feature (#1), redesigned in July 2026 into an
agent-neutral retrospective.*

A native central-pane view (**F9**, gated by the `[features] cc_activity`
flag) that reconstructs **what a session's agent did** — every shell command
it ran, file it edited, file it read, web search/fetch it made, and subagent
it delegated to — from whatever the agent CLI persists on disk. Local
sessions only.

- **Section navigator.** The side column lists sections — Overview /
  Timeline / Commands / Files / Web / Agents (`1`–`6` jump) — with live
  counts; the central pane renders the selection. Event rows are compact
  one-liners (`Enter` expands note + result head); the same fold / `/` find /
  wrap / live-tail engine as transcripts.
- **Providers.** Every supported CLI gets a provider: pure record→event
  parsers in `session::activity::<provider>` plus discovery/tailing glue in
  `app::activity::<provider>`, dispatched by the **command basename** of the
  session's `agents.toml` entry (so wrapper entries like `claude-opus`
  resolve). Sources are stat-signature-gated, append-only files tail
  incrementally by byte offset, SQLite stores are read read-only (WAL-aware),
  and nothing is persisted. Formats were reverse-engineered from each CLI's
  source/docs (July 2026) and every parser degrades to skipped records on
  drift. Twelve providers ship: **claude** (main-transcript `tool_use`
  tailing), **codex** (rollout JSONL, `history_mode`-aware), **gemini**,
  **qwen**, **copilot**, **vibe**, **cursor-agent** (JSONL transcripts),
  **opencode**, **goose**, **crush** (SQLite), **aider** (markdown history),
  **cline** (full-rewrite JSON). Each provider honors its CLI's state-dir
  env overrides (`CODEX_HOME`; `GEMINI_CLI_HOME`; `QWEN_HOME` /
  `QWEN_RUNTIME_DIR`; `COPILOT_HOME`; `VIBE_HOME`; `CURSOR_DATA_DIR`;
  `GOOSE_PATH_ROOT`; `CLINE_DIR` / `CLINE_DATA_DIR` /
  `CLINE_SESSION_DATA_DIR`; `AIDER_CHAT_HISTORY_FILE`; `OPENCODE_DB`;
  `XDG_DATA_HOME` for opencode and goose).
- **Known-unsupported agents** show *why* in the Overview (e.g. `agy`
  encrypts its trajectory store; `amp` keeps threads server-side).
- **The Claude workflow/subagent tree** (the original v1 feature) lives on
  under the Agents section: live + historical tree of a session's workflows
  and Task subagents with full transcripts (thinking / tool calls / output),
  daemon-worker attribution via the replayed `--settings` flag, and the live
  overview of in-flight background runs.
- **Find-in-transcript (`/`).** Incremental find with in-place match
  highlighting, mirroring the code-review / file-viewer find.

Implementation notes (this is a fork-only feature, so its detail lives here
rather than in `docs/FEATURES.md`):

- **Three data homes.** `SessionInfo.cc_activity` is the lightweight Claude
  **tree index** (workflows + agents + standalone subagents; ids, agentType,
  state, mtimes — no transcript bodies), refreshed off the UI thread ~1 s per
  local session, mtime-signature-gated, and **never persisted** (a high-churn
  DB field would bump `PRAGMA data_version`). `App::activity` holds each
  session's **normalized event accumulator** (`ActivityEvent` stream + meta),
  filled by a second ~1 s scan (offset half a cadence from the first) whose
  per-session state *moves* into the `spawn_blocking` pass and back.
  `App::cc_activities` is the **open-view** UI state (navigator rows,
  selection, scroll, wrap, folds); the selected agent's transcript is parsed
  **on demand** and re-read on growth for live-tail.
- **Path resolution.** The dir is found by scanning `projects/*/` for the
  `<agent_session_id>/subagents` child (`paths::claude_projects_dir`), not by
  computing Claude Code's slug (which replaces `/`, `.`, and likely all non-alnum
  with `-`). `agent_session_id` is what friring injects as `FRIRING_SESSION_ID`;
  `$CLAUDE_CONFIG_DIR` is honored.
- **Surface & keys.** A side navigator in the file-viewer column
  (`InputFocus::CcActivityTree`): the six sections, with the Claude
  workflow/subagent tree nested under Agents (Space folds a workflow or the
  whole subtree). The central pane (`InputFocus::CcActivity`) shows the
  selected section's event list, an agent transcript (assistant thinking /
  text / foldable `tool_use` + `tool_result`), or a workflow overview
  (phases, per-agent grid, logs, plus a background run's live pace / what
  it's blocked on). Keys mirror the code-review view (`j`/`k`, PageUp/Down,
  `Ctrl+D`/`U`, `g`/`G`, `w` wrap, `Left`/`Right` h-scroll, `/` find) plus
  `1`–`6` section jumps. Mutually exclusive with the code-review overlay.
- **Code shape.** Pure data + defensive parsers in `session::activity`
  (model + one submodule per provider) and `session::cc_activity` (the
  Claude tree; arch rule `ui ← session`); the event scan + provider
  discovery in `app::activity` (one submodule per provider), the tree scan +
  view state + key handlers in `app::cc_activity`; the renderer in
  `ui::cc_activity` (reuses `focus_block` / `scrollbar` / theme). Parsing is
  isolated per provider because every agent's on-disk layout is undocumented
  and version-specific (Claude verified against v2.1.201–2.1.206), degrading
  to partial data rather than an error.
- **Follow-ups** (named, not silently dropped): event streams from Claude
  subagent/daemon-worker transcripts (main transcript only today, sidechain
  lines aside); hook-injected capture for `agy` (encrypted store) and a
  session-keyed reader for `amp`; markdown rendering of thinking/text
  (blocked on `ui::markdown` not being width-aware); async parse of very
  large transcripts; parsing an in-process run's workflow `scripts/*.js` for
  live phase names; baking the session id into per-session hook commands so a
  daemon worker also reports `working`/`blocked`/`done` **status**; and remote
  (`ssh:`/`wsl:`) support. **Done since v1:** daemon-worker attribution + live
  overview, per-session `--settings` for exact attribution, find-in-transcript,
  the July 2026 multi-agent redesign.

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
  `~/.claude/projects/*/<uuid>.jsonl` (`$CLAUDE_CONFIG_DIR` honored): the
  session's *name* when it has one (the newest `custom-title` line from
  `/rename`, else the newest auto-generated `ai-title` line — both appended on
  change, so the scan reads a 64 KiB tail besides the head, the same window
  Claude Code's own resume picker scans, verified v2.1.207), falling back to
  the message-derived title (Claude Code's `summary` line if present, else the
  first typed prompt — meta lines like slash-command envelopes are skipped),
  plus original cwd, git branch,
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
  skipped. The session-name modal is prefilled from the conversation's name,
  else its title.
- **Code shape.** Pure head/tail parsing (`parse_conversation_head`,
  `parse_session_names`) in `session::cc_activity` beside the other defensive
  Claude Code parsers; scan + staging + modal state + key handlers in
  `app::cc_import`; renderer in `ui::conversation_picker_modal` (mirrors the
  repo picker's search/list/input/footer shape).
- **Follow-ups** (named, not silently dropped): remote (`ssh:`/`wsl:`) imports
  (scan the remote `~/.claude` and stage over the transport); importing
  conversations of *deleted* (tombstoned) sessions currently re-imports rather
  than restoring; surfacing other agents' conversation stores (codex/opencode)
  if they ever expose stable resume-by-id semantics.

#### Session name passed to the agent (`{name}` in agents.toml)

Upstream's session name lives only in the Thurbox DB and UI (plus the
sanitized `tb-<name>` tmux window title); the agent's own conversation gets an
auto-generated title. The fork adds a `{name}` placeholder to the
`agents.toml` argument templates — substituted with the friring session name
alongside `{id}` — and the seeded claude entry uses it (`-n {name}` in
`new_session_args` and `fork_args`, verified claude v2.1.207), so a
conversation friring *creates* shows up under the same name in claude's own
`/resume` picker. Resume templates deliberately omit `{name}`: a restart never
renames a conversation the agent already owns (an in-agent `/rename`
survives), and conversation *imports* keep the CC title untouched for the same
reason. A name-less launch drops a `{name}` token together with its preceding
flag (no dangling `-n`); previously-seeded `agents.toml` files keep working
and opt in by adding the flag pair. Claude only for now — codex/agy have no
launch-time naming, opencode's needs its `run -i` entry mode. Details in
`docs/CONFIG.md` ("agents.toml").

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

#### Global search: centered popup + double-`Shift` opener

Upstream's global search (`Ctrl+/`) is a full-width strip docked above the
footer that shrinks the whole content area (resizing every visible session
PTY on open/close). The fork redesigns it after JetBrains' Search
Everywhere:

- **Centered popup.** Floats horizontally centered with its top edge in the
  upper third, overlaying the content — no panel resize, no PTY reflow, and
  the live in-place match highlighting stays visible around it
  (`global_search_popup` in `src/ui/layout.rs`).
- **Double-`Shift` opens it** (in addition to `Ctrl+/`): two bare `Shift`
  taps within ~400 ms with no key between. Requires the kitty keyboard
  protocol — the fork widens the pushed enhancement flags to
  `REPORT_ALL_KEYS_AS_ESCAPE_CODES | REPORT_ALTERNATE_KEYS` (upstream pushes
  only `DISAMBIGUATE_ESCAPE_CODES`) so bare modifier presses are reported at
  all; on legacy terminals the gesture is silently unavailable. Gated by a
  new `[features] double_shift_search` flag (default on).
- **Scope fixes.** Sessions now match on **cwd** (documented upstream but
  not implemented) and on **every** worktree branch, not just the first; the
  Files scope is pinned to the session that was active at open (it used to
  silently follow the live preview's session switches).
- The per-keystroke performance rework is tracked separately under
  *Performance* (ADR-P13).

Details in `docs/FEATURES.md` ("Global Search") + `docs/CONFIG.md`.

#### Sync base picker (`Ctrl+S` with multiple remotes)

Upstream's worktree sync hardcodes `origin`: `git fetch origin`, rebase onto
the `@{upstream}` → `origin/HEAD` → `origin/main` → `origin/master` chain. On
a repo with several remotes (fork + upstream is the common case) there was no
way to sync onto anything else. The fork lists each repo's remotes off-thread
on `Ctrl+S` (the ADR-P12 no-git-on-the-UI-thread discipline) and, **only when
a repo has more than one remote**, opens a picker for the base remote before
the sync threads start. The choice is persisted per repo (`repo_sync_bases`,
schema v40) and preselected on the next sync; a single non-`origin` remote is
pinned automatically instead of failing upstream's hardcoded fetch. Details in
`docs/FEATURES.md` ("Choosing the base remote").

#### Type-to-filter selectors (host / base-branch / agent pickers)

Upstream's new-session picker modals for the run-on host, the worktree base
branch, and the coding agent are `j`/`k` + `Enter` selection lists — fine at a
handful of rows, tedious once a repo has many branches or the registry many
agents. The fork makes all three **fuzzy-filterable as you type**: a printable
key builds a subsequence query over the row label (`ma` → `main`, `cl` →
`claude`), matched characters are accent-highlighted, and the cursor snaps to
the first match.

- **Keymap shifts** because printable keys now type: navigation moves to
  `↑`/`↓` (and `Ctrl+N`/`Ctrl+P`); `j`/`k` no longer navigate these three
  modals. `Backspace` narrows the query; `Esc` clears an active query first and
  only closes the modal once it is empty (the footer's secondary button reads
  `Clear` while filtering).
- **Shared plumbing.** A new `fuzzy::FuzzyFilter` holds the query + matching row
  indices and remaps the selection cursor across edits (kept in *filtered* row
  space); the highlight/line/query-row rendering is factored into shared
  `ui::` helpers (`fuzzy_highlighted_spans`, `selector_line_filtered`,
  `render_filter_row`, `render_filter_selector_footer`) that the repo and
  conversation pickers' existing highlighter now also route through. The match
  is the same greedy scan already used elsewhere — microseconds, never a frame
  block — and the base-branch query survives the background branch load
  (ADR-P12), applying the instant the list lands. Details in `docs/FEATURES.md`
  ("Type-to-filter selectors").

#### Real-agent e2e harness & scenario demos (`scripts/dev/agent-e2e/`)

Hermetic, offline end-to-end tests that run a **real agent binary** (Claude
Code is the proven reference) inside a Friring-managed pane with the **model
API stubbed on loopback** — zero-dep node sidecars speaking each wire dialect
from hand-curated semantic fixtures. One scenario description runs both as an
asserting bats test (`just agent-e2e`; three drive depths: the agent's own
print/exec mode → bare-tmux interactive → full Friring TUI) and as a VHS demo
recording (`just agent-demo <scenario>`). Ships with a path-gated,
**non-blocking** `agent-e2e` CI job that installs a pinned claude binary, and
one small CLI addition: `session get/list --json` now expose the raw
`hook_state`/`hook_state_at` columns so external observers (the harness,
automations) can watch status transitions without reading SQLite. Architecture
and contracts in `docs/E2E.md`; decision record ADR-23.

Coverage is **multi-agent**, one stub per wire dialect rather than per agent:
`claude` (anthropic dialect) plus `codex` and `opencode` (a shared `openai`
dialect — Responses and Chat Completions). `antigravity` (`agy`) is declared
**unstubbable**: it forces real Google OAuth before any model traffic, with no
API-key or base-URL escape, so its scenarios refuse to run offline instead of
faking a login. A missing *or unresponsive* agent binary skips only that
agent's tests, so any subset of the CLIs stays green.

#### Stub-driven demo recordings (`scripts/demo/`)

The demo media are recorded against those same loopback stubs instead of real,
logged-in agent accounts. Every pane shows a **scripted conversation** —
pre-played through `friring-cli session send` before recording — sourced from
one file, `scripts/demo/demo-content.json`, which also seeds the sample repo,
the review branch's diff, the tasks/automation and the search query. This
makes the demos deterministic (a re-record diffs cleanly instead of capturing
whatever a live model said) and identity-free (every agent talks to
`127.0.0.1`, so no account email, token or usage can reach the frame), and it
lets the panes show *fictional future* model ids (`fable-67`, `gpt-6.x`, …).
`antigravity` is featured logged-out, being unstubbable. The info panel's
Claude account-usage gauges are stubbed the same way: the fork's
`FRIRING_CLAUDE_USAGE_URL` env override (`docs/CONFIG.md`) points the fetch at
the anthropic stub's `/api/oauth/usage` route, fed with scripted numbers from
`demo-content.json` — otherwise every clip films "not logged in". Details in
`docs/DEVELOPMENT.md` § Demo video.

#### Terminal-first focus

Upstream starts focused on the session list, and clicking a session row
focuses the *list* — so the first thing typed after startup or after a
click lands in the list's single-letter hotkeys (`i` opens the import
picker, `Shift+S` re-sorts) instead of reaching the agent. The fork makes
the terminal the default focus target: startup lands in the terminal when
any session was restored, clicking a session row selects it **and**
focuses the terminal (matching `Enter` / a notification click / a
global-search jump), and `Esc` backs out of a focused session list. The
list stays reachable for management (reorder, import) via `Ctrl+H` or a
click on its empty area. See `docs/FEATURES.md` ("Focus model:
terminal-first").

#### Attention navigation (`F10` + blocked badges)

Upstream surfaces a blocked agent only as a red dot (and a desktop
notification) — there is no way to *navigate* by attention. The fork adds
`F10` (rebindable `NextBlockedSession`): jump to the next `Blocked`
session in rendered order (wrapping), focus landing in the terminal, so
repeated presses walk the attention queue and answer each prompt in turn.
The blocked count is badged in the session list's title bar (`◆N` ahead
of the status dots) and in the footer (`◆ N blocked · F10`, with the live
shortcut), so attention is visible even when the sidebar is hidden on a
narrow terminal. See `docs/FEATURES.md` ("Live status & needs attention").

#### Quick session switching (last-session toggle & numbered jumps)

`Ctrl+6` / `Ctrl+^` (rebindable `LastSession`) bounces between the two
most recent sessions — tmux `last-window`, vim's alternate buffer. Every
deliberate switch records the session it left (`Ctrl+J`/`K`, list `j`/`k`,
clicks, jumps, a committed global-search result, spawn/undelete);
bookkeeping moves (restore reshuffles, delete clamps, search
live-previews) don't, so the toggle always means "where I actually was".

`Alt+1`–`9` jumps to the Nth session in rendered order (tmux
`Alt+digit`), and **holding Alt paints the numbers** on the session list
so the target is visible before the digit is pressed. `Alt+A` (rebindable
`JumpToBlocked`) is the attention variant: it numbers only the *blocked*
sessions and a digit jumps among those. This is a deliberate, narrow Alt
exception to upstream's "Ctrl = global, everything else = PTY" philosophy
(documented in `docs/FEATURES.md`); every other Alt chord still forwards
to the agent. The hold-to-peek overlay needs the kitty keyboard protocol:
on top of the modifier-reporting flags the double-`Shift` opener already
pushes (see *Global search* above), the fork adds `REPORT_EVENT_TYPES` so
Alt's *release* — and key auto-repeat — are reported, with repeats
(`Repeat` kind) dispatched like `Press` so held keys keep repeating into
the PTY. Legacy terminals lose only the visual overlay: `Alt+digit` /
`Alt+A` still work, the latter as a sticky overlay dismissed by a digit,
`Esc`, or any other key.

#### New-session wizard redesign (palette picker, back-navigation, prefills)

Upstream's repo picker is a three-focus-zone modal (list / path input / a
separate `/` search bar) where `Tab` completes *or* moves focus depending on
whether a ghost suggestion happens to exist, `Enter` with nothing checked
silently starts a session in `$HOME`, and every step's `Esc` throws the whole
flow away. The fork rebuilds the flow:

- **Always-type palette.** One focused input; typing fuzzy-filters the
  recency-sorted bookmarks, typing a path (`~`, `/`, `./`, `../`) switches the
  list to live directory candidates (git repos marked, local per-keystroke,
  remote only on the explicit `Tab` listing). `Tab` only ever completes. Row
  actions move off typed keys: `Space` (input empty) / `Ctrl+Space` pick,
  `Ctrl+T` worktree (was `w`), `Del` forgets (was `d`), `Ctrl+P` unchanged.
  `Enter` opens the highlighted repo directly, confirms the picked set, opens
  a repo candidate, drills into plain directories, or adds + opens a typed
  path in one step. Typed local paths are validated to exist (remote already
  was). Selection is keyed by path, so it survives filtering and re-scans.
- **Explicit no-repo + first-run help.** The silent `$HOME` fallthrough became
  a pinned `start in ~` row; a first run with zero bookmarks offers one-key
  imports of common project folders (`~/code`, `~/src`, …).
- **Esc steps back** through the whole wizard with state preserved (the
  palette returns exactly as left; branch load + origin fetch re-dispatch per
  ADR-P12). First step cancels; the agent picker with a worktree create in
  flight and a fork stay full cancels.
- **Wizard chrome + name prefill.** Every step is titled `New Session — <step>`
  (fork/import variants say so), the name/branch/agent steps show a muted
  breadcrumb of accumulated choices, and the session name is prefilled from
  the repo basename (deduped `-2`, `-3`, … against existing sessions) so the
  common case is Enter-through. The base-branch and agent steps keep their
  upstream type-to-filter selectors.
- **Optional named workspace dir (`Ctrl+O` on the name step).** Upstream
  always builds a multi-repo session's symlink workspace at
  `workspaces/<agent_session_id>` (a UUID). For a multi-repo **local** spawn
  the fork's name step gains a hidden-by-default second field (`Ctrl+O`
  shows/hides, `Tab` switches focus): a bare name puts the workspace at
  `workspaces/<name>`, a `~`/absolute path puts it exactly there. The choice
  is persisted (`sessions.workspace_dir`, schema v41) so restart, the shell
  pane, and delete resolve the same directory; creation and removal refuse a
  target holding anything but symlinks, so a mistyped path can never destroy
  real files (`workspace::ensure_workspace_at` / `remove_workspace_at`).
  `session get/list --json` expose `workspace_dir` + `additional_dirs`, and
  the `claude-named-workspace` agent-e2e scenario drives the whole flow —
  wizard keys, agent writing through the symlinks, persistence, guarded
  delete — against the real Claude Code binary (`docs/E2E.md`).

Keys and flow are documented in `docs/FEATURES.md`; the back-navigation
interplay with ADR-P12 in `docs/PERFORMANCE.md`.

### Behavior fixes

- **Cancelled multi-repo flow no longer leaks `additional_dirs`.** The
  new-session-name cancel left the wizard's derived extra dirs populated, so
  the *next* spawn silently attached the stale directories. Cleared on
  back-navigation/cancel now (fixed as part of the wizard redesign).

- **Copy falls back to `tmux load-buffer` / OSC 52 when the native clipboard
  can't reach the user.** Upstream copies only through `arboard`, which needs
  X11/Wayland — over SSH to a Linux host, under a display-less tmux, or in WSL
  without WSLg every copy failed with "Clipboard not available". The fork adds
  two fallbacks (`app::clipboard`), tried in the order that actually works:
  (1) inside tmux (`$TMUX` set — the common `tmux -> friring` setup),
  `tmux load-buffer -w -`, which has **tmux itself** set the outer terminal's
  clipboard; a raw application OSC 52 written to our own stdout is *dropped* by
  tmux's default `set-clipboard external` ("ignore attempts by applications to
  set tmux buffers"), so it must come from tmux — and this path returns a real
  exit status rather than being fire-and-forget (needs tmux ≥ 3.2 for `-w`,
  already required). (2) Outside tmux, a raw OSC 52 escape to stdout (for a
  direct OSC-52-capable terminal), whose toast is marked `(OSC 52)` since it is
  fire-and-forget. Applies to all copy surfaces (selection, status bar,
  code-review markdown). The native path is also skipped when it *works but is
  the wrong machine*: on a macOS (or Windows) host reached over SSH the native
  clipboard API is reachable from the SSH login, so `arboard` "succeeded" onto
  the **host's** clipboard — which the user never sees — and the fallbacks never
  ran. An SSH session (`SSH_TTY`/`SSH_CONNECTION`) with no forwarded
  `DISPLAY`/`WAYLAND_DISPLAY` (which on X11 platforms would route the clipboard
  back to the user) now goes straight to the tmux/OSC 52 route
  (`clipboard::native_clipboard_is_remote`) — except a loopback SSH
  (`ssh localhost`, a loopback server address in `SSH_CONNECTION`), where host
  and user are the same machine and native is kept. Paste keeps arboard only —
  terminals block OSC 52 *reads* — and the error points at the terminal's own
  paste key (bracketed paste still works); over SSH paste likewise refuses
  instead of silently pasting the *host's* clipboard.

- **Modifier-Enter inserts a newline in the agent instead of switching
  sessions.** A legacy terminal (Windows Terminal, or anything behind an outer
  tmux, which strips the kitty protocol) encodes `Ctrl+Enter` as the LF byte,
  which crossterm decodes as `Ctrl+J` — upstream's `NextSession` chord, so the
  keystroke switched sessions instead of reaching the agent. The fork adds
  `NextSession`/`PreviousSession` to `Action::terminal_passthrough` (upstream
  deliberately kept them as in-terminal nav): with a terminal focused,
  `Ctrl+J`/`Ctrl+K` now forward to the PTY (`Ctrl+J` is the newline shortcut
  Claude Code & co. understand; `Ctrl+K` is readline kill-to-end), and new
  `Alt+J`/`Alt+K` default alternates keep session cycling reachable there.
  Kitty-protocol `Ctrl+Enter` also no longer degrades to a bare CR:
  `agent::input::key_to_bytes` encodes Shift/Ctrl-modified Enter as CSI-u
  (`ESC [13;<mod> u`), keeping the modifier so agents read "newline", not
  "submit".

- **Worktree branch pre-fill keeps `/`.** In the new-worktree flow, the branch
  name suggested from the session name upstream drops every char that isn't
  alphanumeric / space / `-` / `_`, so a git-flow style session name like
  `fix/branch-naming` was pre-filled as `fixbranch-naming`. The fork preserves
  `/` as a hierarchy separator (collapsing repeats, absorbing adjacent hyphens,
  trimming at the ends). Everything downstream already handled slash branches —
  the worktree directory flattens `/` to `-` and tmux window names sanitize
  separately (`session_name_to_branch` in `src/app/key_handlers.rs`).

### Performance

- **Shell-tab keystrokes echo immediately.** The demand-driven render loop's
  output detector (`App::detect_output_redraw`, ADR-P1) summed only the
  *agent* panes' `last_output_at`, so a shell pane's echo never marked the UI
  dirty and only painted on the next keypress or the 250 ms forced-redraw
  floor — a measured ~280 ms per typed character in the shell tab (~40 ms on
  the agent tab). The fork folds each open shell pane's `last_output_at` into
  the detector's signature, restoring ~keypress-immediate echo. See ADR-P1 in
  `docs/PERFORMANCE.md`.

- **Global-search keystrokes do no I/O (ADR-P13).** Upstream's search re-ran
  a bounded filesystem walk (up to 5000 `read_dir` calls) synchronously on
  **every keystroke** and hit SQLite on every task preview — visible typing
  lag, seconds-long on network mounts. The fork snapshots the Files index
  once per open on a background thread (`BackgroundTask` fire-and-poll) and
  previews tasks from the in-memory cache, so a keystroke only does
  in-memory matching. See ADR-P13 in `docs/PERFORMANCE.md`.

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
  `scripts/install.*`, the `pages.yml` workflow, `website/`, the self-update /
  version-check code, the `min_thurbox_version` manifest key, and the `tb-` /
  `tbs-` tmux window prefixes (`cd.yml` is the exception — the fork cuts its own
  `friring-*` releases). See [Migration](#migration); upstream merges now carry
  rename conflicts on the renamed identifiers.
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
here (while remaining merge-safe). The release pipeline (`cd.yml`) is the
exception — the fork runs it. All build / test / lint jobs run normally on the
fork.

- `.github/workflows/pages.yml` (GitHub Pages) — dormant; the fork has no Pages
  site.
- `.github/workflows/cd.yml` (Release) — **active on the fork.** Every push to
  `main` that includes a `feat` / `fix` / `perf` commit cuts a tag
  (`cog bump --auto`) and publishes a GitHub Release with cross-platform
  `friring-*` binaries + a checksums file — this needs only the built-in
  `GITHUB_TOKEN`. The four package-manager publish jobs
  (AUR / Homebrew / Chocolatey / winget) stay guarded to `Thurbeen/thurbox`:
  those channels carry upstream's identity and the fork has no accounts or
  secrets for them. Two things still point upstream — changelog compare links
  (`cog.toml` `owner`/`repository`) and `scripts/install.*` (which fetch
  `thurbox-*` from upstream); grab the fork's binaries from its Releases page
  directly.
- `.github/workflows/ci.yml` — the `sonarqube` job is dormant; SonarQube is not
  set up for the fork at the moment. The `changes` (paths-filter) job also grants
  `pull-requests: read`, which a **private** repo's default token lacks (public
  upstream doesn't need it).

## Migration

Upgrading an existing `thurbox` install to the renamed `friring`? The rename
changed where the app looks, so move your state across once. The paths below
assume the default XDG roots; if you set `XDG_CONFIG_HOME` / `XDG_DATA_HOME`,
substitute `$XDG_CONFIG_HOME/thurbox` and `$XDG_DATA_HOME/thurbox` accordingly.
**Do these steps in order** — stop every writer before copying the database, or
you lose whatever it writes mid-copy.

1. **Stop all writers first.** Quit the TUI, disable the automation
   units (below) so the heartbeat stops, drain in-flight agents, and stop the
   old tmux server. Session hooks and the automation tick keep writing to the
   database until the server is gone.

   ```bash
   tmux -L thurbox attach        # drain in-flight sessions
   tmux -L thurbox kill-server   # once none are left running
   ```

   The same applies on each remote host (`tmux -L thurbox …` there too).

2. **Config** — copy the config dir. If you have **not** launched `friring`
   yet, `~/.config/friring` doesn't exist and a plain copy is correct;
   if it already exists, copy the *contents* (`cp -rT`, or `cp -r
   ~/.config/thurbox/. ~/.config/friring/`) so the old tree isn't nested as
   `~/.config/friring/thurbox`:

   ```bash
   cp -r ~/.config/thurbox ~/.config/friring   # dest must not pre-exist
   ```

3. **Data + DB** — with writers stopped (step 1), copy the data dir and rename
   the database, including its WAL/SHM sidecars, so the renamed DB keeps its
   uncheckpointed pages:

   ```bash
   cp -r ~/.local/share/thurbox ~/.local/share/friring   # dest must not pre-exist
   cd ~/.local/share/friring
   for ext in "" -wal -shm; do
     [ -e "thurbox.db$ext" ] && mv "thurbox.db$ext" "friring.db$ext"
   done
   ```

   **Keep the old data dir** until every migrated session is retired: sessions,
   worktrees, and multi-repo workspaces store **absolute** paths (both local and
   remote) under `~/.local/share/thurbox`, and the rename does not rewrite them.
   Deleting it early orphans those worktrees/workspaces.

4. **Env vars** — rename only the Friring **runtime / build / dev** variables
   you set in shell rc files, agent wrappers, or hooks: `THURBOX_CONFIG_DIR`,
   `THURBOX_DATA_DIR`, `THURBOX_SOCKET`, `THURBOX_SESSION`, `THURBOX_SESSION_ID`,
   `THURBOX_TASK`, `THURBOX_METRICS_DIR`, `THURBOX_PERF_LOG` → `FRIRING_*`.
   Variables read by the **retained upstream** installer / release tooling keep
   the `THURBOX_` prefix — leave `THURBOX_VERSION`, `THURBOX_INSTALL_DIR`,
   `THURBOX_REPO`, `THURBOX_PS_TEST`, and `THURBOX_RELEASE_VERSION` as-is.

5. **Automation units** — reinstall your systemd / launchd units under the new
   `friring` names and disable the old `thurbox` ones.

6. **Dev sandbox** — profiles under `target/dev-sandbox/*/thurbox-*` are stale;
   recreate them (see `docs/DEVELOPMENT.md`).

7. **Extensions** — previously-installed hooks and managed extension files still
   invoke `thurbox-cli`, and the installer recognizes only the `friring` marker,
   so it won't prune or refresh the old `thurbox`-marked entries automatically
   (it treats them as user-owned). Uninstall the old extensions with the
   *previous* build if you still have it, or remove the stale hook entries by
   hand, then reinstall from this repo's local copies (`friring-cli extension
   install ./extensions/<name>`). Bare-name / upstream-URL installs fetch
   upstream **Thurbox** payloads that call `thurbox-cli`.

8. **Self-update** — the self-update / version-check paths still track upstream
   **Thurbox** releases and aren't meaningful for a source-built `friring`;
   update by pulling this repo and rebuilding.
