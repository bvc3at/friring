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

# Open the repo picker (Ctrl+N). The sample repo is already bookmarked (the
# seeded sessions registered it) and now shows by basename, not full path.
Ctrl+N
Sleep 600ms

# Focus the path input (Tab) and type a PARENT folder containing several repos.
Tab
Sleep 300ms
Type "~/projects"
Sleep 500ms

# Ctrl+P imports it as a parent: its git sub-dirs are listed (by basename),
# nested under a collapsible "▾ projects (parent)" header — and the folder is
# re-scanned on every open, so new repos appear automatically. A whole tree
# arrives here, so this is one of the few beats with something to actually read.
Ctrl+P
Sleep 800ms

# Space on the header folds the tree (▸), then unfolds it (▾) again.
Space
Sleep 450ms
Space
Sleep 450ms

# Move to a child repo and select it (Down to move, Space to check).
Down
Sleep 300ms
Space
Sleep 400ms

# Confirm the selection (Enter).
Enter
Sleep 450ms

# Session-name prompt: type a name, then confirm.
Type "aurora-forecast"
Sleep 500ms
Enter
Sleep 450ms

# Agent picker (shown because several agents are registered): choose one.
Down
Sleep 300ms
Down
Sleep 350ms
Enter

# The new session appears in the list and its agent boots — a real CLI painting
# on camera, so its duration is the machine's, not the demo's. Wait for the pane
# to settle instead of guessing: the 3s guess this replaces filmed as a frozen
# frame right at the end of the clip, where a viewer is most likely to bail.
Wait Stable
Sleep 500ms
