# friring-cli — headless CLI reference

A second binary (`friring-cli`) drives the same SQLite-backed, tmux-hosted
sessions headlessly (no TUI). It shares the database with the TUI; changes
appear via `PRAGMA data_version` polling, so a CLI mutation shows up in a running
TUI (and vice versa) within a tick.

```bash
cargo build --bin friring-cli
friring-cli session create --name demo --repo-path /path \
    --agent codex --worktree-branch feat/x
# Spawn on a remote host from hosts.toml (worktree + tmux live remotely):
friring-cli session create --name demo --repo-path /srv/repo \
    --host devbox --worktree-branch feat/x
# Spawn a worker under a lead session (parent must exist):
friring-cli session create --name worker --repo-path /path \
    --parent <lead-uuid>
# Multi-repo: each --add-repo gets its own worktree on --worktree-branch;
# --add-dir attaches a repo as-is (no branch). The agent launches in a
# symlink workspace gathering every repo. Works on `task create` too.
friring-cli session create --name demo --repo-path /a \
    --agent claude --worktree-branch feat/x \
    --add-repo /b@main --add-repo /c@master --add-dir /reference
friring-cli session list                       # human-readable table
friring-cli session list --json | jq           # machine output for scripts
friring-cli session list --parent <lead-uuid> --json | jq  # direct children only
```

## Subcommands

- **`session`** — create / list / get / delete / restore / restart / send /
  capture / focus / signal.
- **`automation`** (alias `auto`) — create / list / show / dry-run / export /
  import / edit / remove / run / runs / tick. See the Automations section of
  `docs/FEATURES.md`, and the flag reference below.
- **`task`** (alias `todo`) — create / list / show / edit / remove / run. See the
  Tasks section of `docs/FEATURES.md`.
- **`message`** (alias `msg`) — send / inbox / prune (the inter-session mailbox
  queue). See the Inter-Session Messages section of `docs/FEATURES.md`.
