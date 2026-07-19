# shellcheck shell=bash
#
# Scenario: native code review end to end — open the review of the session's
# uncommitted working change, add a classified line comment, and export the
# compiled markdown into the real Claude Code pane. The seed repo commits
# notes.txt at boot; scenario_setup appends a line AFTER that commit, so a
# working-changes diff exists. The review opens on the (empty) branch target,
# is retargeted to "Working changes (uncommitted)" through the t-picker, a
# line comment is composed and saved (default classification Issue), and `e`
# closes the review, pastes the compiled prompt into claude, and submits it —
# proven by the stub matching the review prompt and the ack landing in the
# pane with a done status.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Code review: comment on working changes, export the compiled review to claude"
SCENARIO_AGENT="claude"
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="REVIEW-ACK-FROM-STUB"

scenario_setup() {
    # Uncommitted working change: appended after the harness's seed commit, so
    # the branch diff (main..HEAD) stays empty while `git diff` shows one
    # added line — the review must be retargeted to see it.
    echo "This line is hasty and needs review." >> "$E2E_WS/notes.txt"
}

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60

    # F7 is global (works from Terminal focus). The seed repo resolves base
    # "main" with HEAD on it, so the review opens on the branch target with an
    # empty diff — that Info row doubles as the "initial build finished" sync
    # (the target picker is dead while a build is in flight).
    step_key F7
    step_wait_pane "Code review" 30
    step_wait_pane "No changes to show" 30

    # Retarget to the working tree. "Working changes" is always entry 0 of the
    # picker; the selection starts on the current (branch) target, so two `k`
    # presses saturate onto it regardless of how many commit entries follow.
    step_key t
    step_wait_pane "Review target" 15
    step_key k
    step_key k
    step_key Enter
    step_wait_pane "notes.txt" 30
    step_wait_pane "Working changes" 15

    # Comment on a diff LINE: the cursor reopens on the file header, so walk
    # rows (file header -> hunk header -> line) before composing. Default
    # classification is Issue — left as-is.
    step_key j
    step_key j
    step_key j
    step_key c
    step_wait_pane "Compose" 15
    step_type "Please fix this line before merging."
    step_key C-s
    # Saved-row badge adjacency "[Issue] Please fix…" only exists after the
    # compose box closed and the comment row rendered (`.` stands for the `]`
    # — a literal `[` would open a grep bracket expression).
    step_wait_pane "Issue. Please fix this line" 15

    # Export: closes the review, pastes the compiled markdown into claude with
    # a deferred Enter, and toasts.
    step_key e
    step_wait_pane "Review sent to agent" 15
    step_wait_pane "Please address the following code review" 30

    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 60
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
    step_sleep 2
}

scenario_assert_effects() {
    [ "$(journal_matched review-ack)" -ge 1 ] \
        || e2e_die "review-ack fixture never matched (compiled review never reached the model)"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
