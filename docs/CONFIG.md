# Configuration Reference

Every knob friring reads, where it lives, and how it behaves. One file
per audience/lifecycle: hand-edited registries are TOML, the
machine-written keybindings are JSON, and concurrently-written runtime
state lives in SQLite (see ADR-8/ADR-19 in `ARCHITECTURE.md` for the
rationale).

Dev builds (version `0.0.0-dev`) use `friring-dev` in place of
`friring` in every path below, plus a `friring-dev` tmux socket, so a
development checkout never touches your real setup.

## Files at a glance

| File | Format | Edited by | Read | Purpose |
|------|--------|-----------|------|---------|
| `~/.config/friring/agents.toml` | TOML | you | **live** (mtime poll) | coding-agent CLI definitions |
| `~/.config/friring/hosts.toml` | TOML | you | startup | remote SSH hosts + local WSL distros |
| `~/.config/friring/settings.toml` | TOML | you + `Ctrl+,` panel | **live** (feature flags, `[prefix]`) / startup (rest) | tuning knobs + feature flags |
| `~/.config/friring/themes.toml` | TOML | you | startup | custom theme palettes |
| `~/.config/friring/keybindings.json` | JSON | F1 editor (or you) | **live** (mtime poll) | key chord overrides |
| `~/.config/friring/extensions/<name>.toml` | TOML | `friring-cli extension install` | startup + tick | extension manifests (self-healed resources) |
| `~/.local/share/friring/friring.db` | SQLite | friring | live | sessions, automations, tasks, theme, editor command |
| `~/.local/share/friring/friring.log` | text | friring | — | logs (incl. config warnings) |

