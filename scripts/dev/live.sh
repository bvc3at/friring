#!/usr/bin/env bash
#
# Run a friring *dev build* against your REAL sessions — the installed release's
# tmux server, database, and config. The opposite of sandbox.sh: no isolation.
# Use it to verify a feature against your live workloads before releasing it.
#
# Why it works: quitting friring only detaches (tmux keeps every agent alive),
# and startup re-adopts the live `tb-`/`tbs-` windows. This script launches
# target/debug/friring pointed at the release socket, tmux group session, data
# dir, and config dir (FRIRING_SOCKET / FRIRING_TMUX_SESSION / FRIRING_DATA_DIR
# / FRIRING_CONFIG_DIR), so the dev binary adopts everything the release binary
# just detached from. Quit the dev TUI to hand the sessions back.
#
# Safety:
#   - refuses to start while a client is attached to the release server (there
#     is no single-instance lock; two TUIs would fight over the same panes) —
#     quit your installed friring first
#   - backs up the release DB before launching (schema migrations run on open
#     and are forward-only: a dev branch that bumps SCHEMA_VERSION migrates the
#     real DB irreversibly; the release binary refuses a newer DB)
#
# Usage:
#   scripts/dev/live.sh                # build, then launch the dev TUI live
#   scripts/dev/live.sh --no-build     # skip the cargo build
#   scripts/dev/live.sh --shell        # a shell with the live env (dev friring-cli)
#   scripts/dev/live.sh -- session list # run a friring-cli command live
#
# Requires: cargo, tmux >= 3.2; sqlite3 for the transactional DB backup
# (falls back to a file copy).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

log() { printf '\033[1;36m==>\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# The release binary's compile-time defaults (dev builds use friring-dev).
LIVE_SOCKET="friring"
LIVE_SESSION="friring"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/friring"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/friring"
BACKUP_KEEP=5

build=1
action="tui"
cli_args=()

while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) build=0; shift ;;
        --shell) action="shell"; shift ;;
        --) shift; action="cli"; cli_args=("$@"); break ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

command -v cargo >/dev/null || die "cargo not found"
[ -n "${TBX_IN_SANDBOX:-}" ] && die "running inside a sandbox shell — its TMUX_TMPDIR would hide the real server"

# No single-instance lock exists, so gate the handoff by hand: any client on
# the release server (the release TUI's control-mode attach, or a manual
# `tmux -L friring attach`) means the sessions are still owned elsewhere.
clients="$(tmux -L "$LIVE_SOCKET" list-clients 2>/dev/null || true)"
[ -z "$clients" ] || die "a client is attached to the '$LIVE_SOCKET' tmux server — quit the installed friring first"

if [ "$build" = "1" ]; then
    log "building friring (dev)"
    ( cd "$REPO_ROOT" && cargo build --bin friring --bin friring-cli >&2 )
fi

# Back up the DB before the dev binary's migrations touch it. Prefer sqlite3
# .backup: the automation-heartbeat window keeps writing ticks even with no
# TUI attached, and .backup is transactional where a plain copy is not.
db="$DATA_DIR/friring.db"
if [ -f "$db" ]; then
    backup="$db.dev-live-$(date +%Y%m%d-%H%M%S).bak"
    if command -v sqlite3 >/dev/null; then
        sqlite3 "$db" ".backup '$backup'"
    else
        log "sqlite3 not found — falling back to a file copy (racy vs. heartbeat writes)"
        cp "$db" "$backup"
        for suffix in -wal -shm; do
            [ -f "$db$suffix" ] && cp "$db$suffix" "$backup$suffix"
        done
    fi
    log "DB backed up to $backup"
    # Keep the newest $BACKUP_KEEP backups; the timestamp names sort by age.
    find "$DATA_DIR" -maxdepth 1 -name 'friring.db.dev-live-*.bak' | sort -r |
        tail -n +$((BACKUP_KEEP + 1)) | while IFS= read -r old; do
            rm -f "$old" "$old-wal" "$old-shm"
        done
fi

export FRIRING_SOCKET="$LIVE_SOCKET"
export FRIRING_TMUX_SESSION="$LIVE_SESSION"
export FRIRING_DATA_DIR="$DATA_DIR"
export FRIRING_CONFIG_DIR="$CONFIG_DIR"
# Dev friring-cli first, so a *newly spawned* agent's status hook exercises the
# dev CLI too. Already-running agents keep the PATH they were spawned with.
export PATH="$REPO_ROOT/target/debug:$PATH"

case "$action" in
    shell)
        log "live shell — friring/friring-cli are the DEV build targeting your REAL state; exit to leave"
        "${SHELL:-bash}" -i
        ;;
    cli) exec "$REPO_ROOT/target/debug/friring-cli" "${cli_args[@]}" ;;
    tui)
        log "launching dev friring on your live sessions (socket '$LIVE_SOCKET', session '$LIVE_SESSION')"
        "$REPO_ROOT/target/debug/friring"
        log "live sessions handed back — relaunch your installed friring to resume on the release binary"
        ;;
esac
