---
paths:
  - "**/*.sh"
  - "scripts/**"
---

# Shell scripts

- Keep scripts **shellcheck-clean** (`.shellcheckrc`); `just lint` runs it.
- `scripts/install.sh` is **POSIX `sh`** (pipe-to-shell) — no bashisms. The
  Windows installer `scripts/install.ps1` is PowerShell 5.1+, ASCII-only.
- The dev/e2e harnesses and their shared libraries are documented in
  `docs/DEVELOPMENT.md`.
