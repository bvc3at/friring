# shellcheck shell=bash
#
# Agent profile: Claude Code. Sourced by the agent-e2e harness after the
# sandbox env is up and the stub URL is known ($AGENT_E2E_STUB_URL).
#
# The profile contract (every agents/<name>/profile.sh implements it):
#   AGENT_NAME                  agents.toml entry name; must match the name the
#                               built-in hooks extension patches (hooks are
#                               name-matched, see extensions/hooks/extension.toml)
#   AGENT_STUB_DIALECT          which stub speaks this agent's model API:
#                               "anthropic" (stub/anthropic-stub.mjs) or "none"
#                               (agent can't be stubbed — its scenarios are
#                               skipped offline, declared rather than faked)
#   AGENT_HAS_STATUS_HOOKS      1 if the built-in hooks extension wires this
#                               agent's signals (gates step_wait_state)
#   AGENT_LAUNCH_ARGS           flags shared by all three drive depths
#   agent_binary                print the absolute binary path (or fail)
#   agent_version               print the binary version (artifact metadata)
#   agent_env                   print KEY=VALUE lines the harness exports before
#                               anything that may start a tmux server (panes
#                               inherit the server environment)
#   agent_seed_config           pre-seed config under the sandbox $HOME so the
#                               binary runs non-interactively (onboarding etc.)
#   agent_agents_toml_entry     print the [[agents]] TOML entry

# shellcheck disable=SC2034  # the AGENT_* contract vars are read by harness.sh
AGENT_NAME="claude"
AGENT_STUB_DIALECT="anthropic"
# The built-in hooks extension wires status signals for this agent, so
# scenarios may assert working/done transitions (step_wait_state).
AGENT_HAS_STATUS_HOOKS=1
# Bypass is safe here: throwaway workspace, loopback-only egress, dead-proxy
# for everything else. Shared by agents.toml and the bare-tmux/-p smokes so
# all three drive depths launch the binary identically.
AGENT_LAUNCH_ARGS=(--dangerously-skip-permissions)

# FRIRING_E2E_CLAUDE_BIN pins an exact binary (CI installs a pinned version);
# otherwise whatever `claude` is on PATH.
agent_binary() {
    if [ -n "${FRIRING_E2E_CLAUDE_BIN:-}" ]; then
        [ -x "$FRIRING_E2E_CLAUDE_BIN" ] || return 1
        echo "$FRIRING_E2E_CLAUDE_BIN"
        return 0
    fi
    command -v claude
}

agent_version() {
    e2e_bin_version "$(agent_binary)"
}

# One-shot headless prompt argv (protocol smoke): claude's print mode.
agent_print_args() {
    AGENT_PRINT_ARGS=(-p "$1" "${AGENT_LAUNCH_ARGS[@]}")
}

# ANTHROPIC_AUTH_TOKEN (Bearer) rather than ANTHROPIC_API_KEY: the API-key path
# triggers an interactive "use this key?" approval, the token path doesn't.
# The proxy vars route any non-loopback HTTP(S) egress to a dead port —
# app-level offline enforcement (spike-verified: the full tool loop survives
# it, so nothing external is load-bearing).
agent_env() {
    cat <<EOF
ANTHROPIC_BASE_URL=$AGENT_E2E_STUB_URL
ANTHROPIC_AUTH_TOKEN=friring-e2e-dummy
CLAUDE_CONFIG_DIR=$HOME/claude-config
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
DISABLE_AUTOUPDATER=1
DISABLE_TELEMETRY=1
DISABLE_ERROR_REPORTING=1
DISABLE_BUG_COMMAND=1
http_proxy=http://127.0.0.1:9
https_proxy=http://127.0.0.1:9
HTTP_PROXY=http://127.0.0.1:9
HTTPS_PROXY=http://127.0.0.1:9
no_proxy=127.0.0.1,localhost
NO_PROXY=127.0.0.1,localhost
EOF
}

# hasCompletedOnboarding skips first-run setup; bypassPermissionsModeAccepted
# lets --dangerously-skip-permissions run without an acceptance prompt (safe
# here: throwaway workspace, loopback-only egress); the per-folder trust
# entries skip the interactive-mode "trust this folder?" dialog for every
# workspace the run touches (same trick scripts/demo/record.sh uses). Seeded
# in both locations because claude reads $CLAUDE_CONFIG_DIR/.claude.json when
# the override is set and ~/.claude.json otherwise.
agent_seed_config() {
    local seed
    seed="$(jq -n '{
        hasCompletedOnboarding: true,
        bypassPermissionsModeAccepted: true,
        projects: ($ARGS.positional
                   | map({(.): {hasTrustDialogAccepted: true}}) | add)
    }' --args "$@")"
    mkdir -p "$HOME/claude-config"
    printf '%s' "$seed" > "$HOME/.claude.json"
    printf '%s' "$seed" > "$HOME/claude-config/.claude.json"
}

# Absolute path as command: agents.toml must not depend on PATH luck, and CI
# pins the binary outside PATH. Name "claude" is load-bearing (hooks patch).
agent_agents_toml_entry() {
    local args="" a
    for a in "${AGENT_LAUNCH_ARGS[@]}"; do
        args="$args\"$a\", "
    done
    cat <<EOF
[[agents]]
name = "claude"
command = "$(agent_binary)"
args = [${args%, }]
EOF
}
