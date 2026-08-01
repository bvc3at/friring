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
# ~/.config/friring, or any real tmux server.

: "${AGENT_E2E_DIR:?source suite.bats/run.sh sets AGENT_E2E_DIR}"
REPO_ROOT="$(cd "$AGENT_E2E_DIR/../../.." && pwd)"

# Driver tmux socket (hosts the friring TUI under test). Lives in the
# sandbox's private TMUX_TMPDIR, so it can never collide with a real server.
E2E_DRIVER_SOCKET="agent-e2e-driver"
E2E_DRIVER_SESSION="agent-e2e"

E2E_MODE="test"
E2E_STUB_PID=""
E2E_TAPE=""
E2E_TAPE_ERR=""

e2e_log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
e2e_die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; return 1; }

# Print an agent binary's version line, BOUNDED. Profiles' `agent_version` all
# route through this because it runs on paths that must never block: teardown
# and artifact collection run for every test (including failures), and a
# wedged agent binary would otherwise hang the whole suite with no output
# rather than failing one test. (Seen in the wild: a large CLI stalling
# indefinitely in macOS's dynamic loader, so even `--version` never returns.)
e2e_bin_version() {
    "${E2E_TIMEOUT:-timeout}" 10 "$1" --version 2>/dev/null | head -1
}

