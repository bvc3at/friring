# Demo tape — Friring session-creation workflow feature clip (VHS-tape syntax,
# replayed by scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run directly — run `scripts/demo/record.sh session-creation`,
# which builds the dev binary, sets up the isolated env, seeds one session per
# real agent CLI in a sample repo, seeds a `~/projects` folder of several repos,
# and exports $FRIRING_BIN.
#
# Requires: asciinema + agg + ffmpeg (see record.sh). Ctrl+N opens the repo
# picker. Highlights: repos show by basename (not full path); Ctrl+P imports a
# parent folder whose git sub-dirs are listed beneath a collapsible header;
# then the usual name -> agent picker -> spawn flow.

Output docs/media/friring-session-creation.gif
Output docs/media/friring-session-creation.mp4

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

# The agent CLIs are pre-played and fully rendered; a short settle beat.
Sleep 1200ms

# Open the repo picker (Ctrl+N). The sample repo is already bookmarked (the
# seeded sessions registered it) and now shows by basename, not full path.
Ctrl+N
Sleep 1200ms

# Focus the path input (Tab) and type a PARENT folder containing several repos.
Tab
Sleep 400ms
Type "~/projects"
Sleep 800ms

# Ctrl+P imports it as a parent: its git sub-dirs are listed (by basename),
# nested under a collapsible "▾ projects (parent)" header — and the folder is
# re-scanned on every open, so new repos appear automatically.
Ctrl+P
Sleep 1500ms

# Space on the header folds the tree (▸), then unfolds it (▾) again.
Space
Sleep 900ms
Space
Sleep 900ms

# Move to a child repo and select it (Down to move, Space to check).
Down
Sleep 400ms
Space
Sleep 900ms

# Confirm the selection (Enter).
Enter
Sleep 900ms

# Session-name prompt: type a name, then confirm.
Type "aurora-forecast"
Sleep 800ms
Enter
Sleep 900ms

# Agent picker (shown because several agents are registered): choose one.
Sleep 500ms
Down
Sleep 450ms
Down
Sleep 550ms
Enter
Sleep 900ms

# The new session appears in the list and its agent boots (a real CLI painting
# on camera — leave it the time it needs).
Sleep 3s

# Final beat; the recorder then stops filming (detach) and quits the TUI
# off-camera - see record.sh.
Sleep 1000ms
