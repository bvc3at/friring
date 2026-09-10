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
  never importing `app`/`ui` (ADR-15). Three subcommands run *inside* a
  boundary and are therefore dispatched **before the database is opened**
  (ADR-29 in [`SANDBOX.md`](SANDBOX.md)): `sandbox relay` (which is why
  `cli` may reference `proxy`) and `sandbox launch`, the gated shell-free
  helper every policy launch execs (ADR-33), both in `cli/early.rs`; and
  `bridge`, the queue client in `cli/bridge.rs` (ADR-30).
  The rest of `friring-cli sandbox` is
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

**The orchestration bridge, file by file.** The bridge (ADR-30 … ADR-33) is
spread across six modules on purpose: what a request *is* sits in the dependency
sink, what a request *does* sits in the coordinator, and the two never meet in a
layer that could be reached from inside a boundary.

- **`session/bridge.rs`** — the wire and the state machines as plain data: the
  closed `Verb` set with the `BridgeCapability` each one requires, `RequestKey`,
  the request/response envelopes and their byte caps, `ChildState`, `SagaStep`,
  `Outcome`, `ErrorCode`, and the capability document `friring-cli
  capabilities` prints. No side effects and no crate-internal references, so
  `paths` may read the key format and the caps without inverting the layering.
- **`storage/bridge.rs`** — the v49 tables and the one transaction that commits
  a child (S6). `bridge_children` and the ownership rows are **insert-only in
  SQL** (triggers, not convention), so a verb's authority cannot be edited into
  existence; the module offers no update or delete for them, and
  `sessions.parent_session_id` is display-only and never asked.
- **`app/bridge.rs`** — the broker on the TEA tick: lease a bounded number of
  taken requests per pass, resolve the caller from the directory the file landed
  in, check the grant, and answer `status` / `inbox` / `send` / `report` inline.
  It also owns the nudge (one exact literal, rate limited, one pane per tick)
  and the child-state mirror the UI renders.
- **`app/bridge_spawn.rs`** — the child lifecycle as a decision function:
  validation, slot and cascade rules, and the S0–S9 saga's step transitions
  behind a `ChildEffects` seam so every failure can be injected in a test. It
  decides; it does not block.
- **`app/bridge_saga.rs`** — the driver that executes those decisions. Runs from
  `tick_background`, never `tick_core`, and puts every blocking step
  (`git worktree add`, the gated spawn, the post-quiesce verification) on
  `spawn_blocking` with a timeout (ADR-3).
- **`cli/bridge.rs`** — the client the agent runs *inside* the boundary: it
  writes a request file into the session's own bridge directory and waits for
  the answer, JSON by default and `--human` on request. It is the only bridge
  surface an agent can reach, and it can reach nothing else.
- **`cli/early.rs`** — dispatched before the database is opened (ADR-29), so a
  process running inside a boundary never links a code path that could open it:
  `sandbox relay` and `sandbox launch`, the gated shell-free helper every policy
  launch execs (ADR-33).

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
dependency stays visible at each call site. `paths` may read `session`'s
pure data — the bridge protocol's request-key format and byte caps
(ADR-30), so the file queue and the wire cannot disagree about what a
request is — and nothing else; the dependency runs one way, since
`session` is the sink. `app` is EXEMPT — the coordinator imports every
layer (ADR-22).

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

## ADR-30: The bridge is a file queue over the signal channel

**Context**: A sandboxed agent that orchestrates work needs to ask Friring for
things — create a child, read its mail, report progress. Every obvious channel
is one Friring must not open: the database is ADR-29's whole subject, the host
tmux socket is a way out of the boundary, and a control socket would be a new
authenticated surface to design, authorize and keep alive across restarts.

**Choice**: A request/response queue of **files**, in a subdirectory of the
status-signal directory a sandboxed launch already grants. The client renames a
JSON file into `req/`; the running Friring renames it out into a directory no
sandbox was granted, reads it under the signal channel's own rules, and renames
an answer into `res/`. Verbs are a closed set. `friring-cli bridge` is the
client, dispatched before the database opens.

**Why**:

- *No new grant, no new surface.* The bridge lives inside the one directory a
  launch already exposes, so a boundary that carries the bridge is the same
  boundary in every other respect. There is no socket to authenticate, no daemon
  to keep running, and nothing to reason about across a Friring restart that the
  signal channel did not already answer.
- *The channel is the identity.* Friring exposed exactly one bridge directory
  inside one boundary, so a request in it is by construction a request from that
  session. The protocol has no `from` field, because a caller-supplied one would
  be a claim rather than evidence.
