# shellcheck shell=bash
#
# Scenario: a mailbox send must not answer someone else's permission dialog.
#
# The wake nudge is a paste followed by a *separate* Enter. A recipient
# sitting on claude's tool-approval prompt swallows the paste and reads that
# Enter as the operator answering it — which confirms the highlighted
# `1. Yes`. Before the modal guard, `friring-cli message send` therefore
# approved whatever the recipient was asking permission to do, by default and
# with nobody watching.
#
# This drives the real thing, not a simulation: claude runs without
# --dangerously-skip-permissions (SCENARIO_CLAUDE_PERMISSIONS=default), so the
# stub's Bash tool_use raises claude's own dialog. Then it proves both halves
# of the contract —
#
#   1. Security: a send while the dialog is up types nothing, leaves the
#      dialog up, and leaves the Bash call unexecuted (the proof file stays
#      absent). The retry sweep refuses too, not just the send path.
#   2. Liveness: the nudge is owed, not dropped. Once a *human* answers the
#      dialog, `automation tick` retries and the agent really receives the
#      nudge — proved by the model stub answering a turn whose prompt is the
#      nudge text.
#
# Test-mode only (drives friring-cli mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="A mailbox wake defers instead of answering a live permission dialog, then lands after approval"
SCENARIO_AGENT="claude"
SCENARIO_CLAUDE_PERMISSIONS="default"
SCENARIO_PROMPT="Run the wake-guard-proof command with Bash."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="WAKE-NUDGE-DELIVERED"

# The tool's effect, which must NOT appear while the dialog is unanswered.
wg_proof_file() { printf '%s/wake-guard-proof.txt' "$E2E_WS"; }

# Hold for `secs` and fail the moment the dialog is answered behind our back.
# Proving a negative needs a bounded wait, and this is the assertion the
# original report made by hand: it watched the dialog stay up for 25 s, then
# watched a single `message send` clear it within 5.
wg_require_still_blocked() {
    local secs="$1"
    for _ in $(seq 1 "$((secs * 5))"); do
        [ -f "$(wg_proof_file)" ] \
            && e2e_die "the Bash call ran — a wake answered the permission dialog" && return 1
        [ "$(e2e_hook_state)" = "blocked" ] \
            || e2e_die "session left 'blocked' (now '$(e2e_hook_state)') with nobody answering" \
            || return 1
        sleep 0.2
    done
}

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_sleep 1
    step_type "$SCENARIO_PROMPT"
    step_sleep 1
    step_key Enter

    # The dialog is the sync point: claude renders its Bash permission prompt,
    # then the Notification hook flips the DB row to blocked.
    step_wait_pane "Do you want" 60
    step_wait_state 'blocked' 60
    [ -f "$(wg_proof_file)" ] \
        && e2e_die "proof file exists before the dialog was answered" && return 1

    # --- 1. The send must not answer the dialog -----------------------------
    # Exactly the report's repro: an ordinary mailbox send from any process
    # that can exec friring-cli. (3>&-: the wake path arms the headless
    # automation heartbeat, which can spawn a tmux window.)
    local out
    out="$(friring-cli --json message send --to "$E2E_SESSION_ID" \
        --kind e2e-guard --body "WAKE-GUARD-BODY" 3>&-)" \
        || e2e_die "message send failed: $out" || return 1
    printf '%s' "$out" | jq -e '.enqueued == true and .woke == false
            and .wake_deferred == true
            and (.wake_deferred_reason | test("blocked|modal"))' >/dev/null \
        || e2e_die "send did not defer its wake: $out" || return 1

    # The retry sweep is guarded too — a tick while the dialog is up must
    # deliver nothing. Race-free: the recipient cannot leave `blocked` on its
    # own, so this holds however the 60 s heartbeat interleaves.
    out="$(friring-cli --json automation tick 3>&-)" \
        || e2e_die "automation tick failed: $out" || return 1
    printf '%s' "$out" | jq -e '.woke == []' >/dev/null \
        || e2e_die "tick woke a session that is still on a dialog: $out" || return 1

    # Nothing was typed, so the dialog is untouched and the tool never ran.
    wg_require_still_blocked 4 || return 1
    assert_pane_contains "Do you want" || return 1

    # The payload is queued regardless — the guard defers the nudge, never the
    # message.
    out="$(friring-cli --json message inbox --for "$E2E_SESSION_ID")" \
        || e2e_die "inbox peek failed: $out" || return 1
    printf '%s' "$out" | jq -e 'length == 1 and .[0].body == "WAKE-GUARD-BODY"
            and .[0].read_at == null' >/dev/null \
        || e2e_die "message not queued while the wake was deferred: $out" || return 1

    # --- 2. A human answers, and the owed nudge lands -----------------------
    # Option 1 ("Yes") is pre-selected; Enter confirms it. This is the *only*
    # Enter in the scenario that reaches the dialog, and a person sent it.
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast tail can
    # flip working->done between polls.
    step_wait_state 'working|done' 60
    step_wait_pane "WAKE-GUARD-APPROVED" 60
    step_wait_state 'done' 60

    # The sweep now has a pane it can safely type into. (The armed heartbeat
    # may beat this call to it; either way the agent must receive the nudge,
    # which is what the fixture below proves.)
    friring-cli --json automation tick 3>&- >/dev/null \
        || e2e_die "post-approval automation tick failed" || return 1

    # The stub only answers this turn if the nudge text really arrived as a
    # prompt — typed *and* submitted, not merely pasted.
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
}

scenario_assert_effects() {
    # The approved Bash call ran — once, and only after the human's Enter.
    [ -f "$(wg_proof_file)" ] \
        || e2e_die "wake-guard-proof.txt missing — the approved Bash call never ran" || return 1
    [ "$(journal_matched guarded-tool-call)" -ge 1 ] \
        || e2e_die "guarded-tool-call fixture never matched"
    [ "$(journal_matched after-approval)" -ge 1 ] \
        || e2e_die "after-approval fixture never matched (tool_result never posted)"
    # The retried nudge reached the model as a prompt.
    [ "$(journal_matched nudge-received)" -ge 1 ] \
        || e2e_die "nudge-received fixture never matched (the deferred wake never landed)"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
    [ "$(e2e_hook_state)" != "blocked" ] \
        || e2e_die "session still blocked after approval"
}
