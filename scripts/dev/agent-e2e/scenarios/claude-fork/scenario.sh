# shellcheck shell=bash
#
# Scenario: session fork (<leader> f) + parent linkage, end to end. Turn 1
# runs in the precreated parent; `<leader> f` opens the pre-filled
# "Fork — Name" modal, and accepting it
# spawns a child that records parent_session_id, nests under the parent in
# the sidebar (└ prefix), and launches claude through the fork template
# (--resume <parent-id> --fork-session -n <name>). The fork REPLAYS the
# parent conversation from the on-disk transcript — zero model calls, which
# the journal proves (base fixture matched exactly once) — and then
# continues it with a fresh turn of its own. Asserts the DB parent link and
# that the child got a distinct agent_session_id (a new conversation, not a
# shared one).
#
# Test-mode only (captures parent ids via friring-cli mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Ctrl+F forks claude: parent link, sidebar nesting, forked conversation continues"
SCENARIO_AGENT="claude"
SCENARIO_PROMPT="Draft the cutover plan for the Gulf Stream scheduler."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="CUTOVER-PLAN-READY"

scenario_steps() {
    # Turn 1 in the parent — this is the conversation the fork will replay.
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_type "$SCENARIO_PROMPT"
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "CUTOVER-PLAN-READY" 60
    step_wait_state 'done' 60

    # Parent identity, captured before the fork rebinds E2E_SESSION_ID: the
    # friring session id (the DB parent link) and the conversation id claude
    # owns (the fork must mint a different one).
    E2E_PARENT_SESSION_ID="$E2E_SESSION_ID"
    E2E_PARENT_AGENT_ID="$(friring-cli --json session get "$E2E_SESSION_ID" \
        | jq -r '.agent_session_id')"
    [ -n "$E2E_PARENT_AGENT_ID" ] && [ "$E2E_PARENT_AGENT_ID" != "null" ] \
        || e2e_die "parent has no agent_session_id" || return 1

    # `Ctrl+F` is the leader now, so fork is `<leader> f` — the same letter,
    # one key later. The leader arms from any pane, so the earlier hop to the
    # session list (Ctrl+H) is no longer needed to reach it, but it is kept so
    # the modal opens from the same focus this scenario has always used.
    step_key C-h
    step_key C-f
    step_key f
    step_wait_pane "Fork — Name" 30
    # The modal pre-fills "<parent-name>-fork"; accept it unchanged.
    step_wait_pane "$E2E_SCENARIO_NAME-fork" 15
    step_key Enter

    # Child spawns and becomes active: the header badge names it first (so the
    # replay wait below can't be satisfied by the PARENT's pane, which shows
    # the same marker), then the replayed base turn, then a fresh input box.
    # The badge, not the pane title, is what identifies the active session.
    step_wait_pane "$E2E_SCENARIO_NAME-fork  ◐" 60
    step_wait_pane "CUTOVER-PLAN-READY" 120
    step_wait_pane "$SCENARIO_AGENT_READY" 120

    # Sidebar nesting: the child renders under its parent with the tree glyph.
    step_wait_pane "└ $E2E_SCENARIO_NAME-fork" 30

    # Rebind state waits + asserts to the child row.
    step_resolve_session "$E2E_SCENARIO_NAME-fork" 30

    # Turn 2 in the CHILD: the request history now contains the replayed base
    # turn, so the fixtures key on the LAST user message (promptContains).
    step_type "In this branch, cut over the coldest current first instead."
    step_key Enter
    step_wait_state 'working|done' 30
    step_wait_pane "COLDEST-FIRST-PLAN" 60
    step_wait_state 'done' 60
}

scenario_assert_effects() {
    local child parent_link child_agent
    child="$(friring-cli --json session get "$E2E_SESSION_ID")"
    parent_link="$(printf '%s' "$child" | jq -r '.parent_session_id // empty')"
    child_agent="$(printf '%s' "$child" | jq -r '.agent_session_id // empty')"
    [ "$parent_link" = "$E2E_PARENT_SESSION_ID" ] \
        || e2e_die "child parent_session_id '$parent_link' != parent '$E2E_PARENT_SESSION_ID'" \
        || return 1
    [ -n "$child_agent" ] || e2e_die "child has no agent_session_id" || return 1
    [ "$child_agent" != "$E2E_PARENT_AGENT_ID" ] \
        || e2e_die "child agent_session_id equals the parent's — fork reused the conversation id" \
        || return 1
    # Exactly 1: the fork replay came from the transcript, not the model — a
    # second match would mean the replay silently re-ran the base turn.
    [ "$(journal_matched fork-base)" -eq 1 ] \
        || e2e_die "fork-base matched $(journal_matched fork-base) time(s), want exactly 1" \
        || return 1
    [ "$(journal_matched fork-child)" -ge 1 ] \
        || e2e_die "fork-child fixture never matched"
}

scenario_assert_ui() {
    assert_pane_contains "COLDEST-FIRST-PLAN"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
