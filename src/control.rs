//! tmux control mode client (`tmux -C attach`).
//!
//! The control protocol is line-oriented text. Outgoing: tmux commands on
//! stdin. Incoming on stdout:
//!
//! - `%begin <ts> <cmd-num> <flags>` — start of a response to command N.
//! - `<line>` — body line of the current response.
//! - `%end <ts> <cmd-num> <flags>` — successful end of response N.
//! - `%error <ts> <cmd-num> <flags>` — failed end of response N.
//! - `%<event> <args>` — asynchronous notification (no command frame).
//!
//! This module exposes a [`Connection`] that takes care of the framing.
//! Higher-level state-machine logic lives in `daemon.rs`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

#[derive(Debug)]
#[allow(dead_code)]
pub enum Event {
    Begin {
        cmd: u64,
    },
    End {
        cmd: u64,
        ok: bool,
    },
    /// A body line emitted between `%begin` and `%end`/`%error`.
    Output(String),
    /// Asynchronous notification like `%window-pane-changed $1 %5`.
    Notification {
        name: String,
        args: String,
    },
    /// EOF on tmux's stdout — server exited or detached us.
    Exit,
}

pub struct Connection {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
}

#[derive(Debug)]
pub struct Response {
    pub ok: bool,
    pub output: String,
}

impl Connection {
    /// Spawn `tmux -C attach` against the current server (taken from
    /// `$TMUX` in the spawned process's environment). Drains the initial
    /// `%begin/%end` frame tmux sends on connect so callers see a clean
    /// channel ready for the first command.
    pub fn attach() -> Result<Self, String> {
        let mut child = Command::new("tmux")
            .args(["-C", "attach"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawning `tmux -C attach`: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "no stdin on tmux child".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "no stdout on tmux child".to_string())?;
        let mut conn = Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
        };
        conn.drain_initial_frame()?;
        Ok(conn)
    }

    fn drain_initial_frame(&mut self) -> Result<(), String> {
        let mut got_begin = false;
        loop {
            match self.next_event()? {
                Event::Begin { .. } => got_begin = true,
                Event::End { .. } if got_begin => return Ok(()),
                Event::Exit => {
                    return Err("tmux closed before initial handshake".to_string());
                }
                _ => {}
            }
        }
    }

    /// Send a command and consume events until its `%end`/`%error`.
    /// Notifications received between commands are dropped here —
    /// milestone 1 callers only do request/response. Subsequent
    /// milestones will need a queued or callback-based event sink.
    ///
    /// Identifies the response by ORDER (next `%begin` after we send),
    /// not by cmd-num, because tmux assigns its own monotonic ids
    /// (e.g. `4388032`) rather than echoing client-supplied ones.
    pub fn cmd(&mut self, command: &str) -> Result<Response, String> {
        writeln!(self.stdin, "{command}").map_err(|e| format!("write: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flush: {e}"))?;

        // Wait for response frame to start.
        loop {
            match self.next_event()? {
                Event::Begin { .. } => break,
                Event::Exit => {
                    return Err("tmux closed before response start".to_string());
                }
                _ => {}
            }
        }

        // Collect body lines until %end/%error.
        let mut output = String::new();
        loop {
            match self.next_event()? {
                Event::End { ok, .. } => return Ok(Response { ok, output }),
                Event::Output(line) => {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&line);
                }
                Event::Exit => {
                    return Err("tmux closed during response".to_string());
                }
                _ => {}
            }
        }
    }

    /// Read one event from tmux. Blocks until a full line is available
    /// or stdout closes.
    ///
    /// tmux's `%output` notifications can include raw pane bytes that
    /// aren't valid UTF-8, so we read bytes and lossy-convert. We don't
    /// inspect output bodies here anyway — we just frame on `\n`.
    pub fn next_event(&mut self) -> Result<Event, String> {
        let mut buf = Vec::new();
        let n = self
            .reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Ok(Event::Exit);
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        let line = String::from_utf8_lossy(&buf);
        Ok(parse_line(&line))
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Best-effort tidy: detach (so the server keeps running) and reap
        // the child. Killing rather than detaching would also tear down
        // the user's tmux session if this is the only client; we never
        // want that.
        let _ = writeln!(self.stdin, "detach-client");
        let _ = self.stdin.flush();
        let _ = self.child.wait();
    }
}

fn parse_line(line: &str) -> Event {
    if let Some(rest) = line.strip_prefix("%begin ") {
        let cmd = parse_cmd_num(rest);
        return Event::Begin { cmd };
    }
    if let Some(rest) = line.strip_prefix("%end ") {
        let cmd = parse_cmd_num(rest);
        return Event::End { cmd, ok: true };
    }
    if let Some(rest) = line.strip_prefix("%error ") {
        let cmd = parse_cmd_num(rest);
        return Event::End { cmd, ok: false };
    }
    if let Some(rest) = line.strip_prefix('%') {
        let (name, args) = match rest.split_once(' ') {
            Some((n, a)) => (n.to_string(), a.to_string()),
            None => (rest.to_string(), String::new()),
        };
        return Event::Notification { name, args };
    }
    Event::Output(line.to_string())
}

/// `%begin/%end/%error` line format is `<ts> <cmd-num> <flags>`. We only
/// care about the command number for matching responses.
fn parse_cmd_num(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_begin_extracts_cmd_num() {
        match parse_line("%begin 1733511234 7 0") {
            Event::Begin { cmd } => assert_eq!(cmd, 7),
            other => panic!("expected Begin, got {other:?}"),
        }
    }

    #[test]
    fn parse_end_and_error() {
        match parse_line("%end 1 2 0") {
            Event::End { cmd: 2, ok: true } => {}
            other => panic!("{other:?}"),
        }
        match parse_line("%error 1 3 0") {
            Event::End { cmd: 3, ok: false } => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_notification_with_args() {
        match parse_line("%window-pane-changed $1 %5") {
            Event::Notification { name, args } => {
                assert_eq!(name, "window-pane-changed");
                assert_eq!(args, "$1 %5");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_notification_without_args() {
        match parse_line("%sessions-changed") {
            Event::Notification { name, args } => {
                assert_eq!(name, "sessions-changed");
                assert!(args.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_output_passes_line_through() {
        match parse_line("just a body line") {
            Event::Output(s) => assert_eq!(s, "just a body line"),
            other => panic!("{other:?}"),
        }
    }
}
