# shellcheck shell=bash
#
# Scenario: automations end to end, both firing paths. A one-shot `at:`
# automation is created *headlessly* while the TUI runs; the TUI's
# claim-based ~1s tick fires it and pastes the prompt into the target
# session's PTY (the scripted agent's GOT: echo proves delivery), and the
# recorded run row carries success + the target session id. Then the
# headless path: an exec automation on a far-future `at:` (a past one-shot
# never fires) is marked due with `automation run` and fired by
# `automation tick` with no TUI involvement — the TUI's own tick may race
# for that claim, and the CAS guarantees exactly one firer, so the proof is
# the command's file side effect, never which firer won. Finally the spent
# one-shot: the claim disables it and nulls next_run_at.
#
# Test-mode only (drives friring-cli mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="One-shot automation fires through the TUI tick; exec automation via headless run+tick"
SCENARIO_AGENT="scripted"

# Bounded poll for the run-history row: the run is recorded in the same TUI
# tick that sends the prompt, but the pane echo and the DB write have no
# ordering guarantee relative to each other.
auto_wait_run() {
    local id="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        [ "$(friring-cli --json automation runs "$id" | jq 'length')" -ge 1 ] && return 0
        sleep 0.2
    done
    e2e_die "automation #$id never recorded a run"
}

# Bounded poll for the exec side effect: whichever firer won the claim
# (headless tick, TUI tick, or the heartbeat keeper) touches the file.
auto_wait_file() {
    local f="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        [ -f "$f" ] && return 0
        sleep 0.2
    done
    e2e_die "exec automation never produced: $f"
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # One-shot send automation ~3s out, created by a second CLI process
    # against the shared DB — the running TUI is the intended firer. 3>&-
    # guards the heartbeat-arming tmux call (bats fd-3 hang).
    local when out
    # String(): console.log of a *number* goes through util.inspect, which
    # emits ANSI color under FORCE_COLOR — and colors the CLI's at: parse.
    when="$(node -e 'console.log(String(Date.now()+3000))')"
    out="$(friring-cli --json automation create --name ping \
        --trigger "at:$when" --prompt "AUTOMATION-PING-E2E" \
        --session "$E2E_SESSION_ID" 3>&-)" \
        || e2e_die "automation create ping failed: $out" || return 1
    E2E_AUTO_PING_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_AUTO_PING_ID" ] && [ "$E2E_AUTO_PING_ID" != "null" ] \
        || e2e_die "no automation id in: $out" || return 1

    # The sidebar footer picks the external row up on its ~250ms poll
    # (renders as "1 automation(s)").
    step_wait_pane "1 automation" 15
    # TUI tick claims and fires the due one-shot; the pasted prompt reaches
    # the scripted PTY, whose GOT: echo proves end-to-end delivery.
    step_wait_pane "GOT:AUTOMATION-PING-E2E" 30

    # Exec automation on a far-future at: (never due by itself), marked due
    # manually, then a headless tick — the whole run/tick path with no TUI
    # required. Which firer wins the ~1s race is asserted nowhere.
    out="$(friring-cli --json automation create --name exec-proof \
        --trigger "at:$(node -e 'console.log(String(Date.now()+999999999))')" \
        --command "touch $TBX_SANDBOX_ROOT/exec-proof" 3>&-)" \
        || e2e_die "automation create exec-proof failed: $out" || return 1
    E2E_AUTO_EXEC_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_AUTO_EXEC_ID" ] && [ "$E2E_AUTO_EXEC_ID" != "null" ] \
        || e2e_die "no automation id in: $out" || return 1
    friring-cli automation run "$E2E_AUTO_EXEC_ID" >/dev/null \
        || e2e_die "automation run failed" || return 1
    friring-cli --json automation tick 3>&- >/dev/null \
        || e2e_die "automation tick failed" || return 1
    auto_wait_file "$TBX_SANDBOX_ROOT/exec-proof" 75
}

scenario_assert_effects() {
    # The TUI recorded the one-shot's run: success, addressed to the target
    # session ("sent to {session_id}").
    auto_wait_run "$E2E_AUTO_PING_ID" 50 || return 1
    local run status detail
    run="$(friring-cli --json automation runs "$E2E_AUTO_PING_ID" | jq '.[0]')"
    status="$(printf '%s' "$run" | jq -r '.status')"
    [ "$status" = "success" ] \
        || e2e_die "ping run status '$status' != success ($run)" || return 1
    detail="$(printf '%s' "$run" | jq -r '.detail')"
    printf '%s' "$detail" | grep -qF -- "$E2E_SESSION_ID" \
        || e2e_die "ping run detail '$detail' does not mention $E2E_SESSION_ID" || return 1

    # Spent one-shot: the firing claim advances the schedule to "no future
    # occurrence" — next_run_at nulled AND the automation disabled.
    local spent
    spent="$(friring-cli --json automation show "$E2E_AUTO_PING_ID")"
    [ "$(printf '%s' "$spent" | jq -r '.enabled')" = "false" ] \
        || e2e_die "spent one-shot still enabled: $spent" || return 1
    [ "$(printf '%s' "$spent" | jq -r '.next_run_at')" = "null" ] \
        || e2e_die "spent one-shot still has next_run_at: $spent" || return 1

    # The exec command really ran (whoever won the claim).
    [ -f "$TBX_SANDBOX_ROOT/exec-proof" ] \
        || e2e_die "exec-proof file missing" || return 1
}

scenario_assert_ui() {
    assert_pane_contains "GOT:AUTOMATION-PING-E2E"
}
