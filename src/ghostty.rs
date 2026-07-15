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

/// `Some(pty path)` iff this session runs directly inside Ghostty (no tmux —
/// callers check tmux first) and the Claude process's controlling terminal is
/// resolvable. Keyed off the *Claude* pid, not our own: Claude Code may spawn
/// the statusline detached from the terminal session (no ctty), but the
/// Claude process itself always sits on the surface's pty.
pub fn surface_tty(claude_pid: u32) -> Option<String> {
    if env::var("TERM_PROGRAM").as_deref() != Ok("ghostty") {
        return None;
    }
    crate::util::pid_tty_path(claude_pid)
}

/// What the native progress bar (OSC 9;4, Ghostty >= 1.2) shows for a
/// surface. Ghostty renders it as a thin bar at the top of the split and
/// auto-clears it after ~15s without updates, so the handler's 3s
/// re-emission doubles as a liveness heartbeat: a dead handler leaves a
/// clean surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Normal bar at `pct`: the cache-warmth countdown, draining over the
    /// warm window so bar-present == the warmth segment reading "warm".
    Remaining(u8),
    /// Error (red) bar, full: blocked on the user (needs input / suspended).
    NeedsInput,
    /// Indeterminate pulse: a turn (or background task) is running.
    Working,
    /// Remove the bar (cold, or restore).
    Clear,
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
    /// OSC 9;4 — drive the native progress bar. Callers gate on
    /// [`supports_progress`]: pre-1.2 Ghostty parses `9;4;…` as an
    /// iTerm2-style OSC 9 *notification*, so an ungated write would raise
    /// desktop banners instead of a bar.
    fn set_progress(&self, tty: &str, p: Progress);
    /// OSC 777 — raise a desktop notification for this surface. Ghostty only
    /// banners it while the surface is *unfocused* (clicking focuses the
    /// tab), which is exactly the "finished while you weren't looking"
    /// semantics — no view tracking needed on our side.
    fn notify(&self, tty: &str, title: &str, body: &str);
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

    fn set_progress(&self, tty: &str, p: Progress) {
        write_tty(tty, &osc_progress(p));
    }

    fn notify(&self, tty: &str, title: &str, body: &str) {
        write_tty(tty, &osc777(title, body));
    }
}

/// The rxvt notification sequence: `ESC ] 777 ; notify ; title ; body BEL`.
/// The title must not carry `;` (it would bleed into the body slot); the
/// body is the final field, so its semicolons are safe. Both are stripped
/// of control characters like titles are.
fn osc777(title: &str, body: &str) -> Vec<u8> {
    let title: String = sanitize_title(title).replace(';', ",");
    format!("\x1b]777;notify;{};{}\x07", title, sanitize_title(body)).into_bytes()
}

/// The ConEmu progress sequence for a state: `ESC ] 9 ; 4 ; s [; v] BEL`.
fn osc_progress(p: Progress) -> Vec<u8> {
    match p {
        Progress::Remaining(pct) => format!("\x1b]9;4;1;{}\x07", pct.min(100)),
        Progress::NeedsInput => "\x1b]9;4;2;100\x07".to_string(),
        Progress::Working => "\x1b]9;4;3\x07".to_string(),
        Progress::Clear => "\x1b]9;4;0\x07".to_string(),
    }
    .into_bytes()
}

/// Whether a `TERM_PROGRAM_VERSION` supports OSC 9;4 (Ghostty >= 1.2).
/// Unparseable versions read as unsupported — the failure mode of a wrong
/// "yes" is spurious desktop notifications.
pub fn supports_progress(version: &str) -> bool {
    let mut it = version.split('.');
    let major: u32 = match it.next().and_then(|s| s.parse().ok()) {
        Some(n) => n,
        None => return false,
    };
    let minor: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor) >= (1, 2)
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
    SetProgress(String, Progress),
    Notify(String, String, String),
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

    fn set_progress(&self, tty: &str, p: Progress) {
        self.writes
            .borrow_mut()
            .push(SurfaceWrite::SetProgress(tty.to_string(), p));
    }

    fn notify(&self, tty: &str, title: &str, body: &str) {
        self.writes.borrow_mut().push(SurfaceWrite::Notify(
            tty.to_string(),
            title.to_string(),
            body.to_string(),
        ));
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
    fn osc_progress_encodes_each_state() {
        assert_eq!(osc_progress(Progress::Remaining(73)), b"\x1b]9;4;1;73\x07");
        assert_eq!(
            osc_progress(Progress::Remaining(250)),
            b"\x1b]9;4;1;100\x07"
        ); // clamped
        assert_eq!(osc_progress(Progress::NeedsInput), b"\x1b]9;4;2;100\x07");
        assert_eq!(osc_progress(Progress::Working), b"\x1b]9;4;3\x07");
        assert_eq!(osc_progress(Progress::Clear), b"\x1b]9;4;0\x07");
    }

    #[test]
    fn osc777_encodes_and_keeps_title_single_field() {
        assert_eq!(
            osc777("Claude finished", "⚑ repo ⚠"),
            "\x1b]777;notify;Claude finished;⚑ repo ⚠\x07".as_bytes()
        );
        // A ';' in the title would bleed into the body slot — mapped to ','.
        // The body is the final field, so its ';' passes through.
        assert_eq!(
            osc777("a;b", "c;d"),
            "\x1b]777;notify;a,b;c;d\x07".as_bytes()
        );
        // Control characters stripped from both fields.
        assert_eq!(
            osc777("t\x07t", "b\x1bb"),
            "\x1b]777;notify;tt;bb\x07".as_bytes()
        );
    }

    #[test]
    fn supports_progress_gates_on_1_2() {
        assert!(supports_progress("1.2.0"));
        assert!(supports_progress("1.3.1"));
        assert!(supports_progress("2.0"));
        assert!(!supports_progress("1.1.3"));
        assert!(!supports_progress("1.0.2"));
        assert!(!supports_progress("0.9"));
        assert!(!supports_progress(""));
        assert!(!supports_progress("nightly"));
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
