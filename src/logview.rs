//! `ccstatus log` — follow the backend daemon logs live, merged and tagged by
//! source (like `tail -f` across all of them at once). A companion to
//! `ccstatus top`: where `top` shows the fleet state, this shows what the
//! daemons driving it are doing. Ctrl-C to stop.
//!
//! Poll-based (re-`stat` each file every [`POLL`]) rather than fsevents/inotify,
//! for portability and because the logs are low-volume. New daemons' logs are
//! picked up as their files appear; a truncated/rotated file is re-read from 0.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use crate::cache;
use crate::color::{CYAN, DIM, RESET};

const POLL: Duration = Duration::from_millis(300);

/// Lines of history to show for each log the first time we see it, so you get
/// recent context rather than only what happens after you start watching.
const INITIAL_TAIL_LINES: usize = 20;

pub fn run() -> ExitCode {
    let root = cache::cache_dir().join("server");
    println!(
        "{DIM}streaming ccstatus daemon logs from {} (Ctrl-C to stop){RESET}",
        root.display()
    );

    // path -> byte offset we've emitted up to.
    let mut offsets: HashMap<PathBuf, u64> = HashMap::new();
    loop {
        for path in discover_logs(&root) {
            let Ok(len) = fs::metadata(&path).map(|m| m.len()) else {
                continue;
            };
            match offsets.get(&path).copied() {
                // First sight: show a recent tail, then follow from the end.
                None => {
                    emit_tail(&path, INITIAL_TAIL_LINES);
                    offsets.insert(path, len);
                }
                Some(prev) => {
                    // A shorter file was truncated/rotated — re-read from 0.
                    let from = if len < prev { 0 } else { prev };
                    if len > from {
                        let consumed = emit_from(&path, from);
                        offsets.insert(path, from + consumed);
                    }
                }
            }
        }
        let _ = io::stdout().flush();
        thread::sleep(POLL);
    }
}

/// Every `*.log` under `server/<id>/`, sorted for stable ordering.
fn discover_logs(root: &Path) -> Vec<PathBuf> {
    let mut logs = Vec::new();
    let Ok(servers) = fs::read_dir(root) else {
        return logs;
    };
    for server in servers.flatten() {
        let Ok(files) = fs::read_dir(server.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) == Some("log") {
                logs.push(p);
            }
        }
    }
    logs.sort();
    logs
}

/// A short source tag, `<server-dir>/<stem>` (e.g. `surface/daemon`,
/// `1b08f661/handler-0`), so merged lines are attributable.
fn source_label(path: &Path) -> String {
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{parent}/{stem}")
}

/// Print the last `n` lines currently in `path`.
fn emit_tail(path: &Path, n: usize) {
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    let label = source_label(path);
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        print_line(&label, line);
    }
}

/// Print the complete lines in `path` from byte `from` onward; return how many
/// bytes were consumed (up to and including the last newline). A trailing
/// partial line (mid-write) is left for the next poll.
fn emit_from(path: &Path, from: u64) -> u64 {
    let Ok(mut f) = fs::File::open(path) else {
        return 0;
    };
    if f.seek(SeekFrom::Start(from)).is_err() {
        return 0;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return 0;
    }
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return 0; // no complete line yet
    };
    let text = String::from_utf8_lossy(&buf[..=last_nl]);
    let label = source_label(path);
    for line in text.lines() {
        print_line(&label, line);
    }
    last_nl as u64 + 1
}

fn print_line(label: &str, line: &str) {
    println!("{CYAN}{label:>18}{RESET} {DIM}│{RESET} {line}");
}
