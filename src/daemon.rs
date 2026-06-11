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
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{self, Align, Element};
use crate::control::{self, Connection, EventStream, Writer};
use crate::render_tmux;
use crate::server_dir::ServerDir;
use crate::state;
use crate::tmux::{self, Tmux};

/// Grace after the last Claude pane goes before the handler exits. The bar
/// is already restored by then; this only governs respawn cost.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(5);

/// Timer tick. Focus is event-driven (instant); this only paces the
/// time-based work with no event source: the warmth threshold and
/// Claude-PID-death detection (Claude exits but the shell pane stays open,
/// which tmux doesn't signal).
const TICK: Duration = Duration::from_secs(3);

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
    tmux::restore_session(&tmux::CliTmux, &session);

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
        routing: config::Routing::for_context(true),
        config_mtime: config::mtime(),
        force_rerender: false,
        panes: HashSet::new(),
        focused_pane: initial_focus,
        active: false,
        last_warmth: None,
        last_activity: Instant::now(),
        tmux: Box::new(tmux::CliTmux),
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
    /// One-shot tmux command seam (option get/set/unset, focus query). The
    /// control connection (`writer`) is a separate seam for `refresh-client`.
    tmux: Box<dyn Tmux>,
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
            if should_exit(
                self.panes.is_empty(),
                self.active,
                self.last_activity.elapsed(),
            ) {
                self.log.write("idle with no Claude panes; exiting");
                break;
            }
        }
        // Graceful exit (session still alive): revert our overrides.
        tmux::restore_session(self.tmux.as_ref(), &self.session);
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
                self.handle_message(&line);
                false
            }
        }
    }

    /// Re-query the session's focused pane (active pane of the active
    /// window). Cheap one-shot; triggered by focus events and the timer.
    fn requery_focus(&mut self) {
        if let Some(p) = self.tmux.focused_pane(&self.session)
            && Some(p.as_str()) != self.focused_pane.as_deref()
        {
            self.log.write(&format!("focus -> {p}"));
            self.focused_pane = Some(p);
        }
    }

    /// Handle a one-line socket message from a registrar or an aggregate
    /// surface. `register <pane>` adds a Claude pane; `focus <pane>` is the
    /// "take me to this Claude" jump — actuated only by the daemon that owns
    /// the pane (broadcasts to sibling daemons on the same server no-op), so
    /// exactly one client switch happens.
    fn handle_message(&mut self, line: &str) {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("register"), Some(pane)) => {
                if self.panes.insert(pane.to_string()) {
                    self.log.write(&format!("register pane {pane}"));
                }
                self.last_activity = Instant::now();
            }
            (Some("focus"), Some(pane)) if self.panes.contains(pane) => {
                self.log.write(&format!("focus pane {pane}"));
                self.tmux.focus_pane(pane);
            }
            _ => {}
        }
    }

    fn maybe_reload_config(&mut self) {
        let m = config::mtime();
        if m != self.config_mtime {
            self.config_mtime = m;
            self.routing = config::Routing::for_context(true);
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
                .map(|p| crate::util::pid_alive(p.claude_pid))
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
    /// Computes the inputs to the pure `decide`, then performs the IO its
    /// verdict implies.
    fn reconcile(&mut self) {
        let focused_claude = self
            .focused_pane
            .as_deref()
            .filter(|p| self.panes.contains(*p))
            .map(str::to_string);
        let force = std::mem::take(&mut self.force_rerender);
        let warmth = focused_claude.as_deref().and_then(|p| self.pane_warmth(p));
        let warmth_changed = warmth != self.last_warmth;

        match decide(
            self.active,
            focused_claude.as_deref(),
            force,
            warmth_changed,
        ) {
            Action::Activate(pane) => {
                self.log.write(&format!("activate via focused {pane}"));
                self.render_and_apply(&pane);
                self.active = true;
                self.last_warmth = warmth;
                self.last_activity = Instant::now();
                self.refresh();
            }
            Action::Rerender(pane) => {
                self.render_and_apply(&pane);
                self.last_warmth = warmth;
                self.refresh();
            }
            Action::Deactivate => {
                self.log.write("deactivate (focus left Claude)");
                tmux::restore_session(self.tmux.as_ref(), &self.session);
                self.active = false;
                self.last_warmth = None;
                self.refresh();
            }
            Action::Noop => {}
        }
    }

    fn pane_warmth(&self, pane_id: &str) -> Option<&'static str> {
        let pane = state::read_pane(&self.server_id, pane_id)?;
        let session = state::read_session(&pane.session_id)?;
        let last_turn = session.last_turn_ts?;
        let idle = crate::util::now_unix().saturating_sub(last_turn);
        Some(if idle < render_tmux::warm_threshold_secs(session.cache_ttl_secs) {
            "warm"
        } else {
            "cold"
        })
    }

    /// Read the (focused, Claude) pane's content, build the bar plan from the
    /// routing config, and apply it through the tmux seam.
    fn render_and_apply(&self, pane_id: &str) {
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
        let plan = plan_bar(
            &self.routing,
            &content,
            &tmux::base_row(self.tmux.as_ref()),
            &self.tmux.global("status-left"),
            &self.tmux.global("status-right"),
            self.routing.tmux_background(),
            &self.tmux.global("status-style"),
        );
        apply(self.tmux.as_ref(), &self.session, &plan);
    }
}

