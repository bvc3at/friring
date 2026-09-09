#!/bin/sh
# A worker that exercises what a child may do, and proves what it may not.
set -eu
# shellcheck source=extensions/bridge-conformance/lib/conformance.sh
# shellcheck disable=SC1091
. "${0%/*}/../lib/conformance.sh"

: "${FRIRING_BRIDGE_DIR:?the bridge directory is not set: this child was not granted the bridge}"
: "${CONFORMANCE_HOME:?the private state directory is not set: this child was not narrowed}"

# Before the first bridge call: S9 will not release this child from `starting`
# until its own status channel says it is up, and a `report` is not that channel.
signal working

note planning 10 'conformance worker starting'

printf '== my task ==\n'
task_file="${TMPDIR:-$CONFORMANCE_HOME}/conformance-task.json"
call inbox --claim --json > "$task_file" || die 'inbox was refused'
cat "$task_file"

# A **parking** worker is the one this run stops and resumes rather than lets
# finish. It never sends a result, so nothing terminalizes it, and it keeps
# claiming its mail so a resumed child can be given new work.
#
# Which kind this is cannot come from the mail on a relaunch: a resume
# deliberately delivers no second copy of the task, so the inbox is empty the
# second time. The marker file decides it from then on — and that it is still
# there to decide with is the private-state claim (ADR-31) this stage exists to
# observe.
marker_file="$CONFORMANCE_HOME/park-marker"
if [ -f "$marker_file" ] || grep -q 'conformance-park' "$task_file"; then
    if [ ! -f "$marker_file" ]; then
        printf 'PARK-%s-%s\n' "$$" "$(date +%s)" > "$marker_file"
    fi
    marker=$(cat "$marker_file")
    printf 'conformance: parking worker, marker %s\n' "$marker"
    # Reported rather than mailed, because a report is what the owner reads back
    # through `status` without having to drain a mailbox it also uses for
    # lifecycle mail. `call` and not `note`: `note` swallows a refusal, and a
    # marker that never arrives is the owner waiting out a timeout for a reason
    # only this pane can name.
    call report --phase implementing --progress 40 --summary "PARK-MARKER=$marker" >/dev/null \
        || die 'the parking marker report was refused'

    # Answer whatever was in the last claim, wherever it came from. The
    # **initial** claim above counts: this process signalled it was up before
    # making it, so on a relaunch the owner can see the resume finish and mail
    # its check inside that window — and a check consumed by the initial claim
    # and never looked at again is a run that waits out its timeout with every
    # step of the lifecycle having worked.
    answer_claim() {
        grep -q 'park-resume-check' "$task_file" || return 0
        # Claimed after a resume, from the relaunched process, quoting a marker
        # only the pre-stop process could have written.
        call report --phase implementing --progress 60 \
            --summary "PARK-RESUMED-MARKER=$marker" >/dev/null \
            || die 'the resumed marker report was refused'
    }
    answer_claim
    while :; do
        if call inbox --claim --json > "$task_file" 2>/dev/null; then
            answer_claim
        fi
        sleep 2
    done
fi

printf '== the depth rule ==\n'
# A child may not create children, whatever its profile says: the broker
# intersects a child's grant with what a child may ever hold. This *must* be
# refused, and a conformance run that got a child here would be reporting a
# bridge that is not enforcing its one structural rule.
if call create \
    --key depth-check-00001 \
    --repo-root "$PWD" \
    --branch conformance/depth \
    --agent conformance-worker \
    --task-kind conformance \
    --task-body 'this must never be created' \
    --json 2>/dev/null
then
    die 'a child was allowed to create a child: the depth rule is not holding'
fi
printf 'refused, as it must be\n'

printf '== private state ==\n'
# The family's state directory is in the subtract set; this child's own is not.
if [ -r "$HOME/.friring-conformance/state" ] && [ "$CONFORMANCE_HOME" != "$HOME/.friring-conformance" ]
then
    die 'this child can read its family state directory: the subtract set is not holding'
fi
printf 'private state directory is %s\n' "$CONFORMANCE_HOME"

note verifying 80 'reporting a result'
# The one finish intent. A clean worktree plus `completed` is the only path to
# `done` — friring stops the pane and reads the worktree before deciding.
call send --to owner --kind result \
    --body '{"outcome":"completed","summary":"conformance worker exercised the bridge"}' \
    >/dev/null || die 'the result was refused'

# friring acknowledges, stops this pane and verifies. Nothing after this runs.
sleep 600
