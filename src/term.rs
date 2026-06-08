use std::env;
use std::fs::File;
use std::os::fd::AsRawFd;

/// Returns the controlling terminal width in columns. Falls back to `$COLUMNS`
/// then `default`. Statusline binaries can't rely on stdin/stdout being a TTY,
/// so we go via `/dev/tty`.
pub fn columns(default: u16) -> u16 {
    if let Some(n) = tty_cols() {
        return n;
    }
    if let Ok(v) = env::var("COLUMNS")
        && let Ok(n) = v.parse::<u16>()
        && n > 0
    {
        return n;
    }
    default
}

#[cfg(unix)]
fn tty_cols() -> Option<u16> {
    let file = File::open("/dev/tty").ok()?;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
    if r == 0 && ws.ws_col > 0 {
        Some(ws.ws_col)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn tty_cols() -> Option<u16> {
    None
}
