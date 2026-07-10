---
paths:
  - "**/*.rs"
---

# Rust changes

Authoritative rules live in `AGENTS.md` ("Architecture" + "Every change") and
`docs/ARCHITECTURE.md`. Reminders at the point of editing:

- **Module boundaries are enforced.** `tests/architecture_rules.rs` is an
  allowlist — a new module fails the test until it declares what it may
  reference (`session` ← nothing; `agent` ← session; `ui` ← session + app;
  `app` ← all). `session_ops`/`cli` may reach `crate::agent::…` only via
  fully-qualified paths, never `use`.
- **Comments: why, not what.** No `TODO`/`FIXME`/`HACK` markers, no
  commented-out code. Don't delete a doc comment's intra-doc link or example
  without re-running `cargo doc`.
- **Before you finish a Rust change**, run `just lint` (fmt + clippy `-D
  warnings` + deny + rumdl) and `just test` (nextest). rustfmt is 100-col.
