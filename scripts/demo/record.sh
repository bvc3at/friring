#!/usr/bin/env sh
# Regenerate ALL Friring demo media in one pass, using REAL coding-agent CLIs
# driven by a LOCAL MODEL STUB — no real accounts, no network, no auth.
#
# This single script records every video pair under docs/media/:
#
#   * friring-demo.{gif,mp4}            (agents.tape          — the hero demo)
#   * friring-file-manager.{gif,mp4}    (file-manager.tape)
#   * friring-info-panel.{gif,mp4}      (info-panel.tape)
#   * friring-theme.{gif,mp4}           (theme.tape)
#   * friring-session-creation.{gif,mp4}(session-creation.tape)
#   * friring-fork.{gif,mp4}            (fork.tape)
#   * automations-demo.{gif,mp4}        (automations.tape)
#   * tasks-demo.{gif,mp4}              (tasks.tape)
#   * search-demo.{gif,mp4}             (search.tape)
#   * code-review-demo.{gif,mp4}        (code-review.tape)
#
# Every clip drives the actual `claude`, `codex`, `opencode` and `antigravity`
# CLIs — one per friring session — to showcase real multi-agent orchestration.
# The model API is served by the e2e harness's local stubs
# (scripts/dev/agent-e2e/stub/), so each pane shows a SCRIPTED conversation
# from scripts/demo/demo-content.json (a deadpan techno-optimistic-future bit)
# with no real model call and no account on screen.
#
# Why a stub instead of real auth (the old approach):
#   * No account identity ever renders — every agent talks to 127.0.0.1.
#   * Deterministic: the same scripted exchange every run, so re-records diff
#     cleanly.
#   * Offline: non-loopback egress is dead-proxied; nothing external is
#     load-bearing.
#   * antigravity (agy) is the one exception — it forces real Google OAuth and
#     cannot be stubbed offline (see scripts/dev/agent-e2e/agents/antigravity/
#     profile.sh), so it is featured LOGGED OUT on its clean, branded login
#     screen, which also keeps any account identity off camera.
#
# The stub dialects, agent env, and config seeds are shared with the real-agent
# e2e harness — see docs/E2E.md. The scripted conversations become stub
# fixtures via scripts/demo/lib/gen-stub-fixtures.mjs.
#
# Isolation (so this never touches your real friring, tmux, or agent accounts):
#   * HOME/XDG_*/TMUX_TMPDIR point at a throwaway dir (shared sandbox helper),
#     so the `friring-dev` tmux server, agent configs, and DB are all disposable
#     and cannot reach anything you have running.
#
# Requirements: cargo, git, tmux, sqlite3, jq, node (>= 18), asciinema + agg +
# ffmpeg, and whichever agent CLIs you want to feature (claude / codex /
# opencode / antigravity). Missing agents are skipped with a warning.
#
# Usage:  scripts/demo/record.sh [tape-stem ...]
#
#   With no args, records every tape below. Pass one or more tape stems to
#   re-record only a subset, e.g. `record.sh theme automations`.

set -eu

# Tapes to record (stems of scripts/demo/<stem>.tape), hero first. `agents` is
# the combined hero demo (docs/media/friring-demo.*); the rest are per-feature
# clips (`automations` -> automations-demo.*, `tasks` -> tasks-demo.*, `search`
# -> search-demo.*, others -> friring-<stem>.*).
ALL_TAPES="agents file-manager info-panel theme session-creation fork automations tasks search code-review"
TAPES="${*:-$ALL_TAPES}"

# friring TUI theme every clip starts in (persisted string in metadata.active_theme,
# see src/session/theme_config.rs). The `theme` clip switches away from it to show
# the picker, so we re-apply this before EVERY tape to keep all videos on-brand.
DEMO_THEME="${DEMO_THEME:-doom}"

# --- Locate the repo root (this script lives in scripts/demo/) ---------------
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
cd "$REPO_ROOT"

STUB_SRC="$REPO_ROOT/scripts/dev/agent-e2e/stub"
CONTENT="$SCRIPT_DIR/demo-content.json"

# Validate requested tapes exist before doing any expensive setup.
for tape in $TAPES; do
    if [ ! -f "$SCRIPT_DIR/$tape.tape" ]; then
        echo "error: no such tape: $SCRIPT_DIR/$tape.tape" >&2
        echo "  available: $ALL_TAPES" >&2
        exit 1
    fi
done

# --- Preflight: required tools ----------------------------------------------
missing=
for tool in cargo git tmux asciinema agg ffmpeg sqlite3 jq node; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    echo "error: missing required tool(s):$missing" >&2
    echo "  asciinema + agg: brew install asciinema agg" >&2
    exit 1
fi
[ -f "$CONTENT" ] || { echo "error: no demo content at $CONTENT" >&2; exit 1; }

# The demo font must be installed, and we have to *ask agg* rather than probe the
# system: agg resolves families itself and, when one is missing, silently falls
# back to whatever else it can find — which is how the media's typography
# previously drifted with the recording machine. A one-event cast is enough to
# make it report what it picked.
check_demo_font() {
    _probe=$(mktemp -d "${TMPDIR:-/tmp}/friring-font.XXXXXX")
    printf '{"version": 2, "width": 20, "height": 3}\n[0.0, "o", "probe"]\n' > "$_probe/p.cast"
    # shellcheck disable=SC2086 # DEMO_FONT_DIRS is a pre-split flag list
    _picked=$(agg "$_probe/p.cast" "$_probe/p.gif" --fps-cap 1 $DEMO_FONT_DIRS \
        --text-font-family "$DEMO_FONT" -v 2>&1 \
        | sed -n 's/.*primary text font family: //p' | head -1)
    rm -r "$_probe"
    if [ "$_picked" != "$DEMO_FONT" ]; then
        echo "error: demo font '$DEMO_FONT' is not installed (agg would use '${_picked:-none}')" >&2
        echo "  macOS:  brew install --cask font-meslo-lg" >&2
        echo "  nix:    it is in the flake's demoTools" >&2
        exit 1
    fi
}

