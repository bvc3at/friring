# shellcheck shell=bash
#
# Real-agent e2e harness core — sourced by suite.bats (test mode) and run.sh
# (demo mode). One scenario description (scenario.sh) drives both: the step_*
# primitives either drive the live TUI through a driver tmux and poll for
# results (test mode), or emit a .tape that scripts/demo/lib/drive-tape.mjs
# replays into that same driver tmux while asciinema films it (demo mode).
# See docs/E2E.md.
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
# Second server, demo mode only: asciinema needs a real tty, so it runs in a
# pane here and records a *client attached to* the driver session — i.e. the
# bytes a real terminal would receive. Same split as scripts/demo/record.sh.
E2E_CAST_SOCKET="agent-e2e-cast"
E2E_CAST_SESSION="rec"

# Render settings for demo mode. Deliberately identical to the values in
# scripts/demo/record.sh (which the shipped docs/media clips use) so a
# generated clip and a hand-written one are the same product on screen —
# change them together. Font size and family drive agg's rasterization;
# the palette is Catppuccin Mocha's bg,fg + 16 ANSI slots, which is what the
# agent panes' default-coloured text lands on (friring paints its own theme
# in truecolor over it).
E2E_DEMO_FONT="Meslo LG S"
E2E_DEMO_FONT_SIZE=18
E2E_DEMO_PALETTE="1e1e2e,cdd6f4,45475a,f38ba8,a6e3a1,f9e2af,89b4fa,f5c2e7,94e2d5,bac2de,585b70,f38ba8,a6e3a1,f9e2af,89b4fa,f5c2e7,94e2d5,a6adc8"
# --font-dir flags for agg, filled by e2e_require_tools BEFORE the sandbox
# replaces $HOME: a font is a host resource, not part of what we isolate, and
# agg resolving nothing would silently render in a fallback face.
E2E_DEMO_FONT_DIRS=""
# Milliseconds per typed character. VHS's default (which drive-tape.mjs
# inherits) is 50ms, tuned for the hand-written tapes where a prompt is one
# short line. A scenario prompt is a whole sentence of agent instruction, and
# at 50ms those read as a paragraph being dictated. Fast enough to feel like
# someone who knows what they are typing, slow enough to still be typing:
# about two characters per rendered frame at agg's 30fps cap.
E2E_DEMO_TYPING_MS=16
# The gap between the leader and the key that names its action — see
# step_leader.
E2E_DEMO_LEADER_BEAT="350ms"

