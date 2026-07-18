//! Ghostty backend: detection, surface identity, and the polling daemon.
//!
//! A Claude running directly in Ghostty (no tmux) has no tmux pane id to key
//! its surface on. The addressing handle is instead the **pty path** the
//! emulator allocated for the session — the role tmux's `%N` pane id plays
//! everywhere else. It's resolved from the *Claude* pid, not from the
//! statusline process: Claude Code spawns the statusline detached with no
//! controlling tty, so the statusline can't read its own — but the interactive
//! Claude process's controlling tty *is* the Ghostty pty (see
//! [`crate::util::pid_tty`]).
//!
//! Unlike tmux (a per-session control-mode handler driven by focus events),
//! Ghostty has no session concept and no event stream, so the backend is a
//! **single polling daemon**: it enumerates the registered surfaces every tick
//! and drives two per-surface indicators from plain pty escapes —
//!
//! - **tab title** (OSC 2) carrying the same activity flag the tmux backend
//!   puts on the window name (see [`crate::config::WindowFlag`]); titles are
//!   last-writer-wins, so it's re-asserted every tick;
//! - a **desktop notification** (OSC 777), edge-triggered on an unviewed
//!   completion when `windowFlag.notify` is set.
//!
//! It deliberately does *not* drive the progress bar (OSC 9;4): Claude Code
//! emits that natively (`terminalProgressBarEnabled`) and in real time, so a
//! second writer on the same pty would only fight it.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::WindowFlag;
use crate::dlog::DaemonLog;
use crate::server_dir::ServerDir;
use crate::state::{self, PaneState};
use crate::util::{self, now_unix, pid_alive, resolve_session_id};

/// The `server`/`pane` directory namespace for Ghostty surfaces. Ghostty has no
/// tmux server, so all surfaces (across every Ghostty window) share one
/// namespace and one daemon — distinct from the per-tmux-server hashes.
pub const SURFACE_SERVER_ID: &str = "ghostty";

/// Lock/socket basename for the singleton daemon (there is one, not one per
/// session as in tmux).
const DAEMON_KEY: &str = "daemon";

/// Poll cadence. Ghostty gives us no events, so polling is the only clock —
/// but we adapt it: poll fast while Ghostty is frontmost (you're looking, so the
/// flag clearing / activity glyph should feel responsive), and slowly otherwise
/// (backgrounded — e.g. you're in another app — so keep the ps/git/scripting
/// cost low).
const POLL_ACTIVE: Duration = Duration::from_secs(1);
const POLL_IDLE: Duration = Duration::from_secs(3);

/// Grace after the last surface goes before the daemon exits.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(5);

/// Pure: whether `term_program` names Ghostty. Ghostty sets
/// `TERM_PROGRAM=ghostty`; inside tmux `TERM_PROGRAM` is `tmux` instead, so a
/// tmux-hosted Ghostty correctly reads as *not* Ghostty here (and is driven by
/// the tmux backend). Split from the env read for testing.
pub fn is_ghostty(term_program: Option<&str>) -> bool {
    term_program == Some("ghostty")
}

/// Whether this process is hosted directly by Ghostty (cheap env check, no
/// process-tree walk). Gate the pid resolution behind this so non-Ghostty
/// terminals don't pay for it.
pub fn is_active() -> bool {
    is_ghostty(env::var("TERM_PROGRAM").ok().as_deref())
}

/// `Some(pty_path)` when Claude is running directly in Ghostty: the surface id
/// (the emulator's pty, resolved from `claude_pid`). `None` outside Ghostty, or
/// when the pty can't be resolved (no controlling terminal). The caller passes
/// the already-resolved interactive-Claude pid.
pub fn active_surface(claude_pid: u32) -> Option<String> {
    if !is_active() {
        return None;
    }
    util::pid_tty(claude_pid)
}

/// The OSC 2 "set window title" sequence for `label`. Control characters are
/// stripped so a directory or branch name can't smuggle in a terminator (`\x07`
/// / `\x1b`) or a newline and corrupt the escape.
pub fn osc_title(label: &str) -> String {
    let clean: String = label.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]2;{clean}\x07")
}

