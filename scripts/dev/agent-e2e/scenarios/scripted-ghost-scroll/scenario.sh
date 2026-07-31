# shellcheck shell=bash
#
# Scenario: a ghost's scrollback is reachable from the keyboard. The frame is
# captured with `ghost_scrollback_lines` of history precisely so an unloaded
# session stays readable — if the rows above its visible screen can't be
# scrolled to, that history is dead weight.
#
# Runs the same gesture twice against the same session, live and then frozen,
# so a failure localizes itself: the live half is the control (scrolling a
# normal pane), the ghost half is the claim under test. Both scroll with the
# terminal explicitly focused.
#
# Test-only (drives the friring-dev tmux server and uses Shift+Up); not
# demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="A ghost's saved scrollback is reachable with the scroll keys, like a live pane's"
SCENARIO_AGENT="scripted"

ghost_wait_agent_window_gone() {
    for _ in $(seq 1 100); do
        tmux -L friring-dev list-windows -a 2>/dev/null \
            | grep -q "tb-$E2E_SCENARIO_NAME" || return 0
        sleep 0.1
    done
    e2e_die "agent window tb-$E2E_SCENARIO_NAME still alive after unload"
}

# Bounded poll for a string that must appear only after scrolling.
ghost_scroll_until() {
    local want="$1" tries="${2:-20}" i
    for ((i = 0; i < tries; i++)); do
        tmux -L "$E2E_DRIVER_SOCKET" send-keys -t "$E2E_DRIVER_SESSION" S-PPage
        sleep 0.2
        e2e_pane | grep -qF "$want" && return 0
    done
    echo "--- pane after $tries scroll presses ---" >&2
    e2e_pane >&2
    return 1
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Fill the pane and push the early rows off the top.
    step_type "lines:120"
    step_key Enter
    step_wait_pane "SCROLLLINE-120" 60

    # Control: the same gesture on a LIVE pane, terminal focused. If this half
    # fails the scenario is wrong, not the ghost.
    ghost_scroll_until "SCROLLLINE-001" 20 \
        || e2e_die "control failed: could not scroll a LIVE pane back to its first line"
    # Back to the bottom so the unload captures the tail.
    tmux -L "$E2E_DRIVER_SOCKET" send-keys -t "$E2E_DRIVER_SESSION" S-Down
    sleep 0.5

    # Freeze it. The frame carries ghost_scrollback_lines (default 1000) of
    # history, so every SCROLLLINE row is inside it.
    step_key M-u
    step_wait_pane "unloaded — Enter loads" 30
    ghost_wait_agent_window_gone

    # Unload hands focus to the session list, so step back into the pane —
    # the ghost half must be tested with the terminal focused.
    step_key C-l
    ghost_scroll_until "SCROLLLINE-001" 20 \
        || e2e_die "a ghost's saved scrollback is not reachable with the scroll keys"
}

scenario_assert_ui() {
    assert_pane_contains "SCROLLLINE-001"
}
