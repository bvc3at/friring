# shellcheck shell=bash
#
# Reading friring's own log from a harness, without the read deciding the run.
#
# A harness waits for something to appear in a pane. When the thing it is
# waiting for can never appear — the launch was refused, so no pane was ever
# created — the wait is the whole budget and the run then reports every later
# assertion as a failure of the feature rather than as "it never started". The
# TUI writes the refusal immediately, so it is read rather than waited out.
#
# The subtlety is that this runs under `set -euo pipefail`, where
# `x=$(grep …)` **exits the script** when grep matches nothing — which is the
# ordinary state of every poll before the wizard is answered. So "no match" is
# an empty result with status 0, and only a real read failure says anything.

# Print the first `Failed to spawn session: …` in a friring data directory's
# logs, or nothing. Always succeeds; a genuine read error is reported on stderr
# and still yields no refusal, because "could not read" is not "was refused".
harness_spawn_refusal() {
    local data_dir="$1" name="${2:-harness}" status=0 found=""
    local logs=()
    while IFS= read -r file; do
        logs+=("$file")
    done < <(find "$data_dir" -maxdepth 1 -name 'friring.log.*' -type f 2>/dev/null)
    [ "${#logs[@]}" -eq 0 ] && return 0
    found=$(grep -h -m1 -oE 'Failed to spawn session: .*' "${logs[@]}") || status=$?
    # grep's 1 is "not there yet". Anything above it is a read that failed, and
    # silently treating that as "no refusal" is how a harness waits out a budget
    # for a reason it could have named.
    if [ "$status" -gt 1 ]; then
        echo "$name: could not read the TUI log (grep exit $status)" >&2
        return 0
    fi
    printf '%s' "$found"
}
