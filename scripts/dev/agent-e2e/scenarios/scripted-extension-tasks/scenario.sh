# shellcheck shell=bash
#
# Scenario: the extension lifecycle around the shipped github-issues
# extension, driven entirely through its offline seam. `extension install`
# from the repo dir + `activate` must create the manifest's deterministic
# sync automation (github-issues-tick); the extension's own upsert.sh —
# hand-fed a normalized issue JSON array instead of a live `gh` fetch, which
# is exactly the seam sync.sh uses — must create a task deduped by
# (source, external_id): a second push of the same id edits the task in
# place instead of duplicating it. The running TUI must surface the synced
# task in the F5 panel. Deleting the sync automation and running `automation
# tick` must self-heal it (`.healed` reports the recreate); `deactivate`
# must tear the automation down for good while the synced tasks — user
# data, not a managed resource — survive.
#
# Test-mode only (drives friring-cli and the extension scripts mid-steps);
# not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="github-issues extension: install/activate, offline upsert dedupe, tick self-heal, deactivate keeps tasks"
SCENARIO_AGENT="scripted"

# The normalized shape fetch.sh emits — v2 re-sends the SAME external_id with
# a changed title, so it must resolve to an edit, never a second task.
E2E_ISSUE_V1='[{"external_id":"42","title":"Fix the flux capacitor","status":"todo","url":"https://example.invalid/i/42","description":"from the e2e"}]'
E2E_ISSUE_V2='[{"external_id":"42","title":"Fix the flux capacitor v2","status":"todo","url":"https://example.invalid/i/42","description":"from the e2e"}]'

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Install straight from the repo checkout, then activate. 3>&- : both
    # verbs arm the tmux heartbeat keeper (bats fd-3 guard, like the
    # harness's own session create).
    friring-cli extension install "$REPO_ROOT/extensions/github-issues" >/dev/null 3>&- \
        || e2e_die "extension install failed" || return 1
    friring-cli extension activate github-issues >/dev/null 3>&- \
        || e2e_die "extension activate failed" || return 1
    E2E_EXT_ACTIVE_JSON="$(friring-cli --json extension list)"

    # Activation ensured the manifest's [[automations]]; its id feeds the
    # delete → self-heal round-trip below.
    E2E_SYNC_AUTO_ID="$(friring-cli --json automation list \
        | jq -r '[.[] | select(.name | contains("github-issues"))] | first | .id // empty')"
    [ -n "$E2E_SYNC_AUTO_ID" ] \
        || e2e_die "activation created no github-issues automation" || return 1

    # Offline sync seam: feed upsert.sh by hand (what sync.sh pipes into it
    # from fetch.sh), then snapshot the task list after each pass so the
    # asserts can compare the create against the dedupe-edit.
    printf '%s' "$E2E_ISSUE_V1" \
        | "$REPO_ROOT/extensions/github-issues/scripts/upsert.sh" --source github >/dev/null \
        || e2e_die "first upsert failed" || return 1
    E2E_TASKS_V1="$(friring-cli --json task list)"
    printf '%s' "$E2E_ISSUE_V2" \
        | "$REPO_ROOT/extensions/github-issues/scripts/upsert.sh" --source github >/dev/null \
        || e2e_die "second upsert failed" || return 1
    E2E_TASKS_V2="$(friring-cli --json task list)"

    # The TUI's ~250ms task poll must surface the CLI-synced task in the F5
    # panel. Short prefix: the 20%-wide column ellipsis-truncates the title.
    step_key F5
    step_wait_pane "Fix the flux" 15

    # Self-heal: delete the managed automation, then tick — the heal pass
    # runs before the firing pass, so one tick both reports and repairs it.
    friring-cli automation remove "$E2E_SYNC_AUTO_ID" >/dev/null \
        || e2e_die "automation remove failed" || return 1
    E2E_TICK_JSON="$(friring-cli --json automation tick 3>&-)" \
        || e2e_die "automation tick failed" || return 1
    E2E_AUTOS_HEALED="$(friring-cli --json automation list)"

    # Deactivate LAST: everything after this assert against the torn-down
    # end state (automation gone, tasks kept, extension inactive).
    friring-cli extension deactivate github-issues >/dev/null 3>&- \
        || e2e_die "extension deactivate failed" || return 1
}

# Exactly-one github/42 task in a task-list snapshot, with the given title —
# the dedupe contract in one probe.
assert_synced_task() {
    local snapshot="$1" title="$2" got
    got="$(jq -r --arg t "$title" \
        '[.[] | select(.source == "github" and .external_id == "42")]
         | [length, (first | .title == $t)] | @tsv' <<<"$snapshot")"
    [ "$got" = "$(printf '1\ttrue')" ] \
        || e2e_die "expected exactly 1 github/42 task titled '$title', got: $snapshot"
}

scenario_assert_effects() {
    # Registry state after activate: active and healthy.
    [ "$(jq -r '[.[] | select(.name == "github-issues")] | first
        | "\(.active) \(.healthy)"' <<<"$E2E_EXT_ACTIVE_JSON")" = "true true" ] \
        || e2e_die "extension not active/healthy after activate: $E2E_EXT_ACTIVE_JSON" \
        || return 1

    # First upsert: created with the fed fields intact.
    assert_synced_task "$E2E_TASKS_V1" "Fix the flux capacitor" || return 1
    [ "$(jq -r '[.[] | select(.external_id == "42")] | first
        | "\(.status) \(.external_url) \(.description)"' <<<"$E2E_TASKS_V1")" \
        = "todo https://example.invalid/i/42 from the e2e" ] \
        || e2e_die "synced task fields wrong: $E2E_TASKS_V1" || return 1

    # Second upsert: same task id edited (deduped), not a duplicate.
    assert_synced_task "$E2E_TASKS_V2" "Fix the flux capacitor v2" || return 1
    [ "$(jq -r '[.[] | select(.external_id == "42")] | first | .id' <<<"$E2E_TASKS_V1")" \
        = "$(jq -r '[.[] | select(.external_id == "42")] | first | .id' <<<"$E2E_TASKS_V2")" ] \
        || e2e_die "dedupe minted a new task id" || return 1

    # Tick reported the repair, and the automation really is back.
    [ "$(jq -r '[.healed[] | select(contains("github-issues"))] | length' \
        <<<"$E2E_TICK_JSON")" -ge 1 ] \
        || e2e_die "tick healed nothing for github-issues: $E2E_TICK_JSON" || return 1
    [ "$(jq -r '[.[] | select(.name | contains("github-issues"))] | length' \
        <<<"$E2E_AUTOS_HEALED")" = "1" ] \
        || e2e_die "sync automation not recreated after tick: $E2E_AUTOS_HEALED" || return 1

    # End state after deactivate: automation torn down, extension inactive,
    # but the synced task — user data — survived untouched.
    [ "$(friring-cli --json automation list \
        | jq -r '[.[] | select(.name | contains("github-issues"))] | length')" = "0" ] \
        || e2e_die "sync automation survived deactivate" || return 1
    [ "$(friring-cli --json extension list \
        | jq -r '[.[] | select(.name == "github-issues")] | first | .active')" = "false" ] \
        || e2e_die "extension still active after deactivate" || return 1
    assert_synced_task "$(friring-cli --json task list)" "Fix the flux capacitor v2"
}

scenario_assert_ui() {
    assert_pane_contains " Tasks "
    assert_pane_contains "Fix the flux"
}
