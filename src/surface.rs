//! Direct-terminal backend: detection, surface identity, and the polling
//! daemon. Drives a Claude running *directly* in a GUI terminal emulator
//! (Ghostty, iTerm2, Terminal.app) — no tmux — where the tmux backend's
//! per-session control-mode handler doesn't apply.
//!
//! Such a Claude has no tmux pane id to key its surface on. The addressing
//! handle is instead the **pty path** the emulator allocated for the session —
//! the role tmux's `%N` pane id plays everywhere else. It's resolved from the
//! *Claude* pid, not from the statusline process: Claude Code spawns the
//! statusline detached with no controlling tty, so the statusline can't read
//! its own — but the interactive Claude process's controlling tty *is* the
//! emulator pty (see [`crate::util::pid_tty`]).
//!
//! None of these emulators offer tmux's per-session event stream, so the
//! backend is a **single polling daemon**: it enumerates the registered
//! surfaces every tick and drives two per-surface indicators —
//!
//! - **tab title** (OSC 2, a plain pty escape honored by all three) carrying
//!   the same activity flag the tmux backend puts on the window name (see
//!   [`crate::config::WindowFlag`]); titles are last-writer-wins, so it's
//!   re-asserted every tick;
//! - a **desktop notification**, edge-triggered on an unviewed completion when
//!   `windowFlag.notify` is set. The mechanism is per-emulator ([`Emulator`]):
//!   OSC 777 (Ghostty), OSC 9 (iTerm2), or a native notification (Terminal.app,
//!   which has no notification escape).
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

/// The `server`/`pane` directory namespace for direct-terminal surfaces. These
/// emulators have no tmux server, so all surfaces (across every window, every
/// emulator) share one namespace and one daemon — distinct from the
/// per-tmux-server hashes.
pub const SURFACE_SERVER_ID: &str = "surface";

/// Lock/socket basename for the singleton daemon (there is one, not one per
/// session as in tmux).
const DAEMON_KEY: &str = "daemon";

/// Poll cadence. These emulators give us no events, so polling is the only
/// clock — but we adapt it: poll fast while a supported emulator is frontmost
/// (you're looking, so the flag clearing / activity glyph should feel
/// responsive), and slowly otherwise (backgrounded — e.g. you're in another app
/// — so keep the ps/git/scripting cost low).
const POLL_ACTIVE: Duration = Duration::from_secs(1);
const POLL_IDLE: Duration = Duration::from_secs(3);

/// Grace after the last surface goes before the daemon exits.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(5);

/// Whether this process is hosted directly by a supported terminal emulator
/// (cheap env check, no process-tree walk). Gate the pid resolution behind this
/// so unrecognized terminals — and tmux, whose `TERM_PROGRAM` is `tmux` and is
/// driven by the tmux backend — don't pay for it.
pub fn is_active() -> bool {
    Emulator::from_term_program(env::var("TERM_PROGRAM").ok().as_deref()).is_some()
}

/// `Some(pty_path)` when Claude is running directly in a supported emulator: the
/// surface id (the emulator's pty, resolved from `claude_pid`). `None` outside
/// one, or when the pty can't be resolved (no controlling terminal). The caller
/// passes the already-resolved interactive-Claude pid.
pub fn active_surface(claude_pid: u32) -> Option<String> {
    if !is_active() {
        return None;
    }
    util::pid_tty(claude_pid)
}

/// A GUI terminal emulator hosting a direct (non-tmux) Claude surface. The
/// polling daemon is emulator-neutral except for three things this type owns:
/// which app counts as frontmost, how to identify the surface the user is
/// looking at, and how to raise a completion notification. Everything else —
/// pty identity, OSC 2 title stamping, the poll loop — is shared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Emulator {
    Ghostty,
    ITerm2,
    TerminalApp,
}

/// How the daemon identifies the surface the user is currently looking at, per
/// emulator. Both are stable handles independent of the tab title, so the probe
/// works even when Claude Code owns the title.
///
/// `allow(dead_code)`: the variants are constructed only by the macOS focus
/// probe, so both read as unused when compiling for other platforms.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum FocusKey {
    /// iTerm2 / Terminal.app: the focused tab's controlling tty, matched exactly
    /// against the surface `pane_tty` (precise — no same-dir conflation).
    Tty(String),
    /// Ghostty: the focused terminal's scripting `id`, matched against the id we
    /// learned for each surface via the one-shot title handshake (Ghostty exposes
    /// no tty, but its ids are stable and title-independent).
    GhosttyId(String),
}

