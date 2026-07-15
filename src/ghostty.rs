//! Plain-Ghostty (no tmux) surface actuation.
//!
//! Ghostty has no status bar and no remote-control socket, but it parses
//! whatever arrives on a surface's pty regardless of which process wrote it —
//! so a background daemon holding the pty path can drive the *tab title*
//! (OSC 2) as the plain-Ghostty equivalent of the tmux window-name flag.
//!
//! The addressing key is the pty path (`/dev/ttys007`), which replaces the
//! tmux pane id everywhere: pane-state files, socket messages, and the
//! actuation seam below. Titles are last-writer-wins in Ghostty (Claude Code
//! and shell prompts also set them, there is no compositing), so the handler
//! re-asserts every tick instead of change-detecting.

use std::env;
use std::io::Write;

/// The pane-state "server" namespace for ghostty surfaces (there is no tmux
/// server; all surfaces share one). Files live under `pane/ghostty/`.
pub const SERVER_ID: &str = "ghostty";

/// The `ServerDir` session key for the single ghostty handler's lock/socket
/// (`handlerg.lock` / `handlerg.sock`). One handler drives every surface.
pub const SOCKET_KEY: &str = "g";

/// `Some(pty path)` iff this process runs directly inside Ghostty (no tmux —
/// callers check tmux first) and its controlling terminal is resolvable.
/// The statusline shares Claude Code's controlling terminal, so this is the
/// pty of the surface hosting the session.
pub fn surface_tty() -> Option<String> {
    if env::var("TERM_PROGRAM").as_deref() != Ok("ghostty") {
        return None;
    }
    crate::util::pid_tty_path(std::process::id())
}

/// Escape-sequence actuation onto one Ghostty surface (its pty). Writes are
/// best-effort and non-blocking; callers guard liveness (a pty path can be
/// recycled by an unrelated process — see the handler's prune).
pub trait GhosttySurface {
    /// OSC 2 — set the surface's (tab) title. The plain-Ghostty
    /// `rename-window`.
    fn set_title(&self, tty: &str, title: &str);
    /// OSC 2 with an empty payload — hand the title back to Ghostty's own
    /// logic (config/shell integration), the `automatic-rename on` analogue.
    fn clear_title(&self, tty: &str);
}

/// Production adapter: opens the pty and writes escape bytes.
pub struct CliGhostty;

impl GhosttySurface for CliGhostty {
    fn set_title(&self, tty: &str, title: &str) {
        write_tty(tty, &osc2(title));
    }

    fn clear_title(&self, tty: &str) {
        write_tty(tty, &osc2(""));
    }
}

/// The OSC 2 set-title sequence, BEL-terminated. The title is sanitized so a
/// hostile dirname/branch can't smuggle a second escape sequence into the
/// byte stream.
fn osc2(title: &str) -> Vec<u8> {
    format!("\x1b]2;{}\x07", sanitize_title(title)).into_bytes()
}

/// Strip control characters (including ESC and BEL, which would terminate or
/// nest sequences) from a title. Printable text and Unicode glyphs pass.
fn sanitize_title(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Best-effort write of raw bytes to a pty. `O_NOCTTY` so we never adopt the
/// terminal as our controlling tty; `O_NONBLOCK` so a flow-stopped terminal
/// (^S) can't wedge the handler — a dropped title is re-asserted next tick.
fn write_tty(tty: &str, bytes: &[u8]) {
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(tty)
    else {
        return;
    };
    let _ = f.write_all(bytes);
}

/// A recorded surface write, for asserting the ordered log in tests.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceWrite {
    SetTitle(String, String),
    ClearTitle(String),
}

/// Test adapter: records writes.
#[cfg(test)]
pub struct FakeGhostty {
    pub writes: std::cell::RefCell<Vec<SurfaceWrite>>,
}

#[cfg(test)]
impl FakeGhostty {
    pub fn new() -> Self {
        Self {
            writes: std::cell::RefCell::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl GhosttySurface for FakeGhostty {
    fn set_title(&self, tty: &str, title: &str) {
        self.writes
            .borrow_mut()
            .push(SurfaceWrite::SetTitle(tty.to_string(), title.to_string()));
    }

    fn clear_title(&self, tty: &str) {
        self.writes
            .borrow_mut()
            .push(SurfaceWrite::ClearTitle(tty.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc2_wraps_title_in_the_sequence() {
        assert_eq!(osc2("⚑ repo ⚠"), "\x1b]2;⚑ repo ⚠\x07".as_bytes());
        assert_eq!(osc2(""), b"\x1b]2;\x07");
    }

    #[test]
    fn sanitize_title_strips_control_characters() {
        // ESC and BEL would terminate/nest the sequence; newlines are junk.
        assert_eq!(sanitize_title("a\x1b]0;evil\x07b"), "a]0;evilb");
        assert_eq!(sanitize_title("a\nb\tc"), "abc");
        // Glyphs and plain text pass through.
        assert_eq!(sanitize_title("◐ dir ↑"), "◐ dir ↑");
    }
}
