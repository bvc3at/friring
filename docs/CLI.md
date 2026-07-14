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
- **`automation`** (alias `auto`) — create / list / show / edit / remove / run /
  runs / tick. See the Automations section of `docs/FEATURES.md`.
- **`task`** (alias `todo`) — create / list / show / edit / remove / run. See the
  Tasks section of `docs/FEATURES.md`.
- **`message`** (alias `msg`) — send / inbox / prune (the inter-session mailbox
  queue). See the Inter-Session Messages section of `docs/FEATURES.md`.
- **`editor`** — open a session's working dirs in `$EDITOR`.
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
  startup when the flag is on). **Fork caveat (friring):** `version --check` and
  `update` retain the upstream **Thurbox** release contract — they query
  `Thurbeen/thurbox` releases and replace on-disk `thurbox`/`thurbox-cli` assets
  — so they do **not** update a source-built `friring`; update it by pulling this
  repo and rebuilding (see `FORK.md`).
- **`notify`** — diagnose OS desktop notifications: prints the detected delivery
  backend and last error; `--test` fires a sample. See the OS Notifications
  section of `docs/FEATURES.md`.
- **`perf`** — prints the perf snapshot a running TUI publishes while
  `FRIRING_PERF_LOG` or its perf HUD is active. See `docs/PERFORMANCE.md`.

## Output format

Output is **human-readable by default** and switches to JSON automatically when
stdout is piped (so `… | jq` keeps working). Force a format with `--json`
(compact), `--pretty` (indented JSON), or `--text` (human even when piped).

`session get`/`session list` JSON includes `hook_state`/`hook_state_at` — the
**raw** persisted status-hook columns (`working`/`blocked`/`done`/`idle`, epoch
ms; `null` until the first signal), deliberately *not* the TUI's derived status
(which downgrades a stale `working`). This is the contract external observers —
automations, the real-agent e2e harness (`docs/E2E.md`) — poll for status
transitions instead of reading SQLite.

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
