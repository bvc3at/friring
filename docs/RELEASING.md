# Releasing & Installation

> **On this fork:** Friring cuts its **own** GitHub Releases *and* ships its own
> install surface — `cd.yml` builds cross-platform `friring-*` binaries + a
> checksums file on every releasable push to `main`, then bumps the in-repo
> Homebrew formula. Upstream's other package channels (AUR / Chocolatey /
> winget) carry upstream's identity and are **not** republished here; they were
> dropped from this repo. See [`FORK.md`](../FORK.md).

## Release process

Releases are **fully automated** via GitHub Actions. No version commits are
created — version is determined by git tags only.

### How it works

Every push to `main` automatically triggers the release workflow:

1. **Commit analysis** — analyzes all commits since the last tag using cocogitto.
2. **Release decision**:
   - **If** commits include `feat`, `fix`, or `perf` → create a release.
   - **If** only docs/chore/ci commits → no release (workflow exits).
3. **Automated release** (if needed):
   - Determine the semantic version (feat → minor, fix/perf → patch).
   - Create a lightweight git tag `v{version}` (e.g. `v0.1.0`) and push it.
   - Build binaries for 4 platforms (3 Unix `.tar.gz` + 1 Windows `.zip`;
     version passed via environment variable).
   - Generate a changelog from commits.
   - Publish a GitHub Release with binaries and release notes.

### Commit types and versioning

- **feat** → minor bump (`0.x.0`)
- **fix, perf** → patch bump (`0.0.x`)
- **docs, chore, ci, style, test** → no release (appear in the next version)
- **BREAKING CHANGE** → major bump (`x.0.0`) — use cautiously for 0.x

### Version management

- **Cargo.toml version** is always `0.0.0-dev` (a static development marker).
- The **real version** is determined by the release workflow (`v0.1.0`, …).
- **Build-time injection**: `build.rs` reads the `FRIRING_RELEASE_VERSION`
  environment variable (set by the workflow) to inject the version into the
  binary. Development builds show `0.0.0-dev` when it is unset; release builds
  show the actual version.

**That version string, not the cargo profile, decides the build's flavour.**
`build.rs` sets the `dev_build` cfg from a version *containing* `-dev`, and
`paths::app_dir_name()`, the tmux socket and the tmux group session all follow
it. So `cargo build --release` in a working tree is still a **dev-flavoured**
binary — `~/.config/friring-dev`, `~/.local/share/friring-dev/friring.db`,
socket `friring-dev` — and so is one built with `FRIRING_RELEASE_VERSION` set
to a value that itself contains `-dev`. Only a **non-dev**
`FRIRING_RELEASE_VERSION` produces the release flavour: `~/.config/friring`,
`~/.local/share/friring/friring.db`, socket `friring`.

Those are different databases and different tmux servers, which is what makes
installing the wrong flavour over a running instance misleading rather than
merely wrong: it does not upgrade that installation, it starts a second, empty
one, and the sessions appear to be gone while they are still running on the
other socket. Anything that replaces a running binary has to preserve the
routing the live process was started with — its launcher, and the
`FRIRING_CONFIG_DIR`, `FRIRING_DATA_DIR`, `FRIRING_SOCKET`,
`FRIRING_TMUX_SESSION` and `TMUX_TMPDIR` in its environment, each of which
overrides the compiled-in flavour on its own — rather than assume the default
for the path it is installed to.

### Release artifacts

Each release includes binaries for 4 platforms plus a checksums file and a
categorized changelog:

- `friring-v{ver}-x86_64-unknown-linux-gnu.tar.gz`
- `friring-v{ver}-x86_64-unknown-linux-musl.tar.gz`
- `friring-v{ver}-aarch64-apple-darwin.tar.gz`
- `friring-v{ver}-x86_64-pc-windows-msvc.zip`
- `friring-v{ver}-checksums.txt` (SHA256 sums for verification)

