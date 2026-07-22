# shellcheck shell=bash
#
# Scenario: the fork's F9 agent-activity view, end to end. A real Claude Code
# tool-use turn (Write) leaves a transcript on disk at
# CLAUDE_CONFIG_DIR/projects/<slug>/<agent_session_id>.jsonl — the filename
# only matches because the e2e claude registry entry pins `--session-id {id}`.
# F9 then reconstructs the turn from that file alone: the Overview shows the
# provider, the prompt-derived title and a "1 edits" count, Timeline shows the
# Write as an edit row, Files aggregates the touched path, and Esc closes the
# view. Asserts the transcript file exists under the minted id, the workspace
# side effect, and the journal.
#
# Test-mode only (F9 has no VHS mapping; asserts probe friring-cli); not
# demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="F9 activity view reconstructs a claude Write turn from its on-disk transcript"
SCENARIO_AGENT="claude"
SCENARIO_PROMPT="Create activity-proof.txt using the Write tool."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="ACTIVITY-TURN-DONE"
# Timeline/Files rows print the Write's *absolute* file_path head-truncated to
# the panel width; the sandbox tmpdir path is ~100 chars, so a wide pane is
# what keeps the "activity-proof.txt" tail on screen and greppable.
SCENARIO_COLS=220

scenario_steps() {
    step_wait_pane "$SCENARIO_AGENT_READY" 60
    step_type "$SCENARIO_PROMPT"
    step_key Enter
    # 'working|done': hook_state is overwritten in place, so a fast turn can
    # flip working->done between polls; done implies the turn ran.
    step_wait_state 'working|done' 30
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60
    step_wait_state 'done' 60

    # F9 is global from Terminal focus. The transcript scan runs on a ~1s
    # cadence and the open view live-refreshes, so the Overview may render
    # "No activity captured yet." for a beat — the waits absorb that.
    step_key F9
    step_wait_pane " Activity · Overview " 20
    # Redesigned Overview: identity line is "<agent> · <provider-id> · …", the
    # edit count is a stat tile ("✎ 1 edits"), and the prompt-derived title.
    step_wait_pane "claude · claude-code" 20
    step_wait_pane "1 edits" 20
    step_wait_pane "Title: Create activity-proof.txt" 20

    # Navigator digits jump sections (2 Timeline, 4 Files). grep is line-based,
    # so tag.*path proves ONE row carries both the edit tag and the file.
    step_type "2"
    step_wait_pane "edit.*activity-proof.txt" 15
    # The redesigned Timeline groups events under a "▶" prompt turn header, with
    # the turn's actions in a "│" gutter beneath it.
    step_wait_pane "▶" 15
    step_wait_pane "│" 15
    step_type "4"
    step_wait_pane "Edited (1)" 15
    step_wait_pane "activity-proof.txt.*✎1" 15
    step_key Escape
    # Terminal content re-surfacing proves Esc handed the frame back; the
    # title's absence is asserted in scenario_assert_ui.
    step_wait_pane "$SCENARIO_DONE_PATTERN" 15
}

scenario_assert_effects() {
    assert_ws_file_eq activity-proof.txt "activity!"
    # The reconstruction source itself: claude persisted the transcript under
    # the agent_session_id friring minted (the pinned --session-id) — the
    # filename↔id match is the whole reason the F9 scan finds this session.
    local aid
    aid="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.agent_session_id')"
    [ -n "$aid" ] && [ "$aid" != "null" ] \
        || e2e_die "session has no agent_session_id" || return 1
    ls "$HOME/claude-config/projects/"*/"$aid.jsonl" >/dev/null 2>&1 \
        || e2e_die "no transcript $HOME/claude-config/projects/*/$aid.jsonl" || return 1
    [ "$(journal_matched activity-write-call)" -ge 1 ] \
        || e2e_die "activity-write-call fixture never matched"
    [ "$(journal_matched after-activity-write)" -ge 1 ] \
        || e2e_die "after-activity-write fixture never matched (tool_result never posted)"
}

scenario_assert_ui() {
    [ "$(e2e_hook_state)" = "done" ] \
        || e2e_die "final hook_state '$(e2e_hook_state)' != done" || return 1
    # Esc closed the view: its content must be gone. NOT " Activity · " — the
    # session panel's tab bar reads "… Shell · F8 ─ Activity · F9 …" even with
    # the view closed, so grep the Files-section row that only the open view
    # renders (Esc was pressed from Files). Polled: a repaint may lag the
    # keypress by a frame.
    for _ in $(seq 1 50); do
        e2e_pane | grep -q "Edited (1)" || return 0
        sleep 0.1
    done
    e2e_die "activity view still open after Esc
--- pane ---
$(e2e_pane)"
}
