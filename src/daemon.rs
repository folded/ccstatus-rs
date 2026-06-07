//! Per-session, event-driven status-bar handler.
//!
//! One handler per tmux *session* that hosts Claude. It attaches a control
//! client to exactly that session (`tmux -C attach -t <session>`), so:
//!
//! - focus and pane events arrive as **events** (no polling) and are
//!   naturally scoped to the session;
//! - when the session is killed the control client gets `%exit`, which is
//!   the handler's shutdown signal — no re-attach, the death *is* the
//!   signal. Other sessions are driven by their own handlers, spawned on
//!   demand when Claude first renders there.
//!
//! ```text
//!  registrar (Claude pane in session X) --register %pane--> per-session socket
//!  tmux -C attach -t X  --%subscription-changed (focus)-->  +-----------+
//!                       --%exit (session gone)----------->  |  handler  |
//!                                                           |  (one mpsc|
//!  timer (warmth / Claude-PID death)  --------------------> |   loop)   |
//!                                                           +-----------+
//!                                                                 | one-shot
//!                                                                 v  tmux set -t X
//! ```
//!
//! The handler must never send `refresh-client -C` on the control
//! connection: a control client without a size doesn't participate in
//! session sizing, so it can't resize the user's real client. All bar
//! mutations are session-local (`set -t X …`) and reverted with
//! `set -u -t X …`, so the global config is never written.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{self, Dest, Element};
use crate::control::{self, Connection, EventStream, Writer};
use crate::render_tmux;
use crate::server_dir::ServerDir;
use crate::state;
use crate::tmux;

/// Grace after the last Claude pane goes before the handler exits. The bar
/// is already restored by then; this only governs respawn cost.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(5);

/// Timer tick. Focus is event-driven (instant); this only paces the
/// time-based work with no event source: the warmth threshold and
/// Claude-PID-death detection (Claude exits but the shell pane stays open,
/// which tmux doesn't signal).
const TICK: Duration = Duration::from_secs(3);

/// Threshold (seconds) at which the cache-warmth label flips warm->cold.
const WARM_THRESHOLD_SECS: i64 = 270;

#[derive(Debug)]
enum Incoming {
    Tmux(control::Event),
    Registrar(String),
}

pub fn run(session: String) -> ExitCode {
    let server_id = tmux::server_id().unwrap_or_else(|| "unknown".to_string());

    let dir = match ServerDir::for_current(&server_id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ccstatus handler: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _lock = match dir.try_lock(&session) {
        Ok(Some(l)) => l,
        Ok(None) => return ExitCode::SUCCESS, // another handler owns this session
        Err(e) => {
            eprintln!("ccstatus handler: {e}");
            return ExitCode::FAILURE;
        }
    };
    let socket = match dir.bind_socket(&session) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ccstatus handler: {e}");
            return ExitCode::FAILURE;
        }
    };

    let log = DaemonLog::for_session(&server_id, &session);
    log.write(&format!("handler started for session {session}"));

    // Crash recovery: a predecessor handler may have died mid-active,
    // leaving this session's bar overridden. Clear it before we start; the
    // reconcile below re-applies the correct state.
    restore_session(&session);

    let mut conn = match Connection::attach(&session) {
        Ok(c) => c,
        Err(e) => {
            log.write(&format!("attach failed: {e}"));
            return ExitCode::FAILURE;
        }
    };
    // Capture the initial focused pane. We don't use `refresh-client -B`
    // subscriptions: `%window-pane-changed` (in-window pane switch) and
    // `%session-window-changed` (active-window change) already fire in
    // control mode without one, and we re-query the focused pane on those
    // events (and on the timer) rather than parsing it out of them.
    let initial_focus = conn
        .cmd(&format!("display-message -t {session} -p '#{{pane_id}}'"))
        .ok()
        .filter(|r| r.ok)
        .map(|r| r.output.trim().to_string())
        .filter(|s| !s.is_empty());
    let (writer, events) = conn.split();

    let (tx, rx) = mpsc::channel::<Incoming>();
    spawn_tmux_reader(events, tx.clone());
    spawn_socket_reader(socket, tx);

    let mut handler = Handler {
        session,
        server_id,
        routing: config::Routing::load(),
        config_mtime: config::mtime(),
        force_rerender: false,
        panes: HashSet::new(),
        focused_pane: initial_focus,
        active: false,
        last_warmth: None,
        last_activity: Instant::now(),
        writer,
        log,
    };
    handler.main_loop(rx);
    ExitCode::SUCCESS
}

