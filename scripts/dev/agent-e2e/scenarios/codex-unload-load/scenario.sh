# shellcheck shell=bash
#
# Scenario: unload a real codex session to a ghost, then load it back. Codex
# can't pin a conversation id, so its registry entry resumes id-less
# (`codex resume --last`, cwd-scoped): the load must land in the SAME
# conversation because the respawn reuses the session cwd. The ghost pane
# keeps turn 1's reply on the frozen frame; after the load the resumed TUI
# renders it again from local history — turn 1's fixture must match exactly
# once — and a fresh second turn proves the conversation is live. No status
# hooks (AGENT_HAS_STATUS_HOOKS=0): pane-text sync only.
#
# No clip ships for it: the ghost lifecycle is filmed at fleet scale by
# claude-ghost-fleet, where four rows repricing at once are legible in a way
# one frozen pane is not.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Alt+U unloads codex to a frozen ghost; Enter loads it back via 'resume --last' in the same cwd"
SCENARIO_AGENT="codex"
# `<leader> U` is the fork's own second route to the same unload, and the one
# that films: an Alt chord is invisible on camera, while the leader paints a
# which-key overlay naming the action.
SCENARIO_DEMO_KEYS=("M-u=C-f U")
# The composer-line glyph — codex's stable ready marker (placeholder text
# rotates, the footer varies with cwd; verified against codex-cli 0.144.4).
SCENARIO_AGENT_READY="›"

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

    # Unload: the codex process dies, the ghost keeps the frame.
    step_key M-u
    step_wait_pane "unloaded — Enter loads" 30
    ghost_wait_agent_window_gone
    step_wait_pane "GHOST-MARKER-ONE" 10

    # Enter loads via `resume --last` in the session's cwd; codex re-renders
    # the conversation from local history (the journal pins turn 1 to one
    # match — the load itself makes no model call).
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
