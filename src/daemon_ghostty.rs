//! Machine-wide handler for plain-Ghostty surfaces (no tmux).
//!
//! One handler per machine, spawned on demand by the registrar when Claude
//! renders directly inside Ghostty. Unlike the tmux handler there is no event
//! source at all — Ghostty has no control mode — so the loop is a pure timer:
//! every tick it re-derives each surface's label and re-asserts it as the tab
//! title (OSC 2 written to the surface's pty). Re-assertion, not
//! change-detection, is deliberate: Claude Code and shell prompts also set
//! the title (last writer wins, no compositing), so our stamp only holds if
//! we keep stamping.
//!
//! ```text
//!  registrar (Claude in a ghostty surface) --register /dev/ttysN--> socket
//!  timer (3s) ------------------------------------------------->  handler
//!                                                                    | OSC 2
//!                                                                    v
//!                                                        /dev/ttysN (title)
//! ```
//!
//! Addressing is the pty path. A pty path can be recycled after Claude
//! exits, so every tick re-verifies (from one `ps` snapshot) that each
//! registered Claude pid is alive *and* still controls its recorded pty
//! before anything is written — writing escape bytes into a stranger's
//! terminal is the one unforgivable failure mode.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config;
use crate::daemon::DaemonLog;
use crate::ghostty::{self, GhosttySurface};
use crate::server_dir::ServerDir;
use crate::state;
use crate::util;

/// Grace after the last Claude surface goes before the handler exits.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(5);

/// Timer tick: paces title re-assertion, warmth-independent flag updates,
/// and Claude-death detection. Matches the tmux handler's cadence.
const TICK: Duration = Duration::from_secs(3);

pub fn run() -> ExitCode {
    let dir = match ServerDir::for_current(ghostty::SERVER_ID) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ccstatus ghostty handler: {e}");
            return ExitCode::FAILURE;
        }
    };
    let _lock = match dir.try_lock(ghostty::SOCKET_KEY) {
        Ok(Some(l)) => l,
        Ok(None) => return ExitCode::SUCCESS, // another handler is live
        Err(e) => {
            eprintln!("ccstatus ghostty handler: {e}");
            return ExitCode::FAILURE;
        }
    };
    let socket = match dir.bind_socket(ghostty::SOCKET_KEY) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ccstatus ghostty handler: {e}");
            return ExitCode::FAILURE;
        }
    };

    let log = DaemonLog::for_session(ghostty::SERVER_ID, ghostty::SOCKET_KEY);
    log.write("ghostty handler started");

    let (tx, rx) = mpsc::channel::<String>();
    spawn_socket_reader(socket, tx);

    let mut handler = GhosttyHandler {
        cfg: config::Ghostty::load(),
        flag: config::WindowFlag::load(),
        config_mtime: config::mtime(),
        ttys: HashSet::new(),
        stamped: HashSet::new(),
        notified: HashMap::new(),
        scripting_denied: false,
        last_activity: Instant::now(),
        surface: Box::new(ghostty::CliGhostty),
        log,
    };
    handler.main_loop(rx);
    ExitCode::SUCCESS
}

struct GhosttyHandler {
    cfg: config::Ghostty,
    /// Label template + markers, shared with the tmux window flag. Its
    /// `enabled` gates only tmux window naming; here `cfg.title` is the gate.
    flag: config::WindowFlag,
    config_mtime: Option<std::time::SystemTime>,
    /// Registered surface pty paths hosting a Claude session.
    ttys: HashSet<String>,
    /// Ttys we currently hold a title on, for restore (clear on prune,
    /// on config-off, and on shutdown).
    stamped: HashSet<String>,
    /// Per-tty notification latch: the `last_turn_ts` we last notified for
    /// (or saw at first sight — completions predating the handler don't
    /// banner on spawn). One banner per settled completion, not per tick.
    notified: HashMap<String, i64>,
    /// The user declined the Ghostty automation prompt: stop asking. Focus
    /// then degrades to app-level only (banners defer while Ghostty is
    /// frontmost, `⚑` clears on next prompt instead of on view).
    scripting_denied: bool,
    last_activity: Instant,
    surface: Box<dyn GhosttySurface>,
    log: DaemonLog,
}

