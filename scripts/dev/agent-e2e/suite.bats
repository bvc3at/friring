#!/usr/bin/env bats
# Real-agent e2e suite: real agent binaries against a local model stub,
# driven through the real Friring TUI. Invoke via scripts/dev/agent-e2e/run.sh
# (which builds the dev binaries first). See docs/E2E.md.
#
# Layered on purpose — when the full scenario breaks, the shallower tests
# localize it: protocol (binary↔stub contract) → interactive (agent TUI, no
# Friring) → full scenario (Friring rendering, key forwarding, status hooks).

setup() {
    AGENT_E2E_DIR="$(cd "$BATS_TEST_DIRNAME" && pwd)"
    export AGENT_E2E_DIR
    # shellcheck disable=SC1091
    source "$AGENT_E2E_DIR/lib/harness.sh"
    e2e_require_tools test
}

# Per-test agent gate: an agent binary that is missing — or present but not
# usable — skips only THAT agent's tests, so a machine with any subset of
# claude/codex/opencode/agy stays green. e2e_boot re-sources the profile itself
# (via SCENARIO_AGENT); this early source exists purely for the skip check.
#
# The version probe is the usability check, not just discovery theatre: a
# binary that cannot even print `--version` within the bound cannot run a
# scenario either, and skipping it reports that in one line instead of burning
# the suite's wall-clock on waits that were always going to time out.
require_agent() {
    # shellcheck disable=SC1090
    source "$AGENT_E2E_DIR/agents/$1/profile.sh"
    agent_binary >/dev/null 2>&1 \
        || skip "$1 binary not found (install it or pin via FRIRING_E2E_*_BIN)"
    [ -n "$(agent_version)" ] \
        || skip "$1 binary is present but did not respond to --version"
}

teardown() {
    # bats runs teardown() even when the test failed or timed out, which makes
    # it the reliable reaper (an in-body trap can be bypassed on hard kills).
    local failed=1
    [ -n "${BATS_TEST_COMPLETED:-}" ] && failed=0
    e2e_teardown "$failed" || true
}

@test "protocol: claude -p completes the stubbed tool-use loop (no Friring)" {
    require_agent claude
    e2e_protocol_smoke "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
}

@test "interactive: claude TUI completes the stubbed tool-use loop in bare tmux (no Friring)" {
    require_agent claude
    e2e_interactive_smoke "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
}

@test "e2e: claude text turn through Friring with working→done status transition" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-text-turn"
}

@test "e2e: claude tool-use loop through Friring writes a real workspace file" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-tool-loop"
}

@test "e2e: claude review loop — annotate, structured handoff to the agent, re-review nudge" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-review-loop"
}

@test "e2e: multi-repo wizard with a named workspace dir — spawn, symlink write, persistence, guarded delete" {
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-named-workspace"
}

@test "perf: claude flood turn through Friring publishes a perf report" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-perf-flood"
}

@test "e2e: tmux persistence — quitting and relaunching the TUI re-adopts the live claude pane" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-adopt-restart"
}

@test "e2e: claude permission prompt drives a real blocked signal; approval resumes to done" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-blocked-permission"
}

@test "e2e: Ctrl+R restarts claude with --resume and the conversation continues" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-restart-resume"
}

@test "e2e: <leader> f forks claude — parent link, sidebar nesting, forked conversation continues" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-fork"
}

@test "e2e: import a Claude Code conversation (i) and resume it in a new session" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-import-conversation"
}

@test "e2e: worktree session — claude works on a fresh branch outside the repo checkout" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-worktree-session"
}

@test "e2e: code review — comment on working changes and send the compiled review to claude" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-review-export"
}

@test "e2e: F9 activity view reconstructs the claude turn from its on-disk records" {
    require_agent claude
    e2e_scenario "$AGENT_E2E_DIR/scenarios/claude-activity-view"
}

@test "e2e: shell-script agent through the registry — template expansion, PTY input, resume restart" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-registry-terminal"
}

@test "e2e: multi-session switching (Alt+N, Ctrl+6) with CLI send/capture/focus staying in sync" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-multi-nav"
}

@test "e2e: repo-grouped sidebar with manual reorder (Shift+J/K) persisted as display_order" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-sidebar-order"
}

@test "e2e: blocked-session attention — badges, F10 walk, Alt+A jump" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-blocked-attention"
}

@test "e2e: soft delete with Ctrl+Z undo and the Ctrl+U restore list" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-soft-delete"
}

@test "e2e: Ctrl+S sync rebases the worktree onto a moved origin/main" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-worktree-sync"
}

@test "e2e: Ctrl+S rebase conflict is handed to the session's agent as a prompt" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-sync-conflict"
}

@test "e2e: a one-shot automation fires through the TUI and lands its prompt in the pane" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-automation-fire"
}

@test "e2e: task run (F5 -> r) seeds the agent prompt and tracks status to done" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-task-run"
}

@test "e2e: extension lifecycle and offline issue sync — install, activate, upsert dedupe, self-heal" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-extension-tasks"
}

@test "e2e: inter-session messages — wake nudge, exactly-once claim, reply" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-message-queue"
}

@test "e2e: global search over sessions (live buffer), tasks, automations, and files" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-global-search"
}

@test "e2e: shell pane toggle (Ctrl+T) — real shell beside the agent, tracked per session" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-shell-pane"
}

@test "e2e: OSC 52 copies from agent + shell panes land in the outer clipboard; Cmd+C dispatches Copy" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-osc52-clipboard"
}

@test "e2e: wizard back-navigation and the always-type repo palette (path mode, Tab completion)" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-wizard-backnav"
}

@test "e2e: wizard worktree flow — base branch pick, branch name, spawn into the worktree" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-wizard-worktree"
}

@test "e2e: theme persistence, settings live-reload, config error toast, perf HUD" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-theme-settings"
}

@test "e2e: info panel, seeded keybinding override, F1 rebind persisting to keybindings.json" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-info-keybind"
}

@test "e2e: leader key arms through nested tmux, dispatches, and never leaks to the PTY" {
    require_agent scripted
    e2e_scenario "$AGENT_E2E_DIR/scenarios/scripted-leader-key"
}

@test "protocol: codex exec completes a stubbed text turn (no Friring)" {
    require_agent codex
    e2e_protocol_smoke "$AGENT_E2E_DIR/scenarios/codex-text-turn"
}

@test "e2e: codex text turn through Friring against the openai-dialect stub" {
    require_agent codex
    e2e_scenario "$AGENT_E2E_DIR/scenarios/codex-text-turn"
}

@test "protocol: opencode run completes a stubbed text turn (no Friring)" {
    require_agent opencode
    e2e_protocol_smoke "$AGENT_E2E_DIR/scenarios/opencode-text-turn"
}

@test "e2e: opencode text turn through Friring against the openai-dialect stub" {
    require_agent opencode
    e2e_scenario "$AGENT_E2E_DIR/scenarios/opencode-text-turn"
}
