# github-issues (friring extension)

> **Experimental.** Bidirectionally syncs **GitHub issues** with the friring
> task list: your issues show up as tasks, and marking a task done closes the
> issue.

A `github-issues-tick` **automation** runs a deterministic sync script
(`scripts/sync.sh`) every 15 minutes — **no agent, no LLM, no tokens**. friring's
scheduler runs it (TUI or headless heartbeat) and records the result in the
automation run history. The script only calls `friring-cli` and the `gh` CLI.

## Setup

### 1. Prerequisites

- `friring-cli` **≥ 0.141** on `PATH` (needs the `Exec` automation action +
  `task --source/--external-id/--external-url`; check `friring-cli version`).
- `gh` (GitHub CLI) and `jq`.

### 2. Authenticate GitHub

```sh
gh auth login          # GitHub.com → HTTPS → log in via browser
gh auth status         # confirm
```

No token env var is needed — `gh` stores the credential itself.

### 3. Install the extension

```sh
friring-cli extension install github-issues
# or from a checkout:
friring-cli extension install ./extensions/github-issues
```

This lays down `~/.config/friring/extensions/github-issues/` (override with
`--home`) and activates the `github-issues-tick` automation, which friring
self-heals if deleted.

### 4. Configure the repos to sync

Edit `~/.config/friring/extensions/github-issues/trackers.md` — one row per repo or saved filter:

```markdown
| name    | query                              | push_back |
|---------|------------------------------------|-----------|
| backend | myorg/backend --assignee @me       | no        |
| triage  | myorg/web --label needs-triage     | no        |
```

Keep `push_back = no` for the first run (pull only — nothing is changed on
GitHub).

### 5. First sync & verify

The automation fires every 15 min. To run it now, trigger it from the
**Automations** pane (`Ctrl+P` → select `github-issues-tick` → `r`) or headless:

```sh
friring-cli automation run <id>     # id from: friring-cli automation list
# or run the script directly:
~/.config/friring/extensions/github-issues/scripts/sync.sh
```

Then check the imported tasks:

```sh
friring-cli task list --json | jq -c '.[] | select(.source=="github") | {id,status,external_id,title}'
```

Open issues import as `todo` (or `in_progress` if assigned), closed as `done`.
Re-running is **idempotent** — dedup by `(source, external_id)` means unchanged
issues are left alone. The run's output (created/updated/unchanged counts) shows
in the automation run history.

### 6. Enable two-way sync (optional)

Set `push_back = yes` on a tracker row. Now marking an imported task **done**
closes its GitHub issue, and moving it back to **todo** reopens it (only the
open/closed axis is pushed; `in_progress` stays open). The sync runs
**push-then-pull** so your local change reaches GitHub before the pull reads
state back.

## trackers.md reference

- **query** — `owner/repo` plus any `gh issue list` flags (`--state`,
  `--assignee`, `--label`, `--milestone`, …). Open issues are used if `--state`
  is omitted.
- **push_back** — `yes` enables friring → GitHub status push for that row.

Imported tasks carry `source=github` and `external_id="owner/repo#<number>"`, so
re-syncing never duplicates them.

## How it works

`scripts/sync.sh` (run by the automation) does, in order:

| Step | Script | Behavior |
|------|--------|----------|
| Push | `push-status.sh` | `done` → close, `todo`/`in_progress` → open (only `push_back=yes` rows) |
| Pull | `fetch.sh` + `upsert.sh` | List issues → create/update tasks; dedup by `(source, external_id)` |

GitHub is authoritative for an issue's title, URL, and open-vs-done state; your
local `todo`↔`in_progress` distinction is preserved.

## Troubleshooting

- **Nothing syncs** — check the automation run history (`Ctrl+P`, or
  `friring-cli automation runs <id>`) for the script's output; run
  `~/.config/friring/extensions/github-issues/scripts/sync.sh` by hand to see errors directly.
- **`gh` auth error / empty pull** — `gh auth status`; confirm
  `gh issue list --repo <owner/repo>` works for each tracker's repo.
- **`unknown option '--source'`** — your `friring-cli` predates 0.141; rebuild /
  update friring.

## Turn it off

```sh
friring-cli extension deactivate github-issues          # stop syncing
friring-cli extension uninstall github-issues --purge   # remove home + automation
```

Imported tasks remain in your task list (they are not deleted on uninstall).