E2E_MODE="test"
E2E_STUB_PID=""
E2E_TAPE=""

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
    [ "$mode" = "demo" ] && tools="$tools asciinema agg ffmpeg sqlite3"
    for t in $tools; do
        command -v "$t" >/dev/null 2>&1 || missing="$missing $t"
    done
    [ -z "$missing" ] || e2e_die "missing required tool(s):$missing"
    [ "$mode" = "demo" ] || return 0

    # Host font dirs, captured while $HOME is still the real one (this runs
    # before e2e_boot's sandbox init) — see E2E_DEMO_FONT_DIRS.
    local fd
    for fd in "$HOME/Library/Fonts" /Library/Fonts "$HOME/.local/share/fonts" \
        /usr/share/fonts /usr/local/share/fonts; do
        [ -d "$fd" ] && E2E_DEMO_FONT_DIRS="$E2E_DEMO_FONT_DIRS --font-dir $fd"
    done
    # Ask agg what it would pick rather than probing the system: it resolves
    # families itself and falls back silently when one is missing, which is how
    # the shipped media's typography drifted with the recording box before.
    # A one-event cast is enough to make it report.
    local probe picked
    probe="$(mktemp -d "${TMPDIR:-/tmp}/friring-font.XXXXXX")"
    printf '{"version": 2, "width": 20, "height": 3}\n[0.0, "o", "probe"]\n' > "$probe/p.cast"
    # shellcheck disable=SC2086  # E2E_DEMO_FONT_DIRS is a pre-split flag list
    picked="$(agg "$probe/p.cast" "$probe/p.gif" --fps-cap 1 $E2E_DEMO_FONT_DIRS \
        --text-font-family "$E2E_DEMO_FONT" -v 2>&1 \
        | sed -n 's/.*primary text font family: //p' | head -1)"
    find "$probe" -depth -delete 2>/dev/null || true
    [ "$picked" = "$E2E_DEMO_FONT" ] || e2e_die \
        "demo font '$E2E_DEMO_FONT' is not installed (agg would use '${picked:-none}')
  macOS:  brew install --cask font-meslo-lg
  nix:    it is in the flake's demoTools"
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
    # scripts/demo/record.sh's DEMO_COLS/DEMO_ROWS, and deliberately the same
    # numbers: agg rasterizes the grid, so this geometry is what decides the
    # clip's aspect ratio. 175x42 renders 1918x1084 — pixel-identical to every
    # shipped docs/media clip, which is the point. A scenario overrides it only
    # when the size is itself the subject, and then records at its own ratio.
    SCENARIO_COLS=175
    SCENARIO_ROWS=42
    SCENARIO_PRECREATE=1
    # What the precreated session is called. Defaults to the scenario name,
    # which is right for a scenario with one session and wrong for a fleet:
    # its siblings are named by the steps, and `claude-ghost-fleet` sitting
    # above `ring-02` reads as an accident. It is also the name the app puts
    # on screen, so it is part of what a clip shows.
    SCENARIO_SESSION_NAME=""
    SCENARIO_REQUIRE_ALL_FIXTURES=1
    # Every generated demo films the same theme, so a set of clips reads as one
    # product rather than a screenshot pile. A scenario overrides it only when
    # the theme itself is the subject (scripted-theme-settings picks its own).
    SCENARIO_DEMO_THEME="doom"
    # Demo-mode key substitutions, `<tmux key>=<tmux key>…`. Editorial, not a
    # workaround: the demo can press anything the test can, so this exists for
    # the case where a second route to the same action *films* better. An Alt
    # chord is invisible — `M-u=C-f U` unloads through the fork's leader
    # instead, and the which-key overlay shows the viewer what was pressed.
    # Whether a substitution preserves what the clip shows is always the
    # scenario's judgement, never the harness's: scripted-info-keybind presses
    # F2 to prove it does nothing, so routing it to the same action's leader
    # key would record the opposite of the feature.
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

    # The info panel's Claude usage gauges read "not logged in (no subscription
    # token)" in a hermetic sandbox — true, and wrong on camera in any clip
    # that opens the panel. The fork's own FRIRING_CLAUDE_USAGE_URL points that
    # fetch at the stub's /api/oauth/usage route, which answers only when the
    # scenario's fixtures carry a top-level `usage` key — so a scenario opts in
    # by declaring one, and the rest never make the request at all.
    if jq -e '.usage' "$E2E_FIXTURES" >/dev/null 2>&1; then
        export FRIRING_CLAUDE_USAGE_URL="$AGENT_E2E_STUB_URL/api/oauth/usage"
        # friring reads the OAuth token from `~/.claude/.credentials.json`
        # before it fetches anything, so the URL alone still renders "not
        # logged in". Seeded at $HOME and deliberately NOT under
        # CLAUDE_CONFIG_DIR — that is where the claude CLI keeps its own
        # state, and it must go on authenticating with ANTHROPIC_AUTH_TOKEN
        # rather than try to refresh this fictional token against a dead
        # proxy. Same split, and the same fictional plan tier, as
        # scripts/demo/record.sh.
        mkdir -p "$HOME/.claude"
        printf '%s\n' \
            '{"claudeAiOauth":{"accessToken":"friring-e2e-oauth-dummy","subscriptionType":"max"}}' \
            > "$HOME/.claude/.credentials.json"
    fi
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
    out="$(friring-cli --json session create --name "${SCENARIO_SESSION_NAME:-$E2E_SCENARIO_NAME}" \
        --repo-path "$E2E_WS" --agent "$AGENT_NAME" 3>&-)" \
        || e2e_die "session create failed: $out" || return 1
    E2E_SESSION_ID="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$E2E_SESSION_ID" ] && [ "$E2E_SESSION_ID" != "null" ] \
        || e2e_die "no session id in: $out" || return 1
}

