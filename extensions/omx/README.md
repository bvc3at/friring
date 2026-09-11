# omx — bridge-backed Team execution for oh-my-codex

Runs [oh-my-codex](https://github.com/Yeachan-Heo/oh-my-codex) 0.21.0 as a
friring **leader**, with every Team node as a sandboxed **bridge child**: its own
session, its own boundary, its own git worktree and branch, and its own private
Codex state directory.

Native `omx team` drives a tmux pane layout from inside the leader's terminal.
A friring session's pane is friring's, and its `TMUX` is deliberately stripped,
so native Team refuses there — correctly. This extension replaces that one lane
with `$friring-team`, and leaves the rest of OMX alone.

## What you get

- **`$friring-autopilot`** — the one-command lane: deep interview → plan → goal
  → Team execution → integration → evidence.
- **`$friring-team`** — the Team engine on its own, for a plan you already have.
- Two agents in `agents.toml`: `omx-leader` and `omx-worker-codex`.
- A least-privilege sandbox profile template.

## What friring guarantees, and what it does not

**Guaranteed.** A worker cannot read the leader's transcripts, another worker's
transcripts, another worker's worktree, another worker's private state, friring's
database, or the host's tmux. A worker's own claim about its work is never what
decides whether its branch is merged — friring stops the pane, reads the worktree
itself, and that verdict is what `integrate` uses. A dirty worktree is never
merged.

**Not guaranteed — and this one is load-bearing.** The workers of one leader are
**one trust domain for git history**. The profile shares the repository's `.git`,
because a linked worktree keeps its object and ref stores there and a worker that
cannot write them cannot commit at all. So any worker can move any branch,
including another worker's, and add or delete objects. friring's verdict says
what a branch pointed at when it looked and whether the worktree was clean; it
does not say **who wrote it**, so a moved ref is verified and merged as that
node's work. If one worker's output must not be forgeable by another, drop
`~/dev/your-project/.git` from `child_shared_rw` — and accept that no worker can
then commit.

**Not guaranteed.** That an agent does good work. That a plan is a good plan.
The boundary is about what a worker can *reach*, not about what it should do.

**`depends_on` orders the fan-out; it does not compose the code.** Every node's
worktree is cut from the repository's default branch, so a dependent node cannot
see its dependency's work while it runs — the branches are merged afterwards, by
`integrate`, from what friring verified. Write `depends_on` for sequencing, not
for visibility; a node that has to read another node's code is one node, not
two. The reasoning is in [`OMX.md`](OMX.md).

**A worker that asks a question stops the run.** `friring-omx run` drains the
leader's mailbox each pass and returns as soon as a child is `blocked`, `dirty`,
`stalled` or `stop_failed`, with the child named and the message included, exiting
non-zero. None of those states changes without a person, and waiting out the
six-hour run timer would be the same as not saying so.

## Install

The steps friring will not do for you, in order. Each is an operator decision.

1. Install and authenticate the Codex CLI, install `oh-my-codex@0.21.0`, and
   have **Node.js 20 or newer** on `PATH`. The extension is pinned to that OMX
   release: its argv contract, its routing and the digest of every skill card it
   depends on were read from it. Install and activate check that `node` is
   there, and the extension's own program refuses an older major by number
   before it launches anything.
2. `omx setup --scope user --install-mode legacy`. Both flags are load-bearing.

   **User scope, not project.** Project scope moves `CODEX_HOME` into the
   repository, which silences friring's own status hooks.

   **Legacy install mode, not plugin.** Legacy mode installs the skill cards at
   `~/.codex/skills/<name>/SKILL.md`, which is where step 4's digest gates look.
   Plugin mode puts them under `~/.codex/plugins/` instead and leaves
   `~/.codex/skills` absent altogether, so the install below refuses with eight
   missing files. `omx` remembers the choice in `.omx/setup-scope.json` beside
   the directory you ran it in, so passing it once is not the same as it being
   the mode you get next time — pass it every time.

   Note that `omx setup` **rewrites** each skill card's frontmatter description
   as it installs it, prefixing `[OMX] `. friring's pins are the digests of the
   *installed* files, not of the ones in the release tarball, because the gate
   is a question about what you have.
3. Back up `~/.local/share/friring/friring.db`. The bridge's schema is additive
   and forward-only.
4. `friring-cli extension install omx`

   This refuses — rather than skipping — if `~/.codex/skills/friring-team` or
   `~/.codex/skills/friring-autopilot` already exist and friring did not write
   them. Move them aside if they are yours.

   It also refuses on a version, digest or hooks-evidence mismatch. Every one of
   those means the routing this extension ships was written against a different
   oh-my-codex than the one you have.
5. `friring-cli sandbox import ~/.config/friring/extensions/omx/profiles.toml`,
   then **edit the profile** (`Alt+S` in the TUI). The template's repository path
   and toolchain roots are placeholders.
6. `friring-cli extension activate omx`
7. **Launch `omx` once outside friring**, in an ordinary terminal, and answer
   whatever it asks. Do this **after** the steps above, not before — step 4 is
   what makes one of the questions appear.

   Both are the kind of question a sandboxed leader cannot get past, because
   friring refuses to type into a pane showing a modal
   (`agent::tmux::MODAL_MARKERS`). Unanswered, the leader sits there, no nudge is
   delivered, and nothing in any log says why:

   - Codex's `Hooks need review`. `omx setup` records a trusted hash for each
     entry it writes into `~/.codex/hooks.json`; installing this extension then
     merges friring's own four status hooks into that same file, so four hashes
     stop matching and the next launch offers them for review. Nothing can
     pre-compute the replacement — the hash is Codex's — so it is answered once,
     here, with "Trust all and continue". Reinstalling the extension or re-running
     `omx setup` puts the question back.
   - `[omx] Enjoying oh-my-codex? Star it on GitHub? [Y/n]`, shown once while
     `gh` is installed and `~/.omx/state/star-prompt.json` is absent. Answer it
     out here for a second reason: yes makes OMX run
     `gh api -X PUT /user/starred/…`, which is a write to GitHub as whoever owns
     your `gh` credential. The profile's allowlist does not carry
     `api.github.com`, so friring's proxy would refuse it — and with
     `prompt_new_domains = true` you would be asked about a new domain in the
     middle of a run.

   If a leader ever exits immediately with `session pointer launch aborted:
   session_pointer_unusable`, that is OMX refusing a pointer left behind by a
   launch that died; `omx session` has the recovery surface.
8. Create the leader **from the running TUI**: `Ctrl+N` → repository → worktree
   branch → agent `omx-leader` → sandbox profile `omx`.

   Headless creation is refused for a bridge-required agent, and the reason is
   not arbitrary: `friring-cli session create` exits after spawning, so nothing
   would own the session's egress proxy or answer its bridge requests.
9. In the leader's pane: `$friring-autopilot <your brief>`

## Extending the profile

Two things you will almost certainly have to add, and one rule that governs
both.

**The rule:** a child's boundary is its owner's, narrowed. A worker can never
reach a domain or a directory its leader cannot. So everything below goes in the
**leader's** profile.

**Package registries.** The default allowlist carries the three OpenAI endpoints
and nothing else, so a worker that runs `npm install` fails closed. Add what
your project actually fetches:

```text
registry.npmjs.org:443
repo.maven.apache.org:443
plugins.gradle.org:443
pypi.org:443
files.pythonhosted.org:443
crates.io:443
static.crates.io:443
github.com:443
```

friring asks about an unlisted domain the first time rather than failing
silently, so you can also just run a build and answer the prompts.

**Build caches.** A cache is read *and written*, and a worker writes only its
own worktree unless you say otherwise. Add the caches to the leader's read-write
paths **and** to `child_shared_rw`:

```text
~/.gradle
~/.m2
~/.npm
~/.cache/pip
~/.cargo/registry
```

Listing one in `child_shared_rw` that the leader does not have read-write grants
nothing — the narrowing intersects with the parent's set, deliberately, so a
profile edit cannot widen a child past its owner.

**Where you declare a path decides who inherits it.** A path in the profile's
`paths` reaches every worker, read-only, after the narrowing. A path in an
*agent's* `state_rw` reaches only sessions running that agent. That is why OMX's
own roots — `~/.omx` and `~/.omx-runs`, which hold the leader's session identity,
its launch lineage, its logs and its codebase map — are declared on the
`omx-leader` agent and are deliberately **not** in the profile. Moving one into
the profile would hand all of it to every worker. Put anything a worker genuinely
shares in `child_shared_rw`, and anything only the leader touches on the leader.

## What a worker gets out of `~/.codex`

Exactly six things, each in one mode, and only because the profile authorizes
that exact path in that exact mode:

| Entry | Mode | Why |
|---|---|---|
| `auth.json` | `link-rw` | one credential file, refreshed in place — never a second copy of a rotating token |
| `config.toml` | `copy` | the worker's own; edits do not reach the family's |
| `hooks.json` | `copy-rewrite` | paths inside it are repointed at the private directory, or its hooks would fire against the family's |
| `skills` | `symlink` | read-only, and the same cards the leader runs |
| `prompts` | `symlink` | read-only role prompts |
| `AGENTS.md` | `symlink` | read-only, optional |

`sessions`, `history.jsonl` and `log` are **not** on that list, and are denied:
a worker's transcripts live only under its own `CODEX_HOME`.

## Uninstall

```sh
friring-cli extension uninstall omx
```

Removes the two agents, the two skill cards **it wrote**, and its own home. A
skill card you have since edited is yours and is left alone; so is anything else
in `~/.codex/skills`. The sandbox profile is not removed — profiles are the
operator's, and a profile is often shared with other sessions.

## Testing it

```sh
just omx-test          # node --test over extensions/omx/lib
just test              # includes the manifest invocation-order fixture
```