/// The controller verdict: drive the observed bar to the desired bar, where
/// desired = "show ccstatus iff the focused pane is a registered Claude pane".
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Activate(String),
    Deactivate,
    Rerender(String),
    Noop,
}

/// Pure transition. `focused_claude` is `Some(pane)` iff the focused pane is a
/// registered Claude pane (the handler computes it once and passes it in).
pub fn decide(
    active: bool,
    focused_claude: Option<&str>,
    force: bool,
    warmth_changed: bool,
) -> Action {
    match (active, focused_claude) {
        (false, Some(p)) => Action::Activate(p.to_string()),
        (true, None) => Action::Deactivate,
        (true, Some(p)) if force || warmth_changed => Action::Rerender(p.to_string()),
        _ => Action::Noop,
    }
}

/// A fully-resolved set of bar mutations for one session: the status-format
/// rows (dedicated rows first, the user's base row last), the `status` choice
/// value, and the two base-row edges (`status-left` / `status-right`).
pub struct BarPlan {
    pub formats: Vec<String>,
    pub status: String,
    pub left: Side,
    pub right: Side,
    /// The session's `status-style`: an explicit value (the user's style with
    /// the configured background applied) or revert to inheriting the global.
    pub style: Side,
}

/// A base-row edge (`status-left` / `status-right`): an explicit value to set,
/// or revert to inheriting the global (`unset_session`).
pub enum Side {
    Set(String),
    Inherit,
}

/// Pure: turn routing + already-read element content into a concrete bar plan.
/// `base_row` is the resolved user base status row (`status-format[0]`);
/// `user_left` / `user_right` are the user's global `status-left` /
/// `status-right`, composed onto the correct edge.
///
/// tmux line 0 is the base row: its `left`/`right` elements become the
/// `status-left`/`status-right` edges. Lines >= 1 are dedicated rows, mapped to
/// `status-format[line-1]`; each row composes its left and right groups with a
/// `#[align=right]` break between them.
pub fn plan_bar(
    routing: &config::Routing,
    content: &dyn Fn(Element) -> Option<String>,
    base_row: &str,
    user_left: &str,
    user_right: &str,
    bg: Option<(u8, u8, u8)>,
    user_status_style: &str,
) -> BarPlan {
    let mut formats: Vec<String> = routing
        .tmux_lines()
        .into_iter()
        .filter(|&line| line >= 1)
        .map(|line| row_format(routing, content, line))
        .collect();
    formats.push(base_row.to_string());
    let status = tmux::status_value(formats.len());

    // An explicit background is applied via the session's status-style, which
    // both the dedicated rows' `#[default]` resets and tmux's trailing
    // row-fill honour — so the whole bar reads with one background instead of
    // the inherited theme. Preserve the user's other style tokens.
    let style = match bg {
        Some(rgb) => Side::Set(tmux::with_bg(user_status_style, rgb)),
        None => Side::Inherit,
    };

    BarPlan {
        formats,
        status,
        left: plan_edge(
            routing.tmux_at(0, Align::Left),
            content,
            user_left,
            Align::Left,
        ),
        right: plan_edge(
            routing.tmux_at(0, Align::Right),
            content,
            user_right,
            Align::Right,
        ),
        style,
    }
}

/// Compose one dedicated tmux row (`status-format[line-1]`): the left group,
/// then `#[align=right]` and the right group when present.
fn row_format(
    routing: &config::Routing,
    content: &dyn Fn(Element) -> Option<String>,
    line: u8,
) -> String {
    let group = |align| {
        let parts: Vec<String> = routing
            .tmux_at(line, align)
            .into_iter()
            .filter_map(content)
            .filter(|s| !s.is_empty())
            .collect();
        render_tmux::ansi_to_tmux(&render_tmux::join_segments(
            parts.iter().map(String::as_str),
        ))
    };
    let mut row = group(Align::Left);
    let right = group(Align::Right);
    if !right.is_empty() {
        row.push_str("#[align=right]");
        row.push_str(&right);
    }
    row
}

