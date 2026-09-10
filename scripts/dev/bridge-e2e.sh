#!/usr/bin/env bash
#
# Run the `bridge-conformance` extension end to end against a real friring —
# `just bridge-e2e`.
#
# The extension is the vendor-free proof of the orchestration bridge: both its
# agents are `/bin/sh` scripts, so what a run observes is the **bridge** and not
# an integration with anything. Until this harness existed the extension was
# manifest-linted and never executed, which left the whole operator path — a
# leader created from the TUI, a profile with grants, a child spawned through
# the saga, a verdict friring reached by reading a real worktree — asserted only
# at unit level.
#
# What it stands up, all inside one throwaway sandbox root:
#
# - a fresh `XDG_*` tree, a private tmux socket and a private database
#   (`sandbox-env.sh`), so nothing here can see the operator's own state;
# - a git repository for the leader to work in;
# - the extension installed and activated, and its profile imported with the
#   repository path substituted in;
# - the **real TUI**, in a driver tmux, driven by keystrokes through the
#   new-session wizard — because a bridge-requiring agent is refused a headless
#   create (`bridge_refusal`), deliberately: a one-shot process cannot own the
#   egress proxy or answer the broker.
#
# What it asserts is the leader's own conformance report plus the host's view:
# a child row, a terminal state friring reached itself, and a `bridge_results`
# verdict. The leader script fails the run if any verb it is allowed is refused,
# and its worker fails the run if the depth rule or the private-state subtract
# set does not hold.
#
# # Isolation, proved rather than assumed
#
# A run of this harness once coincided with the operator's live development
# database being migrated a schema forward — a database at
# `~/.local/share/friring-dev/friring.db`, which is not this harness's and which
# nothing here names. The attribution was never proved: the main database file
# was still at the old schema, the new one existed only in its write-ahead log,
# and nothing separated this harness from anything else that ran that day. What
# the episode did establish is that the harness could not have told either. It
# redirected `HOME`, the `XDG_*` roots and friring's `FRIRING_*_DIR` overrides
# and then trusted that every process it started agreed — and a process's
# resolved database path was not observable until the open, which is already the
# write.
#
# It is observable now. `friring-cli config paths` reports the config dir, data
# dir and database file a process resolved *before* any database is opened, and
# names the variable that decided each. The preflight below runs it and aborts
# the harness unless every reported path is inside this run's own root and came
# from the explicit override. Everything after that is started with those
# overrides passed as arguments to `env`, so a tmux server that captured an older
# environment cannot substitute its own.
#
# For a second ring — this preflight is code, and code can be wrong — run the
# harness under `scripts/dev/sacrificial-env.sh`, whose outer `HOME` has canaries
# where a fallback would land. `just bridge-e2e` does.
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
export REPO_ROOT
E2E_NAME="bridge-e2e"

# shellcheck source=scripts/dev/lib/bridge-backend.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/bridge-backend.sh"
bridge_backend_or_skip "$E2E_NAME"

# shellcheck source=scripts/dev/lib/sandbox-env.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/sandbox-env.sh"

cargo build --bin friring --bin friring-cli >/dev/null

tbx_sandbox_init_full fresh
# A shell running inside a friring session carries identity vars that would
# leak into the driver tmux server and misattribute this run's signals.
unset FRIRING_SESSION FRIRING_SESSION_ID FRIRING_TASK FRIRING_METRICS_DIR FRIRING_SOCKET

E2E_ROOT="$TBX_SANDBOX_ROOT"
E2E_WS="$E2E_ROOT/ws"
E2E_ARTIFACTS="$REPO_ROOT/target/bridge-e2e"
DRIVER_SOCKET="friring-bridge-e2e-$$"
DRIVER_SESSION="driver"
FAILURES=0

mkdir -p "$E2E_WS" "$E2E_ARTIFACTS"

note() { printf '\n== %s ==\n' "$*"; }
ok() { printf '  ok      %s\n' "$*"; }
bad() {
    FAILURES=$((FAILURES + 1))
    printf '  FAILED  %s\n' "$*"
}

