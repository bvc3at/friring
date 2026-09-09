#!/usr/bin/env bash
#
# Park and resume a bridge child that is a **real interactive Codex CLI** —
# `just codex-park-e2e`.
#
# `bridge-e2e` proves the bridge with `/bin/sh` agents, which is what makes a
# green run a statement about friring rather than about an integration, and it
# proves the parking lifecycle that way too. What a shell script cannot say
# anything about is the thing parking exists for: an agent with a
# **conversation** stopping and coming back to it. A script has no thread to
# preserve, so "the same child resumed" is only ever a claim about friring's
# bookkeeping there.
#
# So this harness keeps friring's side identical — the leader is still a shell
# script driving the ordinary verbs — and makes the *child* the vendor binary.
# The claims it adds over `bridge-e2e` are the ones that need a real agent:
#
#  - a nudge is typed into a live **vendor** pane and produces a turn;
#  - the turn's private state survives a clean stop and is quoted back after the
#    resume by the relaunched process;
#  - and Codex's own rollout file — read from outside the boundary — is a single
#    thread that grew across the stop, rather than a second thread started fresh.
#
# # No credential, no billing, no network
#
# The child's `config.toml` names a `stub` provider on loopback with **no
# `env_key`**, which is the shape Codex accepts without a login and sends no
# authorization header for; the stub answers from a fixture file. The worker
# wrapper additionally points every proxy variable at a dead port with loopback
# excluded, so an update check or a telemetry post has nowhere to go while the
# stub stays reachable. Nothing here holds a key, and nothing reads the
# developer's own `~/.codex`: every root is redirected into this run's throwaway
# sandbox and the isolation preflight refuses to continue until the binaries
# agree.
#
# # What it does not prove
#
# `network_mode = "full"`, because the child must reach a loopback port that
# friring's egress proxy has no way to name. What the boundary allows is proven
# by `bridge-e2e` (the whole bridge with `network_mode = "none"`) and by
# `just seatbelt-probe` against a real kernel. This one is about the lifecycle.
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
export REPO_ROOT
E2E_NAME="codex-park-e2e"

case "$(uname -s)" in
    Darwin)
        [ -x /usr/bin/sandbox-exec ] || {
            echo "$E2E_NAME: /usr/bin/sandbox-exec is not present; skipping" >&2
            exit 0
        }
        ;;
    Linux)
        command -v bwrap >/dev/null || {
            echo "$E2E_NAME: bubblewrap is not installed; skipping" >&2
            exit 0
        }
        ;;
    *)
        echo "$E2E_NAME: the bridge is carried by seatbelt and bwrap only; skipping on $(uname -s)" >&2
        exit 0
        ;;
esac

# A skip, not a failure: this harness is about a vendor CLI, and a machine
# without it has nothing to say about one. `bridge-e2e` still covers the bridge.
command -v codex >/dev/null || {
    echo "$E2E_NAME: no codex on PATH; skipping" >&2
    exit 0
}
command -v node >/dev/null || {
    echo "$E2E_NAME: no node on PATH, so the model stub cannot run; skipping" >&2
    exit 0
}

# shellcheck source=scripts/dev/lib/sandbox-env.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/sandbox-env.sh"

cargo build --bin friring --bin friring-cli >/dev/null

tbx_sandbox_init_full fresh
# A shell running inside a friring session carries identity vars that would leak
# into the driver tmux server and misattribute this run's signals.
unset FRIRING_SESSION FRIRING_SESSION_ID FRIRING_TASK FRIRING_METRICS_DIR FRIRING_SOCKET

E2E_ROOT="$TBX_SANDBOX_ROOT"
E2E_WS="$E2E_ROOT/ws"
E2E_ARTIFACTS="$REPO_ROOT/target/codex-park-e2e"
DRIVER_SOCKET="friring-codex-park-$$"
DRIVER_SESSION="driver"
LEADER_NAME="codexpark"
FAILURES=0