# Map a featured-agent display name to its actual CLI binary. They differ only
# for antigravity, whose binary is `agy` (the Gemini CLI successor); identity for
# everyone else.
agent_command() {
    case "$1" in
        antigravity) echo "agy" ;;
        *) echo "$1" ;;
    esac
}

# Prefer coreutils `gtimeout` over `timeout`: third-party `timeout` shims exist
# on PATH in the wild and silently break the child.
if command -v gtimeout >/dev/null 2>&1; then
    TIMEOUT=gtimeout
elif command -v timeout >/dev/null 2>&1; then
    TIMEOUT=timeout
else
    TIMEOUT=
fi

# Which agent CLIs can we actually feature? Present on PATH *and* responsive:
# an agent that cannot answer `--version` cannot render a session either, and
# filming its dead pane would be worse than leaving it out (this is the same
# rule the e2e suite's require_agent applies).
AGENTS=
for a in claude codex opencode antigravity; do
    bin=$(agent_command "$a")
    if ! command -v "$bin" >/dev/null 2>&1; then
        echo "warning: '$bin' not found on PATH — skipping '$a' in the demo" >&2
    elif [ -n "$TIMEOUT" ] && ! $TIMEOUT 15 "$bin" --version >/dev/null 2>&1; then
        echo "warning: '$bin' did not respond to --version — skipping '$a' in the demo" >&2
    else
        AGENTS="$AGENTS $a"
    fi
done
if [ -z "$AGENTS" ]; then
    echo "error: none of claude/codex/opencode/antigravity (agy) are installed" >&2
    exit 1
fi

