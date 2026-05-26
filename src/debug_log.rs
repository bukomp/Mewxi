//! File-backed debug logger with size-based rotation.
//!
//! Writes timestamped lines to `~/.cache/mewxi/logs/mewxi-XXXX.log`,
//! rotating to a new index when the current file hits
//! `MAX_LINES_PER_FILE`. Keeps at most `MAX_FILES` files — oldest are
//! removed when a new one is created. Disabled silently if the cache
//! dir can't be created (logging never panics, never bubbles errors).
//!
//! Call [`log`] from anywhere; the first call initialises a shared
//! singleton. The logger is `Send + Sync` so it's safe from any
//! thread (PTY reader, tokio tasks, the TUI main loop).
//!
//! Set `MEWXI_LOG=0` to disable. By default logging is on whenever
//! the cache dir is writable.

use chrono::Utc;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const MAX_LINES_PER_FILE: usize = 10_000;
const MAX_FILES: usize = 20;

struct Logger {
    dir: PathBuf,
    file: Option<BufWriter<File>>,
    file_idx: u64,
    line_count: usize,
}

static LOGGER: OnceLock<Option<Mutex<Logger>>> = OnceLock::new();

fn enabled_via_env() -> bool {
    std::env::var("MEWXI_LOG").ok().as_deref() != Some("0")
}

fn init_logger() -> Option<Mutex<Logger>> {
    if !enabled_via_env() {
        return None;
    }
    let dir = dirs::cache_dir()?.join("mewxi").join("logs");
    fs::create_dir_all(&dir).ok()?;
    // Pick up where the previous run left off so a fresh launch
    // doesn't overwrite the prior session's tail. Start one index past
    // the highest existing file.
    let max_idx = fs::read_dir(&dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_prefix("mewxi-"))
                .and_then(|n| n.strip_suffix(".log"))
                .and_then(|n| n.parse::<u64>().ok())
        })
        .max()
        .unwrap_or(0);
    Some(Mutex::new(Logger {
        dir,
        file: None,
        file_idx: max_idx,
        line_count: MAX_LINES_PER_FILE, // forces rotate on first write
    }))
}

fn logger() -> Option<&'static Mutex<Logger>> {
    LOGGER.get_or_init(init_logger).as_ref()
}

impl Logger {
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        if self.file.is_none() || self.line_count >= MAX_LINES_PER_FILE {
            self.rotate()?;
        }
        let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let writer = self
            .file
            .as_mut()
            .expect("file is Some after rotate()");
        writeln!(writer, "{now} {line}")?;
        writer.flush()?;
        self.line_count += 1;
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file_idx = self.file_idx.saturating_add(1);
        let path = self.dir.join(format!("mewxi-{:04}.log", self.file_idx));
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        self.file = Some(BufWriter::new(f));
        self.line_count = 0;
        self.prune();
        Ok(())
    }

    /// Delete the oldest files when more than `MAX_FILES` exist, so
    /// long-running sessions don't fill the disk.
    fn prune(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        let mut idxs: Vec<(u64, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let idx = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix("mewxi-"))
                    .and_then(|n| n.strip_suffix(".log"))
                    .and_then(|n| n.parse::<u64>().ok())?;
                Some((idx, p))
            })
            .collect();
        if idxs.len() <= MAX_FILES {
            return;
        }
        idxs.sort_by_key(|(i, _)| *i);
        let drop_count = idxs.len() - MAX_FILES;
        for (_, p) in idxs.iter().take(drop_count) {
            let _ = fs::remove_file(p);
        }
    }
}

/// Append a timestamped line to the rotating log. Best-effort —
/// silently does nothing if the logger can't be initialised, the
/// mutex is poisoned, or the underlying write fails. The point of
/// the logger is to help diagnose other bugs; it must never become
/// a bug source itself.
pub fn log(line: &str) {
    let Some(m) = logger() else { return };
    if let Ok(mut g) = m.lock() {
        let _ = g.write_line(line);
    }
}
