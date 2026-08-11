# Architecture Decisions

Each decision follows a mini-ADR format:
**Choice**, **Why**, **Rejected alternatives**.

---

## ADR-1: The Elm Architecture (TEA)

**Choice**: All state lives in a single `App` model.
Events become messages, `update()` applies them,
`view()` renders the result.

**Why**: TEA makes state transitions explicit and testable.
Every input has a traceable path from event to screen change.
There's no hidden state scattered across components, which matters
when multiple PTY sessions are producing concurrent output.

**Event loop** (`run_loop` in `main.rs`): `tokio::main` loads the
`AgentRegistry` (`agents.toml`, ADR-19), initializes the `BackendRegistry`
(the local `local-tmux` backend plus one lazily-registered backend per
`hosts.toml` host, ADR-13), opens the SQLite DB (ADR-8), initializes the
terminal, then spawns/restores sessions before entering the loop. Each
iteration draws a frame (demand-driven, so an idle screen is not
repainted — `docs/PERFORMANCE.md` ADR-P1), polls crossterm events
(~10 ms), converts each into an `AppMessage`
(`KeyPress`/`Resize`/mouse/paste), applies it via
`App::update` → `handle_key`/`handle_resize`, then runs `App::tick`
(status derivation, timer expiry, background-task polling). On exit
`App::shutdown` **detaches** sessions rather than killing them (tmux keeps
them alive across restarts, ADR-2/ADR-12); the terminal is then restored.
A panic hook restores the terminal first so a crash never strands the user
in raw mode, and all logging is file-based since the TUI owns stdout
(ADR-6).

**Rejected**:

- *Component-based (each panel owns state)* — leads to
  synchronization bugs when sessions interact.
- *Ad-hoc event handlers* — untraceable control flow;
  hard to reason about as the app grows.

---

## Module responsibilities

A per-module map of where each responsibility lives; where an ADR owns the
design it is cross-referenced, not restated. The crate is layered around a
pure `session` data core, with `agent` (side effects) and `ui` (rendering)
between it and the `app` coordinator.

- **`app/`** — the TEA Model + Update + View (ADR-1): the `App` struct
  (all state), the `AppMessage` enum + `handle_key`/`handle_resize`
  (Update), and `view` (render). Owns state and coordinates every side
  effect; decomposed into per-domain sub-files under `src/app/` while the
  spine stays on `App` (ADR-22).
- **`agent/`** — the side-effect layer. `AgentProvider` /
  `GenericProvider` build the launch argv from a declarative `AgentDef`
  (ADR-19); `Session` wraps a `SessionBackend` (ADR-11) held in a
  `BackendRegistry` keyed by name; `TmuxBackend` runs tmux over a
  `TmuxTransport` (`transport.rs` — `Local`/`Ssh`/`Wsl`, ADR-12/ADR-13)
  speaking control mode (`control_mode.rs`). Output is parsed into an
  `Arc<Mutex<vt100::Parser>>` on a `spawn_blocking` reader; input is
  written over an mpsc channel (ADR-3), translated from crossterm
  `KeyCode` to xterm ANSI by `input.rs` (ADR-4).
- **`sandbox/`** — isolation boundaries: host probing, policy generation
  (SBPL / bwrap argv) and the wrap of a composed launch. Same tier as
  `agent`, which depends on it and never the reverse (ADR-25/ADR-26 in
  [`SANDBOX.md`](SANDBOX.md)). It also owns the per-session egress-proxy
  lifecycle — `sandbox::egress` starts, replaces and stops one instance per
  sandboxed session — which is why it depends on the leaf `proxy` module,
  one way only.
- **`proxy/`** — the egress filtering proxy (HTTP CONNECT + SOCKS5, TCP and
  unix transports), a self-contained leaf that enforces the policy it is
  handed (ADR-27 in [`SANDBOX.md`](SANDBOX.md)).
- **`session/`** — plain data types, the dependency sink (no
  crate-internal references): `SessionId`, `SessionStatus`, `SessionInfo`
  (carries the `agent` name), `SessionConfig` (agent/backend names, ids,
  cwd, env), `AgentDef`/`AgentRegistry` (ADR-19), and `HostDef` /
  `HostRegistry` / `HostKind` (ADR-13) — mostly `Display`/`Default` impls
  plus the agent-arg substitution logic.
- **`ui/`** — pure rendering functions, no side effects. `layout.rs`
  computes the responsive panel areas (ADR-5); widgets include
  `project_list` (its `compute_session_order` is the single comparator
  shared with `App`'s `Ctrl+J/K` navigation — ordered by `display_order`,
  grouped by repo, never by status; `move_in_order` is the pure reorder
  behind `Shift+J/K`), `terminal_view`, `info_panel`, `status_bar`,
  `repo_picker_modal`, `agent_picker_modal`; `selection.rs` drives
  mouse-drag text selection and `links.rs` detects clickable URLs. Colors
  are centralized in `theme.rs` (ADR-14).
- **`cli/`** — `friring-cli` subcommand dispatch (headless session ops +
  scheduling + the editor command), sharing the SQLite DB with the TUI but
  never importing `app`/`ui` (ADR-15). `friring-cli sandbox relay` is the
  one subcommand that runs *inside* a boundary, and is dispatched before
  the database is opened (ADR-29 in [`SANDBOX.md`](SANDBOX.md)) — which is
  why `cli` may reference `proxy`. The rest of `friring-cli sandbox` is
  host-side management — profiles, places, prune, export/import, keychain
  tokens — and asks the same layer the TUI asks, reaching `crate::sandbox::…`
  by fully-qualified path only, never `use`.
- **`activity/`** — agent-neutral activity: which provider reads a
  session's records, where each agent CLI keeps them, and the incremental
  stat-gated scan that turns them into the `session::activity` event
  stream. Split out of `app` so `cli` can reach it; `app::activity` keeps
  only the scan scheduling and the F9 view's row building (ADR-24).
- **`proctable/`** — the platform process-table read (procfs walk / one
  `ps` / `sysinfo`) behind per-session memory. Shared by `app::memory` and
  `friring-cli session resources` (ADR-24); the parent→child index and
  subtree sum stay pure in `session::memory`.
- **`usage/`** — account-level rate-limit fetches per `(agent, host)`,
  reading each vendor's credentials wherever the agent is logged in.