impl GhosttyHandler {
    fn main_loop(&mut self, rx: mpsc::Receiver<String>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(first) => {
                    self.handle_message(&first);
                    while let Ok(more) = rx.try_recv() {
                        self.handle_message(&more);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            self.maybe_reload_config();
            self.tick();
            if self.ttys.is_empty() && self.last_activity.elapsed() > IDLE_EXIT_AFTER {
                self.log.write("idle with no Claude surfaces; exiting");
                break;
            }
        }
        self.restore_all();
    }

    /// Handle a one-line socket message. `register <tty>` adds a surface;
    /// `focus <id>` (from an aggregate surface — the id may be the sanitized
    /// pane-file name) raises the hosting terminal window, best-effort.
    fn handle_message(&mut self, line: &str) {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("register"), Some(tty)) if valid_tty(tty) => {
                if self.ttys.insert(tty.to_string()) {
                    self.log.write(&format!("register surface {tty}"));
                }
                self.last_activity = Instant::now();
            }
            (Some("focus"), Some(id)) => {
                let target = self
                    .ttys
                    .iter()
                    .find(|t| *t == id || state::sanitize(t) == id)
                    .cloned();
                if let Some(tty) = target {
                    self.log.write(&format!("focus surface {tty}"));
                    focus_surface(&tty);
                }
            }
            _ => {}
        }
    }

    fn maybe_reload_config(&mut self) {
        let m = config::mtime();
        if m != self.config_mtime {
            self.config_mtime = m;
            self.cfg = config::Ghostty::load();
            self.flag = config::WindowFlag::load();
            // A feature may have just been turned off; clear both channels
            // once and let the next tick re-stamp whatever is still enabled.
            self.restore_all();
            self.log.write("config reloaded");
        }
    }

    /// One timer tick: prune dead surfaces, then re-assert every live one's
    /// title. A single `ps` snapshot feeds liveness, pty ownership, and the
    /// bg-task / suspended signals.
    fn tick(&mut self) {
        let procs = if self.ttys.is_empty() {
            Vec::new()
        } else {
            util::ps_snapshot()
        };
        // `ps` failing (empty snapshot with surfaces registered) must not
        // read as "everyone died": skip the tick, retry on the next.
        if !self.ttys.is_empty() && procs.is_empty() {
            return;
        }

        // Prune: drop a surface once its Claude pid is gone or no longer
        // controls the recorded pty (the path may have been recycled). Clear
        // our title on the way out — the pty is normally still the user's
        // shell, and an empty OSC 2 hands naming back to Ghostty.
        let dead: Vec<String> = self
            .ttys
            .iter()
            .filter(|tty| !claude_owns_tty(&procs, ghostty::SERVER_ID, tty))
            .cloned()
            .collect();
        for tty in dead {
            self.log.write(&format!("surface gone: {tty}"));
            if self.stamped.remove(&tty) {
                self.surface.clear_title(&tty);
                self.surface.set_progress(&tty, ghostty::Progress::Clear);
            }
            self.ttys.remove(&tty);
            self.notified.remove(&tty);
            self.last_activity = Instant::now();
        }

        if !self.cfg.active() {
            self.restore_all();
            return;
        }

        let pids: HashSet<u32> = self
            .ttys
            .iter()
            .filter_map(|t| state::read_pane(ghostty::SERVER_ID, t).map(|p| p.claude_pid))
            .collect();
        let bg = util::background_task_pids(&procs, &pids);
        let susp = util::suspended_pids(&procs, &pids);
        let now = util::now_unix();
        let app_focus = ghostty::probe_focus(&mut self.scripting_denied);

        for tty in self.ttys.iter().cloned().collect::<Vec<_>>() {
            let Some(ps) = state::read_pane(ghostty::SERVER_ID, &tty) else {
                continue;
            };
            let mut sess = state::read_session(&ps.session_id).unwrap_or_default();
            let focus = surface_focus(&app_focus, sess.cwd.as_deref());

            // Viewed: stamp `last_view_ts` (the shared "you saw it" signal —
            // clears the ⚑ here, in `top`, and for the banner below), like
            // the tmux handler's stamp_view.
            if focus == SurfaceFocus::Viewed {
                sess.last_view_ts = Some(now);
                let _ = state::write_session(&ps.session_id, &sess);
            }

            let flags = crate::flags::compute(
                &self.flag,
                &sess,
                susp.contains(&ps.claude_pid),
                bg.contains(&ps.claude_pid),
                now,
            );
            // Re-assert unconditionally every tick: titles are last-writer-
            // wins against Claude Code and shell prompts, and OSC 9;4 bars
            // auto-expire after ~15s (the re-emission is the heartbeat).
            if self.cfg.title {
                self.surface.set_title(&tty, &flags.name);
            }
            if self.cfg.progress && progress_supported(&sess) {
                self.surface
                    .set_progress(&tty, progress_for(flags.activity, &sess, now));
            }

            let (banner, latch) = notify_action(
                self.notified.get(&tty).copied(),
                self.cfg.notify,
                flags.attention,
                sess.last_turn_ts.unwrap_or(0),
                focus,
                now,
            );
            if banner {
                self.surface.notify(&tty, "Claude finished", &flags.name);
            }
            if let Some(l) = latch {
                self.notified.insert(tty.clone(), l);
            }
            self.stamped.insert(tty);
        }
    }

    /// Clear every surface we hold (both channels) and forget them
    /// (config-off, reload, or shutdown).
    fn restore_all(&mut self) {
        for tty in self.stamped.drain() {
            self.surface.clear_title(&tty);
            self.surface.set_progress(&tty, ghostty::Progress::Clear);
        }
    }
}