`agents.toml`, `keybindings.json`, and `settings.toml` reload **live**:
the TUI polls their mtime (~1/s) and applies edits with a confirmation
toast — no restart. For `settings.toml` only the **feature flags that
gate UI panels** (`tasks`, `file_viewer`, `info_panel`, `global_search`,
`shell_pane`, `code_review`, `cc_activity`, `session_memory`, `perf_hud`,
`soft_delete`),
`info_panel_position`, and the [`[prefix]`](#prefix--the-leader-key)
leader-key table
apply live; the restart-only values stay published through a write-once
global (so they can't drift mid-frame),
and the reload toast says when a restart is needed. `hosts.toml` (SSH
backends register at startup) and `themes.toml` need a restart.

`settings.toml` can also be edited from the TUI: **`Ctrl+,`** (alt `F6`)
opens a **Settings panel** listing every knob. It writes the file back
**preserving its comments**, and feature flags that gate UI panels (plus
`info_panel_position` and `[prefix]`) apply **live** on save; the rest (`mouse`,
`notifications`, `automations`, `version_check`, `auto_update`, the four
editable `[notifications]` knobs, and the numeric scalars) take effect on
the next launch — the panel marks those rows
with `⟳` and toasts a restart note. The panel exposes only the four
editable notification knobs (`also_on_waiting`, `suppress_for_active`,
`sound`, `min_interval_secs`); `[notifications] backend` and the
`[prefix]` leader-key fields are **not** panel rows — set those only by
hand-editing `settings.toml` (they still apply live). Hand-editing
the file (or the panel in another instance) is picked up the same way,
via the live mtime poll.

All paths respect `$XDG_CONFIG_HOME` / `$XDG_DATA_HOME`.

### Which file do I edit?

A task-to-file map so you don't have to scan every section to find
the right knob:

| I want to… | Edit | Section |
|------------|------|---------|
| Add a coding agent, pin a model, change resume/fork flags | `agents.toml` | [agents.toml](#agentstoml) |
| Run sessions on a remote machine over SSH, or in a local WSL distro | `hosts.toml` | [hosts.toml](#hoststoml) |
| Turn a whole TUI feature on/off (tasks, mouse, notifications…) | `settings.toml` `[features]` | [`[features]`](#features--whole-feature-switches) |
| Tune scrollback, panel breakpoints, audit retention | `settings.toml` | [settings.toml](#settingstoml) |
| Change when/how OS notifications fire | `settings.toml` `[notifications]` | [`[notifications]`](#notifications--os-notification-settings) |
| Add or recolour a TUI theme | `themes.toml` | [themes.toml](#themestoml) |
| Rebind a key | `keybindings.json` (or the F1 editor) | [keybindings.json](#keybindingsjson) |
| Set the `Ctrl+O` editor, pick a theme | (runtime — SQLite) | [SQLite-backed settings](#sqlite-backed-settings) |

None of these files need to exist on a fresh install — every one is
seeded (commented-out where applicable) on first run, and absent files
fall back to built-in defaults.

Config problems are **not silent**: parse errors, unknown fields,
invalid chords, and chord conflicts surface as a status-bar toast on
startup (and in the log file). Unknown TOML keys are tolerated —
stale keys from older versions or typos are *reported by name* but
your file still loads — while syntax/type errors fall back to
built-ins (agents), zero hosts, or defaults (settings).

`agents.toml` degrades **per entry**: a single malformed `[[agents]]`
block (e.g. `args` given a string instead of an array) is skipped with
a toast naming it, and your remaining agents still load — only a
document-level syntax error (or a file with no usable agents) falls
back to the built-ins.

Check everything from the command line:

```bash
friring-cli config validate   # strict parse of every file; exit 1 on problems
friring-cli config show       # effective config + where each value came from
```

`validate` fails on unknown keys (they are typos or leftovers either
way), making it usable as a dotfiles CI gate.

## agents.toml

Declares the launchable coding agents. Seeded with the built-ins
(`claude`, `codex`, `antigravity`, `opencode`, `aider`, `copilot`, `vibe`) on
first run; edit or add `[[agents]]` entries to support any CLI — no
recompile. A malformed `[[agents]]` entry is skipped (with a toast
naming it) and the rest still load; only a document-level syntax error
falls back to the built-ins. Either way the error is shown.

```toml
config_version = 1
default = "claude"          # agent preselected in the picker / headless spawns

[[agents]]
name = "claude"             # display + lookup name (unique)
command = "claude"          # executable
args = []                   # always passed; bake a model here if you want one
resume_args = ["--resume", "{id}"]            # emitted when resuming
fork_args = ["--resume", "{id}", "--fork-session", "-n", "{name}"]
new_session_args = ["--session-id", "{id}", "-n", "{name}"]  # fresh spawn
resume_latest = false       # true = id-less "resume last session in cwd"
# hook_schema = "claude"    # optional: name the hook FAMILY this CLI speaks so
                            #   the built-in hooks extension wires its status
                            #   hooks under this custom agent's name too

[agents.sandbox]            # optional: what this CLI needs inside a sandbox
auth = "auto"               # auto | host-passthrough | env-token | volume-login
                            #   | seed-file. A request, not a verdict: a policy
                            #   sandbox is always host-passthrough, and a
                            #   container never can be
config_dir_env = "CLAUDE_CONFIG_DIR"  # env var relocating its state (place backends)
state_dir = "~/.claude"     # the directory that variable names, `~`-anchored
state_rw = ["~/.claude", "~/.claude.json"]  # dirs it writes and must keep
bypass = ["--dangerously-skip-permissions"]  # flags turning its OWN sandbox off
copy_in = ["~/.claude/skills"]   # config safe to project into a container,
                            #   `~`-anchored: it lands at the same home-relative
                            #   path inside. Linted first — a hook or MCP server
                            #   naming a host path is dropped, and a credential
                            #   never crosses (ADR-28)
# env = { DISABLE_AUTOUPDATER = "1" }  # static env while a sandbox is active
# secret_env = ["ANTHROPIC_API_KEY"]   # token variable NAMES only; the value
                            #   lives in your OS keychain under the service
                            #   "dev.friring.sandbox", never here
# credential_file = "~/.claude/.credentials.json"  # the vendor credential file
# seed_file_supported = false  # true ONLY where the vendor documents copying it
# writeback = true          # a refreshed credential must survive the sandbox
# login_fallback = "/login" # what to type in the pane when the state is empty

[[agents.sandbox.enforced]]  # friring's highest-precedence layer inside a container
path = "~/.codex/config.toml"  # `~`-anchored: a container's only writable
                               #   surface is its own synthetic home
format = "toml"                # json | toml
per_path = "[projects.\"{path}\"]\ntrust_level = \"trusted\""
# `content` is written once ({workspaces} → the granted paths as a list);
# `per_path` is repeated once per granted path ({path} → that path). JSON takes
# `content` only, because two JSON documents cannot be concatenated.
```

`{id}` is substituted with the friring-generated session UUID. Groups
are emitted only when their driving value exists; precedence is
fork > resume > new-session. `args` is always passed. **No model is
ever passed** — each agent uses its own default config, so bake
`["--model", "opus"]` into `args` to pin one. A session stores only its
**agent name**; there are no per-session model/permission/prompt/tool
knobs. An agent that omits `resume_args` starts fresh on restart; the
live tmux process is what carries its state across TUI restarts.

`{name}` is substituted with the **friring session name**, for agents
whose CLI can name a session at launch: the seeded claude entry passes
`-n {name}` when a conversation is *created* (fresh spawn or fork), so
it shows up under the same name in claude's own `/resume` picker. The
resume group deliberately omits `{name}` — a restart never renames a
conversation the agent already owns (e.g. after an in-agent `/rename`).
A launch without a name drops a `{name}` token together with its
preceding flag, so the pair vanishes cleanly; agents with no naming
flag simply don't reference `{name}`. An `agents.toml` seeded before
`{name}` existed keeps working — add `-n {name}` to your claude entry
to opt in.

**Session-id pinning vs. `resume_latest`.** friring generates the
`agent_session_id` (a UUID), but only `claude` accepts it at creation
(`--session-id {id}`), so only claude can resume or fork *by that exact
id*. The other built-ins can't pin or report their id, so they set
`resume_latest = true` with **id-less** resume/fork flags (no `{id}`
token) and let the CLI resolve "the last session in *this* directory"
itself:

| Agent | `resume_latest` | resume flag |
|-------|-----------------|-------------|
| `claude` | `false` | `--resume {id}` (pins the exact id) |
| `codex` | `true` | `resume --last` |
| `opencode` | `true` | `--continue` |
| `antigravity` | `true` | `--continue` (`agy`) |
| `aider` | `true` | `--restore-chat-history` |
| `copilot` | `true` | `--continue` |

This works because restart reuses the session's cwd and a single-repo
fork reuses the parent's cwd. `resume_latest` only changes *when* the
resume group fires (`session_ops::resume_trigger_for`): for the id-less
agents restart always triggers resume, while claude still defers to an
on-disk transcript check. Caveats: an agent with no `fork_args`
(`antigravity`, `aider`, `copilot` — none of these CLIs fork) starts
fresh on `Ctrl+F`; and a multi-repo fork of a cwd-scoped agent lands in a
fresh symlink workspace, so `--last`/`--continue` finds no parent session
(a multi-repo *restart* still resumes — it reuses the same workspace dir).

Internals: `session::AgentDef` / `session::AgentRegistry`
(`session/agent_def.rs`, pure data + the arg-substitution logic) are the
data types; `agent::agent_config::load_or_seed()` reads/seeds the TOML
with `builtin_registry()` as the fallback; `agent::GenericProvider` wraps
an `AgentDef` and implements the `AgentProvider` trait (`command()` +
`build_args(&SessionConfig)`), picked per session by
`App::provider_for(&config)`.

`hook_schema` is optional. Custom agents are agent-neutral, so the built-in
**hooks** extension normally wires status hooks only for the built-ins it knows
by name. Set `hook_schema = "claude"` on a **rebranded** agent (one whose
`command` runs `claude` under a different `name`) and it inherits claude's hook
wiring — the `--settings` patch locally, and the same rewrite on a remote/WSL
host. It names the *family* to imitate, not a boolean; today the useful value is
`"claude"` (the only family wired via a per-agent arg patch — codex/opencode/
antigravity/vibe/copilot are wired through their own config dir, so a rebrand
sharing that dir already reports status).

`[agents.<name>.sandbox]` is optional and every field inside it is too. It is
how friring stays agent-neutral about sandboxing: the flags that turn an agent's
*own* sandbox off (nesting is denied outright under seatbelt), the state
directories it must keep writable, how it authenticates inside a boundary and
which of your configuration is safe to carry into a container are **your**
declaration, never code. It is applied only while a sandbox profile is active,
so an agent that declares nothing still launches — it just gets no help, which
the profile editor says rather than papering over. An `agents.toml` written
before sandboxing existed loads unchanged. Full semantics:
[`docs/SANDBOX.md`](SANDBOX.md) §Credentials, §Config projection and §Inner
agent sandboxes. Sandbox *profiles* themselves are UI-edited and live in SQLite,
not here.

The seeded file also ships two commented, copy-pasteable templates
below the built-ins — **Add your own agent** (every field annotated)
and **Pin a model** (a `claude-opus` variant baking `--model opus`
into `args`). Both stay commented, so a fresh install still resolves
to exactly the seven built-ins.

## hosts.toml

Declares off-local hosts — **remote SSH machines** and **local WSL
distros**. Each `[[hosts]]` entry registers a session backend named
`ssh:<name>` (default kind) or `wsl:<name>`. Seeded fully commented-out
(fresh installs are local-only for SSH). Malformed file → zero
configured hosts, error shown.

```toml
config_version = 1

# An SSH host (the default kind):
[[hosts]]
name = "devbox"               # backend id "ssh:devbox"; what --host expects
destination = "me@devbox"     # "user@host" or a ~/.ssh/config alias
ssh_opts = ["-o", "ControlMaster=auto", "-o", "ControlPersist=10m"]
socket = "friring"            # host `tmux -L` socket   (default "friring")
session = "friring"           # host tmux session name  (default "friring")
worktrees_dir = "/home/me/.local/share/friring/worktrees"  # abs; optional
multiplexer = "tmux"          # "psmux" for a Windows SSH host

# A WSL distro (only to OVERRIDE auto-discovery, e.g. a custom worktrees_dir):
[[hosts]]
name = "ubuntu"               # → backend "wsl:ubuntu"
kind = "wsl"                  # selects the WSL transport
distro = "Ubuntu-22.04"       # the wsl.exe distro name (default = name)
```

| Field | Required | Default | Purpose |
|-------|----------|---------|---------|
| `name` | yes | — | backend id `ssh:<name>` / `wsl:<name>`; what `--host` expects |
| `kind` | no | `ssh` | transport: `ssh` (remote machine) or `wsl` (local distro) |
| `destination` | for ssh | — | ssh target (`user@host` or `~/.ssh/config` alias) |
| `distro` | no | `name` | WSL distro name (`kind = "wsl"` only) |
| `ssh_opts` | no | `[]` | extra ssh flags, one token per element (ssh only) |
| `socket` | no | `friring` | host `tmux -L` socket |
| `session` | no | `friring` | host tmux session name |
| `worktrees_dir` | no | host `$HOME/.local/share/friring/worktrees` | absolute worktrees dir on the host/distro |
| `multiplexer` | no | `tmux` | host multiplexer binary; set to `psmux` for a Windows SSH host |

**SSH** auth comes entirely from your `~/.ssh/config`; friring never
handles credentials. **WSL** distros are reached with
`wsl.exe -d <distro>` and need no config entry at all — on Windows they
are **auto-discovered** (`wsl.exe -l -q`) and appear in the host picker
and `--host` automatically; add a `kind = "wsl"` entry only to override
a default (e.g. `worktrees_dir`). For both kinds, tmux, git, the agent,
and worktrees all run **on the host / inside the distro** at native
paths (a WSL distro's worktrees live in its own Linux filesystem, not on
`/mnt/c`); the distro needs `tmux` >= 3.2 and `git`. Host changes
require a restart (the registry is read once and each host's `$HOME` is
cached for the process lifetime).

### Transports and multiplexers

`TmuxBackend` is transport-neutral (`agent::transport::TmuxTransport`);
only the one-time process launch differs by host kind:

| Kind | Launch prefix | Multiplexer |
|------|---------------|-------------|
| local | `<mux> -L friring …` | `tmux` (Linux/macOS) / `psmux` (Windows) — `DEFAULT_MUX` |
| `ssh:<name>` | `ssh <dest> <mux> -L friring …` | the `multiplexer` field |
| `wsl:<name>` | `wsl.exe -d <distro> tmux -L friring …` | `tmux`, inside the distro |

Everything downstream of the launch is identical: the same POSIX quoting
(`shell::posix_quote`) and the **byte-identical control-mode protocol**
(`control_mode.rs`). `wsl.exe` forwards whitespace-free tokens to the
in-distro shell exactly as `ssh` does; an arg *containing whitespace* is
kept as one word, so a multi-word `sh -c` script goes through
`wsl.exe --exec` instead (`shell::wsl_command` / `git::host_shell_c`).

**psmux** is a native-Windows, drop-in tmux clone (ConPTY, no WSL) that
speaks the **same control-mode wire protocol**, pane-id (`%N`), and `-L`
socket model, so the backend is parameterized by binary name rather than
forked. `DEFAULT_MUX` is `tmux` on Linux/macOS and `psmux` on Windows; a
remote SSH host can also pin `multiplexer = "psmux"`. psmux has known
**divergences** from tmux (verified against psmux 3.3.6, each branched on
`TmuxTransport::uses_psmux()`; a WSL distro's own tmux is unaffected):

- **`send-keys -H`** (hex byte injection) is not implemented, so
  `send_keys_commands` re-encodes keystrokes from the primitives psmux
  *does* support — `send-keys -l` literal runs plus
  `Enter`/`Tab`/`Escape`/`BSpace`/`C-<letter>` key-names — reproducing
  the same PTY byte stream (tmux keeps the byte-exact `-H` path). Literal
  runs go out as `-l -N 1 "…"` (double-quoted, `\"`/`\\` escaped); the
  `-N` flag makes psmux's send-coalescing decoder bail so a typed `'`
  isn't mangled into `\` (`flush_psmux_literal` / `psmux_quote`).
- **`new-window` trailing tokens are not joined** (psmux keeps only the
  first, dropping the agent's args) and **`new-window -e` is ignored** (env
  vars never reach the process). `TmuxBackend::psmux_window_powershell`
  folds env + command into **one PowerShell token**
  (`Set-Item Env:K 'v'; & 'claude' '--session-id' …`, run via
  `powershell -NoLogo -Command` — hence PowerShell single-quoting
  throughout, with backslash literal so `C:\` paths survive); control-mode
  spawns (`psmux_window_command`) frame it in double quotes, and the
  headless local `spawn_window` passes it as a single argv arg.

The **local** socket name honours the `FRIRING_SOCKET` env override
(`local_socket()`) — the only way to fully scope an instance on Windows,
where every `-L <name>` resolves machine-wide (no `TMUX_TMPDIR`). The
**local** group-session name likewise honours `FRIRING_TMUX_SESSION`
(`local_session()`), the escape hatch that lets a dev build adopt a
release server's live sessions (`scripts/dev/live.sh` sets both — see
`docs/DEVELOPMENT.md`). Remote hosts take their socket and session from
`hosts.toml`.

### Backends, worktrees, and restore

Each host registers a backend named `ssh:<name>` / `wsl:<name>`
(`TmuxBackend::from_host`), registered **lazily** at startup from
`host_config::load_all_with_warnings` — a down or slow host must not
block the first frame, so `check_available` / `ensure_ready` are deferred
to first use (`App::backend_for`). Loading unions the configured hosts
with `discover_wsl_hosts()` (deduped; a configured entry wins). Data
types: `session::HostDef` (`kind: HostKind {Ssh, Wsl}`) / `HostRegistry`
live in `session/` (so both `agent` and `git` can use them), with
backend-name helpers `is_ssh_backend` / `is_wsl_backend` /
`is_remote_backend`.

- **Selection.** `SessionConfig.backend` is `ssh:<host>` / `wsl:<distro>`
  (or `None` = local). The TUI new-session flow shows a **host picker**
  first (skipped when none are configured or discovered); the chosen host
  runs git worktree creation + branch listing. Headless:
  `friring-cli session create --host <name>`.
- **Worktrees** run via the host launcher (`git::*_on(host, …)` →
  `git::host_launcher` → `ssh …` / `wsl.exe …`) and live under the host's
  `worktrees_dir` (else `$HOME/.local/share/friring/worktrees`, resolved
  and cached per backend name — a WSL distro has no `destination`).
- **Persistence/restore.** `backend_type` round-trips in SQLite; restore
  discovers windows **per backend**, so off-local sessions re-adopt
  against their own host. Remote backends are readied + discovered **in
  the background** (one thread per host, drained by
  `App::poll_remote_restore` each tick), so an unreachable host never
  blocks the first frame — only local sessions restore synchronously
  (ADR-P7, `docs/PERFORMANCE.md`).

### Agent config, status, and teardown on a host

- **Agent args.** Args that reference friring-managed config by a *local*
  path (the hooks extension's `--settings <config>/hooks/claude.json`)
  would kill a remote agent ("Settings file not found"), so
  `session_ops::spawn::adapt_agent_args_for_remote` rewrites them per
  host: on a **POSIX remote** the home-anchored path is translated to the
  remote home, the file copied there, and the arg substituted; on a
  **psmux host / non-POSIX config root / failed copy** the flag+path pair
  is **stripped** so the agent launches clean. The local-path env hints
  (`FRIRING_METRICS_DIR` / `FRIRING_CONFIG_DIR` / `FRIRING_DATA_DIR`) are
  likewise skipped for remote spawns (`inject_friring_env`); only the
  opaque identity vars travel.
- **Session status** (hooks-driven, like local — see [Session
  status](#session-status)). `friring-cli session signal` can't run from a
  host (there is no CLI there, and it would write the host's own DB), so
  the materialized hook file's commands are rewritten
  (`builtin_hooks::rewrite_hook_signals_for_remote`) to set a tmux **pane
  user option** instead: `tmux set-option -p @friring_state <s>` needs no
  socket, pane id, or identity. The local TUI's control-mode connection
  subscribes once per connection (`refresh-client -B
  'friring-status:%*:#{@friring_state}'`, re-armed on reconnect in
  `ControlMode::start`; tmux ≥ 3.2) and drains `%subscription-changed`
  pushes (≤ 1/s) via `App::drain_remote_hook_events` into the same
  `set_hook_state` columns local signals use — so Done→seen
  acknowledgment, notifications, rollups, and the stuck-`working` fallback
  are shared. Events are matched by **backend name + pane id** (ids
  collide across hosts), allow-listed, and deduped. **Carve-outs:** psmux
  remotes (no subscriptions; hooks stripped) and non-claude agents (hook
  configs aren't materialized remotely) stay Idle-only.
- **Teardown.** `session delete --force` is backend-aware:
  `teardown_runtime_resources` resolves the session's `HostDef` from its
  `backend_type` and, for a remote session, kills the pane
  (`kill_pane_remote`) and removes each worktree
  (`git::remove_worktree_on(Some(host), …)`). It deletes the worktree
  *directory* only, leaving the branch; an unreachable host or a missing
  `hosts.toml` entry is recorded in
  `ForceDeleteReport.remote_teardown_error` (surfaced in the CLI JSON) and
  the row is still soft-/force-deleted. `wsl.exe` arg construction is
  unit-tested (`transport::tests::wsl_*`, `git_command_wsl_*`), not CI-run
  (no WSL runner).

For local testing, `scripts/dev/e2e/linux-container.sh up` spins a
throwaway Podman container (sshd + tmux + git) and `… test` asserts a
session lands on the `ssh:podman` backend without touching your real
`~/.ssh` / `~/.config`. The remote-transport design rationale lives in
ADR-13 (`ARCHITECTURE.md`).

## settings.toml

Scalar tuning knobs plus the `[features]` switches, seeded fully
commented-out (defaults apply when absent). Only knobs a user plausibly
wants are exposed; internals stay hardcoded. The seed closes with a
**Common recipes** block — copy-pasteable groupings (bigger scrollback,
a minimal/focused TUI, notification tuning, enabling the update badge),
all commented so defaults still apply out of the box.

| Key | Default | Purpose |
|-----|---------|---------|
| `scrollback_lines` | `1000` | terminal scrollback kept per session |
| `lazy_session_restore` | `true` | restore dead sessions as greyed **ghosts** (last saved frame) instead of respawning; Enter/restart loads one. Live tmux panes always re-attach |
| `two_panel_min_cols` | `80` | width below which only the terminal renders |
| `three_panel_min_cols` | `120` | width unlocking the optional third column |
| `info_panel_position` | `"auto"` | where the F2 info pane docks: `auto` / `column` / `inline` |
| `audit_retention_days` | `90` | audit-log history kept (pruned on startup) |

`info_panel_position` is the one top-level key that applies **live** (on
panel save or file reload, like the UI feature flags): `auto` docks the
pane at the bottom of the session column whenever the full session list,
the automations pane, and the full info content fit together, and falls
back to the dedicated column otherwise; `column` always uses the dedicated
column (the classic layout, needs `three_panel_min_cols`); `inline` always
docks it under the session list, squeezing the list down to its 3-row
minimum if it must. The inline dock only needs `two_panel_min_cols`, so
`auto`/`inline` keep F2 usable on terminals too narrow for the column —
except while the session column itself is collapsed (`Alt+L`), which takes
the inline dock with it and leaves every position needing
`three_panel_min_cols`. See `docs/FEATURES.md` ("Collapsing the session
list").

A complete `settings.toml` showing every knob at its default — copy
this, uncomment what you want to change, and restart:

```toml
config_version = 1

# Scalar tuning knobs (top level)
scrollback_lines      = 1000   # terminal scrollback kept per session
lazy_session_restore  = true   # dead sessions restore as greyed ghosts (Enter loads)
two_panel_min_cols    = 80     # width below which only the terminal renders
three_panel_min_cols  = 120    # width unlocking the optional third column
info_panel_position   = "auto" # F2 info pane dock: auto | column | inline
audit_retention_days  = 90     # audit-log history kept (pruned on startup)

[features]
tasks         = true
automations   = true
file_viewer   = true
global_search = true
double_shift_search = true   # double-Shift opens the search (kitty-protocol terminals)
info_panel    = true
shell_pane    = true
code_review   = true
cc_activity   = true
session_memory = true        # per-session RSS badge, fleet total, info-panel RAM
perf_hud      = true
mouse         = true
notifications = true
soft_delete   = true
version_check = false          # opt-in: makes a network call
auto_update   = false          # opt-in: downloads + replaces binaries

[notifications]
also_on_waiting     = false    # also fire when a session finishes (Working → Done)
suppress_for_active = true     # skip the session you're currently viewing
sound               = true     # play the OS default notification sound
min_interval_secs   = 5        # per-session floor between notifications

[review]
handoff       = "structured"   # review handoff shape: structured | legacy
nudge_on_idle = true           # toast a re-review nudge when the agent goes idle

[prefix]
mode          = "both"         # leader key: off | both | prefix-only
key           = "ctrl+f"       # the leader
key2          = "f12"          # second leader ("" disables, freeing F12)
hint_delay_ms = 0              # 0 = show the which-key overlay immediately

[navigation]
attention_includes_done = true   # F10/Alt+A also walk finished-but-unseen runs
session_numbers         = "auto" # jump numbers: auto | always
ghost_shelf             = false  # start with unloaded sessions folded away
```

### `[features]` — whole-feature switches

Turn major TUI features off entirely. All default to `true` **except
`version_check` and `auto_update`, which default to `false`** (both
reach the network, so they are opt-in). The UI-panel flags
(`tasks`, `file_viewer`, `info_panel`, `global_search`, `double_shift_search`,
`shell_pane`, `code_review`, `cc_activity`, `session_memory`, `perf_hud`,
`soft_delete`) apply **live** on save; the rest
(`automations`, `mouse`, `notifications`, `version_check`, `auto_update`)
take effect on the next launch.
A disabled feature's pane never renders, its keybinding shows
a status toast instead of acting, and its global-search scope returns
no results. Data is never touched, so re-enabling a flag is lossless.

| Key | Default | Controls |
|-----|---------|----------|
| `tasks` | `true` | tasks panel (`F5`/`Ctrl+W`) and task search results |
| `automations` | `true` | automations pane, `Ctrl+P`, TUI schedule firing, heartbeat arming |
| `file_viewer` | `true` | file viewer column (`F3`) and file search results |
| `global_search` | `true` | global search popup (`Ctrl+/` / double-`Shift`) |
| `double_shift_search` | `true` | the double-`Shift` opener for the global search (kitty-protocol terminals only; `Ctrl+/` is unaffected) |
| `info_panel` | `true` | info panel (`F2`; docking via `info_panel_position`) |
| `shell_pane` | `true` | per-session shell toggle (`Ctrl+T`) |
| `code_review` | `true` | native code-review view (diff + comments, `Ctrl+X`) |
| `cc_activity` | `true` | agent activity view (`F9`): per-session retrospective (commands / edits / reads / web / subagents) across supported agent CLIs, incl. the Claude workflow/subagent tree + conversation import; local sessions only (see `FORK.md`) |
| `session_memory` | `true` | per-session memory: the RSS badge on each list row, the `Σ` fleet total under the list, and the info panel's RAM line. Off = the process table is never read (no procfs walk / `ps` fork). Local sessions only (see `docs/FEATURES.md`) |
| `perf_hud` | `true` | perf HUD overlay (`<leader> m`, or `F12` when `[prefix] mode = "off"`): live perf counters + frame/tick timing (see `docs/PERFORMANCE.md`) |
| `mouse` | `true` | mouse capture: clicks, wheel, drag-select, hover, scrollbars |
| `notifications` | `true` | OS desktop notifications when a session needs attention |
| `soft_delete` | `true` | TUI `Ctrl+D` soft-deletes (Ctrl+Z undo); off = hard delete after a confirmation prompt |
| `version_check` | `false` | GitHub update check: TUI header "update available" badge + `friring-cli version --check` |
| `auto_update` | `false` | Silent self-update: download + verify + replace the binaries on startup + `friring-cli update`; also auto-refreshes stale extensions |

`automations = false` is a full stop on the TUI side: the pane
disappears (the session list takes the whole left column and `j`/`k`
wrap within it), and the TUI neither fires due schedules nor arms the
tmux heartbeat keeper on startup. Explicit `friring-cli automation`
commands still work — and `automation create` still arms the
heartbeat, so an already-armed keeper window (or an OS timer from
`packaging/`) keeps firing schedules externally. Disabling
`shell_pane` hides existing shell panes but never kills their
processes. `mouse = false` skips terminal mouse capture entirely, so
the terminal keeps its native mouse behavior (its own text selection,
URL handling, etc.) and no click/wheel/hover handling runs in the TUI.
`notifications = false` keeps the background dispatcher thread from
ever starting (zero overhead) and silently no-ops every transition;
the session status display itself is unaffected.

`soft_delete = false` turns the TUI's `Ctrl+D` into a destructive
**hard delete**: instead of marking the row deleted with a `Ctrl+Z`
undo window, it kills the session's tmux window, removes its worktrees
and symlink workspace, and disables any pending `Send` automations —
after a confirmation prompt (`Enter`/`y` to delete, `Esc`/`n` to
cancel), since the teardown is irreversible. The soft-deleted row is
still written last, so the session remains restorable via `Ctrl+U`
(which re-spawns it fresh). This flag governs the TUI only:
`friring-cli session delete` always soft-deletes unless you pass
`--force`, regardless of the setting.

`version_check = true` enables the update check (default `false`, since
it makes a network call). On launch the TUI reads a cached result
(`~/.local/share/friring/version-check.json`) and, if it is older than
24 h, fires a single best-effort background fetch of GitHub's latest
release (`api.github.com/repos/bvc3at/friring/releases/latest`, via
`curl`/`wget` — no new dependency); a newer release shows a `⬆ vX.Y.Z
available` badge next to the version in the header. The fetch never runs
on the render path and never blocks startup; failures are silent. Dev
builds (`0.0.0-dev`) never show the badge. The same flag enables
`friring-cli version --check`, which fetches fresh on demand and reports
current vs. latest (`friring-cli version` with no flag always prints the
current version, regardless of the flag).

`auto_update = true` goes a step further than `version_check`: instead of
just showing a badge, the TUI **silently updates itself** on startup. On every
launch (it does **not** reuse the `version_check` badge's 24 h cache — sharing
that gate let the badge keep the cache "fresh" and starve the updater) it
fetches the latest release tag; if a newer release exists it downloads that
release's tarball + checksums from GitHub Releases (`curl`/`wget`, no new
dependency), verifies the SHA256 (`sha256sum`/`shasum`), extracts it
(`tar`), and atomically replaces the installed `friring`/`friring-cli`
binaries in place — mirroring `scripts/install.sh`. The download is verified
**before** any installed file is touched, so a failed/corrupt download leaves
the current binaries untouched; the whole step runs before the TUI takes the
terminal and is best-effort (any failure is logged and startup continues on
the current version). The replaced binary takes effect on the **next launch**
(the running process keeps its open file), so the TUI shows an "Updated to
vX.Y.Z — restart to apply" status line. `friring-cli update` performs the
same update on demand (with `--force` to bypass the up-to-date and dev-build
guards); dev builds (`0.0.0-dev`) never auto-update. The default install
location (`~/.local/bin`) is user-writable; a system-wide install in a
root-owned directory will fail the replace (logged, non-fatal). `version_check`
and `auto_update` are independent — enable either or both.

Both features target **this fork's** releases (`bvc3at/friring`) and its
`friring` / `friring-cli` assets. A source-built binary reports `0.0.0-dev` and
is therefore never auto-updated — pull this repo and rebuild instead.

`auto_update = true` also keeps **installed extensions** in step with the
binary. Extension versions are pinned to the binary's release tag, so an
extension only goes stale (`installed_with` ≠ the running binary) right after an
upgrade. The self-heal pass — which already runs on TUI startup and on the
headless `automation tick` — then refreshes each stale extension in place
(re-fetching it from its recorded source) instead of only nudging you to run
`friring-cli extension update`. The staleness check is local and network-free,
so a launch where nothing is stale does no extra work; a refresh runs at most
once per extension per binary version. With `auto_update` off, the nudge is
shown and you update extensions by hand.

### `[notifications]` — OS notification settings

Surfaces an OS notification when a session **transitions into a state
that needs your attention**. The trigger is the hooks-driven
[session status](#session-status) (reported by the agent's hooks, *not*
the terminal bell / output): the notification fires when a session crosses
into `Blocked` (the agent needs input or approval) and, with
`also_on_waiting = true`, also when it finishes a turn (`Working → Done`).
The edge is observed once per tick in `refresh_session_statuses` — the
same place the session-list status icon is computed, so the banner can
never drift from the list — then deduped per session (`min_interval_secs`)
and skipped for the session you're currently viewing (`suppress_for_active`).
The notification body is the agent's last OSC 9 / OSC 777 message when
present (truncated to 200 chars), otherwise `Waiting for input` — the OSC
message is kept only for the body text, no longer for the trigger.
**Only fires while the TUI is open** — the dispatcher thread runs only
inside the TUI, so a headless `automation tick` never notifies.

**Delivery backend** (`backend`, default `auto`) is detected at startup:

| Backend | When | Click-to-focus |
|---------|------|----------------|
| `dbus` | normal Linux desktop with a running notification daemon (`org.freedesktop.Notifications`) | **yes** — clicking the banner writes a focus request the running TUI reads next tick and switches to that session |
| `windows` (toast) | **native Windows**, or **WSL** / any Linux with no dbus daemon — delivers a Windows toast via `powershell.exe` (WSL needs interop, on by default) | no (a Windows toast can't call back into the friring process) |
| `macos` | macOS native banner. Uses **`terminal-notifier`** when it's in `PATH` (own bundle + icon, looks like a real app notification — `brew install terminal-notifier`), otherwise the built-in **`osascript`** `display notification` (attributed to Apple's Script Editor). The `UNUserNotificationCenter` click API needs a signed `.app` bundle, which friring is not, so the `osascript` path is informational | **with `terminal-notifier`** — clicking the banner runs `friring-cli session focus <id>`, which writes the same metadata row the dbus path does and the TUI picks it up next tick. Without `terminal-notifier` (osascript fallback): no |

`auto` prefers `dbus` whenever a daemon answers, and only falls back to
the Windows toast when no dbus service is reachable. Force a specific
path or disable delivery with `backend = "dbus" | "windows" | "off"`
(`off` is a soft switch distinct from `[features] notifications`, which
stops the dispatcher thread entirely).

This auto-detection fixes a previously **silent failure** on WSL: the
dbus path errored on connect, but the only signal was a line in the
logfile, so the user saw nothing. Delivery errors are now recorded and
surfaced by the diagnostic:

```bash
friring-cli notify          # show the detected backend + last delivery error
friring-cli notify --test   # fire a sample notification to confirm it works
```

| Key | Default | Purpose |
|-----|---------|---------|
| `also_on_waiting` | `false` | also fire when a session finishes (`Working → Done`); the field name is historical |
| `suppress_for_active` | `true` | skip the notification for the session you're currently viewing |
| `sound` | `true` | play the OS default notification sound |
| `min_interval_secs` | `5` | per-session floor between two notifications (dedup) |
| `backend` | `auto` | delivery backend: `auto` \| `dbus` \| `windows` \| `off` |

Notifications fire on the hooks-driven status transitions (see
[Session status](#session-status)): always on `→ Blocked` (the agent needs
you), and with `also_on_waiting = true` also on `Working → Done`.

### `[review]` — native code-review knobs

Knobs for the built-in code-review view (see `docs/FEATURES.md` § Code
Review). Both apply **live** (panel save or file reload), like the UI
feature flags.

| Key | Default | Purpose |
|-----|---------|---------|
| `handoff` | `"structured"` | markdown shape of the compiled review that `e` (Send→Agent) pastes and `y` copies: `structured` \| `legacy` |
| `nudge_on_idle` | `true` | after a review was sent, toast "Agent idle — F7 to re-review, F5 to reload" when that session's agent finishes |

`structured` (the default) is the v2 handoff: a short in-band semantics
preamble (friring is agent-neutral, so no skill or system prompt can be
assumed on the receiving CLI), one `### C<id> [Class] <side>:<line>` record
per comment — `C<id>` is the comment's database id, stable across re-sends —
with the anchored diff line quoted as a grep-able locator (old-side quotes
are marked `(line was removed)`). `legacy` reproduces the original format
(`"Please address the following code review:"` prefix, `## <path>` sections
with `- **[Class]** (side:line)` bullets, `## Summary`) byte-for-byte, for
agent prompts/workflows that depend on it.

### `[prefix]` — the leader key

The tmux-style leader (see `docs/FEATURES.md` § The leader key). Applies
**live** on panel save or file reload.

| Key | Default | Purpose |
|-----|---------|---------|
| `mode` | `"both"` | `off` (no leader — pre-leader behaviour, `F12` is the perf HUD again) \| `both` (direct chords *and* the leader) \| `prefix-only` (direct **global** chords disabled, handing every bare `Ctrl+<letter>` back to the agent CLI; pane-scoped keys still work) |
| `key` | `"ctrl+f"` | the leader chord, in `keybindings.json` notation |
| `key2` | `"f12"` | second leader, tmux's `prefix2`. Set to `""` to disable — which also gives `F12` back to the perf HUD |
| `hint_delay_ms` | `0` | delay before the which-key overlay appears. `0` shows it immediately; raise it only if you know the table by heart, since the overlay *is* the leader's discoverability |

`key2` defaults to `F12` for two reasons: it is layout-independent (chords like
`Ctrl+\` or `Ctrl+]` need AltGr on DE/FR/Nordic keyboards), and it survives an
**outer** tmux that has claimed `Ctrl+A` — the one real cost of the default
leader. The trade is that `F12` no longer toggles the perf HUD while the leader
is on; that moved to `<leader> m`. Set `key2 = ""` to reverse it.

friring **warns at startup** if `key` or `key2` is set to a chord that is
likely to fail or surprise — `ctrl+b` (an outer tmux eats it), `ctrl+a` (screen's
prefix, and beginning-of-line in every agent CLI), `ctrl+c`/`ctrl+d` (reserved
and unrebindable in Claude Code and Codex), `ctrl+z` (SIGTSTP), `ctrl+q`
(XON, a `Cmd+Q` near-miss on macOS, and it kills the terminal in WSL), or
`ctrl+s` (XOFF). These are warnings, never errors: your config wins, and the
warning is silent when `mode = "off"`.

Both leaders accept the same chord syntax as
[`keybindings.json`](#keybindingsjson). An unparseable entry is skipped rather
than fatal, so a typo in `key` still leaves you `key2` to get in with.

### `[navigation]` — session navigation

Knobs for moving between sessions (see `docs/FEATURES.md` § Live status &
"needs attention" and § Collapsing repo groups & the ghost shelf). Applied
**live** on panel save or file reload.

| Key | Default | Purpose |
|-----|---------|---------|
| `attention_includes_done` | `true` | Whether the attention queue (`F10`, `Alt+A`, the badges) falls through to finished-but-unseen sessions once nothing is blocked. `false` restores the blocked-only queue. Blocked sessions always come first either way |
| `session_numbers` | `"auto"` | When the `1`–`9` jump numbers are painted: `auto` only while a gesture is pending (Alt held, leader armed, `Alt+A`), `always` permanently |
| `ghost_shelf` | `false` | Start with unloaded sessions folded out of the list into a title-bar count. Toggled live with `<leader> G` / `Alt+Shift+U`; this only sets the startup state |

Set `session_numbers = "always"` when running friring through an **outer
tmux**: the hold-`Alt` overlay needs the kitty keyboard protocol to see the
key go down, which tmux strips, so without it `Alt+1`–`9` is aim-blind.
Collapsed repo groups aren't a setting — they persist per-fold in the
SQLite `metadata` table (see [SQLite-backed settings](#sqlite-backed-settings)).

## Session status

Each session's state (Blocked / Working / Done / Idle / Error) is driven by
**agent hooks** that call `friring-cli session signal --state
<working|blocked|done|idle>`. Two states live outside the hook pipeline:
`Unreachable` (a remote placeholder whose host is down) and `Unloaded` (a
ghost — the agent process is deliberately not running; see
`lazy_session_restore` above). Both are assigned by the TUI and cleared the
moment the session adopts or loads. The state is persisted on the `sessions` row
(`hook_state`, `hook_state_at`, `seen_at` — schema v34) and survives the TUI
being closed; a hook fired headlessly is picked up via `PRAGMA data_version`.
Identity comes from the injected `FRIRING_SESSION` env var, so a hook passes
no id. A finished turn shows `Done` (blue) — for the session you're watching too
— and becomes `Idle` once you switch focus off it. Remote (`ssh:` /
`wsl:`) sessions can't run the CLI, so their hooks report over a tmux
pane user option delivered by control-mode instead, landing in the same
columns — see [hosts.toml](#hoststoml).

The hooks are wired up automatically by the built-in **hooks** extension
(auto-activated on first run). Opt out with `friring-cli extension deactivate
hooks`. The status colours are tunable theme keys (`status_working` /
`status_blocked` / `status_done` / `status_idle` / `status_error` /
`status_unreachable` — see `themes.toml`).

The wiring is applied **only to agents friring launches** — it never edits your
own global agent config (e.g. your personal `~/.claude/settings.json`). friring's
managed hook config lives per agent, applied by injecting a flag into
`agents.toml` or by a reversible merge into / managed file in the agent's own
config dir:

| Agent | On-disk location | How it's applied |
|-------|------------------|------------------|
| claude | `~/.config/friring/hooks/claude.json` | `--settings` flag (claude merges it with your own settings) |
| aider | — (no file) | `--notifications-command` flag |
| opencode | `~/.config/opencode/plugin/friring-status.js` | managed plugin file |
| codex | `~/.codex/hooks.json` | reversible JSON-merge of friring's entries |
| vibe | `~/.vibe/hooks.toml` | managed file (refused if you already have one) |
| antigravity | `~/.gemini/settings.json` | reversible JSON-merge of friring's entries |

The home dir is `~/.config/friring/hooks` on a release build and
`~/.config/friring-dev/hooks` on a dev build. Because claude *merges* the
`--settings` file, your own hooks still fire inside a friring session — both run.
Hand-edits to a managed file are rewritten from the embedded payload on the next
TUI start / heartbeat tick; to customize, deactivate the extension and wire the
hook yourself, or edit the payload under `extensions/hooks/` and reinstall. A
merged file (the codex/antigravity rows) is re-merged the same way, and when the
payload has actually changed friring's own entries are pruned first, so an
upgrade whose hook commands changed replaces them instead of leaving both
versions firing. That prune matches on the `session signal` command, so it would
also catch a hook you wrote around the same command — which is why it is limited
to the run that changes the payload rather than every tick. Full per-agent
detail: `extensions/hooks/README.md`.

## themes.toml

User-defined themes, offered in the `Ctrl+Y` picker (alt `F4`, which
avoids terminals that grab `Ctrl+Y` as DSUSP) alongside the **thirty-six
built-in presets** and persisted by `name` like any preset. The presets
are twenty-eight dark — **Default**, **Catppuccin Mocha**, **Tokyo
Night**, **Gruvbox Dark**, **Doom**, **Nord**, **Dracula**, **One Dark**,
**Rosé Pine Moon**, **Everforest**, **Kanagawa**, **Solarized Dark**,
**Monokai**, **Ayu Dark**, **Ayu Mirage**, **Material**, **Rosé Pine**,
**Oxocarbon**, **GitHub Dark**, **Nightfox**, **Sonokai**, **Melange**,
**Zenburn**, **Iceberg**, **Vesper**, **Synthwave**, **Nightfly**,
**Tomorrow Night** — and eight light — **Catppuccin Latte**, **Tokyo
Night Day**, **Gruvbox Light**, **Solarized Light**, **Ayu Light**, **One
Light**, **Rosé Pine Dawn**, **GitHub Light**; each is available as a
`base` in its id form (e.g. the `catppuccin-mocha` below). The picker
filters as you type (`/`). Each `[[themes]]` entry starts from a built-in
`base` and overrides only the colours it names:

```toml
[[themes]]
name = "my-mocha"            # stable id; must not shadow a built-in
display_name = "My Mocha"    # picker label (default: name)
base = "catppuccin-mocha"    # starting palette (default: default)
accent = "#fab387"
app_bg = "reset"             # keep the terminal's native background
```

Colours accept anything ratatui parses: `#rrggbb`, ANSI names (`red`,
`lightcyan`), indexed (`14`), or `reset`. The seeded file lists every
overridable key — including the code-review diff colours `diff_added` /
`diff_removed` (added/removed line foreground), `diff_added_bg` /
`diff_removed_bg` (the subtle full-row tint), and `diff_added_word_bg` /
`diff_removed_word_bg` (the stronger per-token background the word-level
intra-line diff paints over changed tokens; every preset derives them one
saturation step brighter than the row tint). Bad colours and built-in name
collisions degrade to startup warnings (the base colour / the built-in stays in
effect).

Custom themes load through
`agent::themes_config::load_or_seed_with_warnings`
(`session::theme_config::CustomThemeDef` → `ThemeEntry`) and are published
to the renderer by `ui::theme::set_custom_themes`. The active choice is
persisted in SQLite (`metadata.active_theme`, see [SQLite-backed
settings](#sqlite-backed-settings)); other friring processes pick up a
change within one tick via `PRAGMA data_version` polling.

## keybindings.json

Maps `Action` names to one or more chord strings:

```json
{ "QuitApp": ["ctrl+a"], "OpenThemePicker": ["ctrl+y", "f4"] }
```

- Preferred editing path is the **F1 panel** (live capture, conflict
  stealing, immediate persistence). Hand-edits are read at startup.
- Chord syntax: `[ctrl+][alt+][shift+][cmd+]<key>` where `<key>` is a
  letter, `f1`–`f12`, or a named key (`enter`, `esc`, `tab`, arrows,
  `home`, `end`, `pageup`, `pagedown`, `backspace`, `delete`,
  `insert`). Case-insensitive. `cmd` (aliases `super`, `command`,
  `win`) is the macOS Command key — delivered only by
  kitty-keyboard-protocol terminals (iTerm2 3.5+, kitty, WezTerm,
  Ghostty; not Terminal.app), and only for chords the emulator
  doesn't claim itself.
- Unknown action names, invalid chords, and the same chord bound to two
  actions in overlapping contexts are reported at startup (the file
  still loads; bad entries fall back to defaults).
- **Terminal passthrough.** When a session **terminal is focused**, the
  readline / shell line-editing chords (`Ctrl+A` start-of-line, `Ctrl+E`
  end-of-line, `Ctrl+W` delete-word, `Ctrl+U` kill-line, `Ctrl+R`
  reverse-search, `Ctrl+D` EOF, plus `Ctrl+B/F/J/K/O/P/S`) are **forwarded to the
  agent CLI** instead of triggering their friring command, so your terminal
  muscle memory works inside a session. Those friring commands stay reachable
  from the **session list** (focus it with `Ctrl+H`) and via their `F`-key
  alternates (`F2` info panel, `F3` file viewer, `F5` tasks). Rebinding such an
  action to a key that isn't a bare `Ctrl+<letter>` makes it work in the
  terminal too. Navigation/quit chords (`Ctrl+H`/`Ctrl+L`, `Ctrl+Q`, `Ctrl+N`)
  are **never** forwarded — they're how you leave the terminal. `Ctrl+J`/`Ctrl+K`
  now defer to the agent in a focused terminal (fork divergence — `Ctrl+J`
  doubles as a legacy `Ctrl+Enter` newline); use `Alt+J`/`Alt+K` to cycle
  sessions there.
- Action names and defaults: see the keybindings table in
  `docs/FEATURES.md` / README, or `src/session/keybindings.rs`.

## extensions/

Each opt-in extension (see `extensions/<name>/`) is described by a single
`extension.toml` manifest. `friring-cli extension install` writes the
home-resolved copy to `~/.config/friring/extensions/<name>.toml` (friring
never seeds this dir). The install **home** (where payload files land and the
session runs) defaults to `~/.config/friring/extensions/<name>/` — a sibling dir
of that manifest — unless the manifest pins a `home` or you pass `--home`. The
manifest has two halves — an **install** spec and a **runtime** spec:

```toml
name = "flow"
description = "Focus-protecting triage agent"
config_version = 1              # manifest *format* version (for migrations)
version = "1.0.0"              # the extension's own version (bumped by its author)
min_thurbox_version = "0.1.0"  # minimum friring; older binaries get a warning
# home = "~/flow"               # OPTIONAL; default is <config>/extensions/<name>.
                                # {home} is substituted everywhere it appears

# install spec ---------------------------------------------------------------
[[agents]]                      # registered in agents.toml (existing kept)
name = "flow"
command = "claude"
args = ["--model", "claude-haiku-4-5"]

[[files]]                       # fetched from the source, written under home
path = "FLOW.md"
[[files]]
path = "scripts/create-task.sh"
executable = true               # chmod +x
[[files]]
path = "repos.md"
if_absent = true                # seed once; never clobbered on reinstall
[[files]]
path = ".claude/settings.json"
source = "claude-settings.json" # source path differs from dest
substitute = true               # replace {home} in the content

[[symlinks]]                    # never clobbers a real file at `link`
link = "CLAUDE.md"
target = "FLOW.md"

# Reaching OUTSIDE the extension home (used by the built-in hooks extension):
[[external_files]]              # write a file into an agent's OWN config dir
path = "~/.config/opencode/plugin/x.js"
source = "x.js"
requires_dir = "~/.config/opencode"  # skip when that agent isn't installed

[[agent_patches]]               # append args to an EXISTING agent (reversible)
name = "claude"
append_args = ["--settings", "{home}/claude.json"]

[[config_merges]]               # reversibly deep-merge JSON into an agent's own
path = "~/.gemini/settings.json"  #   SHARED config file (never clobbered)
source = "antigravity-hooks.json"  # objects recurse, arrays union; uninstall prunes
requires_dir = "~/.gemini"      #   exactly our entries (by marker). no-op write
                                #   when unchanged; malformed target soft-skipped

# runtime spec (ensured on activate, self-healed if deleted) -----------------
[[sessions]]
name = "flow"
agent = "flow"
repo_path = "{home}"            # absolute, `~`-relative, or `{home}`; resolved
                                #   to an absolute path at install

# [[automations]] is an OPTIONAL runtime resource (flow itself ships none —
# it is purely event-driven). An extension that wants a scheduled tick declares:
[[automations]]
name = "example-tick"
trigger = "cron:*/10 * * * *"   # same grammar as `automation create --trigger`
session_ref = "flow"           # must match a [[sessions]] name above
prompt = "tick"
```

### `[[automations]]` grammar

The same block is what `friring-cli automation export` writes and
`automation import` reads, so an exported automation pastes into an
`extension.toml` unchanged. Each entry selects **exactly one** action flavour;
setting several (or none) is a load-time error.

| Key | Flavour | Meaning |
|---|---|---|
| `name` | — | identity: an existing automation of this name is reused, not duplicated |
| `trigger` | — | `hourly` / `daily` / `weekdays` / `weekly` / `cron:<expr>` / `at:<ms>` |
| `timezone` | — | IANA name the schedule is evaluated in (omitted = system local) |
| `enabled` | — | start disabled with `false` (omitted = enabled) |
| `session_ref` | send | an extension's `[[sessions]]` name; on **import**, a plain session name re-resolved per fire |
| `session_id` | send | an exact session UUID (what `export` writes for an id-targeted automation) |
| `repo` | spawn | repository to run a new session in |
| `worktree` / `base` | spawn | worktree branch and its fork point |
| `agent` / `host` | spawn | `agents.toml` / `hosts.toml` names |
| `session_mode` | spawn | `reuse` (default) or `fresh` per fire |
| `extra_repos` | spawn | multi-repo members (`repo_path`, `worktree`, `base_branch`) |
| `command` | exec | shell command run headlessly; `{home}` is substituted |
| `timeout_secs` | exec | kill deadline (omitted = 900 s) |
| `prompt` | send/spawn | the single-step form |
| `prompts` | send/spawn | ordered steps, each its own paste + Enter |
| `step_delay_ms` | send/spawn | settle time between *every* gap (omitted = 1200) |
| `steps` | send/spawn | `[[automations.steps]]` tables — ordered `text` + optional per-step `delay_ms` |

`prompt`, `prompts` and `steps` are three spellings of the same list and are
mutually exclusive; a prompt alongside `command` is rejected — an exec has no
agent to prompt, so it would be silently dropped.

Reach for `steps` only when the gaps differ. `prompts` + `step_delay_ms` applies
one delay to every gap, which is what most sequences want:

```toml
[[automations]]
name = "inbox-triage"
trigger = "weekdays"
repo = "/home/me/app"

# A slash command's popup needs longer to settle than a plain prompt does.
[[automations.steps]]
text = "/model opus"
delay_ms = 2000
[[automations.steps]]
text = "Summarize my inbox."
```

```toml
# A spawn automation with slash-command setup before the real work.
[[automations]]
name = "inbox-triage"
trigger = "weekdays"
timezone = "Europe/Zurich"
repo = "~/code/app"
worktree = "auto/inbox"
agent = "claude"
session_mode = "fresh"
prompts = ["/model opus", "/effort high", "Summarize my inbox."]
step_delay_ms = 1500
```

Manage extensions with the CLI:

```bash
friring-cli extension install flow         # fetch + lay files + agents + activate
friring-cli extension install ./extensions/flow   # from a local dir
friring-cli extension install <url> --home ~/x    # from a URL, custom home
friring-cli extension uninstall <name>     # reverse install (keep home dir)
friring-cli extension uninstall <name> --purge    # also delete the home dir
friring-cli extension list                 # installed + active/healthy + version/stale
friring-cli extension update <name>        # re-fetch from recorded source (refresh)
friring-cli extension update --all         # update every installed extension
friring-cli extension update <name> --force # also overwrite user-edited seed files
friring-cli extension activate <name>      # (re)create resources + mark active
friring-cli extension deactivate <name>    # tear down + stop self-heal
friring-cli extension deactivate <name> --force --purge  # also kill tmux + drop manifest
friring-cli extension status [<name>]      # per-resource presence + version/stale
```

A bare name installs from the official source
(`raw.githubusercontent.com/bvc3at/friring/<ref>/extensions/<name>`,
fetched via curl/wget) — `<ref>` is the running binary's release tag
(`main` for dev builds), so a fetched extension matches your binary. A
path or `http(s)://` URL installs from there instead. Payload paths are
validated against traversal (no absolute paths or `..`), and a
`substitute` file you've edited isn't overwritten on reinstall (use
`--force`). Payload files are fetched as **text** (specs/scripts/JSON),
not binaries.

The official source is this fork's own repo. An **upstream** Thurbox URL still
installs, but its payloads invoke `thurbox-cli` and its manifests declare
version floors on upstream's release line, so it won't work here — install from
a bare name, this repo, or a local directory
(`friring-cli extension install ./extensions/<name>`).

While an extension is **active**, friring **self-heals** its declared
resources: on TUI startup and on every `automation tick` it re-creates
any session/automation that has been deleted. So deleting them by hand is
a no-op (they come back); `extension deactivate` is the real off-switch.
Self-heal while the TUI is closed depends on the automation heartbeat
(`[features] automations = true`); with automations off, healing happens
at the next TUI startup only.

### Versioning + the update lifecycle

Extensions carry two version markers, and the installer stamps two more
into the discovery-dir copy so staleness can be detected:

| Field | Where set | Purpose |
|-------|-----------|---------|
| `version` | source manifest | the extension's own semver (author-bumped) |
| `min_thurbox_version` | source manifest | minimum friring; older binaries warn |
| `installed_with` | stamped on install | the friring version that installed it |
| `source` | stamped on install | the target it was installed from |

A **bare-name** install (`extension install flow`) fetches from the
official source **pinned to the running binary's release tag**, so the
extension you get always matches your friring. When you later **upgrade
friring**, the on-disk copy is now older than the binary — friring
flags it as `stale` (in `extension list`/`status`, and as a one-line
nudge from self-heal at startup). Run `extension update <name>` (or
`--all`) to re-fetch from the recorded `source`; because a bare name
re-resolves against the *new* binary's tag, this pulls the version that
matches your upgraded friring. Updates honour the same file rules as
install — user-edited `substitute` files and `if_absent` seeds are
preserved unless you pass `--force`.

`min_thurbox_version` is a **soft** gate: an extension authored for a
newer friring still installs on an older binary, but install/activate and
self-heal emit a compatibility warning so the mismatch is visible.
**Dev builds** (`0.0.0-dev`) skip both the staleness and compatibility
checks — their version doesn't order against release tags.

The **key name** is a wire format shared with upstream and stays as-is, but the
**value** is compared against the running `friring` binary — so an extension in
this repo declares a floor on *friring's* release line, not upstream's. Carrying
an upstream floor over unchanged would warn on every install, since the two
version lines are numbered independently.

**Rollback.** There's no version snapshot store: to roll an extension
back, pin a specific friring tag — `extension install
https://raw.githubusercontent.com/bvc3at/friring/v0.19.0/extensions/flow`
— or downgrade the binary and run `extension update`, which re-resolves
the bare name to that older tag.

## SQLite-backed settings

Live in the `metadata` table and apply immediately (no restart):

| Key | Set via | Purpose |
|-----|---------|---------|
| `active_theme` | `Ctrl+Y` / `F4` picker | TUI palette (fifteen built-ins) |
| `editor_command` | `friring-cli editor set "<cmd>"` | what `Ctrl+O` runs |
| `editor_mode` | `friring-cli editor mode <auto\|terminal\|gui>` | how `Ctrl+O` runs the editor: `auto` (default) detects terminal vs GUI and gives terminal editors a real TTY (tmux popup / TUI suspend); `terminal` forces the TTY path; `gui` forces detached |
| `active_extensions` | `friring-cli extension activate/deactivate` | JSON array of active extensions to self-heal |
| `builtin_hooks_optout` | `friring-cli extension deactivate hooks` | `1` when the user opted out of the auto-activated hooks extension |
| `perf_snapshot` | the TUI, while perf timing is active (`FRIRING_PERF_LOG` or an open perf HUD) | JSON perf snapshot read by `friring-cli perf` (see `docs/PERFORMANCE.md`) |
| `folded_session_groups` | `h` / `l` in the session list | JSON array of collapsed repo-group keys. Curating a long list is worth doing once, so the arrangement outlives the process; unknown keys are kept, not pruned, so a deleted-and-recreated group comes back the way you left it |

These are in the DB rather than a file because they are written
concurrently by multiple friring processes (TUI, CLI, MCP) and picked
up live via `PRAGMA data_version` polling.

Beyond the `metadata` keys above, the `repo_sync_bases` table stores
each repo's default base remote for the `Ctrl+S` worktree sync —
written when a choice is confirmed in the sync base picker (shown only
for repos with more than one remote; see `docs/FEATURES.md`,
"Choosing the base remote").

## Environment variables

User-set (read by friring):

| Variable | Used for |
|----------|----------|
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME` | config/data roots |
| `VISUAL`, then `EDITOR` | `Ctrl+O` editor when `editor_command` is unset |
| `SHELL` | the `Ctrl+T` companion shell pane (fallback `/bin/sh`). For a remote/WSL session the pane uses the **host's** `$SHELL` as an interactive login shell (the SSH-login environment), not the local one. |
| `RUST_LOG` | log filter for `friring.log` |
| `FRIRING_PERF_LOG` | opt-in performance logging: a one-shot `startup` phase breakdown at first paint, per-session `restore_adopt`/`adopt_split` lines, steady-state `perf_window` lines (~10 s cadence), and wall-clock frame/tick timing collection. Any value enables it. See `docs/PERFORMANCE.md`. |
| `FRIRING_SOCKET` | overrides the **local** multiplexer socket name (default `friring`; dev builds `friring-dev`). For test/sandbox tooling: Unix scoping uses `TMUX_TMPDIR`, but psmux (Windows) resolves every `-L <name>` machine-wide, so this is the only way to fully scope an instance there. Remote hosts are unaffected (socket from `hosts.toml`). Empty = unset. |
| `FRIRING_TMUX_SESSION` | overrides the **local** tmux group-session name (default `friring`; dev builds `friring-dev`) — the window group `discover()` scans on startup. Together with `FRIRING_SOCKET` this lets a dev build adopt a release server's live sessions (`scripts/dev/live.sh` / `just dev-live`, see `docs/DEVELOPMENT.md`). Remote hosts are unaffected (session from `hosts.toml`). Empty = unset. |
| `FRIRING_CLAUDE_USAGE_URL` | overrides the endpoint the info panel's Claude account-usage fetch calls (default `https://api.anthropic.com/api/oauth/usage`), so a test or demo can point it at a local stub — `ANTHROPIC_BASE_URL` doesn't cover it, that's a Messages-API base, not this OAuth account route. See `src/usage/mod.rs`. Empty = unset. |

Set **by** friring into every spawned agent process (not user-set;
`session_ops::inject_friring_env` / `App::build_spawn_inputs`). An
agent — or a `friring-cli` call running inside the session — reads
these to prove its own identity without scraping panes or names:

| Variable | Set into agent process |
|----------|------------------------|
| `FRIRING_SESSION` | the stable friring `SessionId` (the registry key); read back by `friring-cli message`/`inbox` for self-identity |
| `FRIRING_SESSION_ID` | the agent's own conversation id (`agent_session_id`); consumed by the metrics statusline. Distinct from `FRIRING_SESSION` |
| `FRIRING_TASK` | the originating task id; task-spawned sessions only (headless `task run`) |
| `FRIRING_METRICS_DIR` | metrics output dir |
| `FRIRING_CONFIG_DIR` / `FRIRING_DATA_DIR` | the resolved config/data dirs, so the agent's `friring-cli` (its status hook) targets the same DB the TUI reads — independent of XDG, which `friring-cli` is on PATH, or a stale tmux-server env. Also honored if you set them yourself to relocate friring's state. |
| `FRIRING_SIGNAL_FILE` | **sandboxed sessions only.** The one file a policy boundary may write status into: the bundled hooks append a state word here instead of calling `friring-cli session signal`, because the database is denied inside every sandbox (see [`docs/SANDBOX.md`](SANDBOX.md) §Status signals). Unset for every unsandboxed session, which is what makes those hooks byte-identical to before. |

The three *path* variables (`FRIRING_METRICS_DIR`, `FRIRING_CONFIG_DIR`,
`FRIRING_DATA_DIR`) are set only for a session running on **this** machine's
filesystem. An SSH/WSL session and a sandbox place both skip them: over there
those paths name nothing, and for a place the data directory is precisely what
the boundary exists to keep out (ADR-29). The identity variables are opaque and
travel everywhere.

Set **at build time** (not runtime):

| Variable | Used for |
|----------|----------|
| `FRIRING_RELEASE_VERSION` | read by `build.rs` to inject the binary version at build (CI release workflow sets it, e.g. `v0.7.0`); absent → falls back to `CARGO_PKG_VERSION` |

Editor resolution order: DB `editor_command` → `$VISUAL` → `$EDITOR` →
error toast.

## Versioning

The SQLite schema migrates automatically (`schema_version` in
`metadata`). Migrations are forward-only: a DB whose stored version is
*newer* than the binary supports is refused at open (upgrade the binary,
or restore the backup taken before the newer binary migrated it — see
`scripts/dev/live.sh` for the dev workflow that makes one). The TOML
files carry a `config_version = 1` marker so a future format change can
migrate them too; current files are version 1 and the field is optional.
