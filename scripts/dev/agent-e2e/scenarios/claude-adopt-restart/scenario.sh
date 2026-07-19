# shellcheck shell=bash
#
# Scenario: tmux-backed persistence, end to end. Sessions live in the
# friring-dev tmux server, not in the TUI process — quitting the TUI (Ctrl+Q)
# merely detaches, and a relaunched TUI re-adopts the still-live pane. The
# steps run a first stubbed turn, kill the TUI, prove the agent window
# survived in the friring-dev server while the driver session died, relaunch
# the TUI exactly the way the harness boots it, and assert the re-adopted
# pane shows the first turn's content intact. A second turn then proves the
# adopted pane is live end to end: keystrokes reach the same agent process,
# and its status hooks (env frozen into the pane at spawn) still attribute
# working->done to the session.
#
# Test-mode only (drives tmux servers directly mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Quit + relaunch the TUI; the live claude pane is re-adopted with content intact"
SCENARIO_AGENT="claude"
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_MARKER_ONE="ADOPT-MARKER-ONE"
SCENARIO_MARKER_TWO="ADOPT-MARKER-TWO"

# Bounded poll until the driver tmux session is gone: Ctrl+Q tears the TUI
# down asynchronously, and the pane's death takes the driver session (its
# only client of work) with it. A DSL step can't express "wait for absence",
# so the loop lives here.
adopt_wait_driver_gone() {
    for _ in $(seq 1 100); do
        tmux -L "$E2E_DRIVER_SOCKET" has-session -t "$E2E_DRIVER_SESSION" \
            >/dev/null 2>&1 || return 0
        sleep 0.1
    done
    e2e_die "driver tmux session survived the TUI quit"
}

# The persistence claim itself: with no TUI running anywhere, the agent
# window (tb-<session name>, friring-dev server) must still be alive.
adopt_assert_agent_window_alive() {
    tmux -L friring-dev list-windows -a 2>/dev/null \
        | grep -q "tb-$E2E_SCENARIO_NAME" \
        || e2e_die "agent window tb-$E2E_SCENARIO_NAME did not survive the TUI quit"
}

# Relaunch mirrors the harness boot verbatim (same driver socket/session,
# same 3>&- fd guard) so the second TUI differs from the first in nothing
# but its start time — adoption, not respawn, explains what it renders.
adopt_relaunch_tui() {
    tmux -L "$E2E_DRIVER_SOCKET" new-session -d -s "$E2E_DRIVER_SESSION" \
        -x "$SCENARIO_COLS" -y "$SCENARIO_ROWS" "$FRIRING_BIN" 3>&-
    e2e_wait_pane "friring" 100 || e2e_die "relaunched TUI did not boot"
}

scenario_steps() {
    # First turn, like any text-turn scenario: its marker is what the
    # relaunched TUI must still render.
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_type "Say the first adoption turn phrase."
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_state 'done' 60
    step_wait_pane "$SCENARIO_MARKER_ONE" 60

    # Quit the TUI; the driver session dies, the agent pane must not.
    step_key C-q
    adopt_wait_driver_gone
    adopt_assert_agent_window_alive

    # Relaunch and observe re-adoption: session row back in the sidebar,
    # first turn's scrollback intact in the rendered pane.
    adopt_relaunch_tui
    step_wait_pane "$E2E_SCENARIO_NAME" 30
    step_wait_pane "$SCENARIO_MARKER_ONE" 30

    # Adopted sessions boot with Terminal focus, so plain typing lands in
    # the (still-live) agent PTY; its hook env was frozen at spawn, so the
    # second turn's signals still attribute to this session.
    step_wait_pane "$SCENARIO_AGENT_READY" 30
    step_type "Say the second adoption turn phrase."
    step_key Enter
    step_wait_pane "$SCENARIO_MARKER_TWO" 60
    step_wait_state 'done' 30
}

scenario_assert_effects() {
    [ "$(journal_matched first-adoption)" -ge 1 ] \
        || e2e_die "first-adoption fixture never matched" || return 1
    [ "$(journal_matched second-adoption)" -ge 1 ] \
        || e2e_die "second-adoption fixture never matched (pane not live after re-adopt)"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_MARKER_TWO"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