/// The OSC 777 "notify" sequence: `ESC ] 777 ; notify ; <title> ; <body> BEL`,
/// which Ghostty (and urxvt) turn into a desktop notification. Control chars are
/// stripped from both fields so they can't corrupt the escape or inject extra
/// `;` separators.
pub fn osc_notify(title: &str, body: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_control() && *c != ';')
            .collect::<String>()
    };
    format!("\x1b]777;notify;{};{}\x07", clean(title), clean(body))
}

/// Registrar side (runs in the statusline process): record this Ghostty surface
/// so the daemon can find it, and make sure the daemon is running. No-op unless
/// we're in Ghostty and the window flag is enabled (the only surface this phase
/// drives). `claude_pid` is the already-resolved interactive-Claude pid.
pub fn register(input: &Value, claude_pid: u32) {
    let Some(session_id) = resolve_session_id(input) else {
        return;
    };
    let Some(pty) = active_surface(claude_pid) else {
        return;
    };
    if !WindowFlag::load().enabled {
        return;
    }

    // Presence fields are constant for the surface, so once written the pane
    // file never changes — skip the rewrite when it already matches, so a
    // streaming statusline doesn't churn the filesystem.
    if let Some(existing) = state::read_pane(SURFACE_SERVER_ID, &pty)
        && existing.session_id == session_id
        && existing.claude_pid == claude_pid
    {
        ensure_daemon();
        return;
    }

    let surface = PaneState {
        session_id,
        claude_pid,
        pane_tty: pty.clone(),
        transcript_path: input
            .get("transcript_path")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        registered_at: now_unix(),
        last_warmth: None,
        elements: HashMap::new(),
    };
    let _ = state::write_pane(SURFACE_SERVER_ID, &pty, &surface);
    ensure_daemon();
}

/// Spawn the singleton daemon iff none is running. We probe by trying to take
/// the daemon lock: acquiring it means no daemon holds it, so we drop it
/// immediately (end of the `let` statement) and spawn one. A lost race just
/// spawns a second daemon that exits at once on its own `try_lock`.
fn ensure_daemon() {
    let Ok(dir) = ServerDir::for_current(SURFACE_SERVER_ID) else {
        return;
    };
    let free = matches!(dir.try_lock(DAEMON_KEY), Ok(Some(_)));
    if free {
        spawn_daemon_detached();
    }
}

fn spawn_daemon_detached() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--ghostty-daemon");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    // setsid detaches from the registrar's session/process group so the daemon
    // survives the statusline process exiting and SIGHUP from the pty closing.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

/// How often the expensive caches (`ps` snapshot, per-surface git status) are
/// refreshed — independent of, and slower than, the poll cadence. Focus and the
/// timestamp-derived activity are re-read every poll; bg/suspended and git
/// (which change slowly and cost process spawns) ride this slower clock.
const HEAVY_REFRESH: Duration = Duration::from_secs(3);

/// Daemon entrypoint (`--ghostty-daemon`). Single instance, guarded by a lock.
pub fn run() -> ExitCode {
    let dir = match ServerDir::for_current(SURFACE_SERVER_ID) {
        Ok(d) => d,
        Err(_) => return ExitCode::FAILURE,
    };
    let _lock = match dir.try_lock(DAEMON_KEY) {
        Ok(Some(l)) => l,
        Ok(None) => return ExitCode::SUCCESS, // another daemon owns it
        Err(_) => return ExitCode::FAILURE,
    };

    let mut daemon = Daemon::new(DaemonLog::for_ghostty());
    daemon.log.write("ghostty daemon started");
    let mut last_activity = Instant::now();

    loop {
        let flag = WindowFlag::load();
        if !flag.enabled {
            // The title is the only surface the flag drives; nothing to do (a
            // disabled flag also means no notifications).
            daemon
                .log
                .write("window flag disabled; clearing titles and exiting");
            daemon.restore_all();
            return ExitCode::SUCCESS;
        }

        // One cheap `lsappinfo` per tick drives both the view probe and the
        // adaptive cadence.
        let frontmost = ghostty_frontmost();
        let live = daemon.tick(&flag, frontmost);
        if !live.is_empty() {
            last_activity = Instant::now();
        }
        if live.is_empty() && last_activity.elapsed() > IDLE_EXIT_AFTER {
            daemon.log.write("idle with no surfaces; exiting");
            daemon.restore_all();
            return ExitCode::SUCCESS;
        }
        thread::sleep(if frontmost { POLL_ACTIVE } else { POLL_IDLE });
    }
}

