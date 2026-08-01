# shellcheck shell=bash
#
# Scenario: a ghost survives a terminal shrink. vt100's `set_size` truncates
# each row's cells (`Vec::resize`), so narrowing a pane destroys every cell
# past the new width and widening back pads with blanks. A live session hides
# that — its agent repaints on SIGWINCH — but a ghost has no process to
# repaint it, so the frozen frame was permanently clipped to the narrowest
# size the terminal ever hit, leaving bare background where its content was.
#
# Drives the real thing: type a line wider than the shrunk pane, unload to a
# ghost, shrink the terminal, widen it back, and require the line's tail to
# still be there. Friring owns the ghost's bytes, so this is recoverable.
#
# Test-only (resizes the driver tmux window mid-steps); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="A ghost re-renders its saved frame after the terminal shrinks and grows back"
SCENARIO_AGENT="scripted"

# A line long enough to be clipped by the shrunk pane but to fit the wide one.
# 70 chars + the agent's `GOT:` prefix = 74; the wide pane's central column is
# ~90 columns and the shrunk terminal is 50 wide (single-pane, ~48 usable).
GHOST_RESIZE_PAD="AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
GHOST_RESIZE_LINE="GHOST-RESIZE-${GHOST_RESIZE_PAD}-ENDMARK"

# Resize the window friring itself runs in, then let it settle: the TUI must
# see SIGWINCH, re-layout, and re-push the new size to every session.
ghost_resize_to() {
    tmux -L "$E2E_DRIVER_SOCKET" resize-window \
        -t "$E2E_DRIVER_SESSION" -x "$1" -y "$2" \
        || e2e_die "could not resize the driver window to ${1}x${2}"
    sleep 1
}

scenario_steps() {
    # Own the geometry rather than inheriting the harness default: the marker
    # must fit the wide pane and not the narrow one.
    tmux -L "$E2E_DRIVER_SOCKET" set-option -t "$E2E_DRIVER_SESSION" \
        window-size manual 2>/dev/null || true
    ghost_resize_to 200 50

    step_wait_pane "SCRIPTED-READY mode=new" 60
    step_type "$GHOST_RESIZE_LINE"
    step_key Enter
    step_wait_pane "GOT:$GHOST_RESIZE_LINE" 30

    # Freeze it: the ghost keeps the frame, with no agent left to repaint it.
    step_key M-u
    step_wait_pane "unloaded — Enter loads" 30
    step_wait_pane "ENDMARK" 10

    # Shrink well below the marker's width, then restore the original size.
    ghost_resize_to 50 20
    ghost_resize_to 200 50

    # The tail must come back. Before the fix the cells were already gone and
    # the widened pane showed blanks where the marker had been.
    step_wait_pane "ENDMARK" 15
}

scenario_assert_ui() {
    assert_pane_contains "ENDMARK"
    assert_pane_contains "unloaded — Enter loads"
}
