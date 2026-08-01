# shellcheck shell=bash
#
# Scenario: unload a real claude session to a ghost, then load it back.
# Unload (`Alt+U`) kills the claude process (the agent window leaves the
# friring-dev server) but the ghost pane keeps showing the turn's reply from
# the saved frame. Enter loads the ghost through `claude --resume {id}`: the
# transcript replays locally — turn 1's fixture must match exactly once, the
# persisted agent_session_id is unchanged, and a fresh second turn proves the
# resumed conversation is live.
#
# Demo-able: Alt+U records through `<leader> U` (SCENARIO_DEMO_KEYS). The
# mid-step friring-dev/friring-cli probes run at tape-generation time, where
# they only read — the ghost-window poll times out harmlessly before the
# recording starts.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Alt+U unloads claude to a frozen ghost; Enter loads it back via --resume with zero model calls"
SCENARIO_AGENT="claude"
# VHS cannot press Alt (it emits the bare capital); `<leader> U` is the
# fork's own second route to the same unload.
SCENARIO_DEMO_KEYS=("M-u=C-f U")
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"

ghost_wait_agent_window_gone() {
    for _ in $(seq 1 100); do
        tmux -L friring-dev list-windows -a 2>/dev/null \
            | grep -q "tb-$E2E_SCENARIO_NAME" || return 0
        sleep 0.1
    done
    e2e_die "agent window tb-$E2E_SCENARIO_NAME still alive after unload"
}

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_sleep 1
    step_type "This is the first ghost turn."
    step_sleep 1
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "GHOST-MARKER-ONE" 60
    step_wait_state 'done' 60

    # Captured BEFORE the unload so assert_effects can prove the load reused
    # the same conversation.
    E2E_AGENT_CONVO_ID="$(friring-cli --json session get "$E2E_SESSION_ID" \
        | jq -r '.agent_session_id')"
    [ -n "$E2E_AGENT_CONVO_ID" ] && [ "$E2E_AGENT_CONVO_ID" != "null" ] \
        || e2e_die "session has no agent_session_id" || return 1

    # Unload: the claude process dies, the ghost keeps the frame.
    step_key M-u
    step_wait_pane "unloaded — Enter loads" 30
    ghost_wait_agent_window_gone
    step_wait_pane "GHOST-MARKER-ONE" 10

    # Enter loads via the resume template; the replay is local (the journal
    # pins turn 1 to exactly one match).
    step_key Enter
    step_wait_pane "Session loaded" 30
    step_wait_pane "$SCENARIO_AGENT_READY" 120
    step_wait_pane "GHOST-MARKER-ONE" 60

    step_sleep 1
    step_type "Now the second ghost turn."
    step_sleep 1
    step_key Enter
    # Safe after the load: it cleared hook_state, so turn 1's terminal
    # 'done' cannot satisfy this wait.
    step_wait_state 'working|done' 30
    step_wait_pane "GHOST-MARKER-TWO" 60
    step_wait_state 'done' 60
    step_sleep 2
}

scenario_assert_effects() {
    local got
    got="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.agent_session_id')"
    [ "$got" = "$E2E_AGENT_CONVO_ID" ] \
        || e2e_die "agent_session_id changed across unload/load: '$got' != '$E2E_AGENT_CONVO_ID'" \
        || return 1
    # The sharp assert: loading a ghost replays locally — the unload/load
    # cycle never re-sent the conversation to the model.
    [ "$(journal_matched ghost-turn-1)" -eq 1 ] \
        || e2e_die "ghost-turn-1 matched $(journal_matched ghost-turn-1) time(s), want exactly 1 (load must not call the model)" \
        || return 1
    [ "$(journal_matched ghost-turn-2)" -ge 1 ] \
        || e2e_die "ghost-turn-2 fixture never matched (turn 2 never reached the model)"
}

scenario_assert_ui() {
    assert_pane_contains "GHOST-MARKER-TWO"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
