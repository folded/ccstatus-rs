//! Aggregate read model over the on-disk state directory.
//!
//! The registrar and hooks persist per-pane and per-session state under
//! `/tmp/ccstatus-<uid>/` (see [`crate::state`]). This module enumerates all
//! of it and folds it into one cross-session view — the substrate for
//! `ccstatus top` and any future aggregate surface (menubar, notifications).
//!
//! Disk state is *last-known render — may be stale, display-only*. Liveness
//! and addressing are not read from disk: a session is "live" iff its Claude
//! process is alive, and "jumpable" iff a handler is listening on its server
//! (see [`crate::server_dir`]). Both are probed, not trusted from a file.
//!
//! The fold ([`build_views`]) is pure and takes already-read data; the IO
//! shell ([`collect`]) does the directory walk and the liveness probes.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::net::UnixStream;

use crate::cache;
use crate::render_tmux::WARM_THRESHOLD_SECS;
use crate::state::{self, PaneState, SessionState};
use crate::util::{now_unix, pid_alive};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warmth {
    Warm,
    Cold,
    /// No recorded turn yet — warmth is unknown.
    Unknown,
}

/// One Claude session as seen across the whole machine.
#[derive(Debug, Clone)]
pub struct SessionView {
    /// Claude conversation id (the session UUID), the jump key.
    pub claude_session: String,
    /// tmux server hash + pane id — the address the jump resolves through.
    pub server_id: String,
    pub pane_id: String,
    pub model: Option<String>,
    /// Plain-text working dir / git summary (ANSI stripped from the rendered
    /// `cwd` element).
    pub cwd: Option<String>,
    pub context_pct: Option<u32>,
    pub warmth: Warmth,
    /// Seconds since the last recorded turn, or `None` if no turn yet.
    pub idle_secs: Option<i64>,
    /// A handler is listening on this session's server, so a jump can be
    /// routed to it.
    pub jumpable: bool,
}

/// A parsed pane record: `(server_id, pane_id, state)`.
pub type PaneRecord = (String, String, PaneState);

