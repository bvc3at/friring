# shellcheck shell=bash
#
# Scenario: `activity_provider` in agents.toml — declaring which transcript
# format a custom command writes, end to end, with no real agent binary.
#
# The agent is `ringwriter` (agents/ringwriter/profile.sh): a bash script under
# a basename no provider inference can resolve, which replays a fixture
# transcript in the claude-code format into $CLAUDE_CONFIG_DIR and signals its
# own lifecycle. Nothing about it is recognizable — so every tool event this
# scenario reads back is attributable to the one declaration in its registry
# entry, and to nothing else.
#
# Five things are asserted, each with a real observation point:
#
#   1. Precedence + the shared resolution. F9 and `friring-cli session
#      activity` both report `claude-code` for the declared entry, with the
#      transcript's commands / edits / reads / prompts / tokens / files.
#   2. Cache invalidation on reload. Flipping the declaration to `codex` live
#      must repoint the view — a stale claude accumulator surviving the flip
#      would keep rendering the events it had already parsed. Flipping back
#      rebuilds them from the transcript.
#   3. Invalid values. A bogus provider on a FOURTH entry fails `config
#      validate` (naming the ids that would have worked) while its siblings
#      stay in effect — the loader skips only the bad entry.
#   4. Backward compatibility. A sibling with no `activity_provider` whose
#      command basename IS `claude` (a symlink to the same script) resolves by
#      inference exactly as before the field existed.
#   5. Independence from `hook_schema`. A sibling carrying `hook_schema` and no
#      `activity_provider` resolves NO provider — while `ringwriter`, which
#      carries the reverse, reports activity and signals its own hook state.
#      Neither field implies the other.
#
# Test-mode only (CLI probes and mid-run config rewrites); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="agents.toml activity_provider: the declared format beats the basename, in F9 and the CLI alike"
SCENARIO_AGENT="ringwriter"

# Where the harness put the registry this scenario keeps rewriting.
CAP_AGENTS_TOML=""
# The undeclaring siblings' sessions, created in phase 4.
CAP_BARE_SESSION=""
CAP_LEGACY_SESSION=""

scenario_setup() {
    CAP_AGENTS_TOML="$XDG_CONFIG_HOME/friring-dev/agents.toml"
}

# Rewrite the registry with `ringwriter`'s provider set to $1, the entry
# otherwise identical to the profile's. Used to flip the declaration live.
cap_write_registry() {
    local provider="$1"
    mkdir -p "$(dirname "$CAP_AGENTS_TOML")"
    cat > "$CAP_AGENTS_TOML" <<EOF
default = "ringwriter"

[[agents]]
name = "ringwriter"
command = "$HOME/ringwriter"
new_session_args = ["new", "{id}", "{name}"]
resume_args = ["resume", "{id}"]
fork_args = ["fork", "{id}", "{name}"]
resume_latest = true
activity_provider = "$provider"
EOF
}

# The four-entry registry of phases 3-5: the declaring entry, the two siblings
# that declare nothing, and one entry whose provider is not a provider.
cap_write_full_registry() {
    cap_write_registry claude-code
    cat >> "$CAP_AGENTS_TOML" <<EOF

# Declares nothing about transcripts, and carries the OTHER optional family
# field: hook wiring must not imply a transcript format.
[[agents]]
name = "ringwriter-bare"
command = "$HOME/ringwriter"
new_session_args = ["new", "{id}", "{name}"]
resume_args = ["resume", "{id}"]
resume_latest = true
hook_schema = "claude"

# Declares nothing either, but its command basename is one friring knows —
# the inference every agents.toml relied on before this field existed.
[[agents]]
name = "ringwriter-legacy"
command = "$HOME/claude"
new_session_args = ["new", "{id}", "{name}"]
resume_args = ["resume", "{id}"]
resume_latest = true

# Not a provider id. Diagnosed, skipped, and harmless to the three above.
[[agents]]
name = "ringwriter-broken"
command = "$HOME/ringwriter"
activity_provider = "ringwriter-format"
EOF
}

# Bounded poll of a session's headless activity report against a jq predicate.
# The TUI's scan runs on a ~1s cadence and this command re-scans from scratch,
# so a probe fired right after a config rewrite can still precede the change.
cap_wait_activity() {
    local session="$1" filter="$2" tries="${3:-40}" out=""
    for _ in $(seq 1 "$tries"); do
        out="$(friring-cli --json session activity "$session" 2>/dev/null)"
        printf '%s' "$out" | jq -e "$filter" >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    e2e_die "session activity never satisfied '$filter'
--- last report ---
$out"
}