/// Whether this session's recorded Ghostty version supports OSC 9;4. An
/// unrecorded version reads as unsupported: pre-1.2 Ghostty parses the
/// sequence as a desktop notification, so the wrong "yes" is loud.
fn progress_supported(sess: &state::SessionState) -> bool {
    sess.term_program_version
        .as_deref()
        .is_some_and(ghostty::supports_progress)
}

/// Map a surface's activity onto the progress bar: red when blocked on the
/// user, pulsing while a turn or background task runs, and a cache-warmth
/// countdown while settled — draining over the same 90%-of-TTL window the
/// warm/cold flip uses, so bar-present == "warm". Gone once cold.
fn progress_for(
    activity: crate::fleet::Activity,
    sess: &state::SessionState,
    now: i64,
) -> ghostty::Progress {
    use crate::fleet::Activity::*;
    match activity {
        NeedsInput | Suspended => ghostty::Progress::NeedsInput,
        Working | BgRunning => ghostty::Progress::Working,
        Waiting | Idle | Unknown => match cache_remaining_pct(sess, now) {
            Some(pct) if pct > 0 => ghostty::Progress::Remaining(pct),
            _ => ghostty::Progress::Clear,
        },
    }
}

/// A deferred banner (Ghostty frontmost, focused surface unknowable) stays
/// eligible this long past its completion; older ones are dropped silently —
/// a "Claude finished" for something that finished ten minutes ago is noise.
const NOTIFY_DEFER_CAP_SECS: i64 = 120;

/// One surface's share of the app-level focus probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceFocus {
    /// This surface is on screen: never banner, stamp it viewed.
    Viewed,
    /// Definitely not on screen: a banner will actually show.
    Unfocused,
    /// Ghostty is frontmost but the focused surface is unknowable (no
    /// scripting permission): defer rather than spend the one banner.
    Ambiguous,
}