impl Emulator {
    /// Recognize the emulator from `TERM_PROGRAM`. `None` inside tmux (its
    /// `TERM_PROGRAM` is `tmux`, driven by the tmux backend) or for an
    /// unrecognized/absent value.
    fn from_term_program(tp: Option<&str>) -> Option<Self> {
        match tp {
            Some("ghostty") => Some(Self::Ghostty),
            Some("iTerm.app") => Some(Self::ITerm2),
            Some("Apple_Terminal") => Some(Self::TerminalApp),
            _ => None,
        }
    }

    /// Raise a completion notification for a finished turn on `pty`'s surface.
    /// Ghostty and iTerm2 take a pty escape (OSC 777 / OSC 9); Terminal.app has
    /// no notification escape, so it gets a native macOS notification.
    fn notify(self, pty: &str, title: &str, body: &str) {
        match self {
            Self::Ghostty => write_pty(pty, &osc_notify(title, body)),
            Self::ITerm2 => write_pty(pty, &osc9_notify(&format!("{title}: {body}"))),
            Self::TerminalApp => display_notification(title, body),
        }
    }
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

/// The iTerm2 OSC 9 notification: `ESC ] 9 ; <message> BEL`. iTerm2 renders the
/// single message field as a desktop notification; control chars are stripped so
/// it can't corrupt the escape.
pub fn osc9_notify(message: &str) -> String {
    let clean: String = message.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]9;{clean}\x07")
}

/// Registrar side (runs in the statusline process): record this direct-terminal
/// surface so the daemon can find it, and make sure the daemon is running. No-op
/// unless we're in a supported emulator and the window flag is enabled (the only
/// surface this phase drives). `claude_pid` is the already-resolved
/// interactive-Claude pid.
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
    cmd.arg("--surface-daemon");
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

