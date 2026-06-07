//! Long-lived ccstatus process that drives the tmux status bar for the
//! sessions that contain a Claude pane.
//!
//! One daemon per tmux *server* (keyed by the server-socket hash; see
//! [`crate::server_dir`]). It does **not** hold a persistent control-mode
//! connection — a control client dies when its attached session is killed,
//! even if other sessions remain, which would collapse every session's
//! bar. Instead it polls:
//!
//! ```text
//! +------------------+   register <pane> <claude-session>   +-----------+
//! | socket thread    | <----------------------------------- | registrar |
//! |  -> mpsc::Sender |                                       +-----------+
//! +------------------+
//!          | mpsc
//!          v
//! +-------------------------------------------------+
//! | main loop (wakes on ping or POLL timeout)       |   one-shot
//! |  - `tmux list-panes -a` -> live panes+sessions  |---------------> tmux
//! |  - reconcile each session's bar (set -t / -u)   |   `tmux ...`
//! |  - rewrite live rows (warmth) on threshold flip |
//! +-------------------------------------------------+
//! ```
//!
//! ## Per-session, never global
//!
//! All bar mutations are session-local (`set-option -t <session> …`) and
//! removed with `set-option -u -t <session> …`, which reverts the session
//! to inheriting the user's global config. The global `status` /
//! `status-format` are never written, so the user's powerline can never be
//! blanked and there is nothing to "pollute". Sessions we have overridden
//! are tracked in a marker file so a crashed daemon's leftovers are cleared
//! on the next startup.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{self, Dest, Element};
use crate::render_tmux;
use crate::server_dir::ServerDir;
use crate::state;
use crate::tmux;

/// Grace period after the last Claude pane goes before the daemon exits.
/// The bar is already restored at that point (deactivation is decoupled
/// from process exit), so this only governs respawn cost: short enough that
/// cleanup is prompt and frequently exercised, long enough to absorb a
/// quick session restart without churning the process. The registrar
/// respawns the daemon the next time a Claude pane renders.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(5);

/// Maximum time the main loop blocks when nothing is happening. Registrar
/// pings wake it immediately; this bounds how quickly we notice a closed
/// Claude pane (bar collapse) and re-evaluate the warmth threshold. Bar
/// height only changes on Claude start/stop now, so a couple of seconds of
/// latency is invisible.
const POLL: Duration = Duration::from_secs(2);

/// Threshold (seconds) at which the cache-warmth label flips warm->cold.
/// Mirrors the constant in `render_tmux.rs`; sits under Claude's ~5-minute
/// prompt-cache TTL.
const WARM_THRESHOLD_SECS: i64 = 270;

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

    let log = DaemonLog::for_server(&server_id);
    log.write("daemon process started (polling)");

    // Crash recovery: clear bar overrides left on any session by a
    // predecessor that didn't shut down cleanly. We only touch sessions we
    // recorded ourselves, so a user's legitimate per-session config is
    // never disturbed.
    let leftover = read_active(&server_id);
    if !leftover.is_empty() {
        log.write(&format!("clearing {} leftover session(s)", leftover.len()));
        for session in &leftover {
            restore_session(session);
        }
        refresh_clients();
    }
    write_active(&server_id, &HashSet::new());

    let (tx, rx) = mpsc::channel::<String>();
    spawn_socket_reader(socket, tx);

    // Routing is read once at startup (Phase 1). Config edits take effect
    // when the daemon next restarts; live hot-reload is Phase 2.
    let routing = config::Routing::load();

    let mut daemon = Daemon {
        server_id,
        routing,
        panes: HashMap::new(),
        active: HashSet::new(),
        last_warmth: HashMap::new(),
        last_activity: Instant::now(),
        log,
    };
    daemon.main_loop(rx);

    ExitCode::SUCCESS
}

struct Daemon {
    server_id: String,
    /// Where each rendered line is routed (read once at startup).
    routing: config::Routing,
    /// Registered Claude panes: tmux pane_id -> claude session id (uuid).
    panes: HashMap<String, String>,
    /// tmux session ids we currently hold a bar override on.
    active: HashSet<String>,
    /// Per-tmux-session warmth label last rendered, to skip no-op rewrites.
    last_warmth: HashMap<String, &'static str>,
    last_activity: Instant,
    log: DaemonLog,
}

