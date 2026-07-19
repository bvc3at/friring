# shellcheck shell=bash
#
# Scenario: the fork's optional **named workspace dir** for multi-repo
# sessions, end to end. The steps drive the real new-session wizard (no
# headless pre-create): import two sandbox repos as a parent folder, pick
# both, and on the name step reveal the Ctrl+O workspace-dir field and point
# it at <sandbox>/named-ws. The real Claude Code binary then boots *in that
# directory* and writes a file through one of its member symlinks. Asserts
# the symlink layout, the write landing in the real repo, the persisted
# `workspace_dir` on the session row (CLI probe), and — as a lifecycle
# epilogue — that a force delete removes only the symlink dir, never the
# member repos.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Multi-repo wizard with a named workspace dir; agent works through the symlinks"
SCENARIO_AGENT="claude"
# The wizard creates the session — the harness must not pre-create one.
SCENARIO_PRECREATE=0
SCENARIO_PROMPT="Create hello-workspace.txt in the ws repo using the Write tool."
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="NAMED-WS-DONE"
SCENARIO_SESSION_NAME="named-ws-session"

scenario_setup() {
    # Second member repo beside the harness seed repo ($E2E_WS = <root>/ws),
    # so importing the sandbox root as a parent folder yields exactly two
    # child repos: ws, ws-b (scan order is sorted — the steps rely on it).
    local b="$TBX_SANDBOX_ROOT/ws-b"
    mkdir -p "$b"
    ( cd "$b" && git init -q && git commit -qm "e2e seed b" --allow-empty )
    # The agent launches in the named workspace dir, not $E2E_WS — claude's
    # folder-trust entry must cover it or interactive mode stops at the
    # trust dialog.
    SCENARIO_TRUST_DIRS=("$TBX_SANDBOX_ROOT/named-ws")
}

scenario_steps() {
    # emit-tape runs steps with nothing booted; the fallback keeps the tape
    # generator deterministic offline (a recorded demo always has the root).
    local root="${TBX_SANDBOX_ROOT:-/tmp/friring-e2e}"

    # Wizard: import the sandbox root as a parent, pick both child repos.
    step_key C-n
    step_wait_pane "New Session — Repo" 30
    step_type "$root"
    step_key C-p
    # "forget" only appears in the filter-mode empty-input footer — the
    # import cleared the typed path and rebuilt the rows.
    step_wait_pane "forget" 30
    # Rows after import: header(root), ws, ws-b, "start in ~"; highlight
    # resets to the header, and row-order picking makes ws the primary cwd.
    step_key Down
    step_key Space
    step_wait_pane "1 picked" 30
    step_key Down
    step_key Space
    step_wait_pane "2 picked" 30
    step_key Enter

    # Name step: distinct session name, then the Ctrl+O workspace-dir field.
    step_wait_pane "New Session — Name" 30
    step_key C-u
    step_type "$SCENARIO_SESSION_NAME"
    step_key C-o
    step_wait_pane "Workspace dir" 30
    step_type "$root/named-ws"
    step_key Enter

    # Single configured agent -> the picker is skipped and the session
    # spawns directly, launching claude inside the named workspace dir.
    step_wait_pane "$SCENARIO_SESSION_NAME" 60
    step_resolve_session "$SCENARIO_SESSION_NAME" 30
    step_wait_pane "$SCENARIO_AGENT_READY" 120
    step_sleep 1
    step_type "$SCENARIO_PROMPT"
    step_sleep 1
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
    step_sleep 2
}

scenario_assert_effects() {
    local ws_dir="$TBX_SANDBOX_ROOT/named-ws"
    [ -d "$ws_dir" ] || e2e_die "named workspace dir missing: $ws_dir" || return 1
    # One symlink per member, labeled by repo dir name, pointing at the repo.
    [ "$(readlink "$ws_dir/ws")" = "$E2E_WS" ] \
        || e2e_die "symlink ws -> '$(readlink "$ws_dir/ws")', want $E2E_WS" || return 1
    [ "$(readlink "$ws_dir/ws-b")" = "$TBX_SANDBOX_ROOT/ws-b" ] \
        || e2e_die "symlink ws-b -> '$(readlink "$ws_dir/ws-b")', want $TBX_SANDBOX_ROOT/ws-b" \
        || return 1
    # The Write targeted <named-ws>/ws/…, i.e. went *through* the symlink —
    # the file must have landed in the real seed repo.
    assert_ws_file_eq hello-workspace.txt "Hello from the named workspace!"
    # Persistence: the session row carries the custom dir (restart / shell
    # pane / delete all resolve it from here).
    local got
    got="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.workspace_dir // empty')"
    [ "$got" = "$ws_dir" ] \
        || e2e_die "persisted workspace_dir '$got' != '$ws_dir'" || return 1
    [ "$(journal_matched workspace-write)" -ge 1 ] \
        || e2e_die "workspace-write fixture never matched"
    [ "$(journal_matched after-workspace-write)" -ge 1 ] \
        || e2e_die "after-workspace-write fixture never matched (tool_result never posted)"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done" || return 1

    # Lifecycle epilogue (after all read-only asserts): a force delete must
    # remove the named workspace dir — and ONLY the symlinks, never the
    # member repos or their content.
    local ws_dir="$TBX_SANDBOX_ROOT/named-ws"
    friring-cli --json session delete "$E2E_SESSION_ID" --force >/dev/null \
        || e2e_die "session delete --force failed" || return 1
    [ ! -e "$ws_dir" ] \
        || e2e_die "named workspace dir survived delete: $ws_dir" || return 1
    [ -f "$E2E_WS/hello-workspace.txt" ] \
        || e2e_die "delete destroyed real repo content" || return 1
    [ -d "$TBX_SANDBOX_ROOT/ws-b" ] \
        || e2e_die "delete destroyed member repo ws-b" || return 1
}