# Headless `session create` for a sibling entry, printing the new session id.
# 3>&-: the same bats fd-3 guard the harness's own create uses.
cap_create_session() {
    local agent="$1" name="$2" out id
    out="$(friring-cli --json session create --name "$name" \
        --repo-path "$E2E_WS" --agent "$agent" 3>&-)" \
        || e2e_die "session create --agent $agent failed: $out" || return 1
    id="$(printf '%s' "$out" | jq -r '.id')"
    [ -n "$id" ] && [ "$id" != "null" ] \
        || e2e_die "no session id for $agent in: $out" || return 1
    printf '%s' "$id"
}

# Every step here carries `|| return 1` on purpose: errexit is off inside a
# scenario body, so a wait that times out returns 1 into a flat list that checks
# nothing — and a scenario whose every wait expired would still report green.
# The TUI half of this feature is exactly what those waits assert, so it has to
# fail closed.
scenario_steps() {
    # ── 1. The declared provider drives both surfaces ────────────────────────
    step_wait_pane "RINGWRITER-READY mode=new" 60 || return 1
    # The script wrote its transcript where the claude provider looks, under
    # the CLAUDE_CONFIG_DIR the profile relocated.
    step_wait_pane "RINGWRITER-TRANSCRIPT" 30 || return 1

    step_key F9 || return 1
    step_wait_pane " Activity · Overview " 20 || return 1
    # The identity line is `<agent> · <provider-id>`. `claude-code` here can
    # only have come from the entry: `ringwriter` infers nothing.
    step_wait_pane "ringwriter · claude-code" 30 || return 1
    step_wait_pane "1 edits" 20 || return 1
    step_wait_pane "Title: Clamp the southern capture ring" 20 || return 1
    # Timeline (2): the Write lands as one edit row on the file it named.
    step_type "2" || return 1
    step_wait_pane "edit.*drift.md" 20 || return 1

    # ── 2. A live provider change invalidates what was accumulated ───────────
    # `codex` is a real provider with no records here, so the flip shows twice
    # over: the identity repoints AND the claude events stop being served. A
    # stale accumulator would keep rendering the ones it had already parsed.
    cap_write_registry codex || return 1
    step_type "1" || return 1
    step_wait_pane "ringwriter · codex" 30 || return 1
    cap_wait_activity "$E2E_SESSION_ID" \
        '.provider == "codex" and (.counts.edits == 0)' || return 1

    # Flipping back rebuilds from the claude sources: the accumulator the first
    # flip dropped is gone, so this is a fresh parse of the same transcript.
    # The identity line is singular, so having watched it read `codex` and then
    # `claude-code` IS the transition — no separate negative check is possible
    # or needed.
    cap_write_registry claude-code || return 1
    step_wait_pane "ringwriter · claude-code" 30 || return 1
    step_wait_pane "1 edits" 20 || return 1
    step_key Escape || return 1
    step_wait_pane "RINGWRITER-READY" 20 || return 1

    # ── 3. An invalid value is diagnosed without breaking its siblings ───────
    # The reload toast carries the loader's own warning, so the TUI names the
    # bad entry rather than silently dropping it.
    cap_write_full_registry || return 1
    step_wait_pane "skipped agent" 20 || return 1

    # ── 4. The two undeclaring siblings, each with its own session ───────────
    # Same executable, same on-disk records; only the registry entry differs.
    ln -sf "$HOME/ringwriter" "$HOME/claude" || return 1
    CAP_BARE_SESSION="$(cap_create_session ringwriter-bare cap-bare)" || return 1
    CAP_LEGACY_SESSION="$(cap_create_session ringwriter-legacy cap-legacy)" || return 1
    step_wait_pane "cap-legacy" 30 || return 1
}

