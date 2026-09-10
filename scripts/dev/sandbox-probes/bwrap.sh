#!/usr/bin/env bash
#
# Observe the bubblewrap boundary against a real kernel — `just bwrap-probe`.
#
# The Linux twin of `seatbelt.sh`, with the same deny-set and positive-control
# assertions, plus the two things only a namespace can be asked:
#
# - the agent is **pid 1** inside its namespace, which is what makes the relay's
#   lifetime the launch's lifetime;
# - **none of this probe's own launches left a relay** on the host — a global
#   `pgrep` for `friring-cli sandbox relay` after they have all exited. It is not
#   a test of the two exit paths: `sandbox exec` composes no relay at all, so
#   there is never one here to outlive anything. The lifetimes themselves — after
#   the agent argv exits, and after a gate timeout — are covered as real
#   processes by `tests/sandbox_launch_helper.rs::no_relay_survives_the_helper`
#   (Linux).
#
# Skips rather than fails where the capability is absent: user namespaces are
# off on some distributions and in most containers, and a probe that failed
# there would be reporting the machine rather than friring.
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
export REPO_ROOT
PROBE_NAME="bwrap-probe"

if [ "$(uname -s)" != "Linux" ]; then
    echo "$PROBE_NAME: bubblewrap is Linux only; skipping on $(uname -s)" >&2
    exit 0
fi
# The capability check this file already made, moved into the gate the bridge
# harnesses now share — the drift between the two is what let a job report
# success for assertions it never reached. Its `FRIRING_E2E_REQUIRE_BRIDGE`
# contract comes with it, which is what a dedicated CI job sets.
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

OUTER_DIR=$(mktemp -d /tmp/friring-probe-outer.XXXXXX)
OUTER_SOCKET="$OUTER_DIR/outer"
if ! probe_tmux_server "$OUTER_SOCKET"; then
    bridge_require_or_skip "$PROBE_NAME" "could not start the outer tmux server"
fi
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

for mode in full allowlist none; do
    probe_note "network_mode = $mode"
    PROBE_PROFILE="probe-$mode"
    probe_profile "$PROBE_PROFILE" "$mode"
    for socket in "${STARTED[@]}"; do
        probe_denied "tmux at $socket" -- tmux -S "$socket" list-windows
    done
    # shellcheck disable=SC2016  # the inner shell is meant to expand it, not this one
    probe_denied 'the inherited TMUX address is gone' -- sh -c '[ -n "${TMUX:-}" ]'
done

probe_note "positive controls (network_mode = full)"
PROBE_PROFILE="probe-full"
probe_allowed "the workspace is writable" -- sh -c "printf x > '$PROBE_WORKSPACE/probe.txt'"
probe_allowed "the workspace is readable" -- cat "$PROBE_WORKSPACE/probe.txt"
if probe_tmux_server "$INNER_SOCKET"; then
    probe_allowed "a tmux server under the workspace is reachable" \
        -- tmux -S "$INNER_SOCKET" list-windows
else
    printf '  SKIPPED  a tmux server under the workspace (could not start)\n'
fi

# ── The namespace's own two properties ───────────────────────────────────

probe_note "friring's own trees"
probe_other_gate

probe_note "the namespace"

# The agent is pid 1 inside it, which is what makes a relay's lifetime the
# launch's: everything in the namespace goes when pid 1 does.
PID_ONE_STATUS=0
PID_ONE_OUT=$({ probe_run sh -c 'test "$$" = 1' >/dev/null; } 2>&1) || PID_ONE_STATUS=$?
if [ "$PID_ONE_STATUS" -eq 0 ]; then
    probe_ok "the wrapped process is pid 1 inside its namespace"
else
    # The reason matters here for the same cause as the positive controls: a
    # launch that never ran and a launch whose pid is wrong are different
    # findings and this exit code alone cannot tell them apart.
    probe_bad "the wrapped process is not pid 1: a relay could outlive its launch"
    printf '%s\n' "${PID_ONE_OUT:-(no output)}" | head -5 | sed 's/^/          /'
fi

# And nothing of the launch is left on the host. `sandbox exec` composes no
# relay (it refuses a filtered profile — a one-shot cannot own a proxy), so what
# this asserts is the stronger statement: the probe's own launches left none.
RELAYS=$(pgrep -fa 'friring-cli sandbox relay' 2>/dev/null || true)
if [ -z "$RELAYS" ]; then
    probe_ok "no sandbox relay survived a launch"
else
    probe_bad "a sandbox relay is still running: $RELAYS"
fi

probe_summary
