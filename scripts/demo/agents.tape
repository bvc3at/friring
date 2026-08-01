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

# On launch the active session's TERMINAL is focused (session restore focuses
# it), and every chord this tape uses — Ctrl+J walk, Ctrl+B/Ctrl+E panel
# toggles, Ctrl+X review — is a terminal-passthrough chord that would be sent
# to the agent's PTY instead of acting (five Ctrl+J newlines quietly growing
# claude's composer, once upon a take). Ctrl+H (focus previous pane) is a
# navigation chord that is never passed through, so it reliably moves focus to
# the session list first; from there everything below lands.
Ctrl+H
Sleep 300ms

# Walk through each pre-seeded agent session — ONE Ctrl+J PER SEEDED SESSION
# (five: claude / codex / opencode / antigravity / the claude "review"
# worktree), each mid-conversation on its own scripted turn. Restore leaves
# "review" (seeded last) selected, so a full cycle of presses ends back on it
# — which the code-review beat below relies on. Seed one agent fewer and this
# walk needs one press fewer: the count is tied to demo-content.json, and a
# surplus press is not harmless — it lands on an already-selected row and films
# as a frozen frame for the whole beat.
Ctrl+J
Sleep 750ms
Ctrl+J
Sleep 750ms
Ctrl+J
Sleep 750ms
Ctrl+J
Sleep 750ms
Ctrl+J
Sleep 750ms

# Show the info panel (Ctrl+B = alias for F2): per-session details, metrics,
# and the account-usage gauges (served by the demo stub).
Ctrl+B
Sleep 800ms

# Show the file viewer (Ctrl+E = alias for F3): the sample repo's file tree.
Ctrl+E
Sleep 600ms

# Focus the file viewer pane and browse the tree. From the session list,
# Ctrl+H wraps backward around the focus ring straight to the file viewer
# (its last stop); Ctrl+L would land on the terminal instead.
Ctrl+H
Sleep 350ms
Down
Sleep 300ms
Down
Sleep 300ms
Enter
Sleep 500ms
Down
Sleep 350ms

# Return focus to the session list (Ctrl+L wraps forward from the file
# viewer), hide the panels again.
Ctrl+L
Sleep 300ms
Ctrl+E
Sleep 300ms
Ctrl+B
Sleep 400ms

# --- Native code review ------------------------------------------------------
# The walk above cycled the selection back to "review" — a worktree on a branch
# with a real committed diff (seeded by record.sh) — and the panel toggles
# don't move the selection. Focus is on the session list (not a terminal), so
# Ctrl+X runs ToggleReview instead of being forwarded to the agent by
# terminal_passthrough. Open the built-in code-review view: a GitHub-style
# continuous diff of the branch (<base>..HEAD) in the centre, the changed-files
# list on the right, an action bar across the bottom — no external binary.
Ctrl+X
Sleep 900ms

# Scroll the diff — added lines (green) read clearly and the changed-files list
# tracks the current file.
Type "j"
Sleep 300ms
Type "j"
Sleep 300ms
Type "j"
Sleep 450ms

# `}` jumps to the next changed file; the files list highlight follows.
Type "}"
Sleep 600ms

# `v` toggles side-by-side (old | new), then back to unified.
Type "v"
Sleep 800ms
Type "v"
Sleep 500ms

# Esc closes the review, back to the session list. Wait for it to actually be
# gone: Ctrl+N sent while the review is still up does not open the repo picker,
# and every remaining beat then plays into the wrong surface. A fixed dwell here
# is a coin flip — it worked on one take and hung the next.
Escape
# `Wait Stable`, and it is load-bearing: after the code-review view closes, the
# app does not act on Ctrl+N for about three seconds. A fixed 400ms or 600ms
# dwell here sends the chord into that window, where it is dropped — the repo
# picker never opens, every later beat plays into the wrong surface, and the
# take wedges. Only waiting for the screen to settle survives it, and it
# measures ~3.0s. That pause is the app's, not the demo's; see FORK.md.
Wait Stable
Sleep 300ms

# --- Session-creation workflow -----------------------------------------------
# Ctrl+N opens the repo picker. The sample repo is already bookmarked (the
# seeded sessions registered it), so we select it and walk the full flow:
# repo picker -> session name -> agent picker -> spawn.
Ctrl+N
# The picker builds its list from disk, so wait for it rather than dwelling a
# guessed amount: a key on a half-built list lands on whatever row happens to be
# there and silently derails every later beat.
Wait Stable
Sleep 300ms

# The seeded repos live under one parent, so the list opens on a collapsed
# "▾ projects (parent)" header rather than on a repo. Unfold it, step onto a
# child, check it, confirm — the same sequence session-creation.tape uses.
#
# This beat used to be a bare Space+Enter, written for the OLD "Select Repos"
# modal (Repos (0), `Space toggle/fold … Enter ok`). That picker has since been
# replaced by "New Session — Repo", and the tape was never updated: re-recording
# it landed the session name in the picker's filter field and wedged the run.
# The shipped hero predates the redesign, which is why nothing caught it.
Space
Sleep 300ms
Down
Sleep 250ms
Space
Sleep 350ms
Enter
Sleep 450ms

# Session-name prompt. The field arrives PRE-FILLED with the repo's basename,
# so clear it before typing or the two concatenate — an earlier take of this
# clip created a session called `orbital-hvacaurora-forecast`. More Backspaces
# than the prefill is long; the extras are no-ops on an empty field. (Ctrl+U
# would not work: the modal's text input swallows every Ctrl+letter chord.)
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

# End on the new session appearing in the list — the point of the beat — and
# deliberately do NOT film the agent booting behind it.
#
# Measured, not assumed: waiting for opencode's own first-paint marker (`Build
# ·`, what record.sh's pre-play syncs on) takes 3.55s on this machine, and every
# one of those seconds is a blank pane held on screen. No tape edit shortens a
# real boot; the only choice is whether the clip sits and watches it. The
# session row is written as soon as the session exists, so this lands in ~0.3s
# and the demo closes on the thing it set out to show.
Wait /aurora-forecast/
Sleep 500ms
