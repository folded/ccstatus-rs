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
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    reader: Option<BufReader<ChildStdout>>,
}

/// Write half of a split connection. Sends fire-and-forget commands.
/// Responses come back through the matching `EventStream`.
pub struct Writer {
    stdin: ChildStdin,
}

/// Read half of a split connection. Yields `Event`s sequentially.
pub struct EventStream {
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
            child: Some(child),
            stdin: Some(stdin),
            reader: Some(BufReader::new(stdout)),
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

    /// Consume the connection and return separate write and read halves.
    /// After splitting:
    /// - Sending commands is the writer's job; responses are not
    ///   automatically correlated. Treat commands as fire-and-forget.
    /// - The event stream yields every line as a tagged event (begin,
    ///   end, output, notification, exit).
    /// - The tmux child handle is intentionally leaked here — the OS
    ///   reaps it when the daemon process exits. Closing our stdin or
    ///   stdout pipes signals tmux to detach its control client; the
    ///   tmux server itself stays alive for other clients.
    pub fn split(mut self) -> (Writer, EventStream) {
        let stdin = self.stdin.take().expect("stdin already taken");
        let reader = self.reader.take().expect("reader already taken");
        let _child = self.child.take().expect("child already taken");
        // `_child` falls out of scope here. std::process::Child's Drop on
        // Unix is a no-op (no wait, no kill), so the tmux process keeps
        // running. The daemon's lifetime then bounds the tmux child via
        // file-descriptor lifetimes.
        (Writer { stdin }, EventStream { reader })
    }

    /// Send a command and consume events until its `%end`/`%error`.
    /// Identifies the response by ORDER (next `%begin` after we send),
    /// not by cmd-num, because tmux assigns its own monotonic ids.
    ///
    /// Notifications received between commands are dropped here. Callers
    /// that need to interleave commands with notification handling
    /// should `split()` and run an event loop.
    pub fn cmd(&mut self, command: &str) -> Result<Response, String> {
        let stdin = self.stdin.as_mut().ok_or("stdin gone")?;
        writeln!(stdin, "{command}").map_err(|e| format!("write: {e}"))?;
        stdin.flush().map_err(|e| format!("flush: {e}"))?;

        loop {
            match self.next_event()? {
                Event::Begin { .. } => break,
                Event::Exit => {
                    return Err("tmux closed before response start".to_string());
                }
                _ => {}
            }
        }

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
    pub fn next_event(&mut self) -> Result<Event, String> {
        let reader = self.reader.as_mut().ok_or("reader gone")?;
        read_event(reader)
    }
}

impl Writer {
    /// Send a command line. Does not wait for or correlate a response.
    pub fn send(&mut self, command: &str) -> std::io::Result<()> {
        writeln!(self.stdin, "{command}")?;
        self.stdin.flush()
    }
}

impl EventStream {
    pub fn next_event(&mut self) -> Result<Event, String> {
        read_event(&mut self.reader)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Only runs on a non-split Connection (split() takes the fields).
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "detach-client");
            let _ = stdin.flush();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

/// Shared line-reader. tmux's `%output` notifications can carry raw pane
/// bytes that aren't valid UTF-8, so we read bytes and lossy-convert.
/// The body-line vs `%begin`/etc. distinction is purely textual.
fn read_event(reader: &mut BufReader<ChildStdout>) -> Result<Event, String> {
    let mut buf = Vec::new();
    let n = reader
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
