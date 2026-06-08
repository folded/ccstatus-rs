//! Aggregate read model over the on-disk state directory.
//!
//! The registrar and hooks persist per-session state under
//! `/tmp/ccstatus-<uid>/` (see [`crate::state`]). This module enumerates all
//! of it and folds it into one cross-session view — the substrate for
//! `ccstatus top` and any future aggregate surface (menubar, notifications).
//!
//! **Session-driven.** The row identity is the Claude session (its presence
//! record); a *pane* file, when present, supplies the tmux jump address. A
//! session with no pane file is a non-tmux Claude: shown, but not jumpable.
//!
//! Disk state is *last-known render — may be stale, display-only*. Liveness
//! and addressing are not trusted from a file: a session is "live" iff its
//! recorded `claude_pid` is alive, and "jumpable" iff a handler is listening
//! on its server (see [`crate::server_dir`]). Both are probed.
//!
//! The fold ([`build_views`]) is pure and takes already-read data; the IO
//! shell ([`collect`]) does the directory walk and the liveness probes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::net::UnixStream;

use crate::cache;
use crate::state::{self, SessionState};
use crate::util::{is_interactive_claude, now_unix, pid_alive};
use crate::window::{self, WindowTarget};

/// How long after a turn completes a session is still "waiting for you"
/// before it's considered idle (you've moved on).
const ACTIVITY_IDLE_AFTER: i64 = 600;

/// What a session is doing — the "which one needs me?" axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// A turn is in progress (last prompt is newer than the last completion).
    Working,
    /// Finished a turn recently and waiting for input.
    Waiting,
    /// Finished a while ago — you've likely moved on.
    Idle,
    /// Nothing recorded yet.
    Unknown,
}

/// Pure: derive activity from the prompt/turn timestamps. `idle_secs` is
/// seconds since the last completed turn.
fn activity(last_prompt: Option<i64>, last_turn: Option<i64>, idle_secs: Option<i64>) -> Activity {
    match (last_prompt, last_turn) {
        // A prompt newer than the last completion (or with no completion yet)
        // means a turn is running.
        (Some(p), Some(t)) if p > t => Activity::Working,
        (Some(_), None) => Activity::Working,
        // Otherwise the last turn has completed: waiting, or idle once stale.
        (_, Some(_)) => match idle_secs {
            Some(i) if i > ACTIVITY_IDLE_AFTER => Activity::Idle,
            _ => Activity::Waiting,
        },
        (None, None) => Activity::Unknown,
    }
}

/// A tmux jump address: which server, which pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneAddr {
    pub server_id: String,
    pub pane_id: String,
}

/// One Claude session as seen across the whole machine.
#[derive(Debug, Clone)]
pub struct SessionView {
    /// Claude conversation id (the session UUID), the jump key.
    pub claude_session: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub context_pct: Option<u32>,
    pub activity: Activity,
    /// Seconds since the last recorded turn, or `None` if no turn yet.
    pub idle_secs: Option<i64>,
    /// The tmux pane backing this session, or `None` for a non-tmux Claude.
    pub address: Option<PaneAddr>,
    /// The OS terminal window backing a *non-tmux* Claude (iTerm2/Terminal on
    /// macOS), or `None` when there's no addressable window. Mutually exclusive
    /// with `address` in practice — a session is in tmux or it isn't.
    pub window: Option<WindowTarget>,
    /// A jump can land: a pane on a live-handler server, or an addressable
    /// non-tmux window.
    pub jumpable: bool,
}