- **`editor`** — open a session's working dirs in `$EDITOR`. `editor mode
  <auto|terminal|gui>` chooses how `Ctrl+O` launches it: terminal editors get a
  real TTY via a tmux popup or a TUI suspend, GUI editors spawn detached. See
  the Editor Integration section of `docs/FEATURES.md`.
- **`config`** — validate / show: strict-parses every config file, or prints the
  effective resolved config. See `docs/CONFIG.md`.
- **`extension`** (alias `ext`) — install / uninstall / reinstall / list /
  available / update / activate / deactivate / status: manage opt-in extensions.
  See the Extensions section of `docs/FEATURES.md`.
- **`version`** — prints the running version; `--check` queries GitHub's latest
  release (gated on `[features] version_check`, off by default).
- **`update`** — downloads, verifies, and replaces the installed binaries with
  the latest release; `--force` bypasses the up-to-date / dev-build guards (gated
  on `[features] auto_update`, off by default; the TUI also runs this silently on
  startup when the flag is on). Both query this fork's releases
  (`bvc3at/friring`) and replace on-disk `friring` / `friring-cli`. A
  source-built binary reports `0.0.0-dev` and is skipped unless `--force`; update
  it by pulling this repo and rebuilding.
- **`notify`** — diagnose OS desktop notifications: prints the detected delivery
  backend and last error; `--test` fires a sample. See the OS Notifications
  section of `docs/FEATURES.md`.
- **`perf`** — prints the perf snapshot a running TUI publishes while
  `FRIRING_PERF_LOG` or its perf HUD is active. See `docs/PERFORMANCE.md`.

## Typing into a session (the modal guard)

Every headless path that types into a pane — `session send`, `message send` /
`reply`'s wake nudge, a `send` automation, a task's prompt, and the deferred
`run-shell` delivery after a headless spawn — pastes its text and then presses
Enter as a **separate** keystroke. A pane showing a modal swallows the text and
reads that Enter as the operator answering the dialog, so an unguarded send
confirms whatever the session is asking permission to do.

All of them are therefore guarded, on two signals:

- the agent's own hook state (`session signal --state blocked`, wired by the
  built-in `hooks` extension), and
- a scrape of the visible pane for the markers in
  `agent::tmux::MODAL_MARKERS` (`Do you want to`, `❯ 1.`, `Esc to cancel`,
  `[y/N]`, …).

**Only the pane scrape gates scheduled work.** `blocked` means "a dialog was
raised during this turn", not "a dialog is on screen now": Claude Code fires
`PreToolUse` *before* the permission prompt and nothing on approval, so an
approved tool call reports `blocked` for its whole run. The mailbox wake and
`session send` still honor it — a deferred wake is retried and `session send`
has `--force` — but `send` automations and task delivery take the pane scrape
alone, so a session busy with a long approved tool call doesn't silently miss
every fire aimed at it.

What a refusal does depends on who asked:

| Caller | On refusal |
|---|---|
| `message send` / `reply` wake | enqueues silently, reports `"wake_deferred": true` with a reason, retried on each `automation tick`¹ |
| `session send` | **errors** — typing is the command's whole purpose; `--force` types anyway |
| `automation` `send` | run recorded as `Skipped`, naming the marker and how many steps landed |
| `task` `send`/reuse | reported as `skipped`, task left due (not marked in progress) |
| deferred spawn delivery | the `run-shell` script aborts before typing |

¹ The retry rides the `automation tick` heartbeat, so it needs
`[features] automations` on (the default). With automations disabled a deferred
wake is never retried automatically — the message is still durably queued and
the recipient gets it on its next `message inbox`, but the "go look now" nudge
is lost. Pass `--no-wake` and poll the inbox if you run that way.

The guard is deliberately over-broad: a false positive only delays a delivery,
while a false negative answers a security prompt with nobody watching. It is
also not a permission boundary — anything that can run `friring-cli` can still
`session send --force`. What it removes is the *surprise*: an operation whose
contract is "enqueue a payload" no longer presses Enter in another session.

## Automation flags

`automation create` and `automation edit` share one **action-flag group**, so an
automation's action is editable in place rather than delete-and-recreate. On
`create` the group picks the action (exactly one of the four selectors); on
`edit` a selector *switches* the action kind outright and the rest amend the
current one.

| Flag | Action | Meaning |
|---|---|---|
| `--session <uuid>` | send | target this exact session |
| `--session-name <name>` | send | target whichever session has this name, re-resolved on every fire |
| `--repo <path>` | spawn | repository to run a new session in |
| `--worktree <branch>` | spawn | create/attach a worktree branch |
| `--base <branch>` | spawn | fork point for a new worktree (default `main`) |
| `--agent <name>` | spawn | agent from `agents.toml` (default: the registry default) |
| `--host <name>` | spawn | host from `hosts.toml` to run on (default local); needs an absolute path and rules out `--worktree` |
| `--session-mode <reuse\|fresh>` | spawn | one session across fires, or a new one per fire |
| `--add-repo <path[@base]>` | spawn | extra repo on its own worktree (repeatable) |
| `--add-dir <path>` | spawn | extra directory attached as-is (repeatable) |
| `--command <shell>` | exec | run headlessly, no agent or session |
| `--timeout <secs>` | exec | kill the command *and its descendants* after this long (default 900) |

Prompts are separate from the action and apply to send/spawn:

- `--prompt <text>` — **repeatable**. Each occurrence is one delivery step: its
  own paste + Enter, in order. This is how you configure an agent before giving
  it work; a single multi-line prompt would submit as one message.
- `--step-delay <ms>` — settle time between steps (default 1200). On `edit` it
  only applies together with `--prompt`, since the delay belongs to a step.
  This flag applies **one** value to every gap; for delays that differ per step,
  author the `[[automations.steps]]` form and `automation import` it (see
  `docs/CONFIG.md`), or set them in the TUI editor.
- `--timezone <IANA>` — validated on save (a typo is rejected, not silently
  resolved to system local time).

```bash
# A weekday-morning triage agent, configured before it gets its real prompt.
friring-cli automation create --name inbox --trigger weekdays --time 07:00 \
    --timezone Europe/Zurich --repo ~/code/app --worktree auto/inbox \
    --agent claude --session-mode fresh \
    --prompt '/model opus' --prompt '/effort high' \
    --prompt 'Summarize my email history and file anything actionable.'

