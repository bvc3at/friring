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
# The Agents half of the view is REAL: the stub answers the turn with a
# `Workflow` tool_use, so the claude binary runs an actual multi-agent
# workflow whose agents each call back into the stub, and writes the run to
# `subagents/workflows/wf_<id>/` itself. That is also the regression test for
# the v2.1.220 layout (workflow agents are `agent-<id>.json`/`.meta`, and the
# completion record moved to `<session>/workflows/<run>.json`).
#
# Demo-able: F9 records through `<leader> v` (SCENARIO_DEMO_KEYS), at the
# default 120-column canvas. Only the asserts probe friring-cli, and those
# never run in demo mode.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="F9 activity view: a claude Write turn reconstructed, plus the workflow/subagent tree"
SCENARIO_AGENT="claude"
# `<leader> v` is F9's own second route to the same action, and the one that
# films: the which-key overlay names the view before it opens, where a bare
# F9 shows the viewer nothing of what was pressed.
SCENARIO_DEMO_KEYS=("F9=C-f v")
SCENARIO_PROMPT="Log the Antarctic drift fix to drift.md using the Write tool."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="DRIFT-CONTAINED"
SCENARIO_WORKFLOW_PROMPT="Now run the drift audit workflow across the ring."

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
    step_wait_pane "Title: Log the Antarctic drift fix" 20

    # Navigator digits jump sections (2 Timeline, 4 Files). grep is line-based,
    # so tag.*path proves ONE row carries both the edit tag and the file.
    step_type "2"
    # Timeline rows print the Write's *absolute* file_path, truncated to the
    # central pane. It fits the default 120-column pane with ~6 columns to
    # spare only because the sandbox root is /tmp/friring-sandbox.XXXXXX and
    # the file name is short; lengthening either costs the greppable tail and
    # the scenario needs SCENARIO_COLS back — which the demo then records at,
    # so it buys a wider clip, not just a wider test.
    step_wait_pane "edit.*drift.md" 15
    # The redesigned Timeline groups events under a "▶" prompt turn header, with
    # the turn's actions in a "│" gutter beneath it.
    step_wait_pane "▶" 15
    step_wait_pane "│" 15
    # A second turn, this one a real multi-agent workflow: the stub answers with
    # a `Workflow` tool_use, claude runs the script, and each `agent()` inside it
    # calls back into the same stub. Nothing here is seeded.
    step_key Escape
    step_wait_pane "$SCENARIO_AGENT_READY" 30
    step_type "$SCENARIO_WORKFLOW_PROMPT"
    step_key Enter
    step_wait_pane "WORKFLOW-AUDIT-DONE" 120
    step_wait_state 'done' 60

    step_key F9
    step_wait_pane " Activity · Overview " 20
    step_type "6"
    step_wait_pane "drift-audit" 30
    step_wait_pane "ring-telemetry" 20
    step_wait_pane "setpoint-model" 20
    # Selecting the workflow renders its overview centrally — the phase list and
    # the per-agent grid, which only the completion record can supply.
    step_type "j"
    step_wait_pane "Survey" 20
    step_wait_pane "Contain" 20
    # One agent down, its own transcript.
    step_type "j"
    step_wait_pane "southern ring" 20

    step_type "4"
    step_wait_pane "Edited (1)" 15
    step_wait_pane "drift.md.*✎1" 15
    step_key Escape
    # Terminal content re-surfacing proves Esc handed the frame back; the
    # title's absence is asserted in scenario_assert_ui.
    step_wait_pane "$SCENARIO_DONE_PATTERN" 15
}

scenario_assert_effects() {
    assert_ws_file_eq drift.md "capture ring clamped to 92%"
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
