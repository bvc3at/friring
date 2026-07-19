# shellcheck shell=bash
#
# Scenario: the Ctrl+T shell pane — a real $SHELL in a `tbs-` window on the
# agent tmux server, toggled beside the agent view and tracked per session.
# Ctrl+T is deliberately NOT in the terminal-passthrough set, so it must work
# straight from Terminal focus: the pane title flips to ` {name} (shell) `,
# typed input reaches the shell's stdin (proved by an echoed marker — shell
# prompts vary with the harness $SHELL, so only our own marker is stable),
# and the shell/agent view choice is per session: switching to a second
# session (Alt+2) shows *its* agent view, and switching back (Alt+1) must
# land on the shell view again. A final Ctrl+T returns to the agent view
# while the tbs- window survives (the shell is kept for the next toggle,
# not killed).
#
# Test-mode only (probes the friring-dev tmux server mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Ctrl+T shell pane: real shell in a tbs- window, view tracked per session"
SCENARIO_AGENT="scripted"

# Backend-side proof that Ctrl+T spawned a real shell rather than just
# repainting: a `tbs-` window exists on the agent server (the sandbox's
# private friring-dev socket — never a real user server).
shell_window_exists() {
    tmux -L friring-dev list-windows -a -F '#{window_name}' 2>/dev/null \
        | grep -q '^tbs-'
}

scenario_steps() {
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Second session up front so the per-session-tracking leg has somewhere
    # to switch to. 3>&- guards the tmux-window-spawning create (bats fd-3
    # hang); same repo keeps the Alt+N order deterministic (A row 1, peer 2).
    local out
    out="$(friring-cli --json session create --name shell-peer \
        --repo-path "$E2E_WS" --agent scripted 3>&-)" \
        || e2e_die "session create shell-peer failed: $out" || return 1
    step_wait_pane "shell-peer" 15

    # Ctrl+T straight from Terminal focus: the title flips to the shell view
    # and a tbs- window appears on the agent server.
    step_key C-t
    step_wait_pane " scripted-shell-pane (shell) " 30
    shell_window_exists || e2e_die "no tbs- window after Ctrl+T" || return 1

    # A real shell, not a viewer: typing lands on its stdin. The echo OUTPUT
    # line starts at the shell's column 0, which renders flush against the
    # panel's left border — `│MARKER` matches only the output line (the typed
    # line has the prompt and `echo ` before the marker, never the border).
    step_type "echo SHELL-MARKER-E2E"
    step_key Enter
    step_wait_pane "│SHELL-MARKER-E2E" 30

    # Per-session tracking: shell-peer never toggled, so it shows its agent
    # view; back on the first session the shell view must still be selected.
    step_key M-2
    step_wait_pane " shell-peer (scripted)" 15
    step_key M-1
    step_wait_pane " scripted-shell-pane (shell) " 15

    # Toggle back to the agent view (the shell window must survive — see
    # scenario_assert_effects).
    step_key C-t
    step_wait_pane " scripted-shell-pane (scripted)" 15
}

scenario_assert_effects() {
    # The tbs- window outlived the toggle-back: the shell is tracked and
    # kept per session, not killed on every view flip.
    shell_window_exists \
        || e2e_die "tbs- shell window gone after toggling back to the agent view"
}

scenario_assert_ui() {
    assert_pane_contains " scripted-shell-pane (scripted)"
}
