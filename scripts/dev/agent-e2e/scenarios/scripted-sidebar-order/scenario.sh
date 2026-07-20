# shellcheck shell=bash
#
# Scenario: the repo-grouped, manually ordered session list (#10). Three
# CLI-created sessions beside the adopted one span two repos, so the
# sidebar must paint one group header per repo; Shift+J/K then move the
# active session within its group, and the order is authoritative: a move
# renumbers EVERY session densely into `display_order`, persisted and
# readable back through `friring-cli session list`. The scripted agent
# keeps this a pure-UI scenario — zero model calls, runs on any machine.
#
# Test-mode only (CLI-created rows + pane line-number probes); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Repo-grouped sidebar; Shift+J/K manual reorder persisted as display_order"
SCENARIO_AGENT="scripted"
# The 22-char scenario name must render un-truncated in the sidebar for the
# line-number probes below; the list column is 18% of the terminal, so 160
# cols gives it 28 (26 inside the borders — glyph + name is 24).
SCENARIO_COLS=160

# Pane line number of $1's *sidebar* row. The active session's name also
# appears in the header bar, the terminal pane title, and the scripted
# agent's SCRIPTED-READY echo — all of which render above every sidebar
# session row (the echo is the first terminal body line, level with the
# group header), so the LAST match is always the sidebar row.
e2e_sidebar_line() {
    e2e_pane | grep -n -- "$1" | tail -1 | cut -d: -f1
}

# Bounded poll until the sidebar shows $1 on a smaller-numbered row than $2
# — the reorder sync primitive (relative row order isn't a grep pattern, so
# step_wait_pane can't express it).
e2e_wait_row_above() {
    local above="$1" below="$2" la lb
    for _ in $(seq 1 50); do
        la="$(e2e_sidebar_line "$above")"
        lb="$(e2e_sidebar_line "$below")"
        [ -n "$la" ] && [ -n "$lb" ] && [ "$la" -lt "$lb" ] && return 0
        sleep 0.1
    done
    e2e_die "'$above' never rendered above '$below'
--- pane ---
$(e2e_pane)"
}

scenario_setup() {
    # Second repo beside the harness seed repo: a distinct repo-set group
    # key, so the sidebar must render two group headers.
    local b="$TBX_SANDBOX_ROOT/ws-b"
    mkdir -p "$b"
    ( cd "$b" && git init -q && git commit -qm "e2e seed b" --allow-empty )
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Nav rows around the adopted session: two more in the seed repo (its
    # group gains reorder room) and one in ws-b (the second group). 3>&-:
    # headless create spawns tmux windows (bats fd-3 guard, see harness).
    friring-cli session create --name order-b --repo-path "$E2E_WS" \
        --agent scripted >/dev/null 3>&- \
        || e2e_die "create order-b failed" || return 1
    friring-cli session create --name order-c --repo-path "$E2E_WS" \
        --agent scripted >/dev/null 3>&- \
        || e2e_die "create order-c failed" || return 1
    friring-cli session create --name order-d --repo-path "$TBX_SANDBOX_ROOT/ws-b" \
        --agent scripted >/dev/null 3>&- \
        || e2e_die "create order-d failed" || return 1
    step_wait_pane "order-b" 15
    step_wait_pane "order-c" 15
    step_wait_pane "order-d" 15

    # One `{glyph} ── {label} ───…` header per repo group. The trailing
    # space distinguishes the ws header from the ws-b one.
    step_wait_pane "── ws " 15
    step_wait_pane "── ws-b" 15

    # Shift+J/K are session-list scoped (a plain J would land in the agent
    # PTY): focus the list first. The move targets the ACTIVE session —
    # the adopted scenario session, top of the ws group.
    step_key C-h
    step_key J
    e2e_wait_row_above "order-b" "$E2E_SCENARIO_NAME" || return 1
    # K restores the original order…
    step_key K
    e2e_wait_row_above "$E2E_SCENARIO_NAME" "order-b" || return 1
    # …and one final J leaves the moved order in place for the asserts.
    step_key J
    e2e_wait_row_above "order-b" "$E2E_SCENARIO_NAME" || return 1
}

scenario_assert_effects() {
    # save_state persists on the keypress, but poll briefly anyway: the
    # pane repaint the steps waited on doesn't strictly order the DB write.
    local list ok=""
    for _ in $(seq 1 50); do
        list="$(friring-cli --json session list)"
        [ "$(printf '%s' "$list" | jq '[.[].display_order] | all(. != null)')" = "true" ] \
            && ok=1 && break
        sleep 0.1
    done
    # A move renumbers ALL sessions densely 0..n — a null anywhere means
    # the reorder never persisted.
    [ -n "$ok" ] || e2e_die "null display_order after reorder: $(
        printf '%s' "$list" | jq -c 'map([.name, .display_order])')" || return 1
    [ "$(printf '%s' "$list" | jq 'length')" = "4" ] \
        || e2e_die "expected 4 sessions, got: $(printf '%s' "$list" | jq -c 'map(.name)')" \
        || return 1
    # The persisted order is the one on screen: the moved session sits
    # below order-b within the ws group.
    local a b
    a="$(printf '%s' "$list" | jq -r --arg n "$E2E_SCENARIO_NAME" \
        'map(select(.name == $n))[0].display_order')"
    b="$(printf '%s' "$list" | jq -r 'map(select(.name == "order-b"))[0].display_order')"
    [ "$a" -gt "$b" ] 2>/dev/null \
        || e2e_die "display_order does not reflect the move: $E2E_SCENARIO_NAME=$a order-b=$b"
}

scenario_assert_ui() {
    assert_pane_contains "── ws "
    assert_pane_contains "── ws-b"
    # The final J is still on screen: order-b above the moved session.
    e2e_wait_row_above "order-b" "$E2E_SCENARIO_NAME"
}
