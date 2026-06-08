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