/// Daemon entrypoint (`--surface-daemon`). Single instance, guarded by a lock.
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

    let mut daemon = Daemon::new(DaemonLog::for_surface());
    daemon.log.write("surface daemon started");
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
        // adaptive cadence: which supported emulator (if any) is frontmost.
        let front = frontmost_emulator();
        let live = daemon.tick(&flag, front);
        if !live.is_empty() {
            last_activity = Instant::now();
        }
        if live.is_empty() && last_activity.elapsed() > IDLE_EXIT_AFTER {
            daemon.log.write("idle with no surfaces; exiting");
            daemon.restore_all();
            return ExitCode::SUCCESS;
        }
        thread::sleep(if front.is_some() {
            POLL_ACTIVE
        } else {
            POLL_IDLE
        });
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
    /// Whether we've already logged the "Claude Code owns the title" hint (once
    /// per daemon life, so the log isn't spammed each tick).
    title_hinted: bool,
    /// Ghostty surface pty -> its scripting terminal `id`, learned once per
    /// surface via the title handshake. Lets the view probe match the focused
    /// Ghostty terminal by a stable, title-independent handle (Ghostty exposes no
    /// tty), so it works even when Claude Code owns the visible title.
    ghostty_ids: HashMap<String, String>,
    /// Ghostty ptys whose handshake has failed, so the "could not map" note is
    /// logged once each rather than every retry tick.
    ghostty_missed: HashSet<String>,
    /// Monotonic counter making each handshake sentinel unique (no wall clock /
    /// RNG available in a way that survives resume; a counter suffices).
    handshake_seq: u64,
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
            title_hinted: false,
            ghostty_ids: HashMap::new(),
            ghostty_missed: HashSet::new(),
            handshake_seq: 0,
        }
    }

    /// One poll pass: refresh the caches if due, stamp the viewed surface, then
    /// stamp each live surface's title (OSC 2), fire the completion notification
    /// (OSC 777) on the unviewed-completion edge, drop dead ones, and clear the
    /// title for surfaces that have gone. Returns the live surface ids.
    fn tick(&mut self, flag: &WindowFlag, front: Option<Emulator>) -> HashSet<String> {
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
        self.stamp_views(&states, front);

        let mut live = HashSet::new();
        for (id, ps) in &states {
            if !pid_alive(ps.claude_pid) {
                state::remove_pane(SURFACE_SERVER_ID, id);
                self.ghostty_ids.remove(&ps.pane_tty);
                self.ghostty_missed.remove(&ps.pane_tty);
                self.log
                    .write(&format!("surface {} pruned (Claude exited)", ps.pane_tty));
                continue; // its pty is closing anyway; nothing to restore
            }
            // First sight of this surface (tracked via `prev_attn`, which is
            // written for every live surface at the end of the loop — unlike
            // `applied`, which is only populated when we actually stamp a title).
            if live.insert(id.clone()) && !self.prev_attn.contains_key(id) {
                self.log.write(&format!(
                    "surface {} registered (session {})",
                    ps.pane_tty, ps.session_id
                ));
            }

            let sess = state::read_session(&ps.session_id).unwrap_or_default();

            // Learn this Ghostty surface's terminal id once (stable, cached), via
            // the title handshake — the view probe then matches the focused
            // terminal by id, independent of who owns the visible title.
            if matches!(
                Emulator::from_term_program(sess.term_program.as_deref()),
                Some(Emulator::Ghostty)
            ) && !self.ghostty_ids.contains_key(&ps.pane_tty)
            {
                self.handshake_seq += 1;
                if let Some(gid) = ghostty_id_for_pty(&ps.pane_tty, self.handshake_seq) {
                    self.ghostty_missed.remove(&ps.pane_tty);
                    self.log.write(&format!(
                        "mapped surface {} -> ghostty {}",
                        ps.pane_tty, gid
                    ));
                    self.ghostty_ids.insert(ps.pane_tty.clone(), gid);
                } else if self.ghostty_missed.insert(ps.pane_tty.clone()) {
                    // Log the miss once per surface (not every retry): usually
                    // Ghostty isn't scriptable yet or the Automation grant is
                    // missing. We keep retrying quietly on later ticks.
                    self.log.write(&format!(
                        "could not map ghostty surface {} (Automation not granted, or Ghostty not scriptable?)",
                        ps.pane_tty
                    ));
                }
            }

            let activity = crate::flag::activity(&sess, ps.claude_pid, &self.signals, now);
            let git = sess
                .cwd
                .as_deref()
                .and_then(|c| self.git.get(c))
                .and_then(|g| g.as_ref());
            let title = crate::flag::render_label(flag, &sess, activity, git);

            // Stamp the flag into the tab title only when Claude Code has ceded
            // it (CLAUDE_CODE_DISABLE_TERMINAL_TITLE). Otherwise Claude owns the
            // title and we don't fight over the same OSC 2 — the flag glyph just
            // waits on the title being free. (Surface identity for view/jump
            // doesn't ride on the visible flag; see the id handshake.)
            if sess.cc_title_disabled {
                if self.applied.get(id).map(|(_, t)| t.as_str()) != Some(title.as_str()) {
                    write_pty(&ps.pane_tty, &osc_title(&title));
                    self.applied
                        .insert(id.clone(), (ps.pane_tty.clone(), title));
                }
            } else if !self.title_hinted {
                self.log.write(
                    "windowFlag on but Claude Code owns the tab title; set \
                     CLAUDE_CODE_DISABLE_TERMINAL_TITLE=1 to show the flag there",
                );
                self.title_hinted = true;
            }

            // Edge-triggered completion notification: fire once when attention
            // *becomes* true (a turn just finished unviewed). `Some(false)`
            // guards against firing on the daemon's first sight of an
            // already-flagged surface; the view stamp above suppresses it while
            // the surface is on screen.
            let attn = crate::fleet::attention(activity, sess.last_turn_ts, sess.last_view_ts);
            if flag.notify
                && self.prev_attn.get(id) == Some(&false)
                && attn
                && let Some(e) = Emulator::from_term_program(sess.term_program.as_deref())
            {
                e.notify(&ps.pane_tty, "Claude", &completion_body(&sess));
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
        // Drop cached ids for gone surfaces (a returning pty re-handshakes).
        self.ghostty_ids.retain(|pty, _| live.contains(pty));
        self.ghostty_missed.retain(|pty| live.contains(pty));

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
    /// viewing — the direct-terminal analog of the tmux handler's per-tick view
    /// stamp. Its attention flag then clears (and its completion notification is
    /// suppressed) while it's on screen.
    ///
    /// A surface is "viewed" when its emulator is frontmost (the cheap
    /// `lsappinfo` gate in `run`, no Automation permission) *and* the emulator's
    /// focused tab is one of ours. iTerm2/Terminal expose the focused tab's tty,
    /// so the match is exact; Ghostty exposes no tty, so it matches the title we
    /// set (unique against non-Claude tabs and Claudes in a different state)
    /// corroborated by cwd — residual conflation there is two Claudes in the
    /// same directory showing the same glyph. The Automation grant is needed for
    /// the focus query; without it (or on a non-macOS host, where the probes are
    /// no-ops) the flag clears on the next prompt instead.
    fn stamp_views(&mut self, states: &[(String, PaneState)], front: Option<Emulator>) {
        let view = match front {
            _ if states.is_empty() => Viewed::Away,
            None => Viewed::Away,
            Some(e) => match e.focused_surface() {
                Some(key) => match self.match_surface(states, e, &key) {
                    Some(ps) => {
                        if let Some(mut s) = state::read_session(&ps.session_id) {
                            s.last_view_ts = Some(now_unix());
                            let _ = state::write_session(&ps.session_id, &s);
                        }
                        Viewed::Surface(ps.pane_tty.clone())
                    }
                    None => Viewed::OtherTab,
                },
                // Frontmost but the focus query returned nothing — Automation
                // permission not granted, or no window. The likely permission
                // tell.
                None => Viewed::ScriptUnavailable,
            },
        };

        if view != self.prev_view {
            self.log.write(&format!("view -> {}", view.label()));
            self.prev_view = view;
        }
    }

    /// The registered surface a focus key points at, restricted to surfaces of
    /// emulator `e` (so one emulator's key can't match another's). `Tty` is an
    /// exact `pane_tty` match; `GhosttyId` matches the id we learned for the
    /// surface via the handshake.
    fn match_surface<'a>(
        &self,
        states: &'a [(String, PaneState)],
        e: Emulator,
        key: &FocusKey,
    ) -> Option<&'a PaneState> {
        states.iter().find_map(|(_, ps)| {
            let sess = state::read_session(&ps.session_id).unwrap_or_default();
            if Emulator::from_term_program(sess.term_program.as_deref()) != Some(e) {
                return None;
            }
            let hit = match key {
                FocusKey::Tty(tty) => &ps.pane_tty == tty,
                FocusKey::GhosttyId(id) => {
                    self.ghostty_ids.get(&ps.pane_tty).map(String::as_str) == Some(id.as_str())
                }
            };
            hit.then_some(ps)
        })
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
/// once (not every tick). `ScriptUnavailable` is the diagnostic case: an
/// emulator is frontmost but the scripting query failed, which usually means the
/// Automation permission hasn't been granted.
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
            Viewed::Away => "away (no supported emulator frontmost / no surfaces)".to_string(),
            Viewed::ScriptUnavailable => {
                "frontmost, scripting unavailable (Automation not granted?)".to_string()
            }
            Viewed::OtherTab => "other (non-Claude) tab".to_string(),
            Viewed::Surface(pty) => format!("surface {pty}"),
        }
    }
}