# ---------------------------------------------------------------------------
# Tool preflight. The agent binary is checked per-profile (missing agent =>
# bats `skip`, so the suite stays green on machines without it); missing
# infrastructure tools are hard errors (you explicitly invoked this suite).
e2e_require_tools() {
    local mode="${1:-test}" missing=""
    # coreutils timeout bounds the protocol-smoke runs. Prefer `gtimeout` (the
    # coreutils name on macOS) over `timeout`: third-party `timeout` shims
    # exist in the wild (e.g. sysadmin-util's shell script) and silently break
    # TUI child processes.
    if command -v gtimeout >/dev/null 2>&1; then
        E2E_TIMEOUT="gtimeout"
    elif command -v timeout >/dev/null 2>&1; then
        E2E_TIMEOUT="timeout"
    else
        missing=" timeout"
    fi
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
    # Every generated demo films the same theme, so a set of clips reads as one
    # product rather than a screenshot pile. A scenario overrides it only when
    # the theme itself is the subject (scripted-theme-settings picks its own).
    SCENARIO_DEMO_THEME="doom"
    # Demo-mode key substitutions, `<tmux key>=<tmux key>…`, for keys VHS
    # cannot express (F-keys, `Alt+<digit>`, `Ctrl+/`). The fork's leader is
    # usually the equivalent route — `F9=C-f v` reaches the activity view the
    # way `<leader> v` does — but whether a substitution preserves what the
    # clip *shows* is a judgement the scenario has to make, never one the
    # harness may assume: scripted-info-keybind presses F2 to prove it does
    # nothing, so routing it to the same action's leader key would record the
    # opposite of the feature. Declaring nothing leaves the scenario
    # test-only, and `--demo` names the key that stopped it.
    SCENARIO_DEMO_KEYS=()
    SCENARIO_PROMPT=""
    SCENARIO_AGENT_READY=""
    SCENARIO_DONE_PATTERN=""
    SCENARIO_PERF=0
    # Extra dirs the agent profile must pre-trust (beyond $E2E_WS) — a
    # scenario whose agent launches outside the seed workspace (e.g. a named
    # multi-repo workspace dir) fills this from scenario_setup().
    SCENARIO_TRUST_DIRS=()
    E2E_PERF_MARKS=""
    scenario_prepare() { :; }
    # Optional boot hook: runs after the seed workspace exists but before the
    # stub / agent config, so a scenario can lay down extra repos or dirs.
    scenario_setup() { :; }
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
    # init_full unsets FRIRING_{CONFIG,DATA}_DIR, but a shell running inside a
    # Friring session also carries identity/session vars — those would leak
    # into the tmux server env and misattribute hook signals.
    unset FRIRING_SESSION FRIRING_SESSION_ID FRIRING_TASK FRIRING_METRICS_DIR FRIRING_SOCKET

    # Hermeticity for the Ctrl+T shell pane: it launches the inherited $SHELL,
    # which resolves its startup + history files relative to $HOME (sandboxed)
    # — UNLESS one of these points at an absolute path outside it. Scrub them
    # so the shell can only ever read/write inside the throwaway HOME (done
    # before any tmux server starts, so panes never inherit them).
    unset ZDOTDIR ENV BASH_ENV HISTFILE

    # FRIRING_E2E_BIN points perf runs at a release build — timing numbers
    # from an unoptimized debug binary are noise, not measurements.
    FRIRING_BIN="${FRIRING_E2E_BIN:-$REPO_ROOT/target/debug/friring}"
    export FRIRING_BIN
    if [ "$E2E_MODE" != "protocol" ]; then
        [ -x "$FRIRING_BIN" ] \
            || e2e_die "no TUI binary at $FRIRING_BIN (cargo build --bins, or set FRIRING_E2E_BIN)" \
            || return 1
    fi

    # Fresh HOME has no git identity; sessions and scenario workspaces need one.
    git config --global user.name "friring-e2e"
    git config --global user.email "e2e@friring.invalid"
    git config --global init.defaultBranch main

    # Workspace the agent works in (the session's repo). The sandbox root is
    # already canonical (sandbox-env.sh), which the agents' folder-trust seeds
    # depend on.
    E2E_WS="$TBX_SANDBOX_ROOT/ws"
    mkdir -p "$E2E_WS"
    if [ -d "$E2E_SCENARIO_DIR/workspace" ]; then
        cp -r "$E2E_SCENARIO_DIR/workspace/." "$E2E_WS/"
    fi
    ( cd "$E2E_WS" && git init -q && git add -A \
        && git commit -qm "e2e seed" --allow-empty )

    # Scenario-owned extra environment (second repos, trust dirs, …) — after
    # the seed workspace, before anything reads its results (fixtures may pin
    # {{ROOT}} paths; agent_seed_config trusts SCENARIO_TRUST_DIRS).
    scenario_setup || return 1

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
    # with zero friring changes.
    local kv
    while IFS= read -r kv; do
        [ -n "$kv" ] && export "${kv?}"
    done < <(agent_env)
    # ${arr[@]+…}: safe empty-array expansion under set -u on bash 3.2 (macOS).
    agent_seed_config "$E2E_WS" ${SCENARIO_TRUST_DIRS[@]+"${SCENARIO_TRUST_DIRS[@]}"}

    # tmux config the agent panes inherit, written before any server starts (a
    # server reads ~/.tmux.conf once, at start, and $HOME is the sandbox).
    # Mirrors scripts/demo/record.sh, and for the same reason: without
    # focus-events the agent's terminal never learns it has focus, so Claude
    # Code paints a "tmux focus-events off · add 'set -g focus-events on' to
    # ~/.tmux.conf" hint across its pane — noise in every artifact, and filmed
    # in every clip.
    # Only focus-events: record.sh also pins `default-terminal tmux-256color`,
    # but under it codex boots to a permanently blank pane here, and the hint
    # this is here to remove needs nothing but focus-events.
    printf 'set -g focus-events on\n' > "$HOME/.tmux.conf"

    # Perf scenarios make the TUI publish its perf snapshot (counters +
    # frame/tick percentiles) into the sandbox DB for `friring-cli perf`.
    # Exported before the tmux servers start, like the agent env.
    [ "$SCENARIO_PERF" = "1" ] && export FRIRING_PERF_LOG=1

    # Protocol/interactive smokes stop here: no Friring in that loop.
    [ "$E2E_MODE" = "protocol" ] && return 0

    # agents.toml under the dev build's config dir (dev_build XDG subdir
    # scheme — same one record.sh uses).
    local cfg_dir="$XDG_CONFIG_HOME/friring-dev"
    mkdir -p "$cfg_dir"
    {
        echo 'default = "'"$AGENT_NAME"'"'
        agent_agents_toml_entry
    } > "$cfg_dir/agents.toml"

    # Hermeticity: a session flipping to Blocked would otherwise fire a REAL
    # desktop notification on the host ([features] notifications defaults to
    # true; macOS delivers via osascript/terminal-notifier). Tests must never
    # touch the user's desktop. Scenarios that exercise settings behavior may
    # rewrite this file, but must keep notifications off.
    cat > "$cfg_dir/settings.toml" <<'EOF'
[features]
notifications = false
EOF

    # Headless `session create` does NOT wire the built-in hooks extension
    # (only the TUI boot and the extension CLI verbs do), so activate it
    # explicitly — this patches the claude agent's args with the --settings
    # hook file that makes status signals (working/done) fire.
    friring-cli extension activate hooks >/dev/null \
        || e2e_die "extension activate hooks failed" || return 1

    if [ "$SCENARIO_PRECREATE" = "1" ]; then
        e2e_session_create || return 1
    fi

    if [ "$E2E_MODE" = "test" ]; then
        # 3>&- keeps daemonizing children (tmux server) from holding bats' fd 3
        # open, which would hang the run.
        tmux -L "$E2E_DRIVER_SOCKET" new-session -d -s "$E2E_DRIVER_SESSION" \
            -x "$SCENARIO_COLS" -y "$SCENARIO_ROWS" "$FRIRING_BIN" 3>&-
        # "friring" is the brand every build paints in the header bar; a single
        # literal also stays portable across greps.
        e2e_wait_pane "friring" 100 \
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

    # {{WS}} / {{ROOT}} let fixtures pin absolute tool inputs (Write wants an
    # absolute file_path) without knowing the throwaway paths in advance:
    # {{WS}} = the seed workspace repo, {{ROOT}} = the sandbox root (for
    # scenario_setup-created dirs beside it).
    E2E_FIXTURES="$E2E_STUB_DIR/fixtures.json"
    sed -e "s|{{WS}}|$E2E_WS|g" -e "s|{{ROOT}}|$TBX_SANDBOX_ROOT|g" \
        "$E2E_SCENARIO_DIR/fixtures.json" > "$E2E_FIXTURES"

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
    # 3>&-: this is what boots the friring-dev tmux server (bats fd-3 guard,
    # see the TUI launch above).
    out="$(friring-cli --json session create --name "$E2E_SCENARIO_NAME" \
        --repo-path "$E2E_WS" --agent "$AGENT_NAME" 3>&-)" \
        || e2e_die "session create failed: $out" || return 1
    E2E_SESSION_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_SESSION_ID" ] && [ "$E2E_SESSION_ID" != "null" ] \
        || e2e_die "no session id in: $out" || return 1
}

