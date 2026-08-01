# shellcheck shell=sh
#
# Shared dev-sandbox isolation helper — the single source of truth for running a
# friring *dev build* (`0.0.0-dev` => `dev_build` cfg, which uses the
# `friring-dev` tmux socket) against an isolated environment, without polluting
# your real friring config/sessions.
#
# Two isolation flavors:
#
#   tbx_sandbox_init      — *friring-only* isolation (DEFAULT for sandbox.sh):
#                           redirects only FRIRING_CONFIG_DIR / FRIRING_DATA_DIR
#                           (+ TMUX_TMPDIR), leaving HOME/XDG real so your real,
#                           authenticated agent CLIs (claude/codex/antigravity/…) work.
#   tbx_sandbox_init_full — *full* isolation: also overrides HOME + XDG_* under
#                           the sandbox (hermetic; agents boot with no creds).
#                           Used by the demo recorder + TUI smoke test.
#
# Source it (don't execute) AFTER setting REPO_ROOT (or TBX_REPO_ROOT). Written
# in POSIX sh so both bash callers (sandbox.sh, smoke/tui-smoke.sh) and the
# /usr/bin/env sh caller (record.sh) can source it. Both flavors prepend the
# repo's target/debug to PATH so an agent hook's `friring-cli` resolves to *this*
# dev binary (and writes to the sandbox DB the dev TUI reads).
#
# Build the binaries BEFORE calling init so cargo still resolves your ~/.cargo
# (the full flavor overrides $HOME).

: "${TBX_REPO_ROOT:=${REPO_ROOT:-}}"
if [ -z "$TBX_REPO_ROOT" ]; then
    echo "sandbox-env.sh: set REPO_ROOT (or TBX_REPO_ROOT) before sourcing" >&2
    # `return` works when sourced (the intended use); `exit` covers the mistake
    # of executing this file directly.
    # shellcheck disable=SC2317
    return 2 2>/dev/null || exit 2
fi
export TBX_REPO_ROOT

# The dev build's tmux socket name (mirrors src/agent/tmux.rs TMUX_SOCKET for a
# dev_build). It lives inside the sandbox's private TMUX_TMPDIR, so killing it
# can never reach a real server.
export TBX_DEV_SOCKET="friring-dev"

# Populated by an init call.
export TBX_SANDBOX_ROOT=""
export TBX_SANDBOX_FRESH=0

# tbx_sandbox_tmux_dir <profile> — short, stable tmux socket dir for a persistent
# profile (kept off the deep target/ path; AF_UNIX socket paths are length-limited).
tbx_sandbox_tmux_dir() {
    echo "${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/friring-sbx-${1:-default}"
}

# _tbx_resolve_root <fresh|persistent> [profile] — common setup shared by both
# init flavors: resolves TBX_SANDBOX_ROOT, exports TMUX_TMPDIR (short, see above),
# and prepends target/debug to PATH. Internal.
_tbx_resolve_root() {
    mode="${1:-persistent}"
    profile="${2:-default}"

    case "$mode" in
        fresh)
            # Under /tmp rather than $TMPDIR. On macOS the per-user $TMPDIR is
            # /var/folders/<2>/<28 chars>/T/, which makes the sandbox workspace
            # path ~60 characters before it reaches `ws/` — and that path is on
            # camera in every demo recording (an agent prints the file it just
            # wrote) and drives how wide a pane has to be for a scenario to see
            # it. /tmp is short, is where tmux already puts its socket dir
            # below, and is no closer to the user's real files.
            #
            # Canonicalized (`pwd -P`) because both are symlinks (/tmp ->
            # /private/tmp). Agent CLIs resolve their cwd to the real path, so a
            # folder-trust entry seeded under the symlinked path silently misses
            # and the agent boots into a "trust this folder?" dialog instead of
            # a usable UI.
            TBX_SANDBOX_ROOT="$(cd "$(mktemp -d /tmp/friring-sandbox.XXXXXX)" && pwd -P)"
            TBX_SANDBOX_FRESH=1
            # NOT under the root: AF_UNIX socket paths are ~104-byte limited,
            # and <root>/tmux/tmux-<uid>/friring-dev would overflow it under any
            # deeper prefix. Teardown removes this dir alongside the root.
            TBX_SANDBOX_TMUX_FRESH="$(mktemp -d /tmp/friring-sbx.XXXXXX)"
            TMUX_TMPDIR="$TBX_SANDBOX_TMUX_FRESH"
            ;;
        persistent)
            TBX_SANDBOX_ROOT="$TBX_REPO_ROOT/target/dev-sandbox/$profile"
            TBX_SANDBOX_FRESH=0
            mkdir -p "$TBX_SANDBOX_ROOT"
            # The deep target/ path overflows tmux's socket-path limit, so keep
            # the persistent socket dir short + stable per profile.
            TMUX_TMPDIR="$(tbx_sandbox_tmux_dir "$profile")"
            ;;
        *)
            echo "sandbox init: unknown mode '$mode' (want fresh|persistent)" >&2
            return 2
            ;;
    esac

    export TMUX_TMPDIR
    mkdir -p "$TMUX_TMPDIR"
    PATH="$TBX_REPO_ROOT/target/debug:$PATH"
    export PATH
}