# ---------------------------------------------------------------------------
# Step primitives — the scenario's shared vocabulary. Test mode drives the
# driver tmux and polls; demo mode appends tape lines that do the same thing
# later, when the driver replays them on camera.

# The tape lines that press one key, honoring the scenario's demo-mode
# substitutions. `Key <name>` hands the tmux key name straight through to
# drive-tape.mjs, which sends it with `tmux send-keys` — the same call test
# mode makes below. There is deliberately no translation table: the demo
# presses exactly what the asserting test presses, and a name tmux does not
# know fails the recording rather than recording something else.
_demo_key_lines() {
    local want="$1" entry sub="" k first=1
    for entry in ${SCENARIO_DEMO_KEYS[@]+"${SCENARIO_DEMO_KEYS[@]}"}; do
        [ "${entry%%=*}" = "$want" ] && sub="${entry#*=}"
    done
    for k in ${sub:-$want}; do
        # A beat between the keys of a substitution — see step_leader. Every
        # multi-key route is a leader chord, and the app has to see the two as
        # two events.
        [ "$first" = "1" ] || printf 'Sleep %s\n' "$E2E_DEMO_LEADER_BEAT"
        first=0
        printf 'Key %s\n' "$k"
    done
}

# Press the fork's leader (`Ctrl+F`) and then the key that names the action.
#
# The beat between them is load-bearing, not pacing. Test mode gets one for
# free — every step_key is its own `tmux send-keys` process — and a tape does
# not, so the two keystrokes arrive back to back and the chord does not land.
# It is also the only reason the which-key overlay the leader opens is ever on
# camera; scripts/demo's hand-written tapes sleep 500ms in the same place.
step_leader() {
    step_key C-f
    step_sleep "$E2E_DEMO_LEADER_BEAT"
    step_key "$1"
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
        _demo_key_lines "$1" >> "$E2E_TAPE"
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
        # Verbatim, so a beat can be sub-second (`400ms`); a bare number is
        # seconds, which is how every scenario already spells it.
        printf 'Sleep %s\n' "$1" >> "$E2E_TAPE"
    fi
}