/// The frontmost application as one of our supported emulators, or `None` when
/// something else is frontmost. One `lsappinfo` query (a public API needing no
/// Automation permission — the cheap gate before any scripting) drives both the
/// view probe and the adaptive cadence.
#[cfg(target_os = "macos")]
fn frontmost_emulator() -> Option<Emulator> {
    let front = Command::new("lsappinfo").arg("front").output().ok()?;
    let asn = String::from_utf8_lossy(&front.stdout);
    let asn = asn.trim();
    if asn.is_empty() {
        return None;
    }
    let info = Command::new("lsappinfo")
        .args(["info", "-only", "name", asn])
        .output()
        .ok()?;
    // lsappinfo prints `"LSDisplayName"="Ghostty"`.
    let name = String::from_utf8_lossy(&info.stdout);
    [Emulator::Ghostty, Emulator::ITerm2, Emulator::TerminalApp]
        .into_iter()
        .find(|e| name.contains(&format!("=\"{}\"", e.app_name())))
}

#[cfg(target_os = "macos")]
impl Emulator {
    /// The `LSDisplayName` lsappinfo reports for the app, used to map the
    /// frontmost app back to an emulator.
    fn app_name(self) -> &'static str {
        match self {
            Self::Ghostty => "Ghostty",
            Self::ITerm2 => "iTerm2",
            Self::TerminalApp => "Terminal",
        }
    }

    /// The focus key for the tab the user is looking at, or `None` when this
    /// emulator isn't frontmost or the scripting call fails (no Automation grant
    /// / no window). Needs the one-time Automation permission.
    fn focused_surface(self) -> Option<FocusKey> {
        match self {
            Self::Ghostty => run_osascript_line(
                r#"tell application "Ghostty"
  if not frontmost then return ""
  return id of focused terminal of selected tab of front window
end tell"#,
            )
            .map(FocusKey::GhosttyId),
            Self::ITerm2 => run_osascript_line(
                r#"tell application "iTerm2"
  if not frontmost then return ""
  return tty of current session of current window
end tell"#,
            )
            .map(FocusKey::Tty),
            Self::TerminalApp => run_osascript_line(
                r#"tell application "Terminal"
  if not frontmost then return ""
  return tty of selected tab of front window
end tell"#,
            )
            .map(FocusKey::Tty),
        }
    }
}

