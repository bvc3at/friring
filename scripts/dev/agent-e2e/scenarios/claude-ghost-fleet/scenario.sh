# shellcheck shell=bash
#
# Scenario: the ghost lifecycle at FLEET scale, on real Claude Code
# instances — the readable version of scripted-unload-ghost, which proves the
# mechanism on one session and is consequently impossible to read as a clip
# (one frozen frame looks exactly like one idle frame).
#
# Four real claude processes, unloaded one after another. The reason to run
# real ones rather than the scripted stub is the number beside each row: lazy
# sessions exist because an idle agent CLI is expensive, and only a real CLI
# has that cost. Each row carries its process tree's RSS, the list's bottom
# border carries the fleet total, and a ghost reads `—` — a measured absence,
# not a blank. So the clip can show the saving instead of asserting it: four
# live trees, then three, then two, then a fleet with no total at all because
# nothing is running, then one loaded back through `--resume`.
#
# The waits are what make that legible rather than lucky. `◌ ring-0N.*—` is
# the row for that session, frozen and repriced — the `◌` matters, because the
# toast line ("Unloaded 'ring-02' — press Enter…") would otherwise satisfy a
# name-and-dash match while the badge still showed the old figure.
#
# Three of the four are created by the steps so all four share one naming
# family; in demo mode that runs before the TUI starts, so the clip opens on
# the whole fleet (see e2e_demo_record).
#
# Navigation is by *cycling*, never by `<leader> <n>`: a numbered jump taken
# before an unload does not address the same row after one, and cycling is
# order-independent by construction. It is also the gesture being
# demonstrated — `<leader> c` steps over every ghost rather than into one.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Ghost fleet: 4 real claude sessions frozen one by one, memory falling, then one loaded back"
SCENARIO_AGENT="claude"
# The fleet's first member is the precreated session, renamed into the family;
# the steps add the other three. Precreating one matters beyond the name: the
# TUI focuses the terminal when it boots with a session and the session list
# when it boots empty, and demo mode boots AFTER the steps have run their
# `session create` calls while test mode boots before. With nothing
# precreated the two modes therefore start on opposite ends of the focus ring,
# and `Ctrl+L` cycles rather than selects — so no fixed key sequence lands on
# the pane in both. One session at boot removes the fork.
SCENARIO_SESSION_NAME="ring-01"
# VHS could not press Alt at all; the driver can. `<leader> U` is kept because
# it *films* better: an Alt chord is invisible, while the leader paints its
# which-key overlay and names the action it is about to run.
SCENARIO_DEMO_KEYS=("M-u=C-f U")
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"

# How many of the fleet still have an agent window on the friring-dev server.
# Assert-time only — a mid-step poll would run at tape-generation time in demo
# mode, long before the unloads it is meant to observe.
fleet_live_windows() {
    tmux -L friring-dev list-windows -a 2>/dev/null | grep -cE 'tb-ring-0[1-4]' || true
}

# Cycle to the next loaded session, confirm the header badge names it, and
# freeze it. The badge is what identifies the active session; the pane cannot,
# because four idle claude panes are identical.
fleet_freeze() {
    local name="$1"
    step_leader c
    step_wait_pane "$name  ◐" 30
    step_key M-u
    step_wait_pane "◌ $name.*—" 30
}

scenario_steps() {
    # One at a time, each waited for. Sessions render in `display_order, then
    # created_at` — but display_order is NULL for a new session, and the
    # tie-break for those is the order the app first saw them, which for three
    # concurrent creates is a race between three claude boots (observed:
    # 01, 02, 04, 03). Creating them in sequence is what makes `<leader> c`
    # visit them in the order this scenario names below.
    local n
    for n in 2 3 4; do
        friring-cli session create --name "ring-0$n" --repo-path "$E2E_WS" \
            --agent claude >/dev/null 3>&- \
            || e2e_die "session create ring-0$n failed" || return 1
        step_wait_pane "ring-0$n" 60
    done

    step_wait_pane "$SCENARIO_AGENT_READY" 120
    # Booting with a session focuses the terminal. Waited for rather than
    # assumed, and never corrected with `Ctrl+L`: that CYCLES the focus ring,
    # so pressing it when the pane already has focus walks away to the list —
    # where the next typed sentence lands on single-letter hotkeys (its `i`
    # opens the import modal). The footer's first field names whatever holds
    # focus, which is what makes this checkable at all.
    step_wait_pane "Terminal  4 session" 30

    # Start from a KNOWN row rather than whichever session happens to be
    # active. Test mode boots the TUI on ring-01 and adopts the rest around
    # it; demo mode boots with all four already in the DB and lands on
    # ring-04. Everything below is relative — `<leader> c` walks from wherever
    # it starts — so the two modes otherwise film different sessions. This is
    # the one place a numbered jump is right: it addresses a row, and nothing
    # has been unloaded yet to shift them.
    step_leader 1
    step_wait_pane "ring-01  ◐" 30

    # One real turn, so the last ghost has a conversation worth keeping — and
    # so the frozen frame below is visibly this session's, not any session's.
    step_type "Hold the south capture ring at 21C through the swap."
    step_key Enter
    step_wait_state 'working|done' 30
    step_wait_pane "RING-HOLDING" 90
    step_wait_state 'done' 60

    # Freeze the three that were only ever idle. This is the fleet the feature
    # is for: agents kept for later, each still costing a process tree.
    fleet_freeze "ring-02"
    fleet_freeze "ring-03"
    fleet_freeze "ring-04"

    # And the one that did the work. `<leader> c` has three ghosts to step
    # over now and only one place left to land.
    step_leader c
    step_wait_pane "ring-01  ◐" 30
    step_key M-u
    step_wait_pane "◌ ring-01.*—" 30
    # The frozen frame is still that session's own last output, and with no
    # tree left anywhere the list drops its total rather than printing a zero.
    step_wait_pane "RING-HOLDING" 15
    step_sleep 2

    # Unload left the focus on the session list (a ghost has no PTY worth
    # pointing a keyboard at) with ring-01 selected, so Enter loads it back
    # through the resume template.
    step_key Enter
    step_wait_pane "Session loaded" 30
    step_wait_pane "$SCENARIO_AGENT_READY" 120
    step_wait_pane "RING-HOLDING" 60

    # Loading hands focus back to the pane on its own — no Ctrl+L here, which
    # would cycle straight back out to the list and type the next sentence
    # into its hotkeys. Waited for rather than assumed.
    step_wait_pane "Terminal  4 session" 30

    # Live again: the resumed conversation answers a fresh turn, and the row is
    # repriced with a real figure instead of the em dash.
    step_type "Anything drift while you were parked?"
    step_key Enter
    step_wait_state 'working|done' 30
    step_wait_pane "NO-DRIFT" 90
    step_wait_state 'done' 60
    step_sleep 2
}

scenario_assert_effects() {
    # Four unloads, one load: exactly one of the four still holds a process.
    local live
    live="$(fleet_live_windows)"
    [ "$live" = "1" ] \
        || e2e_die "want 1 live agent window after 4 unloads + 1 load, got $live" || return 1
    # The load replayed the transcript locally — a second match would mean it
    # re-ran turn 1 against the model.
    [ "$(journal_matched fleet-turn)" -eq 1 ] \
        || e2e_die "fleet-turn matched $(journal_matched fleet-turn) time(s), want exactly 1"
}

scenario_assert_ui() {
    assert_pane_contains "NO-DRIFT"
    # Every session kept its row, frozen or not.
    local n
    for n in 1 2 3 4; do
        assert_pane_contains "ring-0$n" || return 1
    done
}
