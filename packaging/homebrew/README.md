# Homebrew packaging

Friring ships a [Homebrew](https://brew.sh) formula that installs the
**prebuilt** release binaries (`friring` + `friring-cli`) from the GitHub
Release. **This repo is its own tap** — Homebrew reads formulae from a tap's
`HomebrewFormula/` directory — so there is no separate `homebrew-friring`
repository to keep in sync:

```bash
brew tap bvc3at/friring https://github.com/bvc3at/friring
brew install bvc3at/friring/friring   # `brew install friring` also resolves
```

The explicit URL is required because the repo is named `friring`: `brew tap`
only infers `https://github.com/<user>/homebrew-<name>` from the short form.

The formula is [`HomebrewFormula/friring.rb`](../../HomebrewFormula/friring.rb)
at the repo root — Homebrew discovers a tap's formulae only in `Formula/`,
`HomebrewFormula/` or the tap root, which is why it does not live here under
`packaging/`. Its `version` and `sha256` values always name a **published**
release (a tap user installs whatever `main` currently says); CI rewrites them
on every release.

## Supported platforms

The formula only declares the platforms that have a published release
artifact:

| Platform | Release artifact |
| -------- | ---------------- |
| macOS arm64 (Apple Silicon) | `aarch64-apple-darwin` |
| Linux x86_64 | `x86_64-unknown-linux-musl` (static) |

Intel macOS (`x86_64-apple-darwin`) and aarch64 Linux have **no** release
binary, so `brew install` fails there (*"formula requires at least a URL"*).
Build from source on those platforms — see the README's
[Installation](../../README.md#installation) section.

## Runtime dependencies

- `tmux` (>= 3.2) and `git` — declared as formula `depends_on`.
- A coding-agent CLI (claude-code, codex, antigravity, opencode, aider, …) is
  user-supplied (mentioned in the formula `caveats`).

## Test locally

Homebrew rejects any formula that is not in a tap (`brew install
./HomebrewFormula/friring.rb` fails with *"Homebrew requires formulae to be in
a tap"*), so testing an unpushed edit means tapping a **local clone** — `brew
tap` clones it, so commit first:

```bash
brew tap bvc3at/friring /path/to/your/friring/checkout   # local git clone
brew install bvc3at/friring/friring
brew test bvc3at/friring/friring                          # formula test block
brew audit --strict --formula bvc3at/friring/friring      # lint the recipe
brew untap bvc3at/friring
```

`brew audit` installs Homebrew's audit gem group on first run.

## Automated publishing (CI)

Every release bumps the formula **automatically**. The `publish-homebrew` job
in [`.github/workflows/cd.yml`](../../.github/workflows/cd.yml) runs after the
GitHub Release is created and:

1. downloads the release `friring-<version>-checksums.txt`,
2. runs [`bump-formula.py`](bump-formula.py) to set `version` and each
   per-platform `sha256` from those checksums, then
3. commits the bumped `HomebrewFormula/friring.rb` back to `main`.

No secrets are involved — the workflow's own `GITHUB_TOKEN` pushes the commit,
because the tap *is* this repo. The bump is a `chore` commit (not releasable to
cocogitto) marked `[skip ci]`, so it cannot trigger another release. The job is
a no-op if the formula is already current.

## Manual bump

To move the formula to a published release by hand:

```bash
curl -fsSL -o /tmp/checksums.txt \
  "https://github.com/bvc3at/friring/releases/download/v<version>/friring-v<version>-checksums.txt"
python3 packaging/homebrew/bump-formula.py v<version> HomebrewFormula/friring.rb /tmp/checksums.txt
```

Pick a `<version>` that has **published release assets** (the formula points at
release tarballs).
