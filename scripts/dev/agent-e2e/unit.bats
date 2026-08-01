#!/usr/bin/env bats
# Pure-shell unit tests for the agent-e2e harness itself: the strict-offline
# invariant and the demo tape generator, exercised with NO agent binary, no
# tmux, no network. They run even on a host without claude — so a green
# agent-e2e job always means at least the harness logic was verified (it is
# never entirely skipped), and the fail-closed half of the offline guarantee
# has a real negative test. See docs/E2E.md.

setup() {
    AGENT_E2E_DIR="$(cd "$BATS_TEST_DIRNAME" && pwd)"
    export AGENT_E2E_DIR
    # shellcheck disable=SC1091
    source "$AGENT_E2E_DIR/lib/harness.sh"
    command -v jq >/dev/null 2>&1 || skip "jq required"
    SCENARIO_REQUIRE_ALL_FIXTURES=1
    E2E_JOURNAL="$BATS_TEST_TMPDIR/journal.jsonl"
    E2E_FIXTURES="$BATS_TEST_TMPDIR/fixtures.json"
}

# A fixtures file with one ambient and one non-ambient ("primary") response,
# matching the shipped ambient-first convention.
write_fixtures() {
    cat > "$E2E_FIXTURES" <<'JSON'
{ "responses": [
  { "name": "ambient", "ambient": true, "match": { "modelContains": "haiku" }, "reply": { "text": "ok" } },
  { "name": "primary", "match": { "promptContains": "x" }, "reply": { "text": "y" } }
] }
JSON
}

@test "invariant: passes when every non-ambient fixture matched and no UNMATCHED" {
    write_fixtures
    printf '%s\n' '{"kind":"messages","matched":"primary"}' > "$E2E_JOURNAL"
    run assert_stub_invariants
    [ "$status" -eq 0 ]
}

@test "invariant: fails on an UNMATCHED model call" {
    write_fixtures
    printf '%s\n' \
        '{"kind":"messages","matched":"primary"}' \
        '{"kind":"messages","matched":"UNMATCHED"}' > "$E2E_JOURNAL"
    run assert_stub_invariants
    [ "$status" -ne 0 ]
    [[ "$output" == *unmatched* ]]
}

@test "invariant: fails when a non-ambient fixture was never exercised" {
    write_fixtures
    : > "$E2E_JOURNAL" # primary never appears
    run assert_stub_invariants
    [ "$status" -ne 0 ]
    [[ "$output" == *primary* ]]
}

@test "invariant: an unused ambient fixture does not fail the run" {
    write_fixtures
    printf '%s\n' '{"kind":"messages","matched":"primary"}' > "$E2E_JOURNAL"
    run assert_stub_invariants
    [ "$status" -eq 0 ]
}

@test "demo: emit-tape maps scenario steps to a valid VHS tape" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
    local tape="$BATS_TEST_TMPDIR/out.tape"
    e2e_emit_tape "$tape"
    grep -q '^Output target/agent-e2e/demos/claude-tool-loop.gif$' "$tape"
    grep -q '^Set FontSize 18$' "$tape"
    grep -q '^Wait+Screen' "$tape"            # a step_wait_pane mapped to a wait
    grep -q '^Type "Create hello.txt' "$tape" # the prompt was typed
    grep -q '^Set Width 1920$' "$tape"        # geometry derived from SCENARIO_COLS
    # No Ctrl+Q: quitting inside the recording ends the clip on a bare shell.
    ! grep -q '^Ctrl+Q$' "$tape"
}

@test "demo: named-workspace wizard scenario is fully VHS-mappable (emit-tape)" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-named-workspace"
    local tape="$BATS_TEST_TMPDIR/named-ws.tape"
    # No boot: TBX_SANDBOX_ROOT is unset, so the steps' offline fallback root
    # must keep the tape generator deterministic.
    e2e_emit_tape "$tape"
    grep -q '^Ctrl+N$' "$tape"                          # opens the wizard
    grep -q '^Ctrl+P$' "$tape"                          # parent import
    grep -q '^Ctrl+O$' "$tape"                          # workspace-dir field
    grep -q '^Type "/tmp/friring-e2e/named-ws"$' "$tape" # the custom dir
}

@test "drift: an unexpected non-message endpoint is surfaced but never fails" {
    REPO_ROOT="$BATS_TEST_TMPDIR/repo"
    E2E_SCENARIO_NAME="unit"
    printf '%s\n' \
        '{"kind":"other","method":"HEAD","url":"/"}' \
        '{"kind":"other","method":"GET","url":"/v1/telemetry"}' > "$E2E_JOURNAL"
    run e2e_surface_unexpected_endpoints
    [ "$status" -eq 0 ] # visibility, not a gate
    grep -q 'GET /v1/telemetry' "$REPO_ROOT/target/agent-e2e/unexpected-endpoints.log"
    # the known connectivity probe is allowlisted, not logged as drift
    ! grep -q 'HEAD /' "$REPO_ROOT/target/agent-e2e/unexpected-endpoints.log"
}

@test "drift: only the allowlisted HEAD / probe → nothing surfaced" {
    REPO_ROOT="$BATS_TEST_TMPDIR/repo2"
    E2E_SCENARIO_NAME="unit"
    printf '%s\n' '{"kind":"other","method":"HEAD","url":"/"}' > "$E2E_JOURNAL"
    run e2e_surface_unexpected_endpoints
    [ "$status" -eq 0 ]
    [ ! -f "$REPO_ROOT/target/agent-e2e/unexpected-endpoints.log" ]
}

@test "demo: a key VHS cannot press fails the tape instead of vanishing from it" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
    # Alt parses in VHS's grammar but sends the bare capital, so mapping it
    # would type `U` at the agent; F-keys it rejects outright. Both must stop
    # the tape by name rather than leave a recording that runs to the end
    # having silently skipped the keypress.
    scenario_steps() { step_key Enter; step_key M-u; }
    run e2e_emit_tape "$BATS_TEST_TMPDIR/alt.tape"
    [ "$status" -ne 0 ]
    [[ "$output" == *"M-u"* ]]

    scenario_steps() { step_key Enter; step_key F9; }
    run e2e_emit_tape "$BATS_TEST_TMPDIR/fkey.tape"
    [ "$status" -ne 0 ]
    [[ "$output" == *"F9"* ]]
}

@test "demo: SCENARIO_DEMO_KEYS routes an unpressable key through the leader" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
    SCENARIO_DEMO_KEYS=("F9=C-f v" "M-u=C-f U")
    scenario_steps() { step_key F9; step_key M-u; }
    e2e_emit_tape "$BATS_TEST_TMPDIR/routed.tape"
    grep -zq 'Ctrl+F\nType "v"\nCtrl+F\nType "U"\n' "$BATS_TEST_TMPDIR/routed.tape"
}

@test "demo: a wait keeps the regex a scenario wrote, escaping only what RE2 adds" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
    # `.*` and the `\[` a grep needs for a literal bracket mean the same in
    # RE2; `(` does not, and `/` would close the delimiter.
    scenario_steps() { step_wait_pane "edit.*out.txt" 9; step_wait_pane " x \[Idle\] (1) a/b" 9; }
    e2e_emit_tape "$BATS_TEST_TMPDIR/waits.tape"
    grep -q '^Wait+Screen@9s /edit\.\*out\.txt/$' "$BATS_TEST_TMPDIR/waits.tape"
    grep -q '^Wait+Screen@9s / x \\\[Idle\\\] \\(1\\) a\\/b/$' "$BATS_TEST_TMPDIR/waits.tape"
}