struct Handler {
    /// The tmux session id (e.g. `$1`) this handler drives.
    session: String,
    /// Server hash, for pane-state file paths.
    server_id: String,
    routing: config::Routing,
    config_mtime: Option<std::time::SystemTime>,
    force_rerender: bool,
    /// Registered Claude pane ids in this session.
    panes: HashSet<String>,
    /// The session's currently-focused pane (from control events).
    focused_pane: Option<String>,
    /// Whether the bar is currently showing ccstatus for this session.
    active: bool,
    last_warmth: Option<&'static str>,
    last_activity: Instant,
    /// The control connection's write half: used to send `refresh-client`
    /// and to keep the connection (and thus the event stream) alive —
    /// dropping it detaches the control client.
    writer: Writer,
    log: DaemonLog,
}

impl Handler {
    fn main_loop(&mut self, rx: mpsc::Receiver<Incoming>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(first) => {
                    if self.dispatch(first) {
                        // %exit: session gone, nothing to restore.
                        self.log.write("session exited; shutting down");
                        return;
                    }
                    while let Ok(more) = rx.try_recv() {
                        if self.dispatch(more) {
                            self.log.write("session exited; shutting down");
                            return;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            self.maybe_reload_config();
            self.requery_focus();
            self.prune_dead_panes();
            self.reconcile();
            if self.should_exit() {
                self.log.write("idle with no Claude panes; exiting");
                break;
            }
        }
        // Graceful exit (session still alive): revert our overrides.
        restore_session(&self.session);
        self.refresh();
    }

    /// Refresh status on all clients via the control connection (no fork).
    fn refresh(&mut self) {
        let _ = self.writer.send("refresh-client -S");
    }

    /// Returns true if the session exited (caller should shut down). Focus
    /// notifications just wake the loop, which re-queries focus before
    /// reconciling, so the only event we act on directly is `%exit`.
    fn dispatch(&mut self, ev: Incoming) -> bool {
        match ev {
            Incoming::Tmux(control::Event::Exit) => true,
            Incoming::Tmux(_) => false,
            Incoming::Registrar(line) => {
                self.handle_register(line);
                false
            }
        }
    }

    /// Re-query the session's focused pane (active pane of the active
    /// window). Cheap one-shot; triggered by focus events and the timer.
    fn requery_focus(&mut self) {
        let out = Command::new("tmux")
            .args(["display-message", "-t", &self.session, "-p", "#{pane_id}"])
            .output();
        if let Ok(o) = out
            && o.status.success()
        {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !p.is_empty() && Some(p.as_str()) != self.focused_pane.as_deref() {
                self.log.write(&format!("focus -> {p}"));
                self.focused_pane = Some(p);
            }
        }
    }

    fn handle_register(&mut self, line: String) {
        let mut parts = line.split_whitespace();
        if let (Some("register"), Some(pane)) = (parts.next(), parts.next()) {
            if self.panes.insert(pane.to_string()) {
                self.log.write(&format!("register pane {pane}"));
            }
            self.last_activity = Instant::now();
        }
    }

    fn maybe_reload_config(&mut self) {
        let m = config::mtime();
        if m != self.config_mtime {
            self.config_mtime = m;
            self.routing = config::Routing::load();
            self.force_rerender = true;
            self.log.write("config reloaded");
        }
    }

    /// Drop registered panes whose Claude process has exited or whose pane
    /// state vanished. Runs on the timer (no tmux event for a Claude that
    /// exits while its shell pane stays open).
    fn prune_dead_panes(&mut self) {
        let server_id = self.server_id.clone();
        let before = self.panes.len();
        self.panes.retain(|pane| {
            state::read_pane(&server_id, pane)
                .map(|p| pid_alive(p.claude_pid))
                .unwrap_or(false)
        });
        if self.panes.len() != before {
            self.log.write(&format!(
                "pruned {} pane(s) (Claude exited); {} remain",
                before - self.panes.len(),
                self.panes.len()
            ));
            self.last_activity = Instant::now();
        }
    }

    /// Show ccstatus iff the focused pane is a registered Claude pane.
    fn reconcile(&mut self) {
        let focus_is_claude = self
            .focused_pane
            .as_deref()
            .map(|p| self.panes.contains(p))
            .unwrap_or(false);
        let force = std::mem::take(&mut self.force_rerender);

        match (self.active, focus_is_claude) {
            (false, true) => {
                let pane = self.focused_pane.clone().unwrap();
                self.log.write(&format!("activate via focused {pane}"));
                self.write_rows(&pane);
                self.active = true;
                self.last_warmth = self.pane_warmth(&pane);
                self.last_activity = Instant::now();
                self.refresh();
            }
            (true, false) => {
                self.log.write("deactivate (focus left Claude)");
                restore_session(&self.session);
                self.active = false;
                self.last_warmth = None;
                self.refresh();
            }
            (true, true) => {
                let pane = self.focused_pane.clone().unwrap();
                let warmth = self.pane_warmth(&pane);
                if force || warmth != self.last_warmth {
                    self.write_rows(&pane);
                    self.last_warmth = warmth;
                    self.refresh();
                }
            }
            (false, false) => {}
        }
    }

    fn pane_warmth(&self, pane_id: &str) -> Option<&'static str> {
        let pane = state::read_pane(&self.server_id, pane_id)?;
        let session = state::read_session(&pane.session_id)?;
        let last_turn = session.last_turn_ts?;
        let idle = crate::util::now_unix().saturating_sub(last_turn);
        Some(if idle < WARM_THRESHOLD_SECS { "warm" } else { "cold" })
    }

    /// Write the session-local rows + powerline sides from the routing
    /// config, driven by the given (focused, Claude) pane's content.
    fn write_rows(&self, pane_id: &str) {
        let Some(pane) = state::read_pane(&self.server_id, pane_id) else {
            return;
        };
        let sess = state::read_session(&pane.session_id).unwrap_or_default();
        let warmth = render_tmux::warmth_segment(&sess);
        let content = |e: Element| -> Option<String> {
            if e.is_live() {
                warmth.clone()
            } else {
                pane.elements.get(e.key()).cloned()
            }
        };

        let mut idx = 0usize;
        for row in self.routing.rows_used() {
            let parts: Vec<String> = self
                .routing
                .elements_for(Dest::Row(row))
                .into_iter()
                .filter_map(content)
                .filter(|s| !s.is_empty())
                .collect();
            let joined = render_tmux::join_segments(parts.iter().map(String::as_str));
            let tmux = render_tmux::ansi_to_tmux(&joined);
            set_session(&self.session, &format!("status-format[{idx}]"), &tmux);
            idx += 1;
        }
        set_session(&self.session, &format!("status-format[{idx}]"), &global_powerline_row());
        set_session(&self.session, "status", &status_value(idx + 1));

        self.write_powerline_side(Dest::Left, "status-left", &content);
        self.write_powerline_side(Dest::Right, "status-right", &content);
    }

    fn write_powerline_side(
        &self,
        dest: Dest,
        option: &str,
        content: &dyn Fn(Element) -> Option<String>,
    ) {
        let parts: Vec<String> = self
            .routing
            .elements_for(dest)
            .into_iter()
            .filter_map(content)
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            // Revert to inheriting the global (handles a hot-reload that
            // removed the elements; no-op on a fresh activate).
            let _ = Command::new("tmux")
                .args(["set-option", "-u", "-t", &self.session, option])
                .status();
            return;
        }
        let mine = render_tmux::ansi_to_tmux(&render_tmux::join_segments(
            parts.iter().map(String::as_str),
        ));
        let user = global_option(option);
        let combined = match (user.is_empty(), dest) {
            (true, _) => mine,
            (false, Dest::Left) => format!("{mine} {user}"),
            (false, _) => format!("{user} {mine}"),
        };
        set_session(&self.session, option, &combined);
    }