- *`rename(2)` is the whole primitive.* It is atomic, it never follows a symlink
  and it never opens anything, so a take turns every subsequent check into a
  decision about a fixed inode. That is the same reasoning the status signal
  already rests on, applied to a second channel rather than reinvented.
- *The queue is a descriptor, not a path.* The session holds read-write on its
  own `req/`, so a path-based check is one it can invalidate between the check
  and the act — `rmdir` and re-point the name, and the rename that follows moves
  a host file out. `req/` is opened once with `O_DIRECTORY | O_NOFOLLOW` and
  everything after it is `fdopendir`/`renameat`/`unlinkat` against that
  descriptor. A descriptor names an inode, which is the guarantee a check cannot
  make.
- *Files survive a restart.* A request written while Friring was down is served
  when it comes back; a request taken when it died is recovered from `.taking`
  and answered from the journal. A socket would have dropped both.

**Consequences**:

- Latency is a poll, not a wakeup: a client waits on the response directory. For
  operations measured in worktree checkouts and window spawns, that is free.
- Every hostile shape a file can take has to be refused explicitly — a FIFO, a
  symlink, an oversized or non-UTF-8 body, a filename that would be a path. The
  rules and their reasons are in [`SANDBOX.md`](SANDBOX.md) §The bridge's file
  queue.
- The protocol is documented well enough that any program with `rename(2)` and a
  JSON parser can drive it without the client, which is what keeps the client
  from becoming the specification.

**Rejected**:

- *A unix socket plus an authorization table* — a second control surface to
  design and keep alive, and a credential to place inside a boundary, in
  exchange for latency that does not matter here.
- *A generic `friring-cli` inside the sandbox* — that is a database handle in a
  boundary, which ADR-29 forbids, and an open-ended verb set rather than a closed
  one.
- *Reusing the status-signal file itself* — it is one-way and unstructured by
  design, and overloading it would make a status report and a request the same
  bytes.

## ADR-31: Children inherit and narrow through capability grants

**Context**: A sandboxed leader that creates child sessions raises two questions
at once. What may a child *do* — and to whom? And what may it *see*, given that
it runs the same agent, from the same family's configuration, on the same host?
Answering the first with "whatever the leader may" makes a fan-out a privilege
amplifier. Answering the second with "the leader's boundary" puts every worker's
transcript in reach of every other.

