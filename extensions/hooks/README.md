# hooks — agent lifecycle → friring session status

The **hooks** extension wires each coding agent's lifecycle hooks to
`friring-cli session signal` so every session reports its state back to friring
and the sidebar shows, at a glance, which agents are **blocked**, **working**,
or **done**:

| State | Colour | Meaning |
|-------|--------|---------|
| 🔴 blocked | red | the agent needs input or approval |
| 🟡 working | yellow | the agent is actively running |
| 🔵 done | blue | a turn just finished; shown until you switch away |
| 🟢 idle | green | acknowledged (you moved off it), or at rest |

Repo groups in the session list roll up to their most-urgent member, so the
whole list scans in one pass.

## It's on by default

Unlike other extensions, **hooks ships built into friring and is auto-activated**
on first run — the default agent's hook is pre-configured with zero setup. Opt
out at any time:

```bash
friring-cli extension deactivate hooks   # remove the wiring; won't come back
friring-cli extension activate hooks      # re-enable it
```

## How each agent is wired

The hook command is always `friring-cli session signal --state <working|blocked|done>`,
which identifies the calling session from the injected `$FRIRING_SESSION` (no ids
passed by hand) and is suffixed `|| true` so it can never break the agent.

- **claude** — a managed settings file (under the extension home) is passed via
  `--settings` (an `[[agent_patches]]` that appends the flag to the built-in
  `claude` agent, reversibly). claude merges it with your own settings, so your
  hooks are preserved. Events: `SessionStart` → idle (so a just-booted, idle
  session isn't shown as working), `UserPromptSubmit`/`PreToolUse` → working,
  `Stop` → done. `Notification` → blocked **only for permission/approval
  prompts** — claude also fires `Notification` for its "waiting for your input"
  idle nudge, which the hook ignores (parses the payload) so an idle session
  doesn't flip to red.
- **aider** — `--notifications-command` reports the only edge aider exposes:
  blocked (waiting for input).
- **opencode** — a plugin dropped into `~/.config/opencode/plugin/` (only when
  opencode is installed). Events: `session.created` → idle, `chat.message` →
  working, `permission.asked` → blocked, `session.idle` → done.
- **codex** — codex's `hooks.json` is claude-shaped, loaded from
  `~/.codex/hooks.json`. We **JSON-merge** our entries in (a
  `[[config_merges]]`, guarded by `requires_dir`) so your own hooks are
  preserved; uninstall prunes exactly ours back out. Only that default path is
  managed — the merge expands `~` from `$HOME`, never `CODEX_HOME`, so if you
  point `CODEX_HOME` elsewhere codex reads a different file and you have to wire
  the hooks there yourself. Events: `SessionStart` → idle, `UserPromptSubmit`/`PreToolUse` →
  working, `Stop` → done. **No blocked** — codex's top-level hooks have no
  permission/approval event (that lives only in the legacy `notify`). This
  replaced the old `-c notify=…` override (which only reported done); the trade
  is a reversible write into a separate `~/.codex/hooks.json`, never your
  `config.toml`. Two codex specifics (verified against codex-cli 0.145.0):
  - codex **parses every hook's stdout** and fails the hook on anything that
    isn't empty or JSON it accepts, so our commands discard their output
    (`>/dev/null 2>&1`). Without that, codex reports
    `hook returned invalid <event> JSON output` on every turn.
  - codex **gates hooks behind a trust prompt** keyed on the command string:
    the first codex launch after the extension is installed — or after the
    payload changes — parks on *"Hooks need review"*, and the hooks stay inert
    until you accept them. That approval is yours to give; friring can't
    pre-seed it. (`codex --dangerously-bypass-hook-trust` skips the gate for
    automation, which is how the e2e suite runs.)
- **vibe** *(experimental)* — Mistral Vibe loads hooks from `~/.vibe/hooks.toml`.
  It's TOML, so we can't JSON-merge it — we drop a managed file in (an
  `[[external_files]]`, guarded by `requires_dir`, only when vibe is installed).
  Events: `before_tool` → working, `after_turn` → done, `notification` (awaiting
  approval / question) → blocked. **Caveats:** the `hooks.toml` schema is not
  verified against the live `vibe` binary — if event/key names differ, edit
  `vibe-hooks.toml` (no code change). And if you already maintain your own
  `~/.vibe/hooks.toml`, the write is **refused** (no managed marker) so it's never
  clobbered — vibe simply goes unreported rather than broken.
- **copilot** *(experimental)* — GitHub Copilot CLI (the `copilot` command) loads
  hooks from its own dir, `~/.copilot/hooks/*.json`. We drop a managed standalone
  file in (an `[[external_files]]`, guarded by `requires_dir`, only when copilot is
  installed), so your other hook files are never touched. Events (copilot's own
  schema): `sessionStart` → idle, `userPromptSubmitted`/`preToolUse` → working,
  `agentStop` → done, and `notification` matched to `permission_prompt` → blocked
  (so an `agent_idle`/`shell_completed` notification doesn't flip the dot red).
  Both `bash` and `powershell` commands are shipped, so status works on Windows
  too. **Caveat:** if a future `copilot` changes the hook schema, edit
  `copilot-hooks.json` (no code change).
- **antigravity** — antigravity (the `agy` CLI, the Gemini CLI successor) loads
  hooks only from its shared `~/.gemini/settings.json`, so we **JSON-merge** our
  entries in (a `[[config_merges]]`, guarded by `requires_dir`) without clobbering
  your settings; uninstall prunes exactly ours back out. `agy` adopted Claude
  Code's hook schema (verified against agy 1.0.9), so the mapping mirrors claude:
  `SessionStart` → idle, `PreToolUse` → working, `Stop` → done, and `Notification`
  → blocked **only for permission/approval prompts** (the payload is parsed, same
  as claude, so an idle `Notification` doesn't flip the dot red). It has no
  `UserPromptSubmit`, so working is signaled at the first tool call rather than on
  prompt submit. **Caveat:** if agy sanitizes the hook environment,
  `$FRIRING_SESSION` may not reach the hook, in which case the signal is a
  fail-open no-op. If a future `agy` changes the hook schema, edit
  `antigravity-hooks.json` (no code change).

## Custom agents (`hook_schema`)

The wiring above is keyed to the built-in agent **names**, so a **custom** agent
you add to `agents.toml` (e.g. a rebranded-claude `fleet`) normally gets no
hooks — its status dot stays driven only by the agent-neutral fallbacks (working
inferred from output, done from output quiescence). To opt a custom agent into a
known family, set `hook_schema` on its `[[agents]]` entry:

```toml
[[agents]]
name = "fleet"
command = "fleet"        # runs claude under the hood
hook_schema = "claude"   # ⇒ inherit claude's --settings hook wiring
```

friring then applies the `claude` `[[agent_patches]]` to `fleet` as well, so it
reports working/blocked/done exactly like `claude` (locally and on a remote/WSL
host). `hook_schema` names the *family* to imitate; today `"claude"` is the
useful value — it's the family wired via a per-agent arg patch. The
config-dir-wired families (codex/opencode/antigravity/vibe/copilot) don't need
it: a rebrand that runs the same CLI reads the same `~/.<agent>/…` hook file and
already reports.

## Where the config lives

The wiring is applied **only to agents friring launches** — it never edits your
own global agent config (e.g. your personal `~/.claude/settings.json`). For
claude the managed hooks file is passed with `--settings`, which claude **merges
on top of** your own settings: inside a friring session both your hooks and
friring's fire, while a plain `claude` outside friring sees only your own. The
other agents are wired by a reversible merge into — or a managed file dropped in
— their own config dir.

| Agent | On-disk location | How it's applied |
|-------|------------------|------------------|
| claude | `~/.config/friring/hooks/claude.json` | `--settings` flag on the `claude` agent (claude merges it) |
| aider | — (no file) | `--notifications-command` flag on the `aider` agent |
| opencode | `~/.config/opencode/plugin/friring-status.js` | managed plugin file (`requires_dir`) |
| codex | `~/.codex/hooks.json` | reversible JSON-merge of our entries |
| vibe | `~/.vibe/hooks.toml` | managed file (refused if you already have one) |
| copilot | `~/.copilot/hooks/friring-status.json` | managed standalone file (`requires_dir`) |
| antigravity | `~/.gemini/settings.json` | reversible JSON-merge of our entries |

The home dir is `~/.config/friring/hooks` for a release build and
`~/.config/friring-dev/hooks` for a dev build, so the two stay isolated.

**Inspect or customize.** To see exactly what friring installed, read the file
for the agent above (e.g. `cat ~/.config/friring/hooks/claude.json`). The
injected `--settings` / `--notifications-command` flags themselves live in the
`claude` / `aider` entries of `~/.config/friring/agents.toml`. You can hand-edit
a managed file, but self-heal rewrites it from the embedded payload on the next
TUI start / heartbeat tick — so to keep a change, either deactivate the extension
(`friring-cli extension deactivate hooks`) and wire the hook yourself, or edit
the payload source under `extensions/hooks/` and reinstall.

## Mechanism

This extension exercises two extension-manifest capabilities (see
`src/session/extension_def.rs`):

- `[[agent_patches]]` — append args to an **existing** agent in `agents.toml`
  (reversible; uninstall removes exactly the injected subsequence).
- `[[external_files]]` — place a file into an agent's **own** config dir
  (outside the extension home), guarded by `requires_dir`.
- `[[config_merges]]` — **reversibly deep-merge** shipped JSON into an agent's
  own *shared* config file (antigravity's `~/.gemini/settings.json`) without clobbering the
  user's other settings: objects recurse, arrays union, and uninstall prunes
  exactly the entries we shipped (matched by the `session signal` marker, so it
  stays correct across payload changes). Guarded by `requires_dir`; no-op when
  the merge is already present. A merge whose target is malformed JSON is
  soft-skipped (logged, never aborts the rest of the install).

  Note: on the **first** merge, friring rewrites `settings.json` with normalized
  formatting (alphabetized keys, 2-space indent). This is one-time and lossless —
  your values are untouched and the file is stable afterward.
