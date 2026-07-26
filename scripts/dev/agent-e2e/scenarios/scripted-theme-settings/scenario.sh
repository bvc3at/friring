# shellcheck shell=bash
#
# Scenario: the config surfaces around a live session, with no model in the
# loop. One flow proves four contracts in order: the Ctrl+Y theme picker
# persists its pick (header badge + `config show .theme`); an on-disk
# settings.toml edit live-reloads within the ~1s mtime poll and its
# [features] flags really gate chords (Ctrl+T refuses with a toast while
# shell_pane = false); a *broken* settings.toml degrades to an ERROR toast
# while the TUI keeps routing keys to the agent PTY (never-crash); and the
# The <leader> m perf HUD opens/closes as a pure overlay. The scripted agent keeps it
# hermetic — every settings.toml written here keeps notifications = false so
# a test can never fire a real desktop banner.
#
# Test-mode only (writes config files mid-steps, uses the leader); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Theme pick persists; settings.toml live-reload gates Ctrl+T; invalid TOML toasts, never crashes; <leader> m perf HUD toggles"
SCENARIO_AGENT="scripted"

# The exact file the scenario leaves behind — steps write it, the effects
# assert re-reads it, so the two can never drift apart.
SCENARIO_RESTORED_SETTINGS='[features]
notifications = false'

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Theme picker. "Theme" alone is also a footer pill, so sync on a row
    # label instead; picker order puts Catppuccin Mocha second, and the
    # selection opens on the active theme (unset -> Default, row one).
    step_key C-y
    step_wait_pane "Catppuccin Mocha" 15
    step_key Down
    step_key Enter
    # The ◐ prefix pins the match to the header badge, not a picker row.
    step_wait_pane "◐ Catppuccin Mocha" 15

    # Live-reload: flip shell_pane off on disk (notifications stays false —
    # hermeticity), then prove the flag actually gates the chord: Ctrl+T must
    # toast instead of opening a shell.
    printf '[features]\nnotifications = false\nshell_pane = false\n' \
        > "$XDG_CONFIG_HOME/friring-dev/settings.toml"
    step_wait_pane "settings.toml reloaded" 15
    step_key C-t
    step_wait_pane "Shell pane is disabled" 15

    # Never-crash: a parse error degrades to defaults + an ERROR toast
    # ("Config: settings.toml: …; using defaults"), and the TUI still routes
    # typing to the agent PTY afterwards.
    printf '[features\n' > "$XDG_CONFIG_HOME/friring-dev/settings.toml"
    step_wait_pane " ERROR " 15
    step_wait_pane "using defaults" 15
    step_type "still-alive"
    step_key Enter
    step_wait_pane "GOT:still-alive" 15

    # Restore a valid file: the earlier reload toast was overwritten by the
    # Ctrl+T and ERROR toasts, so this wait can only match the new reload.
    # Ctrl+T is un-gated again: shell pane opens (title flips to "(shell)"),
    # then toggle back to the agent view.
    printf '%s\n' "$SCENARIO_RESTORED_SETTINGS" \
        > "$XDG_CONFIG_HOME/friring-dev/settings.toml"
    step_wait_pane "settings.toml reloaded" 15
    step_key C-t
    step_wait_pane "(shell)" 15
    step_key C-t
    step_wait_pane "(scripted)" 15

    # Perf HUD overlay on top of the agent view; closing is asserted (with a
    # bounded poll) in scenario_assert_ui — nothing new appears to wait on.
    # `F12` is the second leader now, so the HUD is `<leader> m`.
    step_key C-f
    step_key m
    step_wait_pane "idle skips" 15
    step_key C-f
    step_key m
}

scenario_assert_effects() {
    # The pick landed in the DB, not just the frame: one Down from Default is
    # Catppuccin Mocha, persisted under its slug.
    local theme
    theme="$(friring-cli --json config show | jq -r '.theme')"
    [ "$theme" = "catppuccin-mocha" ] \
        || e2e_die "persisted theme '$theme' != 'catppuccin-mocha'" || return 1
    # The file on disk is the restored valid one — the error step didn't eat
    # or rewrite it.
    local got
    got="$(cat "$XDG_CONFIG_HOME/friring-dev/settings.toml")"
    [ "$got" = "$SCENARIO_RESTORED_SETTINGS" ] \
        || e2e_die "settings.toml on disk is not the restored file: '$got'"
}

scenario_assert_ui() {
    # The HUD close is one repaint away from the final <leader> m; poll bounded
    # instead of asserting a single racy frame.
    for _ in $(seq 1 30); do
        e2e_pane | grep -qF "idle skips" || break
        sleep 0.1
    done
    ! e2e_pane | grep -qF "idle skips" \
        || e2e_die "perf HUD counters still rendered after the second <leader> m" || return 1
    ! e2e_pane | grep -qF " Perf " \
        || e2e_die "perf HUD box still rendered after the second <leader> m" || return 1
    # Back on the agent view (not the shell), with the picked theme badge up.
    assert_pane_contains "(scripted)"
    assert_pane_contains "◐ Catppuccin Mocha"
}