The installers, the Homebrew formula, and `friring-cli update` all consume
exactly these assets.

### Distribution packages

Homebrew is the only package channel Friring publishes, and this repo is its own
tap — Homebrew reads formulae from a tap's `HomebrewFormula/` directory, so
there is no second repository to keep in sync. See `packaging/homebrew/README.md`.

- **Homebrew** (`publish-homebrew`) — runs after the Release, bumps
  `version`/`sha256` in `HomebrewFormula/friring.rb` (via
  `packaging/homebrew/bump-formula.py`, reading the release `checksums.txt`) and
  commits the result back to `main`. No secrets: the workflow's own
  `GITHUB_TOKEN` pushes it. The bump is a `chore` commit marked `[skip ci]`, so
  it is neither releasable to cocogitto nor able to trigger another run.
  Install: `brew tap bvc3at/friring https://github.com/bvc3at/friring && brew
  install bvc3at/friring/friring`. macOS arm64 and Linux x86_64 (musl).

## Installation script

**Linux / macOS** — `scripts/install.sh`:

```bash
curl -fsSL https://raw.githubusercontent.com/bvc3at/friring/main/scripts/install.sh | sh
```

**Windows** — `scripts/install.ps1` (PowerShell):

```powershell
irm https://raw.githubusercontent.com/bvc3at/friring/main/scripts/install.ps1 | iex
```

Both installers share the same shape: ASCII banner, platform detection, version
resolution (GitHub API → releases-page scrape fallback), SHA256 checksum
verification, extract, post-install hints. They download from the same release:
`install.sh` pulls the `.tar.gz` for `x86_64-unknown-linux-musl` /
`aarch64-apple-darwin` (Linux x86_64 + Apple-silicon macOS — the only platforms
it installs onto; it errors cleanly on any other); `install.ps1` pulls the
**`friring-<ver>-x86_64-pc-windows-msvc.zip`** (built by `cd.yml`) and extracts
it with the built-in `Expand-Archive` (no tar needed). ARM64 Windows installs the
x86_64 build (runs under x64 emulation).

**`install.sh` (POSIX `sh`) specifics:**

- Colorized output (auto-disabled when stderr is not a TTY, `NO_COLOR` is set, or
  `TERM=dumb`); platforms Linux x86_64 and macOS arm64 (anything else errors
  cleanly — use `cargo install` or a source build).
- No external deps beyond standard tools (curl/wget, tar, sha256sum/shasum).
- Env vars: `VERSION=v0.1.0`, `INSTALL_DIR=/path` (default `~/.local/bin`).
- Non-interactive (safe pipe-to-shell), cleanup via `trap`.
- Tested by `scripts/install.bats` (bats-core, ~28 tests; CI `install-script`
  job).

**`install.ps1` (PowerShell 5.1+) specifics:**

- Parameters `-Version` / `-InstallDir` / `-Repo`, or the matching
  `FRIRING_VERSION` / `FRIRING_INSTALL_DIR` / `FRIRING_REPO` env vars (env vars
  are the reliable path for the `irm | iex` form, which can't pass parameters);
  default install dir `%LOCALAPPDATA%\Programs\friring`.
- Adds the install dir to the **user** `PATH`
  (`[Environment]::SetEnvironmentVariable(... 'User')`) when missing; reflects it
  into the current session.
- ASCII-only source (no BOM needed; survives `irm | iex` decoding on Windows
  PowerShell 5.1); `Write-Host` for UI is intentional (`Write-Output` would leak
  into the `iex` pipeline).
- Pure helpers (`Get-Target`, `Get-ExpectedChecksum`) are guarded by
  `$env:FRIRING_PS_TEST` so the file can be dot-sourced for testing without
  running the installer.
- Tested by `scripts/install.Tests.ps1` (Pester 5; CI `install-script-ps` job,
  run with `pwsh` on ubuntu since the helpers are platform-independent) — the
  PowerShell mirror of `install.bats`.
