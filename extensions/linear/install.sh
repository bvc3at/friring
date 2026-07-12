#!/usr/bin/env sh
# Thin wrapper kept for the curl|sh one-liner. The real installer lives in
# friring itself:
#
#   friring-cli extension install linear
#
# This script just forwards to it — using a local checkout when run from one,
# otherwise the official remote source. It fetches the manifest + payload, lays
# down ~/.config/friring/extensions/linear, and activates the linear-tick automation — a deterministic exec
# sync (no agent, no session), which friring then self-heals.
#
# Usage:
#   ./install.sh                  # from a checkout
#   curl -fsSL https://raw.githubusercontent.com/Thurbeen/thurbox/main/extensions/linear/install.sh | sh
#
# Environment variables:
#   LINEAR_HOME=<dir>   override install home (default: <config>/extensions/linear)
#
# Authenticate afterwards: put your Linear personal API key in
# ~/.config/friring/extensions/linear/credentials.env as `LINEAR_API_KEY=lin_api_xxom` (Settings → Account →
# Security & access → Personal API keys). Then add teams to ~/.config/friring/extensions/linear/trackers.md.
# To turn it off:
#   friring-cli extension deactivate linear [--force --purge]

set -eu

command -v friring-cli >/dev/null 2>&1 || {
  echo "error: friring-cli not found in PATH (install friring first)" >&2
  exit 1
}

# Pass --home when LINEAR_HOME is set.
set --
[ -n "${LINEAR_HOME:-}" ] && set -- --home "$LINEAR_HOME"

# Prefer the checkout this script sits in (has extension.toml next to it);
# otherwise install the official "linear" extension from the remote source.
SRC_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" 2>/dev/null && pwd || true)"
if [ -n "$SRC_DIR" ] && [ -f "$SRC_DIR/extension.toml" ]; then
  exec friring-cli extension install "$SRC_DIR" "$@"
else
  exec friring-cli extension install linear "$@"
fi
