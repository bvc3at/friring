# shellcheck shell=bash
#
# Scenario: worktree sync conflict handoff (#19). Ctrl+S on a worktree whose
# local commit collides with a moved origin/main must not strand the user in
# a half-done rebase: the rebase aborts cleanly (HEAD still the local commit,
# no rebase state, no leaked stash) and recovery is delegated to the
# session's agent — the conflict prompt is bracketed-pasted into the agent
# pane. The scripted agent proves the delegation end to end: its GOT: echo
# is the prompt arriving on the agent's *stdin*, not just terminal key echo.
# A bare repo beside the sandbox plays origin and a colleague clone pushes
# the conflicting upstream change (file transport — offline under the dead
# proxies).
#
# Test-mode only (creates the session via friring-cli mid-steps and commits
# in the worktree with git); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Ctrl+S conflict: rebase aborts cleanly, recovery prompt handed to the agent"
SCENARIO_AGENT="scripted"
# The steps create the worktree session headlessly once the TUI is up.
SCENARIO_PRECREATE=0
SCENARIO_SESSION_NAME="sync-conflict"
SCENARIO_WT_BRANCH="feat/conflict"

scenario_setup() {
    # Bare origin cloned from the seed repo, then a colleague clone rewrites
    # the seeded conflict.txt line on main and pushes — so the worktree's own
    # commit to that line can only rebase with a conflict.
    local origin="$TBX_SANDBOX_ROOT/origin.git"
    git clone -q --bare "$E2E_WS" "$origin"
    git -C "$E2E_WS" remote add origin "$origin"
    local colleague="$TBX_SANDBOX_ROOT/colleague"
    git clone -q "$origin" "$colleague"
    printf 'upstream version\n' > "$colleague/conflict.txt"
    git -C "$colleague" commit -qam "upstream-change"
    git -C "$colleague" push -q origin main
}

scenario_steps() {
    # TUI provably up (and empty) before the external create, so adoption is
    # the external-change poll at work.
    step_wait_pane "No sessions yet" 30
    # 3>&- : same bats fd-3 guard as the harness's own session create.
    friring-cli --json session create --name "$SCENARIO_SESSION_NAME" \
        --repo-path "$E2E_WS" --agent scripted \
        --worktree-branch "$SCENARIO_WT_BRANCH" --base-branch main \
        >/dev/null 3>&- \
        || e2e_die "headless worktree session create failed" || return 1
    step_resolve_session "$SCENARIO_SESSION_NAME" 15
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # The conflicting local commit is made IN THE WORKTREE (same line the
    # colleague already rewrote upstream), where the sync will run.
    local wt
    wt="$(friring-cli --json session get "$E2E_SESSION_ID" \
        | jq -r '.worktrees[0].worktree_path // empty')"
    [ -n "$wt" ] || e2e_die "session persisted no worktree" || return 1
    printf 'local version\n' > "$wt/conflict.txt"
    git -C "$wt" add conflict.txt \
        && git -C "$wt" commit -qm "local-change" \
        || e2e_die "local conflicting commit failed" || return 1

    # An externally-created session is adopted with the session LIST focused
    # (empirical; claude-worktree-session leans on the same fact), which is
    # exactly where C-s means StartSync — from Terminal focus it would be the
    # XOFF passthrough byte. Do NOT press C-h first: it toggles focus INTO
    # the terminal here. The handoff toast and the agent's GOT: echo of the
    # pasted prompt prove the conflict was delegated, not swallowed.
    step_key C-s
    step_wait_pane "conflict(s) (sent to Claude)" 60
    step_wait_pane "GOT:" 30
    # The prompt is one long line the pane wraps: grep a short leading
    # fragment that cannot straddle a wrap boundary.
    step_wait_pane "Please sync this worktree" 15
}

scenario_assert_effects() {
    local wt
    wt="$(friring-cli --json session get "$E2E_SESSION_ID" \
        | jq -r '.worktrees[0].worktree_path // empty')"
    [ -n "$wt" ] || e2e_die "session persisted no worktree" || return 1

    # Aborted cleanly: no rebase in progress, HEAD still the local commit,
    # nothing left on the stash, and the working tree kept the local content
    # (no half-applied upstream hunks).
    ! git -C "$wt" status | grep -qi rebase \
        || e2e_die "rebase left in progress: $(git -C "$wt" status | head -5)" || return 1
    local head_subject
    head_subject="$(git -C "$wt" log -1 --format=%s)"
    [ "$head_subject" = "local-change" ] \
        || e2e_die "HEAD is '$head_subject', want local-change" || return 1
    [ -z "$(git -C "$wt" stash list)" ] \
        || e2e_die "stash leaked: $(git -C "$wt" stash list)" || return 1
    [ "$(cat "$wt/conflict.txt")" = "local version" ] \
        || e2e_die "conflict.txt is '$(cat "$wt/conflict.txt")', want 'local version'" || return 1
}

scenario_assert_ui() {
    # GOT: still on screen = the agent received the recovery prompt (the
    # toast itself has a 5s TTL, so it is waited for in the steps, not here).
    assert_pane_contains "GOT:"
    assert_pane_contains "Please sync this worktree"
}
