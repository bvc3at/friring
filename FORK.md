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

### Deliberately not adopted

Upstream kept changing surfaces this fork had already rewritten. These stay
divergent on purpose:

- **`b6ddf31` copy over SSH via OSC 52** — the fork's clipboard stack already
  covers this and more; see "Copy falls back to `tmux load-buffer` / OSC 52"
  and "In-pane OSC 52 copies reach the user's clipboard" below. Two pieces of
  it *were* since ported onto the fork's own stack: the `OSC52_MAX_BYTES`
  ceiling and `extract_text_from_screen` (see "A terminal selection is read
  from the vt100 grid").
- **`03828a0` footer + status bar on narrow terminals** — the fork's own fix
  (see "The footer's text no longer runs under its buttons") solves the same
  overlap differently. Four ideas from it were ported into that fix rather than
  the commit; see "The footer degrades at narrow widths instead of blanking".
- **`86ab3dc` / `4e19147` / `b951991` F9 session-list collapse** — `F9` is the
  fork's activity view.
- **`1dd5edb` / `2c07e3a` demo regeneration** and the **visual identity** half of
  upstream's website work — the fork records its own demos and ships its own
  site, so upstream's Doom-inspired game-UI redesign, its restyled dividers and
  chrome, and its `iddqd` easter egg stay unadopted. The **bug-fix and
  load-cost** half of that work does apply to the fork's site, which was
  upstream's pre-redesign CSS rebranded, and is adopted separately (see "Own
  website" below).

## Renamed to friring (July 2026)

Friring began as a pure *branding* layer: only the human-facing name was
rebranded, while every functional identifier kept the upstream `thurbox` name.
As of July 2026 the plumbing is renamed too. The app's own identifiers are now
`friring` — the `friring` / `friring-cli` binaries, the crate, the config dir
(`~/.config/friring`), the data dir and DB
(`~/.local/share/friring/friring.db`), the tmux socket (`tmux -L friring`), and
the `FRIRING_*` env vars.

Friring's own distribution is renamed along with it: `cd.yml` publishes
`friring-*` release binaries, `scripts/install.{sh,ps1}` and the Homebrew
formula fetch them from `bvc3at/friring`, self-update / version-check query the
same repo, and `website/` is a Friring-branded site published to
`bvc3at.github.io/friring` (see
[Documentation / branding](#documentation--branding)).

What still says `thurbox` is deliberate, and splits in two:

- **Upstream attribution** — the `LICENSE`, provenance notes, the quality-gate
  badge, and the fork credit carried by the website (footer, FAQ, `llms.txt`)
  point at [`Thurbeen/thurbox`](https://github.com/Thurbeen/thurbox) and stay
  as-is (this is a fork, and the credit is upstream's).
- **Upstream-owned surfaces and shared formats** — the extension payloads a
  bare-name `extension install` fetches from upstream, the
  `min_thurbox_version` extension-manifest key (a wire format shared with
  upstream), and the `tb-` / `tbs-` tmux window prefixes (brand-neutral, kept
  for live-window compatibility).

The tradeoff the branding-only approach used to avoid is now real: upstream
merges carry rename conflicts on the renamed identifiers, and an existing
`thurbox` install needs a one-time [migration](#migration).

## Differences from upstream

### Features

#### Lazy sessions & ghosts (July 2026)

Upstream restores every persisted session eagerly at startup: sessions whose
tmux pane died (after a reboot, all of them) respawn their agent CLIs serially
before the first frame — N × ~500 ms of boot latency and N × hundreds of MB of
agent processes, whether or not the user wanted those sessions running. The
fork makes "not running" a first-class state:

- **Ghost sessions.** A session without an agent process renders as a greyed
  frozen frame of its last state (`SessionStatus::Unloaded`, dotted `◌` icon,
  muted list row, `unloaded — Enter loads` on the pane border). The frame is
  SGR-styled lines (adopt-seed shape) persisted on the `sessions` row (schema
  v45, columns outside the full-row upsert like the hook columns); it re-parses
  at the current pane size, so ghosts survive terminal/font-size changes.
- **`lazy_session_restore`** (settings.toml, default `true` — a changed
  default vs upstream's respawn-everything): pane-less sessions restore as
  ghosts; live panes still adopt. Startup after a reboot goes from N agent
  boots to ~5 ms of frame parsing.
- **Unload** (`Alt+U` direct / `<leader> U`): capture the visible screen, kill
  the agent window + shell pane, swap in the ghost in place. Loading (Enter /
  restart) rides the existing restart-resume machinery. A ghost is one screen
  by design: scrollback only ever holds output that *scrolled out*, and every
  supported agent's TUI repaints in place (`#{history_size}` measures 0), so
  there is nothing above the screen to save.
- **Loaded-only cycling** (`Alt+N`/`Alt+P` direct, `<leader> c`/`<leader> C`):
  session switching that skips ghosts and unreachable placeholders.
- **Crash safety:** frames are re-saved (~1/min, in-memory serialization only)
  for sessions with new output; backend captures happen at
  unload and clean shutdown (remote hosts serialize in-memory at shutdown
  instead, so a dying host cannot hang the exit).
- Measurements behind the design (frame ≈ 4–5 KB raw / ~1 KB compressed;
  parse ≈ 50 µs; grey pass ≈ 7 µs; idle claude CLI ≈ 333 MB RSS) were taken
  with the e2e stub harness on real agent frames.

#### Per-session memory (August 2026)

The ~333 MB above was the argument *for* ghosts, but nothing in either
upstream's or this fork's UI showed it: a user could not see what a session
cost, what unloading saved, or tell a ghost's frozen frame from a live idle one.
The fork measures it and puts it on screen — a badge on each session row
(`331M`), an `Σ` fleet total on the session list's bottom border, and the info
panel's RAM line (now the whole agent **process tree** with its process count,
where upstream sampled only the pane process). A ghost reads `—`: the measured
absence, distinct from a remote/unmeasurable session, which shows nothing at
all rather than claiming a saving nobody observed.

- Read off-thread every ~3 s from one process-table pass per scan (procfs on
  Linux, one `ps` on macOS, `sysinfo` on Windows) — ADR-P14 in
  `docs/PERFORMANCE.md`; behaviour and caveats in `docs/FEATURES.md` →
  *Per-session memory*.
- Gated by `[features] session_memory` (default `true`); off means the process
  table is never read and none of the three surfaces render.

#### Headless sends can't answer a dialog (July 2026)

Upstream's headless senders type their text and press Enter as two separate
`tmux send-keys` calls, with no look at the target pane. A session sitting on a
permission dialog swallows the text and reads the Enter as the operator
answering it — so `friring-cli message send`, whose contract is only "enqueue a
payload", approved whatever the recipient was asking permission to do, by
default and with nobody watching.

The fork guards every path that types into a pane (`session send`,
`message send`/`reply`'s wake, `send` automations, task prompts, and the
deferred `run-shell` delivery after a headless spawn):

- **Two signals veto a write** — the agent's own hook-reported `blocked` state
  (`session signal`) and a scrape of the visible pane for
  `agent::tmux::MODAL_MARKERS`. Each covers the other's blind spot (hooks are
  agent-specific, the scrape is a heuristic).
- **Scheduled work is gated on the pane alone.** `blocked` means "a dialog was
  raised this turn", not "a dialog is up now" — Claude Code fires `PreToolUse`
  *before* the prompt and nothing on approval, so an approved tool call reports
  `blocked` for its entire run (measured and pinned by the
  `claude-blocked-spans-tool-run` e2e). The mailbox wake and `session send`
  honor it anyway, since a false refusal there is cheap; `send` automations and
  task delivery don't, so a session busy with a long approved tool call doesn't
  silently miss every fire aimed at it.
- **A refusal fits the caller.** The mailbox wake defers silently and reports
  `wake_deferred`; `session send` errors, with `--force` to type anyway; an
  automation records a `Skipped` run naming the marker and how many steps
  landed; a task stays due instead of being marked in progress.
- **No timeliness lost.** A deferred wake is owed, not dropped
  (`session_messages.wake_pending`, schema v46), and `automation tick` retries
  it once the pane is clear. `--no-wake` never marks it; reading the inbox
  settles it.
- **The nudge is self-describing.** Upstream types the bare word `inbox`; the
  fork names the command and the sender, since the nudge lands as a user turn
  and a recipient that was never taught the convention can only guess at a
  token. Still pointer-only — the body stays in the queue.
- **Replies thread.** `message reply` records `in_reply_to` (schema v46),
  exposed in `--json` and as the `RE` column, so two conversations in flight on
  one task stay tellable apart without smuggling the id into `kind`.

Not a permission boundary: anything that can run `friring-cli` can still
`session send --force`. What it removes is the surprise.

Proved end to end against a **real** claude permission dialog
(`claude-wake-modal-guard`): a send while the dialog is up types nothing and
leaves the tool unexecuted, and the owed nudge lands once a human answers.
Run against the pre-guard build the same scenario reports `"woke": true` and
the pane shows the Bash call already `Done` — the report's finding, reproduced
as a regression test. `scripted-message-queue` covers the pane-scrape half in
isolation (that agent declares no status hooks, so only the scrape can refuse).

#### tmux-style leader key (July 2026)

Upstream dispatches every global command from a direct `Ctrl+<letter>` chord
and has no prefix/leader concept. The fork adds one, because that namespace is
exhausted: every bare `Ctrl+<letter>` is bound or reserved, `F1`–`F10`/`F12`
are spent (`F11` belongs to the OS/terminal on every platform friring runs on,
so new commands land on `Alt` or the leader — see *Session-list collapse*),
and several chords friring holds are ones the inner agent CLI wants
back (`Ctrl+L` clear-screen, `Ctrl+Z` suspend, `Ctrl+V` image-paste in Claude
Code and Codex, `Ctrl+G` external editor).

- **`Ctrl+F` leader + which-key overlay.** Arming paints a grouped table of
  everything reachable; the next key runs it. No timeout (tmux semantics —
  friring is normally driven over SSH, where a timeout would misroute a paused
  keystroke into the agent). `Esc`/`Ctrl+C` cancels, and an armed badge shows
  in the footer.
- **Session selection by number.** `<leader> 1`–`9` jumps to that session and
  `<leader> a` + digit to the Nth *blocked* one — a route that works where
  upstream's `Alt+1`–`9` cannot, since GNOME Terminal / Konsole / Tilix /
  xfce4 / Ghostty-Linux all claim `Alt+<digit>` for their own tabs and macOS
  terminals ship Option-as-Meta off.
- **Three modes** (`[prefix] mode`): `off` reproduces upstream exactly, `both`
  (default) adds the leader alongside the direct chords, and `prefix-only`
  disables direct **global** chords so every bare `Ctrl+<letter>` reaches the
  agent CLI untouched. Pane-scoped keys are unaffected in all modes.
- **`<leader> <leader>` sends the leader's byte** to the agent (tmux
  `send-prefix`), so `Ctrl+F` stays reachable by the inner CLI. The table also
  accepts its keys with `Ctrl` held (`<leader> C-b` == `<leader> b`), as GNU
  screen does.
- **`prefix2` (`F12` by default)** is the layout-independent second door. The
  trade is that `F12` stops toggling the perf HUD while the leader is on —
  that moved to `<leader> m`. `key2 = ""` reverses it.
- **`Ctrl+F` was chosen** because every program that claims it claims it for
  something with a non-`Ctrl` route: Claude Code leaves it unbound, and Codex /
  aider / opencode bind it only to cursor-right, co-bound to `→`. It is also
  home-row on QWERTY/QWERTZ/AZERTY/Nordic, plain C0 (`0x06`, no kitty protocol
  needed through ssh + tmux), and a slip to `Cmd+F` opens a find bar rather
  than quitting the terminal. `ForkSession` keeps `Ctrl+F` as its direct chord
  for `mode = "off"`, and is `<leader> f` otherwise.
- **Reordering by distance.** `<leader> K`/`<leader> J` then `1`–`9` moves the
  active session that many places up/down, renumbering the list by distance
  while the gesture is pending. Upstream reorders one row at a time
  (`Shift+J`/`Shift+K`), which is still there.
- **Startup warnings for risky leader rebinds** — `ctrl+b`, `ctrl+a`,
  `ctrl+c`, `ctrl+d`, `ctrl+z`, `ctrl+q`, `ctrl+s` each report why they will
  misbehave. Warnings only; the user's config wins.
- The `[prefix]` settings table (`docs/CONFIG.md`) is fork-only.

#### Code review v2 (July 2026)

The built-in review view grows the annotate → agent-fixes → re-review loop:

- **`Question` comment classification.** Tab cycle is now `Note → Issue →
  Suggestion → Question → Praise`; `Question` asks the agent to answer
  rather than change code.
- **Structured agent handoff (new default).** `e` (Send→Agent) and `y`
  (Copy) compile an in-band semantics preamble + one `### C<id> [Class]
  <side>:<line>` record per comment, quoting the anchored diff line as a
  grep-able locator (old side marked `(line was removed)`). The upstream
  bullet format is preserved behind `[review] handoff = "legacy"`
  (`docs/CONFIG.md`); the new `[review]` settings table is fork-only.
- **Manual reload (`F5` / `Ctrl+R`).** Rebuilds the current target in place
  (upstream's only refresh was retarget/reopen), preserving the selection by
  file.
- **Self-invalidating reviewed marks.** Marks store a semantic fingerprint
  of the marked content (schema **v42**, `review_marks.fingerprint`); every
  completed build deletes marks whose file/hunk content changed and toasts a
  summary — upstream marks could silently go stale.
- **Staged-only target.** The `t` picker gains `Staged changes (index vs
  HEAD)` (`git diff --cached`) between Working and Branch.
- **Untracked files in the Working target.** Synthesized as all-added
  entries with a `?` glyph (upstream's `git diff HEAD` never showed them);
  oversized/binary files degrade to a placeholder row.
- **Changed-files filter (`o`).** `All → Unreviewed → Commented`, scoping
  the tree and the `}`/`{` jumps, with auto-advance to the next unreviewed
  file on marking.
- **Comment navigation.** `(`/`)` jump prev/next comment (wrapping,
  unfolding folded files); `@` opens an all-comments popup with `C<id>`
  rows.
- **Word-level intra-line diff.** Changed tokens of an aligned del/add pair
  get a stronger background (new theme keys `diff_added_word_bg` /
  `diff_removed_word_bg`, derived per preset); 30% shared-token gate;
  composes with syntax + search highlighting in both layouts.
- **Syntax highlighting in side-by-side.** Both halves of the paired layout
  now render through the same highlighter pipeline as the unified body
  (upstream painted them as plain tinted text).
- **Context expansion (`=`/`+`).** Cycles `-U3 → -U10 → -U25`, shown as
  `· U<n>` in the title.
- **Range comments (`V`).** `V` + `j`/`k` select a same-side, same-file
  line span, `c` comments on it (schema **v43**,
  `review_comments.line_end`); the handoff record reads `new:10-24` and
  quotes the span's first + last lines with `> …` between.
- **Binary diff placeholder.** A binary body renders an explanatory
  `(binary file[, size])` row instead of upstream's bare `+0 -0` header
  (size only where a local stat is free — the Working target).
- **Search history (`↑`/`↓` in the find bar).** Committed searches recall
  per session (in-memory); match-stepping while typing moved to
  `Ctrl+N`/`Ctrl+P` to free the arrows.
- **Review info popup (`i`).** Target + bases, file counts, `+`/`-`,
  filter/context, and the range's commit list in one overlay.
- **Re-review nudge.** After a review is sent, the agent's next
  Working → idle edge toasts "F7 to re-review, F5 to reload" (once per
  send; `[review] nudge_on_idle` opts out).
- **Open in `$EDITOR` (`E`).** Suspends the TUI, opens the selected line
  in `$VISUAL`/`$EDITOR` (`+<line>` convention), and auto-reloads a
  Working-target diff on return. Local sessions only.
- **Real-agent e2e grounding.** The `claude-review-loop` scenario
  (`scripts/dev/agent-e2e/`) drives the whole loop against a real Claude
  Code binary: annotate → `e` → the structured handoff must reach the
  stubbed model API byte-intact (the fixture pins the C-id, class,
  locator, and quoted anchor) → re-review nudge → reopen restores the
  comment. The harness gained an optional `scenario_prepare()` hook for
  post-boot workspace state (an uncommitted edit for the Working target).

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
- **Overview dashboard** (July 2026 UI redesign): identity line (agent ·
  provider · model), a full token line (`in · out · cache r / w`, folding in
  every subagent/workflow transcript's usage), a stat-tile row (`$ ✎ ⊙ ⌕ ⚲ ⚙`
  counts, a `✗ failed` tile only on failure), an events-over-session
  sparkline with a turns/last-action line, the hottest files, the
  most-repeated commands, the newest few actions, the last error with its
  result head, and a `⟳ indexing history…` loader while a large source
  backfills. Every widget fits the pane: tiles wrap by whole tiles, the
  sparkline max-pools down to the available width, event-row markers are
  width-budgeted, and text wrap is on by default (`w` toggles).
- **Turn-grouped Timeline** (same redesign): each user prompt renders as a
  dash-filled turn header (`▶ HH:MM:SS "prompt" ───`), the turn's events sit
  in a `│` gutter — subagent-origin work nested as `└` with a dim origin
  badge — consecutive read/search/bookkeeping repeats fold to one `×N` row,
  and rows carry call→result durations. Bookkeeping tools (TodoWrite,
  TaskOutput, …) are kept as dim **minor** rows instead of being dropped.
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
  per-session state *moves* into the `spawn_blocking` pass and back. The
  Claude accumulator also tails every **subagent / workflow transcript** the
  tree scan indexed, merging all streams by timestamp (origin-labelled,
  subagent task prompts dropped) so delegated work reaches the Timeline.
  History is never clipped for append-only sources: a huge transcript
  backfills front-to-back in 8 MiB chunks, one per pass, surfaced as a
  loader (`SessionActivity::backfilling`); only snapshot/DB sources keep
  (raised) caps — cursor 32 MiB tail-window, crush 50k / opencode 100k /
  goose 50k rows, codex 45 discovery day-shards.
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
- **Follow-ups** (named, not silently dropped): hook-injected capture for
  `agy` (encrypted store) and a session-keyed reader for `amp`; markdown
  rendering of thinking/text (blocked on `ui::markdown` not being
  width-aware); parsing an in-process run's workflow `scripts/*.js` for live
  phase names; baking the session id into per-session hook commands so a
  daemon worker also reports `working`/`blocked`/`done` **status**; and remote
  (`ssh:`/`wsl:`) support. **Done since v1:** daemon-worker attribution + live
  overview, per-session `--settings` for exact attribution, find-in-transcript,
  the July 2026 multi-agent redesign; and (July 2026 UI redesign) subagent /
  workflow event streams merged into the Timeline, chunked async backfill of
  large transcripts replacing the 8 MiB clip, user-prompt turn markers, and
  the dashboard Overview — covered end-to-end by the `claude-activity-view`
  agent-e2e scenario (dashboard tiles + turn-grouped timeline over a stubbed
  turn).

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

The one exception to "`inline` never falls back to the column" is the
session-list collapse below: the inline dock *is* the left column, so
collapsing it leaves every position column-only.

#### Session-list collapse (`Alt+L`)

Upstream hides the session-list pane with `F9` for a full-width terminal
(upstream commits [`86ab3dc`], [`4e19147`], [`b951991`]). The feature is
adopted here; two things differ.

**The chord is `Alt+L`, plus `<leader> Shift+L`** — `F9` in this fork is the
agent activity view, and there is no free F-key to move to: `F1`–`F10` and
`F12` are bound, and `F11` belongs to Mission Control on macOS and to
fullscreen in GNOME Terminal / Konsole / xfce4-terminal / Windows Terminal, so
a pill labelled `F11` would silently do nothing. `Shift+F9` is worse than that:
xterm-family terminals map `Shift+F1`–`F10` onto legacy `F13`–`F22`, which
crossterm reports as a bare `KeyCode::F(13..22)` with no `SHIFT` bit, so on
those terminals `Shift+F9` arrives as plain `F9` and opens the activity view
instead. `Alt+L` (**L**ist) joins the fork's existing narrow Alt exception
(`Alt+A`, `Alt+U`, `Alt+J/K`, `Alt+N/P`, `Alt+1`–`9`): it encodes as plain
7-bit `ESC l`, so it survives ssh + tmux without the kitty protocol, and it is
not a bare `Ctrl+<letter>`, so it never defers to the PTY. `<leader> Shift+L`
is the route that needs no terminal configuration at all — and the only one
that works under `[prefix] mode = "prefix-only"`.

**The collapse reconciles with the inline info pane.** Upstream drops the left
column wholesale because upstream's info panel is always a dedicated column;
here `info_panel_position = auto` (the default) or `inline` docks it *in* that
column. So while the list is collapsed the pane is column-only: it falls back
to the dedicated column at `three_panel_min_cols` and up, and below that width
it has nowhere to render — collapsing turns it off with a note, and `F2` says
why instead of flipping a flag that changes nothing on screen. The automations
pane shares the column too, so collapsing moves focus out of the whole
automations context, and a global-search jump to an automation brings the
column back.

**The chevron is expand-only.** Upstream draws a `◀`/`▶` affordance in both
states; here it appears only while the list is collapsed. The fork's central
pane packs four tab pills (Agent · Review · F7 · Shell · F8 · Activity · F9)
into ~40 columns and `break`s when it runs out of room — a permanent ~9-cell
chevron would, on a 120-column terminal with tasks + the file viewer open,
silently drop the Activity tab. Upstream's two refinements ([`b951991`]) are
kept: the one-cell gap before the tab strip, and the hover carve-out that keeps
the chevron a subtle band rather than a filled pill.

[`86ab3dc`]: https://github.com/Thurbeen/thurbox/commit/86ab3dc728f5ab307822c442c959ae8cabc1e68d
[`4e19147`]: https://github.com/Thurbeen/thurbox/commit/4e191473dffa01a20b028cbcc456d25665451972
[`b951991`]: https://github.com/Thurbeen/thurbox/commit/b951991a458ae9ca11f2d92aca9ec36b84df6b13

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

The suite has since grown from agent smoke tests into a **core-feature e2e
suite** (31 scenarios, 43 bats tests): tmux-persistence re-adoption, the real
permission→blocked hook path, restart-resume / fork / conversation import
(riding claude's `--session-id {id}` pinning — the harness `agents.toml`
entry now mirrors the production templates), worktree sessions and `Ctrl+S`
sync incl. the conflict handoff to the agent, code-review export, automations,
tasks, inter-session messages, extension lifecycle with offline issue-sync,
global search, the F9 activity view, both wizard flows, and the polish surface
(themes, settings live-reload, keybinding editor, shell pane, soft delete,
attention navigation). Two harness additions keep that hermetic: a
**`scripted` agent profile** — a bash script registered through the ordinary
`agents.toml` machinery (living proof of the agent-neutral registry) that
echoes stdin back, giving fast model-free scenarios that never skip — and a
seeded sandbox `settings.toml` (`[features] notifications = false`) so
blocked-state tests can never fire a real desktop banner. See `docs/E2E.md`.

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

#### Demo pacing budget (`lib/check-pacing.mjs`, `Wait` in the tapes)

The demo clips are held to a measured pacing budget, and the tapes gained a
`Wait` directive so they stop guessing how long the app needs.

The problem was measured before it was fixed. Across the ten clips, **185.0s of
202.2s was a frozen frame — 91.5% dead air**, and only 466 of 6,065 frames were
unique (2.31 unique fps). Auditing the tapes agreed independently: 178.8s of
scripted `Sleep` against 12.85s of typing, 93.3%. The clips were not unusually
long — the 39.4s hero sits near the median of eighteen comparable TUI project
demos — they simply stalled. Every clip also opened on ~2.3s of frozen screen
(a filmed `sleep 1` in the recorder plus a settle `Sleep` in every tape), and
`theme.tape` spent 3.8s — a third of its runtime — on one static image because
it pressed `Up` eight times through a list with four entries above the cursor.

What changed:

- **`Wait /<re>/` and `Wait Stable [<quiet>]`** in the tape driver, replacing
  the "leave it the time it needs" sleeps. Measured against the guesses they
  replace: a forked agent CLI paints in **0.3–0.4s**, not the scripted 3.5s.
  This mirrors what `record.sh` already did for its own pre-play step, which
  has synced on pane markers rather than fixed sleeps all along.
  Stability is only a proxy for readiness — a beat that repaints, pauses, then
  repaints again satisfies it early — so the settle window is a per-beat
  argument, and the driver prints a note naming any wait whose screen kept
  moving afterwards. That check costs nothing (no key is sent during the `Sleep`
  after a wait, so movement there is the app) and it immediately caught a
  shipped clip: `friring-session-creation` was ending on a blank terminal
  because its wait settled while the agent was still booting.
- **The opening is polled, not slept.** `record.sh` waits for the attached
  client to paint one settled frame instead of a blind `sleep 1`, and the tapes
  dropped their settle beats: ~2.3s → ~0.4s. The floor is ~0.35s (the poll plus
  node's own startup before the first keystroke, all of it filmed), which is
  why the budget targets 0.5s but caps at 0.75s.
- **`check-pacing.mjs`** enforces max held frame 1.0s, opening 0.75s, and
  GitHub's 10MB image limit. It reads each held frame's duration straight from
  the GIF's own frame delays — exact, and with no false positive on typing.
  Only the opening metric shells out to ffmpeg, because a static opening split
  by one ticking character is several short frames to a delay reader and needs
  pixels to see. The recorder refuses a take that busts the budget; CI
  (`demo-pacing`) re-checks whatever was committed.
- **The recorder fails closed on a driver error.** `record_tape` is called as
  `record_tape "$t" || …`, which suppresses `set -e` for its whole body, so a
  tape that died half-way still rendered and shipped — a clean recording of the
  first half of a demo, which is not visibly broken.
- **The cast's teardown trim is no longer all-or-nothing.** `trim-cast.mjs` cut
  at the client's leave-alt-screen event, but the teardown is chunked by the pty
  and its screen-clear can land in a SEPARATE, earlier event — which then
  survived the cut and became a blank final frame, held for the whole closing
  hold. It is intermittent (it depends on how the bytes split: nine clips in one
  batch were clean and the tenth was not) and invisible to every pacing metric,
  because a blank frame is perfectly well-paced. The trim now also walks back
  over content-free events, and the budget gained a **final-frame ink** check as
  a backstop — a good closing frame measures 3.4–9.0% ink, a leaked teardown
  0.013%.

Result across all ten clips: 202.2s → 103.3s, dead air 91.5% → within budget,
worst held frame 3.81s → 0.86s, opening 2.3s → 0.25–0.44s. Several content bugs
surfaced on the way, all of which had been shipping unnoticed because the media
was never re-recorded: two dead keypresses (`theme.tape`'s four no-op `Up`s;
`file-manager.tape` pressing `Enter` on a file, which resolves to
`open_file_in_editor` and so renders nothing in-pane), a session-name field that
is now pre-filled with the repo basename, so tapes that typed a name over it
produced sessions called `orbital-hvacaurora-forecast`, and a repo picker in
`agents.tape` still written for the pre-redesign "Select Repos" modal — the
shipped hero predated that redesign.

One caveat worth knowing when re-recording: **the recorder is sensitive to
machine load.** Under a load average of ~5 the same tape recorded at 2.5x its
length, the pause after the code-review view closes stretched from 0.3s to 3.0s
(taking `Ctrl+N` with it, which then landed in a dead window and wedged the
take), and `Wait Stable` overshot its nominal settle window because every poll
spawns tmux. Record on an otherwise idle machine; a take that wedges or busts
the budget under load is not necessarily a tape bug.

#### Dev-live: run a dev build against the real sessions

Upstream (and the fork's sandbox) keeps dev builds fully isolated: a
`-dev`-versioned binary compiles to the `friring-dev` socket, `friring-dev`
tmux group session and `friring-dev` data dir, so it can never see an
installed release's live sessions. The fork adds the deliberate escape hatch
for verifying a feature against real workloads: a `FRIRING_TMUX_SESSION` env
override for the local group-session name (`local_session()`, mirroring
`FRIRING_SOCKET` — both are needed: the socket picks the server, the session
picks the window group `discover()` scans; remote hosts keep their
`hosts.toml` names). Since quitting friring only detaches (tmux keeps every
agent alive) and startup re-adopts by window name/pane id, pointing a dev
binary at the release socket + session + data + config attaches it to all
live sessions — and quitting hands them back to the installed release.

`scripts/dev/live.sh` (`just dev-live`) packages the workflow: build, refuse
while any client is attached to the release server (no single-instance lock
exists — two TUIs would fight over the same panes; the check repeats right
before launch, since the build/backup window is wide enough to lose the race),
back up `friring.db` (transactional `sqlite3 .backup`, **required** — a torn
file copy can't be trusted as the recovery snapshot; migrations are
forward-only and a dev branch may bump `SCHEMA_VERSION`), then launch the dev
TUI with the four overrides set and `target/debug` first on `PATH`. The
automation heartbeat that friring arms in the *already-running* release server
(`ensure_automation_heartbeat`) now forwards the set `FRIRING_*` overrides into
its window (`-e`), so a heartbeat created under dev-live ticks the live DB
rather than the dev build's isolated default — a no-op on a normal launch where
no overrides are set.

`Ctrl+Alt+R` (`Action::ReloadApp`, fork-only) closes the loop in place:
a normal quit followed by an `exec` of the on-disk binary — env (and so a
dev-live attach) carried over, sessions re-adopted by the new image without
the terminal ever returning to the shell. Rebuild, hit the chord, and the
running instance *is* the new build. Details in `docs/CONFIG.md` (env
table), `docs/DEVELOPMENT.md` ("Live mode"), and `docs/FEATURES.md`
("Reload friring in place").

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

#### Automations that can stand up a real agent (July 2026)

Upstream's automations are a thin scheduler: a timestamp fires one action that
delivers one static string, locally. That is too weak for the headline use case
— standing up a fresh, correctly-configured agent to do real work on a
schedule. The fork widens the model in one migration (schema **v44**, every
column nullable so pre-v44 rows keep their exact old behavior):

- **Multi-step prompts.** `send` and `spawn` deliver an *ordered list* of
  prompts, each its own paste + Enter with a settle delay between them, so a
  scheduled agent can be configured before it gets work (`/model opus` →
  `/effort high` → the real prompt). Upstream can only send one string, and a
  multi-line one submits as a single message, so slash-command setup was
  impossible. Stored as JSON in `prompt_steps` (`NULL` = the single legacy
  `prompt` column); the default settle delay is 1200 ms, overridable per step.
  Headless delivery emits the whole sequence as one `tmux run-shell` script, so
  the sub-second gaps survive (`run-shell -d` takes whole seconds only).
- **Remote hosts.** A `spawn` automation takes a `hosts.toml` host, so the
  session, the tmux window and the prompt delivery all land there. A remote
  spawn runs in the repo root: a host combined with a worktree branch, a
  worktree extra-repo, or a `~` path is rejected at save, because the TUI
  provisions worktrees through the local git helper and would build the
  checkout on the wrong machine. Upstream
  hard-codes `host: None` and its headless prompt helpers hard-code
  `local_mux_command`, so a remote automation would have spawned a session and
  typed into a window on the wrong machine. The fork routes those helpers
  through a `MuxTarget` (transport + socket + group session + the host's own
  multiplexer binary), resolved from the action's host; an unknown host errors
  *before* the spawn.
- **Fresh session per fire.** `session_mode = fresh` spawns
  `auto-<id>-<UTC stamp>` per run instead of piling every run into one
  `auto-<id>` conversation, stamping the worktree branch the same way (else two
  live runs share one checkout, since `create_or_attach_worktree` is
  idempotent) and capping concurrently-open sessions at 5 so a short cron can't
  accumulate them unboundedly. `reuse` remains the default and matches upstream.
- **Send follows the session's own backend.** Delivery resolves the target
  session's `backend_type`, so an automation can prompt a session running on a
  remote host — and the TUI and the headless tick agree about it. Upstream (and
  this fork's first pass) hardcoded the local multiplexer headlessly, so the
  same automation succeeded from the TUI and recorded a skip from the keeper,
  depending only on which firer won the claim.
- **Send by session name.** A `Send` target is an id *or* a name, re-resolved
  per fire. Upstream's hard UUID dies with the session (force-deleting it
  disables the automation); the name form survives a close-and-recreate — the
  behavior upstream already grants extension-declared automations via re-linking
  but not user-authored ones.
- **Exec off the tick thread, with a process-tree deadline.** Upstream runs an
  `exec` automation's command synchronously inside `tick_core`, so a hung
  command freezes the whole render loop. The fork records a `running` run, hands
  the command to a worker, and updates that same row when it exits — one history entry per fire, visible
  while it works. Commands are killed at a deadline (`--timeout`, default
  900 s) — the whole process group, not just the shell, since a backgrounded
  worker would otherwise outlive the deadline while holding the pipes open —
  with output drained to a bounded tail on separate threads, and a
  `running` row orphaned by a crash is reaped — on the next startup and on every
  headless tick — once it outlives its own command's timeout.
- **The editor reaches the whole model.** Upstream's editor exposes repo /
  worktree / agent as free text and can't set a base branch, extra repos, a
  host, a session mode or an exec timeout at all. The fork makes **agent** and
  **host** selectors over the live registries (an unknown name is a save-time
  error, not a fire-time one), validates the **timezone** (upstream silently
  falls back to system local on a typo, so the automation fires hours off), and
  adds base branch, multi-repo, session mode, exec timeout, and the prompt-step
  editor.
- **CLI parity + dry run + export.** `automation edit` takes the same action
  flags as `create` (upstream can only edit name/trigger/prompt/enabled —
  changing an action meant delete-and-recreate). `automation dry-run` and the
  TUI's `p` overlay show what the next fire *would* do without firing;
  `automation export`/`import` round-trip through the existing
  `[[automations]]` manifest grammar, which the fork widened (spawn actions,
  prompt steps with an optional per-step `[[automations.steps]]` table, host,
  timezone, enabled) rather than forking into a second format; export picks the
  narrowest form that survives a round trip.

Behaviour is identical across all three firing paths (TUI tick, headless
`automation tick`, OS timer) and claim-based at-most-once firing is untouched.
Details in `docs/FEATURES.md` § Automations, the manifest grammar in
`docs/CONFIG.md`, the flags in `docs/CLI.md`.

Deliberately **not** built: automation→automation chaining. See the design note
at the end of `docs/FEATURES.md` § Automations for why multi-step prompts
already cover the case it was meant to serve.

### Behavior fixes

- **A forced send is refused at a dead pane too.** Adopting upstream's
  dead-pane guard (`c89eecd`) meant choosing where it sits. Upstream had one
  entry point; the fork has two — the modal-guarded `send_prompt_now_on` and
  `send_prompt_unguarded_on`, the escape hatch behind
  `friring-cli session send --force`. The guard goes in the *unguarded* one,
  which every send funnels through: `--force` exists to override the **modal**
  check for an operator who is looking at the pane, and a pane whose process
  has exited accepts nothing either way — so forcing into one would report a
  delivery that did not happen, which is the exact bug being fixed. The
  liveness probe is host-aware (`pane_is_dead_on` runs through the session's
  `MuxTarget`), so a remote session is asked about its own pane rather than a
  local one that may not exist.

- **A database written by a newer friring is refused, not silently opened.**
  Upstream's schema migrations are forward-only and unguarded: a binary opening
  a DB whose stored `schema_version` is *higher* than its own ran no steps and
  proceeded anyway, deferring the breakage to whichever later query hit a
  rebuilt/dropped column (or to silent bad data). The fork's `initialize`
  (`src/storage/schema.rs`, `reject_newer_schema`) now errors **before any
  DDL** — so the `CREATE … IF NOT EXISTS` batch can't recreate a table the
  newer schema dropped — with the two ways out: upgrade the binary or restore
  the pre-upgrade backup. Chiefly hit by relaunching the release binary after a
  schema-bumping dev build ran on the real DB via `scripts/dev/live.sh` (which
  backs the DB up first for exactly this reason).

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
  fire-and-forget. That raw-escape route alone is length-capped
  (`OSC52_MAX_BYTES`, 74,994 bytes — a 100,000-byte total sequence less base64
  overhead and framing, the ceiling upstream derives in `b6ddf31`): a terminal
  that abandons an over-long sequence keeps *printing* the rest of the base64
  over the TUI, so an oversized copy is refused up front with its size instead.
  The `tmux load-buffer` route has no such cap — tmux reads the text over a
  pipe. Applies to all copy surfaces (selection, status bar,
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
  terminals block OSC 52 *reads* — and over SSH it likewise refuses instead of
  silently pasting the *host's* clipboard. Both refusals are **Info**, not
  Error: over SSH, or on a display-less host, having no readable clipboard is
  the correct steady state, not a fault, so the status names the key that does
  work (`Ctrl+Shift+V` / `Cmd+V`, whichever the user's own terminal uses —
  bracketed paste reaches friring either way) rather than painting a red banner
  on every paste. A read that is attempted and *fails* still errors.

- **In-pane OSC 52 copies reach the user's clipboard.** A program inside a
  pane that sets the clipboard via OSC 52 — Claude Code's `/copy`, nvim's
  OSC 52 provider — copied nothing, twice over: seeing `$TMUX`, Claude Code
  wraps the escape in the tmux DCS passthrough (`ESC P tmux ;` + inner ESCs
  doubled), which tmux's default `allow-passthrough off` silently discards;
  and even unwrapped, friring is that pane's "terminal", and the vt100
  parser ignores the escape (nor can its `unhandled_osc` callback carry it —
  vte truncates OSC payloads at 1 KiB, which would corrupt any real copy).
  The fork scans the raw pane byte stream *before* the parser
  (`agent::osc52`, an incremental scanner robust to `%output` chunk splits,
  parsing both the plain and the passthrough-wrapped forms; the control-mode
  stream carries the escape raw, whatever the inner tmux's `set-clipboard` /
  `allow-passthrough` say) and routes each completed payload through the same
  `App::set_clipboard_text` stack as every other copy surface (so it lands
  native locally, or via tmux/OSC 52 over SSH — agent and shell panes, local
  or remote sessions alike), with a `Copied from <session>` toast naming the
  originating pane. Clipboard *queries* (`52;<sel>;?`) are dropped, never
  answered; payloads over 8 MiB of base64 are dropped whole rather than
  truncated. Per-pane queues are generation-gated (ADR-P10: the every-tick
  nothing-new poll is one atomic load) and drop-oldest at 8 so a spamming
  pane can't grow memory — the newest copy is the one that must win. Every
  queue is drained each tick but only that newest copy (by global capture
  sequence, so cross-pane order holds) is *written*: the rest would be
  overwritten before anyone could paste them, and writing them all would put
  up to eight blocking `tmux load-buffer` spawns per pane on the event-loop
  tick.

- **A terminal selection is read from the vt100 grid, not the painted cells.**
  Upstream drags copy whatever glyphs the frame buffer holds, so a URL or path
  long enough to soft-wrap arrives with a newline where the pane edge was — it
  stops being one string exactly when pasting it as one string is the point,
  and a drag past the last line of output carries blank rows along. Selections
  inside the central pane's terminal view are extracted from the session's own
  vt100 screen instead (`ui::selection::extract_text_from_screen`, ported from
  Thurbox's `b6ddf31`), which knows a wrap seam from a hard newline and rejoins
  it, trims per *logical* line, and drops trailing blank lines. It runs under
  the parser lock the central-pane render already takes, so a live drag costs
  no extra lock (ADR-P). Panes with no grid behind them — session list, info
  panel, review, activity — still read the painted cells.

- **`Cmd+C` / `Cmd+V` are macOS default chords for Copy/Paste.** `Ctrl+C`
  doubles as SIGINT (no-selection case), which upstream accepts as the only
  copy chord; the fork appends `Cmd+C`/`Cmd+V` to the macOS default set
  (`Action::default_chords_for`, alongside the existing `Cmd+J`/`Cmd+L`
  family) so copying doesn't share a key with interrupting. They reach
  friring only from terminals that forward unconsumed Cmd chords (Ghostty's
  `performable:` defaults forward `Cmd+C` whenever the emulator has no
  selection of its own — always, under friring's mouse capture); where the
  emulator consumes them its copy/paste semantics still apply, so the chords
  are never in conflict. `Cmd+C` with no selection is swallowed (SUPER never
  forwards to the PTY) — it can't SIGINT the agent.

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

- **The central pane's title no longer collides with its tab strip.** Upstream
  right-aligns ` {name} ({agent}) [{branch}] [{status}] ` on the same top
  border the Agent/Review/Shell/Activity pills are painted over, and paints the
  pills last — so on a narrow pane, or with the session name and branch that a
  worktree session usually shares, the tabs simply overwrote the title's head
  (an e2e scenario had to anchor its waits on the name's *tail* for this
  reason). Two changes in `ui::terminal_view::pane_title`: the **session name
  is gone** from this title (the header badge one row up, right-aligned to the
  same edge, already shows the active session — the pane title now carries only
  what the header can't: `claude [branch] [Idle]`, or `shell` in the shell
  view), and what remains is **fitted to the columns the strip leaves**
  (`app::view::central_tabs_width`). The fit is measured per frame rather than
  against a worst-case `[Unreachable]`, so a short status hands its columns back
  to the branch; over budget, the branch truncates (`[fix/displa…]`), then the
  agent sheds, then the branch drops — status and the scrollback marker are
  never dropped.

- **The footer's text no longer runs under its buttons.** Upstream paints the
  left-hand text (focus label, session/automation counts, key hints) across the
  whole footer row and the right-aligned pills on top of it, so any terminal too
  narrow for both left the text chopped mid-word *and* leaking through the
  one-column gaps between the pills — at 100 cols the row read
  `Sessions  Help · F1 s Info · F2 c Files · F3 …`, where the stray `s` and `c`
  are what survived of `0 session(s)`, painted in a different colour from the
  chips around them. Both blocks are now fitted to the same column budget and
  painted into **disjoint** rects (`ui::status_bar::render_footer`), degrading in
  order: the pills' ` · ` separators first (` Help · F1 ` → ` Help F1 `), then
  the left-hand text segment by segment (the global `^H/^L Focus ^O Open` hints,
  then the counts, then the file viewer's hints, and last the `◆ N blocked`
  badge), then the optional panel-toggle pills as a set, then the pills'
  shortcuts (` Theme `), and finally their labels, leaving key-only chips
  (` F1 `) so the freed columns go back to the text. The armed-leader badge and
  the focus label are never dropped: the pills make room for them instead (see
  the next entry). The file viewer's navigation hints, previously
  right-aligned into whatever room was left of the buttons — where they
  overlapped the left-hand text rather than the pills — are segments in the same
  flow now, trimming from their tail (`n/N Next/Prev` goes long before
  `j/k Move`); they deliberately **outrank the counts**, because while the
  viewer is open they are the live guidance for the pane being driven and
  nothing else on screen carries them, where the session count is also in the
  sidebar.

- **The footer degrades at narrow widths instead of blanking.** The ladder above
  only trimmed the pills *after* the left-hand text had been shed entirely, so
  in two bands — 44–50 and 76–82 columns, **80** among them — a full set of
  chips sat above an empty left half: no focus label, no session count, nothing
  saying which pane the keys applied to. Four things upstream's `03828a0` does
  better were taken into the fork's own layout rather than adopting the commit:
  the pills now hold back the columns the armed-leader badge and the focus label
  need (`reserved_left_width`, capped at half the row so a long label can't
  starve the chips); a label-only rung sits between the tight and key-only forms
  so a squeezed chip reads ` Theme ` rather than ` F4 `; `Quit` outlives `Help`
  as the last chip standing, because at those widths it is the only one whose
  action still works (the help overlay needs room to render); and both
  the footer's left cluster and the status row end in `…` rather than being cut
  mid-word by ratatui. The result is that no width from 20 to 200 columns leaves
  the left half of the footer empty, guarded by a width-parameterised test.
  Upstream's version was not adopted for the failure modes it keeps: a blank
  footer at 1–5 columns, a left cluster collapsed to a lone `…` at 92, and
  span-by-span trimming that renders key chords without their descriptions
  (`^H/^L` → `^H/` → `^H`).

- **Widths are measured in display columns, not `char`s.** Upstream's
  `ui::truncate_ellipsis`, `button_width` and the footer's own width helpers all
  count `chars()`, so a double-width glyph — CJK or an emoji in a session or
  task title, a rebound shortcut — is budgeted one column and painted in two: a
  row that "fits" overruns its rect and shoves the chrome right. They measure
  `unicode-width` now (already a direct dependency, used by `ui::links`), and a
  glyph that would straddle a truncation is dropped whole rather than
  half-painted.

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
  still says `thurbox`: upstream **attribution** (`LICENSE`, provenance, the
  quality-gate badge) and upstream-owned surfaces the fork does not republish —
  upstream extension payloads, the `min_thurbox_version` manifest key, and the
  `tb-` / `tbs-` tmux window prefixes. See [Migration](#migration); upstream
  merges now carry rename conflicts on the renamed identifiers.
- **Own install surface (August 2026).** The fork stopped reusing upstream's
  installers and package channels, so nothing it ships installs a `thurbox`
  binary any more:
  - `scripts/install.sh` / `install.ps1` fetch `friring-*` archives from
    `bvc3at/friring` and install `friring` / `friring-cli`; the PowerShell
    installer's env vars are now `FRIRING_VERSION` / `FRIRING_INSTALL_DIR` /
    `FRIRING_REPO` / `FRIRING_PS_TEST` and its default install dir is
    `%LOCALAPPDATA%\Programs\friring`.
  - Self-update and version-check (`friring-cli update`, `version --check`,
    the header badge) query this repo's releases and replace `friring` /
    `friring-cli`; `cog.toml`'s changelog links resolve here too (upstream has
    none of these SHAs).
  - **Homebrew is the only package channel**, and this repo *is* the tap:
    `HomebrewFormula/friring.rb` at the root (Homebrew reads a tap's root,
    `Formula/` or `HomebrewFormula/`), installed with
    `brew tap bvc3at/friring https://github.com/bvc3at/friring`. The
    `publish-homebrew` job bumps it from the release checksums and commits it
    back to `main` — no tap repo, no secrets.
  - Upstream's AUR / Chocolatey / winget manifests and their publish jobs were
    **deleted**: they carry upstream's package identities (`thurbox`,
    `thurbox-bin`, `Thurbeen.thurbox`), which this fork cannot publish under.
    Upstream merges touching those paths now conflict as delete/modify.
- **Own website (August 2026).** `website/` was upstream's Thurbox site, kept
  dormant here; it is now Friring's own, published by `pages.yml` to
  **<https://bvc3at.github.io/friring>**:
  - The site is renamed throughout — prose, binaries, `~/.config/friring` /
    `~/.local/share/friring` paths, `FRIRING_*` env vars, repo links, demo
    videos, and the `fri`/`ring` wordmark. `logo-mark.svg`, `favicon.svg` and
    `og-image.svg` carry the fire-ring mark from `logo.svg` instead of
    upstream's shell-box-and-tree.
  - Its install docs match the install surface above: curl / PowerShell, the
    self-tap Homebrew pair, `cargo install --git`, and a source build. The AUR /
    Chocolatey / winget tabs and sections were removed with those channels.
  - **No custom domain.** Upstream's `website/CNAME` (`thurbox.thurbeen.eu`) and
    its Eleventy passthrough were dropped — `actions/deploy-pages` reads a CNAME
    out of the artifact and would claim that hostname. The site is subpath-clean
    (every link resolves through the per-page `root` depth variable), so it
    needs no Eleventy `pathPrefix` to serve from `/friring/`.
  - `pages.yml` lost its `github.repository == 'Thurbeen/thurbox'` guard and
    moved to `runs-on: k3s-arc` like every other fork-active Linux job.
  - **Pages from a private repo needs GitHub Pro** (Free allows Pages only from
    public repos). The published site is public either way — access-controlled
    Pages is Enterprise Cloud-only.
  - Upstream credit moved into the site content: the landing-page footer, an
    FAQ entry, and an `llms.txt` entry all point at `Thurbeen/thurbox`.
  - **Docs tables scroll in their own box, at every width.** Wide reference
    tables used to drag the whole page sideways. Upstream's fix puts
    `display: block; overflow-x: auto` on the table itself below 640px; the
    fork instead wraps each docs table in a `.table-scroll` box at build time
    (the `wrap-tables` Eleventy transform). The wrapper keeps real table layout
    — `display: block` reflows the rows through an anonymous table box and
    shrink-wraps them — and needs no breakpoint, which matters because these
    tables outgrow the prose column at intermediate desktop widths too, not
    only on phones.
  - **"On This Page" is generated, and stays in the sidebar.** It used to be
    restated in each page's `onThisPage` front matter — 17 of 21 pages carried
    one and the rest silently got none, including `features.html` with its 19
    sections. A `docs-toc` Eleventy transform now derives it from the rendered
    heading ids. Upstream moves the result into a third column at 1280px+ as
    part of its docs-layout rework; the fork keeps it where it already was, at
    the foot of the sidebar, so the change is the generation and not the
    layout. The fork's rule also falls back to the nearest enclosing block's
    id when a heading has none, which is what keeps the generated
    `ui-review.html` list intact — that page is emitted as
    `<div class="review-card" id="screen-N"><h3>…`, with the id on the card.
  - **Self-hosted fonts ship their licence.** The three web fonts are served
    from `website/assets/fonts/` rather than Google Fonts, as upstream does.
    The fork also ships `assets/fonts/OFL.txt` — the SIL Open Font License 1.1
    plus each family's copyright notice, read out of the font files' own name
    tables — because serving the `woff2` files is redistribution and the
    licence requires it to travel with them.
  - **No `overflow-x: clip` backstop.** Upstream guards residual sideways
    scroll with `body { overflow-x: clip }`. That declaration does nothing:
    overflow only propagates from `body` to the viewport for the values
    Chromium and WebKit actually propagate, and `clip` is not one of them — a
    page that overflows still drags sideways with the rule in place, in both
    engines. The fork fixes the causes instead (responsive display headings,
    breakable inline code, shrinkable `.step-content`, scrollable tables) and
    does not carry the rule. Putting it on `html` would work, but it would
    silently clip any future overflow out of reach rather than surfacing it.
  - **The shared stylesheets ship as one generated bundle.** Upstream links
    `variables`, `base`, `layout` and `components` separately, so the chrome
    every page needs costs four render-blocking requests. The fork keeps the
    four authored apart under `website/css/` and concatenates them — in that
    order, so the cascade is unchanged — into `_site/css/core.css` at build
    time (`eleventy.config.js`). The page-specific sheets (`landing`, `docs`,
    `ui-review`) are deliberately left unbundled, since bundling them would
    ship landing CSS to docs pages and vice versa. `core.css` is generated
    output: it is never edited, never passthrough-copied, and only the four
    sources are.
- `README.md` and the agent-guide prose call the project **Friring**, and so do
  the install commands; what still points at upstream is attribution and the
  shared formats above.
- A fork notice at the top of `README.md` explains the fork, the name, and what
  still points upstream.
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
here (while remaining merge-safe). The release (`cd.yml`) and website
(`pages.yml`) pipelines are the exceptions — the fork runs both. All build /
test / lint jobs run normally on the fork.

- `.github/workflows/pages.yml` (GitHub Pages) — **active on the fork.** Its
  upstream guard was dropped and it deploys `website/` to
  <https://bvc3at.github.io/friring> on any push to `main` touching `website/`
  or `docs/media/`. Publishing Pages from this **private** repo requires a
  GitHub Pro plan; the site it serves is public regardless.
- `.github/workflows/cd.yml` (Release) — **active on the fork, end to end.**
  Every push to `main` that includes a `feat` / `fix` / `perf` commit cuts a tag
  (`cog bump --auto`), publishes a GitHub Release with cross-platform
  `friring-*` binaries + a checksums file, and then bumps
  `HomebrewFormula/friring.rb` to that release and commits it back to `main`.
  All of it needs only the built-in `GITHUB_TOKEN`. Upstream's AUR / Chocolatey
  / winget publish jobs were deleted along with their manifests — those channels
  carry upstream's package identity and the fork has no accounts for them.
- `.github/workflows/ci.yml` — the `sonarqube` job is dormant; SonarQube is not
  set up for the fork at the moment. The `changes` (paths-filter) job also grants
  `pull-requests: read`, which a **private** repo's default token lacks (public
  upstream doesn't need it).
- **Linux CI/CD jobs run on the self-hosted `k3s-arc` runner.** Every
  fork-active Linux job in `ci.yml`, `cd.yml` and `pages.yml` targets
  `runs-on: k3s-arc` — an Actions Runner Controller scale set on k3s — instead
  of GitHub-hosted `ubuntu-latest` — including `cd.yml`'s `publish-homebrew`,
  whose formula bump needs `python3` on the runner. The upstream-only jobs stay
  on plain `ubuntu-latest` — they never run on the fork and upstream has no
  `k3s-arc` runner: `ci.yml`'s `sonarqube`. The Windows / macOS jobs and the
  release build matrix are unchanged — a Linux ARC runner can't service them.
- **`demo-pacing` job (fork-only).** Checks `docs/media/*.gif` against the
  pacing budget on any change under `docs/media/` or `scripts/demo/`. The media
  is recorded by hand on a workstation, so nothing else would catch a clip that
  regressed into holding a frozen frame: the asset is binary, so the diff shows
  nothing, and no reviewer plays ten gifs. It installs `ffmpeg` for the
  opening-hold metric only — without it the job still gates on the held-frame
  and size budgets and says loudly that the opening went unchecked, rather than
  passing quietly. See [Demo pacing budget](#demo-pacing-budget-libcheck-pacingmjs-wait-in-the-tapes).

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
   The Windows installer's variables moved too (August 2026): `THURBOX_VERSION`,
   `THURBOX_INSTALL_DIR`, `THURBOX_REPO`, `THURBOX_PS_TEST` → `FRIRING_*`. The
   release build already used `FRIRING_RELEASE_VERSION`.

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

8. **Self-update** — self-update and version-check now track *this* fork's
   releases and replace `friring` / `friring-cli`, so an installed release keeps
   itself current. A source build reports `0.0.0-dev` and is skipped; update it
   by pulling this repo and rebuilding. A `version-check.json` cache left over
   from the upstream endpoint is ignored and refetched, so no stale upstream tag
   is reported after the switch. Remove the old `thurbox` binaries
   (`rm ~/.local/bin/thurbox ~/.local/bin/thurbox-cli`) once nothing needs them
   — nothing prunes them for you.
