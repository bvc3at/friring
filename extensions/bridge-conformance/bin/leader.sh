#!/bin/sh
# A leader that exercises every verb it is allowed and then stops.
#
# What it proves, in order: a session can ask about itself, create a child in a
# repository it works in, mail that child, read the child's mail back, see
# friring's own verdict after the child finishes, and be refused the things it
# must be refused.
set -eu
# shellcheck source=extensions/bridge-conformance/lib/conformance.sh
# shellcheck disable=SC1091
. "${0%/*}/../lib/conformance.sh"

: "${FRIRING_BRIDGE_DIR:?the bridge directory is not set: this session was not granted the bridge}"
repo=${CONFORMANCE_REPO:-$PWD}
# Scratch, in a directory this boundary actually grants. **Never `/tmp`**: the
# host temp root is where friring's own tmux server listens, so every profile
# denies it outright (`sandbox::dirs`) — a leader that wrote there would die on
# its first redirect, inside a boundary that was working perfectly.
# `TMPDIR` inside a sandbox is the per-session scratch directory friring minted.
work=${TMPDIR:-$repo}

# The leader has no hooks either, so it reports its own status the same way its
# worker does — otherwise the pane looks idle to an operator watching the list.
signal working

note planning 5 'conformance leader starting'

printf '== status ==\n'
call status --json || die 'status was refused'

# What only a **launched** session can be asked. `friring-cli sandbox exec`
# composes a one-shot with no gate and no proxy, so these three are the ones its
# probes cannot reach: this process is inside a boundary a real launch built, and
# it is running at all only because the launch helper could read this launch's
# own gate through the read-only re-grant.
printf '== the boundary, from inside a real launch ==\n'
boundary_allowed "this session's own bridge directory is writable" \
    sh -c "printf x > '$FRIRING_BRIDGE_DIR/.probe'"
rm -f "$FRIRING_BRIDGE_DIR/.probe" 2>/dev/null || true
if [ -n "${FRIRING_DATA_DIR:-}" ]; then
    boundary_denied "friring's gate root is unreadable" ls "$FRIRING_DATA_DIR/gates"
    boundary_denied "friring's gate root is unwritable" \
        sh -c "printf x > '$FRIRING_DATA_DIR/gates/planted'"
    boundary_denied "friring's database is unreadable" cat "$FRIRING_DATA_DIR/friring.db"
else
    printf 'conformance: FRIRING_DATA_DIR is not set; skipping the host-tree assertions\n'
fi

printf '== depth rule ==\n'
# A child may not create children. Asserted from the leader by creating one and
# having *it* try — see worker.sh; here we only record that we are the owner.
note implementing 20 'creating one child'

key=$(printf 'conformance-%s' "$(date +%s)")
if call create \
    --key "$key" \
    --repo-root "$repo" \
    --branch "conformance/$key" \
    --agent conformance-worker \
    --task-kind conformance \
    --task-body 'Reply with a result. Do not commit anything.' \
    --json > "$work/conformance-create.json"
then
    printf '== created ==\n'
    cat "$work/conformance-create.json"
else
    die 'create was refused; check that the profile grants child-lifecycle and lists the agent'
fi

note verifying 60 'waiting for the child to finish'
# The child sends a `result`; friring stops it, reads its worktree and records a
# verdict. `status` is where that verdict appears.
i=0
terminal=0
while [ "$i" -lt 120 ]; do
    call status --json > "$work/conformance-status.json" || die 'status was refused'
    if grep -q '"state":"done"' "$work/conformance-status.json" ||
       grep -q '"state":"failed"' "$work/conformance-status.json" ||
       grep -q '"state":"dirty"' "$work/conformance-status.json"
    then
        terminal=1
        break
    fi
    i=$((i + 1))
    sleep 2
done

printf '== final status ==\n'
cat "$work/conformance-status.json"

# The wait running out is a failure, not a pass. A child left in `starting`,
# `ready`, `stalled`, `finishing` or `stop_failed` means the bridge did *not*
# answer every verb, and certifying it would invert the point of this extension.
[ "$terminal" = 1 ] || die 'the child never reached a terminal state within 240s'

# ── The parking lifecycle ────────────────────────────────────────────────
#
# A clean `stop` releases a child's runtime and its fan-out slot while keeping
# the child: its id, its ownership row, its branch and worktree, and its private
# agent state. A later `resume` brings that same child back. Everything below is
# that cycle driven through the real broker, against real worktrees and real
# panes — the part no in-process test can reach, because what it is about is
# what survives a process going away.
printf '== parking ==\n'
note implementing 30 'parking a child and resuming it'

child_id_of() { sed -n 's/.*"child_id":"\([^"]*\)".*/\1/p' "$1" | head -1; }

# `status --human`, not `--json`: the JSON answer is one compact line whose
# child objects contain nested objects, so no `sh`-sized pattern can bound one
# child's fields — an early attempt matched from a child's `"id"` to the *next*
# child's `"state"` and read every state as empty. The human rendering puts one
# child per line as `<state> <name> <id>`, with its latest report on the line
# after, which is exactly what is needed and needs no parser.
status_human() {
    call status --human > "$work/park-status.txt" || die 'status was refused'
}

# How many of this owner's children hold a fan-out slot.
live_children() {
    status_human
    awk '$1 ~ /^(starting|ready|working|blocked|stalled|finishing|dirty|stop_failed)$/ {n++}
         END {print n+0}' "$work/park-status.txt"
}

