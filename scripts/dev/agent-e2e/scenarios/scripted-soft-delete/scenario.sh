# shellcheck shell=bash
#
# Scenario: soft delete with undo (#15). Ctrl+D on the session list
# soft-deletes the active session — the sidebar row vanishes but the DB
# keeps the row — Ctrl+Z undoes it in place, and a second delete is brought
# back through the Ctrl+U "Restore Deleted Sessions" modal (Enter restores
# directly; a confirm modal only guards force-deleted rows). Every
# "row gone / row back" signal is an id-keyed `friring-cli session list`
# poll, never pane text: the delete toast itself contains the session name,
# so the name on screen can't distinguish "deleted" from "toasted". A
# keeper session created via the CLI guarantees the list never goes empty.
# The final asserts prove identity: the same session id resolves to the
# same row after two delete/restore cycles (soft delete preserved it), and
# the restored session renders as the active terminal pane.
#
# Test-mode only (extracts ids via friring-cli mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Soft delete (Ctrl+D), Ctrl+Z undo, Ctrl+U restore list — DB row preserved across cycles"
SCENARIO_AGENT="scripted"

# Bounded polls of the CLI list for a session id. `session list` excludes
# soft-deleted rows, so presence/absence of the *id* is the ground truth the
# pane can never give (the toast echoes the session name).
del_wait_gone() {
    local sid="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        friring-cli --json session list 2>/dev/null \
            | jq -e --arg id "$sid" 'all(.[]; .id != $id)' >/dev/null && return 0
        sleep 0.2
    done
    e2e_die "session $sid never left the session list"
}

del_wait_listed() {
    local sid="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        friring-cli --json session list 2>/dev/null \
            | jq -e --arg id "$sid" 'any(.[]; .id == $id)' >/dev/null && return 0
        sleep 0.2
    done
    e2e_die "session $sid never (re)appeared in the session list"
}

# Bounded wait for the restore modal to be GONE. A soft-delete restore closes
# the modal itself; asserting its absence (rather than sending a key that
# would swallow a stuck-open modal) is what actually catches that regression.
del_wait_modal_gone() {
    local tries="$1"
    for _ in $(seq 1 "$tries"); do
        e2e_pane | grep -q "Restore Deleted Sessions" || return 0
        sleep 0.2
    done
    e2e_die "restore modal stayed open after Enter (should close itself on restore)"
}

scenario_steps() {
    # Session A (the precreated $E2E_SCENARIO_NAME) is adopted and ready.
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Keeper session so the list never goes empty (last-session semantics are
    # not under test). 3>&- guards the tmux-window-spawning create (bats fd-3
    # hang), like the harness's own create.
    local out
    out="$(friring-cli --json session create --name del-keeper \
        --repo-path "$E2E_WS" --agent scripted 3>&-)" \
        || e2e_die "session create del-keeper failed: $out" || return 1
    E2E_DEL_KEEPER_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_DEL_KEEPER_ID" ] && [ "$E2E_DEL_KEEPER_ID" != "null" ] \
        || e2e_die "no id for del-keeper in: $out" || return 1
    step_wait_pane "del-keeper" 15

    # Cycle 1 — delete + undo. Ctrl+D is terminal-passthrough, so leave the
    # terminal for the session list first (Ctrl+H); the delete targets the
    # ACTIVE session, which is still A.
    step_key C-h
    step_key C-d
    step_wait_pane "Ctrl+Z to undo" 15
    del_wait_gone "$E2E_SESSION_ID" 50

    # Ctrl+Z is global; the undo re-adds the same row and makes it active.
    step_key C-z
    step_wait_pane "Restored" 15
    del_wait_listed "$E2E_SESSION_ID" 50

    # Cycle 2 — delete + restore via the modal. Focus is STILL the session
    # list (neither delete nor undo moves it), and Ctrl+H is a focus *cycle*,
    # not idempotent — pressing it again would leave the list. The undo made
    # A active again, so Ctrl+D hits the same session.
    step_key C-d
    step_wait_pane "Ctrl+Z to undo" 15
    del_wait_gone "$E2E_SESSION_ID" 50

    # Ctrl+U (list-scoped: passthrough while the terminal is focused) opens
    # the restore modal; the soft-deleted row restores directly on Enter —
    # only force-deleted rows get a confirm modal.
    step_key C-u
    step_wait_pane "Restore Deleted Sessions" 15
    step_wait_pane "$E2E_SCENARIO_NAME (scripted)" 15
    step_key Enter
    step_wait_pane "Restored" 15
    del_wait_listed "$E2E_SESSION_ID" 50
    # The restore must close the modal on its own — assert its absence rather
    # than pressing a key that would mask a stuck-open modal (a real
    # regression this scenario is meant to catch).
    del_wait_modal_gone 30

    # The restored session is active with Terminal focus (restore sets both),
    # so the header badge names it again and its pane title renders — the
    # session still has a live terminal.
    step_wait_pane "$E2E_SCENARIO_NAME  ◐" 15
    step_wait_pane " scripted \[" 15
}

scenario_assert_effects() {
    # Identity: the same id still resolves to the same row — two soft
    # delete/restore cycles never re-created the session under a new id.
    local got
    got="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.name')"
    [ "$got" = "$E2E_SCENARIO_NAME" ] \
        || e2e_die "session $E2E_SESSION_ID resolves to '$got', want '$E2E_SCENARIO_NAME'" \
        || return 1
    # Final list: exactly the two live sessions, both by their original ids.
    local n
    n="$(friring-cli --json session list | jq 'length')"
    [ "$n" = "2" ] || e2e_die "final session list has $n rows, want 2" || return 1
    friring-cli --json session list \
        | jq -e --arg id "$E2E_SESSION_ID" 'any(.[]; .id == $id)' >/dev/null \
        || e2e_die "restored session $E2E_SESSION_ID missing from final list" || return 1
    friring-cli --json session list \
        | jq -e --arg id "$E2E_DEL_KEEPER_ID" 'any(.[]; .id == $id)' >/dev/null \
        || e2e_die "keeper session $E2E_DEL_KEEPER_ID missing from final list"
}

scenario_assert_ui() {
    # The restored session is the active one (header badge) and its terminal
    # pane renders (title).
    assert_pane_contains "$E2E_SCENARIO_NAME  ◐"
    assert_pane_contains " scripted ["
}
