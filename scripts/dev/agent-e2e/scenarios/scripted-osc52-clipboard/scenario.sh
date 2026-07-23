# shellcheck shell=bash
#
# Scenario: in-pane clipboard copies and the Cmd+C chord, end to end.
#
# A program inside a pane copies by writing OSC 52 to its tty; under $TMUX
# real agents (Claude Code's /copy) wrap it in the tmux DCS passthrough,
# which the pane server's default `allow-passthrough off` silently discards
# — friring must recover the copy from the raw control-mode stream
# (`agent::osc52`) and route it out through `App::set_clipboard_text`. Here
# that full chain runs against real plumbing: scripted agent emits →
# friring-dev tmux `%output` → TUI scanner/drain → `tmux load-buffer -w`
# against the DRIVER server (the TUI's `$TMUX`) — whose paste buffer the
# test then reads back with `show-buffer`. The native clipboard leg is
# forced off via a fake SSH environment (scenario_setup) so the copy is
# byte-assertable and can never touch the host's real clipboard.
#
# The Cmd+C legs inject the raw kitty-protocol encoding of super+c
# (`CSI 99;9u`) into the TUI's tty: from the session list it must dispatch
# `Copy` (copying the status message — asserted via the driver buffer), and
# in a focused terminal with no selection it must be swallowed — no SIGINT,
# no stray literal `c` typed into the agent (the scripted agent surviving
# and echoing the next line proves both).
#
# Test-mode only (driver-buffer probes + raw byte injection); not demo-able.
#
# shellcheck disable=SC2034,SC2317  # vars/functions are consumed by lib/harness.sh
SCENARIO_SUMMARY="OSC 52 pane copies reach the outer clipboard; Cmd+C dispatches Copy"
SCENARIO_AGENT="scripted"

# Force the deterministic clipboard route BEFORE the driver tmux server (and
# so the TUI) starts: a fake non-loopback SSH session with no display makes
# `clipboard::native_clipboard_is_remote` skip the native (arboard) path on
# every platform — without this, a macOS dev machine would write the
# developer's REAL clipboard (tests must never touch the host desktop), and
# Linux/CI would depend on whether a display server happens to exist. With
# native skipped and `$TMUX` set (the driver server), every copy lands as a
# driver-server paste buffer, hermetic and readable below.
scenario_setup() {
    export SSH_TTY=/dev/e2e-fake-tty
    export SSH_CONNECTION="10.0.0.9 11111 10.0.0.10 22"
    unset DISPLAY WAYLAND_DISPLAY
}

# Bounded poll (the sanctioned e2e_wait_pane mirror) of the driver server's
# newest paste buffer — where the TUI's `tmux load-buffer -w` copies land.
wait_driver_clipboard() {
    local want="$1" got=""
    for _ in $(seq 1 50); do
        got="$(tmux -L "$E2E_DRIVER_SOCKET" show-buffer 2>/dev/null || true)"
        [ "$got" = "$want" ] && return 0
        sleep 0.2
    done
    e2e_die "driver clipboard never became '$want' (last: '$got')"
}

# The raw kitty-keyboard-protocol bytes a terminal sends for Cmd+C
# (CSI 99;9u — codepoint 'c', modifier 1+super). Injected straight into the
# TUI's tty: crossterm parses CSI u unconditionally, so this exercises the
# exact decode → keybinding-lookup path a real forwarding terminal (e.g.
# Ghostty) drives.
send_super_c() {
    tmux -L "$E2E_DRIVER_SOCKET" send-keys -t "$E2E_DRIVER_SESSION" \
        -H 1b 5b 39 39 3b 39 75
}

scenario_steps() {
    # Every wait is `|| return 1`-guarded: this scenario exists as a
    # regression net, and an unguarded failed wait would print its error yet
    # let the remaining steps run (errexit is suppressed inside
    # `scenario_steps || return 1`) — a later green wait could then mask it.
    #
    # Title waits are anchored on the NAME'S TAIL ("clipboard (shell) "):
    # this scenario's long session name collides with the central pane's tab
    # bar, which clips the title's head, never its tail.
    step_wait_pane "SCRIPTED-READY mode=new" 60 || return 1

    # 1. The Claude Code /copy shape: tmux-passthrough-wrapped OSC 52 from
    #    the agent pane. GOT: proves the command line reached the script; the
    #    driver buffer proves the emitted escape crossed the whole chain.
    step_type "copy-wrapped:WRAPPED-CLIP-E2E"
    step_key Enter
    step_wait_pane "GOT:copy-wrapped:WRAPPED-CLIP-E2E" 15 || return 1
    wait_driver_clipboard "WRAPPED-CLIP-E2E" || return 1
    # The toast attributes the copy to the originating session.
    step_wait_pane "Copied from scripted-osc52-clipboard" 15 || return 1

    # 2. The bare (un-wrapped) OSC 52 form.
    step_type "copy-plain:PLAIN-CLIP-E2E"
    step_key Enter
    step_wait_pane "GOT:copy-plain:PLAIN-CLIP-E2E" 15 || return 1
    wait_driver_clipboard "PLAIN-CLIP-E2E" || return 1

    # 3. The shell pane is a copy source too: a real shell's printf emits the
    #    escape (the typed command itself is plain text; the shell expands
    #    the \033). Payload is pre-encoded ("SHELL-CLIP-E2E").
    step_key C-t
    step_wait_pane "clipboard (shell) " 30 || return 1
    step_type "printf '\\033]52;c;U0hFTEwtQ0xJUC1FMkU=\\007'"
    step_key Enter
    wait_driver_clipboard "SHELL-CLIP-E2E" || return 1
    step_key C-t
    step_wait_pane "clipboard (scripted)" 15 || return 1

    # 4. Cmd+C dispatches Copy: with the session list focused and no
    #    selection, Copy copies the current status message. Refresh the toast
    #    first (status expires after ~5s) with one more pane copy, then
    #    inject super+c — the driver buffer must become the toast text.
    step_type "copy-plain:TOAST-REFRESH"
    step_key Enter
    wait_driver_clipboard "TOAST-REFRESH" || return 1
    step_wait_pane "Copied from scripted-osc52-clipboard" 15 || return 1
    step_key C-h
    send_super_c
    step_wait_pane "Status message copied" 15 || return 1
    wait_driver_clipboard "Copied from scripted-osc52-clipboard" || return 1

    # 5. Cmd+C in a focused terminal with no selection is swallowed: never a
    #    SIGINT, never a literal 'c' into the agent. The scripted agent still
    #    being alive — and echoing exactly the next line, un-prefixed — proves
    #    both (a forwarded 'c' would make it "GOT:calive-…", a SIGINT would
    #    kill the read loop).
    step_key Escape
    send_super_c
    step_type "alive-after-cmd-c"
    step_key Enter
    step_wait_pane "GOT:alive-after-cmd-c" 15 || return 1
}

scenario_assert_effects() {
    # Final clipboard state is the status-copy from leg 4 — leg 5 must not
    # have copied anything.
    local got
    got="$(tmux -L "$E2E_DRIVER_SOCKET" show-buffer 2>/dev/null || true)"
    [ "$got" = "Copied from scripted-osc52-clipboard" ] \
        || e2e_die "driver clipboard changed after the swallow leg: '$got'"
}

scenario_assert_ui() {
    # Tail-anchored like the title waits above (the tab bar clips the head).
    assert_pane_contains "clipboard (scripted)"
}