/// Cross-tick daemon state. The cheap bits (applied titles, edge-tracking) are
/// updated every poll; `signals` and `git` are caches refreshed on the slower
/// [`HEAVY_REFRESH`] clock so a fast poll doesn't respawn `ps`/`git`.
struct Daemon {
    log: DaemonLog,
    /// surface id -> (pty, last title written): change detection + restore.
    applied: HashMap<String, (String, String)>,
    /// surface id -> attention last tick: edge-triggers the completion notify.
    prev_attn: HashMap<String, bool>,
    /// Last resolved view, so transitions log once.
    prev_view: Viewed,
    /// `ps`-derived bg/suspended signals, refreshed on the heavy clock.
    signals: crate::flag::PsSignals,
    /// cwd -> git state, refreshed on the heavy clock (git spawns are the
    /// per-surface expense we keep off the fast path).
    git: HashMap<String, Option<crate::git::GitState>>,
    /// When the caches were last refreshed (`None` forces a refresh next tick).
    last_heavy: Option<Instant>,
}

impl Daemon {
    fn new(log: DaemonLog) -> Self {
        Self {
            log,
            applied: HashMap::new(),
            prev_attn: HashMap::new(),
            prev_view: Viewed::Away,
            signals: crate::flag::PsSignals::capture(&HashSet::new()),
            git: HashMap::new(),
            last_heavy: None,
        }
    }

    /// One poll pass: refresh the caches if due, stamp the viewed surface, then
    /// stamp each live surface's title (OSC 2), fire the completion notification
    /// (OSC 777) on the unviewed-completion edge, drop dead ones, and clear the
    /// title for surfaces that have gone. Returns the live surface ids.
    fn tick(&mut self, flag: &WindowFlag, frontmost: bool) -> HashSet<String> {
        let states: Vec<(String, PaneState)> = state::list_panes(SURFACE_SERVER_ID)
            .into_iter()
            .filter_map(|id| state::read_pane(SURFACE_SERVER_ID, &id).map(|p| (id, p)))
            .collect();

        if self
            .last_heavy
            .map(|t| t.elapsed() >= HEAVY_REFRESH)
            .unwrap_or(true)
        {
            self.refresh_heavy(&states);
        }

        let now = now_unix();
        // Stamp the viewed surface *before* rendering, so its cleared attention
        // flag shows this tick rather than next.
        self.stamp_views(&states, frontmost);

        let mut live = HashSet::new();
        for (id, ps) in &states {
            if !pid_alive(ps.claude_pid) {
                state::remove_pane(SURFACE_SERVER_ID, id);
                self.log
                    .write(&format!("surface {} pruned (Claude exited)", ps.pane_tty));
                continue; // its pty is closing anyway; nothing to restore
            }
            if live.insert(id.clone()) && !self.applied.contains_key(id) {
                self.log.write(&format!(
                    "surface {} registered (session {})",
                    ps.pane_tty, ps.session_id
                ));
            }

            let sess = state::read_session(&ps.session_id).unwrap_or_default();
            let activity = crate::flag::activity(&sess, ps.claude_pid, &self.signals, now);
            let git = sess
                .cwd
                .as_deref()
                .and_then(|c| self.git.get(c))
                .and_then(|g| g.as_ref());
            let title = crate::flag::render_label(flag, &sess, activity, git);

            if self.applied.get(id).map(|(_, t)| t.as_str()) != Some(title.as_str()) {
                write_pty(&ps.pane_tty, &osc_title(&title));
                self.applied
                    .insert(id.clone(), (ps.pane_tty.clone(), title));
            }

            // Edge-triggered completion notification: fire once when attention
            // *becomes* true (a turn just finished unviewed). `Some(false)`
            // guards against firing on the daemon's first sight of an
            // already-flagged surface; the view stamp above suppresses it while
            // the surface is on screen.
            let attn = crate::fleet::attention(activity, sess.last_turn_ts, sess.last_view_ts);
            if flag.notify && self.prev_attn.get(id) == Some(&false) && attn {
                write_pty(&ps.pane_tty, &osc_notify("Claude", &completion_body(&sess)));
                self.log
                    .write(&format!("notified completion on {}", ps.pane_tty));
            }
            self.prev_attn.insert(id.clone(), attn);
        }

        // Surfaces we stamped but that are no longer live: clear the title.
        let gone: Vec<String> = self
            .applied
            .keys()
            .filter(|id| !live.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            if let Some((pty, _)) = self.applied.remove(&id) {
                write_pty(&pty, &osc_title(""));
                self.log
                    .write(&format!("surface {pty} gone; title cleared"));
            }
        }
        // Forget notification edge-state for gone surfaces, so a returning
        // session re-notifies rather than being suppressed by a stale `true`.
        self.prev_attn.retain(|id, _| live.contains(id));

        live
    }