# Wait until the rendered TUI shows $1 (grep pattern in test mode; a
# drive-tape.mjs `Wait /re/` in demo mode). The one synchronization primitive
# both modes share — no open-loop sleeps around agent latency. Both poll the
# same pane through `tmux capture-pane`, so a wait that holds in the test
# holds in the recording, and a wait that never resolves fails BOTH (vhs used
# to record a clip that ran to completion showing the wrong thing).
step_wait_pane() {
    local pattern="$1" timeout="${2:-30}"
    if [ "$E2E_MODE" = "demo" ]; then
        # Test mode greps (POSIX BRE); drive-tape.mjs builds a JS RegExp from
        # what sits between the /…/. Translate rather than flatten to a
        # literal: the two dialects already agree on everything these patterns
        # use — `.`, `.*`, and the `\[`/`\]` a BRE needs for a literal bracket
        # are spelled the same in JS — so escaping wholesale silently broke
        # every wait a scenario meant as a regex. Only the characters BRE takes
        # literally and JS does not need escaping, or a pane title like
        # `Edited (1)` becomes a capture group matching `Edited 1`. `/` is
        # escaped because it delimits. A pattern with a bare unbalanced `[`
        # would still make an invalid RegExp — the driver then fails the
        # recording loudly, which is the right failure.
        local esc
        esc="$(printf '%s' "$pattern" | sed 's#[/+?(){}|]#\\&#g')"
        # Hold on what the wait resolved on. asciinema films the wait itself
        # (the clip carries the app's real latency, spinners and all), but it
        # ends the instant the marker paints — so without this the frame the
        # scenario waited for is on screen for as long as it takes to send the
        # next key. Keep it under the 1s max-held-frame budget: consecutive
        # waits that land on one screen add up, and only a `step_sleep` should
        # ever hold longer.
        printf 'Wait /%s/ %ss\nSleep 300ms\n' "$esc" "$timeout" >> "$E2E_TAPE"
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
# short beat instead ($3 overrides it). Deliberately short: this is a guess,
# not a wait, and in every scenario that uses it a real `step_wait_pane` on
# the turn's own marker follows within a step or two — which now films the
# agent working for exactly as long as it works. A two-second guess on top of
# that is a held frame, not pacing.
step_wait_state() {
    local want="$1" timeout="${2:-30}"
    if [ "$E2E_MODE" = "demo" ]; then
        printf 'Sleep %s\n' "${3:-400ms}" >> "$E2E_TAPE"
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

# Regex sibling of assert_pane_contains, for the patterns e2e_wait_pane waits
# on: those go through `grep -q`, so a wait and an assert written against the
# same pattern only agree if the assert matches as a regex too. Same `grep -q`
# and not `-E` for exactly that reason — BRE and ERE disagree on unescaped
# `()+?{}|`, so a pattern lifted from a step_wait_pane would match differently
# here, silently, which is the bug this helper exists to close rather than move.
assert_pane_matches() {
    e2e_pane | grep -q -- "$1" \
        || e2e_die "pane does not match: $1
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
# Emit a drive-tape.mjs .tape from the scenario's steps into $1, WITHOUT
# booting anything real: the demo-mode step_* primitives are pure string
# mapping, so this needs only a loaded scenario (e2e_scenario_load). It is the
# testable seam for the demo path and backs `run.sh --emit-tape` (preview a
# tape offline).
#
# No `Hide … Show` launch preamble and no framerate: the recorder boots the
# TUI itself (it has to — recording attaches to an already-running session),
# and agg renders the cast offline at a 30fps cap, so there is no live capture
# to starve and nothing to derive a rate from. Output paths are relative
# because the render runs from the repo root.
e2e_emit_tape() {
    E2E_MODE=demo
    E2E_TAPE="$1"
    mkdir -p "$(dirname "$E2E_TAPE")"
    cat > "$E2E_TAPE" <<EOF
Output target/agent-e2e/demos/$E2E_SCENARIO_NAME.gif
Output target/agent-e2e/demos/$E2E_SCENARIO_NAME.mp4

Set FontSize $E2E_DEMO_FONT_SIZE
Set Cols $SCENARIO_COLS
Set Rows $SCENARIO_ROWS
Set Theme "$SCENARIO_DEMO_THEME"
EOF
    scenario_steps || return 1
    # Closing beat: linger on the last frame. Deliberately no `Ctrl+Q` — quitting
    # inside the recording ends every clip on ~1s of bare shell (measured at
    # 0.07-0.09% ink, which check-pacing.mjs rejects as a leaked teardown). The
    # TUI is torn down by e2e_teardown afterwards, off camera.
    cat >> "$E2E_TAPE" <<'EOF'
Sleep 2s
EOF
}

# ---------------------------------------------------------------------------
# Demo mode: boot the hermetic env, generate the tape (e2e_emit_tape), then
# record and render it the way scripts/demo/record.sh records the shipped
# clips — asciinema captures the TUI's terminal *byte stream* and agg renders
# it offline.
#
# This is not an implementation detail. Grabbing pixels off a live GUI (vhs's
# model) costs real time per frame, so the capture starves the moment the box
# cannot rasterize fast enough, and vhs writes the gif at the nominal rate
# regardless: the clip does not lose quality, it plays back sped up. At
# 1920x1080 that meant a sustainable ~5fps, which had to be pinned per-canvas
# from a measured pixel budget — and 5fps is also why typing arrived in visible
# chunks of five or six characters. Capturing bytes costs nothing, so every
# paint friring emits is kept with its true timestamp and the render can take
# as long as it likes.
e2e_demo_record() {
    local out_dir="$REPO_ROOT/target/agent-e2e/demos"
    mkdir -p "$out_dir"
    local gif="$out_dir/$E2E_SCENARIO_NAME.gif"
    local mp4="$out_dir/$E2E_SCENARIO_NAME.mp4"
    local cast="$TBX_SANDBOX_ROOT/$E2E_SCENARIO_NAME.cast"

    # Mirror the three drive depths: apply the scenario's uncommitted workspace
    # edit before recording (the boot already ran via `e2e_boot demo` in run.sh).
    # Without this the claude-review-loop demo records an empty Working target.
    scenario_prepare || return 1

    if [ -n "$SCENARIO_DEMO_THEME" ]; then
        sqlite3 "$XDG_DATA_HOME/friring-dev/friring.db" \
            "INSERT INTO metadata (key, value) VALUES ('active_theme', '$SCENARIO_DEMO_THEME')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value"
    fi

    # Generate BEFORE the TUI starts. A scenario's steps are a flat list, and
    # anything in it that is not a step_* runs here rather than on camera —
    # `friring-cli session create` for the extra sessions a fleet scenario
    # needs, a mid-step id probe. Off camera and before boot is the one
    # placement where that is predictable: the TUI opens on the state those
    # commands left behind, instead of racing them mid-clip.
    # Tape lives in the throwaway sandbox during recording.
    e2e_emit_tape "$TBX_SANDBOX_ROOT/$E2E_SCENARIO_NAME.tape" || return 1

    # Boot the TUI off camera, at the size the scenario was written against.
    tmux -L "$E2E_DRIVER_SOCKET" new-session -d -s "$E2E_DRIVER_SESSION" \
        -x "$SCENARIO_COLS" -y "$SCENARIO_ROWS" "$FRIRING_BIN" 3>&-
    # No status bar: an attached client renders one, so it would be filmed.
    tmux -L "$E2E_DRIVER_SOCKET" set -g status off
    e2e_wait_pane "friring" 300 || e2e_die "TUI did not boot" || return 1

    e2e_log "recording $E2E_SCENARIO_NAME ($(basename "$E2E_TAPE"))"
    # asciinema needs a real tty, which a script has no way to hand it, so it
    # runs inside its own tmux pane and records an attached client of the
    # driver session — i.e. exactly the bytes a real terminal would receive.
    tmux -L "$E2E_CAST_SOCKET" new-session -d -s "$E2E_CAST_SESSION" \
        -x "$SCENARIO_COLS" -y "$SCENARIO_ROWS" \
        "asciinema rec '$cast' --overwrite -f asciicast-v2 \
            --command 'tmux -L $E2E_DRIVER_SOCKET attach -t $E2E_DRIVER_SESSION'" 3>&-
    tmux -L "$E2E_CAST_SOCKET" set -g status off

    # Wait for the attached client to paint one settled frame — do NOT sleep a
    # fixed amount. asciinema is recording from the moment the attach starts,
    # so every millisecond spent waiting is filmed, and it lands on the opening
    # frame where it costs the most. Two identical consecutive captures mean
    # the redraw is done.
    local prev="" now same=0 i=0
    while [ "$i" -lt 400 ]; do
        now="$(tmux -L "$E2E_CAST_SOCKET" capture-pane -p -t "$E2E_CAST_SESSION" 2>/dev/null || true)"
        if printf '%s' "$now" | grep -q "friring" && [ "$now" = "$prev" ]; then
            same=$((same + 1))
        else
            same=0
        fi
        [ "$same" -ge 2 ] && break
        prev="$now"
        sleep 0.025
        i=$((i + 1))
    done
    [ "$i" -lt 400 ] || e2e_die "the recorded attach never painted" || return 1

    # Drive it. Every Wait in here polls the same pane the asserting test
    # polls and fails the take if it never resolves, so a recording can no
    # longer run to completion having silently skipped what it came to film.
    local drove=0
    DEMO_TYPING_SPEED_MS="$E2E_DEMO_TYPING_MS" \
        node "$REPO_ROOT/scripts/demo/lib/drive-tape.mjs" "$E2E_TAPE" \
        --socket "$E2E_DRIVER_SOCKET" --session "$E2E_DRIVER_SESSION" || drove=1

    # End the recording by DETACHING the filmed client, with the TUI still up:
    # a quit on camera films its own teardown (friring leaves the alternate
    # screen, the dying client resets the terminal) and that becomes the held
    # closing frame. Only asciinema's own exit flushes the tail of the cast.
    tmux -L "$E2E_DRIVER_SOCKET" detach-client >/dev/null 2>&1 || true
    i=0
    while tmux -L "$E2E_CAST_SOCKET" has-session -t "$E2E_CAST_SESSION" 2>/dev/null \
        && [ "$i" -lt 40 ]; do
        sleep 0.25
        i=$((i + 1))
    done
    tmux -L "$E2E_CAST_SOCKET" kill-server >/dev/null 2>&1 || true
    [ "$drove" -eq 0 ] || e2e_die "the tape driver failed part-way through" || return 1
    [ -s "$cast" ] || e2e_die "no cast recorded" || return 1

    # Drop the detach teardown from the tail, so the clip ends on the final
    # live TUI frame — see scripts/demo/lib/trim-cast.mjs.
    node "$REPO_ROOT/scripts/demo/lib/trim-cast.mjs" "$cast" \
        || e2e_die "could not trim the detach tail" || return 1

    # shellcheck disable=SC2086  # E2E_DEMO_FONT_DIRS is a pre-split flag list
    agg "$cast" "$gif" --font-size "$E2E_DEMO_FONT_SIZE" --fps-cap 30 \
        --idle-time-limit 30 --last-frame-duration 0.5 --theme "$E2E_DEMO_PALETTE" \
        --text-font-family "$E2E_DEMO_FONT" $E2E_DEMO_FONT_DIRS \
        || e2e_die "agg failed" || return 1
    # gif -> mp4. ffmpeg reads the gif's per-frame delays as timestamps, so
    # `fps=30` re-times to a constant rate for players that need one WITHOUT
    # changing the duration. Never re-encode the gif: its variable delays are
    # where the exact pacing lives.
    ffmpeg -y -loglevel error -i "$gif" -movflags +faststart -pix_fmt yuv420p \
        -vf "fps=30,scale=trunc(iw/2)*2:trunc(ih/2)*2" "$mp4" \
        || e2e_die "ffmpeg failed" || return 1

    # Report the pacing budget; do NOT gate on it. It is calibrated for the
    # hand-written tapes, which film a seeded TUI and no agent latency at all.
    # These clips film real CLIs booting and answering, so a held frame here is
    # often the app being honestly slow — which is the thing the scenario came
    # to show. Worth seeing, never worth silently discarding a take for.
    node "$REPO_ROOT/scripts/demo/lib/check-pacing.mjs" "$gif" || true
    e2e_log "recorded $gif"
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
    friring-cli --json session capture "${E2E_SESSION_ID:-}" --lines 500 \
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
    tmux -L "$E2E_CAST_SOCKET" kill-server >/dev/null 2>&1 || true
    [ -n "$E2E_STUB_PID" ] && kill "$E2E_STUB_PID" >/dev/null 2>&1 || true
    if [ "${FRIRING_E2E_KEEP:-0}" = "1" ]; then
        e2e_log "FRIRING_E2E_KEEP=1: sandbox left at $TBX_SANDBOX_ROOT"
        # shellcheck disable=SC2034  # read by tbx_sandbox_teardown
        TBX_SANDBOX_FRESH=0
    fi
    tbx_sandbox_teardown 2>/dev/null || true
}
