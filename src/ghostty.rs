//! Ghostty backend: detection, surface identity, and the title daemon.
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
//! and stamps each one's tab title (OSC 2) with the same activity flag the tmux
//! backend puts on the window name (see [`crate::config::WindowFlag`]). Ghostty
//! titles are last-writer-wins, so the flag is re-asserted every tick.

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
use crate::server_dir::ServerDir;
use crate::state::{self, PaneState};
use crate::util::{self, now_unix, pid_alive, resolve_session_id};

/// The `server`/`pane` directory namespace for Ghostty surfaces. Ghostty has no
/// tmux server, so all surfaces (across every Ghostty window) share one
/// namespace and one daemon — distinct from the per-tmux-server hashes.
const SURFACE_SERVER_ID: &str = "ghostty";

/// Lock/socket basename for the singleton daemon (there is one, not one per
/// session as in tmux).
const DAEMON_KEY: &str = "daemon";

/// Poll cadence — matches the tmux handler's timer. Ghostty gives us no events,
/// so this is the *only* clock: focus/activity are re-derived each tick.
const TICK: Duration = Duration::from_secs(3);

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

/// Daemon entrypoint (`--ghostty-daemon`). Single instance, guarded by a lock;
/// polls the registered surfaces each tick and stamps their titles.
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

    // surface id -> (pty path, last title we wrote) — for change detection
    // (skip redundant writes) and restore (clear the title when a surface
    // leaves or the flag is turned off).
    let mut applied: HashMap<String, (String, String)> = HashMap::new();
    let mut last_activity = Instant::now();

    loop {
        let flag = WindowFlag::load();
        if !flag.enabled {
            // The title is the only surface this phase drives; nothing to do.
            restore_all(&applied);
            return ExitCode::SUCCESS;
        }

        let live = stamp_titles(&flag, &mut applied);
        if !live.is_empty() {
            last_activity = Instant::now();
        }
        if live.is_empty() && last_activity.elapsed() > IDLE_EXIT_AFTER {
            restore_all(&applied);
            return ExitCode::SUCCESS;
        }
        thread::sleep(TICK);
    }
}

/// One poll pass: stamp every live surface's title with its flag, drop dead
/// ones, and restore titles for surfaces that have gone. Returns the set of
/// live surface ids.
fn stamp_titles(
    flag: &WindowFlag,
    applied: &mut HashMap<String, (String, String)>,
) -> HashSet<String> {
    let surfaces = state::list_panes(SURFACE_SERVER_ID);

    // One `ps` snapshot per tick feeds the bg-task / suspended signals.
    let states: Vec<(String, PaneState)> = surfaces
        .into_iter()
        .filter_map(|id| state::read_pane(SURFACE_SERVER_ID, &id).map(|p| (id, p)))
        .collect();
    let pids: HashSet<u32> = states.iter().map(|(_, p)| p.claude_pid).collect();
    let procs = if pids.is_empty() {
        Vec::new()
    } else {
        util::ps_snapshot()
    };
    let bg = util::background_task_pids(&procs, &pids);
    let susp = util::suspended_pids(&procs, &pids);
    let now = now_unix();

    let mut live = HashSet::new();
    for (id, ps) in states {
        if !pid_alive(ps.claude_pid) {
            state::remove_pane(SURFACE_SERVER_ID, &id);
            continue; // its pty is closing anyway; nothing to restore
        }
        live.insert(id.clone());

        let sess = state::read_session(&ps.session_id).unwrap_or_default();
        let idle_secs = sess.last_turn_ts.map(|t| (now - t).max(0));
        let act = crate::fleet::activity(
            sess.last_prompt_ts,
            sess.last_turn_ts,
            susp.contains(&ps.claude_pid),
            sess.last_notify_ts.is_some(),
            bg.contains(&ps.claude_pid),
            idle_secs,
        );
        let attn = crate::fleet::attention(act, sess.last_turn_ts, sess.last_view_ts);
        let git = sess.cwd.as_deref().and_then(crate::git::status);
        let title = flag.render(act, attn, git.as_ref(), sess.cwd.as_deref());

        if applied.get(&id).map(|(_, t)| t.as_str()) != Some(title.as_str()) {
            write_pty(&ps.pane_tty, &osc_title(&title));
            applied.insert(id, (ps.pane_tty, title));
        }
    }

    // Surfaces we stamped but that are no longer live: clear our title override.
    let gone: Vec<String> = applied
        .keys()
        .filter(|id| !live.contains(*id))
        .cloned()
        .collect();
    for id in gone {
        if let Some((pty, _)) = applied.remove(&id) {
            write_pty(&pty, &osc_title(""));
        }
    }

    live
}

/// Clear the title on every surface we stamped (daemon shutdown / flag off).
fn restore_all(applied: &HashMap<String, (String, String)>) {
    for (pty, _) in applied.values() {
        write_pty(pty, &osc_title(""));
    }
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
}
