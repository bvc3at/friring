# shellcheck shell=bash
#
# Scenario: the inter-session message queue, end to end. Session A (the
# precreated scenario session) sends a structured message to a second,
# CLI-created session by *name* (exercising name resolution); the wake
# nudge types a self-describing "you have new mail" line into the
# recipient's PTY (proved by the scripted agent's GOT: echo, read back
# headlessly). The recipient then peeks (unread, read_at null), claims
# (exactly-once: a second claim returns nothing), and replies by message id
# alone — the reply routes back to A by stored provenance, threads on
# `in_reply_to`, and wakes A's visible pane. Sender identity is passed both
# ways (UUID for A, name for the peer), since the scripted agent has no
# injected FRIRING_SESSION identity of its own.
#
# It then isolates the **pane-scrape half** of the modal guard, which the
# claude-wake-modal-guard scenario cannot: this agent declares no status
# hooks, so hook_state stays null and only the visible-pane check can
# refuse. See that scenario for the guard against a real dialog.
#
# Test-mode only (drives friring-cli mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Inter-session mailbox: wake nudge, exactly-once claim, reply threading, pane-scrape guard"
SCENARIO_AGENT="scripted"

# Bounded poll for the wake nudge landing in a session's PTY, read back via
# headless `session capture`. `GOT:` proves the nudge was *submitted* (the
# script echoed the line), not merely typed; the send path wraps it in
# bracketed-paste escapes, so match on the GOT: line rather than a contiguous
# "GOT:friring".
mq_wait_wake() {
    local sid="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        friring-cli --json session capture "$sid" 2>/dev/null \
            | jq -r '.output // empty' | grep -q "GOT:.*new mail" && return 0
        sleep 0.2
    done
    e2e_die "session $sid pane never echoed the wake nudge"
}