    /// Refresh the expensive caches: one `ps` snapshot for the bg/suspended
    /// signals, and a git status per distinct surface cwd.
    fn refresh_heavy(&mut self, states: &[(String, PaneState)]) {
        let pids: HashSet<u32> = states.iter().map(|(_, p)| p.claude_pid).collect();
        self.signals = crate::flag::PsSignals::capture(&pids);
        let mut git = HashMap::new();
        for (_, ps) in states {
            if let Some(cwd) = state::read_session(&ps.session_id).and_then(|s| s.cwd)
                && !git.contains_key(&cwd)
            {
                let status = crate::git::status(cwd.as_str());
                git.insert(cwd, status);
            }
        }
        self.git = git;
        self.last_heavy = Some(Instant::now());
    }

    /// Stamp `last_view_ts` on the session of the surface the user is currently
    /// viewing in Ghostty — the Ghostty analog of the tmux handler's per-tick
    /// view stamp. Its attention flag then clears (and its completion
    /// notification is suppressed) while it's on screen.
    ///
    /// A surface is "viewed" when Ghostty is frontmost (the cheap `lsappinfo`
    /// gate in `run`, no Automation permission) *and* the focused terminal is
    /// one of ours — matched by the tab title we set (unique against non-Claude
    /// tabs and Claudes in a different state) corroborated by the working
    /// directory. The Automation grant is needed for the title/cwd query;
    /// without it (or on a non-macOS host, where the probes are no-ops) the flag
    /// clears on the next prompt instead. Residual conflation: two Claudes in
    /// the same directory showing the same activity glyph.
    fn stamp_views(&mut self, states: &[(String, PaneState)], frontmost: bool) {
        let view = if states.is_empty() || !frontmost {
            Viewed::Away
        } else if let Some((name, cwd)) = focused_terminal() {
            let hit = states.iter().find(|(id, ps)| {
                self.applied.get(id).map(|(_, t)| t.as_str()) == Some(name.as_str())
                    && state::read_session(&ps.session_id)
                        .and_then(|s| s.cwd)
                        .as_deref()
                        == Some(cwd.as_str())
            });
            match hit {
                Some((_, ps)) => {
                    if let Some(mut s) = state::read_session(&ps.session_id) {
                        s.last_view_ts = Some(now_unix());
                        let _ = state::write_session(&ps.session_id, &s);
                    }
                    Viewed::Surface(ps.pane_tty.clone())
                }
                None => Viewed::OtherTab,
            }
        } else {
            // Frontmost but the AppleScript query returned nothing — Automation
            // permission not granted, or no window. The likely permission tell.
            Viewed::ScriptUnavailable
        };

        if view != self.prev_view {
            self.log.write(&format!("view -> {}", view.label()));
            self.prev_view = view;
        }
    }

    /// Clear the title on every surface we stamped (daemon shutdown / flag off).
    /// We don't touch the progress bar — Claude Code owns OSC 9;4 natively.
    fn restore_all(&self) {
        for (pty, _) in self.applied.values() {
            write_pty(pty, &osc_title(""));
        }
    }
}

/// The notification body for a completed turn: the session's directory
/// basename, or a neutral fallback.
fn completion_body(sess: &crate::state::SessionState) -> String {
    let dir = sess
        .cwd
        .as_deref()
        .map(|p| p.trim_end_matches('/'))
        .and_then(|p| p.rsplit('/').find(|c| !c.is_empty()))
        .filter(|c| !c.is_empty());
    match dir {
        Some(d) => format!("Finished in {d}"),
        None => "Finished".to_string(),
    }
}

/// What the view probe resolved to on a tick, tracked so transitions are logged
/// once (not every tick). `ScriptUnavailable` is the diagnostic case: Ghostty is
/// frontmost but the scripting query failed, which usually means the Automation
/// permission hasn't been granted.
#[derive(Clone, PartialEq)]
enum Viewed {
    Away,
    ScriptUnavailable,
    OtherTab,
    Surface(String),
}

