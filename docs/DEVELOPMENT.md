# Development

How to set up a reproducible friring dev environment, build/test/lint friring,
run the app in an isolated sandbox, and regenerate the demo media.

## 1. Toolchain — the dev environment

### Recommended: Nix flake

The `flake.nix` pins the whole toolchain CI uses — the Rust toolchain (read from
`rust-toolchain.toml`), `tmux`, `shellcheck`, `bats`, Node, `cargo-nextest`,
`cargo-deny`, `cocogitto`, `just`, and the demo stack (`asciinema`/`agg`/`ffmpeg`).

```bash
# one-time, if not done already: enable flakes
#   mkdir -p ~/.config/nix && echo 'experimental-features = nix-command flakes' >> ~/.config/nix/nix.conf

nix flake lock        # one-time: generate/commit flake.lock (pins inputs)
nix develop           # enter the pinned shell
# ...or, with direnv installed, once:
direnv allow          # auto-enters the shell on `cd` (see .envrc)
```

A couple of tools aren't packaged in nixpkgs yet (`prek`, `rumdl`, nightly
`cargo-pup`); the shell prints a hint to install them via
`scripts/install-dev-tools.sh`.

### Fallback: no Nix

```bash
scripts/install-dev-tools.sh   # cargo-binstall/cargo install the dev tools
prek install                   # install the git hooks
```

You'll also need, from your package manager: `tmux >= 3.2`, `shellcheck`,
`bats`, Node + npm (website linters), and `git`.

## 2. Everyday tasks — `just`

`just` (in the dev shell) is the task entrypoint — run `just` for the list:

| Task | What it does |
|------|--------------|
| `just build` | build the dev binaries (`friring` + `friring-cli`) |
| `just test` | `cargo nextest run --all` |
| `just lint` | fmt-check + clippy + cargo-deny + rumdl + shellcheck |
| `just fmt` | format Rust + website |
| `just arch` | architecture-rule + rustdoc checks |
| `just hooks-install` | `prek install` |
| `just smoke` | black-box TUI smoke test |
| `just sandbox*` | dev runtime sandbox (below) |

Bare `cargo` still works for everything `just` wraps:

```bash
cargo build --bin friring --bin friring-cli   # what `just build` runs
cargo check --all                             # type check
cargo build --release                         # release build (LTO, stripped)
```

## 3. Runtime sandbox — run friring isolated

The sandbox runs the dev build (`0.0.0-dev` → `dev_build` cfg, which uses a
`friring-dev` tmux socket) with **friring's own config/data redirected** into the
sandbox (via `FRIRING_CONFIG_DIR` / `FRIRING_DATA_DIR`), so it never touches your
real `~/.config/friring` or sessions. It **keeps your real `HOME`**, so your
authenticated agent CLIs (`claude`/`codex`/`antigravity`/…) work normally — and it puts
the dev `target/debug` first on `PATH`, so an agent's status hook calls *this*
`friring-cli` and writes to the sandbox DB the TUI reads.

```bash
scripts/dev/sandbox.sh                 # persistent "default" profile, launch the TUI
scripts/dev/sandbox.sh --fresh         # throwaway env, wiped on exit
scripts/dev/sandbox.sh --profile foo   # a named persistent profile
scripts/dev/sandbox.sh --isolate-home  # full hermetic isolation (fresh HOME; agents have NO creds)
scripts/dev/sandbox.sh --shell         # a shell with the sandbox env (run friring-cli by hand)
scripts/dev/sandbox.sh -- session list # run a friring-cli command in the sandbox
scripts/dev/sandbox.sh --clean [name]  # kill + wipe a persistent profile
```

Or via `just`: `just sandbox`, `just sandbox-fresh`, `just sandbox-shell`,
`just sandbox-clean [profile]`.

**Isolation flavors:**

- **friring-only (default)** — real `HOME`/agents; only `friring-config` +
  `friring-data` (+ a private `TMUX_TMPDIR`) are redirected. Use this to dev with
  your real, logged-in agents without polluting your real friring state.
