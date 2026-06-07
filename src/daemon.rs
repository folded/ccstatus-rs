//! Long-lived ccstatus process driving tmux via control mode.
//!
//! Architecture (milestone 5):
//!
//! ```text
//! +--------------------+   tmux control mode   +--------------------+
//! | reader thread      | <-- BufReader<stdout> | tmux -C attach     |
//! |   yields Event::*  |                       |                    |
//! |   -> mpsc::Sender  |                       |                    |
//! +--------------------+                       +--------------------+
//!          |                                            ^
//!          | mpsc                                       | Writer.send()
//!          v                                            |
//! +----------------------------------------+            |
//! | main loop                              |------------+
//! |   - selects Tmux events & Registrar    |
//! |     messages from one merged channel   |
//! |   - tracks focused-pane state machine  |
//! |   - injects rows / restores snapshot   |
//! +----------------------------------------+
//!          ^
//!          | mpsc
//!          |
//! +--------------------+
//! | socket thread      | <-- UnixListener
//! |   yields lines     |
//! +--------------------+
//! ```

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::control::{self, Connection, EventStream, Writer};
use crate::render_tmux;
use crate::server_dir::ServerDir;
use crate::snapshot::{self, Snapshot};
use crate::state;
use crate::tmux;

/// Idle time after which a daemon with no Claude panes left exits and
/// restores the user's bar. Generous so we don't churn on quick session
/// restarts. Milestone 9 replaces this with proper shutdown signalling.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(60);

/// Maximum time the main loop waits when nothing is happening. Events
/// (tmux notifications, registrar pings) wake the loop immediately;
/// this is the *idle* poll interval, used solely to refresh the warmth
/// indicator in row content. Short enough that warm→cold flips appear
/// near-instantly without busy-waiting.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// tmux's built-in default value for `status-format[0]`. The default
/// isn't stored as an option (so `show-options -gv` returns empty), it's
/// implicit in tmux's renderer. When the user's snapshot captures `None`
/// at `[0]`, we use this when injecting at `[3]` so the row that should
/// be the powerline window list isn't blank. Copied verbatim from a
/// fresh tmux server's `show-options -g status-format[0]`.
const DEFAULT_STATUS_FORMAT_0: &str = "#[align=left range=left #{E:status-left-style}]#[push-default]#{T;=/#{status-left-length}:status-left}#[pop-default]#[norange default]#[list=on align=#{status-justify}]#[list=left-marker]<#[list=right-marker]>#[list=on]#{W:#[range=window|#{window_index} #{E:window-status-style}#{?#{&&:#{window_last_flag},#{!=:#{E:window-status-last-style},default}}, #{E:window-status-last-style},}#{?#{&&:#{window_bell_flag},#{!=:#{E:window-status-bell-style},default}}, #{E:window-status-bell-style},#{?#{&&:#{||:#{window_activity_flag},#{window_silence_flag}},#{!=:#{E:window-status-activity-style},default}}, #{E:window-status-activity-style},}}]#[push-default]#{T:window-status-format}#[pop-default]#[norange default]#{?loop_last_flag,,#{window-status-separator}},#[range=window|#{window_index} list=focus #{?#{!=:#{E:window-status-current-style},default},#{E:window-status-current-style},#{E:window-status-style}}#{?#{&&:#{window_last_flag},#{!=:#{E:window-status-last-style},default}}, #{E:window-status-last-style},}#{?#{&&:#{window_bell_flag},#{!=:#{E:window-status-bell-style},default}}, #{E:window-status-bell-style},#{?#{&&:#{||:#{window_activity_flag},#{window_silence_flag}},#{!=:#{E:window-status-activity-style},default}}, #{E:window-status-activity-style},}}]#[push-default]#{T:window-status-current-format}#[pop-default]#[norange list=on default]#{?loop_last_flag,,#{window-status-separator}}}#[nolist align=right range=right #{E:status-right-style}]#[push-default]#{T;=/#{status-right-length}:status-right}#[pop-default]#[norange default]";

#[derive(Debug)]
enum Incoming {
    Tmux(control::Event),
    Registrar(String),
}

#[derive(Debug, PartialEq, Eq)]
enum BarState {
    Idle,
    Active(String),
}

