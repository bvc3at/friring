#!/usr/bin/env bash
#
# Run the `omx` extension's Team fan-out end to end, with the **real vendor
# package** as the leader and real interactive Codex workers — `just
# omx-team-e2e`.
#
# This is the scenario `docs/E2E.md` recorded as not built. It was recorded as
# blocked on an authenticated Codex endpoint, and that was wrong: a custom
# `[model_providers.*]` with no `env_key` needs no login, so `omx` and every
# worker it spawns can run against the same local stub the rest of the e2e
# family uses. What actually stood in the way was three pieces of **vendor
# first-run state**, each of which renders a string in `MODAL_MARKERS` and so
# makes friring correctly refuse to type into the pane. All three are seeded
# below, and each seed says what it is standing in for.
#
# What the run proves, none of which `bridge-conformance` or `codex-park` can:
#
#  - `oh-my-codex@0.21.0` from the registry, `omx setup --scope user
#    --install-mode legacy`, and then friring's own `extension install` with all
#    26 requirement gates satisfied at once by what that installer produced;
#  - the **vendor binary** as a sandboxed friring leader: `omx --direct` inside
#    the boundary, launching Codex against the stub, reaching a live composer;
#  - `friring-omx run` driving a real fan-out through the real broker — one
#    bridge child per DAG node, each a real Codex in its own worktree, each
#    reaching a state **friring** decided by stopping the pane and reading the
#    worktree;
#  - and `friring-omx integrate` merging only what friring verified.
#
# # No credential, no billing
#
# The seeded `~/.codex/config.toml` names a `stub` provider on loopback with no
# `env_key`. `auth.json` is a synthetic placeholder, present only because the
# worker's `link-rw` seed is `required = true`; nothing reads it, because the
# stub provider sends no authorization header. Nothing here touches the
# developer's own `~/.codex`, `~/.omx` or npm prefix: every root is redirected
# into this run's throwaway sandbox and the shared preflight refuses to continue
# otherwise.
#
# # What it does not prove
#
# `network_mode = "full"`, for the reason `codex-park` has it: the child must
# reach a dynamically-numbered loopback port that friring's egress proxy has no
# way to name. Egress is proven by `bridge-e2e` (the whole bridge with
# `network_mode = "none"`) and by `just seatbelt-probe`.
set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
export REPO_ROOT
E2E_NAME="omx-team-e2e"
OMX_VERSION="0.21.0"
PLAN_SLUG="demo"

# shellcheck source=scripts/dev/lib/bridge-backend.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/bridge-backend.sh"
bridge_backend_or_skip "$E2E_NAME"

for tool in codex node npm git jq; do
    command -v "$tool" >/dev/null || {
        bridge_require_or_skip "$E2E_NAME" "no $tool on PATH"
    }
done

# shellcheck source=scripts/dev/lib/sandbox-env.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/sandbox-env.sh"

cargo build --bin friring --bin friring-cli >/dev/null

tbx_sandbox_init_full fresh
unset FRIRING_SESSION FRIRING_SESSION_ID FRIRING_TASK FRIRING_METRICS_DIR FRIRING_SOCKET

E2E_ROOT="$TBX_SANDBOX_ROOT"
E2E_WS="$E2E_ROOT/ws"
E2E_ARTIFACTS="$REPO_ROOT/target/omx-team-e2e"
DRIVER_SOCKET="friring-omx-team-$$"
DRIVER_SESSION="driver"
LEADER_NAME="omxteam"
FAILURES=0

# Cleared, not merely created: the stub appends to its journal, so a count or an
# `UNMATCHED` check would otherwise read a previous run's evidence too.
rm -rf "$E2E_ARTIFACTS"
mkdir -p "$E2E_WS" "$E2E_ARTIFACTS"

note() { printf '\n== %s ==\n' "$*"; }
ok() { printf '  ok      %s\n' "$*"; }
bad() {
    FAILURES=$((FAILURES + 1))
    printf '  FAILED  %s\n' "$*"
}

# shellcheck source=scripts/dev/lib/e2e-preflight.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/e2e-preflight.sh"

e2e_preflight_paths || exit 1

