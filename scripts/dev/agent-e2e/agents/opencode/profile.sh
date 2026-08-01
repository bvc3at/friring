# shellcheck shell=bash
#
# Agent profile: opencode. Sourced by the agent-e2e harness after the sandbox
# env is up and the stub URL is known ($AGENT_E2E_STUB_URL). See
# agents/claude/profile.sh for the profile contract.
#
# Conformance (probed against opencode 1.17.15): the @ai-sdk/openai-compatible
# provider runtime is bundled in the binary (nothing fetched from npm), a
# fictional model id passes with no catalog validation, and a fully cold cache
# works offline — the models.dev fetch is best-effort and disabled below
# anyway. One AMBIENT call: title generation fires on the first message of
# every session, to the SAME stub model, with a "title generator" system
# prompt (match it via systemContains; its reply becomes the visible session
# title). No trust or onboarding dialogs exist; it boots straight to ready.

# shellcheck disable=SC2034  # the AGENT_* contract vars are read by harness.sh
AGENT_NAME="opencode"
AGENT_STUB_DIALECT="openai"
# The built-in hooks extension patches claude only; opencode scenarios must
# not use step_wait_state.
AGENT_HAS_STATUS_HOOKS=0
AGENT_LAUNCH_ARGS=()
# Model id (displayed in the TUI status line as "<model> <provider name>").
# Callers may pre-set AGENT_MODEL / AGENT_PROVIDER_LABEL before sourcing.
AGENT_MODEL="${AGENT_MODEL:-tempest-oss-140b}"
AGENT_PROVIDER_LABEL="${AGENT_PROVIDER_LABEL:-Tempest}"

agent_binary() {
    if [ -n "${FRIRING_E2E_OPENCODE_BIN:-}" ]; then
        [ -x "$FRIRING_E2E_OPENCODE_BIN" ] || return 1
        echo "$FRIRING_E2E_OPENCODE_BIN"
        return 0
    fi
    command -v opencode
}

agent_version() {
    e2e_bin_version "$(agent_binary)"
}

# One-shot headless prompt argv: `opencode run`.
agent_print_args() {
    AGENT_PRINT_ARGS=(run "$1")
}

# OPENCODE_CONFIG pins the config file explicitly (opencode consults both
# $HOME/.config and XDG_CONFIG_HOME, which the full sandbox points at
# different dirs — don't rely on that resolution order). The kill switches
# suppress the best-effort models.dev fetch and the npm update check; the
# dead proxies enforce app-level offline like every profile (no_proxy for
# loopback is mandatory — the stub call itself must not be proxied).
agent_env() {
    cat <<EOF
OPENCODE_CONFIG=$HOME/opencode.json
OPENCODE_DISABLE_MODELS_FETCH=1
OPENCODE_DISABLE_AUTOUPDATE=1
OPENCODE_DISABLE_LSP_DOWNLOAD=1
http_proxy=http://127.0.0.1:9
https_proxy=http://127.0.0.1:9
HTTP_PROXY=http://127.0.0.1:9
HTTPS_PROXY=http://127.0.0.1:9
no_proxy=127.0.0.1,localhost
NO_PROXY=127.0.0.1,localhost
EOF
}

# A single custom provider against the stub; apiKey is a dummy Bearer token
# (opencode requires the field, the stub ignores it). Workspace paths are
# irrelevant: opencode has no folder-trust concept.
agent_seed_config() {
    jq -n --arg url "$AGENT_E2E_STUB_URL/v1" --arg model "$AGENT_MODEL" \
        --arg label "$AGENT_PROVIDER_LABEL" '{
        "$schema": "https://opencode.ai/config.json",
        provider: {
            stub: {
                npm: "@ai-sdk/openai-compatible",
                name: $label,
                options: { baseURL: $url, apiKey: "dummy" },
                models: { ($model): { name: $model } }
            }
        },
        model: ("stub/" + $model),
        autoupdate: false,
        share: "disabled"
    }' > "$HOME/opencode.json"
}

agent_agents_toml_entry() {
    cat <<EOF
[[agents]]
name = "opencode"
command = "$(agent_binary)"
args = []
EOF
}
