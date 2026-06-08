//! Persistent per-pane and per-session state shared between the registrar,
//! hook, and tmux-renderer modes.
//!
//! Files live under [`crate::cache::cache_dir`] (`/tmp/ccstatus-<uid>`):
//!
//! ```text
//! pane/<server_id>/<TMUX_PANE>.json    written by registrar mode
//! session/<session_id>.json            written by hook mode
//! ```
//!
//! `<server_id>` is an 8-char hash of the tmux socket path so pane ids
//! from different tmux servers (which share a `%N` namespace) can't
//! collide. Session ids are UUIDs and globally unique, so they don't
//! need additional scoping.
//!
//! Both files are JSON objects written atomically via
//! [`crate::cache::write_atomic`]. Reads tolerate missing/corrupt files by
//! returning `None`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Value, json};

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
    /// Pre-rendered status elements from the registrar, keyed by element
    /// name (see [`crate::config::Element`]). Raw ANSI content; the daemon
    /// pulls the elements routed to each tmux surface and composes them.
    /// The live `warmth` element is not stored — the daemon computes it.
    pub elements: HashMap<String, String>,
}

/// Per-Claude-session record. The hook owns the turn fields
/// (`last_turn_ts`, `turn_count`); the registrar owns the **presence** fields
/// (`model`, `cwd`, `context_pct_used`, `cache_read_pct`, `claude_pid`),
/// written on every render — *including outside tmux*, so a non-tmux session
/// still appears in the fleet (as a non-jumpable row). Both writers
/// read-modify-write the whole record, so each preserves the other's fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionState {
    pub last_turn_ts: Option<i64>,
    /// When the user last submitted a prompt (the UserPromptSubmit hook). A
    /// turn is in progress — Claude is *working* — when this is newer than
    /// `last_turn_ts`.
    pub last_prompt_ts: Option<i64>,
    pub model: Option<String>,
    pub turn_count: u64,
    pub context_pct_used: Option<u32>,
    pub cache_read_pct: Option<u32>,
    /// Working directory (presence). Plain text, unlike the pane's rendered
    /// `cwd` element.
    pub cwd: Option<String>,
    /// The Claude process pid (presence) — the fleet's liveness anchor for
    /// sessions with no pane file (i.e. outside tmux).
    pub claude_pid: Option<u32>,
    /// `TERM_PROGRAM` of the hosting terminal (`iTerm.app`, `Apple_Terminal`,
    /// `tmux`, …). Lets the fleet raise the OS window for a non-tmux Claude.
    pub term_program: Option<String>,
    /// `ITERM_SESSION_ID` (`wNtNpN:GUID`) when hosted directly in iTerm2 — the
    /// addressable handle for an iTerm2 window jump (see [`crate::window`]).
    pub iterm_session_id: Option<String>,
    /// The graphical display backing the session (`WAYLAND_DISPLAY`, else
    /// `DISPLAY`), or `None` when there is none (a headless/SSH Claude). On
    /// Linux this is what makes a non-tmux session window-jumpable: with no
    /// display there is no window to raise (see [`crate::window`]).
    pub display: Option<String>,
}

pub fn pane_path(server_id: &str, pane_id: &str) -> PathBuf {
    cache::cache_dir()
        .join("pane")
        .join(sanitize(server_id))
        .join(format!("{}.json", sanitize(pane_id)))
}

pub fn session_path(session_id: &str) -> PathBuf {
    cache::cache_dir()
        .join("session")
        .join(format!("{}.json", sanitize(session_id)))
}

pub fn read_pane(server_id: &str, pane_id: &str) -> Option<PaneState> {
    let text = std::fs::read_to_string(pane_path(server_id, pane_id)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let elements = v
        .get("elements")
        .and_then(|x| x.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
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
        elements,
    })
}

pub fn write_pane(server_id: &str, pane_id: &str, s: &PaneState) -> std::io::Result<()> {
    let v = json!({
        "session_id": s.session_id,
        "claude_pid": s.claude_pid,
        "pane_tty": s.pane_tty,
        "transcript_path": s.transcript_path,
        "registered_at": s.registered_at,
        "last_warmth": s.last_warmth,
        "elements": s.elements,
    });
    cache::write_atomic(&pane_path(server_id, pane_id), &v.to_string())
}

pub fn read_session(session_id: &str) -> Option<SessionState> {
    let text = std::fs::read_to_string(session_path(session_id)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(SessionState {
        last_turn_ts: v.get("last_turn_ts").and_then(|x| x.as_i64()),
        last_prompt_ts: v.get("last_prompt_ts").and_then(|x| x.as_i64()),
        model: v.get("model").and_then(|x| x.as_str()).map(str::to_string),
        turn_count: v.get("turn_count").and_then(|x| x.as_u64()).unwrap_or(0),
        context_pct_used: v
            .get("context_pct_used")
            .and_then(|x| x.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        cache_read_pct: v
            .get("cache_read_pct")
            .and_then(|x| x.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        cwd: v.get("cwd").and_then(|x| x.as_str()).map(str::to_string),
        claude_pid: v
            .get("claude_pid")
            .and_then(|x| x.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        term_program: v
            .get("term_program")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        iterm_session_id: v
            .get("iterm_session_id")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        display: v
            .get("display")
            .and_then(|x| x.as_str())
            .map(str::to_string),
    })
}

pub fn write_session(session_id: &str, s: &SessionState) -> std::io::Result<()> {
    let v = json!({
        "last_turn_ts": s.last_turn_ts,
        "last_prompt_ts": s.last_prompt_ts,
        "model": s.model,
        "turn_count": s.turn_count,
        "context_pct_used": s.context_pct_used,
        "cache_read_pct": s.cache_read_pct,
        "cwd": s.cwd,
        "claude_pid": s.claude_pid,
        "term_program": s.term_program,
        "iterm_session_id": s.iterm_session_id,
        "display": s.display,
    });
    cache::write_atomic(&session_path(session_id), &v.to_string())
}

pub fn remove_pane(server_id: &str, pane_id: &str) {
    let _ = std::fs::remove_file(pane_path(server_id, pane_id));
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
