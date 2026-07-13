# shellcheck shell=bash
#
# Perf scenario: a real Claude Code turn whose reply is a large streamed flood
# (~85KB, 1200 lines, no pacing) rendered through the Friring pane — realistic
# load on the whole pipeline (tmux -> control-mode reader -> vt100 -> tui_term)
# with an exactly reproducible input signal. SCENARIO_PERF=1 makes the TUI
# publish its perf snapshot and writes a report (wall-clock marks + counters +
# frame/tick percentiles) under target/agent-e2e/perf/.
#
# The report is a benchmark, not a gate: the test passes/fails on the
# functional asserts and on the perf-publishing chain working, never on
# timing thresholds (wall-clock gates flake on shared runners — see
# docs/PERFORMANCE.md for the counter-based regression tests). For numbers
# worth comparing, run against a release build:
#   FRIRING_E2E_BIN=target/release/friring just agent-e2e 'perf'
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Flood turn: large streamed reply through Friring with perf capture"
SCENARIO_AGENT="claude"
SCENARIO_PERF=1
SCENARIO_PROMPT="Reply with the flood."
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="PERF-FLOOD-DONE"

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    perf_mark ready
    step_type "$SCENARIO_PROMPT"
    step_key Enter
    perf_mark prompt_sent
    step_wait_state 'working|done' 30
    step_wait_pane "$SCENARIO_DONE_PATTERN" 120
    perf_mark flood_rendered
    step_wait_state 'done' 60
    perf_mark turn_done
}

scenario_assert_effects() {
    [ "$(journal_matched flood)" -ge 1 ] || e2e_die "flood fixture never matched"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
}