# Cleared, not merely created: the stub **appends** to its journal, and an
# assertion that counts turns or looks for an `UNMATCHED` entry would otherwise
# be reading a previous run's evidence as well as this one's.
rm -rf "$E2E_ARTIFACTS"
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
# see the header of `bridge-e2e.sh` for the incident that produced it.

# shellcheck source=scripts/dev/lib/e2e-preflight.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/e2e-preflight.sh"

e2e_preflight_paths || exit 1

STUB_PID=""
# Every step is best-effort and the status is restored on the way out. Under
# `set -e` a failing `kill` — an already-exited stub is the ordinary way — would
# leave the trap before either tmux server was killed, so a stub that died would
# strand a TUI and its agent panes *and* replace the run's real exit status.
cleanup() {
    local status=$?
    if [ -n "$STUB_PID" ]; then
        kill "$STUB_PID" >/dev/null 2>&1 || true
    fi
    tmux -L "$DRIVER_SOCKET" kill-server >/dev/null 2>&1 || true
    tmux -L "$TBX_DEV_SOCKET" kill-server >/dev/null 2>&1 || true
    return "$status"
}
trap cleanup EXIT

pane() { tmux -L "$DRIVER_SOCKET" capture-pane -p -t "$DRIVER_SESSION" 2>/dev/null || true; }

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

git config --global user.name "friring-codex-park-e2e"
git config --global user.email "codex-park-e2e@friring.invalid"
git config --global init.defaultBranch main
(
    cd "$E2E_WS"
    git init -q
    printf 'seed\n' > README.md
    git add -A
    git commit -qm "codex-park-e2e seed"
)
ok "a git repository at $E2E_WS"

# ---------------------------------------------------------------------------
note "the model stub, on loopback"

# Three fixtures, none of them positional, because the number of turns is not
# knowable in advance: friring nudges on a rate limit rather than a count, both
# children are nudged, and any turn may be a repeat. Each is keyed on the shape
# of the turn instead.
#
# `title` is Codex's own side call for the thread name and is answered with text
# so it cannot pick up the turn script — ambient traffic goes first, per
# docs/E2E.md. `run-turn` answers a nudge by running the turn script; `turn-done`
# answers the script's output with text and ends the turn. Neither carries
# `maxUses`, so an extra nudge is harmless.
#
# The pair only works repeatedly because `hasToolResult` describes the turn being
# answered and not the whole thread (docs/E2E.md): codex resends the entire
# transcript, so under an "anywhere" reading the terminator would shadow the call
# from the second turn onwards and a resumed child would narrate instead of act.
cat > "$E2E_ROOT/fixtures.json" <<'JSON'
{
  "responses": [
    {
      "name": "title",
      "ambient": true,
      "match": { "promptContains": "single-line task title" },
      "reply": { "text": "codex-park turn" }
    },
    {
      "name": "turn-done",
      "match": { "hasToolResult": true },
      "reply": { "text": "CODEX-PARK-TURN-DONE" }
    },
    {
      "name": "run-turn",
      "match": { "promptContains": "you have new mail", "hasToolResult": false },
      "reply": {
        "text": "taking a codex-park turn",
        "toolUse": {
          "id": "call_codex_park",
          "name": "exec_command",
          "input": {
            "cmd": "sh \"$CODEX_PARK_TURN\" 2>&1",
            "shell": "/bin/sh",
            "login": false,
            "yield_time_ms": 20000
          }
        }
      }
    }
  ]
}
JSON

node "$REPO_ROOT/scripts/dev/agent-e2e/stub/openai-stub.mjs" \
    --port 0 --port-file "$E2E_ROOT/stub.port" \
    --journal "$E2E_ARTIFACTS/stub-journal.ndjson" \
    --raw-dir "$E2E_ARTIFACTS/stub-raw" \
    --fixtures "$E2E_ROOT/fixtures.json" > "$E2E_ARTIFACTS/stub.log" 2>&1 &
STUB_PID=$!
for _ in $(seq 1 100); do [ -s "$E2E_ROOT/stub.port" ] && break; sleep 0.1; done
STUB_PORT=$(cat "$E2E_ROOT/stub.port" 2>/dev/null || true)
if [ -n "$STUB_PORT" ]; then
    ok "the stub is listening on 127.0.0.1:$STUB_PORT"