# Is an agent in the available set? (space-delimited membership test.)
have_agent() {
    case " $AGENTS " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

# --- Build the dev binaries (version 0.0.0-dev => dev_build cfg) -------------
# Build BEFORE the HOME override so cargo still finds ~/.cargo.
echo "==> Building friring (dev) ..."
cargo build --bin friring --bin friring-cli

FRIRING_BIN="$REPO_ROOT/target/debug/friring"
CLI_BIN="$REPO_ROOT/target/debug/friring-cli"
export FRIRING_BIN   # consumed by the tapes (they `exec "$FRIRING_BIN"`)

# Where the host keeps its fonts — captured BEFORE the sandbox replaces $HOME,
# and handed to agg with --font-dir. A font is a host resource, not part of the
# environment we isolate: the throwaway HOME hides the user's font directory, and
# agg (which resolves families itself) would then find *nothing* and silently
# render with a fallback. The symbol fallbacks friring needs live there too.
DEMO_FONT_DIRS=
for _fd in "$HOME/Library/Fonts" /Library/Fonts "$HOME/.local/share/fonts" \
    /usr/share/fonts /usr/local/share/fonts; do
    [ -d "$_fd" ] && DEMO_FONT_DIRS="$DEMO_FONT_DIRS --font-dir $_fd"
done

# --- Isolated environment (shared dev-sandbox helper) ------------------------
# shellcheck source=scripts/dev/lib/sandbox-env.sh
# shellcheck disable=SC1091
. "$REPO_ROOT/scripts/dev/lib/sandbox-env.sh"
tbx_sandbox_init_full fresh              # throwaway temp HOME/XDG/TMUX_TMPDIR
DEMO_HOME="$TBX_SANDBOX_ROOT"
CFG_DIR="$XDG_CONFIG_HOME/friring-dev"   # dev_build subdir
DB_FILE="$XDG_DATA_HOME/friring-dev/friring.db"  # SQLite db (dev_build subdir)
mkdir -p "$CFG_DIR"

STUB_DIR="$DEMO_HOME/stubs"
mkdir -p "$STUB_DIR/raw"
STUB_PIDS=""

cleanup() {
    # The isolated tmux server (in TMUX_TMPDIR) hosts every agent pane, so the
    # helper's single kill reaps all the real agent processes too — and cannot
    # reach any tmux server outside this throwaway directory. Then the stubs.
    for pid in $STUB_PIDS; do kill "$pid" >/dev/null 2>&1 || true; done
    tbx_sandbox_teardown
}
trap cleanup EXIT INT TERM

# --- Model stubs: one per wire dialect, from the scripted conversations ------
echo "==> Generating stub fixtures from $(basename "$CONTENT")"
node "$SCRIPT_DIR/lib/gen-stub-fixtures.mjs" "$CONTENT" "$STUB_DIR"

# start_stub <dialect> <fixtures> -> echoes the base URL; records PID + port.
start_stub() {
    _dialect="$1"; _fixtures="$2"
    _portfile="$STUB_DIR/$_dialect.port"
    node "$STUB_SRC/$_dialect-stub.mjs" --port 0 --port-file "$_portfile" \
        --journal "$STUB_DIR/$_dialect-journal.jsonl" \
        --raw-dir "$STUB_DIR/raw" --fixtures "$_fixtures" \
        > "$STUB_DIR/$_dialect.log" 2>&1 &
    STUB_PIDS="$STUB_PIDS $!"
    _i=0
    while [ ! -s "$_portfile" ] && [ "$_i" -lt 50 ]; do sleep 0.1; _i=$((_i + 1)); done
    [ -s "$_portfile" ] || { echo "error: $_dialect stub did not start" >&2; exit 1; }
    echo "http://127.0.0.1:$(cat "$_portfile")"
}

ANTHROPIC_URL=$(start_stub anthropic "$STUB_DIR/anthropic-fixtures.json")
OPENAI_URL=$(start_stub openai "$STUB_DIR/openai-fixtures.json")
echo "==> Stubs up: anthropic=$ANTHROPIC_URL openai=$OPENAI_URL"

# --- Per-agent model ids (shown in each CLI's own status line) ---------------
# Pulled from the scripted content so the fictional-future model names on
# screen match the conversation. One value per dialect: all claude sessions
# share ANTHROPIC_MODEL; codex/opencode read theirs from their config files.
demo_model() { jq -r --arg a "$1" 'first(.sessions[] | select(.agent==$a) | .model) // empty' "$CONTENT"; }
CLAUDE_MODEL=$(demo_model claude)
CODEX_MODEL=$(demo_model codex)
OPENCODE_MODEL=$(demo_model opencode)

# --- Shared tmux-server environment (exported BEFORE any tmux server starts) --
# tmux panes inherit the server environment, and the server inherits ours; this
# is how each agent's stub URL / config reach it with zero friring changes. The
# vars are differently named per agent, so a single shared env carries all of
# them without collision. Non-loopback egress is dead-proxied (app-level
# offline); no_proxy keeps the loopback stub calls direct.
export CLAUDE_CONFIG_DIR="$HOME/claude-config"
export CODEX_HOME="$HOME/codex-home"
export OPENCODE_CONFIG="$HOME/opencode.json"
export ANTHROPIC_BASE_URL="$ANTHROPIC_URL"
export ANTHROPIC_AUTH_TOKEN="friring-demo-dummy"
# The info panel's Usage gauges: friring fetches the account-usage route with
# the OAuth token from .credentials.json (seeded below) — point that fetch at
# the stub too, or every clip films "not logged in (no subscription token)".
export FRIRING_CLAUDE_USAGE_URL="$ANTHROPIC_URL/api/oauth/usage"
[ -n "$CLAUDE_MODEL" ] && export ANTHROPIC_MODEL="$CLAUDE_MODEL"
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
export DISABLE_AUTOUPDATER=1 DISABLE_TELEMETRY=1 DISABLE_ERROR_REPORTING=1 DISABLE_BUG_COMMAND=1
export OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_DISABLE_AUTOUPDATE=1 OPENCODE_DISABLE_LSP_DOWNLOAD=1
export http_proxy=http://127.0.0.1:9 https_proxy=http://127.0.0.1:9
export HTTP_PROXY=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9
export no_proxy=127.0.0.1,localhost NO_PROXY=127.0.0.1,localhost

# --- Sample repo (the review object's files) + a parent folder of repos ------
# demo-content.review defines a small, realistic Rust repo (the orbital-HVAC
# bit); its base_files are the sample repo the file viewer browses and the
# code-review demo diffs against. Written straight from the JSON.
DEMO_REPO="$DEMO_HOME/$(jq -r '.review.repo_name' "$CONTENT")"
# Write a JSON object of {relative-path: file-content} to disk. The paths are
# plain relative filenames (no newlines), so a keys loop + per-key jq lookup is
# safe and avoids base64 (whose flags differ across BSD/GNU).
write_files() { # <jq-path-to-object> <dest-dir>
    jq -r "$1 | keys_unsorted[]" "$CONTENT" | while IFS= read -r _rel; do
        _dst="$2/$_rel"
        mkdir -p "$(dirname "$_dst")"
        jq -r "$1[\$k]" --arg k "$_rel" "$CONTENT" > "$_dst"
    done
}
mkdir -p "$DEMO_REPO"
write_files '.review.base_files' "$DEMO_REPO"
git init -q "$DEMO_REPO"
git -C "$DEMO_REPO" -c user.email=demo@friring -c user.name=demo add -A
git -C "$DEMO_REPO" -c user.email=demo@friring -c user.name=demo \
    commit -q -m "init sample project"
DEMO_BASE_BRANCH=$(git -C "$DEMO_REPO" symbolic-ref --short HEAD)

# A parent folder of several repos, for the "import as parent" demo. Lives
# under $HOME so the session-creation tape can type `~/projects` and have the
# picker's tilde-expansion resolve it during recording.
PROJECTS_DIR="$HOME/projects"
for r in $(jq -r '.projects_dirs[]' "$CONTENT"); do
    repo="$PROJECTS_DIR/$r"
    mkdir -p "$repo"
    printf '# %s\n' "$r" > "$repo/README.md"
    git init -q "$repo"
    git -C "$repo" -c user.email=demo@friring -c user.name=demo add -A
    git -C "$repo" -c user.email=demo@friring -c user.name=demo commit -q -m "init $r"
done

# --- Per-agent config seeds (no auth; each points at its loopback stub) -------
# The trusted dirs: the sample repo plus the parent-folder repos the
# session-creation tape browses.
TRUST_DIRS="$DEMO_REPO $PROJECTS_DIR"
for r in $(jq -r '.projects_dirs[]' "$CONTENT"); do
    TRUST_DIRS="$TRUST_DIRS $PROJECTS_DIR/$r"
done

# claude: onboarding + bypass + per-folder trust; no credentials (the stub
# needs none). Seeded in both HOME and CLAUDE_CONFIG_DIR (claude reads the
# latter when the override is set). A binary symlink keeps its self-install
# check quiet under the throwaway HOME.
if have_agent claude; then
    mkdir -p "$HOME/claude-config" "$HOME/.local/bin"
    # shellcheck disable=SC2086 # word-split TRUST_DIRS into positional args
    claude_seed=$(jq -n '{hasCompletedOnboarding: true,
        bypassPermissionsModeAccepted: true,
        projects: ($ARGS.positional | map({(.): {hasTrustDialogAccepted: true}}) | add)}' \
        --args $TRUST_DIRS)
    printf '%s' "$claude_seed" > "$HOME/.claude.json"
    printf '%s' "$claude_seed" > "$HOME/claude-config/.claude.json"
    claude_bin=$(command -v claude 2>/dev/null || true)
    [ -n "$claude_bin" ] && ln -sf "$(readlink -f "$claude_bin" 2>/dev/null || echo "$claude_bin")" \
        "$HOME/.local/bin/claude"
    # A fake subscription credential for friring's usage fetch (scripted
    # numbers served by the stub via FRIRING_CLAUDE_USAGE_URL). Seeded ONLY at
    # the ~/.claude fallback path, never in CLAUDE_CONFIG_DIR: friring checks
    # the override dir first but falls back when the file is absent there
    # (src/usage/mod.rs), while the claude CLI reads its own state from
    # CLAUDE_CONFIG_DIR — so the pane keeps authing with ANTHROPIC_AUTH_TOKEN
    # and never tries to refresh this fictional OAuth token against a dead
    # proxy. The plan tier is the fictional-future subscription on camera.
    mkdir -p "$HOME/.claude"
    printf '{"claudeAiOauth":{"accessToken":"friring-demo-oauth-dummy","subscriptionType":"max-100x"}}\n' \
        > "$HOME/.claude/.credentials.json"
fi

# codex: a custom provider against the openai stub (Responses wire); fictional
# model; every demo dir trusted; no approval/sandbox prompts. No env_key, so no
# auth header is sent (and no ChatGPT login is ever required).
if have_agent codex; then
    mkdir -p "$CODEX_HOME"
    {
        printf 'model = "%s"\nmodel_provider = "stub"\n' "${CODEX_MODEL:-gpt-6.x}"
        printf 'approval_policy = "never"\nsandbox_mode = "read-only"\n\n'
        printf '[model_providers.stub]\nname = "Stub"\nbase_url = "%s/v1"\nwire_api = "responses"\n' "$OPENAI_URL"
        for p in $TRUST_DIRS; do
            printf '\n[projects."%s"]\ntrust_level = "trusted"\n' "$p"
        done
    } > "$CODEX_HOME/config.toml"
fi

# opencode: a custom provider against the openai stub (Chat Completions wire);
# fictional model; dummy api key (the field is required, the stub ignores it).
if have_agent opencode; then
    jq -n --arg url "$OPENAI_URL/v1" --arg model "${OPENCODE_MODEL:-stub-model}" '{
        "$schema": "https://opencode.ai/config.json",
        provider: { stub: { npm: "@ai-sdk/openai-compatible", name: "Stub",
            options: { baseURL: $url, apiKey: "dummy" },
            models: { ($model): { name: $model } } } },
        model: ("stub/" + $model), autoupdate: false, share: "disabled"
    }' > "$OPENCODE_CONFIG"
