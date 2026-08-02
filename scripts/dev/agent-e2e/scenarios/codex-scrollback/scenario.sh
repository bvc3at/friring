# shellcheck shell=bash
#
# Scenario: a real Codex CLI answers with a reply taller than the pane, and the
# transcript that scrolls off the top is still reachable with Shift+Up.
#
# This is the end-to-end half of the inline-viewport scrollback fix. Codex runs
# on the NORMAL screen and grows its transcript the way ratatui's inline
# viewport does: pin a `DECSTBM` region anchored at row 1, scroll inside it,
# reset. Stock vt100 discards every line that leaves a region, so the pane's
# scrollback stayed empty and Shift+Up / the wheel / the scrollbar were silent
# no-ops — the session looked like it had no history at all. The unit tests in
# src/agent/backend.rs pin the emulator rule on synthetic bytes; this pins it
# against whatever the real binary emits, so a Codex release that re-renders
# its transcript differently fails here.
#
# Test-mode only (Shift+Up has no VHS key); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Shift+Up reaches the top of a real Codex transcript that scrolled off the pane"
SCENARIO_AGENT="codex"
SCENARIO_PROMPT="Say the long reply now."
# The composer-line glyph. Codex's input placeholder text rotates and the
# footer varies with the cwd, so the prompt glyph is the stable ready marker
# (verified against codex-cli 0.146.0).
SCENARIO_AGENT_READY="›"
# The first and last markers of the reply. The fixture is deliberately taller
# than any pane the harness renders, so the head is certain to leave the screen
# — without that the scroll assert could pass without scrolling anything.
SCENARIO_HEAD_PATTERN="SCROLLBACK-HEAD"
SCENARIO_TAIL_PATTERN="SCROLLBACK-TAIL"

# Press Shift+Up until the pane shows $1, at most $2 times (one line per tick,
# like the keybinding itself).
scroll_up_until() {
    local pattern="$1" ticks="${2:-120}"
    for _ in $(seq 1 "$ticks"); do
        e2e_pane | grep -q "$pattern" && return 0
        step_key S-Up
        sleep 0.05
    done
    return 1
}

# Wait until the pane STOPS showing $1. The reply's tail appears while codex is
# still repainting the rows above it, so "the head is off screen" needs its own
# wait rather than being read off the frame the tail arrived in.
wait_pane_gone() {
    local pattern="$1" tries="${2:-100}"
    for _ in $(seq 1 "$tries"); do
        e2e_pane | grep -q "$pattern" || return 0
        sleep 0.1
    done
    return 1
}

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_sleep 1
    step_type "$SCENARIO_PROMPT"
    # Sync on the composer echo before Enter: step_sleep is a no-op in test
    # mode, so without this the Enter races codex's composer.
    step_wait_pane "long reply" 30
    step_key Enter
    step_wait_pane "$SCENARIO_TAIL_PATTERN" 60
}

scenario_assert_effects() {
    [ "$(journal_matched long-reply)" -ge 1 ] || e2e_die "long-reply fixture never matched"
}

scenario_assert_ui() {
    # The premise: the reply pushed its own head out of the viewport. Without
    # it the scroll below would "succeed" without scrolling anything.
    wait_pane_gone "$SCENARIO_HEAD_PATTERN" \
        || e2e_die "reply did not overflow the pane — nothing scrolled off to scroll back to:
--- pane ---
$(e2e_pane)" || return 1
    scroll_up_until "$SCENARIO_HEAD_PATTERN" \
        || e2e_die "Shift+Up never reached the top of the codex transcript (empty pane scrollback):
--- pane ---
$(e2e_pane)" || return 1
    # Friring's own marker for "you are scrolled up", in the pane title.
    e2e_pane | grep -qE '\[[0-9]+↑\]' \
        || e2e_die "scrolled up, but the pane title shows no [N↑] marker:
--- pane ---
$(e2e_pane)"
}