/// Pure: fold parsed pane + session state into sorted session views.
/// `live_servers` is the set of server ids with a listening handler; `now` is
/// the current unix time (injected for testability).
pub fn build_views(
    panes: &[PaneRecord],
    sessions: &HashMap<String, SessionState>,
    live_servers: &HashSet<String>,
    now: i64,
) -> Vec<SessionView> {
    let mut views: Vec<SessionView> = panes
        .iter()
        .map(|(server_id, pane_id, pane)| {
            let sess = sessions.get(&pane.session_id);
            let idle_secs = sess
                .and_then(|s| s.last_turn_ts)
                .map(|t| (now - t).max(0));
            let warmth = match idle_secs {
                Some(i) if i < WARM_THRESHOLD_SECS => Warmth::Warm,
                Some(_) => Warmth::Cold,
                None => Warmth::Unknown,
            };
            SessionView {
                claude_session: pane.session_id.clone(),
                server_id: server_id.clone(),
                pane_id: pane_id.clone(),
                model: sess
                    .and_then(|s| s.model.clone())
                    .or_else(|| pane.elements.get("model").map(|s| strip_ansi(s))),
                cwd: pane.elements.get("cwd").map(|s| strip_ansi(s)),
                context_pct: sess.and_then(|s| s.context_pct_used),
                warmth,
                idle_secs,
                jumpable: live_servers.contains(server_id),
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

/// IO shell: walk the state dir, drop panes whose Claude process has exited,
/// probe handler liveness, and fold into [`SessionView`]s.
pub fn collect() -> Vec<SessionView> {
    let panes: Vec<PaneRecord> = read_live_panes();
    let sessions = read_sessions();
    let live_servers = live_servers();
    build_views(&panes, &sessions, &live_servers, now_unix())
}

/// Every live pane across every server: `pane/<server_id>/<pane>.json` whose
/// `claude_pid` is still alive.
fn read_live_panes() -> Vec<PaneRecord> {
    let mut out = Vec::new();
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
            if let Some(pane) = state::read_pane(&server_id, pane_id)
                && pid_alive(pane.claude_pid)
            {
                out.push((server_id.clone(), pane_id.to_string(), pane));
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

/// Strip ANSI SGR sequences (`\x1b[…m`) so rendered element content can be
/// shown as plain text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'm' {
                j += 1;
            }
            i = if j < bytes.len() { j + 1 } else { bytes.len() };
            continue;
        }
        let start = i;
        i += 1;
        while i < bytes.len() && bytes[i] != 0x1b {
            i += 1;
        }
        out.push_str(&s[start..i]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(session: &str, model_el: Option<&str>, cwd_el: Option<&str>) -> PaneState {
        let mut elements = HashMap::new();
        if let Some(m) = model_el {
            elements.insert("model".to_string(), m.to_string());
        }
        if let Some(c) = cwd_el {
            elements.insert("cwd".to_string(), c.to_string());
        }
        PaneState {
            session_id: session.to_string(),
            claude_pid: 1234,
            pane_tty: "/dev/ttys001".to_string(),
            transcript_path: None,
            registered_at: 0,
            last_warmth: None,
            elements,
        }
    }

    fn session(last_turn: Option<i64>, model: Option<&str>, ctx: Option<u32>) -> SessionState {
        SessionState {
            last_turn_ts: last_turn,
            model: model.map(str::to_string),
            turn_count: 0,
            context_pct_used: ctx,
            cache_read_pct: None,
        }
    }

    #[test]
    fn warmth_flips_at_threshold() {
        let now = 10_000;
        let panes = vec![
            ("srv".into(), "%1".into(), pane("warm-sess", None, None)),
            ("srv".into(), "%2".into(), pane("cold-sess", None, None)),
            ("srv".into(), "%3".into(), pane("new-sess", None, None)),
        ];
        let mut sessions = HashMap::new();
        sessions.insert("warm-sess".to_string(), session(Some(now - 10), None, None));
        sessions.insert(
            "cold-sess".to_string(),
            session(Some(now - WARM_THRESHOLD_SECS - 1), None, None),
        );
        // new-sess has no session file / no turn.
        let views = build_views(&panes, &sessions, &HashSet::new(), now);

        let by_sess = |s: &str| views.iter().find(|v| v.claude_session == s).unwrap().clone();
        assert_eq!(by_sess("warm-sess").warmth, Warmth::Warm);
        assert_eq!(by_sess("cold-sess").warmth, Warmth::Cold);
        assert_eq!(by_sess("new-sess").warmth, Warmth::Unknown);
    }

    #[test]
    fn sorts_most_recently_active_first_unknown_last() {
        let now = 10_000;
        let panes = vec![
            ("srv".into(), "%1".into(), pane("idle", None, None)),
            ("srv".into(), "%2".into(), pane("active", None, None)),
            ("srv".into(), "%3".into(), pane("never", None, None)),
        ];
        let mut sessions = HashMap::new();
        sessions.insert("idle".to_string(), session(Some(now - 200), None, None));
        sessions.insert("active".to_string(), session(Some(now - 5), None, None));
        let views = build_views(&panes, &sessions, &HashSet::new(), now);
        let order: Vec<&str> = views.iter().map(|v| v.claude_session.as_str()).collect();
        assert_eq!(order, vec!["active", "idle", "never"]);
    }

    #[test]
    fn model_falls_back_to_stripped_element() {
        let now = 10_000;
        let panes = vec![(
            "srv".into(),
            "%1".into(),
            pane("s", Some("\x1b[38;2;0;0;0mOpus\x1b[0m"), Some("\x1b[2mdemo\x1b[0m@main")),
        )];
        let sessions = HashMap::new(); // no session file -> model from element
        let views = build_views(&panes, &sessions, &HashSet::new(), now);
        assert_eq!(views[0].model.as_deref(), Some("Opus"));
        assert_eq!(views[0].cwd.as_deref(), Some("demo@main"));
    }

    #[test]
    fn session_model_wins_over_element() {
        let now = 10_000;
        let panes = vec![("srv".into(), "%1".into(), pane("s", Some("FromElement"), None))];
        let mut sessions = HashMap::new();
        sessions.insert("s".to_string(), session(Some(now), Some("FromSession"), Some(42)));
        let views = build_views(&panes, &sessions, &HashSet::new(), now);
        assert_eq!(views[0].model.as_deref(), Some("FromSession"));
        assert_eq!(views[0].context_pct, Some(42));
    }

    #[test]
    fn jumpable_tracks_live_servers() {
        let now = 10_000;
        let panes = vec![
            ("live".into(), "%1".into(), pane("a", None, None)),
            ("dead".into(), "%2".into(), pane("b", None, None)),
        ];
        let mut live = HashSet::new();
        live.insert("live".to_string());
        let views = build_views(&panes, &HashMap::new(), &live, now);
        let by_sess = |s: &str| views.iter().find(|v| v.claude_session == s).unwrap().clone();
        assert!(by_sess("a").jumpable);
        assert!(!by_sess("b").jumpable);
    }

    #[test]
    fn strip_ansi_removes_sgr_keeps_text() {
        assert_eq!(strip_ansi("\x1b[38;2;0;153;255mClaude\x1b[0m"), "Claude");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[2m·\x1b[0m mid \x1b[1mX\x1b[0m"), "· mid X");
    }
}