fi

# antigravity (agy): featured LOGGED OUT — it forces real Google OAuth and
# can't be stubbed (profile.sh explains). Seed only enough ~/.gemini state that
# its logged-out screen is tidy (onboarding complete, folders pre-trusted, auth
# type chosen), and launch it with the keyring/D-Bus cut off so it boots to its
# clean "select login method" screen instead of fetching+printing the signed-in
# Google account's email. Mirrors the claude treatment: featured, identity-free.
if have_agent antigravity; then
    mkdir -p "$HOME/.gemini/antigravity-cli/cache"
    printf '{"security":{"auth":{"selectedType":"oauth-personal"}}}\n' > "$HOME/.gemini/settings.json"
    # shellcheck disable=SC2086 # word-split TRUST_DIRS into positional args
    jq -n '$ARGS.positional | map({(.): "TRUST_FOLDER"}) | add' --args $TRUST_DIRS \
        > "$HOME/.gemini/trustedFolders.json"
    printf '{"consumerOnboardingComplete":true,"enterpriseOnboardingComplete":false,"onboardingComplete":true}\n' \
        > "$HOME/.gemini/antigravity-cli/cache/onboarding.json"
fi

# --- Agent registry: one entry per available CLI -----------------------------
{
    # shellcheck disable=SC2086 # $AGENTS is a space-separated list, split on purpose
    first=$(printf '%s\n' $AGENTS | head -n1)
    echo "default = \"$first\""
    for a in $AGENTS; do
        if [ "$a" = "antigravity" ]; then
            # Cut the keyring/D-Bus so agy can't fetch+print the account email
            # (see the seeding note above).
            printf '\n[[agents]]\nname = "antigravity"\ncommand = "env"\nargs = ["-u", "GNOME_KEYRING_CONTROL", "DBUS_SESSION_BUS_ADDRESS=/dev/null", "agy"]\n'
        else
            printf '\n[[agents]]\nname = "%s"\ncommand = "%s"\n' "$a" "$(agent_command "$a")"
        fi
    done
} > "$CFG_DIR/agents.toml"

