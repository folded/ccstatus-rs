# ccstatus bar lifecycle redesign

## Problem

The daemon drives the tmux status bar by writing **server-global** options
(`set-option -g status`, `set-option -g status-format[*]`), toggling the bar
height on pane focus. This produced several defects:

1. **Black bar with no daemon running.** `set -gu status-format[0]` does *not*
   restore tmux's built-in window-list template once the slot has been touched
   — on macOS tmux it leaves the slot empty, which renders as a blank bar over
   the user's `status-style` background. After the first activate/deactivate
   cycle the user's powerline window list was gone for good, server-wide, even
   with no daemon running. (Mitigated already by writing the default template
   back explicitly, but the global write is the root cause.)

2. **Pollution detection.** Because global options persist after a crash, the
   daemon sniffs `@ccstatus-active` and `status-format[1..5]` on startup and
   resets to defaults — a heuristic that clobbers a user's legitimate
   `status=2` config.

3. **Cross-session bleed.** `status` is a *session* option whose `-g` value is
   the server-wide default. A Claude pane focused in session A grows the bar in
   session B too, and focus tracking via `list-clients` picks an arbitrary
   client in a multi-session/multi-client setup.

4. **Reflow on focus.** Bar height is a session/client-display property.
   Toggling it on pane focus resizes every pane in the displayed window and
   fires SIGWINCH, so switching panes makes content jump.

## Verified tmux facts

- One **server** per socket; it owns all sessions/windows/panes. Commands from
  any connection reach the whole server.
- `status`, `status-format[*]`, `status-position`, `status-left`,
  `status-right`, `status-style`, `window-status-*` are **session** options.
  `set -g` writes the global default inherited by every session;
  `set -t <session>` writes a session-local override; `set -u -t <session>`
  drops the override back to inheriting the global. The global is never touched
  by per-session overrides.
- A control-mode client (`tmux -C attach`) always has a *current* session and
  **dies when that session is killed**, even if other sessions remain
  (`%session-changed` → `%exit`, no migration). There is no "global attach".
- Changing `status` height changes the pane row budget for the displayed
  window → SIGWINCH reflow.

## Architecture

**One polling daemon per tmux server.** No persistent control-mode attach.

- The registrar (statusline render in a Claude pane) writes per-pane state and
  pings the daemon over the per-server unix socket, spawning it if absent
  (unchanged transport).
- The daemon wakes on a timer (a few seconds; warmth/cache-expiry flips on a
  time threshold, so sub-second resolution is unnecessary) and on socket pings.
  Each tick it:
  - enumerates panes + their tmux sessions in one `tmux list-panes -a -F …`
    call (also detects closed panes);
  - prunes registered panes whose tmux pane is gone **or whose `claude_pid`
    has exited** — so the bar collapses when the user quits Claude even if
    the shell pane stays open ("last Claude exited", not "pane closed");
  - reconciles each session's bar against its **focused** pane (the active
    pane of its active window): the bar shows only while focus is on a
    registered Claude pane, and that pane drives the content;
  - rewrites live elements (warmth) for active sessions.
- All bar mutations are **session-local** (`set -t <session> …`), composed with
  the user's captured values, and removed with `set -u -t <session> …` on
  teardown. **The global config is never written**, so:
  - the black bar is structurally impossible (we never blank the global
    `status-format[0]`);
  - pollution detection is deleted — a crash leaves at most a session-local
    override, cleared on next startup by unsetting our own overrides.
- **Focus-driven.** A session's bar shows ccstatus only while its focused
  pane is a registered Claude pane; switching to any other pane clears it
  (reverts to just the powerline). Height = (elements routed to dedicated
  rows) + 1 powerline row. Consequence: for `row*`-routed content, focus
  changes between a Claude and non-Claude pane change the height and so
  reflow that session's panes — inherent to a per-session status height.
  Elements routed to `left`/`right` clear with **no reflow** (the powerline
  row count is unchanged), so route there to avoid reflow entirely.
