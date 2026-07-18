//! The per-surface activity-flag label, shared by the surfaces that carry it:
//! the tmux window name (`daemon`) and the Ghostty tab title (`ghostty`).
//!
//! Both backends do the same thing each tick — turn a session's timestamps plus
//! a `ps`-derived view of its process into a [`WindowFlag`]-rendered label —
//! and differ only in how they *apply* it (`rename-window` vs OSC 2). This
//! module owns the computation so the two stay identical; the backends own the
//! enumeration and the write.

use std::collections::HashSet;

use crate::config::WindowFlag;
use crate::state::SessionState;
use crate::util;

/// Process-state signals derived once per tick from a single `ps` snapshot and
/// shared across every surface: which Claude pids have a live background task,
/// and which are suspended. These are the states the turn timestamps alone
/// can't reveal (see [`crate::fleet::activity`]).
pub struct PsSignals {
    bg: HashSet<u32>,
    susp: HashSet<u32>,
}

impl PsSignals {
    /// Take one `ps` snapshot and classify `pids`. Empty (no snapshot taken)
    /// when `pids` is empty, so a tick with no surfaces costs no `ps`.
    pub fn capture(pids: &HashSet<u32>) -> Self {
        let procs = if pids.is_empty() {
            Vec::new()
        } else {
            util::ps_snapshot()
        };
        Self {
            bg: util::background_task_pids(&procs, pids),
            susp: util::suspended_pids(&procs, pids),
        }
    }
}

/// Render the activity-flag label for one surface's session: derive its
/// [`crate::fleet::Activity`] and attention state from the session timestamps
/// and the tick's [`PsSignals`], then apply the [`WindowFlag`] template
/// (`{claude}`/`{dir}`/`{git}`/`{branch}`). `now` is the tick's wall clock
/// (passed in so all surfaces share one reading).
pub fn label(
    flag: &WindowFlag,
    sess: &SessionState,
    claude_pid: u32,
    signals: &PsSignals,
    now: i64,
) -> String {
    let idle_secs = sess.last_turn_ts.map(|t| (now - t).max(0));
    let act = crate::fleet::activity(
        sess.last_prompt_ts,
        sess.last_turn_ts,
        signals.susp.contains(&claude_pid),
        sess.last_notify_ts.is_some(),
        signals.bg.contains(&claude_pid),
        idle_secs,
    );
    let attn = crate::fleet::attention(act, sess.last_turn_ts, sess.last_view_ts);
    let git = sess.cwd.as_deref().and_then(crate::git::status);
    flag.render(act, attn, git.as_ref(), sess.cwd.as_deref())
}
