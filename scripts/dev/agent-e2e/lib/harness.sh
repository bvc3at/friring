# shellcheck shell=bash
#
# Real-agent e2e harness core — sourced by suite.bats (test mode) and run.sh
# (demo mode). One scenario description (scenario.sh) drives both: the step_*
# primitives either drive the live TUI through a driver tmux and poll for
# results (test mode), or emit a VHS .tape (demo mode). See docs/E2E.md.
#
# Hermeticity: everything runs under scripts/dev/lib/sandbox-env.sh
# `tbx_sandbox_init_full fresh` (throwaway HOME/XDG/TMUX_TMPDIR), the model API
# is a loopback stub, and non-loopback HTTP(S) egress is routed to a dead
# proxy port by the agent profile. Nothing touches the real ~/.claude,
# ~/.config/thurbox, or any real tmux server.

: "${AGENT_E2E_DIR:?source suite.bats/run.sh sets AGENT_E2E_DIR}"
REPO_ROOT="$(cd "$AGENT_E2E_DIR/../../.." && pwd)"

# Driver tmux socket (hosts the thurbox TUI under test). Lives in the
# sandbox's private TMUX_TMPDIR, so it can never collide with a real server.
E2E_DRIVER_SOCKET="agent-e2e-driver"
E2E_DRIVER_SESSION="agent-e2e"

E2E_MODE="test"
E2E_STUB_PID=""
E2E_TAPE=""

e2e_log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
e2e_die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; return 1; }

# ---------------------------------------------------------------------------
# Tool preflight. The agent binary is checked per-profile (missing agent =>
# bats `skip`, so the suite stays green on machines without it); missing
# infrastructure tools are hard errors (you explicitly invoked this suite).
e2e_require_tools() {
    local mode="${1:-test}" missing=""
    local tools="tmux node jq git curl"
    [ "$mode" = "demo" ] && tools="$tools vhs sqlite3"
    for t in $tools; do
        command -v "$t" >/dev/null 2>&1 || missing="$missing $t"
    done
    [ -z "$missing" ] || e2e_die "missing required tool(s):$missing"
}

# ---------------------------------------------------------------------------
# Scenario loading. A scenario is a directory with:
#   scenario.sh    SCENARIO_* metadata + scenario_steps() + scenario_assert()
#   fixtures.json  the stub's semantic model script ({{WS}} is substituted)
#   workspace/     optional seed files for the git workspace
e2e_scenario_load() {
    E2E_SCENARIO_DIR="$1"
    [ -f "$E2E_SCENARIO_DIR/scenario.sh" ] \
        || e2e_die "no scenario.sh in $E2E_SCENARIO_DIR" || return 1
    E2E_SCENARIO_NAME="$(basename "$E2E_SCENARIO_DIR")"
    # defaults a scenario.sh may override
    SCENARIO_AGENT="claude"
    SCENARIO_COLS=120
    SCENARIO_ROWS=40
    SCENARIO_PRECREATE=1
    SCENARIO_REQUIRE_ALL_FIXTURES=1
    SCENARIO_DEMO_THEME=""
    SCENARIO_PROMPT=""
    SCENARIO_AGENT_READY=""
    SCENARIO_DONE_PATTERN=""
    scenario_assert_effects() { :; }
    scenario_assert_ui() { :; }
    # shellcheck disable=SC1091
    source "$E2E_SCENARIO_DIR/scenario.sh"
}

