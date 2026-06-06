//! Persistent per-pane and per-session state shared between the registrar,
//! hook, and tmux-renderer modes.
//!
//! Files live under [`crate::cache::cache_dir`] (`/tmp/claude`):
//!
//! ```text
//! /tmp/claude/pane/<TMUX_PANE>.json     written by registrar mode
//! /tmp/claude/session/<session_id>.json written by hook mode
//! ```
//!
//! Both files are JSON objects written atomically via
//! [`crate::cache::write_atomic`]. Reads tolerate missing/corrupt files by
//! returning `None`.

#![allow(dead_code)]

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::cache;

pub const WARMTH_WARM: &str = "warm";
pub const WARMTH_COLD: &str = "cold";

#[derive(Debug, Clone)]
pub struct PaneState {
    pub session_id: String,
    pub claude_pid: u32,
    pub pane_tty: String,
    pub transcript_path: Option<String>,
    pub registered_at: i64,
    pub last_warmth: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionState {
    pub last_turn_ts: Option<i64>,
    pub model: Option<String>,
    pub turn_count: u64,
    pub context_pct_used: Option<u32>,
    pub cache_read_pct: Option<u32>,
}

pub fn pane_path(pane_id: &str) -> PathBuf {
    cache::cache_dir()
        .join("pane")
        .join(format!("{}.json", sanitize(pane_id)))
}

pub fn session_path(session_id: &str) -> PathBuf {
    cache::cache_dir()
        .join("session")
        .join(format!("{}.json", sanitize(session_id)))
}

pub fn read_pane(pane_id: &str) -> Option<PaneState> {
    let text = std::fs::read_to_string(pane_path(pane_id)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(PaneState {
        session_id: v.get("session_id")?.as_str()?.to_string(),
        claude_pid: u32::try_from(v.get("claude_pid")?.as_u64()?).ok()?,
        pane_tty: v.get("pane_tty")?.as_str()?.to_string(),
        transcript_path: v
            .get("transcript_path")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        registered_at: v.get("registered_at")?.as_i64()?,
        last_warmth: v
            .get("last_warmth")
            .and_then(|x| x.as_str())
            .map(str::to_string),
    })
}

pub fn write_pane(pane_id: &str, s: &PaneState) -> std::io::Result<()> {
    let v = json!({
        "session_id": s.session_id,
        "claude_pid": s.claude_pid,
        "pane_tty": s.pane_tty,
        "transcript_path": s.transcript_path,
        "registered_at": s.registered_at,
        "last_warmth": s.last_warmth,
    });
    cache::write_atomic(&pane_path(pane_id), &v.to_string())
}

pub fn read_session(session_id: &str) -> Option<SessionState> {
    let text = std::fs::read_to_string(session_path(session_id)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(SessionState {
        last_turn_ts: v.get("last_turn_ts").and_then(|x| x.as_i64()),
        model: v
            .get("model")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        turn_count: v
            .get("turn_count")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        context_pct_used: v
            .get("context_pct_used")
            .and_then(|x| x.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        cache_read_pct: v
            .get("cache_read_pct")
            .and_then(|x| x.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
    })
}

pub fn write_session(session_id: &str, s: &SessionState) -> std::io::Result<()> {
    let v = json!({
        "last_turn_ts": s.last_turn_ts,
        "model": s.model,
        "turn_count": s.turn_count,
        "context_pct_used": s.context_pct_used,
        "cache_read_pct": s.cache_read_pct,
    });
    cache::write_atomic(&session_path(session_id), &v.to_string())
}

pub fn remove_pane(pane_id: &str) {
    let _ = std::fs::remove_file(pane_path(pane_id));
}

/// Strip path separators and other awkward characters so that an externally
/// supplied id can't escape the state directory or break shell substitution.
/// tmux pane ids look like `%5`; session ids are UUID-ish; both survive.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | '\n' | '\r' | '\t' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_tmux_pane_format() {
        assert_eq!(sanitize("%5"), "%5");
        assert_eq!(sanitize("%123"), "%123");
    }

    #[test]
    fn sanitize_keeps_uuid_format() {
        assert_eq!(
            sanitize("0123abcd-89ef-4567-89ab-cdef01234567"),
            "0123abcd-89ef-4567-89ab-cdef01234567"
        );
    }

    #[test]
    fn sanitize_strips_separators() {
        assert_eq!(sanitize("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize("a\nb"), "a_b");
    }
}