# ---------------------------------------------------------------------------
# Step primitives — the scenario's shared vocabulary. Test mode drives the
# driver tmux and polls; demo mode appends VHS tape lines.

# One tmux key name -> the VHS tape line that presses it. Prints the line;
# returns 1 for a key VHS cannot press, which needs a SCENARIO_DEMO_KEYS route
# or leaves the scenario test-only.
#
# The accepted set was established by capturing what vhs 0.11.0 actually writes
# to a pty (`stty raw; cat > file`), not from its grammar — the two disagree,
# and only one of them is what the app receives:
#
#   Ctrl+<letter>       0x01-0x1a          ✔
#   Ctrl+^ \ [ ] - @ .  the control byte   ✔  (no other punctuation; no `Ctrl+/`)
#   Shift+<letter>      the capital        ✔  (so a bare `J` covers it)
#   Escape Tab Enter …  ✔
#   Alt+<letter>        the BARE CAPITAL — no ESC prefix. Accepted by the
#                       parser, so a mapping here would silently type `U` into
#                       the agent instead of unloading a session.
#   Ctrl+Alt+<letter>   NOTHING AT ALL — parsed, then dropped.
#   Alt+<digit>, F-keys rejected by the parser.
#
# So every Alt chord demos through the fork's leader (`<leader> U` for Alt+U),
# which is a real second route to the same action rather than a workaround.
_vhs_key_line() {
    case "$1" in
        Enter|Escape|Tab|Space|Up|Down|Left|Right|PageUp|PageDown|Backspace|Delete|Insert)
            printf '%s\n' "$1" ;;
        # Uppercase the letter: VHS's canonical chord form is `Ctrl+N`.
        C-[a-zA-Z]) printf 'Ctrl+%s\n' "$(printf '%s' "${1#C-}" | tr '[:lower:]' '[:upper:]')" ;;
        'C-^'|'C-['|'C-]'|'C--'|'C-@'|'C-.'|C-[\\]) printf 'Ctrl+%s\n' "${1#C-}" ;;
        # A bare key is that character typed — including the uppercase letters
        # tmux uses for `Shift+<letter>` (`J`). Single character only: a longer
        # name is one VHS lacks (F-keys, Home/End), and typing it literally
        # would silently record nonsense.
        ?) printf 'Type "%s"\n' "$1" ;;
        *) return 1 ;;
    esac
}

