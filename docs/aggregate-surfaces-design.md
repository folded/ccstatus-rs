# Aggregate surfaces & "take me to this Claude"

Status: first slice shipped (`ccstatus top` + jump, plus the working/waiting
state it needed); the rest is a roadmap. The shipped architecture lives in
`CONTEXT.md` (`top`, `fleet`, `jump`); this doc keeps the motivation and the
unbuilt surfaces.

## Motivation

ccstatus today renders one tmux bar per tmux session, driven by the focused
pane. But a heavy user runs several Claude sessions at once and can't answer
the questions that actually matter across them:

- Which of my sessions is **waiting for my input** right now?
- Am I about to hit my **5h / 7d limit**? (account-global, identical in every
  session — wasteful to repeat per-pane)
- How much have I **burned today** across everything?
- **Take me to** the session I care about, wherever it lives.

The daemon model already maintains the substrate to answer these. This doc
names the architecture and scopes the first slice.

## The substrate: a read model over the state dir

The registrar and hooks already persist a shared on-disk contract under
`/tmp/ccstatus-<uid>/` (see `state.rs`):

```
pane/<server_id>/<pane>.json   session_id, claude_pid, pane_tty,
                               transcript_path, registered_at, last_warmth,
                               elements{model,cwd,tokens,limits,version,…}
session/<session_id>.json      last_turn_ts, model, turn_count,
                               context_pct_used, cache_read_pct
```

Any process can enumerate these and reconstruct a complete cross-session
picture **without touching tmux**. That is the aggregation substrate, and it
exists today. Disk state is *last-known render — may be stale, display-only.*

## Three scopes of data

The information has natural homes, and aggregation only makes sense per scope:

- **Account-global** — `limits` (5h/7d utilization, reset times, extra-usage
  credits), update availability, CLI version. Identical across every session
  (sourced from the OAuth usage API / the binary, not the conversation). Wants
  **one** always-visible home, not per-pane repetition.
- **Session-local** — `model`, `cwd`, context `%`, `warmth`, `turn_count`.
  Only meaningful next to its pane.
- **Aggregatable** — derived by folding across sessions: total tokens today,
  active-session count, warm vs cold, most-recent turn, **who's waiting for
  you**. Only meaningful in a surface that sees all sessions.

## Three driver models (what tells a surface to update)

1. **Render-driven, pane-local** — the registrar already runs on every render
   *inside the focused pane*. It can emit a terminal/tab-title or badge escape
   with zero daemon involvement; it *is* the focused render. (Cheapest new
   surface; e.g. tab title "Opus · 34% · warm".)
2. **Focus-driven, session-scoped** — today's handler (control connection →
   focus events → tmux bar). Mirroring "the focused session" onto a non-tmux
   target lives here and needs a non-tmux focus/event source. **Expensive;
   deferred.**
3. **Poll/event-driven, global aggregate** — a *new consumer* that reads the
   whole state dir and shows everything or a summary. **Sidesteps focus and
   lifecycle entirely** — it doesn't care which pane is focused. This is where
   aggregation lives, and it's cheap precisely because it skips the hard half.

Key insight: aggregation is the **cheap, high-value** direction. It's a
pull-based read model — no `Surface` trait, no event-source abstraction, no
per-surface lifecycle.

## Surfaces (roadmap)

Aggregate (model 3):
- **`ccstatus top` / `sessions`** — a TUI/CLI: per-session table (model, cwd,
  ctx%, warm/cold, last-turn age, tokens today) + an account-usage header.
  "htop for Claude sessions." **First slice.**
- **macOS menubar item** (NSStatusItem, or an xbar/SwiftBar plugin shelling to
  `ccstatus`) — always visible; account usage + aggregate count
  ("◐ 3 Claude · 1 waiting · 5h 62%").
- **Linux bar modules** (waybar/polybar/i3status) — same "emit a line" contract
  as the menubar.
- **OS notifications** — transient: "Claude is waiting", "5h at 90%", "weekly
  reset". Off the existing Stop hook + a usage threshold.
- **Dock / iTerm badge** — just the "# waiting" integer.

Session/pane-local (models 1 & 2):
- **Tab/window title + iTerm badge** (registrar-emitted) — nearly free.
- **tmux window-status / pane-border** — mark which window has an
  active/waiting Claude.
- **Focused-session mirror onto iTerm title** (model 2) — the expensive one;
  needs the event-source abstraction. Deferred.

