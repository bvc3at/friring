# shellcheck shell=bash
#
# Agent profile: Antigravity CLI (`agy`). DECLARED UNSTUBBABLE.
#
# Probed against agy 1.1.2: agy has no plain-API-key path that honors a custom
# base URL. Even with settings.json security.auth.selectedType =
# "gemini-api-key" and GEMINI_API_KEY set, every run forces an interactive
# Antigravity OAuth login against accounts.google.com (cloud-platform scope,
# fixed antigravity.google client_id) and NEVER contacts a configured
# endpoint — a local stub receives zero requests, offline or not. So agy
# cannot run offline against a stub the way claude/codex/opencode can.
#
# This is a DECLARED capability, not a gap the harness papers over: scenarios
# for agy refuse to run offline (e2e_boot errors on dialect "none") instead of
# faking a login. In the demo recorder agy is featured LOGGED OUT on its clean,
# branded "select login method" screen — which is also the only way to keep the
# signed-in Google account's email/name (fetched from the server via keyring
# auth) off camera. See scripts/demo/record.sh for the logged-out seeding
# (trustedFolders + onboarding cache + the D-Bus/keyring cutoff).

# shellcheck disable=SC2034  # the AGENT_* contract vars are read by harness.sh
AGENT_NAME="antigravity"
AGENT_STUB_DIALECT="none"
AGENT_HAS_STATUS_HOOKS=0
AGENT_LAUNCH_ARGS=()

# agy's binary is `agy` (the Gemini CLI successor), not the display name.
agent_binary() {
    if [ -n "${FRIRING_E2E_ANTIGRAVITY_BIN:-}" ]; then
        [ -x "$FRIRING_E2E_ANTIGRAVITY_BIN" ] || return 1
        echo "$FRIRING_E2E_ANTIGRAVITY_BIN"
        return 0
    fi
    command -v agy
}

agent_version() {
    e2e_bin_version "$(agent_binary)"
}

# Unstubbable: the remaining contract functions exist so the profile loads
# cleanly for discovery/metadata, but e2e_boot aborts before they run (dialect
# "none"). They must never be reached in an offline scenario.
agent_print_args() {
    AGENT_PRINT_ARGS=(-p "$1")
}

agent_env() {
    :
}

agent_seed_config() {
    :
}

agent_agents_toml_entry() {
    cat <<EOF
[[agents]]
name = "antigravity"
command = "$(agent_binary)"
args = []
EOF
}
