//! Long-lived ccstatus process driving tmux via control mode.
//!
//! Milestone 2 scope: snapshot the user's status-bar options on start,
//! demonstrate a visible mutation, then restore on exit. Subsequent
//! milestones add lockfile/socket, registrar integration, focus tracking,
//! and reload detection.

use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use crate::control;
use crate::server_dir::ServerDir;
use crate::snapshot::{self, Snapshot};
use crate::tmux;

pub fn run() -> ExitCode {
    let server_id = tmux::server_id().unwrap_or_else(|| "unknown".to_string());

    let dir = match ServerDir::for_current(&server_id) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

    let _lock = match dir.try_lock() {
        Ok(Some(l)) => l,
        Ok(None) => {
            // Another daemon already owns this server. Exit silently so
            // registrar pings (the thing that spawns us) treat this as
            // "already running" rather than an error.
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

    let _socket = match dir.bind_socket() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut conn = match control::Connection::attach() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

    let snap = match Snapshot::capture(&mut conn) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ccstatus daemon: snapshot failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = snapshot::save(&server_id, &snap) {
        eprintln!("ccstatus daemon: persisting snapshot: {e}");
        // Non-fatal: the daemon can still restore in-process on graceful
        // exit even if persistence fails. The disk copy is only there
        // for crash recovery.
    }
    eprintln!(
        "ccstatus daemon: lock + socket acquired (server={server_id}), snapshot captured (status={}, position={})",
        snap.status, snap.status_position
    );

    // Visible mutation so the operator can verify the daemon is driving
    // tmux. Replaced with the real focus-driven row injection in a
    // later milestone.
    if let Err(e) = mutate_for_demo(&mut conn) {
        eprintln!("ccstatus daemon: mutate failed: {e}");
        let _ = snap.restore(&mut conn);
        return ExitCode::FAILURE;
    }

    thread::sleep(Duration::from_secs(5));

    if let Err(e) = snap.restore(&mut conn) {
        eprintln!("ccstatus daemon: restore failed: {e}");
        return ExitCode::FAILURE;
    }
    eprintln!("ccstatus daemon: restored, exiting");
    ExitCode::SUCCESS
}

fn mutate_for_demo(conn: &mut control::Connection) -> Result<(), String> {
    let r = conn.cmd("set-option -g status 2")?;
    if !r.ok {
        return Err(r.output);
    }
    let r = conn.cmd(
        "set-option -g 'status-format[1]' '#[fg=red,bold]ccstatus daemon demo row#[default]'",
    )?;
    if !r.ok {
        return Err(r.output);
    }
    let r = conn.cmd("refresh-client -S")?;
    if !r.ok {
        return Err(r.output);
    }
    Ok(())
}