scenario_steps() {
    # Session A (the precreated $E2E_SCENARIO_NAME) is adopted and ready.
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Recipient session, created headlessly while the TUI runs (3>&- guards
    # the tmux-window-spawning create against the bats fd-3 hang).
    local out
    out="$(friring-cli --json session create --name msg-peer \
        --repo-path "$E2E_WS" --agent scripted 3>&-)" \
        || e2e_die "session create msg-peer failed: $out" || return 1
    E2E_PEER_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_PEER_ID" ] && [ "$E2E_PEER_ID" != "null" ] \
        || e2e_die "no id for msg-peer in: $out" || return 1
    step_wait_pane "msg-peer" 15

    # Send addressed by NAME, provenance by UUID — both reference forms in
    # one call. `.woke` must be true: the peer's tmux window exists, so the
    # nudge had somewhere to land. (3>&-: the wake path arms the headless
    # automation heartbeat, which can spawn a tmux window.)
    out="$(friring-cli --json message send --to msg-peer --kind e2e-ping \
        --body "MSG-BODY-E2E" --from "$E2E_SESSION_ID" 3>&-)" \
        || e2e_die "message send failed: $out" || return 1
    printf '%s' "$out" | jq -e '.enqueued == true and .woke == true' >/dev/null \
        || e2e_die "send not enqueued+woke: $out" || return 1
    E2E_MSG_ID="$(printf '%s' "$out" | jq -r '.message_id')"
    [ -n "$E2E_MSG_ID" ] && [ "$E2E_MSG_ID" != "null" ] \
        || e2e_die "no message_id in: $out" || return 1

    # The wake nudge reached the peer's PTY (typed + submitted).
    mq_wait_wake "$E2E_PEER_ID" 75 || return 1

    # Peek does not consume: the message is there, unread.
    out="$(friring-cli --json message inbox --for msg-peer)" \
        || e2e_die "inbox peek failed: $out" || return 1
    printf '%s' "$out" | jq -e 'length == 1 and .[0].kind == "e2e-ping"
            and .[0].body == "MSG-BODY-E2E" and .[0].read_at == null' >/dev/null \
        || e2e_die "peek mismatch: $out" || return 1

    # Claim drains it exactly once: first claim returns the message, a
    # second claim comes back empty.
    out="$(friring-cli --json message inbox --for msg-peer --claim)" \
        || e2e_die "inbox claim failed: $out" || return 1
    printf '%s' "$out" | jq -e --argjson mid "$E2E_MSG_ID" \
        'length == 1 and .[0].id == $mid and .[0].body == "MSG-BODY-E2E"' >/dev/null \
        || e2e_die "claim mismatch: $out" || return 1
    out="$(friring-cli --json message inbox --for msg-peer --claim)" \
        || e2e_die "second inbox claim failed: $out" || return 1
    printf '%s' "$out" | jq -e 'length == 0' >/dev/null \
        || e2e_die "second claim not empty: $out" || return 1

    # Reply needs only the message id — routing back to A comes from the
    # stored sender provenance. A is the visible session, so its wake echo
    # lands in the driver pane.
    out="$(friring-cli --json message reply "$E2E_MSG_ID" --body "REPLY-E2E" \
        --from msg-peer 3>&-)" \
        || e2e_die "message reply failed: $out" || return 1
    printf '%s' "$out" | jq -e '.enqueued == true and .woke == true' >/dev/null \
        || e2e_die "reply not enqueued+woke: $out" || return 1
    mq_wait_wake "$E2E_SESSION_ID" 75 || return 1

    # --- The pane-scrape half of the modal guard ---------------------------
    # The scripted agent declares no status hooks, so hook_state stays null
    # here and the DB half of the guard has nothing to say. That isolates the
    # other half: put dialog-shaped text on the peer's screen (the agent
    # echoes whatever it is sent, so `GOT:Do you want to …` renders a line
    # matching MODAL_MARKERS) and the wake must refuse on the *pane* alone.
    friring-cli --json session send "$E2E_PEER_ID" \
        "Do you want to proceed with this?" >/dev/null \
        || e2e_die "session send of the dialog-shaped line failed" || return 1
    for _ in $(seq 1 75); do
        friring-cli --json session capture "$E2E_PEER_ID" 2>/dev/null \
            | jq -r '.output // empty' | grep -q "GOT:.*Do you want to" && break
        sleep 0.2
    done

    out="$(friring-cli --json message send --to msg-peer --kind e2e-guard \
        --body "GUARD-BODY-E2E" --from "$E2E_SESSION_ID" 3>&-)" \
        || e2e_die "guarded message send failed: $out" || return 1
    printf '%s' "$out" | jq -e '.enqueued == true and .woke == false
            and .wake_deferred == true
            and (.wake_deferred_reason | test("modal"))' >/dev/null \
        || e2e_die "send did not defer on the pane scrape alone: $out" || return 1

    # `session send` is refused for the same reason, but loudly — typing is
    # its whole purpose, so a silent skip would be a lie to the caller.
    if friring-cli --json session send "$E2E_PEER_ID" "SHOULD-NOT-ARRIVE" \
        >/dev/null 2>&1; then
        e2e_die "session send typed into a pane showing a dialog" || return 1
    fi
    # …and --force is the way through for an operator who means it.
    friring-cli --json session send "$E2E_PEER_ID" "FORCED-E2E" --force >/dev/null \
        || e2e_die "session send --force was refused" || return 1
}

scenario_assert_effects() {
    # Exactly-once held for the drained message; the deferred-wake send is
    # still queued (the guard withholds the nudge, never the payload).
    local out
    out="$(friring-cli --json message inbox --for msg-peer --claim)" \
        || e2e_die "post-run peer claim failed: $out" || return 1
    printf '%s' "$out" | jq -e 'length == 1 and .[0].body == "GUARD-BODY-E2E"' >/dev/null \
        || e2e_die "deferred-wake message not queued for the peer: $out" || return 1

    # The reply landed in A's mailbox with the default kind, the peer's id as
    # provenance — the id-only reply really resolved both endpoints — and a
    # reference to the message it answers.
    out="$(friring-cli --json message inbox --for "$E2E_SESSION_ID" --claim)" \
        || e2e_die "reply claim failed: $out" || return 1
    printf '%s' "$out" | jq -e --arg peer "$E2E_PEER_ID" --argjson mid "$E2E_MSG_ID" \
        'length == 1 and .[0].kind == "reply" and .[0].body == "REPLY-E2E"
            and .[0].from_session_id == $peer and .[0].in_reply_to == $mid' >/dev/null \
        || e2e_die "reply mismatch: $out" || return 1
}

scenario_assert_ui() {
    # A's visible pane shows the reply's wake echo (submitted, not just typed).
    e2e_pane | grep -q "GOT:.*new mail" \
        || e2e_die "driver pane never showed A's wake echo
--- pane ---
$(e2e_pane)"
}
