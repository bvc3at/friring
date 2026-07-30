# shellcheck shell=bash
#
# Scenario: `lazy_session_restore = false` still respawns. Lazy restore is the
# default, so the opt-out is the path that silently rots — and it cannot be
# unit-tested, because settings are published once per process through a
# OnceLock that a test cannot re-set. Here it is real: write the setting, kill
# the agent window with no TUI running (a reboot, as far as friring can tell),
# relaunch, and require the session to come back **running** rather than as a
# ghost.
#
# The scripted agent makes the distinction unambiguous: an eager restore goes
# through the resume template, so the respawned process prints
# `SCRIPTED-READY mode=resume` with no key pressed, and no ghost hint is ever
# painted.
#
# Test-only (rewrites settings.toml and relaunches the TUI mid-steps); not
# demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="lazy_session_restore = false respawns a pane-less session at startup instead of ghosting it"
SCENARIO_AGENT="scripted"

eager_wait_driver_gone() {
    for _ in $(seq 1 100); do
        tmux -L "$E2E_DRIVER_SOCKET" has-session -t "$E2E_DRIVER_SESSION" \
            >/dev/null 2>&1 || return 0
        sleep 0.1
    done
    e2e_die "driver tmux session survived the TUI quit"
}

eager_relaunch_tui() {
    tmux -L "$E2E_DRIVER_SOCKET" new-session -d -s "$E2E_DRIVER_SESSION" \
        -x "$SCENARIO_COLS" -y "$SCENARIO_ROWS" "$FRIRING_BIN" 3>&-
    e2e_wait_pane "friring" 100 || e2e_die "relaunched TUI did not boot"
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60
    step_type "probe-eager"
    step_key Enter
    step_wait_pane "GOT:probe-eager" 30

    # Opt out of lazy restore. Restart-only (read once at startup), so it has
    # to be on disk before the relaunch below. Notifications stay off — the
    # harness's hermeticity rule for any scenario that rewrites this file.
    step_key C-q
    eager_wait_driver_gone
    mkdir -p "$XDG_CONFIG_HOME/friring-dev"
    cat > "$XDG_CONFIG_HOME/friring-dev/settings.toml" <<'EOF'
lazy_session_restore = false

[features]
notifications = false
EOF

    # No TUI is running: kill the agent window, so the session has no live pane
    # to adopt on the next boot — the reboot case, where lazy restore would
    # normally paint a ghost.
    tmux -L friring-dev kill-window -t "tb-$E2E_SCENARIO_NAME" 2>/dev/null \
        || e2e_die "could not kill the agent window"

    eager_relaunch_tui
    # The payoff: a *running* agent, launched through the resume template with
    # no keystroke — not a ghost waiting to be loaded.
    step_wait_pane "SCRIPTED-READY mode=resume" 60
    # And it is live, not a frozen frame.
    step_type "probe-after-respawn"
    step_key Enter
    step_wait_pane "GOT:probe-after-respawn" 30
}

scenario_assert_effects() {
    tmux -L friring-dev list-windows -a 2>/dev/null \
        | grep -q "tb-$E2E_SCENARIO_NAME" \
        || e2e_die "the eagerly restored session has no agent window"
    grep -q 'lazy_session_restore = false' \
        "$XDG_CONFIG_HOME/friring-dev/settings.toml" \
        || e2e_die "the opt-out was not the setting under test"
}

scenario_assert_ui() {
    assert_pane_contains "GOT:probe-after-respawn"
    # The ghost path must not have run at all.
    if e2e_pane | grep -qF "unloaded — Enter loads"; then
        e2e_die "session was ghosted despite lazy_session_restore = false"
    fi
}
