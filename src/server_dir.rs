//! Per-tmux-server directory and lock/socket management.
//!
//! Layout (one per tmux server):
//!
//! ```text
//! /tmp/ccstatus-<uid>/server/<server-hash>/
//!   daemon.lock      flock-held by the live daemon; contains its pid
//!   daemon.sock      Unix socket the registrar pings
//!   snapshot.json    captured user state (see snapshot.rs)
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use crate::cache;

pub struct ServerDir {
    pub root: PathBuf,
}

/// Held by the daemon for its lifetime. Drop releases the flock; the
/// underlying file persists (its mtime is a useful "when was a daemon
/// last running" marker for diagnostics).
pub struct DaemonLock {
    _file: File,
}

impl ServerDir {
    pub fn for_current(server_id: &str) -> Result<Self, String> {
        let root = cache::cache_dir()
            .join("server")
            .join(sanitize(server_id));
        fs::create_dir_all(&root)
            .map_err(|e| format!("creating {}: {e}", root.display()))?;
        Ok(Self { root })
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.root.join("daemon.sock")
    }

    /// Try to acquire the per-server daemon lock. Returns
    /// `Ok(Some(lock))` if we got it, `Ok(None)` if another live daemon
    /// holds it. Stale lock files (whose owner died) are taken over
    /// automatically because `flock` releases on FD close, including
    /// process exit.
    pub fn try_lock(&self) -> Result<Option<DaemonLock>, String> {
        let path = self.lock_path();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("opening {}: {e}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error();
            // EWOULDBLOCK / EAGAIN: someone else holds it.
            if errno.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(format!("flock {}: {errno}", path.display()));
        }
        // Record our pid for diagnostics.
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Some(DaemonLock { _file: file }))
    }

    /// Bind the per-server Unix socket. Removes any stale socket file
    /// first — only safe because we hold the lock by the time this is
    /// called, so no other live daemon could be listening.
    pub fn bind_socket(&self) -> Result<UnixListener, String> {
        let path = self.socket_path();
        let _ = fs::remove_file(&path);
        UnixListener::bind(&path)
            .map_err(|e| format!("binding {}: {e}", path.display()))
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c == '/' || c.is_control() { '_' } else { c })
        .collect()
}
