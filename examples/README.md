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
- **Outside tmux**, every element falls back to Claude's statusline
  regardless of this file.
- Your base status row (the window list) and your `status-left`/`status-right`
  theme are never overwritten — ccstatus composes alongside them per session
  and reverts cleanly when Claude exits.

## Elements

`model`, `cwd`, `tokens`, `effort`, `limits`, `version`, `updates`,
`warmth`, `heatmap_main`, `heatmap_sub`.
