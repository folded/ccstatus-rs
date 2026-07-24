use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn cache_dir() -> PathBuf {
    // Per-user root so multiple users on the same machine don't fight over
    // ownership of the cache directory. `geteuid` is the right id here
    // (cache state belongs to whoever is actually executing ccstatus, not
    // the login user). Named `ccstatus-` rather than `claude-` to avoid
    // colliding with Claude Code's own `/tmp/claude-<uid>/` task directory.
    let uid = unsafe { libc::geteuid() };
    PathBuf::from(format!("/tmp/ccstatus-{uid}"))
}

pub fn ensure_cache_dir() -> io::Result<()> {
    fs::create_dir_all(cache_dir())
}

/// Whether `path` exists and its mtime is younger than `max_age`.
pub fn is_fresh(path: &Path, max_age: Duration) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(mtime)
        .is_ok_and(|age| age < max_age)
}

pub fn read_if_fresh(path: &Path, max_age: Duration) -> Option<String> {
    if is_fresh(path, max_age) {
        fs::read_to_string(path).ok()
    } else {
        None
    }
}

pub fn read_stale(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().filter(|s| !s.is_empty())
}

pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

pub fn touch(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    // Update mtime to "now" so stampede-lock works even if the file already exists.
    let now = SystemTime::now();
    let _ = filetime_set(path, now);
    Ok(())
}

pub fn remove_if_empty(path: &Path) {
    if let Ok(meta) = fs::metadata(path)
        && meta.len() == 0
    {
        let _ = fs::remove_file(path);
    }
}

fn filetime_set(path: &Path, t: SystemTime) -> io::Result<()> {
    // Set both atime and mtime to `t` using libc::utimes via std on unix.
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let ft = std::fs::FileTimes::new()
        .set_modified(UNIX_EPOCH + dur)
        .set_accessed(UNIX_EPOCH + dur);
    let f = fs::OpenOptions::new().write(true).open(path)?;
    f.set_times(ft)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_fresh_tracks_mtime_age() {
        let dir = std::env::temp_dir().join(format!("ccstatus-test-cache-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("stamp");
        let _ = fs::remove_file(&path);
        assert!(
            !is_fresh(&path, Duration::from_secs(60)),
            "missing file is not fresh"
        );
        touch(&path).unwrap();
        assert!(is_fresh(&path, Duration::from_secs(60)));
        filetime_set(&path, SystemTime::now() - Duration::from_secs(120)).unwrap();
        assert!(
            !is_fresh(&path, Duration::from_secs(60)),
            "aged mtime is stale"
        );
        let _ = fs::remove_file(&path);
    }
}
