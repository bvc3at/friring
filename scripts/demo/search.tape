# Demo tape for the Friring Global Search feature (Ctrl+A) (VHS-tape syntax,
# replayed by scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run this directly — run `scripts/demo/record.sh search`, which builds
# the dev binary, sets up an isolated environment, seeds one session per real
# agent CLI plus a few tasks/automations to search over, and exports the env
# vars this tape relies on ($FRIRING_BIN).
#
# Requires: asciinema + agg + ffmpeg (see record.sh). Global search opens with
# Ctrl+A (a reliable chord on every terminal); the strip docks full-width along
# the bottom and highlights matches live in the panels themselves.

Output docs/media/search-demo.gif
Output docs/media/search-demo.mp4

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

# Sessions are pre-seeded, plus a few tasks and an automation to search across.
# Surface the tasks panel first so its matches highlight live during the search.
Ctrl+W
Sleep 450ms
Escape
Sleep 300ms

# Open global search (Ctrl+A): a non-modal strip docks along the bottom and
# searches EVERY scope at once — sessions, tasks, automations, and files.
Ctrl+A
Sleep 500ms

# Type a query. `carbo` fuzzy-matches across every scope at once — the
# carbon-drawdown session, the carbon-capture task, and the carbon-drift-watch
# automation — so the grouped results span all three. Matches highlight live in
# the panels themselves (accent + bold + underline on matching rows, the rest
# dimmed); the strip shows per-scope counts and a grouped result list.
#
# The session buffer-content scan lands after a short debounce, so this dwell
# has to outlast it — one of the few here doing more than letting the eye
# settle. Shortening it films the results arriving after the beat has moved on.
Type "carbo"
Sleep 700ms

# Move the selection through the grouped results; the owning panel previews the
# selected row (its cursor moves) while focus stays in the strip.
Down
Sleep 400ms
Down
Sleep 400ms
Down
Sleep 450ms

# Refine the query — counts and highlights update on every keystroke.
Backspace
Backspace
Backspace
Backspace
Backspace
Sleep 300ms
Type "drift"
Sleep 700ms

# Enter jumps to the selected result and focuses its pane (here, a session's
# terminal). Esc would instead restore the exact pre-search state.
Enter
Sleep 700ms

# Open it once more to show a task jump, then cancel with Esc (state restored).
Ctrl+A
Sleep 450ms
Type "habitat"
Sleep 650ms
Down
Sleep 450ms
Escape
Sleep 500ms
