//! Raising the OS terminal window that hosts a Claude session — the non-tmux
//! half of "take me to this Claude" (jump).
//!
//! Inside tmux, [`crate::tmux::Tmux::focus_pane`] is the whole jump: it switches
//! the attached client onto the pane, which appears in the window you're already
//! looking at. A Claude running *directly* in a terminal emulator has no tmux to
//! switch — the actuator is the emulator's own scripting interface, and it's
//! per-platform. This module covers macOS (iTerm2 and Terminal.app) and Linux.
//!
//! Addressing comes from the session's presence record (see
//! [`crate::state::SessionState`]), captured by the registrar from its
//! environment:
//! - **iTerm2** (macOS) — `ITERM_SESSION_ID` is `wNtNpN:GUID`; the GUID is
//!   exactly what iTerm2's AppleScript `id of session` returns, so we address
//!   the session directly (stable across tab moves, unlike a tty).
//! - **Terminal.app** (macOS) — no per-session id, so we match the controlling
//!   tty of the Claude process (`ps -o tty=`), which equals a Terminal tab's
//!   `tty`.
//! - **Linux** — there is no portable per-session window handle, so we defer to
//!   a *jump command* (the user's `jump.linux`, else a bundled best-effort X11
//!   script) that maps the Claude pid to its terminal window at focus time. A
//!   session is window-jumpable only when it has a graphical display, so a
//!   headless/SSH Claude stays correctly non-jumpable.
//!
//! On any other platform [`focus`] is a no-op and [`target_for`] yields `None`,
//! so non-tmux sessions stay non-jumpable there.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// A non-tmux terminal window/tab hosting a Claude session, addressable for
/// "raise this window". Built purely from presence fields by [`target_for`];
/// the actual window lookup (an extra `ps`, or the WM query the jump command
/// runs) is resolved lazily in [`focus`], only when a jump actually fires.
///
/// `allow(dead_code)`: which variants are constructed is platform-dependent —
/// the macOS variants on macOS, [`LinuxWindow`](WindowTarget::LinuxWindow) on
/// Linux — so each is "unused" on the other platform.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowTarget {
    /// iTerm2 session, addressed by its stable session GUID (macOS).
    ITerm2 { session_id: String },
    /// Terminal.app tab, matched by the Claude process's controlling tty
    /// (resolved at focus time from this pid) (macOS).
    TerminalApp { claude_pid: u32 },
    /// A Linux terminal window, raised by mapping this pid (or an ancestor —
    /// the emulator) to its window via the jump command at focus time.
    LinuxWindow { claude_pid: u32 },
    /// A Ghostty terminal (macOS), focused via Ghostty's AppleScript `focus`
    /// command. Matched by the tab title we set (`title`, the activity flag —
    /// distinct from a plain shell tab in the same dir) and corroborated by
    /// working directory; falls back to the first terminal in `cwd` when the
    /// title doesn't match (e.g. the flag changed since the fleet was read).
    Ghostty { cwd: String, title: Option<String> },
}

