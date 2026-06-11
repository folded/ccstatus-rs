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
/// notification mid-turn. Non-blocking kinds (auth, elicitation
/// acknowledgements) carry no pending decision and are dropped by type; the
/// idle nudge is dropped by timing — it fires ~60s *after* a turn completes, so
/// the turn isn't running and `should_latch` is false. That's plain "waiting",
/// already covered by `Stop` + elapsed time, not "needs input".
fn handle_notification(input: &Value) {
    let kind = input.get("type").and_then(|v| v.as_str());
    let Some(session_id) = resolve_session_id(input) else {
        return;
    };
    let mut s = state::read_session(&session_id).unwrap_or_default();
    if !should_latch(kind, s.last_prompt_ts, s.last_turn_ts) {
        return;
    }
    s.last_notify_ts = Some(now_unix());
    let _ = state::write_session(&session_id, &s);
}

/// Whether a notification should latch `NeedsInput`: a blocking kind, raised
/// while a turn is in progress (`last_prompt` newer than `last_turn` — the same
/// condition as [`crate::fleet::Activity::Working`]). The mid-turn requirement
/// is what distinguishes "Claude is blocked asking you" from the idle nudge
/// that fires after a turn has already finished.
fn should_latch(kind: Option<&str>, last_prompt: Option<i64>, last_turn: Option<i64>) -> bool {
    is_blocking_notification(kind) && last_prompt > last_turn
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
        assert!(!is_blocking_notification(Some("auth_success")));
        assert!(!is_blocking_notification(Some("elicitation_complete")));
        assert!(!is_blocking_notification(Some("elicitation_response")));
    }

    #[test]
    fn latches_only_mid_turn() {
        // Mid-turn (prompt newer than the last completion): a blocking prompt
        // means Claude is blocked on you -> latch.
        assert!(should_latch(None, Some(200), Some(100)));
        assert!(should_latch(Some("permission_prompt"), Some(200), Some(100)));
        // First turn, no completion yet -> still mid-turn.
        assert!(should_latch(None, Some(200), None));
        // The idle nudge: fires after the turn completed (turn newer than
        // prompt) -> not blocked, don't latch. This is the false-positive the
        // mid-turn guard exists to kill, regardless of how it's typed.
        assert!(!should_latch(None, Some(100), Some(200)));
        assert!(!should_latch(Some("idle_prompt"), Some(100), Some(200)));
        // No turn data at all -> nothing to block on.
        assert!(!should_latch(None, None, None));
        // A non-blocking ack mid-turn still doesn't latch.
        assert!(!should_latch(Some("elicitation_complete"), Some(200), Some(100)));
    }
}