# ---------------------------------------------------------------------------
# The isolation preflight, shared with every other bridge harness. Nothing below
# may run until the binaries agree with the environment this script composed —
# see the header.

# shellcheck source=scripts/dev/lib/e2e-preflight.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/e2e-preflight.sh"

e2e_preflight_paths || exit 1

cleanup() {
    tmux -L "$DRIVER_SOCKET" kill-server >/dev/null 2>&1 || true
    tmux -L "$TBX_DEV_SOCKET" kill-server >/dev/null 2>&1 || true
}
trap cleanup EXIT

pane() { tmux -L "$DRIVER_SOCKET" capture-pane -p -t "$DRIVER_SESSION" 2>/dev/null || true; }

# wait_pane <extended-regex> <seconds> — poll the TUI's pane for a marker.
wait_pane() {
    local want=$1 secs=$2 i=0
    while [ "$i" -lt $((secs * 5)) ]; do
        pane | grep -qE "$want" && return 0
        i=$((i + 1))
        sleep 0.2
    done
    return 1
}

need_pane() {
    if wait_pane "$1" "$2"; then
        ok "the TUI showed '$1'"
    else
        bad "the TUI never showed '$1'"
        printf -- '--- pane ---\n%s\n------------\n' "$(pane)"
        return 1
    fi
}

key() { tmux -L "$DRIVER_SOCKET" send-keys -t "$DRIVER_SESSION" "$1"; sleep 0.3; }
type_text() { tmux -L "$DRIVER_SOCKET" send-keys -t "$DRIVER_SESSION" -l "$1"; sleep 0.3; }

# ---------------------------------------------------------------------------
note "the repository the leader works in"

git config --global user.name "friring-bridge-e2e"
git config --global user.email "bridge-e2e@friring.invalid"
git config --global init.defaultBranch main
(
    cd "$E2E_WS"
    git init -q
    printf 'seed\n' > README.md
    git add -A
    git commit -qm "bridge-e2e seed"
)
ok "a git repository at $E2E_WS"

# ---------------------------------------------------------------------------
note "the extension, its agents and its profile"

# The directory the preflight just watched a binary resolve, not a second
# spelling of it: a config file written anywhere else is a file nothing reads.
CFG_DIR="$FRIRING_CONFIG_DIR"
mkdir -p "$CFG_DIR"
# A default agent so the wizard's agent step has something to pick before the
# extension's own two are registered.
cat > "$CFG_DIR/agents.toml" <<'TOML'
default = "conformance-leader"
TOML
cat > "$CFG_DIR/settings.toml" <<'TOML'
[features]
notifications = false
TOML

# From this **working tree**, by path. A bare-name install resolves against the
# fork's published `extensions/` over the network, which is the wrong thing to
# test here twice over: it is not what this branch changed, and this harness is
# offline by construction.
fcli extension install "$REPO_ROOT/extensions/bridge-conformance" >/dev/null \
    || { bad "extension install refused"; exit 1; }
fcli extension activate bridge-conformance >/dev/null \
    || { bad "extension activate refused"; exit 1; }
ok "bridge-conformance installed and activated"

for agent in conformance-leader conformance-worker; do
    if grep -q "name = \"$agent\"" "$CFG_DIR/agents.toml"; then
        ok "$agent is registered in agents.toml"
    else
        bad "$agent never reached agents.toml"
    fi
done

# The shipped profile, with its one placeholder path replaced by this run's
# repository. Imported rather than written by hand, so what is stored went
# through the same validation a session's would.
EXT_HOME="$CFG_DIR/extensions/bridge-conformance"
[ -f "$EXT_HOME/profiles.toml" ] || EXT_HOME="$REPO_ROOT/extensions/bridge-conformance"
sed -e "s|{ path = \"~/dev/scratch\", mode = \"rw\" }|{ path = \"$E2E_WS\", mode = \"rw\" }|" \
    "$EXT_HOME/profiles.toml" > "$E2E_ROOT/profile.toml"
fcli sandbox import "$E2E_ROOT/profile.toml" --replace >/dev/null \
    || { bad "sandbox import refused the shipped profile"; exit 1; }
