# shellcheck shell=bash
#
# Scenario: the four headless agent-metrics commands, end to end against a real
# Claude Code turn. One tool-use turn leaves everything the commands read —
# a statusline JSON under $FRIRING_METRICS_DIR, a live agent process tree, and
# an on-disk transcript — and each command is then asserted through
# `friring-cli --json`:
#
#   session metrics    cost / tokens / context / churn from the statusline file
#   session resources  summed RSS + process count of the agent tree
#   session activity   commands / edits / tokens reconstructed from transcripts
#   usage              account rate-limit windows, served by the stub
#
# The point of asserting these headlessly is that they must work with **no TUI
# attached** — so every probe here runs against the sandbox DB and the same
# on-disk sources the TUI reads, never a value the TUI published.
#
# Two seams are seeded by scenario_setup because friring does not own them:
# the `statusLine` script (friring only injects FRIRING_METRICS_DIR and
# FRIRING_SESSION_ID — the agent's own statusline writes the file, see
# docs/CLI.md) and Claude's OAuth credentials (which `usage` reads before it
# will call the endpoint at all).
#
# Test-mode only (the asserts are CLI probes, nothing to film); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="friring-cli reports statusline, resource, activity and usage metrics headlessly"
SCENARIO_AGENT="claude"
SCENARIO_PROMPT="Create metrics-proof.txt using the Write tool."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes.
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="METRICS-TURN-DONE"

# Where the seeded statusline writes, mirroring what friring injects into the
# agent. Resolved in scenario_setup (the sandbox HOME is known by then).
E2E_METRICS_DIR=""

scenario_setup() {
    # 1. The statusline producer. friring ships no statusline and cannot wire
    #    one: `statusLine` is a scalar setting, so a managed --settings file
    #    (how the hooks extension injects hooks, which *merge*) would silently
    #    replace the user's own statusline instead of composing with it. So it
    #    injects FRIRING_METRICS_DIR + FRIRING_SESSION_ID and reads whatever
    #    the user's statusline writes there. This is that user-side half — the
    #    recording lines are the ones documented in docs/CLI.md, so this
    #    scenario fails if the documented contract stops working. (The doc's
    #    snippet then prints the user's own content where this prints a
    #    constant; only the recording half is the contract.)
    local statusline="$TBX_SANDBOX_ROOT/friring-statusline.sh"
    cat > "$statusline" <<'SL'
#!/bin/sh
# Claude passes its statusline payload on stdin; friring reads it back from
# $FRIRING_METRICS_DIR/$FRIRING_SESSION_ID.json.
input=$(cat)
if [ -n "$FRIRING_METRICS_DIR" ] && [ -n "$FRIRING_SESSION_ID" ]; then
    mkdir -p "$FRIRING_METRICS_DIR"
    printf '%s' "$input" > "$FRIRING_METRICS_DIR/$FRIRING_SESSION_ID.json"
fi
printf 'friring'
SL
    chmod +x "$statusline"

    # claude merges $CLAUDE_CONFIG_DIR/settings.json with the hooks
    # extension's --settings file, so this coexists with the status hooks.
    mkdir -p "$HOME/claude-config"
    jq -n --arg cmd "$statusline" \
        '{statusLine: {type: "command", command: $cmd}}' \
        > "$HOME/claude-config/settings.json" || return 1

    # 2. Credentials for `usage`. It reads the subscription OAuth token before
    #    it will call the endpoint at all; without this the fetch reports "not
    #    logged in" and never reaches the stub.
    jq -n '{claudeAiOauth: {accessToken: "friring-e2e-usage-token",
                            subscriptionType: "max"}}' \
        > "$HOME/claude-config/.credentials.json" || return 1

    E2E_METRICS_DIR="$XDG_DATA_HOME/friring-dev/metrics"
}

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_type "$SCENARIO_PROMPT"
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
}

# Poll for the statusline file: claude renders its statusline on its own
# cadence, so the turn finishing does not mean the file is on disk yet.
metrics_file_ready() {
    local aid="$1" f
    f="$E2E_METRICS_DIR/$aid.json"
    for _ in $(seq 1 100); do
        [ -s "$f" ] && jq -e '.model or .cost or .context_window' "$f" >/dev/null 2>&1 \
            && return 0
        sleep 0.2
    done
    return 1
}

