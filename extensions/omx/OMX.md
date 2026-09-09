# How this extension is bound to oh-my-codex 0.21.0

Everything here is read from one release. This file records **which facts**, so
that when a later oh-my-codex lands somebody can tell what has to be re-checked
rather than guessing.

Pinned commit: `3ad79a8a6fe6e95fdbb8c00e40716fffe4011ce2` (`v0.21.0`).

## The argv contract

`omx`'s `resolveCliInvocation` (`src/cli/index.ts`) reads its **first** argument:

- `--help` / `-h` → help; `--version` / `-v` → version.
- anything else starting with `--` → a **launch**, with every argument passed on.
- `launch`, `exec`, `resume` → that command, with the rest as its arguments.
- anything else → the subcommand it names, with **no** arguments.

friring's `AgentDef::build_args` emits the session-selection group *before* the
static `args`, so the argv it hands the command is:

| launch | argv |
|---|---|
| fresh | `["--direct"]` |
| restart | `["resume", "--last", "--direct"]` |

Both are already in the order `omx` accepts, which is why `leaderArgv` passes
them through rather than reordering. What it refuses is a first argument that is
neither a flag nor `resume`: `omx` would read that as a *different subcommand*.

The last line matters if OMX ever changes that dispatch — the refusal is what
turns a silent wrong-command launch into a stop.

## Why the command is a wrapper and not `node`

`bin/omx-leader` is one line:

```sh
exec node "${0%/*}/../lib/friring-omx.mjs" leader "$@"
```

`${0%/*}` rather than `$(dirname "$0")` on purpose: a parameter expansion runs
no program, so the wrapper needs nothing on `PATH` inside the boundary but
`node` itself. It relies on `$0` containing a separator, which it always does —
friring resolves `{home}` at install and launches the command by absolute path.

The wrapper exists so the program always receives its subcommand first and
friring's argv after it, whatever friring emits. It `exec`s and Node cannot, so a leader pane's
process tree is `node → omx → codex`. Nothing in friring keys on a pane's
command being the agent binary: readiness and hook state key on the signal file,
and the pane guard on screen content.

Those two lines are the only shell in the extension.

## Why native Team is unavailable

`src/team/runtime.ts` throws `Team mode requires running inside tmux current
leader pane` when `TMUX` is unset. friring strips `TMUX` from every sandboxed
launch (`MUX_NESTING_ENV`) *and* denies the host's multiplexer sockets, so the
variable is absent and the socket unreachable — the refusal is the boundary
working, not a bug to route around.

`$friring-team` replaces that one lane. Everything else in OMX is untouched.

## The routing overlay

`OMX_RALPH_APPEND_INSTRUCTIONS_FILE` points at `leader/APPENDIX.md`, which
`readLaunchAppendInstructions` (`src/cli/index.ts`) appends to the session
instructions. **A missing file throws**, so the overlay fails closed: a leader
either has it or does not start.

`OMX_HOOK_PLUGINS` is deliberately **not** set. In
`src/hooks/extensibility/dispatcher.ts`, native and derived events dispatch
plugins regardless of that variable, and nothing this extension relies on
depends on plugin dispatch — so setting it would change nothing and imply
something.

## The Team DAG handoff

`plan` reproduces the `TeamDagHandoff` v1 shape from `src/team/dag-schema.ts`:
`schema_version` must be 1, `nodes` non-empty, each node needing `id`, `subject`
and `description`, with optional `role`, `lane`, `filePaths`, `domains`,
`depends_on`, `requires_code_change` and `acceptance`; ids unique, dependencies
resolvable, the graph acyclic.

Reproduced rather than imported because this program has no dependencies and OMX
is TypeScript that would need a build. `lib/friring-omx.test.mjs` pins the shape,
so a divergence is a test failure rather than a runtime surprise.

Resolution order matches OMX's: the `team-dag-<slug>.json` sidecar in
`.omx/plans/` first, then the fenced block after a `Team DAG Handoff` heading in
the latest approved PRD. A PRD with no matching test spec is treated as
incomplete planning, exactly as OMX treats it.

## What `depends_on` means here, and what it does not

**It orders the fan-out. It does not compose the code.** A node whose
dependencies are all `done` is created next, and its worktree is cut from the
repository's **default branch** — not from the head its dependency produced. So
a dependent node cannot see its dependency's work while it runs; it sees the
base, plus whatever its own task body tells it.

That is deliberate, and it is the model the rest of this path is built on:
workers run in parallel in boundaries that deny each other's worktrees, nothing
a worker claims is trusted, and the branches are merged **afterwards** by
`integrate`, in plan order, from what friring itself verified. A dependent
worktree based on a dependency's head would mean a worker merging another
worker's unreviewed output into its own workspace, which is the thing the
verification exists to prevent.

The practical consequence for a plan author: write `depends_on` for *sequencing*
— "do not start the migration until the schema node has finished" — and not for
*visibility*. A node that genuinely needs to read another node's code is one
node, not two.

## The digests

`pins.json` carries the SHA-256 of every skill card and role prompt the routing
depends on, taken from `omx-capabilities.lock.json` at the pinned commit. The
manifest turns each into a `file-digest` requirement, so an install or an
activate against a different oh-my-codex refuses and says which file moved.

`codex --version` is **not** a gate. OMX 0.21.0 reviewed a range of Codex
releases, and pinning one would refuse installs that work. The leader records it
instead.