ok "the bridge-conformance profile is stored"

# The family's state directory, which this run's HOME is far too fresh to have.
#
# Both agents declare `state_rw = ["~/.friring-conformance/state"]`, and a policy
# backend grants a declared path **as it stands on the host**: friring creates
# nothing on an agent's behalf, so an agent's state has to exist before its first
# sandboxed launch. The two backends then diverge on the same gap — bubblewrap
# refuses to bind a source that is not there and the leader's pane dies on
# `Can't find source path` before it runs a line, while seatbelt writes a rule
# about a path nothing created and the run gets that far by luck.
#
# The file in it is what makes the child's claim an observation: `worker.sh`
# asserts it cannot reach the family's state, and against an absent directory
# that assertion passes for a narrowed child and for an un-narrowed one alike.
E2E_FAMILY_STATE="$HOME/.friring-conformance/state"
mkdir -p "$E2E_FAMILY_STATE"
chmod 700 "$HOME/.friring-conformance" "$E2E_FAMILY_STATE"
printf 'family-secret\n' > "$E2E_FAMILY_STATE/family-secret"
ok "the family's state directory holds a file only the family may read"

# The repository the leader creates its child's worktree in, handed to the
# leader script through its own environment rather than guessed from `$PWD`.
export CONFORMANCE_REPO="$E2E_WS"

# ---------------------------------------------------------------------------
note "a headless create of a bridge agent is refused"

# The refusal is a design property, not an accident, so it is asserted here
# rather than worked around: a one-shot process cannot own the egress proxy or
# answer the broker, so a bridge-required agent is only ever created from a
# running TUI.
if fcli --json session create --name headless-leader \
    --repo-path "$E2E_WS" --agent conformance-leader \
    --sandbox bridge-conformance >/dev/null 2>&1
then
    bad "a headless create of a bridge-requiring agent was allowed"
else
    ok "a headless create of a bridge-requiring agent is refused"
fi

# ---------------------------------------------------------------------------
note "the driver server is poisoned, and the pane's own environment wins"

# The driver server is created **first**, with path variables pointing at
# directories nothing may use — so it holds an environment as wrong as the one a
# server surviving an earlier run would hold. Every pane it opens inherits those
# unless the launch overrides them, which is exactly what this stage is here to
# make falsifiable: without `env` in front of the binary the harness would still
# go green on a fresh server, because a fresh server's environment happens to be
# this run's.
POISON="$E2E_ROOT/poison"
mkdir -p "$POISON/config/friring-dev" "$POISON/data/friring-dev"
# `env` in front of **tmux**, not in front of the command. A pane's environment
# comes from the server, and the server's comes from whatever created it — so
# poisoning the command would only poison that one pane and prove nothing about
# the ones opened later.
env "HOME=$POISON" "XDG_CONFIG_HOME=$POISON/config" "XDG_DATA_HOME=$POISON/data" \
    "FRIRING_CONFIG_DIR=$POISON/config/friring-dev" \
    "FRIRING_DATA_DIR=$POISON/data/friring-dev" \
    tmux -L "$DRIVER_SOCKET" new-session -d -s poison -x 200 -y 50 \
    sh -c 'while :; do sleep 3600; done' 3>&-

# pane_paths <name> <file> [env words…] — run `config paths` in a pane on the
# poisoned driver server, optionally behind an override, and print the data
# directory it resolved. A pane rather than a plain command: the environment a
# tmux server hands its children is the whole subject of this stage.
pane_paths() {
    local name=$1 out=$2 prefix=""
    shift 2
    # `printf '%q '` with no arguments still runs its conversion once and emits
    # an empty word, which `sh -c` then tries to execute.
    [ "$#" -gt 0 ] && prefix="$(printf '%q ' "$@")"
    : > "$out"
    tmux -L "$DRIVER_SOCKET" new-session -d -s "$name" \
        sh -c "$prefix'$REPO_ROOT/target/debug/friring-cli' --json config paths > '$out' 2>&1" 3>&-
    local i=0
    while [ "$i" -lt 50 ] && [ ! -s "$out" ]; do
        i=$((i + 1))
        sleep 0.2
    done
    jq -r '.data_dir // ""' < "$out" 2>/dev/null || true
}

