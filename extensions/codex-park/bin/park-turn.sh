#!/bin/sh
# One turn of the codex-park child, run by the **child's own Codex** through its
# shell tool.
#
# A script rather than a shell one-liner inside the model fixture: what this does
# is the substance of the parking proof, and it belongs where it can be read,
# linted and reasoned about — beside the agent it runs as, not quoted into JSON.
#
# It is deliberately the same on every turn. Which turn this is cannot be a
# parameter, because the model stub cannot tell one nudge from the next: friring
# sends the same text every time. So the turn decides from state instead — the
# marker file its first life wrote, and whatever is actually in the mailbox now.
# That makes an extra nudge harmless, which matters because nudges are rate
# limited rather than counted and a run must not depend on how many arrive.
set -eu

# shellcheck source=extensions/codex-park/lib/park.sh
# shellcheck disable=SC1091
. "${0%/*}/../lib/park.sh"

: "${CODEX_HOME:?CODEX_HOME is not set: this turn is not running inside a bridge child}"

task="${TMPDIR:-$CODEX_HOME}/park-turn-task.json"

# Identifies the **process** that took this turn, where the marker below
# deliberately identifies the child across all of them. Printed so it lands in
# Codex's own rollout, where it is the only thing that tells a replayed
# conversation from a blank one: the marker file survives a stop, so a brand-new
# thread would report the same marker on its first turn, and a rollout carrying
# a turn id from a process that no longer exists could only have been replayed.
turn_id="TURN-$$-$(date +%s)"
printf 'codex-park: turn %s\n' "$turn_id"

# An empty mailbox is an ordinary outcome here, not a refusal: friring nudges on
# unread mail, and a turn can still be the one that arrives after the mail was
# already claimed. `|| :` keeps the last claim in place for the check below.
call inbox --claim --json > "$task" 2>/dev/null || : > "$task"

# The one piece of state the whole harness turns on. Written into this child's
# **private** CODEX_HOME (ADR-31), which is the directory a clean stop keeps and
# a resume hands back — so a marker quoted after the resume could only have come
# from the process that ran before it.
marker_file="$CODEX_HOME/park-marker"
[ -f "$marker_file" ] || printf 'PARK-%s-%s\n' "$$" "$(date +%s)" > "$marker_file"
marker=$(cat "$marker_file")

# `call` and not `note`: `note` swallows a refusal, and a marker that never
# arrives is the owner waiting out a timeout for a reason only this pane knows.
if grep -q 'park-resume-check' "$task" 2>/dev/null; then
    call report --phase implementing --progress 60 \
        --summary "PARK-RESUMED-MARKER=$marker" >/dev/null \
        || die 'the resumed marker report was refused'
    printf 'codex-park: answered the resume check with %s\n' "$marker"
else
    call report --phase implementing --progress 40 \
        --summary "PARK-MARKER=$marker" >/dev/null \
        || die 'the parking marker report was refused'
    printf 'codex-park: recorded %s\n' "$marker"
fi
