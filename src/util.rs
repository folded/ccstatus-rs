//! Small helpers shared across modes.

use std::collections::{HashMap, HashSet};
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

/// Substring identifying a Claude Code Bash-tool shell: every tool command
/// (foreground or background) is run as `<shell> -c 'source …/shell-snapshots/
/// snapshot-<shell>-….sh … && eval <command>'`, so this marker appears in the
/// wrapper's argv and in nothing else Claude spawns (MCP servers, `caffeinate`,
/// the statusline/hook `ccstatus` invocations). A *background* task is one of
/// these wrappers still alive as a descendant of the session after its turn
/// ended — a foreground command only lives while the turn is in progress.
const BG_WRAPPER_MARKER: &str = "shell-snapshots/snapshot";

/// IO: which of `claude_pids` currently host a live background Bash task. One
/// `ps` snapshot of the whole process table, then a pure tree walk. Empty input
/// → empty output without spawning `ps`.
pub fn pids_with_background_tasks(claude_pids: &HashSet<u32>) -> HashSet<u32> {
    if claude_pids.is_empty() {
        return HashSet::new();
    }
    roots_with_descendant_matching(&ps_snapshot(), claude_pids, BG_WRAPPER_MARKER)
}

/// `(pid, ppid, command)` for every process, via one `ps`. The flags are the
/// portable intersection of BSD (macOS) and procps (Linux) `ps`; `=` suffixes
/// suppress headers. Empty on failure (background detection degrades to off).
fn ps_snapshot() -> Vec<(u32, u32, String)> {
    let Ok(out) = Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let cmd = it.collect::<Vec<_>>().join(" ");
            Some((pid, ppid, cmd))
        })
        .collect()
}

/// Pure: the subset of `roots` that have a descendant whose command contains
/// `marker`. Builds a ppid→children map and DFS-walks each root's subtree (the
/// root's own command is not matched — a session process never carries the
/// marker). Cycle-guarded.
fn roots_with_descendant_matching(
    procs: &[(u32, u32, String)],
    roots: &HashSet<u32>,
    marker: &str,
) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut cmd: HashMap<u32, &str> = HashMap::new();
    for (pid, ppid, c) in procs {
        children.entry(*ppid).or_default().push(*pid);
        cmd.insert(*pid, c.as_str());
    }
    let mut out = HashSet::new();
    for &root in roots {
        let mut stack: Vec<u32> = children.get(&root).cloned().unwrap_or_default();
        let mut seen = HashSet::new();
        while let Some(p) = stack.pop() {
            if !seen.insert(p) {
                continue;
            }
            if cmd.get(&p).is_some_and(|c| c.contains(marker)) {
                out.insert(root);
                break;
            }
            if let Some(cs) = children.get(&p) {
                stack.extend(cs);
            }
        }
    }
    out
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

    #[test]
    fn background_task_detection_walks_the_tree() {
        use super::{roots_with_descendant_matching, BG_WRAPPER_MARKER as M};
        use std::collections::HashSet;
        // pid 100 = a claude session with a live bg-task wrapper (102) whose
        //           leaf is `sleep 600` (103); also an MCP server (104).
        // pid 200 = a claude session with only an MCP server (201) — no task.
        let procs = vec![
            (100, 1, "claude --resume".into()),
            (102, 100, "/bin/zsh -c source /home/u/.claude/shell-snapshots/snapshot-zsh-1.sh && eval 'sleep 600'".into()),
            (103, 102, "sleep 600".into()),
            (104, 100, "/usr/bin/python3 mcp_server.py".into()),
            (200, 1, "claude".into()),
            (201, 200, "node mcp.js".into()),
            (300, 1, "caffeinate -i -t 300".into()),
        ];
        let roots: HashSet<u32> = [100, 200].into_iter().collect();
        let hit = roots_with_descendant_matching(&procs, &roots, M);
        assert!(hit.contains(&100), "session with a bg wrapper should match");
        assert!(!hit.contains(&200), "session with only an MCP server should not");
    }
}
