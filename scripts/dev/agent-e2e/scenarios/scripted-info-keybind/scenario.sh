# shellcheck shell=bash
#
# Scenario: fully rebindable keys + the info panel, end to end. A seeded
# keybindings.json (`ToggleInfoPanel: ["f11"]`) must take effect at boot —
# overrides REPLACE the action's chords, so the default F2 must open
# nothing while F11 opens the " Info " panel. The F1 editor then proves the
# interactive path: the chord column renders the seeded `f11`, `r` on the
# first row enters capture mode, and a captured Ctrl+A becomes that row's
# sole binding, persisted to keybindings.json immediately (poll-read with
# jq) with the seeded override still intact. Which action sits in the
# first row is deliberately NOT hardcoded: the asserts discover the
# ctrl+a owner from the file and report it.
#
# Test-mode only (reads keybindings.json mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Seeded keybindings.json override active at boot; F1 editor rebind persists to disk"
SCENARIO_AGENT="scripted"
# The F1 body keeps the *selected* row visible and selection starts at row
# 0, so at the default 40 rows the UI section (where the info-panel row
# lives) is scrolled off-screen. 50 rows fit it without moving selection.
SCENARIO_ROWS=50

scenario_setup() {
    mkdir -p "$XDG_CONFIG_HOME/friring-dev"
    printf '{"ToggleInfoPanel": ["f11"]}\n' \
        > "$XDG_CONFIG_HOME/friring-dev/keybindings.json"
}

# Bounded poll for a pane string being GONE — step_wait_pane waits only for
# presence, and both panel closes here are proven by a title disappearing.
kb_wait_pane_gone() {
    local pattern="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        e2e_pane | grep -qF -- "$pattern" || return 0
        sleep 0.2
    done
    e2e_die "pane still shows: $pattern
--- pane ---
$(e2e_pane)"
}

# Poll keybindings.json until exactly one action carries the captured
# chord. The TUI writes the file synchronously on capture; the poll only
# absorbs render/IO latency.
kb_wait_file_chord() {
    local chord="$1" tries="$2" f="$XDG_CONFIG_HOME/friring-dev/keybindings.json"
    for _ in $(seq 1 "$tries"); do
        if jq -e --arg c "$chord" \
            '[to_entries[] | select(.value | index($c))] | length == 1' \
            "$f" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.2
    done
    e2e_die "keybindings.json never gained chord '$chord':
$(cat "$f" 2>/dev/null)"
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # F2 lost the info-panel binding to the seeded override. An unbound key
    # in Terminal focus forwards to the PTY, so Ctrl+U (passthrough) kills
    # any buffered escape bytes to keep later GOT: lines clean; the probe
    # round-trip then proves the TUI processed the F2 press (events are
    # handled in order) before the absence check.
    step_key F2
    step_key C-u
    step_type "kb-probe-after-f2"
    step_key Enter
    step_wait_pane "GOT:kb-probe-after-f2" 15
    # " Info ─" is the panel's border title; a bare " Info " would false-match
    # the always-on status bar hint ("Info · F11" — which itself tracks the
    # override, so the hint shows the seeded chord, not F2).
    ! e2e_pane | grep -qF -- " Info ─" \
        || e2e_die "F2 still opens the info panel despite the override" || return 1

    # The override's chord works: F11 opens the panel with its labels.
    step_key F11
    step_wait_pane " Info ─" 15
    step_wait_pane "Name: " 15
    step_wait_pane "Agent: " 15

    # …and toggles it closed again, keeping the editor asserts unambiguous.
    step_key F11
    kb_wait_pane_gone " Info ─" 50

    # F1 editor: the info-panel chord column must show the seeded f11 and
    # not the compiled-in defaults it replaced.
    step_key F1
    step_wait_pane "Keybindings" 15
    step_wait_pane "f11" 15
    ! e2e_pane | grep -qF -- "ctrl+b" \
        || e2e_die "editor still shows the replaced default info-panel chord" || return 1

    # Interactive rebind of the first (selected-by-default) row, whatever
    # action it is: r -> capture -> Ctrl+A. ctrl+a is free by default (the
    # rebind steals from no one) and nothing later in the scenario needs it.
    step_key r
    step_wait_pane "Press the new shortcut" 15
    step_key C-a
    step_wait_pane "ctrl+a" 15
    kb_wait_file_chord "ctrl+a" 50

    step_key Escape
    kb_wait_pane_gone "Keybindings" 50

    # Editor closed back to Terminal focus: the session is still drivable.
    step_type "kb-final-alive"
    step_key Enter
    step_wait_pane "GOT:kb-final-alive" 15
}

scenario_assert_effects() {
    local f="$XDG_CONFIG_HOME/friring-dev/keybindings.json"
    # The seeded override survived the editor's full-map rewrite.
    jq -e '.ToggleInfoPanel == ["f11"]' "$f" >/dev/null \
        || e2e_die "ToggleInfoPanel lost the seeded f11 override:
$(cat "$f")" || return 1
    # Exactly one action owns the captured chord…
    local owners owner
    owners="$(jq -r '[to_entries[] | select(.value | index("ctrl+a")) | .key] | length' "$f")"
    [ "$owners" = "1" ] \
        || e2e_die "expected exactly one ctrl+a owner, got $owners" || return 1
    owner="$(jq -r 'to_entries[] | select(.value | index("ctrl+a")) | .key' "$f")"
    e2e_log "first F1 row (ctrl+a owner): $owner"
    # …and a rebind replaces ALL previous chords, so ctrl+a is its sole one.
    jq -e --arg a "$owner" '.[$a] == ["ctrl+a"]' "$f" >/dev/null \
        || e2e_die "rebound action '$owner' kept chords beyond ctrl+a:
$(cat "$f")" || return 1
}

scenario_assert_ui() {
    ! e2e_pane | grep -qF -- "Keybindings" \
        || e2e_die "F1 editor still open after Esc" || return 1
    assert_pane_contains "GOT:kb-final-alive"
}
