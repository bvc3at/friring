# Demo tape — Friring session forking (parent sessions) feature clip (VHS-tape
# syntax, replayed by scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run directly — run `scripts/demo/record.sh fork`, which builds the
# dev binary, sets up the isolated env, seeds one session per real agent CLI
# in a sample repo, and exports $FRIRING_BIN.
#
# Requires: asciinema + agg + ffmpeg (see record.sh). Ctrl+F forks the active
# session: a name modal opens pre-filled with `<name>-fork`, and the spawned
# copy records the source as its parent — the session list nests children under
# their parent with a `└` tree prefix, and the info panel (Ctrl+B) shows a
# `Parent:` row.

Output docs/media/friring-fork.gif
Output docs/media/friring-fork.mp4

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

# The TUI adopts the seeded sessions with the terminal focused, so hop back to
# the session list first (Ctrl+F would otherwise forward to the agent's PTY).
Sleep 1200ms
Ctrl+H
Sleep 500ms

# Fork the active session (Ctrl+F): a name modal opens, pre-filled with
# `<name>-fork`. Accept it as-is.
Ctrl+F
Sleep 1200ms
Enter
# The fork spawns a real agent CLI that boots on camera — leave it the time it
# needs to paint.
Sleep 3500ms

# The fork spawned nested under its parent (`└` tree prefix) and now has its
# terminal focused. Back to the list, then hop up to the parent ...
Ctrl+H
Sleep 500ms
Ctrl+K
Sleep 900ms

# ... and fork it again, this time tweaking the suggested name.
Ctrl+F
Sleep 900ms
Type "2"
Sleep 500ms
Enter
Sleep 3500ms

# Two children now nest under the same parent. Walk the tree — Ctrl+J/K follow
# the parent → children order automatically.
Ctrl+H
Sleep 500ms
Ctrl+K
Sleep 800ms
Ctrl+K
Sleep 800ms
Ctrl+J
Sleep 800ms

# The info panel shows the lineage: a `Parent:` row on the selected child.
Ctrl+B
Sleep 2200ms
Ctrl+J
Sleep 1600ms
Ctrl+B
Sleep 800ms

# Final beat; the recorder then stops filming (detach) and quits the TUI
# off-camera - see record.sh.
Sleep 1000ms