# The tape lines for a key, honoring the scenario's demo-mode substitutions.
_vhs_key_lines() {
    local want="$1" entry sub="" k
    for entry in ${SCENARIO_DEMO_KEYS[@]+"${SCENARIO_DEMO_KEYS[@]}"}; do
        [ "${entry%%=*}" = "$want" ] && sub="${entry#*=}"
    done
    [ -n "$sub" ] || { _vhs_key_line "$want"; return $?; }
    for k in $sub; do
        _vhs_key_line "$k" || return 1
    done
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
        local lines
        # A scenario's steps are a flat list with no error checking, so a
        # `return 1` here would be swallowed and the keypress would simply go
        # missing from the tape — a recording that runs to completion showing
        # the wrong thing. Flag it for e2e_emit_tape to fail on instead.
        lines="$(_vhs_key_lines "$1")" || {
            E2E_TAPE_ERR="${E2E_TAPE_ERR}key '$1' has no VHS mapping and no SCENARIO_DEMO_KEYS route"$'\n'
            return 1
        }
        printf '%s\n' "$lines" >> "$E2E_TAPE"
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
        # Test mode greps (POSIX BRE); VHS matches a Go (RE2) regexp delimited
        # by /…/. Translate rather than flatten to a literal: the two dialects
        # already agree on everything these patterns use — `.`, `.*`, and the
        # `\[`/`\]` a BRE needs for a literal bracket are spelled the same in
        # RE2 — so escaping wholesale silently broke every wait a scenario
        # meant as a regex. Only the characters BRE takes literally and RE2
        # does not need escaping, or a pane title like `Edited (1)` becomes a
        # capture group matching `Edited 1`. `/` is escaped because it
        # delimits. A pattern with a bare unbalanced `[` would still make
        # invalid RE2 — vhs then fails the recording loudly, which is the
        # right failure.
        local esc
        esc="$(printf '%s' "$pattern" | sed 's#[/+?(){}|]#\\&#g')"
        # vhs captures no frames while `Wait` blocks — a tape that synchronizes
        # on ten waits and sleeps twice records as a two-second jump-cut past
        # everything the app was doing. The wait still does the synchronizing;
        # this dwell is what puts the state it waited for on screen. Keep it
        # under the 1s max-held-frame budget: consecutive waits that land on
        # one screen add up, and only a `step_sleep` should ever hold longer.
        printf 'Wait+Screen@%ss /%s/\nSleep 600ms\n' "$timeout" "$esc" >> "$E2E_TAPE"
    else
        e2e_wait_pane "$pattern" "$((timeout * 10))" \
            || e2e_die "timed out waiting for pane: $pattern"
    fi
}

# Wait for the session's persisted hook state (sessions.hook_state — written
# by the agent's status hook via `friring-cli session signal`, readable
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

