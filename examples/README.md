# Example ccstatus config

Copy [`config.json`](./config.json) to `~/.config/ccstatus/config.json`
(or `$XDG_CONFIG_HOME/ccstatus/config.json`). The daemon hot-reloads on
save — no restart needed.

## Structure

The config is keyed by **layout**, chosen at runtime: `tmux` when Claude Code
runs inside tmux, `default` otherwise. Each layout holds **surfaces**, and each
surface maps a **region** to an ordered, comma-separated element list:

```
<layout>:                 tmux | default
  <surface>:              claude | tmux
    background: "#hex"     (optional, per surface)
    <side>[.<line>]: "el, el, …"
```

- **Surfaces.** `claude` is Claude's own statusline (the lines above the
  prompt), available in every layout. `tmux` is the daemon-driven status bar,
  only in the `tmux` layout.
- **Regions.** `left`/`right` plus an optional line index: `left`, `right`,
  `left.1`, `right.2`, …. A bare side is line 0. On the `claude` surface a line
  is a printed line; on the `tmux` surface line 0 is the base status row (its
  `status-left`/`status-right` edges) and lines ≥1 are dedicated rows.
- **Order.** The element list is rendered in the order you write it.
- **Off.** An element you don't list anywhere is hidden — there's no `"off"`.
- **Conflicts.** If an element appears on both surfaces of a layout, the
  `claude` surface wins.

## What this example does

Inside tmux (status at the bottom):

```
┌───────────────────────── pane(s) ─────────────────────────┐
└────────────────────────────────────────────────────────────┘
 <heatmap_main>                                              tmux left.1
 44k/200k (22% · cache 90%) | 5h 12% | 7d 40%                tmux left.2
 model | <your status-left>  <window list>  <…right> | warm  base row (line 0)
```

and on Claude's own statusline (above the prompt):

```
~/demo@main (+3 -1) | effort: high                       v2.1.0
```

Outside tmux, the `default` layout puts everything on Claude's statusline:

```
Opus 4.8 | demo@main (+3 -1) | effort: high              v2.1.0
44k/200k (22% · cache 90%) | 5h 12% | 7d 40%
<heatmap_main>
```

## Background

`background` is a reserved per-surface key (`#rrggbb`) so the bar reads
consistently regardless of the terminal or tmux theme:

```json
{ "tmux": { "tmux": { "background": "#1a1b26", "left": "model" } } }
```

On the `tmux` surface it's applied via the session's `status-style` (preserving
your foreground/attributes) and reverts when Claude exits. On the `claude`
surface each printed line is filled to the full width with the colour. The
base-row *edges* (`left`/`right` on line 0) share your own status bar, so they
can't be repainted — dedicated rows and Claude's lines can.

## Window flag (activity in the tmux window name)

`windowFlag` is an opt-in block (sibling to the layouts) that stamps each Claude
pane's **tmux window name** with a per-activity marker — the across-tab "which
one needs me?" cue, visible in the whole window strip even for unfocused panes
(unlike the status bar, which only shows the focused session):

```json
{ "windowFlag": { "enabled": true,
                  "markers": { "needsInput": "● ", "working": "◐ " } } }
```

- A `format` template controls the layout (default `"{claude} {dir} {git}"`).
  Tokens: `{claude}` (activity marker), `{dir}` (cwd basename), `{git}`
  (git-state glyph), `{branch}` (git branch). An empty token contributes
  nothing and the result is trimmed, so absent pieces leave no stray spaces —
  e.g. an idle, clean repo is just `pubmedifier`, a working one with unpushed
  commits is `◐ ccstatus-rs ↑`. Reorder or drop tokens freely
  (`"{git} {dir}@{branch}"`, `"{claude}{dir}"`, …).