/// Pure: fold session presence + pane addressing into sorted views.
/// `pane_index` maps a session id to its tmux address; `live_servers` is the
/// set of server ids with a listening handler; `now` is injected for tests.
pub fn build_views(
    sessions: &HashMap<String, SessionState>,
    pane_index: &HashMap<String, PaneAddr>,
    live_servers: &HashSet<String>,
    now: i64,
) -> Vec<SessionView> {
    let mut views: Vec<SessionView> = sessions
        .iter()
        .map(|(id, s)| {
            let idle_secs = s.last_turn_ts.map(|t| (now - t).max(0));
            let address = pane_index.get(id).cloned();
            // A non-tmux session has no pane; fall back to an OS window target.
            let window = address.is_none().then(|| {
                window::target_for(
                    s.term_program.as_deref(),
                    s.iterm_session_id.as_deref(),
                    s.claude_pid,
                )
            }).flatten();
            let pane_jumpable = address
                .as_ref()
                .map(|a| live_servers.contains(&a.server_id))
                .unwrap_or(false);
            let jumpable = pane_jumpable || window.is_some();
            SessionView {
                claude_session: id.clone(),
                model: s.model.clone(),
                cwd: s.cwd.clone(),
                context_pct: s.context_pct_used,
                activity: activity(s.last_prompt_ts, s.last_turn_ts, idle_secs),
                idle_secs,
                address,
                window,
                jumpable,
            }
        })
        .collect();
    // Most recently active first; sessions with no turn yet sink to the bottom.
    views.sort_by(|a, b| {
        a.idle_secs
            .unwrap_or(i64::MAX)
            .cmp(&b.idle_secs.unwrap_or(i64::MAX))
            .then_with(|| a.claude_session.cmp(&b.claude_session))
    });
    views
}

/// IO shell: read every session presence record, drop those whose Claude
/// process has exited (or whose recorded pid is the shared daemon / a `--bg-*`
/// helper rather than a live interactive session), collapse prior conversations
/// of the same process, attach pane addressing, probe handler liveness, and
/// fold into [`SessionView`]s.
pub fn collect() -> Vec<SessionView> {
    let sessions: HashMap<String, SessionState> = read_sessions()
        .into_iter()
        // `pid_alive` is a cheap syscall; `is_interactive_claude` spawns `ps`,
        // so gate it behind the liveness check (and short-circuit on dead pids).
        .filter(|(_, s)| {
            s.claude_pid
                .is_some_and(|p| pid_alive(p) && is_interactive_claude(p))
        })
        .collect();
    let sessions = dedup_to_current(sessions);
    let pane_index = read_pane_index();
    let live_servers = live_servers();
    build_views(&sessions, &pane_index, &live_servers, now_unix())
}

/// Pure: collapse sessions that share a `claude_pid` down to the most
/// recently active one. A Claude process hosts conversations *sequentially*
/// (start one, `/clear` or resume into another), and every conversation writes
/// its own session file stamped with that process's pid — so several live
/// session files with the same pid are prior conversations of one process, and
/// only the latest is current. Recency is the freshest of the prompt/turn
/// timestamps; ties break on session id for determinism. Sessions with no pid
/// pass through untouched.
fn dedup_to_current(
    sessions: HashMap<String, SessionState>,
) -> HashMap<String, SessionState> {
    fn recency(s: &SessionState) -> Option<i64> {
        s.last_prompt_ts.max(s.last_turn_ts)
    }
    let mut best: HashMap<u32, (String, SessionState)> = HashMap::new();
    let mut out: HashMap<String, SessionState> = HashMap::new();
    for (id, s) in sessions {
        let Some(pid) = s.claude_pid else {
            out.insert(id, s);
            continue;
        };
        let wins = match best.get(&pid) {
            None => true,
            Some((cur_id, cur)) => {
                let (rs, rc) = (recency(&s), recency(cur));
                rs > rc || (rs == rc && id < *cur_id)
            }
        };
        if wins {
            best.insert(pid, (id, s));
        }
    }
    for (_, (id, s)) in best {
        out.insert(id, s);
    }
    out
}

/// Map each session id to its tmux pane address, from `pane/<server>/<pane>`
/// files. (A session may have at most one pane; last write wins on the rare
/// duplicate.)
fn read_pane_index() -> HashMap<String, PaneAddr> {
    let mut out = HashMap::new();
    let pane_root = cache::cache_dir().join("pane");
    let Ok(servers) = fs::read_dir(&pane_root) else {
        return out;
    };
    for server in servers.flatten() {
        let server_id = server.file_name().to_string_lossy().to_string();
        let Ok(files) = fs::read_dir(server.path()) else {
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            let Some(pane_id) = name.strip_suffix(".json") else {
                continue;
            };
            if let Some(pane) = state::read_pane(&server_id, pane_id) {
                out.insert(
                    pane.session_id,
                    PaneAddr {
                        server_id: server_id.clone(),
                        pane_id: pane_id.to_string(),
                    },
                );
            }
        }
    }
    out
}

