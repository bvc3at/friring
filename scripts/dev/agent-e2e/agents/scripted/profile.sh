# shellcheck shell=bash
#
# Agent profile: "scripted" — a plain shell script registered as a coding
# agent. Friring is agent-neutral by design (any CLI described in agents.toml
# launches through the same GenericProvider), and this profile is the living
# proof: no real agent binary, no model, no network. It exists for two
# reasons:
#
#   1. It e2e-tests the declarative registry itself: `command` is a bash
#      script, and the `new_session_args` / `resume_args` templates carry the
#      `{id}`/`{name}` placeholders, whose expansion the script prints for
#      the scenario to assert against `friring-cli session get --json`.
#   2. It makes pure-UI scenarios (sidebar, wizard, panels, search, delete,
#      messaging, …) fast and runnable on any machine — bash is always
#      present, so `require_agent scripted` never skips.
#
# The script prints one READY line (mode + expanded placeholders), then
# echoes every stdin line back prefixed `GOT:` — which lets a scenario tell
# "text typed into the PTY" apart from "text the agent received" (terminal
# echo shows the former, the GOT: line proves the latter).
#
# No model traffic: the anthropic stub is booted (harness contract) but the
# scenario's fixtures.json is just `{"responses": []}` — the strict-offline
# invariant then proves the agent made zero calls.

# shellcheck disable=SC2034  # the AGENT_* contract vars are read by harness.sh
AGENT_NAME="scripted"
AGENT_STUB_DIALECT="anthropic"
# No lifecycle hooks: hook_state stays null unless a scenario signals
# explicitly via `friring-cli session signal --session <uuid>`.
AGENT_HAS_STATUS_HOOKS=0
AGENT_LAUNCH_ARGS=()
AGENT_MODEL="none"

# The interpreter, not the script: require_agent probes this before the
# sandbox (and the script) exists. bash answers --version everywhere, so
# scripted scenarios never skip.
agent_binary() {
    command -v bash
}

agent_version() {
    e2e_bin_version "$(agent_binary)"
}

# Protocol smoke is meaningless for a script agent; emit a self-describing
# no-op so a mistaken `e2e_protocol_smoke` fails loudly on its asserts
# rather than mysteriously.
agent_print_args() {
    AGENT_PRINT_ARGS=(-c "echo 'scripted agent has no print mode'")
}

# No model API, but keep the dead-proxy contract: if the script (or anything
# it runs) tried the network it would hit the dead port like every agent.
agent_env() {
    cat <<EOF
http_proxy=http://127.0.0.1:9
https_proxy=http://127.0.0.1:9
HTTP_PROXY=http://127.0.0.1:9
HTTPS_PROXY=http://127.0.0.1:9
no_proxy=127.0.0.1,localhost
NO_PROXY=127.0.0.1,localhost
EOF
}

# The "binary" is written at boot into the sandbox HOME. Trust dirs are
# irrelevant (no trust dialog), so the extra args are ignored.
agent_seed_config() {
    cat > "$HOME/scripted-agent.sh" <<'EOF'
#!/usr/bin/env bash
# Minimal stand-in coding agent for the friring e2e suite: prove argv
# template expansion, then echo stdin lines back with a GOT: prefix.
printf 'SCRIPTED-READY mode=%s id=%s name=%s\n' "${1:-none}" "${2:-}" "${3:-}"
while IFS= read -r line; do
    printf 'GOT:%s\n' "$line"
done
EOF
    chmod +x "$HOME/scripted-agent.sh"
}

# resume_latest: a script has no transcript store, so restart must not gate
# on the claude-transcript check — presence of resume_latest makes Ctrl+R
# always take the resume template (mirrors the codex/opencode entries).
agent_agents_toml_entry() {
    cat <<EOF
[[agents]]
name = "scripted"
command = "$HOME/scripted-agent.sh"
new_session_args = ["new", "{id}", "{name}"]
resume_args = ["resume", "{id}"]
fork_args = ["fork", "{id}", "{name}"]
resume_latest = true
EOF
}
