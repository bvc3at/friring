# shellcheck shell=bash
#
# Scenario: the full ghost-session lifecycle on the scripted agent — the
# pure-UI proof that runs on any machine. Unload (`Alt+U`) must save the
# frame, kill the agent window in the friring-dev server, and leave a greyed
# ghost whose pane still shows the pre-unload content and swallows keystrokes
# (the `GOT:` echo is the live-vs-frozen oracle). Enter then loads the ghost
# through the resume template — the scripted binary prints `mode=resume`,
# which e2e-tests the registry expansion on the *load* path. A quit + agent
# window kill + TUI relaunch afterwards simulates a reboot: lazy restore
# (default on) must show the ghost from the shutdown-saved frame instead of
# respawning, and Enter must load it again.
#
# Test-mode only (drives the friring-dev tmux server mid-steps and relaunches
# the TUI; Alt+U has no VHS key); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Unload to a frozen ghost, load via resume, and lazy-restore the ghost after the agent window dies"
SCENARIO_AGENT="scripted"

# Bounded poll until the agent window is GONE from the friring-dev server —
# the memory claim itself (unload kills the agent process, not just the row).
ghost_wait_agent_window_gone() {
    for _ in $(seq 1 100); do
        tmux -L friring-dev list-windows -a 2>/dev/null \
            | grep -q "tb-$E2E_SCENARIO_NAME" || return 0
        sleep 0.1
    done
    e2e_die "agent window tb-$E2E_SCENARIO_NAME still alive after unload"
}

# Mirror of claude-adopt-restart's quit/relaunch helpers (same fd guard).
ghost_wait_driver_gone() {
    for _ in $(seq 1 100); do
        tmux -L "$E2E_DRIVER_SOCKET" has-session -t "$E2E_DRIVER_SESSION" \
            >/dev/null 2>&1 || return 0
        sleep 0.1
    done
    e2e_die "driver tmux session survived the TUI quit"
}

ghost_relaunch_tui() {
    tmux -L "$E2E_DRIVER_SOCKET" new-session -d -s "$E2E_DRIVER_SESSION" \
        -x "$SCENARIO_COLS" -y "$SCENARIO_ROWS" "$FRIRING_BIN" 3>&-
    e2e_wait_pane "friring" 100 || e2e_die "relaunched TUI did not boot"
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60
    step_type "probe-one"
    step_key Enter
    step_wait_pane "GOT:probe-one" 30

    # Unload from terminal focus: Alt+U is a Global chord and not
    # terminal-passthrough, so it dispatches without leaving the pane.
    step_key M-u
    step_wait_pane "unloaded — Enter loads" 30
    ghost_wait_agent_window_gone
    # The ghost renders the frozen frame — pre-unload content still visible.
    step_wait_pane "GOT:probe-one" 10

    # Keystrokes into a ghost are swallowed with a hint, never queued: after
    # the load below, no `GOT:zzz` may ever appear.
    step_type "zzz"
    step_wait_pane "press Enter to load" 15

    # Enter loads: the respawn goes through the agent's resume template, so
    # the fresh scripted instance prints mode=resume — registry expansion
    # proven on the load path.
    step_key Enter
    step_wait_pane "Session loaded" 30
    step_wait_pane "SCRIPTED-READY mode=resume" 60
    step_type "probe-two"
    step_key Enter
    step_wait_pane "GOT:probe-two" 30

    # Reboot simulation: quit (shutdown saves every live session's frame),
    # kill the agent window while no TUI runs, relaunch. Lazy restore must
    # ghost the session from the shutdown frame — not respawn it.
    step_key C-q
    ghost_wait_driver_gone
    tmux -L friring-dev kill-window -t "tb-$E2E_SCENARIO_NAME" 2>/dev/null \
        || e2e_die "could not kill the agent window for the reboot simulation"
    ghost_relaunch_tui
    step_wait_pane "unloaded — Enter loads" 30
    step_wait_pane "GOT:probe-two" 10
    ghost_wait_agent_window_gone

    # And the lazily-restored ghost loads the same way.
    step_key Enter
    step_wait_pane "SCRIPTED-READY mode=resume" 60
    step_type "probe-three"
    step_key Enter
    step_wait_pane "GOT:probe-three" 30
}

scenario_assert_effects() {
    # The load respawned a live agent window.
    tmux -L friring-dev list-windows -a 2>/dev/null \
        | grep -q "tb-$E2E_SCENARIO_NAME" \
        || e2e_die "agent window missing after the final load"
}

scenario_assert_ui() {
    assert_pane_contains "GOT:probe-three"
    # The ghost really dropped the keystrokes: had they been queued anywhere,
    # the loaded agent would have echoed them.
    if e2e_pane | grep -qF "GOT:zzz"; then
        e2e_die "keystrokes typed into the ghost reached an agent"
    fi
}
