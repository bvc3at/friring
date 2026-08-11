# Sandboxed agents

How Friring runs a coding agent inside an isolation boundary — the profile
model, the per-OS backends, the egress firewall, credential handling, and the
UI that ties them together.

This document is the **design contract** for the feature: implementation work
reads it, and any change to the behaviour described here updates it in the same
change. Fork-visible divergences are also listed in [`FORK.md`](../FORK.md).

**Friring — fork-only.** Upstream has no sandboxing; see
[`FORK.md`](../FORK.md).

Deciding whether to rely on this? [**Not in it**](#not-in-it) is the complete
list of what the feature does not do, in one place.

---

## Goals and non-goals

**Goals.**

- Run *any* agent from the registry (`agents.toml`) inside a sandbox, without
  the agent knowing. Friring stays agent-neutral: what an agent needs in order
  to survive being sandboxed is **declared data**, never special-cased code.
- Scope a sandbox to a folder, a worktree, or a combination of them, with
  per-path read-only / read-write intent.
- Restrict egress to an allowlist of domains, defaulting to deny.
- Offer every backend the host supports, and let the user pick per session with
  a sensible default rather than forcing one isolation technology.
- Keep sessions crash-survivable. A sandboxed session reattaches after a
  Friring restart exactly like an unsandboxed one.

**Non-goals.**

- Defeating a determined adversary. Native policy backends are *guardrails*:
  they share the host kernel, and a domain allowlist is bypassable through
  domain fronting and through any allowed domain that can host arbitrary
  content. Container and VM backends are stronger, but the honest framing is
  "reduce blast radius", not "prove containment". The UI says so.
- A general container-management product. Friring manages the instances it
  creates and nothing else.
- Native Windows process sandboxing (restricted tokens / AppContainer). Windows
  isolation is delivered through WSL2 and through containers. See
  [Backend catalogue](#backend-catalogue).
- **Isolating the sessions that share a place from each other.** A place is one
  container per profile, and the sessions in it run as one uid, in one pid
  namespace, over one filesystem. Per-session isolation is what a *policy*
  backend gives — one kernel policy per process tree — and what a place holding
  one session gives; between siblings in a place there is none, and the boundary
  that does hold is the place's. The lever is a profile per session, and the UI
  says so rather than leaving it to be assumed.
- **Detecting a pre-existing hardlink alias.** Both policy backends protect a
  file by *name*: seatbelt denies a pathname, bwrap covers one with a mask. A
  hard link made on the host *before* the launch — sitting inside a read-write
  grant, pointing at the database or at a secret — is a second name for the
  same inode that neither rule mentions, and it is reachable. This is the one
  stated exception to ADR-29's absolute, and it is written here rather than
  implied. What narrows it: the profile that would most plausibly host such a
  link is refused outright, because read-write roots may not enclose the data
  directory; a rename cannot bring a protected path under a grant, because
  `file-write-unlink` is denied on every writable anchor and on every ancestor
  of a protected path; and under bwrap the protected pathname resolves to
  `/dev/null`, so a link made from inside would alias that, not the database.
  What remains is a link the user planted themselves, and friring does not go
  looking for one. Closing it needs inode identity rather than pathnames — a
  Landlock or overlay-based boundary — which is a backend change, not a rule
  change.

## The two sandbox shapes

Every backend is one of two shapes, and the difference decides where tmux runs.

**Policy backends** apply a kernel policy to a process tree. They wrap the
agent's argv (`sandbox-exec -f … claude …`, `bwrap … claude …`). The tmux
window is *outside* the sandbox and holds the wrapped process. Nothing about
window discovery, pane-id reattach, `remain-on-exit`, scrollback capture or
restart changes: the sandbox is invisible to the session layer.

**Place backends** are environments that outlive an individual command — a
container, a VM, a WSL distro. The agent runs in a tmux server *inside* the
place, reached through a transport, exactly as an SSH host is reached today.
The place is created once per profile and shared by every session using that
profile.

```text
policy backend                        place backend
──────────────                        ─────────────
friring                               friring
  └─ tmux (host)                        └─ tmux (host, control mode)
       └─ sandbox-exec / bwrap               └─ docker exec -i <ctr> tmux …
            └─ agent                              └─ tmux (in place)
                                                       └─ agent
```

Consequences worth internalising:

- A policy sandbox costs milliseconds and reuses the host toolchain. A place
  sandbox costs a container start and needs an image containing the toolchain
  and tmux, with the agent either baked into that image or installed once into
  the profile's home — friring's default image carries no agent CLI, and a
  launch whose agent is not in the place is refused with the command that puts
  one there rather than opening a pane that dies at once.
- When a policy sandbox's process dies, one pane dies — the existing dead-pane
  UX. When a place dies, every session in it dies at once.
- Only place backends can enforce memory/CPU limits, and only place backends
  give a filesystem the host cannot see.

Sessions in one place are therefore **not isolated from each other**: one uid,
one pid namespace, one filesystem, so each can read the others' files, processes
and environment — the proxy credential and the first-use grants that go with it
included. The trust domain is the place, not the session in it. A profile per
session gives a place per session, and that lever is stated where a profile is
chosen: the profile editor's footer and the profile list's `shared place` mark.

Overlapping paths resolve **most-specific-first on both backends**: `repo`
read-only with `repo/work` read-write leaves `repo/work` writable, and the
reverse nesting leaves the descendant read-only. Each backend gets there its own
way — seatbelt emits both sets in one ancestor-before-descendant pass, because
the last matching SBPL rule wins; bwrap binds them in the same order, because
the last mount over a path wins — so the shared rule is a conformance test over
one table of profiles, not a shared implementation. The database and secret
denies are still final and win over any path grant, including one naming the
protected path itself.

Neither backend may grant more than the other for the same profile. Where a
kernel primitive is quietly wider, the wider backend takes the difference back
explicitly: bwrap's read-only host bind would leave the control-socket trees
connectable, and seatbelt's `(deny default)` would not, so bwrap masks them (see
its bullets below).

## Data model

Sandbox profiles are UI-edited collections, so they live in SQLite following
the automations pattern (list modal + editor modal + storage module + schema
migration), not in a TOML file. Only `settings.toml` has config write-back in
this codebase, and it is a fixed-schema file rather than a collection.
`friring-cli sandbox export|import` covers portability: profiles round-trip as a
`[[profile]]` TOML document, carrying the recipe and not the bookkeeping —
storage owns `created_at`/`updated_at`, and a profile holds no credential, so an
export is safe to commit. Import **validates the whole document before it writes
any of it**, in one transaction: the profile validator, the name collision
(`--replace` is the deliberate answer), and the same path refusals a launch makes
— a read-write root reaching the data directory or the database file itself, one
reaching friring's own `agents.toml`/`hosts.toml`/`config.toml`, a path in either
mode reaching friring's own sandbox state or a container engine's control
socket — in the same words, so a profile that could never launch is never
stored. A key friring does
not recognise is refused rather than ignored, because the ignored value may be
part of the boundary; and a row that did not decode is not exported at all, since
its columns hold friring's own substitutions.

The dormant upstream `containers`, `project_container_config`, `vms` and
`project_vm_config` tables (created by schema v8/v10/v11, referenced nowhere in
live code) are **superseded**: this feature adds its own tables and drops the
old ones in the same migration. Schema v22 already dropped all four, so the drop
is a safety net for a database that reacquired one (an upstream merge, a
hand-restored backup) rather than a cleanup — no reachable database still holds
that data. Recorded in `FORK.md`; upstream merges touching those tables conflict
and resolve toward the Friring tables.

### `sandbox_profiles`

| Column | Type | Meaning |
|---|---|---|
| `name` | TEXT PK | Profile identity, shown everywhere in the UI |
| `backend` | TEXT | `auto` \| `seatbelt` \| `apple-container` \| `bwrap` \| `docker` \| `podman` \| `wsl-distro` |
| `paths` | TEXT (JSON) | `[{path, mode}]`, `mode` ∈ `ro` \| `rw` |
| `network_mode` | TEXT | `none` \| `allowlist` \| `full` |
| `network_allow` | TEXT (JSON) | `["api.anthropic.com", "github.com:443", …]` |
| `network_deny` | TEXT (JSON) | Denies win over allows |
| `prompt_new_domains` | INTEGER | Ask on first use of an unlisted domain, remember the answer |
| `read_scope` | TEXT | `workspace` \| `host-minus-secrets` (policy backends only) |
| `memory_mb`, `cpus` | INTEGER | Place backends only; NULL elsewhere |
| `image` | TEXT | Place backends: image reference |
| `containerfile` | TEXT | Place backends: build source, alternative to `image` |
| `allow_unsandboxed_fallback` | INTEGER | Whether the agent may escape for a specific command |
| `created_at`, `updated_at` | INTEGER | Unix millis, the convention every other table uses |

Paths are stored as written (`~` preserved) and expanded at launch, so a
profile stays meaningful if `$HOME` differs on a remote host.

A read-write path is refused at launch if it **reaches** the friring data
directory (ADR-29), if it reaches a tmux server socket directory, or if it
reaches friring's own `agents.toml`, `hosts.toml` or `config.toml` — those write
down the command lines the host launches every agent with, so a sandbox that can
write one chooses what friring runs next, outside the boundary. "Reaches the data
directory" is deliberately **two** tests: enclosing the *directory*, or naming
the *database file* or its `-wal`/`-shm` siblings. They cannot be one test,
because friring mints a launch's own scratch and signal directories underneath
the data directory — so the directory can only be judged from above — while a
path naming a file encloses no directory at all and has to be judged from both
sides. A security-relevant path
that is not valid UTF-8 is refused too, because a rule built from a lossy
conversion names a different file. The first of those is also checked when the
editor saves — the check needs the data directory and the database path, which
the `session` layer may not resolve, so it lives in the save path rather than in
`SandboxProfile::validate` — so a profile that cannot launch is never stored in
the first place. Refusing rather than trimming is deliberate: a profile that
says "my home is writable" and silently is not produces a boundary nobody can
reason about.

`name` is a **case-insensitive identifier** (`COLLATE NOCASE`): two spellings
would be one container/distro name, so the database enforces the same rule the
validator does.

Because the name is the key, **renaming is its own operation** with referential
consequences: `rename_sandbox_profile` rewrites the profile row, its instances,
`sessions.sandbox_profile` and a place-backed session's `backend_type` in one
transaction, and the plain upsert deliberately refuses to move identity. The
editor calls the rename when the name field changed.

**Deleting a profile leaves referencing sessions dangling on purpose.** Clearing
the link would silently relaunch those agents on the host with no boundary; a
dangling name fails the next launch loudly instead, and the delete confirmation
reports how many sessions are affected. Deleting does not stop any real place —
tear those down first or they leak.

`allow_unsandboxed_fallback` is the visible, per-profile escape hatch. It decides
what happens when the profile **cannot be applied** at launch — an engine that is
not running, a mount that cannot be honoured, an image that is not there: off
(the default) fails the spawn, on starts the agent unsandboxed with the reason in
front of the user. The per-command escape the name also suggests is not built.

Falling back **never clears** `sessions.sandbox_profile`. The profile is the
desired boundary and the escape hatch is a property of one launch, so a
session that started on the host because its backend was momentarily missing
is sandboxed again on the next relaunch. What the fallback does instead is
record `SandboxState::Unenforced` on the live session and put the reason in
front of the user.

**A row friring cannot decode is listed, not launched.** Reading a profile is
deliberately lenient — a hand-edited or imported value must not hide the row
from the list modal, which is the only place it can be repaired or deleted —
but leniency has to pick a value, and the value it would otherwise pick is the
wider one: an unrecognised `read_scope` is `host-minus-secrets`, a malformed
`network_deny` is an empty deny list. So each undecodable column is recorded
beside the profile, the substituted value is the **narrowest** its column
allows (`workspace`, network `none`, empty lists, uncapped limits), the list
row reports `unreadable <columns>` in place of a summary of values friring
invented, the editor's footer names them and states that saving replaces them,
and every launch path refuses the profile with a message naming each column
and the text it refused.

### `sandbox_instances`

Tracks live places for the manager view and for garbage collection:
`profile`, `engine`, `external_id` (container id / distro name), `state`,
`created_at`, `last_used_at`. Policy backends never create rows here.

Keyed on `(engine, external_id)`, **not** on the profile: a rebuild leaves the
old container behind, and keying on the profile would overwrite the previous id
into an unfindable leak — precisely what the garbage-collection purpose exists
to prevent. One profile may therefore own several rows. `state` is free text: the vocabulary
belongs to whichever backend wrote the row, so a place backend friring gains
later needs no migration to describe itself. The container backend writes
`running` and nothing else — `ensure` returns when the place is up or not at
all. Refreshing a row is deliberately not an upsert (`touch_sandbox_instance`):
a launch reusing a place must not re-insert a row a collection pass has just
deleted.

### Session linkage

`sessions` gains a nullable `sandbox_profile` column, carried end to end like
`backend_type`: written by the full-row session upsert, preserved through
soft-delete so a restore comes back inside its boundary, and re-read on every
relaunch so an edited profile takes effect and a deleted one fails loudly.
A policy backend leaves `backend_type` alone — the boundary is invisible to the
session layer — so the column is the only record of it. Place-backed sessions
additionally set `backend_type = "sandbox:<profile>"`, mirroring how `ssh:<host>`
and `wsl:<host>` already drive restore and reattach. Both are written in the
same migration that adds the tables.

Schema v48 adds `sessions.sandbox_unenforced` beside it: the reason the last
launch could not apply the profile, or `NULL`. Unlike `sandbox_profile` it is
**not** part of the full-row upsert's unconditional write — a writer with no
launch verdict leaves it untouched, the same protection the hook columns get —
and a relaunch that does not rewrite the row (`restart_session_headless`)
records it with a targeted update.

## Backend catalogue

**Four of the five backends below are wired end to end**: the two policy
backends (`seatbelt`, `bwrap`) and the container places (`docker`/`podman`,
`apple-container`) probe the host, are chosen by the ladder or by pinning, and
run agents. Availability is probed per host, and the profile editor, the profile
list and the session-creation step all show the reason a backend cannot be used
here rather than leaving it to fail at launch.

**`wsl-distro` is the exception, and the honest statement is narrower than
"built".** Its code registers, hardens, adopts and reclaims a distro, and all of
that is unit-tested against a stub `wsl.exe` — but **nothing in the running
application calls any of it.** What is reachable in this build is the backend's
*probe* and its *capabilities* (so the editor, the list and the session step say
what a WSL place could and could not do, and a profile asking for something it
cannot enforce is refused at the save) and the launch refusal that names the two
ways to get a boundary on Windows today. No session launches into a distro, no
distro is ever registered or hardened by the application, and the reclaiming pass
does not walk distros at all — `PLACE_KINDS` lists the three container engines
and nothing else. See [its entry](#wsl-distro--windows-place) and
[Not in it](#not-in-it).

The three *container* place backends share the parts a boundary is made of, as
**code** rather than as three implementations of one description: one mount plan,
one owner-label set, one spec digest, one garbage-collection decision, and one
`PlaceBackend` seam that the launch path, teardown, the reclaiming pass and
`friring-cli` all drive. A second copy of those rules would be a second set of
escapes, and `sandbox::tests::place_conformance` holds every one of them — and
`wsl-distro`, through its own two seams — to the same refusals.

### `seatbelt` — macOS, policy

`sandbox-exec` with a generated SBPL profile, parameterised per session
(`-D WORKSPACE=…`). Deprecated in name since macOS 10.8 and fully functional
through macOS 26; it is what Codex CLI, Claude Code, Cursor, Bazel and Chromium
use, and Apple ships no replacement for sandboxing arbitrary headless
processes. Treat the deprecation as a real but low-probability risk mitigated
by the backend being pluggable.

- Per-path `file-read*` / `file-write*` scoping with `subpath` filters, plus
  `file-write-unlink` denies so a move cannot escape a write boundary. SBPL
  matches by pathname, so a deny is only as good as the path staying put: the
  unlink of every writable anchor *and* of every directory leading to a
  protected path (the database, each secret) is denied, which is what stops a
  rename from bringing a protected file out from under the rule naming it.
  bwrap needs no counterpart — a directory holding a mount point cannot be
  renamed.
- The generated profile is written under `<data dir>/sandbox/profiles/`, `0600`,
  refusing a symlink at the final component and staged through an `O_EXCL`
  sibling so a link planted in the race window is replaced rather than followed.
  It is *the policy*: a sandbox that could write it could rewrite what
  constrains it, which is why it does not live in the host temp directory and
  why no profile may enclose the data directory.
- **Keychain works.** Under a restrictive profile a process can still reach
  `securityd`, so Claude Code's Keychain-stored OAuth keeps working with no
  credential handling at all. A default-deny profile must explicitly allow
  `mach-lookup` for `com.apple.SecurityServer`, `com.apple.securityd`,
  `com.apple.trustd`, `com.apple.ocspd` and `com.apple.cfprefsd.daemon`.
- **Network granularity is all-or-localhost.** SBPL host filters accept only
  `*` or `localhost` (with ports); there is no domain predicate. Domain
  allowlists therefore mean: deny all outbound except the loopback proxy port.
  See [Egress firewall](#egress-firewall).
- **Nesting fails hard.** Under any outer profile containing a deny rule, an
  inner `sandbox_apply` returns `Operation not permitted`. Agents with built-in
  seatbelt sandboxes must run with them disabled; see
  [Inner agent sandboxes](#inner-agent-sandboxes).

### `apple-container` — macOS, place

Apple's `container` CLI (Containerization.framework): one lightweight VM per
container, sub-second boot, OCI images, virtiofs mounts. Apple Silicon only, and
**macOS 26 or newer**, which is where `container network create` exists. The
strongest boundary macOS offers — the kernel inside a place is not the host's —
and the weakest egress control of any place backend, which the probe says on its
own line rather than leaving to a launch.

- Mounts, labels, the synthetic home, the spec digest, the owner label and the
  collection decision are the container engines', shared as code: identical
  absolute paths through `--mount type=bind,source=…,target=…`, `.git/hooks`
  read-only inside every writable root, the same mount-source symlink refusal
  checked twice, the same ADR-29, tmux-socket and engine-socket refusals. A
  second copy of those rules would be a second set of escapes.
- **The egress proxy cannot be reached from inside a VM.** Every filtered mode
  is enforced by a proxy outside the boundary that a place reaches over a
  bind-mounted unix socket (ADR-27), and an `AF_UNIX` listener lives in the
  kernel that bound it: virtiofs carries the socket *file* into the guest, and
  `connect(2)` on it there finds no listener in the guest's own table. So
  `allowlist`, and `full` carrying denies, are **refused** rather than started
  believing they are filtered.
- **`none` is refused too**, in the other direction: the tool attaches every
  container to a network, and the networks macOS 26 adds separate containers
  from each other rather than from the internet — so friring cannot prove it cut
  the route, and a place that says `none` while reaching whatever that network
  reaches is worse than a refusal.
- **`full` with no denies is therefore the one network mode this backend can
  honour**, and the only one `Caps::network_modes` offers. The place still has
  the VM boundary and only the paths the profile mounted. Closing the gap needs
  an endpoint a VM can dial — a proxy listener on the place's own network
  address rather than on loopback or a socket — which is an ADR-27 change, not a
  backend change.
- Every place is attached to **friring's own network** (`friring-sandbox`,
  created once and adopted by name, because this tool has no owner label for a
  network) rather than the one every other container on the Mac shares. That is
  why macOS 26 is a hard requirement here rather than a nice-to-have. Places of
  two profiles share it, so it is not a boundary between them.
- **No `--cap-drop`, `--security-opt`, `--user` or `--userns`.** Those harden a
  process sharing the host's kernel; here the guest kernel is its own, and
  virtiofs performs host-side access as the user running the VM, so an
  identical-path bind mount stays writable without friring naming a uid.
- The CLI is young and its options move, so friring **reads its option surface**
  out of `container run --help` at probe time and refuses with the missing one
  named: `--detach`, `--name`, `--label`, `--mount` and `--network` are what a
  place is built from, and `--memory` / `--cpus` are checked against the profile
  that asks for a limit. `container system status` is this tool's "is the daemon
  running", `container exec -i` is what the transport needs, and both carry
  their own fix.
- No Compose and no Docker socket API, which this feature does not need. amd64
  images run under Rosetta inside the guest; friring passes no `--arch`, so the
  image's own architecture decides. friring publishes no image, so a missing
  default is refused with `container builder start && container build --tag
  friring/sandbox:1 --file packaging/sandbox/Containerfile packaging/sandbox`.

Everything above about the CLI's own surface — its flags, its JSON, its
subcommands — is **unverified against the real tool**, which needs Apple Silicon
hardware this fork was not written on. That is why each one is a runtime-probed
precondition that fails closed with the missing thing named, rather than an
assumption baked into an argv.

### `bwrap` — Linux and WSL2, policy

Bubblewrap: per-path `--ro-bind` / `--bind`, `--tmpfs`, `--unshare-net`,
`--unshare-pid`, `--die-with-parent`. This is the industry mainline — Codex
made bwrap its primary Linux backend and Claude Code's sandbox uses it too.

- Version 0.11 or newer adds unprivileged overlays (`--overlay`,
  `--tmp-overlay`), which back the optional copy-on-write workspace: the real
  directory is the read-only lower layer, the agent's writes land in an
  inspectable upper layer under `<data dir>/sandbox/overlay/<session>/`, and the
  merged view is mounted back at the path's own name. Availability is probed by
  mounting one, because the two answers that matter are invisible to a version
  check — a setuid bubblewrap cannot mount an unprivileged overlay at all, and
  neither can a kernel that refuses one from a user namespace. A host that cannot
  **refuses** a profile asking for it rather than binding the root read-write,
  which is the one outcome the mode exists to prevent. The layers deliberately do
  not live in the session scratch: that directory is writable from inside, and an
  `upperdir` a sandbox can redirect is every write in the boundary landing
  wherever it points.
  The overlay is emitted in the same sorted mount pass as every other path, so
  a nested read-only path still wins over it and everything friring takes back
  afterwards — `.git/hooks`, the secret masks, the ADR-29 database masks —
  still wins over the overlay. A nested read-**write** grant is that same
  ordering pointed the other way, and is **refused** with the nested path
  named: `--bind /repo/sub /repo/sub` lands after the overlay and puts the real
  host directory back over the merged view, so every write the profile calls
  discardable would land in the real repository. A profile that quietly means
  the opposite of what it says is worse than one that will not load. The layers
  go with the session: teardown drops `<data>/sandbox/overlay/<session>`
  alongside the scratch directory and the place session directory, and the
  per-session component carries a digest as well as the sanitised key — the
  sanitiser folds every character it does not accept onto `-`, and a session
  key is not always a UUID, so without it two sessions could share, or delete,
  each other's layers. **No profile column selects the
  mode yet**: the capability is built and reachable through
  `BwrapBackend::wrap_copy_on_write`, and adding the third `‹ ro | rw | cow ›`
  value to a path row is a profile change rather than a backend one.
- Requires unprivileged user namespaces. Probe for them: Ubuntu 23.10 through
  24.04 restrict `clone(CLONE_NEWUSER)` via AppArmor and need either the
  `bwrap-userns-restrict` profile or a sysctl; 25.04 and newer ship the fix.
  The probe failure message states the fix.
- Landlock is available as optional in-process hardening (per-hierarchy
  filesystem rights, port-level TCP from ABI v4). It cannot express host
  allowlists and is never the primary boundary.
- `/tmp` is a private tmpfs and is never re-bound from the host: it is where
  friring's own tmux server listens, and `--unshare-net` does not stop
  `connect(2)` on a pathname unix socket. The agent's scratch is a per-session
  directory friring mints under the data directory instead.
- The control-socket trees — `/run`, the user runtime directory, `/var/run`,
  and the directory tmux keeps its sockets in — are covered with an empty tmpfs
  under `host-minus-secrets`. A read-only bind is no barrier to a socket, and
  seatbelt's `(deny default)` already refuses unix-domain sockets, so without
  this bwrap would grant strictly more than seatbelt for the same profile. A
  path the profile lists inside one of them still wins. The cost is real and
  stated: masking `/run` breaks DNS on a systemd-resolved host under
  `network = full`, because `/etc/resolv.conf` points into it. The fix is
  first-class — list `/run/systemd/resolve` read-only in the profile.
- `bwrap` is resolved to an absolute path once, at probe time, and a copy
  living anywhere a sandboxed agent could rewrite (`$HOME`, `/tmp`, `/var/tmp`,
  `/dev/shm`, friring's own sandbox tree) is refused with the fix; so is one the
  profile itself makes writable. The user's environment must not choose what
  applies the policy — the same rule `/usr/bin/sandbox-exec` follows by being
  absolute.

### `docker` / `podman` — everywhere, place

The universal fallback and the only isolation available to a **native Windows
binary** user who is not going through WSL. Also the backend to pick when the
workload needs a toolchain the host does not have.

- Podman rootless is preferred on shared and remote hosts (no daemon, no root
  socket). It is CLI-compatible enough that one backend implementation covers
  both, with a probe distinguishing them: the `info` template that names a
  version differs, and so does how a container is given the host user's
  identity.
- Mounts use **identical absolute paths**, `--mount type=bind` rather than `-v`
  so a missing source is refused rather than invented as a root-owned directory,
  and every refusal is friring's own sentence naming the profile's path. A path
  that cannot be spelled in a `--mount` value (a comma, a quote), one that would
  carry the data directory **or the database file itself** across (ADR-29 — the
  directory refused from above, the file and its `-wal`/`-shm` siblings from
  either side), one naming friring's own sandbox-state or status-signal tree, a
  tmux socket directory or the engine's own
  control socket, and two paths landing on one target inside are all
  refused. The tmux socket directory is refused from below as well as above: a
  profile naming one server's `tmux-<uid>` directory encloses nothing, and a
  read-only bind is no defence, because a read-only superblock does not take
  write permission away from a socket inode — `connect(2)` still succeeds.
- **A mount source is judged as the kernel resolves it, and is refused if it
  travels through a symlink.** The string friring checks is the string the engine
  hands over, and the kernel resolves it *again* when it sets the bind up, so a
  source reached through a link means one thing to the check and another to the
  mount. This needs no user error: a plan adds `<writable root>/.git/hooks` by
  itself, and an agent inside a place can replace that path with a link to
  friring's tmux socket directory for the next ensure to mount. A legitimately
  symlinked path is refused too — friring cannot tell the two apart — with a
  message naming the link and where it points. The check runs twice, once when
  the plan is built and once immediately before the container is created; what is
  left is the window between friring's last `realpath` and the engine's own
  resolution of the same string, which nothing outside the engine can close.
  friring's own place directories are judged from `<data dir>/sandbox/pl` down:
  everything above it is unreachable from any sandbox (ADR-29 keeps the whole
  data directory out), and a machine whose data directory sits behind a link —
  `/var` → `/private/var` on macOS — would otherwise be refused its own egress
  directory. Where the engine runs on another host, a source is not on friring's
  filesystem to resolve and the launch's own existence check is the only
  authority; the symlink rule does not apply there.
  A refused source is never *rewritten* to its resolved spelling: rewriting one
  would mount a directory the profile does not name, and identical absolute paths
  are what keep a git linked worktree working (see below).
- **A filtered mode is measured, not assumed.** These are the only backends
  whose boundary may or may not carry the egress proxy's socket depending on
  the *host*: an `AF_UNIX` listener lives in the kernel that called `bind(2)`,
  and on a Mac or a Windows box the daemon's kernel is a VM's, not friring's.
  Rather than keep a list of which engines are VM-backed where — a list that
  goes stale — friring asks: when a filtered profile's place is ensured it
  binds a listener in the place's own directory (mounted at the same absolute
  path), has the place dial it, and treats its own `accept` as the answer.
  Nothing is read from the connection, so no dialer's output can manufacture a
  pass. A place that dials is remembered for as long as it lives; one that is
  turned down, or that friring could not get an answer out of, refuses the
  profile with the same argument `apple-container` makes and the same three
  ways out — an engine on this machine's kernel, `full` with no denies, or
  seatbelt/bwrap. Refusing costs a usable sandbox; starting it would give the
  place `--network none` and a relay dialling nothing, with the UI reporting
  the allowlist applied. The dialer is `tmux`, which the image contract
  already requires and which is a unix-socket client by nature; a one-shot
  connect in `friring-cli` would read better, but that binary inside a place
  is the *image's*, so a new subcommand would refuse every image built before
  it.
- **The engine's control socket is never mounted**, wherever it lives:
  `/var/run/docker.sock`, `/run/podman/podman.sock`, the per-user trees a
  rootless engine or Docker Desktop keeps one in (`/run/user/<uid>`,
  `$XDG_RUNTIME_DIR`, `~/.docker`, `~/.local/share/containers/podman/machine`),
  and whatever `DOCKER_HOST` or `CONTAINER_HOST` names. This is deliberately not
  the explicit-loopback escape hatch: that hatch shares one service the user
  chose, and the engine socket is the ability to start a privileged container
  with the host's filesystem in it — the whole host, which no profile can
  coherently intend. The same refusal applies to a **policy** profile, where the
  socket would otherwise be reachable: bubblewrap masks `/run` and `/var/run`
  only under `host-minus-secrets`, and a path the profile lists explicitly wins
  that mask.
- **A profile that makes the engine binary writable is refused**, the rule and
  the sentence bubblewrap already applies to itself: an engine at a user-writable
  prefix (`/usr/local/bin`, `/opt/homebrew/bin`) plus a profile granting that
  prefix read-write lets the sandbox replace the program the host runs next. Both
  spellings are compared, so an engine reached through a symlink is judged by
  where it lands — as it is at probe time.
- Network is `--network none` for every mode but an unrestricted `full`, plus
  the bind-mounted proxy socket.
- **The container runs as the host user** — `--user uid:gid` under a rootful
  engine, `--userns=keep-id` under rootless Podman, and nothing under rootless
  Docker, whose container root already *is* the unprivileged host user. That is
  what keeps an identical-path bind mount writable and what lets the sandbox
  connect to the `0o600` proxy socket **without widening it**.
- `--cap-drop ALL`, `--security-opt no-new-privileges` and `--init`, none of
  them configurable. The stated cost: a place cannot `sudo apt-get install` at
  runtime; tools belong in the image.
- **One instance per profile**, lazily started and shared by that profile's
  sessions. The container's name carries a digest of everything a profile edit
  could change (mounts, limits, image, network, user), so an edited profile asks
  for a *new* container rather than silently reusing one whose mounts no longer
  describe it, and the superseded one stays findable until garbage collection
  reclaims it.
- friring touches **only what it created**: an owner label is set at creation,
  every lookup filters on it, every removal re-checks it, and a same-named
  container without it is neither adopted nor removed — the launch is refused
  instead.
- `memory_mb` and `cpus` become `--memory` and `--cpus` here, the one shape that
  can enforce them.
- Reached with `<engine> exec -i <container> tmux …`: `-i` carries the
  control-mode protocol, and `-t` is deliberately absent because a pty makes the
  engine translate line endings through a line-delimited protocol.
- The image is the profile's `image`, the image built from its `containerfile`,
  or the default tag `friring/sandbox:1`. friring publishes no registry image,
  so a missing default is refused with the command that builds it from
  `packaging/sandbox/Containerfile`.
- **The image carries no agent, by design.** Baking one in would put a vendor's
  release train inside the image contract, and choosing which vendors to carry
  is a decision friring's agent-neutrality does not make. An agent reaches a
  place either through the profile's own `image`/`containerfile` or through a
  one-time install into the profile's synthetic home, which lives outside the
  image and survives every container the profile rebuilds — the same directory
  `volume-login` keeps its credential in, so one install and one sign-in per
  profile. friring composes that install as a *throwaway* container of the same
  image: the place's own network is whatever the profile granted it, and an
  install made in the image is built for the image's architecture rather than
  the host's. Every launch asks the place for the agent it is about to run
  (per launch, not per place — sessions of one profile may run different
  agents) and refuses with that command when it is not there.
- **`full` with no denies joins the engine's default network**, which is the one
  mode with no `--network none` and no proxy — so the container can reach the
  host at the bridge gateway (`172.17.0.1`) and anything the host binds on
  `0.0.0.0`. ADR-27's refusal to let the proxy dial its own machine does not
  apply, because under this mode there is no proxy in the path. That is the
  honest reading of "unrestricted", and the reason every other mode gets
  `--network none`; a profile that wants a way out but not the host's own
  services should use `full` **with** denies, or `allowlist`.

### `wsl-distro` — Windows, place

**Nothing in the running application drives this backend.** Everything in this
section describes code that exists and is tested against a stub `wsl.exe`, and
that no code path outside the tests calls: `SandboxHost::wsl_distro` — the only
accessor that hands the backend out as a place — has no caller, and
`PLACE_KINDS`, which every lifecycle, teardown and reclaiming path walks, lists
the three container engines only. So on a real machine **no distro is
registered, hardened, adopted, reclaimed or unregistered by friring**, and no
session runs inside one. What the running application does with this backend is
exactly two things: it *probes* it (so the UI can say whether a WSL place is
possible here and why not), and it *refuses a launch* pinned to it, naming the
two ways to get a boundary on Windows today. Read the rest as the design and
the tested implementation of a rung that is not yet plugged in — not as a
boundary anything is relying on.

One cloned distro per profile, addressed by the existing `wsl.exe -d <distro>`
transport with no new plumbing. Templates come from `wsl --export --format vhd`
plus `wsl --import --vhd`; teardown is `wsl --unregister`.

The decisive constraint: **all WSL distros share one utility VM, one kernel and
one network namespace.** Per-distro firewalling is impossible at the Windows
layer (Hyper-V firewall rules scope to the whole VM and accept IPs, not
FQDNs), and `iptables` set in one distro applies to all. Per-sandbox egress
control inside WSL therefore comes from running the `bwrap` backend *inside*
the distro, whose `--unshare-net` creates a real per-sandbox namespace. Memory
and CPU caps are global to the VM, so they are reported as unavailable rather
than faked.

Repositories should live on ext4 inside the distro. Mounting a Windows-side
folder pays a 10–100× metadata penalty and weakens the boundary; the UI warns.

The template is the profile's `image` — a distro already registered on this host
— or the host's default distro. A WSL1 template is refused (no utility VM, no
VHD), and so is one of friring's own sandbox distros: cloning it would copy that
profile's whole filesystem, its agent's login included, into a second boundary
(ADR-28).

The clone is `friring-sbx-<profile>`, and it is **hardened before anything runs
in it**: `/etc/wsl.conf` gets `automount enabled = false` and `interop
enabled = false` (with `appendWindowsPath = false`), written with the ownership
marker `/etc/friring-sandbox` in one script, then `wsl --terminate` — **whose
result is checked** — so WSL reads the file at the next start. That check is what
makes the verification after it mean anything: WSL applies `wsl.conf` only when a
distro *starts*, and writing the file started this one, so a stop nobody looked
at would leave the bytes on disk saying "hardened" while every later command
reaches the unhardened instance the import left running. Reading the file back
then confirms the file and nothing else. A registration whose distro would not
stop is refused, and rolled back.

Both settings remove the *default* exposure rather than containing anything:
automount puts every Windows drive, and friring's data directory with it, inside
the place, and interop lets a process in there `execve` a Windows binary that
runs outside the VM. Neither is a boundary against a process that is already root
inside the distro — which the distro's own user is, and which can mount a Windows
drive by hand whatever `wsl.conf` says. Per-path containment in there is
bubblewrap's, which is why it is required rather than optional.

A distro whose `/etc/wsl.conf` is no longer the one friring
wrote is refused as interference, outside `allow_unsandboxed_fallback`. That
check is a **tamper detector on the file**: on the adopting path the distro may
already be running, and friring neither restarts it to find out nor reads the
state `wsl --list` reports, which is a localised string nothing here compares.
Ownership
is that marker plus the name prefix, both re-checked immediately before
`wsl --unregister`; a distro that will not say who it belongs to is left alone
and reported.

**Bubblewrap inside the distro is required, not optional.** A distro is one
filesystem and one identity, so without it the agent sees all of the distro
read-write whatever the profile's paths say; friring resolves it once when the
distro is ensured, refuses one sitting somewhere the sandbox could rewrite, and
refuses the launch with the install command when the distro carries none. `$HOME`
is the distro's own, and every path a launch names is judged against the distro's
filesystem — a host-side scratch or signal directory is not in there, and is
refused with the way in rather than left to die in the pane.
The same profile that says which paths are writable may not make bubblewrap
itself one of them — the rule the three engine backends apply to their own CLI.
It is checked where the distro is ensured, against the profile's read-write
paths, and again where the command is composed, against the launch's whole
writable set: a launch adds its workspace, its signal and its scratch
directories, which the profile never named. The program is compared as the
distro spelled it; friring does not ask the distro to resolve it further, so a
link planted inside from a system prefix into a writable one is the residual,
exactly as it is for a remote engine.

Memory and CPU caps, a containerfile, and a filtered network mode are refused
rather than accepted and ignored. **The egress relay is not wired**: the proxy
enforces from outside the boundary over a unix socket bind-mounted across it, and
friring is on the Windows side of the utility VM the distro lives in — so
`allowlist` and `full`-with-denies are refused, and `none` (bubblewrap's own
namespace) and `full` (the VM's network) are what a WSL place offers. Probed from
*inside* a distro the rung is unavailable, naming bwrap — the rung above it — as
what to use instead.

**Nothing above runs in this build**, and the launch path is only the largest of
the reasons. A distro is reached by the `wsl:` transport rather than the
container one, and the projected hooks, the credentials and the per-session
directories a place is launched with are all composed for a filesystem friring
can write to directly — so the launch path was deliberately not wired for this
slice. With it unwired, nothing else calls the backend either: registering,
hardening, adopting and reclaiming are all reached from `SandboxHost::wsl_distro`
or from `PLACE_KINDS`, and neither leads here. Concretely, in this build:

| Operation | Reachable? |
|---|---|
| Probe (is a WSL place possible here, and why not) | Yes — the editor, the profile list and the session-creation step all show it |
| Capabilities (no caps, no `allowlist`, `workspace` scope only) | Yes — the editor gates its rows and refuses a save asking for more |
| Refusing a launch pinned to `wsl-distro` | Yes — with the two ways to get a boundary today |
| Register / harden / adopt a distro | **No caller** |
| Run a session in a distro | **No caller** |
| Reclaim, unregister, or list friring's distros | **No caller** — `PLACE_KINDS` is the three container engines |
| `friring-cli sandbox` over WSL places | **No** — `prune` and the manager view walk `PLACE_KINDS`; `list`/`show` would print a recorded distro row, and none is ever written |

Pinning `wsl-distro` therefore refuses at the launch, naming the two ways to get
a boundary today — run friring *inside* the distro and use `bwrap`, or use
`docker`/`podman`. Refusing rather than half-composing is the same rule as
everywhere else here: a session that opens on a pane with no hooks, no login and
no status is worse than one that does not open. The refusals this backend does
implement are pinned by `sandbox::tests::place_conformance` **now**, while nobody
is launching into them, because the moment that path is wired is the moment
nobody is looking at them.

### Identical absolute paths

Every place backend mounts each profile path at **exactly its host path**. This
is not a convenience:

- A git linked worktree references its main repository by absolute path and
  vice versa. Mounting both at their real paths is what keeps `git status`,
  `git log` and `git commit` working inside.
- Claude Code keys session transcripts by absolute project path, and Codex keys
  `projects.<path>.trust_level` the same way. A path mismatch silently breaks
  resume and re-triggers trust prompts.

The one deliberate exception is `$HOME`. A place gets a **synthetic per-profile
home** — friring's own directory under `<data dir>/sandbox/pl/<profile>/home`,
mounted at a fixed path inside — never a bind of the host's agent configuration
(ADR-28), and never the host's home path, which an image built for another user
may not even be able to create. Nothing keys project state by `$HOME`; agents
key it by the *project* path, which is identical.

Place instances also set `safe.directory = *` (bind mounts surface foreign
ownership) and a per-sandbox committer identity so agent commits are
attributable.

## Backend selection

`backend = "auto"` resolves down a per-OS ladder:

| Host | Order |
|---|---|
| macOS (Apple Silicon, macOS 26+) | `seatbelt` → `apple-container` → `docker`/`podman` |
| macOS (other) | `seatbelt` → `docker`/`podman` |
| Linux | `bwrap` → `podman` → `docker` |
| Windows via WSL transport | `bwrap` (inside the distro) → `wsl-distro` → `docker` |
| Windows native binary | `docker`/`podman` |

The default is the first *available* rung, which favours startup latency,
credential passthrough and zero image maintenance. The user overrides per
profile, and the session-creation step shows the resolved backend so the choice
is never invisible.

A **pinned** backend never falls back: silently running an isolation technology
the user did not choose is worse than refusing to launch. An exhausted `auto`
ladder fails with every rung's reason attached.

`SandboxBackend` is the trait every backend implements:

```rust
trait SandboxBackend {
    fn kind(&self) -> SandboxBackendKind;
    /// Cheap and cached: is this usable, and if not, why (actionably)?
    /// The host is injected at construction, which is also what makes probing
    /// testable on a machine with nothing installed.
    fn probe(&self) -> Availability;
    /// What the UI may offer: shape, limits, network modes, read scopes,
    /// persistence, host-credential passthrough, inner-sandbox verdict.
    fn capabilities(&self) -> Caps;
    /// Policy backends: argv in, wrapped argv out. Takes the whole launch, not
    /// just the policy — a generated profile file needs a session key, and the
    /// per-session paths and the proxy hole are launch inputs.
    fn wrap(&self, argv: Argv, launch: &SandboxLaunch<'_>) -> SandboxResult<Argv>;
    /// Place backends: ensure the environment exists and is running.
    fn ensure(&self, profile: &SandboxProfile) -> SandboxResult<SandboxInstance>;
}
```

`SandboxBackend` is deliberately small, and a place needs more than it: an
environment that outlives a command has a lifecycle, and the launch path,
teardown, the reclaiming pass and `friring-cli` all drive it. That is a second
seam, `PlaceBackend` — ensure the place, ask it for the agent, ask for the digest
a profile resolves to now, hand over the vetted engine path, list what friring
created, reap what a plan named, and hand out a relay port. It is a *supertrait*
of `SandboxBackend`, so one `&dyn PlaceBackend` still answers `probe` and
`capabilities`, and every caller of a place names it rather than one accessor per
tool: a backend that skipped a step would have to skip it in the implementation,
where the conformance test is, rather than by not being wired to a caller. The
`wsl-distro` place is deliberately outside it — it hands back a distro name
rather than a container, and the launch path refuses it with what is missing.

A policy backend implements `wrap` and a place backend implements `ensure`. A
place backend implements `wrap` **as well**, and this is not a contradiction: a
place's command is composed for the *inside* of the environment, where the egress
relay has to run beside the agent because the network namespace is the place's.
Reaching the place stays entirely the transport's business — nothing a place's
`wrap` returns names the engine. What the default bodies still catch is a backend
implementing neither half, and a place backend refuses a launch composed without
a `PlaceLaunch` rather than returning the argv unchanged: for a policy backend an
unwrapped argv is a bug, for a place it would be an agent running on the host
under a profile that says otherwise. `probe` results
are what the UI renders — a backend is never silently skipped, and a backend the
*build* does not have yet says so in the same shape as one the host is missing.

Two things a default-deny profile breaks that are easy to miss, and that both
policy backends therefore handle explicitly: the pane's pseudo-terminal
(read/write/`ioctl` on the tty, without which the agent renders nothing), and
bwrap's `--new-session`, which calls `setsid()` and detaches the agent from the
pane. `--new-session` is deliberately **not** used.

The `wsl-distro` rung sits below `bwrap` for a Windows host reached through the
WSL transport, as the table says — but the platform detected *inside* a distro
is the distro itself, and cloning a new one is a Windows-side operation a
sandbox running in one cannot perform. The rung is reachable only from the
Windows side. And it does not resolve to a usable boundary even there: an `auto`
ladder that reached it produces a launch refusal naming what to use instead,
because nothing in this build launches into a distro (see
[its entry](#wsl-distro--windows-place)). It stays on the ladder rather than
being removed so that the reason is a sentence rather than a silently missing
rung.

## Egress firewall

One engine on every backend: **the sandbox has no direct network, and a
Friring-owned filtering proxy outside the boundary enforces the allowlist.**

```text
sandbox (no route to the internet)
   │  HTTP_PROXY / HTTPS_PROXY / ALL_PROXY
   ▼
friring proxy  ──  allow?  ──►  upstream
               └─  deny   ──►  403 with a reason, event to the TUI
```

Per backend the "no direct network" half is: seatbelt denies all outbound
except the loopback proxy port; bwrap uses `--unshare-net`; containers use
`--network none`; `apple-container` cannot express it at all, so only an
unrestricted `full` launches there. Because the kernel blocks everything else, a
process that ignores the proxy environment variables gets *no* network rather
than an escape route.

### Reaching the proxy

That same denial is why the arrow above is not one mechanism. **A sandbox with
its own network namespace cannot reach the host's loopback at all** — inside
`--unshare-net`, `127.0.0.1` is the sandbox's own loopback, and there is no
address that resolves to the host. The proxy therefore listens on two
transports, and each backend uses the one its kernel primitive leaves open:

| Backend | Transport | Why |
|---|---|---|
| `seatbelt` | host TCP loopback | Shares the host network stack; the profile denies non-loopback traffic but leaves the proxy port reachable. SBPL's `localhost` covers both loopback families and cannot be told which, so the proxy holds the port on both. |
| `bwrap` | unix socket + relay | A new network namespace has no route to the host. A unix socket is a filesystem object, so a bind mount carries it across — and the sandbox is a process on friring's own kernel, so the listener really is on the far side of the mount. |
| `docker`/`podman` on `--network none` | unix socket + relay, **proved before it is relied on** | The same transport, but these engines are not always on this kernel: Docker Desktop, `podman machine` and colima run the daemon in a Linux VM, where the mount carries the socket *file* and the listener stays behind in the kernel that bound it. friring does not infer that from the engine's name. When a filtered profile's place is ensured it binds a listener under the place's own directory, has the place dial it once, and refuses the profile — naming what could not be reached — if nothing arrives. Measured once per place, not per session. |
| `wsl-distro` | none — the mode is refused | Per-distro firewalling is impossible (one shared VM network namespace), so egress control would have to come from `bwrap` inside the distro — and friring is on the Windows side of the utility VM, so its socket does not reach in. A filtered mode is refused rather than started unproxied. Nothing launches into a distro in this build anyway. |
| `apple-container` | none — the mode is refused | A place is a VM with its own kernel, so a bind-mounted socket carries no reachable listener across it. Every mode that needs the proxy is refused rather than started unfiltered. |

No mainstream HTTP or SOCKS client can *dial* a proxy over a unix socket:
`HTTP_PROXY` and `ALL_PROXY` take a host and a port. So a small relay runs
**inside** the namespace, offering a TCP endpoint and forwarding each
connection to the bind-mounted socket:

```text
agent  →  127.0.0.1:PORT   (the sandbox's own loopback)
       →  friring-cli sandbox relay
       →  /…/proxy.sock    (bind-mounted; `proxy-b.sock` across a relaunch)
       →  friring proxy    →  policy  →  upstream
```

This is the shape such setups usually build out of `socat`; Friring ships it
instead, so it inherits the same timeouts, connection cap and clean shutdown as
the proxy. The relay is protocol-agnostic — it never parses a byte, so
`CONNECT` and SOCKS5 both cross unchanged — and it holds **no credential and no
policy**: the proxy's token is still demanded at the far end, and the decision
is still made outside the boundary. Giving the relay either would put both
inside the boundary they exist to constrain.

The relay is `friring-cli sandbox relay --listen 127.0.0.1:8118 --socket <path>`,
started inside the namespace beside the agent by a two-line `/bin/sh` launcher
that takes every value as a positional parameter and then `exec`s the agent, so
the agent is still pid 1 of the sandbox's pid namespace and the relay dies with
it. The port is fixed because each `--unshare-net` sandbox has a private
loopback. friring's own CLI is the relay: it is resolved from the running
binary, never from `PATH`, bound read-only into a `workspace`-scope sandbox,
and a launch that cannot find it is refused.

A **place** is the exception, and for the same reason stated the other way
round: it is created once per profile and shared by that profile's sessions, so
they share one loopback and a fixed port would collide. Each session's relay
takes the lowest free port in a span of 64 from the same base and keeps it across
relaunches, and the address is composed into that session's proxy environment. In
a place the relay is a binary of the **image's**, resolved once inside the
container when the place is ensured, so a filtered profile whose image carries no
`friring-cli` is refused with the fix rather than started believing it is
proxied. The socket is bound read-write —
`connect(2)` on a unix socket needs write permission — which also makes it a
mount point, so a sandbox cannot unlink its own way out.

The socket lives in the **per-session scratch directory**, `0o600` under a
`0o700` directory. That directory is writable by the sandbox by design; the
socket is safe there because it is a mount point while the launch is live, and
because the next start replaces only a socket nothing is serving and refuses
anything else — an agent that plants a file at the path refuses its own next
launch and nothing more.

`0o600` is the default rather than a fixed rule: the socket is a
credential-bearing endpoint, and a backend whose sandbox could run as a different
uid has to answer for it rather than inherit a world-connectable socket. The
container backend answers by giving the container the host user's identity
instead of widening the socket, so `0o600` holds on every backend.

Because the endpoint shape differs, a backend rejects the wrong one rather than
silently failing later: `bwrap` refuses a loopback endpoint and says why.

Both policy backends are wired to the proxy. A launch whose mode the kernel
cannot express on its own — `allowlist` always, and `full` when it carries
denies — is **refused** unless an instance is running for it, so a sandbox is
never started believing it is filtered. The refusal goes through the profile's
own `allow_unsandboxed_fallback` switch, like every other boundary friring
will not grant. `full` with no denies needs no proxy and keeps direct egress.

At the kernel layer `allowlist` and `none` are still identical — both deny
direct egress — so the allowlist is entirely the proxy's doing, and a profile
that defaults to `allowlist` with an empty list starts closed.

**Allowlist matching** is exact, over a host canonicalised first (see below)
and therefore case-insensitively: `github.com` covers `github.com` and nothing
else. A subtree is spelled out — `*.x`, or `.x`, one pattern — and covers `x`
and every subdomain of it on a label boundary, so `*.github.com` covers
`api.github.com` but not `evilgithub.com`, `github.com.evil.net` or
`github.co`. Bare-is-exact is what makes the first-use prompt's promise true
("that host on that port only"); the wildcard covers the apex because the deny
direction decides it, since a user refusing `*.x` means "no x traffic" and a
matcher sparing the apex would be a silent hole. A rule without a port covers
every port. Denies are checked **first in every
mode**, so a deny entry narrows `full` too — and a `full` profile that carries
denies is proxied exactly like an allowlist, because a deny list is enforceable
nowhere else: the kernel blocks direct egress and the proxy applies the denies
with mode `full`. `prompt_new_domains` is meaningful only under `allowlist` —
nothing is unlisted under `full` and nothing leaves under `none` — and the
editor greys it out elsewhere.

Two matchers implement this vocabulary: `session::DomainRule`, which the
profile validator and the UI use, and `proxy::HostRule`, which the proxy
enforces at connection time. They deliberately do not share a type — the proxy
is a leaf in the architecture allowlist — so
`tests/egress_matcher_conformance.rs` runs both over one table of stored
spellings and fails if either drifts. Both accept the same grammar, and hold a
*request* host to it too — a spelling that could never be written as a rule can
never be allowed: ASCII labels of letters, digits, `-` and `_`, no empty label,
none edged with `-`, 63 bytes per label and 253 overall, with brackets reserved
for an address. Both also canonicalise before comparing, so a rule and a request
meet in one spelling. A shape no request host could ever carry is refused rather
than stored as a rule that matches nothing, and an international name is written
in punycode because that is how it is spelled on the wire — and because guessing
at a U-label would compare a rule against one name and dial another.

This is chosen over IP-based `iptables`/`ipset` allowlists (the pattern in
Anthropic's devcontainer reference and in the `friring-autonomous` rig) because
resolved-IP snapshots break mid-run when a CDN rotates addresses, and because
`iptables` rules need `NET_ADMIN`, differ per host, and cannot work at all
under seatbelt or in a shared WSL network namespace. The proxy's semantics are
identical across every backend and over the SSH transport, where it simply runs
on the remote end. IP-level filtering remains available as optional
defence-in-depth inside containers.

The proxy is a Friring-owned Rust component. It lives at **`src/proxy/`**, a
top-level module rather than a child of `sandbox`: it enforces a policy handed
to it and references no other crate module, so it is a leaf in the architecture
allowlist and testable without a session, a database or a backend.

- HTTP `CONNECT` and SOCKS5, allowlist matched on the requested host, with
  optional `:port` scoping; denies win over allows, in every network mode. A
  bare rule is one host (`github.com` matches `github.com` alone);
  `*.github.com` and `.github.com` are one spelling of its subtree, on a label
  boundary (`api.github.com`, never `evilgithub.com`) and apex included; an
  address rule is exact, prefix or no prefix — there is nothing under an
  address, so `*.127.0.0.1` is the address rule.
- Both protocols share **one listener**, selected by the first byte (`0x05` is
  a SOCKS greeting, anything else starts an HTTP request line), on either
  transport. `ALL_PROXY` must be **`socks5h://`**, not `socks5://`: the `h`
  keeps hostname resolution on the proxy's side, and without it the client
  resolves first and hands the proxy an address that no domain rule can match —
  silently defeating the allowlist rather than failing loudly.
- Per-instance bearer token so only the intended sandbox can use it —
  `Proxy-Authorization` (`Bearer`, or the `Basic` header a client derives from
  the proxy URL) over HTTP, username/password over SOCKS5. Unauthenticated
  callers are refused before any policy is consulted, so they learn nothing
  about the allowlist.
- The token never reaches a log or a toast. It is handed to the sandbox inside
  the proxy URL, which travels in the tmux `new-window` command line, and the
  two paths that report a failed control-mode command — a stall and a `%error`
  — quoted that line verbatim. Both now run it through a redactor that
  withholds any userinfo-bearing URL and any value whose name reads like a
  credential, while keeping the verb, the window and the environment *keys*, so
  a stalled launch is still diagnosable. The policy is by shape rather than by
  variable name: this is shared tmux plumbing, so a credential arrives from an
  agent's registry environment or a user's argv just as easily as from the
  boundary. The types that carry the token in memory (`ProxyConfig`,
  `ProxyGrant`, the supervisor's `Bound`) hand-write `Debug` to withhold it,
  because a derived one puts it in every `{:?}` and every `expect`.
- **The proxy will not dial the machine it runs on.** It connects with
  friring's reachability, not the sandbox's, so a `CONNECT 127.0.0.1:2375`
  honoured on the sandbox's behalf would hand back exactly the route
  `--unshare-net` exists to remove — a rootless container API, a language
  server, an SSH forward. Loopback, the unspecified address and link-local
  (including `169.254.169.254`, the cloud metadata endpoint) are refused with a
  reason of their own, in **every** mode including `full`, and the check runs
  again on the address a *name* resolved to — so `127.0.0.1.nip.io`, a record
  pointed inward and a rebinding TTL all land on it, and the socket only ever
  opens to an address that was vetted. The single exception is an allow rule
  naming the literal address (`127.0.0.1:11434`): the user wrote it, it is
  scoped to the port they wrote, and neither `*` nor a name nor `full` is that
  sentence. RFC 1918 and IPv6 unique-local are **not** refused — those are the
  user's network rather than the user's machine.
- A plaintext request is forwarded with the `Host` header **replaced** by the
  authority the policy authorised. An absolute-form request carries the
  destination twice, and letting the two disagree is domain fronting the proxy
  can actually see: allowed against `raw.githubusercontent.com`, served by
  whatever virtual host the header named. Inside a `CONNECT` tunnel it cannot
  see the mismatch, which is the disclosure below.
- Denials carry a reason and surface as a TUI event; with
  `prompt_new_domains`, an unlisted domain raises a confirm modal whose answer
  is written back to the profile and applied to the running proxy without a
  restart. See [First-use domain prompts](#first-use-domain-prompts).
- Optional HTTP-method restriction (`GET`/`HEAD`/`OPTIONS` only) as a cheap
  brake on exfiltration through allowed hosts. It reaches **plaintext HTTP
  only** — a `CONNECT` tunnel is opaque, so the method inside it is unknowable
  — and each forwarded plaintext request gets its own connection, so every one
  of them is policed rather than only the first on a reused socket. The proxy
  implements it; no profile column selects it yet, so every launch runs with it
  off. Adding the column is a profile change, not a proxy change.
- No TLS interception in the first release. Inside a `CONNECT` tunnel the allow
  decision therefore trusts the client-supplied hostname and cannot see the SNI
  or `Host` that follows, so domain fronting can bypass it there — documented in
  the UI, not hidden. Plaintext is the case the proxy *can* see, and it does
  (the `Host` bullet above).
- Both matchers **canonicalise a host before comparing it**, and the request
  path canonicalises once at its edge — so the host the policy decided on is
  the host the socket is opened to, and an address is dialled as an address
  rather than handed back to the resolver. Case, one root dot, IPv6 brackets
  and a leading empty label are presentation; `127.1`, `2130706433`,
  `0x7f.0.0.1`, `0177.0.0.1` and `127.000.000.001` are the address
  `getaddrinfo(3)` reads them as; `::ffff:127.0.0.1` folds onto `127.0.0.1`.
  The deprecated IPv4-*compatible* form (`::127.0.0.1`) deliberately does not
  fold: it is a different destination, and `::1` lives in that range. An
  address rule therefore covers every spelling of its address and nothing else
  — `127.0.0.1.evil.net` is still a name.
- **A host with any non-ASCII byte is refused**, in a request and in a stored
  rule alike, with the punycode form named as the fix. Canonicalising a U-label
  correctly means UTS-46 — case folding, NFC normalisation, and tables of
  disallowed and deviation characters — and only the punycode *encoding* half
  of that is table-free. Encoding without mapping would be worse than refusing:
  two spellings of one name encode to two A-labels, so a deny rule on the
  punycode form would still be dodged while the feature looked closed. A UTS-46
  dependency would buy exactly that and nothing else. The refusal carries its
  own reason (`UnsupportedHost`), distinct from "not allowlisted" so it never
  raises the first-use prompt — whose answer would be a rule the profile
  validator refuses.

The environment friring injects is `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`
and `NO_PROXY` in both cases, because tools disagree about which spelling they
read. `NO_PROXY` is `localhost,127.0.0.1,::1`, so an agent's own local traffic
— a dev server it started, a language server — is not tunnelled through a
filter that would refuse it for not being in a domain allowlist. That is a
convenience and not the boundary: a client is free to ignore it, and what makes
the boundary hold is the proxy's own refusal to dial the host it runs on.

Written in Rust rather than shelling out to an external runtime: Friring ships
as a self-contained binary, and a CONNECT/SOCKS filter is a small, testable
component. The policy vocabulary deliberately mirrors the de-facto standard
(`allowRead`/`denyRead`/`allowWrite`/`denyWrite`/`allowedDomains`/
`deniedDomains`/`allowUnixSockets`) so profiles stay legible to anyone who
knows the ecosystem.

### First-use domain prompts

An allowlist nobody can extend without stopping the agent is an allowlist
people turn off. So a refusal under `prompt_new_domains` becomes a question:
the TUI drains the proxy's denial stream on its own tick, and an unlisted host
raises a confirm modal naming the session, the host, the port, and the rule an
answer would store.

**Allow does both halves.** It reaches the *running* proxy first
(`update_policy`, no restart — the agent's next attempt succeeds), and then
writes the rule into the profile so the next launch still has it. That order is
also the check: a session torn down while the question waited, or a rule the
proxy will not load, must not leave the stored profile permanently wider on the
strength of an answer that reached no boundary. If the profile write is the half
that fails — deleted, or no longer decodable — the live grant stands and the
message says the profile did not keep it.

**The rule is scoped to the port that was refused**, and stored canonically.
`api.github.com:443` grants that host on that port, not every port on it, and
not the host's subtree; a wider grant is an edit to the profile, made
deliberately. The grammar does that work rather than a second code path: a bare
rule *is* one host, so the stored rule and the sentence in the modal say the
same thing. It is also why the answer is built from the host alone
(`DomainRule::exact`) and not by parsing the client's text as a rule — the
sandbox picks the spelling it is refused under, and a request for `.github.com`
(which resolves to the apex) must not be answered with the subtree that text
would mean in a profile. Canonically because the *sandbox* chose the spelling:
the question shows `192.0.2.10` for whatever legacy notation the agent wrote,
and stores the address every later request canonicalises to.

**Answering takes a deliberate key, on a question that has been read.** This is
the only modal in friring that is not opened by a keypress — a sandboxed
agent's request raises it, from the background tick, while the user's focus is
a terminal pane and their hands are mid-sentence in it. So `y` grants and
`Enter` does not: `Enter` is the most-pressed key in an agent pane, and here it
means "leave it blocked". And nothing at all is answered until the question has
been on screen for a moment; keystrokes before that are swallowed rather than
acted on, because a key already in flight belongs to the pane behind the modal.
Without both, typing "yes, go ahead" into an agent pane is a security grant,
and an agent that can provoke a refusal can choose the moment it lands.

**A destination on the host itself is never offered.** `NO_PROXY` exists so the
agent's own local traffic bypasses the filter, which means a question about
`127.0.0.1:2375` reads like the agent asking for a dev server it started — while
the address the proxy would dial is on the machine outside the boundary. The
proxy refuses those with a reason that does not raise this prompt, and the rule
builder declines to construct one anyway.

**Every refusal is reported; only some are asked about.** An agent that cannot
reach the network is failing, and the reason is the only way to know why — so a
newly refused host reaches the status bar whatever its reason and whatever the
profile says. It becomes a *question* only when all four hold: the reason is
"not in the allowlist" (a deny rule, `network = none` and a rejected token are
answers the user already gave, and `UnsupportedHost` must never be asked about
— its answer would be a rule the profile validator then refuses to store);
friring still holds the session, so there is a profile to write to; that profile
decodes and carries `prompt_new_domains`; and the host can be spelled as a rule
at all. Each of the four fails towards *not* asking.

**Asked once.** The far side of this prompt is an agent, and an agent that
wanted a domain wants it again a millisecond later. A refusal is therefore
turned into something the user sees exactly once per `(session, host)`: the
first is reported and asked about, every later one is silent — which is also
how a refusal is remembered, since "no" needs no separate record. Two bounds
guard the rest, and both fail towards saying less: a session refused more than
256 distinct hosts is scanning rather than asking, and is told so once and then
ignored; and at most eight questions wait at a time, past which a refusal is
reported without being asked, because a queue the user clears one keypress at a
time is worse than a message.

A question waits for the single modal slot rather than taking the screen from
an open editor, and a relaunch clears the session's history — a fresh proxy
from a re-read profile is a new boundary, not the one the earlier answers were
about. That includes a question already **on screen**: the session key is
stable across a relaunch by design, so an answer given then would apply a grant
to the instance that replaced the one it was about, and write it into whatever
profile the question named rather than the one now in force. The question is
withdrawn instead, and the new boundary asks for itself if the agent asks again.

## Credentials

Two facts drive the whole design.

**Refresh tokens are single-use and rotating.** Both major agent vendors issue
OAuth refresh tokens that are invalidated on use. Copying a credentials file
into N sandboxes creates N consumers of one token: the first refresh wins, the
siblings get `invalid_grant`, and some versions then delete their own
credential file. Copy-per-sandbox is broken by construction, not merely
inelegant.

**Keychain does not cross a VM boundary.** On macOS, Claude Code stores OAuth in
the Keychain and actively migrates any file-based credential into it. A
container cannot reach it. A seatbelt sandbox can.

Therefore, in resolution order:

1. **`host-passthrough`** — policy backends only. The agent sees the real
   credential store subject to path policy; Keychain works; no login, no
   copying, nothing to expire. Default read policy is `host-minus-secrets`:
   the host home is readable except a deny list (SSH keys, cloud credentials,
   other agents' credential files), writes confined to the workspace plus the
   agent's own state directory.

   "Other" is decided by **credential family**: the agent's registry name, or
   its `hook_schema` when it is a rebrand of a built-in. Getting that wrong
   either logs the agent out or lets it read a sibling's token.

   The deny list covers `~/.ssh`, which **breaks git over SSH inside every
   sandbox** — and `host-minus-secrets` is the default read policy. Use HTTPS
   with a token for pushes from inside a profile, or add the key path back
   deliberately.
2. **`env-token`** — a long-lived token the user supplies once. Friring keeps it
   in its own OS keychain entry (service `dev.friring.sandbox`, account
   `<credential family>/<variable>`) and injects it into the sandbox's window
   environment at launch. Never in the database, never in a config file, never
   in the registry — the registry declares the variable *names* (`secret_env`)
   and nothing else. No rotation, no races. This is the recommended path for
   place backends.

   The value never reaches a command line, in either direction. Reads use the
   platform tool's stdout; the write uses its stdin where one exists
   (`secret-tool store`), and where none does (`security` takes a new item's
   value only as `-w <value>`) friring refuses to write and prints the
   prompting command instead. Injection is the control-mode window
   environment, so a headless launch — which passes window environment as
   `tmux -e KEY=VALUE` argv — refuses rather than exposes it.

   `friring-cli sandbox token set <agent> [VARIABLE]` is what puts one there, and
   the value is never an argument: it is read from stdin or from a no-echo
   prompt, capped, held in a type with no `Display`, and never rendered, logged
   or quoted back in a refusal — every refusal about *which* entry is raised
   before the value is asked for. `token rm` forgets one; `token list` answers
   whether friring holds each variable an agent declares, never what it holds,
   and drives the store per declared entry rather than enumerating it. It asks
   the store's *existence* question, which on macOS is
   `security find-generic-password` **without** `-w`: not printing a value is not
   the same as not extracting one, and the value flag is what opens the item,
   pulls every declared secret into friring's address space and can raise a
   keychain prompt per entry. A tool with no metadata-only query —
   `secret-tool search` prints the secret beside the attributes — still reads and
   drops the value. The difference is stated rather than hidden: the check
   answers "friring holds an entry", so an entry cleared to whitespace lists as
   held while a launch treats it as absent. On a host
   whose store friring cannot write to, `token set` says so **before** asking for
   a value, so a secret is never typed into a command that was going to refuse.
3. **`volume-login`** — the profile's own persistent state directory, with one
   interactive login per *profile*, done inside the session pane. The
   "volume" is friring's synthetic per-profile home, which is already
   per profile, already mounted and already survives a container rebuild;
   `config_dir_env` relocates the agent's state (`state_dir`) into it. Every
   session of that profile shares one credential rather than holding copies of
   it, which is the safe single-writer case and the answer to "logging in for
   each sandbox is bad UX": login is per profile and effectively annual.
4. **`seed-file`** — copy a credential file in once and honour write-back.
   Opt-in (`auth = "seed-file"`; `auto` never picks it) and gated on the
   declaration asserting vendor support (`seed_file_supported`), which a
   rotating single-use refresh token can never do. **At most one profile per
   credential family, per host**: friring records which profile holds the copy
   under `<data dir>/sandbox/seeds/`, outside every boundary, and refuses a
   second copy with the reason. Deleting the profile that holds it releases the
   family again, because the boundary the copy lived in went with it. Honouring
   write-back means the sandbox's copy is the durable one — friring never writes
   a refreshed credential back to the host store, because host and sandbox would
   then be two writers of one rotating token.

A credential problem never fails a launch. A missing token, an unreadable
store, a refused copy and an agent that declares nothing all resolve to the
same thing: the agent starts signed out and the session's `Sandbox:` row says
so and says what to type. Refusing would route the launch through
`allow_unsandboxed_fallback`, and answering a missing token by running the
agent outside the boundary is worse than an agent that asks you to log in.

Friring answers "is this sandbox signed in?" from the declared
`credential_file` alone, and only by asking whether it exists — never by
opening it. Where an agent declares no such file the answer is *unknown*, which
is what is reported: the state directory having something in it would be a
guess, and since config projection writes the user's own settings into exactly
that directory it would be a guess that is wrong on every first launch.

Never: bind-mounting the host agent configuration directory read-write into a
place. It is useless on macOS (the credentials are not in the file) and it is
an escape channel — an agent that can write the host's agent settings can plant
hooks that the *host* agent later executes outside the boundary.

Each agent declares what it needs, in the registry, as data:

```toml
[agents.<name>.sandbox]
config_dir_env      = "…"     # env var relocating agent state into the sandbox
state_dir           = "~/.x"  # the directory that variable names, home-relative
auth                = "auto"  # auto | host-passthrough | env-token | volume-login | seed-file
state_rw            = […]     # directories the agent writes and must keep (policy backends)
copy_in             = […]     # config safe to project, subject to the lint pass
env                 = { … }   # static env (e.g. disable self-update in a place)
secret_env          = […]     # NAMES of tokens Friring may inject from its keychain
credential_file     = "~/.x/auth.json"  # the vendor credential file
seed_file_supported = false   # true only where the vendor documents copying it
bypass              = […]     # flags meaning "the outer boundary is the sandbox"
writeback           = true    # a refreshed credential must survive the sandbox
login_fallback      = "…"     # what to type in the pane when state is empty
```

`auth` is a *request*, not a verdict. A policy backend is always
`host-passthrough` — it returns before any lookup, copy or relocation — and a
place can never be, so a declaration asking for it there degrades to the ladder
with the reason in front of the user. In a place, `auto` takes `env-token` when
friring holds a token for that agent's credential family and `volume-login`
otherwise; it never chooses `seed-file`, because that one copies.

Friring never reads a credential file to inspect it, and never logs credential
contents. That covers the boundary's *own* token too, not just the agent's —
see the redaction bullet under [Egress firewall](#egress-firewall) and the
matching entry in [Failure modes](#failure-modes). A future host-side credential broker (the sandbox asks,
the host refreshes) is the theoretically cleanest endpoint and is deliberately
deferred: `volume-login` removes the urgency.

## Config projection

A place gets a **synthetic per-profile home**, and the safe subset of the user's
agent configuration is copied into it — never a bind of the host's agent
directory (ADR-28), which is useless on macOS (the credentials are not in the
file) and an escape channel through hooks the *host* agent later runs.

Every entry is home-relative on both sides: `~/.claude/skills` lands at
`$HOME/.claude/skills` inside. A place already relocates `$HOME`, so nothing
about an agent's layout has to change and no path inside the boundary has to be
invented. What crosses is declared data (`copy_in`), never a list of one
vendor's file names.

Copying is not enough, because agent configuration routinely names the host
filesystem and a place is a different filesystem — usually a different operating
system:

- Lifecycle hook commands, status-line commands and credential-helper scripts
  are arbitrary shell, usually with absolute host paths.
- Plugin, skill and rule directories may live outside the config directory.
- Stdio MCP servers name host binaries.

So a **lint pass** classifies every entry, with a reason, and only what crosses
is written. Structured documents (JSON, TOML) are parsed and every string in
them examined; everything else is content — instructions, skills, commands —
and is copied byte for byte, because a path mentioned in prose names nothing and
executes nothing.

| Verdict | What it means |
|---|---|
| `projected` | Crossed as it stands. |
| `rewritten` | Crossed, with a reference to projected content pointed at where it lands inside. |
| `needs a mount` | Did not cross; a read-only grant for that path in the profile brings it in, and the reference then works unchanged, because a place mounts every path at exactly its host path. |
| `host-only` | Did not cross, and no grant would help — a host script or binary is not one the image can run, and a credential never crosses at all. |

The rule for what is *removed* is structural rather than schema-driven, because
friring bakes in no agent knowledge: the smallest whole entry around the
offending string goes — an array element alone, a nested object's own slot
(`mcpServers.docs.command` takes `mcpServers.docs`), a root member alone.
Removing only the string would leave a hook with no command, which is a broken
agent rather than a projected one. The session's `Sandbox:` row carries the
counts and the launch log carries the verdicts, and the profile editor answers
the same question on demand — `Ctrl+L` lists `ProjectionPlan::actionable`'s
entries per agent ("3 hooks reference host paths — mount read-only, drop, or
rewrite?"), computed against the form in front of the user rather than on every
keystroke (see [UI](#ui)).
The `friring-autonomous` rig's hard-coded read-only hooks mount becomes a
per-entry choice.

**Enforced settings** go in through each agent's highest-precedence
configuration layer, so a repository-level file cannot override the
orchestrator's intent. The declaration is a template with two placeholders
friring fills — `{workspaces}`, the paths the profile granted, and `{path}`,
repeated once per granted path — which is what pre-seeds the workspace trust
several agents otherwise prompt for on first run in a fresh home. The rendered
document is parsed before it is written and merged *over* whatever projected to
the same path, so friring's keys win and a template typo writes nothing rather
than a file the agent fails to parse on startup.

**Projection is a boundary in both directions.** No credential file crosses,
ever — not even the launching agent's own, because a rotating refresh token with
two consumers invalidates itself on the first refresh (ADR-28), so a place's
agent signs in inside its own pane. Nothing reaching the data directory or a
tmux socket directory crosses, in a declared entry or in a document's text
(ADR-29). Projection adds no mount: "needs a mount" is a sentence for the user,
and widening the boundary stays a deliberate profile edit. And every write
refuses a symlink at every component — the synthetic home is writable by the
sandbox, so a link from its own home to the host's real agent directory would
turn the next projection into a write into the one directory ADR-28 keeps out of
reach. Projected files are `0600`, or `0700` where the source was executable:
content crosses, and nothing else does. A projected JSON or TOML document is
friring's re-rendering of the user's file rather than a byte copy, so comments
are lost and keys are ordered.

Nothing is pruned. A file the user deletes on the host lingers in the place,
deliberately: the projected tree is also the agent's writable state directory,
and a launch that wiped it would sign the agent out on every restart.

Policy backends need none of this: the real home is already visible, subject to
path policy.

## Inner agent sandboxes

Several agents sandbox their own tool calls. When Friring provides the outer
boundary, the inner one must be disabled — under seatbelt it *cannot* work
(nested `sandbox_apply` is denied outright), and inside a container it usually
cannot either, because it needs user namespaces the container does not grant.

This is why every agent declares `bypass` flags. They are applied only when a
sandbox profile is active, and the UI states the composition plainly:

```text
sandbox: friring-dev (seatbelt) · inner agent sandbox: off — Friring is the boundary
```

The tradeoff is real and worth stating in the docs: with the inner sandbox off,
anything inside the boundary — including the agent's own credentials — is
reachable by whatever the agent runs. That is an argument for narrow profiles
and for `env-token` credentials with a limited blast radius, not an argument for
double sandboxing that does not work.

Where a backend supports it, `.git/hooks` stays write-protected inside **every**
writable root — a shared constant both policy backends apply, and a no-op when
the root is not a repository. Hook scripts are run by whichever git touches the
repository next, including the *host's*, outside the boundary.

`.git/config` is deliberately **not** protected despite naming commands
(`core.pager`, `core.fsmonitor`, aliases): `git config` and `git remote add` are
ordinary sandboxed work, and a profile that breaks git gets turned off, which
protects nothing.

The agent's own configuration is likewise not blanket-protected, because
`state_rw` exists: an agent that cannot write its state directory dies on first
launch. The narrow rule wins over the broad one.

## Launch integration

**Policy backends** are a decorator applied where the invocation is composed
(`build_agent_invocation` in `session_ops`, and `Session::spawn`/`Session::restart`
in `agent::backend`, both through `agent::sandboxing`). Argv in, wrapped argv
out. Wrapping happens *before* per-transport composition, because transports
differ in how they quote and fold the command — the Windows multiplexer path
collapses everything into a single token, so wrapping at the shell-string level
would not survive. Spawn, restart and resume are all covered by the one seam.

The call order is fixed: resolve the backend from the profile's `auto` ladder →
resolve the profile against it → fold in what the agent declares
(`state_rw`, `bypass`, `env`) → build the launch → wrap. The agent's bypass
flags go on the *agent's* own argv, inside the wrapper.

**A sandbox profile applies to a local session only.** Both shapes are built on
the machine friring runs on: a policy backend generates its artefacts here (a
`.sb` profile file, an argv naming local paths), and a place is created by an
engine here, with *this* machine's paths mounted. Either one applied to a
session whose worktrees and tmux are on another host would be a boundary around
the wrong filesystem, so an `ssh:`/`wsl:` session with a profile is refused
rather than composed. Sandboxing a remote session needs a place created *on*
that host, which is not wired. A place-backed session's own
`sandbox:<profile>` backend is not a remote one and never trips this: it says
where the boundary is, not where the session's machine is.

**Place backends** add an ensure-instance step before spawn and then reach the
place through `TmuxTransport::Sandbox`, a launch prefix in front of the tmux argv
exactly as `ssh <dest>` is — `<engine> exec -i <container> tmux -L friring …`.
Control mode is transport-agnostic by design, so discovery, adoption, input,
scrollback and the `tb-`/`tbs-` window naming are the SSH path's, unchanged. Two
things do differ, and both would break silently the other way round: the tokens
are **not** shell-quoted, because an engine `exec` takes an argv rather than a
command string a login shell re-splits (a quoted `-F '#{pane_id}|…'` would arrive
with its quotes and discovery would find nothing); and the window command is
**not** wrapped in a login shell, because a place's `PATH` comes from its image
rather than from an account profile, and `sh -l` inside a container commonly
replaces it with `/etc/profile`'s. The engine is named by absolute path, resolved
once where the backend was probed, for the reason `bwrap` is. Materialising agent
configuration into a place generalises the existing remote-argument adaptation: a
sandbox is a third kind of "elsewhere".

### A place-backed session's lifecycle

`backend_type` carries `sandbox:<profile>` the way `ssh:<host>` does, and every
lifecycle path keys off it:

- **Spawn.** Composing the launch ensures the place (idempotent — one `inspect`
  when it is already running), takes this session's relay port, mints its egress
  directory *inside* the place's own tree, and hands back the transport. The
  spawn goes through that transport rather than through the backend the session's
  row named, which on a first spawn does not name it yet. The container is
  recorded in `sandbox_instances` once the pane exists.
- **Restart.** The profile is re-read, so an edited one asks for a *new*
  container and the session moves into it: the old pane is killed where it still
  is (a kill that cannot reach a container that has gone is the outcome, not a
  failure), the new one is spawned where the launch says, and the registry learns
  the place it is in now. A profile edited *off* a place backend is refused
  rather than relaunched — this session's tmux is inside the container, and a
  policy backend's argv names host binaries the image does not have.
- **Restore and adopt.** Place-backed sessions restore on the background path
  with the remote ones, one worker per place, because opening a cold container
  is seconds and must not block the first frame. The worker ensures the place
  first — which is also the recovery: a container stopped by a host reboot is
  started again there — and the retry sweep keeps trying, so a place that comes
  back adopts its sessions without a restart.
- **Delete.** The window is killed inside the place, found by friring's own
  owner and profile labels rather than by starting anything. Addressed by its
  `tb-<session>` **name**, and tried in *every* live place of the profile: a
  profile can have several containers at once (an edit builds a new one while
  the sessions already launched stay in the old), `sessions` records no
  container, and a pane id is per tmux server — so `%1` in the container this
  session is not in names a different session's agent. A name is unique to the
  session wherever it lives, so missing costs a leaked window instead of a
  stranger's agent. Worktree removal stays local: a place mounts every path at
  exactly its host path, so the checkout the container sees *is* the host's.
- **Headless.** `friring-cli session create --sandbox <profile>` spawns into the
  place and persists `sandbox:<profile>`; `session restart` refuses a
  place-backed session for the same reason it refuses a remote one — the local
  kill would find nothing and the local spawn would put an unsandboxed agent on
  the host. Automation delivery asks every live place of the profile for the
  session's window and sends into the one that has it, skipping (rather than
  erroring) when no place holds it — picking the first labelled container
  instead would report a running session as not running and never fire.

### Reclaiming places

A place outlives its launch and the friring that made it, so a slow background
pass reconciles `sandbox_instances` against what the engines hold: a row whose
container is gone is forgotten, a container whose profile is gone is removed, a
container a profile edit superseded is removed, and one of friring's that no row
describes is adopted rather than leaked. Every candidate is checked against
friring's owner label twice — once in the engine query, once again immediately
before removal — so nothing friring did not create is ever named.

Two rules keep the pass from taking a place out from under a running agent.
**Idleness never reclaims one**: a place is the environment a session lives in,
not a cache, and "unused for a while" is indistinguishable from "the user is on
holiday". And **a live session protects every place it could be in**: this
instance knows container ids only for the places it opened itself, so a live
session of a profile protects *all* of those ids — a profile edit builds a new
container while the sessions already launched keep running in the old one — and
a profile whose live sessions this instance is not driving, another friring's,
protects every container of that profile by name. The cost either way is a
superseded container surviving until those sessions end.

**A container's profile label is what it was created with; the record is what a
rename keeps current.** An engine cannot relabel a running container, while
renaming a profile rewrites the profile row, its instance rows and the
`sandbox:<profile>` of every session on it in one transaction — so after a rename
the protection set holds the new name and the container still answers to the old
one. Every in-use test therefore matches a container on **both** names, the row's
first and the label's second, in the TUI's pass and in `friring-cli sandbox
prune` alike. Matched on the label alone, a renamed profile's live place reads as
a place whose profile is gone, and rule 3 stops and removes it with an agent
working inside. The same pair of names is what teardown, automation delivery and
the manager view's stop/rebuild ask a running place by, so a rename does not
leave a pane un-killed or an automation silently undelivered.

**A rename does not move the profile's place tree, and the pass eventually
collects the old one.** The tree is named by profile name
(`<data>/sandbox/pl/<profile>`) while the rename is a database transaction, so
the synthetic home — the login `volume-login` made, and the projected
configuration — stays under the old name. It is spared for as long as a live
container still carries the old *label*; once that container goes, the name is in
no profile row and the sweep removes the tree by elimination, so the renamed
profile's next launch builds a fresh home and opens signed out. Moving the tree
at rename time is not safe for the reason deleting a profile does not remove it
either — it is bind-mounted into a container that is still running. The
`seed-file` marker files the *holder* profile's name for the same reason, so
after a rename that credential family still reads as held by the old name and a
seed for the renamed profile is refused naming it. **Renaming a profile that has
a place is therefore a sign-in again**, and it is stated here rather than
discovered.

Two details that silently break things if missed:

- **Environment forwarding.** Session environment is set on the tmux window.
  That window is *outside* a policy sandbox and *inside* a place, and in both
  cases it is the only channel inward: neither policy backend applies
  environment in argv — a policy is a rule on a process, so the wrapped agent
  inherits the window — and a place inherits nothing from friring at all,
  because `<engine> exec` gives the process the image's environment. So "the
  wrap only ever **adds** environment" is an invariant with a test on it.
  Host-only path variables are skipped for a place exactly as they are for an
  SSH host — the data directory is not in there (ADR-29), so a forwarded
  `FRIRING_DATA_DIR` would name nothing or name the one thing the boundary
  exists to keep out. Without this, status reporting dies quietly. The
  environment never rides in the engine's own command line: that is on the
  host's process table, and one of its values is the proxy credential.
- **Resume identity.** A place keeps agent transcripts inside its own volume,
  so a resume by id can target a transcript that does not exist there. The
  launch path detects this and starts a fresh session under the requested id
  instead, so later restarts resume normally. (The `friring-autonomous` rig
  proved this pattern in a wrapper script; it belongs in core.)
- **Scratch and policy files.** friring mints a per-session directory under
  `<data dir>/sandbox/tmp/` for the agent's scratch and writes the generated
  seatbelt profile to `<data dir>/sandbox/profiles/`, both `0700`, both refusing
  a symlink. Neither may be the host temp root, and a profile is refused if its
  read-write roots reach the data directory, the database file, a tmux socket
  directory or friring's own configuration files.
  A profile may not name anything under `<data dir>/sandbox` or
  `<data dir>/signals` in **either** mode either: that tree holds the other
  profiles' synthetic homes — and therefore the logins in them — the markers that
  keep one credential to one boundary (ADR-28), the generated seatbelt policies,
  and the other sessions' egress sockets and status files. All of those are taken
  by being *readable*, so read-only is no defence, and the check is applied to
  the paths the **profile** declares rather than to the launch's own grants: the
  directories friring mints for a launch live in exactly that tree and are the
  point of it. `host-minus-secrets` names no path and grants the whole host, so
  it is taken back separately: both policy backends deny `<data>/sandbox/pl`,
  `<data>/sandbox/profiles` and `<data>/sandbox/seeds` under that scope,
  alongside the host credential list, and bubblewrap denies
  `<data>/sandbox/overlay` as well — the copy-on-write layers every write inside
  a boundary lands in, which the scope would otherwise expose at their host
  paths. Not `<data>/sandbox` whole — the launch's own scratch is under
  `<data>/sandbox/tmp`, and an agent that cannot write a temp file dies on
  startup.
  The scratch is keyed on the session and adopted, not recreated, so a crashed
  run's files survive into the next launch; session teardown drops both.

  A proxy instance is **per session, not per profile**: the policy is per
  profile, but the bearer token and the socket belong to one launch. That is what
  makes a refusal name the session that provoked it, a first-use answer reach the
  session that was asked, an instance die with its session, and a relaunch rotate
  one session's credential without cutting off anybody else's. A launch that
  cannot be told apart — no friring session id and no agent conversation id — is
  refused rather than filed under a shared fallback name, because two launches
  under one key would replace each other's instance mid-run.

  It is **not** isolation between the sessions of a place. They share a uid, a
  pid namespace and the place tree, so a sibling reads the proxy URL out of
  `/proc/<pid>/environ`, connects to any socket under that tree, and dials any
  relay port on the loopback they share; a grant one of them is given is
  reachable by all of them, and is written back to the profile they all use. Per
  session is the shape for what it buys above — attribution, lifetime, rotation —
  and per place is the boundary. See
  [The two sandbox shapes](#the-two-sandbox-shapes).

  It is bound before the agent — argv has to name the port or the socket — but
  it belongs to no session until that launch has a pane: the session keeps the
  instance it is already using, and a launch that fails in between (a wrapper
  that will not compose, a pane that will not spawn, a row that will not
  persist) releases what it bound instead of taking a healthy agent's way out.
  Committing is what replaces the previous instance, so an edited profile takes
  effect and the token rotates on every relaunch, and the last one is stopped
  where the scratch directory is dropped. A relaunch therefore binds its socket
  while the previous one is still serving, so the two alternate between
  `proxy.sock` and `proxy-b.sock` in the session's scratch directory. It lives in
  the friring process that launched it: a session created by the short-lived
  `friring-cli` starts with no egress until a running friring relaunches it.

  On the loopback transport the instance holds its port on **both** loopback
  families. Seatbelt's one hole is written `(remote ip "localhost:<port>")`, and
  SBPL's `localhost` is a semantic predicate covering `::1` as well as
  `127.0.0.1` with no way to name one family — while a listener on `[::1]:P`
  does not conflict with one on `127.0.0.1:P`. Claiming both is what keeps that
  hole pointing only at the proxy instead of at whatever local service happened
  to own the other one.

## Status signals

Agents report working/blocked/done by running `friring-cli session signal`,
which writes SQLite directly. Neither half of that works inside a sandbox:

- The binary may not exist there (a Linux place on a macOS host).
- **Database write access is a sandbox escape.** Automations stored in the
  database carry shell commands that the *host* Friring executes. An agent that
  can write the database can schedule arbitrary host commands.

Sandboxed sessions therefore signal through a narrow file channel. Every
**policy** launch mints `<data dir>/signals/<session>/`, `0700`, exposes **that
directory and nothing else** read-write, and exports the file inside it as
`FRIRING_SIGNAL_FILE`. The bundled hook payloads branch on that variable: set,
they append a state word to the file; unset — every unsandboxed session — they
run `friring-cli session signal` exactly as before. The database stays outside
every boundary, see [ADR-29](#adr-29-the-database-never-enters-a-sandbox).

The file is written from inside the boundary, so it is read as hostile input.
The host's status poll **takes** it with a `rename(2)` into the signals root —
which no sandbox is granted — before inspecting anything: that one syscall is
atomic, follows no symlink and opens nothing, so what friring then examines is a
fixed inode the agent can no longer swap. Only a regular file is read (a FIFO
would block the render loop until a writer appeared; a symlink would redirect the
read out of the boundary), only up to 4 KiB (enforced again during the read,
because a descriptor the agent still holds keeps writing after the rename), and
only as UTF-8. The text is then matched against a closed vocabulary — `idle`,
`working`, `blocked`, `done` — and what reaches the database is the matched
constant, never the file's own bytes. Anything else is dropped whole, and the
next file is read normally. Taking rather than peeking also delivers each write
once and consumes what a crashed run left behind; minting clears the file, so a
`done` from before a restart is never replayed.

The directory is minted and torn down with the session's other per-launch state,
alongside the scratch directory and the egress proxy.

**A place reports through a tmux pane option**, not through the file channel.
The rewrite is the one that already solves this for SSH hosts: friring's own
hook configuration is projected into the place with every
`friring-cli session signal --state X` turned into
`tmux set-option -p @friring_state X`, which the control-mode subscription
delivers. Both halves of the CLI call are unavailable in there — the binary need
not be in the image, and the database is what ADR-29 keeps outside every
boundary — while `set-option -p` needs no socket, no pane id and no identity.
The launch points the agent's own argument at the projected copy instead of
dropping it. One marker spells `friring-cli session signal --state` for both
paths, so the SSH rewrite and the place rewrite cannot drift apart and kill
status on one of them. A launch whose hook payload could not be projected says
so on the session's `Sandbox:` row, because a session that silently never leaves
`idle` looks like a broken session.

## UI

Everything below clones machinery that already exists, so the feature adds
screens rather than patterns.

**Profile list** (`Modal::SandboxList`, `Alt+S` or `<leader> S`) — the automations
list: `n` new, `e`/`Enter` edit, `d` delete, empty-state hint. Each row shows the
resolved backend (`auto → seatbelt`), the path count, the network mode,
`shared place` when the backend it resolved to is a place — this row is where a
profile is picked for a second session, and picking a shared one is the decision
that puts two agents in one box — the state of its live places when it has any,
and — when the backend it would run on is not available here — the probe's own
reason. The row's place count is what friring **recorded**
(`sandbox_instances`), not what the engines hold now: making it live would cost
an engine listing on every list open and every post-sweep refresh. So a row can
read `3 places` while the confirmation below it says one — the row reports what
friring wrote down, the question reports what it is about to act on, and the
question is the one that is authoritative. The resolution is as invisible-free here
as at the creation step, and so is the availability: picking `docker` on a machine
with no engine says so on the row rather than at launch.

`s` stops a profile's places, `r` rebuilds them — they go, and a fresh one starts
from the profile as it reads *now* — and `p` runs the reclaiming pass for that
row. The first two take a container away from whatever is running in it, so each
is confirmed first: the question lands in the footer naming how many places go —
counted by asking the **engines**, not `sandbox_instances`, so a row a rebuild
left behind cannot inflate it and a container friring adopted cannot hide from it
— and how many sessions stop with them, and **`y` alone answers it**. `Enter` and
`d` already mean edit and delete on this list, so neither may double as yes;
anything else cancels, and so does the list being rebuilt. All three share the
reclaiming pass's single background slot, because two workers reconciling one
engine's containers would be two opinions about what to remove.

**Profile editor** (`Modal::SandboxEditor`) — the automation editor's shape:
text fields, `‹ ›` selectors, and its add/remove sub-list (`n add · d remove`,
`[`/`]` reorder) twice, once for paths (each row a path plus a `‹ ro | rw ›`
selector) and once for allowed domains. The profile has more selector-shaped
knobs than the two the sketch above named, and all of them are rendered:
backend, network mode, read scope, per-path mode, the `prompt_new_domains` and
`allow_unsandboxed_fallback` toggles, and `containerfile`. A capability the
chosen backend cannot honour stays **visible but inert**, with the reason in
place of its value, and drops out of the saved profile so an invisible value can
never decide a save. The **network mode** is gated on the same authority
(`Caps::network_modes`) but keeps its row, because it is a value rather than a
knob: the selector walks only the modes the backend enforces, and the row names
what is missing — muted while the profile's mode is one of them
(`apple-container can enforce only full`), and as a warning when it is not, since
that profile's own launch would refuse it. Unlike a resource cap, an
unenforceable mode is **refused at the save** rather than dropped: substituting
`none` would take the network away from an agent that needs it and `full` would
grant egress nobody authorised, so the two ways out — a mode this backend
enforces, or a backend that enforces this mode — are the user's.
An unresolved `auto` rules nothing out. A backend the host
cannot offer stays selectable — the user may be about to install the engine, or
authoring a profile for another machine — with the probe's reason on the row.

`network_deny` has no editor: denies beat allows in every mode, and the list
is carried through a save untouched rather than dropped, but authoring one is
import/CLI-only for now. The allow list stays editable under `none`/`full`,
annotated `(inactive — network is full)`.

Path entry is a plain text field. Reusing the repo picker's live completion
needs a refactor first — that code is a private `App` method bound to
`Modal::RepoPicker` — so it follows rather than ships with the screen.

A profile that resolves to a place also says what sharing it costs, pinned in
the footer under the shape line: `sessions sharing a place are not isolated from
each other` / `a profile per session gives each session a place of its own`.
This is the screen where that is decided, and the footer is pinned because the
body scroll-windows — a caveat that can scroll out of sight is one that gets
missed.

Validation returns a message that surfaces through the existing error toast;
there is no inline form-error widget today and adding one is deferred. It runs
the launch's own path refusals at the *save*, so a profile that could never be
launched is a form that will not store: a read-write root reaching the data
directory, the database file, a tmux socket directory or Friring's own
`agents.toml`/`hosts.toml`/`config.toml`, a path in either mode reaching
Friring's own sandbox state or a container engine's control socket, and a network
mode the chosen backend cannot enforce.

**Session creation** — a dedicated step in the `Ctrl+N` sequence, after
directory selection so it can rank profiles covering the chosen directories
first and label the ones that do not. Every profile stays selectable: the wizard
knows the launch cwd, not what the user intends to reach from it. The step shows
each profile's resolved backend and the reason it is unavailable when it is,
is skipped entirely when no profile exists,
keeps its choice in the wizard state, contributes a breadcrumb line, and steps
back to the repo palette (Esc from the name modal returns to it with the
previous answer selected). *Create a sandbox for this selection* with pre-filled
paths is not built yet — the step offers the stored profiles and `none`.

**Place-backed sessions** are marked by their backend rather than by a second
indicator: `backend_type` is `sandbox:<profile>`, and a place that is not running
turns every session in it into an unreachable placeholder whose pane says so —
naming the place, and saying that they all stopped together — while friring keeps
trying to start it again. They carry no remote-host mark: a place runs on this
machine and mounts its paths, so calling it remote would be untrue.

**Credentials and configuration** are on the same panel, one row each. The
`Sandbox:` row carries what the launch decided — the strategy in force, and for
a place what the lint pass did with the user's configuration (`config
projected: 14 files · 2 need a read-only mount`). A boundary the agent has no
credential in adds a `Login:` row naming the exact command to type in that pane,
and raises an ordinary status notice at the launch; both are absent whenever
there is nothing to do, and neither is persisted, so an adopted session claims
nothing about a launch it did not make.

The profile editor answers the same question **on demand**: `Ctrl+L` on a
place-backed profile runs the lint over the form as it reads at that moment and
lists, per agent that declares configuration, what would cross and what the user
still has a decision about — a `NeedsMount` entry names a read-only path they can
add without leaving the screen. On demand rather than live because the pass reads
the user's configuration off disk and classifies every entry, and an editor that
stats and parses a dozen files per keystroke is one nobody keeps open; the panel
says which form it answered about, and asking again is one key. It covers **the
user's** configuration: friring's own hook payload crosses on top of it, and
which file that is comes from the launch's own arguments, so it stays a launch
fact on the session's `Sandbox:` row. A policy profile is not offered the key at
all — nothing is projected, because the agent reads the host's own configuration
where it always did.

**Indicators** — two `SessionInfo` fields carry it, and they say different
things. `sandbox_profile` (persisted) is the boundary the session **asked
for**: it survives a deleted profile and a fallback launch alike, because it
is what the next relaunch rebuilds from. `sandbox_state` (half-persisted) is
what the last launch **applied**: `Applied` carries the resolved backend and
the inner-sandbox composition, `Unenforced` carries the reason there is no
boundary. The session-list prefix marks read
the applied state, not the desired one — `⛨` beside the remote and worktree
marks for a boundary that is in effect, `⚠` for a session running on the host
under a profile that could not be applied — and the info panel's `Sandbox:`
row spells the same distinction out. A fallback also raises an error toast at
the moment of the launch, and `friring-cli session create|restart` prints it
(`sandbox_unenforced` in the JSON, which `session get|list` now also report per
session). An `Unenforced` reason is persisted
(`sessions.sandbox_unenforced`, schema v48); an `Applied` composition is not,
because the next launch re-derives it and a stale positive claim is the one
thing worse than no mark at all. So a session friring only adopted still
inherits the warning the launch recorded, and `sandbox_state` is `None` — the
persisted profile the only evidence — just when no launch ever recorded one.
The profile also appears in the creation breadcrumb.

The applied state survives friring itself, so an adopted session shows what the
launch it is adopting really did. Only the negative half is stored: the column
is a reason or nothing, which means no value it could ever hold — hand-edited,
truncated, written by an older binary — can manufacture a claim of protection,
and the write is skipped entirely by a writer that has no launch of its own to
report, so a full-row write-back can preserve a warning but never erase one. A
launch that regains the boundary clears it.

**Firewall prompts** — a refusal for an unlisted domain raises a status line
and, under `prompt_new_domains`, a confirm modal naming the session, the host
and the port; the answer applies to the running proxy and is written back to
the profile. It is the one confirmation whose grant key is `y` rather than
`Enter`, and the one that ignores keys for a moment after it appears, because
it is the one an agent can put under the user's hands unasked. See
[First-use domain prompts](#first-use-domain-prompts) for what is asked, what
is only reported, and why the same host is never asked about twice.

UI polish is explicitly a later pass. The first implementation aims for correct,
complete and consistent with existing screens.

## Failure modes

- **Probe failures** are actionable text, not a missing option: the AppArmor
  fix for user namespaces, "Docker is not running", "requires macOS 26",
  "WSL1 is not supported".
- **Dead instances** surface as dead panes with the error visible, because
  sessions keep `remain-on-exit`.
- **Unavailable capabilities** are shown as unavailable — memory limits on a
  policy backend, per-sandbox limits on WSL — rather than accepted and ignored,
  and a stored profile that carries one anyway is refused at launch with the
  reason, because the editor greying a field is not the same as the boundary
  honouring it.
- **Escape hatch.** Every profile carries an explicit
  `allow_unsandboxed_fallback` switch. Escape hatches exist in every comparable
  product; the design makes this one visible and per-profile instead of
  ambient. When it fires the session keeps its profile, the indicator switches
  from `⛨` to `⚠` — and that switch is persisted, so it survives a friring
  restart and reaches another instance that adopts the session — and the reason
  reaches the user as an error toast (TUI) or on the command's own output
  (`friring-cli`) — not only the log.
- **A failure never spends the credential it reports.** Log files persist, get
  shared, and get pasted into bug reports, and the proxy's token plus the port
  it names is a working local egress channel for anything else on the machine.
  Every tmux failure that quotes a command redacts it first, and where
  legibility and redaction pull against each other redaction wins:
  over-redacting a diagnostic costs a detail, under-redacting one costs the
  boundary.
  That covers `Debug` as much as log text. The composition a launch hands back
  carries the proxy URLs in its environment — the proxy's credential is *in*
  them, and never in the secret-environment channel an agent's own token uses —
  so its `Debug` prints those variables by name with the values held back. One
  `{:?}` in a tracing field or a failing assertion is a log line like any other.
- **The headless launch path still puts the proxy variables in a tmux client's
  argv.** The TUI composes a window's environment into a control-mode command
  over the tmux socket, which never reaches a process table; `friring-cli`
  has no control connection and passes each variable as a `-e KEY=VALUE`
  argument instead. On Linux and WSL another local user can read
  `/proc/<pid>/cmdline` by default, so for as long as that `tmux` client runs
  they can read the sandbox's proxy URL and use its egress. The exposure is
  bounded by the CLI's own lifetime — a proxy lives in the process that started
  it, so a `friring-cli`-created sandbox's instance is gone the moment the
  command exits — and macOS restricts cross-uid argv reads. Closing it properly
  needs a route into the window's environment that is not argv, which tmux does
  not offer; it is called out here rather than papered over. A place-backed
  launch is not affected: it spawns through the same control-mode `new-window`
  the TUI uses, so its environment reaches the window over the connection rather
  than through any client's argv.
- **A place that goes takes every session in it.** The two shapes fail
  differently and the UI says which (ADR-26): a policy sandbox's death is one
  dead pane, and a place's is every pane of that profile at once. Those panes
  become unreachable placeholders naming the place rather than a host, friring
  keeps trying to start it again, and they reattach by themselves when it comes
  back.
- **A place carries the safe subset of the user's configuration, and says what
  it left behind.** Instructions, skills and commands cross; a hook or an MCP
  server naming a host path does not, and neither does any credential (ADR-28),
  so the agent signs in inside the pane. The session's `Sandbox:` row and the
  launch's log line carry the count and the verdicts, because an agent that
  silently has none of the user's setup looks like a broken one.
- **A place starts signed out until somebody signs it in.** The synthetic home
  is per profile and fresh the first time, so unless friring holds a token for
  that agent the first session in a new profile opens at a sign-in prompt. That
  is stated three times over rather than left to be discovered: an ordinary
  status notice at the launch, a `Login:` row on the info panel naming the exact
  command, and a line on `friring-cli session create`'s own output
  (`sandbox_login` in its JSON). One login serves every session of the profile,
  and it survives a container rebuild.
- **A place whose image has no agent is refused, not launched.** A place runs
  the agent inside itself, so a missing binary is a pane that dies the instant
  it opens — taking the sign-in that happens *in that pane* with it. friring
  asks the place for the program before it composes anything and refuses with
  the install command filled in, through the profile's own
  `allow_unsandboxed_fallback` switch like every other boundary it will not
  grant. An `exec` that could not run at all (no `/bin/sh`, an image built for
  another architecture) is reported as that instead, because "install the
  agent" would be the wrong fix.
- **Deleting a profile keeps its tree while sessions are still in it.** The
  synthetic home is bind-mounted into a container the delete does not stop, so
  removing it would take `$HOME` away from a live agent mid-turn, unlink the
  egress sockets its siblings are talking through and destroy the login. The
  confirmation says the tree and the login are kept; the reclaiming pass removes
  the container once nothing is running in it, and collects the tree in the same
  sweep. That pass only collects a tree when every engine it can drive answered
  and every removal it attempted succeeded — "the engine would not say" has to
  read as "something may still be running in there".
- **A credential never rides a command line, so a launch that has only one is
  refused.** The local headless spawn passes a window's environment as
  `tmux -e KEY=VALUE` argv; a launch carrying an injected token there is
  refused with "start the session from the TUI" rather than exposed on the
  process table or quietly started without its token. Unreachable today —
  only a place injects one, and a place never spawns through that path — and
  checked anyway, so the next strategy cannot make it reachable in silence.
- **An unreadable profile** is refused at launch, named column by column, and
  repaired by re-saving it in the editor. Listing it permissively and
  launching it permissively are different decisions: the first keeps it
  fixable, the second would run a policy nobody wrote.
- **A boundary friring will not grant** fails the launch with the reason and
  the fix, whatever the profile asked for: read-write roots reaching the data
  directory or the database file, a tmux socket directory, or friring's own
  `agents.toml`/`hosts.toml`/`config.toml`; a path in either mode reaching
  friring's own sandbox state or the container engine's control socket; denies
  under a network mode that cannot enforce them; a filtered mode on a place that
  could not be shown to reach the proxy; a path that cannot be spelled exactly.
  Refusing is the whole point — every one of these has a silent alternative that
  grants more than the profile says. A refusal routes through the profile's
  `allow_unsandboxed_fallback` switch like any other, so a profile with it on
  runs the agent on the host instead; the refusals below are the exception.
- **A boundary whose own state has been interfered with is refused outright, and
  the escape hatch does not apply.** That switch means "this host cannot apply
  this profile" — a backend that is not installed, a policy this build cannot
  express. It must not also mean "the state friring builds the next boundary out
  of is wrong, so run outside it", because then an agent's way out of its
  sandbox is to break it: the scratch directory and the place tree are writable
  from inside by design, and a sandboxed agent can plant a symlink where friring
  mints a directory, or bind its own listener at the socket path its next launch
  needs. So a mount source that travels through a symlink, a directory friring
  mints that has become a link, and an egress proxy that will not bind all fail
  the launch whatever the profile says. The cost is stated rather than hidden: a
  user whose profile legitimately names a symlinked path must name the resolved
  one, and a filtered profile whose proxy cannot start refuses instead of
  running unfiltered — which is the same trade ADR-27 already makes.
- **A headless prune protects more than the background pass does.**
  `friring-cli` drives no session, so it cannot know which container one is
  running in: it protects every place of every profile a live session names —
  the conservative half of the TUI's rule, applied always. The cost is a
  superseded container surviving a `sandbox prune` while any session of its
  profile is alive.
- **An engine friring cannot drive is holding nothing; one that will not answer
  may be holding anything.** Both look the same to a command that failed, so the
  reclaiming pass asks the *probe* instead: a backend that is not installed
  created no place and is skipped in silence, and one that is installed and
  would not answer stops the pass collecting any place tree that sweep. Without
  the distinction a Mac with only Docker would never reclaim a tree, because two
  of the three engines the pass walks can never answer there.

## Testing and privacy

**The user's real agent state is off limits.** No test, script, fixture,
harness or agent working on this feature may read, copy, mount or otherwise
touch the user's personal agent directories, credential stores, keychain items,
`.env` files or tokens. This is a hard constraint on the feature *and* on the
work that builds it.

Concretely:

- Tests use fabricated home directories under the test temporary directory,
  populated with synthetic config. The e2e harness already builds isolated
  config and data directories; sandbox tests extend that, and never point at a
  real home.
- Credential handling is tested with fake tokens and a stub endpoint. Nothing
  reads a real credential store, including through the keychain-extraction
  path, which is exercised against a stubbed command.
- The proxy is tested against a local test server: allow, deny, deny-reason,
  token rejection, SOCKS and CONNECT parity, method restriction, a live policy
  update turning a refusal into a tunnel, and the same three through the relay
  and its unix socket. Every listener binds `127.0.0.1:0` or a path in the test
  temporary directory, and no test opens a connection off the machine.
- The first-use prompt is tested at the seam it actually spans: a refusal on the
  proxy's own channel, through the TUI tick, to a modal — and an answer through
  a real proxy instance to both the running policy and the stored profile.
- The bubblewrap relay launcher is *run*, not read: `/bin/sh` with the argv the
  backend composes, a non-existent path where the relay goes and `echo` where
  the agent goes, so a miscounted `shift` fails the test instead of shipping.
- **Every place backend is held to the same refusals**
  (`sandbox::tests::place_conformance`): friring's data directory *and the
  database file and `-wal` inside it* (ADR-29), another profile's place tree, the
  tmux socket directory, a container engine's control socket, a read-write root
  containing the program that applies the boundary, a mount source reached
  through a symlink, and identical absolute paths in both renderers. Asserted
  through the seam the launch path uses, so a backend that skipped one would run
  on to an engine command the stub has no answer for and fail on the sentence
  rather than on the refusal. **`wsl-distro` is in that table**, driven through
  its own two seams — `ensure_distro`, then `wrap` — because it hands back a
  distro name rather than an `EnsuredPlace` and has no `PlaceBackend` to go
  through. Its refusals are latent until a session launches into a distro, which
  is exactly why they are pinned now: the moment that path is wired is the moment
  nobody is looking at them. The WSL stub scripts an *existing* distro, so the
  whole path exercised is `adopt`; registering one is not scripted at all, and a
  refusal that stopped being one would fail on the unscripted `--export` rather
  than import a distro. The projection lint the editor runs is tested
  against a **fabricated** home with synthetic files, never the author's own.
- Backend probes and argv/profile generation are unit-testable without running
  the backend; where a backend is present in CI, integration tests run behind a
  capability check and are skipped with a reason otherwise.
- **No test starts, pulls or builds a container** or a virtual machine, none
  creates a network, and none registers, imports or unregisters a WSL distro.
  Every engine, `container` and `wsl.exe` command in the place backends' tests
  goes through the injected probe host, so the argv, the plan, the
  garbage-collection decision and the whole launch composition are exercised
  with no engine installed. The one capability-gated test runs `info`
  and the label-filtered `ps` against a real engine when there is one and skips
  with a printed reason otherwise; it creates nothing and only ever sees
  friring-labelled containers. Nothing is ever bind-mounted from the author's
  own agent configuration, in a test or anywhere else — a place's home is
  friring's own directory (ADR-28).
- Documentation examples use placeholder paths, never the author's own.

## Delivery phases

Each phase is independently useful and lands with its own tests, docs and
`FORK.md` entry.

**P1 — Policy sandboxes.** The `sandbox` module, profile storage and migration,
the profile list and editor, the session-creation step, the session indicator,
`seatbelt` and `bwrap` backends, network `none` and `full` only,
`host-passthrough` credentials. Local sessions only — see
[Launch integration](#launch-integration) and [Status signals](#status-signals).

**P2 — The firewall.** Shipped: the Rust filtering proxy and its in-namespace
relay, the `allowlist` network mode, `full`-with-denies proxied, both policy
backends wired to a per-session instance, host canonicalisation on both sides of
the matcher, the refusal to dial the host's own loopback, first-use domain
prompts and their persistence, and the persisted applied state. Not in it: an
HTTP-method restriction has no profile column (the proxy supports one; nothing
sets it), a proxy dies with the friring process that started it, the headless
launch path still passes the proxy environment as `tmux` client argv (see
[Failure modes](#failure-modes)), and the relay binds a few milliseconds after
the agent starts — a request in that window gets one connection refused, which
fails closed.

**P3 — Place sandboxes.** Shipped: the `docker`/`podman` backend and the sandbox
transport, instance lifecycle and garbage collection, identical-path mounts, the
default image (`packaging/sandbox/Containerfile`, tag `friring/sandbox:1` —
friring publishes no registry image, so a missing one is refused with the build
command), resource limits, the per-profile synthetic home, the default image
deliberately carrying no agent CLI — a launch whose agent is not in the place is
refused with the one-time install command — and the signal-file channel for
policy backends. Not in it: a place on a remote host is not wired — the backend
probes and creates where friring runs, and the mount-source symlink rule has no
authority over another host's filesystem, so it does not apply there; nothing
installs an agent for you — friring prints the command and the registry declares
no install, so `<the package that provides 'x'>` stays a placeholder; and tmux
and `/bin/sh` are assumed of an image rather than checked, so a tmux-less one
surfaces as a control-mode stall.

**P3b — A usable place.** Shipped: the credential strategies
(`host-passthrough`, `env-token`, `volume-login`, `seed-file`) resolved against
the boundary, friring's own OS keychain entry with the value never on a command
line in either direction, config projection and its lint pass, enforced settings
with pre-seeded workspace trust, and the projected hook payload that makes a
place report status. Not in it: only JSON and TOML are linted (anything else is
copied verbatim), and nothing prunes a projected file the user has since deleted
on the host.

**P4 — Breadth.** Shipped:

- The **`apple-container`** place backend — probed for Apple Silicon, macOS 26,
  the tool, its system service and the `run`/`exec` options a place is built
  from; friring's own network; the container engines' mount plan, labels, spec
  digest and collection decision reused unchanged.
- The **`wsl-distro`** backend's own half, as code with no caller — a hardened
  clone per profile (`automount` and `interop` off, ownership marker, a checked
  `--terminate` before the hardening is verified), bubblewrap required inside it,
  VHD export/import, and the reclaiming primitives. Its probe and its
  capabilities are wired to the UI; nothing else about it runs. See
  [its entry](#wsl-distro--windows-place).
- **Copy-on-write workspaces on `bwrap`**, with the overlay probed by mounting
  one and the layers kept outside the sandbox's own writable scratch.
- The **`friring-cli sandbox`** commands — `list`/`show`/`rm`/`prune` over
  profiles and their places, `export`/`import` as TOML with the launch's own path
  refusals applied at the import, and `token set|rm|list` for the `env-token`
  value with the value never on a command line in either direction (the internal
  `sandbox relay` shipped in P2 and remains the one subcommand dispatched before
  the database is opened, ADR-29).
- The **profile list as a manager view**: places per profile, stop, rebuild and
  prune, with a confirmation on the two that take a container away.
- The **profile editor's lint verdicts**, on demand (`Ctrl+L`) rather than per
  keystroke, because the pass reads the user's configuration off disk.
- **One `PlaceBackend` seam** every caller of a place drives, and a conformance
  test holding all three container place backends — and, through its own two
  seams, `wsl-distro` — to the same refusals.

What P4 did **not** deliver is in the next section, along with everything else
this feature does not do.

## Not in it

One list, because a reader deciding whether to rely on this needs the gaps in one
place rather than spread through the phase notes above. Nothing here is a bug
report: each is a deliberate stopping point with the reason it stopped there.

### Backends and boundaries

- **No session launches into a `wsl-distro` place, and nothing else drives that
  backend either.** Registering, hardening, adopting, reclaiming and
  unregistering a distro are implemented and stub-tested with **no production
  caller**; `PLACE_KINDS` — which every lifecycle, teardown, reclaiming and
  `friring-cli` path walks — is the three container engines. What runs is the
  probe, the capabilities and the launch refusal. See
  [its entry](#wsl-distro--windows-place).
- **No filtered network mode on `apple-container`.** The proxy is unreachable
  across a VM boundary; closing it needs an ADR-27 endpoint change, not a backend
  change. `full` with no denies is the one mode that backend offers, and one
  network per profile.
- **A filtered place on a Mac or a Windows host is refused, not filtered.** The
  probe settles it per place instead of guessing, but where the engine's daemon
  is in a VM the answer is always no — and it needs the same ADR-27 endpoint
  change `apple-container` needs.
- **No native Windows process sandboxing.** Restricted tokens and AppContainer
  are not built; Windows isolation is `docker`/`podman`.
- **No place on a remote host.** A backend probes and creates where friring runs,
  and an `ssh:`/`wsl:` session carrying a profile is refused rather than composed
  around the wrong filesystem. The mount-source symlink rule has no authority
  over another host's filesystem, so it does not apply there.
- **No profile column selects a copy-on-write workspace.** The capability is
  built and reachable only through `BwrapBackend::wrap_copy_on_write`.
- **No pinned `container` CLI version.** Apple's option surface is read from
  `--help` at probe time, and everything this feature says about that tool is
  unverified against the real one — the fork was not written on Apple Silicon.
- **No inode-level containment.** Both policy backends protect by pathname, so a
  hardlink alias planted on the host before the launch is the one stated
  exception to ADR-29; see [Goals and non-goals](#goals-and-non-goals).
- **No isolation between the sessions sharing a place.** One uid, one pid
  namespace, one filesystem — the trust domain is the place. A profile per
  session is the lever.

### Egress

- **No TLS interception**, so inside a `CONNECT` tunnel the allow decision trusts
  the client-supplied hostname and domain fronting can bypass it.
- **No profile column for the HTTP-method restriction.** The proxy implements
  one; nothing sets it, so every launch runs with it off.
- **No `network_deny` editor.** Denies are carried through a save untouched, but
  authoring one is import/CLI-only.
- **A proxy dies with the friring process that started it**, so a session created
  by `friring-cli` has no egress until a running friring relaunches it.
- **The headless launch path passes the proxy environment as `tmux` client
  argv**, readable through `/proc/<pid>/cmdline` on Linux and WSL for that
  command's lifetime. See [Failure modes](#failure-modes).
- **The relay binds a few milliseconds after the agent starts**; a request in
  that window gets one connection refused, which fails closed.
- **No IDN support.** A host with any non-ASCII byte is refused in a request and
  in a stored rule alike, with the punycode form named as the fix.

### Credentials and configuration

- **Nothing installs an agent for you.** friring prints the command; the registry
  declares no install, so `<the package that provides 'x'>` stays a placeholder.
- **Only JSON and TOML are linted** by config projection; anything else is copied
  byte for byte.
- **Nothing prunes a projected file** the user has since deleted on the host.
- **No host-side credential broker.** `volume-login` removes the urgency.
- **`~/.ssh` is on the `host-minus-secrets` deny list**, which breaks git over
  SSH inside every profile using the default read scope.

### Lifecycle and UI

- **A rename does not move a profile's place tree or its `seed-file` marker**, so
  renaming a profile that has a place means signing in again. See
  [Reclaiming places](#reclaiming-places).
- **The profile list's place count is what friring recorded**, not what the
  engines hold now; the confirmation is the number that is acted on.
- **tmux and `/bin/sh` are assumed of an image**, not checked, so a tmux-less one
  surfaces as a control-mode stall rather than a refusal.
- **No orphan sweep for copy-on-write layers.** Teardown covers the normal path
  and deletion-while-running; a friring killed between a launch and a teardown
  leaves `<data>/sandbox/overlay/<key>` behind for the next launch of that
  session to adopt.
- **No "create a sandbox for this selection"** in the session wizard, and no live
  path completion in the profile editor — the repo picker's completion is a
  private `App` method that needs a refactor first.
- **No inline form errors** in the profile editor; validation surfaces through
  the error toast.
- **`auto` never picks `wsl-distro` on a native-Windows host** — it is chosen by
  pinning, which then refuses.
- **The per-command escape `allow_unsandboxed_fallback`'s name suggests is not
  built.** It decides what happens when the profile cannot be applied at launch,
  and nothing finer.

## ADR-25: Sandboxing is a core feature, not an extension

**Context**: Extensions are declarative data (ADR-20, ADR-21). An extension can
register an agent whose `command` is a wrapper script, so a minimal "sandboxed
agent" ships as an extension today.

**Choice**: Build sandboxing into the core, in a new `sandbox` module.

**Why**: Everything that makes the feature usable is outside what a manifest can
express — extensions contribute no UI, have no session-lifecycle hook, cannot
add a transport (the host kind is a closed enum), cannot add tables, and cannot
add a session indicator. A wrapper-script extension also cannot forward session
identity into a place reliably, so status reporting breaks. The extension route
remains valid for prototyping and for user-authored wrappers.

**Consequences**: A new module in the architecture allowlist (`sandbox` may
reference `session`, `paths`, `shell`; never `ui`, `git` or `app`), a schema
migration, and new modals. Sandbox *recipes* stay declarative data so they
remain inspectable in the spirit of ADR-20.

## ADR-26: Policy backends wrap argv; place backends are transports

**Context**: Sandboxing technologies split into process-scoped policies and
persistent environments.

**Choice**: Model both. Policy backends wrap the composed invocation with tmux
outside. Place backends are reached by a transport with tmux inside, mirroring
ADR-13's SSH/WSL transports.

**Why**: Forcing one shape breaks something. Wrapping a container start in a
tmux pane makes every session a separate container start and loses the
environment's persistence; putting tmux inside a policy sandbox gains nothing
and complicates reattach. The transport seam already exists and is
transport-agnostic at the control-mode layer, so a place backend is
substantially free.

**Consequences**: Two code paths, one profile model. Crash-survival semantics
differ per shape and the UI says which is in effect. A place backend also
composes the command that runs inside it, so "one shape, one half of the trait"
holds for policy backends and is one half short for places: the egress relay
lives inside the boundary, and only the backend that built the boundary knows the
binary and the port it gets.

## ADR-27: One egress engine — a Friring-owned filtering proxy

**Context**: Domain-level egress control has several possible mechanisms; the
backends have wildly different network primitives, and one of them (seatbelt)
cannot express host filtering at all.

**Choice**: Deny direct egress at the kernel level in every backend, and filter
by domain in a Friring-owned Rust proxy outside the boundary.

**Why**: It is the only mechanism whose semantics are identical across every
backend and over the existing remote transports, and the kernel-level denial
means ignoring the proxy is not a bypass. Resolved-IP allowlists break when
CDN addresses rotate. An external runtime would contradict the single-binary
distribution.

**Consequences**: Friring runs a network service while a sandbox is alive,
token-authenticated and scoped to host loopback, to a `0o600` unix socket, or
to both — a sandbox in its own network namespace can only reach the socket, and
reaches it through a Friring-run relay inside the namespace. One instance per
session, not per profile: the token and the socket belong to one launch, so a
denial is attributable, a grant applies where it was asked for, and a relaunch
rotates one session's credential alone; a launch with no identity of its own is
refused rather than sharing a key. Inside a *place* that is bookkeeping and not
separation — the sessions there share a uid and a pid namespace, so the place is
the trust domain (see [The two sandbox shapes](#the-two-sandbox-shapes)). Allow
and deny decisions are taken on a canonicalised host, and a host with no single
canonical spelling is refused in every mode rather than compared. A bare rule
names one host and the subtree is spelled `*.x`/`.x`, so the narrow reading is
the default and the wide one is written down. Because the proxy dials with the
host's reachability rather than the sandbox's, it refuses destinations local to
the host — loopback, unspecified, link-local — in every mode, and re-checks
after resolving a name, unless an allow rule names the literal address. Allow
decisions trust the client-supplied hostname inside a `CONNECT` tunnel until
TLS termination is added, which the UI discloses; on a plaintext request, where
the proxy *can* see a `Host` that disagrees with the authority it authorised,
it replaces it.

## ADR-28: Credentials are never copied per sandbox

**Context**: Vendor OAuth refresh tokens are single-use and rotating; the
macOS Keychain is unreachable from a VM.

**Choice**: Prefer host passthrough under policy backends; otherwise an
injected long-lived token or a per-profile login volume. Never copy a rotating
credential file into more than one place, and never bind the host agent
configuration directory read-write into a sandbox.

**Why**: Copies invalidate each other on first refresh, cascading logouts
across every sandbox. A writable host agent configuration is an escape channel
through hooks the host agent later runs.

**Consequences**: Place backends need one login per profile, or a token the
user supplies once. Each agent declares its credential strategy as registry
data. Enforced rather than advised: `host-passthrough` is structurally
unreachable in a place, `auto` never chooses a strategy that copies, and a
second profile seeding one credential family is refused by a marker friring
keeps outside every boundary.

## ADR-29: The database never enters a sandbox

**Context**: Sandboxed agents still need to report status, and the existing
mechanism writes SQLite directly.

**Choice**: Sandboxed sessions signal through a per-session file channel that
host Friring polls. The database is never mounted into a sandbox, read-only or
otherwise.

**Why**: Automations stored in the database carry shell commands executed by
the host. Database write access from inside a sandbox is therefore arbitrary
host command execution — a complete escape.

**Consequences**: One more signal path to maintain, reusing the existing poll.
Transport-reached places may instead reuse the remote hook rewrite, which
already avoids database access for SSH hosts. The database and its `-wal`/`-shm`
siblings are masked unconditionally wherever an ancestor is writable — a `-wal`
created after launch is replayed by the host on next open — and a profile that
**reaches** the data directory is refused outright: enclosing the directory, or
naming the database file or a sidecar. Both spellings of the directory are
compared, because the file need not exist yet and a data directory behind a
symlink would otherwise be missed. The refusal is checked at the editor's save,
at `sandbox import`, at the launch, and — for a place, twice — when the mount
plan is built and again immediately before the container is created. A place is
where it matters most: a policy backend has an unconditional mask underneath it,
and a place bind-mounts what the profile names with no mask at all, so the
refusal is the whole of the rule there. The one
residual is a hardlink alias planted on the host before the launch; see
[Goals and non-goals](#goals-and-non-goals).

A place is also never *told* where the database is. The `FRIRING_*` variables
naming Friring's own config, data and metrics directories are withheld from
every off-host launch — but that decision reads `sessions.backend_type`, which
does not say `sandbox:<profile>` until a session has been launched into a place
once, so the **first** launch of a place-backed session is composed as a local
one. They are taken back out where the invocation learns it is place-bound
(`agent::backend::window_env`), on both launch paths.
