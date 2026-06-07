//! Helpers for inspecting the tmux environment.

use std::env;
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};

use crate::state;

/// Number of status rows when the focused pane is hosting a Claude
/// session: 1 row for the existing powerline window list, plus 3 rows of
/// ccstatus rich output.
const ROWS_WITH_CLAUDE: u32 = 4;

/// Number of status rows when there's no Claude session: just the
/// powerline window list. The ccstatus rows collapse out of view.
const ROWS_WITHOUT_CLAUDE: u32 = 1;

/// Stable 8-char identifier for the current tmux server, derived from the
/// socket path in `$TMUX` (`socket_path,server_pid,session_id`).
///
/// Pane ids (e.g. `%5`) are unique within a server but collide across
/// servers, so the pane state directory is keyed `(server_id, pane_id)`.
/// Returns `None` outside tmux (no `$TMUX` set or malformed).
pub fn server_id() -> Option<String> {
    let tmux = env::var("TMUX").ok().filter(|s| !s.is_empty())?;
    let socket = tmux.split(',').next()?;
    if socket.is_empty() {
        return None;
    }
    Some(short_hash(socket))
}

/// Set the tmux server's `status` option to match whether the focused
/// pane carries a Claude session. Invoked from a `pane-focus-in` hook so
/// that the rich ccstatus rows only consume vertical space when they
/// have something to show.
pub fn on_focus(hint: Option<&str>) -> ExitCode {
    // Don't trust the pane id the hook passed via `#{pane_id}` — some hook
    // events (`client-session-changed`, `session-window-changed`, …) fire
    // with the *previously*-active pane id, which would race against the
    // earlier `pane-focus-in` firing and ratchet status back to 4. Always
    // ask tmux which pane the client currently considers focused.
    let pane_id = match current_pane().or_else(|| hint.map(str::to_string)) {
        Some(p) => p,
        None => return ExitCode::SUCCESS,
    };
    let target = if pane_has_claude(&pane_id) {
        ROWS_WITH_CLAUDE
    } else {
        ROWS_WITHOUT_CLAUDE
    };
    let _ = Command::new("tmux")
        .args(["set-option", "-g", "status", &target.to_string()])
        .status();
    // Force an immediate redraw — without this the row-count change and
    // the per-pane #() substitutions wouldn't reflect visually until the
    // next status-interval tick.
    let _ = Command::new("tmux")
        .args(["refresh-client", "-S"])
        .status();
    ExitCode::SUCCESS
}

fn current_pane() -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "#{pane_id}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn pane_has_claude(pane_id: &str) -> bool {
    match server_id() {
        Some(s) => state::read_pane(&s, pane_id).is_some(),
        None => false,
    }
}

fn short_hash(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let bytes = h.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_hash_is_stable_and_8_chars() {
        let a = short_hash("/private/tmp/tmux-501/default");
        let b = short_hash("/private/tmp/tmux-501/default");
        assert_eq!(a, b);
        assert_eq!(a.len(), 8);
    }

    #[test]
    fn short_hash_differs_per_socket() {
        let a = short_hash("/private/tmp/tmux-501/default");
        let b = short_hash("/private/tmp/tmux-501/alt");
        assert_ne!(a, b);
    }
}
