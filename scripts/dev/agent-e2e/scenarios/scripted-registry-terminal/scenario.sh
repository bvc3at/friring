# shellcheck shell=bash
#
# Scenario: the agent-neutral registry, end to end, with no real agent
# binary. A bash script registered in agents.toml is spawned through the
# full Friring path; the pane shows the `{id}`/`{name}` placeholders
# expanded from the `new_session_args` template (asserted against the
# session's persisted `agent_session_id`/name), typed input reaches the
# script's stdin (terminal-first focus), and a Ctrl+H → Ctrl+R restart
# respawns it through the `resume_args` template with the same id.
#
# The agent's argv (the mode/id/name it echoes) is asserted against its OWN
# pane via `friring-cli session capture`, not the TUI-rendered driver pane,
# for two reasons the CI (tmux 3.4, slower) surfaced that a local run (tmux
# 3.7b) hid: the full ready line is wider than the narrow agent panel and
# the TUI splits it across rows (a single-row grep then misses the wrapped
# tail), and after a restart the scripted agent prints one line then blocks
# on read — that one-shot line can race the TUI reader re-attaching to the
# respawned pane, so the rendered copy may never show it. `session capture`
# runs `tmux capture-pane -J` on the real pane, immune to both.
#
# Test-mode only (extracts ids via friring-cli mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="Shell-script agent through the registry: template expansion, PTY input, resume on restart"
SCENARIO_AGENT="scripted"

# Bounded poll of the agent's OWN pane (joined, unwrapped) for a fixed
# string — see the header for why this beats step_wait_pane here.
reg_wait_agent_pane() {
    local pat="$1" tries="$2"
    for _ in $(seq 1 "$((tries * 5))"); do
        friring-cli --json session capture "$E2E_SESSION_ID" 2>/dev/null \
            | jq -r '.output // empty' | grep -qF -- "$pat" && return 0
        sleep 0.2
    done
    e2e_die "agent pane never showed: $pat"
}

scenario_steps() {
    # The TUI adopts and renders the freshly spawned agent pane.
    step_wait_pane "SCRIPTED-READY mode=new" 60

    # The conversation id friring minted and substituted for {id}.
    E2E_AGENT_CONVO_ID="$(friring-cli --json session get "$E2E_SESSION_ID" \
        | jq -r '.agent_session_id')"
    [ -n "$E2E_AGENT_CONVO_ID" ] && [ "$E2E_AGENT_CONVO_ID" != "null" ] \
        || e2e_die "session has no agent_session_id" || return 1
    # {id} and {name} really expanded from new_session_args ("new {id} {name}").
    reg_wait_agent_pane "mode=new id=$E2E_AGENT_CONVO_ID name=$E2E_SCENARIO_NAME" 15

    # Terminal-first focus: an adopted session boots with Terminal focus, so
    # plain typing must land on the script's stdin (GOT: echo), not in the UI.
    step_type "hello-from-the-pty"
    step_key Enter
    step_wait_pane "GOT:hello-from-the-pty" 15

    # Ctrl+R is terminal-passthrough, so it must NOT restart while the
    # terminal is focused — leave for the session list first (Ctrl+H), then
    # restart. The respawn takes the resume template ("resume {id}"): mode
    # flips to resume, same id, no name.
    step_key C-h
    step_key C-r
    step_wait_pane "Session restarted" 30
    reg_wait_agent_pane "SCRIPTED-READY mode=resume id=$E2E_AGENT_CONVO_ID" 30
}

scenario_assert_effects() {
    # The registry entry really was data, not code: same id persisted.
    local got
    got="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.agent_session_id')"
    [ "$got" = "$E2E_AGENT_CONVO_ID" ] \
        || e2e_die "agent_session_id changed across restart: '$got' != '$E2E_AGENT_CONVO_ID'"
}

scenario_assert_ui() {
    reg_wait_agent_pane "mode=resume id=$E2E_AGENT_CONVO_ID" 5
}
