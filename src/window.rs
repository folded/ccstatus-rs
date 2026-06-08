//! Raising the OS terminal window that hosts a Claude session — the non-tmux
//! half of "take me to this Claude" (jump).
//!
//! Inside tmux, [`crate::tmux::Tmux::focus_pane`] is the whole jump: it switches
//! the attached client onto the pane, which appears in the window you're already
//! looking at. A Claude running *directly* in a terminal emulator has no tmux to
//! switch — the actuator is the emulator's own scripting interface, and it's
//! per-emulator. This module covers macOS iTerm2 and Terminal.app.
//!
//! Addressing comes from the session's presence record (see
//! [`crate::state::SessionState`]), captured by the registrar from its
//! environment:
//! - **iTerm2** — `ITERM_SESSION_ID` is `wNtNpN:GUID`; the GUID is exactly what
//!   iTerm2's AppleScript `id of session` returns, so we address the session
//!   directly (stable across tab moves, unlike a tty which can be reused).
//! - **Terminal.app** — no per-session id, so we match the controlling tty of
//!   the Claude process (`ps -o tty=`), which equals a Terminal tab's `tty`.
//!
//! Non-macOS builds keep the types but [`focus`] is a no-op and [`target_for`]
//! yields `None`, so non-tmux sessions stay non-jumpable there.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// A non-tmux terminal window/tab hosting a Claude session, addressable for
/// "raise this window" on macOS. Built purely from presence fields by
/// [`target_for`]; the tty (an extra `ps`) is resolved lazily in [`focus`],
/// only when a Terminal.app jump actually fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowTarget {
    /// iTerm2 session, addressed by its stable session GUID.
    ITerm2 { session_id: String },
    /// Terminal.app tab, matched by the Claude process's controlling tty
    /// (resolved at focus time from this pid).
    TerminalApp { claude_pid: u32 },
}

/// Pure: derive a window target from a session's captured terminal identity.
/// `None` when the terminal is unrecognised (e.g. inside tmux, where
/// `TERM_PROGRAM` is `tmux`), when iTerm2 left no addressable GUID, or off
/// macOS. Decides *window-jumpability* without any IO.
pub fn target_for(
    term_program: Option<&str>,
    iterm_session_id: Option<&str>,
    claude_pid: Option<u32>,
) -> Option<WindowTarget> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    match term_program {
        Some("iTerm.app") => {
            let guid = iterm_session_guid(iterm_session_id?)?;
            Some(WindowTarget::ITerm2 { session_id: guid })
        }
        Some("Apple_Terminal") => claude_pid.map(|p| WindowTarget::TerminalApp { claude_pid: p }),
        _ => None,
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
pub use macos::focus;

#[cfg(not(target_os = "macos"))]
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
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim() == "1"
            }
            _ => false,
        }
    }

    /// The controlling tty of `pid` as a `/dev/ttysNNN` path. `ps` prints the
    /// short form (`ttys001`) or `??`/empty for a process with no controlling
    /// terminal; we reject the latter and prefix `/dev/` to match what the
    /// emulators report. Validated to a tty-safe alphabet for the AppleScript
    /// literal.
    fn pid_tty(pid: u32) -> Option<String> {
        let out = Command::new("ps")
            .args(["-o", "tty=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if t.is_empty() || t == "??" || t == "?" {
            return None;
        }
        let path = if t.starts_with("/dev/") { t } else { format!("/dev/{t}") };
        let body = path.strip_prefix("/dev/").unwrap_or("");
        let ok = !body.is_empty()
            && body
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '.');
        ok.then_some(path)
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
        assert_eq!(iterm_session_guid("noseparator"), Some("noseparator".to_string()));
        // A value carrying AppleScript-breaking characters is refused.
        assert_eq!(iterm_session_guid("w0t0p0:bad\"quote"), None);
        assert_eq!(iterm_session_guid("w0t0p0:"), None);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn target_for_iterm_uses_guid() {
        let t = target_for(
            Some("iTerm.app"),
            Some("w0t0p0:CD4CA6CF-3A9C-464A-B736-B13BFEC9452C"),
            Some(123),
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
        let t = target_for(Some("Apple_Terminal"), None, Some(456));
        assert_eq!(t, Some(WindowTarget::TerminalApp { claude_pid: 456 }));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn target_for_unrecognised_terminal_is_none() {
        // Inside tmux TERM_PROGRAM is "tmux" — not a window we address here.
        assert_eq!(target_for(Some("tmux"), None, Some(1)), None);
        assert_eq!(target_for(None, None, Some(1)), None);
        // iTerm with no session id can't be addressed.
        assert_eq!(target_for(Some("iTerm.app"), None, Some(1)), None);
    }
}
