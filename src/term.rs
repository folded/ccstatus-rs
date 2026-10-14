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

/// Whether the terminal renders OSC 8 hyperlinks, judged from the environment
/// Claude Code passes to the statusline command. Conservative: an
/// unrecognised terminal gets plain text, since one that ignores OSC 8 may
/// print the sequence as junk. `FORCE_HYPERLINK` is the override Claude Code's
/// docs give for when its own detection misses a terminal; `0` turns links off.
pub fn hyperlinks_supported() -> bool {
    hyperlinks_supported_in(|k| env::var(k).ok())
}

/// Pure core of [`hyperlinks_supported`], reading variables through `get`.
fn hyperlinks_supported_in(get: impl Fn(&str) -> Option<String>) -> bool {
    if let Some(v) = get("FORCE_HYPERLINK") {
        return v.trim().parse::<i64>() != Ok(0);
    }
    // Inside tmux `TERM_PROGRAM` is `tmux` and `TERM` is `screen`/`tmux-*`, so
    // none of these match unless the user opts in with `FORCE_HYPERLINK`.
    matches!(
        get("TERM_PROGRAM").as_deref(),
        Some("iTerm.app" | "WezTerm" | "ghostty")
    ) || get("TERM").as_deref() == Some("xterm-kitty")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn hyperlinks_recognise_iterm_wezterm_ghostty_and_kitty() {
        for pairs in [
            &[("TERM_PROGRAM", "iTerm.app")][..],
            &[("TERM_PROGRAM", "WezTerm")],
            &[("TERM_PROGRAM", "ghostty")],
            &[("TERM", "xterm-kitty")],
        ] {
            assert!(hyperlinks_supported_in(env_of(pairs)), "{pairs:?}");
        }
    }

    #[test]
    fn hyperlinks_off_for_unknown_terminals_and_tmux() {
        assert!(!hyperlinks_supported_in(env_of(&[])));
        assert!(!hyperlinks_supported_in(env_of(&[(
            "TERM_PROGRAM",
            "Apple_Terminal"
        )])));
        assert!(!hyperlinks_supported_in(env_of(&[
            ("TERM_PROGRAM", "tmux"),
            ("TERM", "tmux-256color")
        ])));
    }

    #[test]
    fn force_hyperlink_overrides_detection_both_ways() {
        assert!(hyperlinks_supported_in(env_of(&[("FORCE_HYPERLINK", "1")])));
        assert!(!hyperlinks_supported_in(env_of(&[
            ("FORCE_HYPERLINK", "0"),
            ("TERM_PROGRAM", "iTerm.app")
        ])));
    }
}
