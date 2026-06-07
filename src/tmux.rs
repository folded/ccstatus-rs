//! Helpers for inspecting the tmux environment, and the one-shot tmux
//! command seam (`Tmux`).

use std::env;
use std::process::Command;

use sha2::{Digest, Sha256};

/// One-shot tmux commands (each forks a `tmux` process). The persistent
/// control connection (`control.rs`) is a SEPARATE seam: it carries focus
/// events and `refresh-client -S`, and must never fork or send a size.
pub trait Tmux {
    // Queries
    /// `display-message -t S -p '#{pane_id}'` — the session's focused pane.
    fn focused_pane(&self, session: &str) -> Option<String>;
    /// `display -t P -p '#{session_id}'` — the session containing a pane.
    fn session_of(&self, pane: &str) -> Option<String>;
    /// `display -t P -p '#{pane_tty}'` — a pane's controlling tty.
    fn pane_tty(&self, pane: &str) -> Option<String>;

    // Session-local overrides (the handler's writes)
    /// `set-option -t S name value`.
    fn set_session(&self, session: &str, name: &str, value: &str);
    /// `set-option -u -t S name`.
    fn unset_session(&self, session: &str, name: &str);

    // Global options (only --tmux-reset touches these)
    /// `show-options -gv name` (trimmed). The user's global value, never the
    /// session-effective one, so our overrides can't feed back into a later
    /// compose.
    fn global(&self, name: &str) -> String;
    /// `set-option -g name value`.
    fn set_global(&self, name: &str, value: &str);
    /// `set-option -gu name`.
    fn unset_global(&self, name: &str);

    /// Repaint all clients. One-shot fork — used by `--tmux-reset` ONLY. The
    /// handler refreshes over its control connection (`Writer`), not here.
    fn refresh(&self);
}

/// Production adapter: each method builds args and spawns `tmux`.
pub struct CliTmux;

impl CliTmux {
    fn display(&self, target: &str, fmt: &str) -> Option<String> {
        let out = Command::new("tmux")
            .args(["display-message", "-t", target, "-p", fmt])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }
}

impl Tmux for CliTmux {
    fn focused_pane(&self, session: &str) -> Option<String> {
        self.display(session, "#{pane_id}")
    }

    fn session_of(&self, pane: &str) -> Option<String> {
        self.display(pane, "#{session_id}")
    }

    fn pane_tty(&self, pane: &str) -> Option<String> {
        self.display(pane, "#{pane_tty}")
    }

    fn set_session(&self, session: &str, name: &str, value: &str) {
        let _ = Command::new("tmux")
            .args(["set-option", "-t", session, name, value])
            .status();
    }

    fn unset_session(&self, session: &str, name: &str) {
        let _ = Command::new("tmux")
            .args(["set-option", "-u", "-t", session, name])
            .status();
    }

    fn global(&self, name: &str) -> String {
        let out = Command::new("tmux").args(["show-options", "-gv", name]).output();
        match out {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout).trim_end_matches('\n').to_string()
            }
            _ => String::new(),
        }
    }

    fn set_global(&self, name: &str, value: &str) {
        let _ = Command::new("tmux").args(["set-option", "-g", name, value]).status();
    }

    fn unset_global(&self, name: &str) {
        let _ = Command::new("tmux").args(["set-option", "-gu", name]).status();
    }

    fn refresh(&self) {
        let _ = Command::new("tmux").arg("refresh-client").status();
    }
}

/// The session-local bar options the handler overrides (and `restore_session`
/// reverts).
pub const SESSION_BAR_OPTS: [&str; 4] = ["status-format", "status", "status-left", "status-right"];

/// Drop every bar override for a session, reverting it to inheriting the
/// user's global config.
pub fn restore_session(t: &dyn Tmux, session: &str) {
    for opt in SESSION_BAR_OPTS {
        t.unset_session(session, opt);
    }
}

/// tmux's `status` is a choice option (`off`/`on`/`2`..`5`); `"1"` is
/// rejected, so a single row must be spelled `on`.
pub fn status_value(rows: usize) -> String {
    match rows {
        0 => "off".to_string(),
        1 => "on".to_string(),
        n => n.to_string(),
    }
}

