#!/bin/sh
# Shared helpers for the bridge conformance agents.
#
# Deliberately POSIX `sh` with no dependencies: what these prove is the
# **bridge**, so anything they need beyond it would weaken the proof.

# Every bridge call goes through here, so a refusal is reported once and the
# same way. `friring-cli bridge` exits non-zero on a refusal, and the refusal is
# a well-formed JSON answer that the caller can read.
call() {
    verb=$1
    shift
    if ! friring-cli bridge "$verb" "$@"; then
        printf 'conformance: bridge %s was refused\n' "$verb" >&2
        return 1
    fi
}

# Say where we are. Every phase of the run reports, so an operator watching
# `session get --json` sees the contract being exercised rather than a pane
# that has gone quiet.
note() {
    call report --phase "$1" --progress "$2" --summary "$3" >/dev/null || true
}

# Report running through the status channel a sandboxed session has.
#
# This is what S9 accepts as proof that a child is up, and it accepts nothing
# else: a live pane shows something is running, not that it is running from the
# private state friring seeded. A real agent's hooks write this file; these
# agents have no hooks (`hook_schema = "none"`), so they write it themselves —
# one of the four words `parse_status_signal` reads, appended, and nothing else.
#
# A child that skipped this would sit in `starting` until the readiness timeout,
# and the conformance run would be proving the timeout rather than the bridge.
signal() {
    [ -n "${FRIRING_SIGNAL_FILE:-}" ] || return 0
    printf '%s\n' "$1" >>"$FRIRING_SIGNAL_FILE" 2>/dev/null || true
}

# Assert something about the boundary **from inside a launched session**.
#
# Distinct from what `friring-cli sandbox exec` can ask, and that is the point:
# `sandbox exec` composes a one-shot with no gate and no proxy, so the deny set
# it observes is the one a *question* gets. These run inside a boundary a real
# launch built, which is the only place some of the design's claims are true or
# false at all.
#
# Reported rather than fatal on their own — the run's own harness counts them —
# except where a hole would make the rest of the run meaningless.
boundary_denied() {
    what=$1
    shift
    if "$@" >/dev/null 2>&1; then
        printf 'conformance: BOUNDARY HOLE — %s was allowed\n' "$what"
        return 1
    fi
    printf 'conformance: boundary ok — %s is refused\n' "$what"
}

boundary_allowed() {
    what=$1
    shift
    if "$@" >/dev/null 2>&1; then
        printf 'conformance: boundary ok — %s is allowed\n' "$what"
        return 0
    fi
    printf 'conformance: BOUNDARY HOLE — %s was refused\n' "$what"
    return 1
}

# Refuse loudly. A conformance agent that carried on after an unexpected answer
# would report success for a bridge that is not working.
die() {
    printf 'conformance: %s\n' "$1" >&2
    exit 1
}