# --- tmux config the agent panes inherit -------------------------------------
# Written before any tmux server starts: a server reads ~/.tmux.conf once, at
# start, and $HOME is the sandbox here.
#
# focus-events is the point. Without it the agents' own terminals never learn
# they have focus, so Claude Code paints a "tmux focus-events off · add 'set -g
# focus-events on' to ~/.tmux.conf" hint across the pane — filmed in every clip
# — and the focus/query escapes the recorder's attach generates are passed
# through to the agent instead of being consumed, where they land as a stray
# glyph in its composer.
printf 'set -g focus-events on\nset -g default-terminal "tmux-256color"\n' > "$HOME/.tmux.conf"

# --- Keybindings: rebind global search to Ctrl+A for the demo ----------------
# The real default for Action::GlobalSearch is Ctrl+/ (plus the Ctrl+7/Ctrl+_
# raw-0x1F encodings), which VHS+ttyd do not deliver reliably across terminals.
# The search.tape opens the strip with Ctrl+A — an unambiguous chord every
# terminal sends — so seed a keybindings.json that maps it. Only GlobalSearch is
# overridden; every other action keeps its built-in default.
printf '{\n  "GlobalSearch": ["ctrl+a"]\n}\n' > "$CFG_DIR/keybindings.json"

# --- Seed the demo state -----------------------------------------------------
# Called fresh before EVERY tape, because the clips mutate the very state the
# next one poses against: `agents` and `session-creation` each spawn a session,
# `fork` spawns two more, `tasks`/`automations` add rows. Seeding once and
# filming all ten in a row therefore drifts — by the last clip the session list
# has accumulated strangers and the *selected* session is whatever was spawned
# most recently, so `code-review` opened on a session with no branch and filmed
# "No changes to show for this target". Re-seeding makes each clip independent
# and reproducible on its own (`record.sh code-review` films exactly what the
# full run does), at the cost of a rebuild per tape.
seed_demo_state() {
    # Wipe the state the previous clip left: the agent panes (the tmux server
    # owns them) and every session/task/automation row.
    tmux -L "$TBX_DEV_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "$DB_FILE"
    # Wire the built-in hooks extension into the fresh DB before any session
    # exists. Two reasons: a session only gets the hook wiring patched into its
    # agent args if the extension is active when it is spawned (this is what
    # makes the clips' working/done status indicators live), and the TUI would
    # otherwise do this itself on first launch and toast the outcome — which it
    # reports at Error level, painting a red "Config: hooks: wired agent hooks
    # for claude" banner across the bottom of every frame we film.
    "$CLI_BIN" extension activate hooks >/dev/null 2>&1 \
        || echo "warning: could not activate the hooks extension" >&2
    seed_sessions
    preplay_conversations
    seed_tasks_and_automation
    set_theme "$DEMO_THEME"
}

# --- Create one session per scripted session ---------------------------------
# Order follows demo-content.json (review LAST, so restore leaves it selected on
# launch — finish_adopted_session makes the last-restored session active — which
# is what the code-review and hero tapes rely on). The "review" session is a
# worktree off the sample repo whose branch carries the review object's diff, so
# the code-review view shows a real, colourful <base>..HEAD change.
seed_sessions() {
echo "==> Seeding one session per scripted agent"
REVIEW_BRANCH=$(jq -r '.review.branch' "$CONTENT")
# The review worktree + branch outlive the DB, so a re-seed would otherwise
# collide with the previous clip's leftovers.
git -C "$DEMO_REPO" worktree list --porcelain 2>/dev/null \
    | awk '/^worktree /{print substr($0,10)}' \
    | while IFS= read -r wt; do
        [ "$wt" = "$DEMO_REPO" ] || git -C "$DEMO_REPO" worktree remove --force "$wt" 2>/dev/null
    done
git -C "$DEMO_REPO" worktree prune 2>/dev/null || true
git -C "$DEMO_REPO" branch -D "$REVIEW_BRANCH" >/dev/null 2>&1 || true
session_count=$(jq '.sessions | length' "$CONTENT")
i=0
while [ "$i" -lt "$session_count" ]; do
    sname=$(jq -r ".sessions[$i].session_name" "$CONTENT")
    sagent=$(jq -r ".sessions[$i].agent" "$CONTENT")
    i=$((i + 1))
    have_agent "$sagent" || { echo "  (skipping $sname — $sagent not installed)"; continue; }
    if [ "$sname" = "review" ]; then
        "$CLI_BIN" session create --name "review" --repo-path "$DEMO_REPO" \
            --agent "$sagent" --worktree-branch "$REVIEW_BRANCH" \
            --base-branch "$DEMO_BASE_BRANCH" >/dev/null
        REVIEW_WT=$(git -C "$DEMO_REPO" worktree list --porcelain \
            | awk -v b="branch refs/heads/$REVIEW_BRANCH" \
                '/^worktree /{p=substr($0,10)} $0==b{print p}')
        if [ -n "$REVIEW_WT" ]; then
            write_files '.review.branch_files' "$REVIEW_WT"
            git -C "$REVIEW_WT" -c user.email=demo@friring -c user.name=demo add -A
            git -C "$REVIEW_WT" -c user.email=demo@friring -c user.name=demo \
                commit -q -m "$(jq -r '.review.commit_message' "$CONTENT")"
        fi
    else
        "$CLI_BIN" session create --name "$sname" --repo-path "$DEMO_REPO" \
            --agent "$sagent" >/dev/null
    fi
done
}