else
    bad "the stub never reported a port"
    cat "$E2E_ARTIFACTS/stub.log"
    exit 1
fi

# ---------------------------------------------------------------------------
note "the family ~/.codex the child's private state is seeded from"

# `~/.codex` and not a differently-named directory: the worker agent declares
# `state_dir = "~/.codex"`, and a seed's `src` is resolved under exactly that.
# What lands here is what a child receives — a provider block and status hooks,
# and deliberately no credential.
mkdir -p "$HOME/.codex"
cat > "$HOME/.codex/config.toml" <<EOF
model = "gpt-6.x"
model_provider = "stub"
approval_policy = "never"
# The child is already inside friring's boundary. Codex's own sandbox would nest
# a second seatbelt inside the first, which is both redundant and a way to fail
# for reasons that have nothing to do with what this harness measures.
sandbox_mode = "danger-full-access"
check_for_update_on_startup = false

# No \`env_key\`: with one set Codex hard-errors, and without one it sends no
# authorization header at all. This is the shape that needs no login.
[model_providers.stub]
name = "Stub"
base_url = "http://127.0.0.1:$STUB_PORT/v1"
wire_api = "responses"
EOF
cp "$REPO_ROOT/extensions/hooks/codex-hooks.json" "$HOME/.codex/hooks.json"
ok "a family ~/.codex with a stub provider and friring's status hooks"

# ---------------------------------------------------------------------------
note "the extension, its agents and its profile"

CFG_DIR="$FRIRING_CONFIG_DIR"
mkdir -p "$CFG_DIR"
cat > "$CFG_DIR/agents.toml" <<'TOML'
default = "codex-park-leader"
TOML
cat > "$CFG_DIR/settings.toml" <<'TOML'
[features]
notifications = false
TOML

fcli extension install "$REPO_ROOT/extensions/codex-park" >/dev/null \
    || { bad "extension install refused"; exit 1; }
fcli extension activate codex-park >/dev/null \
    || { bad "extension activate refused"; exit 1; }
ok "codex-park installed and activated"

for agent in codex-park-leader codex-park-worker; do
    if grep -q "name = \"$agent\"" "$CFG_DIR/agents.toml"; then
        ok "$agent is registered in agents.toml"
    else
        bad "$agent never reached agents.toml"
    fi
done

