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
use crate::render_tmux::WARM_THRESHOLD_SECS;
use crate::state::{self, SessionState};
use crate::util::{now_unix, pid_alive};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warmth {
    Warm,
    Cold,
    /// No recorded turn yet — warmth is unknown.
    Unknown,
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
    pub warmth: Warmth,
    /// Seconds since the last recorded turn, or `None` if no turn yet.
    pub idle_secs: Option<i64>,
    /// The tmux pane backing this session, or `None` for a non-tmux Claude.
    pub address: Option<PaneAddr>,
    /// Addressable *and* a handler is live on its server, so a jump can land.
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
            let warmth = match idle_secs {
                Some(i) if i < WARM_THRESHOLD_SECS => Warmth::Warm,
                Some(_) => Warmth::Cold,
                None => Warmth::Unknown,
            };
            let address = pane_index.get(id).cloned();
            let jumpable = address
                .as_ref()
                .map(|a| live_servers.contains(&a.server_id))
                .unwrap_or(false);
            SessionView {
                claude_session: id.clone(),
                model: s.model.clone(),
                cwd: s.cwd.clone(),
                context_pct: s.context_pct_used,
                warmth,
                idle_secs,
                address,
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
/// process has exited, attach pane addressing, probe handler liveness, and
/// fold into [`SessionView`]s.
pub fn collect() -> Vec<SessionView> {
    let sessions: HashMap<String, SessionState> = read_sessions()
        .into_iter()
        .filter(|(_, s)| s.claude_pid.map(pid_alive).unwrap_or(false))
        .collect();
    let pane_index = read_pane_index();
    let live_servers = live_servers();
    build_views(&sessions, &pane_index, &live_servers, now_unix())
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
            model: model.map(str::to_string),
            turn_count: 0,
            context_pct_used: ctx,
            cache_read_pct: None,
            cwd: None,
            claude_pid: Some(1234),
        }
    }

    fn addr(server: &str, pane: &str) -> PaneAddr {
        PaneAddr { server_id: server.into(), pane_id: pane.into() }
    }

    #[test]
    fn warmth_flips_at_threshold() {
        let now = 10_000;
        let mut sessions = HashMap::new();
        sessions.insert("warm".to_string(), session(Some(now - 10), None, None));
        sessions.insert("cold".to_string(), session(Some(now - WARM_THRESHOLD_SECS - 1), None, None));
        sessions.insert("new".to_string(), session(None, None, None));
        let views = build_views(&sessions, &HashMap::new(), &HashSet::new(), now);

        let by = |s: &str| views.iter().find(|v| v.claude_session == s).unwrap().clone();
        assert_eq!(by("warm").warmth, Warmth::Warm);
        assert_eq!(by("cold").warmth, Warmth::Cold);
        assert_eq!(by("new").warmth, Warmth::Unknown);
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
    fn non_tmux_session_has_no_address_and_is_not_jumpable() {
        let now = 10_000;
        let mut sessions = HashMap::new();
        sessions.insert("s".to_string(), session(Some(now), None, None));
        // No pane file for it -> not in tmux.
        let views = build_views(&sessions, &HashMap::new(), &HashSet::new(), now);
        assert!(views[0].address.is_none());
        assert!(!views[0].jumpable);
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
