use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn cache_dir() -> PathBuf {
    PathBuf::from("/tmp/claude")
}

pub fn ensure_cache_dir() -> io::Result<()> {
    fs::create_dir_all(cache_dir())
}

pub fn read_if_fresh(path: &Path, max_age: Duration) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(mtime).ok()?;
    if age < max_age {
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
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() == 0 {
            let _ = fs::remove_file(path);
        }
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