# ---------------------------------------------------------------------------
# Boot: sandbox -> workspace -> stub -> agent profile -> agents.toml -> hooks
# -> session -> (test mode) TUI in driver tmux.
e2e_boot() {
    E2E_MODE="${1:-test}"

    # shellcheck disable=SC2034  # consumed by sandbox-env.sh when sourced
    TBX_REPO_ROOT="$REPO_ROOT"
    # shellcheck disable=SC1091
    source "$REPO_ROOT/scripts/dev/lib/sandbox-env.sh"
    tbx_sandbox_init_full fresh
    # init_full unsets THURBOX_{CONFIG,DATA}_DIR, but a shell running inside a
    # Friring session also carries identity/session vars — those would leak
    # into the tmux server env and misattribute hook signals.
    unset THURBOX_SESSION THURBOX_SESSION_ID THURBOX_TASK THURBOX_METRICS_DIR THURBOX_SOCKET

    THURBOX_BIN="$REPO_ROOT/target/debug/thurbox"
    export THURBOX_BIN
    if [ "$E2E_MODE" != "protocol" ]; then
        [ -x "$THURBOX_BIN" ] || e2e_die "build first: cargo build --bins" || return 1
    fi

    # Fresh HOME has no git identity; sessions and scenario workspaces need one.
    git config --global user.name "thurbox-e2e"
    git config --global user.email "e2e@thurbox.invalid"
    git config --global init.defaultBranch main

    # Workspace the agent works in (the session's repo).
    E2E_WS="$TBX_SANDBOX_ROOT/ws"
    mkdir -p "$E2E_WS"
    if [ -d "$E2E_SCENARIO_DIR/workspace" ]; then
        cp -r "$E2E_SCENARIO_DIR/workspace/." "$E2E_WS/"
    fi
    ( cd "$E2E_WS" && git init -q && git add -A \
        && git commit -qm "e2e seed" --allow-empty )

    # Agent profile first (it names the stub dialect), then the stub, then the
    # profile env (which needs the stub URL).
    local profile="$AGENT_E2E_DIR/agents/$SCENARIO_AGENT/profile.sh"
    [ -f "$profile" ] || e2e_die "no agent profile: $profile" || return 1
    # shellcheck disable=SC1090
    source "$profile"

    if [ "$AGENT_STUB_DIALECT" = "none" ]; then
        e2e_die "agent '$AGENT_NAME' declares itself unstubbable; scenario cannot run offline"
        return 1
    fi
    e2e_stub_start "$AGENT_STUB_DIALECT" || return 1

    # Export the profile env NOW — before anything that can start a tmux
    # server. tmux panes inherit the *server* environment, and the server
    # inherits ours; this is how ANTHROPIC_BASE_URL reaches the agent process
    # with zero thurbox changes.
    local kv
    while IFS= read -r kv; do
        [ -n "$kv" ] && export "${kv?}"
    done < <(agent_env)
    agent_seed_config "$E2E_WS"

    # Protocol/interactive smokes stop here: no Friring in that loop.
    [ "$E2E_MODE" = "protocol" ] && return 0

    # agents.toml under the dev build's config dir (dev_build XDG subdir
    # scheme — same one record.sh uses).
    local cfg_dir="$XDG_CONFIG_HOME/thurbox-dev"
    mkdir -p "$cfg_dir"
    {
        echo 'default = "'"$AGENT_NAME"'"'
        agent_agents_toml_entry
    } > "$cfg_dir/agents.toml"

    # Headless `session create` does NOT wire the built-in hooks extension
    # (only the TUI boot and the extension CLI verbs do), so activate it
    # explicitly — this patches the claude agent's args with the --settings
    # hook file that makes status signals (working/done) fire.
    thurbox-cli extension activate hooks >/dev/null \
        || e2e_die "extension activate hooks failed" || return 1

    if [ "$SCENARIO_PRECREATE" = "1" ]; then
        e2e_session_create || return 1
    fi

    if [ "$E2E_MODE" = "test" ]; then
        # 3>&- keeps daemonizing children (tmux server) from holding bats' fd 3
        # open, which would hang the run.
        tmux -L "$E2E_DRIVER_SOCKET" new-session -d -s "$E2E_DRIVER_SESSION" \
            -x "$SCENARIO_COLS" -y "$SCENARIO_ROWS" "$THURBOX_BIN" 3>&-
        # "thurbox" is the header brand every build paints (the fork keeps the
        # binary name); a single literal also stays portable across greps.
        e2e_wait_pane "thurbox" 100 \
            || e2e_die "TUI did not boot" || return 1
    fi
}

