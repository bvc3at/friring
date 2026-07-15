# VHS tape for the Friring Automations feature demo.
#
# Do NOT run this directly — run `scripts/demo/record.sh automations`, which
# builds the dev binary, sets up an isolated environment, seeds one session per
# real agent CLI, and exports the env vars this tape relies on ($FRIRING_BIN).
#
# Requires: vhs (charmbracelet/vhs) + ffmpeg + ttyd. VHS has no function-key
# commands, so this uses Ctrl-key bindings + j/k / Tab / arrows only.

Output docs/media/automations-demo.gif
Output docs/media/automations-demo.mp4

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

# Sessions are pre-seeded; the always-present Automations pane sits beneath the
# session list in the left column.
Sleep 2500ms

# The Automations pane sits in the left column, beneath the session list. Focus
# the list first (Ctrl+H from the terminal), then Ctrl+P (OpenAutomations) jumps
# straight to the pane. Ctrl+P is in terminal_passthrough (it is readline's
# previous-history), so it only acts as the chord once focus has left the
# terminal — hence the Ctrl+H first.
#
# This used to walk `Down` past the last session to show the left column being
# one continuous list. That counted on a fixed number of sessions: seed one
# agent fewer and the walk lands somewhere else entirely, the keystrokes meant
# for the editor get eaten by the session list's single-letter hotkeys (the `i`
# in "morning-triage" opens the conversation-import picker), and the clip films
# the wrong screen. The chord is count-independent.
Ctrl+H
Sleep 1200ms
Ctrl+P
Sleep 1600ms

# Create a new automation right from the pane (`n`, like Ctrl+N for a session).
# The editor opens in the central pane — one screen, no separate popup.
Type "n"
Sleep 1800ms

# Name it.
Type "morning-triage"
Sleep 900ms

# Trigger is a selector cycled with the arrows — step through, land on "daily".
Tab
Sleep 700ms
Right
Sleep 800ms
Right
Sleep 800ms
Left
Sleep 800ms
Left
Sleep 900ms

# Hour stepper (arrows adjust, wrapping); watch the "next:" preview update.
Tab
Sleep 600ms
Right
Sleep 500ms
Right
Sleep 700ms

# Minute stepper.
Tab
Sleep 600ms
Right
Sleep 450ms
Right
Sleep 700ms

# Skip timezone, leave the Action on "send".
Tab
Sleep 700ms
Tab
Sleep 900ms

# Target selector — cycle through the running sessions to pick where the
# prompt is sent.
Tab
Sleep 700ms
Right
Sleep 800ms
Right
Sleep 900ms

# The prompt that fires on schedule.
Tab
Sleep 600ms
Type "Triage failing CI and summarize blockers"
Sleep 1200ms

# Save with Ctrl+S — returns to the pane; the automation shows a live next-run
# countdown. NOT Enter: `prompt` is the one multi-line field, so Enter inserts a
# newline there rather than saving (see AutomationEditorModal::handle_key), which
# left the modal open and swallowed every later beat, including the closing quit.
Ctrl+S
Sleep 2000ms

# Toggle enabled off and back on, straight from the pane.
Space
Sleep 1100ms
Space
Sleep 1400ms

# Step into the central pane: Ctrl+L focuses the editor (its fields), again
# focuses the run-history panel beneath it.
Ctrl+L
Sleep 1600ms
Ctrl+L
Sleep 1600ms

# Run it now from the history panel (`r`); the run is recorded and the prompt
# is delivered to the target session.
Type "r"
Sleep 2500ms

# Ctrl+H walks back to the editor, then to the Automations list (like Esc) —
# the cycle stays within the automation context, never jumping to a session.
Ctrl+H
Sleep 1000ms
Ctrl+H
Sleep 1600ms

# Final beat, then quit (detaches sessions).
Sleep 1500ms
Ctrl+Q
Sleep 1s
