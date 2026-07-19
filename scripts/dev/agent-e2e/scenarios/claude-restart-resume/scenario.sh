# shellcheck shell=bash
#
# Scenario: session restart resumes the SAME claude conversation. After a
# first stubbed turn, Ctrl+H -> Ctrl+R (Ctrl+R is terminal-passthrough)
# respawns the pane through the agent's resume template
# (`claude --resume {id}`). The resumed binary replays the transcript
# locally — turn 1's marker reappears with NO model call (the journal pins
# turn 1's fixture to exactly one match), the persisted agent_session_id is
# unchanged, and a second turn proves the conversation actually continues.
#
# Test-mode only (extracts the conversation id via friring-cli mid-steps);
# not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Ctrl+R restart resumes claude with --resume: same id, local replay, conversation continues"
SCENARIO_AGENT="claude"
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_sleep 1
    step_type "This is the first resume turn."
    step_sleep 1
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "RESUME-MARKER-ONE" 60
    step_wait_state 'done' 60

    # The conversation id friring minted (--session-id {id}) — captured
    # BEFORE the restart so assert_effects can prove it survived unchanged.
    E2E_AGENT_CONVO_ID="$(friring-cli --json session get "$E2E_SESSION_ID" \
        | jq -r '.agent_session_id')"
    [ -n "$E2E_AGENT_CONVO_ID" ] && [ "$E2E_AGENT_CONVO_ID" != "null" ] \
        || e2e_die "session has no agent_session_id" || return 1

    # Ctrl+R is terminal-passthrough, so it must not restart while the
    # terminal is focused — leave for the session list first (Ctrl+H).
    step_key C-h
    step_key C-r
    step_wait_pane "Session restarted" 30
    # Restart leaves the list focused; Enter re-opens the session so turn 2
    # typing lands on the resumed claude's stdin (Terminal focus).
    step_key Enter
    step_wait_pane "$SCENARIO_AGENT_READY" 120
    # The resumed claude replays the transcript locally — marker one
    # reappears; that the replay made no model call is pinned by the
    # journal assert (resume-turn-1 matched exactly once).
    step_wait_pane "RESUME-MARKER-ONE" 60

    step_sleep 1
    step_type "Now the second resume turn."
    step_sleep 1
    step_key Enter
    # Safe after the restart: it cleared hook_state, so turn 1's terminal
    # 'done' cannot satisfy this wait.
    step_wait_state 'working|done' 30
    step_wait_pane "RESUME-MARKER-TWO" 60
    step_wait_state 'done' 60
    step_sleep 2
}

scenario_assert_effects() {
    local got
    got="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.agent_session_id')"
    [ "$got" = "$E2E_AGENT_CONVO_ID" ] \
        || e2e_die "agent_session_id changed across restart: '$got' != '$E2E_AGENT_CONVO_ID'" \
        || return 1
    # The sharp assert: the resume replay is local — turn 1's fixture matched
    # exactly once, i.e. the restart never re-sent the conversation.
    [ "$(journal_matched resume-turn-1)" -eq 1 ] \
        || e2e_die "resume-turn-1 matched $(journal_matched resume-turn-1) time(s), want exactly 1 (replay must not call the model)" \
        || return 1
    [ "$(journal_matched resume-turn-2)" -ge 1 ] \
        || e2e_die "resume-turn-2 fixture never matched (turn 2 never reached the model)"
}

scenario_assert_ui() {
    assert_pane_contains "RESUME-MARKER-TWO"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
