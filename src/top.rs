//! `ccstatus top` — an interactive aggregate view of every live Claude
//! session, with "take me to this Claude" (jump).
//!
//! A poll-driven aggregate surface: it reads the whole state dir via
//! [`crate::fleet`] (no tmux focus needed), renders a table, and on Enter
//! jumps to the selected session's pane. Same-server jumps go straight through
//! the [`crate::tmux`] seam; cross-server jumps route through that server's
//! handler (which lives in the right tmux environment) via
//! [`crate::ipc::notify_focus`].
//!
//! Hand-rolled raw-mode TUI (termios + ANSI), matching the crate's no-TUI-deps
//! style. The terminal is always restored on exit through an RAII guard.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::color::*;
use crate::fleet::{self, SessionView, Warmth};
use crate::ipc;
use crate::tmux::{self, Tmux};

/// How often the table re-reads the state dir when idle (also the input poll
/// timeout, so keys stay responsive).
const REFRESH: Duration = Duration::from_millis(1500);

pub fn run() -> ExitCode {
    let stdin = io::stdin();
    if !is_tty(stdin.as_raw_fd()) {
        eprintln!("ccstatus top: not a terminal");
        return ExitCode::FAILURE;
    }
    let _raw = match RawMode::enable(stdin.as_raw_fd()) {
        Some(r) => r,
        None => {
            eprintln!("ccstatus top: failed to set raw mode");
            return ExitCode::FAILURE;
        }
    };
    let mut screen = Screen::enter();

    let my_server = tmux::server_id();
    let mut views = fleet::collect();
    let mut selected = 0usize;
    let mut last_refresh = Instant::now();

    loop {
        clamp(&mut selected, views.len());
        screen.draw(&render(&views, selected, my_server.as_deref()));

        match read_key(REFRESH) {
            Some(Key::Quit) => break,
            Some(Key::Up) => selected = selected.saturating_sub(1),
            Some(Key::Down) => selected += 1,
            Some(Key::Jump) => {
                if let Some(v) = views.get(selected)
                    && v.jumpable
                {
                    jump(v, my_server.as_deref());
                    break; // jumping switches the client away; nothing to show
                }
            }
            Some(Key::Refresh) | None => {}
        }

        if read_key_pending() || last_refresh.elapsed() >= REFRESH {
            views = fleet::collect();
            last_refresh = Instant::now();
        }
    }
    ExitCode::SUCCESS
}

/// Perform the jump. Same server as us → straight through the tmux seam (fast).
/// Different server (or we're not in tmux) → route through that server's
/// handler, which runs in the correct tmux environment. No-op for a session
/// with no pane (a non-tmux Claude — caller gates on `jumpable`).
fn jump(v: &SessionView, my_server: Option<&str>) {
    let Some(addr) = &v.address else { return };
    if my_server == Some(addr.server_id.as_str()) {
        tmux::CliTmux.focus_pane(&addr.pane_id);
    } else {
        ipc::notify_focus(&addr.server_id, &addr.pane_id);
    }
}

fn clamp(selected: &mut usize, len: usize) {
    if len == 0 {
        *selected = 0;
    } else if *selected >= len {
        *selected = len - 1;
    }
}

// ---- rendering -----------------------------------------------------------

fn render(views: &[SessionView], selected: usize, my_server: Option<&str>) -> String {
    let cols = crate::term::columns(100) as usize;
    let mut out = String::new();

    let warm = views.iter().filter(|v| v.warmth == Warmth::Warm).count();
    out.push_str(&format!(
        "{BLUE}ccstatus{RESET} {DIM}·{RESET} {} session(s), {GREEN}{warm} warm{RESET}\r\n",
        views.len()
    ));
    out.push_str(&format!(
        "{DIM}{}{RESET}\r\n",
        "─".repeat(cols.min(80))
    ));

    if views.is_empty() {
        out.push_str(&format!("{DIM}No live Claude sessions.{RESET}\r\n"));
    } else {
        for (i, v) in views.iter().enumerate() {
            out.push_str(&row(v, i == selected, my_server, cols));
            out.push_str("\r\n");
        }
    }

    out.push_str(&format!(
        "\r\n{DIM}j/k or ↑/↓ move · Enter jump · r refresh · q quit{RESET}\r\n"
    ));
    out
}

fn row(v: &SessionView, selected: bool, my_server: Option<&str>, cols: usize) -> String {
    let marker = if selected { format!("{BLUE}▶{RESET} ") } else { "  ".to_string() };
    let (glyph, gcolor) = match v.warmth {
        Warmth::Warm => ("●", GREEN),
        Warmth::Cold => ("●", RED),
        Warmth::Unknown => ("○", DIM),
    };
    let model = v.model.as_deref().unwrap_or("Claude");
    let ctx = v
        .context_pct
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "-".to_string());
    let age = v
        .idle_secs
        .map(human_age)
        .unwrap_or_else(|| "-".to_string());
    // `*` = on a different tmux server than us (still jumpable via its handler).
    let here = match &v.address {
        Some(a) if my_server == Some(a.server_id.as_str()) => "",
        Some(_) => "*",
        None => "",
    };
    let note = match &v.address {
        None => format!(" {DIM}(not in tmux){RESET}"),
        Some(_) if !v.jumpable => format!(" {DIM}(no handler){RESET}"),
        Some(_) => String::new(),
    };

    // Fixed-ish left columns, then cwd fills the rest.
    let left = format!(
        "{marker}{gcolor}{glyph}{RESET} {WHITE}{model:<14}{RESET} {DIM}ctx{RESET} {ctx:<4} {DIM}idle{RESET} {age:<5}{here} "
    );
    // Budget the cwd column by terminal width, ignoring ANSI in the estimate.
    let used = 2 + 2 + 15 + 5 + 5 + 6 + 5 + here.len() + 1;
    let budget = cols.saturating_sub(used).max(8);
    let cwd = v.cwd.as_deref().unwrap_or("");
    let cwd = truncate(cwd, budget);
    format!("{left}{CYAN}{cwd}{RESET}{note}")
}