**Mouse routing (per-frame click registry).** Mouse input is unified with
the keyboard through one per-frame registry (`App::click_targets`,
mirroring the `scrollbar_hits` registry): list/modal/button renderers
return `ui::RowHitbox`es / `ui::ButtonHit`es, `App::view` records them as
`ClickAction`s, and `handle_mouse_click` / `handle_modal_click` hit-test
them. A modal-button click **replays the paired key** through the modal's
own handler, so a click always follows the exact keyboard path. Clickable
"pill" buttons (`ui::render_button_bar`) draw the status-bar footer
(Help/Info/Files/Theme/Tasks/Settings/Quit — feature-gated, and
responsively degraded so the pills and the text left of them never share a
column: separators, then the text segment by segment, then the optional
panel toggles as a set, then the labels themselves — see the ladder on
`ui::status_bar::render_footer`) plus every modal's action buttons
(`ui::ModalButtons`). The `ClickAction`
variants (`Global`, `ModalButton`, `ModalField`, `PaneField`, `RepoFocus`,
`CentralTab`, …) select or toggle what was clicked — a Settings bool row
toggles on click, scalar rows only select. The whole subsystem is gated by
`[features] mouse`: disabled, mouse capture is never enabled and the
terminal keeps native mouse behavior.

**Central-pane tab strip.** The agent terminal, per-session shell, code
review, and Claude Code activity view share the central pane, surfaced as
a clickable tab strip painted on the pane's top border by
`App::draw_central_tabs` (each tab a `ui::render_pill`; the active view is
the accent-filled "primary" pill). `central_tab_cells` lays out the
on-border hitboxes, recorded as
`ClickAction::CentralTab(CentralTab::{Agent,Shell,Review})` **before** the
pane's whole-rect focus fallback so a tab click wins; a click runs
`App::select_central_tab`, which *selects* a view (distinct from the
keyboard `Ctrl+T`/`Ctrl+X` *toggles*). Each tab shows its toggle's F-key
hint, because a focused terminal passes `Ctrl+<letter>` chords through to
the CLI while the F-key dispatches in every pane; Shell/Review tabs are
feature-gated.

**Enforcement.** `tests/architecture_rules.rs` is an **allowlist** (the
dependency table itself lives in `AGENTS.md`): every module under `src/`
must declare a `ModuleRules` entry naming the crate modules it may
reference — in *any* form (`use`, `pub use`, brace groups,
fully-qualified `crate::…` paths) — and a new module fails the test until
its place is declared. `ui → app` is the deliberate TEA `view(model)`
coupling (ui renders `app`-owned modal/status state but triggers no side
effects); `session_ops` and `cli` may reach `crate::agent::…` via
fully-qualified paths **only** (never `use`) so the headless→backend
dependency stays visible at each call site. `app` is EXEMPT — the
coordinator imports every layer (ADR-22).

---

## ADR-2: Session pipeline — SessionBackend + vt100 + tui-term

**Choice**: A `SessionBackend` trait abstracts session lifecycle
(spawn, adopt, resize, kill, detach, discover). Each session runs
one coding-agent CLI inside the backend. The default backend is
local tmux (`tmux -L friring`); the same `TmuxBackend` also runs
over SSH for remote hosts (ADR-13).
`vt100::Parser` interprets escape sequences,
`tui_term::PseudoTerminal` renders the parsed screen into ratatui.

**Why**: The trait-based design keeps the session transport
behind a clean boundary so the app layer never touches tmux
directly. tmux provides truly persistent sessions
that survive friring crashes/restarts, multiple friring instances
share the same running sessions, and external recovery is
possible via `tmux -L friring attach`.

**Previous design**: `portable-pty` spawned the agent CLI
directly. Sessions died when friring exited, terminal content was
lost on restart, and multiple instances had no coordination.

**Which vt100**: not the crates.io one. Stock `vt100` 0.16.2 discards
any line that scrolls off the top of the screen while a `DECSTBM`
scrolling region is set — even a region anchored at row 1, where
xterm, tmux and iTerm2 all keep it. That is exactly how ratatui's
*inline* viewport grows a transcript on the normal screen, so an
agent built on it (Codex CLI) left the pane's scrollback empty
forever: `Shift+Up`, the wheel and the scrollbar were silent no-ops.
Friring resolves `vt100` to `panoptes-vt100`, which relaxes that one
condition to "the region starts at row 1", through a
`[patch.crates-io]` entry — `tui_term` renders a `vt100::Screen`, so
both crates must resolve to the *same* vt100 or the types don't
unify. `[patch]` matches on package name and cannot rename, hence the
one-line re-export crate at `vendor/vt100/` that carries the required
name. The rule is pinned by `inline_viewport_scrollback` in
`src/agent/backend.rs` and end-to-end by the `codex-scrollback`
scenario; retire both the patch and `vendor/vt100/` if upstream ever
ships the fix.

**Rejected**:

- *`portable-pty` (previous)* — no session persistence,
  no multi-instance sharing, terminal content lost on restart.
- *`alacritty_terminal`* — full terminal emulator,
  far heavier than needed.
- *Parsing raw ANSI ourselves* — error-prone,
  massive surface area, already solved by `vt100`.
- *Reading scrollback back out of tmux* (`capture-pane -S -N` into a
  second parser, rendered while scrolled) — no dependency games, and
  it would paper over any emulator gap, not just this one. Rejected
  as a second render path with its own resize / live-output /
  remote-host edge cases, against a one-condition fix upstream of all
  of them.
- *Vendoring the whole patched emulator in-tree* — full control, but
  ~4k lines of third-party code to re-patch on every bump. The
  re-export shim keeps the emulator itself on crates.io, versioned
  and checksummed like any other dependency.

---

## ADR-3: Async — tokio multi-threaded + spawn_blocking

**Choice**: The app runs on tokio's multi-threaded runtime.
PTY read loops run inside `spawn_blocking`
(blocking I/O in a threadpool), while PTY write and event handling
run in `tokio::spawn` (async).

**Why**: PTY reads are blocking by nature
(`read()` on a file descriptor). Putting them in `spawn_blocking`
prevents stalling the async executor. The writer side is naturally
async — it awaits messages from an mpsc channel
and writes when they arrive.

**Generalized off-the-hot-path pattern**: the same
`spawn_blocking` → `mpsc` → poll-in-`tick()` shape keeps every other
blocking side effect off the UI thread, so neither rendering nor
`Ctrl+N` ever freezes. Each operation owns an in-flight guard + result
receiver on `App`, kicks off the blocking work, and applies the result
when `tick()` polls `try_recv()`:

- **Worktree sync** (`Ctrl+S`) — `git rebase` per worktree
  (`worktree_sync_rx`, the original instance of the pattern).
