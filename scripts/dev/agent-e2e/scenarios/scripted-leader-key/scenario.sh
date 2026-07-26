# shellcheck shell=bash
#
# Scenario: the tmux-style leader key, end to end through the real stack.
#
# The acceptance tests drive `App::update` directly, so they prove the state
# machine but say nothing about whether the chord *arrives*. That is the part
# most likely to break here: friring runs inside tmux, and a leader that works
# in a bare terminal can still be swallowed a layer up — opencode's `ctrl+x`
# leader is documented as emitting a literal `^X` under tmux
# (sst/opencode#4097). This scenario drives keys through the driver tmux into
# friring's tmux into the agent pane, which is the path that actually matters.
#
# What it pins:
#   - `Ctrl+F` (byte 0x06) survives the nesting and arms the leader;
#   - the which-key overlay lists what the next key does;
#   - `<leader> b` dispatches, and `<leader> Ctrl+B` does the same (the
#     Ctrl-held form, so a sequence needs no modifier release);
#   - a mistyped leader sequence is swallowed rather than injected into the
#     agent's prompt — the failure mode that would corrupt a real prompt;
#   - `<leader> <leader>` reaches the PTY instead of running a command;
#   - `Esc` cancels, leaving the session drivable.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Leader key arms through nested tmux, dispatches, and never leaks to the PTY"
SCENARIO_AGENT="scripted"

# Bounded poll for a pane string being GONE (step_wait_pane only waits for
# presence, and the overlay closing is proven by its rows disappearing).
leader_wait_pane_gone() {
    local pattern="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        e2e_pane | grep -qF -- "$pattern" || return 0
        sleep 0.2
    done
    e2e_die "pane still shows: $pattern
--- pane ---
$(e2e_pane)"
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # 1. Ctrl+F arms. If 0x06 were eaten anywhere between the driver tmux and
    #    friring, the overlay would never paint and this fails loudly — which
    #    is the single most important assertion in the file.
    step_key C-f
    step_wait_pane "go to session N" 15
    step_wait_pane "send key to agent" 15

    # 2. Esc cancels without running anything.
    step_key Escape
    leader_wait_pane_gone "go to session N" 50

    # 2b. `prefix2` (F12) arms the same table. Worth asserting through the real
    #     transport rather than only in-process: F-keys are terminfo-dependent
    #     in a way a bare `Ctrl+<letter>` is not, and prefix2 is the documented
    #     escape hatch for anyone whose outer multiplexer eats the primary.
    step_key F12
    step_wait_pane "go to session N" 15
    step_key Escape
    leader_wait_pane_gone "go to session N" 50

    # 3. `<leader> b` opens the info panel. " Info ─" is the panel's border
    #    title; a bare " Info " would false-match the footer hint.
    step_key C-f
    step_key b
    step_wait_pane " Info ─" 15

    # 4. The Ctrl-held form of the same table key closes it again, proving
    #    `<leader> C-b` == `<leader> b` (GNU screen's convention) over the
    #    same transport.
    step_key C-f
    step_key C-b
    leader_wait_pane_gone " Info ─" 50

    # 5. A mistyped leader sequence must not reach the agent. `C-f` then an
    #    unbound key, then a probe line: the echoed text must be exactly the
    #    probe, with no stray character from the swallowed key. Events are
    #    handled in order, so the probe round-trip proves the earlier press
    #    was already processed.
    step_key C-f
    step_key "§"
    step_type "leader-no-leak"
    step_key Enter
    step_wait_pane "GOT:leader-no-leak" 15
    ! e2e_pane | grep -qE 'GOT:.?§' \
        || e2e_die "an unbound leader key leaked into the agent prompt" || return 1

    # 6. `<leader> <leader>` sends the literal byte instead of dispatching:
    #    the info panel must NOT have toggled, and the session stays drivable.
    step_key C-f
    step_key C-f
    ! e2e_pane | grep -qF -- " Info ─" \
        || e2e_die "double-leader dispatched a command instead of sending the byte" \
        || return 1

    # 7. Still alive after every leader gesture.
    step_key C-u
    step_type "leader-final-alive"
    step_key Enter
    step_wait_pane "GOT:leader-final-alive" 15
}
