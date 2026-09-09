# friring dev task runner. Run `just` (or `just --list`) to see tasks.
#
# Enter the pinned toolchain first with `nix develop` (or `direnv allow`); these
# tasks assume the dev tools (cargo-nextest, cargo-deny, rumdl, shellcheck, …)
# are on PATH. See docs/DEVELOPMENT.md.

# Default: show the task list.
default:
    @just --list

# Type-check everything.
check:
    cargo check --all

# Build the dev binaries (TUI + CLI).
build:
    cargo build --bin friring --bin friring-cli

# Run the full test suite (nextest), under a throwaway outer HOME/XDG so a test
# that starts a friring binary cannot reach the developer's own config or
# database even if its own isolation is wrong (scripts/dev/sacrificial-env.sh).
test:
    scripts/dev/sacrificial-env.sh cargo nextest run --all

# The same suite with the developer's real environment. For debugging a test
# that depends on it; not the default for the reason above.
test-unprotected:
    cargo nextest run --all

# Run a single test by name: `just test-one perf_`.
test-one NAME:
    scripts/dev/sacrificial-env.sh cargo nextest run -E 'test({{NAME}})'

# Format Rust + website code.
fmt:
    cargo fmt --all
    npm run fmt:website

# Lint everything CI lints (Rust + deny + markdown + shell).
lint:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo deny check advisories
    cargo deny check bans licenses sources
    rumdl check .
    git ls-files -z '*.sh' | xargs -0 shellcheck

# Architecture-rule + doc checks.
arch:
    cargo test --test architecture_rules
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features

# Install the non-Nix dev tools (fallback when not using the flake).
dev-tools:
    scripts/install-dev-tools.sh

# Install the prek git hooks.
hooks-install:
    prek install

# Run the dev TUI in the persistent default sandbox.
sandbox *ARGS:
    scripts/dev/sandbox.sh {{ARGS}}

# Run the dev TUI in a throwaway sandbox (wiped on exit).
sandbox-fresh:
    scripts/dev/sandbox.sh --fresh

# Drop into a shell with the sandbox env (run `friring-cli …` by hand).
sandbox-shell:
    scripts/dev/sandbox.sh --shell

# Wipe a persistent sandbox profile (default: "default").
sandbox-clean PROFILE="default":
    scripts/dev/sandbox.sh --clean {{PROFILE}}

# Run the dev TUI against your REAL sessions (release socket/session/DB/config).
dev-live *ARGS:
    scripts/dev/live.sh {{ARGS}}

# Black-box TUI smoke test (real binary in a throwaway tmux pane).
smoke:
    scripts/dev/smoke/tui-smoke.sh

# Real-agent e2e suite: real agent binary, model stubbed locally, offline.
agent-e2e *ARGS:
    scripts/dev/agent-e2e/run.sh {{ARGS}}

# Observe the seatbelt boundary against a real kernel (macOS only).
seatbelt-probe:
    scripts/dev/sacrificial-env.sh scripts/dev/sandbox-probes/seatbelt.sh

# Observe the bwrap boundary against a real kernel (Linux, user namespaces).
bwrap-probe:
    scripts/dev/sacrificial-env.sh scripts/dev/sandbox-probes/bwrap.sh

# Run the bridge-conformance extension end to end against a real TUI. Two rings
# of isolation: the harness proves its own with `friring-cli config paths`
# before it installs anything, and the wrapper bounds what a wrong proof could
# reach.
bridge-e2e:
    scripts/dev/sacrificial-env.sh scripts/dev/bridge-e2e.sh

# Park and resume a bridge child that is a real interactive Codex, against a
# local model stub — no credential, no account, no network. Same two rings of
# isolation as `bridge-e2e`; skipped when codex or node is absent.
codex-park-e2e:
    scripts/dev/sacrificial-env.sh scripts/dev/codex-park-e2e.sh

# The omx extension's Team fan-out end to end: the real vendor package as a
# sandboxed leader, one real Codex worker per DAG node, all against a local
# model stub with no credential. Installs oh-my-codex into the run's own npm
# prefix; skipped when codex, node or the registry is unreachable.
omx-team-e2e:
    scripts/dev/sacrificial-env.sh scripts/dev/omx-team-e2e.sh

# The omx extension's own program tests (Node 20+; skipped when node is absent).
omx-test:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v node >/dev/null 2>&1; then
        echo "omx-test: node is not installed; skipping" >&2
        exit 0
    fi
    major=$(node --version | sed 's/^v//; s/\..*//')
    if [ "$major" -lt 20 ]; then
        echo "omx-test: node $major is older than 20; skipping" >&2
        exit 0
    fi
    node --test "extensions/omx/lib/*.test.mjs"

# Record a scenario as a demo clip: `just agent-demo claude-tool-loop`.
agent-demo *ARGS:
    scripts/dev/agent-e2e/run.sh --demo {{ARGS}}

# Drive tests against a real SSH host: `just lab <host> <verb>`.
lab HOST *ARGS:
    scripts/dev/e2e/real-host.sh {{HOST}} {{ARGS}}