**Choice**: Three generic capabilities a **profile** grants
(`child-lifecycle`, `mailbox`, `report`) and an **agent** declares it needs
(`bridge_requires`); a child's effective grant is `{mailbox, report} ∩ owner
grants`. A child's *policy* is its owner's, put through a pure monotone
`narrow`: the child's own directories, a profile-listed intersection of shared
read-write paths, a **subtract set** denied after every allow, and the exact seed
targets re-granted after that. A bridge child always runs from a **private**
agent state directory, seeded from what the agent declares and the profile
authorizes, in one of four closed modes.

**Why**:

- *Monotone is the property, not a policy.* `narrow` cannot widen any dimension
  for any input, so a child's boundary is describable by a small overlay and
  re-derivable at every launch. That is what lets a leader whose profile is
  narrowed later have its children narrowed to match, rather than having
  children that outlive the grant they were made under.
- *The subtract set has to come after.* A parent may grant the whole home
  directory, and the family's state is inside it, so "the child does not have
  this" is not expressible as an absent grant. Seatbelt's last-match-wins and
  bubblewrap's later-mount-wins both give a deny that comes after every allow.
- *One shared credential file, not two copies.* ADR-28's condition is that a
  rotating token never exists in two places. `link-rw` gives a child read and
  write on exactly the file the owner uses — the same sharing the vendor's own
  concurrent sessions already do — and its three conditions (the declared
  `credential_file`, `writeback = true`, the profile's authorization in that
  exact mode) are what keep it from becoming "any file under the state
  directory".
- *Depth is structural.* A child never holds `child-lifecycle` because the
  intersection removes it, so there is no depth counter to miscount and no
  recursion to bound.

**Consequences**:

- A bridge-required agent is refused — with `integrity: true`, so
  `allow_unsandboxed_fallback` cannot answer it — where it cannot be served: no
  profile, a profile that grants less, a backend without `Caps::bridge`, a
  remote host, or a headless launch. Each is worse than not starting.
- `state_unrelocatable` is a refusal in every one of its cases and never a
  degraded launch. There is no shared-state mode.
- The operator carries a real burden: the shared build caches a project needs
  (`~/.gradle`, `~/.m2`, `~/.npm`, `~/.cache/pip`, `~/.cargo/registry`) have to
  be listed in `child_shared_rw`, or workers cannot build. The plan deliberately
  does not shrink a child to its worktree alone.

**Rejected**:

- *A second profile per child* — an operator would have to keep two profiles
  consistent, and nothing would make the second narrower than the first.
- *Copying the family's state per child* — that is ADR-28's forbidden second
  copy of a rotating credential, and it multiplies disk by the fan-out.
- *Sharing the family's state directory* — a leader and its workers would read
  each other's conversations, which is the property the private directory exists
  to give.
- *A depth counter* — a number that can be wrong. The intersection cannot be.

## ADR-32: Ownership is immutable, and a child is quiesced before it is judged

**Context**: A bridge child is a session another session created. Two questions
follow, and both have to be answered by something a sandboxed agent cannot
reach. *Who owns it* decides every verb's authority. *What it produced* decides
whether a branch is integrated.

Creating one child is a worktree checkout, five minted directories, a state
seeding, a proxy bind, a multiplexer window, a database transaction, an
acknowledgement, a `rename(2)` and a wait for the child's own hook. Each can
fail and friring can be killed between any two of them.

**Decision**:

*Ownership is an insert-only row.* `bridge_children` carries `BEFORE UPDATE` and
`BEFORE DELETE` triggers that raise, so the row a verb's authority is read from
cannot be re-pointed by anything holding a write handle to the file — including a
bug in friring. `sessions.parent_session_id` stays display-only and is never
asked. Archival is a stamp on `bridge_child_state`; the ownership row outlives
every prune.

*Creation is a saga whose every external effect is recorded first.* `child_sagas`
holds the step and the identity of what that step made — the worktree path, the
gate directory, the window, the pane and its pid — written **before** the effect.
`SagaStep::is_committed` is the line recovery turns on: below it the saga's
effects are removed and the request is failed; at or above it the child is a real
session that is adopted and carried on. Recovery therefore acts on recorded
identities only. There is no prefix scan and no "kill what has no row". The
`committed` step itself is the one place where the record *is* the effect, so it
is written **inside** S6's transaction alongside the rows it describes: written
after, a crash in between would leave recovery reading a pre-commit saga against
an ownership row it may not delete, and the child would hold one of its owner's
fan-out slots for good.

*A reclaim needs proof of ownership, not a recorded path.* S2's worktree path is
written before `git` runs, so a saga that lost the branch to another instance
holds the *winner's* directory against its own failure — and `git worktree add
-b` cannot tell a lost race from a `post-checkout` hook that failed after the
checkout, because both end with a registered worktree and a non-zero exit. So S2
is two commands: `git branch` claims the ref atomically (exactly one winner), and
only the winner adds the worktree. `child_sagas.branch_claimed` records which it
was. An unwind or a recovery that cannot read that flag removes nothing and tells
the operator where the directory is — a leaked worktree can be deleted by hand,
and a wrongly deleted one cannot be brought back.

*Owning the branch is not owning the path, so both are established.* The worktree
layout sanitizes `/` to `-`, so `feat/one` and `feat-one` are two git-legal branch
names for one directory. Two creates in one tick each win their own ref, so both
report the branch claimed, and only one wins `git worktree add`. Trusting the
branch claim alone, the loser's unwind would force-remove the winner's freshly
created — therefore clean — worktree. So the reclaim asks `git worktree list`
which branch the directory is actually on and removes it only on its own; any
other answer, including "git would not say", leaves the directory and tells the
operator. The branch is still reclaimed on that path, because it really was this
attempt's and leaking one ref per collision is what the unwind is for.

*The agent is gated between S5 and S8.* The pane exists five steps before the
agent starts, so every step in between can fail without a turn having run, a file
having been written or a token having been spent. The gate is opened by a
`rename(2)` into a directory the boundary sees read-only (ADR-33), so the only
thing that can open it is the host — and it is opened only after
`revalidate_identity` proves the pane is still the recorded one and the egress
supervisor has acknowledged the commit.

*Readiness is the child's own hook report and nothing else.* A live pane proves a
process is running; it does not prove the process is running from the private
state directory friring seeded (ADR-31). The hook fires from inside the boundary,
from that directory, so it is the only accepted proof — the pane-pid fallback
ordinary sessions use is refused here. The child's hook row is cleared as the
gate opens, because the status-file channel drops a report repeating the recorded
state: a resumed child whose new agent first says what its previous life said
would otherwise never re-stamp `state_at` and would time out while running fine.
That drop-a-repeat rule asks the **database**, not the reading instance's cache:
friring supports several instances on one database (ADR-7b), and a peer that had
not reloaded since the clear would compare the new agent's first report against
the previous life's state and drop it.

*A `result` is an intent, not a verdict.* It is the one finish kind, and it is
typed: a `result` whose body is not a `ResultBody` is refused and starts no
quiesce, because "the child asked to finish" must never be inferred from free
text. Accepting the intent moves the child to `finishing`; the host then sends
its `ack`, stops the exact pane, and only then reads the worktree with four
read-only `git` commands. A **dirty** worktree lands in `dirty` whatever the
intent claimed — never integrated, slot still held, the owner's to `resume` or
`stop`. A pane that will not die, or whose identity does not match, lands in
`stop_failed`: never integrated, never reused, surfaced for an operator. The
verdict and the terminal state are written before anything is answered or
retired, and neither landing refuses the request and leaves the child
`finishing` — live, so its slot is held, and re-run by recovery on the next
start. A caller told `done` over an empty `bridge_results` row is a branch an
integration step would merge as one friring verified.

An intent that arrives while the child's own launch is still running is **held**
rather than dropped: the agent starts at S8, one step before S9, so a worker
small enough to finish inside that window is the ordinary case for a small node.
It becomes a quiesce the moment the launch ends. The same holds for a `stop` that
lands on a launch — attaching it to that job would answer the caller with the
launch's own `ok` and `state: "ready"`, over a child nothing stopped.

Held intents are **written down**, on `child_sagas.finish_outcome`. The launch has
not reached `finishing`, so nothing else on record would carry the intent across a
crash between S8 and S9 — and the `send` that brought it was already answered
`ok`, an answer a replay returns verbatim rather than re-running. Without the
column, recovery adopts a child that had already finished, with no verdict and
its owner's fan-out slot held until somebody stops it by hand.

When **both** a held `result` and a held `stop` are waiting on one launch, the one
that arrived first decides, which is what the live path does: a `result` first
creates the quiesce and a later `stop` joins it as a waiter, so both callers are
answered from the child's own outcome; a `stop` first wins over a later result.
`FollowUp` records that order, because without it the deferred path always
behaved as if the stop came first and turned a `completed` child into
`failed`/`stopped`.

**Consequences**:

- `create`, `stop` and `resume` cannot be answered on the tick that accepts them.
  They are **deferred**: the journal entry stays `accepted`, no response file is
  written, and the saga writes both when it reaches a final step. A client
  retrying with the same key waits rather than starting a second child.
- A clean owner `stop` is also the slot-releasing parking primitive. It retires
  the runtime while preserving immutable ownership, the worktree and private
  agent state; a later `resume` relaunches that same child **into that same
  child's conversation** — `child_resume_identity` keeps the recorded
  `agent_session_id` and emits the agent's own resume group, and a resume friring
  cannot **prove** will reach that conversation is refused rather than launched
  into a blank one. The proof is the agent's own declaration
  (`[agents.<name>.transcript]`: a directory, a suffix, and whether the file name
  carries the id), evaluated against the private state directory *this* launch
  uses — so the core stays generic and holds no agent-specific knowledge, and an
  agent that declares a resume contract with no way to check it is refused rather
  than guessed at. The refusal is decided in `begin_resume` before any mutation,
  because a `stalled` child's pane is stopped on the way to a relaunch and a
  refusal after that would leave a killed agent and no resume. This is also why
  the readiness rule above matters: a resumed agent replays its transcript and
  its first report can repeat what its previous life recorded. `stopped` and
  `unusable` must reacquire fan-out capacity before any state or pane changes;
  its `starting` claim and relaunch saga land in one transaction. `dirty` and
  `stalled` already hold their slot.
- A saga's blocking steps — the checkout, the window spawn, the post-stop
  inspection — run on blocking tasks polled from `tick_background`, not
  `tick_core` (ADR-3). `tick_core` is documented never to spawn a task.
- `session_repos` is a **row**, not a derivation from the sandbox profile's
  grants. Deriving it would make editing a profile into an authority change.
- Deleting an owner stops its children first and deletes none of them; deleting a
  child mails its owner `child.removed_by_operator`.
- `RequestBody` is untagged, and `{"child": "c1"}` is a well-formed `stop` *and*
  `resume`. The verb is the authority, so every body is re-read against it
  (`Request::body_as`) rather than matched on the arm serde happened to pick.

**Rejected**:

- *A mutable owner column* — anything that can write the file could reassign a
  child, and a verb's authority would be a thing a bug could move.
- *Trusting the child's `result` as the terminal state* — a worker claiming
  `completed` over an uncommitted worktree would have its branch integrated.
- *Killing by pane id alone* — a pane id is reused after a server restart, so a
  recovery that killed by id would eventually kill somebody else's window.
- *A pane-pid readiness check* — it would pass for a child running from its
  family's shared state, which is exactly what ADR-31 forbids.
- *Deciding a child's git metadata grant from its own `.git` marker* — the marker
  is a file inside the child's writable worktree, so a child could name a
  sibling's metadata directory and be handed that sibling's index, `HEAD` and
  refs on its next resume. The repository's own `gitdir` record decides instead,
  and disagreement grants nothing (`docs/SANDBOX.md` §What a shared git directory
  does and does not give away).
- *Cleaning up by scanning for friring-looking worktrees or windows* — it would
  act on things a user created that happened to look like friring's.

## ADR-33: A gated, shell-free launch helper runs inside every policy sandbox

**Context**: Two problems shared one seam. First, the in-namespace egress relay
was started by a two-line `/bin/sh -c` script with positional parameters — the
only place on the path from a profile to a running agent where a command string
existed at all. Second, an orchestrated child session must not run before its
session row, its immutable ownership row, its starting state, its first task
mail and its egress commit exist; without a hold, a Friring that died mid-spawn
would leave an agent working from a session nobody owns.

**Choice**: Every policy backend composes `friring-cli sandbox launch` as the
program the boundary runs, with the agent's own argv after its `--`. The helper
starts the relay when there is one, removes the multiplexer-nesting variables,
waits for a release file in a read-only gate directory when there is one, and
then `execvp`s the agent in place. It is dispatched before the database opens,
beside `sandbox relay` (ADR-29), in `cli::early`.

**Why**:

- *Argv the whole way down.* No shell means nothing is quoted, re-split or
  re-parsed. A socket path or an agent argument containing a space, a quote or a
  `;` arrives as one element, whatever the profile or the agent registry says.
- *One exec path.* The variables tmux sets in the pane point at Friring's own
  server. Stripping them only on launches that happened to need a relay would be
  a guarantee that holds sometimes.
- *A read-only directory is a provable gate.* Seatbelt's `(deny default)` plus a
  `file-read*` allow, and bubblewrap's read-only bind, both make creating,
  renaming and unlinking a regular file impossible from inside. So the existence
  of a regular file there is a signal only the host can send — no token, no
  socket, no new control surface. The proof and its residuals are in
  [`SANDBOX.md`](SANDBOX.md) §The launch gate.
- *The relay lifetime invariant survives.* The helper execs and never forks the
  agent, so the agent inherits the helper's process in the launch's own pid
  namespace and the namespace teardown still takes the relay with it. Every
  helper exit is an exit of that process. (bwrap keeps a reaper at pid 1 unless
  `--as-pid-1` is passed, which friring does not; the number is bwrap's, the
  namespace is what the invariant rests on.)

**Consequences**:

- A host with no `friring-cli` beside `friring` is refused, with the fix named.
  It is an ordinary refusal, so `allow_unsandboxed_fallback` still answers it.
  The binary is resolved from the running executable and handed down as a launch
  input, never looked up on `PATH` and never resolved inside a backend.
- The `workspace` read scope must grant that binary explicitly — a bind under
  bwrap, a literal read allow under seatbelt.
- The pane's process tree gains no level: the helper is gone by the time the
  agent runs.
- A gate that never opens exits `75`. That is the backstop for a host that died
  before committing the child, and it is why recovery can distinguish "a window
  with no live agent" from "an agent running from a half-built session".

**Rejected**:

- *A FIFO or a unix socket as the release primitive* — a read-only bind stops
  neither `connect(2)` nor a FIFO opened for writing, so the sandbox could
  release its own gate.
- *Keeping the shell and adding the gate to it* — the gate needs an
  `O_NOFOLLOW`, `O_NONBLOCK`, regular-file-on-the-descriptor check that a shell
  `test -f` does not make, and the command string was the thing worth deleting.
- *Stripping the multiplexer environment through tmux instead* — `tmux setenv`
  does not reach a pane that already exists, and the variable Friring most needs
  gone is the one tmux sets in that pane.
- *Holding the child by not spawning the pane until the row exists* — the pane
  is what the spawn saga records identity from, so it has to exist first; the
  hold has to be inside the boundary.
