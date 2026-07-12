#!/usr/bin/env sh
# Thin wrapper kept for the curl|sh one-liner. The real installer lives in
# friring itself:
#
#   friring-cli extension install github-issues
#
# This script just forwards to it — using a local checkout when run from one,
# otherwise the official remote source. It fetches the manifest + payload, lays
# down ~/.config/friring/extensions/github-issues, and activates the github-issues-tick automation — a
# deterministic exec sync (no agent, no session), which friring then self-heals.
#
# Usage:
#   ./install.sh                  # from a checkout
#   curl -fsSL https://raw.githubusercontent.com/Thurbeen/thurbox/main/extensions/github-issues/install.sh | sh
#
# Environment variables:
#   GITHUB_ISSUES_HOME=<dir>   override install home (default: <config>/extensions/github-issues)
#
# Authenticate afterwards: `gh auth login`. Then add repos to
# ~/.config/friring/extensions/github-issues/trackers.md. To turn it off:
#   friring-cli extension deactivate github-issues [--force --purge]

set -eu

command -v friring-cli >/dev/null 2>&1 || {
  echo "error: friring-cli not found in PATH (install friring first)" >&2
  exit 1
}

# Pass --home when GITHUB_ISSUES_HOME is set.
set --
[ -n "${GITHUB_ISSUES_HOME:-}" ] && set -- --home "$GITHUB_ISSUES_HOME"

# Prefer the checkout this script sits in (has extension.toml next to it);
# otherwise install the official "github-issues" extension from the remote source.
SRC_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" 2>/dev/null && pwd || true)"
if [ -n "$SRC_DIR" ] && [ -f "$SRC_DIR/extension.toml" ]; then
  exec friring-cli extension install "$SRC_DIR" "$@"
else
  exec friring-cli extension install github-issues "$@"
fi
