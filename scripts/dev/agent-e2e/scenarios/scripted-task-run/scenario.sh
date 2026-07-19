# shellcheck shell=bash
#
# Scenario: the tasks pipeline end to end — a CLI-created task shows up in
# the F5 panel, `r` opens the trigger-time "Run task" picker, and the Send
# action seeds the full-context agent prompt (title, description, the
# friring-cli self-service hints) into the running session's PTY. The
# scripted agent's GOT: echoes prove the multi-line paste arrived intact;
# the run flips the task todo → in_progress (panel glyph + DB), and the
# scenario then closes it out the way the seeded prompt instructs the agent
# to (`friring-cli task edit <id> --status done`), watching the glyph
# advance to done.
#
# Test-mode only (extracts the task id via friring-cli mid-steps); not
# demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Task run (F5 -> r) seeds the agent prompt; status tracks todo -> in_progress -> done"
SCENARIO_AGENT="scripted"

# Bounded poll for the task's persisted status — the DB write happens on the
# TUI thread (Send trigger) or in a second CLI process (edit), so the read
# must not race either.
task_wait_status() {
    local want="$1" tries="$2" got=""
    for _ in $(seq 1 "$tries"); do
        got="$(friring-cli --json task show "$E2E_TASK_ID" 2>/dev/null \
            | jq -r '.status // empty')"
        [ "$got" = "$want" ] && return 0
        sleep 0.2
    done
    e2e_die "task $E2E_TASK_ID status never became $want (last: '$got')"
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # A second CLI process writes the task into the shared DB; the live TUI
    # picks it up on its ~250ms poll.
    local out
    out="$(friring-cli --json task create --title "Ship the e2e suite" \
        --description "Prove the task pipeline works end to end.")" \
        || e2e_die "task create failed: $out" || return 1
    E2E_TASK_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_TASK_ID" ] && [ "$E2E_TASK_ID" != "null" ] \
        || e2e_die "no task id in: $out" || return 1

    # F5 shows AND focuses the tasks panel; the single task renders with the
    # todo glyph and is selected by default (selection resets to row 0).
    step_key F5
    step_wait_pane " Tasks " 15
    step_wait_pane "☐ Ship the e2e suite" 15

    # r -> trigger-time action picker; the precreated session is a Send
    # target (U+2192 arrow in the entry label).
    step_key r
    step_wait_pane "Run task" 15
    step_wait_pane "Send → $E2E_SCENARIO_NAME" 15
    step_key Enter

    # While the tasks panel is focused the central pane shows the task
    # preview, not the terminal — leave the panel (it stays visible,
    # inactive) so the agent pane and its GOT: echoes are on screen.
    step_key Escape
    step_wait_pane "Sent task to $E2E_SCENARIO_NAME" 15

    # The seeded prompt landed on the scripted agent's stdin (bracketed
    # paste + Enter; each prompt line echoes as its own GOT: line): the
    # header names the task id, and the closing self-service hint tells the
    # agent how to mark it done.
    step_wait_pane "working on Friring task #$E2E_TASK_ID" 30
    step_wait_pane "task edit $E2E_TASK_ID --status done" 30

    # The Send trigger advanced todo -> in_progress: DB first, then the
    # panel glyph on the TUI's task poll.
    task_wait_status in_progress 75 || return 1
    step_wait_pane "◐ Ship the e2e suite" 15

    # Close the task out exactly as the seeded prompt instructs the agent
    # to; the running TUI must render the flip on its poll.
    friring-cli task edit "$E2E_TASK_ID" --status "done" >/dev/null \
        || e2e_die "task edit --status done failed" || return 1
    task_wait_status "done" 75 || return 1
    step_wait_pane "☑ Ship the e2e suite" 15
}

scenario_assert_effects() {
    # The full lifecycle persisted: exactly the one task, and it ended done.
    local n
    n="$(friring-cli --json task list | jq 'length')"
    [ "$n" = "1" ] || e2e_die "task list has $n rows, want 1" || return 1
    task_wait_status "done" 5
}

scenario_assert_ui() {
    assert_pane_contains "☑ Ship the e2e suite"
}
