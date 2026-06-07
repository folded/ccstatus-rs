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

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: String,
    pub status_position: String,
    /// `None` for slots that were unset; restore via `set -gu status-format[N]`.
    pub status_format: [Option<String>; STATUS_FORMAT_SLOTS],
}

impl Snapshot {
    pub fn capture(conn: &mut Connection) -> Result<Self, String> {
        // Detect pollution from a crashed predecessor daemon. When the
        // daemon enters Active state it sets @ccstatus-active=1; on
        // graceful Deactivate / shutdown it unsets the option. A
        // surviving value means a previous daemon was killed without
        // restoring the user's bar — every status/status-format value
        // we'd read right now might be our own leftover rather than
        // the user's true config.
        let polluted = read_option(conn, "@ccstatus-active")?.is_some();
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
        })
    }

    /// Fire-and-forget restore via the split Writer half (the
    /// event-driven daemon path). Errors are swallowed individually —
    /// if a single set-option fails, the rest still get attempted.
    ///
    /// Always unsets status-format[0..N] regardless of captured value:
    /// the captured values can't be trusted to be the user's true
    /// intent (a crashed predecessor daemon may have left its own
    /// content in those slots). Unsetting them returns tmux to its
    /// built-in default rendering, which is what the user got from
    /// their config in the first place.
    ///
    /// Also clears the @ccstatus-active sentinel so the next daemon
    /// start sees a clean shutdown and trusts its captured values.
    pub fn apply_via_writer(&self, w: &mut crate::control::Writer) {
        let _ = w.send(&format!("set-option -g status {}", quoted(&self.status)));
        let _ = w.send(&format!(
            "set-option -g status-position {}",
            quoted(&self.status_position)
        ));
        for i in 0..STATUS_FORMAT_SLOTS {
            let _ = w.send(&format!("set-option -gu 'status-format[{i}]'"));
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
    // `show-options -gv` prints the value (empty if unset). We treat
    // empty as "unset" for the options we care about — none of the bar
    // settings are legitimately empty strings.
    let r = conn.cmd(&format!("show-options -gv {name}"))?;
    if !r.ok {
        return Err(format!("show-options -gv {name} failed: {}", r.output));
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
