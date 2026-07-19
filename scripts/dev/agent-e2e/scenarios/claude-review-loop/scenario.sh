# shellcheck shell=bash
#
# Scenario: the native code-review loop end to end. An uncommitted edit is
# reviewed in Friring's review pane (switched to the Working target through
# the `t` picker), annotated with a classified comment, and sent to a real
# Claude Code binary with `e` — the structured handoff (C-id, class,
# `side:line` locator, quoted anchor line) must arrive at the model API
# byte-intact for the fixture to match. The agent's working → done edge then
# fires the re-review nudge toast, and reopening the review proves the
# annotation survived close-on-send (SQLite).
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Native review loop: annotate, send structured handoff to real Claude Code, re-review nudge"
SCENARIO_AGENT="claude"
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="REVIEW-LOOP-DONE"

scenario_prepare() {
    # The harness commits every workspace/ seed; the Working-target review
    # shows *uncommitted* changes, so the reviewable edit is made post-boot.
    printf 'def greet():\n    print("hello, world")\n' > "$E2E_WS/app.py"
}

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_sleep 1
    # Ctrl+X is terminal-passthrough (the emacs prefix must reach the agent),
    # so leave the terminal first: Ctrl+H focuses the session list, then
    # Ctrl+X opens the review. "Review summary" is the wait marker — the pane
    # title is right-aligned and clipped at this width, but the summary
    # section always renders once the async build lands.
    step_key C-h
    step_key C-x
    step_wait_pane "Review summary" 30
    step_sleep 1
    # The base resolves (main), so the review opens on the empty main..HEAD
    # branch target; the uncommitted edit lives in the Working target. The
    # `t` picker opens with the current target (Branch, entry 3) selected —
    # two `k` reach Working. Plain letters go through step_type: VHS has no
    # bare-letter Key mapping, and a literal send is the same keystroke.
    step_type "t"
    step_wait_pane "Review target" 15
    step_type "kk"
    step_key Enter
    # The Working build lands the seeded edit's del/add pair.
    step_wait_pane 'print("hello, world")' 30
    step_sleep 1
    # Header → hunk → first diff line (context `def greet():`, new:1), then
    # compose: `c` opens the box, Tab cycles Note → Issue, Ctrl+S saves.
    step_type "jj"
    step_type "c"
    step_key Tab
    step_sleep 1
    step_type "Use a warmer greeting"
    step_sleep 1
    step_key C-s
    step_wait_pane "Comment saved" 15
    step_sleep 1
    # `e` compiles the structured handoff, closes the pane (close-on-send),
    # and pastes it into the agent with a deferred Enter.
    step_type "e"
    step_wait_pane "Review sent to agent" 15
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 60
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
    # The send armed the re-review nudge; the working → done edge fires it.
    step_wait_pane "F7 to re-review" 30
    step_sleep 2
    # Reopen: the annotation lives in SQLite, so it survives close-on-send.
    # A fresh open lands on the Branch target again — the comment anchors to
    # a Working-target line, so switch back before looking for its row.
    step_key C-h
    step_key C-x
    step_wait_pane "Review summary" 30
    step_type "t"
    step_wait_pane "Review target" 15
    step_type "kk"
    step_key Enter
    step_wait_pane "Use a warmer greeting" 30
    step_sleep 2
}

scenario_assert_effects() {
    # The load-bearing pin lives in the fixture match itself (the exact
    # handoff shape); here it is enough that it matched at all.
    [ "$(journal_matched review-handoff)" -ge 1 ] \
        || e2e_die "review-handoff fixture never matched (the compiled review never reached the model)"
}

scenario_assert_ui() {
    # The reopened review shows the persisted comment row (badge + body — a
    # combination the agent pane's echo of the handoff never renders on one
    # line, so this can only be the review row).
    assert_pane_contains "[Issue] Use a warmer greeting"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
