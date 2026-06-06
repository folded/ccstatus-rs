# tmux mode for ccstatus

## Motivation

Claude Code's prompt cache has a ~5 min TTL. Once it expires, the next turn
incurs a re-warm cost. Today nothing in the UI signals this to the user — by
the time you send the next prompt, you have already committed to paying for a
cold read.

The native `statusLine` hook is pull-only and only fires on Claude render
events; while a session is idle it doesn't tick, so we cannot use it on its
own to surface "cache is going cold". We also can't render *inside* a pane's
content from outside the program that owns the pane.

tmux *can* tick (`status-interval`) and *can* render outside the pane (status
rows, pane borders, pane titles, OSC title escapes). Goal: teach `ccstatus`
to delegate display to tmux when it is available, so the warmth indicator
keeps updating while idle.

## Goals

- Single binary, no new daemon (for now).
- Outside tmux: behaviour unchanged from today (one-line render on stdout,
  all current blocks preserved).
- Inside tmux: ccstatus becomes a two-role tool sharing one binary:
  - **registrar**: invoked by Claude Code via `statusLine.command`, writes
    per-pane state, emits empty stdout (tmux owns the display).
  - **renderer**: invoked by tmux via `status-format` / `pane-border-format`
    / `pane-border-format` shell substitutions, reads state, formats line.
- Cache-warmth indicator that keeps ticking when the session is idle and
  when Claude has been suspended (`Ctrl-Z`), and that disappears once Claude
  has exited.

## Non-goals (for the first cut)

- A long-running daemon.
- Surfaces that lack a natural tick (notification on warm→cold edge,
  free-floating status windows). Will revisit only if we add such a surface.
- Reworking the existing single-line render or its options — tmux mode adds
  alternative emission paths, it does not replace them.

## What already exists in this crate

(For agents/humans reading this cold.)

- `main.rs` — entry point. Reads stdin, parses JSON, calls `render()`,
  prints to stdout. ~510 lines.
- `cli.rs` — argument parser, `Config` struct of feature toggles.
- `cache.rs` — `/tmp/claude` cache dir, `write_atomic`, `read_if_fresh`,
  `read_stale`, `touch`, `remove_if_empty`. Reusable for the new state
  files.
- `install.rs` — writes `statusLine.command` into `~/.claude/settings.json`
  (or `$CLAUDE_CONFIG_DIR`) preserving other keys.
- `heatmap.rs`, `git.rs`, `api.rs`, `oauth.rs`, `format.rs`, `color.rs`,
  `term.rs`, `cache.rs` — existing rendering blocks and helpers.

The Claude JSON schema the binary already consumes (see `render()` in
`main.rs`): `model.display_name`, `context_window.context_window_size`,
`context_window.current_usage.{input_tokens, cache_creation_input_tokens,
cache_read_input_tokens}`, `cwd`, `effort.level`, `rate_limits.{five_hour,
seven_day}.{used_percentage, resets_at}`.

Notably **not present in the per-render JSON**: a `session_id` field, a
`transcript_path`, the Claude Code pid. The first cut will need to verify
what `session_id` looks like in the stdin payload (Claude has added new keys
over time — there is a `session_id` at the top level in recent versions,
worth confirming on real input rather than guessing).

## Architecture

```
                                                       ┌─► tmux status row
Claude render → ccstatus (registrar mode)              │
                  writes pane state, prints "" ┐       │
                                               │       │
Claude Stop  → ccstatus (hook mode)            ▼       │
                  writes session state ────► state ────►─► pane border / pane title
                                               ▲       │
PostToolUse  → ccstatus (hook mode)            │       │
                  writes session deltas ───────┘       │
                                                       │
                                                       └─► future surfaces
```

Three writers, one reader (the renderer), arbitrarily many display surfaces.
Filesystem is the contract.

## State layout

Lives under `cache::cache_dir()` (currently `/tmp/claude`) — survives
suspend, evaporates on reboot, which is the right lifetime for "who is
currently running" data.

```
/tmp/claude/
  pane/<TMUX_PANE>.json       written by registrar mode
  session/<session_id>.json   written by hook mode
```

`pane/<TMUX_PANE>.json`:

```json
{
  "session_id": "abc123",
  "claude_pid": 12345,
  "pane_tty": "/dev/ttys003",
  "transcript_path": "/Users/tjs/.claude/projects/.../abc123.jsonl",
  "registered_at": 1733511234,
  "last_warmth": "warm"
}
```

`session/<session_id>.json`:

```json
{
  "last_turn_ts": 1733511234,
  "model": "claude-opus-4-7",
  "cost_so_far_usd": 0.42,
  "turn_count": 17,
  "context_pct_used": 17,
  "cache_read_pct": 84
}
```