- **Per-tick metrics** — `refresh_system_metrics` (sysinfo + statusline
  file reads + the active pane's PID lookup) and `refresh_active_git_stats`
  (`git` diff/status shell-outs). The `sysinfo::System` is *moved into*
  the worker and returned with the result so CPU deltas persist across
  refreshes; a single in-flight guard prevents overlap.
- **Interactive spawn** — `git worktree add` (`spawn_worktree_session`)
  and `Session::spawn` (PTY/tmux window creation, 500 ms+) for the
  new-session wizard run on blocking tasks, with the follow-up
  (session adoption, task-prompt delivery) carried in a `Pending*`
  continuation applied on completion. Programmatic spawns
  (automations/tasks, restore) stay **synchronous** — they read the new
  session's id straight back, so they cannot defer it to a later tick.
- **Automation `exec`** — the one deliberate exception to the shape: the
  tick records a `running` run row and hands the command to a *detached*
  `std::thread` that opens its own `Database` connection and closes the row
  out itself, because the run outlives the tick that started it and no
  result has to reach the model. (The headless `automation tick` runs it
  inline instead — a short-lived process that detached would exit and
  strand the row.) A worker that dies with its process is recovered by
  `reap_orphaned_automation_runs`; see `docs/FEATURES.md`.

**Rejected**:

- *Single-threaded tokio* — PTY reads would block the entire
  runtime, freezing the UI.
- *`std::thread` for everything* — works but loses tokio's
  structured concurrency, select!, and channel ergonomics.

---

## ADR-4: Input translation — crossterm KeyCode to xterm ANSI

**Choice**: `input.rs` maps crossterm `KeyCode`/`KeyModifiers`
to raw xterm ANSI byte sequences before writing to the PTY.

**Why**: crossterm gives us structured key events.
PTYs expect raw bytes. The translation layer is explicit and
testable — each key has a known byte sequence, and edge cases
(arrow keys, function keys, modifier combos)
are handled in one place.

**Rejected**:

- *Raw passthrough (forward crossterm's raw bytes)* —
  crossterm's internal byte representation doesn't match xterm
  sequences. Modifier keys, in particular, would break.

---

## ADR-5: Responsive layout breakpoints

**Choice**: Three layout tiers based on terminal width:

- `<80 cols` — terminal panel only (full screen)
- `>=80 cols` — two panels (left panel + terminal)
- `>=120 cols` — three panels (left panel + terminal + info)

The left panel is a single session list.

**Why**: 80 columns is the smallest usable terminal width. Below
that, showing a sidebar wastes too much space. At 120+, there's
room for supplementary info without shrinking the terminal panel
below readable width. Fixed breakpoints are predictable — the
layout never "jitters" near a threshold.

**Rejected**:

- *Fixed layout (always 3 panels)* — unusable on small terminals.
- *User-configurable breakpoints* — premature complexity.
  Can be added later if needed.

---

## ADR-6: File-based logging only

**Choice**: All tracing output goes to
`~/.local/share/friring/friring.log`.
Nothing writes to stdout or stderr.

**Why**: The TUI owns stdout entirely. Any stray `println!` or
log line to stdout would corrupt the terminal display. File-based
logging also makes it easy to `tail -f` the log in a second
terminal while developing.

**Rejected**:

- *Stderr logging* — crossterm's alternate screen captures stderr
  on some platforms, still risks display corruption.
- *In-app log panel* — useful eventually, but adds complexity
  before the core features are stable.

---

## ADR-7: Build profiles

| Profile | `opt-level` | LTO | Strip | Debug | Use case |
|---|---|---|---|---|---|
| `dev` | 0 | off | no | yes | Fast iteration |
| `test` | 1 | off | no | yes | Faster tests, still debuggable |
| `release` | 3 | full | yes | no | Distribution binary |
| `release-with-debug` | 3 | full | no | yes | Profiling / flamegraph |

**Why**: `test` at opt-level 1 catches optimization-dependent bugs
earlier while keeping compile times reasonable. The release profile
strips everything for a minimal binary. `release-with-debug` exists
specifically for `perf` / `flamegraph` workflows.

---

## ADR-8: State storage — SQLite

**Choice**: All persistent state (sessions, worktrees,
automations) is stored in a single SQLite
database at `~/.local/share/friring/friring.db` (respects
`$XDG_DATA_HOME`). WAL mode enables concurrent multi-instance
access. Agent definitions are the one exception: they live in a
human-editable TOML file (see ADR-19), not the database.

*This supersedes the original TOML file-based approach
(`~/.config/friring/config.toml`), which was eliminated after
the SQLite migration.*

**Why**: SQLite provides atomic transactions, concurrent access
via WAL mode, and a single source of truth. Multi-instance sync
uses `PRAGMA data_version` polling (see ADR-7b). The TUI provides
all editing UI — there is no need for a human-editable config file.

Every connection sets a **5 s busy_timeout** (the DB is shared by
the TUI, `friring-cli`, and the automation heartbeat; writes are
short single-row upserts, so a bounded wait beats an immediate
`SQLITE_BUSY` error or an unbounded freeze) plus the WAL-friendly
performance pragmas `synchronous = NORMAL`, `cache_size`, `mmap_size`,
and `temp_store = MEMORY` (`storage::schema::initialize`; rationale in
`docs/PERFORMANCE.md` ADR-P6). The append-only
**audit log is pruned to 90 days** on `Database::open` — entries
are debugging breadcrumbs, not compliance data, and unbounded
growth would bloat the database over months of use.

**Rejected**:

- *TOML config file (previous)* — race conditions when multiple
  instances write concurrently; split source of truth between
  config.toml and state files (sessions); no atomic multi-key
  updates. (Agent definitions are read-mostly and not subject to
  concurrent writes, so they remain in TOML — see ADR-19.)
- *JSON* — verbose for config, no atomic writes without
  temp-file-rename pattern.
- *CLI flags only* — doesn't scale to multiple sessions and
  long-lived configuration.
- *Embedded in CLAUDE.md* — mixes repo-specific AI guidance with
  application configuration; wrong separation of concerns.

---

## ADR-8b: Automations fire with or without the TUI

**Choice**: Automations fire from three places that all funnel
through one headless entry point, `friring-cli automation tick`:
the TUI tick loop, a detached **tmux heartbeat keeper** window
(`automation-heartbeat`, armed on TUI startup and on `automation
create`, looping `tick` every 60 s), and optional systemd/launchd
units (`packaging/`) for reboot-proof firing. Concurrency is made
safe by **claim-based firing** — `Database::claim_due_automation`
advances `next_run_at` with an atomic compare-and-swap, so exactly
one firer wins per due automation.

**Why**: The previous one-shot "scheduled command" fired even with
the TUI shut down by riding tmux's `run-shell` timers; the new
model must keep that durability for recurring + spawn automations.
A live keeper window both runs the heartbeat and keeps the tmux
server alive (a bare pending `run-shell` job does not), so even
spawn-only automations fire with no other sessions. Claim-first
ordering gives at-most-once semantics (a crash loses a run rather
than double-firing), the right default for agent prompts.

Dispatch is **host-aware**: a `spawn` action resolves its `hosts.toml`
host into an `agent::tmux::MuxTarget` (transport + socket + group
session + that host's multiplexer binary), used for both the headless
window lookup and the deferred prompt delivery, so a remote automation
creates its session *and* is prompted on the right machine (an unknown
host is an error before the spawn, never a silent local one). A
*reused* spawn session is delivered over the backend it was created
on. A `send` follows the **target session's own** `backend_type`
(`MuxTarget::for_backend`), so a session started on a remote host is
reached there rather than typed at the local server — and the TUI and
the headless tick agree about the same automation instead of the
outcome depending on which firer won the claim. A backend naming a
host that is no longer in `hosts.toml` is an error run, never a
delivery to the wrong machine.

**Rejected**:

- *Per-automation `run-shell` timers (old style)* — precise to the
  second but require bookkeeping + re-arming N timers on startup; a
  single polling keeper is simpler and naturally handles
  create/edit/delete.
- *A bespoke long-running daemon* — duplicates what tmux (already
  required) and systemd/launchd provide; more moving parts.

---

## ADR-9: Flat session list (no project grouping)

**Choice**: The sidebar is a single flat list of sessions. There
is no "project" layer above sessions: each session picks its own
agent and repo selection at creation time.

**Why**: Earlier versions grouped sessions under projects (one
project → many sessions, with shared repos). In practice users
created one session per task, so the project layer was pure
overhead — an extra navigation level, an extra creation step, and
an extra deletion guard. Storage migration v16 dropped the
`projects`, `project_repos`, `project_vm_config`, and
`project_container_config` tables and removed `project_id` columns
from `sessions`, `vms`, and `containers`.

**Rejected**:

- *Two-section sidebar (projects on top, sessions on bottom)* —
  the previous design. Cost a navigation level and a creation
  step for no gain in the typical one-session-per-task workflow.
- *Modal/popup project selector* — hides context while working,
  forces re-opening to switch.
- *Tabs for projects* — horizontal tabs consume vertical space
  and don't scale well past 4-5 entries.

---

## ADR-11: Trait-based session backends

**Choice**: Session lifecycle is abstracted behind a
`SessionBackend` trait (`src/agent/backend.rs`). The `Session`
struct wraps the trait and manages reader/writer loops once,
regardless of which backend is active.

**Why**: Keeping session lifecycle behind a trait boundary leaves
the app layer completely backend-agnostic. The backends today are
local tmux and one SSH backend per configured host (both
`TmuxBackend` over a `TmuxTransport`; see ADR-13), and the seam means
the transport can evolve without touching `App`, `Session`, or any UI
code.

**Trait methods**: `check_available`, `ensure_ready`, `spawn`,
`adopt`, `discover`, `resize`, `is_dead`, `kill`, `detach`.

**Key design decisions**:

- `spawn()` returns `(backend_id, output_reader, input_writer)`.
  The `Session` struct owns the reader/writer loops.
- `adopt()` reconnects to an existing session and returns initial
  screen content for parser seeding.
- `discover()` lists existing sessions for restore-on-startup.
- `detach()` stops streaming without killing the session.
- `kill()` permanently destroys the session.

**Rejected**:

- *Async trait methods* — added complexity for no benefit since
  the tmux backend uses synchronous `Command::new("tmux")`.
  Can be added via `async-trait` if a future backend needs it.

---

## ADR-12: Local tmux as default backend

**Choice**: The default `SessionBackend` is `TmuxBackend`
parameterized over its `Local` transport (`TmuxTransport::Local`)
and registered as `local-tmux`, using a dedicated tmux server
(`tmux -L friring`) with session name `friring`. All I/O goes
through tmux control mode (`-C`). (The transport abstraction that
also enables remote SSH backends is ADR-13; here the choice is
simply that the out-of-the-box backend runs tmux locally.)

**Why**: tmux provides session persistence (survives crashes),
multi-instance support (multiple friring processes can independently
interact with the same sessions), and external recovery
(`tmux -L friring attach`). It handles terminal capability queries
(DA1/DA2) natively via `extended-keys on`, eliminating the need for
friring to intercept and respond to these sequences.

Control mode (`-C`) supports multiple concurrent client connections,
each receiving independent output streams. Each friring instance
establishes its own control mode connection, allowing all instances
to simultaneously monitor and interact with the same tmux sessions.
Output arrives as `%output` notifications (octal-encoded), input is
sent via `send-keys -H` (hex-encoded). This eliminates the previous
`pipe-pane` + FIFO approach which suffered from tmux data-loss
bugs (#641, #2989), required 3 external deps in the data path
(`mkfifo`, `stdbuf`, `cat`), and had no flow control.

**Configuration on init**:

- `remain-on-exit on` — keeps panes alive after process exit
- `status off` — no tmux status bar (friring renders its own)
- `default-terminal xterm-256color` — standard terminal type
- `history-limit 5000` — reasonable scrollback
- `extended-keys on` — enhanced key reporting
- `extended-keys-format csi-u` — the modern, unambiguous format some agents
  (e.g. `pi`) probe for at startup; friring injects keys via `send-keys` so this
  only sets the reported format, not the bytes agents receive. Best-effort: the
  option is tmux 3.3+ while friring's floor is 3.2, so a 3.2 host silently skips it
- `window-size manual` — windows size independently
- `pause-after 5` — flow control (auto-resumed by reader)

**Window naming**: `tb-<session-name>` prefix for discovery. The sanitized
window name is the authoritative identity for re-adoption; a persisted pane id
is a per-server cache used only to choose among windows sharing that name,
because tmux re-allocates `%N` per server lifetime. One pane may be claimed by
at most one session per restore sweep.

**Output streaming**: `%output` notifications from control mode,
demultiplexed by pane ID into per-pane broadcast channels. Multiple
instances can simultaneously register the same pane; output is
broadcast to all registered channels via `HashMap<String, Vec<SyncSender>>`.
Each channel feeds a `ControlModeReader` (implements `Read`) consumed
by the existing `Session::reader_loop`. This allows multiple instances
to independently parse and render terminal state in real-time.

**Input**: `send-keys -H <hex>` through the shared control mode
stdin, wrapped in a `ControlModeWriter` (implements `Write`).

**Command synchronization**: All commands that precede a
`send_command` (waited) call must themselves be waited. A
fire-and-forget (`send_command_nowait`) leaves an unclaimed
`%begin`/`%end` response in the stream that can steal the next
waiter. `send_command_nowait` is only safe when nothing follows
(e.g., `detach`) or when issued from the reader thread itself
(e.g., pause resume).

**Session restore**: On reconnect (`TmuxBackend::adopt`),
`capture-pane -e -p -J -S -<scrollback_lines>` seeds the fresh
vt100 parser with the pane's scrollback history **and** visible
screen (text + colors; `-J` rejoins wrapped lines so they re-wrap
at the new width). Without this seed the parser starts empty and
a session's pre-restart history cannot be scrolled in the UI —
the `%output` stream only carries bytes emitted after connect. A
forced resize then triggers SIGWINCH, causing the TUI application
to repaint its visible screen through the normal `%output` stream
— this delivers pixel-perfect rendering of the live region on top
of the seeded history. Seeding is best-effort: a failed capture
logs a warning and adoption proceeds with an empty seed.

**Rejected**:

- *`pipe-pane` + FIFO (previous)* — intermittent data loss from
  tmux bugs #641/#2989, required `mkfifo`/`stdbuf`/`cat` in the
  data path, no flow control, timing race on initial capture.
- *Screen/dtach* — less widely available, fewer features.

---

## ADR-13: Off-local sessions via an SSH / WSL tmux transport

**Choice**: Run agent sessions on a remote host (over SSH) or in a
local WSL distro (via `wsl.exe`) by launching the same tmux
control-mode protocol behind a launch prefix. `LocalTmuxBackend` is
generalized into `TmuxBackend { transport, socket, session, name }`
where `transport: TmuxTransport` is `Local` (a bare
`Command::new("tmux")`), `Ssh { destination, ssh_opts, mux }`
(`ssh <dest> <mux> …`), or `Wsl { distro, mux }`
(`wsl.exe -d <distro> <mux> …`). `mux` is the host multiplexer binary
(`tmux` by default, or `psmux` for a Windows SSH host; a WSL distro
runs `tmux`). The transport's *only* job is to build the `Command`;
everything downstream — the control-mode reader/writer threads, pane
registration, `send-keys`/`%output` — is byte-for-byte identical
(`control_mode.rs` was already transport-agnostic). The SSH and WSL
arms share `TmuxTransport::prefixed`, since both join + shell-interpret
the trailing POSIX-quoted tokens identically; only the launcher prefix
differs.

Hosts are declared as data in `~/.config/friring/hosts.toml`
(`session::HostDef { kind: HostKind {Ssh, Wsl}, … }`/`HostRegistry`),
and WSL distros are additionally **auto-discovered** on Windows
(`agent::host_config::discover_wsl_hosts` via `wsl.exe -l -q`). The
combined set is loaded by `agent::host_config::load_all`, each
registered as a backend named `ssh:<host>` / `wsl:<distro>` via
`TmuxBackend::from_host`.

**Why WSL = "SSH without the ssh"**: `wsl.exe` runs `tmux`, `git`, the
agent, and the worktrees all *inside* the distro at native Linux paths,
so there's no Windows↔Linux path translation (`wslpath`) and the
worktree layout matches the SSH path exactly. Modeling WSL as a host
kind (rather than a per-session "run in WSL" flag wrapping a native
psmux pane) reuses the entire remote-host subsystem — picker,
persistence/restore, `git::*_on`, headless `--host` — for free.

**Why** (general): The local-vs-off-local difference is exactly one
line (how the tmux process is launched). The per-session control
commands travel over the stdin pipe, not the launcher argv, so only the
one-time `attach-session` launch crosses the boundary. SSH relies on
the system `ssh` binary + `~/.ssh/config` for auth/keys/multiplexing;
WSL needs no credentials at all.

**Key design decisions**:

- **Lazy registration**: off-local backends are registered but *not*
  connected at startup (`check_available`/`ensure_ready` deferred to
  first use via `App::backend_for`), so a down host (or slow WSL
  discovery) never blocks the TUI.
- **Auto-discovery**: WSL distros appear with zero config; an explicit
  `kind = "wsl"` entry of the same name wins (for overrides like
  `worktrees_dir`). `discover_wsl_hosts` decodes `wsl.exe`'s UTF-16LE
  output and is a no-op off Windows / without `wsl.exe`.
- **Selection**: `SessionConfig.backend` (`ssh:<host>` / `wsl:<distro>`
  or `None`); `is_remote_backend` covers both. The TUI shows a host
  picker as the first new-session step (skipped when none configured/
  discovered); `friring-cli session create --host` is the headless
  equivalent.
- **Persistence/restore**: `backend_type` round-trips in SQLite;
  restore discovers windows **per backend** so off-local sessions
  re-adopt against their own host's tmux.
- **Off-local worktrees**: `git::*_on(host, …)` run git via
  `git::host_launcher` (`ssh …` or `wsl.exe …`). Worktree paths resolve
  under the host's `worktrees_dir` (or `$HOME/.local/share/friring/…`
  resolved + cached, keyed by backend name since a WSL host has no
  `destination`).

**Module placement**: `HostDef`/`HostRegistry`/`HostKind` live in
`session/` (the dependency sink) so both `agent` (builds the backend)
and `git` (runs git on the host) can depend on them without violating
the module-isolation rules.

**Riskiest area**: SSH reconnect on a flapping link — `reconnect_control`
reopens the ssh connection; ControlMaster + keepalives mitigate
stalls. Worth the most manual testing.

**Rejected**:

- *A `TmuxTransport` trait with `Box<dyn>`* — an enum with two
  variants is simpler; promote to a trait only if a third transport
  (e.g. container exec) appears.
- *Embedded SSH library (russh, etc.)* — reimplements `~/.ssh/config`,
  agent forwarding, and multiplexing that the system `ssh` already
  provides.

---

## ADR-7b: Multi-Instance Sync — SQLite with PRAGMA data_version

**Choice**: Multiple friring instances synchronize all state
(sessions, worktrees, automations)
via a shared SQLite database
(`~/.local/share/friring/friring.db`). Each instance polls
`PRAGMA data_version` to detect external changes. SQLite's WAL mode
handles concurrent access safely. Deletions use soft delete
(`deleted_at` column).

*This supersedes the original TOML file-based approach. The migration
to SQLite resolved race conditions where concurrent `save_state()` calls
could overwrite each other's writes.*

Session **I/O is NOT coordinated** via the database. Instead, each
instance independently connects to tmux and adopts all visible sessions.
Tmux natively handles concurrent clients: output is broadcast to all
connected clients, and input commands are serialized. This enables true
multi-instance collaboration without application-level locks or
ownership restrictions.

**Why**: This approach is:

- **Atomic**: SQLite transactions prevent torn writes and race conditions
- **Portable**: Works on Linux, macOS, any system with a filesystem
- **TEA-compatible**: External changes flow through the message pipeline
- **Graceful**: Single instance has zero polling overhead
- **Collaborative**: All instances can interact with the same sessions
  simultaneously (like tmux attach with multiple clients)
- **Single source of truth**: No split-brain between state files and DB

**Multi-Instance I/O Model**: Rather than using an ownership model
to prevent duplicate I/O, each instance maintains its own control mode
connection to tmux. Tmux's architecture already supports this:

- Each control mode client receives independent output streams
- Output is duplicated by tmux to all connected clients
- Input commands (`send-keys`) are serialized by tmux
- No application-level coordination needed

This design choice (post-ADR) was made to enable true collaboration while
avoiding the complexity of application-level locks or message-passing for
I/O coordination.

**Trade-offs**:

- **Not human-readable**: Unlike TOML, users cannot directly edit state.
  The TUI provides all editing UI (session creation, scheduling, theme
  selection). Agent definitions are the deliberate exception and remain
  hand-editable TOML (ADR-19).
- **Independent terminal state**: Each instance maintains its own
  `vt100::Parser`, so concurrent updates may briefly diverge. Instances
  converge quickly as output is replayed.
- **Concurrent input interleaving**: When multiple users type
  simultaneously, characters arrive in order at tmux but may display
  interleaved (same as `tmux attach` with multiple clients). This is
  **expected behavior** for multi-user terminal sessions.

**Rejected**:

- *Event-based sync (inotify/kqueue)* — platform-specific, requires
  different implementations for Linux/macOS/BSD, more complex error
  handling (file deletion, permission issues), adds monitoring
  overhead even for single-instance deployments.
- *gRPC/REST daemon* — requires deploying and managing a persistent
  service, adds operational complexity, increases failure surface area
  (daemon crashes, socket issues), incompatible with offline usage.
- *Git-based sync* — requires git repo for state, introduces gc/
  rebase issues, incompatible with non-repo environments.
- *TOML file-based sync (previous approach)* — race conditions when
  multiple instances write concurrently; no atomic multi-key updates;
  split source of truth between config.toml and state files
  (sessions) caused sync bugs.

---

## ADR-15: Headless CLI as Separate Binary

**Choice**: Headless automation lives in a separate binary
(`friring-cli`) that shares the same SQLite database as the TUI.
It exposes `session`, `automation`, `task`, `message`, `editor`,
`config`, `extension`, `version`, `update`, and `notify` management
as subcommands, printing JSON results.

**Why**: A separate binary keeps scripting/automation out of the
TUI's event loop. The TUI already polls `PRAGMA data_version`
on every tick (~10 ms event-loop cadence) (ADR-7b), so changes
made by `friring-cli` appear
automatically — no new synchronization mechanism is needed. The
`cli` module imports `storage`, `session`, `session_ops`, `sync`,
and `agent::tmux`, but never `app` or `ui`, so it can operate
without a terminal UI.

**Rejected**:

- *Embedded in the TUI binary* — would force the TUI to multiplex
  a non-interactive command path alongside its crossterm event
  loop.
- *A long-running daemon* — adds operational complexity; the
  shared SQLite DB plus tmux already provide the coordination a
  one-shot CLI needs.

---

## ADR-14: Centralized Theme Module

**Choice**: All UI colors are defined as associated constants on a
`Theme` struct in `src/ui/theme.rs`. Widget files import `Theme::*`
instead of using `Color::Cyan`, `Color::Gray`, etc. directly.

**Why**: ~50 hard-coded color values were scattered across 13+ widget
files. This made visual consistency difficult to maintain and made
any color scheme change require editing every file. Semantic names
(`ACCENT`, `STATUS_BUSY`, `TEXT_MUTED`) clarify intent at each call
site and enable future theming (dark/light/custom) with a single
module swap.

**Design**: `Theme` uses `const` associated items rather than a
global singleton or trait. This keeps it zero-cost (no runtime
dispatch, no initialization), works in const contexts, and is
trivially testable. Composite styles (e.g., `focused_title()`) are
`const fn` methods that combine colors with modifiers.

**Rejected**:

- *Global singleton / `lazy_static`* — runtime overhead, mutex
  contention in render path, unnecessary for static color values.
- *Trait-based theming* — over-engineering for the current need.
  Can be layered on top later if user-selectable themes are added.
- *CSS-like stylesheets* — no Rust TUI framework supports this
  natively; would require a custom parser and resolver.

---

## ADR-19: Declarative agent definitions

**Choice**: Each session runs exactly one coding-agent CLI chosen
at creation time; each agent runs with its own default config.
Agents are described as **data** in `~/.config/friring/agents.toml`
(sibling of any other config), seeded with built-ins (claude,
codex, antigravity, opencode, aider, copilot, vibe) on first run via
`agent::agent_config::load_or_seed`. An `AgentDef` carries a
`command`, `args` (always passed — bake in flags like a model
here if you want), and argument-template groups (`resume_args`,
`fork_args`, `new_session_args`), plus a `resume_latest` flag. A
single `agent::GenericProvider` (an `AgentProvider`) launches any
defined agent by substituting `{id}` and appending each group only
when its driving value is present. Only `claude` can be addressed by
the friring-generated id (`--session-id {id}`); the other built-ins
can't pin or report a session id, so they set `resume_latest = true`
and use id-less, cwd-scoped flags (`codex resume --last`, `opencode
--continue`, …) that make the agent resolve "the last session in this
directory" itself. `resume_latest` only governs *when* the resume
group fires at restart (`session_ops::resume_trigger_for`): for these
agents restart always resumes; claude still defers to an on-disk
transcript check.

**Why**: Friring started as Claude-Code-specific, with a hard-coded
`ClaudeProvider` plus roles, skills, profiles, and an MCP/plugin
surface tied to one agent's permission model. Generalizing to "run
any coding agent" meant the launch contract had to be data, not
code: users add or tweak agents by editing TOML, with no recompile
and no per-session permission/prompt/tool configuration. The
`session::AgentDef` / `AgentRegistry` types are pure data (no
filesystem, no local imports) so they satisfy the `session/`
isolation rule; the TOML loading and the provider bridge live in
`agent`.

**Group precedence**: fork wins over resume, which wins over a
fresh `new_session` id; static `args` follow. A group with no
value is simply omitted — no "unresolved placeholder" heuristics.

**Config, not DB**: Agent definitions deliberately live in TOML
rather than SQLite (ADR-8). They are read-mostly, hand-editable,
and shared across instances by re-reading the file — there is no
concurrent-write hazard that would justify moving them into the
database.

**Rejected**:

- *Hard-coded providers per agent* — the previous `ClaudeProvider`
  approach; adding an agent meant a code change and release.
- *Per-session roles / permissions / prompts / tools* — removed
  with the pivot. They were Claude-specific and did not generalize
  across agents; a session now configures only its agent.
- *Agent definitions in SQLite* — overkill for read-mostly,
  user-authored config; TOML keeps them inspectable and diffable.

## ADR-20: Agent-agnostic extensions in `extensions/`

**Choice**: Opt-in workflows that *compose* friring (rather than
extend the binary) live in `extensions/<name>/` as data + shell:
a plain-markdown behavior spec, portable scripts built on
`friring-cli` + `jq`, and a curl-able, idempotent `install.sh` —
the same distribution model as `scripts/install.sh` and
`packaging/`. The first extension is **flow** (an experimental
focus-protecting triage agent; see FEATURES.md). Extensions reach
agents only through `agents.toml` **aliases** (e.g. `flow-worker`)
that the user maps to any CLI, and surface their spec through
context-file symlinks (`CLAUDE.md`/`AGENTS.md`/`GEMINI.md` → the
spec), so no vendor is named anywhere.

**Why**: ADR-19's pivot made friring agent-neutral; an opinionated
LLM workflow (prompts, triage rubrics, tick cadences) would undo
that if baked into core, and it iterates on a much faster cadence
than the binary (editing a markdown spec vs. cutting a release).
Keeping extensions as data over the public surface (`friring-cli`
plus `agents.toml`) also makes that surface's stability a tested,
load-bearing contract.

**Rejected**:

- *Vendor plugin formats* (e.g. a Claude Code plugin) — couples
  the workflow to one agent's ecosystem; the same agent brain must
  be runnable by codex, antigravity, opencode, vibe, ….
- *A `friring-cli flow init` subcommand with embedded assets* —
  puts one opinionated workflow inside the agent-neutral core and
  ties spec iteration to the release cycle.
- *A separate repository* — the extension scripts against
  `friring-cli`'s JSON surface and should version and CI alongside
  it.

## ADR-21: Declarative extension manifests + first-class lifecycle

**Choice**: Extend ADR-20 by teaching the core a single declarative
**manifest format** (`extension.toml`, `session::ExtensionDef`) and a
first-class lifecycle on the public surface:
`friring-cli extension install/uninstall/activate/deactivate/list/status`
(`session_ops::*`, `agent::extension_config`). The manifest has an
*install* half (`home`, `[[agents]]`, `[[files]]`, `[[symlinks]]`) and a
*runtime* half (`[[sessions]]`, `[[automations]]`). `install` resolves a
source (a bare name → the official repo pinned to the binary's release
tag; a path; or an `http(s)://` base — fetched via `curl`/`wget`), lays
down the payload, registers agents (append-only, comment-preserving),
writes the home-resolved manifest to the discovery dir, and activates.
Active extensions are recorded in SQLite `metadata` and **self-healed**
(missing sessions/automations recreated) at TUI startup and on every
`automation tick`. The core still knows the *format*, never a specific
extension; flow's `install.sh` becomes a thin shim over the CLI.

**Why**: ADR-20 left each extension to reimplement bootstrap in bespoke
shell, and gave no way to recover from a half-removed extension. Folding
the mechanics behind one data-driven command makes install reproducible
and uninstall symmetric, and self-heal makes an active extension robust
against accidental deletion — all while staying extension-neutral
(reusing `spawn_session_headless`, `db.create_automation`, `AgentDef`).
Pinning the fetch to the binary's release tag keeps a fetched extension
in sync with the binary that reads it.

**Rejected**:

- *Embedding extension assets in the binary* (the option ADR-20
  rejected) — still rejected; `install` fetches **data** at runtime, it
  does not bake assets in, so the agent-neutral core is preserved.
- *Adding an HTTP client dependency* — `curl`/`wget` shell-out matches
  the existing installer and keeps the dependency tree small.
- *Re-serializing `agents.toml` to add/remove agents* — would drop user
  comments/formatting; the installer edits text (append on install,
  block-removal by name on uninstall) instead.

## ADR-22: `App` decomposition — coordinator + per-domain sub-modules

**Choice**: Keep the single `App` model (ADR-1, TEA) but split its
~11.7k-line `app/mod.rs` into per-domain sub-files under `src/app/`,
relocating cohesive `impl App` method clusters out of `mod.rs` while the
state they own lives in small per-cluster sub-structs. `app` stays one
**EXEMPT** module in `tests/architecture_rules.rs` (the coordinator that
imports every layer), and governance is directory-level, so the new
`app/*.rs` files introduce **no** new cross-layer edges and need no
allowlist entries — the split is entirely intra-`app`.

Two halves:

- *State* — already mostly done: `task_ui: TaskUiState`, `automation_ui:
  AutomationUiState`, `new_session: NewSessionWizardState`,
  `global_search: GlobalSearchState`, `worktree_sync: WorktreeSyncState`,
  `metrics`, `notification_state`. Two remain to extract: a new
  `PointerState` (text-selection / click-target / scrollbar / hover
  registries) and a `SpawnController` holding **only** the
  background-task machinery (`worktree_create`/`session_spawn` + their
  `pending_*`).
- *Behavior* — relocate the method clusters into domain files:
  `app/tasks.rs`, `app/automation.rs`, finish `app/search.rs`,
  `app/mouse.rs`, `app/worktree_sync.rs` + `app/git_stats.rs`, and
  `app/spawn.rs`. Methods stay `impl App` (they coordinate side effects);
  only pure state/logic lands on the sub-structs.

**The spine stays on `App`** (clusters borrow it, never own it): the
session vector + selection cursor (`sessions`, `active_index`), the
backend registry (`backends`), per-session render views
(`session_terminal_views`), the render-loop flags (`needs_redraw`,
`last_draw_at`, `last_output_gen`), the status/order caches
(`cached_hook_states`/`hook_states_version`, `cached_session_order`,
`last_active_session_id`, `spinner_frame`), and
`metrics`/`db`/`session_counter`/`terminal_rows`. The TEA methods
(`update`, `tick`, `view`, `handle_key`/`dispatch_action`, `new`,
`shutdown`), session restore/adopt, and all navigation/status/ordering
stay too — navigation *is* manipulation of the shared cursor. Two
cross-cluster handoff slots stay explicit and `pub(crate)`:
`pending_task_prompt` (tasks↔spawn) and `deferred_inputs`
(spawn/sync/paste).

**The spawn boundary**: `SpawnController` owns only its background tasks
and exposes `poll() -> SpawnEvent` (`WorktreesReady`/`Spawned`/`Failed`);
`App` applies the event via the existing `finalize_spawned_session`. The
controller never owns session *adoption* — that body touches `sessions`,
`active_index`, `focus`, `db`, `deferred_inputs`, `metrics`, and
`task_ui` in one place, and pushing it into a sub-struct would re-create
the god-object through a `&mut App` parameter.

**Order** (each its own PR, green throughout; `app/acceptance.rs` is the
safety net): (1) tasks → (2) automations → (3) search — the safe
relocations, state already extracted — then (4) mouse (first new
sub-struct), (5) sync, (6) spawn (machinery only; last and hardest).
Because all relocations carve from the same `mod.rs`/`key_handlers.rs`,
they are **sequenced**, not run in parallel, so each rebases onto the
prior cleanly.

**Why**: `mod.rs` is the repo's hottest merge-conflict file and
interleaves spawn/mouse/task/automation/sync/metrics, so no single flow
can be read without scrolling past four others. The split shrinks
`mod.rs` toward a coordinator + spine (~5–6k lines) with each domain's
invariants local, and *strengthens* the TEA spirit — side effects stay
concentrated at the coordinator, pure state/logic gets isolated — rather
than bending it. The state half is already underway, so most of the work
is mechanical relocation against existing tests: low risk, high
readability gain.

**Rejected**:

- *Splitting `App` into multiple models / TEA loops* — breaks ADR-1's
  single `update`/`view` and the `data_version`-driven redraw; the
  coupling is real (every cluster reads the selection cursor), so one
  model with a borrowed spine is correct.
- *Owning the spine in sub-controllers* (e.g. a `SessionController`
  owning `sessions`/`active_index`) — every other cluster borrows it, so
  this merely relocates the god-object and forces `&mut App`-style
  params everywhere.
- *Pushing side-effecting methods onto the sub-structs* — would drag
  `db`/`sessions`/`deferred_inputs` into each cluster and reintroduce the
  coupling; behavior stays `impl App`, only pure logic moves.
- *One big relocation PR* — unreviewable and merge-hostile; the value is
  in independently-reviewable, test-green increments.

## ADR-23: Real-agent e2e — stub the model API, one scenario drives test and demo

**Choice**: Test real agent binaries (Claude Code first) end-to-end
through the real TUI by stubbing the **model HTTP API on loopback** — a
zero-dependency node sidecar speaking the Anthropic Messages dialect
(SSE + `tool_use`), answering from hand-curated *semantic* fixtures —
and by expressing each covered flow as a **scenario** whose one
description runs both as an asserting bats test and as a demo
recording (`scripts/dev/agent-e2e/`, `docs/E2E.md`). The suite lives
outside cargo/nextest (the repo's established shape for
process-spawning e2e), reuses `scripts/dev/lib/sandbox-env.sh` for
hermeticity, and its CI job is path-gated and excluded from
`all-checks.needs`.

**Why**: real-agent behavior had zero coverage — the in-process
acceptance suite fakes the backend, the smoke test never runs an agent —
and the demo tapes could drive but not assert. The HTTP boundary is the
narrowest stable seam for an opaque real binary (proven against the
pinned claude: plain-HTTP loopback, full tool loop, offline under a
dead proxy). Steps are a thin dual-backend vocabulary (tmux send-keys /
tape lines replayed into a filmed tmux), so test and demo cannot drift
apart.

**Rejected**:

- *Faking at the `SessionBackend` trait* (`FakeBackend`) — no real
  process ever runs; exactly the gap being closed.
- *Record/replay cassettes* — tool-use loops make bodies cumulative and
  machine-specific; semantic turn-shape matching survives reruns.
- *An in-crate Rust stub* — new heavy deps (hyper/axum) fail cargo-deny
  policy and bloat the graph for a test sidecar; node ≥ 18 is already a
  dev prerequisite of the toolchain.
- *A scenario DSL (Gherkin or custom)* — scenarios stay plain bash
  (flat step calls + assert functions); logic beyond a flat list belongs
  in the harness, per the well-known DSL-maintenance failure mode.
- *Putting the suite in cargo/nextest* — would drag tmux/node/claude
  into `cargo nextest --all` and the prek pre-commit hook, and fight the
  120s slow-timeout kill; the repo convention is shell harnesses.

---

## ADR-24: Agent metrics are read from source, never cached into SQLite

**Decision**: `friring-cli`'s four metrics commands (`session
metrics`/`resources`/`activity`, `usage`) re-read the **same sources the TUI
reads** — the agent's statusline JSON, the machine's process table, the agent
CLI's transcripts, the vendor usage API — instead of reading a value a running
TUI published into the DB. The sources each got a module `cli` may reference
(`activity`, `proctable`, `usage`), split out of `app` where two of them lived.

**Why**:

- **The CLI must work with no TUI running.** Sessions outlive the TUI inside
  tmux; that is the product's premise. A published cache is empty exactly when
  cron and scripts want these numbers — the failure mode `friring-cli perf`
  already has ("No perf snapshot published"), acceptable for a debug command
  about the render loop but not for what an agent is costing.
- **A tick-rate write would spam every other instance.** The DB is the
  multi-instance channel and peers detect writes with `PRAGMA data_version`
  (ADR-7b); a metrics row written each metrics tick would bump every other
  connection's version continuously and force a full shared-state reload on
  each poll, defeating the throttle the perf counters exist to protect.
  `App::publish_perf_snapshot` is gated behind `FRIRING_PERF_LOG`/the perf HUD
  for exactly this reason.
- **The TUI is not the source of truth for any of them** — it is another
  reader. Persisting would cache a cache, and add staleness the CLI could not
  detect.
- **Coverage is identical either way.** The statusline dir is never injected
  into a remote agent (`session_ops::inject_friring_env`), and the process
  table and transcripts are local, so the TUI knows nothing extra about a
  remote session.

**Consequences**:

- These commands have **no history**. The statusline file holds current totals
  and is overwritten in place, so cost-over-time is not derivable. A trend
  feature would be a deliberate separate sampler on a slow cadence (minutes),
  driven headlessly rather than by the TUI.
- CPU is opt-in (`--cpu`): it is a rate, so a one-shot process must sample
  twice around a delay. Memory, being instantaneous, is always reported.
- `session activity` parses a transcript from scratch where the TUI tails it
  incrementally — the one command whose cost scales with history. It drains the
  per-pass ingest budget in a bounded loop (`activity::scan_once`) and reports
  when history is still incomplete rather than under-reporting silently.

**Rejected**:

- *A TUI-published snapshot in the `metadata` table* (the `perf` pattern) —
  fails the no-TUI case and causes the `data_version` churn above.
- *An IPC socket to the running TUI* — friring has no RPC surface by design;
  the CLI and TUI are peers over SQLite, tmux and the filesystem, and adding a
  daemon protocol for read-only numbers buys nothing the sources don't give.
- *Duplicating source discovery in `cli`* — where every agent CLI keeps its
  transcripts is the expensive, version-specific knowledge in this codebase;
  two copies would drift. Hence the module split rather than a second reader.
