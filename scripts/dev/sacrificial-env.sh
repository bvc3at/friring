#!/usr/bin/env bash
#
# Run a command under a throwaway outer environment — `scripts/dev/sacrificial-env.sh
# <command> [args…]`.
#
# This is the *outer* ring, and it exists because the inner ring can be wrong.
# Every dev harness here redirects HOME, the XDG roots and friring's own
# FRIRING_*_DIR overrides before it runs anything, and asserts as it goes. That
# is a guard, and a guard is code.
#
# What prompted it: the operator's live development database was once found at a
# schema an installed binary could not open, so something had resolved its data
# directory against the real HOME rather than an override. Which process was
# never established — a dev build shares that database with every other dev
# binary on the machine — and the harness suspected at the time has since been
# cleared of anything beyond opportunity. The durable lesson is not about that
# harness: no harness here could have told, because a process's resolved
# database path was not observable until the open, which is already the write.
#
# `friring-cli config paths` makes it observable, and each harness now proves its
# own isolation with it. This wrapper is the ring around that proof, for the case
# where the proof itself is wrong: it mints one private root, points every path
# variable a friring consults at it, and plants a **canary** exactly where a
# fallback to HOME would land. A leak then writes into the canary instead of the
# operator's tree, and is a reported failure rather than an invisible one.
#
# Use it around any command that starts a friring binary — the full test suite,
# the e2e harnesses, a probe. The command still does its own inner isolation;
# this only bounds what a broken inner guard can reach.
#
#   scripts/dev/sacrificial-env.sh cargo nextest run --all
#   scripts/dev/sacrificial-env.sh scripts/dev/bridge-e2e.sh
#
# CARGO_HOME/RUSTUP_HOME keep pointing at the real ones: cargo resolves its
# registry and toolchain through them, and a build under a fresh HOME would
# re-download the world. They are a build cache, not operator state — nothing
# here is what the wrapper is protecting.
set -euo pipefail

if [ "$#" -eq 0 ]; then
    echo "usage: sacrificial-env.sh <command> [args...]" >&2
    exit 2
fi

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)

# Resolved before HOME is replaced.
: "${CARGO_HOME:=$HOME/.cargo}"
: "${RUSTUP_HOME:=$HOME/.rustup}"
export CARGO_HOME RUSTUP_HOME

SACRIFICIAL_ROOT="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/friring-sacrificial.XXXXXX")" && pwd -P)"
export SACRIFICIAL_ROOT

HOME="$SACRIFICIAL_ROOT/home"
XDG_CONFIG_HOME="$SACRIFICIAL_ROOT/config"
XDG_DATA_HOME="$SACRIFICIAL_ROOT/data"
XDG_STATE_HOME="$SACRIFICIAL_ROOT/state"
XDG_CACHE_HOME="$SACRIFICIAL_ROOT/cache"
FRIRING_CONFIG_DIR="$SACRIFICIAL_ROOT/friring-config"
FRIRING_DATA_DIR="$SACRIFICIAL_ROOT/friring-data"
export HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME
export FRIRING_CONFIG_DIR FRIRING_DATA_DIR
mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" \
    "$XDG_CACHE_HOME" "$FRIRING_CONFIG_DIR" "$FRIRING_DATA_DIR"

# A private tmux socket dir, outside the root so a server started in here can
# never be the operator's.
#
# Under /tmp with a short name, not under $TMPDIR and not under the root: an
# AF_UNIX path is ~104 bytes, macOS's per-user $TMPDIR is already 49 of them,
# and tmux appends `/tmux-<uid>/<socket name>` — which leaves a socket two bytes
# inside the limit and a longer socket name outside it. `sandbox-env.sh` keeps
# its own dir short for exactly this reason. /tmp is no closer to the operator's
# files than $TMPDIR is.
SACRIFICIAL_TMUX="$(mktemp -d /tmp/fr-sacr.XXXXXX)"
export TMUX_TMPDIR="$SACRIFICIAL_TMUX"

# Session identity from a shell running inside a real friring session would be
# inherited by everything below and misattribute its signals.
unset FRIRING_SESSION FRIRING_SESSION_ID FRIRING_TASK FRIRING_METRICS_DIR \
    FRIRING_SOCKET FRIRING_SIGNAL_FILE FRIRING_BRIDGE_DIR

