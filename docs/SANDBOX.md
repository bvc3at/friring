# Sandboxed agents

How Friring runs a coding agent inside an isolation boundary — the profile
model, the per-OS backends, the egress firewall, credential handling, and the
UI that ties them together.

This document is the **design contract** for the feature: implementation work
reads it, and any change to the behaviour described here updates it in the same
change. Fork-visible divergences are also listed in [`FORK.md`](../FORK.md).

**Friring — fork-only.** Upstream has no sandboxing; see
[`FORK.md`](../FORK.md).

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
  sandbox costs a container start and needs an image containing the toolchain,
  the agent, and tmux.
- When a policy sandbox's process dies, one pane dies — the existing dead-pane
  UX. When a place dies, every session in it dies at once.
- Only place backends can enforce memory/CPU limits, and only place backends
  give a filesystem the host cannot see.

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
this codebase, and it is a fixed-schema file rather than a collection. A
`friring-cli sandbox export|import` command will cover portability (P4 — see
[Delivery phases](#delivery-phases)).

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

A read-write path is refused at launch if it encloses the friring data directory
(ADR-29) or reaches a tmux server socket directory; a security-relevant path
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

`allow_unsandboxed_fallback` is the visible, per-profile escape hatch. In P1 it
decides what happens when the profile **cannot be applied** at launch: off (the
default) fails the spawn, on starts the agent unsandboxed with the reason in
front of the user. The per-command escape the name also suggests arrives with
the place backends.

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
to prevent. One profile may therefore own several rows. `state` is free text
until a place backend exists to define the vocabulary.

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

## Backend catalogue

Every backend below is part of the design; **this release ships the two policy
backends (`seatbelt`, `bwrap`) only** — the place backends are probed as
unavailable with the reason, and land in P3/P4 (see
[Delivery phases](#delivery-phases)). Availability is probed per host, and the
session-creation UI shows what is available with the reason a backend was
excluded.

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
container, sub-second boot, OCI images, virtiofs mounts. Apple Silicon only;
macOS 26 or newer for isolated networks. Strongest isolation available on
macOS, and the backend most likely to improve over time, which is why it ships
alongside `seatbelt` rather than after it. No Compose and no Docker socket API,
which this feature does not need. amd64 images run under Rosetta.

### `bwrap` — Linux and WSL2, policy

Bubblewrap: per-path `--ro-bind` / `--bind`, `--tmpfs`, `--unshare-net`,
`--unshare-pid`, `--die-with-parent`. This is the industry mainline — Codex
made bwrap its primary Linux backend and Claude Code's sandbox uses it too.

- Version 0.11 or newer adds unprivileged overlays (`--overlay`,
  `--tmp-overlay`), which back the optional copy-on-write workspace mode. Not
  available when bwrap is installed setuid.
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
  both, with a probe distinguishing them.
- Mounts use **identical absolute paths** (see below). Network is `none` plus a
  bridge to the proxy.
- The agent runs as a non-root user; several agents refuse permissive modes as
  root.

### `wsl-distro` — Windows, place

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

### Identical absolute paths

Every place backend mounts each profile path at **exactly its host path**. This
is not a convenience:

- A git linked worktree references its main repository by absolute path and
  vice versa. Mounting both at their real paths is what keeps `git status`,
  `git log` and `git commit` working inside.
- Claude Code keys session transcripts by absolute project path, and Codex keys
  `projects.<path>.trust_level` the same way. A path mismatch silently breaks
  resume and re-triggers trust prompts.

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

A backend implements `wrap` or `ensure`, never both; the default bodies fail
with the shape mismatch, so forgetting the right half is loud. `probe` results
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
Windows side.

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
`--network none`. Because the kernel blocks everything else, a process that
ignores the proxy environment variables gets *no* network rather than an escape
route.

### Reaching the proxy

That same denial is why the arrow above is not one mechanism. **A sandbox with
its own network namespace cannot reach the host's loopback at all** — inside
`--unshare-net`, `127.0.0.1` is the sandbox's own loopback, and there is no
address that resolves to the host. The proxy therefore listens on two
transports, and each backend uses the one its kernel primitive leaves open:

| Backend | Transport | Why |
|---|---|---|
| `seatbelt` | host TCP loopback | Shares the host network stack; the profile denies non-loopback traffic but leaves the proxy port reachable. |
| `bwrap`, `docker`/`podman` on `--network none` | unix socket + relay | A new network namespace has no route to the host. A unix socket is a filesystem object, so a bind mount carries it across. |
| `wsl-distro` | as `bwrap`, inside the distro | Per-distro firewalling is impossible (one shared VM network namespace), so egress control comes from `bwrap` inside the distro — and so does its transport. |

No mainstream HTTP or SOCKS client can *dial* a proxy over a unix socket:
`HTTP_PROXY` and `ALL_PROXY` take a host and a port. So a small relay runs
**inside** the namespace, offering a TCP endpoint and forwarding each
connection to the bind-mounted socket:

```text
agent  →  127.0.0.1:PORT   (the sandbox's own loopback)
       →  friring-cli sandbox relay
       →  /…/proxy.sock    (bind-mounted from the host)
       →  friring proxy    →  policy  →  upstream
```

This is the shape such setups usually build out of `socat`; Friring ships it
instead, so it inherits the same timeouts, connection cap and clean shutdown as
the proxy. The relay is protocol-agnostic — it never parses a byte, so
`CONNECT` and SOCKS5 both cross unchanged — and it holds **no credential and no
policy**: the proxy's token is still demanded at the far end, and the decision
is still made outside the boundary. Giving the relay either would put both
inside the boundary they exist to constrain.

The socket is `0o600` by default. It is a credential-bearing endpoint, and a
backend whose sandbox runs as a different uid (containers usually do) has to
widen that deliberately rather than inherit a world-connectable socket.

Because the endpoint shape differs, a backend rejects the wrong one rather than
silently failing later: `bwrap` refuses a loopback endpoint and says why.

Until a backend is wired to the proxy, `allowlist` configures the kernel exactly
like `none` — the two are identical at that layer, so `allowlist` grants nothing
on its own. That is why a new profile can default to `allowlist` with an empty
list and still start closed.

**Allowlist matching** is suffix matching on label boundaries, case-insensitive:
`github.com` covers `api.github.com` but not `evilgithub.com`,
`github.com.evil.net` or `github.co`. `*.x` and `.x` are spellings of `x`,
apex included — the deny direction decides it, because a user refusing `*.x`
means "no x traffic" and a matcher sparing the apex would be a silent hole.
A rule without a port covers every port. Denies are checked **first in every
mode**, so a deny entry narrows `full` too — and until the proxy exists, a
`full` profile that carries denies is **refused at launch** rather than started
with rules no kernel policy can express. `prompt_new_domains` is meaningful
only under `allowlist` — nothing is unlisted under `full` and nothing leaves
under `none` — and the editor greys it out elsewhere.

Two matchers implement this vocabulary: `session::DomainRule`, which the
profile validator and the UI use, and `proxy::HostRule`, which the proxy
enforces at connection time. They deliberately do not share a type — the proxy
is a leaf in the architecture allowlist — so
`tests/egress_matcher_conformance.rs` runs both over one table of stored
spellings and fails if either drifts. Both accept the same grammar: ASCII
labels of letters, digits, `-` and `_`, no empty label, none edged with `-`,
63 bytes per label and 253 overall. A spelling no request host could ever
carry is refused rather than stored as a rule that matches nothing, and an
international name is written in punycode because that is how it is spelled on
the wire.

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
  bare rule covers its own subtree on a label boundary (`github.com` matches
  `api.github.com`, never `evilgithub.com`); `*.github.com` and `.github.com`
  are spellings of that same rule and cover the apex with it; an address rule
  is exact.
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
- Denials carry a reason and surface as a TUI event; with
  `prompt_new_domains`, an unlisted domain raises a confirm modal whose answer
  is written back to the profile and applied to the running proxy without a
  restart.
- Optional HTTP-method restriction (`GET`/`HEAD`/`OPTIONS` only) as a cheap
  brake on exfiltration through allowed hosts. It reaches **plaintext HTTP
  only** — a `CONNECT` tunnel is opaque, so the method inside it is unknowable
  — and each forwarded plaintext request gets its own connection, so every one
  of them is policed rather than only the first on a reused socket.
- No TLS interception in the first release. Allow decisions therefore trust the
  client-supplied hostname, so domain fronting can bypass them — documented in
  the UI, not hidden.
- Both matchers compare ASCII, and neither treats `127.1` or `2130706433` as
  the address `getaddrinfo` resolves them to. Under `allowlist` that fails
  closed: an unlisted spelling is denied. It becomes a real gap only for a
  *deny* list under `full`, which is refused at launch until the proxy is wired
  — and the fix belongs in the proxy's request path (normalise the host to
  A-labels, or refuse a non-ASCII request host outright), not in the matchers.

Written in Rust rather than shelling out to an external runtime: Friring ships
as a self-contained binary, and a CONNECT/SOCKS filter is a small, testable
component. The policy vocabulary deliberately mirrors the de-facto standard
(`allowRead`/`denyRead`/`allowWrite`/`denyWrite`/`allowedDomains`/
`deniedDomains`/`allowUnixSockets`) so profiles stay legible to anyone who
knows the ecosystem.

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
2. **`env-token`** — a long-lived token the user supplies once. Friring stores
   it in its own OS keychain entry and injects it at instance creation. No
   rotation, no races. This is the recommended path for place backends.
3. **`volume-login`** — a per-profile named volume holding the agent's state
   directory, with one interactive login per *profile*. Both major agents have
   TUI-compatible headless flows (paste-code, device-code), so the login
   happens inside the session pane. Sessions sharing a profile share the
   volume, which is the safe single-writer case. This is the answer to "logging
   in for each sandbox is bad UX": login is per profile and effectively annual.
4. **`seed-file`** — copy a credential file in once and honour write-back.
   Opt-in, and only for agents whose vendor documents it.

Never: bind-mounting the host agent configuration directory read-write into a
place. It is useless on macOS (the credentials are not in the file) and it is
an escape channel — an agent that can write the host's agent settings can plant
hooks that the *host* agent later executes outside the boundary.

Each agent declares what it needs, in the registry, as data:

```toml
[agents.<name>.sandbox]
config_dir_env = "…"     # env var relocating agent state into the sandbox
auth           = "auto"  # host-passthrough | env-token | volume-login | seed-file
state_rw       = […]     # directories the agent writes and must keep
copy_in        = […]     # config safe to project, subject to the lint pass
env            = { … }   # static env (e.g. disable self-update in a place)
secret_env     = […]     # names of tokens Friring may inject from its keychain
bypass         = […]     # flags meaning "the outer boundary is the sandbox"
writeback      = true    # refreshed credentials must persist
login_fallback = "…"     # how to log in inside the pane when state is empty
```

Friring never reads a credential file to inspect it, and never logs credential
contents. A future host-side credential broker (the sandbox asks, the host
refreshes) is the theoretically cleanest endpoint and is deliberately deferred:
`volume-login` removes the urgency.

## Config projection

A place sandbox gets a **synthetic per-profile home**, never a bind of the
host's agent configuration. Safe configuration is copied in through a lint
pass, because agent config routinely references the host filesystem:

- Lifecycle hook commands, status-line commands and credential-helper scripts
  are arbitrary shell, usually with absolute host paths.
- Plugin, skill and rule directories may live outside the config directory.
- Stdio MCP servers name host binaries.

The lint pass classifies every such entry as *projectable*, *needs a mount*, or
*host-only*, and the profile editor surfaces the result ("3 hooks reference
host paths — mount read-only, drop, or rewrite?"). The `friring-autonomous`
rig's hard-coded read-only hooks mount becomes a per-entry choice.

Enforced settings go in through each agent's highest-precedence configuration
layer, so a repository-level file cannot override the orchestrator's intent —
including pre-seeded workspace trust, which several agents otherwise prompt for
on first run inside a fresh home.

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

**A policy backend applies to a local session only, in P1.** Both shipped
backends generate their artefacts (a `.sb` profile file, an argv naming local
paths) on the machine friring runs on, so an `ssh:`/`wsl:` session with a
profile is refused rather than wrapped with the wrong machine's answers.
Sandboxing a remote session arrives with the place backends.

**Place backends** add an ensure-instance step before spawn and then use a new
`TmuxTransport::Sandbox` variant, built exactly like the existing SSH transport
(a command prefix wrapping the tmux argv). Control mode is transport-agnostic
by design, so discovery, adoption, input and scrollback need no changes.
Materialising agent configuration into a place generalises the existing
remote-argument adaptation: a sandbox is a third kind of "elsewhere".

Two details that silently break things if missed:

- **Environment forwarding.** Session environment is set on the tmux window,
  which is *outside* a policy sandbox and on the *host* side of a place. Neither
  policy backend applies environment in argv — a policy is a rule on a process,
  so the wrapped agent inherits the window — which makes the window the only
  channel inward, and makes "the wrap only ever **adds** environment" an
  invariant with a test on it. Host-only path variables must be skipped or
  translated exactly as the remote path already does. Without this, status
  reporting dies quietly.
- **Resume identity.** A place keeps agent transcripts inside its own volume,
  so a resume by id can target a transcript that does not exist there. The
  launch path detects this and starts a fresh session under the requested id
  instead, so later restarts resume normally. (The `friring-autonomous` rig
  proved this pattern in a wrapper script; it belongs in core.)
- **Scratch and policy files.** friring mints a per-session directory under
  `<data dir>/sandbox/tmp/` for the agent's scratch and writes the generated
  seatbelt profile to `<data dir>/sandbox/profiles/`, both `0700`, both refusing
  a symlink. Neither may be the host temp root, and a profile is refused if its
  read-write roots enclose the data directory or reach a tmux socket directory.
  The scratch is keyed on the session and adopted, not recreated, so a crashed
  run's files survive into the next launch; session teardown drops both.

## Status signals

Agents report working/blocked/done by running `friring-cli session signal`,
which writes SQLite directly. Neither half of that works inside a sandbox:

- The binary may not exist there (a Linux place on a macOS host).
- **Database write access is a sandbox escape.** Automations stored in the
  database carry shell commands that the *host* Friring executes. An agent that
  can write the database can schedule arbitrary host commands.

Sandboxed sessions therefore signal through a narrow file channel: a
per-session directory under the data directory, mounted read-write, into which
the hook writes a small status file that host Friring picks up on its existing
poll. The database stays outside every boundary — see
[ADR-29](#adr-29-the-database-never-enters-a-sandbox). Place backends reached
through a transport may alternatively reuse the existing remote hook rewrite,
which already solves the same problem for SSH hosts.

The file channel ships with the place backends. **Until it does, a sandboxed
session does not report status**: the database path is denied on every launch
(with its `-wal`/`-shm` siblings), so `friring-cli session signal` from inside
fails and the session shows as idle. That is the correct trade — a session that
looks idle is recoverable, a sandbox that can schedule host commands is not.
The database path is a *launch input*, not something the sandbox layer resolves:
the friring that owns the session is not necessarily on the host where the agent
runs.

## UI

Everything below clones machinery that already exists, so the feature adds
screens rather than patterns.

**Profile list** (`Modal::SandboxList`, `Alt+S` or `<leader> S`) — the automations
list: `n` new, `e`/`Enter` edit, `d` delete, empty-state hint. Each row shows the
resolved backend (`auto → seatbelt`), the path count and the network mode; the
resolution is as invisible-free here as at the creation step. Instance state and
the stop / rebuild / prune actions arrive with the place backends.

**Profile editor** (`Modal::SandboxEditor`) — the automation editor's shape:
text fields, `‹ ›` selectors, and its add/remove sub-list (`n add · d remove`,
`[`/`]` reorder) twice, once for paths (each row a path plus a `‹ ro | rw ›`
selector) and once for allowed domains. The profile has more selector-shaped
knobs than the two the sketch above named, and all of them are rendered:
backend, network mode, read scope, per-path mode, the `prompt_new_domains` and
`allow_unsandboxed_fallback` toggles, and `containerfile`. A capability the
chosen backend cannot honour stays **visible but inert**, with the reason in
place of its value, and drops out of the saved profile so an invisible value can
never decide a save. An unresolved `auto` rules nothing out.

`network_deny` has no editor: denies beat allows in every mode, and the list
is carried through a save untouched rather than dropped, but authoring one is
import/CLI-only for now. The allow list stays editable under `none`/`full`,
annotated `(inactive — network is full)`.

Path entry is a plain text field. Reusing the repo picker's live completion
needs a refactor first — that code is a private `App` method bound to
`Modal::RepoPicker` — so it follows rather than ships with the screen.

Validation returns a message that surfaces through the existing error toast;
there is no inline form-error widget today and adding one is deferred.

**Session creation** — a dedicated step in the `Ctrl+N` sequence, after
directory selection so it can rank profiles covering the chosen directories
first and label the ones that do not. Every profile stays selectable: the wizard
knows the launch cwd, not what the user intends to reach from it. The step shows
each profile's resolved backend, is skipped entirely when no profile exists,
keeps its choice in the wizard state, contributes a breadcrumb line, and steps
back to the repo palette (Esc from the name modal returns to it with the
previous answer selected). *Create a sandbox for this selection* with pre-filled
paths is not built yet — the step offers the stored profiles and `none`.

**Indicators** — two `SessionInfo` fields carry it, and they say different
things. `sandbox_profile` (persisted) is the boundary the session **asked
for**: it survives a deleted profile and a fallback launch alike, because it
is what the next relaunch rebuilds from. `sandbox_state` (not persisted — it
describes a running process) is what the last launch **applied**: `Applied`
carries the resolved backend and the inner-sandbox composition, `Unenforced`
carries the reason there is no boundary. The session-list prefix marks read
the applied state, not the desired one — `⛨` beside the remote and worktree
marks for a boundary that is in effect, `⚠` for a session running on the host
under a profile that could not be applied — and the info panel's `Sandbox:`
row spells the same distinction out. A fallback also raises an error toast at
the moment of the launch, and `friring-cli session create|restart` prints it
(`sandbox_unenforced` in the JSON). `sandbox_state` is `None` for a session
friring only adopted: it did not make that launch, so the persisted profile
is the only evidence it has. The profile also appears in the creation
breadcrumb.

Because the applied state is not persisted, an adopted session renders as `⛨`
even if the launch friring is adopting had fallen back to the host. The window
is bounded — any relaunch re-derives the truth and re-reports it — and closing
it needs a persisted column, which is a schema migration rather than a
rendering fix.

**Firewall prompts** — with the firewall (P2 — see
[Delivery phases](#delivery-phases)), a denial for an unlisted domain will raise
a notification and a confirm modal naming the domain and the command that wanted
it; the answer is persisted to the profile.

UI polish is explicitly a later pass. The first implementation aims for correct,
complete and consistent with existing screens.

## Failure modes

- **Probe failures** are actionable text, not a missing option: the AppArmor
  fix for user namespaces, "Docker is not running", "requires macOS 26",
  "WSL1 is not supported".
- **Dead instances** surface as dead panes with the error visible, because
  sessions keep `remain-on-exit`.
- **Unavailable capabilities** are shown as unavailable — memory limits on a
  policy backend, per-sandbox limits on WSL — rather than accepted and ignored.
- **Escape hatch.** Every profile carries an explicit
  `allow_unsandboxed_fallback` switch. Escape hatches exist in every comparable
  product; the design makes this one visible and per-profile instead of
  ambient. When it fires the session keeps its profile, the indicator switches
  from `⛨` to `⚠`, and the reason reaches the user as an error toast (TUI) or
  on the command's own output (`friring-cli`) — not only the log.
- **An unreadable profile** is refused at launch, named column by column, and
  repaired by re-saving it in the editor. Listing it permissively and
  launching it permissively are different decisions: the first keeps it
  fixable, the second would run a policy nobody wrote.
- **A boundary friring will not grant** fails the launch with the reason and
  the fix, whatever the profile asked for: read-write roots reaching the
  database or a tmux socket directory, denies under a network mode that cannot
  enforce them, a path that cannot be spelled exactly. Refusing is the whole
  point — every one of these has a silent alternative that grants more than the
  profile says.

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
  token rejection, SOCKS and CONNECT parity, method restriction.
- Backend probes and argv/profile generation are unit-testable without running
  the backend; where a backend is present in CI, integration tests run behind a
  capability check and are skipped with a reason otherwise.
- Documentation examples use placeholder paths, never the author's own.

## Delivery phases

Each phase is independently useful and lands with its own tests, docs and
`FORK.md` entry.

**P1 — Policy sandboxes.** The `sandbox` module, profile storage and migration,
the profile list and editor, the session-creation step, the session indicator,
`seatbelt` and `bwrap` backends, network `none` and `full` only,
`host-passthrough` credentials. Local sessions only, and no status reporting
from inside a boundary until P3's file channel — see
[Launch integration](#launch-integration) and [Status signals](#status-signals).

**P2 — The firewall.** The Rust filtering proxy, `allowlist` network mode,
first-use domain prompts and their persistence, wiring into both policy
backends.

**P3 — Place sandboxes.** The `docker`/`podman` backend, the sandbox transport,
instance lifecycle and garbage collection, identical-path mounts, the default
image, `env-token` and `volume-login` credentials, config projection and its
lint pass, and the signal-file channel.

**P4 — Breadth.** `apple-container` and `wsl-distro` backends, copy-on-write
workspaces, resource limits, the sandbox manager view, `friring-cli sandbox`
subcommands, and profile export/import.

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
differ per shape and the UI says which is in effect.

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
reaches it through a Friring-run relay inside the namespace. Allow decisions
trust the client-supplied hostname until TLS termination is added, which the UI
discloses.

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
data.

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
created after launch is replayed by the host on next open — and a profile whose
read-write roots enclose the data directory is refused outright. The one
residual is a hardlink alias planted on the host before the launch; see
[Goals and non-goals](#goals-and-non-goals).
