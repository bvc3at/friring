#!/usr/bin/env bash
#
# Observe the seatbelt boundary against a real kernel — `just seatbelt-probe`.
#
# Everything else about the multiplexer deny set is an argument about generated
# policy text. This is the observation: friring's own tmux server, three others
# at the locations tmux derives socket directories under, and an outer server
# whose socket this process inherits through `$TMUX` — each dialled from inside
# a boundary friring composed.
#
# The positive controls matter as much as the denials, and they run **first** in
# each network mode because they are the precondition for it: a deny assertion
# is an exit status, so a mode whose launches never start passes every one of
# them for the reason nothing ran. `network_mode = allowlist` is exactly that —
# friring refuses a filtered profile to a one-shot, correctly — so this harness
# records it as not asked instead of counting six denials it never made. A
# boundary that refused everything would otherwise pass the whole deny set and
# be useless; here the workspace is readable and writable, and a tmux server
# started at a `-S` path under it is reachable. That last one is the documented
# residual — friring denies the *host's* sockets, not the concept of a socket.
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
export REPO_ROOT
PROBE_NAME="seatbelt-probe"

if [ "$(uname -s)" != "Darwin" ]; then
    echo "$PROBE_NAME: seatbelt is macOS only; skipping on $(uname -s)" >&2
    exit 0
fi
# The same capability gate the bridge harnesses share, for its
# `FRIRING_E2E_REQUIRE_BRIDGE` contract: this probe's whole output is
# assertions, so a dedicated job must not report success for making none.
# shellcheck source=scripts/dev/lib/bridge-backend.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/bridge-backend.sh"
bridge_backend_or_skip "$PROBE_NAME"

# shellcheck source=scripts/dev/lib/sandbox-env.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/sandbox-env.sh"
# shellcheck source=scripts/dev/sandbox-probes/common.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/sandbox-probes/common.sh"

cargo build --bin friring --bin friring-cli >/dev/null

# An outer server, started **before** the isolation so its socket is a real one
# outside the sandbox — which is the point: friring must deny the server it is
# itself running inside.
OUTER_DIR=$(mktemp -d /tmp/friring-probe-outer.XXXXXX)
OUTER_SOCKET="$OUTER_DIR/outer"
if ! probe_tmux_server "$OUTER_SOCKET"; then
    bridge_require_or_skip "$PROBE_NAME" "could not start the outer tmux server"
fi
# What friring reads to learn it is inside a pane: the socket is the first
# `,`-separated field.
export TMUX="$OUTER_SOCKET,1,0"

tbx_sandbox_init_full fresh
PROBE_ROOT="$TBX_SANDBOX_ROOT"
PROBE_WORKSPACE="$PROBE_ROOT/ws"
mkdir -p "$PROBE_WORKSPACE"

UID_NOW=$(id -u)
FRIRING_SOCKET="$TMUX_TMPDIR/tmux-$UID_NOW/$TBX_DEV_SOCKET"
TMP_SOCKET="/tmp/tmux-$UID_NOW/probe-other"
TMUX_TMPDIR_SOCKET="$TMUX_TMPDIR/tmux-$UID_NOW/probe-x"
TMPDIR_SOCKET="${TMPDIR:-/tmp}/tmux-$UID_NOW/probe-y"
INNER_SOCKET="$PROBE_WORKSPACE/inner.sock"

cleanup() {
    for socket in "$FRIRING_SOCKET" "$TMP_SOCKET" "$TMUX_TMPDIR_SOCKET" \
        "$TMPDIR_SOCKET" "$OUTER_SOCKET" "$INNER_SOCKET"; do
        probe_kill_server "$socket"
    done
    rm -r -f -- "$OUTER_DIR"
    # The repo's own teardown, so the probe removes what it made the same way
    # every other dev script does.
    tbx_sandbox_teardown
    return 0
}
trap cleanup EXIT

probe_note "servers"
STARTED=()
for socket in "$FRIRING_SOCKET" "$TMP_SOCKET" "$TMUX_TMPDIR_SOCKET" "$TMPDIR_SOCKET"; do
    if probe_tmux_server "$socket"; then
        STARTED+=("$socket")
        printf '  started %s\n' "$socket"
    else
        printf '  SKIPPED %s (could not start)\n' "$socket"
    fi
