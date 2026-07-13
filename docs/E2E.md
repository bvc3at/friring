# Real-agent e2e & scenario demos

Friring's deepest integration risk is the one nothing in `cargo nextest` exercises: a **real
coding-agent binary** doing real work inside a Friring-managed tmux pane. This harness
(`scripts/dev/agent-e2e/`) closes that gap. A feature is described **once** as a *scenario*, and
that one description runs two ways:

```bash
just agent-e2e                        # asserting, hermetic, offline e2e suite (bats)
just agent-demo claude-tool-loop      # the same scenario as a VHS demo recording
```

The agent binary is real (Claude Code is the reference agent); the **model API is stubbed
locally**, so runs are deterministic, fully offline, and free. See ADR-23 in
`docs/ARCHITECTURE.md` for the decision record.

## The seam: stub the model at the HTTP boundary

The stub (`stub/anthropic-stub.mjs`, a zero-dependency node ≥ 18 sidecar — deliberately outside
the Rust dependency graph) speaks the Anthropic Messages dialect: `POST /v1/messages` answered as
an SSE stream, including `tool_use` blocks and `input_json_delta`. The agent is pointed at it via
`ANTHROPIC_BASE_URL` — plain HTTP on loopback works against the pinned binary; no TLS games.

Responses come from **hand-curated semantic fixtures** (`fixtures.json` per scenario), not
recorded cassettes: tool-use loops make raw record/replay brittle (request bodies grow
cumulatively and embed machine-specific tool results). A fixture matches on stable turn shape —
`modelContains`, `promptContains` / `anyUserContains` (last / any user message), `hasToolResult`,
`toolResultFor` (a pinned `tool_use` id) — first match wins, `{{WS}}` is substituted with the
run's workspace path. `ambient: true` marks background traffic (e.g. side-model calls) that is
answered but not required; `maxUses` guards against loops; `delayMs` paces SSE deltas for demos.
**List `ambient` fixtures first**: they are model-keyed (e.g. `modelContains: "haiku"`), so an
ambient-first order catches a side call before a primary fixture whose prompt text it happens to
echo can shadow it. There is deliberately no catch-all default — it would answer surprise calls
`200` and silently disable the strictness the `UNMATCHED` marker enforces.

Strictness is enforced **at assert time, not response time**: an unmatched model call gets a
benign marker reply (so the pane stays alive and debuggable) plus an `UNMATCHED` journal entry,
and the post-run invariant fails the scenario on any `UNMATCHED` — or on any non-ambient fixture
that was never exercised. The journal (`journal.jsonl` + raw request bodies) is both the top
assertion layer and the failure artifact.

## Three drive depths

The same scenario runs at three depths, so a failure localizes itself:

1. **protocol** — `claude -p` against the stub. No tmux, no Friring. Proves the binary↔stub
   contract (streaming, tool loop, auth/onboarding bypass).
2. **interactive** — the agent's own TUI in a bare tmux pane. Proves interactive-mode behavior
   (extra traffic, trust dialogs) without Friring in the loop.
3. **full scenario** — through the real Friring TUI: headless `session create`, TUI boot +
   adoption, keystrokes forwarded through the session panel, status hooks, pane rendering.

Assertion layers, most → least semantic: stub journal (every expected call matched, no
surprises) → workspace filesystem/git side effects → session `hook_state` transitions (via
`friring-cli session get --json`; the raw persisted value, deliberately not the TUI's derived
status) → targeted pane text. Waits are always bounded event-polls; **`step_sleep` is a no-op in
test mode** (demo pacing only), so a scenario physically cannot lean on a fixed sleep to pass.

## Hermeticity & offline model

- `tbx_sandbox_init_full fresh` (shared `scripts/dev/lib/sandbox-env.sh`): throwaway
  `HOME`/`XDG_*`/`TMUX_TMPDIR`, dev `friring-dev` socket in a private dir. Nothing touches the
  real `~/.claude`, `~/.config/friring`, or any running tmux server. Leaked `FRIRING_*` identity
  vars (from running inside a Friring session) are scrubbed.
- Agent env (`ANTHROPIC_BASE_URL`, dummy `ANTHROPIC_AUTH_TOKEN`, telemetry kill-switches) is
  exported **before the first tmux command** — panes inherit the tmux *server* environment, which
  freezes at server start. That ordering is load-bearing; it is how the stub URL reaches the
  agent with zero core changes.
- Offline enforcement is app-level: `http(s)_proxy` point at a dead loopback port with
  `no_proxy=127.0.0.1,localhost`, and the tool-use loop is proven to survive that, so nothing
  external is load-bearing. A kernel-level egress block (netns/iptables) would be a CI hardening
  step on top, not a replacement.
