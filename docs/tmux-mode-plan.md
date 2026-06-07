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

- **Zero modifications to the user's tmux config.** Adding ccstatus must
  not require any `set-option`, `set-hook`, or `status-format` lines in
  `tmux.conf`. Removing ccstatus must leave the user's bar exactly as it
  was. The status bar should behave entirely normally when Claude isn't
  active.
- Outside tmux: behaviour unchanged from today (one-line render on stdout,
  all current blocks preserved).
- Inside tmux: ccstatus runs a long-lived daemon that holds a tmux control
  channel and drives the status bar over it. The binary has three roles:
  - **registrar**: invoked by Claude Code via `statusLine.command`, writes
    per-pane state, ensures a daemon is running for the local tmux server,
    notifies the daemon over a local socket.
  - **daemon** (`ccstatus --daemon`): long-lived process. Connects to tmux
    via `tmux -C attach -t <session>`; snapshots the user's `status`,
    `status-format[*]`, and `status-position` on start; subscribes to
    focus events; mutates the bar to inject ccstatus rows when the
    focused pane has a registered Claude session and restores the
    snapshot otherwise. Exits and restores when no Claude sessions
    remain.
  - **hook handlers** (`ccstatus --hook stop` etc.): unchanged from
    before, write per-session state.
- Cache-warmth indicator that keeps ticking when the session is idle and
  when Claude has been suspended (`Ctrl-Z`), and that disappears once
  Claude has exited.
- Daemon adapts to user reloads (`prefix + r`) — when the user changes
  their bar config, the daemon re-snapshots and the new config becomes
  the new baseline.

## Non-goals (for the first cut)

- Surfaces that lack a natural tick (notification on warm→cold edge,
  free-floating status windows). Will revisit only if we add such a surface.
- Reworking the existing single-line render or its options — tmux mode adds
  alternative emission paths, it does not replace them.
- Cross-server coordination. One daemon per tmux server.

## What already exists in this crate

- `main.rs` — entry point and `render()` (the rich ANSI statusline).
  Dispatches modes from `cli::parse_args`.
- `cli.rs` — argument parser. Current modes: default render, `--install`,
  `--hook stop`, `--render-tmux <flavor> <pane>`, `--tmux-on-focus`
  (legacy, to be retired with the daemon).
- `state.rs` — `PaneState`, `SessionState`, JSON IO under
  `/tmp/ccstatus-<uid>/pane/<server>/<pane>.json` and
  `/tmp/ccstatus-<uid>/session/<session_id>.json`. Pane state currently
  stores pre-rendered ANSI lines from the registrar.
- `hooks.rs` — `--hook stop` handler: bumps `last_turn_ts` and
  `turn_count` in session state.
- `render_tmux.rs` — current `--render-tmux line N` and `row` flavors:
  reads state, formats with tmux format strings (`#[fg=…]`), includes an
  ANSI→tmux converter for stashed lines. **Will mostly go away once the
  daemon pushes content directly via control mode.**
- `tmux.rs` — small env helpers (`server_id` from `$TMUX` socket hash)
  and the legacy `on_focus` handler (also to be retired).
- `cache.rs` — atomic write helpers, per-user cache dir.
- `install.rs`, `heatmap.rs`, `git.rs`, `api.rs`, `oauth.rs`,
  `format.rs`, `color.rs`, `term.rs` — unchanged, reused.

## Architecture

```
                                                                ┌── tmux server ──┐
Claude render → ccstatus (registrar)                            │                 │
                  writes pane state ───┐                        │   status bar    │
                  notifies daemon ─────┼────────► UNIX SOCKET ─► daemon process   │
                                       │           (notify)    │     │            │
Claude Stop hook → ccstatus            │                       │     │ control    │
                  writes session state │                       │     │ channel    │
                                       │                       │     │            │
                                       ▼                       │     ▼            │
                                STATE FILES                    │   set-option,    │
                          /tmp/ccstatus-<uid>/                 │   refresh-client │
                          (pane/, session/)                    │                  │
                                       │                       │                  │
                                       └───── daemon reads ────┘                  │
                                                                └─────────────────┘
```

The state files are still the data contract. The change is *display*: the
daemon owns the status bar via control mode rather than tmux-config
substitutions calling ccstatus back. One long-lived daemon per tmux server.
The registrar's job shrinks to "write state and ping the daemon."

## State layout

Lives under `cache::cache_dir()` (currently `/tmp/ccstatus-&lt;uid&gt;`) — survives
suspend, evaporates on reboot, which is the right lifetime for "who is
currently running" data.

```
/tmp/ccstatus-&lt;uid&gt;/
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

```
ccstatus                       Default render mode. If $TMUX is set,
                               ALSO writes pane state and pokes the
                               daemon (spawning it if absent). Either
                               way prints the rich line to stdout.

ccstatus --hook <kind>         kinds: stop. Reads hook JSON on stdin,
                               updates session/<session_id>.json.

ccstatus --daemon              The long-lived process. Connects to the
                               local tmux server via control mode,
                               snapshots and restores user state, owns
                               the bar while Claude sessions are
                               registered. Exits when none remain.

ccstatus --install             Wires statusLine.command (and Stop hook
                               with --with-hooks) into settings.json.
