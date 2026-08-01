# Homebrew formula for friring.
#
# This repo *is* the tap: Homebrew reads formulae from `Formula/`,
# `HomebrewFormula/` or a tap's root, so there is no separate homebrew-friring
# repo to keep in sync.
#
#   brew tap bvc3at/friring https://github.com/bvc3at/friring
#   brew install friring
#
# `version` and both `sha256` values therefore have to name a *published*
# release at all times — a tap user installs whatever `main` currently says.
# The `publish-homebrew` job in .github/workflows/cd.yml bumps them from the
# release checksums (via packaging/homebrew/bump-formula.py) and pushes the
# result back to `main` right after each GitHub Release.
#
# Only the platforms with a published release artifact are supported:
#   - macOS arm64 (Apple Silicon) -> aarch64-apple-darwin
#   - Linux x86_64                -> x86_64-unknown-linux-musl (static)
# Intel macOS and aarch64 Linux have no release binary, so they are omitted
# (brew reports "no available formula" on those platforms).
class Friring < Formula
  desc "TUI for orchestrating multiple coding-agent CLI sessions in persistent tmux panels"
  homepage "https://github.com/bvc3at/friring"
  version "0.13.0"
  license "MIT"

  depends_on "git"
  depends_on "tmux"

  on_macos do
    on_arm do
      url "https://github.com/bvc3at/friring/releases/download/v#{version}/friring-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "7c79d5740b050b29f55319964a7c72e5dca3ef0c4e051fb09a842fcd15027ef0"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/bvc3at/friring/releases/download/v#{version}/friring-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "52d6fc2c4f90c915a57fdabbbc3ac355eef290f2fb64a9a0fb797d35296508a5"
    end
  end

  def install
    # The release tarball ships both maintained binaries plus LICENSE; install
    # only the binaries (Homebrew records the license from the formula).
    bin.install "friring"
    bin.install "friring-cli"
  end

  def caveats
    <<~EOS
      friring needs tmux >= 3.2 and a coding-agent CLI (claude, codex, antigravity,
      opencode, aider, …) on your PATH. Launch the TUI with `friring`; the
      scriptable headless interface is `friring-cli`.
    EOS
  end

  test do
    # The TUI (`friring`) has no headless mode, so only assert it is installed
    # and executable. `friring-cli` is a clap CLI: `--version` exits 0 and
    # prints a semver-shaped marker (the build-time release version is injected
    # into the TUI's status bar, not into clap's CARGO_PKG_VERSION).
    assert_predicate bin/"friring", :executable?
    assert_match(/\d+\.\d+\.\d+/, shell_output("#{bin}/friring-cli --version"))
  end
end