done
STARTED+=("$OUTER_SOCKET")
printf '  inherited %s through the TMUX variable\n' "$OUTER_SOCKET"

# ── The deny set, in every network mode ──────────────────────────────────
#
# The deny set is about paths and processes, not about the network, so it must
# hold identically under all three. A mode-dependent hole is the interesting
# kind.
#
# The positive controls come **first** in each mode, as a precondition rather
# than a nicety: `probe_denied` reads an exit status, so a mode whose launches
# never start passes the entire deny set for the reason nothing ran, and a tally
# counting those would report a boundary nothing observed.
for mode in full allowlist none; do
    probe_note "network_mode = $mode"
    PROBE_PROFILE="probe-$mode"
    probe_profile "$PROBE_PROFILE" "$mode"
    if ! probe_launches; then
        probe_unexercised "network_mode = $mode" \
            "no one-shot launch composes in this mode, so nothing ran in it and \
nothing about it is counted"
        continue
    fi
    probe_allowed "the workspace is writable" \
        -- sh -c "printf x > '$PROBE_WORKSPACE/probe.txt'"
    probe_allowed "the workspace is readable" -- cat "$PROBE_WORKSPACE/probe.txt"
    for socket in "${STARTED[@]}"; do
        probe_denied "tmux at $socket" -- tmux -S "$socket" list-windows
    done
    # The inherited address is stripped as well as denied — belt and braces, and
    # the strip is what keeps a tool from finding one way out and reporting a
    # confusing failure.
    # shellcheck disable=SC2016  # the inner shell is meant to expand it, not this one
    probe_denied 'the inherited TMUX address is gone' -- sh -c '[ -n "${TMUX:-}" ]'
done

# Why `allowlist` is the mode that goes unexercised, asserted rather than
# asserted-about: friring refuses a **filtered** profile to a one-shot outright,
# because the proxy that enforces one lives in a running friring and a one-shot
# would bind that listener and take it away again. The refusal is the product's
# and it is correct; it also means no harness here puts a filtered profile under
# a real kernel, and `bridge-conformance` runs `none`, so nothing else covers it.
probe_note "a one-shot under a filtered profile"
PROBE_PROFILE="probe-allowlist"
if probe_launches; then
    probe_bad "a one-shot ran under a filtered profile: it would take over the \
egress listener a running friring owns"
else
    probe_ok "a one-shot is refused under a filtered profile — the proxy that \
enforces one lives in a running friring"
fi

# ── Positive controls ────────────────────────────────────────────────────
#
# The boundary is useful only if it still lets the session's own IPC through.
probe_note "positive controls (a socket under the workspace)"
PROBE_PROFILE="probe-full"

# A tmux server under the *session's own* directory: friring denies the host's
# sockets, not the concept of one. This is the documented residual, and a probe
# that did not assert it would be claiming a stronger boundary than friring has.
if probe_tmux_server "$INNER_SOCKET"; then
    probe_allowed "a tmux server under the workspace is reachable" \
        -- tmux -S "$INNER_SOCKET" list-windows
else
    printf '  SKIPPED  a tmux server under the workspace (could not start)\n'
fi

# ── friring's own trees ──────────────────────────────────────────────────
#
# The gate is the launch's one-way switch (ADR-33) and the database is host
# command execution (ADR-29). Neither is granted to a boundary that did not ask
# for it, and both are asserted here rather than argued about, because "a
# sandbox cannot open somebody else's gate" is the property the whole gated
# launch rests on.
#
# What this cannot reach: a launch's **own** gate being read-only, and a
# filtered session's proxy being usable. Both need a real session launch —
# `sandbox exec` composes no gate and refuses a filtered profile, because a
# one-shot cannot own a proxy — so they are asserted at unit level and recorded
# as unobserved in `docs/SANDBOX.md`.
probe_note "friring's own trees"
probe_other_gate

probe_summary
