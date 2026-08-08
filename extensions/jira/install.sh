#!/usr/bin/env sh
# Thin wrapper kept for the curl|sh one-liner. The real installer lives in
# friring itself:
#
#   friring-cli extension install jira
#
# This script just forwards to it — using a local checkout when run from one,
# otherwise the official remote source. It fetches the manifest + payload, lays
# down ~/.config/friring/extensions/jira, and activates the jira-tick automation — a deterministic exec
# sync (no agent, no session), which friring then self-heals.
#
# Usage:
#   ./install.sh                  # from a checkout
#   curl -fsSL https://raw.githubusercontent.com/bvc3at/friring/main/extensions/jira/install.sh | sh
#
# Environment variables:
#   JIRA_HOME=<dir>   override install home (default: <config>/extensions/jira)
#
# Authenticate afterwards by creating ~/.config/friring/extensions/jira/credentials.env with JIRA_BASE_URL,
# JIRA_EMAIL, and JIRA_API_TOKEN (an Atlassian API token). Then add
# projects/filters to ~/.config/friring/extensions/jira/trackers.md. To turn it off:
#   friring-cli extension deactivate jira [--force --purge]

set -eu

command -v friring-cli >/dev/null 2>&1 || {
  echo "error: friring-cli not found in PATH (install friring first)" >&2
  exit 1
}

# Pass --home when JIRA_HOME is set.
set --
[ -n "${JIRA_HOME:-}" ] && set -- --home "$JIRA_HOME"

# Prefer the checkout this script sits in (has extension.toml next to it);
# otherwise install the official "jira" extension from the remote source.
SRC_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" 2>/dev/null && pwd || true)"
if [ -n "$SRC_DIR" ] && [ -f "$SRC_DIR/extension.toml" ]; then
  exec friring-cli extension install "$SRC_DIR" "$@"
else
  exec friring-cli extension install jira "$@"
fi
