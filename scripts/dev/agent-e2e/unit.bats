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

# The driver parses the whole tape before it drives anything and errors on any
# line it does not know, so parsing a generated tape is the real check that the
# generator and the driver still speak the same language. `--print-duration`
# does exactly that and touches no tmux.
drive_tape_parses() {
    node "$REPO_ROOT/scripts/demo/lib/drive-tape.mjs" "$1" --print-duration
}

@test "demo: emit-tape maps scenario steps to a tape the driver accepts" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
    local tape="$BATS_TEST_TMPDIR/out.tape"
    e2e_emit_tape "$tape"
    run drive_tape_parses "$tape"
    [ "$status" -eq 0 ]
    grep -q '^Output target/agent-e2e/demos/claude-tool-loop.gif$' "$tape"
    grep -q '^Set FontSize 18$' "$tape"
    grep -q '^Wait /' "$tape"                              # a step_wait_pane
    grep -q '^Type "Write the moon re-enable note' "$tape"  # the prompt was typed
    # No Ctrl+Q: quitting inside the recording ends the clip on a bare shell.
    ! grep -q '^Key C-q$' "$tape"
    # No launch preamble: the recorder boots the TUI itself, off camera.
    ! grep -q '^Hide$' "$tape"
}

@test "demo: named-workspace wizard scenario emits a drivable tape" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-named-workspace"
    local tape="$BATS_TEST_TMPDIR/named-ws.tape"
    # No boot: TBX_SANDBOX_ROOT is unset, so the steps' offline fallback root
    # must keep the tape generator deterministic.
    e2e_emit_tape "$tape"
    run drive_tape_parses "$tape"
    [ "$status" -eq 0 ]
    grep -q '^Key C-n$' "$tape"                         # opens the wizard
    grep -q '^Key C-p$' "$tape"                         # parent import
    grep -q '^Key C-o$' "$tape"                         # workspace-dir field
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

@test "demo: a key with no VHS spelling records as the key itself" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
    # The whole point of `Key`: an Alt chord and an F-key are keys tmux knows
    # and VHS does not, and the demo has to press what the test presses. VHS
    # parsed both and sent the wrong thing (the bare capital) or nothing.
    scenario_steps() { step_key Enter; step_key M-u; step_key F9; }
    e2e_emit_tape "$BATS_TEST_TMPDIR/keys.tape"
    grep -zq 'Key Enter\nKey M-u\nKey F9\n' "$BATS_TEST_TMPDIR/keys.tape"
    run drive_tape_parses "$BATS_TEST_TMPDIR/keys.tape"
    [ "$status" -eq 0 ]
}

@test "demo: SCENARIO_DEMO_KEYS routes a key through the leader instead" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
    SCENARIO_DEMO_KEYS=("F9=C-f v" "M-u=C-f U")
    scenario_steps() { step_key F9; step_key M-u; }
    e2e_emit_tape "$BATS_TEST_TMPDIR/routed.tape"
    # With the leader's beat between the two keys — every multi-key route is a
    # leader chord, and back-to-back keys do not land as one (see step_leader).
    grep -zq 'Key C-f\nSleep 350ms\nKey v\nKey C-f\nSleep 350ms\nKey U\n' \
        "$BATS_TEST_TMPDIR/routed.tape"
}

@test "demo: a wait keeps the regex a scenario wrote, escaping only what JS adds" {
    e2e_scenario_load "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
    # `.*` and the `\[` a grep needs for a literal bracket mean the same in a
    # JS RegExp; `(` does not, and `/` would close the delimiter.
    scenario_steps() { step_wait_pane "edit.*out.txt" 9; step_wait_pane " x \[Idle\] (1) a/b" 9; }
    e2e_emit_tape "$BATS_TEST_TMPDIR/waits.tape"
    grep -q '^Wait /edit\.\*out\.txt/ 9s$' "$BATS_TEST_TMPDIR/waits.tape"
    grep -q '^Wait / x \\\[Idle\\\] \\(1\\) a\\/b/ 9s$' "$BATS_TEST_TMPDIR/waits.tape"
    run drive_tape_parses "$BATS_TEST_TMPDIR/waits.tape"
    [ "$status" -eq 0 ]
}
