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
use std::io::{BufRead, BufReader};
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

/// One main-loop iteration's wait timeout. Bounds latency on
/// recomputing derived state (warmth in particular).
const TICK: Duration = Duration::from_millis(500);

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

    let (writer, events) = conn.split();

    // Merge tmux events and registrar messages onto one channel.
    let (tx, rx) = mpsc::channel::<Incoming>();
    spawn_tmux_reader(events, tx.clone());
    spawn_socket_reader(socket, tx);

    let mut daemon = Daemon {
        server_id: server_id.clone(),
        writer,
        snapshot: snap,
        panes: HashMap::new(),
        focused_pane: None,
        state: BarState::Idle,
        last_activity: Instant::now(),
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
}

impl Daemon {
    fn main_loop(&mut self, rx: mpsc::Receiver<Incoming>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Incoming::Tmux(ev)) => self.handle_tmux(ev),
                Ok(Incoming::Registrar(line)) => self.handle_registrar(line),
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
    }

    fn handle_tmux(&mut self, ev: control::Event) {
        if let control::Event::Notification { name, args } = ev {
            match name.as_str() {
                // `%window-pane-changed @<window> %<pane>` — args is two
                // tokens. The second is the now-active pane.
                "window-pane-changed" => {
                    if let Some(p) = args.split_whitespace().nth(1) {
                        if Some(p) != self.focused_pane.as_deref() {
                            self.focused_pane = Some(p.to_string());
                        }
                    }
                }
                // Session/window changes don't directly tell us the new
                // active pane; rely on the follow-up window-pane-changed
                // that tmux emits with the new context. We could query
                // here for more responsive switching, but milestone 5
                // keeps it minimal.
                _ => {}
            }
        }
    }

    fn handle_registrar(&mut self, line: String) {
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
                self.activate(&pane);
                self.state = BarState::Active(pane);
            }
            (BarState::Active(_), None) => {
                self.deactivate();
                self.state = BarState::Idle;
            }
            (BarState::Active(active), Some(pane)) if active != &pane => {
                self.activate(&pane);
                self.state = BarState::Active(pane);
            }
            // Active(p) with target Some(p) → still active; re-render so
            // the warmth indicator updates between events.
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
        if let Some(user_fmt0) = self.snapshot.status_format[0].clone() {
            self.set_format(3, &user_fmt0);
        }
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
        match events.next_event() {
            Ok(ev) => {
                let exit = matches!(ev, control::Event::Exit);
                if tx.send(Incoming::Tmux(ev)).is_err() || exit {
                    return;
                }
            }
            Err(_) => return,
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