- **full (`--isolate-home`)** — also overrides `HOME` + `XDG_*`, so the env is
  hermetic and agents boot with no credentials. This is what `scripts/demo/
  record.sh` and `scripts/dev/smoke/tui-smoke.sh` use (via
  `tbx_sandbox_init_full`).

**Profile lifetimes:**

- **Persistent** profiles live under `target/dev-sandbox/<profile>/` (gitignored;
  `cargo clean` or `--clean` removes them). Their tmux socket dir is kept short
  under `$XDG_RUNTIME_DIR` (AF_UNIX socket paths are length-limited, and the
  repo's `target/` path is often too long). Sessions survive across runs.
- **Fresh** (`--fresh`) is a `mktemp` dir wiped on exit — same isolation the
  demo/smoke scripts use.

The isolation logic is one helper, `scripts/dev/lib/sandbox-env.sh`, sourced by
`scripts/dev/sandbox.sh`, `scripts/demo/record.sh`, and
`scripts/dev/smoke/tui-smoke.sh` (one source of truth).

### Example: watch a session's status hook end-to-end

```bash
scripts/dev/sandbox.sh --shell
# inside the sandbox shell (friring/friring-cli target the sandbox):
friring-cli session create --name demo --repo-path "$PWD" --agent claude
friring-cli session signal --state blocked --session <id>   # what an agent hook does
friring-cli session list --json | jq '.[].name'
```

### Live mode — run the dev build on your REAL sessions

`scripts/dev/live.sh` (`just dev-live`) is the sandbox's opposite: it attaches
the dev build to the **installed release's** live state to verify a feature
against real workloads. Quitting friring only detaches (tmux keeps every agent
alive), so the handoff is: quit your installed friring → `just dev-live` → the
dev TUI adopts all live sessions → quit → relaunch the installed binary.

It works by overriding the dev build's compile-time isolation — the script
exports `FRIRING_SOCKET` + `FRIRING_TMUX_SESSION` (both halves of the tmux
identity: the socket picks the server, the session the window group) and
`FRIRING_DATA_DIR` + `FRIRING_CONFIG_DIR` to the release locations, and puts
`target/debug` first on `PATH`. Two guards run before launch: it refuses while
any client is attached to the release tmux server (there is no single-instance
lock — quit the installed friring first), and it backs up `friring.db`
(`friring.db.dev-live-<timestamp>.bak`, newest five kept) because migrations
are forward-only — if your branch bumps `SCHEMA_VERSION`, the migrated DB is
the one thing the quit-and-relaunch round trip does **not** undo: the release
binary will refuse it, and you restore the backup (or keep using the dev
build). `--shell` / `-- <cli args>` mirror the sandbox script; `--no-build`
skips the rebuild.

While the dev TUI runs, iterate without leaving it: `cargo build` in another
terminal (or a `Ctrl+T` shell pane), then press `Ctrl+Alt+R` — friring quits
and `exec`s the on-disk binary in place, env carried over, and the new build
re-adopts every session (see `docs/FEATURES.md`, "Reload friring in place").

## 4. Testing

`cargo nextest` is the preferred runner:

```bash
cargo nextest run --all              # run all tests (preferred runner)
cargo nextest run -E 'test(name)'    # run a single test by name
cargo nextest run --all --profile ci # run with the CI profile
cargo test test_name                 # run a single test via cargo test
bats scripts/install.bats            # test the install script (needs bats-core)
```

### TUI acceptance (e2e) tests

The TUI has three layers of end-to-end coverage:

- **In-process driver + snapshots** (`src/app/acceptance.rs`, a `#[cfg(test)]`
  module). A `Harness` builds a real `App` on a no-op `FakeBackend` +
  `Database::open_in_memory()` + a `TestPathGuard` tempdir (fully hermetic),
  feeds `AppMessage::KeyPress` events exactly as `main.rs`'s loop does, and
  renders to a headless ratatui `TestBackend`. It also drives the loop's
  **tick**: `App::tick` is split into a deterministic `tick_core` (status
  derivation, timer expiry, search debounce, automation firing, external-change
  polling — what `Harness::tick` runs, hermetic and runtime-free) and a spawning
  `tick_background` (sysinfo/git/usage shell-outs, update checks — `main` only).
  Wall-clock-gated behavior is fast-forwarded via `Harness::advance` (the
  `app::clock` test clock — a thread-local offset every UI-thread timer reads
  through), and agent output is injected per session via `Harness::feed_output`
  (same vt100 + `TermSignals` path as the PTY reader), so redraw detection, OSC
  title/bell signals, buffer-content search, and terminal rendering are all
  testable. Stable screens (welcome state, F1 help, theme picker) are pinned with
  **`insta`** snapshots (`src/app/snapshots/`); dynamic flows (navigation,
  modals, panel toggles, quit) assert on `App` state instead, so live
  metrics/clock never make them flaky. Runs in the normal `cargo nextest --all` —
  no tmux/TTY needed. Update snapshots with `INSTA_UPDATE=always cargo test` (or
  `cargo insta review`).
- **Invariant monkey test** (`monkey_random_events_uphold_invariants` in
  `src/app/acceptance.rs`). Seeded pseudo-random event streams (keys, chords,
  mouse, ticks, clock jumps, resizes, injected agent output) against the harness,
  rendering after **every** step and checking `assert_invariants` (selection
  indices in bounds, focus never on a hidden surface, panels never outlive their
  feature flag). A failure prints the seed + step for exact replay. When a "weird
  TUI behavior" reduces to a rule, add it to `assert_invariants` and let the
  monkey hunt for a violating sequence.
- **Black-box smoke test** (`scripts/dev/smoke/tui-smoke.sh`, `just smoke`).
  Launches the real `friring` binary inside a throwaway tmux pane (isolated
  `HOME`/XDG/`TMUX_TMPDIR`, mirroring `scripts/demo/record.sh`), drives it with
  `tmux send-keys`, and asserts on captured frames (boot → F1 → theme → quit).
  Gated behind the `tui-smoke` CI job (needs tmux).
- **Real-agent e2e** (`scripts/dev/agent-e2e/`, `just agent-e2e`). A *real*
  agent binary (Claude Code is the reference) inside a Friring-managed pane,
  with the model API stubbed on loopback — hermetic, deterministic, offline.
  One scenario description drives both the asserting bats suite and a VHS demo
  recording (`just agent-demo <scenario>`). Not part of `cargo nextest`; runs
  via the non-blocking `agent-e2e` CI job and skips cleanly when the agent
  binary is missing. Architecture, scenario/agent-profile contracts, and the
  conformance status live in **`docs/E2E.md`** (decision record: ADR-23).
- **Performance counter tests** (`perf_*` in `src/app/acceptance.rs`). Assert on
  `App::perf_counters()` — wall-clock-free `u64` counters bumped at the
  render/tick hot paths (`MetricsState::perf`) — to gate the perf optimizations
  without flaky timing: e.g. idle iterations skip the paint, the session order is
  rebuilt only when its inputs change. Run with `cargo nextest run -E
  'test(perf_)'`. See `docs/PERFORMANCE.md`.

### Dev harness layout (`scripts/dev/`)

The session-backend e2e harnesses form one family under `scripts/dev/e2e/`
(`linux-container.sh` = ephemeral Podman, `windows-vm.sh` = ephemeral dockur
Windows VM, `real-host.sh` = a machine you own) sharing one sourced library,
`e2e/lib/e2e-common.sh` — colour logging, the PASS/FAIL result contract (`pass`/
`fail`, `E2E_JSON=1` for a machine-readable line), an in-shell `json_field`
extractor (**no `python3`**), the `hosts_block` `[[hosts]]` emitter, and the
`session create → get → assert` core (`e2e_create_and_get`/`e2e_assert`). The TUI
smoke test lives at `scripts/dev/smoke/tui-smoke.sh`. `scripts/dev/README.md` is
the newcomer index (which script for which job) and carries the old→new path map
(the flat `remote-ssh-test.sh`/`windows-test.sh`/`lab-test.sh`/`tui-smoke-test.sh`
names were renamed, not kept as shims).

### Windows test environment (VM)

`scripts/dev/e2e/windows-vm.sh` provisions a throwaway **Windows VM** to exercise
friring's Windows support, where the session backend is
[psmux](https://github.com/psmux/psmux) (a native-Windows tmux clone — same
command language, `-L` sockets, and `-C`/`-CC` control mode that `TmuxBackend`
drives, so it installs a `tmux.exe`). Mirroring `e2e/linux-container.sh`, it runs
a real KVM-accelerated Windows VM inside a single Podman container via
[`dockur/windows`](https://github.com/dockur/windows), with an unattended
first-boot `/oem` payload that installs psmux + OpenSSH + `cargo-nextest.exe` so
the harness drives the VM **headlessly over SSH**. Default edition is **Windows
11** (`VERSION=11`); dockur has no "tiny" edition token, so override
`FRIRING_WIN_VERSION` only with values dockur recognizes (`11`, `10`, `2025`, …).

```bash
scripts/dev/e2e/windows-vm.sh up         # build /oem payload + boot the VM (first run installs Windows, ~10-20 min)
scripts/dev/e2e/windows-vm.sh wait       # block until the VM's SSH is reachable
scripts/dev/e2e/windows-vm.sh test       # headless smoke test (psmux/tmux + a -L control session round-trip)
scripts/dev/e2e/windows-vm.sh test-suite # run the FULL nextest suite inside the VM (see below)
scripts/dev/e2e/windows-vm.sh deploy     # cross-build friring for x86_64-pc-windows-gnu + copy the .exe in
scripts/dev/e2e/windows-vm.sh ssh        # PowerShell shell in the VM; `web`/`rdp` for eyes-on; `down`/`clean` to tear down
```

`test-suite` runs the **entire `cargo nextest` suite** inside the VM. The VM has
**no Rust toolchain**, so the host cross-builds a self-contained **nextest
archive** (`cargo nextest archive --target x86_64-pc-windows-gnu`), ships it plus
a tarball of the working tree (uncommitted changes included — needed so insta
snapshots / fixtures resolve), and runs it with `cargo-nextest.exe
--archive-file … --workspace-remap …`. CI runs the same suite natively in the
`windows` job (`.github/workflows/ci.yml`, `windows-latest` + `cargo nextest
run`); the VM is the local/offline mirror. Tests that genuinely assume Unix are
`#[cfg(unix)]`-gated; the rest are written to source the home dir from the
platform var (`USERPROFILE`/`HOME`) and use `tempfile`/`std::env::temp_dir()`
rather than hardcoded `/tmp`.

All state lives under `target/windows-test/` (gitignored): the throwaway SSH
keypair, the cached psmux + nextest zips, the generated `/oem` payload, the
cross-built test archive, and the VM disk image. Needs `/dev/kvm` +
`/dev/net/tun`. **Gotcha:** dockur forwards only `3389` to a Windows guest by
default, so the script sets `USER_PORTS=22` to push the published SSH port
through qemu's host-forward into the VM.

### Lab (real-host) test environment

`scripts/dev/e2e/real-host.sh <host> <verb>` (or `just lab <host> <verb>`) drives
the same checks against **any real machine over SSH** — a `~/.ssh/config` alias
or `user@address`. Linux/Windows is auto-detected. Because lab machines may also
run *regular* friring sessions, the e2e test is fully scoped: a private
`-L friring-lab-test` socket + session, all remote state under one
`friring-lab-test` directory (repo + `worktrees_dir`), and an isolated local
`FRIRING_CONFIG_DIR`/`FRIRING_DATA_DIR` — the release socket (`friring`), the dev
socket (`friring-dev`), and real config/DB are never touched. Verbs: `check`
(readiness probe), `hosts` (print the `hosts.toml` block), `test` (headless
ssh-backend e2e, mirrors `e2e/linux-container.sh test`), `tui` (wire the host
into the persistent `lab` sandbox profile + launch for manual testing), `ssh`
(interactive shell or a one-off command), `clean`; Windows-only: `deploy`
(cross-build + install to `C:\Tools\friring`), `run` (the deployed TUI over
`ssh -t`), `test-suite` (nextest archive, mirrors `e2e/windows-vm.sh`),
`wsl-setup` / `wsl-check` (provision + verify a WSL distro as a friring target),
`native-test [agent]` (headless e2e of the **deployed** binaries natively on the
host: `friring-cli.exe` creates a local psmux session — agent argv + `FRIRING_*`
env asserted intact — and `friring.exe` boots inside a scoped psmux pane and must
show/adopt it; isolated via the `FRIRING_SOCKET` env override, since psmux has no
`TMUX_TMPDIR`-style socket-dir isolation). Local state: `target/lab-test/`
(gitignored).

## 5. Linting & formatting

`just lint` bundles fmt-check + clippy + cargo-deny + rumdl + shellcheck; the
individual commands:

```bash
cargo fmt --all                      # format (rustfmt: 100 char max)
cargo clippy --all-targets --all-features -- -D warnings   # lint
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features   # docs
rumdl check .                        # markdown lint (.rumdl.toml)
rumdl fmt .                          # markdown auto-fix
```

### Website linting

```bash
npm ci                               # install deps (use lockfile)
npm run lint:website                 # run all website linters
npm run fmt:website                  # auto-fix formatting (Prettier)
```

### Architecture enforcement

`just arch` wraps the architecture-rule + rustdoc checks; the underlying
commands:

```bash
cargo test --test architecture_rules              # arch rules
cargo deny check advisories                       # advisories
cargo deny check bans licenses sources            # dep policy
```

## 6. Pre-commit hooks

17 hooks run automatically via `prek` (Rust-based pre-commit framework). Install
with `prek install` (or `just hooks-install`). Stages:

- **commit-msg**: conventional commit validation (`cog verify`)
- **pre-commit**: fmt, clippy, check, nextest, architecture, deny, doc, bats,
  shellcheck, rumdl, prettier, htmlhint, stylelint, eslint
- **pre-push**: commit history check (`cog check`)

Shell scripts are linted with **shellcheck** (config in `.shellcheckrc`); install
it from your package manager (it is not a cargo crate —
`scripts/install-dev-tools.sh` prints a reminder).

## 7. Demo video

The demo media is **generated**, not hand-recorded. A single script drives the
*real* TUI, records it, and writes GIF **and** MP4 straight into `docs/media/`
(needs `asciinema` + `agg` + `ffmpeg` + `tmux`):

```bash
scripts/demo/record.sh                 # regenerate ALL demo videos
scripts/demo/record.sh theme automations   # re-record a subset
```

Real-agent e2e scenarios are demo-able too: `just agent-demo <scenario>`
records the same scenario the asserting suite runs — real agent, stubbed model,
deterministic `Wait+Screen` sync — into `target/agent-e2e/demos/` (see
`docs/E2E.md`).

`record.sh` records every video pair in one pass: the combined hero demo
(`friring-demo.*` via `agents.tape`), one clip per feature
(`friring-{file-manager,info-panel,theme,session-creation,fork}.*`), and the
automations/tasks/search demos (`automations-demo.*`, `tasks-demo.*`,
`search-demo.*`) — one tape each (`scripts/demo/<feature>.tape`). With no args
it records all of them; pass tape stems to re-record a subset (the `agents`
stem is the hero, `automations`/`tasks`/`search` map to `<stem>-demo.*`, every
other stem maps to `friring-<stem>.*`).

### How a clip is captured (and why not VHS)

The recorder captures the TUI's **terminal byte stream** with `asciinema` and
renders it to a GIF **offline** with `agg`; `lib/drive-tape.mjs` reads the tape
and replays its beats as tmux keystrokes into the recorded session.

This is the difference between a demo that sells the tool and one that doesn't.
Capturing *pixels* off a live GUI — VHS's model, via a headless Chromium — makes
the output a function of the recording machine: it drops frames as soon as the
box cannot rasterize fast enough, and it can grab a half-drawn screen (tearing).
On a 2019 Intel Mac this pipeline sustains only ~6fps at 1080p, and because VHS
stamps a *fixed* delay per surviving frame rather than each frame's real
timestamp, a 15s clip was emitted as a 1.08s one — roughly 8x too fast, and
unreadable.

Recording the byte stream costs approximately nothing, so **every paint friring
emits is kept, with its true timestamp**, and rendering can take as long as it
needs. `agg` then emits a frame only when the terminal's content actually
changed, giving it the delay it truly held for. So a clip's pacing is exact and
identical on any machine, every frame is a complete redraw (tearing is
structurally impossible), and the files are smaller. friring helps here: it
paints on demand (ADR-P1), so transitions are captured crisply and idle screens
simply have nothing to animate.

Consequences worth knowing when editing a tape or the recorder:

- The tape's `Hide … Show` preamble is **skipped**: recording attaches to an
  already-running session, so `record.sh` boots the TUI off-camera itself.
- `Type` is replayed character-by-character (`DEMO_TYPING_SPEED_MS`, default
  50ms, matching VHS). Pasting a line at once reads as a glitch, not as someone
  using the tool.
- The tmux status bar is turned **off** on both sockets — an attached client
  renders it, so it would otherwise be filmed.
- **No teardown is filmed.** The tapes don't quit the TUI; the recorder stops
  filming by **detaching** the recorded client, then quits the TUI off-camera —
  where the quit still serves as a fail-closed check that the tape ended in a
  state the TUI can quit from (a swallowed chord means a beat landed in the
  wrong context). A quit on camera films its own teardown (friring clearing its
  alternate screen, then the dying client's reset + `[exited]`) as the clip's
  held closing frame. The detach's smaller tail (leave-alt-screen, reset,
  `[detached]`) is trimmed from the cast before rendering
  (`lib/trim-cast.mjs`), so every clip ends on the last live TUI frame.
- `agg --idle-time-limit` is set far above any beat in the tapes; it would
  otherwise silently compress the very pauses the tapes exist to script.
- The GIF keeps **variable** frame delays — that is where the exact pacing
  lives, so never re-encode it. The MP4 is derived from it with ffmpeg's
  `fps` filter, which re-times to a constant rate for players that need one
  without changing the duration.
- Grid and size live in `record.sh` (`DEMO_COLS`/`DEMO_ROWS`/`DEMO_FONT_SIZE`):
  175x42 at font-size 18 renders ~1920x1080, at about the column count VHS's
  ttyd produced, so the TUI lays itself out as before.
- The font is pinned to **Meslo LG S** (`DEMO_FONT`) and the run **refuses to
  record without it**. agg resolves families itself and silently falls back when
  one is missing — its default list starts with JetBrains Mono, which is rarely
  installed, so the clips used to inherit whatever the recording box happened to
  have. It is passed as `--text-font-family`, never `--font-family`: the latter
  bypasses agg's automatic fallbacks, and those are where friring's symbol glyphs
  (`❯ ◐ ⏺ ✻`, box drawing) come from. Install with
  `brew install --cask font-meslo-lg`; the Nix flake pins it.
- Renderer: agg's default **`swash`**. `--renderer resvg` is *worse* here — it
  breaks box-drawing borders into dashed segments and drops glyphs.

Every clip uses **real agent CLIs driven by the e2e model stubs** — no accounts,
no network, nothing to log in to. The script seeds one session per installed CLI
(`claude`, `codex`, `opencode`, `antigravity`) in a throwaway sample repo, points
each at a loopback stub (`scripts/dev/agent-e2e/stub/`, shared with the e2e
suite — see `docs/E2E.md`), and **pre-plays a scripted conversation** into every
pane before recording starts, so each agent is caught mid-work rather than idling
on a splash screen.

The conversations, the sample repo, the review diff, the tasks/automation and the
search query all come from **`scripts/demo/demo-content.json`** — one file that is
the demo's script. `scripts/demo/lib/gen-stub-fixtures.mjs` compiles it into stub
fixtures plus a pre-play plan (each turn's prompt and a marker to wait for). To
change what the demos say, edit that JSON; nothing else needs touching.

Two consequences worth keeping:

- **Deterministic**: the same scripted exchange every run, so a re-record diffs
  cleanly instead of capturing whatever a live model happened to answer.
- **Identity-free**: every agent talks to `127.0.0.1`, so no account email, token
  or usage can appear on camera. Each CLI's *fictional* model id (`fable-67`,
  `gpt-6.x`, …) is what renders in its own status line. The info panel's Claude
  account-usage gauges are stubbed the same way: `FRIRING_CLAUDE_USAGE_URL`
  (see `docs/CONFIG.md`) points friring's fetch at the anthropic stub, which
  serves the scripted numbers from `demo-content.json`'s `usage` key against a
  fake credentials file seeded at the `~/.claude` fallback path only — the
  claude CLI itself reads `CLAUDE_CONFIG_DIR` and never sees it.

`antigravity` (`agy`) is the exception: it forces real Google OAuth and cannot be
stubbed offline, so it is featured **logged out** on its clean login screen —
which is also what keeps a signed-in account's identity off camera. See
`scripts/dev/agent-e2e/agents/antigravity/profile.sh`.

Pre-play (and every other step) syncs on pane markers, never fixed sleeps: several
real CLIs boot concurrently, so "long enough" is not knowable up front. Missing
agents are skipped with a warning.

The whole state is **re-seeded before every tape**, because the clips mutate what
the next one poses against: `agents` and `session-creation` each spawn a session,
`fork` spawns two more, `tasks`/`automations` add rows. Seeding once and filming
all ten in a row drifts — the session list accumulates strangers, and since the
*selected* session is whichever was spawned most recently, `code-review` ends up
opening a session that has no branch and filming "No changes to show". Re-seeding
costs a rebuild per tape and buys clips that are independent and individually
reproducible: `record.sh code-review` films exactly what the full run does.

The tapes exercise the session list, info panel (`Ctrl+B`), file viewer
(`Ctrl+E`), native code review (`Ctrl+X`, the default `ToggleReview` chord; `F7`
alternate), theme picker, session-creation flow, and the Automations pane over the
seeded sessions and sample tree. The hero `agents` demo also opens the code-review
view, so it seeds the same worktree-with-a-committed-diff session the dedicated
`code-review` clip uses.

It runs fully isolated from your real environment — a dev build (`0.0.0-dev` →
`dev_build` cfg) uses the `friring-dev` socket and XDG subdirs, and the script
points `TMUX_TMPDIR` and `XDG_{DATA,CONFIG,STATE,CACHE}_HOME` at a throwaway temp
dir. **`TMUX_TMPDIR` is essential**: the `friring-dev` socket *name* is shared by
every dev build, so without a private socket directory the cleanup `kill-server`
would tear down dev sessions you already have running.

The deterministic recording path (a hidden `__demo-agent` subcommand streaming
canned scenarios) was retired in favor of the single real-agents script and has
been removed from the binary.

`.github/workflows/pages.yml` copies the mp4s into `website/assets/` at deploy
time and `README.md` embeds the gifs, so regenerating these files propagates
everywhere.
