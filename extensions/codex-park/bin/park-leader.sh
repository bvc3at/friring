#!/bin/sh
# Drive one bridge child's whole parking lifecycle, with a **real interactive
# Codex** as the child.
#
# In order: create, park with a clean stop, watch the fan-out slot come back,
# fill the fan-out so a resume must be refused for capacity, check the refusal
# changed nothing, free the slot, resume the same child, and give it new work
# that the relaunched process claims and answers from the private state its
# previous life wrote.
#
# The worker is the vendor CLI, so every one of those steps is about a process
# that really stops existing and really comes back.
set -eu

# shellcheck source=extensions/codex-park/lib/park.sh
# shellcheck disable=SC1091
. "${0%/*}/../lib/park.sh"

: "${FRIRING_BRIDGE_DIR:?the bridge directory is not set: this session was not granted the bridge}"
repo=${CODEX_PARK_REPO:-$PWD}
# `TMPDIR` inside a sandbox is the per-session scratch directory friring minted.
# Never `/tmp`: every profile denies the host temp root outright, because that
# is where friring's own tmux server listens.
work=${TMPDIR:-$repo}

signal working
note planning 5 'codex-park leader starting'

printf '== status ==\n'
call status --human || die 'status was refused'

# ── helpers ──────────────────────────────────────────────────────────────
#
# `status --human` rather than `--json`: the JSON answer is one compact line
# whose child objects contain nested objects, so no `sh`-sized pattern can bound
# one child's fields. The human rendering is one child per line as
# `<state> <name> <id>`, with its latest report on the line after.
status_human() {
    call status --human > "$work/park-status.txt" || die 'status was refused'
}

live_children() {
    status_human
    awk '$1 ~ /^(starting|ready|working|blocked|stalled|finishing|dirty|stop_failed)$/ {n++}
         END {print n+0}' "$work/park-status.txt"
}

child_state() {
    status_human
    awk -v id="$1" '$3 == id {print $1}' "$work/park-status.txt"
}

child_report() {
    status_human
    grep -A2 -F "$1" "$work/park-status.txt" \
        | sed -n 's/.*child-authored report \[[^]]*\]: //p' | head -1
}

child_id_of() { sed -n 's/.*"child_id":"\([^"]*\)".*/\1/p' "$1" | head -1; }

# Poll until a child reaches one of `$2` (an ERE alternation), for at most `$3`
# passes of two seconds. A running child is `ready` or `working` depending on
# whether its own status signal has been mirrored yet.
await_state() {
    j=0
    while [ "$j" -lt "${3:-90}" ]; do
        if child_state "$1" | grep -Eqx "$2"; then
            return 0
        fi
        j=$((j + 1))
        sleep 2
    done
    printf 'codex-park: %s is "%s", not %s\n' "$1" "$(child_state "$1")" "$2"
    cat "$work/park-status.txt"
    return 1
}

codex_child() {
    call create --key "$1" --repo-root "$repo" --branch "codex-park/$1" \
        --agent codex-park-worker --task-kind codex-park \
        --task-body 'Report the park marker from your private state.' \
        --timeout 600 --json > "$work/$1.json" \
        || die "the create '$1' was refused"
    child_id_of "$work/$1.json"
}

# ── the lifecycle ────────────────────────────────────────────────────────

note implementing 20 'creating an interactive codex child'
parked=$(codex_child "codex-park-$(date +%s)")
[ -n "$parked" ] || die 'the create named no child'
printf 'codex-park: child is %s\n' "$parked"

# Its marker, written by the child into its own private CODEX_HOME on its first
# turn and reported from there. The turn happens because friring nudges the
# child about the task mail its create enqueued — so this line also says the
# nudge reached a live vendor pane.
marker=''
i=0
while [ "$i" -lt 90 ]; do
    marker=$(child_report "$parked" | sed -n 's/^PARK-MARKER=//p')
    [ -z "$marker" ] || break
    i=$((i + 1))
    sleep 2
done
[ -n "$marker" ] || die 'the child never reported a marker from its private state'
printf 'codex-park: marker before the stop is %s\n' "$marker"

call stop "$parked" --grace-secs 0 --json > "$work/park-stop.json" \
    || die 'the stop was refused'
await_state "$parked" stopped || die 'the stopped child never reached stopped'
if [ "$(live_children)" = "0" ]; then
    printf 'codex-park: ok — a clean stop released the fan-out slot\n'
else
    die 'a clean stop did not release the fan-out slot'
fi

# Fill the fan-out, so the resume below has to be refused for capacity and for
# nothing else. This profile allows one.
filler=$(codex_child "codex-fill-$(date +%s)")
await_state "$filler" 'ready|working|starting' || die 'the filler never came up'

if call resume "$parked" --json > "$work/park-resume-full.json"; then
    die 'a resume was allowed with the fan-out full'
fi
if grep -q '"error":"fanout_exhausted"' "$work/park-resume-full.json"; then
    printf 'codex-park: ok — a resume at a full fan-out is refused fanout_exhausted\n'
else
    printf 'codex-park: the refusal was not fanout_exhausted: %s\n' \
        "$(cat "$work/park-resume-full.json")"
    die 'the resume was refused for the wrong reason'
fi
await_state "$parked" stopped || die 'a refused resume moved the parked child'
printf 'codex-park: ok — the refused resume left the child stopped\n'

call stop "$filler" --grace-secs 0 --json > "$work/park-fill-stop.json" \
    || die 'stopping the filler was refused'
await_state "$filler" stopped || die 'the filler never released its slot'

call resume "$parked" --json > "$work/park-resume.json" \
    || die 'the resume was refused once a slot was free'
await_state "$parked" 'ready|working' || die 'the resumed child never came up again'
printf 'codex-park: ok — the same child resumed once a slot was free\n'

# New work, claimed by the **relaunched** Codex process, answered with the
# marker its previous life wrote into the private state directory friring kept.
call send --to "$parked" --kind task --body '{"park-resume-check":true}' >/dev/null \
    || die 'mailing the resumed child was refused'
i=0
resumed=0
while [ "$i" -lt 120 ]; do
    if [ "$(child_report "$parked")" = "PARK-RESUMED-MARKER=$marker" ]; then
        resumed=1
        break
    fi
    i=$((i + 1))
    sleep 2
done
if [ "$resumed" = 1 ]; then
    printf 'codex-park: ok — the resumed child claimed new mail and still holds %s\n' "$marker"
else
    cat "$work/park-status.txt"
    die 'the resumed child never answered from its preserved private state'
fi

# Leave nothing running.
call stop "$parked" --grace-secs 0 --json >/dev/null || true

note 'done' 100 'codex-park leader finished'
printf 'codex-park: the interactive parking lifecycle held\n'