```

Removed (or made internal/deprecated by the daemon):

- `--render-tmux <flavor> <pane_id>` — daemon pushes directly via
  control mode; no shell-substitution renderer needed.
- `--tmux-on-focus [<pane_id>]` — daemon subscribes to focus events
  directly; no hook glue required.

## Daemon (`--daemon`)

Single process per tmux server. Lifecycle and responsibilities:

**Startup.**
1. Discover the server socket from `$TMUX`. Acquire a per-server lock
   (`/tmp/ccstatus-<uid>/server-<hash>/daemon.lock`). If another daemon
   holds it, exit silently (the registrar's poke already woke them).
2. Spawn `tmux -C attach -t <session>` as a child with piped stdin/stdout.
3. Snapshot user state: `status`, `status-position`, `status-interval`,
   and every set `status-format[N]`. Persist to disk so a crashed daemon
   can still restore on restart.
4. Open a local Unix socket
   (`/tmp/ccstatus-<uid>/server-<hash>/daemon.sock`) for the registrar
   to send notifications.

**Main loop.** Multiplex three sources with non-blocking IO:
- Control-mode notifications from tmux (focus events, exit, output).
- Registrar messages on the socket ("session X registered for pane Y",
  "session X exited").
- Self timer for periodic refreshes (cache-warmth ticks).

**State machine.** Tracks the *visible* state of the bar:
- `Idle` — no Claude pane focused, bar matches user snapshot.
- `Active(pane_id)` — bar has injected rows for `pane_id`.

Transitions:
- Focus into a pane that has registered state → `Active`. Send
  `set-option status N` and `set-option status-format[N]` for each row.
- Focus out of a Claude pane → `Idle`. Restore from snapshot.
- Same Claude pane focus tick (timer) → re-render line content if
  warmth changed.

**Reload detection.** Subscribe via `refresh-client -B` to formats that
mirror the user's bar config (e.g.
`@ccstatus-baseline:#{status}|#{status-format[0]}|…`). When tmux emits a
`%subscription-changed` notification *while we're in `Idle`*, treat the
new values as the new baseline and update the snapshot. When we're
`Active`, our own writes trigger subscriptions too; suppress those by
comparing against pending-write tracking.

**Shutdown.** Triggered by: no Claude sessions remaining for >N seconds,
SIGTERM/SIGINT, or tmux server exit. Steps: restore snapshot, write a
"clean shutdown" marker, exit.

**Crash recovery.** On startup, if a stale lockfile/snapshot exists from
a previous daemon that didn't clean shut down: still restore from the
snapshot (the user's pre-ccstatus state), then continue.

## Hook mode (`--hook`)

Unchanged. `--hook stop` reads JSON on stdin, updates session state. Will
also send a tiny notification to the daemon socket if present, so warmth
flips visually within a tick rather than waiting for the next focus event.

## Liveness / lifecycle table

| Claude state            | `kill -0` | `comm = claude` | Renderer output  |
|-------------------------|-----------|-----------------|------------------|
| Running, recent turn    | yes       | yes             | warm, ticking    |
| Running, idle > thresh  | yes       | yes             | cold, ticking    |
| Suspended (`Ctrl-Z`)    | yes       | yes             | ticks (intended) |
| Exited                  | no        | n/a             | empty            |
| Pid reused              | yes       | no              | empty            |

## Implementation order

Each milestone is a separate commit (or small commit-set) per the
global preference.

1. **Skeleton control-mode connection.** New `src/control.rs` with a
   `Connection` type that spawns `tmux -C attach`, sends commands, and
   parses `%begin/%end/%error` framing into responses. Plus
   `%event-name` notifications into a stream. No business logic yet —
   verify we can round-trip a `display-message -p '#{pane_id}'`.
2. **`--daemon` subcommand.** Owns the connection. On start: snapshot
   user state (read all relevant options), persist, log. On shutdown
   signal: restore + exit. Run for a few seconds in a `sleep` body and
   verify snapshot/restore by hand.
3. **Lock + socket.** Per-server lockfile, refuse to run concurrently.
   Unix socket for registrar pings.
4. **Registrar integration.** The default mode now: writes pane state
   (as today), then ensures the daemon is running (spawns if not),
   sends a `register <pane_id> <session_id>` line over the socket.
5. **Focus tracking + row injection.** Daemon subscribes to focus
   events; on focus into a registered pane, computes the row content
   from state files and pushes `set-option status N`,
   `set-option status-format[i] "..."`. On focus out, restores
   snapshot.
6. **Periodic refresh + warmth ticks.** Internal timer in the daemon
   re-renders the active line's warmth indicator. No more
   `status-interval` dependency for our content.
7. **Reload detection via subscription.** Subscribe to a format that
   mirrors the user's bar config; when it changes while we're `Idle`,
   update the snapshot. Distinguish user changes from our own writes.
8. **Cleanup of old code.** Drop `--render-tmux`, `--tmux-on-focus`,
   the on-disk `lines` cache. Tighten `pane_state` to what the daemon
   actually needs.
9. **Crash recovery + auto-shutdown.** Stale-lock detection; restore on
   recovery. Auto-shutdown when no Claude sessions for N seconds.

## Open risks

- **Control mode error handling.** Tmux errors come as `%error` with a
  client-side timestamp/command-num. Need to correlate to in-flight
  commands and surface them. Don't crash on parse failure of unknown
  notifications — tmux adds new ones across versions.
- **Restore correctness.** Restoring N options must be byte-identical,
  including unsetting indices we set that the user hadn't set. Use
  `set -gu` for indices that were unset in the snapshot.
- **User reloads during `Active`.** If the user runs `source-file` while
  we have rows injected, the daemon's writes and the user's writes
  race. Defer: handle by snapshotting only in `Idle`; reloads while
  `Active` are tolerated but may temporarily look wrong.
- **Multiple clients on one server.** `status` is server-global. If
  client A is focused on a Claude pane and client B on a shell, the bar
  shows Claude rows for both. Accept the limitation; document it.
- **Daemon process model.** Long-lived process started by a Claude
  subprocess: needs to detach properly (fork + setsid) so its lifetime
  isn't bound to the Claude session that spawned it.