- Teardown (bats `teardown()`, runs on failure too) reaps the stub by PID, the driver tmux
  server, and the `friring-dev` server (which kills the agent panes), then wipes the sandbox
  root. On failure, artifacts land in `target/agent-e2e/artifacts/<scenario>-<ts>/`: both panes,
  journal + raw bodies, `agents.toml`, workspace diff, redacted env, versions.

## Scenario anatomy

```text
scripts/dev/agent-e2e/scenarios/<name>/
  scenario.sh     metadata + scenario_steps() + assertions (plain bash, no DSL)
  fixtures.json   the stub's semantic model script
  workspace/      optional seed files for the git workspace
```

`scenario.sh` sets `SCENARIO_*` vars (`AGENT`, `PROMPT`, `AGENT_READY`, `DONE_PATTERN`, …) and
defines `scenario_steps()` plus `scenario_assert_effects()` (mode-independent: journal, files)
and `scenario_assert_ui()` (pane/status). Steps use a small dual-mode vocabulary — `step_type`,
`step_key`, `step_wait_pane`, `step_wait_state`, `step_sleep` — that either drives the driver
tmux and polls (test mode) or emits VHS tape lines (demo mode; `step_wait_pane` becomes
`Wait+Screen@timeout /regex/`). Keep steps a flat list: no branching, loops, or variables — the
moment a scenario needs logic, that logic belongs in the assert functions or the harness, not in
a grown-by-accident DSL.

Scenario keystrokes go wherever the TUI routes them: an adopted session boots with **Terminal
focus**, so plain typing lands in the agent pane; chords in the terminal-passthrough set are
forwarded to the agent, and `Ctrl+H` returns focus to the session list for Friring-UI actions.
Demo-able scenarios must stick to keys VHS knows (no F-keys; `C-x` → `Ctrl+X`).

## Agent profiles

`agents/<name>/profile.sh` is the whole per-agent surface — adding an agent is a profile plus
(if it speaks a new API) a stub dialect, never a harness change:

| Contract item | Meaning |
|---|---|
| `AGENT_NAME` | `agents.toml` entry name (hooks patch by name — `claude` is load-bearing) |
| `AGENT_STUB_DIALECT` | which `stub/<dialect>-stub.mjs` to boot, or `none` |
| `AGENT_HAS_STATUS_HOOKS` | `1` if the built-in hooks extension wires this agent's signals |
| `AGENT_LAUNCH_ARGS` | flags shared by all three drive depths |
| `agent_binary` / `agent_version` | discovery (env-var pin override → `PATH`) |
| `agent_env` | `KEY=VALUE` lines exported before any tmux server starts |
| `agent_seed_config <ws>` | pre-seed config so the binary runs non-interactively |
| `agent_agents_toml_entry` | the `[[agents]]` entry (absolute binary path) |

`AGENT_STUB_DIALECT="none"` **declares** an agent unstubbable (e.g. a CLI hard-wired to GitHub
auth): its scenarios refuse to run offline with a clear message instead of faking anything.
Status hooks are likewise a declared capability, not a framework assumption — `step_wait_state`
errors on an agent that never signals.

The Claude profile pins down what a new profile typically needs: `ANTHROPIC_AUTH_TOKEN` (Bearer;
the API-key path prompts interactively), a seeded `.claude.json` with `hasCompletedOnboarding`,
`bypassPermissionsModeAccepted` and per-workspace `hasTrustDialogAccepted`, and the
nonessential-traffic kill switches.

## Demo mode

`run.sh --demo <scenario>` boots the *same* hermetic env + stub (env inheritance mirrors
`scripts/demo/record.sh`: everything exported before the `session create` that starts the
`friring-dev` server), generates a tape with the standard `scripts/demo` Set block, runs `vhs`,
and writes `target/agent-e2e/demos/<name>.{gif,mp4}`. `SCENARIO_DEMO_THEME` seeds
`metadata.active_theme` like `record.sh` does. Unlike the hand-written tapes, generated tapes
synchronize on `Wait+Screen` instead of open-loop sleeps, so a slow turn can't desync the
recording; `step_sleep`/`delayMs` control the rhythm.

VHS renders through a headless Chromium (go-rod): a packaged system browser is used when
present. The dead-proxy vars are dropped for the `vhs` process only — the agent pane's env was
frozen into the tmux server before vhs starts, so the offline guarantee is unaffected.

Tape generation is factored out of recording (`e2e_emit_tape`): `run.sh --emit-tape <scenario>`
writes the `.tape` and prints its path **without** booting a session or running vhs — pure
step→tape mapping, so it works offline with no agent binary and is unit-tested (`unit.bats`).