/// Run an AppleScript that returns a (possibly multi-line) string, yielding its
/// trimmed stdout or `None` on failure / empty output.
#[cfg(target_os = "macos")]
fn run_osascript_line(script: &str) -> Option<String> {
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
    (!s.is_empty()).then(|| s.to_string())
}

/// Every Ghostty terminal as `(id, name)`. The `id` is a stable,
/// title-independent scripting handle; `name` is the current tab title (used
/// only to spot our sentinel during the handshake).
#[cfg(target_os = "macos")]
fn ghostty_terminals() -> Vec<(String, String)> {
    let script = r#"tell application "Ghostty"
  set out to ""
  repeat with w in windows
    repeat with t in terminals of w
      set out to out & (id of t) & tab & (name of t) & linefeed
    end repeat
  end repeat
  return out
end tell"#;
    let Ok(out) = Command::new("osascript").arg("-e").arg(script).output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(id, name)| (id.to_string(), name.to_string()))
        .collect()
}

/// Learn the Ghostty scripting `id` of the terminal on `pty` via a one-shot title
/// handshake: snapshot the terminals, plant a unique sentinel title through the
/// pty, find the terminal that now carries it, then restore its prior title.
/// `None` when Ghostty isn't scriptable or the sentinel was clobbered mid-window
/// by a concurrent title write — the caller just retries next tick. Because the
/// id is stable and title-independent, this runs once per surface.
#[cfg(target_os = "macos")]
fn ghostty_id_for_pty(pty: &str, nonce: u64) -> Option<String> {
    let before = ghostty_terminals();
    if before.is_empty() {
        return None;
    }
    let sentinel = format!("ccstatus-map-{nonce}");
    write_pty(pty, &osc_title(&sentinel));
    thread::sleep(Duration::from_millis(80));
    let id = ghostty_terminals()
        .into_iter()
        .find(|(_, name)| *name == sentinel)
        .map(|(id, _)| id)?;
    // Hand the title back: restore what the terminal showed before we borrowed
    // it. (In owned mode the daemon re-stamps the flag next tick anyway.)
    if let Some((_, old)) = before.iter().find(|(i, _)| *i == id) {
        write_pty(pty, &osc_title(old));
    }
    Some(id)
}

#[cfg(not(target_os = "macos"))]
fn ghostty_id_for_pty(_pty: &str, _nonce: u64) -> Option<String> {
    None
}

/// Fire a native macOS notification — Terminal.app has no notification escape,
/// so `display notification` is how its completions surface.
#[cfg(target_os = "macos")]
fn display_notification(title: &str, body: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        esc(body),
        esc(title)
    );
    let _ = Command::new("osascript").arg("-e").arg(&script).output();
}

#[cfg(not(target_os = "macos"))]
fn frontmost_emulator() -> Option<Emulator> {
    None
}

#[cfg(not(target_os = "macos"))]
impl Emulator {
    fn focused_surface(self) -> Option<FocusKey> {
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn display_notification(_title: &str, _body: &str) {}

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
    fn osc_title_wraps_and_strips_control_chars() {
        assert_eq!(osc_title("⚑ myproj"), "\x1b]2;⚑ myproj\x07");
        assert_eq!(osc_title(""), "\x1b]2;\x07");
        // An embedded terminator / newline can't break out of the sequence.
        assert_eq!(osc_title("a\x07b\nc\x1b"), "\x1b]2;abc\x07");
    }

    #[test]
    fn emulator_recognized_from_term_program() {
        assert_eq!(
            Emulator::from_term_program(Some("ghostty")),
            Some(Emulator::Ghostty)
        );
        assert_eq!(
            Emulator::from_term_program(Some("iTerm.app")),
            Some(Emulator::ITerm2)
        );
        assert_eq!(
            Emulator::from_term_program(Some("Apple_Terminal")),
            Some(Emulator::TerminalApp)
        );
        // tmux is driven by the tmux backend, not the direct-terminal daemon.
        assert_eq!(Emulator::from_term_program(Some("tmux")), None);
        assert_eq!(Emulator::from_term_program(None), None);
    }

    #[test]
    fn osc9_notify_wraps_and_strips_control_chars() {
        assert_eq!(
            osc9_notify("Claude: Finished in demoproj"),
            "\x1b]9;Claude: Finished in demoproj\x07"
        );
        // An embedded terminator / newline can't break out of the sequence.
        assert_eq!(osc9_notify("a\x07b\nc"), "\x1b]9;abc\x07");
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
