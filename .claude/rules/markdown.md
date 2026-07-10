---
paths:
  - "**/*.md"
---

# Markdown / docs

- Keep markdown **rumdl-clean** (`.rumdl.toml`): `rumdl check .` / `rumdl fmt .`.
  Wrap prose to the configured width (tables and code fences are exempt).
- **Keep `AGENTS.md` lean.** It is loaded every turn — put subsystem detail in
  the matching `docs/` file, not the always-loaded brief. The `AGENTS.md`
  "Where to read" table maps topic → doc.
- **Keep docs current.** If a change alters a documented decision, update the
  relevant `docs/` file in the *same* change — and record any **divergence from
  upstream** in `FORK.md`.
