# codex-park

Park and resume a bridge child that is a **real interactive Codex CLI**, with no
credential and no account, against a local model stub.

Run it with `just codex-park-e2e`. It is a development harness, not something to
install against your own machine: its profile and its stub are fixtures.

## What it is for

`bridge-conformance` proves the bridge with `/bin/sh` agents, which is what makes
a green run a statement about friring rather than about an integration — and it
proves the parking lifecycle that way too. What it cannot say anything about is
the thing parking exists for: an agent with a **conversation** stopping and
coming back to it. A shell script has no thread to preserve.

So this one keeps friring's side identical — the leader is still a shell script
driving the ordinary verbs — and makes the *child* the vendor binary:

1. create an interactive Codex child;
2. its first turn writes a marker into its own private `CODEX_HOME` and reports
   it — the turn happens because friring nudged its pane about the task mail, so
   nudge delivery into a live vendor pane is observed here too;
3. a clean owner `stop`, and the fan-out slot comes back;
4. the fan-out is filled, so a `resume` must be refused `fanout_exhausted`, and
   the parked child is checked to be untouched by the refusal;
5. the slot is freed and the **same** child resumes — the same worktree, the
   same private state directory, and through Codex's own `resume` group, the
   same conversation;
6. it is mailed new work, which the relaunched process claims and answers by
   quoting the marker its previous life wrote.

The harness additionally reads the child's rollout files from outside the
boundary, so "the same thread" is checked against Codex's own state and not only
against friring's bookkeeping. Running it is what found that a bridge `resume`
had been minting a new conversation every time; friring now resolves the child's
own recorded conversation and emits the agent's resume group, and a resume that
cannot reach it is refused rather than launched blank. See `docs/SANDBOX.md`.

## No credential, no billing

The child's `config.toml` names a `stub` provider on loopback with **no
`env_key`**, which is the shape Codex accepts without a login and sends no
authorization header for. The stub answers from a fixture file. Nothing here
reaches a network, holds a key, or costs anything — and nothing reads the
developer's own `~/.codex`, because every root is redirected into the run's
throwaway sandbox.

## What it does not prove

`network_mode = "full"`, because the child must reach a loopback port that
friring's egress proxy has no way to name. The boundary's egress is proven by
`bridge-conformance` (which runs the whole bridge with `network_mode = "none"`)
and by `just seatbelt-probe` against a real kernel. This harness is about the
lifecycle.
