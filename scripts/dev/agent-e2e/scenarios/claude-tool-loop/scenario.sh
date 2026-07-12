# shellcheck shell=bash
#
# Scenario: a real tool-use loop through Friring. The stub replies with a
# pinned Write tool_use; the real Claude Code binary executes its Write tool
# against the throwaway workspace, posts the tool_result back, and the stub's
# follow-up lands in the pane. Asserts the file side effect on disk, the
# journal (tool_result carried the pinned id), and the done status.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Real Claude Code tool-use loop (Write) through Friring, model stubbed"
SCENARIO_AGENT="claude"
SCENARIO_PROMPT="Create hello.txt using the Write tool."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="TOOL-LOOP-DONE"

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
    assert_ws_file_eq hello.txt "Hello from the Friring e2e stub!"
    [ "$(journal_matched write-tool-call)" -ge 1 ] \
        || e2e_die "write-tool-call fixture never matched"
    [ "$(journal_matched after-write)" -ge 1 ] \
        || e2e_die "after-write fixture never matched (tool_result never posted)"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