/// Every session file, keyed by session id.
fn read_sessions() -> HashMap<String, SessionState> {
    let mut out = HashMap::new();
    let session_root = cache::cache_dir().join("session");
    let Ok(files) = fs::read_dir(&session_root) else {
        return out;
    };
    for f in files.flatten() {
        let name = f.file_name().to_string_lossy().to_string();
        if let Some(id) = name.strip_suffix(".json")
            && let Some(s) = state::read_session(id)
        {
            out.insert(id.to_string(), s);
        }
    }
    out
}

/// Server ids with at least one handler socket we can connect to — a live
/// handler is listening, so a jump can be routed there.
fn live_servers() -> HashSet<String> {
    let mut out = HashSet::new();
    let server_root = cache::cache_dir().join("server");
    let Ok(servers) = fs::read_dir(&server_root) else {
        return out;
    };
    for server in servers.flatten() {
        let server_id = server.file_name().to_string_lossy().to_string();
        let Ok(files) = fs::read_dir(server.path()) else {
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            if name.ends_with(".sock") && UnixStream::connect(f.path()).is_ok() {
                out.insert(server_id.clone());
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(last_turn: Option<i64>, model: Option<&str>, ctx: Option<u32>) -> SessionState {
        SessionState {
            last_turn_ts: last_turn,
            last_prompt_ts: None,
            model: model.map(str::to_string),
            turn_count: 0,
            context_pct_used: ctx,
            cache_read_pct: None,
            cwd: None,
            claude_pid: Some(1234),
            term_program: None,
            iterm_session_id: None,
        }
    }

    fn addr(server: &str, pane: &str) -> PaneAddr {
        PaneAddr { server_id: server.into(), pane_id: pane.into() }
    }

    /// Build a session with an explicit pid and recency timestamps.
    fn sess_pid(pid: Option<u32>, prompt: Option<i64>, turn: Option<i64>) -> SessionState {
        SessionState {
            last_turn_ts: turn,
            last_prompt_ts: prompt,
            model: None,
            turn_count: 0,
            context_pct_used: None,
            cache_read_pct: None,
            cwd: None,
            claude_pid: pid,
            term_program: None,
            iterm_session_id: None,
        }
    }

    #[test]
    fn dedup_keeps_most_recently_active_per_pid() {
        let mut s = HashMap::new();
        // Same pid 100: current conversation (recent prompt) vs an old one.
        s.insert("current".into(), sess_pid(Some(100), Some(9_999), Some(9_000)));
        s.insert("old".into(), sess_pid(Some(100), None, Some(1_000)));
        // A distinct pid survives independently.
        s.insert("other".into(), sess_pid(Some(200), None, Some(5_000)));
        let out = dedup_to_current(s);
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("current"));
        assert!(out.contains_key("other"));
        assert!(!out.contains_key("old"));
    }

    #[test]
    fn dedup_breaks_recency_ties_on_id() {
        let mut s = HashMap::new();
        // Same pid, neither has any timestamp -> tie -> smaller id wins.
        s.insert("bbb".into(), sess_pid(Some(7), None, None));
        s.insert("aaa".into(), sess_pid(Some(7), None, None));
        let out = dedup_to_current(s);
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("aaa"));
    }

    #[test]
    fn sorts_most_recently_active_first_unknown_last() {
        let now = 10_000;
        let mut sessions = HashMap::new();
        sessions.insert("idle".to_string(), session(Some(now - 200), None, None));
        sessions.insert("active".to_string(), session(Some(now - 5), None, None));
        sessions.insert("never".to_string(), session(None, None, None));
        let views = build_views(&sessions, &HashMap::new(), &HashSet::new(), now);
        let order: Vec<&str> = views.iter().map(|v| v.claude_session.as_str()).collect();
        assert_eq!(order, vec!["active", "idle", "never"]);
    }

    #[test]
    fn presence_fields_pass_through() {
        let now = 10_000;
        let mut sessions = HashMap::new();
        let mut s = session(Some(now), Some("Opus"), Some(42));
        s.cwd = Some("~/demo".into());
        sessions.insert("s".to_string(), s);
        let views = build_views(&sessions, &HashMap::new(), &HashSet::new(), now);
        assert_eq!(views[0].model.as_deref(), Some("Opus"));
        assert_eq!(views[0].cwd.as_deref(), Some("~/demo"));
        assert_eq!(views[0].context_pct, Some(42));
    }

    #[test]
    fn activity_derivation() {
        let now = 10_000;
        // Prompt newer than last turn -> working.
        assert_eq!(activity(Some(now), Some(now - 50), Some(50)), Activity::Working);
        // Prompt, never completed -> working.
        assert_eq!(activity(Some(now), None, None), Activity::Working);
        // Completed after the prompt, recently -> waiting.
        assert_eq!(activity(Some(now - 100), Some(now - 10), Some(10)), Activity::Waiting);
        // Completed long ago -> idle.
        assert_eq!(
            activity(Some(now - 5000), Some(now - 4000), Some(4000)),
            Activity::Idle
        );
        // Turn but no recorded prompt, recent -> waiting.
        assert_eq!(activity(None, Some(now - 5), Some(5)), Activity::Waiting);
        // Nothing -> unknown.
        assert_eq!(activity(None, None, None), Activity::Unknown);
    }

    #[test]
    fn build_views_sets_activity_working_when_prompt_is_newer() {
        let now = 10_000;
        let mut sessions = HashMap::new();
        let mut s = session(Some(now - 100), None, None); // completed at now-100
        s.last_prompt_ts = Some(now - 5); // new prompt since -> working
        sessions.insert("s".to_string(), s);
        let views = build_views(&sessions, &HashMap::new(), &HashSet::new(), now);
        assert_eq!(views[0].activity, Activity::Working);
    }

    #[test]
    fn non_tmux_session_has_no_address_and_is_not_jumpable() {
        let now = 10_000;
        let mut sessions = HashMap::new();
        sessions.insert("s".to_string(), session(Some(now), None, None));
        // No pane file, no addressable terminal -> not jumpable.
        let views = build_views(&sessions, &HashMap::new(), &HashSet::new(), now);
        assert!(views[0].address.is_none());
        assert!(views[0].window.is_none());
        assert!(!views[0].jumpable);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn non_tmux_iterm_session_is_window_jumpable() {
        let now = 10_000;
        let mut s = session(Some(now), None, None);
        s.term_program = Some("iTerm.app".into());
        s.iterm_session_id = Some("w0t0p0:CD4CA6CF-3A9C-464A-B736-B13BFEC9452C".into());
        let mut sessions = HashMap::new();
        sessions.insert("s".to_string(), s);
        // No pane file -> not in tmux, but the iTerm2 window is addressable.
        let views = build_views(&sessions, &HashMap::new(), &HashSet::new(), now);
        assert!(views[0].address.is_none());
        assert!(matches!(
            views[0].window,
            Some(WindowTarget::ITerm2 { .. })
        ));
        assert!(views[0].jumpable);
    }

    #[test]
    fn jumpable_requires_a_pane_on_a_live_server() {
        let now = 10_000;
        let mut sessions = HashMap::new();
        sessions.insert("live".to_string(), session(Some(now), None, None));
        sessions.insert("deadsrv".to_string(), session(Some(now), None, None));
        sessions.insert("notmux".to_string(), session(Some(now), None, None));
        let mut panes = HashMap::new();
        panes.insert("live".to_string(), addr("L", "%1"));
        panes.insert("deadsrv".to_string(), addr("D", "%2"));
        let mut live = HashSet::new();
        live.insert("L".to_string());

        let views = build_views(&sessions, &panes, &live, now);
        let by = |s: &str| views.iter().find(|v| v.claude_session == s).unwrap().clone();
        assert!(by("live").jumpable); // pane on a live server
        assert_eq!(by("live").address, Some(addr("L", "%1")));
        assert!(!by("deadsrv").jumpable); // pane, but no live handler
        assert!(by("deadsrv").address.is_some());
        assert!(!by("notmux").jumpable); // no pane at all
        assert!(by("notmux").address.is_none());
    }
}
