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
#
# Forking spawns a REAL agent CLI, so the two beats after Enter are the only
# genuinely unknown durations in this clip. They use `Wait Stable`, not a Sleep:
# a sleep long enough to cover a cold boot is frozen screen on a warm one, and
# that is what previously put two 3.5s holds — 5.2s of a 22s clip — on frames
# where not a single pixel changed.
#
# They keep the DEFAULT settle window, and the driver's "screen was still
# changing" note on these two lines is expected rather than a defect: the later
# stage it is reporting is the forked agent painting, which this clip
# deliberately does not film. The beats after here are session-list navigation
# that does not depend on that pane, and waiting for it would add seconds of
# blank terminal.
#
# Widening the window was tried and is worse. A settle window IS dwell — a wait
# ends by definition on `quiet` ms of unchanged screen and the Sleep after it
# lands on that same frame — and because every poll spawns tmux, its wall-clock
# cost overshoots the nominal figure under load. At 600ms this beat rendered a
# 1.04s held frame and the take was refused twice.

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
Ctrl+H
Sleep 300ms

# Fork the active session (Ctrl+F): a name modal opens, pre-filled with
# `<name>-fork`. Accept it as-is.
Ctrl+F
Sleep 500ms
Enter
# The fork spawns a real agent CLI that boots on camera. Hold for exactly as
# long as it takes to paint and no longer.
Wait Stable
Sleep 400ms

# The fork spawned nested under its parent (`└` tree prefix) and now has its
# terminal focused. Back to the list, then hop up to the parent ...
Ctrl+H
Sleep 300ms
Ctrl+K
Sleep 400ms

# ... and fork it again, this time tweaking the suggested name.
Ctrl+F
Sleep 400ms
Type "2"
Sleep 300ms
Enter
Wait Stable
Sleep 400ms

# Two children now nest under the same parent. Walk the tree — Ctrl+J/K follow
# the parent → children order automatically.
Ctrl+H
Sleep 300ms
Ctrl+K
Sleep 400ms
Ctrl+K
Sleep 400ms
Ctrl+J
Sleep 400ms

# The info panel shows the lineage: a `Parent:` row on the selected child. This
# is the one beat in the clip with text to actually read, so it gets the longest
# dwell here — still inside the per-pause budget check-pacing.mjs enforces.
Ctrl+B
Sleep 800ms
Ctrl+J
Sleep 600ms
Ctrl+B
Sleep 400ms
