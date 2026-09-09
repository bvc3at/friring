# bridge-conformance

Proves friring's orchestration bridge end to end **with no vendor agent
involved**.

Both agents are `/bin/sh` scripts. They have no login, no model, no network and
no dependencies, so what a green run demonstrates is the bridge itself: the
verbs, the authority rules, the child boundary, and the quiesce protocol that
decides whether a branch is integrated.

Use it to answer three questions:

- **Does the bridge work on this host?** Before blaming an agent integration.
- **What does a profile have to grant?** `profiles.toml` is the minimum.
- **Does a change to the bridge still hold its rules?** The worker asserts the
  ones that must not bend, and fails the run when one does.

## Run it

```sh
friring-cli extension install bridge-conformance
friring-cli sandbox import ~/.config/friring/extensions/bridge-conformance/profiles.toml
# Edit the profile's one path (Alt+S), then:
friring-cli extension activate bridge-conformance
```

Create the leader **from the running TUI** — `Ctrl+N` → the repository → a
worktree branch → agent `conformance-leader` → profile `bridge-conformance`.
Headless creation of a bridge-required agent is refused, deliberately: a
one-shot process would exit before owning the session's egress proxy or
answering its bridge requests.

The leader runs on its own and prints what it found.

## What a green run proves

**The verbs answer.** `status`, `create`, `inbox`, `send` and `report` from a
session inside a boundary, through a directory friring minted for it.

**Authority is the row and the directory.** The leader creates a child; the
child asks about itself and gets its own state, not its owner's.

**The depth rule holds structurally.** The worker *tries* to create a child and
must be refused. A child's effective grant is `{mailbox, report}` intersected
with its owner's — never `child-lifecycle`, whatever the profile says — so a run
where that call succeeded is a failing run, and the worker says so.

**The child boundary holds.** The worker checks it cannot read the family state
directory its own private one was seeded from. Its transcripts, and every
sibling's, are its own.

**The quiesce protocol decides.** The worker sends one typed `result`. friring
acknowledges it, stops the exact pane, reads the worktree itself and records a
verdict. The leader reads that verdict out of `status` — not the worker's
summary, which is not evidence of anything.

## What it does not prove

That any particular agent works under a boundary. That is what an agent-specific
extension is for; this is the floor beneath one.