friring-cli automation dry-run 3        # what would the next fire do?
friring-cli automation edit 3 --agent codex     # amend, don't recreate
friring-cli automation export --id 3 > inbox.toml
friring-cli automation import inbox.toml --replace
```

`dry-run` resolves the schedule, target/spawn parameters, host and every prompt
step **without firing** and without touching the run history; its JSON is an
ordered array of `{"label", "value"}` rows, because a plan repeats labels (one
`extra repo` row per extra repository). `export`/`import`
round-trip through the `[[automations]]` TOML grammar extension manifests use
(see `docs/CONFIG.md`); import matches on name and skips an existing automation
unless `--replace`.

## Output format

Output is **human-readable by default** and switches to JSON automatically when
stdout is piped (so `… | jq` keeps working). Force a format with `--json`
(compact), `--pretty` (indented JSON), or `--text` (human even when piped).

`session get`/`session list` JSON includes `hook_state`/`hook_state_at` — the
**raw** persisted status-hook columns (`working`/`blocked`/`done`/`idle`, epoch
ms; `null` until the first signal), deliberately *not* the TUI's derived status
(which downgrades a stale `working`). This is the contract external observers —
automations, the real-agent e2e harness (`docs/E2E.md`) — poll for status
transitions instead of reading SQLite. Multi-repo sessions additionally expose
`additional_dirs` (the non-primary member dirs) and `workspace_dir` (the
user-chosen symlink-workspace directory from the wizard's `Ctrl+O` field;
`null` = the default id-derived path).

## Delete and restore semantics

`session delete <uuid>` **soft-deletes** by default — only the DB row is marked
deleted (the TUI tears down the tmux window/worktree on its next sync), and
`session restore` revives it. Pass `--force`
(`session_ops::delete_session_headless`) to also kill the tmux window, remove
worktrees + the symlink workspace, and disable `send` automations targeting the
session — for headless cleanup when no TUI is running. Teardown is best-effort
(failures land in the JSON report); the row is always soft-deleted last.

A `--force` delete also stamps the `sessions.force_deleted` column (schema v37):
the row still appears in the restore list **tagged `force-deleted`** and is
restorable **best-effort** — force-delete removes the worktree *directory* but
not the git branch, so restore reattaches each surviving branch's committed work
(`App::recreate_worktrees`); only uncommitted/untracked changes are gone. Because
that recovery is lossy, the headless `session restore` **refuses a force-deleted
row unless `--best-effort`** is passed (its JSON then carries `best_effort:
true`); a plain soft delete restores with no flag. `restore_session` clears both
`deleted_at` and `force_deleted`.

The **TUI** `Ctrl+D` soft-deletes by default too (with a `Ctrl+Z` undo window).
The `[features] soft_delete` flag (settings.toml, default `true`) governs only
this TUI path: set it `false` and `Ctrl+D` becomes a hard delete — the same
`delete_session_headless(.., force=true)` teardown — since there is no `Ctrl+Z`
for it. That hard delete is **conditional**: a confirmation modal
(`Modal::ConfirmDelete`, rendered by `ui::confirm_delete_modal`) appears **only
when the session has work at risk** — uncommitted/untracked files or unmerged
commits, or a state that can't be verified (remote host / git error → confirm to
be safe; `App::assess_delete_risk` + `modals::DeleteRisk::from_stats` over
`git::worktree_stats`). The modal itemizes what would be lost; a known-clean
session is deleted with no prompt. A force-deleted session is then tagged in the
restore list; restoring one via `Ctrl+U` (`Enter`) first asks for confirmation
(`Modal::ConfirmRestore`, rendered by `ui::confirm_restore_modal`) since the
recovery is best-effort (committed branch state only), then runs the normal
restore path. The flag never changes `friring-cli session delete`, which stays
soft unless `--force`.
