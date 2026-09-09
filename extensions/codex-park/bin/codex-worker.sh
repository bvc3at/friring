#!/bin/sh
# The bridge child: the **real** Codex CLI, interactive, in the pane friring
# opened for it.
#
# A wrapper rather than `command = "codex"` for three reasons.
#
# Codex gates hooks behind a per-entry trust prompt keyed on the command string,
# and the hash it persists is not something friring can pre-seed — so a child
# would sit on "Hooks need review" and its status hooks would never fire, which
# is precisely what S9 waits for. `--dangerously-bypass-hook-trust` is Codex's
# own escape hatch for automation that has already vetted its hook sources, and
# here the only hooks present are the ones friring seeded a moment ago.
#
# The second is placement: the flag is global on a fresh launch and has to
# follow the subcommand on a relaunch, and friring appends `resume_args` as a
# tail.
#
# The third is folder trust, below.
#
# Interactive on purpose. `codex exec` would prove nothing about parking: a
# one-shot has no thread to preserve.
set -eu

# shellcheck source=extensions/codex-park/lib/park.sh
# shellcheck disable=SC1091
. "${0%/*}/../lib/park.sh"

: "${CODEX_HOME:?CODEX_HOME is not set: this child was not given private state}"

codex=${CODEX_PARK_BIN:-codex}
command -v "$codex" >/dev/null 2>&1 \
    || die "no '$codex' on PATH inside this boundary — the profile must grant the directory it lives in, read-only"

# The turn script, handed to the model stub through the environment: a fixture
# names `$CODEX_PARK_TURN`, so the substance of a turn stays in a file rather
# than quoted into JSON. `$0` is absolute here (the manifest's `{home}` is), so
# this survives any working directory.
CODEX_PARK_TURN="${0%/*}/park-turn.sh"
export CODEX_PARK_TURN

# App-level offline, the way `scripts/dev/agent-e2e/agents/codex/profile.sh` does
# it. This child's profile is `network_mode = "full"` — it has to reach a
# loopback port friring's egress proxy cannot name — so the boundary is not the
# thing keeping Codex off the network here. A dead proxy for everything except
# loopback is: the stub stays reachable and an update check, a telemetry post or
# a plugin clone has nowhere to go.
http_proxy=http://127.0.0.1:9
https_proxy=$http_proxy
HTTP_PROXY=$http_proxy
HTTPS_PROXY=$http_proxy
no_proxy=127.0.0.1,localhost
NO_PROXY=$no_proxy
export http_proxy https_proxy HTTP_PROXY HTTPS_PROXY no_proxy NO_PROXY

# Folder trust. Codex asks "Do you trust the contents of this directory?" for
# any directory it has no `[projects]` entry for, and friring's own modal guard
# then refuses to type into that pane (`agent::tmux::MODAL_MARKERS` matches both
# the question and its numbered list) — so an untrusted child never receives the
# nudge that would give it its first turn, and the run stalls with no error
# anywhere.
#
# It cannot be seeded ahead of time: the entry is keyed on the child's worktree,
# which friring mints during the spawn saga and no fixture knows in advance.
# Written from inside the child, where `$PWD` *is* that worktree.
#
# `pwd -P` because Codex records the resolved path: under a `/tmp` root — where
# every harness here puts its sandbox — the symlinked spelling silently misses.
# `-c projects."…".trust_level=…` was probed against 0.153.4 and does **not**
# work; the config file is the only surface that takes.
trust_dir=$(pwd -P)
if ! grep -qF "[projects.\"$trust_dir\"]" "$CODEX_HOME/config.toml" 2>/dev/null; then
    printf '\n[projects."%s"]\ntrust_level = "trusted"\n' "$trust_dir" \
        >> "$CODEX_HOME/config.toml" \
        || die "could not record folder trust in $CODEX_HOME/config.toml"
fi

case "${1:-}" in
    resume)
        sub=$1
        shift
        exec "$codex" "$sub" --dangerously-bypass-hook-trust "$@"
        ;;
    *)
        exec "$codex" --dangerously-bypass-hook-trust "$@"
        ;;
esac
