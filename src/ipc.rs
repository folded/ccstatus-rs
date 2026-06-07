//! Registrar → handler notifications over the per-session Unix socket.
//!
//! Protocol is one line per message:
//!
//! ```text
//! register <pane_id>
//! ```
//!
//! The socket is already per session, so the session isn't in the message.
//! No reply — fire and forget. If no handler is listening the registrar
//! spawns one (`tmux -C attach -t <session>`, detached) and retries the
//! connect briefly.

use std::env;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::server_dir::ServerDir;

/// Tell the handler for `tmux_session` that `pane_id` is a Claude pane,
/// spawning the handler (one `tmux -C attach -t <session>` per session) if
/// none is listening. The per-session socket means the message only needs
/// the pane id.
pub fn notify_register(server_id: &str, tmux_session: &str, pane_id: &str) {
    let Ok(dir) = ServerDir::for_current(server_id) else {
        return;
    };
    let socket = dir.socket_path(tmux_session);
    let msg = format!("register {pane_id}\n");
    if try_send(&socket, &msg) {
        return;
    }
    spawn_handler_detached(tmux_session);
    // Brief retry while the freshly-spawned handler attaches and binds the
    // socket. ~1s total — first registrar call eats this; later ones
    // connect immediately.
    for _ in 0..20 {
        thread::sleep(Duration::from_millis(50));
        if try_send(&socket, &msg) {
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

fn spawn_handler_detached(tmux_session: &str) {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--session").arg(tmux_session);
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