## Running & knobs

```bash
just agent-e2e                              # whole suite (builds dev binaries first)
just agent-e2e 'tool-use loop'              # filter by TEST NAME (bats --filter regex)
just agent-demo claude-text-turn            # record a scenario as gif+mp4
scripts/dev/agent-e2e/run.sh --emit-tape claude-text-turn  # .tape only, offline
scripts/dev/agent-e2e/run.sh --list         # list scenarios
```

The suite runs `unit.bats` (pure-shell harness tests — the strict-offline invariant and the tape
generator, no agent binary) before `suite.bats` (the real-agent scenarios), so a green run always
verified the harness logic even where claude is absent. The `--filter` matches bats *test names*,
not scenario directory names — a non-matching filter runs zero tests and still exits green (bats
semantics), so check the `1..N` line when filtering.

`FRIRING_E2E_CLAUDE_BIN` pins the binary; `FRIRING_E2E_KEEP=1` keeps the sandbox for post-mortem;
`FRIRING_E2E_SKIP_BUILD=1` skips the cargo build. Requires tmux, node ≥ 18, jq, git, curl,
`timeout` (coreutils — `gtimeout` on macOS), bats (tests) / vhs + sqlite3 + a browser (demos). A
missing *agent binary* makes the real-agent scenarios **skip**, not fail, so machines without
claude stay green; missing infrastructure tools are hard errors.

CI: the `agent-e2e` job (`.github/workflows/ci.yml`) installs tmux + bats + the **pinned**
`@anthropic-ai/claude-code` and runs the suite. It is path-gated like every job and deliberately
**not** in `all-checks.needs` — it exercises an externally-pinned binary, so its failures need a
human eye (conformance drift vs real regression) and must never block a merge.

## Performance scenarios

A scenario with `SCENARIO_PERF=1` doubles as a **benchmark rig**: the whole real pipeline (tmux →
control-mode reader → `vt100` → `tui_term`) under an exactly reproducible load — the stub's
`flood` fixture field generates large streamed replies (`{"line": "…", "count": 1200}` ≈ 85KB in
1KiB SSE chunks) without megabytes of hand-written JSON. The harness exports `FRIRING_PERF_LOG=1`
to the TUI, records wall-clock marks (`perf_mark`, ~100ms resolution), and writes a report to
`target/agent-e2e/perf/<scenario>-<ts>/`: mark deltas, plus the TUI's own published snapshot
(`friring-cli perf` — counters, frame/tick percentiles, startup breakdown, slow ops).

Reports are benchmarks, **not gates**: the test passes/fails on functional asserts and on the
perf-publishing chain working (a missing snapshot fails the scenario), never on timing thresholds
— wall-clock gates flake on shared runners, and counter-based regression *gating* stays with the
`perf_*` tests (`docs/PERFORMANCE.md`). Numbers from the default debug build are for plumbing
only; for measurements, point the harness at a release build:

```bash
cargo build --release --bins
FRIRING_E2E_BIN=target/release/friring just agent-e2e 'perf'
```

The shipped `claude-perf-flood` scenario is the template: flood turn through the pane, marks at
ready / prompt-sent / flood-rendered / turn-done, snapshot polled after the turn (the TUI
publishes once per ~1000-tick perf window, so the report waits up to 30s for it).

## Updating the pinned agent binary

The npm pin in the CI job is the version the fixtures are conformance-tested against (not
tracked by renovate — it lives in a run command on purpose). To bump: update the pin and run
`just agent-e2e` locally against that version. Two drift signals catch new traffic for you: a new
**model / side-model call** with no matching fixture is `UNMATCHED` — a hard failure — so cover
genuine new background traffic with an `ambient` fixture; a new **non-message endpoint** (a
telemetry or config probe) is harmless (a custom `ANTHROPIC_BASE_URL` proxy ignores it too), so
it doesn't fail the run but is surfaced to `target/agent-e2e/unexpected-endpoints.log` and a CI
`::warning::` annotation — review it, and extend the allowlist in
`e2e_surface_unexpected_endpoints` if it's expected.

## Conformance status (claude 2.1.207)

Proven empirically: plain-HTTP loopback `ANTHROPIC_BASE_URL`; SSE streaming required; full
tool-use loop (real `Write` executed, `tool_result` posted with the pinned id); `-p` and
interactive modes; traffic is `HEAD /` + `POST /v1/messages?beta=true` only (no `count_tokens`,
no side-model calls, with or without the nonessential-traffic switch, in these flows); the whole
loop survives dead-proxied egress; interactive mode makes **no** model calls before the first
prompt. The `❯` input-box glyph is the ready marker; the footer text varies by permission mode.
