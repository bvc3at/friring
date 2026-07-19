#!/usr/bin/env bats
# Real-agent e2e suite: real agent binaries against a local model stub,
# driven through the real Friring TUI. Invoke via scripts/dev/agent-e2e/run.sh
# (which builds the dev binaries first). See docs/E2E.md.
#
# Layered on purpose — when the full scenario breaks, the shallower tests
# localize it: protocol (binary↔stub contract) → interactive (agent TUI, no
# Friring) → full scenario (Friring rendering, key forwarding, status hooks).

setup() {
    AGENT_E2E_DIR="$(cd "$BATS_TEST_DIRNAME" && pwd)"
    export AGENT_E2E_DIR
    # shellcheck disable=SC1091
    source "$AGENT_E2E_DIR/lib/harness.sh"
    e2e_require_tools test
}

# Per-test agent gate: an agent binary that is missing — or present but not
# usable — skips only THAT agent's tests, so a machine with any subset of
# claude/codex/opencode/agy stays green. e2e_boot re-sources the profile itself
# (via SCENARIO_AGENT); this early source exists purely for the skip check.
#
# The version probe is the usability check, not just discovery theatre: a
# binary that cannot even print `--version` within the bound cannot run a
# scenario either, and skipping it reports that in one line instead of burning
# the suite's wall-clock on waits that were always going to time out.
require_agent() {
    # shellcheck disable=SC1090
    source "$AGENT_E2E_DIR/agents/$1/profile.sh"
    agent_binary >/dev/null 2>&1 \
        || skip "$1 binary not found (install it or pin via FRIRING_E2E_*_BIN)"
    [ -n "$(agent_version)" ] \
        || skip "$1 binary is present but did not respond to --version"
}

teardown() {
    # bats runs teardown() even when the test failed or timed out, which makes
    # it the reliable reaper (an in-body trap can be bypassed on hard kills).
    local failed=1
    [ -n "${BATS_TEST_COMPLETED:-}" ] && failed=0
    e2e_teardown "$failed" || true
}

@test "protocol: claude -p completes the stubbed tool-use loop (no Friring)" {
    require_agent claude
    e2e_protocol_smoke "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
}

@test "interactive: claude TUI completes the stubbed tool-use loop in bare tmux (no Friring)" {
    require_agent claude
    e2e_interactive_smoke "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
}

@test "e2e: claude text turn through Friring with working→done status transition" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-text-turn"
}

@test "e2e: claude tool-use loop through Friring writes a real workspace file" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
}

@test "e2e: claude review loop — annotate, structured handoff to the agent, re-review nudge" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-review-loop"
}

@test "e2e: multi-repo wizard with a named workspace dir — spawn, symlink write, persistence, guarded delete" {
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-named-workspace"
}

@test "perf: claude flood turn through Friring publishes a perf report" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-perf-flood"
}

@test "protocol: codex exec completes a stubbed text turn (no Friring)" {
    require_agent codex
    e2e_protocol_smoke "$AGENT_E2E_DIR/scenarios/codex-text-turn"
}

@test "e2e: codex text turn through Friring against the openai-dialect stub" {
    require_agent codex
    e2e_scenario "$AGENT_E2E_DIR/scenarios/codex-text-turn"
}

@test "protocol: opencode run completes a stubbed text turn (no Friring)" {
    require_agent opencode
    e2e_protocol_smoke "$AGENT_E2E_DIR/scenarios/opencode-text-turn"
}

@test "e2e: opencode text turn through Friring against the openai-dialect stub" {
    require_agent opencode
    e2e_scenario "$AGENT_E2E_DIR/scenarios/opencode-text-turn"
}