e2e_stub_start() {
    local dialect="$1"
    local stub="$AGENT_E2E_DIR/stub/$dialect-stub.mjs"
    [ -f "$stub" ] || e2e_die "no stub for dialect '$dialect'" || return 1

    E2E_STUB_DIR="$TBX_SANDBOX_ROOT/stub"
    E2E_JOURNAL="$E2E_STUB_DIR/journal.jsonl"
    mkdir -p "$E2E_STUB_DIR/raw"

    # {{WS}} lets fixtures pin absolute tool inputs (Write wants an absolute
    # file_path) without knowing the throwaway workspace path in advance.
    E2E_FIXTURES="$E2E_STUB_DIR/fixtures.json"
    sed "s|{{WS}}|$E2E_WS|g" "$E2E_SCENARIO_DIR/fixtures.json" > "$E2E_FIXTURES"

    node "$stub" --port 0 --port-file "$E2E_STUB_DIR/port" \
        --journal "$E2E_JOURNAL" --raw-dir "$E2E_STUB_DIR/raw" \
        --fixtures "$E2E_FIXTURES" > "$E2E_STUB_DIR/stub.log" 2>&1 3>&- &
    E2E_STUB_PID=$!

    for _ in $(seq 1 50); do
        [ -s "$E2E_STUB_DIR/port" ] && break
        sleep 0.1
    done
    [ -s "$E2E_STUB_DIR/port" ] || e2e_die "stub did not start (see $E2E_STUB_DIR/stub.log)" || return 1
    AGENT_E2E_STUB_URL="http://127.0.0.1:$(cat "$E2E_STUB_DIR/port")"
    export AGENT_E2E_STUB_URL
    curl -sf --noproxy '*' "$AGENT_E2E_STUB_URL/health" >/dev/null \
        || e2e_die "stub health check failed" || return 1
}

e2e_session_create() {
    local out
    # 3>&-: this is what boots the thurbox-dev tmux server (bats fd-3 guard,
    # see the TUI launch above).
    out="$(thurbox-cli --json session create --name "$E2E_SCENARIO_NAME" \
        --repo-path "$E2E_WS" --agent "$AGENT_NAME" 3>&-)" \
        || e2e_die "session create failed: $out" || return 1
    E2E_SESSION_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_SESSION_ID" ] && [ "$E2E_SESSION_ID" != "null" ] \
        || e2e_die "no session id in: $out" || return 1
}

# ---------------------------------------------------------------------------
# Step primitives — the scenario's shared vocabulary. Test mode drives the
# driver tmux and polls; demo mode appends VHS tape lines.

# tmux key name -> VHS key name. VHS has no F-keys, so scenarios that want a
# demo must stick to this subset (F-keys still work in test mode).
_vhs_key() {
    case "$1" in
        Enter|Escape|Tab|Space|Up|Down|Left|Right|PageUp|PageDown|Backspace|Delete) echo "$1" ;;
        C-?) echo "Ctrl+${1#C-}" ;;
        *) return 1 ;;
    esac
}

step_type() {
    if [ "$E2E_MODE" = "demo" ]; then
        local esc="${1//\\/\\\\}"
        esc="${esc//\"/\\\"}"
        printf 'Type "%s"\n' "$esc" >> "$E2E_TAPE"
    else
        tmux -L "$E2E_DRIVER_SOCKET" send-keys -t "$E2E_DRIVER_SESSION" -l -- "$1"
    fi
}

step_key() {
    if [ "$E2E_MODE" = "demo" ]; then
        local vk
        vk="$(_vhs_key "$1")" || e2e_die "key '$1' has no VHS mapping (demo mode)" || return 1
        printf '%s\n' "$vk" >> "$E2E_TAPE"
    else
        tmux -L "$E2E_DRIVER_SOCKET" send-keys -t "$E2E_DRIVER_SESSION" "$1"
    fi
}

# Pacing only, and structurally so: test mode ignores sleeps entirely, which
# makes it impossible for a scenario to lean on a fixed sleep for
# synchronization (waits are the only sync primitive). Demo mode honors them
# as recording rhythm.
step_sleep() {
    if [ "$E2E_MODE" = "demo" ]; then
        printf 'Sleep %ss\n' "$1" >> "$E2E_TAPE"
    fi
}

