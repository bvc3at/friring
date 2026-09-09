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
# Run the agent inside a sandbox profile (docs/SANDBOX.md). The profile is
# authored in the TUI; an unknown name fails the spawn rather than silently
# running on the host, and the profile is persisted so a restart rebuilds it.
friring-cli session create --name demo --repo-path /path --sandbox dev
friring-cli session list                       # human-readable table
friring-cli session list --json | jq           # machine output for scripts
friring-cli session list --parent <lead-uuid> --json | jq  # direct children only
```

## Subcommands

- **`session`** — create / list / get / delete / restore / restart / send /
  capture / focus / signal, plus the per-session metrics readers
  `metrics` / `resources` / `activity` (see Agent metrics below).
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
- **`config`** — paths / validate / show: strict-parses every config file, or
  prints the effective resolved config. `paths` reports the resolved config dir,
  data dir and database file together with the environment variable that decided
  each, and answers *before* any database is opened — which is what makes it
  usable as a preflight (`--json` fields: `config_dir`, `config_source`,
  `data_dir`, `data_source`, `database`, `app_dir_name`, `database_opened`). See
  `docs/CONFIG.md`.
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
- **`sandbox`** (alias `sb`) — manage sandbox profiles and the places they run
  in, without a TUI. See [Sandboxes](#sandboxes) below.
- **`usage`** — account-level rate-limit windows for an agent (see Agent
  metrics below).
- **`perf`** — prints the perf snapshot a running TUI publishes while
  `FRIRING_PERF_LOG` or its perf HUD is active. See `docs/PERFORMANCE.md`.

## Sandboxes

`friring-cli sandbox` (alias `sb`) is the headless half of
[`docs/SANDBOX.md`](SANDBOX.md): the same profiles the TUI's `Alt+S` list edits,
the same places its manager view drives, and the same refusals a launch makes.

| Command | Does |
|---|---|
| `sandbox list [--instances]` | Every profile: resolved backend, path count, network mode, and why the backend is unavailable here when it is. `--instances` adds each profile's live places. |
| `sandbox show <name>` | One profile in full: its places, any columns friring could not decode, and whether its backend is available here. |
| `sandbox rm <name> [--force]` | Delete a profile. Refused while sessions reference it unless `--force`; the place tree and the profile's login are kept while anything still names it. |
| `sandbox prune [--profile <name>] [--dry-run]` | Reclaim superseded and orphaned places, through the same decision the TUI's background pass makes. |
| `sandbox export [<name>] [--output <file>]` | One profile, or every profile, as a `[[profile]]` TOML document — the *human* rendering, so a redirected stdout follows the CLI-wide JSON default. `--output` refuses to overwrite. |
| `sandbox import <file> [--replace]` | Validate the **whole** document, then write it in one transaction. |
| `sandbox exec --profile <name> [--cwd <dir>] -- <command…>` | Run one command inside a profile's boundary and report what applied. |
| `sandbox token set <agent> [VAR]` / `token rm <agent> [VAR]` / `token list` | The `env-token` value in friring's own OS keychain entry. `VAR` may be omitted when the agent declares exactly one. |
| `sandbox relay` | **Internal** — see below. |
| `sandbox launch` | **Internal** — see below. |

Eight things about it are deliberate:

- **`sandbox exec` never falls back to the host.** It is the way to ask "does
  this profile actually let that through?" without spawning a session, and it is
  what the boundary probes (`scripts/dev/sandbox-probes/`) use to observe the
  deny set against a real kernel rather than against generated policy text. A
  profile that cannot be applied here refuses, whatever its
  `allow_unsandboxed_fallback` says: that switch means "start the session
  anyway", there is no session, and "it ran outside the boundary" is not an
  answer to the question that was asked. It refuses a **place** too (an
  environment rather than a wrapper — running one command in one would create or
  adopt a container as a side effect of a question) and an **egress-filtered**
  profile (whose proxy lives in a running friring; a one-shot process would bind
  a listener, exec, and take it away). The command's own exit status becomes
  friring-cli's, so a probe asserting "this was refused" gets the refusal.
- **A token is never an argument.** `token set` takes no value: it reads stdin
  when one is piped, otherwise it prompts with echo off, and the value is never
  rendered, logged or quoted back in a refusal. Every refusal about *which*
  entry — an agent that declares no `secret_env`, a variable it does not declare,
  a spelling the platform tool would read as a flag — is raised **before** the
  value is asked for, and so is "this host's store cannot be written to", so a
  refused token is never one you have to rotate. `token list` answers whether
  friring holds each variable an agent declares, never what it holds — and asks
  the store's *existence* question rather than reading and discarding: on macOS
  that is `security find-generic-password` without `-w`, so a listing over a
  dozen declared variables neither pulls your tokens into friring's memory nor
  raises a keychain prompt per entry. It therefore reports an entry cleared to
  whitespace as held, while a launch treats it as absent.
- **`show` renders a profile; it does not validate one.** It prints the stored
  fields, the profile's places, any column friring could not decode, and whether
  the backend is available on this host. The path and boundary refusals are made
  where a profile is written or used — the editor's save and an import, and again
  at the launch itself — so a legacy row a launch would refuse still prints here.
- **An export is TOML for a human and JSON for a pipe.** The `[[profile]]`
  document is the *text* rendering, so a redirected stdout follows the CLI-wide
  JSON default like every other command. The two spellings `sandbox import`
  reads back are `friring-cli sandbox export --text > profiles.toml` and
  `friring-cli sandbox export --output profiles.toml`.
- **An import is refused for anything a launch would refuse**, in the launch's
  own words: a read-write root reaching the data directory or the database file
  itself, one reaching friring's own `agents.toml`/`hosts.toml`/`config.toml`, a
  path in either mode reaching friring's sandbox state or a container engine's
  control socket.
  A key friring does not recognise is refused rather than ignored — a typo'd
  `network_alow` would otherwise import a boundary wider than the document says.
- **A headless prune protects more than the TUI's pass does.** `friring-cli`
  drives no session, so it cannot know which container one is in: it protects
  every place of every profile a live session names, matching a container on
  **both** names it can answer to — the row friring recorded it under and the
  label it was created with, which stop agreeing after a profile rename. An
  engine that will not answer is skipped whole rather than read as holding
  nothing. It never walks WSL distros: nothing in this build registers one.
- **`sandbox relay` is internal.** It is the half of the egress firewall that
  runs *inside* a boundary, offering a TCP endpoint on the sandbox's own loopback
  and forwarding each connection to the bind-mounted proxy socket, because no
  HTTP or SOCKS client can dial a unix socket. Friring composes the command
  itself; there is nothing to run by hand. It holds no credential and makes no
  policy decision.
- **`sandbox launch` is internal too**, and is the process a policy boundary
  runs *instead of* the agent (ADR-33):

  ```text
  friring-cli sandbox launch [--gate <dir> --key <key> --timeout <secs>]
      [--relay-listen <addr> --relay-socket <path>] [--unset <VAR>]... -- <agent argv…>
  ```

  It starts the relay when there is one, removes each `--unset` variable, waits
  for `<gate>/release` to hold `<key>` when there is a gate, and then `execvp`s
  the agent argv verbatim, in place. A gate that never opens exits `75`. Every
  value is its own argv element and nothing is parsed by a shell. Friring
  composes it; there is nothing to run by hand.

  Both it and `sandbox relay` are dispatched **before the database is opened** —
  ADR-29 keeps the database out of every sandbox — which is what `cli/early.rs`
  is for.

## The bridge (inside a sandbox)

`friring-cli bridge` is the orchestration bridge's client (ADR-30): the one
command a **sandboxed** agent runs to ask friring for something. Like `sandbox
relay` and `sandbox launch` it is dispatched before the database opens, because
ADR-29 keeps friring's SQLite file out of every boundary and this runs inside
one.

| Command | Does |
|---|---|
| `bridge create --repo-root <p> --branch <b> --agent <a> --task-body <t> [--task-kind k] [--role-hint r] [--depends-on <child>]…` | Create one child session on a **new** worktree cut on `--branch` (see the branch rules below). `--depends-on` is repeatable and each named child must be `done` first, or the create is refused `dependency_unfinished` |
| `bridge stop <child> [--grace-secs N]` | Ask a child to finish, then stop and verify it. A clean `stopped` child releases its slot but keeps its ownership, worktree and agent state for an explicit resume |
| `bridge resume <child>` | Relaunch a dirty, stalled, stopped or unusable child. Resuming a released `stopped`/`unusable` child first reacquires fan-out capacity |
| `bridge status` | This session's own state, and its children's |
| `bridge inbox [--claim] [--limit N]` | Read this session's own mail |
| `bridge send --to <child\|owner> --kind <k> --body <b>` | Mail its owner, or a child it owns |
| `bridge report --phase <p> [--progress N] [--summary s] [--needs-operator] [--artifact <path>]…` | A bounded progress note about itself. `--needs-operator` raises the attention badge and mails the owner; `--artifact` is repeatable and each path must be inside this session's own worktree |

Every verb takes the same five flags:

- `--key` — the idempotency key. Minted when omitted; supplying it is what makes
  a retry safe.
- `--timeout` — how many seconds the **client** waits for a response file,
  default `120`. It ends the wait, never friring's work: a `create` that outlives
  it is still building a child, so a retry must reuse the same `--key` to be
  handed that child rather than to start a second one.
- `--ack` — delete the response file once it has been read.
- `--deadline` — absolute Unix **milliseconds** after which friring must not
  *start* this work; a request that arrives late is refused `expired`. A key that
  already has a journal row replays its recorded answer instead of expiring,
  because refusing it would destroy the only record a caller that timed out has
  of the child it already created.
- `--human` — print a readable summary. JSON is the default, because the caller
  is almost always an agent parsing the answer.

`--task-body`, `--body` and `--summary` accept `-` to read the value from stdin,
which is how a long task avoids a command line.

Four things about it are deliberate:

- **The channel is the identity.** Nothing in the protocol says who the caller
  is, and there is no field for it. Authority comes from *which* directory the
  request was written into: friring minted one bridge directory per session and
  exposed exactly that one inside that session's boundary, so a request in it is
  by construction a request from that session. A caller-supplied session id would
  be a claim, and a claim is not evidence. `FRIRING_BRIDGE_DIR` names it, inserted
  on the sandbox *policy* so an agent that declares the variable in `agents.toml`
  cannot point the channel elsewhere.
- **The verbs are a closed set.** There is no `exec`, no arbitrary
  `friring-cli`, no SQL, no pane capture and no way to name another session's
  anything. Every verb acts on the caller's own session or on a child it provably
  owns, and a verb friring does not know is refused rather than passed through.
- **Nothing is a fallback.** A queue that cannot be found, a friring that never
  answers, a response that will not parse: each is an error and none is a
  degraded mode. A client that quietly did nothing and exited zero would let a
  leader believe it had created a child that does not exist. A refusal *is* a
  well-formed answer — it is printed, and the exit status is non-zero so a shell
  chain stops.
- **The key makes a retry safe.** The same key with the same body returns the
  first attempt's exact bytes rather than doing the work twice; the same key with
  a *different* body is refused `key_reused` and has no effect at all. A client
  that timed out should retry with the same `--key`.

**What `create --branch` accepts.** The name decides a path — a child's worktree
is cut under friring's worktree root — so it is checked twice and neither answer
is trusted to imply the other:

- **git's own answer.** `git check-ref-format --branch` must accept it, which
  rules out `..`, a leading `-`, control characters, `~ ^ : ? * [`, `@{`, a
  trailing `.lock`, and the rest of that grammar. Asked of git rather than
  reimplemented.
- **it must still name one directory.** The sanitized form — every `/` replaced
  by `-` — has to be a single ordinary directory name: nothing empty, no `.` or
  `..`, no separator left. Slash-separated names are therefore fine (which is why
  the omx extension's `omx/<slug>/<node>` branches work); what is refused is a
  name that would still be a path afterwards.
- **at most 200 bytes** (not characters — a multi-byte name reaches the cap
  sooner), since it becomes a path segment on some filesystem eventually.

A create is also refused when a worktree **already exists** at the path that name
resolves to, and when another create resolving to that same path is **still in
flight** — the directory is cut off the tick, so two creates sent together would
otherwise both see it missing and end up sharing one workspace. Note that this is
about the *directory*, not the spelling: `feat/one` and `feat-one` are two branch
names for one worktree. friring cuts a child's workspace; it never attaches one
to a directory it did not just make.

It is refused for the **branch** on the same grounds: `create` always cuts a new
one (`git worktree add -b`), so a branch that already exists in the repository is
refused even when nothing has a worktree on it. Only `resume` reuses a child's
existing branch and worktree — it relaunches the child friring already made
rather than making a second one.

The protocol is a JSON file renamed into `<dir>/req/<key>.req` and an answer
renamed into `<dir>/res/<key>.res`, so any program with `rename(2)` and a JSON
parser can drive it without this client.

## `friring-cli capabilities`

What this friring's bridge offers — protocol version, capability names, verbs
and error codes — printed from the constants the broker itself uses, so a version
it reports is one it speaks. An extension's `binary-capability` requirement is
checked against it.

```json
{
  "bridge": {
    "protocol": 1,
    "capabilities": ["child-lifecycle", "mailbox", "report"],
    "verbs": ["create", "stop", "resume", "status", "inbox", "send", "report"],
    "errors": ["expired", "key_reused", "..."]
  },
  "extension_requires": 1
}
```

## Session egress fields

`session get` and `session list` carry three fields about a sandboxed session's
way out, and they answer different questions from `sandbox_unenforced`:

| Field | Means |
|---|---|
| `egress_state` | `none` \| `preparing` \| `active` \| `restoring` \| `unrestorable` |
| `egress_unrestorable_reason` | Why a restart could not rebind it, or `null` |
| `egress_endpoint` | `tcp:<port>` or `unix:<path>` — where it listened |

A session whose proxy could not be rebound after a restart is still **sandboxed**:
its kernel policy holds and its agent has no network rather than an unfiltered
one. Reporting that as `sandbox_unenforced` would say the agent is running on the
host, so it is a separate field. The credential the proxy demands is never
reported: nothing that renders a session may carry it.

## Agent metrics

Four commands expose what an agent is costing, in the four shapes friring
collects. They differ from `perf` in the way that matters: `perf` reads a blob
a *running TUI* publishes, while these read the **same sources the TUI reads**
and therefore work with **no TUI running** — the normal case for cron and
scripts, since sessions outlive the TUI inside tmux.

| Command | Reports | Source |
|---|---|---|
| `session metrics [<uuid>\|--all]` | model, cost, token totals, context use, lines +/- | the agent's statusline JSON under `FRIRING_METRICS_DIR` |
| `session resources [<uuid>\|--all] [--cpu]` | summed RSS + process count of the agent's process tree | the machine's process table, rooted at the pane pid |
| `session activity [<uuid>\|--all]` | commands / edits / reads / subagents / tokens / touched files | the agent CLI's own transcripts (the F9 view's sources) |
| `usage [--agent <name>]…` | account rate-limit windows, plan tier | the vendor's usage API on the target host |

**Nothing is cached into SQLite.** Writing metrics on the TUI's tick cadence
would bump every *other* friring connection's `data_version` and force a full
shared-state reload on each poll — the reason `App::publish_perf_snapshot` is
gated behind a debug flag. The sources are cheap, so each command re-reads
them and is never stale. The consequence is that these commands have **no
history**: the statusline file holds current totals and is overwritten in
place, so cost-over-time is not derivable from them.

Coverage matches the TUI's, including its gaps. All three per-session commands
are **local-only** and report `null` with a `note` (never a zero) for any
**off-host** session — an ssh/wsl host *or* a sandbox place. friring never
injects `FRIRING_METRICS_DIR` into an off-host agent, and the process table and
transcripts are not on this machine either: for ssh/wsl the sources live on the
host, and for a place the metrics directory sits under friring's data directory,
which no sandbox is ever given (ADR-29). `usage` is the exception —
it reads credentials wherever they are, so `--host <name>` queries a host from
`hosts.toml`. One gap is the CLI's own: `session activity` reads the session's
**main transcript only** — the F9 view folds Claude subagent and workflow
transcripts into its counts from the TUI's cc tree scan, which is TUI state, so
delegated work is absent from the CLI's counts, files and tokens.

Cost varies by three orders of magnitude, which is why these are separate
commands rather than one: `metrics` is a file read, `resources` is one process
sweep plus one tmux call for any number of sessions, `activity` parses the
session's transcript from scratch, and `usage` reaches the network (and spawns
`codex app-server` for codex). `usage` fetches its agents concurrently under a
single `--timeout`.

`--cpu` is opt-in on `resources` because CPU is a *rate*: it needs two samples,
so it delays the command by `--cpu-sample-ms` (default 200). Memory is
instantaneous and always reported.

A single UUID returns the object (like `session get`); `--all` returns an array
(like `session list`). Both carry `session_id`/`name`/`agent` per row, so an
`--all` sweep needs no second call to identify rows.

### Wiring the statusline (opt-in, and why friring can't do it for you)

`session metrics` reads a file **the agent writes**, not one friring produces:
friring injects `FRIRING_METRICS_DIR` and `FRIRING_SESSION_ID` into every local
agent process (see `docs/CONFIG.md`) and reads back
`$FRIRING_METRICS_DIR/$FRIRING_SESSION_ID.json`. Until something writes that
file the command reports `no statusline metrics file written yet`.

**Only this command needs it.** `session activity` already reports token
tallies with no statusline at all — it reads the agent's transcript, which
records per-message `usage`. What the transcript does *not* carry is
`total_cost_usd` (Claude computes it client-side), the context-window
percentages, and the lines +/- tally. Those four are the whole reason to wire a
statusline; if you don't need them, skip this section.

**Why the hooks extension can't just wire it.** friring auto-wires status
*hooks* (`friring-cli session signal`) through a managed settings file passed
as `--settings`, and that is safe because **hook entries merge across settings
scopes** — ours are added to yours, never instead of them. `statusLine` is a
single scalar object, and scalar settings **override**: the `--settings` scope
outranks your `~/.claude/settings.json`, so a friring-managed statusline would
silently replace whatever statusline you had, with no way to compose the two.
Verified against claude 2.1.224 — with a user statusline and a `--settings`
statusline both configured, only the `--settings` one renders. So this stays
opt-in and hand-wired rather than becoming an extension that eats a UI surface
you own.

**The same caution applies to you.** Setting `statusLine` replaces your current
one, so if you already have a statusline, add the two recording lines to *your*
script rather than pasting this one over it. The payload is passed on stdin and
your script's stdout is what renders, so recording it is additive — a statusline
that saves the JSON and still prints your own content costs you nothing:

```sh
#!/bin/sh
input=$(cat)
# Record for `friring-cli session metrics`. Both vars are injected by friring
# into local sessions only, so this is inert outside one.
if [ -n "$FRIRING_METRICS_DIR" ] && [ -n "$FRIRING_SESSION_ID" ]; then
    mkdir -p "$FRIRING_METRICS_DIR"
    printf '%s' "$input" > "$FRIRING_METRICS_DIR/$FRIRING_SESSION_ID.json"
fi
# Whatever you want on screen — this is where your existing statusline goes.
printf '%s' "$input" | jq -r '"[\(.model.display_name)] \(.workspace.current_dir)"'
```

Then point `statusLine` at it in `~/.claude/settings.json`:

```json
{ "statusLine": { "type": "command", "command": "~/.claude/friring-statusline.sh" } }
```

The `claude-metrics-cli` e2e scenario seeds this snippet's recording half, so
the documented contract is asserted rather than assumed (`docs/E2E.md`).

One field note: as of claude 2.1.132, `context_window.total_input_tokens` /
`total_output_tokens` report *current context usage*, not cumulative session
totals — so `session metrics` token columns track the live window, while
`session activity` tokens are cumulative over the transcript.

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

The same JSON carries both halves of a session's sandbox, and they answer
different questions (`docs/SANDBOX.md`):

- `sandbox_profile` — the boundary the session **asked for**. It survives a
  launch that could not deliver it, because that is what the next relaunch
  rebuilds from; `null` = the agent asked for no boundary.
- `sandbox_unenforced` — why the last launch did **not** deliver it: the reason
  the profile could not be applied and `allow_unsandboxed_fallback` let the
  agent start on the host anyway. `null` = no launch recorded a complaint.
  `session create`/`session restart` report their own launch's verdict under
  the same key, and print `NOT sandboxed — <reason>` on the human output.
- `sandbox_login` (`session create` only) — what to type **in the session's
  pane** to sign the agent in, when the boundary went on and the agent has no
  credential inside it. `null` whenever there is nothing to do, which is every
  policy-backed launch and every container that already holds a login. Not
  persisted and not on `session get`/`session list`: it is only true of the
  launch that computed it, and a stale "sign in" against an agent that has since
  signed itself in is a prompt for work nobody needs to do.

A non-null `sandbox_profile` therefore does **not** mean the agent is
sandboxed — check `sandbox_unenforced` too. Both are persisted, so they answer
for a session this friring only adopted, and `session get`'s human output folds
them into one `sandbox` row (`dev — NOT enforced: <reason>`).

Two things about a **sandboxed** session created from the CLI rather than the
TUI. An egress proxy lives in the process that started it, so one started here
dies when the command exits: the agent keeps running under tmux with the kernel
policy still denying everything, which fails closed, and a relaunch from a
running friring restores its egress. And this path hands the window its
environment as `tmux -e KEY=VALUE` arguments rather than over a control-mode
socket, so while the command runs the proxy URL — token included — is visible in
that `tmux` client's argv to anything on the machine that can read a process
list. Both are covered under
[Failure modes](SANDBOX.md#failure-modes). A **place**-backed session is exempt
from the second: it spawns over the same control-mode connection the TUI uses,
so nothing of its environment reaches a process table. A launch that would have
to put a **credential** on that argv is refused outright rather than exposed —
"start the session from the TUI instead".

A place-backed session persists `backend_type = sandbox:<profile>`, which is why
`session restart` refuses one exactly as it refuses an `ssh:` session: the
headless restart drives the *local* tmux, so it would find no window to kill and
would spawn an unsandboxed agent on the host. Restart it from the TUI, which
reaches the place through its transport.

The **first** launch of a place-backed session is composed before its row says
`sandbox:<profile>`, so the variables naming friring's own config, data and
metrics directories are injected as they would be for a local session — and then
taken back out where the launch learns it is place-bound. An in-place
`friring-cli` therefore sees none of them and resolves its own defaults, which
is what ADR-29 requires: the database is never mounted into a place, and naming
it would only point that CLI at a host path that does not exist in there.

## Delete and restore semantics

`session delete <uuid>` **soft-deletes** by default — only the DB row is marked
deleted (the TUI tears down the tmux window/worktree on its next sync), and
`session restore` revives it. Pass `--force`
(`session_ops::delete_session_headless`) to also kill the tmux window, remove
worktrees + the symlink workspace, and disable `send` automations targeting the
session — for headless cleanup when no TUI is running. Teardown is best-effort
(failures land in the JSON report); the row is always soft-deleted last.

**Bridge children are the exception, in both directions.** A *non-forced* delete
of a **live** bridge child is **refused**: without `--force` there is no runtime
teardown, so the row would be marked deleted and its owner told the child was
gone while its agent kept writing the worktree a later verdict reads. The path
that stops one is the owner's `friring-cli bridge stop <child>`, which has
friring verify the pane died first; `--force` is the operator's override and does
tear the runtime down. In the other direction, `--force` on an **owner** first
stops every live child it has (`stop_owned_children`) — the owner is the only
session that could ever answer a child's `blocked`, read its `result` or
integrate its branch. Each child's recorded state is what the teardown
*achieved*: a pane that would not die is `stop_failed` and keeps its slot, and so
is a child whose own session row friring could not read — nothing was torn down,
so nothing was verified dead. The
children's own rows, branches and worktrees are **preserved** — only the owner is
deleted — and the report carries `stopped_children` (also a `stopped children`
line in the human summary).

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
