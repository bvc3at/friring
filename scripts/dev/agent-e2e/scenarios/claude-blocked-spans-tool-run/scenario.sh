# shellcheck shell=bash
#
# Scenario: `hook_state = blocked` spans an approved tool call's whole run.
#
# This pins a third-party behavior the modal guard's *policy* rests on, so a
# claude release that changes it fails here instead of silently changing what
# friring skips. Measured sequence for a permission-gated Bash call:
#
#   UserPromptSubmit -> working
#   PreToolUse       -> working     (fires BEFORE the permission dialog)
#   Notification     -> blocked     (the dialog is up)
#   << a human approves; NO hook fires >>
#   … the tool runs, for as long as it takes, still reporting `blocked` …
#   Stop             -> done
#
# There is no "approved" hook, so between the approval and the end of the turn
# `blocked` means "a dialog was raised at some point in this turn" — it does
# *not* mean a dialog is on screen now. That is why the automation and task
# delivery paths deliberately consult only the visible pane (which cannot go
# stale) and not this state: gating them on it would skip every scheduled fire
# aimed at a session that is merely running a long approved tool call, which is
# an ordinary thing to be doing. See `cli::pane_guard`.
#
# The 20 s sleep is the point of the scenario, not padding: a short tool call
# cannot tell "the state never leaves blocked" apart from "the poll missed a
# transient working".
#
# Test-mode only (samples the DB mid-turn); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="hook_state stays 'blocked' for an approved tool call's whole run — no 'working' in between"
SCENARIO_AGENT="claude"
SCENARIO_CLAUDE_PERMISSIONS="default"
SCENARIO_PROMPT="Run the longtool-probe command with Bash."
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="LONGTOOL-PROBE-DONE"

# How long the approved Bash call sleeps (must match fixtures.json).
LT_TOOL_SECS=20

# States sampled from approval until the tool's effect lands, oldest first,
# and the wall-clock seconds that sampling actually spanned.
LT_SAMPLES=""
LT_WATCHED=0

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_sleep 1
    step_type "$SCENARIO_PROMPT"
    step_sleep 1
    step_key Enter
    step_wait_pane "Do you want" 60
    step_wait_state 'blocked' 60

    # Approve, then sample every 0.5 s until the tool's effect lands. Bounded
    # by construction (the tool sleeps LT_TOOL_SECS), so this is a poll with a
    # ceiling rather than an open-loop wait.
    step_key Enter
    local i started=$SECONDS
    for i in $(seq 1 $((LT_TOOL_SECS * 4))); do
        LT_SAMPLES="$LT_SAMPLES $(e2e_hook_state)"
        [ -f "$E2E_WS/longtool-proof.txt" ] && break
        sleep 0.5
    done
    LT_WATCHED=$((SECONDS - started))
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
}

scenario_assert_effects() {
    [ -f "$E2E_WS/longtool-proof.txt" ] \
        || e2e_die "longtool-proof.txt missing — the approved Bash call never ran" || return 1

    # The observation has to have spanned most of the tool call, so "never saw
    # working" is a real finding and not a poll that quit early. Gate on the
    # wall clock, not the sample count: each poll costs a DB round-trip on top
    # of its sleep, so a slower machine yields fewer samples for the same span.
    [ "$LT_WATCHED" -ge $((LT_TOOL_SECS - 5)) ] \
        || e2e_die "sampling spanned only ${LT_WATCHED}s of a ${LT_TOOL_SECS}s tool call — \
too short to conclude
samples:$LT_SAMPLES" || return 1

    # The invariant: an approved tool call reports `blocked` throughout. If
    # claude ever grows an after-approval hook this flips to `working`, and the
    # pane-only veto for automations/tasks is worth revisiting.
    case "$LT_SAMPLES" in
        *working*)
            e2e_die "hook_state reported 'working' during the approved tool call — claude now \
signals after approval, so 'blocked' is no longer stale mid-run and the pane-only veto for \
automations/tasks (cli::pane_guard) should be reconsidered
samples:$LT_SAMPLES" || return 1
            ;;
    esac
    # …and it really was blocked, not merely absent.
    case "$LT_SAMPLES" in
        *blocked*) ;;
        *) e2e_die "never observed 'blocked' after approval; samples:$LT_SAMPLES" || return 1 ;;
    esac
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
