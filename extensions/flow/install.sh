#!/usr/bin/env sh
# Thin wrapper kept for the curl|sh one-liner. The real installer now lives in
# friring itself:
#
#   friring-cli extension install flow
#
# This script just forwards to it — using a local checkout when run from one,
# otherwise the official remote source. It fetches the manifest + payload, lays
# down ~/.config/friring/extensions/flow, registers the flow agents in agents.toml, and activates the flow
# session (which friring then self-heals). Flow is event-driven — worker pushes
# over the mailbox queue wake it; there is no scheduled automation.
#
# Usage:
#   ./install.sh                  # from a checkout
#   curl -fsSL https://raw.githubusercontent.com/bvc3at/friring/main/extensions/flow/install.sh | sh
#
# Environment variables:
#   FLOW_HOME=<dir>   override install home (default: <config>/extensions/flow)
#
# To turn flow off:  friring-cli extension deactivate flow [--force --purge]

set -eu

command -v friring-cli >/dev/null 2>&1 || {
  echo "error: friring-cli not found in PATH (install friring first)" >&2
  exit 1
}

# Pass --home when FLOW_HOME is set (preserves the old override).
set --
[ -n "${FLOW_HOME:-}" ] && set -- --home "$FLOW_HOME"

# Prefer the checkout this script sits in (has extension.toml next to it);
# otherwise install the official "flow" extension from the remote source.
SRC_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" 2>/dev/null && pwd || true)"
if [ -n "$SRC_DIR" ] && [ -f "$SRC_DIR/extension.toml" ]; then
  exec friring-cli extension install "$SRC_DIR" "$@"
else
  exec friring-cli extension install flow "$@"
fi
