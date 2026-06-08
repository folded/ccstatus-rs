#!/bin/sh
# ccstatus: raise the terminal window hosting a non-tmux Claude session.
#
# This is the default "jump" actuator for Linux (Layer 3 of "take me to this
# Claude"). ccstatus pipes this script to `sh` and passes the Claude process
# id as the first argument (also as $CCSTATUS_CLAUDE_PID). The terminal
# emulator is an *ancestor* of that process, so we look for a window owned by
# the pid or one of its ancestors and activate it.
#
# Covers X11 (EWMH) via wmctrl or xdotool -- i.e. most desktops on X11,
# including GNOME, KDE, XFCE and i3. Install one of those tools to get jumps.
#
# Wayland has no generic "activate window by pid" protocol, so there is no
# portable default. If you run a Wayland compositor, point `jump.linux` in
# ~/.config/ccstatus/config.json at your own command, e.g. (resolve the
# emulator pid from the ancestry first, as below):
#   Sway:     swaymsg "[pid=<emulator-pid>] focus"
#   Hyprland: hyprctl dispatch focuswindow "pid:<emulator-pid>"
#
# Best-effort throughout: exits non-zero (ccstatus then reports the jump
# failed) when no supported tool or matching window is found.

set -u

pid="${1:-${CCSTATUS_CLAUDE_PID:-}}"
[ -n "$pid" ] || exit 1

# Print the pid and each of its ancestors, one per line. The terminal emulator
# is somewhere up this chain; the WM knows windows by the emulator's pid.
ancestry() {
	p=$1
	while [ "${p:-0}" -gt 1 ] 2>/dev/null; do
		printf '%s\n' "$p"
		p=$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ')
		[ -n "$p" ] || break
	done
}

procs=$(ancestry "$pid")
[ -n "$procs" ] || exit 1

if command -v wmctrl >/dev/null 2>&1; then
	# `wmctrl -lp` columns: <window-id> <desktop> <pid> <host> <title>
	list=$(wmctrl -lp 2>/dev/null || true)
	for ap in $procs; do
		win=$(printf '%s\n' "$list" | awk -v p="$ap" '$3 == p { print $1; exit }')
		if [ -n "$win" ] && wmctrl -ia "$win" 2>/dev/null; then
			exit 0
		fi
	done
fi

if command -v xdotool >/dev/null 2>&1; then
	for ap in $procs; do
		win=$(xdotool search --pid "$ap" 2>/dev/null | head -n1)
		if [ -n "$win" ] && xdotool windowactivate "$win" 2>/dev/null; then
			exit 0
		fi
	done
fi

exit 1
