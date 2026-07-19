# shellcheck shell=bash
#
# Scenario: the attention surface for blocked sessions — what friring DOES
# with a blocked row, decoupled from how an agent gets blocked (the real
# permission-dialog edge is claude-blocked-permission; here the state is
# forced headlessly via `friring-cli session signal`, which is exactly what
# any agent's hook does). Two extra sessions flip to blocked; the sidebar
# must paint the ◆N title badge and the "◆ N blocked · F10" footer badge,
# F10 must walk the blocked queue in order, and the Alt+A overlay must jump
# to the Nth blocked session by digit. Unblocking one session drops the
# badge count and F10 skips straight to the only remaining blocked row.
# No desktop banner fires through all of this because the harness settings
# keep the notifications feature off — asserted via `friring-cli notify`.
#
# Test-mode only (signals/probes via friring-cli mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Forced blocked sessions: ◆ badges, F10 queue walk, Alt+A digit jump, notify gating"
SCENARIO_AGENT="scripted"

scenario_steps() {
    # Session A (the precreated $E2E_SCENARIO_NAME) is adopted and ready. It
    # never signals, so it stays Idle throughout — the badges below count
    # only the two sessions we flip.
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # The blocked queue members, created headlessly on the same repo so the
    # rendered order (A, att-b, att-c) is the creation order. 3>&- guards the
    # tmux-window-spawning create (bats fd-3 hang).
    local out
    out="$(friring-cli --json session create --name att-b \
        --repo-path "$E2E_WS" --agent scripted 3>&-)" \
        || e2e_die "session create att-b failed: $out" || return 1
    E2E_ATT_B_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_ATT_B_ID" ] && [ "$E2E_ATT_B_ID" != "null" ] \
        || e2e_die "no id for att-b in: $out" || return 1
    out="$(friring-cli --json session create --name att-c \
        --repo-path "$E2E_WS" --agent scripted 3>&-)" \
        || e2e_die "session create att-c failed: $out" || return 1
    E2E_ATT_C_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_ATT_C_ID" ] && [ "$E2E_ATT_C_ID" != "null" ] \
        || e2e_die "no id for att-c in: $out" || return 1
    step_wait_pane "att-b" 15
    step_wait_pane "att-c" 15

    # Force blocked the way a status hook would: `session signal` writes the
    # raw hook_state row; the TUI's ~100ms poll derives Blocked from it.
    friring-cli session signal --state blocked --session "$E2E_ATT_B_ID" >/dev/null \
        || e2e_die "signal blocked att-b failed" || return 1
    friring-cli session signal --state blocked --session "$E2E_ATT_C_ID" >/dev/null \
        || e2e_die "signal blocked att-c failed" || return 1

    # Persistent attention badges (toasts expire after 5s; these don't):
    # sidebar title carries ◆2, the footer spells it out with the F10 hint,
    # and each blocked row shows the word Blocked as its agent-status line.
    step_wait_pane "◆2" 15
    step_wait_pane "2 blocked" 15
    step_wait_pane "Blocked" 15

    # F10 walks the blocked queue in rendered order: A (Idle, active) is
    # skipped, first stop att-b, second stop att-c. The terminal pane title
    # is the proof the jump switched the ACTIVE session, not just selection.
    step_key F10
    step_wait_pane " att-b (scripted) \[Blocked\]" 15
    step_key F10
    step_wait_pane " att-c (scripted) \[Blocked\]" 15

    # Alt+A digit jump: the overlay sticks (legacy-terminal chord), then a
    # digit addresses the Nth *blocked* session — 1 = att-b, even though
    # att-b is row 2 of the sidebar (Alt+A indexes the blocked queue, not
    # the session list).
    step_key M-a
    step_type "1"
    step_wait_pane " att-b (scripted) \[Blocked\]" 15

    # Unblock att-b the same headless way (a Stop hook would signal done):
    # the badge count must drop, and F10 from the now-done att-b must land
    # on att-c — the only session still blocked.
    friring-cli session signal --state "done" --session "$E2E_ATT_B_ID" >/dev/null \
        || e2e_die "signal done att-b failed" || return 1
    step_wait_pane "◆1" 15
    step_key F10
    step_wait_pane " att-c (scripted) \[Blocked\]" 15
}

scenario_assert_effects() {
    # Why no banner ever fired: the harness settings keep the notifications
    # feature off, and the CLI probe reports that gate authoritatively.
    local enabled
    enabled="$(friring-cli --json notify | jq -r '.feature_enabled')"
    [ "$enabled" = "false" ] \
        || e2e_die "notify feature_enabled '$enabled' != false (sandbox must gate banners)" \
        || return 1
    # The raw persisted hook states (not the TUI's derived status): att-b's
    # unblock stuck, att-c is still waiting for attention.
    local b c
    b="$(friring-cli --json session get "$E2E_ATT_B_ID" | jq -r '.hook_state')"
    [ "$b" = "done" ] || e2e_die "att-b hook_state '$b' != done" || return 1
    c="$(friring-cli --json session get "$E2E_ATT_C_ID" | jq -r '.hook_state')"
    [ "$c" = "blocked" ] || e2e_die "att-c hook_state '$c' != blocked"
}

scenario_assert_ui() {
    # End state: one blocked session left, and it is the active one.
    assert_pane_contains "◆1"
    assert_pane_contains " att-c (scripted) [Blocked]"
}