/// Compose a base-row edge (`status-left`/`status-right`) from its line-0
/// elements, merged with the user's existing edge value (ours nearest the
/// centre). Empty → revert to inheriting the global.
fn plan_edge(
    elements: Vec<Element>,
    content: &dyn Fn(Element) -> Option<String>,
    user: &str,
    align: Align,
) -> Side {
    let parts: Vec<String> = elements
        .into_iter()
        .filter_map(content)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        // Revert to inheriting the global (handles a hot-reload that removed
        // the elements; no-op on a fresh activate).
        return Side::Inherit;
    }
    let mine = render_tmux::ansi_to_tmux(&render_tmux::join_segments(
        parts.iter().map(String::as_str),
    ));
    let combined = match (user.is_empty(), align) {
        (true, _) => mine,
        (false, Align::Left) => format!("{mine} {user}"),
        (false, Align::Right) => format!("{user} {mine}"),
    };
    Side::Set(combined)
}

/// Effect: apply a bar plan to a session through the tmux seam.
fn apply(t: &dyn Tmux, session: &str, plan: &BarPlan) {
    for (i, f) in plan.formats.iter().enumerate() {
        t.set_session(session, &format!("status-format[{i}]"), f);
    }
    t.set_session(session, "status", &plan.status);
    match &plan.left {
        Side::Set(v) => t.set_session(session, "status-left", v),
        Side::Inherit => t.unset_session(session, "status-left"),
    }
    match &plan.right {
        Side::Set(v) => t.set_session(session, "status-right", v),
        Side::Inherit => t.unset_session(session, "status-right"),
    }
    match &plan.style {
        Side::Set(v) => t.set_session(session, "status-style", v),
        Side::Inherit => t.unset_session(session, "status-style"),
    }
}

/// Pure: the handler exits once no Claude panes remain, it isn't active, and
/// the idle grace has elapsed.
fn should_exit(panes_empty: bool, active: bool, idle: Duration) -> bool {
    panes_empty && !active && idle > IDLE_EXIT_AFTER
}