# --- Pre-play each scripted conversation into its pane -----------------------
# `friring-cli session send` types the prompt into the agent's pane (paste →
# brief delay → Enter); the stub answers from the matching fixture. We poll the
# pane for a short marker of each reply so the next turn (and, later, recording)
# only starts once the exchange has rendered. Unstubbable agents (antigravity)
# have no pre-play entry — they stay on their logged-out screen.
sid() { "$CLI_BIN" --json session list | jq -r --arg n "$1" '.[] | select(.name==$n) | .id'; }

# Wait for text to appear in a session's pane. Every pre-play step syncs on a
# pane marker rather than a fixed sleep: several real CLIs boot concurrently
# here, so "long enough" is not knowable up front — a prompt sent before its
# agent is ready lands nowhere and the turn silently never renders.
wait_pane() { # <session-id> <marker> [tries]
    _w=0; _tries=${3:-120}
    while [ "$_w" -lt "$_tries" ]; do
        if "$CLI_BIN" --text session capture "$1" --lines 80 2>/dev/null \
            | grep -qF "$2"; then return 0; fi
        sleep 0.5; _w=$((_w + 1))
    done
    return 1
}

# The "ready for input" marker each agent's own TUI paints (verified against
# claude 2.1.209, codex 0.144.4, opencode 1.17.15). Same markers the e2e
# scenarios wait on — see scripts/dev/agent-e2e/scenarios/*/scenario.sh.
agent_ready_marker() {
    case "$1" in
        claude) printf '❯' ;;
        codex) printf '›' ;;
        opencode) printf 'Build ·' ;;
        *) printf '' ;;
    esac
}

preplay_conversations() {
pp_count=$(jq 'length' "$STUB_DIR/preplay.json")
p=0
while [ "$p" -lt "$pp_count" ]; do
    psession=$(jq -r ".[$p].session" "$STUB_DIR/preplay.json")
    pagent=$(jq -r ".[$p].agent" "$STUB_DIR/preplay.json")
    p=$((p + 1))
    have_agent "$pagent" || continue
    psid=$(sid "$psession")
    [ -n "$psid" ] || { echo "  (no session id for $psession — skipping pre-play)"; continue; }
    echo "==> Pre-playing $psession ($pagent)"
    ready=$(agent_ready_marker "$pagent")
    if [ -n "$ready" ] && ! wait_pane "$psid" "$ready"; then
        echo "  warning: $pagent never became ready in $psession — skipping" >&2
        continue
    fi
    tcount=$(jq ".[$((p - 1))].turns | length" "$STUB_DIR/preplay.json")
    t=0
    while [ "$t" -lt "$tcount" ]; do
        prompt=$(jq -r ".[$((p - 1))].turns[$t].prompt" "$STUB_DIR/preplay.json")
        marker=$(jq -r ".[$((p - 1))].turns[$t].marker" "$STUB_DIR/preplay.json")
        t=$((t + 1))
        "$CLI_BIN" session send "$psid" "$prompt" >/dev/null
        wait_pane "$psid" "$marker" \
            || echo "  warning: '$marker' never rendered in $psession" >&2
    done
done
}

# --- Pre-seed the scripted tasks + automation --------------------------------
# Seeded for every clip, not just `tasks`/`search`: the Automations pane sits in
# the left column of every frame, so an empty one would read as "this feature has
# nothing in it" in nine clips out of ten.
seed_tasks_and_automation() {
    echo "==> Seeding scripted tasks + an automation"
    tk_count=$(jq '.tasks | length' "$CONTENT")
    k=0
    while [ "$k" -lt "$tk_count" ]; do
        title=$(jq -r ".tasks[$k].title" "$CONTENT")
        status=$(jq -r ".tasks[$k].status // \"todo\"" "$CONTENT")
        desc=$(jq -r ".tasks[$k].description // \"\"" "$CONTENT")
        k=$((k + 1))
        if [ -n "$desc" ]; then
            "$CLI_BIN" task create --title "$title" --status "$status" \
                --description "$desc" >/dev/null 2>&1 || true
        else
            "$CLI_BIN" task create --title "$title" --status "$status" >/dev/null 2>&1 || true
        fi
    done
    au_name=$(jq -r '.automation.name' "$CONTENT")
    au_prompt=$(jq -r '.automation.prompt' "$CONTENT")
    "$CLI_BIN" automation create --name "$au_name" --trigger daily --time "09:00" \
        --repo "$DEMO_REPO" --prompt "$au_prompt" >/dev/null 2>&1 || true
}

# --- Record -----------------------------------------------------------------
# Each tape declares its own Output paths, so one tape == one gif+mp4 pair.
# Persist the TUI theme into the seeded db so the next launched TUI starts in it.
# No TUI is running between recordings, so this write is conflict-free.
set_theme() {
    sqlite3 "$DB_FILE" \
        "INSERT INTO metadata (key, value) VALUES ('active_theme', '$1') \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value"
}