# The directory the codex **binary** lives in, with symlinks resolved. A package
# manager puts a link on PATH and the real file elsewhere — on macOS
# `/usr/local/bin/codex` points into a Homebrew cask — and the profile has to
# grant where the file actually is, because that is what the wrapper execs.
CODEX_BIN=$(command -v codex)
while [ -L "$CODEX_BIN" ]; do
    link=$(readlink "$CODEX_BIN")
    case "$link" in
        /*) CODEX_BIN=$link ;;
        *) CODEX_BIN=$(dirname "$CODEX_BIN")/$link ;;
    esac
done
CODEX_DIR=$(cd "$(dirname "$CODEX_BIN")" && pwd -P)
ok "codex resolves to $CODEX_BIN"

# The shipped profile with its two placeholder paths replaced. Imported rather
# than written by hand, so what is stored went through the same validation a
# session's would.
EXT_HOME="$HOME/.config/friring/extensions/codex-park"
[ -f "$EXT_HOME/profiles.toml" ] || EXT_HOME="$REPO_ROOT/extensions/codex-park"
sed -e "s|{ path = \"~/dev/scratch\", mode = \"rw\" }|{ path = \"$E2E_WS\", mode = \"rw\" }|" \
    -e "s|{ path = \"/usr/local/bin\", mode = \"ro\" }|{ path = \"$CODEX_DIR\", mode = \"ro\" }|" \
    "$EXT_HOME/profiles.toml" > "$E2E_ROOT/profile.toml"
fcli sandbox import "$E2E_ROOT/profile.toml" --replace >/dev/null \
    || { bad "sandbox import refused the shipped profile"; exit 1; }
ok "the codex-park profile is stored"

# Handed to the agents through the environment rather than guessed from `$PWD`.
# `CODEX_PARK_BIN` is the resolved path for the same reason the profile grants
# the resolved directory: the wrapper must not depend on a symlink it may not be
# able to read through.
export CODEX_PARK_REPO="$E2E_WS"
export CODEX_PARK_BIN="$CODEX_BIN"
E2E_ENV+=("CODEX_PARK_REPO=$E2E_WS" "CODEX_PARK_BIN=$CODEX_BIN")

# ---------------------------------------------------------------------------
note "the leader, created from the TUI"

# `env` in front of the binary, not merely an exported environment: a pane's
# environment comes from the tmux *server*, so a server that already existed
# would hand the TUI whatever was current when it started.
tmux -L "$DRIVER_SOCKET" new-session -d -s "$DRIVER_SESSION" -x 200 -y 50 \
    env "${E2E_ENV[@]}" "$REPO_ROOT/target/debug/friring" 3>&-
need_pane "friring" 60 || exit 1
need_pane "No sessions yet" 30 || exit 1

key C-n
need_pane "New Session — Repo" 30 || exit 1
type_text "$E2E_WS"
key Enter

need_pane "New Session — Sandbox" 30 || exit 1
type_text "codex-park"
key Enter

need_pane "New Session — Name" 30 || exit 1
key C-u
type_text "$LEADER_NAME"
key Enter

if wait_pane "New Session — Agent" 10; then
    type_text "codex-park-leader"
    key Enter
fi

# An agent pane that dies keeps its output, so a launch that failed is a
# transcript rather than a closed window.
for _ in $(seq 1 50); do
    tmux -L "$TBX_DEV_SOCKET" set -g remain-on-exit on >/dev/null 2>&1 && break
    sleep 0.2
done

# ---------------------------------------------------------------------------
note "the run"

# The parked child's private state directory, once friring has minted it. The
# rollout inventory is snapshotted the moment the leader reports the stop, so
# "the resume extended the thread it already had" is measured rather than
# inferred from the end state alone.
CHILD_STATE_ROOT="$FRIRING_DATA_DIR/sandbox/tmp"
rollouts() {
    find "$CHILD_STATE_ROOT" -path '*/state/sessions/*' -name 'rollout-*.jsonl' \
        -exec sh -c 'for f; do printf "%s %s\n" "$(wc -c < "$f" | tr -d " ")" "$f"; done' _ {} + \
        2>/dev/null | sort -k2 || true
}

leader_dead() {
    tmux -L "$TBX_DEV_SOCKET" list-panes -a -F '#{window_name} #{pane_dead}' 2>/dev/null \
        | grep -qE "^tb-$LEADER_NAME 1$"
}

# The leader's **own** pane with its scrollback, not the TUI's rendering of it on
# the driver screen. Both the stop snapshot and the completion check read a line
# the leader printed once, and the driver screen scrolls: a poll that lands after
# the line has moved off would miss the snapshot window entirely and then wait
# out the whole budget for a run that had already finished.
leader_scroll() {
    tmux -L "$TBX_DEV_SOCKET" capture-pane -p -S -10000 -t "tb-$LEADER_NAME" 2>/dev/null || true
}

# Every child pane, kept current while the run is going. A stopped child's
# window is closed, so a capture taken after the run has nothing left to read —
# and the nudge, which is the thing that makes a vendor pane take a turn at all,
# is only ever visible there.
CHILD_PANES="$E2E_ARTIFACTS/child-panes"
rm -rf "$CHILD_PANES"
mkdir -p "$CHILD_PANES"
capture_children() {
    tmux -L "$TBX_DEV_SOCKET" list-panes -a -F '#{pane_id} #{window_name}' 2>/dev/null \
        | while read -r pid wname; do
            case "$wname" in
                "tb-$LEADER_NAME" | zsh | automation-heartbeat | "") continue ;;
            esac
            safe=$(printf '%s' "$wname" | tr -c '[:alnum:]._-' '_')
            tmux -L "$TBX_DEV_SOCKET" capture-pane -p -S -2000 -t "$pid" \
                > "$CHILD_PANES/$safe.txt" 2>/dev/null || true
        done
}

# Which processes friring has running, sampled each pass and tagged with which
# side of the stop it was taken on. The leader's claim that a stop was clean is
# a claim about the bridge's own state; whether the **vendor process** actually
# went away is a different question, and only a timeline taken from outside can
# answer it.
child_pids() {
    tmux -L "$TBX_DEV_SOCKET" list-panes -a -F "$1 #{pane_pid} #{window_name} #{pane_dead}" \
        2>/dev/null | grep -v " tb-$LEADER_NAME " | grep -v ' \(zsh\|automation-heartbeat\) '
}

run_ok=0
snapped=0
phase=before
: > "$E2E_ARTIFACTS/rollouts-at-stop.txt"
: > "$E2E_ARTIFACTS/pane-timeline.txt"
for _ in $(seq 1 1500); do
    capture_children
    child_pids "$phase" >> "$E2E_ARTIFACTS/pane-timeline.txt" || true
    screen=$(leader_scroll)
    # Snapshotted while the child is parked: what is on disk now is what the
    # stop left behind, and the comparison at the end is against this.
    if [ "$snapped" = 0 ] \
        && printf '%s' "$screen" | grep -qF "a clean stop released the fan-out slot"
    then
        rollouts > "$E2E_ARTIFACTS/rollouts-at-stop.txt"
        child_pids at-stop > "$E2E_ARTIFACTS/panes-at-stop.txt" || true
        snapped=1
        phase=after
    fi
    if printf '%s' "$screen" | grep -qF "codex-park: the interactive parking lifecycle held"; then
        run_ok=1
        break
    fi
    # One more look after the death, for the reason `bridge-e2e` records at the
    # same place: the leader's last line and its exit are one instant, and this
    # reads the TUI's pane, which repaints asynchronously.
    if leader_dead; then
        sleep 1
        leader_scroll | grep -qF "codex-park: the interactive parking lifecycle held" \
            && run_ok=1
        break
    fi
    sleep 1
done
rollouts > "$E2E_ARTIFACTS/rollouts-at-end.txt"
if [ "$run_ok" = 1 ]; then
    ok "the leader completed the interactive parking lifecycle"
else
    bad "the leader never completed the interactive parking lifecycle"
    printf -- '--- pane ---\n%s\n------------\n' "$(pane)"
fi
pane > "$E2E_ARTIFACTS/pane.txt"

# ---------------------------------------------------------------------------
note "what the leader observed from inside its own boundary"

tmux -L "$TBX_DEV_SOCKET" list-panes -a -F '#{pane_id} #{window_name} dead=#{pane_dead}' \
    > "$E2E_ARTIFACTS/panes.txt" 2>&1 || true
LEADER_PANE=$(awk -v w="tb-$LEADER_NAME" '$2 == w {print $1; exit}' "$E2E_ARTIFACTS/panes.txt")
if [ -n "$LEADER_PANE" ]; then
    # `-J` joins wrapped lines. Without it a value this script has to read back
    # — a child id is 36 characters — is split at the pane width wherever the
    # leader's own output happened to start, which is not a fixed column: a
    # nudge is typed into the same pane and lands mid-line.
    tmux -L "$TBX_DEV_SOCKET" capture-pane -p -J -S -10000 -t "$LEADER_PANE" \
        > "$E2E_ARTIFACTS/leader-pane.txt" 2>/dev/null || true
    ok "captured the leader's own pane"
else
    bad "could not find the leader's pane on friring's tmux server"
    printf -- '--- panes ---\n%s\n' "$(cat "$E2E_ARTIFACTS/panes.txt")"
    : > "$E2E_ARTIFACTS/leader-pane.txt"
fi

find "$FRIRING_DATA_DIR" -maxdepth 1 -name 'friring.log.*' -exec cat {} + \
    > "$E2E_ARTIFACTS/friring.log" 2>/dev/null || true
if [ "$FAILURES" -gt 0 ] && [ -s "$E2E_ARTIFACTS/friring.log" ]; then
    printf -- '--- friring log (errors) ---\n%s\n' \
        "$(grep -iE 'error|warn|refus' "$E2E_ARTIFACTS/friring.log" | tail -20)"
fi

# ---------------------------------------------------------------------------
note "the parking lifecycle, with a real agent as the child"

for claim in \
    "a clean stop released the fan-out slot" \
    "a resume at a full fan-out is refused fanout_exhausted" \
    "the refused resume left the child stopped" \
    "the same child resumed once a slot was free"
do
    if grep -qF "codex-park: ok — $claim" "$E2E_ARTIFACTS/leader-pane.txt"; then
        ok "parking: $claim"
    else
        bad "parking: never observed — $claim"
    fi
done

# Not anchored at the start of a line: friring types its nudge into this same
# pane, so the leader's next line of output begins wherever that left the cursor.
MARKER=$(sed -n 's/.*codex-park: marker before the stop is \(PARK-[0-9A-Za-z-]*\).*/\1/p' \
    "$E2E_ARTIFACTS/leader-pane.txt" | head -1)