fn spawn_tmux_reader(mut events: EventStream, tx: mpsc::Sender<Incoming>) {
    thread::spawn(move || {
        loop {
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
        let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Dest, Routing};
    use crate::tmux::{FakeTmux, Write};

    #[test]
    fn decide_covers_the_transition_table() {
        // inactive + Claude focused -> activate
        assert_eq!(
            decide(false, Some("%1"), false, false),
            Action::Activate("%1".into())
        );
        // active + focus left Claude -> deactivate
        assert_eq!(decide(true, None, false, false), Action::Deactivate);
        // active + Claude focused, nothing changed -> noop
        assert_eq!(decide(true, Some("%1"), false, false), Action::Noop);
        // active + Claude focused, forced -> rerender
        assert_eq!(
            decide(true, Some("%1"), true, false),
            Action::Rerender("%1".into())
        );
        // active + Claude focused, warmth flipped -> rerender
        assert_eq!(
            decide(true, Some("%1"), false, true),
            Action::Rerender("%1".into())
        );
        // inactive + nothing focused -> noop
        assert_eq!(decide(false, None, true, true), Action::Noop);
    }

    #[test]
    fn plan_bar_default_has_three_rows_plus_base_row() {
        let routing = Routing::default();
        let content = |e: Element| (e == Element::Model).then(|| "M".to_string());
        let plan = plan_bar(&routing, &content, "BASE", "", "", None, "");
        assert_eq!(plan.formats.len(), 4); // dedicated lines 1/2/3 + base row
        assert_eq!(plan.formats[3], "BASE");
        assert_eq!(plan.status, "4");
        assert!(matches!(plan.left, Side::Inherit));
        assert!(matches!(plan.right, Side::Inherit));
        assert!(matches!(plan.style, Side::Inherit)); // no background configured
    }

    #[test]
    fn plan_bar_applies_background_over_user_style() {
        let routing = Routing::default();
        let content = |_: Element| None;
        let plan = plan_bar(
            &routing,
            &content,
            "BASE",
            "",
            "",
            Some((0x1a, 0x1b, 0x26)),
            "fg=colour137,bg=colour234",
        );
        match plan.style {
            Side::Set(s) => assert_eq!(s, "fg=colour137,bg=#1a1b26"),
            Side::Inherit => panic!("expected an explicit status-style"),
        }
    }

    #[test]
    fn plan_bar_joins_row_segments_with_separator() {
        let left = Dest::Tmux {
            line: 1,
            align: Align::Left,
        };
        let routing = Routing::from_pairs(&[(Element::Model, left), (Element::Cwd, left)]);
        let content = |e: Element| match e {
            Element::Model => Some("M".to_string()),
            Element::Cwd => Some("C".to_string()),
            _ => None,
        };
        let plan = plan_bar(&routing, &content, "BASE", "", "", None, "");
        let expected = render_tmux::ansi_to_tmux(&render_tmux::join_segments(["M", "C"]));
        assert_eq!(plan.formats[0], expected);
        assert!(expected.contains('|')); // the ` | ` separator survived
    }

    #[test]
    fn plan_bar_row_right_group_uses_align_directive() {
        // line 1, left = model; line 1, right = tokens. The row breaks to the
        // right with tmux's #[align=right].
        let routing = Routing::from_pairs(&[
            (
                Element::Model,
                Dest::Tmux {
                    line: 1,
                    align: Align::Left,
                },
            ),
            (
                Element::Tokens,
                Dest::Tmux {
                    line: 1,
                    align: Align::Right,
                },
            ),
        ]);
        let content = |e: Element| match e {
            Element::Model => Some("M".to_string()),
            Element::Tokens => Some("T".to_string()),
            _ => None,
        };
        let plan = plan_bar(&routing, &content, "BASE", "", "", None, "");
        assert_eq!(
            plan.formats[0],
            format!(
                "{}#[align=right]{}",
                render_tmux::ansi_to_tmux("M"),
                render_tmux::ansi_to_tmux("T")
            )
        );
    }

    #[test]
    fn plan_bar_line0_becomes_base_edges() {
        // tmux line 0 right = tokens -> status-right, merged with the user's.
        let routing = Routing::from_pairs(&[(
            Element::Tokens,
            Dest::Tmux {
                line: 0,
                align: Align::Right,
            },
        )]);
        let content = |e: Element| (e == Element::Tokens).then(|| "T".to_string());
        let plan = plan_bar(&routing, &content, "BASE", "UL", "UR", None, "");
        let mine = render_tmux::ansi_to_tmux("T");
        match plan.right {
            Side::Set(s) => assert_eq!(s, format!("UR {mine}")), // user value toward the edge
            Side::Inherit => panic!("expected Set"),
        }
        // Nothing on the left edge -> inherit the global. And no dedicated rows.
        assert!(matches!(plan.left, Side::Inherit));
        assert_eq!(plan.formats.len(), 1); // base row only
    }

    #[test]
    fn apply_emits_ordered_writes() {
        let t = FakeTmux::new();
        let plan = BarPlan {
            formats: vec!["row0".into(), "base".into()],
            status: "2".into(),
            left: Side::Set("L".into()),
            right: Side::Inherit,
            style: Side::Set("bg=#1a1b26".into()),
        };
        apply(&t, "$1", &plan);
        assert_eq!(
            *t.writes.borrow(),
            vec![
                Write::SetSession("$1".into(), "status-format[0]".into(), "row0".into()),
                Write::SetSession("$1".into(), "status-format[1]".into(), "base".into()),
                Write::SetSession("$1".into(), "status".into(), "2".into()),
                Write::SetSession("$1".into(), "status-left".into(), "L".into()),
                Write::UnsetSession("$1".into(), "status-right".into()),
                Write::SetSession("$1".into(), "status-style".into(), "bg=#1a1b26".into()),
            ]
        );
    }

    #[test]
    fn deactivate_restore_unsets_every_bar_option() {
        let t = FakeTmux::new();
        tmux::restore_session(&t, "$1");
        assert_eq!(
            *t.writes.borrow(),
            vec![
                Write::UnsetSession("$1".into(), "status-format".into()),
                Write::UnsetSession("$1".into(), "status".into()),
                Write::UnsetSession("$1".into(), "status-left".into()),
                Write::UnsetSession("$1".into(), "status-right".into()),
                Write::UnsetSession("$1".into(), "status-style".into()),
            ]
        );
    }

    #[test]
    fn focus_pane_round_trips_through_the_seam() {
        let t = FakeTmux::new();
        t.focus_pane("%7");
        assert_eq!(*t.writes.borrow(), vec![Write::Focus("%7".into())]);
    }

    #[test]
    fn should_exit_requires_no_panes_inactive_and_grace_elapsed() {
        assert!(should_exit(
            true,
            false,
            IDLE_EXIT_AFTER + Duration::from_secs(1)
        ));
        assert!(!should_exit(
            false,
            false,
            IDLE_EXIT_AFTER + Duration::from_secs(1)
        )); // panes remain
        assert!(!should_exit(
            true,
            true,
            IDLE_EXIT_AFTER + Duration::from_secs(1)
        )); // active
        assert!(!should_exit(true, false, Duration::from_secs(0))); // within grace
    }
}