# tbx_sandbox_init <fresh|persistent> [profile] — FRIRING-ONLY isolation.
# Real HOME/XDG (so authenticated agent CLIs work); only friring's config/data
# are redirected into the sandbox via the FRIRING_*_DIR overrides paths.rs honors.
tbx_sandbox_init() {
    _tbx_resolve_root "$@" || return $?
    FRIRING_CONFIG_DIR="$TBX_SANDBOX_ROOT/friring-config"
    FRIRING_DATA_DIR="$TBX_SANDBOX_ROOT/friring-data"
    export FRIRING_CONFIG_DIR FRIRING_DATA_DIR
    mkdir -p "$FRIRING_CONFIG_DIR" "$FRIRING_DATA_DIR"
}

# tbx_sandbox_init_full <fresh|persistent> [profile] — FULL isolation.
# Overrides HOME + XDG_* under the sandbox too (hermetic; agents boot fresh with
# no creds). For the demo recorder + smoke test. Uses the dev_build `friring-dev`
# XDG subdir (no FRIRING_*_DIR override needed).
tbx_sandbox_init_full() {
    _tbx_resolve_root "$@" || return $?
    # Hermetic: drop any inherited FRIRING_*_DIR overrides — paths.rs honors them
    # ahead of XDG, so an inherited one would silently defeat the isolation.
    unset FRIRING_CONFIG_DIR FRIRING_DATA_DIR
    HOME="$TBX_SANDBOX_ROOT/home"
    XDG_CONFIG_HOME="$TBX_SANDBOX_ROOT/config"
    XDG_DATA_HOME="$TBX_SANDBOX_ROOT/data"
    XDG_STATE_HOME="$TBX_SANDBOX_ROOT/state"
    XDG_CACHE_HOME="$TBX_SANDBOX_ROOT/cache"
    export HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME
    mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" \
        "$XDG_CACHE_HOME"
}

# tbx_sandbox_teardown — kill the sandbox's tmux server (safe: private
# TMUX_TMPDIR) and, for a `fresh` sandbox, remove the whole root. Persistent
# sandboxes are left intact (use tbx_sandbox_clean to wipe them).
tbx_sandbox_teardown() {
    tmux -L "$TBX_DEV_SOCKET" kill-server >/dev/null 2>&1 || true
    if [ "$TBX_SANDBOX_FRESH" = "1" ] && [ -n "$TBX_SANDBOX_ROOT" ]; then
        rm -rf "$TBX_SANDBOX_ROOT"
        # The fresh socket dir lives outside the root (path-length limit,
        # see _tbx_resolve_root).
        [ -n "${TBX_SANDBOX_TMUX_FRESH:-}" ] && rm -rf "$TBX_SANDBOX_TMUX_FRESH"
    fi
}

# tbx_sandbox_clean [profile] — kill a persistent profile's tmux server and
# remove its root + short tmux dir. Default profile "default".
tbx_sandbox_clean() {
    profile="${1:-default}"
    root="$TBX_REPO_ROOT/target/dev-sandbox/$profile"
    tdir="$(tbx_sandbox_tmux_dir "$profile")"
    if [ -d "$tdir" ]; then
        TMUX_TMPDIR="$tdir" tmux -L "$TBX_DEV_SOCKET" kill-server >/dev/null 2>&1 || true
    fi
    rm -rf "$root" "$tdir"
}
