# shellcheck shell=bash
#
# Scenario: the fork's conversation import (`i` in the session list), end to
# end with a REAL transcript. A stubbed first turn harvests one: claude runs
# with `--session-id {id}`, so the turn lands a genuine
# $CLAUDE_CONFIG_DIR/projects/<slug>/<id>.jsonl. A force delete then frees
# that conversation (the picker excludes live sessions' conversations), and
# the steps drive the three-stage import flow — picker → working directory
# (prefilled with the transcript's recorded cwd) → name — which relaunches
# claude with `--resume <id>`. Asserts the imported session pinned the
# ORIGINAL conversation id, the seed fixture matched exactly once (resume is
# a replay, not a re-run), and a follow-up turn continues the conversation.
#
# Test-mode only (captures ids and rebinds E2E_SESSION_ID via friring-cli
# mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Import an on-disk Claude Code conversation and resume it in a new session"
SCENARIO_AGENT="claude"
SCENARIO_PROMPT="seed the transcript for import"
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="IMPORT-RESUMED-MARKER"

# Rebind E2E_SESSION_ID to the imported session. After the force delete the
# DB has zero live rows, so the import is the single row once it spawns.
# (Not step_resolve_session: that resolves by name, and the imported name
# derives from the transcript title — an implementation detail this test
# must not pin twice.)
resolve_imported_session() {
    local id=""
    for _ in $(seq 1 150); do
        id="$(friring-cli --json session list 2>/dev/null \
            | jq -r 'if length == 1 then .[0].id else empty end')"
        [ -n "$id" ] && break
        sleep 0.2
    done
    [ -n "$id" ] || e2e_die "imported session never became the single DB row" || return 1
    E2E_SESSION_ID="$id"
}

scenario_steps() {
    # Turn 1 — harvest the transcript the import flow will pick up.
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_type "$SCENARIO_PROMPT"
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "IMPORT-SEED-MARKER" 60
    step_wait_state 'done' 60

    # The conversation id the import must carry over — read while the seed
    # session's row still exists.
    E2E_IMPORT_CONVO_ID="$(friring-cli --json session get "$E2E_SESSION_ID" \
        | jq -r '.agent_session_id // empty')"
    [ -n "$E2E_IMPORT_CONVO_ID" ] \
        || e2e_die "seed session has no agent_session_id" || return 1

    # Free the conversation: the picker excludes live sessions' conversations
    # (importing one would race the running agent on the same transcript), so
    # only the delete makes the harvested transcript appear.
    friring-cli --json session delete "$E2E_SESSION_ID" --force >/dev/null \
        || e2e_die "session delete --force failed" || return 1
    step_wait_pane "No sessions yet" 30

    # 'i' is session-list scoped; C-h forces list focus in case the delete
    # left the (now dead) terminal focused.
    step_key C-h
    step_key i
    step_wait_pane "Import Claude Code Conversation" 30
    # The scan found exactly the harvested transcript, titled by its first
    # user prompt.
    step_wait_pane "Conversations (1)" 30
    step_wait_pane "seed the transcript" 30

    # Directory step: prefilled with the transcript's recorded cwd ($E2E_WS);
    # "resume here" is the dir-focus footer, so it doubles as the step marker.
    step_key Enter
    step_wait_pane "resume here" 30

    # Name step (prefilled from the transcript title), then spawn.
    step_key Enter
    step_wait_pane "Import — Name" 30
    step_key Enter

    # The imported session relaunches claude --resume <id>: the seeded turn
    # replays into the pane WITHOUT a model call (asserted via the journal).
    step_wait_pane "$SCENARIO_AGENT_READY" 120
    step_wait_pane "IMPORT-SEED-MARKER" 60
    resolve_imported_session || return 1

    # Turn 2 — the resumed conversation continues where it left off.
    step_type "follow up after import"
    step_key Enter
    step_wait_state 'working|done' 30
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
}

scenario_assert_effects() {
    # Identity carried over: the imported session runs the ORIGINAL
    # conversation, so every id-keyed feature (hooks, restart, F9) lines up.
    local got
    got="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.agent_session_id')"
    [ "$got" = "$E2E_IMPORT_CONVO_ID" ] \
        || e2e_die "imported agent_session_id '$got' != harvested '$E2E_IMPORT_CONVO_ID'" \
        || return 1
    # Exactly one: --resume replays the transcript without calling the model;
    # a second match would mean the import re-ran the seeded turn.
    [ "$(journal_matched import-seed)" = "1" ] \
        || e2e_die "import-seed matched $(journal_matched import-seed) time(s), want exactly 1" \
        || return 1
    [ "$(journal_matched import-followup)" -ge 1 ] \
        || e2e_die "import-followup fixture never matched"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
