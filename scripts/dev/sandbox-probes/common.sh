# shellcheck shell=bash
#
# Shared scaffolding for the sandbox boundary probes.
#
# The probes exist because everything else about the deny set is an argument
# about generated text. These observe it: a real kernel, a real policy friring
# composed, and a real command that either reaches a socket or does not.
#
# Source it (don't execute) after setting REPO_ROOT.

: "${REPO_ROOT:?set REPO_ROOT before sourcing}"

PROBE_PASS=0
PROBE_FAIL=0

# probe_note <text> — a heading in the transcript.
probe_note() { printf '\n== %s ==\n' "$*"; }

# probe_ok <what> — record a passing assertion.
probe_ok() {
    PROBE_PASS=$((PROBE_PASS + 1))
    printf '  ok      %s\n' "$*"
}

# probe_bad <what> — record a failing one. The probe keeps going: one run
# should report every hole, not the first.
probe_bad() {
    PROBE_FAIL=$((PROBE_FAIL + 1))
    printf '  FAILED  %s\n' "$*"
}

# probe_denied <what> -- <argv…> — assert the command FAILS inside the boundary.
#
# The deny-set assertions. A command that succeeds here is a hole, and the
# probe says which one.
probe_denied() {
    local what=$1
    shift
    [ "$1" = "--" ] && shift
    if probe_run "$@" >/dev/null 2>&1; then
        probe_bad "$what — the boundary allowed it"
    else
        probe_ok "$what — refused"
    fi
}

# probe_allowed <what> -- <argv…> — assert the command SUCCEEDS inside it.
#
# The positive controls, and they matter as much: a boundary that denied
# everything would pass every deny assertion and be useless.
probe_allowed() {
    local what=$1
    shift
    [ "$1" = "--" ] && shift
    if probe_run "$@" >/dev/null 2>&1; then
        probe_ok "$what — allowed"
    else
        probe_bad "$what — the boundary refused it"
    fi
}

# probe_run <argv…> — run one command inside $PROBE_PROFILE's boundary.
probe_run() {
    friring-cli --json sandbox exec --profile "$PROBE_PROFILE" --cwd "$PROBE_WORKSPACE" -- "$@"
}

# probe_summary — print the tally and set the exit status.
probe_summary() {
    printf '\n%s: %d passed, %d failed\n' "${PROBE_NAME:-probe}" "$PROBE_PASS" "$PROBE_FAIL"
    [ "$PROBE_FAIL" -eq 0 ]
}

# probe_profile <name> <network-mode> — store a profile granting the probe
# workspace and nothing else.
#
# Composed through `sandbox import`, so what is stored is what a session would
# get: the same validation, the same path refusals.
probe_profile() {
    local name=$1 mode=$2
    cat > "$PROBE_ROOT/$name.toml" <<TOML
[[profile]]
name = "$name"
backend = "seatbelt"
paths = [{ path = "$PROBE_WORKSPACE", mode = "rw" }]
network_mode = "$mode"
network_allow = ["example.invalid:443"]
network_deny = []
prompt_new_domains = false
read_scope = "host-minus-secrets"
allow_unsandboxed_fallback = false
TOML
    if [ "$(uname -s)" = "Linux" ]; then
        sed -i 's/backend = "seatbelt"/backend = "bwrap"/' "$PROBE_ROOT/$name.toml"
    fi
    friring-cli sandbox import "$PROBE_ROOT/$name.toml" --replace >/dev/null
}

# probe_tmux_server <socket-path> — start a tmux server at an exact socket path
# and leave one window in it. Every server the probe starts is inside the
# sandbox root or the sandbox TMUX_TMPDIR, so nothing here can reach a real one.
probe_tmux_server() {
    local socket=$1
    mkdir -p "$(dirname "$socket")"
    tmux -S "$socket" new-session -d -s probe 'sleep 3600' 2>/dev/null || return 1
    # Prove it answers *outside* the boundary, or the deny assertion below would
    # pass for the wrong reason.
    tmux -S "$socket" list-windows >/dev/null 2>&1
}

# probe_kill_server <socket-path> — best effort teardown.
probe_kill_server() {
    tmux -S "$1" kill-server >/dev/null 2>&1 || true
}

# probe_other_gate — assert a gate directory and friring's database are
# unreachable from a boundary that was not granted them.
#
# Two properties the whole design rests on, asserted against a real kernel
# rather than against generated text:
#
# - **A gate is only the host's to open** (ADR-33). A boundary that could read
#   another session's gate directory could watch for its release file; one that
#   could write it could open the gate on a launch that friring has not finished
#   preparing.
# - **The database is host command execution** (ADR-29). It carries the
#   automation commands the host runs, so reading or writing it from inside is
#   an escape rather than a leak.
#
# Uses `friring-cli`'s own resolved paths, so the probe cannot assert against a
# directory friring does not actually use.
probe_other_gate() {
    local data gate db
    # Resolved the way friring resolves it: the explicit override first, then
    # the XDG root with the dev build's own subdirectory — which is what
    # `tbx_sandbox_init_full` sets up.
    data=${FRIRING_DATA_DIR:-}
    if [ -z "$data" ] && [ -n "${XDG_DATA_HOME:-}" ]; then
        data="$XDG_DATA_HOME/friring-dev"
    fi
    if [ -z "$data" ]; then
        printf '  SKIPPED  friring data directory is not pinned\n'
        return 0
    fi
    gate="$data/gates/probe-other"
    db="$data/friring.db"
    mkdir -p "$gate"
    : > "$gate/release"
    probe_denied "another session's gate directory is unreadable" \
        -- cat "$gate/release"
    probe_denied "another session's gate directory is unwritable" \
        -- sh -c "printf x > '$gate/planted'"
    if [ -f "$db" ]; then
        probe_denied "friring's database is unreadable" -- cat "$db"
    else
        printf '  SKIPPED  the database (not created in this sandbox)\n'
    fi
    rm -r -f -- "$gate"
}
