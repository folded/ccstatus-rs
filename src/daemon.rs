//! Long-lived ccstatus process driving tmux via control mode.
//!
//! Milestone 4 scope: accept registrar pings over the per-server Unix
//! socket and log them. Subsequent milestones add focus tracking and
//! the real row injection.

use std::io::{BufRead, BufReader};
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::control;
use crate::server_dir::ServerDir;
use crate::snapshot::{self, Snapshot};
use crate::tmux;

/// Demo lifetime for milestones 2–4: daemon runs for this long after the
/// last message, then restores and exits. Replaced by event-driven
/// shutdown criteria in milestone 9.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(20);

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

    let socket = match dir.bind_socket() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Push registrar messages onto a channel so the main loop can
    // multiplex them with timeouts (and, in later milestones, tmux
    // events).
    let (msg_tx, msg_rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for stream in socket.incoming() {
            let Ok(s) = stream else { continue };
            let reader = BufReader::new(s);
            for line in reader.lines().map_while(Result::ok) {
                if msg_tx.send(line).is_err() {
                    return;
                }
            }
        }
    });

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

    eprintln!("ccstatus daemon: ready, awaiting registrar pings");
    let mut last_message = Instant::now();
    loop {
        let elapsed = last_message.elapsed();
        let remaining = IDLE_EXIT_AFTER.saturating_sub(elapsed);
        if remaining.is_zero() {
            break;
        }
        match msg_rx.recv_timeout(remaining) {
            Ok(line) => {
                eprintln!("ccstatus daemon: registrar msg: {line}");
                last_message = Instant::now();
                // Milestone 5 will parse the message and act on it.
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if let Err(e) = snap.restore(&mut conn) {
        eprintln!("ccstatus daemon: restore failed: {e}");
        return ExitCode::FAILURE;
    }
    eprintln!("ccstatus daemon: restored, exiting");
    ExitCode::SUCCESS
}
