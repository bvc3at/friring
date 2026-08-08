# shellcheck shell=bash
#
# Scenario: the global-search popup (Ctrl+/), every scope at once. A task,
# a disabled automation, a seeded file, and a *live terminal buffer* marker
# form the corpus; one "flux" query must surface the TASKS / AUTOMATIONS /
# FILES groups (the file index is built off-thread after open, so its group
# may arrive last), and a second query must find the scenario session by
# buffer **content** — text the agent echoed into its PTY, never persisted
# anywhere — via the debounced content scan. Esc restores the pre-search
# snapshot; Enter on the session result closes the popup and lands Terminal
# focus on the session (a self-jump: focus-wise a no-op, but the popup must
# still tear down).
#
# Demo-able: the mid-step friring-cli calls are one-shot seeding, so in demo
# mode they simply land before the first frame; Ctrl+/ records through
# `<leader> /` (SCENARIO_DEMO_KEYS).
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Global search across live session content, tasks, automations, and files; Esc restore + Enter jump"
SCENARIO_AGENT="scripted"
# `<leader> /` is global search's own second route, and the one that films:
# tmux spells `Ctrl+/` as the unreadable `C-_`, while the overlay names the
# search.
SCENARIO_DEMO_KEYS=("C-_=C-f /")

# Bounded poll for the popup being GONE — step_wait_pane can only wait for
# presence, and both the Esc close and the Enter jump are proven by the
# " Search " title disappearing (nothing else in this scenario renders it).
gs_wait_popup_gone() {
    local tries="$1"
    for _ in $(seq 1 "$tries"); do
        e2e_pane | grep -qF -- " Search " || return 0
        sleep 0.2
    done
    e2e_die "global-search popup never closed
--- pane ---
$(e2e_pane)"
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # A marker that exists ONLY in the live vt100 buffer (the scripted agent
    # echoes stdin as GOT:) — nothing on disk or in the DB carries it, so a
    # later hit proves the content scan reads the running PTY.
    step_type "SEARCHME-XYZ"
    step_key Enter
    step_wait_pane "GOT:SEARCHME-XYZ" 15

    # Corpus rows via a second CLI process against the shared DB. The task
    # first: the TUI refreshes the task and automation caches in the same
    # tick, so the automation count reaching the sidebar footer below also
    # proves the task cache (which search reads) includes our row. Disabled
    # automation: it must exist for search without ever firing. 3>&- guards
    # the heartbeat-arming tmux call (bats fd-3 hang).
    local out
    out="$(friring-cli --json task create --title "Find the flux capacitor")" \
        || e2e_die "task create failed: $out" || return 1
    out="$(friring-cli --json automation create --name nightly-flux \
        --trigger daily --prompt "n" --session "$E2E_SESSION_ID" --disabled 3>&-)" \
        || e2e_die "automation create failed: $out" || return 1
    E2E_GS_AUTO_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_GS_AUTO_ID" ] && [ "$E2E_GS_AUTO_ID" != "null" ] \
        || e2e_die "no automation id in: $out" || return 1
    # Cache-sync marker: the Automations pane row's summary. The footer badge
    # is useless here — it counts only *enabled* automations, and ours is
    # deliberately disabled.
    step_wait_pane "daily 00:00" 15

    # Open the popup (Ctrl+/ arrives as legacy C-_; global even from
    # Terminal focus) — empty query shows the scopes hint.
    step_key C-_
    step_wait_pane " Search " 15
    step_wait_pane "type to search sessions" 15

    # One query, three scopes. Tasks/automations match from the caches at
    # once; the FILES group depends on the off-thread index walk delivering
    # after open, hence the generous wait.
    step_type "flux"
    step_wait_pane " TASKS" 15
    step_wait_pane "Find the flux capacitor" 15
    step_wait_pane " AUTOMATIONS" 15
    step_wait_pane "nightly-flux" 15
    step_wait_pane " FILES" 15
    step_wait_pane "flux_notes" 15

    # Esc = cancel: the snapshot restore must tear the popup down.
    step_key Escape
    gs_wait_popup_gone 50 || return 1

    # Reopen fresh (query cleared) and find the session by live buffer
    # content — the SESSIONS group only renders when a session result
    # exists, and only the ~150ms-debounced content scan can produce one
    # ("SEARCHME-XYZ" matches no session metadata).
    step_key C-_
    step_wait_pane "type to search sessions" 15
    step_type "SEARCHME-XYZ"
    step_wait_pane " SESSIONS" 15

    # Enter = jump to the (only) result: our own session. The focus change
    # is a no-op, but the popup must close and Terminal focus must hold.
    step_key Enter
    gs_wait_popup_gone 50 || return 1
}

scenario_assert_effects() {
    # The disabled automation was searchable but never fired.
    local runs
    runs="$(friring-cli --json automation runs "$E2E_GS_AUTO_ID" | jq 'length')"
    [ "$runs" = "0" ] \
        || e2e_die "disabled automation recorded $runs run(s), want 0"
}

scenario_assert_ui() {
    # The jump committed: popup gone, terminal pane titled with the session.
    e2e_pane | grep -qF -- " Search " \
        && { e2e_die "global-search popup still open after Enter
--- pane ---
$(e2e_pane)"; return 1; }
    # The jumped-to session is the active one (header badge — the pane title
    # carries the agent, not the name) and its terminal pane renders.
    assert_pane_contains "$E2E_SCENARIO_NAME  ◐"
    assert_pane_contains " scripted ["
}
