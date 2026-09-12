# shellcheck shell=bash
#
# Agent profile: "ringwriter" — a synthetic coding agent whose executable is
# called nothing friring has ever heard of, and which writes a transcript in a
# format friring *does* know how to read.
#
# It exists for one thing the `scripted` agent cannot show: `activity_provider`
# in agents.toml. Activity reporting otherwise resolves its provider from the
# command **basename**, so a wrapper script or a rebranded binary — anything not
# literally named `claude`, `codex`, `gemini`, … — reported nothing at all,
# however ordinary its records. This entry declares
# `activity_provider = "claude-code"` and nothing else about the format, so a
# scenario asserting real tool events through F9 and `friring-cli session
# activity` is asserting that the declaration is what selected the parser.
#
# Three properties are deliberate, and each is load-bearing for the scenario:
#
#   1. The basename (`ringwriter`) resolves to NO provider. Without the
#      declaration the view says activity isn't supported for this agent —
#      which the scenario also asserts, on a sibling entry.
#   2. `hook_schema` is unset, so the built-in hooks extension wires nothing.
#      Lifecycle state is signalled by the script itself via `friring-cli
#      session signal` — which is what makes the two fields visibly
#      independent: this agent reports activity with no hook wiring at all.
#   3. Transcripts go under $CLAUDE_CONFIG_DIR, pointed at a sandbox dir this
#      profile owns. The claude provider honours that override, so the scenario
#      also covers "a declared provider still honours its state-dir override".
#
# No model traffic: the anthropic stub is booted (harness contract) but the
# scenario's fixtures.json is `{"responses": []}` — the strict-offline
# invariant then proves the agent made zero calls.

# shellcheck disable=SC2034  # the AGENT_* contract vars are read by harness.sh
AGENT_NAME="ringwriter"
AGENT_STUB_DIALECT="anthropic"
# See (2) above: no hooks-extension wiring, so step_wait_state is unavailable
# and scenarios poll for the signals the script sends instead.
AGENT_HAS_STATUS_HOOKS=0
AGENT_LAUNCH_ARGS=()
AGENT_MODEL="none"

# The interpreter, not the script: require_agent probes this before the sandbox
# (and the script) exists. bash answers --version everywhere, so ringwriter
# scenarios never skip.
agent_binary() {
    command -v bash
}

agent_version() {
    e2e_bin_version "$(agent_binary)"
}

# Protocol smoke is meaningless for a script agent; emit a self-describing
# no-op so a mistaken `e2e_protocol_smoke` fails loudly on its asserts.
agent_print_args() {
    AGENT_PRINT_ARGS=(-c "echo 'ringwriter has no print mode'")
}

# CLAUDE_CONFIG_DIR names where the script writes and where the claude provider
# reads — deliberately NOT ~/.claude, so the scenario proves the override is
# honoured rather than that a default path happened to work. RINGWRITER_* carry
# the two things the script cannot derive: the fixture it replays (copied into
# the sandbox by agent_seed_config) and the workspace its tool inputs name.
# The dead-proxy vars keep the contract every agent gets: if the script (or
# anything it runs) tried the network, it would hit the dead port.
agent_env() {
    cat <<EOF
CLAUDE_CONFIG_DIR=$HOME/ringwriter-state
RINGWRITER_TRANSCRIPT=$HOME/ringwriter-transcript.jsonl
RINGWRITER_WS=$E2E_WS
http_proxy=http://127.0.0.1:9
https_proxy=http://127.0.0.1:9
HTTP_PROXY=http://127.0.0.1:9
HTTPS_PROXY=http://127.0.0.1:9
no_proxy=127.0.0.1,localhost
NO_PROXY=127.0.0.1,localhost
EOF
}

# The "binary" is written at boot into the sandbox HOME, under a basename no
# provider inference can resolve. It replays the fixture transcript into the
# place the claude provider looks — $CLAUDE_CONFIG_DIR/projects/<slug>/<id>.jsonl
# — then behaves like the `scripted` agent (echo stdin back) so the pane stays
# alive and drivable.
#
# The <slug> is claude's own rule (every non-alphanumeric byte becomes `-`), but
# only for realism: friring finds a transcript by scanning projects/*/ for
# <id>.jsonl precisely because that rule is undocumented, so the dir name is not
# what the scenario rests on.
agent_seed_config() {
    cp "$AGENT_E2E_DIR/agents/ringwriter/transcript.jsonl" \
        "$HOME/ringwriter-transcript.jsonl"
    cat > "$HOME/ringwriter" <<'EOF'
#!/usr/bin/env bash
# Synthetic coding agent for the friring e2e suite. Argv is the agents.toml
# template expanded: <mode> <id> [name].
printf 'RINGWRITER-READY mode=%s id=%s name=%s\n' "${1:-none}" "${2:-}" "${3:-}"
id="${2:-}"

# No hooks extension wires this agent (hook_schema is unset on purpose), so it
# reports its own lifecycle. `session signal` resolves the session from
# $FRIRING_SESSION, which friring injects into every pane.
signal() { friring-cli session signal --state "$1" >/dev/null 2>&1 || true; }

# Replay the fixture transcript into the format's own on-disk home. Skipped
# when one is already there (a resume): rewriting it would reset the byte
# offsets friring tails by.
if [ -n "$id" ] && [ -n "${CLAUDE_CONFIG_DIR:-}" ] && [ -s "${RINGWRITER_TRANSCRIPT:-}" ]; then
    dir="$CLAUDE_CONFIG_DIR/projects/$(printf '%s' "$RINGWRITER_WS" | sed 's/[^A-Za-z0-9]/-/g')"
    mkdir -p "$dir"
    if [ ! -s "$dir/$id.jsonl" ]; then
        signal working
        sed -e "s|{{ID}}|$id|g" -e "s|{{WS}}|$RINGWRITER_WS|g" \
            "$RINGWRITER_TRANSCRIPT" > "$dir/$id.jsonl"
        # The transcript claims a Write; make it true, so the scenario can
        # assert the workspace side effect alongside the reconstruction.
        printf 'capture ring clamped to 92%%\n' > "$RINGWRITER_WS/drift.md"
        printf 'RINGWRITER-TRANSCRIPT %s\n' "$dir/$id.jsonl"
        signal done
    fi
fi

while IFS= read -r line; do
    printf 'GOT:%s\n' "$line"
done
EOF
    chmod +x "$HOME/ringwriter"
}

# `activity_provider` is the entry this whole profile exists to exercise.
# `hook_schema` is deliberately absent — see (2) in the header.
#
# resume_latest: a script has no transcript store friring can probe, so restart
# must not gate on the claude-transcript check (mirrors the scripted/codex
# entries).
agent_agents_toml_entry() {
    cat <<EOF
[[agents]]
name = "ringwriter"
command = "$HOME/ringwriter"
new_session_args = ["new", "{id}", "{name}"]
resume_args = ["resume", "{id}"]
fork_args = ["fork", "{id}", "{name}"]
resume_latest = true
activity_provider = "claude-code"
EOF
}
