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
    /// Claude sent a notification. A *blocking* one (permission prompt /
    /// elicitation) means it's waiting on the user mid-turn.
    Notification,
    /// A tool call finished — Claude is making forward progress, so any pending
    /// "waiting on you" latch is stale.
    PostToolUse,
}

pub fn run(kind: HookKind) -> std::process::ExitCode {
    let input = match read_stdin_json() {
        Some(v) => v,
        None => return std::process::ExitCode::SUCCESS,
    };
    match kind {
        HookKind::Stop => handle_stop(&input),
        HookKind::UserPromptSubmit => handle_prompt(&input),
        HookKind::Notification => handle_notification(&input),
        HookKind::PostToolUse => handle_post_tool_use(&input),
    }
    std::process::ExitCode::SUCCESS
}

fn handle_stop(input: &Value) {
    let Some(session_id) = resolve_session_id(input) else {
        return;
    };
    let mut s = state::read_session(&session_id).unwrap_or_default();
    s.last_turn_ts = Some(now_unix());
    s.last_notify_ts = None; // the turn ended; any pending prompt is moot
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
    s.last_notify_ts = None; // the user responded
    let _ = state::write_session(&session_id, &s);
}

/// Latch the "waiting on you" state when Claude raises a *blocking*
/// notification. Non-blocking kinds (idle nudge, auth, elicitation
/// acknowledgements) carry no pending decision, so they're ignored entirely —
/// idle is already covered by `Stop` + elapsed time.
fn handle_notification(input: &Value) {
    let kind = input.get("type").and_then(|v| v.as_str());
    if !is_blocking_notification(kind) {
        return;
    }
    let Some(session_id) = resolve_session_id(input) else {
        return;
    };
    let mut s = state::read_session(&session_id).unwrap_or_default();
    s.last_notify_ts = Some(now_unix());
    let _ = state::write_session(&session_id, &s);
}

/// Clear a pending "waiting on you" latch once a tool completes — Claude has
/// resumed (e.g. the user granted the permission). Skips the write when nothing
/// is latched, which is the common case (a tool ran with no prompt pending), so
/// the per-tool hook stays cheap.
fn handle_post_tool_use(input: &Value) {
    let Some(session_id) = resolve_session_id(input) else {
        return;
    };
    let Some(mut s) = state::read_session(&session_id) else {
        return;
    };
    if s.last_notify_ts.is_none() {
        return;
    }
    s.last_notify_ts = None;
    let _ = state::write_session(&session_id, &s);
}

/// Whether a `Notification` `type` represents Claude blocking on a user
/// decision. A denylist of the known non-blocking kinds, so an unknown or
/// absent type (older clients, or a tool like `AskUserQuestion` whose kind we
/// haven't catalogued) is treated as blocking rather than silently dropped.
fn is_blocking_notification(kind: Option<&str>) -> bool {
    !matches!(
        kind,
        Some("idle_prompt" | "auth_success" | "elicitation_complete" | "elicitation_response")
    )
}

fn read_stdin_json() -> Option<Value> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    if buf.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&buf).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocking_notification_classification() {
        // Pending a user decision — latches NeedsInput.
        assert!(is_blocking_notification(Some("permission_prompt")));
        assert!(is_blocking_notification(Some("elicitation_dialog")));
        // Unknown / absent type errs toward blocking (don't silently drop).
        assert!(is_blocking_notification(Some("some_future_kind")));
        assert!(is_blocking_notification(None));
        // Known non-blocking kinds carry no pending decision.
        assert!(!is_blocking_notification(Some("idle_prompt")));
        assert!(!is_blocking_notification(Some("auth_success")));
        assert!(!is_blocking_notification(Some("elicitation_complete")));
        assert!(!is_blocking_notification(Some("elicitation_response")));
    }
}
