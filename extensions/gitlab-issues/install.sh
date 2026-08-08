#!/usr/bin/env sh
# Thin wrapper kept for the curl|sh one-liner. The real installer lives in
# friring itself:
#
#   friring-cli extension install gitlab-issues
#
# This script just forwards to it — using a local checkout when run from one,
# otherwise the official remote source. It fetches the manifest + payload, lays
# down ~/.config/friring/extensions/gitlab-issues, and activates the gitlab-issues-tick automation — a
# deterministic exec sync (no agent, no session), which friring then self-heals.
#
# Usage:
#   ./install.sh                  # from a checkout
#   curl -fsSL https://raw.githubusercontent.com/bvc3at/friring/main/extensions/gitlab-issues/install.sh | sh
#
# Environment variables:
#   GITLAB_ISSUES_HOME=<dir>   override install home (default: <config>/extensions/gitlab-issues)
#
# Authenticate afterwards: `glab auth login`. Then add projects to
# ~/.config/friring/extensions/gitlab-issues/trackers.md. To turn it off:
#   friring-cli extension deactivate gitlab-issues [--force --purge]

set -eu

command -v friring-cli >/dev/null 2>&1 || {
  echo "error: friring-cli not found in PATH (install friring first)" >&2
  exit 1
}

# Pass --home when GITLAB_ISSUES_HOME is set.
set --
[ -n "${GITLAB_ISSUES_HOME:-}" ] && set -- --home "$GITLAB_ISSUES_HOME"

# Prefer the checkout this script sits in (has extension.toml next to it);
# otherwise install the official "gitlab-issues" extension from the remote source.
SRC_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" 2>/dev/null && pwd || true)"
if [ -n "$SRC_DIR" ] && [ -f "$SRC_DIR/extension.toml" ]; then
  exec friring-cli extension install "$SRC_DIR" "$@"
else
  exec friring-cli extension install gitlab-issues "$@"
fi
