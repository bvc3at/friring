# shellcheck shell=bash
#
# Scenario: session fork (<leader> f) + parent linkage, end to end. Turn 1
# runs in the precreated parent; `<leader> f` opens the pre-filled
# "Fork — Name" modal, and accepting it spawns a child that records
# parent_session_id, nests under the parent in the sidebar (└ prefix), and
# launches claude through the fork template
# (--resume <parent-id> --fork-session -n <name>). The fork REPLAYS the parent
# conversation from the on-disk transcript — zero model calls, which the
# journal proves (base fixture matched exactly once).
#
# Then BOTH branches take a turn of their own. That is the feature, and one
# turn cannot show it: a clip that stops after the child's reply has filmed a
# copy, not a fork. The child answers a question the parent never asked, the
# parent answers one the child never saw, and neither transcript carries the
# other's turn — see the fork-context-leak fixture, which exists to catch
# exactly that and must never match.
#
# Not named `claude-fork`: the scenario name IS the session name, so the child
# would render as `claude-fork-fork` in the sidebar and in the header badge.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Ctrl+F forks claude: parent link, sidebar nesting, and both branches continue apart"
SCENARIO_AGENT="claude"
SCENARIO_PROMPT="Draft the cutover plan for the Gulf Stream scheduler."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="CUTOVER-PLAN-READY"

scenario_steps() {
    # Turn 1 in the parent — this is the conversation the fork will replay,
    # and the last turn the two branches will ever have in common.
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
    step_leader f
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

    # Rebind state waits + asserts to the child row, and keep its id: the last
    # turn happens back in the parent, which rebinds E2E_SESSION_ID again.
    step_resolve_session "$E2E_SCENARIO_NAME-fork" 30
    E2E_FORK_SESSION_ID="$E2E_SESSION_ID"

    # Turn 2, in the CHILD: the request history now contains the replayed base
    # turn, so the fixtures key on the LAST user message (promptContains).
    step_type "In this branch, cut over the coldest current first instead."
    step_key Enter
    step_wait_state 'working|done' 30
    step_wait_pane "COLDEST-FIRST-PLAN" 60
    step_wait_state 'done' 60

    # Turn 3, back in the ORIGINAL. `<leader> c` steps to the next loaded
    # session; with two of them that is the parent. Its badge is matched
    # without the `-fork` suffix, so this cannot be satisfied by the child.
    step_leader c
    step_wait_pane "$E2E_SCENARIO_NAME  ◐" 30
    step_resolve_session "$E2E_SCENARIO_NAME" 30

    # The parent still holds turn 1 and nothing else: its pane shows the plan
    # it drafted, with no trace of the branch that ran beside it.
    step_type "Stay on the warm-first order. How long is the freeze window?"
    step_key Enter
    step_wait_state 'working|done' 30
    step_wait_pane "WARM-FIRST-WINDOW" 60
    step_wait_state 'done' 60
    step_sleep 2
}

scenario_assert_effects() {
    local child parent_link child_agent
    child="$(friring-cli --json session get "$E2E_FORK_SESSION_ID")"
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
        || e2e_die "fork-child fixture never matched" || return 1
    [ "$(journal_matched fork-parent)" -ge 1 ] \
        || e2e_die "fork-parent fixture never matched" || return 1
    # The whole point, asserted at the wire: a request carrying BOTH branches'
    # turns would have matched the leak fixture ahead of either real one. Zero
    # means the two conversations never merged.
    [ "$(journal_matched fork-context-leak)" -eq 0 ] \
        || e2e_die "a branch's turn reached the other branch's conversation"
}

scenario_assert_ui() {
    assert_pane_contains "WARM-FIRST-WINDOW"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