    fn should_exit(&self) -> bool {
        self.panes.is_empty() && !self.active && self.last_activity.elapsed() > IDLE_EXIT_AFTER
    }
}

/// `set-option -t <session> <name> <value>` (session-local override).
fn set_session(session: &str, name: &str, value: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-t", session, name, value])
        .status();
}

/// Drop all bar overrides for a session, reverting it to inheriting the
/// user's global config.
fn restore_session(session: &str) {
    for opt in ["status-format", "status", "status-left", "status-right"] {
        let _ = Command::new("tmux")
            .args(["set-option", "-u", "-t", session, opt])
            .status();
    }
}

/// Read a global (user) option value — never the session-effective one, so
/// our own session-local overrides can't feed back into a later compose.
fn global_option(name: &str) -> String {
    let out = Command::new("tmux")
        .args(["show-options", "-gv", name])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim_end_matches('\n').to_string()
        }
        _ => String::new(),
    }
}

/// The effective global `status-format[0]` (the powerline window list),
/// falling back to tmux's built-in default template when empty.
fn global_powerline_row() -> String {
    let value = global_option("status-format[0]");
    if value.is_empty() {
        tmux::DEFAULT_STATUS_FORMAT_0.to_string()
    } else {
        value
    }
}

/// tmux's `status` is a choice option (`off`/`on`/`2`..`5`); `"1"` is
/// rejected, so a single row must be spelled `on`.
fn status_value(rows: usize) -> String {
    match rows {
        0 => "off".to_string(),
        1 => "on".to_string(),
        n => n.to_string(),
    }
}

