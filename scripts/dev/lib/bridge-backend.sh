# shellcheck shell=bash
#
# Whether this host can carry the orchestration bridge — asked the way friring
# itself asks it, and shared so its callers cannot drift apart.
#
# `Caps::bridge` is true for seatbelt and bwrap only. For bwrap that is a
# **namespace** question rather than a packaging one: bubblewrap installs from
# every distribution's archive and then fails at `unshare(2)` wherever a kernel
# or an LSM refuses an unprivileged user namespace. So `command -v bwrap`
# answers a question nobody asked — the binary being present says nothing about
# whether friring's own probe will get a namespace, and where it does not,
# `auto` falls through the ladder to a backend that cannot carry the bridge.
#
# What bwrap's failure *means* is not assumed: its stderr is captured and shown,
# bounded, because the same non-zero exit covers a refused namespace, a mount
# that could not be made, an inaccessible working directory and a missing
# `true`.
#
# `FRIRING_E2E_REQUIRE_BRIDGE=1` turns every skip below into a failure. A
# dedicated CI gate sets it: a job whose whole output is these assertions must
# not report success for having made none of them. A developer's machine leaves
# it unset and still gets an honest skip.

# How much of a failed probe's stderr is worth carrying into a CI log.
BRIDGE_PROBE_STDERR_LINES=${BRIDGE_PROBE_STDERR_LINES:-5}

# Skip with a reason, or fail where the caller demanded the real thing.
#
# Every other conditional `exit 0` in these harnesses routes through here too —
# an absent tmux, a missing vendor tool — so that dedicated mode cannot be
# satisfied by a later skip after this gate has passed.
bridge_require_or_skip() {
    local name="$1" why="$2" detail="${3:-}"
    if [ "${FRIRING_E2E_REQUIRE_BRIDGE:-0}" = 1 ]; then
        echo "$name: $why — FRIRING_E2E_REQUIRE_BRIDGE is set, so this is a" \
            "failure rather than a skip" >&2
        [ -n "$detail" ] && printf '%s\n' "$detail" >&2
        exit 1
    fi
    echo "$name: $why; skipping" >&2
    [ -n "$detail" ] && printf '%s\n' "$detail" >&2
    exit 0
}

# The capability gate itself. Returns where the bridge can be carried.
bridge_backend_or_skip() {
    local name="$1" probe_err="" status=0
    case "$(uname -s)" in
        Darwin)
            [ -x /usr/bin/sandbox-exec ] ||
                bridge_require_or_skip "$name" "/usr/bin/sandbox-exec is not present"
            ;;
        Linux)
            command -v bwrap >/dev/null 2>&1 ||
                bridge_require_or_skip "$name" "bubblewrap is not installed"
            # stdout into /dev/null inside the group; what the caller
            # captures is the group's stderr, which is the reason.
            probe_err=$({ bwrap --ro-bind / / --unshare-all true >/dev/null; } 2>&1) ||
                status=$?
            if [ "$status" -ne 0 ]; then
                bridge_require_or_skip "$name" \
                    "bwrap cannot create a sandbox here (exit $status)" \
                    "$(printf '  bwrap: %s\n' \
                        "$(printf '%s' "${probe_err:-(no stderr)}" |
                            head -n "$BRIDGE_PROBE_STDERR_LINES")")"
            fi
            ;;
        *)
            bridge_require_or_skip "$name" \
                "the bridge is carried by seatbelt and bwrap only, not $(uname -s)"
            ;;
    esac
}