scenario_assert_effects() {
    # The transcript the whole reconstruction reads, under the id friring
    # minted and inside the relocated state dir — both halves of "a declared
    # provider still honours its CLI's state-dir override".
    local aid
    aid="$(friring-cli --json session get "$E2E_SESSION_ID" | jq -r '.agent_session_id')"
    [ -n "$aid" ] && [ "$aid" != "null" ] \
        || e2e_die "session has no agent_session_id" || return 1
    ls "$HOME/ringwriter-state/projects/"*/"$aid.jsonl" >/dev/null 2>&1 \
        || e2e_die "no transcript under CLAUDE_CONFIG_DIR for $aid" || return 1
    [ ! -d "$HOME/.claude/projects" ] \
        || e2e_die "the state-dir override was ignored: ~/.claude/projects exists" || return 1
    assert_ws_file_eq drift.md "capture ring clamped to 92%"

    # --- the declaring entry, headlessly ----------------------------------
    # The same provider the TUI showed, from the same resolution — and the
    # whole fixture turn, so the parser really ran over these records.
    local act
    act="$(friring-cli --json session activity "$E2E_SESSION_ID")" \
        || e2e_die "session activity failed" || return 1
    [ "$(printf '%s' "$act" | jq -r '.provider')" = "claude-code" ] \
        || e2e_die "declared provider not used headlessly: $act" || return 1
    printf '%s' "$act" | jq -e '.counts.commands == 1 and .counts.edits == 1
        and .counts.reads == 1 and .counts.prompts == 1 and .counts.failed == 0' \
        >/dev/null || e2e_die "activity counts do not match the fixture: $act" || return 1
    # Token tallies are summed from the transcript's own usage records.
    printf '%s' "$act" | jq -e '.tokens.output == 400 and .tokens.input == 1350
        and .tokens.cache_read == 800 and .tokens.cache_write == 64' \
        >/dev/null || e2e_die "activity tokens do not match the fixture: $act" || return 1
    printf '%s' "$act" | jq -e '[.files[].path] | any(endswith("drift.md"))' \
        >/dev/null || e2e_die "activity did not aggregate the edited file: $act" || return 1
    printf '%s' "$act" | jq -e '.title | startswith("Clamp the southern capture ring")' \
        >/dev/null || e2e_die "activity title not derived from the transcript: $act" || return 1

    # The script's own `session signal` calls landed: this agent has activity
    # reporting AND lifecycle state with no hooks-extension wiring at all.
    cap_wait_hook_state "done" || return 1

    # --- the siblings that declare nothing --------------------------------
    # No `activity_provider`, an unrecognizable basename, `hook_schema` set:
    # unmeasured, and the note names the command it could not place. This is
    # the before-picture of the entry above, and the proof that hook wiring
    # implies nothing about transcripts.
    cap_wait_activity "$CAP_BARE_SESSION" \
        '.provider == null and (.note | test("no activity provider"))'
    local bare
    bare="$(friring-cli --json session activity "$CAP_BARE_SESSION")"
    printf '%s' "$bare" | jq -e '.counts == null and .tokens == null' >/dev/null \
        || e2e_die "an unmeasured row must null every key: $bare" || return 1

    # No `activity_provider` either, but a basename friring knows: inference,
    # unchanged. Every agents.toml written before the field keeps working.
    cap_wait_activity "$CAP_LEGACY_SESSION" \
        '.provider == "claude-code" and .counts.edits == 1'

    # --- the invalid entry ------------------------------------------------
    # `config validate` fails the file and names what could have been written.
    local report status=0
    report="$(friring-cli --json config validate 2>/dev/null)" || status=$?
    [ "$status" -ne 0 ] \
        || e2e_die "config validate passed with an invalid activity_provider" || return 1
    printf '%s' "$report" | jq -e '.valid == false' >/dev/null \
        || e2e_die "config validate did not fail the registry: $report" || return 1
    printf '%s' "$report" | jq -e '[.agents_toml.problems[]]
        | any(contains("claude-code"))' >/dev/null \
        || e2e_die "the diagnostic does not list the valid ids: $report" || return 1

    # …and the load skipped only that entry: its three siblings are still
    # registered, and the one that matters still reports.
    local names
    names="$(friring-cli --json config show | jq -c '.agents.names')"
    printf '%s' "$names" | jq -e 'index("ringwriter") != null
        and index("ringwriter-bare") != null
        and index("ringwriter-legacy") != null
        and index("ringwriter-broken") == null' >/dev/null \
        || e2e_die "the invalid entry broke its siblings: $names" || return 1
    [ "$(friring-cli --json session activity "$E2E_SESSION_ID" | jq -r '.provider')" \
        = "claude-code" ] \
        || e2e_die "a broken sibling entry disturbed the declaring one" || return 1
}

# The agent signals its own lifecycle (no hooks extension wires it), so this
# polls rather than using step_wait_state — which the harness gates on
# AGENT_HAS_STATUS_HOOKS, and this profile truthfully declares 0.
cap_wait_hook_state() {
    local want="$1"
    for _ in $(seq 1 60); do
        [ "$(e2e_hook_state)" = "$want" ] && return 0
        sleep 0.2
    done
    e2e_die "hook_state '$(e2e_hook_state)' != $want (the agent's own signal never landed)"
}

scenario_assert_ui() {
    # Esc closed the view: its content must be gone. Polled — a repaint may
    # lag the keypress by a frame.
    for _ in $(seq 1 50); do
        e2e_pane | grep -q "Activity · Overview" || return 0
        sleep 0.1
    done
    e2e_die "activity view still open after Esc
--- pane ---
$(e2e_pane)"
}
