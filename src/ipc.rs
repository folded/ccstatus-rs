//! Registrar → daemon notifications over the per-server Unix socket.
//!
//! Protocol is one line per message:
//!
//! ```text
//! register <pane_id> <session_id>
//! ```
//!
//! Lines are simple whitespace-separated tokens. No reply — fire and
//! forget. If no daemon is listening the registrar spawns one
//! (detached) and retries the connect briefly.

use std::env;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::server_dir::ServerDir;

pub fn notify_register(server_id: &str, pane_id: &str, session_id: &str) {
    let Ok(dir) = ServerDir::for_current(server_id) else {
        return;
    };
    let msg = format!("register {pane_id} {session_id}\n");
    if try_send(&dir.socket_path(), &msg) {
        return;
    }
    spawn_daemon_detached();
    // Brief retry loop while the freshly-spawned daemon attaches and
    // binds the socket. ~1s total — first registrar call after a tmux
    // start eats this; subsequent calls connect immediately.
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(50));
        if try_send(&dir.socket_path(), &msg) {
            return;
        }
    }
}

fn try_send(socket_path: &Path, msg: &str) -> bool {
    match UnixStream::connect(socket_path) {
        Ok(mut s) => {
            let _ = s.write_all(msg.as_bytes());
            let _ = s.shutdown(std::net::Shutdown::Write);
            true
        }
        Err(_) => false,
    }
}

fn spawn_daemon_detached() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    // setsid puts the child in a new session and process group, so it
    // survives the registrar exiting and SIGHUP propagating from the
    // controlling terminal closing.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                // Non-fatal: the daemon still has a chance to run, just
                // potentially in the same session as the registrar.
            }
            Ok(())
        });
    }
    let _ = cmd.spawn();
}
