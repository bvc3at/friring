# shellcheck shell=bash
#
# Scenario: the new-session wizard's Esc back-navigation and the always-type
# repo palette's path mode. From an empty DB (no pre-create), the steps first
# prove Esc on the wizard's first step cancels the whole flow, then drive the
# real one: typing an absolute path flips the palette into path mode with live
# directory candidates (git repos marked "(repo)"), Tab completes the unique
# candidate into the input, and Enter commits the typed path — advancing to
# the name step prefilled from the repo. Esc there goes BACK to the palette
# with the produced state preserved (the committed repo as a checked, picked
# bookmark row), and a second Enter re-submits that pick without retyping
# anything. Asserts the spawned session's cwd is the repo itself with no
# worktree indirection.
#
# Test-mode only (polls the pane for a marker to disappear); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Wizard Esc back-navigation; path-mode repo palette with Tab completion"
SCENARIO_AGENT="scripted"
# The wizard creates the session — the harness must not pre-create one.
SCENARIO_PRECREATE=0
SCENARIO_SESSION_NAME="wiz-backnav"

# Bounded poll for a marker to LEAVE the pane. step_wait_pane can only wait
# for presence, and "the wizard closed" has no positive marker of its own —
# the sidebar's empty-state text stays visible beside the modal the whole
# time, so waiting for it again would prove nothing.
backnav_wait_pane_gone() {
    local needle="$1" tries="$2"
    for _ in $(seq 1 "$tries"); do
        e2e_pane | grep -qF -- "$needle" || return 0
        sleep 0.1
    done
    e2e_die "pane still shows: $needle"
}

scenario_steps() {
    # emit-tape runs steps with nothing booted; the fallback keeps the tape
    # generator deterministic offline (mirrors claude-named-workspace).
    local root="${TBX_SANDBOX_ROOT:-/tmp/friring-e2e}"

    # Empty DB: the sidebar shows the first-run hint, no adopted session.
    step_wait_pane "No sessions yet" 30
    step_wait_pane "Press Ctrl+N" 15

    # Cancel path: Esc on the wizard's first step abandons the whole flow.
    step_key C-n
    step_wait_pane "New Session — Repo" 30
    step_key Escape
    backnav_wait_pane_gone "New Session — Repo" 50

    # Real flow: an absolute-path lead flips the always-focused input into
    # path mode — live directory candidates replace the bookmark list, and
    # the git-repo candidate carries the "(repo)" marker.
    step_key C-n
    step_wait_pane "New Session — Repo" 30
    step_wait_pane "Filter or path" 15
    step_type "$root/w"
    step_wait_pane "Directories (1" 15
    step_wait_pane "ws/ (repo)" 15

    # Tab completes the unique candidate and descends (trailing slash): the
    # list now shows ws's children (none — .git is hidden), and the extended
    # input's tail stays visible in the field.
    step_key Tab
    step_wait_pane "Directories (0" 15
    step_wait_pane "No matching directories" 15
    assert_pane_contains "ws/"

    # Enter with no highlighted candidate commits the typed path (the footer's
    # "Enter open / drill in" acts on a *highlighted* row; the typed path is
    # the target at no highlight): the repo is bookmarked + selected and the
    # flow advances to the name step, prefilled from the repo basename.
    step_key Enter
    step_wait_pane "New Session — Name" 30

    # Esc steps BACK, restoring the parked palette. The raw typed text is
    # cleared by design when a path commits — the preserved state is the
    # selection it produced: the repo as a checked row, still counted picked.
    step_key Escape
    step_wait_pane "New Session — Repo" 30
    step_wait_pane "1 picked" 15
    assert_pane_contains "[x] ws"

    # Enter re-submits the preserved pick — same name step, nothing retyped.
    # Single configured agent, so the agent picker is skipped and the session
    # spawns straight into the scripted agent.
    step_key Enter
    step_wait_pane "New Session — Name" 30
    step_key C-u
    step_type "$SCENARIO_SESSION_NAME"
    step_key Enter
    step_wait_pane "SCRIPTED-READY mode=new" 60
    step_wait_pane "name=$SCENARIO_SESSION_NAME" 15
    step_resolve_session "$SCENARIO_SESSION_NAME" 30
}

scenario_assert_effects() {
    # The wizard-created session runs IN the picked repo: cwd is the repo
    # itself (trailing-slash input normalized away) and no worktree rows.
    local json cwd n
    json="$(friring-cli --json session get "$E2E_SESSION_ID")"
    cwd="$(printf '%s' "$json" | jq -r '.cwd')"
    [ "$cwd" = "$E2E_WS" ] || e2e_die "session cwd '$cwd' != '$E2E_WS'" || return 1
    n="$(printf '%s' "$json" | jq -r '.worktrees | length')"
    [ "$n" = "0" ] || e2e_die "expected no worktrees, got $n"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_SESSION_NAME  ◐"
    assert_pane_contains " scripted ["
}
