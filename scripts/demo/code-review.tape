# Demo tape for the Friring native code-review feature (VHS-tape syntax,
# replayed by scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run this directly — run `scripts/demo/record.sh code-review`, which
# builds the dev binary, sets up an isolated environment, seeds a worktree
# session ("review") whose branch has a real committed diff, and exports the env
# vars this tape relies on ($FRIRING_BIN).
#
# Requires: asciinema + agg + ffmpeg (see record.sh). The tape driver has no
# function-key commands, so the review toggle is opened with its default
# primary chord Ctrl+X (F7 is the alternate the driver can't send); everything
# else is Ctrl chords + plain keys.

Output docs/media/code-review-demo.gif
Output docs/media/code-review-demo.mp4

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

# The worktree "review" session is seeded LAST, so restore leaves it selected on
# launch with its terminal focused. Ctrl+X is in terminal_passthrough (the emacs
# prefix key), so from a focused terminal it is forwarded to the agent — focus
# the session list first (Ctrl+H), then Ctrl+X (default ToggleReview) opens the
# native code-review view: a continuous diff of the branch (<base>..HEAD) in the
# centre, the changed-files list in the right column, an action bar across the
# bottom.
Sleep 1200ms
Ctrl+H
Sleep 400ms
Ctrl+X
Sleep 1800ms

# Scroll the diff — added lines (green) and the file/hunk headers read clearly;
# the changed-files list tracks the current file.
Type "j"
Sleep 350ms
Type "j"
Sleep 350ms
Type "j"
Sleep 800ms

# `}` jumps to the next changed file; the files list highlight follows.
Type "}"
Sleep 1100ms

# `v` toggles side-by-side (old | new), then back to unified.
Type "v"
Sleep 1500ms
Type "v"
Sleep 1000ms

# Move onto a changed line, then comment: `c` opens an inline compose box; type
# a note, Tab cycles its classification (issue / suggestion / note / praise),
# Ctrl+S saves. The comment renders inline under its line with a coloured badge.
Type "j"
Sleep 300ms
Type "j"
Sleep 600ms
Type "c"
Sleep 900ms
Type "Use checked_add to surface overflow"
Sleep 600ms
Tab
Sleep 450ms
Tab
Sleep 650ms
Ctrl+S
Sleep 1500ms

# `r` marks the current file reviewed (a ✓ in the list + header).
Type "r"
Sleep 1100ms

# `t` opens the review-target picker: working changes, the whole branch, or a
# single commit — like tuicr's -w / -r / commit targets.
Type "t"
Sleep 1200ms
Type "j"
Sleep 900ms
Escape
Sleep 800ms

# Esc closes the review back to the agent terminal. Final beat; the recorder
# then stops filming (detach) and quits the TUI off-camera - see record.sh.
Escape
Sleep 1000ms