# Wait until the rendered TUI shows $1 (grep pattern in test mode; VHS
# Wait+Screen regex in demo mode). The one synchronization primitive both
# modes share — no open-loop sleeps around agent latency.
step_wait_pane() {
    local pattern="$1" timeout="${2:-30}"
    if [ "$E2E_MODE" = "demo" ]; then
        # VHS wants Go regexp; escape characters that would change meaning.
        local esc
        # shellcheck disable=SC2016  # sed class, not an unexpanded variable
        esc="$(printf '%s' "$pattern" | sed 's/[.[\*^$()+?{|]/\\&/g')"
        printf 'Wait+Screen@%ss /%s/\n' "$timeout" "$esc" >> "$E2E_TAPE"
    else
        e2e_wait_pane "$pattern" "$((timeout * 10))" \
            || e2e_die "timed out waiting for pane: $pattern"
    fi
}

# Wait for the session's persisted hook state (sessions.hook_state — written
# by the agent's status hook via `thurbox-cli session signal`, readable
# without the TUI). `want` may be an alternation ('working|done'): the column
# is overwritten in place, so a transient state can flip between two polls —
# waiting on a transient alone is a latent race; accept the successor state
# too. Status hooks are a per-agent capability, not a framework guarantee —
# the profile must declare AGENT_HAS_STATUS_HOOKS=1 to use this. Demo mode
# has no DB probe; the state flip has no fixed visual anchor, so pace with a
# short sleep instead.
step_wait_state() {
    local want="$1" timeout="${2:-30}"
    if [ "$E2E_MODE" = "demo" ]; then
        printf 'Sleep %ss\n' "${3:-2}" >> "$E2E_TAPE"
        return 0
    fi
    [ "${AGENT_HAS_STATUS_HOOKS:-0}" = "1" ] \
        || e2e_die "agent '$AGENT_NAME' does not declare status hooks; step_wait_state unusable" \
        || return 1
    for _ in $(seq 1 "$((timeout * 5))"); do
        [[ "$(e2e_hook_state)" =~ ^($want)$ ]] && return 0
        sleep 0.2
    done
    e2e_die "timed out waiting for hook_state=$want (last: '$(e2e_hook_state)')"
}

# ---------------------------------------------------------------------------
# Test-mode inspection + assertion API (scenario_assert uses these).

e2e_pane() { tmux -L "$E2E_DRIVER_SOCKET" capture-pane -p -t "$E2E_DRIVER_SESSION"; }

e2e_wait_pane() {
    local pattern="$1" tries="${2:-50}"
    for _ in $(seq 1 "$tries"); do
        e2e_pane | grep -q "$pattern" && return 0
        sleep 0.1
    done
    return 1
}

e2e_hook_state() {
    thurbox-cli --json session get "$E2E_SESSION_ID" 2>/dev/null \
        | jq -r '.hook_state // empty'
}

assert_pane_contains() {
    e2e_pane | grep -qF -- "$1" \
        || e2e_die "pane does not contain: $1
--- pane ---
$(e2e_pane)"
}

assert_ws_file_eq() {
    local f="$E2E_WS/$1"
    [ -f "$f" ] || e2e_die "workspace file missing: $1" || return 1
    local got
    got="$(cat "$f")"
    [ "$got" = "$2" ] || e2e_die "workspace file $1 mismatch: got '$got' want '$2'"
}

journal_matched() { jq -rs "[.[] | select(.matched == \"$1\")] | length" "$E2E_JOURNAL"; }

# Strict-offline invariants every scenario gets for free: the stub answered
# every model call from a fixture (no UNMATCHED), and every non-ambient
# fixture was actually exercised (a dead fixture usually means the flow the
# scenario describes silently didn't happen).
assert_stub_invariants() {
    local unmatched
    unmatched="$(jq -rs '[.[] | select(.matched == "UNMATCHED")] | length' "$E2E_JOURNAL")"
    [ "$unmatched" = "0" ] \
        || e2e_die "stub saw $unmatched unmatched model call(s) — see journal" || return 1
    [ "$SCENARIO_REQUIRE_ALL_FIXTURES" = "1" ] || return 0
    local name
    while IFS= read -r name; do
        [ "$(journal_matched "$name")" -ge 1 ] \
            || e2e_die "fixture '$name' was never matched" || return 1
    done < <(jq -r '.responses[] | select(.ambient != true) | .name' "$E2E_FIXTURES")
}