park_worker() {
    call create --key "$1" --repo-root "$repo" --branch "conformance/$1" \
        --agent conformance-worker --task-kind conformance-park \
        --task-body '{"conformance-park":true}' --json > "$work/$1.json" \
        || die "the parking create '$1' was refused"
    child_id_of "$work/$1.json"
}

# The state `status` reports for one child.
child_state() {
    status_human
    awk -v id="$1" '$3 == id {print $1}' "$work/park-status.txt"
}

# The summary of one child's latest report. The report line follows its child's
# own line, so the id is what anchors it.
child_report() {
    status_human
    grep -A2 -F "$1" "$work/park-status.txt" \
        | sed -n 's/.*child-authored report \[[^]]*\]: //p' | head -1
}

# Poll until a child reaches one of `$2` (an ERE alternation). A running child
# is `ready` or `working` depending on whether its own status signal has been
# mirrored yet, so waiting for either is waiting for "it is up".
await_state() {
    j=0
    while [ "$j" -lt 90 ]; do
        if child_state "$1" | grep -Eqx "$2"; then
            return 0
        fi
        j=$((j + 1))
        sleep 2
    done
    printf 'conformance: %s is "%s", not %s\n' "$1" "$(child_state "$1")" "$2"
    cat "$work/park-status.txt"
    return 1
}

parked=$(park_worker "conformance-park-$(date +%s)")
[ -n "$parked" ] || die 'the parking create named no child'
await_state "$parked" 'ready|working' || die "the parked child never became ready"
printf 'conformance: park child is %s\n' "$parked"

# Its marker, written by the child into its own private state directory on its
# first launch and reported from there. Read before the stop, so the comparison
# after the resume is against something recorded while the first process was
# alive. Polled, because being up and having reported are two different moments.
marker=''
i=0
while [ "$i" -lt 60 ]; do
    marker=$(child_report "$parked" | sed -n 's/^PARK-MARKER=//p')
    [ -z "$marker" ] || break
    i=$((i + 1))
    sleep 2
done
[ -n "$marker" ] || die 'the parked child never reported its private-state marker'
printf 'conformance: park marker before the stop is %s\n' "$marker"

call stop "$parked" --grace-secs 0 --json > "$work/park-stop.json" \
    || die 'the stop of the parked child was refused'
await_state "$parked" stopped || die 'the stopped child never reached stopped'
if [ "$(live_children)" = "0" ]; then
    printf 'conformance: park ok — a clean stop released the fan-out slot\n'
else
    die 'a clean stop did not release the fan-out slot'
fi

# Fill the fan-out, so the resume below has to be refused for capacity and for
# nothing else. Two, because that is what this profile allows.
fill_a=$(park_worker "conformance-fill-a-$(date +%s)")
fill_b=$(park_worker "conformance-fill-b-$(date +%s)")
await_state "$fill_a" 'ready|working' || die 'the first filler never became ready'
await_state "$fill_b" 'ready|working' || die 'the second filler never became ready'

if call resume "$parked" --json > "$work/park-resume-full.json"; then
    die 'a resume was allowed with the fan-out full'
fi
if grep -q '"error":"fanout_exhausted"' "$work/park-resume-full.json"; then
    printf 'conformance: park ok — a resume at a full fan-out is refused fanout_exhausted\n'
else
    printf 'conformance: park refusal was not fanout_exhausted: %s\n' \
        "$(cat "$work/park-resume-full.json")"
    die 'the resume was refused for the wrong reason'
fi
# And the refusal changed nothing: the parked child is still exactly parked.
await_state "$parked" stopped || die 'a refused resume moved the parked child'
printf 'conformance: park ok — the refused resume left the child stopped\n'

call stop "$fill_a" --grace-secs 0 --json > "$work/park-fill-stop.json" \
    || die 'stopping a filler was refused'
await_state "$fill_a" stopped || die 'the filler never released its slot'

call resume "$parked" --json > "$work/park-resume.json" \
    || die 'the resume was refused once a slot was free'
await_state "$parked" 'ready|working' || die 'the resumed child never became ready again'
printf 'conformance: park ok — the same child resumed once a slot was free\n'

# New work, claimed by the **relaunched** process, answered with the marker its
# previous life wrote. That is the whole claim in one exchange: same child, same
# private state, and a mailbox that still works.
call send --to "$parked" --kind task --body '{"park-resume-check":true}' >/dev/null \
    || die 'mailing the resumed child was refused'
i=0
resumed=0
while [ "$i" -lt 60 ]; do
    if [ "$(child_report "$parked")" = "PARK-RESUMED-MARKER=$marker" ]; then
        resumed=1
        break
    fi
    i=$((i + 1))
    sleep 2
done
if [ "$resumed" = 1 ]; then
    printf 'conformance: park ok — the resumed child claimed new mail and still holds %s\n' \
        "$marker"
else
    cat "$work/park-status.txt"
    die 'the resumed child never answered from its preserved private state'
fi

# Leave nothing running: every later assertion reads the same status.
call stop "$parked" --grace-secs 0 --json >/dev/null || true
call stop "$fill_b" --grace-secs 0 --json >/dev/null || true

note 'done' 100 'conformance leader finished'
printf 'conformance: the parking lifecycle held\n'
printf 'conformance: the bridge answered every verb this leader is allowed\n'
