# shellcheck shell=bash
#
# Agent profile: OpenAI Codex CLI. Sourced by the agent-e2e harness after the
# sandbox env is up and the stub URL is known ($AGENT_E2E_STUB_URL). See
# agents/claude/profile.sh for the profile contract.
#
# Conformance (probed against codex-cli 0.144.4): custom providers need no
# login at all — the ChatGPT auth flow only guards the built-in `openai`
# provider, and with no `env_key` in the provider block codex sends no auth
# header. `wire_api = "chat"` is REMOVED in this version (hard startup error);
# the stub speaks the Responses API.

# shellcheck disable=SC2034  # the AGENT_* contract vars are read by harness.sh
AGENT_NAME="codex"
AGENT_STUB_DIALECT="openai"
# The hooks extension JSON-merges codex's hooks.json (see agent_env for how the
# sandbox makes friring's ~/.codex the dir codex actually reads).
AGENT_HAS_STATUS_HOOKS=1
# codex won't run a hook until its command string is accepted at the "Hooks need
# review" prompt, and the trust hash it persists is not something friring can
# pre-seed. This flag is codex's own escape hatch for automation that already
# vets its hook sources — which is exactly the sandbox: the only hooks present
# are the ones friring just installed.
AGENT_LAUNCH_ARGS=(--dangerously-bypass-hook-trust)
# The model id codex runs (and displays in its header + footer). Fictional ids
# are accepted — codex only prints a "Model metadata not found" warning.
# Callers (demo recorder, scenarios) may pre-set AGENT_MODEL before sourcing.
AGENT_MODEL="${AGENT_MODEL:-gpt-6.x}"

agent_binary() {
    if [ -n "${FRIRING_E2E_CODEX_BIN:-}" ]; then
        [ -x "$FRIRING_E2E_CODEX_BIN" ] || return 1
        echo "$FRIRING_E2E_CODEX_BIN"
        return 0
    fi
    command -v codex
}

agent_version() {
    e2e_bin_version "$(agent_binary)"
}

# One-shot headless prompt argv: `codex exec`. --skip-git-repo-check keeps it
# independent of the workspace's git state (the harness stdin-redirects
# /dev/null globally: codex APPENDS piped stdin to the prompt otherwise).
agent_print_args() {
    AGENT_PRINT_ARGS=(exec --skip-git-repo-check "${AGENT_LAUNCH_ARGS[@]}" "$1")
}

# CODEX_HOME confines all codex state (config, sessions, sqlite) to the
# sandbox. It points at the sandbox HOME's `.codex` — not a differently-named
# dir — because the hooks extension merges into the literal `~/.codex/hooks.json`
# (guarded by `requires_dir = "~/.codex"`): pointing codex anywhere else would
# leave friring writing hooks the binary never reads. Dead proxies enforce
# app-level offline, same as the claude profile; codex was probed to make zero
# non-stub calls with them in place.
agent_env() {
    cat <<EOF
CODEX_HOME=$HOME/.codex
http_proxy=http://127.0.0.1:9
https_proxy=http://127.0.0.1:9
HTTP_PROXY=http://127.0.0.1:9
HTTPS_PROXY=http://127.0.0.1:9
no_proxy=127.0.0.1,localhost
NO_PROXY=127.0.0.1,localhost
EOF
}

# approval_policy/sandbox_mode suppress the interactive mode prompts; the
# [projects] tables suppress the folder-trust dialog for every workspace the
# run touches. No `env_key` on purpose: with it unset codex hard-errors, and
# without it no auth header is sent at all. check_for_update_on_startup=false
# keeps the release banner (and the `brew upgrade` it can launch on a stray
# Enter) out of the pane. NOTE: the codex TUI rewrites this file on startup
# (adds its own keys) — seed it fresh per run, never assume it stays
# byte-identical.
#
# The dir must exist before the harness activates the hooks extension: its codex
# merge is guarded by `requires_dir = "~/.codex"` and is silently skipped when
# the dir is missing.
agent_seed_config() {
    mkdir -p "$HOME/.codex"
    {
        cat <<EOF
model = "$AGENT_MODEL"
model_provider = "stub"
approval_policy = "never"
sandbox_mode = "read-only"
check_for_update_on_startup = false

[model_providers.stub]
name = "Stub"
base_url = "$AGENT_E2E_STUB_URL/v1"
wire_api = "responses"
EOF
        local ws
        for ws in "$@"; do
            printf '\n[projects."%s"]\ntrust_level = "trusted"\n' "$ws"
        done
    } > "$HOME/.codex/config.toml"
}

agent_agents_toml_entry() {
    local args="" a
    for a in "${AGENT_LAUNCH_ARGS[@]}"; do
        args="$args\"$a\", "
    done
    cat <<EOF
[[agents]]
name = "codex"
command = "$(agent_binary)"
args = [${args%, }]
EOF
}
