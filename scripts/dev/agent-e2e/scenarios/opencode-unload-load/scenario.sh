# shellcheck shell=bash
#
# Scenario: unload a real opencode session to a ghost, then load it back.
# opencode resumes id-less (`opencode --continue`, cwd-scoped), so the load
# must land in the SAME session because the respawn reuses the session cwd.
# The ghost pane keeps turn 1's reply on the frozen frame; the resumed TUI
# renders it again from local history — turn 1's fixture must match exactly
# once — and a fresh second turn proves the session is live. One ambient
# call (title generation on the session's first message) is answered via its
# "title generator" system prompt, listed first per the ambient-first
# convention. No status hooks: pane-text sync only.
#
# Test-mode only (drives the friring-dev tmux server mid-steps; Alt+U has no
# VHS key); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Alt+U unloads opencode to a frozen ghost; Enter loads it back via --continue in the same cwd"
SCENARIO_AGENT="opencode"
# VHS cannot press Alt (it emits the bare capital); `<leader> U` is the
# fork's own second route to the same unload.
SCENARIO_DEMO_KEYS=("M-u=C-f U")
# The input-box footer renders "Build · <model> <provider>" once the TUI is
# interactive — the stable ready marker (verified against opencode 1.17.15).
SCENARIO_AGENT_READY="Build ·"

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
    step_type "Say the first ghost phrase now."
    # Sync on the composer echo before Enter (step_sleep is a no-op in test
    # mode, so Enter would otherwise race the composer).
    step_wait_pane "first ghost phrase" 30
    step_key Enter
    step_wait_pane "GHOST-MARKER-ONE" 60

    # Unload: the opencode process dies, the ghost keeps the frame.
    step_key M-u
    step_wait_pane "unloaded — Enter loads" 30
    ghost_wait_agent_window_gone
    step_wait_pane "GHOST-MARKER-ONE" 10

    # Enter loads via `--continue` in the session's cwd; opencode re-renders
    # the session from local history (the journal pins turn 1 to one match —
    # the load itself makes no model call).
    step_key Enter
    step_wait_pane "Session loaded" 30
    step_wait_pane "$SCENARIO_AGENT_READY" 120
    step_wait_pane "GHOST-MARKER-ONE" 60

    step_sleep 1
    step_type "Say the second ghost phrase now."
    step_wait_pane "second ghost phrase" 30
    step_key Enter
    step_wait_pane "GHOST-MARKER-TWO" 60
    step_sleep 2
}

scenario_assert_effects() {
    [ "$(journal_matched ghost-turn-1)" -eq 1 ] \
        || e2e_die "ghost-turn-1 matched $(journal_matched ghost-turn-1) time(s), want exactly 1 (load must not call the model)" \
        || return 1
    [ "$(journal_matched ghost-turn-2)" -ge 1 ] \
        || e2e_die "ghost-turn-2 fixture never matched (turn 2 never reached the model)"
}

scenario_assert_ui() {
    assert_pane_contains "GHOST-MARKER-TWO"
}