# ---------------------------------------------------------------------------
# Demo mode: emit a record.sh-compatible tape from the same scenario steps,
# then run vhs inside the (already exported) hermetic env.
e2e_demo_record() {
    local out_dir="$REPO_ROOT/target/agent-e2e/demos"
    mkdir -p "$out_dir"
    E2E_TAPE="$TBX_SANDBOX_ROOT/$E2E_SCENARIO_NAME.tape"

    if [ -n "$SCENARIO_DEMO_THEME" ]; then
        sqlite3 "$XDG_DATA_HOME/thurbox-dev/thurbox.db" \
            "INSERT INTO metadata (key, value) VALUES ('active_theme', '$SCENARIO_DEMO_THEME')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
    fi

    # Same Set block as scripts/demo/*.tape so generated demos match the
    # hand-written ones frame-for-frame in styling. Output paths are relative
    # (vhs runs from the repo root, and its parser rejects absolute paths).
    cat > "$E2E_TAPE" <<EOF
Output target/agent-e2e/demos/$E2E_SCENARIO_NAME.gif
Output target/agent-e2e/demos/$E2E_SCENARIO_NAME.mp4

Set Shell "bash"
Set FontSize 18
Set Width 1920
Set Height 1080
Set Padding 16
Set Theme "Catppuccin Mocha"
Set PlaybackSpeed 1.0
Set WaitTimeout 60s

Hide
Type \`exec "\$THURBOX_BIN"\`
Enter
Sleep 2s
Show
Sleep 1s
EOF
    scenario_steps || return 1
    # Closing beat: linger, then quit so the recording ends on a clean frame.
    cat >> "$E2E_TAPE" <<'EOF'
Sleep 2s
Ctrl+Q
Sleep 1s
EOF
    e2e_log "recording $E2E_SCENARIO_NAME ($(basename "$E2E_TAPE"))"
    # vhs renders through a headless Chromium (go-rod): use the system browser
    # if present, else let rod download one into a cache that survives the
    # throwaway sandbox (XDG_CACHE_HOME points into the sandbox). Dropping the
    # dead-proxy vars here does NOT weaken the agent's offline guarantee — the
    # agent pane env was frozen into the thurbox-dev tmux server at session
    # create, before vhs starts; only vhs's own tooling gets network.
    local vhs_cache="$REPO_ROOT/target/agent-e2e/cache"
    mkdir -p "$vhs_cache"
    ( cd "$REPO_ROOT" && env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
        XDG_CACHE_HOME="$vhs_cache" vhs "$E2E_TAPE" )
}

# ---------------------------------------------------------------------------
# The three drive depths, all fed by the same scenario directory. When a full
# scenario fails, the shallower layers localize the break: protocol = the
# binary↔stub contract, interactive = the agent's own TUI vs the stub,
# full = Friring's rendering/keying/hooks on top.

# Depth 1 — protocol: `claude -p` (print mode), no tmux, no Friring.
e2e_protocol_smoke() {
    e2e_scenario_load "$1" || return 1
    e2e_boot protocol || return 1
    local bin out
    bin="$(agent_binary)"
    ( cd "$E2E_WS" && timeout 120 "$bin" -p "$SCENARIO_PROMPT" \
        "${AGENT_LAUNCH_ARGS[@]}" > "$TBX_SANDBOX_ROOT/p-stdout.txt" 2>&1 ) \
        || { out="$(cat "$TBX_SANDBOX_ROOT/p-stdout.txt")"; \
             e2e_die "claude -p failed: $out"; return 1; }
    assert_stub_invariants || return 1
    scenario_assert_effects || return 1
}

# Depth 2 — interactive: the agent's own TUI in a bare tmux pane, still no
# Friring in the loop.
e2e_interactive_smoke() {
    e2e_scenario_load "$1" || return 1
    e2e_boot protocol || return 1
    local bin
    bin="$(agent_binary)"
    tmux -L "$E2E_DRIVER_SOCKET" new-session -d -s "$E2E_DRIVER_SESSION" \
        -x "$SCENARIO_COLS" -y "$SCENARIO_ROWS" -c "$E2E_WS" \
        "$bin" "${AGENT_LAUNCH_ARGS[@]}" 3>&-
    e2e_wait_pane "$SCENARIO_AGENT_READY" 300 \
        || e2e_die "agent TUI did not become ready" || return 1
    step_type "$SCENARIO_PROMPT"
    sleep 0.3
    step_key Enter
    step_wait_pane "$SCENARIO_DONE_PATTERN" 60 || return 1
    assert_stub_invariants || return 1
    scenario_assert_effects || return 1
}

# Depth 3 — full: the scenario as written, through the Friring TUI.
e2e_scenario() {
    e2e_scenario_load "$1" || return 1
    e2e_boot test || return 1
    scenario_steps || return 1
    assert_stub_invariants || return 1
    scenario_assert_effects || return 1
    scenario_assert_ui || return 1
}

# ---------------------------------------------------------------------------
# Artifacts + teardown. Teardown must reap every child even on failure:
# the stub, the driver tmux server, the thurbox-dev server and the agent
# processes inside it (killed with the server), then the throwaway root.
e2e_collect_artifacts() {
    local dest="$REPO_ROOT/target/agent-e2e/artifacts/$E2E_SCENARIO_NAME-$(date +%Y%m%d-%H%M%S)"
    mkdir -p "$dest"
    {
        echo "scenario: $E2E_SCENARIO_NAME"
        echo "agent: ${AGENT_NAME:-?} $(agent_version 2>/dev/null || true)"
        echo "thurbox: $("$THURBOX_BIN" --version 2>/dev/null || true)"
        echo "tmux: $(tmux -V)"
        echo "session: ${E2E_SESSION_ID:-none} hook_state: $(e2e_hook_state 2>/dev/null || true)"
    } > "$dest/meta.txt" 2>/dev/null
    e2e_pane > "$dest/driver-pane.txt" 2>/dev/null || true
    tmux -L "$E2E_DRIVER_SOCKET" capture-pane -p -S -200 -t "$E2E_DRIVER_SESSION" \
        > "$dest/driver-pane-history.txt" 2>/dev/null || true
    # The agent's own pane, straight from the thurbox-dev server.
    thurbox-cli --json session capture "$E2E_SESSION_ID" --lines 500 \
        > "$dest/agent-pane.json" 2>/dev/null || true
    cp "$E2E_JOURNAL" "$dest/" 2>/dev/null || true
    cp -r "$E2E_STUB_DIR/raw" "$dest/" 2>/dev/null || true
    cp "$E2E_STUB_DIR/stub.log" "$dest/" 2>/dev/null || true
    cp "$E2E_FIXTURES" "$dest/" 2>/dev/null || true
    cp "$XDG_CONFIG_HOME/thurbox-dev/agents.toml" "$dest/" 2>/dev/null || true
    ( cd "$E2E_WS" 2>/dev/null && { git status --short; git diff; } > "$dest/workspace.diff" ) || true
    env | grep -E '^(THURBOX|ANTHROPIC|CLAUDE|XDG|HOME|no_proxy|http_proxy)' \
        | sed -E 's/((TOKEN|KEY|SECRET|PASSWORD)=).*/\1<redacted>/' > "$dest/env.txt" 2>/dev/null || true
    e2e_log "failure artifacts: $dest"
}

e2e_teardown() {
    local failed="${1:-0}"
    [ "$failed" != "0" ] && e2e_collect_artifacts
    tmux -L "$E2E_DRIVER_SOCKET" kill-server >/dev/null 2>&1 || true
    [ -n "$E2E_STUB_PID" ] && kill "$E2E_STUB_PID" >/dev/null 2>&1 || true
    if [ "${THURBOX_E2E_KEEP:-0}" = "1" ]; then
        e2e_log "THURBOX_E2E_KEEP=1: sandbox left at $TBX_SANDBOX_ROOT"
        # shellcheck disable=SC2034  # read by tbx_sandbox_teardown
        TBX_SANDBOX_FRESH=0
    fi
    tbx_sandbox_teardown 2>/dev/null || true
}