Both written via `cache::write_atomic` (already does tmp+rename).
`last_warmth` exists so the renderer can detect warm↔cold transitions
without re-reading history; useful when we add edge-triggered surfaces.

## CLI surface

Extend `cli.rs`. New flags route to new code paths:

```
ccstatus                       # current behaviour: render statusline from stdin
                               # NEW: if $TMUX set, additionally register pane
                               # state and emit nothing on stdout

ccstatus --render-tmux <flavor> <pane_id>
  flavors: row | border | title
  Reads pane state for <pane_id>, prints formatted line on stdout.
  For `title`, also emits `tmux select-pane -T` so window-title chain works.

ccstatus --hook <kind>
  kinds: stop | post-tool-use
  Reads hook JSON on stdin, updates session/<session_id>.json.

ccstatus --install
  As today, plus optional `--with-hooks` to also write the Stop /
  PostToolUse hook entries into settings.json.
```

A single binary keeps install/distribution simple. Mode dispatch in
`main.rs` happens after `cli::parse_args` but before `render()`.

## Mode 1 — registrar (default mode, augmented)

Today: read stdin JSON → `render()` → stdout.

New behaviour, only when `$TMUX` is set:

1. Resolve Claude pid. `$PPID` is the script's parent; if that doesn't have
   `comm == claude`, walk up `ps -o ppid= -p <pid>` until it does. Cache the
   walk in `pane_state` so we don't redo it every render. Fall back to
   `$PPID` if the walk fails.
2. Capture `$TMUX_PANE` (e.g. `%5`) and the pane's tty via
   `tmux display -t $TMUX_PANE -p '#{pane_tty}'`.
3. Pull `session_id` from the stdin JSON (verify the key path on a real
   payload — likely `/session_id` at the top level).