# Resolve E2E_SESSION_ID for a session the *steps* created through the TUI (a
# SCENARIO_PRECREATE=0 wizard flow), so step_wait_state / CLI probes can
# address it. Test mode only — a demo has no DB probe (mirrors
# step_wait_state), and the id is only meaningful to asserts anyway.
step_resolve_session() {
    local name="$1" timeout="${2:-15}" id=""
    [ "$E2E_MODE" = "test" ] || return 0
    for _ in $(seq 1 "$((timeout * 5))"); do
        id="$(friring-cli --json session list 2>/dev/null \
            | jq -r --arg n "$name" '[.[] | select(.name == $n)] | first | .id // empty')"
        [ -n "$id" ] && break
        sleep 0.2
    done
    [ -n "$id" ] || e2e_die "session '$name' never appeared in the DB" || return 1
    E2E_SESSION_ID="$id"
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
    friring-cli --json session get "$E2E_SESSION_ID" 2>/dev/null \
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
# Perf capture (SCENARIO_PERF=1). Reports are benchmarks, not gates: the
# scenario still passes/fails on its functional asserts; the report records
# wall-clock marks + the TUI's own perf snapshot for a human (or a trend
# script) to compare across runs. Resolution of marks is bounded by the
# wait-poll interval (~100ms).

perf_mark() {
    [ "$E2E_MODE" = "test" ] || return 0
    E2E_PERF_MARKS="${E2E_PERF_MARKS}$1 $(date +%s%3N)"$'\n'
}

# Write marks + the TUI-published snapshot to target/agent-e2e/perf/. The
# snapshot only publishes once per perf window (~1000 ticks ≈ 10s idle), so
# poll for it — a missing snapshot after the wait is a real failure: it means
# the FRIRING_PERF_LOG → publish → `friring-cli perf` chain is broken.
e2e_perf_report() {
    local dest
    dest="$REPO_ROOT/target/agent-e2e/perf/$E2E_SCENARIO_NAME-$(date +%Y%m%d-%H%M%S)"
    mkdir -p "$dest"
    {
        echo "scenario: $E2E_SCENARIO_NAME"
        # version via the CLI binary (same build): the TUI binary answers
        # --version with terminal-mode escapes, not text
        echo "bin: $FRIRING_BIN ($(friring-cli --text version 2>/dev/null | head -1))"
        echo "agent: ${AGENT_NAME:-?} $(agent_version 2>/dev/null || true)"
        echo "pane: ${SCENARIO_COLS}x${SCENARIO_ROWS}"
    } > "$dest/meta.txt"
    printf '%s' "$E2E_PERF_MARKS" > "$dest/marks.txt"
    # Derived deltas between consecutive marks — the numbers a human actually
    # compares across runs.
    awk 'prev { printf "%s -> %s: %dms\n", pname, $1, $2 - prev }
         { prev = $2; pname = $1 }' "$dest/marks.txt" > "$dest/deltas.txt"
    local ok=1
    for _ in $(seq 1 150); do
        if friring-cli --json perf > "$dest/snapshot.json" 2>/dev/null; then
            ok=0
            break
        fi
        sleep 0.2
    done
    [ "$ok" = "0" ] || {
        e2e_die "TUI never published a perf snapshot (FRIRING_PERF_LOG chain broken?)"
        return 1
    }
    # --text: piped stdout would otherwise auto-switch this copy to JSON too
    friring-cli --text perf > "$dest/snapshot.txt" 2>/dev/null || true
    e2e_log "perf report: $dest"
}

# ---------------------------------------------------------------------------
# Emit a record.sh-compatible .tape from the scenario's steps into $1, WITHOUT
# booting anything real: the demo-mode step_* primitives are pure string
# mapping, so this needs only a loaded scenario (e2e_scenario_load). It is the
# testable seam for the demo path and backs `run.sh --emit-tape` (preview a
# tape offline). The Set block mirrors scripts/demo/*.tape so generated demos
# match the hand-written ones frame-for-frame; Output paths are relative
# because vhs runs from the repo root and its parser rejects absolute paths.
e2e_emit_tape() {
    E2E_MODE=demo
    E2E_TAPE="$1"
    E2E_TAPE_ERR=""
    mkdir -p "$(dirname "$E2E_TAPE")"
    # VHS sizes the canvas in pixels, so derive it from the columns/rows the
    # scenario was written against at the standard demo font — the default
    # 120x40 reproduces scripts/demo's 1920x1080 exactly, and a scenario that
    # asks for a wider pane gets one instead of silently recording against a
    # narrower screen (which truncates the very text its waits look for).
    local width=$((SCENARIO_COLS * 16)) height=$((SCENARIO_ROWS * 27))
    # …and cap the framerate to what screenshotting that canvas can sustain.
    # vhs captures through headless Chromium but writes the gif at the nominal
    # rate regardless, so a starved capture does not drop quality — it makes
    # the clip *play back sped up*: at the default rate, 8s of scripted Sleep
    # recorded as 0.84s (21 frames) at 1920x1080. Measured sustainable rates
    # were ~5 fps at 1920x1080 and ~3 fps at 3520x1080 — near enough a constant
    # pixel-rate budget, kept deliberately under the measured ceiling because
    # under-shooting only costs smoothness while over-shooting silently
    # compresses time. `scripts/demo`'s own tapes need none of this: agg
    # renders them offline from an asciicast, with no capture to starve.
    local fps=$((9000000 / (width * height)))
    [ "$fps" -ge 2 ] || fps=2
    cat > "$E2E_TAPE" <<EOF
Output target/agent-e2e/demos/$E2E_SCENARIO_NAME.gif
Output target/agent-e2e/demos/$E2E_SCENARIO_NAME.mp4

Set Shell "bash"
Set FontSize 18
Set Width $width
Set Height $height
Set Framerate $fps
Set Padding 16
Set Theme "Catppuccin Mocha"
Set PlaybackSpeed 1.0
Set WaitTimeout 60s

Hide
Type \`exec "\$FRIRING_BIN"\`
Enter
Sleep 2s
Show
Sleep 1s
EOF
    scenario_steps
    local steps_rc=$?
    # An unmappable key can't fail the flat step list (see step_key), so the
    # tape is only trustworthy once every step got written. Checked ahead of
    # the steps' own status: a bad key *is* a non-zero return when it happens
    # to be the last step, and reporting only "steps failed" would bury the
    # one line that says which key and why.
    [ -z "$E2E_TAPE_ERR" ] \
        || e2e_die "$E2E_SCENARIO_NAME is not demoable:
$E2E_TAPE_ERR" || return 1
    [ "$steps_rc" -eq 0 ] || return 1
    # Closing beat: linger on the last frame. Deliberately no `Ctrl+Q` — quitting
    # inside the recording ends every clip on ~1s of bare shell (measured at
    # 0.07-0.09% ink, which check-pacing.mjs rejects as a leaked teardown). The
    # TUI is torn down by e2e_teardown afterwards, off camera.
    cat >> "$E2E_TAPE" <<'EOF'
Sleep 2s
EOF
}

# ---------------------------------------------------------------------------
# Demo mode: boot the hermetic env, generate the tape (e2e_emit_tape), then run
# vhs inside the (already exported) env.
e2e_demo_record() {
    local out_dir="$REPO_ROOT/target/agent-e2e/demos"
    mkdir -p "$out_dir"

    # Mirror the three drive depths: apply the scenario's uncommitted workspace
    # edit before recording (the boot already ran via `e2e_boot demo` in run.sh).
    # Without this the claude-review-loop demo records an empty Working target.
    scenario_prepare || return 1

    if [ -n "$SCENARIO_DEMO_THEME" ]; then
        sqlite3 "$XDG_DATA_HOME/friring-dev/friring.db" \
            "INSERT INTO metadata (key, value) VALUES ('active_theme', '$SCENARIO_DEMO_THEME')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
    fi

    # Tape lives in the throwaway sandbox during recording.
    e2e_emit_tape "$TBX_SANDBOX_ROOT/$E2E_SCENARIO_NAME.tape" || return 1
    e2e_log "recording $E2E_SCENARIO_NAME ($(basename "$E2E_TAPE"))"
    # vhs renders through a headless Chromium (go-rod): use the system browser
    # if present, else let rod download one into a cache that survives the
    # throwaway sandbox (XDG_CACHE_HOME points into the sandbox). Dropping the
    # dead-proxy vars here does NOT weaken the agent's offline guarantee — the
    # agent pane env was frozen into the friring-dev tmux server at session
    # create, before vhs starts; only vhs's own tooling gets network.
    local vhs_cache="$REPO_ROOT/target/agent-e2e/cache"
    mkdir -p "$vhs_cache"
    # go-rod caches that browser under `$HOME/.cache/rod` and does NOT honor
    # the XDG_CACHE_HOME handed to vhs below — and $HOME is the throwaway
    # sandbox, so every single recording would re-download ~150MB. Point just
    # that path at a repo-persistent cache; the sandbox stays hermetic because
    # only vhs's own browser lives there, and the agent's env was frozen into
    # the tmux server long before.
    mkdir -p "$vhs_cache/rod" "$HOME/.cache"
    ln -sfn "$vhs_cache/rod" "$HOME/.cache/rod"
    ( cd "$REPO_ROOT" && env -u http_proxy -u https_proxy -u HTTP_PROXY -u HTTPS_PROXY \
        XDG_CACHE_HOME="$vhs_cache" vhs "$E2E_TAPE" )
}

# ---------------------------------------------------------------------------
# The three drive depths, all fed by the same scenario directory. When a full
# scenario fails, the shallower layers localize the break: protocol = the
# binary↔stub contract, interactive = the agent's own TUI vs the stub,
# full = Friring's rendering/keying/hooks on top.

# Depth 1 — protocol: the agent's print/exec mode, no tmux, no Friring. The
# argv is profile-provided (agent_print_args): each CLI spells "one-shot
# headless prompt" differently (claude -p, codex exec, opencode run, …).
e2e_protocol_smoke() {
    e2e_scenario_load "$1" || return 1
    e2e_boot protocol || return 1
    scenario_prepare || return 1
    local bin out
    bin="$(agent_binary)"
    agent_print_args "$SCENARIO_PROMPT"
    # </dev/null: codex APPENDS piped/inherited stdin to the prompt; harmless
    # for the other agents.
    ( cd "$E2E_WS" && "${E2E_TIMEOUT:-timeout}" 120 "$bin" "${AGENT_PRINT_ARGS[@]}" \
        > "$TBX_SANDBOX_ROOT/p-stdout.txt" 2>&1 < /dev/null ) \
        || { out="$(cat "$TBX_SANDBOX_ROOT/p-stdout.txt")"; \
             e2e_die "$AGENT_NAME print-mode run failed: $out"; return 1; }
    assert_stub_invariants || return 1
    scenario_assert_effects || return 1
}

# Depth 2 — interactive: the agent's own TUI in a bare tmux pane, still no
# Friring in the loop.
e2e_interactive_smoke() {
    e2e_scenario_load "$1" || return 1
    e2e_boot protocol || return 1
    scenario_prepare || return 1
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
    # Post-boot workspace mutation (e2e_boot commits every workspace/ seed, so
    # an *uncommitted* state — what a Working-target review shows — can only
    # be made here). Runs before any keystroke in every drive depth.
    scenario_prepare || return 1
    scenario_steps || return 1
    assert_stub_invariants || return 1
    scenario_assert_effects || return 1
    scenario_assert_ui || return 1
    if [ "$SCENARIO_PERF" = "1" ]; then
        e2e_perf_report || return 1
    fi
}

# ---------------------------------------------------------------------------
# Artifacts + teardown. Teardown must reap every child even on failure:
# the stub, the driver tmux server, the friring-dev server and the agent
# processes inside it (killed with the server), then the throwaway root.
e2e_collect_artifacts() {
    local dest="$REPO_ROOT/target/agent-e2e/artifacts/$E2E_SCENARIO_NAME-$(date +%Y%m%d-%H%M%S)"
    mkdir -p "$dest"
    {
        echo "scenario: $E2E_SCENARIO_NAME"
        echo "agent: ${AGENT_NAME:-?} $(agent_version 2>/dev/null || true)"
        echo "friring: $("$FRIRING_BIN" --version 2>/dev/null || true)"
        echo "tmux: $(tmux -V)"
        echo "session: ${E2E_SESSION_ID:-none} hook_state: $(e2e_hook_state 2>/dev/null || true)"
    } > "$dest/meta.txt" 2>/dev/null
    e2e_pane > "$dest/driver-pane.txt" 2>/dev/null || true
    tmux -L "$E2E_DRIVER_SOCKET" capture-pane -p -S -200 -t "$E2E_DRIVER_SESSION" \
        > "$dest/driver-pane-history.txt" 2>/dev/null || true
    # The agent's own pane, straight from the friring-dev server.
    friring-cli --json session capture "$E2E_SESSION_ID" --lines 500 \
        > "$dest/agent-pane.json" 2>/dev/null || true
    cp "$E2E_JOURNAL" "$dest/" 2>/dev/null || true
    cp -r "$E2E_STUB_DIR/raw" "$dest/" 2>/dev/null || true
    cp "$E2E_STUB_DIR/stub.log" "$dest/" 2>/dev/null || true
    cp "$E2E_FIXTURES" "$dest/" 2>/dev/null || true
    cp "$XDG_CONFIG_HOME/friring-dev/agents.toml" "$dest/" 2>/dev/null || true
    ( cd "$E2E_WS" 2>/dev/null && { git status --short; git diff; } > "$dest/workspace.diff" ) || true
    env | grep -E '^(FRIRING|ANTHROPIC|CLAUDE|XDG|HOME|no_proxy|http_proxy)' \
        | sed -E 's/((TOKEN|KEY|SECRET|PASSWORD)=).*/\1<redacted>/' > "$dest/env.txt" 2>/dev/null || true
    e2e_log "failure artifacts: $dest"
}

# Surface (never fail on) unexpected non-message endpoints. The correctness-
# critical surface — model + secondary-model calls — arrives as POST
# /v1/messages and is already gated by assert_stub_invariants (a surprise one
# is UNMATCHED → hard failure). The ancillary endpoints a client may add
# between versions (telemetry, config probes) are harmless — a custom
# ANTHROPIC_BASE_URL proxy just ignores them, which is why they aren't a
# stable contract — but a pin bump that starts hitting a new one should be
# VISIBLE, not silently answered {} on a green run. So we allowlist the known
# HEAD / connectivity probe and record anything else to a durable log (kept
# across green runs, unlike the failure-only artifacts) plus, in bats, FD 3
# (shown even for passing tests). Never fails: this is drift *visibility*, and
# the harmless-but-unstable surface must not turn a benign bump red.
e2e_surface_unexpected_endpoints() {
    [ -f "$E2E_JOURNAL" ] || return 0
    local unexpected
    unexpected="$(jq -rs '[.[]
        | select(.kind == "other")
        | select(.method != "HEAD" or .url != "/")
        | "\(.method) \(.url)"] | unique | .[]' "$E2E_JOURNAL" 2>/dev/null)"
    [ -n "$unexpected" ] || return 0
    local drift="$REPO_ROOT/target/agent-e2e/unexpected-endpoints.log"
    mkdir -p "$(dirname "$drift")"
    {
        echo "# ${E2E_SCENARIO_NAME:-?} @ $(date -u +%Y-%m-%dT%H:%M:%SZ) — agent ${AGENT_NAME:-?} $(agent_version 2>/dev/null)"
        printf '%s\n' "$unexpected"
    } >> "$drift"
    e2e_log "unexpected endpoint(s) hit — NOT a failure; logged to $drift:"
    printf '  %s\n' "$unexpected" >&2
    # bats surfaces FD 3 even for passing tests, so a green run still flags it.
    if { : >&3; } 2>/dev/null; then
        printf '# agent-e2e drift: %s hit %s\n' \
            "${E2E_SCENARIO_NAME:-?}" "$(printf '%s' "$unexpected" | tr '\n' ' ')" >&3
    fi
}

e2e_teardown() {
    local failed="${1:-0}"
    # Always surface endpoint drift (pass or fail), before the sandbox — and
    # its journal — is wiped.
    e2e_surface_unexpected_endpoints
    [ "$failed" != "0" ] && e2e_collect_artifacts
    tmux -L "$E2E_DRIVER_SOCKET" kill-server >/dev/null 2>&1 || true
    [ -n "$E2E_STUB_PID" ] && kill "$E2E_STUB_PID" >/dev/null 2>&1 || true
    if [ "${FRIRING_E2E_KEEP:-0}" = "1" ]; then
        e2e_log "FRIRING_E2E_KEEP=1: sandbox left at $TBX_SANDBOX_ROOT"
        # shellcheck disable=SC2034  # read by tbx_sandbox_teardown
        TBX_SANDBOX_FRESH=0
    fi
    tbx_sandbox_teardown 2>/dev/null || true
}
