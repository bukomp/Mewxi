//! Git dirty-count lookup.
//!
//! A status block can show how many files are dirty in the working tree, so
//! it needs to ask `git status --porcelain` on every `mewxi status` refresh.
//! Two things keep that safe and snappy:
//!
//!   - **Bounded time**: the process is reaped after a 300ms budget (a
//!     reader thread + `recv_timeout`, then `kill`), so a slow/hung `git`
//!     (e.g. over a network filesystem) can never stall the refresh.
//!   - **Best-effort TTL cache**: results are cached on disk keyed by the
//!     working directory, so repeated refreshes within the TTL return
//!     instantly without re-spawning `git`.
//!
//! `git` is spawned directly (no shell), so there is no injection surface
//! and the behavior is the same across platforms.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// Bound on how long we'll wait for `git status` before giving up.
const RUN_TIMEOUT: Duration = Duration::from_millis(300);

/// How long a cached result stays fresh.
const CACHE_TTL: Duration = Duration::from_secs(5);

/// Sentinel stored on disk to mean "not a repo / unavailable" (as opposed
/// to a parsed count).
const NOT_A_REPO: &str = "-";

/// Number of dirty (non-empty `git status --porcelain` line) entries in
/// `cwd`'s working tree, or `None` if `cwd` isn't a git repo, `git` isn't
/// available, or the lookup times out. Honors a short on-disk TTL cache so
/// back-to-back refreshes don't re-spawn `git`.
pub(crate) fn dirty_count(cwd: &Path) -> Option<u64> {
    if let Some(hit) = cached(cwd) {
        return decode(&hit);
    }
    let result = run_git_status(cwd);
    let stored = match result {
        Some(n) => n.to_string(),
        None => NOT_A_REPO.to_string(),
    };
    store_cache(cwd, &stored);
    result
}

/// Spawn `git status --porcelain` in `cwd` and count non-empty output
/// lines, bounded by [`RUN_TIMEOUT`]. Returns `None` on spawn error,
/// timeout, or non-zero exit (e.g. exit 128 outside a work tree).
fn run_git_status(cwd: &Path) -> Option<u64> {
    let mut child = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Read stdout on a detached thread so a chatty command can't deadlock
    // against the pipe buffer; bound the wait with recv_timeout.
    let stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = stdout;
        let _ = reader.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    let buf = match rx.recv_timeout(RUN_TIMEOUT) {
        Ok(buf) => buf,
        Err(_) => {
            // Timed out: reap and bail.
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    // Stdout is closed (the reader thread finished), so the process should
    // exit promptly; get its status.
    let status = child.wait().ok()?;
    if !status.success() {
        return None;
    }

    let count = buf
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count() as u64;
    Some(count)
}

/// Cache file for a given cwd, keyed by a hash of the path.
fn cache_path(cwd: &Path) -> Option<PathBuf> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(cwd.to_string_lossy().as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mewxi")
        .join("statusline-git");
    Some(dir.join(format!("{hex}.txt")))
}

/// Return the cached raw value (decimal count or [`NOT_A_REPO`]) if it's
/// younger than [`CACHE_TTL`].
fn cached(cwd: &Path) -> Option<String> {
    let path = cache_path(cwd)?;
    let meta = std::fs::metadata(&path).ok()?;
    let age = meta
        .modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())?;
    if age > CACHE_TTL {
        return None;
    }
    std::fs::read_to_string(&path).ok()
}

/// Persist `value` (atomically) as the cached raw value for `cwd`.
fn store_cache(cwd: &Path, value: &str) {
    let Some(path) = cache_path(cwd) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("txt.tmp");
    if std::fs::write(&tmp, value).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Decode a cached raw value into the `dirty_count` result.
fn decode(raw: &str) -> Option<u64> {
    if raw == NOT_A_REPO {
        return None;
    }
    raw.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn clean_repo_is_some_zero() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        assert_eq!(dirty_count(dir.path()), Some(0));
    }

    #[test]
    fn untracked_files_are_counted() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .arg("init")
            .current_dir(dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        for i in 0..3 {
            std::fs::write(dir.path().join(format!("file{i}.txt")), "content").unwrap();
        }

        assert_eq!(dirty_count(dir.path()), Some(3));
    }

    #[test]
    fn non_repo_dir_is_none() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(dirty_count(dir.path()), None);
    }
}