fn human_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

// ---- terminal handling ---------------------------------------------------

/// Alternate-screen + hidden-cursor guard. Restores on drop.
struct Screen;

impl Screen {
    fn enter() -> Self {
        // Alt screen, clear, hide cursor.
        print!("\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l");
        let _ = io::stdout().flush();
        Screen
    }

    fn draw(&mut self, body: &str) {
        // Home, then clear-to-end-of-screen as we write each line ends with a
        // CRLF; a final clear-below removes any leftover from a longer frame.
        let mut out = io::stdout().lock();
        let _ = write!(out, "\x1b[H{body}\x1b[J");
        let _ = out.flush();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // Show cursor, leave alt screen.
        print!("\x1b[?25h\x1b[?1049l");
        let _ = io::stdout().flush();
    }
}

/// Raw-mode guard over a tty fd. Disables canonical mode and echo so we get
/// keystrokes immediately; restores the original termios on drop.
struct RawMode {
    fd: i32,
    orig: libc::termios,
}

impl RawMode {
    fn enable(fd: i32) -> Option<Self> {
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return None;
        }
        let orig = term;
        // Keep ISIG (Ctrl-C still aborts; Drop restores either way).
        term.c_lflag &= !(libc::ICANON | libc::ECHO);
        term.c_cc[libc::VMIN] = 1;
        term.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) } != 0 {
            return None;
        }
        Some(RawMode { fd, orig })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig) };
    }
}

fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

// ---- input ---------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Jump,
    Refresh,
    Quit,
}

/// Whether stdin has a byte ready right now (zero-timeout poll).
fn read_key_pending() -> bool {
    poll_stdin(0)
}

/// Block up to `timeout` for a keystroke. Returns `None` on timeout (caller
/// should refresh). Reads the whole burst in one `libc::read` so a multi-byte
/// escape (e.g. arrow = `ESC [ A`) arrives intact — going through buffered
/// `io::stdin` would swallow the tail of the sequence and mis-read every arrow
/// as a lone Esc.
fn read_key(timeout: Duration) -> Option<Key> {
    if !poll_stdin(timeout.as_millis() as i32) {
        return None;
    }
    let mut buf = [0u8; 8];
    let n = unsafe {
        libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };
    if n <= 0 {
        return Some(Key::Quit); // EOF / error
    }
    Some(decode(&buf[..n as usize]))
}

/// Decode a raw input burst into a key. Pure, so the escape handling is
/// testable. A lone `Esc` quits; `Esc [ A/B` is an arrow.
fn decode(b: &[u8]) -> Key {
    match b {
        [b'q', ..] => Key::Quit,
        [b'j', ..] => Key::Down,
        [b'k', ..] => Key::Up,
        [b'r', ..] => Key::Refresh,
        [b'\r', ..] | [b'\n', ..] => Key::Jump,
        [0x1b, b'[', b'A', ..] => Key::Up,
        [0x1b, b'[', b'B', ..] => Key::Down,
        [0x1b] => Key::Quit,
        _ => Key::Refresh,
    }
}

fn poll_stdin(timeout_ms: i32) -> bool {
    let mut fds = [libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    }];
    unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) > 0 && (fds[0].revents & libc::POLLIN) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_age_units() {
        assert_eq!(human_age(5), "5s");
        assert_eq!(human_age(90), "1m");
        assert_eq!(human_age(3700), "1h");
        assert_eq!(human_age(90_000), "1d");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefgh", 4), "abc…");
        assert_eq!(truncate("x", 1), "x");
    }

    #[test]
    fn decode_arrows_and_keys() {
        assert_eq!(decode(b"q"), Key::Quit);
        assert_eq!(decode(b"j"), Key::Down);
        assert_eq!(decode(b"k"), Key::Up);
        assert_eq!(decode(b"r"), Key::Refresh);
        assert_eq!(decode(b"\r"), Key::Jump);
        assert_eq!(decode(b"\n"), Key::Jump);
        // Arrow escape sequences arrive as one burst.
        assert_eq!(decode(&[0x1b, b'[', b'A']), Key::Up);
        assert_eq!(decode(&[0x1b, b'[', b'B']), Key::Down);
        // Lone Esc quits; an unknown escape just refreshes.
        assert_eq!(decode(&[0x1b]), Key::Quit);
        assert_eq!(decode(&[0x1b, b'[', b'C']), Key::Refresh);
    }

    #[test]
    fn clamp_keeps_selection_in_range() {
        let mut s = 5;
        clamp(&mut s, 3);
        assert_eq!(s, 2);
        let mut s = 5;
        clamp(&mut s, 0);
        assert_eq!(s, 0);
    }
}
