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
`bats`, Node + npm (website linters), and `git`. `sqlite3` is required for
live mode (`just dev-live`, § 3) — it takes a consistent DB backup before the
dev build's migrations run; ships with macOS, one package away elsewhere.

## 2. Everyday tasks — `just`

`just` (in the dev shell) is the task entrypoint — run `just` for the list:

| Task | What it does |
|------|--------------|
| `just build` | build the dev binaries (`friring` + `friring-cli`) |
| `just test` | `cargo nextest run --all`, under a throwaway outer HOME (§ 2.1) |
| `just test-unprotected` | the same suite with your real environment |
| `just lint` | fmt-check + clippy + cargo-deny + rumdl + shellcheck |
| `just fmt` | format Rust + website |
| `just arch` | architecture-rule + rustdoc checks |
| `just hooks-install` | `prek install` |
| `just smoke` | black-box TUI smoke test |
| `just sandbox*` | dev runtime sandbox (below) |
| `just seatbelt-probe` | observe the seatbelt boundary against a real kernel (macOS; § 4) |
| `just bwrap-probe` | the same, against bubblewrap (Linux with user namespaces; § 4) |
| `just bridge-e2e` | the orchestration bridge end to end against a real TUI (§ 4) |
| `just codex-park-e2e` | park and resume a real interactive Codex child (§ 4) |
| `just omx-team-e2e` | the omx Team fan-out, real vendor leader and workers (§ 4) |
| `just omx-test` | the `omx` extension's own `node --test` suite (Node 20+) |
| `just dev-live` | dev build against your **real** sessions (§ 3, Live mode) |

Bare `cargo` still works for everything `just` wraps:

```bash
cargo build --bin friring --bin friring-cli   # what `just build` runs
cargo check --all                             # type check
cargo build --release                         # release build (LTO, stripped)
```

### 2.1 Anything that starts a friring binary runs in a throwaway environment

A dev build reads `~/.config/friring-dev` and writes
`~/.local/share/friring-dev/friring.db` — **your own** development config and
database, shared with every other dev binary on the machine, including the TUI
you may have running. A test or harness that starts one and resolves its paths
wrongly writes there. A schema migration is one-way, so a single such write can
leave an installed binary unable to open its own database.

Two rings stop that, and both are cheap enough to be on by default:

- **Each harness proves its own isolation.** `friring-cli config paths` reports
  the config dir, data dir and database file a process resolved, and the
  environment variable that decided each, *before* any database is opened
  (`cli::early`). `tbx_sandbox_init_full` now sets `FRIRING_CONFIG_DIR` and
  `FRIRING_DATA_DIR` explicitly rather than relying on the `XDG_*` fallback,
  whose last link is `$HOME`; a harness runs `config paths` and refuses to
  continue unless every path is inside its own root and both sources are those
  overrides.
- **`scripts/dev/sacrificial-env.sh` bounds a wrong proof.** It mints one
  throwaway root, points HOME, all four `XDG_*` roots and both `FRIRING_*_DIR`
  overrides at it, and plants canaries at `$HOME/.local/share/friring[-dev]` and
  `$HOME/.config/friring[-dev]` — where a fallback lands. It runs the command,
  then fails the run if a canary changed. `just test`, `just test-one`,
  `just bridge-e2e`, `just seatbelt-probe` and `just bwrap-probe` all go through
  it.

`CARGO_HOME`/`RUSTUP_HOME` keep pointing at the real ones, so a build under the
wrapper does not re-download the registry. Use `just test-unprotected` when you
are debugging a test that genuinely needs your environment.

Run anything else that starts a friring binary the same way:

