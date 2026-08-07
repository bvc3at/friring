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
- **No dates in documentation** without a specific reason. "When" is what `git
  log`/`git blame` are for, and a stamped heading or line rots: the next person
  edits the text and leaves the date, so it ends up describing something that
  is no longer true. This covers `FORK.md` section headings, `docs/` prose and
  code comments alike. Write a date only when it is part of the fact itself —
  a deprecation deadline, a migration cutover, a dated external reference.
  Several older `FORK.md` headings still carry `(Month Year)`; leave them be
  unless you are already editing that section, and don't add new ones.
