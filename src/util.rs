//! Small helpers shared across modes.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Seconds since the UNIX epoch. Falls back to 0 if the system clock is
/// before the epoch (which would be... unusual).
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Extract the Claude session id from a stdin payload. Prefers an explicit
/// `session_id` top-level field; falls back to the basename (without
/// extension) of `transcript_path` so older / leaner payloads still work.
pub fn resolve_session_id(input: &Value) -> Option<String> {
    if let Some(s) = input.get("session_id").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    let path = input.get("transcript_path").and_then(|v| v.as_str())?;
    let basename = path.rsplit('/').next()?;
    let stem = basename.rsplit_once('.').map(|(s, _)| s).unwrap_or(basename);
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}
