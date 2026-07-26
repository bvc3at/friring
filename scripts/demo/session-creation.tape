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

# Session-name prompt. The field arrives PRE-FILLED with the chosen repo's
# basename, so clear it first or the two concatenate — the clip this replaces
# shipped a session called `orbital-hvacaurora-forecast`. More Backspaces than
# the prefill is long; the extras are no-ops on an empty field.
Backspace 16
Sleep 250ms
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

# End on the new session appearing in the list — the payoff of a
# session-creation clip — and deliberately do not film its agent booting.
#
# `Wait Stable` is wrong here and was measurably wrong: this beat happens in two
# stages (the picker closes and the row appears, then ~3.5s later the agent CLI
# paints), so a settle window resolves on the first stage and the clip ended on
# a blank terminal. Waiting for the boot instead would film 3.5s of that blank
# pane, which no settle window can shorten. The row is written as soon as the
# session exists, so this marker lands in ~0.3s. It cannot match early: the name
# is not on screen once the name modal closes.
Wait /aurora-forecast/
Sleep 500ms
