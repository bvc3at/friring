# Demo tape for the Friring Automations feature (VHS-tape syntax, replayed by
# scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run this directly — run `scripts/demo/record.sh automations`, which
# builds the dev binary, sets up an isolated environment, seeds one session per
# real agent CLI, and exports the env vars this tape relies on ($FRIRING_BIN).
#
# Requires: asciinema + agg + ffmpeg (see record.sh). The tape driver has no
# function-key commands, so this uses Ctrl-key bindings + j/k / Tab / arrows.

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
Sleep 1200ms

# Focus the session list (Ctrl+H from the terminal), then step INTO the
# Automations pane: the left column reads as one continuous list, and `Down`
# on the LAST session flows into the pane (act_session_list_next). The seeder
# creates "review" last and restore leaves it selected, so one `Down` lands in
# the pane regardless of how many agents were available — unlike a fixed walk,
# and unlike Ctrl+P, which opens the automations list as a centered overlay
# rather than focusing the pane (that path closes back to the session list on
# save, which once left two-thirds of this tape typing at a static screen).
Ctrl+H
Sleep 600ms
Down
Sleep 900ms

# Create a new automation right from the pane (`n`, like Ctrl+N for a session).
# This is the in-pane path: the editor opens in the central pane — one screen,
# no popup — and saving returns focus HERE, to the pane.
Type "n"
Sleep 1000ms

# Name it.
Type "morning-triage"
Sleep 700ms

# Trigger is a selector cycled with the arrows — step through, land on "daily".
Tab
Sleep 400ms
Right
Sleep 350ms
Right
Sleep 350ms
Left
Sleep 350ms
Left
Sleep 500ms

# Hour stepper (arrows adjust, wrapping); watch the "next:" preview update.
Tab
Sleep 350ms
Right
Sleep 300ms
Right
Sleep 450ms

# Minute stepper.
Tab
Sleep 350ms
Right
Sleep 250ms
Right
Sleep 450ms

# Skip timezone, leave the Action on "send".
Tab
Sleep 400ms
Tab
Sleep 500ms

# Target selector — cycle through the running sessions to pick where the
# prompt is sent.
Tab
Sleep 400ms
Right
Sleep 350ms
Right
Sleep 500ms

# The prompt that fires on schedule.
Tab
Sleep 350ms
Type "Triage failing CI and summarize blockers"
Sleep 800ms

# Save with Ctrl+S — back to the pane with the new automation selected (a new
# one lands at the top) and a live next-run countdown. NOT Enter: `prompt` is
# the one multi-line field, so Enter inserts a newline there rather than saving
# (see AutomationEditorModal::handle_key).
Ctrl+S
Sleep 1400ms

# Toggle enabled off and back on, straight from the pane.
Space
Sleep 700ms
Space
Sleep 900ms

# Step into the central pane: Ctrl+L focuses the editor (its fields), again
# focuses the run-history panel beneath it. The focus ring stays within the
# automation context (pane → editor → history), never jumping to a session.
Ctrl+L
Sleep 900ms
Ctrl+L
Sleep 900ms

# Run it now from the history panel (`r`); the run is recorded and the prompt
# is delivered to the target session.
Type "r"
Sleep 1500ms

# Ctrl+H walks back to the editor, then to the Automations pane (like Esc).
Ctrl+H
Sleep 500ms
Ctrl+H
Sleep 900ms

# Final beat; the recorder then stops filming (detach) and quits the TUI
# off-camera - see record.sh.
Sleep 1000ms
