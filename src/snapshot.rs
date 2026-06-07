//! Snapshot and restore of the tmux status-bar options that the daemon
//! manipulates.
//!
//! On start the daemon captures the user's `status`, `status-position`,
//! and every `status-format[N]` slot — including whether each slot is set
//! at all. On shutdown (graceful or via crash-recovery), it writes those
//! exact values back so the user's bar returns to what they had before
//! ccstatus injected anything.

use serde_json::{json, Value};

use crate::cache;
use crate::control::Connection;

/// Number of status-format slots tmux supports.
pub const STATUS_FORMAT_SLOTS: usize = 6;

/// tmux's built-in default value for `status-format[0]` — the powerline
/// window list (status-left + `#{W:…}` window loop + status-right).
///
/// The default isn't stored as an option (`show-options -gv` returns it
/// on a *fresh* server, but once any index has been touched, `set -gu`
/// does **not** restore it: on macOS tmux it leaves the slot empty,
/// which renders as a blank bar). So whenever we need to put the user's
/// bar back and the captured slot was unset/empty, we must write this
/// template explicitly rather than unsetting. Copied verbatim from a
/// fresh tmux server's `show-options -g status-format[0]`.
pub const DEFAULT_STATUS_FORMAT_0: &str = "#[align=left range=left #{E:status-left-style}]#[push-default]#{T;=/#{status-left-length}:status-left}#[pop-default]#[norange default]#[list=on align=#{status-justify}]#[list=left-marker]<#[list=right-marker]>#[list=on]#{W:#[range=window|#{window_index} #{E:window-status-style}#{?#{&&:#{window_last_flag},#{!=:#{E:window-status-last-style},default}}, #{E:window-status-last-style},}#{?#{&&:#{window_bell_flag},#{!=:#{E:window-status-bell-style},default}}, #{E:window-status-bell-style},#{?#{&&:#{||:#{window_activity_flag},#{window_silence_flag}},#{!=:#{E:window-status-activity-style},default}}, #{E:window-status-activity-style},}}]#[push-default]#{T:window-status-format}#[pop-default]#[norange default]#{?loop_last_flag,,#{window-status-separator}},#[range=window|#{window_index} list=focus #{?#{!=:#{E:window-status-current-style},default},#{E:window-status-current-style},#{E:window-status-style}}#{?#{&&:#{window_last_flag},#{!=:#{E:window-status-last-style},default}}, #{E:window-status-last-style},}#{?#{&&:#{window_bell_flag},#{!=:#{E:window-status-bell-style},default}}, #{E:window-status-bell-style},#{?#{&&:#{||:#{window_activity_flag},#{window_silence_flag}},#{!=:#{E:window-status-activity-style},default}}, #{E:window-status-activity-style},}}]#[push-default]#{T:window-status-current-format}#[pop-default]#[norange list=on default]#{?loop_last_flag,,#{window-status-separator}}}#[nolist align=right range=right #{E:status-right-style}]#[push-default]#{T;=/#{status-right-length}:status-right}#[pop-default]#[norange default]";

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: String,
    pub status_position: String,
    /// `None` for slots that were unset; restore via `set -gu status-format[N]`.
    pub status_format: [Option<String>; STATUS_FORMAT_SLOTS],
    /// True if the `@ccstatus-active` sentinel was set when we
    /// captured. Means a previous daemon left without restoring; the
    /// caller should apply this (already-defaulted) snapshot
    /// immediately to clean up the live tmux state.
    pub was_polluted: bool,
}

impl Snapshot {
    pub fn capture(conn: &mut Connection) -> Result<Self, String> {
        // Detect pollution from a crashed predecessor daemon. Two
        // signals:
        //   (a) the @ccstatus-active sentinel set during activate.
        //   (b) any of status-format[1..5] being set. tmux's defaults
        //       for those higher indices are unset; we set them during
        //       activate. A non-empty value there is a strong hint
        //       that the previous daemon didn't clean up. This catches
        //       the case of (a) being absent — e.g. predecessor was
        //       an older binary, or @ccstatus-active was manually
        //       unset.
        // Tradeoff: a user who runs with status=2 and a custom
        // status-format[1] gets misidentified and reset to defaults.
        let sentinel = read_option(conn, "@ccstatus-active")?.is_some();
        let any_extra_format = (1..STATUS_FORMAT_SLOTS).any(|i| {
            read_indexed(conn, "status-format", i)
                .ok()
                .flatten()
                .is_some()
        });
        let polluted = sentinel || any_extra_format;
        let status = if polluted {
            "on".to_string()
        } else {
            read_option(conn, "status")?.unwrap_or_else(|| "on".to_string())
        };
        let status_position = read_option(conn, "status-position")?
            .unwrap_or_else(|| "bottom".to_string());
        let mut status_format: [Option<String>; STATUS_FORMAT_SLOTS] =
            Default::default();
        if !polluted {
            for (i, slot) in status_format.iter_mut().enumerate() {
                *slot = read_indexed(conn, "status-format", i)?;
            }
        }
        Ok(Self {
            status,
            status_position,
            status_format,
            was_polluted: polluted,
        })
    }

