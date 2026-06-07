//! Helpers for inspecting the tmux environment.

use std::env;

use sha2::{Digest, Sha256};

/// tmux's built-in default value for `status-format[0]` — the powerline
/// window list (status-left + `#{W:…}` window loop + status-right).
///
/// We read the *effective* global `status-format[0]` at activate time and
/// reuse it as the session's powerline row, but fall back to this when the
/// global is empty/unset. Copied verbatim from a fresh tmux server's
/// `show-options -g status-format[0]`. (tmux does not expose the default
/// via `show-options` once the slot has been touched, and `set -gu` does
/// not restore it — on macOS tmux it leaves the slot empty.)
pub const DEFAULT_STATUS_FORMAT_0: &str = "#[align=left range=left #{E:status-left-style}]#[push-default]#{T;=/#{status-left-length}:status-left}#[pop-default]#[norange default]#[list=on align=#{status-justify}]#[list=left-marker]<#[list=right-marker]>#[list=on]#{W:#[range=window|#{window_index} #{E:window-status-style}#{?#{&&:#{window_last_flag},#{!=:#{E:window-status-last-style},default}}, #{E:window-status-last-style},}#{?#{&&:#{window_bell_flag},#{!=:#{E:window-status-bell-style},default}}, #{E:window-status-bell-style},#{?#{&&:#{||:#{window_activity_flag},#{window_silence_flag}},#{!=:#{E:window-status-activity-style},default}}, #{E:window-status-activity-style},}}]#[push-default]#{T:window-status-format}#[pop-default]#[norange default]#{?loop_last_flag,,#{window-status-separator}},#[range=window|#{window_index} list=focus #{?#{!=:#{E:window-status-current-style},default},#{E:window-status-current-style},#{E:window-status-style}}#{?#{&&:#{window_last_flag},#{!=:#{E:window-status-last-style},default}}, #{E:window-status-last-style},}#{?#{&&:#{window_bell_flag},#{!=:#{E:window-status-bell-style},default}}, #{E:window-status-bell-style},#{?#{&&:#{||:#{window_activity_flag},#{window_silence_flag}},#{!=:#{E:window-status-activity-style},default}}, #{E:window-status-activity-style},}}]#[push-default]#{T:window-status-current-format}#[pop-default]#[norange list=on default]#{?loop_last_flag,,#{window-status-separator}}}#[nolist align=right range=right #{E:status-right-style}]#[push-default]#{T;=/#{status-right-length}:status-right}#[pop-default]#[norange default]";

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
