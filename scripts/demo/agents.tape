# Demo tape for the Friring REAL-AGENTS hero video (VHS-tape syntax, replayed
# by scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run this directly — run `scripts/demo/record.sh`, which builds
# the dev binary, sets up an isolated environment, seeds one session per real
# agent CLI, and exports $FRIRING_BIN. See that script for context.
#
# Requires: asciinema + agg + ffmpeg (see record.sh). The tape driver has no
# function-key commands, so this uses the Ctrl aliases of the panel toggles:
# Ctrl+B = info panel, Ctrl+E = file viewer, Ctrl+X = code review (its default
# primary chord; F7 is the alternate the driver can't send).

Output docs/media/friring-demo.gif
Output docs/media/friring-demo.mp4

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
Sleep 1500ms

# On launch the active session's TERMINAL is focused (session restore focuses
# it), and every chord this tape uses — Ctrl+J walk, Ctrl+B/Ctrl+E panel
# toggles, Ctrl+X review — is a terminal-passthrough chord that would be sent
# to the agent's PTY instead of acting (five Ctrl+J newlines quietly growing
# claude's composer, once upon a take). Ctrl+H (focus previous pane) is a
# navigation chord that is never passed through, so it reliably moves focus to
# the session list first; from there everything below lands.
Ctrl+H
Sleep 500ms

# Walk through each pre-seeded agent session — ONE Ctrl+J PER SEEDED SESSION
# (five: claude / codex / opencode / antigravity / the claude "review"
# worktree), each mid-conversation on its own scripted turn. Restore leaves
# "review" (seeded last) selected, so a full cycle of presses ends back on it
# — which the code-review beat below relies on. Seed one agent fewer and this
# walk needs one press fewer: the count is tied to demo-content.json.
Ctrl+J
Sleep 1800ms
Ctrl+J
Sleep 1800ms
Ctrl+J
Sleep 1800ms
Ctrl+J
Sleep 1800ms
Ctrl+J
Sleep 1800ms

# Show the info panel (Ctrl+B = alias for F2): per-session details, metrics,
# and the account-usage gauges (served by the demo stub).
Ctrl+B
Sleep 2000ms

# Show the file viewer (Ctrl+E = alias for F3): the sample repo's file tree.
Ctrl+E
Sleep 1200ms

# Focus the file viewer pane and browse the tree. From the session list,
# Ctrl+H wraps backward around the focus ring straight to the file viewer
# (its last stop); Ctrl+L would land on the terminal instead.
Ctrl+H
Sleep 600ms
Down
Sleep 350ms
Down
Sleep 350ms
Enter
Sleep 900ms
Down
Sleep 500ms

# Return focus to the session list (Ctrl+L wraps forward from the file
# viewer), hide the panels again.
Ctrl+L
Sleep 500ms
Ctrl+E
Sleep 400ms
Ctrl+B
Sleep 800ms

# --- Native code review ------------------------------------------------------
# The walk above cycled the selection back to "review" — a worktree on a branch
# with a real committed diff (seeded by record.sh) — and the panel toggles
# don't move the selection. Focus is on the session list (not a terminal), so
# Ctrl+X runs ToggleReview instead of being forwarded to the agent by
# terminal_passthrough. Open the built-in code-review view: a GitHub-style
# continuous diff of the branch (<base>..HEAD) in the centre, the changed-files
# list on the right, an action bar across the bottom — no external binary.
Ctrl+X
Sleep 1800ms

# Scroll the diff — added lines (green) read clearly and the changed-files list
# tracks the current file.
Type "j"
Sleep 350ms
Type "j"
Sleep 350ms
Type "j"
Sleep 700ms

# `}` jumps to the next changed file; the files list highlight follows.
Type "}"
Sleep 1000ms

# `v` toggles side-by-side (old | new), then back to unified.
Type "v"
Sleep 1500ms
Type "v"
Sleep 1000ms

# Esc closes the review, back to the session list.
Escape
Sleep 800ms

# --- Session-creation workflow -----------------------------------------------
# Ctrl+N opens the repo picker. The sample repo is already bookmarked (the
# seeded sessions registered it), so we select it and walk the full flow:
# repo picker -> session name -> agent picker -> spawn.
Ctrl+N
Sleep 1200ms

# Select the highlighted repo (Space toggles selection), then confirm (Enter).
Space
Sleep 700ms
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
Sleep 400ms
Down
Sleep 500ms
Enter
Sleep 900ms

# The new session appears in the list and its agent boots (a real CLI painting
# on camera — leave it the time it needs).
Sleep 3s

# Final beat; the recorder then stops filming (detach) and quits the TUI
# off-camera - see record.sh.
Sleep 1000ms