## "Take me to this Claude" (jump)

Jump turns an aggregate surface from a read-only display into an **actuator**:
select a session → focus its tmux window + pane, wherever it is.

### The daemon is the live addressing authority — no disk schema change

Ephemeral addressing (the tmux socket) must **not** be persisted to disk: it
would outlive the server it names. The live daemon already holds everything
needed, bounded by exactly the right lifetime:

- **Liveness is intrinsic.** A handler holds an `flock` for its lifetime and
  binds `handler<sess>.sock` (see `server_dir.rs`). Daemon alive ⇔ flock held
  ⇔ socket connectable ⇔ session jumpable. You reach the authority by
  *connecting to it*, not by reading a path it left behind — nothing stale to
  misinterpret.
- **It's in the right server's environment.** The daemon attaches with
  `tmux -C attach -t <session>` and **no `-S`**, relying on inherited `$TMUX`.
  Every `tmux` command it issues already targets the correct server — exactly
  what a persisted socket path was trying to reconstruct.
- **It already tracks its panes** (`Handler::panes`) and owns a `Tmux` seam to
  act through.

So disk state stays display-only/may-be-stale; addressing + actuation live
with the daemon.

### Mechanism

- New `Tmux::focus_pane(&self, pane)` — `select-pane` + `select-window`
  (+ `switch-client`). `CliTmux` actuates; `FakeTmux` records. (Reuses the
  seam from the tmux-seam refactor.)
- New IPC verb (the protocol is one line today, `register <pane>`):
  ```
  focus <pane_id>
  ```
  The daemon dispatches it to `focus_pane`.
- **Routing the jump from `ccstatus top`:** the target pane's `server_id` is
  already in its state-file path. Enumerate `*.sock` under that server dir and
  send `focus %5`; pane ids are **server-unique**, so exactly one daemon owns
  it and the rest no-op. No tmux-session id or socket path on disk.
- **Same-server shortcut:** if the TUI runs inside tmux and `tmux::server_id()`
  matches the target's, it runs the commands through its own `CliTmux` — no IPC
  at all. The daemon route is only needed for *cross-server* jumps, which is
  exactly where the daemon's inherited `$TMUX` is essential.
- **Graceful failure:** a session with no live daemon → connection refused →
  rendered "not jumpable." Correct by construction.

### Layer 3 (deferred): raise the OS terminal window

Within tmux, `focus_pane` is enough. Bringing the *hosting terminal emulator's*
OS window to the foreground (needed for a menubar jump, not a TUI-internal one)
is per-emulator: iTerm2 Python/AppleScript + addressable session GUIDs,
Terminal.app AppleScript, kitty/wezterm/Ghostty remote control. `pane_tty` is
the thread to pull (tty → OS window). Best-effort, pluggable, out of MVP.

## Data-model gaps (not blockers, but wanted)

- **Working vs. waiting-for-input** — *shipped.* The UserPromptSubmit hook
  marks "working" on turn start and the Stop hook marks "waiting" on turn end;
  `ccstatus top` folds these into a `working | waiting | idle` activity column
  (see `fleet::Activity`). This was the killer aggregate signal called out
  above.
- **Cost / cumulative tokens** — per-render context counts exist; no per-session
  running total. `transcript_path` is in pane state, so it's derivable.

## Consequence to track: presence vs. bar routing

Handlers today only spawn when `routing.any_tmux()` (something is routed to a
tmux surface). A user who routes everything to `claude`/`off` has no daemon →
not jumpable, and (for liveness via daemon) potentially under-represented.
Guaranteeing jumpability for every session would mean spawning a lightweight
"presence" handler regardless of bar routing — a policy decision, not an MVP
blocker, but the one place "TUI sees it" and "TUI can jump to it" can diverge.

## First slice (shipped)

1. **Read-model module** (`fleet.rs`): enumerate the state dir →
   `Vec<SessionView>` + an account-usage summary. Pure over a filesystem read
   (`build_views`), testable with fixture dirs.
2. **`ccstatus top` TUI** on top of it (`top.rs`): live table + account header.
3. **Jump:** `Tmux::focus_pane`, the `focus` IPC verb, handler dispatch, and the
   TUI keybinding (same-server direct; cross-server via the handler).

Follow-ups still open: menubar/bar emitter (reuse the read model),
notifications, title/badge surfaces, cost rollup, presence handler.