    /// Fire-and-forget restore via the split Writer half (the
    /// event-driven daemon path). Errors are swallowed individually —
    /// if a single set-option fails, the rest still get attempted.
    ///
    /// Restores each captured `status-format[N]`:
    ///   - captured `Some(v)` → write `v` back verbatim;
    ///   - captured `None` at slot 0 → write the built-in default
    ///     template (`DEFAULT_STATUS_FORMAT_0`). We must NOT `set -gu`
    ///     here: on macOS tmux unsetting leaves the slot empty rather
    ///     than restoring the default, which renders as a blank (black)
    ///     bar — the user's powerline window list vanishes;
    ///   - captured `None` at slots 1..N → unset (those higher slots
    ///     have no built-in default and only render as extra rows when
    ///     `status >= 2`, which the restored `status` value governs).
    ///
    /// Also clears the @ccstatus-active sentinel so the next daemon
    /// start sees a clean shutdown and trusts its captured values.
    pub fn apply_via_writer(&self, w: &mut crate::control::Writer) {
        let _ = w.send(&format!("set-option -g status {}", quoted(&self.status)));
        let _ = w.send(&format!(
            "set-option -g status-position {}",
            quoted(&self.status_position)
        ));
        for (i, slot) in self.status_format.iter().enumerate() {
            match slot {
                Some(v) => {
                    let escaped = escape_for_tmux(v);
                    let _ = w.send(&format!("set-option -g 'status-format[{i}]' \"{escaped}\""));
                }
                None if i == 0 => {
                    let escaped = escape_for_tmux(DEFAULT_STATUS_FORMAT_0);
                    let _ = w.send(&format!("set-option -g 'status-format[0]' \"{escaped}\""));
                }
                None => {
                    let _ = w.send(&format!("set-option -gu 'status-format[{i}]'"));
                }
            }
        }
        let _ = w.send("set-option -gu @ccstatus-active");
    }

    pub fn to_json(&self) -> Value {
        let formats: Vec<Value> = self
            .status_format
            .iter()
            .map(|s| match s {
                Some(v) => Value::String(v.clone()),
                None => Value::Null,
            })
            .collect();
        json!({
            "status": self.status,
            "status_position": self.status_position,
            "status_format": formats,
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        let mut s = Self {
            status: v.get("status")?.as_str()?.to_string(),
            status_position: v.get("status_position")?.as_str()?.to_string(),
            status_format: Default::default(),
            was_polluted: v.get("was_polluted").and_then(|x| x.as_bool()).unwrap_or(false),
        };
        let arr = v.get("status_format")?.as_array()?;
        for (i, slot) in s.status_format.iter_mut().enumerate() {
            *slot = arr
                .get(i)
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
        Some(s)
    }
}

/// Persist a snapshot under the per-server cache directory so a crashed
/// daemon can still restore the user's state on restart.
pub fn save(server_id: &str, snapshot: &Snapshot) -> std::io::Result<()> {
    cache::write_atomic(
        &snapshot_path(server_id),
        &snapshot.to_json().to_string(),
    )
}

/// Read a previously-saved snapshot. Returns `None` if missing or
/// unparseable; the caller decides whether to fall back to a fresh
/// capture or skip restore. Used by crash recovery (milestone 9).
#[allow(dead_code)]
pub fn load(server_id: &str) -> Option<Snapshot> {
    let text = std::fs::read_to_string(snapshot_path(server_id)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Snapshot::from_json(&v)
}

#[allow(dead_code)]
pub fn forget(server_id: &str) {
    let _ = std::fs::remove_file(snapshot_path(server_id));
}

fn snapshot_path(server_id: &str) -> std::path::PathBuf {
    cache::cache_dir()
        .join("server")
        .join(sanitize(server_id))
        .join("snapshot.json")
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c == '/' || c.is_control() { '_' } else { c })
        .collect()
}

fn read_option(conn: &mut Connection, name: &str) -> Result<Option<String>, String> {
    // `show-options -gv` prints the value (empty if unset for built-in
    // options) but returns `%error invalid option: <name>` for user
    // options that have never been set. Treat both empty *and* error
    // as "unset" — the caller doesn't care about the distinction and
    // crashing the daemon at snapshot time over a missing user-defined
    // option is worse than the loss of fidelity.
    let r = conn.cmd(&format!("show-options -gv {name}"))?;
    if !r.ok {
        return Ok(None);
    }
    let v = r.output.trim().to_string();
    Ok(if v.is_empty() { None } else { Some(v) })
}

fn read_indexed(
    conn: &mut Connection,
    name: &str,
    index: usize,
) -> Result<Option<String>, String> {
    read_option(conn, &format!("'{name}[{index}]'"))
}

/// Escape a string value for inclusion in a tmux command argument
/// surrounded by double quotes. tmux's quoting follows shell-ish rules:
/// inside `"..."`, a `\"` is a literal quote and `\\` is a literal
/// backslash. The format-string `#{…}` and `#(…)` survive unescaped.
pub fn escape_for_tmux(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Quote a short scalar (option value with no whitespace / special
/// characters) for use after `set-option`. Bare values are accepted by
/// tmux's parser; wrapping in single quotes is safer for diagnostics.
fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_preserves_nulls() {
        let mut s = Snapshot {
            status: "on".into(),
            status_position: "bottom".into(),
            status_format: Default::default(),
            was_polluted: false,
        };
        s.status_format[0] = Some("first".into());
        s.status_format[2] = Some("third".into());
        let v = s.to_json();
        let s2 = Snapshot::from_json(&v).unwrap();
        assert_eq!(s2.status, "on");
        assert_eq!(s2.status_position, "bottom");
        assert_eq!(s2.status_format[0].as_deref(), Some("first"));
        assert_eq!(s2.status_format[1].as_deref(), None);
        assert_eq!(s2.status_format[2].as_deref(), Some("third"));
    }

    #[test]
    fn escape_doubles_backslash_then_quote() {
        assert_eq!(escape_for_tmux(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}