# The poison took: a pane with no override of its own resolves into it.
case "$(pane_paths probe-inherit "$E2E_ROOT/poisoned.json")" in
    "$POISON"/*) ok "an un-overridden pane on this server resolves into the poison" ;;
    *)
        bad "the poisoned server did not take, so this stage proves nothing"
        printf -- '--- poisoned paths ---\n%s\n' "$(cat "$E2E_ROOT/poisoned.json")"
        ;;
esac

# And the override beats it, which is the property the TUI launch below relies
# on. Asserted directly rather than left to be inferred from a green run.
case "$(pane_paths probe-override "$E2E_ROOT/overridden.json" env "${E2E_ENV[@]}")" in
    "$E2E_ROOT_REAL"/* | "$E2E_ROOT"/*)
        ok "an env-prefixed pane on the same server resolves into this run"
        ;;
    *)
        bad "the explicit override did not beat the server's own environment"
        printf -- '--- overridden paths ---\n%s\n' "$(cat "$E2E_ROOT/overridden.json")"
        ;;
esac

# ---------------------------------------------------------------------------
note "the leader, created from the TUI"

# `env` in front of the binary, not merely an exported environment: a pane's
# environment comes from the tmux *server*, so a server that already existed —
# from an interrupted run, or from anything else on this socket — would hand the
# TUI whatever was current when it started. The server above holds exactly such
# an environment; these words override it.
tmux -L "$DRIVER_SOCKET" new-session -d -s "$DRIVER_SESSION" -x 200 -y 50 \
    env "${E2E_ENV[@]}" "$REPO_ROOT/target/debug/friring" 3>&-
need_pane "friring" 60 || exit 1
need_pane "No sessions yet" 30 || exit 1

# Fixture-only, and set before the first launch on purpose: a pane whose command
# dies immediately is *gone* by the time friring resizes it, and its stderr —
# which is the launch's own reason for exiting — goes with it. `remain-on-exit`
# keeps the dead pane and its last screen, so a launch that fails leaves the
# evidence rather than only the shape of its absence. friring never sets this
# itself for a whole server; this is the harness arranging to be able to see.
tmux -L "$TBX_DEV_SOCKET" set-option -g remain-on-exit on 2>/dev/null || true

key C-n
need_pane "New Session — Repo" 30 || exit 1
type_text "$E2E_WS"
key Enter

# The sandbox step opens whenever any profile is stored. Typed rather than
# arrowed to, so the pick does not depend on row order.
need_pane "New Session — Sandbox" 30 || exit 1
type_text "bridge-conformance"
key Enter

need_pane "New Session — Name" 30 || exit 1
key C-u
type_text "conformance"
key Enter

# One configured default plus the extension's two: the agent step may or may
# not appear depending on how many are registered, so accept either.
if wait_pane "New Session — Agent" 10; then
    type_text "conformance-leader"
    key Enter
fi

# An agent pane that dies keeps its output, so a launch that failed is a
# transcript rather than a closed window. Set as soon as friring's own server
# exists, which the session create above is what starts.
for _ in $(seq 1 50); do
    tmux -L "$TBX_DEV_SOCKET" set -g remain-on-exit on >/dev/null 2>&1 && break
    sleep 0.2
done

# ---------------------------------------------------------------------------
note "the run"

# The leader's own transcript is the assertion: it exercises every verb it is
# allowed and dies on the first refusal.
# Polled against two outcomes, not one. The leader dies on the first answer it
# does not expect, and a dead pane will never print the success line — waiting
# the whole budget out for it turns a run that already failed into a quarter of
# an hour of sleeping, and hides *when* it failed. `remain-on-exit` is on, so
# the pane and its transcript survive the process.
leader_dead() {
    tmux -L "$TBX_DEV_SOCKET" list-panes -a -F '#{window_name} #{pane_dead}' 2>/dev/null \
        | grep -qE '^tb-conformance 1$'
}
#
# A death is not the end of the evidence, so it is not the end of the poll. The
# leader's last line and its exit are the same instant, and what this loop reads
# is the **TUI's** pane, which repaints asynchronously: a capture taken between
# the two shows a run that has finished and a marker that has not arrived yet.
# Observed exactly once that way — every later assertion in this run passed, and
# the dump printed a completed transcript a moment after the loop gave up on it.
# So a dead pane buys one more look rather than a verdict.
#
# And a third outcome: the launch is *refused*, so no `tb-conformance` window is
# ever created and `leader_dead` has nothing to find. Left to the budget, that
# spends the whole wait and then reports every later assertion as a conformance
# failure — which reads as "the bridge is broken" and means "the leader never
# started". friring writes the refusal the moment the wizard is answered, so it
# is read rather than waited out, and reported as itself.
# shellcheck source=scripts/dev/lib/harness-log.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/harness-log.sh"
launch_refused() { harness_spawn_refusal "$FRIRING_DATA_DIR" "$E2E_NAME"; }

# Every **child's** own pane, kept as the run goes.
#
# friring removes a child's window when its spawn saga gives up, so by the time
# anything below runs there is nothing left to capture — and a child that
# started and then never reported says why in its own pane and nowhere else.
# friring's log has only the host's half of that: "this child's own hook did not
# report within 60s" is the symptom, never the cause.
#
# Snapshotted each poll, and a snapshot **replaces** the last one only when it
# has something in it. A pane that has already died still answers `capture-pane`,
# so overwriting unconditionally lets the run's last poll replace what the child
# printed with the empty screen it left behind.
#
# The metadata line is kept beside the transcript, because the two answer
# different questions: an empty transcript with `status=1` says the process
# never wrote to its terminal, which is a different failure from one that
# printed an error, and `#{pane_start_command}` is the only place the composed
# launch appears at all. **Appended**, not overwritten — the last poll runs
# after the window is gone, so a list written then has no line for the pane the
# run is about. Deduped where it is printed.
capture_children() {
    tmux -L "$TBX_DEV_SOCKET" list-panes -a \
        -F '#{pane_id} #{window_name} dead=#{pane_dead} status=#{pane_dead_status} cmd=#{pane_start_command}' \
        2>/dev/null >> "$E2E_ARTIFACTS/children.txt" || true
    tmux -L "$TBX_DEV_SOCKET" list-panes -a -F '#{pane_id} #{window_name}' 2>/dev/null \
        | while read -r child_pane child_window; do
        case "$child_window" in
            # The leader has a capture of its own, below.
            tb-conformance) continue ;;
            tb-*) ;;
            *) continue ;;
        esac
        child_file="$E2E_ARTIFACTS/child-$child_window.txt"
        tmux -L "$TBX_DEV_SOCKET" capture-pane -p -J -S - -t "$child_pane" \
            > "$child_file.new" 2>/dev/null || true
        if [ -n "$(tr -d '[:space:]' < "$child_file.new" 2>/dev/null)" ]; then
            mv "$child_file.new" "$child_file"
        else
            [ -e "$child_file" ] || mv "$child_file.new" "$child_file"
            rm -f "$child_file.new"
        fi
    done
}

# Replay a dead child's own mount composition, with nothing run inside it.
#
# A child that exits **without writing to its terminal** leaves a status and no
# reason, and friring's log has only the host's half of that. The pane's start
# command is the exact boundary friring composed, so everything up to the first
# `--` — the bubblewrap options, with `/bin/true` in place of the agent — asks
# the kernel the same question in a place where the answer is not lost. Nothing
# of the agent runs: the separator is what divides the two, and the replacement
# is the smallest program there is.
#
# A replay that *succeeds* is as useful as one that fails. It says the mounts
# compose, which moves the question to the launch helper and off the boundary.
replay_child_boundary() {
    local cmd boundary replay_out replay_status=0
    cmd=$(awk '$3 == "dead=1" && $4 != "status=0" {
            sub(/^([^ ]+ ){4}cmd=/, "")
            print
            exit
        }' "$E2E_ARTIFACTS/children.txt" 2>/dev/null) || return 0
    [ -n "$cmd" ] || return 0
    boundary=${cmd%% -- *}
    [ "$boundary" != "$cmd" ] || return 0
    printf -- '--- replaying that boundary with nothing inside it ---\n'
    if command -v timeout >/dev/null 2>&1; then
        replay_out=$(timeout 20 sh -c "$boundary -- /bin/true" 2>&1) || replay_status=$?
    else
        replay_out=$(sh -c "$boundary -- /bin/true" 2>&1) || replay_status=$?
    fi
    printf 'replay exit: %s\n' "$replay_status"
    printf '%s\n' "${replay_out:-(the boundary composed and said nothing)}" | head -20
}

# Print what those captures hold. Bounded, because a child that loops prints a
# great deal and the useful part is where it stopped.
dump_children() {
    if [ -s "$E2E_ARTIFACTS/children.txt" ]; then
        printf -- '--- every pane this run had, first seen first ---\n'
        awk '!seen[$0]++' "$E2E_ARTIFACTS/children.txt"
    fi
    for child in "$E2E_ARTIFACTS"/child-*.txt; do
        [ -e "$child" ] || continue
        printf -- '--- %s ---\n' "$(basename "$child")"
        tail -40 "$child"
    done
    replay_child_boundary
}
run_ok=0
refused=""
# Started fresh: the artifacts directory survives between runs, and an appended
# list would otherwise open with the panes of a run that is not this one.
: > "$E2E_ARTIFACTS/children.txt"
rm -f "$E2E_ARTIFACTS"/child-*.txt
for _ in $(seq 1 900); do
    capture_children
    if pane | grep -qE "conformance: the bridge answered every verb"; then
        run_ok=1
        break
    fi
    refused=$(launch_refused)
    if [ -n "$refused" ]; then
        break
    fi
    if leader_dead; then
        sleep 1
        capture_children
        pane | grep -qE "conformance: the bridge answered every verb" && run_ok=1
        break
    fi
    sleep 1
done
if [ -n "$refused" ] && [ "$run_ok" != 1 ]; then
    bad "the leader was never launched, so nothing below was exercised: $refused"
    printf -- '--- pane ---\n%s\n------------\n' "$(pane)"
    # What the launch itself said before it exited. `remain-on-exit` above is
    # what keeps this readable: the message friring reports is the *host* side
    # of the failure, and the pane holds the other side.
    printf -- '--- friring panes ---\n'
    tmux -L "$TBX_DEV_SOCKET" list-panes -a \
        -F '#{pane_id} #{window_name} dead=#{pane_dead} status=#{pane_dead_status}' \
        2>&1 || true
    for dead in $(tmux -L "$TBX_DEV_SOCKET" list-panes -a -F '#{pane_id}' 2>/dev/null); do
        printf -- '--- %s ---\n' "$dead"
        tmux -L "$TBX_DEV_SOCKET" capture-pane -p -S -200 -t "$dead" 2>&1 || true
    done
    dump_children
    mkdir -p "$E2E_ARTIFACTS"
    pane > "$E2E_ARTIFACTS/pane.txt" || true
    find "$FRIRING_DATA_DIR" -maxdepth 1 -name 'friring.log.*' -exec cat {} + \
        > "$E2E_ARTIFACTS/friring.log" 2>/dev/null || true
    printf '\n%s: %d failed\n' "$E2E_NAME" "$FAILURES"
    printf 'artifacts: %s\n' "$E2E_ARTIFACTS"
    exit 1
fi
if [ "$run_ok" = 1 ]; then
    ok "the leader completed its conformance run"
else
    bad "the leader never completed its conformance run"
    printf -- '--- pane ---\n%s\n------------\n' "$(pane)"
    dump_children
fi
pane > "$E2E_ARTIFACTS/pane.txt"

# ---------------------------------------------------------------------------
note "what the leader observed from inside its own boundary"

# The leader's pane is friring's, not the driver's: friring runs its sessions on
# its own tmux socket, and this is where a nudge is typed and where the agent's
# own transcript is.
tmux -L "$TBX_DEV_SOCKET" list-panes -a -F '#{pane_id} #{window_name} dead=#{pane_dead}' \
    > "$E2E_ARTIFACTS/panes.txt" 2>&1 || true
LEADER_PANE=$(awk '/conformance/ {print $1; exit}' "$E2E_ARTIFACTS/panes.txt")
if [ -n "$LEADER_PANE" ]; then
    tmux -L "$TBX_DEV_SOCKET" capture-pane -p -S -10000 -t "$LEADER_PANE" \
        > "$E2E_ARTIFACTS/leader-pane.txt" 2>/dev/null || true
    ok "captured the leader's own pane"
else
    bad "could not find the leader's pane on friring's tmux server"
    printf -- '--- panes ---\n%s\n' "$(cat "$E2E_ARTIFACTS/panes.txt")"
    : > "$E2E_ARTIFACTS/leader-pane.txt"
fi
# friring's own log, always: a launch that refused says so here and nowhere the
# pane can show.
find "$FRIRING_DATA_DIR" -maxdepth 1 -name 'friring.log.*' -exec cat {} + \
    > "$E2E_ARTIFACTS/friring.log" 2>/dev/null || true
if [ "$FAILURES" -gt 0 ] && [ -s "$E2E_ARTIFACTS/friring.log" ]; then
    printf -- '--- friring log (errors) ---\n%s\n' \
        "$(grep -iE 'error|warn|refus' "$E2E_ARTIFACTS/friring.log" | tail -20)"
fi

# ---------------------------------------------------------------------------
note "the parking lifecycle, through the real broker"

# A clean `stop` releases a child's runtime and its fan-out slot while keeping
# the child; a later `resume` brings that same child back with its branch, its
# worktree and its private agent state. The in-process tests assert each step
# against a fake backend; only here does a child actually stop being a process
# and come back as one, so only here can "what survived" mean anything.
#
# The leader drives the whole cycle and dies on the first answer it does not
# expect, so these are the claims it got far enough to print.
for claim in \
    "a clean stop released the fan-out slot" \
    "a resume at a full fan-out is refused fanout_exhausted" \
    "the refused resume left the child stopped" \
    "the same child resumed once a slot was free"
do
    if grep -qF "conformance: park ok — $claim" "$E2E_ARTIFACTS/leader-pane.txt"; then
        ok "parking: $claim"
    else
        bad "parking: never observed — $claim"
    fi
done

# The one that is about state rather than about accounting: the marker the
# child wrote into its private state directory before the stop, quoted back by
# the relaunched process after it claimed new mail.
if grep -q "conformance: park ok — the resumed child claimed new mail and still holds" \
    "$E2E_ARTIFACTS/leader-pane.txt"
then
    ok "parking: the resumed child kept its private agent state and its mailbox"
    grep -o "still holds PARK-[0-9-]*" "$E2E_ARTIFACTS/leader-pane.txt" | tail -1
else
    bad "parking: the resumed child never answered from its preserved private state"
fi

# The in-boundary assertions the leader script makes. A hole is reported by the
# leader itself; this turns it into a failure here.
if grep -q "BOUNDARY HOLE" "$E2E_ARTIFACTS/leader-pane.txt"; then
    bad "the leader found a hole in its own boundary"
    grep "BOUNDARY HOLE" "$E2E_ARTIFACTS/leader-pane.txt"
else
    ok "the leader found no hole in its own boundary"
fi
for claim in "gate root is unreadable" "gate root is unwritable" "database is unreadable"; do
    if grep -q "boundary ok — friring's $claim" "$E2E_ARTIFACTS/leader-pane.txt"; then
        ok "observed from inside a real launch: friring's $claim"
    else
        bad "never observed from inside a real launch: friring's $claim"
    fi
done
# The agent-declared grant, from the owner's side. Without these two the child's
# refusal below could equally be a family state directory nobody can reach.
for claim in "state file is readable by its owner" "state directory is writable"; do
    if grep -q "boundary ok — the family's $claim" "$E2E_ARTIFACTS/leader-pane.txt"; then
        ok "observed from inside a real launch: the family's $claim"
    else
        bad "never observed from inside a real launch: the family's $claim"
    fi
done
# And the child's own side of it. Only the *child's* pane holds its assertions
# and nothing captures that, so the worker puts the outcome on the report
# channel and the leader prints it with the rest of the child's status. The
# exact wording matters: the worker reports a different one where it skipped the
# assertion, so a skip cannot read as a refusal here.
if grep -q "family state refused" "$E2E_ARTIFACTS/leader-pane.txt"; then
    ok "the child reported that its family's state was refused to it"
else
    bad "the child never reported a refusal of its family's state"
fi

# The launch's **own** gate, read-only, observed the only way it can be: the
# launch helper runs inside this boundary and cannot exec the agent until it has
# read the release file out of the gate directory. A leader that is running is a
# gate that was readable from inside; the deny of the gate *root* above is what
# says it was read-only and only this launch's.
if grep -q "^== status ==" "$E2E_ARTIFACTS/leader-pane.txt"; then
    ok "the launch helper read this launch's own gate from inside the boundary"
else
    bad "the leader never started, so nothing was observed about its gate"
fi

# ---------------------------------------------------------------------------
note "a nudge delivered into a live pane"

# The give-up rule is unit-tested against the counter; **delivery** needs a real
# multiplexer, which is what this harness has. friring mails the leader on every
# child transition, so a run that reached a verdict owes it at least one nudge.
NUDGE="friring: you have new mail."
if grep -qF "$NUDGE" "$E2E_ARTIFACTS/leader-pane.txt"; then
    ok "a nudge was typed into the leader's live pane"
else
    bad "no nudge reached the leader's pane"
fi

# ---------------------------------------------------------------------------
note "what the host recorded"

LEADER_ID=$(fcli --json session list 2>/dev/null \
    | jq -r '.[] | select(.name == "conformance") | .id' | head -1)
if [ -n "$LEADER_ID" ] && [ "$LEADER_ID" != "null" ]; then
    ok "the leader session exists ($LEADER_ID)"
else
    bad "no leader session row"
fi

LEADER=$(fcli --json session get "$LEADER_ID" 2>/dev/null || echo '{}')
printf '%s\n' "$LEADER" > "$E2E_ARTIFACTS/leader.json"

# The boundary really applied: a fallback to the host would have produced a
# working session with no bridge at all.
if [ "$(printf '%s' "$LEADER" | jq -r '.sandbox_profile // empty')" = "bridge-conformance" ]; then
    ok "the leader runs under the bridge-conformance profile"
else
    bad "the leader is not sandboxed into bridge-conformance"
fi

CHILDREN=$(printf '%s' "$LEADER" | jq -r '.bridge.children // [] | length')
if [ "${CHILDREN:-0}" -ge 1 ]; then
    ok "the leader owns $CHILDREN child(ren)"
else
    bad "the leader owns no children"
fi

# The host's own verdict, not the child's claim: `state` is what friring
# reached after it stopped the pane and read the worktree.
CHILD_STATE=$(printf '%s' "$LEADER" | jq -r '.bridge.children[0].state // empty')
case "$CHILD_STATE" in
    done|failed|stopped|dirty)
        ok "the child reached a host-decided state: $CHILD_STATE"
        ;;
    "")
        bad "no child state was recorded"
        ;;
    *)
        bad "the child is still $CHILD_STATE — the quiesce did not finish"
        ;;
esac

if [ "$(printf '%s' "$LEADER" | jq -r '.bridge.children[0].result.verified_at // empty')" != "" ]; then
    ok "friring recorded a verdict it verified itself"
else
    bad "no host verdict for the child"
fi

# ---------------------------------------------------------------------------
printf '\n%s: %d failed\n' "$E2E_NAME" "$FAILURES"
printf 'artifacts: %s\n' "$E2E_ARTIFACTS"
[ "$FAILURES" -eq 0 ]
