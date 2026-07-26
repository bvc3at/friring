# Demo tape — Friring file manager (file viewer) feature clip (VHS-tape
# syntax, replayed by scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run directly — run `scripts/demo/record.sh file-manager`, which
# builds the dev binary, sets up the isolated env, seeds one session per real
# agent CLI in a sample repo, and exports $FRIRING_BIN.
#
# Requires: asciinema + agg + ffmpeg (see record.sh). Ctrl+E toggles the file
# viewer; Ctrl+L/H cycle pane focus. (No F-keys.)

Output docs/media/friring-file-manager.gif
Output docs/media/friring-file-manager.mp4

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
# it), and Ctrl+E (ToggleFileViewer) is a terminal-passthrough chord — so from
# the terminal it would be sent to the agent's PTY instead of toggling the
# panel. Ctrl+H (focus previous pane) is a navigation chord that is never passed
# through, so it reliably moves focus to the session list first.
Ctrl+H
Sleep 300ms

# Open the file viewer (Ctrl+E = alias for F3): the sample repo's file tree.
Ctrl+E
Sleep 600ms

# Focus the file viewer pane. From the session list, Ctrl+H wraps backward
# around the focus ring straight to the file viewer (its last stop).
Ctrl+H
Sleep 400ms

# Browse the tree: move down to `src` and expand it (Enter = FileViewerExpand).
#
# Only the tree is ever filmed, never a file's contents. Enter on a FILE
# resolves to `open_file_in_editor`, which hands the path to $EDITOR in another
# process — nothing about it renders in this pane. An earlier cut pressed it
# anyway and captioned it "open a file": a dead keypress that filmed as a held
# frame for its entire dwell.
Down
Sleep 300ms
Down
Sleep 300ms
Enter
Sleep 500ms

# Walk the expanded directory; the selection moves through the source files.
Down
Sleep 300ms
Down
Sleep 300ms
Down
Sleep 400ms

# Incremental search beat.
Type "/"
Sleep 300ms
Type "lib"
Sleep 600ms
Escape
Sleep 400ms

# Return focus to the session list (Ctrl+L wraps forward from the file viewer),
# then hide the viewer with Ctrl+E now that the terminal isn't focused.
Ctrl+L
Sleep 300ms
Ctrl+E
Sleep 400ms