/// The effective global `status-format[0]` (the powerline window list),
/// falling back to tmux's built-in default template when empty.
pub fn powerline_row(t: &dyn Tmux) -> String {
    let value = t.global("status-format[0]");
    if value.is_empty() {
        DEFAULT_STATUS_FORMAT_0.to_string()
    } else {
        value
    }
}

/// Restore the global bar to tmux defaults. Manual cleanup for when a handler
/// left ccstatus content in the tmux status options and there's no live
/// handler to restore it. Use after a crash or when ccstatus is uninstalled.
pub fn reset(t: &dyn Tmux) {
    // status-format[0] must be written back to tmux's built-in default
    // template explicitly. `set -gu` does NOT restore it once the slot has
    // been touched (macOS tmux leaves it empty -> black bar), so unsetting
    // here is exactly what left the bar broken.
    t.set_global("status-format[0]", DEFAULT_STATUS_FORMAT_0);
    // Higher slots have no built-in default and only render as extra rows
    // when status >= 2; unsetting them is correct.
    for name in [
        "@ccstatus-active",
        "status-format[1]",
        "status-format[2]",
        "status-format[3]",
        "status-format[4]",
        "status-format[5]",
    ] {
        t.unset_global(name);
    }
    t.set_global("status", "on");
    t.refresh();
}

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

/// A recorded mutation, for asserting the ordered write log in tests.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    SetSession(String, String, String),
    UnsetSession(String, String),
    SetGlobal(String, String),
    UnsetGlobal(String),
    Refresh,
}

/// Test adapter: records writes, serves canned reads.
#[cfg(test)]
pub struct FakeTmux {
    pub focused: std::cell::RefCell<std::collections::HashMap<String, String>>, // session -> pane
    pub globals: std::cell::RefCell<std::collections::HashMap<String, String>>, // name -> value
    pub writes: std::cell::RefCell<Vec<Write>>, // ordered log of mutations
}

#[cfg(test)]
impl FakeTmux {
    pub fn new() -> Self {
        Self {
            focused: std::cell::RefCell::new(std::collections::HashMap::new()),
            globals: std::cell::RefCell::new(std::collections::HashMap::new()),
            writes: std::cell::RefCell::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl Tmux for FakeTmux {
    fn focused_pane(&self, session: &str) -> Option<String> {
        self.focused.borrow().get(session).cloned()
    }

    fn session_of(&self, _pane: &str) -> Option<String> {
        None
    }

    fn pane_tty(&self, _pane: &str) -> Option<String> {
        None
    }

    fn set_session(&self, session: &str, name: &str, value: &str) {
        self.writes.borrow_mut().push(Write::SetSession(
            session.to_string(),
            name.to_string(),
            value.to_string(),
        ));
    }

    fn unset_session(&self, session: &str, name: &str) {
        self.writes
            .borrow_mut()
            .push(Write::UnsetSession(session.to_string(), name.to_string()));
    }

    fn global(&self, name: &str) -> String {
        self.globals.borrow().get(name).cloned().unwrap_or_default()
    }

    fn set_global(&self, name: &str, value: &str) {
        self.writes
            .borrow_mut()
            .push(Write::SetGlobal(name.to_string(), value.to_string()));
    }

    fn unset_global(&self, name: &str) {
        self.writes.borrow_mut().push(Write::UnsetGlobal(name.to_string()));
    }

    fn refresh(&self) {
        self.writes.borrow_mut().push(Write::Refresh);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_session_emits_four_unsets() {
        let t = FakeTmux::new();
        restore_session(&t, "$1");
        assert_eq!(
            *t.writes.borrow(),
            vec![
                Write::UnsetSession("$1".into(), "status-format".into()),
                Write::UnsetSession("$1".into(), "status".into()),
                Write::UnsetSession("$1".into(), "status-left".into()),
                Write::UnsetSession("$1".into(), "status-right".into()),
            ]
        );
    }

    #[test]
    fn status_value_spells_single_row_as_on() {
        assert_eq!(status_value(0), "off");
        assert_eq!(status_value(1), "on");
        assert_eq!(status_value(3), "3");
    }

    #[test]
    fn powerline_row_falls_back_to_default() {
        let t = FakeTmux::new();
        assert_eq!(powerline_row(&t), DEFAULT_STATUS_FORMAT_0);
        t.globals
            .borrow_mut()
            .insert("status-format[0]".into(), "custom".into());
        assert_eq!(powerline_row(&t), "custom");
    }

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
