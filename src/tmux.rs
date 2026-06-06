//! Helpers for inspecting the tmux environment.

use std::env;

use sha2::{Digest, Sha256};

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