impl Viewed {
    fn label(&self) -> String {
        match self {
            Viewed::Away => "away (Ghostty not frontmost / no surfaces)".to_string(),
            Viewed::ScriptUnavailable => {
                "frontmost, scripting unavailable (Automation not granted?)".to_string()
            }
            Viewed::OtherTab => "other (non-Claude) tab".to_string(),
            Viewed::Surface(pty) => format!("surface {pty}"),
        }
    }
}

/// Whether Ghostty is the frontmost application, via `lsappinfo` (a public API
/// needing no Automation permission — the cheap gate before any scripting).
#[cfg(target_os = "macos")]
fn ghostty_frontmost() -> bool {
    let Ok(front) = Command::new("lsappinfo").arg("front").output() else {
        return false;
    };
    let asn = String::from_utf8_lossy(&front.stdout);
    let asn = asn.trim();
    if asn.is_empty() {
        return false;
    }
    let Ok(info) = Command::new("lsappinfo")
        .args(["info", "-only", "name", asn])
        .output()
    else {
        return false;
    };
    // lsappinfo prints `"LSDisplayName"="Ghostty"`.
    String::from_utf8_lossy(&info.stdout).contains("=\"Ghostty\"")
}

/// The `(title, working directory)` of the terminal the user is focused on in
/// Ghostty, or `None` when Ghostty isn't frontmost or the scripting call fails
/// (no Automation grant / no window). Needs the one-time Automation permission.
#[cfg(target_os = "macos")]
fn focused_terminal() -> Option<(String, String)> {
    let script = r#"tell application "Ghostty"
  if not frontmost then return ""
  set t to focused terminal of selected tab of front window
  return (name of t) & linefeed & (working directory of t)
end tell"#;
    let out = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim_end_matches('\n');
    if s.is_empty() {
        return None;
    }
    let (name, cwd) = s.split_once('\n')?;
    Some((name.to_string(), cwd.to_string()))
}

#[cfg(not(target_os = "macos"))]
fn ghostty_frontmost() -> bool {
    false
}

#[cfg(not(target_os = "macos"))]
fn focused_terminal() -> Option<(String, String)> {
    None
}

/// Write bytes to a pty, best-effort. `O_NOCTTY` is essential: the daemon is a
/// session leader (post-`setsid`) with no controlling terminal, so opening a
/// tty *without* it would make that tty our controlling terminal. `O_NONBLOCK`
/// avoids blocking if the pty's output is flow-controlled.
fn write_pty(pty: &str, data: &str) {
    if let Ok(mut f) = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(pty)
    {
        let _ = f.write_all(data.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_ghostty_only() {
        assert!(is_ghostty(Some("ghostty")));
        assert!(!is_ghostty(Some("tmux"))); // ghostty-in-tmux is driven by tmux
        assert!(!is_ghostty(Some("iTerm.app")));
        assert!(!is_ghostty(Some("Apple_Terminal")));
        assert!(!is_ghostty(None));
    }

    #[test]
    fn osc_title_wraps_and_strips_control_chars() {
        assert_eq!(osc_title("⚑ myproj"), "\x1b]2;⚑ myproj\x07");
        assert_eq!(osc_title(""), "\x1b]2;\x07");
        // An embedded terminator / newline can't break out of the sequence.
        assert_eq!(osc_title("a\x07b\nc\x1b"), "\x1b]2;abc\x07");
    }

    #[test]
    fn osc_notify_formats_and_strips_separators() {
        assert_eq!(
            osc_notify("Claude", "Finished in demoproj"),
            "\x1b]777;notify;Claude;Finished in demoproj\x07"
        );
        // Stray `;` / control chars can't inject extra fields or terminate early.
        assert_eq!(osc_notify("a;b", "c\x07d\ne"), "\x1b]777;notify;ab;cde\x07");
    }

    #[test]
    fn completion_body_uses_dir_basename() {
        let mut s = crate::state::SessionState::default();
        assert_eq!(completion_body(&s), "Finished");
        s.cwd = Some("/Users/x/repo/demoproj/".to_string());
        assert_eq!(completion_body(&s), "Finished in demoproj");
    }
}