if grep -qF "codex-park: ok — the resumed child claimed new mail and still holds $MARKER" \
    "$E2E_ARTIFACTS/leader-pane.txt" && [ -n "$MARKER" ]
then
    ok "parking: the relaunched Codex answered from its preserved private state ($MARKER)"
else
    bad "parking: the relaunched Codex never answered from its preserved private state"
fi

# ---------------------------------------------------------------------------
note "a nudge delivered into a live vendor pane"

# The child takes turns only because friring types into it: nothing in this
# harness prompts Codex. A marker report therefore already implies a delivered
# nudge, but the pane is where it is visible as the thing friring did.
NUDGE="friring: you have new mail."
if grep -rqF "$NUDGE" "$CHILD_PANES" 2>/dev/null; then
    ok "a nudge was typed into a live Codex pane"
else
    bad "no nudge reached a Codex pane"
fi
if grep -rqF "CODEX-PARK-TURN-DONE" "$CHILD_PANES" 2>/dev/null; then
    ok "the nudge produced a completed Codex turn"
else
    bad "no Codex turn ever completed"
fi

# ---------------------------------------------------------------------------
note "the stop ended a real process, not just a row"

# The leader's "clean stop" claim is about the bridge's own state. Whether the
# **vendor process** went away is a separate question, and one the run depends
# on: a Codex left running through the park would make the resume a no-op, and
# every marker and rollout check after it would still pass. `begin_resume` stops
# a leftover pane itself, so after the resume there is nothing left to observe.
#
# Read from the timeline rather than from a snapshot taken at the stop line: the
# leader creates the filler immediately afterwards, so no sampling rate makes a
# point-in-time "no child is alive" reading reliable. A process's whole lifetime
# is not a race — the parked child's pane is the **first** one this run ever
# sees, because it is the only child that exists before the stop.
PARKED_PANE=$(awk '$1 == "before" && $4 == 0 {print $2, $3; exit}' \
    "$E2E_ARTIFACTS/pane-timeline.txt" 2>/dev/null)
