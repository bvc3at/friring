# Demo tape — Friring theme switcher feature clip (VHS-tape syntax, replayed
# by scripts/demo/lib/drive-tape.mjs).
#
# Do NOT run directly — run `scripts/demo/record.sh theme`, which builds
# the dev binary, sets up the isolated env, seeds one session per real agent CLI
# in a sample repo, and exports $FRIRING_BIN.
#
# Requires: asciinema + agg + ffmpeg (see record.sh). Ctrl+Y opens the theme
# picker; Down/Up move, Enter selects. (No F-keys.)
#
# Moving the selection LIVE-PREVIEWS the palette (the whole TUI recolors), so
# every keypress here is a full-screen repaint — this clip is continuous motion
# by construction and needs no dwell beyond the palette registering.
#
# The picker lists `ThemePreset::all()` (src/session/theme_config.rs): the dark
# presets first, the four light ones grouped at the end. The demo starts in
# `doom`, which record.sh re-applies before every tape, and doom sits at index 4
# of 15 — NOT last. Stepping DOWN from it sweeps the ten themes below, ending on
# the light block, which is the clip's visual payoff. Keep the move counts in
# sync with that list: a press past either end is silently clamped and films as
# a frozen frame, which is exactly how this clip previously spent 3.8s — a third
# of its runtime — on one static image.

Output docs/media/friring-theme.gif
Output docs/media/friring-theme.mp4

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

# Open the theme picker (Ctrl+Y). It pre-selects the active theme, so the list
# opens on `doom`. The recorder only starts the beats once the attached client
# has painted a settled frame, so there is nothing to settle for here.
Ctrl+Y
Sleep 400ms

# Sweep DOWN the ten presets below doom: nord, dracula, one-dark, rose-pine-moon,
# everforest, kanagawa, then the light block — catppuccin-latte, tokyo-night-day,
# gruvbox-light, solarized-light. The dark-to-light flip is the strongest beat in
# the clip, so it lands last.
Down
Sleep 400ms
Down
Sleep 400ms
Down
Sleep 400ms
Down
Sleep 400ms
Down
Sleep 400ms
Down
Sleep 400ms
Down
Sleep 400ms
Down
Sleep 400ms
Down
Sleep 400ms
Down
Sleep 500ms

# Back UP four to kanagawa — a rich dark palette to leave the TUI in — and
# commit it with Enter.
Up
Sleep 300ms
Up
Sleep 300ms
Up
Sleep 300ms
Up
Sleep 400ms
Enter

# A plain dwell, deliberately — NOT `Wait Stable`. Committing a theme only
# recolors: the pane's text is identical before and after, and Wait reads
# `tmux capture-pane`, which returns text without styling. It cannot see a
# colour-only repaint, so it would sit out its full grace and film the pause it
# exists to prevent. Wait is for beats that change what the screen SAYS.
Sleep 500ms
