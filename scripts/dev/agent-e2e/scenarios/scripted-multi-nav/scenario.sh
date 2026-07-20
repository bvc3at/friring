# shellcheck shell=bash
#
# Scenario: multi-session orchestration and quick switching, with a headless
# CLI instance mutating the same SQLite DB the running TUI renders. Two
# extra sessions are created *externally* (`friring-cli session create`)
# while the TUI is live — the sidebar must pick them up on its poll, proving
# multi-instance sync. Then the switching surface: Alt+2 jumps to the 2nd
# session in rendered order (repo groups: ws first, then ws-b holding
# nav-b/nav-c) and hands it Terminal focus (typed text reaches that PTY),
# Alt+1 jumps back, Ctrl+6 toggles to the alternate. Finally the headless
# side again: `session send`/`capture` reach a session's PTY with no TUI
# involvement, and `session focus` makes the running TUI switch the active
# session within its poll.
#
# Test-mode only (extracts ids via friring-cli mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Alt+N / Ctrl+6 switching across TUI- and CLI-created sessions; headless send/capture/focus"
SCENARIO_AGENT="scripted"

scenario_setup() {
    # Second repo beside the harness seed repo: the two CLI-created sessions
    # live here, so the sidebar renders two repo groups and the Alt+N order
    # (A row 1, nav-b row 2, nav-c row 3) is deterministic.
    local b="$TBX_SANDBOX_ROOT/ws-b"
    mkdir -p "$b"
    ( cd "$b" && git init -q && git commit -qm "e2e seed b" --allow-empty )
}

# Bounded poll for `session capture` output — the headless proof must not
# lean on the TUI's pane, and the PTY echo lands asynchronously after send.
nav_wait_capture() {
    local sid="$1" needle="$2" tries="$3"
    for _ in $(seq 1 "$tries"); do
        friring-cli --json session capture "$sid" 2>/dev/null \
            | jq -r '.output // empty' | grep -qF -- "$needle" && return 0
        sleep 0.2
    done
    e2e_die "session $sid capture never showed: $needle"
}

scenario_steps() {
    # Session A (the precreated $E2E_SCENARIO_NAME) is adopted and ready.
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Multi-instance sync: a second friring-cli process writes new sessions
    # into the shared DB; the live TUI must show the rows on its ~250ms
    # poll. 3>&- guards the tmux-window-spawning create (bats fd-3 hang).
    local out
    out="$(friring-cli --json session create --name nav-b \
        --repo-path "$TBX_SANDBOX_ROOT/ws-b" --agent scripted 3>&-)" \
        || e2e_die "session create nav-b failed: $out" || return 1
    E2E_NAV_B_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_NAV_B_ID" ] && [ "$E2E_NAV_B_ID" != "null" ] \
        || e2e_die "no id for nav-b in: $out" || return 1
    out="$(friring-cli --json session create --name nav-c \
        --repo-path "$TBX_SANDBOX_ROOT/ws-b" --agent scripted 3>&-)" \
        || e2e_die "session create nav-c failed: $out" || return 1
    E2E_NAV_C_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_NAV_C_ID" ] && [ "$E2E_NAV_C_ID" != "null" ] \
        || e2e_die "no id for nav-c in: $out" || return 1
    step_wait_pane "nav-b" 15
    step_wait_pane "nav-c" 15

    # Alt+2 = jump to the 2nd session in rendered order (ws group first,
    # then ws-b: A, nav-b, nav-c). The terminal pane title flips to the new
    # active session, and Terminal focus means plain typing lands on
    # nav-b's PTY — the GOT: echo proves the input reached *that* script.
    step_key M-2
    step_wait_pane " nav-b (scripted)" 15
    step_type "hello-b"
    step_key Enter
    step_wait_pane "GOT:hello-b" 15

    # Alt+1 back to A, then Ctrl+6 (alternate toggle) back to nav-b — the
    # toggle target is the session we just left.
    step_key M-1
    step_wait_pane " scripted-multi-nav (scripted)" 15
    step_key C-6
    step_wait_pane " nav-b (scripted)" 15

    # Headless send/capture on nav-c: text reaches a PTY the TUI never
    # focused, and the echo is read back without the TUI in the loop.
    friring-cli session send "$E2E_NAV_C_ID" "cli-sent-hello" >/dev/null \
        || e2e_die "session send to nav-c failed" || return 1
    nav_wait_capture "$E2E_NAV_C_ID" "GOT:cli-sent-hello" 75 || return 1

    # Headless focus: the pending-focus row makes the running TUI switch
    # its active session on the next poll (~250ms).
    friring-cli session focus "$E2E_SESSION_ID" >/dev/null \
        || e2e_die "session focus failed" || return 1
    step_wait_pane " scripted-multi-nav (scripted)" 15
}

scenario_assert_effects() {
    # Exactly the three live sessions — the external creates persisted, and
    # nothing was duplicated by the TUI adopting them.
    local n
    n="$(friring-cli --json session list | jq 'length')"
    [ "$n" = "3" ] || e2e_die "session list has $n rows, want 3" || return 1
    # The headless round-trip left its trace in nav-c's pane.
    nav_wait_capture "$E2E_NAV_C_ID" "GOT:cli-sent-hello" 5
}

scenario_assert_ui() {
    # The CLI focus won: the scenario session is active again.
    assert_pane_contains " scripted-multi-nav (scripted)"
}
