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
# The Agents half of the view — the workflow tree, its phase/agent grid and
# each agent's transcript — is driven from a *seeded* subagents/ tree (see
# activity_seed_agent_tree): a real Claude Code workflow spawns agents that
# each talk to the model API, which no offline run can do.
#
# Demo-able: F9 records through `<leader> v` (SCENARIO_DEMO_KEYS), at the
# default 120-column canvas. Only the asserts probe friring-cli, and those
# never run in demo mode.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="F9 activity view: a claude Write turn reconstructed, plus the workflow/subagent tree"
SCENARIO_AGENT="claude"
# VHS has no F-keys; `<leader> v` is F9's own second route to the same
# action, so the clip opens the view exactly as the scenario does.
SCENARIO_DEMO_KEYS=("F9=C-f v")
SCENARIO_PROMPT="Log the Antarctic drift fix to drift.md using the Write tool."
# The input-box prompt glyph — the stable "ready for input" marker across
# claude 2.x permission modes (verified against 2.1.207).
SCENARIO_AGENT_READY="❯"
SCENARIO_DONE_PATTERN="DRIFT-CONTAINED"

# Lay a workflow run + a standalone Task subagent into the session's own
# `subagents/` tree, in the exact on-disk shapes src/session/cc_activity.rs
# parses (`workflows/wf_<id>/journal.jsonl` + per-agent transcripts, the
# sibling `wf_<id>.json` completion record, and a top-level `agent-*.jsonl`
# pair). This is **seeded**, not driven: a Claude Code workflow spawns agents
# that each talk to the model API, and there is no offline way to make the real
# binary run one — so the Agents section would otherwise film as "No workflows
# or subagents yet." The parsers, the tree scan and the renderer under test are
# the real ones; only the producer is stood in for.
activity_seed_agent_tree() {
    local aid proj sub wf
    aid="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.agent_session_id')"
    [ -n "$aid" ] && [ "$aid" != "null" ] \
        || e2e_die "session has no agent_session_id" || return 1
    # Prefer the dir already holding this conversation, found by scanning for
    # it exactly as paths::claude_projects_dir does — never by recomputing
    # Claude Code's slug. In demo mode this runs before the recorded turn has
    # happened, so there may be no transcript yet; any project dir works,
    # because the tree resolver scans every `projects/*/` for an
    # `<agent_session_id>/subagents` child rather than guessing a slug.
    proj="$(find "$HOME/claude-config/projects" -maxdepth 2 -name "$aid.jsonl" \
        -exec dirname {} \; 2>/dev/null | head -1)"
    [ -n "$proj" ] || proj="$HOME/claude-config/projects/friring-e2e-seeded"
    sub="$proj/$aid/subagents"
    wf="$sub/workflows/wf_drift01"
    mkdir -p "$wf"

    printf '%s\n' \
        '{"type":"started","agentId":"ring01"}' \
        '{"type":"started","agentId":"filt02"}' \
        '{"type":"started","agentId":"setp03"}' \
        '{"type":"result","agentId":"ring01","result":"southern ring surveyed"}' \
        '{"type":"result","agentId":"filt02","result":"2 filters back online"}' \
        > "$wf/journal.jsonl"

    activity_seed_transcript "$wf/agent-ring01.jsonl" \
        "Sampling the southern capture ring telemetry." \
        "Ring ran hot after the filter swap: 0.4 ppm overshoot, setpoint integral wound up."
    activity_seed_transcript "$wf/agent-filt02.jsonl" \
        "Checking the two filter units that came back." \
        "Both are within spec. Neither explains the overshoot on its own."
    activity_seed_transcript "$wf/agent-setp03.jsonl" \
        "Replaying the setpoint model against the swap window." \
        "Clamping the ring to 92% contains it without a pause."
    printf '{"agentType":"workflow-subagent","spawnDepth":1}\n' \
        > "$wf/agent-ring01.meta.json"

    cat > "$sub/workflows/wf_drift01.json" <<'JSON'
{
  "workflowName": "drift-audit",
  "status": "completed",
  "totalTokens": 48120,
  "totalToolCalls": 26,
  "durationMs": 91000,
  "defaultModel": "fable-67",
  "logs": ["[phase] Survey complete", "[phase] Contain complete"],
  "phases": [{"index": 1, "title": "Survey"}, {"index": 2, "title": "Contain"}],
  "workflowProgress": [
    {"type": "workflow_phase", "index": 1, "title": "Survey"},
    {"type": "workflow_agent", "agentId": "ring01", "label": "ring-telemetry",
     "phaseTitle": "Survey", "state": "done", "model": "fable-67",
     "tokens": 18400, "toolCalls": 11, "lastToolName": "Bash"},
    {"type": "workflow_agent", "agentId": "filt02", "label": "filter-fleet",
     "phaseTitle": "Survey", "state": "done", "model": "fable-67",
     "tokens": 12600, "toolCalls": 7, "lastToolName": "Read"},
    {"type": "workflow_phase", "index": 2, "title": "Contain"},
    {"type": "workflow_agent", "agentId": "setp03", "label": "setpoint-model",
     "phaseTitle": "Contain", "state": "done", "model": "fable-67",
     "tokens": 17120, "toolCalls": 8, "lastToolName": "Write"}
  ]
}
JSON

    # A standalone Task subagent beside the workflow (the other half of the tree).
    activity_seed_transcript "$sub/agent-scout9.jsonl" \
        "Walking the ring's sector map." \
        "Twelve sectors, two on filter-swap standby. Nothing else is drifting."
    printf '%s\n' \
        '{"agentType":"Explore","description":"Map the southern capture ring","toolUseId":"toolu_e2e_task_1","spawnDepth":1}' \
        > "$sub/agent-scout9.meta.json"
}

# One agent transcript: the same JSONL line shape as the top-level conversation.
activity_seed_transcript() {
    local path="$1" thinking="$2" text="$3"
    jq -nc --arg th "$thinking" --arg tx "$text" \
        '{type:"assistant",message:{content:[{type:"thinking",thinking:$th},{type:"text",text:$tx}]}}' \
        > "$path"
}

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
    # Agents: the workflow tree + the standalone Task subagent. Seeded above
    # (see activity_seed_agent_tree) because no offline path makes the real
    # binary run a workflow; the scan, parsers and renderer are the real ones.
    activity_seed_agent_tree || return 1
    step_type "6"
    step_wait_pane "drift-audit" 20
    step_wait_pane "ring-telemetry" 15
    step_wait_pane "setpoint-model" 15
    step_wait_pane "Explore" 15

    # Selecting the workflow row renders its overview in the central pane —
    # the phase list and the per-agent grid, which is the part worth filming.
    step_type "j"
    step_wait_pane "Survey" 20
    step_wait_pane "Contain" 20
    # And one agent down, its own transcript.
    step_type "j"
    step_wait_pane "southern capture ring telemetry" 20

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
