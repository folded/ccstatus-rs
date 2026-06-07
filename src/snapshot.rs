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
        let status = read_option(conn, "status")?.unwrap_or_else(|| "on".to_string());
        let status_position = read_option(conn, "status-position")?
            .unwrap_or_else(|| "bottom".to_string());
        let mut status_format: [Option<String>; STATUS_FORMAT_SLOTS] =
            Default::default();
        for (i, slot) in status_format.iter_mut().enumerate() {
            *slot = read_indexed(conn, "status-format", i)?;
        }
        Ok(Self {
            status,
            status_position,
            status_format,
        })
    }

    pub fn restore(&self, conn: &mut Connection) -> Result<(), String> {
        set_option(conn, "status", &self.status)?;
        set_option(conn, "status-position", &self.status_position)?;
        for (i, slot) in self.status_format.iter().enumerate() {
            match slot {
                Some(v) => set_indexed(conn, "status-format", i, v)?,
                None => unset_indexed(conn, "status-format", i)?,
            }
        }
        Ok(())
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

fn set_option(conn: &mut Connection, name: &str, value: &str) -> Result<(), String> {
    let escaped = escape_for_tmux(value);
    let r = conn.cmd(&format!("set-option -g {name} \"{escaped}\""))?;
    if !r.ok {
        return Err(format!("set-option -g {name} failed: {}", r.output));
    }
    Ok(())
}

fn set_indexed(
    conn: &mut Connection,
    name: &str,
    index: usize,
    value: &str,
) -> Result<(), String> {
    set_option(conn, &format!("'{name}[{index}]'"), value)
}

fn unset_indexed(conn: &mut Connection, name: &str, index: usize) -> Result<(), String> {
    let r = conn.cmd(&format!("set-option -gu '{name}[{index}]'"))?;
    if !r.ok {
        return Err(format!("set-option -gu {name}[{index}] failed: {}", r.output));
    }
    Ok(())
}

/// Escape a string value for inclusion in a tmux command argument
/// surrounded by double quotes. tmux's quoting follows shell-ish rules:
/// inside `"..."`, a `\"` is a literal quote and `\\` is a literal
/// backslash. The format-string `#{…}` and `#(…)` survive unescaped.
fn escape_for_tmux(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