/// Whether a process is still alive (`kill(pid, 0)`: 0 = exists, EPERM =
/// exists but unsignalable, ESRCH = gone).
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn spawn_tmux_reader(mut events: EventStream, tx: mpsc::Sender<Incoming>) {
    thread::spawn(move || loop {
        let ev = match events.next_event() {
            Ok(ev) => ev,
            Err(_) => return,
        };
        // Forward only what the handler reacts to; tmux emits a lot of
        // chatter (%output, command frames) that would just backpressure.
        let forward = match &ev {
            control::Event::Notification { name, .. } => matches!(
                name.as_str(),
                "window-pane-changed" | "session-window-changed"
            ),
            control::Event::Exit => true,
            _ => false,
        };
        if !forward {
            continue;
        }
        let exit = matches!(ev, control::Event::Exit);
        if tx.send(Incoming::Tmux(ev)).is_err() || exit {
            return;
        }
    });
}

fn spawn_socket_reader(socket: std::os::unix::net::UnixListener, tx: mpsc::Sender<Incoming>) {
    thread::spawn(move || {
        for stream in socket.incoming() {
            let Ok(s) = stream else { continue };
            let reader = BufReader::new(s);
            for line in reader.lines().map_while(Result::ok) {
                if tx.send(Incoming::Registrar(line)).is_err() {
                    return;
                }
            }
        }
    });
}

struct DaemonLog {
    path: PathBuf,
}

impl DaemonLog {
    fn for_session(server_id: &str, session: &str) -> Self {
        let path = crate::cache::cache_dir()
            .join("server")
            .join(server_id)
            .join(format!("handler{}.log", sanitize_session(session)));
        Self { path }
    }

    fn write(&self, msg: &str) {
        let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) else {
            return;
        };
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{ts} {msg}");
    }
}

/// Session ids (`$1`) → a filename-safe suffix.
fn sanitize_session(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
