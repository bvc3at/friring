# shellcheck shell=bash
#
# Scenario: the REAL blocked path of the hook-driven live status. Claude runs
# WITHOUT --dangerously-skip-permissions (SCENARIO_CLAUDE_PERMISSIONS=default,
# read by the profile at boot), so the stub's Bash tool_use raises claude's
# own permission dialog; its Notification hook (payload contains
# "permission") signals blocked into the session DB. Approving the dialog
# lets the real `touch` run, the tool_result round-trips to the stub, and
# Stop signals done. This is the only e2e of the blocked edge with a real
# permission dialog — the scripted variants only fake the signal via the CLI.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Real claude permission dialog -> blocked signal; approval resumes to done"
SCENARIO_AGENT="claude"
SCENARIO_CLAUDE_PERMISSIONS="default"
SCENARIO_PROMPT="Run the blocked-proof command with Bash."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="BLOCKED-FLOW-DONE"

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_sleep 1
    step_type "$SCENARIO_PROMPT"
    step_sleep 1
    step_key Enter
    # The dialog itself is the sync point for the blocked edge: claude renders
    # its Bash permission prompt, then the Notification hook flips the DB row.
    # "Do you want" is the dialog's question line — the tool name/command also
    # appear, but those echo the prompt text already on screen.
    step_wait_pane "Do you want" 60
    step_wait_state 'blocked' 60
    # Approve: option 1 ("Yes") is pre-selected; Enter confirms it. The tool
    # then really runs (PreToolUse -> working), the tool_result reaches the
    # stub, and its follow-up text lands in the pane before Stop -> done.
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast tail can
    # flip working->done between polls; done implies the tool turn ran.
    step_wait_state 'working|done' 60
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60
    step_sleep 2
}

scenario_assert_effects() {
    # `touch` proves the approved Bash call really executed (content empty by
    # design — existence is the effect).
    [ -f "$E2E_WS/blocked-proof.txt" ] \
        || e2e_die "blocked-proof.txt missing — approved Bash call never ran" || return 1
    [ "$(journal_matched bash-tool-call)" -ge 1 ] \
        || e2e_die "bash-tool-call fixture never matched"
    [ "$(journal_matched after-approval)" -ge 1 ] \
        || e2e_die "after-approval fixture never matched (tool_result never posted)"
}

scenario_assert_ui() {
    assert_pane_contains "$SCENARIO_DONE_PATTERN"
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done"
}