impl Daemon {
    fn main_loop(&mut self, rx: mpsc::Receiver<String>) {
        loop {
            match rx.recv_timeout(POLL) {
                Ok(first) => {
                    self.handle_register(first);
                    while let Ok(more) = rx.try_recv() {
                        self.handle_register(more);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            match self.reconcile() {
                Reconcile::ServerGone => {
                    // The tmux server is gone; its options went with it.
                    // Nothing to restore — just exit.
                    self.log.write("tmux server gone; exiting");
                    self.active.clear();
                    break;
                }
                Reconcile::Ok => {}
            }

            if self.should_exit() {
                self.log.write("idle with no Claude panes; exiting");
                self.restore_all();
                break;
            }
        }
        // Persist whatever override set we ended on (empty after a clean
        // restore_all; on a Disconnected break it reflects the live state
        // so the next daemon can clean up).
        write_active(&self.server_id, &self.active);
    }

    fn handle_register(&mut self, line: String) {
        self.log.write(&format!("registrar: {line}"));
        let mut parts = line.split_whitespace();
        if let (Some("register"), Some(pane), Some(session)) =
            (parts.next(), parts.next(), parts.next())
        {
            self.panes.insert(pane.to_string(), session.to_string());
            self.last_activity = Instant::now();
        }
    }

    /// Reconcile every tmux session's bar against current Claude panes.
    fn reconcile(&mut self) -> Reconcile {
        let Some(live) = list_panes() else {
            return Reconcile::ServerGone;
        };

        // Drop registrations whose tmux pane no longer exists OR whose
        // Claude process has exited. The latter is what makes the bar
        // collapse when the user quits Claude but leaves the shell pane
        // open — "last Claude exited", not "pane closed".
        let live_pane_ids: HashSet<&str> = live.iter().map(|(p, _)| p.as_str()).collect();
        let server_id = self.server_id.clone();
        let keep: HashSet<String> = self
            .panes
            .keys()
            .filter(|pane| live_pane_ids.contains(pane.as_str()))
            .filter(|pane| {
                state::read_pane(&server_id, pane)
                    .map(|p| pid_alive(p.claude_pid))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        let before = self.panes.len();
        self.panes.retain(|pane, _| keep.contains(pane));
        if self.panes.len() != before {
            self.log.write(&format!(
                "pruned {} pane(s) (closed or Claude exited); {} remain",
                before - self.panes.len(),
                self.panes.len()
            ));
            // Reset the idle clock so the exit grace runs from when the
            // last pane actually went, not the last registrar ping.
            self.last_activity = Instant::now();
        }

        // pane_id -> tmux session for live panes.
        let pane_session: HashMap<&str, &str> =
            live.iter().map(|(p, s)| (p.as_str(), s.as_str())).collect();

        // For each tmux session, the registered Claude panes it contains.
        let mut session_panes: HashMap<String, Vec<String>> = HashMap::new();
        for pane in self.panes.keys() {
            if let Some(session) = pane_session.get(pane.as_str()) {
                session_panes
                    .entry((*session).to_string())
                    .or_default()
                    .push(pane.clone());
            }
        }

        // Deactivate sessions that no longer have a Claude pane.
        let stale: Vec<String> = self
            .active
            .iter()
            .filter(|s| !session_panes.contains_key(*s))
            .cloned()
            .collect();
        let mut changed = false;
        for session in stale {
            self.log.write(&format!("deactivate session {session}"));
            restore_session(&session);
            self.active.remove(&session);
            self.last_warmth.remove(&session);
            changed = true;
        }

        // Activate / refresh sessions that have a Claude pane.
        for (session, panes) in &session_panes {
            let Some(pane_id) = self.pick_pane(panes) else {
                continue;
            };
            let warmth = self.pane_warmth(&pane_id);
            if !self.active.contains(session) {
                self.log.write(&format!("activate session {session} via {pane_id}"));
                self.write_rows(session, &pane_id);
                self.active.insert(session.clone());
                self.last_activity = Instant::now();
                changed = true;
            } else if self.last_warmth.get(session).copied() != warmth {
                self.log.write(&format!(
                    "warmth flip on {session}: {:?} -> {:?}",
                    self.last_warmth.get(session),
                    warmth
                ));
                self.write_rows(session, &pane_id);
                changed = true;
            }
            match warmth {
                Some(w) => {
                    self.last_warmth.insert(session.clone(), w);
                }
                None => {
                    self.last_warmth.remove(session);
                }
            }
        }

        if changed {
            write_active(&self.server_id, &self.active);
            refresh_clients();
        }
        Reconcile::Ok
    }

    /// Choose which Claude pane drives a session's rows: the most recently
    /// registered one (registrar rewrites `registered_at` on each render,
    /// so this tracks the pane the user is actively using).
    fn pick_pane(&self, panes: &[String]) -> Option<String> {
        panes
            .iter()
            .max_by_key(|p| {
                state::read_pane(&self.server_id, p)
                    .map(|s| s.registered_at)
                    .unwrap_or(0)
            })
            .cloned()
    }

    fn pane_warmth(&self, pane_id: &str) -> Option<&'static str> {
        let pane = state::read_pane(&self.server_id, pane_id)?;
        let session = state::read_session(&pane.session_id)?;
        let last_turn = session.last_turn_ts?;
        let idle = crate::util::now_unix().saturating_sub(last_turn);
        Some(if idle < WARM_THRESHOLD_SECS { "warm" } else { "cold" })
    }

    /// Write the session-local status-format rows and row count from the
    /// routing config. Each used row (ascending; `row0` nearest the panes)
    /// gets the elements routed to it, joined inline and translated to tmux
    /// format; the powerline window list sits below them. Setting any index
    /// replaces the whole session-local array, so we write each row
    /// explicitly. Higher indices left over from a previous layout are
    /// harmless: the `status` row count gates what renders.
    fn write_rows(&self, session: &str, pane_id: &str) {
        let Some(pane) = state::read_pane(&self.server_id, pane_id) else {
            return;
        };
        let sess = state::read_session(&pane.session_id).unwrap_or_default();
        // `warmth` is computed live here; all other elements come from the
        // registrar's pane state.
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
            set_session(session, &format!("status-format[{idx}]"), &tmux);
            idx += 1;
        }
        set_session(session, &format!("status-format[{idx}]"), &global_powerline_row());
        set_session(session, "status", &status_value(idx + 1));

        // Powerline sides: inject the left/right-routed elements into the
        // powerline row's status-left/status-right, composed with the
        // user's (global) value. Zero added height.
        self.write_powerline_side(session, Dest::Left, "status-left", &content);
        self.write_powerline_side(session, Dest::Right, "status-right", &content);
    }

    /// Compose the elements routed to a powerline side and set the
    /// session-local `status-left`/`status-right`, keeping the user's
    /// global value at the screen edge. Leaves the option untouched (so it
    /// keeps inheriting the global) when nothing is routed there.
    fn write_powerline_side(
        &self,
        session: &str,
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
            return;
        }
        let mine = render_tmux::ansi_to_tmux(&render_tmux::join_segments(
            parts.iter().map(String::as_str),
        ));
        let user = global_option(option);
        let combined = match (user.is_empty(), dest) {
            (true, _) => mine,
            // Our segment sits at the screen edge; the user's value next to
            // the window list.
            (false, Dest::Left) => format!("{mine} {user}"),
            (false, _) => format!("{user} {mine}"),
        };
        set_session(session, option, &combined);
    }

    fn restore_all(&mut self) {
        for session in self.active.drain() {
            restore_session(&session);
        }
        self.last_warmth.clear();
        refresh_clients();
    }

    fn should_exit(&self) -> bool {
        self.panes.is_empty()
            && self.active.is_empty()
            && self.last_activity.elapsed() > IDLE_EXIT_AFTER
    }
}

enum Reconcile {
    Ok,
    ServerGone,
}

/// Whether a process is still alive. `kill(pid, 0)` sends no signal but
/// performs the permission/existence checks: `0` means it exists; `EPERM`
/// means it exists but we can't signal it (still alive); `ESRCH` means
/// gone.
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

/// `set-option -t <session> <name> <value>` (session-local override).
fn set_session(session: &str, name: &str, value: &str) {
    let _ = Command::new("tmux")
        .args(["set-option", "-t", session, name, value])
        .status();
}

/// Drop all bar overrides for a session, reverting it to inheriting the
/// user's global config. `status-format` is unset as a whole array (no
/// index) so every row reverts at once; `status-left`/`status-right` revert
/// the powerline-side injections.
fn restore_session(session: &str) {
    for opt in ["status-format", "status", "status-left", "status-right"] {
        let _ = Command::new("tmux")
            .args(["set-option", "-u", "-t", session, opt])
            .status();
    }
}

fn refresh_clients() {
    let _ = Command::new("tmux").args(["refresh-client", "-S"]).status();
}

/// Read a global (user) option value. We always read the *global* value,
/// never the session-effective one, so our own session-local overrides
/// can't feed back into a later compose (double-injection).
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

/// The effective global `status-format[0]` — the user's powerline window
/// list. Falls back to tmux's built-in default template when empty.
fn global_powerline_row() -> String {
    let value = global_option("status-format[0]");
    if value.is_empty() {
        tmux::DEFAULT_STATUS_FORMAT_0.to_string()
    } else {
        value
    }
}

/// All live panes as `(pane_id, session_id)`. `None` means the tmux server
/// is gone (the command failed), which the caller treats as a shutdown
/// signal.
fn list_panes() -> Option<Vec<(String, String)>> {
    let out = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_id} #{session_id}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut v = Vec::new();
    for line in s.lines() {
        let mut it = line.split_whitespace();
        if let (Some(pane), Some(session)) = (it.next(), it.next()) {
            v.push((pane.to_string(), session.to_string()));
        }
    }
    Some(v)
}

// --- active-session marker file (crash recovery) ---------------------------

fn active_path(server_id: &str) -> PathBuf {
    crate::cache::cache_dir()
        .join("server")
        .join(server_id)
        .join("active-sessions")
}

fn read_active(server_id: &str) -> HashSet<String> {
    match std::fs::read_to_string(active_path(server_id)) {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => HashSet::new(),
    }
}

fn write_active(server_id: &str, sessions: &HashSet<String>) {
    let body = sessions.iter().cloned().collect::<Vec<_>>().join("\n");
    let _ = crate::cache::write_atomic(&active_path(server_id), &body);
}

// --- diagnostics -----------------------------------------------------------

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

fn spawn_socket_reader(socket: std::os::unix::net::UnixListener, tx: mpsc::Sender<String>) {
    thread::spawn(move || {
        for stream in socket.incoming() {
            let Ok(s) = stream else { continue };
            let reader = BufReader::new(s);
            for line in reader.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        }
    });
}