```bash
scripts/dev/sacrificial-env.sh cargo nextest run -E 'test(sandbox)'
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
`target/debug` first on `PATH`. Two guards run before launch (and the
client check repeats right before the TUI starts, since the build + backup
window is wide enough for someone to reopen the installed friring): it refuses
while any client is attached to the release tmux server (there is no
single-instance lock — quit the installed friring first), and it backs up
`friring.db` with `sqlite3 .backup` (`friring.db.dev-live-<timestamp>.bak`,
newest five kept) because migrations are forward-only — if your branch bumps
`SCHEMA_VERSION`, the migrated DB is the one thing the quit-and-relaunch round
trip does **not** undo: the release binary refuses a newer DB. **`sqlite3` is
required** (a torn file copy could not be trusted as the recovery snapshot); if
it is missing the launcher fails before touching anything. `--shell` /
`-- <cli args>` mirror the sandbox script; `--no-build` skips the rebuild (and
its `cargo` requirement).

**Restoring the backup.** If the release binary later refuses the migrated DB
(`schema is vN … a newer friring wrote it`), stop every friring process, then
swap the snapshot back in. A `.backup` snapshot is a single self-contained
file, so no `-wal`/`-shm` sidecars are involved:

```bash
tmux -L friring kill-server            # stop the heartbeat/agents writing to it
cd "${XDG_DATA_HOME:-$HOME/.local/share}/friring"
rm -f friring.db-wal friring.db-shm    # drop any WAL the dev build left behind
cp friring.db.dev-live-<timestamp>.bak friring.db
```

Then relaunch the installed release. (Or just keep using the dev build until
the branch ships — the migrated DB is fine for *it*.)

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
  testable. Clipboard writes are captured in memory too, so acceptance keys and
  mouse gestures can never reach the developer's host clipboard. Stable screens
  (welcome state, F1 help, theme picker) are pinned with
  **`insta`** snapshots (`src/app/snapshots/`); dynamic flows (navigation,
  modals, panel toggles, quit) assert on `App` state instead, so live
  metrics/clock never make them flaky. Runs in the normal `cargo nextest --all` —
  no tmux/TTY needed. Update snapshots with `INSTA_UPDATE=always cargo test` (or
  `cargo insta review`).
- **Invariant monkey test** (`monkey_random_events_uphold_invariants` in
  `src/app/acceptance.rs`). Seeded pseudo-random event streams (keys, chords,
  mouse, ticks, clock jumps, resizes, injected agent output) against the harness,
  rendering after **every** step and checking `assert_invariants` (selection
  indices in bounds, live session IDs unique, focus never on a hidden surface,
  panels never outlive their feature flag). A failure prints the seed + step for
  exact replay. When a "weird TUI behavior" reduces to a rule, add it to
  `assert_invariants` and let the monkey hunt for a violating sequence.
- **Black-box smoke test** (`scripts/dev/smoke/tui-smoke.sh`, `just smoke`).
  Launches the real `friring` binary inside a throwaway tmux pane (isolated
  `HOME`/XDG/`TMUX_TMPDIR`, mirroring `scripts/demo/record.sh`), drives it with
  `tmux send-keys`, and asserts on captured frames (boot → F1 → theme → quit).
  Gated behind the `tui-smoke` CI job (needs tmux).
- **Real-agent e2e** (`scripts/dev/agent-e2e/`, `just agent-e2e`). A *real*
  agent binary (Claude Code is the reference) inside a Friring-managed pane,
  with the model API stubbed on loopback — hermetic, deterministic, offline.
  One scenario description drives both the asserting bats suite and a demo
  recording (`just agent-demo <scenario>`, rendered the same way the shipped
  clips are). Not part of `cargo nextest`; runs
  via the non-blocking `agent-e2e` CI job and skips cleanly when the agent
  binary is missing. Architecture, scenario/agent-profile contracts, and the
  conformance status live in **`docs/E2E.md`** (decision record: ADR-23).
- **Performance counter tests** (`perf_*` in `src/app/acceptance.rs`). Assert on
  `App::perf_counters()` — wall-clock-free `u64` counters bumped at the
  render/tick hot paths (`MetricsState::perf`) — to gate the perf optimizations
  without flaky timing: e.g. idle iterations skip the paint, the session order is
  rebuilt only when its inputs change. Run with `cargo nextest run -E
  'test(perf_)'`. See `docs/PERFORMANCE.md`.

### Boundary probes (`scripts/dev/sandbox-probes/`)

Everything a unit test can say about a sandbox is a statement about *generated
policy text*. The probes are the other half: they ask a real kernel.

- **`just seatbelt-probe`** (macOS) starts five tmux servers — friring's own,
  one under each directory tmux derives a socket path from, and an **outer**
  server whose socket the probe process inherits through `$TMUX` — then dials
  every one of them from inside a boundary friring composed, in each of the
  three network modes. It also asserts the two **positive controls**, because a
  boundary that refused everything would pass every deny assertion and be
  useless: the session's own workspace is readable and writable, and a tmux
  server started at a `-S` path *under that workspace* is reachable. That last
  one is the documented residual — friring denies the host's sockets, not the
  concept of a socket.
- **`just bwrap-probe`** (Linux) is the twin, with the same assertions plus the
  two only a namespace can be asked: the launch is in a **pid namespace of its
  own** (compared by `/proc/self/ns/pid` against the host's), and **no relay
  survives** a launch.
- Both go through **`friring-cli sandbox exec --profile <name> -- <cmd>`**, which
  composes the boundary the same way a session launch does and never falls back
  to the host. Building the boundary inside the probe would have proved
  something about the probe.
- Both **skip rather than fail** where the platform cannot answer — a probe that
  failed on a kernel without user namespaces would be reporting the machine
  rather than friring. The conformance status each one establishes is recorded
  in [`docs/SANDBOX.md`](SANDBOX.md#conformance-what-has-been-observed).

#### A skip is not a pass, and a dedicated job may not accept one

That skip is right on a developer's machine and wrong in a job whose whole
output is these assertions. Both are served by one gate,
`scripts/dev/lib/bridge-backend.sh`, which every probe and every bridge harness
calls:

- it asks the **capability**, not the packaging — `bwrap --ro-bind / /
  --unshare-all true` — because bubblewrap installs everywhere and is then
  refused a namespace by a kernel or an LSM, which `command -v bwrap` cannot
  see;
- it carries the failure's **own stderr**, bounded, since one non-zero exit
  covers a refused namespace, a mount that could not be made and an
  inaccessible directory;
- and `FRIRING_E2E_REQUIRE_BRIDGE=1` turns every skip in these scripts — an
  absent backend, an absent tmux, a missing vendor tool — into a failure.

The three dedicated CI jobs set it and are **blocking**. Where a hosted runner
withholds user namespaces, `scripts/ci/allow-user-namespaces.sh` grants them
first: it probes and changes nothing if a namespace is already available,
prefers the AppArmor profile Ubuntu ships for bwrap alone, and only then clears
the global restriction, saying which was needed. It refuses to run anywhere but
a GitHub-hosted runner — a self-hosted one is somebody's real machine, and `CI`
and `GITHUB_ACTIONS` are equally true there. None of it touches friring's own
boundary: it changes what the kernel grants, never what a profile asks for.

`tests/harness_capability_gate.rs` is what keeps the two halves from drifting
apart again.

### The bridge, end to end (`just bridge-e2e`)

`scripts/dev/bridge-e2e.sh` is the operator-path proof for the orchestration
bridge, and the only harness that stands up a **whole friring**. In one
throwaway sandbox root it makes a git repository, installs
`extensions/bridge-conformance` from this working tree, imports its profile with
the repository path substituted in, boots the real TUI in a driver tmux and
drives the new-session wizard by keystrokes — because a bridge-requiring agent is
refused a headless create, which it also asserts. Then it reads what the host
recorded: an ownership row, a terminal state friring reached by stopping the pane
and inspecting the worktree, and a `bridge_results` verdict.

Both agents are `/bin/sh` scripts with no vendor, no login, no model and no
network, so a green run is a statement about friring rather than about an
integration. Three things it observes that `sandbox exec` cannot — because a
one-shot composes no gate and no proxy — are asserted by the leader from **inside
its own launched boundary**: friring's gate root is neither readable nor
writable, and the database is unreadable. It also captures the leader's pane off
friring's own tmux server, which is where a **nudge's delivery into a live pane**
is observed rather than inferred.

Before it installs anything or starts a TUI it runs the § 2.1 preflight: a
`friring-cli config paths` whose every reported path must canonicalize inside
this run's own root and must have come from the explicit override, never a
fallback. Every binary after that point is started through `env` with those
variables as arguments, so a tmux server that captured an older environment
cannot hand a pane a different one. `just bridge-e2e` wraps the whole thing in
`sacrificial-env.sh` as well.

Skips on any platform that is not macOS-with-seatbelt or Linux-with-bwrap, since
`Caps::bridge` is false everywhere else and there is no queue to exercise.
Artifacts land in `target/bridge-e2e/` — the TUI pane, the leader's pane, the
session row and friring's log.

### Parking a real agent (`just codex-park-e2e`)

`scripts/dev/codex-park-e2e.sh` answers the one question `bridge-e2e` cannot.
Parking exists so an agent with a **conversation** can stop and come back to it,
and a `/bin/sh` worker has no conversation — so there, "the same child resumed"
is only ever a statement about friring's bookkeeping. This harness keeps
friring's side identical (the leader is still a shell script driving the ordinary
verbs) and makes the child `extensions/codex-park`: a **real interactive Codex
CLI**, in the pane friring opened for it.

It reuses the § 2.1 preflight and the same throwaway-root discipline, and adds a
local model stub (`scripts/dev/agent-e2e/stub/openai-stub.mjs`) on loopback. The
child's seeded `config.toml` names that stub as a provider with **no `env_key`**,
which is the shape Codex accepts with no login and sends no authorization header
for; the worker wrapper also points every proxy variable at a dead port with
loopback excluded, so nothing but the stub is reachable. No credential, no
account, no billing, and the developer's own `~/.codex` is never read.

What it asserts beyond `bridge-e2e`:

- a nudge typed into a live **vendor** pane produces a turn — the child acts only
  because friring typed into it, and the pane is captured while it is alive,
  since a stopped child's window is closed;
- the process that took the pre-stop turn never runs again, so the park ended a
  process rather than only a row;
- the marker the child wrote into its private `CODEX_HOME` survives a clean stop
  and is quoted back by the relaunched process after it claims new mail;
- and, read from outside the boundary in Codex's own rollout files, the
  relaunched process comes back to the **same thread** — it carries the pre-stop
  turn as well as the new work. Turn ids make that checkable; the marker cannot,
  because it survives the stop on purpose and a blank conversation reports it
  too. Running this is what found that a bridge `resume` had been minting a new
  conversation every time, which is fixed in
  `app::bridge_saga::child_resume_identity` (`docs/SANDBOX.md`).

Two fixture-only trades, both recorded in the extension's profile: the child runs
`network_mode = "full"`, because a dynamically-numbered loopback port is not a
shape friring's egress proxy can name, and Codex's own sandbox is off, because
nesting a second seatbelt inside friring's proves nothing and fails for unrelated
reasons. What the boundary allows is proven by `bridge-e2e` (the whole bridge
with `network_mode = "none"`) and by `just seatbelt-probe`.

Skips when `codex` or `node` is absent, and on any platform without seatbelt or
bwrap. Artifacts land in `target/codex-park-e2e/`, including every child pane and
the stub's request journal.

### The omx Team fan-out (`just omx-team-e2e`)

`scripts/dev/omx-team-e2e.sh` is the scenario `docs/E2E.md` recorded for a long
time as not built. It runs the `omx` extension end to end with the **real vendor
package** as the leader: `oh-my-codex@0.21.0` into the run's own npm prefix,
`omx setup --scope user --install-mode legacy` into a throwaway `~/.codex`,
friring's own `extension install` against what that installer produced, `omx` as
a sandboxed leader with Codex behind it, `friring-omx run` fanning out one bridge
child per DAG node, and `integrate` merging what friring verified.

Everything talks to the same loopback stub the rest of the e2e family uses, with
a provider carrying no `env_key` — so there is no login, no account and no
credential. `auth.json` is a synthetic placeholder, present only because the
worker's `link-rw` seed is `required = true`.

The reason it took a harness to find anything is that it seeds **vendor**
first-run state, and each seed stands for a question a sandboxed agent cannot be
asked: OMX's one-time GitHub star prompt (answered in advance and declined, so
no `gh api` call as the operator is possible), Codex's hook trust (which the
extension install itself invalidates — see the extension README's step order),
and a stale OMX session pointer. Each is then asserted absent rather than assumed
away.

Two of its assertions are about the **boundary and the history** rather than the
outcome, and both exist because a weaker version passed while the thing they
check was broken. It reads the generated seatbelt profile of the leader and of
each child, and requires OMX's state roots in the first and in neither of the
others. And it traces each DAG node's `result.head` — the commit friring itself
verified — into `main` with `git merge-base --is-ancestor`, rather than counting
commit subjects, which cannot tell two nodes from one node that committed twice.

Skips when `codex`, `node`, `npm` or the registry is unreachable. Artifacts land
in `target/omx-team-e2e/`.

### The `omx` extension's program (`just omx-test`)

`extensions/omx/lib/friring-omx.mjs` is a dependency-free Node 20 module, so it
is tested by `node --test` rather than by cargo: `just omx-test` runs it and
skips cleanly when node is absent or older than 20. Its *manifest* contract —
that friring's argv reaches the wrappers in the order OMX accepts — is a Rust
test instead (`tests/omx_manifest_invocation.rs`), which runs each wrapper for
real against a `node` shim on `PATH`.

Three more checks need something this repository does not carry, and each skips
cleanly when what it needs is absent.

`OMX_SOURCE_DIR`, pointed at an unpacked oh-my-codex 0.21.0 tree, makes
`just omx-test` verify every **role prompt** digest in `pins.json` against the
release's own files. `OMX_CODEX_HOME`, pointed at a `CODEX_HOME` that a real
`omx setup` produced, verifies every **skill** digest — which is a different
question, because `omx setup` rewrites a skill card's frontmatter description as
it installs it and the gate is on what the operator actually has. A drifted pin
in either direction makes its `file-digest` gate in the manifest unsatisfiable,
and the operator sees a refused install with nothing to tell them whether their
tree or friring's pins are wrong.

Both come from one disposable fixture, and nothing here may run against your own
`~/.codex`:

```bash
FIX=$(mktemp -d); mkdir -p "$FIX/home" "$FIX/cache" "$FIX/npm"
(cd "$FIX" && HOME=$FIX/home npm_config_cache=$FIX/cache \
  npm pack oh-my-codex@0.21.0 && tar -xzf oh-my-codex-0.21.0.tgz)
