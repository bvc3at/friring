# Packaging

Optional OS-level integration and distribution packages for Friring.

## Distribution packages

| Directory   | Channel                    | Installs                                  |
| ----------- | -------------------------- | ----------------------------------------- |
| `homebrew/` | Homebrew tap (macOS/Linux) | prebuilt release binaries — see [`homebrew/README.md`](homebrew/README.md) |

Homebrew is the only package channel Friring publishes: this repo doubles as
its own tap, so the formula itself lives at
[`HomebrewFormula/friring.rb`](../HomebrewFormula/friring.rb) (Homebrew only
looks at a tap's root, `Formula/` or `HomebrewFormula/`) and `homebrew/` keeps
the bump tooling and its notes. The `publish-homebrew` job in
[`.github/workflows/cd.yml`](../.github/workflows/cd.yml) bumps it on every
release.

Everywhere else, install with the one-liner installers
([`scripts/install.sh`](../scripts/install.sh),
[`scripts/install.ps1`](../scripts/install.ps1)) or build from source — see the
README's [Installation](../README.md#installation) section.

## Reboot-proof automations (`systemd/`, `launchd/`)

Friring automations fire from two places out of the box:

1. **The TUI tick loop** — while the TUI is open.
2. **A tmux heartbeat keeper window** (`automation-heartbeat`) — armed
   automatically on TUI startup and on `friring-cli automation create`. It runs
   `friring-cli automation tick` every 60 s and keeps the tmux server alive, so
   automations fire even after you close the TUI.

The keeper covers the common case but is **not reboot-proof** (a reboot or
`tmux kill-server` ends it until friring runs again). For guaranteed,
session-independent firing, enable one of these units, which run the same
`friring-cli automation tick` on a timer. Firing is **claim-based** (atomic
compare-and-swap on `next_run_at`), so running the TUI, the tmux keeper, and one
of these timers simultaneously never double-fires an automation.

- **Linux**: `systemd/friring-automations.{service,timer}` — see the header
  comment in the `.service` file for install steps.
- **macOS**: `launchd/dev.friring.automations.plist` — see the comment block in
  the plist for install steps.

Both default to a 1-minute cadence; adjust the timer/`StartInterval` to taste.
