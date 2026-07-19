# shellcheck shell=bash
#
# Scenario: the new-session wizard's worktree flow (#11/#17), end to end and
# entirely through keystrokes — no headless create. Ctrl+T in the repo
# palette marks the highlighted repo for worktree mode (modal-scoped: it must
# NOT toggle the shell pane, and it auto-checks the repo — observed as the
# `[wt]` row suffix plus the "1 picked" footer). Enter then routes through
# the base-branch selector (fed off-thread, ADR-P12), the name step, and the
# branch step whose prefill derives from the session name; the final Enter
# creates the worktree and spawns the scripted agent inside it. Asserts the
# persisted worktree row (branch, path under the app's worktrees dir, cwd),
# the real git state of the created worktree, and the worktree UI marks.
#
# Ctrl+T acts on palette *rows* (bookmarks), never on path-mode completion
# candidates — so the seed repo is first imported via its parent folder
# (Ctrl+P), which rebuilds the rows as header(root) + ws.
#
# Test-mode only (asserts via friring-cli/git probes); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Wizard worktree flow: Ctrl+T mark, base-branch pick, derived branch name, spawn into the worktree"
SCENARIO_AGENT="scripted"
# The wizard creates the session — the harness must not pre-create one.
SCENARIO_PRECREATE=0
SCENARIO_SESSION_NAME="wiz-wt"

scenario_steps() {
    # emit-tape runs steps with nothing booted; the fallback keeps the tape
    # generator deterministic offline.
    local root="${TBX_SANDBOX_ROOT:-/tmp/friring-e2e}"

    # The TUI booted empty — the wizard is the only thing under test.
    step_wait_pane "No sessions yet" 30
    step_key C-n
    step_wait_pane "New Session — Repo" 30

    # Import the sandbox root as a parent so the seed repo becomes a palette
    # row ($E2E_WS is its only child git repo). "forget" only renders in the
    # filter-mode empty-input footer — the import cleared the typed path and
    # rebuilt the rows: header(root), ws, "start in ~".
    step_type "$root"
    step_key C-p
    step_wait_pane "forget" 30

    # Highlight resets to the header; one Down lands on the ws repo row.
    # Ctrl+T flags it for worktree mode AND auto-checks it: the row gains the
    # [wt] suffix and the footer flips to "open 1 picked".
    step_key Down
    step_key C-t
    step_wait_pane "\[wt\]" 15
    step_wait_pane "1 picked" 15
    step_key Enter

    # A worktree-flagged pick routes to the base-branch selector instead of
    # spawning. Enter is inert while "Loading branches…" shows, so wait for
    # the sole local branch row (main — the seed repo has no origin) before
    # accepting the default selection.
    step_wait_pane "New Session — Base Branch (ws)" 30
    step_wait_pane "main" 30
    step_key Enter

    # Name step, prefilled from the repo basename — replace it, since the
    # branch prefill on the next step derives from this name.
    step_wait_pane "New Session — Name" 30
    step_key C-u
    step_type "$SCENARIO_SESSION_NAME"
    step_key Enter

    # Branch step: title carries the picked base, input prefilled with the
    # name-derived branch (wiz-wt). Enter accepts it and spawns (the status
    # row may flash "Creating worktree(s)…"; the single configured agent
    # skips the picker).
    step_wait_pane "Branch (from main)" 30
    step_wait_pane "wiz-wt" 15
    step_key Enter

    # The scripted agent boots inside the new worktree with the wizard's
    # name expanded into its argv template.
    step_wait_pane "SCRIPTED-READY mode=new" 60
    step_wait_pane "name=$SCENARIO_SESSION_NAME" 15
    step_resolve_session "$SCENARIO_SESSION_NAME" 30
}

scenario_assert_effects() {
    local sess wt branch cwd
    sess="$(friring-cli --json session get "$E2E_SESSION_ID")"
    wt="$(printf '%s' "$sess" | jq -r '.worktrees[0].worktree_path // empty')"
    branch="$(printf '%s' "$sess" | jq -r '.worktrees[0].branch // empty')"
    cwd="$(printf '%s' "$sess" | jq -r '.cwd // empty')"
    [ -n "$wt" ] || e2e_die "session persisted no worktree" || return 1
    [ "$branch" = "$SCENARIO_SESSION_NAME" ] \
        || e2e_die "persisted branch '$branch' != $SCENARIO_SESSION_NAME" || return 1

    # The app owns worktree placement: outside the checkout, under its
    # per-repo-hash worktrees dir in the data dir.
    case "$wt" in
        "$XDG_DATA_HOME/friring-dev/worktrees/"*) ;;
        *) e2e_die "worktree '$wt' not under $XDG_DATA_HOME/friring-dev/worktrees/"
           return 1 ;;
    esac

    # The worktree is real git state: it exists and has the new branch
    # checked out (forked from main by `git worktree add -b`).
    [ -d "$wt" ] || e2e_die "worktree dir missing: $wt" || return 1
    local got
    got="$(git -C "$wt" branch --show-current)"
    [ "$got" = "$SCENARIO_SESSION_NAME" ] \
        || e2e_die "worktree branch '$got' != $SCENARIO_SESSION_NAME" || return 1

    # The session runs IN the worktree — its persisted cwd is the worktree
    # path, not the repo checkout.
    [ "$cwd" = "$wt" ] || e2e_die "session cwd '$cwd' != worktree '$wt'" || return 1

    # The wizard left the main checkout untouched.
    [ -z "$(git -C "$E2E_WS" status --porcelain)" ] \
        || e2e_die "main checkout dirty: $(git -C "$E2E_WS" status --porcelain)"
}

scenario_assert_ui() {
    # Terminal pane title carries the worktree branch; the sidebar row wears
    # the worktree glyph (U+2442, subordinate to the status dot).
    assert_pane_contains "[wiz-wt]"
    assert_pane_contains "⑂ "
}
