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
# Requires: cargo (unless --no-build), tmux >= 3.2, and sqlite3 — the DB backup
# uses sqlite3's transactional `.backup`; there is no racy file-copy fallback,
# so the recovery copy is always a coherent snapshot.

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

[ -n "${TBX_IN_SANDBOX:-}" ] && die "running inside a sandbox shell — its TMUX_TMPDIR would hide the real server"

# No single-instance lock exists, so gate the handoff by hand: any client on
# the release server (the release TUI's control-mode attach, or a manual
# `tmux -L friring attach`) means the sessions are still owned elsewhere.
# Checked twice — now (fail fast, before a slow build) and again right before
# launch, since the build/backup window is long enough for someone to reopen
# the installed friring in between.
require_no_clients() {
    local clients
    clients="$(tmux -L "$LIVE_SOCKET" list-clients 2>/dev/null || true)"
    [ -z "$clients" ] || die "a client is attached to the '$LIVE_SOCKET' tmux server — quit the installed friring first"
}
require_no_clients

if [ "$build" = "1" ]; then
    command -v cargo >/dev/null || die "cargo not found"
    log "building friring (dev)"
    ( cd "$REPO_ROOT" && cargo build --bin friring --bin friring-cli >&2 )
fi

# Back up the DB before the dev binary's migrations touch it. sqlite3 `.backup`
# is transactional — the automation-heartbeat window keeps writing ticks even
# with no TUI attached, so a plain file copy could capture a torn WAL. Fail
# closed rather than write an unusable "backup": the whole safety story here is
# a restorable snapshot, and sqlite3 ships with macOS / is one package away.
db="$DATA_DIR/friring.db"
if [ -f "$db" ]; then
    command -v sqlite3 >/dev/null || die "sqlite3 not found — needed for a consistent DB backup before live migrations (install it, or restore manually and use --no-build with care)"
    backup="$db.dev-live-$(date +%Y%m%d-%H%M%S).bak"
    sqlite3 "$db" ".backup '$backup'"
    log "DB backed up to $backup"
    # Keep the newest $BACKUP_KEEP backups; the timestamp names sort by age. A
    # `.backup` snapshot is a single self-contained file (no -wal/-shm sidecars).
    find "$DATA_DIR" -maxdepth 1 -name 'friring.db.dev-live-*.bak' | sort -r |
        tail -n +$((BACKUP_KEEP + 1)) | while IFS= read -r old; do
            rm -f "$old"
        done
fi

# The build + backup above can take a while; make sure the installed friring
# wasn't reopened in that window before we grab the panes.
require_no_clients

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
