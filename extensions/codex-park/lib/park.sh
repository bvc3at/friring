#!/bin/sh
# Shared helpers for the codex-park agents.
#
# POSIX `sh` with no dependencies, for the reason `bridge-conformance`'s are:
# what the leader proves is the **bridge**, so anything it needed beyond the
# bridge would weaken the proof.

# Every bridge call goes through here, so a refusal is reported once and the
# same way. `friring-cli bridge` exits non-zero on a refusal, and the refusal is
# a well-formed JSON answer the caller can read.
call() {
    verb=$1
    shift
    if ! friring-cli bridge "$verb" "$@"; then
        printf 'codex-park: bridge %s was refused\n' "$verb" >&2
        return 1
    fi
}

# Say where we are, so an operator watching `session get --json` sees the
# lifecycle being exercised rather than a pane that has gone quiet.
note() {
    call report --phase "$1" --progress "$2" --summary "$3" >/dev/null || true
}

# Report running through the status channel a sandboxed session has.
#
# The leader has no hooks (`hook_schema = "none"`), so it writes the file its
# own launch exported. Its **worker** does not use this: the worker is Codex,
# and its `SessionStart` hook is what writes the same file — which is the point
# of seeding the hooks at all.
signal() {
    [ -n "${FRIRING_SIGNAL_FILE:-}" ] || return 0
    printf '%s\n' "$1" >>"$FRIRING_SIGNAL_FILE" 2>/dev/null || true
}

# Refuse loudly. A leader that carried on after an unexpected answer would
# report success for a lifecycle that is not working.
die() {
    printf 'codex-park: %s\n' "$1" >&2
    exit 1
}
