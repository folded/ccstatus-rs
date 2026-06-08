//! Small helpers shared across modes.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Seconds since the UNIX epoch. Falls back to 0 if the system clock is
/// before the epoch (which would be... unusual).
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Whether a process is still alive (`kill(pid, 0)`: 0 = exists, EPERM =
/// exists but unsignalable, ESRCH = gone). `pid == 0` is treated as dead.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether `pid` is an **interactive Claude session** process — a `claude`
/// binary that is *not* the shared `claude daemon` or a `--bg-*` background
/// helper. Both of those outlive any single conversation (the daemon is
/// effectively immortal), so binding a session's liveness to one makes the
/// session immortal in the fleet. Returns false if the pid is gone or not
/// Claude at all.
///
/// Recognises both launcher forms — `.../bin/claude` and the versioned binary
/// `.../share/claude/versions/<v>` — by looking for a `claude` path component,
/// then excludes the daemon/helpers by their argv markers.
pub fn is_interactive_claude(pid: u32) -> bool {
    let Some(args) = ps_command(pid) else {
        return false;
    };
    is_interactive_claude_cmd(&args)
}

/// Pure predicate over a process's full command line, split out for testing.
fn is_interactive_claude_cmd(args: &str) -> bool {
    let exe = args.split_whitespace().next().unwrap_or("");
    let is_claude = exe.split('/').any(|c| c == "claude")
        || exe
            .rsplit('/')
            .next()
            .is_some_and(|l| l == "claude" || l.starts_with("claude-"));
    is_claude && !args.contains(" daemon ") && !args.contains("--bg-")
}

/// The full command line of `pid` via `ps -o command=`, or `None` if the
/// process is gone.
fn ps_command(pid: u32) -> Option<String> {
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Extract the Claude session id from a stdin payload. Prefers an explicit
/// `session_id` top-level field; falls back to the basename (without
/// extension) of `transcript_path` so older / leaner payloads still work.
pub fn resolve_session_id(input: &Value) -> Option<String> {
    if let Some(s) = input.get("session_id").and_then(|v| v.as_str())
        && !s.is_empty()
    {
        return Some(s.to_string());
    }
    let path = input.get("transcript_path").and_then(|v| v.as_str())?;
    let basename = path.rsplit('/').next()?;
    let stem = basename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(basename);
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::is_interactive_claude_cmd as ic;

    #[test]
    fn interactive_claude_accepts_both_launcher_forms() {
        assert!(ic("claude"));
        assert!(ic("/Users/x/.local/bin/claude"));
        assert!(ic(
            "/Users/x/.local/share/claude/versions/2.1.168 --some-flag"
        ));
    }

    #[test]
    fn interactive_claude_rejects_daemon_and_helpers() {
        // The shared daemon (ppid 1, immortal).
        assert!(!ic(
            "/Users/x/.local/bin/claude daemon run --origin transient"
        ));
        // Background pty-host / spare helpers spawned by the daemon.
        assert!(!ic(
            "/Users/x/.local/share/claude/versions/2.1.168 --bg-pty-host /tmp/x.sock 200 50"
        ));
        assert!(!ic(
            "/Users/x/.local/share/claude/versions/2.1.168 --bg-spare /tmp/x.sock"
        ));
    }

    #[test]
    fn interactive_claude_rejects_non_claude() {
        assert!(!ic("/bin/zsh"));
        assert!(!ic("node /some/server.js"));
        assert!(!ic(""));
    }
}
