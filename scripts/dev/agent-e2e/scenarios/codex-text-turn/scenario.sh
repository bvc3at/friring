# shellcheck shell=bash
#
# Scenario: a real Codex CLI binary answers a prompt through the Friring TUI,
# with the model stubbed locally (openai dialect, Responses API), and its
# status hook transition (idle -> working -> done) observed in the session DB.
#
# The hook asserts are the regression test for codex's strict hook contract:
# it parses every hook's stdout and fails the hook on anything but empty/valid
# JSON, so a `friring-cli session signal` that isn't output-silenced reports
# nothing and paints "hook returned invalid <event> JSON output" in the pane.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Real Codex CLI text turn rendered through Friring, model stubbed"
SCENARIO_AGENT="codex"
SCENARIO_PROMPT="Say the ready phrase now."
# The composer-line glyph. Codex's input placeholder text rotates and the
# footer varies with the cwd, so the prompt glyph is the stable ready marker
# (verified against codex-cli 0.144.4).
SCENARIO_AGENT_READY="›"
SCENARIO_DONE_PATTERN="FRIRING-E2E-READY"

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    # SessionStart fires before the first prompt, so a booted-but-idle codex
    # already reports; the alternation covers a poll landing after the turn.
    step_wait_state 'idle|working|done' 30
    step_sleep 1
    step_type "$SCENARIO_PROMPT"
    # Sync on the composer echo before Enter: step_sleep is a no-op in test
    # mode, so without this the Enter races codex's composer and lands before
    # the text registers, leaving the prompt typed-but-unsubmitted.
    step_wait_pane "ready phrase" 30
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
    step_sleep 2
}

scenario_assert_effects() {
    [ "$(journal_matched text-turn)" -ge 1 ] || e2e_die "text-turn fixture never matched"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN" || return 1
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done" || return 1
    # The state asserts above cannot catch a rejected hook on their own: codex
    # runs the command, then refuses its output, so the signal still lands in
    # the DB and only the pane says anything. An accepted hook renders no cell
    # at all, so any hook cell here is a failure.
    if e2e_pane | grep -qiE 'hook \(failed\)|hook returned invalid'; then
        e2e_die "codex rejected a friring hook:
--- pane ---
$(e2e_pane)"
    fi
}
