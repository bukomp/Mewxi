//! Command-block execution.
//!
//! A command block runs a user-authored shell command on every
//! `mewxi status` refresh and shows its (sanitized) stdout. Three things
//! keep that safe and snappy:
//!
//!   - **Bounded time**: the command is reaped after `timeout` (a reader
//!     thread + `recv_timeout`, then `kill`), so a slow/hung command can
//!     never stall the every-~5s status refresh.
//!   - **Sanitized output**: only the first line is used, with ESC /
//!     control bytes stripped and the result truncated — no raw ANSI or
//!     newline can leak in and break the one-line status.
//!   - **Best-effort TTL cache**: results are cached on disk keyed by the
//!     command text, so repeated refreshes within the TTL return instantly
//!     without re-spawning.
//!
//! Security: blocks are the user's own local config — the same trust level
//! as the `$EDITOR` mewxi already shells out to. No `{field}` value is ever
//! interpolated into the command, so there is no injection surface beyond
//! what the user wrote themselves.

use super::engine;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

/// Max rendered width of a command block's output, in chars.
const MAX_OUTPUT_CHARS: usize = 40;

/// How long a cached result stays fresh. Matched to the statusline
/// refresh cadence so a command runs at most ~once per refresh.
const CACHE_TTL: Duration = Duration::from_secs(5);

/// Run `command`, returning its sanitized (and optionally colored) output,
/// or an empty string on failure/timeout/empty output. Honors a short
/// on-disk TTL cache so back-to-back refreshes don't re-spawn.
pub fn run_command_block(command: &str, timeout: Duration, color: Option<&str>) -> String {
    let cleaned = match cached(command) {
        Some(c) => c,
        None => {
            let raw = run_capture(command, timeout).unwrap_or_default();
            let c = sanitize(&raw);
            store_cache(command, &c);
            c
        }
    };
    if cleaned.is_empty() {
        return String::new();
    }
    match color.and_then(engine::color_code) {
        Some(code) => format!("\x1b[{code}m{cleaned}\x1b[0m"),
        None => cleaned,
    }
}

/// Spawn the command under the platform shell and capture stdout, bounded
/// by `timeout`. Returns `None` on spawn error or timeout.
fn run_capture(command: &str, timeout: Duration) -> Option<String> {
    let mut child = shell_command(command)
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

    let result = rx.recv_timeout(timeout).ok();
    // Reap regardless — kill is a no-op if it already exited.
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn shell_command(command: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

/// First line only, with ESC sequences + C0 control chars removed, trimmed
/// and truncated. Guarantees the result can't break the one-line status.
fn sanitize(raw: &str) -> String {
    let first = raw.lines().next().unwrap_or("");
    let mut out = String::with_capacity(first.len());
    let mut it = first.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            // Drop the whole escape. CSI is `ESC [ params… final(0x40..=0x7e)`.
            if it.peek() == Some(&'[') {
                it.next();
                while let Some(&n) = it.peek() {
                    it.next();
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            } else {
                it.next(); // best-effort: drop the single following byte
            }
            continue;
        }
        if (c as u32) < 0x20 {
            continue; // other C0 controls
        }
        out.push(c);
    }
    truncate_chars(out.trim(), MAX_OUTPUT_CHARS)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Cache file for a given command, keyed by a hash of the command text.
fn cache_path(command: &str) -> Option<PathBuf> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(command.as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("mewxi")
        .join("statusline-cmd");
    Some(dir.join(format!("{hex}.txt")))
}

/// Return the cached sanitized output if it's younger than [`CACHE_TTL`].
fn cached(command: &str) -> Option<String> {
    let path = cache_path(command)?;
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

/// Persist `value` (atomically) as the cached output for `command`.
fn store_cache(command: &str, value: &str) {
    let Some(path) = cache_path(command) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("txt.tmp");
    if std::fs::write(&tmp, value).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_ansi_and_takes_first_line() {
        let raw = "\x1b[32mmain\x1b[0m\nsecond line";
        assert_eq!(sanitize(raw), "main");
    }

    #[test]
    fn sanitize_truncates_long_output() {
        let raw = "x".repeat(100);
        let out = sanitize(&raw);
        assert_eq!(out.chars().count(), MAX_OUTPUT_CHARS);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn sanitize_drops_control_chars() {
        assert_eq!(sanitize("a\tb\x07c"), "abc");
    }

    #[cfg(unix)]
    #[test]
    fn echo_command_runs_and_colors() {
        let out = run_command_block("printf 'hello'", Duration::from_millis(1000), Some("green"));
        assert_eq!(out, "\x1b[32mhello\x1b[0m");
    }

    #[cfg(unix)]
    #[test]
    fn hung_command_times_out_empty() {
        let start = std::time::Instant::now();
        // Unique command text so the TTL cache from other tests can't hit.
        let out = run_command_block("sleep 5; echo nope-unique-xyz", Duration::from_millis(120), None);
        assert_eq!(out, "");
        assert!(start.elapsed() < Duration::from_secs(2), "should not block on the sleep");
    }
}