pub fn run() -> ExitCode {
    let server_id = tmux::server_id().unwrap_or_else(|| "unknown".to_string());

    let dir = match ServerDir::for_current(&server_id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _lock = match dir.try_lock() {
        Ok(Some(l)) => l,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    let socket = match dir.bind_socket() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut conn = match Connection::attach() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };
    let snap = match Snapshot::capture(&mut conn) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ccstatus daemon: snapshot: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _ = snapshot::save(&server_id, &snap);

    // Subscribe to a per-session format that gives the current
    // window's active pane id. tmux emits %subscription-changed
    // whenever the value changes — which covers both:
    //   - active pane of a window changes (in-window switching);
    //   - session's active window changes (which makes a different
    //     pane visible without an in-window switch).
    // Without this subscription, neither case generates any
    // notification we'd see in control mode.
    let _ = conn.cmd("refresh-client -B 'focus=:#{window_active_pane}'");
    // Capture the initial focused pane synchronously so we don't start
    // in an "unknown focus" state and miss the first reconcile.
    let initial_focus = conn
        .cmd("display-message -p '#{window_active_pane}'")
        .ok()
        .filter(|r| r.ok)
        .map(|r| r.output.trim().to_string())
        .filter(|s| !s.is_empty());

    let (writer, events) = conn.split();

    // Merge tmux events and registrar messages onto one channel.
    let (tx, rx) = mpsc::channel::<Incoming>();
    spawn_tmux_reader(events, tx.clone());
    spawn_socket_reader(socket, tx);

    let log = DaemonLog::for_server(&server_id);
    log.write(&format!(
        "startup: snapshot status={} pos={} initial_focus={:?}",
        &snap.status, &snap.status_position, initial_focus
    ));
    let mut daemon = Daemon {
        server_id: server_id.clone(),
        writer,
        snapshot: snap,
        panes: HashMap::new(),
        focused_pane: initial_focus,
        state: BarState::Idle,
        last_activity: Instant::now(),
        log,
    };
    daemon.main_loop(rx);

    ExitCode::SUCCESS
}

struct Daemon {
    server_id: String,
    writer: Writer,
    snapshot: Snapshot,
    /// pane_id -> session_id of Claude sessions registered against this server.
    panes: HashMap<String, String>,
    /// The pane the user's interactive client most recently focused, if known.
    focused_pane: Option<String>,
    state: BarState,
    last_activity: Instant,
    log: DaemonLog,
}

struct DaemonLog {
    path: PathBuf,
}

impl DaemonLog {
    fn for_server(server_id: &str) -> Self {
        let path = crate::cache::cache_dir()
            .join("server")
            .join(server_id)
            .join("daemon.log");
        Self { path }
    }

    fn write(&self, msg: &str) {
        // Best-effort append. If logging fails, drop silently — diagnostics
        // shouldn't be the reason the daemon halts.
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

impl Daemon {
    fn main_loop(&mut self, rx: mpsc::Receiver<Incoming>) {
        loop {
            // Block until *something* happens (or the tick deadline).
            // Then drain everything else that's already in the queue
            // before touching tmux again — this is what keeps the
            // channel empty and avoids backpressure on the reader and,
            // through it, on tmux itself.
            match rx.recv_timeout(IDLE_POLL) {
                Ok(first) => {
                    self.dispatch(first);
                    while let Ok(more) = rx.try_recv() {
                        self.dispatch(more);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.reconcile();
            if self.should_exit() {
                break;
            }
        }
        self.snapshot.apply_via_writer(&mut self.writer);
        let _ = self.writer.send("refresh-client -S");
        // Tell tmux we're done so the server cleans up our client slot
        // promptly. Without this it lingers until the OS reaps the pipe.
        let _ = self.writer.send("detach-client");
    }

    fn dispatch(&mut self, ev: Incoming) {
        match ev {
            Incoming::Tmux(e) => self.handle_tmux(e),
            Incoming::Registrar(line) => self.handle_registrar(line),
        }
    }

    fn handle_tmux(&mut self, ev: control::Event) {
        if let control::Event::Notification { name, args } = ev {
            self.log.write(&format!("tmux event: %{name} {args}"));
            match name.as_str() {
                // tmux doesn't emit %window-pane-changed for in-window
                // pane switches without a subscription. We subscribed at
                // attach time to a per-session `#{window_active_pane}`
                // format, so we get %subscription-changed with the new
                // value here. Arg layout per tmux docs:
                //   %subscription-changed name session window window_index pane data
                // The format value is the *last* whitespace-separated
                // field. If tmux ever changes that, we'll see it in the
                // log and adjust.
                "subscription-changed" => {
                    let toks: Vec<&str> = args.split_whitespace().collect();
                    if let Some((_name, rest)) = toks.split_first() {
                        if let Some(value) = rest.last() {
                            if !value.is_empty()
                                && Some(*value) != self.focused_pane.as_deref()
                            {
                                self.log.write(&format!("focus -> {value} (subscription)"));
                                self.focused_pane = Some((*value).to_string());
                            }
                        }
                    }
                }
                // Backstop for the case where window-pane-changed *does*
                // fire (e.g. via `select-pane`). Same args as before:
                // `@<window> %<pane>` — pane is the second token.
                "window-pane-changed" => {
                    if let Some(p) = args.split_whitespace().nth(1) {
                        if Some(p) != self.focused_pane.as_deref() {
                            self.log.write(&format!("focus -> {p}"));
                            self.focused_pane = Some(p.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_registrar(&mut self, line: String) {
        self.log.write(&format!("registrar: {line}"));
        let mut parts = line.split_whitespace();
        let kind = parts.next().unwrap_or("");
        match kind {
            "register" => {
                let (Some(pane), Some(session)) = (parts.next(), parts.next()) else {
                    return;
                };
                self.panes.insert(pane.to_string(), session.to_string());
                self.last_activity = Instant::now();
                // The first register often arrives before tmux has told
                // us the focused pane — treat the registered pane as
                // focused (it usually is, since the registrar runs from
                // a Claude render in that pane).
                if self.focused_pane.is_none() {
                    self.focused_pane = Some(pane.to_string());
                }
            }
            _ => {}
        }
    }

    fn reconcile(&mut self) {
        let target = self
            .focused_pane
            .as_ref()
            .filter(|p| self.panes.contains_key(*p))
            .cloned();
        match (&self.state, target) {
            (BarState::Idle, Some(pane)) => {
                self.log.write(&format!("reconcile: Idle -> Active({pane})"));
                self.activate(&pane);
                self.state = BarState::Active(pane);
            }
            (BarState::Active(_), None) => {
                self.log.write(&format!(
                    "reconcile: Active -> Idle (focused={:?}, panes={:?})",
                    self.focused_pane, self.panes.keys().collect::<Vec<_>>()
                ));
                self.deactivate();
                self.state = BarState::Idle;
            }
            (BarState::Active(active), Some(pane)) if active != &pane => {
                self.log.write(&format!("reconcile: Active({active}) -> Active({pane})"));
                self.activate(&pane);
                self.state = BarState::Active(pane);
            }
            (BarState::Active(active), Some(pane)) if active == &pane => {
                self.refresh_rows(&pane);
            }
            _ => {}
        }
    }

    fn activate(&mut self, pane_id: &str) {
        self.write_rows_for(pane_id);
        let _ = self.writer.send("set-option -g status 4");
        let _ = self.writer.send("refresh-client -S");
    }

    fn refresh_rows(&mut self, pane_id: &str) {
        self.write_rows_for(pane_id);
        let _ = self.writer.send("refresh-client -S");
    }

    /// Layout (status-position bottom):
    ///   [0] sub heatmap   (closest to panes, top of status area)
    ///   [1] main heatmap
    ///   [2] rich Claude line + warmth
    ///   [3] user's original [0] (powerline window list)
    fn write_rows_for(&mut self, pane_id: &str) {
        let Some(pane) = state::read_pane(&self.server_id, pane_id) else {
            return;
        };
        let session = state::read_session(&pane.session_id).unwrap_or_default();
        let line0 = render_tmux::format_stashed_line(&pane, &session, 0);
        let line1 = render_tmux::format_stashed_line(&pane, &session, 1);
        let line2 = render_tmux::format_stashed_line(&pane, &session, 2);
        self.set_format(0, &line2);
        self.set_format(1, &line1);
        self.set_format(2, &line0);
        // [3] should render the user's original status-format[0]
        // (powerline window list, typically). If the user hadn't
        // explicitly set [0], the snapshot captured `None` because the
        // default template isn't exposed via show-options — fall back
        // to tmux's built-in default so the row isn't blank.
        let user_fmt0 = self.snapshot.status_format[0]
            .clone()
            .unwrap_or_else(|| DEFAULT_STATUS_FORMAT_0.to_string());
        self.set_format(3, &user_fmt0);
    }

    fn set_format(&mut self, i: usize, value: &str) {
        let escaped = snapshot::escape_for_tmux(value);
        let _ = self.writer.send(&format!(
            "set-option -g 'status-format[{i}]' \"{escaped}\""
        ));
    }

    fn deactivate(&mut self) {
        self.snapshot.apply_via_writer(&mut self.writer);
        let _ = self.writer.send("refresh-client -S");
    }

    fn should_exit(&self) -> bool {
        self.panes.is_empty()
            && matches!(self.state, BarState::Idle)
            && self.last_activity.elapsed() > IDLE_EXIT_AFTER
    }
}

fn spawn_tmux_reader(mut events: EventStream, tx: mpsc::Sender<Incoming>) {
    thread::spawn(move || loop {
        let ev = match events.next_event() {
            Ok(ev) => ev,
            Err(_) => return,
        };
        // Allowlist what reaches the main channel. tmux emits a lot of
        // chatter in control mode (per-pane %output, server-wide
        // %sessions-changed bookkeeping, command-response %begin/%end
        // for our own writes, …) and queueing any of it backpressures
        // tmux. We forward only the events the daemon actually reacts
        // to plus Exit so the main loop can shut down cleanly.
        let forward = match &ev {
            control::Event::Notification { name, .. } => matches!(
                name.as_str(),
                "subscription-changed" | "window-pane-changed" | "session-window-changed"
            ),
            control::Event::Exit => true,
            // Command frames and body lines: ignored — we use the
            // Writer half (fire-and-forget) after the synchronous
            // snapshot phase, so responses are never expected.
            control::Event::Begin { .. }
            | control::Event::End { .. }
            | control::Event::Output(_) => false,
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