scenario_assert_effects() {
    assert_ws_file_eq metrics-proof.txt "metrics!"

    local aid
    aid="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.agent_session_id')"
    [ -n "$aid" ] && [ "$aid" != "null" ] \
        || e2e_die "session has no agent_session_id" || return 1

    # --- session metrics -------------------------------------------------
    # The statusline wrote the file friring documents, and the command parses
    # it into the same fields the info panel shows.
    metrics_file_ready "$aid" \
        || e2e_die "statusline never wrote $E2E_METRICS_DIR/$aid.json
--- metrics dir ---
$(ls -la "$E2E_METRICS_DIR" 2>&1)" || return 1

    local metrics
    metrics="$(friring-cli --json session metrics "$E2E_SESSION_ID")" \
        || e2e_die "session metrics failed" || return 1
    [ "$(printf '%s' "$metrics" | jq -r '.session_id')" = "$E2E_SESSION_ID" ] \
        || e2e_die "session metrics reported the wrong session: $metrics" || return 1
    [ "$(printf '%s' "$metrics" | jq -r '.note')" = "null" ] \
        || e2e_die "session metrics reported a note: $metrics" || return 1
    # The stub pins token counts, so the context-window fields are real
    # numbers rather than nulls — that is the whole point of the file.
    printf '%s' "$metrics" | jq -e '.metrics.model_id != null' >/dev/null \
        || e2e_die "session metrics has no model_id: $metrics" || return 1
    printf '%s' "$metrics" | jq -e '.metrics.total_input_tokens >= 0' >/dev/null \
        || e2e_die "session metrics has no token tally: $metrics" || return 1
    # Every documented key is present even when the statusline omitted it —
    # a stable key set is the CLI's contract with jq pipelines.
    printf '%s' "$metrics" | jq -e 'has("metrics") and (.metrics | has("total_cost_usd")
        and has("context_window_size") and has("cache_read_input_tokens"))' >/dev/null \
        || e2e_die "session metrics key set is incomplete: $metrics" || return 1

    # --- session resources ------------------------------------------------
    # A live pane must price as a real tree: the agent process plus whatever
    # it forked, never zero and never the caller's own pid.
    local res
    res="$(friring-cli --json session resources "$E2E_SESSION_ID")" \
        || e2e_die "session resources failed" || return 1
    [ "$(printf '%s' "$res" | jq -r '.state')" = "live" ] \
        || e2e_die "session resources not live: $res" || return 1
    printf '%s' "$res" | jq -e '.rss_bytes > 1048576' >/dev/null \
        || e2e_die "session resources reported an implausible rss: $res" || return 1
    printf '%s' "$res" | jq -e '.procs >= 1 and .pid > 1' >/dev/null \
        || e2e_die "session resources reported no processes: $res" || return 1
    # CPU is opt-in (it costs a sampling delay); without --cpu it stays null.
    [ "$(printf '%s' "$res" | jq -r '.cpu_percent')" = "null" ] \
        || e2e_die "session resources sampled cpu without --cpu: $res" || return 1
    printf '%s' "$(friring-cli --json session resources "$E2E_SESSION_ID" --cpu)" \
        | jq -e '.cpu_percent != null' >/dev/null \
        || e2e_die "session resources --cpu reported no cpu" || return 1

    # --- session activity -------------------------------------------------
    # Reconstructed from the transcript claude wrote under the pinned
    # --session-id: the Write shows up as one edit on the touched file.
    local act
    act="$(friring-cli --json session activity "$E2E_SESSION_ID")" \
        || e2e_die "session activity failed" || return 1
    [ "$(printf '%s' "$act" | jq -r '.provider')" = "claude-code" ] \
        || e2e_die "session activity resolved the wrong provider: $act" || return 1
    printf '%s' "$act" | jq -e '.counts.edits >= 1 and .counts.prompts >= 1' >/dev/null \
        || e2e_die "session activity missed the write turn: $act" || return 1
    printf '%s' "$act" | jq -e '[.files[].path] | any(endswith("metrics-proof.txt"))' \
        >/dev/null \
        || e2e_die "session activity did not aggregate the touched file: $act" || return 1
    printf '%s' "$act" | jq -e '.tokens.output > 0' >/dev/null \
        || e2e_die "session activity reported no output tokens: $act" || return 1

    # --all covers every active session and keeps the array shape.
    friring-cli --json session activity --all | jq -e 'type == "array" and length >= 1' \
        >/dev/null || e2e_die "session activity --all is not an array" || return 1

    # --- usage ------------------------------------------------------------
    # Account windows come from the stub's /api/oauth/usage route, reached the
    # same way the info panel reaches it.
    local usage
    usage="$(FRIRING_CLAUDE_USAGE_URL="$AGENT_E2E_STUB_URL/api/oauth/usage" \
        friring-cli --json usage --agent claude)" \
        || e2e_die "usage failed" || return 1
    [ "$(printf '%s' "$usage" | jq -r '.[0].plan')" = "max" ] \
        || e2e_die "usage did not read the credentials' plan: $usage" || return 1
    # The fixture pins 34% / 12%; asserting the values proves the whole chain
    # (credentials -> bearer token -> endpoint -> parser), not just a 200.
    printf '%s' "$usage" | jq -e '[.[0].windows[] | select(.label == "5h")]
        | first | .used_percent == 34' >/dev/null \
        || e2e_die "usage 5h window wrong: $usage" || return 1
    printf '%s' "$usage" | jq -e '[.[0].windows[] | select(.label == "Week")]
        | first | .used_percent == 12' >/dev/null \
        || e2e_die "usage weekly window wrong: $usage" || return 1
    printf '%s' "$usage" | jq -e '.[0].windows[0].resets_at > 0' >/dev/null \
        || e2e_die "usage window has no reset time: $usage" || return 1
    [ "$(jq -rs '[.[] | select(.kind == "usage")] | length' "$E2E_JOURNAL")" -ge 1 ] \
        || e2e_die "the stub never served the usage route" || return 1

    [ "$(journal_matched metrics-write-call)" -ge 1 ] \
        || e2e_die "metrics-write-call fixture never matched" || return 1
    [ "$(journal_matched after-metrics-write)" -ge 1 ] \
        || e2e_die "after-metrics-write fixture never matched" || return 1
}

scenario_assert_ui() {
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done" || return 1
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
}
