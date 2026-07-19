# shellcheck shell=bash
#
# Scenario: git worktree integration (#17), end to end. A session created on
# --worktree-branch must run the agent in an isolated worktree OUTSIDE the
# repo checkout, leaving the main checkout untouched. The session is created
# headlessly by friring-cli while the TUI is already running, which also
# proves external-create adoption: the TUI boots empty ("No sessions yet"),
# the row appears via the external-change poll, and the sidebar/title paint
# the worktree marks. The real Claude Code binary then proves *where* it
# runs: a Bash tool call records `pwd` and the checked-out branch into files
# that the asserts canonicalize and compare against the worktree path from
# `session get` (macOS /var vs /private/var).
#
# Test-mode only (creates the session via friring-cli mid-steps); not
# demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Worktree session: claude works on a fresh branch outside the checkout, main stays clean"
SCENARIO_AGENT="claude"
# The steps create the session headlessly after the TUI has booted empty —
# the harness must not pre-create one.
SCENARIO_PRECREATE=0
SCENARIO_PROMPT="prove your workspace location"
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="WORKTREE-PROOF-DONE"
SCENARIO_SESSION_NAME="wt-e2e"
SCENARIO_WT_BRANCH="feat/e2e"

scenario_setup() {
    # The agent launches inside the not-yet-created worktree, so claude's
    # folder-trust entry must cover it or interactive mode stops at the trust
    # dialog. Predict the app's path: <data dir>/worktrees/<repo-hash>/
    # <branch with / -> ->, where <repo-hash> is FNV-1a 64 of the repo path
    # string — replicated from src/git/mod.rs stable_repo_hash ($E2E_WS is
    # already canonical, so it hashes byte-identically to what the app sees).
    local hash
    hash="$(node -e '
        let h = 14695981039346656037n;
        for (const b of Buffer.from(process.argv[1])) {
            h ^= BigInt(b);
            h = (h * 1099511628211n) & 0xffffffffffffffffn;
        }
        process.stdout.write(h.toString(16).padStart(16, "0"));
    ' "$E2E_WS")"
    E2E_WT_PREDICTED="$XDG_DATA_HOME/friring-dev/worktrees/$hash/feat-e2e"
    SCENARIO_TRUST_DIRS=("$E2E_WT_PREDICTED")
}

scenario_steps() {
    # External-create adoption: the TUI is up and provably empty BEFORE the
    # CLI creates the session, so the row appearing can only be the
    # external-change poll at work.
    step_wait_pane "No sessions yet" 30
    # 3>&- : same bats fd-3 guard as the harness's own session create.
    friring-cli --json session create --name "$SCENARIO_SESSION_NAME" \
        --repo-path "$E2E_WS" --agent claude \
        --worktree-branch "$SCENARIO_WT_BRANCH" --base-branch main \
        >/dev/null 3>&- \
        || e2e_die "headless worktree session create failed" || return 1
    step_resolve_session "$SCENARIO_SESSION_NAME" 15

    # The adopted row, the branch in the terminal title, and the sidebar's
    # worktree mark (U+2442, subordinate to the status dot).
    step_wait_pane "$SCENARIO_SESSION_NAME" 15
    step_wait_pane "\[feat/e2e\]" 15
    step_wait_pane "⑂" 15

    # An externally-created session is adopted with the session LIST focused
    # (unlike a pre-boot create, which boots into Terminal focus) — Esc hands
    # focus to the terminal so the prompt reaches the agent PTY, not the UI.
    step_key Escape

    # Ready glyph appearing (instead of a trust dialog) proves the predicted
    # trust path matched the worktree the app actually created.
    step_wait_pane "$SCENARIO_AGENT_READY" 120
    step_type "$SCENARIO_PROMPT"
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
}

# `cd && pwd -P` canonicalization: on macOS the agent resolves its cwd to
# /private/var/… while the harness paths say /var/… — string equality only
# holds after both sides are physically resolved.
_wt_canon() { (cd "$1" 2>/dev/null && pwd -P); }

scenario_assert_effects() {
    local sess wt branch
    sess="$(friring-cli --json session get "$E2E_SESSION_ID")"
    wt="$(printf '%s' "$sess" | jq -r '.worktrees[0].worktree_path // empty')"
    branch="$(printf '%s' "$sess" | jq -r '.worktrees[0].branch // empty')"
    [ -n "$wt" ] || e2e_die "session persisted no worktree" || return 1
    [ "$branch" = "$SCENARIO_WT_BRANCH" ] \
        || e2e_die "persisted branch '$branch' != $SCENARIO_WT_BRANCH" || return 1

    # Isolation: the worktree lives OUTSIDE the repo checkout.
    case "$wt" in
        "$E2E_WS"|"$E2E_WS"/*)
            e2e_die "worktree '$wt' is inside the repo checkout $E2E_WS"
            return 1 ;;
    esac

    # The agent's own pwd, recorded from inside the pane, must be the
    # persisted worktree path (canonicalized on both sides).
    [ -f "$wt/pwd-proof.txt" ] \
        || e2e_die "pwd-proof.txt missing in worktree $wt" || return 1
    local got want
    got="$(_wt_canon "$(cat "$wt/pwd-proof.txt")")"
    want="$(_wt_canon "$wt")"
    [ -n "$want" ] || e2e_die "worktree dir not resolvable: $wt" || return 1
    [ "$got" = "$want" ] \
        || e2e_die "agent pwd '$got' != worktree '$want'" || return 1
    [ "$(cat "$wt/branch-proof.txt")" = "$SCENARIO_WT_BRANCH" ] \
        || e2e_die "agent branch '$(cat "$wt/branch-proof.txt")' != $SCENARIO_WT_BRANCH" \
        || return 1

    # The main checkout stayed clean: no proof files leaked into it and git
    # sees no working-tree changes at all.
    [ ! -e "$E2E_WS/pwd-proof.txt" ] \
        || e2e_die "pwd-proof.txt leaked into the main checkout" || return 1
    [ -z "$(git -C "$E2E_WS" status --porcelain)" ] \
        || e2e_die "main checkout dirty: $(git -C "$E2E_WS" status --porcelain)" \
        || return 1

    [ "$(journal_matched worktree-proof)" -ge 1 ] \
        || e2e_die "worktree-proof fixture never matched"
    [ "$(journal_matched worktree-done)" -ge 1 ] \
        || e2e_die "worktree-done fixture never matched (tool_result never posted)"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
