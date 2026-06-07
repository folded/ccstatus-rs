//! Long-lived ccstatus process driving tmux via control mode.
//!
//! Milestone 1 scope: prove out the control connection with a single
//! request/response round-trip and exit. Subsequent milestones add the
//! snapshot/restore, focus tracking, and notification multiplexing.

use std::process::ExitCode;

use crate::control;

pub fn run() -> ExitCode {
    let mut conn = match control::Connection::attach() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Sanity round-trip: a literal display-message that just echoes a
    // string. Proves the framing logic end-to-end and that we can send a
    // command and parse its response. Later milestones add the snapshot,
    // event loop, and registrar IPC.
    match conn.cmd("display-message -p 'ccstatus-control-mode-ok'") {
        Ok(r) if r.ok => {
            println!("ccstatus daemon: {}", r.output);
            ExitCode::SUCCESS
        }
        Ok(r) => {
            eprintln!("ccstatus daemon: tmux refused command: {}", r.output);
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("ccstatus daemon: {e}");
            ExitCode::FAILURE
        }
    }
}
