# shellcheck shell=bash
#
# Scenario: the F9 activity view over a real stubbed turn. The stub scripts a
# Write then a Bash round-trip; the real Claude Code binary executes both
# against the throwaway workspace and its transcript lands under the
# sandbox's CLAUDE_CONFIG_DIR. Pressing F9 must then render the retrospective
# from that transcript alone: the Overview dashboard (stat tiles) and the
# turn-grouped Timeline (prompt turn header + gutter event rows). No real
# user data is ever touched — HOME, XDG and CLAUDE_CONFIG_DIR are all
# sandbox-private, and the model is a loopback stub.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="F9 activity view renders dashboard + turn timeline from a stubbed tool turn"
SCENARIO_AGENT="claude"
SCENARIO_PROMPT="Create activity.txt with the Write tool, then run the marker command."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="ACTIVITY-DONE"

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_sleep 1
    step_type "$SCENARIO_PROMPT"
    step_sleep 1
    step_key Enter
    step_wait_state 'working|done' 30
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
    # Open the activity view; the ~1 s background event scan fills it in.
    step_key F9
    step_wait_pane "Activity · Overview" 20
    # Dashboard tiles reflect the turn: one edit (Write), one command (Bash).
    step_wait_pane "1 edits" 20
    step_wait_pane "1 cmds" 20
    # The Timeline groups the turn under its prompt header.
    step_key 2
    step_wait_pane "Activity · Timeline" 20
    step_wait_pane "Create activity.txt" 20
    step_sleep 2
}

scenario_assert_effects() {
    assert_ws_file_eq activity.txt "Activity view e2e artifact"
    [ "$(journal_matched activity-write)" -ge 1 ] \
        || e2e_die "activity-write fixture never matched"
    [ "$(journal_matched activity-bash)" -ge 1 ] \
        || e2e_die "activity-bash fixture never matched (Write result never posted)"
    [ "$(journal_matched activity-done)" -ge 1 ] \
        || e2e_die "activity-done fixture never matched (Bash result never posted)"
}

scenario_assert_ui() {
    # Timeline is open: the typed prompt is a turn header, the agent's
    # actions sit in its gutter as tagged rows.
    assert_pane_contains "Activity · Timeline"
    assert_pane_contains "▶"
    assert_pane_contains "│"
    assert_pane_contains "activity.txt"
    assert_pane_contains "echo ACTIVITY-MARKER"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
