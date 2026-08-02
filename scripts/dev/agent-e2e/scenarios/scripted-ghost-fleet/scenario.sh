# shellcheck shell=bash
#
# Scenario: the ghost lifecycle at FLEET scale — the readable version of
# scripted-unload-ghost, which proves the mechanism on one session and is
# consequently impossible to read as a recording (one frozen frame looks
# exactly like one idle frame).
#
# Five scripted sessions, each with output of its own. Three are unloaded one
# after another. What that shows is the thing the feature is *for*: a sidebar
# where most rows are frozen (`◌`), each pane still carrying its own last
# output, and `<leader> c` walking only the sessions that still have a process
# behind them. The last ghost is loaded back through the resume template.
#
# Driven entirely through the leader (`<leader> c`, `<leader> U`) rather than
# the Alt chords, because that route is identical in both modes — VHS cannot
# press Alt at all (docs/E2E.md § Demo mode).
#
# Every loaded scripted session paints the same SCRIPTED-READY line, so it
# cannot serve as a "the switch landed" sync — the pace beat after each
# cycle is what separates the switch from the typing that follows it.
#
# Navigation is by *cycling*, never by `<leader> <n>`: the rendered order is
# not creation order and it shifts as sessions unload, so a numbered jump
# taken before an unload does not address the same row after one. Cycling is
# order-independent by construction, and it is the gesture being demonstrated.
#
# The memory this saves is the other half of the story and is deliberately NOT
# claimed on screen: friring does not surface per-session RSS yet, and a host
# total cannot separate one agent from the rest of the machine.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Ghost fleet: unload 3 of 5 sessions, cycle past them loaded-only, load one back"
SCENARIO_AGENT="scripted"

# How many of the five sessions still have an agent window on the friring-dev
# server. Assert-time only — a mid-step poll would run at tape-generation time
# in demo mode, long before the unloads it is meant to observe.
fleet_live_windows() {
    tmux -L friring-dev list-windows -a 2>/dev/null \
        | grep -cE "tb-(${E2E_SCENARIO_NAME}|ring-0[2-5])" || true
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Four more sessions in the same repo, so the sidebar is a fleet rather
    # than a pair. In demo mode this lands before the first frame, which is
    # exactly right — the clip should open on a populated sidebar.
    local n
    for n in 2 3 4 5; do
        friring-cli session create --name "ring-0$n" --repo-path "$E2E_WS" \
            --agent scripted >/dev/null 3>&- \
            || e2e_die "session create ring-0$n failed" || return 1
    done
    step_wait_pane "ring-05" 30

    # Round 1: step to the next LOADED session, give it output of its own, and
    # freeze it. Repeating this three times leaves three ghosts, and because
    # `<leader> c` skips what is already frozen, it never lands twice on the
    # same one.
    step_key C-f
    step_key c
    step_wait_pane "SCRIPTED-READY" 30
    step_sleep 1
    step_type "ring holding 21.0C"
    step_key Enter
    step_wait_pane "GOT:ring holding" 20
    step_key C-f
    step_key U
    step_wait_pane "unloaded — Enter loads" 30
    # The frozen frame still carries that session's own last output.
    step_wait_pane "GOT:ring holding" 15
    # Unload hands focus to the session list (a ghost has no PTY worth pointing
    # a keyboard at), so step back into the pane before typing into the next
    # session — otherwise the text lands in the list's single-letter hotkeys.
    step_key C-l

    step_key C-f
    step_key c
    step_wait_pane "SCRIPTED-READY" 30
    step_sleep 1
    step_type "filter swap done"
    step_key Enter
    step_wait_pane "GOT:filter swap done" 20
    step_key C-f
    step_key U
    step_wait_pane "unloaded — Enter loads" 30
    step_key C-l

    step_key C-f
    step_key c
    step_wait_pane "SCRIPTED-READY" 30
    step_sleep 1
    step_type "drift band flat"
    step_key Enter
    step_wait_pane "GOT:drift band flat" 20
    step_key C-f
    step_key U
    step_wait_pane "unloaded — Enter loads" 30
    step_wait_pane "GOT:drift band flat" 15

    # Three frozen, two live. Cycling twice more must land on a live agent both
    # times — `<leader> c` steps over every ghost rather than into one.
    step_key C-f
    step_key c
    step_wait_pane "SCRIPTED-READY" 20
    step_key C-f
    step_key c
    step_wait_pane "SCRIPTED-READY" 20

    # And back: unload hands focus to the session list (a ghost has no PTY
    # worth pointing a keyboard at), so one Ctrl+L steps into the pane and
    # Enter loads it through the resume template.
    step_key C-f
    step_key c
    step_key C-f
    step_key U
    step_wait_pane "unloaded — Enter loads" 30
    step_key C-l
    step_key Enter
    step_wait_pane "SCRIPTED-READY mode=resume" 60
    step_type "back online"
    step_key Enter
    step_wait_pane "GOT:back online" 20
    step_sleep 2
}

scenario_assert_effects() {
    # Four unloads, one load: two of the five sessions still hold a process.
    local live
    live="$(fleet_live_windows)"
    [ "$live" = "2" ] \
        || e2e_die "want 2 live agent windows after 4 unloads + 1 load, got $live"
}

scenario_assert_ui() {
    assert_pane_contains "GOT:back online"
    # Every session kept its row, frozen or not.
    local n
    for n in 2 3 4 5; do
        assert_pane_contains "ring-0$n" || return 1
    done
}
