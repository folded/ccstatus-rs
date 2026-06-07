//! Claude Code hook entry points (`ccstatus --hook <kind>`).
//!
//! Hooks read their own JSON payload from stdin (a different schema from
//! the statusline payload) and update `session/<session_id>.json`. They
//! must never block Claude: parse failures, missing fields, IO errors all
//! return success.

use std::io::Read;

use serde_json::Value;

use crate::state;
use crate::util::{now_unix, resolve_session_id};

#[derive(Debug, Clone, Copy)]
pub enum HookKind {
    /// Claude finished a turn — now waiting for input.
    Stop,
    /// The user submitted a prompt — a turn is starting (Claude is working).
    UserPromptSubmit,
}

pub fn run(kind: HookKind) -> std::process::ExitCode {
    let input = match read_stdin_json() {
        Some(v) => v,
        None => return std::process::ExitCode::SUCCESS,
    };
    match kind {
        HookKind::Stop => handle_stop(&input),
        HookKind::UserPromptSubmit => handle_prompt(&input),
    }
    std::process::ExitCode::SUCCESS
}

fn handle_stop(input: &Value) {
    let Some(session_id) = resolve_session_id(input) else {
        return;
    };
    let mut s = state::read_session(&session_id).unwrap_or_default();
    s.last_turn_ts = Some(now_unix());
    s.turn_count = s.turn_count.saturating_add(1);
    if let Some(m) = input
        .pointer("/model/display_name")
        .or_else(|| input.get("model"))
        .and_then(|v| v.as_str())
    {
        s.model = Some(m.to_string());
    }
    let _ = state::write_session(&session_id, &s);
}

fn handle_prompt(input: &Value) {
    let Some(session_id) = resolve_session_id(input) else {
        return;
    };
    let mut s = state::read_session(&session_id).unwrap_or_default();
    s.last_prompt_ts = Some(now_unix());
    let _ = state::write_session(&session_id, &s);
}

fn read_stdin_json() -> Option<Value> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    if buf.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&buf).ok()
}
