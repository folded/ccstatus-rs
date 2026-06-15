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

/// One row of a [`ps_snapshot`]: a process's id, parent, kernel state code, and
/// full command line.
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    /// The `ps` state field; its first character is the primary code (`T` =
    /// stopped/suspended, `S` sleeping, `R` running, `Z` zombie, …).
    pub state: String,
    pub command: String,
}

/// IO: `(pid, ppid, state, command)` for every process, via one `ps`. The flags
/// are the portable intersection of BSD (macOS) and procps (Linux) `ps`; `=`
/// suffixes suppress headers. `command` is last (it has spaces); the first three
/// fields are single tokens. Empty on failure (detectors degrade to off).
pub fn ps_snapshot() -> Vec<ProcInfo> {
    let Ok(out) = Command::new("ps")
        .args(["-axo", "pid=,ppid=,state=,command="])
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
            let state = it.next()?.to_string();
            let command = it.collect::<Vec<_>>().join(" ");
            Some(ProcInfo {
                pid,
                ppid,
                state,
                command,
            })
        })
        .collect()
}

/// Pure: which of `claude_pids` host a live background Bash task — a session
/// process with a descendant carrying the [`BG_WRAPPER_MARKER`].
pub fn background_task_pids(procs: &[ProcInfo], claude_pids: &HashSet<u32>) -> HashSet<u32> {
    roots_with_descendant_matching(procs, claude_pids, BG_WRAPPER_MARKER)
}

/// Pure: which of `claude_pids` are suspended (Ctrl-Z'd or `SIGSTOP`'d) — `ps`
/// state code `T`. A stopped process can't make progress, so the fleet flags it
/// rather than showing a stale working/waiting state.
pub fn suspended_pids(procs: &[ProcInfo], claude_pids: &HashSet<u32>) -> HashSet<u32> {
    procs
        .iter()
        .filter(|p| claude_pids.contains(&p.pid) && p.state.starts_with('T'))
        .map(|p| p.pid)
        .collect()
}

/// Pure: the subset of `roots` that have a descendant whose command contains
/// `marker`. Builds a ppid→children map and DFS-walks each root's subtree (the
/// root's own command is not matched — a session process never carries the
/// marker). Cycle-guarded.
fn roots_with_descendant_matching(
    procs: &[ProcInfo],
    roots: &HashSet<u32>,
    marker: &str,
) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut cmd: HashMap<u32, &str> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p.pid);
        cmd.insert(p.pid, p.command.as_str());
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

    fn proc(pid: u32, ppid: u32, state: &str, command: &str) -> super::ProcInfo {
        super::ProcInfo {
            pid,
            ppid,
            state: state.into(),
            command: command.into(),
        }
    }

    #[test]
    fn background_task_detection_walks_the_tree() {
        use super::background_task_pids;
        use std::collections::HashSet;
        // pid 100 = a claude session with a live bg-task wrapper (102) whose
        //           leaf is `sleep 600` (103); also an MCP server (104).
        // pid 200 = a claude session with only an MCP server (201) — no task.
        let procs = vec![
            proc(100, 1, "S", "claude --resume"),
            proc(
                102,
                100,
                "S",
                "/bin/zsh -c source /home/u/.claude/shell-snapshots/snapshot-zsh-1.sh && eval 'sleep 600'",
            ),
            proc(103, 102, "S", "sleep 600"),
            proc(104, 100, "S", "/usr/bin/python3 mcp_server.py"),
            proc(200, 1, "S", "claude"),
            proc(201, 200, "S", "node mcp.js"),
            proc(300, 1, "S", "caffeinate -i -t 300"),
        ];
        let roots: HashSet<u32> = [100, 200].into_iter().collect();
        let hit = background_task_pids(&procs, &roots);
        assert!(hit.contains(&100), "session with a bg wrapper should match");
        assert!(
            !hit.contains(&200),
            "session with only an MCP server should not"
        );
    }

    #[test]
    fn suspended_detection_reads_state_code() {
        use super::suspended_pids;
        use std::collections::HashSet;
        let procs = vec![
            proc(100, 1, "T", "claude --resume"), // stopped (Ctrl-Z)
            proc(200, 1, "S", "claude"),          // sleeping
            proc(300, 1, "R+", "claude"),         // running, foreground
            proc(400, 1, "T", "vim"),             // stopped but not a tracked pid
        ];
        let roots: HashSet<u32> = [100, 200, 300].into_iter().collect();
        let susp = suspended_pids(&procs, &roots);
        assert_eq!(susp, [100].into_iter().collect::<HashSet<u32>>());
    }
}
