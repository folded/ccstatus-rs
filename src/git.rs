use std::process::Command;

pub struct GitInfo {
    pub branch: String,
    pub added: u64,
    pub deleted: u64,
}

pub fn collect(cwd: &str) -> Option<GitInfo> {
    let branch_out = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !branch_out.status.success() {
        return None;
    }
    let branch = String::from_utf8(branch_out.stdout)
        .ok()?
        .trim()
        .to_string();
    if branch.is_empty() {
        return None;
    }

    let (added, deleted) = diff_numstat(cwd).unwrap_or((0, 0));
    Some(GitInfo {
        branch,
        added,
        deleted,
    })
}

/// Working-tree + upstream-relative state, from a single local `git status`
/// (no network — ahead/behind are vs the last-fetched remote-tracking ref).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitStatus {
    /// Uncommitted changes present (modified, staged, or untracked).
    pub dirty: bool,
    /// Local commits not in the upstream (0 if no upstream).
    pub ahead: u32,
    /// Upstream commits not local, as of the last fetch (0 if no upstream).
    pub behind: u32,
}

/// One local `git status --porcelain=v2 --branch` in `cwd`. `None` when `cwd`
/// isn't a git work tree (the command fails). No network access.
pub fn status(cwd: &str) -> Option<GitStatus> {
    let out = Command::new("git")
        .args(["-C", cwd, "status", "--porcelain=v2", "--branch"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_status_v2(&String::from_utf8_lossy(&out.stdout)))
}

/// Pure: fold porcelain-v2 output into a [`GitStatus`]. The `# branch.ab +A -B`
/// header carries ahead/behind (absent when there's no upstream); any other
/// non-`#` line is a changed or untracked path, i.e. a dirty tree.
fn parse_status_v2(s: &str) -> GitStatus {
    let mut st = GitStatus::default();
    for line in s.lines() {
        if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for tok in ab.split_whitespace() {
                if let Some(n) = tok.strip_prefix('+') {
                    st.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = tok.strip_prefix('-') {
                    st.behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.is_empty() && !line.starts_with('#') {
            st.dirty = true;
        }
    }
    st
}

fn diff_numstat(cwd: &str) -> Option<(u64, u64)> {
    let out = Command::new("git")
        .args(["-C", cwd, "diff", "--numstat"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut a = 0u64;
    let mut d = 0u64;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(ap), Some(dp)) = (it.next(), it.next()) else {
            continue;
        };
        a += ap.parse::<u64>().unwrap_or(0);
        d += dp.parse::<u64>().unwrap_or(0);
    }
    Some((a, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_clean_in_sync() {
        let s = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n";
        assert_eq!(
            parse_status_v2(s),
            GitStatus {
                dirty: false,
                ahead: 0,
                behind: 0
            }
        );
    }

    #[test]
    fn parse_status_ahead_behind_diverged() {
        let ahead = parse_status_v2("# branch.ab +2 -0\n");
        assert_eq!((ahead.ahead, ahead.behind, ahead.dirty), (2, 0, false));
        let behind = parse_status_v2("# branch.ab +0 -3\n");
        assert_eq!((behind.ahead, behind.behind, behind.dirty), (0, 3, false));
        let div = parse_status_v2("# branch.ab +2 -3\n");
        assert_eq!((div.ahead, div.behind, div.dirty), (2, 3, false));
    }

    #[test]
    fn parse_status_dirty_from_changed_and_untracked() {
        // A changed tracked file (1 ...) marks dirty.
        let changed = parse_status_v2("# branch.ab +0 -0\n1 .M N... 100644 100644 100644 a b file.rs\n");
        assert!(changed.dirty);
        // An untracked file (? ...) also marks dirty.
        let untracked = parse_status_v2("# branch.ab +0 -0\n? new.rs\n");
        assert!(untracked.dirty);
    }

    #[test]
    fn parse_status_no_upstream_has_no_counts() {
        // No `# branch.ab` line when there's no upstream.
        let s = "# branch.oid abc\n# branch.head main\n";
        let st = parse_status_v2(s);
        assert_eq!((st.ahead, st.behind, st.dirty), (0, 0, false));
    }
}