- **Lifecycle.** Spawn on first registrar ping. Exit when the server is gone
  (`list-panes` fails) or no Claude panes remain after a short grace (5s).
  Bar deactivation is decoupled from process exit — a session's bar collapses
  the moment its last Claude pane goes, regardless of when the daemon exits —
  so the grace only governs respawn cost, not bar correctness.

## Routing

Each rendered piece is independently routable to one of:

- **`tmux-row-N`** — a dedicated status row (adds height);
- **`powerline-left` / `powerline-right`** — injected into the existing
  powerline row via per-session `status-left` / `status-right` overrides
  (composed with the user's captured value); zero added height, still live;
- **`claude`** — printed to stdout so Claude renders it in its own statusline
  (updates only on Claude activity — *not* live; unsuitable for warmth);
- **`off`**.

Config is a single JSON file (matching the codebase's `serde_json`-only
dependency set — no `toml` crate) read by both registrar and daemon:

```json
// ~/.config/ccstatus/config.json
{
  "route": {
    "rich": "tmux",
    "heatmap_main": "tmux",
    "heatmap_sub": "off"
  }
}
```

Phase 1 destinations are `"tmux"` (a dedicated row), `"claude"` (stdout), and
`"off"`; Phase 2 adds `"powerline-left"` / `"powerline-right"`. Outside tmux
the registrar forces every line to `"claude"` (no tmux surface exists).

**Constraint:** the live cache-expiry indicator only ticks on a
daemon-controlled surface (a tmux row, or — Phase 2 — a powerline segment).
Routed to `claude` it updates only when Claude re-renders.

## Phases

### Phase 1 — structural (done)

- Polling daemon (control mode dropped; `control.rs` removed).
- Per-session session-local overrides; restore by whole-array unset, reverting
  to the inherited global. No global write, no captured snapshot needed.
- Focus-driven per session: the bar shows only while the focused pane is a
  Claude pane.
- Pollution detection deleted; crash recovery = unset our own session
  overrides on startup, tracked via an `active-sessions` marker file.
- Routing at line granularity (the 3 existing lines) via `config.json`, read
  once at daemon startup. Outside tmux everything routes to Claude.

### Phase 2 — element granularity

Routing granularity moves from 3 lines to named **elements**:

| element | kind | source |
|---------|------|--------|
| `model`, `cwd`, `tokens`, `effort`, `limits`, `version`, `updates` | segment (inline, join with ` | `) | registrar render |
| `warmth` | segment | daemon (live; computed each tick) |
| `heatmap_main`, `heatmap_sub` | row (standalone) | registrar render |

Destinations: `off`, `claude` (stdout), `row0`/`row1`/`row2` (dedicated tmux
rows, `row0` nearest the panes), and — Phase 2b — `left`/`right` (the
powerline row's `status-left`/`status-right`, zero added height).

Composition: a surface's segment elements join with ` | ` in `Element::ALL`
order; row elements stand alone. Registrar elements are stored by name in
pane state; the daemon pulls them and computes `warmth` itself. tmux row
height = (distinct rows used) + 1 powerline row.

Sub-phases:

- **2a** (done): element decomposition; `config.json`
  element→`{off,claude,rowN}` routing; `warmth` as a live daemon element;
  pane state stores named elements. Default routing reproduces the Phase 1
  look (segments→row2, heatmap_main→row1, heatmap_sub→row0, warmth→row2).
  Removed the now-dead `--render-tmux` subprocess path.
- **2b** (done): `left`/`right` via per-session `status-left`/`status-right`
  composition. The daemon reads the user's *global* value (never the
  session-effective one, to avoid double-injection), puts the ccstatus
  segment at the screen edge with the user's value beside the window list,
  and reverts by unsetting on teardown. Zero added height.
- **2c** (done): config-file hot reload — the daemon re-reads on mtime
  change each loop and re-renders active sessions, so editing `config.json`
  re-lays-out the bar with no restart.