STUB_PID=""
TRUST_SOCKET="friring-omx-trust-$$"
cleanup() {
    local status=$?
    if [ -n "$STUB_PID" ]; then
        kill "$STUB_PID" >/dev/null 2>&1 || true
    fi
    tmux -L "$TRUST_SOCKET" kill-server >/dev/null 2>&1 || true
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
note "the vendor package, in this run's own prefix"

OMX_PREFIX="$E2E_ROOT/npm"
mkdir -p "$OMX_PREFIX" "$E2E_ROOT/npm-cache"
# The one step that reaches the network, and the one that makes this harness
# skip rather than fail when it cannot: a machine with no registry access has
# nothing to say about the vendor package.
if ! npm_config_cache="$E2E_ROOT/npm-cache" npm install --prefix "$OMX_PREFIX" \
    "oh-my-codex@$OMX_VERSION" > "$E2E_ARTIFACTS/npm-install.log" 2>&1
then
    bridge_require_or_skip "$E2E_NAME" "could not fetch oh-my-codex@$OMX_VERSION" \
        "$(tail -5 "$E2E_ARTIFACTS/npm-install.log")"
fi
OMX_BIN_DIR="$OMX_PREFIX/node_modules/.bin"
PATH="$OMX_BIN_DIR:$PATH"
export PATH
# The pinned override list carries `PATH`, and it was read before this prefix
# existed. Every `fcli` below runs through `env` with that list, so without this
# the extension's own `omx --version` gate would look for a binary on the
# operator's `PATH` — and fail, which is the correct answer to the wrong
# question.
E2E_ENV=()
while IFS= read -r line; do E2E_ENV+=("$line"); done < <(tbx_sandbox_env_args)

# App-level offline for everything friring launches from here on. The profile is
# `network_mode = "full"` — the child has to reach a loopback port the egress
# proxy cannot name — so the boundary is not what keeps these agents off the
# network. A dead proxy for everything except loopback is: the stub stays
# reachable and an update check, a telemetry post or Codex's own `codex_apps`
# MCP server has nowhere to go. Added **after** the npm install above, which is
# the one step that legitimately reaches the registry.
E2E_ENV+=(
    "http_proxy=http://127.0.0.1:9"
    "https_proxy=http://127.0.0.1:9"
    "HTTP_PROXY=http://127.0.0.1:9"
    "HTTPS_PROXY=http://127.0.0.1:9"
    "no_proxy=127.0.0.1,localhost"
    "NO_PROXY=127.0.0.1,localhost"
)
ok "omx is $(omx --version 2>&1 | head -1)"

# ---------------------------------------------------------------------------
note "the model stub, on loopback"

# Four fixtures, keyed on the shape of the turn rather than its position,
# because friring nudges on a rate limit and any turn may be a repeat.
#
# `title` is Codex's own side call for a thread name; `turn-done` ends a turn by
# answering the script's output with text; `team` is the leader's one job, and
# `worker` is a worker's. Both run a script committed in the fixture repository
# rather than a shell one-liner quoted into JSON — what they do is the substance
# of the run and belongs where it can be read.
#
# `worker` is keyed on the **instructions**, not on the nudge: friring mails the
# leader on every child transition, so leader and worker are nudged with the
# identical literal and a prompt-only match had the leader running the worker's
# script in its own worktree. What differs is that a worker is launched with
# `-c model_instructions_file=<home>/worker/AGENTS.md`. `leader-mail` then
# answers the leader's own nudges with text, so they are no-op turns rather than
# unmatched requests.
cat > "$E2E_ROOT/fixtures.json" <<'JSON'
{
  "responses": [
    {
      "name": "title",
      "ambient": true,
      "match": { "promptContains": "single-line task title" },
      "reply": { "text": "omx team run" }
    },
    {
      "name": "turn-done",
      "match": { "hasToolResult": true },
      "reply": { "text": "OMX-TEAM-TURN-DONE" }
    },
    {
      "name": "team",
      "match": { "promptContains": "friring-team", "hasToolResult": false },
      "reply": {
        "text": "planning and running the team",
        "toolUse": {
          "id": "call_omx_team",
          "name": "exec_command",
          "input": {
            "cmd": "sh .omx/team-run.sh 2>&1 | tail -40",
            "shell": "/bin/sh",
            "login": false,
            "yield_time_ms": 30000
          }
        }
      }
    },
    {
      "name": "worker",
      "match": {
        "systemContains": "You are a friring bridge child",
        "promptContains": "you have new mail",
        "hasToolResult": false
      },
      "reply": {
        "text": "doing the node's work",
        "toolUse": {
          "id": "call_omx_worker",
          "name": "exec_command",
          "input": {
            "cmd": "sh .omx/worker-turn.sh 2>&1",
            "shell": "/bin/sh",
            "login": false,
            "yield_time_ms": 20000
          }
        }
      }
    },
    {
      "name": "leader-mail",
      "match": { "promptContains": "you have new mail" },
      "reply": { "text": "OMX-TEAM-LEADER-MAIL: noted" }
    }
  ]
}
JSON

node "$REPO_ROOT/scripts/dev/agent-e2e/stub/openai-stub.mjs" \
    --port 0 --port-file "$E2E_ROOT/stub.port" \
    --journal "$E2E_ARTIFACTS/stub-journal.ndjson" \
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
note "omx setup, and the three first-run gates it leaves behind"

(
    cd "$E2E_WS"
    CODEX_HOME="$HOME/.codex" OMX_AUTO_UPDATE=0 \
        omx setup --scope user --install-mode legacy
) > "$E2E_ARTIFACTS/omx-setup.log" 2>&1 || {
    bad "omx setup failed"
    tail -20 "$E2E_ARTIFACTS/omx-setup.log"
    exit 1
}
ok "omx setup installed skills, prompts and hooks into this run's ~/.codex"

# (1) The one-time GitHub star prompt. `[Y/n]` is a `MODAL_MARKERS` entry, so a
# leader showing it never receives a nudge — and answering yes makes OMX run
# `gh api -X PUT /user/starred/…`, a write to GitHub as whoever owns the `gh`
# credential, from inside the boundary. The state file records that the question
# was put; with it present OMX never asks and never calls `gh`, which is the
# declined outcome and the only one a harness may take.
mkdir -p "$HOME/.omx/state"
cat > "$HOME/.omx/state/star-prompt.json" <<'JSON'
{ "prompted_at": "2000-01-01T00:00:00.000Z" }
JSON
ok "the star prompt is answered in advance, and declined: no gh call is possible"

# (3) A stale session pointer makes OMX exit 1 with
# `session_pointer_unusable` rather than starting. A fresh root has none; this
# says so rather than relying on it, because the root is only fresh until
# something in this script writes there.
rm -rf "$HOME/.omx/state/sessions"
ok "no OMX session pointer survives from anything"

# ---------------------------------------------------------------------------
note "the codex configuration the leader and its workers run under"

# (2) Hook trust. `omx setup` writes `[hooks.state."<hooks.json path>:…"]`
# trusted hashes keyed on the path **as it resolved it**, and Codex looks them up
# by the path *it* resolves — which differs the moment either side sees a
# symlinked root. No match means "Hooks need review", a numbered list, another
# `MODAL_MARKERS` entry, and a leader nobody can nudge. Rewriting the keys to the
# canonical spelling is the fixture's job; on a real `$HOME` the two already
# agree.
CODEX_REAL=$(cd "$HOME/.codex" && pwd -P)
python3 - "$HOME/.codex/config.toml" "$HOME/.codex" "$CODEX_REAL" "$STUB_PORT" \
    "$(cd "$E2E_WS" && pwd -P)" <<'PY'
import sys

path, home, real, port, ws = sys.argv[1:6]
text = open(path).read()
if home != real:
    text = text.replace(f'hooks.state."{home}/', f'hooks.state."{real}/')
# The stub provider, and the two policies that keep Codex from nesting its own
# sandbox inside friring's. `model_provider` must precede any table.
text = text.replace(
    'model_reasoning_effort = "medium"',
    'model_reasoning_effort = "medium"\n'
    'model_provider = "stub"\n'
    'approval_policy = "never"\n'
    'sandbox_mode = "danger-full-access"\n'
    'check_for_update_on_startup = false',
    1,
)
text += (
    f'\n[model_providers.stub]\nname = "Stub"\n'
    f'base_url = "http://127.0.0.1:{port}/v1"\nwire_api = "responses"\n'
    f'\n[projects."{ws}"]\ntrust_level = "trusted"\n'
)
open(path, "w").write(text)
PY
if grep -q "hooks.state.\"$CODEX_REAL/" "$HOME/.codex/config.toml"; then
    ok "the hook trust entries name the path a launch resolves"
else
    bad "the hook trust entries were not rewritten"
fi

# Required by the worker's `link-rw` seed, and synthetic on purpose: the stub
# provider carries no `env_key`, so Codex sends no authorization header and
# nothing ever reads this. A harness that needed a real one would not be a
# harness anybody could run.
cat > "$HOME/.codex/auth.json" <<'JSON'
{ "OPENAI_API_KEY": null, "tokens": null, "last_refresh": null }
JSON
chmod 600 "$HOME/.codex/auth.json"
ok "a synthetic auth.json is in place for the link-rw seed"

# ---------------------------------------------------------------------------
note "the repository, its plan, and the two scripts a turn runs"

git config --global user.name "friring-omx-team-e2e"
git config --global user.email "omx-team-e2e@friring.invalid"
git config --global init.defaultBranch main
EXT_HOME="$HOME/.config/friring/extensions/omx"
(
    cd "$E2E_WS"
    git init -q
    mkdir -p .omx/plans

    printf 'seed\n' > README.md
    printf 'alpha\n' > alpha.txt
    printf 'beta\n' > beta.txt

    # OMX's planning artifacts. `readTeamDag` wants an approved PRD, a matching
    # test spec and either a sidecar DAG or a fenced handoff block in the PRD.
    cat > ".omx/plans/prd-$PLAN_SLUG.md" <<'MD'
# PRD: demo

Two independent pieces of work, one per node.
MD
    cat > ".omx/plans/test-spec-$PLAN_SLUG.md" <<'MD'
# Test spec: demo

Each node's worker commits its own file and reports a result.
MD
    # Two nodes with disjoint `filePaths` and no dependency edge, so nothing is
    # serialized and the fan-out is genuinely two wide.
    cat > ".omx/plans/team-dag-$PLAN_SLUG.json" <<MD
{
  "schema_version": 1,
  "plan_slug": "$PLAN_SLUG",
  "nodes": [
    {
      "id": "alpha",
      "subject": "alpha",
      "description": "Edit alpha.txt and report a result.",
      "role": "executor",
      "filePaths": ["alpha.txt"],
      "requires_code_change": true
    },
    {
      "id": "beta",
      "subject": "beta",
      "description": "Edit beta.txt and report a result.",
      "role": "executor",
      "filePaths": ["beta.txt"],
      "requires_code_change": true
    }
  ]
}
MD

    # The leader's one job, run by its Codex through the shell tool. `plan`
    # turns the approved DAG into a run plan; `run` creates one bridge child per
    # ready node and drives them to terminal states.
    # Everything it says goes to a **file** as well as to stdout. `run` polls
    # for minutes, so Codex's shell tool yields and leaves it as a background
    # terminal — after which its output never reaches the pane again, and a
    # harness watching the pane for a completion marker waits out its whole
    # budget on a run that finished.
    cat > .omx/team-run.sh <<MD
#!/bin/sh
set -eu
: "\${FRIRING_BRIDGE_DIR:?this leader was not granted the bridge}"
omx_lib="$EXT_HOME/lib/friring-omx.mjs"
log="\$PWD/.omx/team-run.log"
say() { printf '%s\n' "\$*" | tee -a "\$log"; }
: > "\$log"
say "OMX-TEAM: planning"
node "\$omx_lib" plan "\$PWD" > "\$PWD/.omx/plan.json" 2>>"\$log"
say "OMX-TEAM: planned \$(jq -r '.nodes | length' "\$PWD/.omx/plan.json" 2>/dev/null || echo '?') nodes"
say "OMX-TEAM: running"
node "\$omx_lib" run "\$PWD" "$PLAN_SLUG" >> "\$log" 2>&1 || say "OMX-TEAM: run exited non-zero"
say "OMX-TEAM: integrating"
node "\$omx_lib" integrate "\$PWD" "$PLAN_SLUG" >> "\$log" 2>&1 || say "OMX-TEAM: integrate exited non-zero"
say "OMX-TEAM: finished"
MD

    # A worker's one turn: claim its task, do the node's work, commit it, and
    # send the one finish intent. friring stops the pane and reads the worktree
    # before deciding whether that intent becomes `done`.
    cat > .omx/worker-turn.sh <<'MD'
#!/bin/sh
set -eu
work=${TMPDIR:-.}
friring-cli bridge inbox --claim --json > "$work/task.json" 2>/dev/null || : > "$work/task.json"
# Named for this node's own worktree. A shared filename is a real merge conflict
# at `integrate` time — observed, and correctly refused — but the DAG here
# declares disjoint `filePaths`, so a worker writing outside its own is the
# fixture disagreeing with its own plan.
printf 'worked by %s\n' "$(basename "$PWD")" >> "OMX-WORKER-$(basename "$PWD").txt"
git add -A
git -c user.name=omx-worker -c user.email=worker@friring.invalid \
    commit -qm "omx worker output" || true
friring-cli bridge send --to owner --kind result \
    --body '{"outcome":"completed","summary":"omx worker finished its node"}'
echo "OMX-WORKER: reported a result"
MD

    chmod +x .omx/team-run.sh .omx/worker-turn.sh
    git add -A
    git commit -qm "omx-team-e2e seed"
)
ok "a git repository at $E2E_WS with a two-node plan"

# ---------------------------------------------------------------------------
note "the extension, its agents and its profile"

CFG_DIR="$FRIRING_CONFIG_DIR"
mkdir -p "$CFG_DIR"
cat > "$CFG_DIR/agents.toml" <<'TOML'
default = "omx-leader"
TOML
cat > "$CFG_DIR/settings.toml" <<'TOML'
[features]
notifications = false
TOML

# From this working tree, by path — and every one of the extension's 26
# requirement gates is evaluated here against what the real installer produced.
if fcli extension install "$REPO_ROOT/extensions/omx" > "$E2E_ARTIFACTS/install.json" 2>&1; then
    ok "the omx extension installed: all 26 requirement gates satisfied"
else
    bad "extension install refused"
    tail -5 "$E2E_ARTIFACTS/install.json"
    exit 1
fi
fcli extension activate omx > "$E2E_ARTIFACTS/activate.json" 2>&1 \
    || { bad "extension activate refused"; tail -3 "$E2E_ARTIFACTS/activate.json"; exit 1; }
for card in friring-team friring-autopilot; do
    if [ -f "$HOME/.codex/skills/$card/SKILL.md" ]; then
        ok "activate wrote the $card skill card"
    else
        bad "activate did not write the $card skill card"
    fi
done
for agent in omx-leader omx-worker-codex; do
    if grep -q "name = \"$agent\"" "$CFG_DIR/agents.toml"; then
        ok "$agent is registered in agents.toml"
    else
        bad "$agent never reached agents.toml"
    fi
done

# The template's placeholders, replaced with this run's own roots. The toolchain
# entries become the three directories a leader and its workers actually exec
# from — resolved, because a package manager puts a symlink on `PATH` and the
# file elsewhere.
resolve_dir() {
    local bin
    bin=$(command -v "$1") || return 1
    while [ -L "$bin" ]; do
        local link
        link=$(readlink "$bin")
        case "$link" in
            /*) bin=$link ;;
            *) bin=$(dirname "$bin")/$link ;;
        esac
    done
    (cd "$(dirname "$bin")" && pwd -P)
}
CODEX_DIR=$(resolve_dir codex)
NODE_DIR=$(resolve_dir node)
[ -f "$EXT_HOME/profiles.toml" ] || EXT_HOME="$REPO_ROOT/extensions/omx"
python3 - "$EXT_HOME/profiles.toml" "$E2E_ROOT/profile.toml" \
    "$E2E_WS" "$OMX_PREFIX" "$CODEX_DIR" "$NODE_DIR" <<'PY'
import sys

src, dst, ws, omx_prefix, codex_dir, node_dir = sys.argv[1:7]
text = open(src).read()
text = text.replace(
    '{ path = "~/dev/your-project", mode = "rw" }',
    f'{{ path = "{ws}", mode = "rw" }}',
)
# The same repository, in the entry that lets a worker commit.
text = text.replace('"~/dev/your-project/.git"', f'"{ws}/.git"')
# One template entry stands for "the prefix that provides omx, codex and node";
# this run has three separate ones, so it becomes three.
text = text.replace(
    '{ path = "~/.local/share/mise", mode = "ro" }',
    "\n    ".join(
        f'{{ path = "{d}", mode = "ro" }},'
        for d in dict.fromkeys([omx_prefix, codex_dir, node_dir])
    ).rstrip(","),
)
# The toolchain roots this fixture does not build with.
for drop in ('{ path = "~/.cargo/bin", mode = "ro" },', '{ path = "~/.rustup", mode = "ro" },'):
    text = text.replace(drop, "")
# The same fixture-only trade `codex-park` makes: the model is a Node stub on a
# dynamically-numbered loopback port, which is not a shape the egress proxy can
# name. `prompt_new_domains` off because nothing may raise a modal here.
text = text.replace('network_mode = "allowlist"', 'network_mode = "full"')
text = text.replace("prompt_new_domains = true", "prompt_new_domains = false")
open(dst, "w").write(text)
PY
fcli sandbox import "$E2E_ROOT/profile.toml" --replace >/dev/null \
    || { bad "sandbox import refused the profile"; cat "$E2E_ROOT/profile.toml"; exit 1; }
ok "the omx profile is stored, with this run's repository and toolchain roots"

# ---------------------------------------------------------------------------
note "the hook trust the install just invalidated"

# (2), and it has to happen **here** rather than before the install. `omx setup`
# writes `~/.codex/hooks.json` and records a trusted hash per entry; friring's
# install then merges its own four status hooks into that same file, so four
# hashes stop matching and the first launch shows "Hooks need review" — a
# numbered list, a `MODAL_MARKERS` entry, and a leader friring will not type
# into. Nothing can pre-compute the hash, so it is answered the way an operator
# answers it: once, in an ordinary terminal, before the sandboxed leader exists.
tmux -L "$TRUST_SOCKET" kill-server >/dev/null 2>&1 || true
tmux -L "$TRUST_SOCKET" new-session -d -s trust -x 200 -y 50 -c "$E2E_WS" \
    env CODEX_HOME="$HOME/.codex" codex 3>&-
trust_pane() { tmux -L "$TRUST_SOCKET" capture-pane -p -J -t trust 2>/dev/null || true; }
saw_dialog=0
for _ in $(seq 1 90); do
    if trust_pane | grep -qF "Hooks need review"; then
        saw_dialog=1
        break
    fi
    trust_pane | grep -qF "Ask Codex to do anything" && break
    sleep 1
done
if [ "$saw_dialog" = 1 ]; then
    ok "the install did invalidate the hook trust, as an operator would find"
    tmux -L "$TRUST_SOCKET" send-keys -t trust "2"
    sleep 0.5
    tmux -L "$TRUST_SOCKET" send-keys -t trust Enter
    for _ in $(seq 1 60); do
        trust_pane | grep -qF "Ask Codex to do anything" && break
        sleep 1
    done
else
    ok "no hook trust was owed"
fi
tmux -L "$TRUST_SOCKET" kill-server >/dev/null 2>&1 || true
if grep -qF "hooks.state" "$HOME/.codex/config.toml"; then
    ok "the trusted hashes are recorded for the hooks the leader will run"
else
    bad "no hook trust survived"
fi

# ---------------------------------------------------------------------------
note "the leader, created from the TUI"

tmux -L "$DRIVER_SOCKET" new-session -d -s "$DRIVER_SESSION" -x 200 -y 50 \
    env "${E2E_ENV[@]}" "$REPO_ROOT/target/debug/friring" 3>&-
need_pane "friring" 60 || exit 1
need_pane "No sessions yet" 30 || exit 1

key C-n
need_pane "New Session — Repo" 30 || exit 1
type_text "$E2E_WS"
key Enter

need_pane "New Session — Sandbox" 30 || exit 1
type_text "omx"
key Enter

need_pane "New Session — Name" 30 || exit 1
key C-u
type_text "$LEADER_NAME"
key Enter

if wait_pane "New Session — Agent" 10; then
    type_text "omx-leader"
    key Enter
fi

for _ in $(seq 1 50); do
    tmux -L "$TBX_DEV_SOCKET" set -g remain-on-exit on >/dev/null 2>&1 && break
    sleep 0.2
done

# ---------------------------------------------------------------------------
note "the vendor leader, inside the boundary"

leader_scroll() {
    tmux -L "$TBX_DEV_SOCKET" capture-pane -p -J -S -10000 -t "tb-$LEADER_NAME" 2>/dev/null || true
}

leader_up=0
for _ in $(seq 1 180); do
    if leader_scroll | grep -q "Ask Codex to do anything"; then
        leader_up=1
        break
    fi
    sleep 1
done
if [ "$leader_up" = 1 ]; then
    ok "omx launched Codex inside the boundary and reached a live composer"
else
    bad "the omx leader never reached a composer"
    printf -- '--- leader pane ---\n%s\n' "$(leader_scroll)"
fi
# The three gates, asserted as absent rather than assumed away.
for gate in "Star it on GitHub" "Hooks need review" "session_pointer_unusable"; do
    if leader_scroll | grep -qF "$gate"; then
        bad "a vendor first-run gate was not seeded away: $gate"
    else
        ok "no first-run gate: $gate"
    fi
done

# ---------------------------------------------------------------------------
note "the fan-out"

LEADER_ID=$(fcli --json session list 2>/dev/null \
    | jq -r --arg n "$LEADER_NAME" '.[] | select(.name == $n) | .id' | head -1)
if [ -z "$LEADER_ID" ] || [ "$LEADER_ID" = "null" ]; then
    bad "no leader session row"
    exit 1
fi

# Typed through friring's own delivery path, the way an operator invokes the
# skill this extension ships — so the modal guard is exercised rather than
# stepped around. Single-quoted: `$friring-team` is the skill's name, not a
# variable.
#
# The Enter is delivered separately, after the composer has echoed the text.
# `session send` types and presses Enter in one go, and Codex's composer needs
# the text to register first: `scripts/dev/agent-e2e/scenarios/codex-text-turn`
# records the same race, and here it left the prompt sitting unsubmitted while
# the run waited out its budget. The second keystroke goes to this run's own
# tmux server, on the private socket directory `tbx_sandbox_init_full` exported.
# shellcheck disable=SC2016
fcli session send "$LEADER_ID" '$friring-team run the approved plan' >/dev/null 2>&1 \
    || bad "friring refused to type into the leader's pane"
typed=0
for _ in $(seq 1 60); do
    if leader_scroll | grep -qF 'friring-team run the approved plan'; then
        typed=1
        break
    fi
    sleep 1
done
if [ "$typed" = 1 ]; then
    ok "the prompt reached the leader's composer"
    sleep 1
    tmux -L "$TBX_DEV_SOCKET" send-keys -t "tb-$LEADER_NAME" Enter 2>/dev/null || true
else
    bad "the prompt never appeared in the leader's composer"
fi

CHILD_PANES="$E2E_ARTIFACTS/child-panes"
mkdir -p "$CHILD_PANES"
capture_children() {
    tmux -L "$TBX_DEV_SOCKET" list-panes -a -F '#{pane_id} #{window_name}' 2>/dev/null \
        | while read -r pid wname; do
            case "$wname" in
                "tb-$LEADER_NAME" | zsh | automation-heartbeat | "") continue ;;
            esac
            safe=$(printf '%s' "$wname" | tr -c '[:alnum:]._-' '_')
            tmux -L "$TBX_DEV_SOCKET" capture-pane -p -J -S -2000 -t "$pid" \
                > "$CHILD_PANES/$safe.txt" 2>/dev/null || true
        done
}

# The script's own log, not the pane: Codex backgrounds a command that outlives
# its yield, and a backgrounded command's output never comes back to the pane.
TEAM_LOG="$E2E_WS/.omx/team-run.log"
run_ok=0
started=0
for i in $(seq 1 1800); do
    capture_children
    [ -f "$TEAM_LOG" ] && started=1
    if grep -qF "OMX-TEAM: finished" "$TEAM_LOG" 2>/dev/null; then
        run_ok=1
        break
    fi
    # A leader that never took the turn is not going to take it in another
    # twenty-five minutes, and waiting the budget out hides *when* it failed.
    if [ "$started" = 0 ] && [ "$i" -ge 240 ]; then
        bad "the leader never took the team turn: nothing was typed into it, or nothing answered"
        break
    fi
    sleep 1
done
leader_scroll > "$E2E_ARTIFACTS/leader-pane.txt"
cp "$TEAM_LOG" "$E2E_ARTIFACTS/team-run.log" 2>/dev/null || true
if [ "$started" = 1 ]; then
    ok "the leader's own team script ran: $(wc -l < "$TEAM_LOG" | tr -d ' ') lines logged"
fi
pane > "$E2E_ARTIFACTS/pane.txt"
if [ "$run_ok" = 1 ]; then
    ok "the leader's team run reached its end"
else
    bad "the leader's team run never finished"
    tail -40 "$E2E_ARTIFACTS/leader-pane.txt"
fi

find "$FRIRING_DATA_DIR" -maxdepth 1 -name 'friring.log.*' -exec cat {} + \
    > "$E2E_ARTIFACTS/friring.log" 2>/dev/null || true

# ---------------------------------------------------------------------------
note "what the host recorded"

LEADER=$(fcli --json session get "$LEADER_ID" 2>/dev/null || echo '{}')
printf '%s\n' "$LEADER" > "$E2E_ARTIFACTS/leader.json"

if [ "$(printf '%s' "$LEADER" | jq -r '.sandbox_profile // empty')" = "omx" ]; then
    ok "the leader runs under the omx profile"
else
    bad "the leader is not sandboxed into omx"
fi

CHILDREN=$(printf '%s' "$LEADER" | jq -r '.bridge.children // [] | length')
if [ "${CHILDREN:-0}" -ge 2 ]; then
    ok "the fan-out made $CHILDREN children, one per DAG node"
else
    bad "the fan-out made ${CHILDREN:-0} children, not the plan's two"
fi

# The host's own verdict, not a child's claim: `state` is what friring reached
# after stopping the pane and reading the worktree.
DECIDED=$(printf '%s' "$LEADER" \
    | jq -r '[.bridge.children // [] | .[] | select(.state == "done")] | length')
if [ "${DECIDED:-0}" -ge 2 ]; then
    ok "friring verified $DECIDED children into done"
else
    bad "friring verified ${DECIDED:-0} children into done"
    printf '%s' "$LEADER" | jq -r '.bridge.children // [] | .[] | "\(.name) \(.state)"' || true
fi

if [ "$(printf '%s' "$LEADER" \
    | jq -r '[.bridge.children // [] | .[] | select(.result.verified_at != null)] | length')" \
    -ge 2 ]
then
    ok "each child carries a verdict friring verified itself"
else
    bad "not every child carries a host verdict"
fi

# ---------------------------------------------------------------------------
note "what a worker's own boundary says"

# The generated seatbelt profile *is* the boundary, so it is the thing to read.
# The leader may write OMX's two roots — its session identity, its launch
# lineage, its logs, its codebase map — and no worker may even read them: they
# are declared in the leader agent's `state_rw` rather than in the profile's
# `paths`, and a worker's own agent block does not declare them. A profile path
# would be inherited by every child, read-only, after narrowing.
#
# Read as *rules*, not as substrings: a profile that names a path in a deny is
# not one that grants it, and `~/.omx` is a prefix of `~/.omx-runs`, so a
# substring search would report a pass for one root while the other leaked. Both
# roots are asked about separately, and only `allow` lines count.
#
# Seatbelt only, because the generated `.sb` file is the only backend artifact
# that *is* the policy. Under bubblewrap the boundary is an argv this run does
# not keep, so there is nothing equivalent to read; the unit tests carry that
# backend, and this block says so rather than passing silently.
PROFILE_DIR="$FRIRING_DATA_DIR/sandbox/profiles"
LEADER_SB="$PROFILE_DIR/omx-$LEADER_ID.sb"
grants_root() { # <profile file> <path>
    grep -E '^\(allow file-(read|write)' "$1" 2>/dev/null | grep -qF "\"$2\""
}
if [ ! -f "$LEADER_SB" ]; then
    if [ "$(uname)" = "Darwin" ]; then
        bad "the leader has no generated seatbelt profile to read"
        find "$PROFILE_DIR" -maxdepth 1 -name '*.sb' 2>/dev/null | head
    else
        note "  (no seatbelt profiles on $(uname): the boundary is a bwrap argv)"
    fi
else
    for root in "$HOME/.omx" "$HOME/.omx-runs"; do
        if grants_root "$LEADER_SB" "$root"; then
            ok "the leader's own boundary grants $root"
        else
            bad "the leader's boundary does not grant $root"
            grep -F "$root" "$LEADER_SB" | head -3
        fi
    done
    leaked=0
    checked=0
    missing=0
    while read -r child; do
        [ -n "$child" ] || continue
        sb="$PROFILE_DIR/omx-$child.sb"
        if [ ! -f "$sb" ]; then
            missing=$((missing + 1))
            continue
        fi
        checked=$((checked + 1))
        for root in "$HOME/.omx" "$HOME/.omx-runs"; do
            # Any mention at all, allow or deny: a worker's profile has no
            # business naming either root, and an *enclosing* grant would name
            # the ancestor instead — which is why the leader assertion above
            # exists in the same breath.
            if grep -qF "\"$root\"" "$sb"; then
                leaked=$((leaked + 1))
                grep -F "$root" "$sb" | head -3
            fi
        done
    done <<< "$(printf '%s' "$LEADER" | jq -r '.bridge.children // [] | .[] | .id // empty')"
    if [ "$checked" -ge 2 ] && [ "$leaked" -eq 0 ] && [ "$missing" -eq 0 ]; then
        ok "neither worker's boundary names OMX's state roots ($checked profiles read)"
    else
        bad "$leaked leaks across $checked worker boundaries, $missing profile(s) missing"
    fi
fi

# ---------------------------------------------------------------------------
note "what the run left in the repository"

# Per node, and against the **commit friring verified** rather than against a
# commit message. Counting subjects on `main` cannot tell two nodes from one
# node that committed twice — the worker's turn script commits on every turn —
# and it says nothing about whether the merge carried the work the host actually
# looked at. `result.head` is what the verdict was reached over, so ancestry in
# `main` is the whole claim: this exact commit is in the merged history.
VERIFIED_HEADS=()
for node in alpha beta; do
    branch="omx/$PLAN_SLUG/$node"
    if git -C "$E2E_WS" branch --list "$branch" | grep -q .; then
        ok "node $node has the branch friring cut for it"
    else
        bad "node $node has no branch"
        git -C "$E2E_WS" branch --list | head
    fi
    head=$(printf '%s' "$LEADER" | jq -r --arg b "$branch" \
        '[.bridge.children // [] | .[]
          | select(.result.branch == $b) | .result.head // empty] | first // empty')
    if [ -z "$head" ]; then
        bad "no host verdict names branch $branch, so there is no verified head to trace"
        printf '%s' "$LEADER" | jq -r '.bridge.children // [] | .[] | .result' || true
        continue
    fi
    VERIFIED_HEADS+=("$head")
    if git -C "$E2E_WS" merge-base --is-ancestor "$head" main 2>/dev/null; then
        ok "node $node's verified head ${head:0:7} is an ancestor of main"
    else
        bad "node $node's verified head $head never reached main"
        git -C "$E2E_WS" log --oneline -6 main || true
        grep -A 6 '"conflict"' "$E2E_ARTIFACTS/team-run.log" 2>/dev/null | head -8
    fi
    # The commit is that node's own work. Judged on the **files** it touched,
    # not on its subject: every worker turn commits the same literal message, so
    # a subject match would fail an honest run and prove nothing on a dishonest
    # one. The fixture's worker writes `OMX-WORKER-<its worktree>.txt`, and the
    # worktree is named for the node's branch.
    if git -C "$E2E_WS" show --name-only --format= "$head" 2>/dev/null \
        | grep -qi "$node"; then
        ok "node $node's verified head touches $node's own file"
    else
        bad "node $node's verified head touches nothing of $node's"
        git -C "$E2E_WS" show --name-only --format='%H %s' "$head" || true
    fi
done
# Two nodes, two commits: one child's head satisfying both checks would pass
# every assertion above.
DISTINCT=$(printf '%s\n' "${VERIFIED_HEADS[@]:-}" | sort -u | grep -c .)
if [ "${DISTINCT:-0}" -ge 2 ]; then
    ok "the two nodes were verified at two distinct commits"
else
    bad "the run verified ${DISTINCT:-0} distinct commits, not the plan's two"
fi

# ---------------------------------------------------------------------------
note "what the stub was asked for"

if grep -q '"matched":"UNMATCHED"' "$E2E_ARTIFACTS/stub-journal.ndjson" 2>/dev/null; then
    bad "the stub saw a request no fixture matched"
    grep '"matched":"UNMATCHED"' "$E2E_ARTIFACTS/stub-journal.ndjson" | head -3
else
    ok "every model request matched a fixture"
fi
WORKER_TURNS=$(grep -c '"matched":"worker"' "$E2E_ARTIFACTS/stub-journal.ndjson" 2>/dev/null || true)
if [ "${WORKER_TURNS:-0}" -ge 2 ]; then
    ok "$WORKER_TURNS worker turns ran against the stub, with no credential anywhere"
else
    bad "only ${WORKER_TURNS:-0} worker turns ran"
fi

# ---------------------------------------------------------------------------
printf '\n%s: %d failed\n' "$E2E_NAME" "$FAILURES"
printf 'artifacts: %s\n' "$E2E_ARTIFACTS"
[ "$FAILURES" -eq 0 ]
