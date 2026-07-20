# shellcheck shell=bash
#
# Scenario: Ctrl+S worktree sync, happy path, fully offline. A local bare
# repo plays `origin`; a "colleague" clone advances origin/main past the
# seed commit the worktree branched from, so origin/main is strictly ahead
# of the worktree until the sync runs. The session is created headlessly
# with a worktree (feat/sync off local main), the worktree is dirtied with
# an uncommitted tracked change, and Ctrl+S (from list focus — C-s is
# terminal-passthrough) must stash → fetch origin → rebase onto origin/main
# → pop. Observed via the success toast and asserted against real git
# state: origin/main became an ancestor of HEAD, the upstream commit's file
# materialized, the dirty change survived the stash round-trip, the stash
# is empty, and the branch is still feat/sync. The single remote is named
# `origin`, so no base-picker modal interrupts the run.
#
# Test-mode only (creates the session and dirties the worktree via
# friring-cli mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Ctrl+S sync: stash, fetch, rebase onto a moved origin/main, pop — real git state"
SCENARIO_AGENT="scripted"
# The steps create the session themselves — it needs worktree flags the
# harness pre-create doesn't pass.
SCENARIO_PRECREATE=0
SCENARIO_SESSION_NAME="sync-e2e"

scenario_setup() {
    # Local bare origin: file transport, so the dead proxies are irrelevant
    # and the sync's own `git fetch origin` works fully offline.
    local bare="$TBX_SANDBOX_ROOT/origin.git"
    git clone -q --bare "$E2E_WS" "$bare"
    git -C "$E2E_WS" remote add origin "$bare"
    # Fetch now, BEFORE the colleague pushes: origin/main in the session
    # repo starts at the seed commit, so only the sync's own fetch can make
    # the advanced upstream state visible — the rebase proves it ran.
    git -C "$E2E_WS" fetch -q origin
    # The colleague moves origin/main one commit ahead of the seed.
    git clone -q "$bare" "$TBX_SANDBOX_ROOT/colleague"
    ( cd "$TBX_SANDBOX_ROOT/colleague" \
        && echo "upstream" > upstream.txt \
        && git add upstream.txt \
        && git commit -qm "upstream-advance" \
        && git push -q origin main )
}

scenario_steps() {
    # Worktree session, created headlessly while the TUI is live: feat/sync
    # branches from LOCAL main (the seed commit), so origin/main is strictly
    # ahead until the sync rebases. 3>&- guards the tmux-window-spawning
    # create (bats fd-3 hang).
    friring-cli --json session create --name "$SCENARIO_SESSION_NAME" \
        --repo-path "$E2E_WS" --agent scripted \
        --worktree-branch feat/sync --base-branch main >/dev/null 3>&- \
        || e2e_die "session create failed" || return 1
    step_resolve_session "$SCENARIO_SESSION_NAME" 30
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # Dirty the worktree with an uncommitted change to a tracked file so the
    # sync's stash → pop leg actually carries something (plain `git stash`
    # ignores untracked files).
    E2E_SYNC_WT="$(friring-cli --json session get "$E2E_SESSION_ID" \
        | jq -r '.worktrees[0].worktree_path // empty')"
    [ -n "$E2E_SYNC_WT" ] || e2e_die "session has no worktree path" || return 1
    echo "local-wip" > "$E2E_SYNC_WT/tracked.txt"

    # C-s is terminal-passthrough, so sync only dispatches from list focus.
    # A CLI-created session never steals focus: the TUI booted with an empty
    # list, so the list is STILL focused (pressing C-h here would cycle
    # backward INTO the terminal and feed C-s to the PTY as XOFF). The footer
    # renders the focus label next to the session count on one line — wait on
    # that pair to pin the assumption before the chord.
    step_wait_pane "Sessions  1 session" 15
    step_key C-s
    # Only the final toast is waitable: the whole local-transport sync
    # (stash, fetch, rebase, pop) finishes in well under a second, so the
    # transient "Syncing 1 worktree(s)..." toast can be replaced before the
    # first 100ms pane poll — and burning a timeout on it would also outlive
    # the 5s TTL of the success toast. Empirically flaky; observed 1-in-3.
    step_wait_pane "1 worktree(s) synced" 30
}

scenario_assert_effects() {
    local wt
    wt="$(friring-cli --json session get "$E2E_SESSION_ID" \
        | jq -r '.worktrees[0].worktree_path // empty')"
    [ -n "$wt" ] || e2e_die "session lost its worktree path" || return 1
    # The rebase really targeted the advanced origin/main: it is now an
    # ancestor of HEAD, and the colleague's file materialized in the tree.
    git -C "$wt" merge-base --is-ancestor origin/main HEAD \
        || e2e_die "origin/main is not an ancestor of HEAD — rebase didn't happen" || return 1
    [ -f "$wt/upstream.txt" ] \
        || e2e_die "upstream.txt missing — worktree not rebased onto origin/main" || return 1
    # The stash round-trip: the dirty change came back, and nothing was left
    # parked in the stash.
    local got
    got="$(cat "$wt/tracked.txt")"
    [ "$got" = "local-wip" ] \
        || e2e_die "dirty change lost across sync: tracked.txt is '$got'" || return 1
    [ -z "$(git -C "$wt" stash list)" ] \
        || e2e_die "stash not empty after sync: $(git -C "$wt" stash list)" || return 1
    # Sync rebases in place — it must never move the worktree off its branch.
    local branch
    branch="$(git -C "$wt" rev-parse --abbrev-ref HEAD)"
    [ "$branch" = "feat/sync" ] \
        || e2e_die "worktree branch changed: '$branch' != 'feat/sync'"
}

scenario_assert_ui() {
    # The toasts were asserted while visible (TTL 5s) in the steps; here only
    # hook-free UI state: the sidebar's worktree glyph and the terminal pane
    # title carrying the worktree branch.
    assert_pane_contains "⑂ "
    assert_pane_contains "[feat/sync]"
}