/// Pure: derive a window target from a session's captured terminal identity.
/// `None` when the session isn't window-jumpable: inside tmux (`TERM_PROGRAM`
/// is `tmux` — jump via `focus_pane` instead), when the terminal is
/// unrecognised, when iTerm2 left no addressable GUID, when a Linux session has
/// no graphical display, or on an unsupported OS. Decides *window-jumpability*
/// without any IO.
pub fn target_for(
    term_program: Option<&str>,
    iterm_session_id: Option<&str>,
    claude_pid: Option<u32>,
    display: Option<&str>,
    cwd: Option<&str>,
) -> Option<WindowTarget> {
    // A tmux session is jumped by switching its client (`focus_pane`), never by
    // raising a window — its `TERM_PROGRAM` is `tmux` on every platform.
    if term_program == Some("tmux") {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = display;
        match term_program {
            Some("iTerm.app") => {
                let guid = iterm_session_guid(iterm_session_id?)?;
                Some(WindowTarget::ITerm2 { session_id: guid })
            }
            Some("Apple_Terminal") => {
                claude_pid.map(|p| WindowTarget::TerminalApp { claude_pid: p })
            }
            // Ghostty exposes no per-session handle to its scripting API, so we
            // address the terminal by working directory (+ the flag title, which
            // the fleet fills in) at focus time.
            Some("ghostty") => cwd
                .filter(|c| !c.is_empty())
                .map(|c| WindowTarget::Ghostty {
                    cwd: c.to_string(),
                    title: None,
                }),
            _ => None,
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = iterm_session_id;
        let _ = cwd;
        // No portable per-session window handle on Linux; the jump command maps
        // the pid to its window at focus time. Addressable only with a
        // graphical display (else there's nothing to raise — headless/SSH).
        match (claude_pid, display) {
            (Some(p), Some(d)) if !d.is_empty() => {
                Some(WindowTarget::LinuxWindow { claude_pid: p })
            }
            _ => None,
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (term_program, iterm_session_id, claude_pid, display, cwd);
        None
    }
}

/// The session GUID from an `ITERM_SESSION_ID` (`wNtNpN:GUID`): the part after
/// the colon. Validated to the GUID alphabet so it's safe to splice into an
/// AppleScript literal.
fn iterm_session_guid(iterm_session_id: &str) -> Option<String> {
    let guid = iterm_session_id.rsplit(':').next()?;
    if !guid.is_empty() && guid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Some(guid.to_string())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
pub use macos::{focus, focus_tty};

#[cfg(target_os = "linux")]
pub use linux::focus;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn focus(_target: &WindowTarget) -> bool {
    false
}

#[cfg(target_os = "macos")]
mod macos {
    use std::process::Command;

    use super::WindowTarget;

    /// Raise the terminal window/tab for a target. Returns `true` if the
    /// emulator reported it found and selected the session. Best-effort: a
    /// closed session, a quit emulator, or missing Automation permission all
    /// return `false` rather than erroring.
    pub fn focus(target: &WindowTarget) -> bool {
        match target {
            WindowTarget::ITerm2 { session_id } => focus_iterm2(session_id),
            WindowTarget::TerminalApp { claude_pid } => match pid_tty(*claude_pid) {
                Some(tty) => focus_terminal_app(&tty),
                None => false,
            },
            WindowTarget::Ghostty { cwd, title } => focus_ghostty(cwd, title.as_deref()),
            // Not produced on macOS (see `target_for`).
            WindowTarget::LinuxWindow { .. } => false,
        }
    }

    /// Focus the Ghostty terminal for this surface and bring Ghostty forward,
    /// via the scripting `focus` command (needs the one-time Automation grant).
    /// Prefer the terminal whose title is our flag *and* whose working directory
    /// matches — so a plain shell tab in the same dir doesn't win; fall back to
    /// the first terminal in `cwd` when no title matches.
    fn focus_ghostty(cwd: &str, title: Option<&str>) -> bool {
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let cwd = esc(cwd);
        // `missing value` is AppleScript's null; an empty title literal never
        // equals a real title, so a `None`/empty title just uses the fallback.
        let title = title.map(esc).unwrap_or_default();
        let script = format!(
            r#"tell application "Ghostty"
  set fallback to missing value
  repeat with w in windows
    repeat with t in terminals of w
      if working directory of t is "{cwd}" then
        if name of t is "{title}" then
          focus t
          activate
          return "1"
        end if
        if fallback is missing value then set fallback to t
      end if
    end repeat
  end repeat
  if fallback is not missing value then
    focus fallback
    activate
    return "1"
  end if
end tell
return "0""#
        );
        run_osascript(&script)
    }

    /// Raise the emulator window whose session/tab is on `tty`, trying iTerm2
    /// then Terminal.app. This is the GUI half of a *tmux* jump: `focus_pane`
    /// switches the client at the tmux layer but never raises the emulator
    /// window hosting it (the jump assumed you were already looking at it). The
    /// caller passes a tmux `#{client_tty}` — the pty the emulator allocated for
    /// the tmux client, which both iTerm2 (`tty of session`) and Terminal
    /// (`tty of tab`) report. Best-effort: `false` if the tty is unsafe or no
    /// local emulator owns it (e.g. an SSH/nested client).
    pub fn focus_tty(tty: &str) -> bool {
        let Some(tty) = sanitize_tty(tty) else {
            return false;
        };
        focus_iterm2_tty(&tty) || focus_terminal_app(&tty) || focus_ghostty_tty(&tty)
    }

    /// Surface the Ghostty tab whose pty is `tty` — used to raise the window
    /// hosting a tmux client after a tmux jump. Ghostty's scripting can't address
    /// a terminal by tty, so we identify it with a one-shot **title handshake**:
    /// gate on the tty actually being Ghostty-hosted (process ancestry, so we
    /// never touch another emulator's title), plant a unique title through the
    /// pty, focus the terminal that now carries it, and restore the original
    /// title. Best-effort; `false` if not Ghostty-hosted or the terminal wasn't
    /// found.
    fn focus_ghostty_tty(tty: &str) -> bool {
        if !tty_hosted_by_ghostty(tty) {
            return false;
        }
        // (id, name) before the handshake, so we can restore the real title.
        let before = ghostty_terminals();
        if before.is_empty() {
            return false;
        }
        let sentinel = format!("ccstatus-focus-{}", tty.trim_start_matches("/dev/"));
        write_tty_title(tty, &sentinel);
        // Give Ghostty a moment to parse the OSC 2 before we look it up.
        std::thread::sleep(std::time::Duration::from_millis(80));
        match focus_terminal_named(&sentinel) {
            Some(id) => {
                if let Some((_, old)) = before.iter().find(|(i, _)| *i == id) {
                    write_tty_title(tty, old);
                }
                true
            }
            None => false,
        }
    }

    /// Whether a process with controlling tty `tty` descends from the Ghostty
    /// app — the gate that keeps the title handshake from writing to a non-Ghostty
    /// emulator's pty.
    fn tty_hosted_by_ghostty(tty: &str) -> bool {
        use std::collections::HashMap;
        let short = tty.trim_start_matches("/dev/");
        let Ok(out) = Command::new("ps")
            .args(["-t", short, "-o", "pid="])
            .output()
        else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        let tty_pids: Vec<u32> = String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if tty_pids.is_empty() {
            return false;
        }
        let procs = crate::util::ps_snapshot();
        let mut ppid: HashMap<u32, u32> = HashMap::new();
        let mut cmd: HashMap<u32, &str> = HashMap::new();
        for p in &procs {
            ppid.insert(p.pid, p.ppid);
            cmd.insert(p.pid, p.command.as_str());
        }
        const GHOSTTY_EXE: &str = "Ghostty.app/Contents/MacOS/ghostty";
        for start in tty_pids {
            let mut cur = start;
            for _ in 0..12 {
                if cmd.get(&cur).is_some_and(|c| c.contains(GHOSTTY_EXE)) {
                    return true;
                }
                match ppid.get(&cur) {
                    Some(&p) if p > 1 => cur = p,
                    _ => break,
                }
            }
        }
        false
    }

    /// Every Ghostty terminal as `(id, name)`, for capturing titles before the
    /// handshake.
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

    /// Focus (and bring forward) the Ghostty terminal whose title is `name`,
    /// returning its id. `name` is our ASCII sentinel, safe to splice.
    fn focus_terminal_named(name: &str) -> Option<String> {
        let script = format!(
            r#"tell application "Ghostty"
  repeat with w in windows
    repeat with t in terminals of w
      if name of t is "{name}" then
        focus t
        activate
        return id of t
      end if
    end repeat
  end repeat
end tell
return """#
        );
        let out = Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!id.is_empty()).then_some(id)
    }

    /// Write an `OSC 2` set-title to a pty (control chars stripped). Best-effort;
    /// `O_NOCTTY` so opening the tty never makes it our controlling terminal.
    fn write_tty_title(tty: &str, title: &str) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let clean: String = title.chars().filter(|c| !c.is_control()).collect();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(tty)
        {
            let _ = write!(f, "\x1b]2;{clean}\x07");
        }
    }

    /// Select the iTerm2 session with this GUID and bring iTerm2 forward.
    fn focus_iterm2(session_id: &str) -> bool {
        let script = format!(
            r#"tell application "iTerm2"
  repeat with w in windows
    repeat with t in tabs of w
      repeat with s in sessions of t
        if id of s is "{session_id}" then
          select w
          select t
          select s
          activate
          return "1"
        end if
      end repeat
    end repeat
  end repeat
end tell
return "0""#
        );
        run_osascript(&script)
    }

    /// Select the iTerm2 session on this tty and bring iTerm2 forward. The
    /// tty-keyed counterpart of [`focus_iterm2`], used for tmux jumps where we
    /// have the client's tty rather than a per-session GUID.
    fn focus_iterm2_tty(tty: &str) -> bool {
        let script = format!(
            r#"tell application "iTerm2"
  repeat with w in windows
    repeat with t in tabs of w
      repeat with s in sessions of t
        if tty of s is "{tty}" then
          select w
          select t
          select s
          activate
          return "1"
        end if
      end repeat
    end repeat
  end repeat
end tell
return "0""#
        );
        run_osascript(&script)
    }

    /// Select the Terminal.app tab on this tty and bring its window to the
    /// front. `tty` is a `/dev/ttysNNN` path.
    fn focus_terminal_app(tty: &str) -> bool {
        let script = format!(
            r#"tell application "Terminal"
  repeat with w in windows
    repeat with t in tabs of w
      if tty of t is "{tty}" then
        set selected of t to true
        set frontmost of w to true
        activate
        return "1"
      end if
    end repeat
  end repeat
end tell
return "0""#
        );
        run_osascript(&script)
    }

    /// Run an AppleScript and report whether it returned our "found" sentinel.
    fn run_osascript(script: &str) -> bool {
        match Command::new("osascript").arg("-e").arg(script).output() {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim() == "1",
            _ => false,
        }
    }

    /// The controlling tty of `pid` as an AppleScript-safe `/dev/ttysNNN` path.
    /// [`crate::util::pid_tty`] resolves and `/dev/`-prefixes it (rejecting a
    /// process with no controlling terminal); we then validate it to a tty-safe
    /// alphabet before splicing into an AppleScript literal.
    fn pid_tty(pid: u32) -> Option<String> {
        sanitize_tty(&crate::util::pid_tty(pid)?)
    }

    /// Normalise a tty to a `/dev/ttysNNN` path, rejecting any value carrying
    /// characters that aren't safe to splice into an AppleScript literal.
    /// Accepts the short form (`ttys001`) or a full `/dev/` path. Shared by the
    /// pid-derived ([`pid_tty`]) and tmux-client-derived ([`focus_tty`]) paths.
    pub(super) fn sanitize_tty(tty: &str) -> Option<String> {
        let path = if tty.starts_with("/dev/") {
            tty.to_string()
        } else {
            format!("/dev/{tty}")
        };
        let body = path.strip_prefix("/dev/").unwrap_or("");
        let ok = !body.is_empty()
            && body
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '.');
        ok.then_some(path)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::WindowTarget;
    use crate::config;

    /// The bundled best-effort X11 jump, used when the user hasn't set
    /// `jump.linux`. Piped to `sh` on stdin, so it needs no install step.
    const DEFAULT_JUMP: &str = include_str!("../examples/jump-linux.sh");

    /// Raise the terminal window hosting the Claude pid by running the jump
    /// command — the user's `jump.linux` if set, else the bundled X11 default.
    /// The pid goes through as `$CCSTATUS_CLAUDE_PID` and as the first
    /// argument; the command maps it (or an ancestor — the emulator) to a
    /// window and activates it. Best-effort: returns `false` when the command
    /// finds no tool or matching window, so the jump shows as failed rather
    /// than erroring.
    pub fn focus(target: &WindowTarget) -> bool {
        let WindowTarget::LinuxWindow { claude_pid } = target else {
            return false;
        };
        let pid = claude_pid.to_string();
        match config::jump_command() {
            Some(cmd) => run_user_command(&cmd, &pid),
            None => run_default(&pid),
        }
    }

    /// Run a user `jump.linux` command via `sh -c`, with the pid as `$1` (and
    /// `$CCSTATUS_CLAUDE_PID`).
    fn run_user_command(cmd: &str, pid: &str) -> bool {
        Command::new("sh")
            .args(["-c", cmd, "sh", pid])
            .env("CCSTATUS_CLAUDE_PID", pid)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Pipe the bundled default script to `sh -s`, with the pid as `$1`.
    fn run_default(pid: &str) -> bool {
        let Ok(mut child) = Command::new("sh")
            .args(["-s", pid])
            .env("CCSTATUS_CLAUDE_PID", pid)
            .stdin(Stdio::piped())
            .spawn()
        else {
            return false;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(DEFAULT_JUMP.as_bytes());
        }
        child.wait().map(|s| s.success()).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_guid_after_colon() {
        assert_eq!(
            iterm_session_guid("w0t0p0:CD4CA6CF-3A9C-464A-B736-B13BFEC9452C").as_deref(),
            Some("CD4CA6CF-3A9C-464A-B736-B13BFEC9452C")
        );
    }

    #[test]
    fn rejects_guid_with_no_colon_or_bad_chars() {
        assert_eq!(
            iterm_session_guid("noseparator"),
            Some("noseparator".to_string())
        );
        // A value carrying AppleScript-breaking characters is refused.
        assert_eq!(iterm_session_guid("w0t0p0:bad\"quote"), None);
        assert_eq!(iterm_session_guid("w0t0p0:"), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn sanitize_tty_normalises_and_rejects() {
        use super::macos::sanitize_tty;
        // Short form gets the /dev/ prefix.
        assert_eq!(sanitize_tty("ttys003").as_deref(), Some("/dev/ttys003"));
        // Full path passes through.
        assert_eq!(
            sanitize_tty("/dev/ttys003").as_deref(),
            Some("/dev/ttys003")
        );
        // AppleScript-breaking characters are refused.
        assert_eq!(sanitize_tty("ttys003\" then activate"), None);
        assert_eq!(sanitize_tty(""), None);
        assert_eq!(sanitize_tty("/dev/"), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn target_for_iterm_uses_guid() {
        let t = target_for(
            Some("iTerm.app"),
            Some("w0t0p0:CD4CA6CF-3A9C-464A-B736-B13BFEC9452C"),
            Some(123),
            None,
            None,
        );
        assert_eq!(
            t,
            Some(WindowTarget::ITerm2 {
                session_id: "CD4CA6CF-3A9C-464A-B736-B13BFEC9452C".into()
            })
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn target_for_terminal_uses_pid() {
        let t = target_for(Some("Apple_Terminal"), None, Some(456), None, None);
        assert_eq!(t, Some(WindowTarget::TerminalApp { claude_pid: 456 }));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn target_for_ghostty_uses_cwd() {
        // Ghostty is addressed by working directory (title filled in by the
        // fleet, so `None` here).
        assert_eq!(
            target_for(Some("ghostty"), None, Some(9), None, Some("/repo/x")),
            Some(WindowTarget::Ghostty {
                cwd: "/repo/x".into(),
                title: None,
            })
        );
        // No cwd (or empty) — nothing to match on.
        assert_eq!(target_for(Some("ghostty"), None, Some(9), None, None), None);
        assert_eq!(
            target_for(Some("ghostty"), None, Some(9), None, Some("")),
            None
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn target_for_unrecognised_terminal_is_none() {
        // Inside tmux TERM_PROGRAM is "tmux" — not a window we address here.
        assert_eq!(target_for(Some("tmux"), None, Some(1), None, None), None);
        assert_eq!(target_for(None, None, Some(1), None, None), None);
        // iTerm with no session id can't be addressed.
        assert_eq!(
            target_for(Some("iTerm.app"), None, Some(1), None, None),
            None
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn target_for_linux_needs_a_display() {
        // A non-tmux Claude with a pid and a display is window-jumpable.
        assert_eq!(
            target_for(None, None, Some(42), Some(":0"), None),
            Some(WindowTarget::LinuxWindow { claude_pid: 42 })
        );
        // Wayland display works too; TERM_PROGRAM is irrelevant on Linux (a
        // Ghostty session jumps by pid there, not by cwd).
        assert_eq!(
            target_for(
                Some("ghostty"),
                None,
                Some(7),
                Some("wayland-1"),
                Some("/repo/x")
            ),
            Some(WindowTarget::LinuxWindow { claude_pid: 7 })
        );
        // No display (headless/SSH), no pid, empty display, or tmux -> none.
        assert_eq!(target_for(None, None, Some(42), None, None), None);
        assert_eq!(target_for(None, None, Some(42), Some(""), None), None);
        assert_eq!(target_for(None, None, None, Some(":0"), None), None);
        assert_eq!(
            target_for(Some("tmux"), None, Some(42), Some(":0"), None),
            None
        );
    }
}
