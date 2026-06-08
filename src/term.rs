use std::env;
use std::fs::File;
use std::os::fd::AsRawFd;

/// Returns the terminal width in columns. Prefers `$COLUMNS`, falling back to a
/// `/dev/tty` ioctl, then `default`.
///
/// `$COLUMNS` comes first because Claude Code captures the statusline command's
/// output rather than connecting it to the terminal, so `tput`/ioctl-based
/// detection can't read the real size from inside the script — Claude Code sets
/// `COLUMNS`/`LINES` to the current dimensions before each run (v2.1.153+). The
/// `/dev/tty` fallback covers other contexts where `COLUMNS` isn't exported.
pub fn columns(default: u16) -> u16 {
    if let Ok(v) = env::var("COLUMNS")
        && let Ok(n) = v.parse::<u16>()
        && n > 0
    {
        return n;
    }
    if let Some(n) = tty_cols() {
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