- `markers` set the `{claude}` glyph per activity state — the same states
  `ccstatus top` uses: `needsInput`, `working`, `bgRunning`, `suspended`,
  `waiting`, `idle`, `unknown` (the last three default to empty). Plus `done`
  (default `⚑`): the **attention flag** shown in the `{claude}` slot when a
  session finishes a turn (or its background tasks drain) **while you're not
  looking at it**. It clears when you focus the pane; for a non-tmux Claude
  (which we can't detect focus for) it clears on your next prompt. `top` floats
  these rows just under `needsInput` and marks them with the same `⚑`.
- `git` sets the `{git}` glyph: `ahead` (`↑`), `behind` (`↓`), `diverged` (`↕`),
  `dirty` (`⚠`), `clean` (empty). A dirty tree takes precedence over the arrows.
  **Ahead/behind are local-only** — read from the last-fetched remote-tracking
  ref, with no network access — so `behind`/`diverged` are only as current as
  your last `git fetch`. Computed from one `git status` per pane on the tick.
- **Presence of the block opts in.** Set `"enabled": false` to keep custom
  markers configured but dormant.
- Owned by the per-session daemon and updated on its tick, so a state change
  (e.g. a turn finishing) shows within a few seconds. The flag is removed —
  tmux's own command-based naming resumes — when Claude exits or the flag is
  disabled.
- **Requires `set -g allow-rename off` in your tmux config**, otherwise Claude
  Code's own window rename (its version) clobbers the flag. The OSC 2 pane title
  (`#T`, Claude's task summary) is a separate channel and is unaffected.

## Ghostty (activity in the tab title, no tmux)

`ghostty` is an opt-in block (sibling to the layouts) for Claude sessions
running **directly in [Ghostty](https://ghostty.org)** — no tmux. Ghostty has
no status bar, but its tab titles are settable via escape codes, so a small
handler stamps each Claude surface's **tab title** with the same label the
tmux window flag uses:

```json
{ "ghostty": { "title": true, "progress": true },
  "windowFlag": { "format": "{claude} {dir} {git}" } }
```

- **Presence of the block opts in** (set `"enabled": false` to disable). The
  label's template and glyphs come from the `windowFlag` block above — only
  `windowFlag.enabled` is ignored here (that gates tmux window naming).
- `progress` (Ghostty ≥ 1.2) drives Ghostty's **native progress bar** — the
  thin bar at the top of the surface — as a live session indicator: a
  **cache-warmth countdown** while the session waits (full right after a
  turn, draining to empty exactly when the warmth indicator would flip
  cold), an **indeterminate pulse** while a turn or background task runs,
  and a **red bar** when Claude is blocked on you (permission prompt /
  suspended). Ghostty auto-clears the bar ~15 s after updates stop, so a
  killed handler leaves clean surfaces.
- Titles in Ghostty are last-writer-wins (Claude Code and shell prompts also
  set them), so the handler re-asserts the title every ~3 s; expect the other
  writers' titles to flash through briefly on prompt redraws.
- **Does not work if you set `title` in your Ghostty config** — that makes
  Ghostty ignore all escape-code titles.
- The title is handed back to Ghostty's own naming when Claude exits or the
  block is disabled. The handler (`ccstatus --ghostty-daemon`) is spawned
  automatically on the first statusline render inside Ghostty and exits when
  the last Claude session does. After a rebuild: `pkill -f 'ccstatus
  --ghostty-daemon'` (it respawns from the new binary).
- The attention `⚑` clears on your next prompt (surface focus isn't
  detectable from outside Ghostty), and Ghostty's command-palette session
  search indexes these titles — searching `⚑` finds sessions awaiting you.

## Notes

- **The bar shows only while the focused pane is running Claude.** Switching to
  another pane clears the ccstatus content. Base-row edges (line 0) are
  reflow-free; dedicated rows (lines ≥1) change the status height, so the
  session's panes reflow on the switch — keep elements on line 0 if you want
  zero reflow.
- **`warmth` ticks live on the tmux surface**, driven by the daemon. On the
  `claude` surface it is recomputed on each statusline run, so it ticks
  warm→cold only as often as Claude re-renders — `--install` sets
  `statusLine.refreshInterval` (60s) so it still flips while the session is
  idle.
- **No matching layout in the file** = a sensible built-in (rich row + heatmap
  rows in tmux; everything on Claude's statusline otherwise).
- Your base status row (the window list) and your `status-left`/`status-right`
  theme are never overwritten — ccstatus composes alongside them per session
  and reverts cleanly when Claude exits.

## Elements

Use these names in a region's element list.

| element | shows |
|---------|-------|
| `model` | the (shortened) model display name, e.g. `Opus 4.8` |
| `cwd` | current directory basename, with git branch and `+added -deleted` counts |
| `tokens` | context usage: `used/total (pct% · cache hit% · sub-agent%)` |
| `effort` | reasoning effort level (`low`/`med`/`high`/`xhigh`/`max`) |
| `limits` | rate-limit windows (5h / 7d usage % and reset times) |
| `version` | installed Claude CLI version |
| `updates` | a notice when a newer ccstatus release is available |
| `warmth` | prompt-cache warm/cold pip — **live** (see the note above) |
| `heatmap_main` | activity heatmap of main-agent token usage — **full-width row** |
| `heatmap_sub` | activity heatmap of sub-agent token usage — **full-width row** |

The two heatmaps are full-width: they ignore `left`/`right` and take the whole
line they're placed on. Everything else is an inline segment that joins its
region's siblings with ` | `.

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
