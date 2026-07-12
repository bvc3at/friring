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
    # shellcheck disable=SC1091
    source "$AGENT_E2E_DIR/agents/claude/profile.sh"
    agent_binary >/dev/null 2>&1 \
        || skip "claude binary not found (set THURBOX_E2E_CLAUDE_BIN or install claude)"
}

teardown() {
    # bats runs teardown() even when the test failed or timed out, which makes
    # it the reliable reaper (an in-body trap can be bypassed on hard kills).
    local failed=1
    [ -n "${BATS_TEST_COMPLETED:-}" ] && failed=0
    e2e_teardown "$failed" || true
}

@test "protocol: claude -p completes the stubbed tool-use loop (no Friring)" {
    e2e_protocol_smoke "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
}

@test "interactive: claude TUI completes the stubbed tool-use loop in bare tmux (no Friring)" {
    e2e_interactive_smoke "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
}

@test "e2e: claude text turn through Friring with working→done status transition" {
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-text-turn"
}

@test "e2e: claude tool-use loop through Friring writes a real workspace file" {
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
}

@test "perf: claude flood turn through Friring publishes a perf report" {
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-perf-flood"
}