PRE_STOP_PID=${PARKED_PANE%% *}
if [ -z "$PRE_STOP_PID" ]; then
    bad "never saw a live child pane before the stop, so nothing was observed about the stop"
elif awk -v p="$PRE_STOP_PID" '$1 == "after" && $2 == p && $4 == 0 {found = 1}
     END {exit !found}' "$E2E_ARTIFACTS/pane-timeline.txt"
then
    bad "the pre-stop child process $PRE_STOP_PID was alive again after the stop"
    grep " $PRE_STOP_PID " "$E2E_ARTIFACTS/pane-timeline.txt" | tail -5
else
    ok "the process that took the pre-stop turn (${PARKED_PANE}) never ran again"
fi

# ---------------------------------------------------------------------------
note "the thread, read from Codex's own state"

# The claim friring's bookkeeping cannot make on its own, read from outside the
# boundary in the private state directory ADR-31 gives the child.
#
# Asserted on the **content** of the thread, not on the file layout, because the
# layout is the vendor's to change and was observed both ways against 0.153.4: a
# resumed thread may extend the rollout it already had, or land in a new file
# seeded with the replayed conversation (`thread_source: "user"`). What cannot
# happen either way is a *fresh* conversation carrying a turn that was taken
# before the stop — so that is what is checked.
# The parked child is named by the snapshot rather than by the pane: at the stop
# it is the only child that exists, so the one rollout on disk is its. Its
# scratch directory *is* its session key, which the host's own child list is
# then checked against — a stronger identification than reading an id back off a
# rendered terminal, and one that cannot be confused with the filler's.
PRE_STOP=$(awk 'NR == 1 {print $2}' "$E2E_ARTIFACTS/rollouts-at-stop.txt")
PARKED_DIR=${PRE_STOP%%/state/sessions/*}/state/sessions
CHILD_ID=$(basename "${PRE_STOP%%/state/sessions/*}")
printf 'parked child: %s\nmarker: %s\nat the stop:\n%s\nat the end:\n%s\n' \
    "${CHILD_ID:-<none>}" "${MARKER:-<none>}" \
    "$(cat "$E2E_ARTIFACTS/rollouts-at-stop.txt")" \
    "$(cat "$E2E_ARTIFACTS/rollouts-at-end.txt")"

if [ -n "$MARKER" ] && [ -n "$PRE_STOP" ] && [ -f "$PRE_STOP" ] \
    && grep -qF "codex-park: recorded $MARKER" "$PRE_STOP"
then
    ok "the parked child had a thread holding its pre-stop turn before it was stopped"
    ok "and the stop kept that thread on disk"
else
    bad "no pre-stop thread carrying ${MARKER:-<no marker>} at ${PRE_STOP:-<none>}"
fi

# And the relaunched process came back to **that** conversation.
#
# `resume` means the same child *and* the same thread: friring resolves the
# child's recorded `agent_session_id` and emits the agent's own resume group
# (`app::bridge_saga::child_resume_identity`), so Codex reopens the conversation
# rather than starting one. A resume that cannot reach it is refused, never
# launched blank, because a blank one looks identical from the outside.
#
# Checked on the **turn id**, and the marker cannot do it: the marker file
# survives the stop on purpose, so a child that came back to a blank thread
# reports the same marker on its first turn and a marker-only check passes on
# exactly the failure it exists to catch. A turn id names the process that
# printed it, and the pre-stop one belongs to a process this run has already
# proved never ran again.
#
# The file layout is the vendor's, not friring's: 0.153.4 was observed extending
# the rollout it had and writing a new one seeded with the replayed
# conversation. Both are the same thread, so the assertion is on what the
# resumed process's thread *contains*.
RESUMED=$(awk -v d="$PARKED_DIR/" 'index($2, d) == 1 {print $2}' \
    "$E2E_ARTIFACTS/rollouts-at-end.txt" | tail -1)
# Guarded: `grep PATTERN ""` reads standard input, which in a harness that runs
# unattended is a hang rather than a failure.
PRE_TURN=""
RESUMED_TURNS=0
[ -f "$PRE_STOP" ] \
    && PRE_TURN=$(grep -o 'codex-park: turn TURN-[0-9-]*' "$PRE_STOP" | head -1)
[ -f "$RESUMED" ] \
    && RESUMED_TURNS=$(grep -o 'codex-park: turn TURN-[0-9-]*' "$RESUMED" | sort -u | wc -l | tr -d ' ')
if [ -z "$PRE_TURN" ]; then
    bad "the pre-stop thread records no turn, so nothing can be said about the resume"
elif [ ! -f "$RESUMED" ]; then
    bad "the relaunched process wrote no thread at all"
elif ! grep -qF "$PRE_TURN" "$RESUMED"; then
    bad "the resumed thread does not carry the pre-stop turn: the resume started a new conversation"
    printf 'pre-stop turn: %s\nresumed thread: %s\n' "$PRE_TURN" "$RESUMED"
    grep -o 'codex-park: [a-zA-Z ]*\(TURN\|PARK\)-[0-9-]*' "$RESUMED" | sort -u || true
elif [ "${RESUMED_TURNS:-0}" -lt 2 ]; then
    bad "the resumed thread holds only $RESUMED_TURNS turn, so nothing was added after the resume"
elif grep -qF "codex-park: answered the resume check with $MARKER" "$RESUMED"; then
    ok "the relaunched Codex came back to the same thread: $(basename "$RESUMED")"
    ok "it carries the pre-stop process's turn ($PRE_TURN) and $RESUMED_TURNS turns in all"
    ok "and it answered new mail from that thread and from the state the stop kept"
else
    bad "the resumed thread never answered the resume check"
    grep -o 'codex-park: [a-zA-Z ]*\(TURN\|PARK\)-[0-9-]*' "$RESUMED" | sort -u || true
fi

# ---------------------------------------------------------------------------
note "what the stub was asked for"

# Fail-open at response time, fail-closed here: an unmatched request is answered
# so a stray probe cannot hang the run, and reported as a failure so drift in
# the CLI's wire shape is visible rather than silently absorbed.
if grep -q '"matched":"UNMATCHED"' "$E2E_ARTIFACTS/stub-journal.ndjson" 2>/dev/null; then
    bad "the stub saw a request no fixture matched"
    grep '"matched":"UNMATCHED"' "$E2E_ARTIFACTS/stub-journal.ndjson" | head -3
else
    ok "every model request matched a fixture"
fi
TURNS=$(grep -c '"matched":"run-turn"' "$E2E_ARTIFACTS/stub-journal.ndjson" 2>/dev/null || true)
if [ "${TURNS:-0}" -ge 2 ]; then
    ok "the child took $TURNS turns, at least one on each side of the stop"
else
    bad "the child took ${TURNS:-0} turns, so it cannot have acted both before and after the stop"
fi

# ---------------------------------------------------------------------------
note "what the host recorded"

LEADER_ID=$(fcli --json session list 2>/dev/null \
    | jq -r --arg n "$LEADER_NAME" '.[] | select(.name == $n) | .id' | head -1)
if [ -n "$LEADER_ID" ] && [ "$LEADER_ID" != "null" ]; then
    ok "the leader session exists ($LEADER_ID)"
else
    bad "no leader session row"
fi

LEADER=$(fcli --json session get "$LEADER_ID" 2>/dev/null || echo '{}')
printf '%s\n' "$LEADER" > "$E2E_ARTIFACTS/leader.json"

if [ "$(printf '%s' "$LEADER" | jq -r '.sandbox_profile // empty')" = "codex-park" ]; then
    ok "the leader runs under the codex-park profile"
else
    bad "the leader is not sandboxed into codex-park"
fi

CHILDREN=$(printf '%s' "$LEADER" | jq -r '.bridge.children // [] | length')
if [ "${CHILDREN:-0}" -ge 2 ]; then
    ok "the leader owns $CHILDREN children (the parked one and the filler)"
else
    bad "the leader owns ${CHILDREN:-0} children, so the fan-out was never filled"
fi

# The thread section above identified the parked child by the scratch directory
# its rollout is in. That directory name is a session key, so the host is what
# says whose it was — and saying so is what rules out having read the filler's.
if printf '%s' "$LEADER" | jq -e --arg id "$CHILD_ID" \
    '.bridge.children // [] | map(.id) | index($id)' >/dev/null 2>&1
then
    ok "the threads read above belong to a child this leader owns ($CHILD_ID)"
else
    bad "no child of this leader has the id $CHILD_ID that owns the threads read above"
    printf '%s' "$LEADER" | jq -r '.bridge.children // [] | .[] | "\(.id) \(.state)"' || true
fi

# ---------------------------------------------------------------------------
printf '\n%s: %d failed\n' "$E2E_NAME" "$FAILURES"
printf 'artifacts: %s\n' "$E2E_ARTIFACTS"
[ "$FAILURES" -eq 0 ]
