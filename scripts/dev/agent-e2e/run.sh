#!/usr/bin/env bash
#
# Real-agent e2e runner — one scenario description, two outputs:
#
#   run.sh                     run the whole asserting suite (bats)
#   run.sh <filter…>           run matching tests only (bats --filter, regex)
#   run.sh --demo <scenario…>  record the scenario(s) as VHS demos instead
#                              (target/agent-e2e/demos/<name>.{gif,mp4})
#   run.sh --list              list scenarios
#
# Env knobs:
#   THURBOX_E2E_CLAUDE_BIN     pin the claude binary (else `claude` on PATH)
#   THURBOX_E2E_KEEP=1         keep the throwaway sandbox for debugging
#   THURBOX_E2E_SKIP_BUILD=1   don't cargo-build first (binaries are current)
#
# Hermetic + offline by construction: throwaway HOME/XDG/tmux dirs, the model
# API stubbed on loopback, all other HTTP(S) egress dead-ended. Requires:
# tmux, node >= 18, jq, git, curl, bats (tests) / vhs + sqlite3 (demos), and
# the agent binary — tests SKIP (not fail) when the agent binary is missing.
# See docs/E2E.md.
set -euo pipefail

AGENT_E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export AGENT_E2E_DIR
REPO_ROOT="$(cd "$AGENT_E2E_DIR/../../.." && pwd)"

die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

MODE="test"
ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --demo) MODE="demo" ;;
        --list)
            for d in "$AGENT_E2E_DIR"/scenarios/*/; do
                basename "$d"
            done
            exit 0
            ;;
        --keep) export THURBOX_E2E_KEEP=1 ;;
        -h|--help)
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) ARGS+=("$1") ;;
    esac
    shift
done

if [ "${THURBOX_E2E_SKIP_BUILD:-0}" != "1" ]; then
    ( cd "$REPO_ROOT" && cargo build --bin thurbox --bin thurbox-cli )
fi

if [ "$MODE" = "demo" ]; then
    [ "${#ARGS[@]}" -gt 0 ] || die "--demo needs at least one scenario name (see --list)"
    for name in "${ARGS[@]}"; do
        dir="$AGENT_E2E_DIR/scenarios/$name"
        [ -d "$dir" ] || die "no such scenario: $name (see --list)"
        # Subshell per scenario: each recording gets a fresh sandbox and its
        # own teardown, and one failure doesn't poison the next env.
        (
            # shellcheck disable=SC1091
            source "$AGENT_E2E_DIR/lib/harness.sh"
            e2e_require_tools demo
            trap 'e2e_teardown 1' ERR
            e2e_scenario_load "$dir"
            e2e_boot demo
            e2e_demo_record
            e2e_teardown 0
        )
    done
    exit 0
fi

command -v bats >/dev/null 2>&1 \
    || die "bats not found — install bats-core (https://bats-core.readthedocs.io)"

if [ "${#ARGS[@]}" -gt 0 ]; then
    filter="$(IFS='|'; echo "${ARGS[*]}")"
    exec bats --filter "$filter" "$AGENT_E2E_DIR/suite.bats"
fi
exec bats "$AGENT_E2E_DIR/suite.bats"
