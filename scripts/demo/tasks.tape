# Demo tape for the Friring Tasks (todo list) feature (VHS-tape syntax,
# replayed by scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run this directly — run `scripts/demo/record.sh tasks`, which builds
# the dev binary, sets up an isolated environment, seeds one session per real
# agent CLI, and exports the env vars this tape relies on ($FRIRING_BIN).
#
# Requires: asciinema + agg + ffmpeg (see record.sh). The tape driver has no
# function-key commands, so this uses Ctrl-key bindings + j/k / Tab / arrows
# only (the tasks panel toggles with Ctrl+W, the F5 alternative).
#
# The typed beats here are the clip's best material: typing is continuous
# motion, so the long `Type` runs cost runtime but never dead air. The dwells
# around them are what gets kept short.

Output docs/media/tasks-demo.gif
Output docs/media/tasks-demo.mp4

Set Shell "bash"
Set FontSize 18
Set Width 1920
Set Height 1080
Set Padding 16
Set Theme "Catppuccin Mocha"
Set PlaybackSpeed 1.0

# Launch the dev binary (isolated friring-dev socket + XDG dirs from the runner).
Hide
Type `exec "$FRIRING_BIN"`
Enter
Sleep 2s
Show

# On launch the active session's TERMINAL is focused (session restore focuses
# it), and Ctrl+W (FocusTasks) is a terminal-passthrough chord — so from the
# terminal it would be sent to the agent's PTY instead of toggling the panel.
# Ctrl+H (focus previous pane) is a navigation chord that is never passed
# through, so it reliably moves focus to the session list first.
Ctrl+H
Sleep 300ms

# Sessions are pre-seeded (incl. a task with a markdown description). Open AND
# focus the tasks panel — a toggleable right-side column between the terminal
# and the file viewer (Ctrl+W, the F5 alternative). The central pane shows the
# selected task full-screen: its title, metadata, and rendered-markdown body.
Ctrl+W
Sleep 700ms

# j/k move through the list; the central preview follows the selection.
Type "j"
Sleep 450ms
Type "k"
Sleep 450ms

# Create a task right from the panel (`n`, like Ctrl+N for a session). The
# editor opens full-screen in the central pane — one screen, no popup.
Type "n"
Sleep 500ms

# Title it (the Title field is focused first).
Type "Add integration tests"
Sleep 400ms

# Tab to the multi-line Description: Enter inserts newlines, so you can write
# real markdown. Ctrl+S saves from any field.
Tab
Sleep 300ms
Type "## Acceptance"
Enter
Type "- cover the **parser** edge cases"
Enter
Type "- add a CI gate"
Sleep 500ms

# Saving returns to the panel, where the preview renders that markdown
# full-screen and the footer advertises the actions: e edit · r run · Space
# status · n new · d del. Two beats' worth of new content on one frame.
Ctrl+S
Sleep 800ms

# Space cycles the selected task's status in place (todo -> in progress -> done).
Space
Sleep 450ms

# `r` runs the task: a trigger-time picker offers Send -> a running session or
# Spawn a new one — the action is chosen now, not configured up front.
Type "r"
Sleep 700ms

# Pick the first option (Send to a session) and run it; the task advances to
# in-progress.
Enter
Sleep 700ms

# Esc returns focus to the session list; Ctrl+W hides the panel again.
Escape
Sleep 400ms
Ctrl+W
Sleep 400ms
