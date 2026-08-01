# Demo tape — Friring info panel feature clip (VHS-tape syntax, replayed by
# scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run directly — run `scripts/demo/record.sh info-panel`, which
# builds the dev binary, sets up the isolated env, seeds one session per real
# agent CLI in a sample repo, and exports $FRIRING_BIN.
#
# Requires: asciinema + agg + ffmpeg (see record.sh). Ctrl+B toggles the info
# panel; Ctrl+J/K move between sessions. (No F-keys.)
#
# This clip is almost all "read the panel" beats, so its dwells sit at the top
# of the pacing budget rather than the bottom — but they are still dwells the
# viewer spends on new text, not on a screen that already stopped changing.

Output docs/media/friring-info-panel.gif
Output docs/media/friring-info-panel.mp4

Set Shell "bash"
Set FontSize 18
Set Width 1920
Set Height 1080
Set Padding 16
Set Theme "Catppuccin Mocha"
Set PlaybackSpeed 1.0

# Launch the dev binary (isolated env from record.sh).
Hide
Type `exec "$FRIRING_BIN"`
Enter
Sleep 2s
Show

# On launch the active session's TERMINAL is focused (session restore focuses
# it), and Ctrl+B (ToggleInfoPanel) is a terminal-passthrough chord — so from
# the terminal it would be sent to the agent's PTY instead of toggling the
# panel. Ctrl+H (focus previous pane) is a navigation chord that is never passed
# through, so it reliably moves focus to the session list first.
Ctrl+H
Sleep 300ms

# Open the info panel (Ctrl+B = alias for F2): name/status/agent, CPU/RAM
# gauges, and the account-usage windows (served by the demo stub). The most new
# text in the clip lands on this frame, so it gets the longest dwell.
Ctrl+B
Sleep 900ms

# Move between sessions — the info panel tracks the selected session. Hop to
# the first session (Ctrl+J wraps forward from "review", the last) and back:
# both stops are claude sessions, whose usage gauges the stub feeds — the
# other agents would just film a "not logged in" note here.
Ctrl+J
Sleep 700ms
Ctrl+K
Sleep 700ms

# Close the info panel.
Ctrl+B
Sleep 400ms
