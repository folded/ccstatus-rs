//! Ghostty backend: detection and surface identity.
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
//! This module is the Ghostty analog of [`crate::main`]'s `active_tmux_pane`:
//! detect the backend from the environment, then mint the surface id. The
//! polling daemon that consumes these surfaces is added in a later phase.
#![allow(dead_code)]

use std::env;

/// Pure: whether this process is hosted directly by Ghostty. Ghostty sets
/// `TERM_PROGRAM=ghostty`; inside tmux `TERM_PROGRAM` is `tmux` instead, so a
/// tmux-hosted Ghostty correctly reads as tmux (and is driven by the tmux
/// backend). Split from the env read for testing.
pub fn is_ghostty(term_program: Option<&str>) -> bool {
    term_program == Some("ghostty")
}

/// `Some(pty_path)` when Claude is running directly in Ghostty: the surface id
/// (the emulator's pty, resolved from `claude_pid`). `None` outside Ghostty, or
/// when the pty can't be resolved (no controlling terminal). The caller passes
/// the already-resolved interactive-Claude pid.
pub fn active_surface(claude_pid: u32) -> Option<String> {
    let term_program = env::var("TERM_PROGRAM").ok();
    if !is_ghostty(term_program.as_deref()) {
        return None;
    }
    crate::util::pid_tty(claude_pid)
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
}
