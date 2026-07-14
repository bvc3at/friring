# Releasing & Installation

> **On this fork:** Friring cuts its **own** GitHub Releases — `cd.yml` builds
> cross-platform `friring-*` binaries + a checksums file on every releasable push
> to `main`. What still routes through upstream: the package-manager channels
> described below (the AUR / Homebrew / Chocolatey / winget publish jobs are
> guarded to `Thurbeen/thurbox`) and the `scripts/install.*` one-liners (they
> fetch upstream `thurbox-*`; grab the fork's binaries from its Releases page
> directly). See [`FORK.md`](../FORK.md).

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

### Release artifacts

Each release includes binaries for 4 platforms plus a checksums file and a
categorized changelog:

- `friring-v{ver}-x86_64-unknown-linux-gnu.tar.gz`
- `friring-v{ver}-x86_64-unknown-linux-musl.tar.gz`
- `friring-v{ver}-aarch64-apple-darwin.tar.gz`
- `friring-v{ver}-x86_64-pc-windows-msvc.zip`
- `friring-v{ver}-checksums.txt` (SHA256 sums for verification)

The upstream package channels below (Chocolatey / winget / Homebrew / AUR) and
the `install.*` scripts still consume upstream's `thurbox-*` assets — those jobs
are guarded to `Thurbeen/thurbox` and don't run here.

### Distribution packages

After the GitHub Release is published, `cd.yml` also updates the downstream
package channels (each gated on its secret, skipped on forks). See
`packaging/README.md` for the full overview.

- **Homebrew** (`publish-homebrew`) — bumps `version`/`sha256` in
  `packaging/homebrew/Formula/thurbox.rb` (via `bump-formula.py`, reading the
  release `checksums.txt`) and pushes to the `Thurbeen/homebrew-thurbox` tap over
  SSH. Needs `HOMEBREW_TAP_DEPLOY_KEY` (a write deploy key; the org blocks
  cross-repo PATs). Install: `brew install thurbeen/thurbox/thurbox`. macOS arm64
  and Linux x86_64 (musl).
- **AUR** (`publish-aur`) — bumps + pushes `thurbox`/`thurbox-bin` PKGBUILDs.
  Needs `AUR_SSH_PRIVATE_KEY`.
- **Chocolatey** (`publish-chocolatey`) — bumps `<version>` in
  `packaging/chocolatey/thurbox.nuspec` and `$url64`/`$checksum64` in
  `tools/chocolateyinstall.ps1` (via `bump-nuspec.py`), then `choco pack` +
  `choco push`. Runs on `windows-latest`; needs `CHOCOLATEY_API_KEY`. New
  versions go through community-repo moderation. Install: `choco install
  thurbox`. Windows x86_64 only.
- **winget** (`publish-winget`) — bumps `PackageVersion`/`InstallerUrl`/
  `InstallerSha256`/`ReleaseNotesUrl` across the three manifests under
  `packaging/winget/manifests/` (via `bump-manifests.py`), then `wingetcreate
  submit`s the set as a PR to `microsoft/winget-pkgs`. Runs on `windows-latest`;
  needs `WINGET_TOKEN` (a `public_repo` PAT owning a fork of `winget-pkgs`). The
  zip is a `portable` `NestedInstallerType` installer (PATH aliases
  `thurbox`/`thurbox-cli`, no MSI). Install: `winget install Thurbeen.thurbox`.
  Windows x86_64 only.

## Installation script

**Linux / macOS** — `scripts/install.sh`:

```bash
curl -fsSL https://raw.githubusercontent.com/Thurbeen/thurbox/main/scripts/install.sh | sh
```

**Windows** — `scripts/install.ps1` (PowerShell):

```powershell
irm https://raw.githubusercontent.com/Thurbeen/thurbox/main/scripts/install.ps1 | iex
```

Both installers share the same shape: ASCII banner, platform detection, version
resolution (GitHub API → releases-page scrape fallback), SHA256 checksum
verification, extract, post-install hints. They download from the same release:
`install.sh` pulls the `.tar.gz` for `x86_64-unknown-linux-musl` /
`aarch64-apple-darwin` (Linux x86_64 + Apple-silicon macOS — the only platforms
it installs onto; it errors cleanly on any other); `install.ps1` pulls the
**`thurbox-<ver>-x86_64-pc-windows-msvc.zip`** (built by `cd.yml`) and extracts
it with the built-in `Expand-Archive` (no tar needed). ARM64 Windows installs the
x86_64 build (runs under x64 emulation).

**`install.sh` (POSIX `sh`) specifics:**

- Colorized output (auto-disabled when stderr is not a TTY, `NO_COLOR` is set, or
  `TERM=dumb`); platforms Linux/macOS × x86_64/aarch64.
- No external deps beyond standard tools (curl/wget, tar, sha256sum/shasum).
- Env vars: `VERSION=v0.1.0`, `INSTALL_DIR=/path` (default `~/.local/bin`).
- Non-interactive (safe pipe-to-shell), cleanup via `trap`.
- Tested by `scripts/install.bats` (bats-core, ~28 tests; CI `install-script`
  job).

**`install.ps1` (PowerShell 5.1+) specifics:**

- Parameters `-Version` / `-InstallDir` / `-Repo`, or the matching
  `THURBOX_VERSION` / `THURBOX_INSTALL_DIR` / `THURBOX_REPO` env vars (env vars
  are the reliable path for the `irm | iex` form, which can't pass parameters);
  default install dir `%LOCALAPPDATA%\Programs\thurbox`.
- Adds the install dir to the **user** `PATH`
  (`[Environment]::SetEnvironmentVariable(... 'User')`) when missing; reflects it
  into the current session.
- ASCII-only source (no BOM needed; survives `irm | iex` decoding on Windows
  PowerShell 5.1); `Write-Host` for UI is intentional (`Write-Output` would leak
  into the `iex` pipeline).
- Pure helpers (`Get-Target`, `Get-ExpectedChecksum`) are guarded by
  `$env:THURBOX_PS_TEST` so the file can be dot-sourced for testing without
  running the installer.
- Tested by `scripts/install.Tests.ps1` (Pester 5; CI `install-script-ps` job,
  run with `pwsh` on ubuntu since the helpers are platform-independent) — the
  PowerShell mirror of `install.bats`.