4. Atomically write `pane/<TMUX_PANE>.json`. Rate-limit: skip the write if
   the existing file is <500 ms old and the session_id/pid match (statusline
   can fire on every streamed chunk; we don't want a write storm).
5. Emit empty string on stdout (tmux owns the visible row), exit 0.

When `$TMUX` is unset, fall through to the existing `render()` path
unchanged.

Open question: should the registrar *also* paint iTerm surfaces (title,
badge) when running outside tmux but inside iTerm directly? Probably yes,
but defer to Phase 3 — outside-tmux behaviour stays identical until then.

## Mode 2 — renderer (`--render-tmux`)

Invoked by tmux with a flavor and a pane id; reads state and prints. Must
be fast (we will run it at 1–2 s interval × N panes). Target <5 ms wall.

1. Read `pane/<pane_id>.json`. Missing → exit 0 with empty output.
2. **Liveness:** `kill -0 claude_pid`. Failure → empty output, optionally
   `unlink` the stale pane file. Success → continue.
3. **Pid-reuse guard:** `ps -p <pid> -o comm=`; if it doesn't match
   `claude` (or `claude` is not the last path segment), treat as dead. Same
   handling.
4. **Suspended state:** `ps -o state=` returning `T`/`Tsl` means the process
   is stopped. We still render — `idle = now - last_turn_ts` keeps ticking,
   which is the honest answer to "what will happen if I resume and submit".
5. Read `session/<session_id>.json`. Compute:
   - `idle = now - last_turn_ts`
   - `warmth` band: `warm` if `idle < 270 s`, else `cold` (single threshold
     for now, single config knob later).
6. Format for flavor:
   - `row`: full one-liner, can reuse format helpers from `format.rs` and
     `color.rs`. Coloured `warm`/`cold` plus `idle` (mm:ss).
   - `border`: shorter, single-cell-friendly variant.
   - `title`: plain text, no ANSI. After printing to stdout, also exec
     `tmux select-pane -t <pane_id> -T "<title>"` so the user's `set-titles`
     config can propagate it into the window title.
7. Optionally update `pane.last_warmth` if it changed (atomic write). This
   gives a future notifier something to diff against.

## Hook mode (`--hook`)

`Stop` is load-bearing; everything else is icing.

`--hook stop`:
1. Read stdin JSON. Pull `session_id` and `transcript_path` (verify keys on
   real payload).
2. Update `session/<session_id>.json`: set `last_turn_ts = now`, refresh
   `model` and `turn_count` if present.
3. Exit 0 immediately. Never block.

`--hook post-tool-use` (Phase 2): bump `turn_count`, increment cost if
hook provides it.

Hooks should *never* fail loudly. If state directory is unwritable, log to
stderr (Claude swallows it in the transcript) and exit 0.

## tmux configuration (in dotfiles, separate commit)

```tmux
set -g status 2
set -g status-interval 2
set -g status-format[1] '#(ccstatus --render-tmux row #{pane_id})'

# pane border surface (optional)
set -g pane-border-status bottom
set -g pane-border-format '#(ccstatus --render-tmux border #{pane_id})'

# pane title → window title (optional)
set -g set-titles on
set -g set-titles-string '#{pane_title}'
# pane title is set by the renderer via `tmux select-pane -T` on each tick.
```

Note: `#{pane_id}` is substituted before tmux's shell-substitution cache,
so each pane gets its own cache entry. Focused-pane switching just works.

## Liveness / lifecycle table

| Claude state            | `kill -0` | `comm = claude` | Renderer output  |
|-------------------------|-----------|-----------------|------------------|
| Running, recent turn    | yes       | yes             | warm, ticking    |
| Running, idle > thresh  | yes       | yes             | cold, ticking    |
| Suspended (`Ctrl-Z`)    | yes       | yes             | ticks (intended) |
| Exited                  | no        | n/a             | empty            |
| Pid reused              | yes       | no              | empty            |

## Implementation order

Each step is a separate commit per global preferences.

1. **`state.rs` module:** define `PaneState`, `SessionState`, IO helpers
   on top of `cache::write_atomic` + a `read_json`. No behaviour change yet.
2. **Registrar branch in `main.rs`:** when `$TMUX` set, write pane state
   and print nothing; otherwise existing `render()` path. Add rate-limit
   on writes.
3. **`--hook stop` mode:** new subcommand, updates session state. Wire it
   into `--install --with-hooks`.
4. **`--render-tmux row` mode:** basic ANSI output (`warm`/`cold` + idle).
   First user-visible win.
5. **Liveness check (`kill -0` + `ps -o comm=`):** robustness.
6. **`--render-tmux border` and `--render-tmux title`:** add the other
   surfaces. `title` flavor also calls `tmux select-pane -T`.
7. **`--hook post-tool-use`:** richer fields (turn count, cost if
   available).
8. **Settings install ergonomics:** confirm install behaviour adds/removes
   hooks idempotently; tests for the JSON merging.
9. **Stretch / deferred:** iTerm badge / user var via OSC 1337 (needs
   `allow-passthrough on`); macOS notification on warm→cold edge; daemon
   for surfaces without a tick.

## Decisions to make before coding

- **`session_id` location in stdin JSON.** Check real payloads from
  current Claude Code; the registrar relies on this and a fallback strategy
  (e.g. derive from transcript_path) may be needed.
- **Where to store pane/session state.** Reusing `/tmp/claude` (current
  `cache_dir`) is the obvious answer; if there's any chance of multiple
  Claude processes on multi-user machines, namespace by `$UID`. For a
  single-user laptop, this doesn't matter.
- **Threshold for warm/cold.** Default 270 s (under documented 5 min TTL
  with margin). Plumb as `STATUSLINE_CACHE_WARMTH_SECS` env var and a CLI
  flag.
- **Cost source.** `cost_so_far_usd` is wishful unless a hook payload
  provides it. Confirm what `Stop` and `PostToolUse` actually deliver
  before committing to the field.
- **Existing `cache::cache_dir()` returns `/tmp/claude`.** State files
  share that root. Fine, but worth a docstring on each new file path.

## Open risks

- **Statusline call frequency.** Claude Code may invoke the statusline on
  every streamed chunk; the registrar must not write storm. Mitigation:
  500 ms write coalescing.
- **`ps` portability.** macOS and Linux both have `ps -p <pid> -o comm=`
  but the output format differs slightly (trailing whitespace, full path).
  Trim aggressively; match by suffix not exact string.
- **Pane id stability across tmux server restart.** Pane ids reset; stale
  state files in `pane/` survive and fail the liveness check on next
  read. Acceptable.
- **iTerm passthrough complexity (Phase 3).** Defer until needed; the
  pane-title route gives us most of the title-surface value without it.

## File-level plan summary

New files:

- `src/state.rs` — PaneState/SessionState structs + IO
- `src/tmux.rs` — tmux helpers (`display -p`, `select-pane -T`, env reads)
- `src/hooks.rs` — `--hook stop`, `--hook post-tool-use` handlers
- `src/render_tmux.rs` — `--render-tmux row|border|title` handlers

Touched:

- `src/main.rs` — dispatch new modes before falling through to `render()`
- `src/cli.rs` — new flags (`--render-tmux`, `--hook`, `--with-hooks`)
- `src/install.rs` — optional hook-block insertion in settings.json

Untouched (by design):

- `src/heatmap.rs`, `src/git.rs`, `src/api.rs`, `src/oauth.rs`,
  `src/format.rs`, `src/color.rs`, `src/term.rs`, `src/cache.rs`
