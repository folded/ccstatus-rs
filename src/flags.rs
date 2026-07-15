//! Per-pane window-label derivation shared by the surface daemons.
//!
//! The tmux handler stamps window names (`rename-window`) and the ghostty
//! handler stamps tab titles (OSC 2) with the same label: activity marker +
//! dirname + git glyph, rendered from the `windowFlag` template. This module
//! holds the one computation both feed with their own signals.

use crate::config::WindowFlag;
use crate::fleet;
use crate::state::SessionState;

/// Pure-ish (spawns `git` for the working-tree glyph): derive a pane's window
/// label from its session state plus the two process-level signals the
/// timestamps can't carry (`suspended`, `bg_running` — from a `ps` snapshot).
pub fn window_name(
    flag: &WindowFlag,
    sess: &SessionState,
    suspended: bool,
    bg_running: bool,
    now: i64,
) -> String {
    let idle_secs = sess.last_turn_ts.map(|t| (now - t).max(0));
    let act = fleet::activity(
        sess.last_prompt_ts,
        sess.last_turn_ts,
        suspended,
        sess.last_notify_ts.is_some(),
        bg_running,
        idle_secs,
    );
    let attn = fleet::attention(act, sess.last_turn_ts, sess.last_view_ts);
    let git = sess.cwd.as_deref().and_then(crate::git::status);
    flag.render(act, attn, git.as_ref(), sess.cwd.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_name_renders_activity_and_dir() {
        let flag = WindowFlag::default();
        let now = 10_000;
        // Prompt newer than turn -> Working -> the ◐ marker.
        let sess = SessionState {
            last_prompt_ts: Some(now - 5),
            last_turn_ts: Some(now - 100),
            cwd: Some("/tmp/nonexistent-ccstatus-test".into()),
            ..Default::default()
        };
        assert_eq!(
            window_name(&flag, &sess, false, false, now),
            "◐ nonexistent-ccstatus-test"
        );
    }

    #[test]
    fn window_name_flags_unviewed_completion() {
        let flag = WindowFlag::default();
        let now = 10_000;
        // Settled, never viewed -> the ⚑ done flag replaces the marker.
        let sess = SessionState {
            last_turn_ts: Some(now - 10),
            cwd: Some("/tmp/nonexistent-ccstatus-test".into()),
            ..Default::default()
        };
        assert_eq!(
            window_name(&flag, &sess, false, false, now),
            "⚑ nonexistent-ccstatus-test"
        );
    }

    #[test]
    fn window_name_suspension_trumps_turn_state() {
        let flag = WindowFlag::default();
        let now = 10_000;
        let sess = SessionState {
            last_prompt_ts: Some(now - 5),
            last_turn_ts: Some(now - 100),
            cwd: Some("/tmp/nonexistent-ccstatus-test".into()),
            ..Default::default()
        };
        assert_eq!(
            window_name(&flag, &sess, true, false, now),
            "⏸ nonexistent-ccstatus-test"
        );
    }
}
