# shellcheck shell=bash
#
# Scenario: a real Claude Code binary answers a prompt through the Friring
# TUI, with the model stubbed locally, and its status hook transition
# (working -> done) observed in the session DB.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Real Claude Code text turn rendered through Friring, model stubbed"
SCENARIO_AGENT="claude"
SCENARIO_PROMPT="Is the habitat ring thermal loop still holding after the swap?"
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="RING-NOMINAL"

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_sleep 1
    step_type "$SCENARIO_PROMPT"
    step_sleep 1
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
    step_sleep 2
}

scenario_assert_effects() {
    [ "$(journal_matched text-turn)" -ge 1 ] || e2e_die "text-turn fixture never matched"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