/// Refine the app-level probe for one surface. Surfaces are matched by
/// working directory (the scripting API exposes no tty), so two sessions in
/// the same directory are conflated — both read as viewed when either is.
fn surface_focus(app: &ghostty::AppFocus, sess_cwd: Option<&str>) -> SurfaceFocus {
    match app {
        ghostty::AppFocus::Background => SurfaceFocus::Unfocused,
        ghostty::AppFocus::Front(Some(cwd)) => match sess_cwd {
            Some(c) if paths_eq(c, cwd) => SurfaceFocus::Viewed,
            _ => SurfaceFocus::Unfocused,
        },
        ghostty::AppFocus::Front(None) => SurfaceFocus::Ambiguous,
        // No probe on this platform: behave as before (emit once, let
        // Ghostty's own focus gate decide).
        ghostty::AppFocus::Unknown => SurfaceFocus::Unfocused,
    }
}

/// Trailing-slash-insensitive path equality.
fn paths_eq(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Pure notification latch: `(banner now?, new latch value)`. `seen` is the
/// turn timestamp we last notified for (`None` = first sight of this
/// surface). First sight seeds quietly — completions predating the handler
/// (or a handler restart) must not banner on spawn. After that, one banner
/// per settled-unviewed completion (`attention` with a turn newer than the
/// latch) — but only once the surface is *known* unfocused, so the single
/// shot isn't spent while the user is still looking (Ghostty would suppress
/// it and the moment would be lost). Viewed completions and toggled-off
/// notifies still advance the latch, so nothing stale ever bursts later.
fn notify_action(
    seen: Option<i64>,
    notify_on: bool,
    attention: bool,
    turn: i64,
    focus: SurfaceFocus,
    now: i64,
) -> (bool, Option<i64>) {
    match seen {
        None => (false, Some(turn)),
        Some(s) if attention && turn != s => match focus {
            SurfaceFocus::Viewed => (false, Some(turn)),
            SurfaceFocus::Unfocused => (notify_on, Some(turn)),
            SurfaceFocus::Ambiguous => {
                if now - turn > NOTIFY_DEFER_CAP_SECS {
                    (false, Some(turn)) // stale: drop silently
                } else {
                    (false, None) // defer; retry next tick
                }
            }
        },
        _ => (false, None),
    }
}

/// Percent of the warm window left (100 right after a turn, 0 at the
/// warm->cold flip), or `None` with no recorded turn.
fn cache_remaining_pct(sess: &state::SessionState, now: i64) -> Option<u8> {
    let ts = sess.last_turn_ts?;
    let idle = (now - ts).max(0);
    let window = crate::render_tmux::warm_threshold_secs(sess.cache_ttl_secs);
    if window <= 0 || idle >= window {
        return Some(0);
    }
    Some(((window - idle) * 100 / window) as u8)
}

/// Whether the pane state behind `tty` names a Claude pid that is alive in
/// the snapshot *and* still has `tty` as its controlling terminal.
fn claude_owns_tty(procs: &[util::ProcInfo], server_id: &str, tty: &str) -> bool {
    let Some(pane) = state::read_pane(server_id, tty) else {
        return false;
    };
    procs
        .iter()
        .any(|p| p.pid == pane.claude_pid && util::tty_matches(tty, &p.tty))
}

/// A registrable pty path: absolute under `/dev/`, no traversal, and only
/// path-safe characters — anything else is ignored (the socket is writable
/// by the uid, but a bad message must never reach `open`).
fn valid_tty(tty: &str) -> bool {
    tty.starts_with("/dev/")
        && !tty.contains("..")
        && tty
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
}

/// Focus the Ghostty tab hosting `tty`, best-effort: Ghostty's scripting
/// `focus` command selects the exact tab and raises its window, matched by
/// the session's working directory (the API exposes no tty). Falls back to
/// the shared tty-keyed window raise (iTerm2/Terminal) if scripting fails.
#[cfg(target_os = "macos")]
fn focus_surface(tty: &str) {
    let cwd = state::read_pane(ghostty::SERVER_ID, tty)
        .and_then(|p| state::read_session(&p.session_id))
        .and_then(|s| s.cwd);
    if let Some(cwd) = cwd
        && ghostty::focus_tab_by_cwd(&cwd)
    {
        return;
    }
    let _ = crate::window::focus_tty(tty);
}

#[cfg(not(target_os = "macos"))]
fn focus_surface(_tty: &str) {}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ghostty::FakeGhostty;

    #[test]
    fn valid_tty_accepts_pty_paths_and_rejects_junk() {
        assert!(valid_tty("/dev/ttys007")); // macOS
        assert!(valid_tty("/dev/pts/3")); // Linux
        assert!(!valid_tty("ttys007")); // not absolute
        assert!(!valid_tty("/dev/../etc/passwd")); // traversal
        assert!(!valid_tty("/dev/ttys007;rm")); // shell junk
        assert!(!valid_tty("/tmp/x")); // outside /dev
    }

    #[test]
    fn claude_owns_tty_requires_live_pid_on_the_same_pty() {
        // No pane state on disk for this key -> not owned.
        assert!(!claude_owns_tty(&[], "ghostty-test-none", "/dev/ttys099"));
    }

    fn handler_for_test() -> GhosttyHandler {
        GhosttyHandler {
            cfg: config::Ghostty::default(),
            flag: config::WindowFlag::default(),
            config_mtime: None,
            ttys: HashSet::new(),
            stamped: HashSet::new(),
            notified: HashMap::new(),
            scripting_denied: false,
            last_activity: Instant::now(),
            surface: Box::new(FakeGhostty::new()),
            // Parent dir never exists in tests, so writes no-op silently.
            log: DaemonLog::for_session("ghostty-test", "t"),
        }
    }

    #[test]
    fn register_records_valid_ttys_and_rejects_junk() {
        let mut h = handler_for_test();
        h.handle_message("register /dev/ttys009");
        assert!(h.ttys.contains("/dev/ttys009"));
        h.handle_message("register /dev/../etc/passwd");
        h.handle_message("register");
        h.handle_message("bogus /dev/ttys001");
        assert_eq!(h.ttys.len(), 1);
    }

    #[test]
    fn restore_all_clears_stamped_titles_once() {
        let mut h = handler_for_test();
        h.stamped.insert("/dev/ttys001".into());
        h.restore_all();
        assert!(h.stamped.is_empty()); // cleared and forgotten
        // (The write itself lands on the boxed surface; ordering is a
        // single-element set, so emptiness is the observable contract.)
    }

    #[test]
    fn progress_maps_activity_and_warmth() {
        use crate::fleet::Activity::*;
        use crate::ghostty::Progress;
        let now = 10_000;
        let sess = |turn: Option<i64>, ttl: Option<i64>| state::SessionState {
            last_turn_ts: turn,
            cache_ttl_secs: ttl,
            ..Default::default()
        };
        // Blocked on the user -> red, regardless of warmth.
        assert_eq!(
            progress_for(NeedsInput, &sess(Some(now), None), now),
            Progress::NeedsInput
        );
        assert_eq!(
            progress_for(Suspended, &sess(Some(now), None), now),
            Progress::NeedsInput
        );
        // Running -> pulse.
        assert_eq!(
            progress_for(Working, &sess(None, None), now),
            Progress::Working
        );
        assert_eq!(
            progress_for(BgRunning, &sess(Some(now), None), now),
            Progress::Working
        );
        // Settled: countdown over the warm window (default TTL 300 -> 270s).
        assert_eq!(
            progress_for(Waiting, &sess(Some(now), None), now),
            Progress::Remaining(100)
        );
        assert_eq!(
            progress_for(Waiting, &sess(Some(now - 135), None), now),
            Progress::Remaining(50)
        );
        // Past the flip -> bar gone. No turn at all -> bar gone.
        assert_eq!(
            progress_for(Idle, &sess(Some(now - 300), None), now),
            Progress::Clear
        );
        assert_eq!(
            progress_for(Unknown, &sess(None, None), now),
            Progress::Clear
        );
        // 1h cache: half the 3240s window left.
        assert_eq!(
            progress_for(Waiting, &sess(Some(now - 1620), Some(3600)), now),
            Progress::Remaining(50)
        );
    }

    #[test]
    fn notify_latch_banners_once_per_new_completion() {
        use SurfaceFocus::*;
        let now = 210;
        // First sight: seed quietly, even if attention is up.
        assert_eq!(
            notify_action(None, true, true, 100, Unfocused, now),
            (false, Some(100))
        );
        // A new settled completion banners once while unfocused...
        assert_eq!(
            notify_action(Some(100), true, true, 200, Unfocused, now),
            (true, Some(200))
        );
        // ...and later ticks of the same completion stay quiet.
        assert_eq!(
            notify_action(Some(200), true, true, 200, Unfocused, now),
            (false, None)
        );
        // Not settled-unviewed -> quiet, latch unchanged.
        assert_eq!(
            notify_action(Some(100), true, false, 200, Unfocused, now),
            (false, None)
        );
        // Feature off: latch still follows the completion (no burst when
        // notify is later re-enabled), but no banner.
        assert_eq!(
            notify_action(Some(100), false, true, 200, Unfocused, now),
            (false, Some(200))
        );
    }

    #[test]
    fn notify_latch_respects_focus() {
        use SurfaceFocus::*;
        let now = 210;
        // Viewed: consumed silently — you're looking at it.
        assert_eq!(
            notify_action(Some(100), true, true, 200, Viewed, now),
            (false, Some(200))
        );
        // Ambiguous (Ghostty front, no scripting): defer — don't spend the
        // one banner while the user may be watching.
        assert_eq!(
            notify_action(Some(100), true, true, 200, Ambiguous, now),
            (false, None)
        );
        // Deferred completion fires the moment the app goes to background...
        assert_eq!(
            notify_action(Some(100), true, true, 200, Unfocused, now),
            (true, Some(200))
        );
        // ...unless it went stale first: dropped silently.
        let later = 200 + NOTIFY_DEFER_CAP_SECS + 1;
        assert_eq!(
            notify_action(Some(100), true, true, 200, Ambiguous, later),
            (false, Some(200))
        );
    }

    #[test]
    fn surface_focus_maps_app_probe_by_cwd() {
        use ghostty::AppFocus;
        assert_eq!(
            surface_focus(&AppFocus::Background, Some("/a")),
            SurfaceFocus::Unfocused
        );
        assert_eq!(
            surface_focus(&AppFocus::Front(Some("/a/".into())), Some("/a")),
            SurfaceFocus::Viewed // trailing slash tolerated
        );
        assert_eq!(
            surface_focus(&AppFocus::Front(Some("/b".into())), Some("/a")),
            SurfaceFocus::Unfocused
        );
        assert_eq!(
            surface_focus(&AppFocus::Front(Some("/a".into())), None),
            SurfaceFocus::Unfocused // no cwd recorded: can't claim viewed
        );
        assert_eq!(
            surface_focus(&AppFocus::Front(None), Some("/a")),
            SurfaceFocus::Ambiguous
        );
        assert_eq!(
            surface_focus(&AppFocus::Unknown, Some("/a")),
            SurfaceFocus::Unfocused
        );
    }

    #[test]
    fn progress_supported_requires_recorded_1_2() {
        let mut s = state::SessionState::default();
        assert!(!progress_supported(&s)); // unrecorded -> no
        s.term_program_version = Some("1.1.3".into());
        assert!(!progress_supported(&s));
        s.term_program_version = Some("1.3.1".into());
        assert!(progress_supported(&s));
    }
}