# The terminal grid every clip is recorded at, and the font size it is rendered
# with. 175x42 at font-size 18 lands on ~1921x1082 — the 1920x1080 the media has
# always been, at ~the same column count VHS's ttyd produced, so the TUI lays
# itself out exactly as before.
DEMO_COLS=175
DEMO_ROWS=42
DEMO_FONT_SIZE=18
# Meslo LG S — pinned rather than left to agg's default list, whose first entry
# (JetBrains Mono) is rarely installed, so the clips silently fell back to
# whatever the machine happened to have (Menlo here) and the media's typography
# changed with the recording box. Set via --text-font-family, NOT --font-family:
# the latter bypasses agg's automatic fallbacks, which is where the symbol glyphs
# friring paints (❯ ◐ ⏺ ✻, box drawing) come from.
DEMO_FONT="Meslo LG S"
# Catppuccin Mocha (bg,fg + the 16 ANSI slots) — the palette the tapes' `Set
# Theme` asked VHS for. friring paints its own theme in truecolor on top; this
# is what the agent panes' default-coloured text lands on.
DEMO_PALETTE="1e1e2e,cdd6f4,45475a,f38ba8,a6e3a1,f9e2af,89b4fa,f5c2e7,94e2d5,bac2de,585b70,f38ba8,a6e3a1,f9e2af,89b4fa,f5c2e7,94e2d5,a6adc8"
# Sockets live in the sandbox's private TMUX_TMPDIR, so they can never collide
# with a real server: one hosts the TUI being filmed, one gives asciinema a tty.
DEMO_SOCKET="friring-demo"
CAST_SOCKET="friring-cast"

