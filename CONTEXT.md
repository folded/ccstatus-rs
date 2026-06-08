# ccstatus domain glossary

Shared vocabulary for the codebase. Architecture terms (module, interface,
seam, adapter, depth, leverage, locality) follow their usual meaning; the
nouns below are specific to ccstatus.

## Roles (the binary wears one per invocation)

- **registrar** — the default render mode (`main.rs`). Invoked by Claude
  Code's `statusLine.command` on every render. Renders the **elements**,
  writes per-pane **state**, and pings the **handler**. Outside tmux it just
  prints to stdout.
- **handler** — `ccstatus --session <id>` (`daemon.rs`). One per tmux
  **session** that hosts Claude. Holds a control-mode connection to that
  session and drives the bar. Spawned on demand by the registrar; exits on
  `%exit` (session killed) or after an idle grace with no Claude panes.
- **hook** — `ccstatus --hook stop` (`hooks.rs`). Updates per-session
  **state** (last turn timestamp, model) on Claude's Stop event.
- **top** — `ccstatus top` (`top.rs`). An interactive aggregate surface over
  **all** live Claude sessions (not one tmux session): reads the **fleet**,
  renders a table, and **jumps** to a selected session. Pull-based — it needs
  no focus events, just the on-disk **state**.

## Concepts

- **element** — a named, independently routable piece of the statusline
  (`model`, `cwd`, `tokens`, `effort`, `limits`, `version`, `updates`,
  `warmth`, `heatmap_main`, `heatmap_sub`). See `config::Element`.
- **surface** — where an element can land: a dedicated tmux **row**, a
  **base-row edge** (the user's existing status row's `status-left` /
  `status-right`), Claude's own statusline (stdout), or `off`. See
  `config::Dest`.
- **routing** — the element→surface map, a single JSON file read by both
  registrar and handler so they agree. See `config::Routing`.
- **reconcile** — the handler's controller step: drive the *observed* bar to
  the *desired* bar, where desired = "show ccstatus iff the focused pane is a
  registered Claude pane". See `daemon::Handler::reconcile`.
- **warmth** — the live cache-warm/cold indicator. Flips warm→cold once the
  session has been idle past the prompt-cache TTL (~270s). Computed live by
  the handler so it ticks while idle.
- **state** — the on-disk data contract under `/tmp/ccstatus-<uid>/`:
  `pane/<server>/<pane>.json` (registrar, tmux-only) and `session/<id>.json`.
  The session file carries **presence** (model, cwd, context %, `claude_pid`,
  and the terminal identity `term_program`/`iterm_session_id` for a non-tmux
  **window** jump) written by the registrar on *every* render — including
  outside tmux — plus
  the turn fields (`last_turn_ts`, `turn_count`) written by the hook. Two
  writers, disjoint fields, each read-modify-writing the whole record.
- **fleet** — the aggregate read model over **all** sessions' **state**
  (`fleet.rs`). **Session-driven**: the row is a Claude session (its presence
  record); a pane file, when present, supplies the tmux jump address, and a
  non-tmux Claude falls back to an OS-window target (see **window**), so it
  still shows — jumpable when its window is addressable. Drops dead-pid
  sessions, probes handler liveness, folds into sorted `SessionView`s. Pure
  core (`build_views`) + IO shell (`collect`). Disk state is display-only;
  liveness/addressing are probed, never trusted from a file. Substrate for
  **top** and future aggregate surfaces.
- **jump** — "take me to this Claude": bring a session to the foreground from
  an aggregate surface. In tmux, focus its window + pane via `Tmux::focus_pane`
  (same-server direct; cross-server through that server's **handler** as a
  `focus <pane>` IPC message). Outside tmux, raise its OS terminal window (see
  **window**). See `docs/aggregate-surfaces-design.md`.
- **window** — the non-tmux jump actuator (`window.rs`, macOS). Raises the
  hosting terminal emulator's OS window: iTerm2 by session GUID (from
  `ITERM_SESSION_ID`), Terminal.app by the Claude pid's controlling tty.
  `target_for` decides window-jumpability purely; `focus` does the `osascript`.
  Best-effort — a closed session or denied Automation permission fails soft.

## Deepened modules

- **tmux seam** (`Tmux` trait) — the single owner of one-shot tmux commands
  (option get/set/unset, focused-pane / session / tty queries, refresh,
  reset). CLI adapter in prod, recording fake in tests. Distinct from the
  persistent **control connection** (`control.rs`), which carries focus
  events and `refresh-client -S` and must never fork.
- **bar plan** (`BarPlan`) — a fully-resolved set of bar mutations for one
  session, computed purely from routing + element content, then applied
  through the tmux seam.
- **usage module** (`usage::render`) — the deep home for the `limits`
  element: OAuth fetch, usage cache, builtin-vs-API branching, extra-usage
  credits, reset-time formatting. Internal pure `format_segment` is its test
  surface.