HOME=$FIX/home npm_config_cache=$FIX/cache \
  npm install --prefix "$FIX/npm" oh-my-codex@0.21.0
(cd "$FIX" && HOME=$FIX/home CODEX_HOME=$FIX/home/.codex \
  XDG_CONFIG_HOME=$FIX/home/.config XDG_DATA_HOME=$FIX/home/.local/share \
  "$FIX/npm/node_modules/.bin/omx" setup --scope user --install-mode legacy)
OMX_SOURCE_DIR=$FIX/package OMX_CODEX_HOME=$FIX/home/.codex just omx-test
```

`tests/codex_private_state.rs` is the third: it runs the extension's two ship
gates against an installed Codex CLI — that a `copy-rewrite`d `hooks.json` fires
from the child's private `CODEX_HOME`, and that `auth.json` is written in place
through the `link-rw` link.

The same fixture answers the question the `omx-friring-team` scenario used to be
recorded as blocked on — whether the vendor leader path needs an authenticated
endpoint. It does not. Point the fixture's `~/.codex/config.toml` at the local
stub (a `[model_providers.stub]` on loopback with **no `env_key`**, as
`scripts/dev/codex-park-e2e.sh` writes) and both `omx exec "<prompt>"` and
`omx --direct` launch codex against it with no login, no account and no billing.
`friring-cli extension install extensions/omx` also exits 0 against that fixture,
which is the only check that exercises all 26 requirement gates at once. What is
still not built, and the three pieces of vendor first-run state that would have
to be seeded for it, are recorded in [`docs/E2E.md`](E2E.md).

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

### Website

```bash
npm ci                               # install deps (use lockfile)
npm run build:website                # build the site into _site/
npm run dev:website                  # build + serve with live rebuild
npm run lint:website                 # run all website linters
npm run fmt:website                  # auto-fix formatting (Prettier)
```

`website/css/{variables,base,layout,components}.css` are the authored sources
for the shared chrome; the Eleventy build concatenates them into the generated
`_site/css/core.css` that every page links. Edit the sources — the bundle is
overwritten on every build.

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
waits that poll the same pane the test polls — into `target/agent-e2e/demos/`.
It goes through the pipeline below (asciinema + `drive-tape.mjs` + agg), off a
generated tape rather than a hand-written one (see `docs/E2E.md`).

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
- **`Sleep` is the viewer's time; `Wait` is the app's.** Use `Sleep` only for a
  beat someone is meant to read. When the tape is waiting on the app — a picker
  building its list, a forked agent CLI booting — use `Wait`, which polls
  `tmux capture-pane` and continues the moment the screen says it is ready:

  - `Wait /<regex>/ [<timeout>]` — until the pane matches. Prefer this whenever
    the beat has a marker; `agent_ready_marker()` in `record.sh` has one per
    agent (`claude` → `❯`, `codex` → `›`, `opencode` → `Build ·`).
  - `Wait Stable [<quiet>] [<timeout>]` — until the pane *changes* and then
    holds still for `quiet` (default 250ms). Both halves matter: waiting only
    for quiet resolves inside the gap before the app reacts and calls the old
    screen settled.

  Limits, all of them learned by shipping a clip that was wrong:

  - **Quiet is a proxy for readiness, and they come apart.** A beat that
    repaints, pauses longer than the settle window, then repaints again — a
    picker closing, then an agent CLI painting 3.5s later — satisfies `Wait
    Stable` on the intermediate screen. Hence the per-beat `<quiet>` argument,
    and hence preferring a marker. The driver detects this for free and prints a
    `note:` naming the line: no key is sent during the `Sleep` after a wait, so
    anything that moves there was the app still working. It is a note, not an
    error — it is *expected* when a tape deliberately stops short of a later
    stage, which `fork.tape` and `session-creation.tape` both do rather than
    film seconds of blank terminal.
  - **A settle window is dwell.** `Wait Stable <quiet>` ends by definition on
    `quiet` ms of unchanged screen, and the `Sleep` after it lands on that same
    frame, so `quiet + Sleep` is one held frame and must stay under the budget.
    Worse, every poll spawns `tmux`, so the wall-clock cost overshoots the
    nominal figure under load — a 600ms window rendered a 1.04s held frame.
  - `capture-pane` returns **text without styling**, so `Wait Stable` cannot see
    a colour-only repaint (committing a theme) — use a plain `Sleep`.
  - It needs the screen to actually go quiet, which the multi-session view never
    does for long because live agent panes repaint themselves; there it measures
    seconds.
  - A `Wait` that never resolves is a hard error, deliberately: a beat that
    changes nothing is a bug in the tape, and the earlier lenient behaviour
    filmed the wait as a frozen frame instead of saying so.
- **Every clip is held to a pacing budget** (`lib/check-pacing.mjs`), enforced by
  the recorder before a take is allowed to replace good media, and again in CI.
  A held frame may not exceed 1.0s (0.5s is the target) and the opening may not
  exceed 0.75s. The budget also rejects a clip whose **final frame is blank**,
  which is a correctness check rather than a pacing one: the detach teardown is
  chunked by the pty and can leave a screen-clear behind that `trim-cast.mjs`
  does not catch, and an empty closing frame is otherwise perfectly well-paced.
  Runtime is deliberately *not* budgeted — a clip may be as long as it earns.
  See `FORK.md` § Demo pacing budget for the measurements behind the numbers.
- **Record on an idle machine.** The pipeline drives real processes in real
  time, so load distorts it badly: at load ~5 the same tape recorded 2.5x
  longer, the settle after closing the code-review view stretched 0.3s → 3.0s
  and swallowed the `Ctrl+N` that followed it, and `Wait Stable` overshot its
  window because every poll spawns tmux. A take that wedges or busts the budget
  under load is not necessarily a tape bug — re-run it on a quiet box first.
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
