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
pub fn on_focus(pane_id: &str) -> ExitCode {
    let target = if pane_has_claude(pane_id) {
        ROWS_WITH_CLAUDE
    } else {
        ROWS_WITHOUT_CLAUDE
    };
    // tmux short-circuits if the value already matches, so this is cheap
    // to call on every focus event.
    let _ = Command::new("tmux")
        .args(["set-option", "-g", "status", &target.to_string()])
        .status();
    // Force an immediate status redraw — otherwise the new pane's #()
    // substitutions wouldn't appear until the next status-interval tick,
    // and the row-count change wouldn't apply visually until something
    // else triggered a redraw.
    let _ = Command::new("tmux")
        .args(["refresh-client", "-S"])
        .status();
    ExitCode::SUCCESS
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
