//! Append-only diagnostic log for the backend daemons, streamed live by
//! `ccstatus log`. One file per daemon under the server directory:
//!
//! ```text
//! /tmp/ccstatus-<uid>/server/<server-hash>/handler<sess>.log   tmux handler
//! /tmp/ccstatus-<uid>/server/ghostty/daemon.log                ghostty daemon
//! ```
//!
//! Each line is `<unix-ts> <message>`. Writes are best-effort: a failed open
//! or write is dropped rather than disrupting the daemon.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cache;

pub struct DaemonLog {
    path: PathBuf,
}

impl DaemonLog {
    fn at(server_id: &str, stem: &str) -> Self {
        let path = cache::cache_dir()
            .join("server")
            .join(server_id)
            .join(format!("{stem}.log"));
        Self { path }
    }

    /// The tmux per-session handler's log (`handler<sess>.log`).
    pub fn for_session(server_id: &str, session: &str) -> Self {
        Self::at(server_id, &format!("handler{}", sanitize_session(session)))
    }

    /// The singleton Ghostty daemon's log (`server/ghostty/daemon.log`).
    pub fn for_ghostty() -> Self {
        Self::at(crate::ghostty::SURFACE_SERVER_ID, "daemon")
    }

    pub fn write(&self, msg: &str) {
        let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{ts} {msg}");
    }
}

/// Session ids (`$1`) → a filename-safe suffix.
fn sanitize_session(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
