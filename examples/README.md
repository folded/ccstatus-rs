# Example ccstatus config

Copy [`config.json`](./config.json) to `~/.config/ccstatus/config.json`
(or `$XDG_CONFIG_HOME/ccstatus/config.json`). The daemon hot-reloads on
save — no restart needed.

## What this example does

It spreads the status elements across every destination kind:

```
route:
  model         -> left      base-row status-left (pinned far left)
  warmth         -> right    base-row status-right (live warm/cold pip)
  heatmap_main  -> row0      dedicated row, nearest the panes
  tokens        -> row1      dedicated row, with limits beside it
  limits        -> row1      (shares row1 with tokens, joined by " | ")
  cwd           -> claude    Claude's own statusline
  effort        -> claude    Claude's own statusline
  version       -> claude    Claude's own statusline
  heatmap_sub   -> off
  updates       -> off
```

Resulting tmux bar while a Claude pane is focused (status at the bottom):

```
┌───────────────────────── pane(s) ─────────────────────────┐
│                                                            │
└────────────────────────────────────────────────────────────┘
 <heatmap_main>                                                  row0  (nearest panes)
 6k/200k (3% · cache 83%) | 5h 12% @14:30 | 7d 40%               row1
 Opus | <your status-left>  <window list>  <your status-right> | warm   base row
```

and in Claude's own statusline (above the prompt):

```
~/demo@main (+3 -1) | effort: high | v2.1.0
```

## Destinations

| value     | surface                                            | height | live? |
|-----------|----------------------------------------------------|--------|-------|
| `row0`…`rowN` | a dedicated tmux status row (`row0` nearest panes) | adds 1 row | yes |
| `left`    | base row `status-left`, composed with yours        | none   | yes |
| `right`   | base row `status-right`, composed with yours       | none   | yes |
| `claude`  | Claude's own statusline (stdout)                   | n/a    | no — updates only when Claude re-renders |
| `off`     | hidden                                             | —      | — |

Elements on the same row/side join with ` | ` in this fixed order:
`model, cwd, tokens, effort, limits, version, updates, warmth`. The
heatmap elements are full-width rows.

### Claude statusline layout (`claude.<line>.<align>`)

`claude` accepts an optional printed line and horizontal alignment, so the
statusline Claude renders above its prompt can be more than one left-packed
line:

- `claude` — first line, left (the default).
- `claude.right` — first line, right-aligned (padded out to the terminal width).
- `claude.1` — second printed line, left.
- `claude.1.right` — second line, right-aligned.

Tokens and line may appear in either order (`claude.1.right` == `claude.right.1`).
Left- and right-aligned groups on the same line are padded apart; heatmap
elements always take their own full-width line. This layout applies both inside
and outside tmux.

### Background (`background`)

A top-level `"background"` hex colour paints the surfaces ccstatus owns so the
bar reads consistently regardless of the terminal or tmux theme:

```json
{ "background": "#1a1b26", "route": { "model": "row2" } }
```

Inside tmux it is applied via the session's `status-style` (preserving your
foreground/attributes) and reverts when Claude exits; outside tmux each printed
line is filled to the full width with the colour. The base-row *edges*
(`left`/`right`) share your own status bar, so they can't be repainted —
dedicated rows and Claude's lines can.

## Notes

- **The bar shows only while the focused pane is running Claude.** Switching
  to any other pane in the session clears the ccstatus content. For
  `left`/`right`-routed elements this is reflow-free; for `row*` elements the
  status height changes, so that session's panes reflow on the switch — route
  to `left`/`right` if you want zero reflow.
- **`warmth` only ticks live on a tmux surface** (`row*`, `left`, `right`).
  Routed to `claude` it can't update on its own, so it's best on `right` or
  a row.
- **No config file** = the default layout (rich line + heatmap rows on
  dedicated rows, the base row below).
- **Outside tmux**, the tmux-only destinations (`row*`/`left`/`right`) have no
  surface, so those elements fall back to Claude's first line; explicit
  `claude.<line>.<align>` and `off` choices are still honored. Route to
  `claude.right` for right alignment without tmux.
- Your base status row (the window list) and your `status-left`/`status-right`
  theme are never overwritten — ccstatus composes alongside them per session
  and reverts cleanly when Claude exits.

## Elements

`model`, `cwd`, `tokens`, `effort`, `limits`, `version`, `updates`,
`warmth`, `heatmap_main`, `heatmap_sub`.

## Jumping to a non-tmux session (Linux)

Pressing Enter in `ccstatus top` jumps to the selected session. Inside tmux it
switches panes; for a Claude running directly in a terminal emulator it raises
that emulator's OS window.

On Linux the actuator is a **jump command** that maps the Claude pid (or an
ancestor — the emulator) to its window. With nothing configured, ccstatus runs
a bundled best-effort **X11** script ([`jump-linux.sh`](./jump-linux.sh)) using
`wmctrl` or `xdotool` — install one of those and most X11 desktops (GNOME, KDE,
XFCE, i3) just work.

Wayland has no portable activate-by-pid protocol, so point `jump.linux` at your
own command. It receives the Claude pid as `$CCSTATUS_CLAUDE_PID` (and as `$1`);
resolve the emulator pid from the ancestry as the bundled script does:

```json
{
  "jump": {
    "linux": "swaymsg \"[pid=$(ps -o ppid= -p $CCSTATUS_CLAUDE_PID)] focus\""
  }
}
```

A session is only shown as jumpable when it has a graphical display, so a
headless/SSH Claude stays non-jumpable. Jumps are best-effort: if no tool or
matching window is found, the jump simply does nothing.