# A neutral global git config, because `git` reads its own from $HOME and a
# great deal of what runs under here commits. Without one, `git commit` fails on
# an unset identity and `git init` picks a default branch by git's version
# rather than by this repository's convention — failures that say nothing about
# the code and would be blamed on the isolation. Placeholder identity: nothing
# here may carry the operator's.
cat > "$HOME/.gitconfig" <<'GITCONFIG'
[user]
    name = friring sacrificial env
    email = sacrificial@friring.invalid
[init]
    defaultBranch = main
[commit]
    gpgsign = false
GITCONFIG

# --- the canary -------------------------------------------------------------
#
# Both spellings of the fallback, for both build flavors: a process that ignores
# FRIRING_DATA_DIR *and* XDG_DATA_HOME lands on $HOME/.local/share/friring[-dev],
# which is the exact path the incident touched. Pre-created with known content,
# so a leak is a changed digest rather than a guess.
CANARY_DIRS=()
for flavor in friring friring-dev; do
    CANARY_DIRS+=("$HOME/.local/share/$flavor" "$HOME/.config/$flavor")
done
for dir in "${CANARY_DIRS[@]}"; do
    mkdir -p "$dir"
done
for flavor in friring friring-dev; do
    printf 'sacrificial-env canary: nothing may write here\n' \
        > "$HOME/.local/share/$flavor/friring.db"
    printf 'sacrificial-env canary: nothing may write here\n' \
        > "$HOME/.config/$flavor/settings.toml"
done

# `shasum` on macOS, `sha256sum` on most Linux images; either is fine.
if command -v shasum >/dev/null; then
    sha256() { shasum -a 256; }
elif command -v sha256sum >/dev/null; then
    sha256() { sha256sum; }
else
    echo "sacrificial-env: no shasum or sha256sum; cannot check the canaries" >&2
    exit 2
fi

# A digest over every canary path *and* every canary file's content, so both an
# edit and a newly created file change it.
canary_digest() {
    {
        find "${CANARY_DIRS[@]}" 2>/dev/null | sort
        find "${CANARY_DIRS[@]}" -type f -exec cat {} + 2>/dev/null
    } | sha256 | awk '{print $1}'
}

CANARY_BEFORE="$(canary_digest)"

# Pre-create friring's own grouping session on the private socket dir, under
# both build flavors' names (`src/agent/tmux.rs`: socket and session share the
# name, `friring` or `friring-dev`).
#
# Several tests spawn a real tmux window, and friring's spawn path is
# check-then-create: `has-session`, and if absent `new-session -d -s <name>`.
# Run concurrently against one server, two tests can both see it absent and both
# create it, and tmux fails the loser with "duplicate session". Outside this
# wrapper that never happens, because the developer's own friring is already
# running and the session always exists — which is exactly the condition this
# wrapper otherwise removes. Creating it up front restores it, privately.
#
# Failure is ignored: a host without tmux still runs every test that needs none.
for name in friring friring-dev; do
    tmux -L "$name" new-session -d -s "$name" -x 80 -y 24 \
        'while :; do sleep 3600; done' >/dev/null 2>&1 || true
done

printf 'sacrificial-env: root %s\n' "$SACRIFICIAL_ROOT"
printf 'sacrificial-env: running %s\n' "$*"

set +e
(cd "$REPO_ROOT" && "$@")
STATUS=$?
set -e

# Kill anything left listening on the private socket dir before the check, so a
# server that is still writing cannot race it.
tmux -L friring-dev kill-server >/dev/null 2>&1 || true
tmux -L friring kill-server >/dev/null 2>&1 || true

CANARY_AFTER="$(canary_digest)"

if [ "$CANARY_BEFORE" != "$CANARY_AFTER" ]; then
    printf '\nsacrificial-env: ISOLATION FAILED\n' >&2
    printf 'Something resolved its friring paths against HOME rather than the\n' >&2
    printf 'overrides this wrapper exported. On a real environment that write\n' >&2
    printf 'would have landed on the operator'"'"'s own config or database.\n' >&2
    printf 'The evidence is kept at: %s\n' "$SACRIFICIAL_ROOT" >&2
    printf -- '--- what is under the canary paths now ---\n' >&2
    find "${CANARY_DIRS[@]}" 2>/dev/null >&2 || true
    exit 1
fi

printf 'sacrificial-env: canaries intact\n'

# Only a clean run is cleaned up: a failure's tree is the artifact you debug.
if [ "$STATUS" -eq 0 ]; then
    rm -rf "$SACRIFICIAL_ROOT" "$SACRIFICIAL_TMUX"
else
    printf 'sacrificial-env: kept %s (exit %d)\n' "$SACRIFICIAL_ROOT" "$STATUS" >&2
fi

exit "$STATUS"
