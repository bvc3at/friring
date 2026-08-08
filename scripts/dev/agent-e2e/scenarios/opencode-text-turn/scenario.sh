# shellcheck shell=bash
#
# Scenario: a real opencode binary answers a prompt through the Friring TUI,
# with the model stubbed locally (openai dialect, chat completions). opencode
# fires ONE ambient call — session title generation, to the same model — which
# the fixtures answer via its "title generator" system prompt (listed first,
# per the ambient-first convention). No status hooks; pane-text sync only.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Real opencode text turn rendered through Friring, model stubbed"
SCENARIO_AGENT="opencode"
SCENARIO_PROMPT="Move ocean-current balancing off the legacy cron box."
# The input-box footer renders "Build · <model> <provider>" once the TUI is
# interactive — the stable ready marker (verified against opencode 1.17.15).
SCENARIO_AGENT_READY="Build ·"
SCENARIO_DONE_PATTERN="CRON-CUTOVER-STAGED"

scenario_steps() {
    # No settle beat after this wait: the recorder's pre-roll already waits for
    # this same marker off camera, so a `step_sleep` here would hold the opening
    # frame for a second with nothing happening on it.
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_type "$SCENARIO_PROMPT"
    # Sync on the composer echo before Enter (step_sleep is a no-op in test
    # mode, so Enter would otherwise race the composer).
    step_wait_pane "legacy cron box" 30
    step_key Enter
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_sleep 2
}

scenario_assert_effects() {
    [ "$(journal_matched text-turn)" -ge 1 ] || e2e_die "text-turn fixture never matched"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
}