# record_tape <stem> — film one tape and render its gif+mp4.
#
# Recording captures the TUI's *terminal byte stream* (asciinema), not pixels,
# and renders it offline (agg). That is what makes the output independent of
# this machine: capture costs nothing, so every paint friring emits is kept with
# its true timestamp, and the render can take as long as it likes. Grabbing
# pixels off a live GUI instead — VHS's model — drops frames the moment the box
# can't rasterize fast enough and can catch a half-drawn screen, which is
# exactly what made the previous media play ~8x too fast and tear.
record_tape() {
    _tape="$1"
    _cast="$TBX_SANDBOX_ROOT/$_tape.cast"
    _gif="$REPO_ROOT/$(node "$SCRIPT_DIR/lib/drive-tape.mjs" "$SCRIPT_DIR/$_tape.tape" --print-outputs | grep '\.gif$')"
    _mp4="$REPO_ROOT/$(node "$SCRIPT_DIR/lib/drive-tape.mjs" "$SCRIPT_DIR/$_tape.tape" --print-outputs | grep '\.mp4$')"
    mkdir -p "$(dirname "$_gif")"

    # Boot the TUI off-camera. This is the tapes' `Hide … Show` preamble: the
    # recorder attaches to an already-running session, so the launch cannot be
    # on film anyway, and drive-tape.mjs skips that block.
    tmux -L "$DEMO_SOCKET" kill-server 2>/dev/null || true
    tmux -L "$DEMO_SOCKET" new-session -d -s demo -x "$DEMO_COLS" -y "$DEMO_ROWS" "$FRIRING_BIN"
    # No status bar: an attached client renders it, so it would be filmed.
    tmux -L "$DEMO_SOCKET" set -g status off
    # Fail closed if the TUI never paints: recording a session that isn't up
    # yet films a blank or half-drawn screen, and the run would still report
    # success and overwrite good media with it.
    _i=0
    while [ "$_i" -lt 300 ]; do
        tmux -L "$DEMO_SOCKET" capture-pane -p -t demo 2>/dev/null | grep -q "friring" && break
        sleep 0.1; _i=$((_i + 1))
    done
    if [ "$_i" -ge 300 ]; then
        echo "error: the TUI never painted for $_tape — refusing to record" >&2
        tmux -L "$DEMO_SOCKET" kill-server 2>/dev/null || true
        return 1
    fi

    # asciinema needs a real tty, which this script has no way to hand it, so it
    # runs inside its own tmux pane and records an attached client of the demo
    # session — i.e. exactly the bytes a real terminal would receive.
    tmux -L "$CAST_SOCKET" kill-server 2>/dev/null || true
    tmux -L "$CAST_SOCKET" new-session -d -s rec -x "$DEMO_COLS" -y "$DEMO_ROWS" \
        "asciinema rec '$_cast' --overwrite -f asciicast-v2 --command 'tmux -L $DEMO_SOCKET attach -t demo'"
    tmux -L "$CAST_SOCKET" set -g status off
    sleep 1   # let the attach paint its first full frame before the beats start

    node "$SCRIPT_DIR/lib/drive-tape.mjs" "$SCRIPT_DIR/$_tape.tape" \
        --socket "$DEMO_SOCKET" --session demo

    # The tape's closing Ctrl+Q quits the TUI, which ends the attach, which ends
    # asciinema and flushes the cast. Fail closed if it is still recording: only
    # asciinema's own exit flushes the tail of the cast, so killing it here
    # (because the TUI never quit — a tape whose last chord was swallowed by a
    # modal, say) would leave a truncated stream that still renders happily into
    # a clip missing its ending. `-s` below only catches an EMPTY cast.
    _i=0
    while tmux -L "$CAST_SOCKET" has-session -t rec 2>/dev/null && [ "$_i" -lt 40 ]; do
        sleep 0.25; _i=$((_i + 1))
    done
    _still_recording=0
    if tmux -L "$CAST_SOCKET" has-session -t rec 2>/dev/null; then
        _still_recording=1
        # Whatever is on screen is why the tape could not quit — a modal still
        # open, a field still focused, a beat that landed somewhere unintended.
        # Keep it: without the pane this failure is just an assertion, and the
        # session is about to be killed.
        _dump="$REPO_ROOT/target/demo-failed-$_tape.txt"
        mkdir -p "$(dirname "$_dump")"
        tmux -L "$DEMO_SOCKET" capture-pane -p -t demo > "$_dump" 2>/dev/null || true
    fi
    tmux -L "$DEMO_SOCKET" kill-server 2>/dev/null || true
    tmux -L "$CAST_SOCKET" kill-server 2>/dev/null || true
    if [ "$_still_recording" = "1" ]; then
        echo "error: $_tape never quit the TUI; the cast is truncated" >&2
        echo "  the screen it was stuck on: $_dump" >&2
        return 1
    fi
    [ -s "$_cast" ] || { echo "error: no cast recorded for $_tape" >&2; return 1; }

    # Render beside the real thing, not onto it: everything below can still
    # reject this clip, and a rejected take must not have already replaced good
    # media. Only a clip that passes gets moved into docs/media.
    _staged="$TBX_SANDBOX_ROOT/$_tape.gif"
    # --idle-time-limit is deliberately far above any beat in the tapes: agg
    # would otherwise silently compress the pauses the tapes exist to script.
    # shellcheck disable=SC2086 # DEMO_FONT_DIRS is a pre-split flag list
    agg "$_cast" "$_staged" --font-size "$DEMO_FONT_SIZE" --fps-cap 30 \
        --idle-time-limit 30 --last-frame-duration 1 --theme "$DEMO_PALETTE" \
        --text-font-family "$DEMO_FONT" $DEMO_FONT_DIRS \
        >/dev/null 2>&1 || { echo "error: agg failed for $_tape" >&2; return 1; }

    # Does the clip actually run for as long as the tape says? Recording drives
    # real processes in real time, so it can stall (a wedged machine keeps the
    # TUI repainting identical frames, which agg merges into one very long
    # frame — `--idle-time-limit` never fires, because those are events, not
    # idle) or come up short (a truncated cast). Both produce media that render
    # fine and are silently wrong, which is how the previous recorder shipped a
    # 15s demo as a 1.08s one. Compare and refuse.
    _want=$(node "$SCRIPT_DIR/lib/drive-tape.mjs" "$SCRIPT_DIR/$_tape.tape" --print-duration)
    _got=$(node -e '
        const fs = require("fs");
        const b = fs.readFileSync(process.argv[1]);
        let cs = 0;
        for (let i = 0; i < b.length - 8; i++)
            if (b[i] === 0x21 && b[i + 1] === 0xf9 && b[i + 2] === 0x04)
                cs += b[i + 4] | (b[i + 5] << 8);
        console.log((cs / 100).toFixed(2));
    ' "$_staged")
    if ! node -e '
        const [want, got] = [Number(process.argv[1]), Number(process.argv[2])];
        // Generous: the clip legitimately carries the closing hold plus a beat
        // of boot/attach settle on top of the script.
        process.exit(got > want + 20 || got < want * 0.6 ? 1 : 0);
    ' "$_want" "$_got"; then
        echo "error: $_tape rendered ${_got}s but its tape scripts ${_want}s — refusing" >&2
        echo "  (a stall under load, usually; the existing clip is left alone)" >&2
        return 1
    fi
    mv "$_staged" "$_gif"

    # gif -> mp4. ffmpeg reads the gif's per-frame delays as timestamps, so
    # `fps=30` re-times to a constant rate for players that need one WITHOUT
    # changing the duration. The gif itself keeps its variable delays — never
    # re-encode it, that is where the exact pacing lives.
    ffmpeg -y -loglevel error -i "$_gif" -movflags +faststart -pix_fmt yuv420p \
        -vf "fps=30,scale=trunc(iw/2)*2:trunc(ih/2)*2" "$_mp4" \
        || { echo "error: ffmpeg failed for $_tape" >&2; return 1; }
}

check_demo_font

# Keep going after a refused clip rather than abandoning the batch: recording
# drives real processes in real time, so a clip can lose to a stall on a busy
# machine, and that take is simply rejected (its existing media is untouched).
# Failing the whole run on the first one would throw away the other nine
# recordings — each of which costs a full re-seed — for a retryable fault.
_failed=
for tape in $TAPES; do
    echo "==> Seeding demo state for $tape.tape ..."
    seed_demo_state
    echo "==> Recording $tape.tape (theme: $DEMO_THEME) ..."
    record_tape "$tape" || _failed="$_failed $tape"
done

echo "==> Done. Updated docs/media/ for tape(s):$([ "$TAPES" = "$ALL_TAPES" ] && echo " all" || echo " $TAPES")"
for tape in $TAPES; do
    case " $_failed " in *" $tape "*) continue ;; esac
    case "$tape" in
        agents)      echo "    friring-demo.{gif,mp4}" ;;
        automations) echo "    automations-demo.{gif,mp4}" ;;
        tasks)       echo "    tasks-demo.{gif,mp4}" ;;
        search)      echo "    search-demo.{gif,mp4}" ;;
        code-review) echo "    code-review-demo.{gif,mp4}" ;;
        *)           echo "    friring-$tape.{gif,mp4}" ;;
    esac
done

if [ -n "$_failed" ]; then
    echo "error: refused (their existing media is untouched — re-run them):$_failed" >&2
    exit 1
fi
