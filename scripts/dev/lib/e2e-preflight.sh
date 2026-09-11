# shellcheck shell=bash
#
# The isolation preflight every bridge harness runs before it installs anything
# or starts a TUI — `scripts/dev/bridge-e2e.sh` and
# `scripts/dev/codex-park-e2e.sh` both source it.
#
# Shared rather than copied because it is a guard, and two copies of a guard
# drift: the one that matters here is that a binary's *resolved* database path
# is checked before any database is opened, which is the only moment a wrong
# answer is still only a mismatch. See the header of `bridge-e2e.sh` for the
# incident that produced it.
#
# The caller provides, before sourcing:
#
#   E2E_NAME    the harness's name, for messages
#   E2E_ROOT    this run's sandbox root; every resolved path must be inside it
#   ok / bad    the harness's own result reporters
#
# and afterwards has:
#
#   E2E_ENV     this run's path overrides, as words for `env`
#   pinned …    run a program with those overrides supplied, never inherited
#   fcli …      `friring-cli` from this working tree's build, pinned
#   e2e_preflight_paths   the check itself; non-zero means do not continue

# ---------------------------------------------------------------------------
# This run's overrides, as words for `env`. Read once into an array so every
# launch below uses the same list and none can drift from it. A read loop rather
# than `mapfile`, which macOS's bundled bash 3.2 does not have.
E2E_ENV=()
while IFS= read -r line; do E2E_ENV+=("$line"); done < <(tbx_sandbox_env_args)

# pinned <program> [args…] — run a friring binary with this run's path variables
# supplied directly, never merely inherited.
pinned() { env "${E2E_ENV[@]}" "$@"; }

# fcli [args…] — `friring-cli` from this working tree's build, pinned.
fcli() { pinned "$REPO_ROOT/target/debug/friring-cli" "$@"; }

# e2e_preflight_paths — the check itself.
#
# A function and not a body that runs when this file is sourced, because the
# decision to abort belongs to the harness: only it knows what it has already
# started and has to tear down. Non-zero means do not continue.
e2e_preflight_paths() {
    note "the binaries resolve the paths this run composed"

    # Answered before any database is opened, so a mismatch is caught while it
    # is still only a mismatch. `--json` keeps the shape stable; `jq -r` fails
    # loudly on a report this script cannot read.
    local paths_json="$E2E_ROOT/paths.json"
    if ! fcli --json config paths > "$paths_json" 2>"$E2E_ROOT/paths.err"; then
        printf '%s: config paths failed:\n%s\n' \
            "$E2E_NAME" "$(cat "$E2E_ROOT/paths.err")" >&2
        return 1
    fi
    printf 'resolved paths:\n%s\n' "$(jq . "$paths_json")"

    # Every reported path must be inside this run's own root, compared after
    # canonicalization: `$TMPDIR` and `/tmp` are symlinks on macOS, so a prefix
    # test on the raw strings would pass paths that are not actually below the
    # root and fail paths that are.
    #
    # Deliberately not `local`: `bridge-e2e`'s poisoned-server stage compares
    # against it after this function has returned.
    E2E_ROOT_REAL="$(cd "$E2E_ROOT" && pwd -P)"

    local preflight_bad=0
    local field value probe real suffix outer_real pair want got
    for field in config_dir data_dir database; do
        value="$(jq -r --arg f "$field" '.[$f] // ""' "$paths_json")"
        if [ -z "$value" ]; then
            printf '%s: config paths reported no %s\n' "$E2E_NAME" "$field" >&2
            preflight_bad=1
            continue
        fi
        # A `..` anywhere would let the suffix re-appended below climb back out
        # of the root the comparison just proved, so refuse it outright rather
        # than trying to resolve a component that may not exist yet.
        case "/$value/" in
            */../*)
                printf '%s: %s contains a ".." component: %s\n' \
                    "$E2E_NAME" "$field" "$value" >&2
                preflight_bad=1
                continue
                ;;
        esac
        # The path may not exist yet (the database certainly does not), so
        # canonicalize the deepest existing ancestor and re-append the rest.
        probe="$value"
        while [ ! -e "$probe" ] && [ "$probe" != "/" ] && [ -n "$probe" ]; do
            probe="$(dirname "$probe")"
        done
        real="$(cd "$probe" 2>/dev/null && pwd -P || printf '%s' "$probe")"
        suffix=${value#"$probe"}
        real="$real$suffix"
        case "$real" in
            "$E2E_ROOT_REAL" | "$E2E_ROOT_REAL"/*)
                ok "$field is inside this run's root ($value)"
                ;;
            *)
                printf '%s: %s resolved OUTSIDE this run: %s\n' \
                    "$E2E_NAME" "$field" "$value" >&2
                preflight_bad=1
                ;;
        esac
        # Under `sacrificial-env.sh` the outer ring exports its own overrides.
        # This run must have replaced them, not inherited them: sharing a root
        # would mean the outer canaries and this harness's own state live in one
        # tree, and neither could then say anything about the other.
        if [ -n "${SACRIFICIAL_ROOT:-}" ]; then
            outer_real="$(cd "$SACRIFICIAL_ROOT" 2>/dev/null && pwd -P \
                || printf '%s' "$SACRIFICIAL_ROOT")"
            case "$real" in
                "$outer_real" | "$outer_real"/*)
                    printf '%s: %s is the OUTER ring'"'"'s path, not this run'"'"'s: %s\n' \
                        "$E2E_NAME" "$field" "$value" >&2
                    preflight_bad=1
                    ;;
                *) ok "$field is distinct from the outer sacrificial root" ;;
            esac
        fi
    done

    # And it must have got there by the explicit override, not by a fallback
    # that happens to agree today. `HOME` here is the exact failure the header
    # describes.
    for pair in "config_source:FRIRING_CONFIG_DIR" "data_source:FRIRING_DATA_DIR"; do
        field="${pair%%:*}"
        want="${pair##*:}"
        got="$(jq -r --arg f "$field" '.[$f] // ""' "$paths_json")"
        if [ "$got" = "$want" ]; then
            ok "$field is $want"
        else
            printf '%s: %s is "%s", not the explicit %s\n' \
                "$E2E_NAME" "$field" "$got" "$want" >&2
            preflight_bad=1
        fi
    done

    if [ "$preflight_bad" -ne 0 ]; then
        cat >&2 <<ABORT
$E2E_NAME: aborting before installing anything or starting a TUI.
The binaries do not resolve the directories this script composed, so a run would
write somewhere this script cannot account for.
ABORT
        return 1
    fi
}
