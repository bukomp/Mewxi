//! Structured debug logger with an in-memory ring buffer and a
//! file-backed, size-rotated log on disk.
//!
//! Every event is tagged with a [`LogOrigin`] (which subsystem) and a
//! [`LogKind`] (what kind of thing happened), plus a free-form
//! `message`. Call [`log_event`] to record one; [`log`] is a thin
//! legacy shim for older call sites that only have a plain string
//! (it maps to `LogOrigin::Tui` / `LogKind::Info`).
//!
//! Every event is always pushed to a bounded in-memory ring
//! (`RING_CAP` = 1000 entries, oldest dropped once full) regardless of
//! whether file logging is enabled — this is what the TUI's live logs
//! panel polls via [`recent`] and [`ring_version`]. The ring is pure
//! process memory, never touches disk, and can't fail.
//!
//! In parallel, each event is best-effort appended as a line to
//! `~/.cache/mewxi/logs/mewxi-XXXX.log`:
//! `2026-07-09T12:34:56.789Z [origin] [kind] message`. The file
//! rotates to a new index once the current one hits
//! `MAX_LINES_PER_FILE` lines, and keeps at most `MAX_FILES` files —
//! oldest are removed when a new one is created. Disabled silently if
//! the cache dir can't be created, or if `MEWXI_LOG=0` is set.
//!
//! On disk this bounds growth to `MAX_FILES` * `MAX_LINES_PER_FILE`
//! (20 * 10,000 = 200,000 lines); in memory it's capped at
//! `RING_CAP` (1000) entries. Logging never panics and never bubbles
//! errors — the point of the logger is to help diagnose other bugs,
//! it must never become a bug source itself.

use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

const MAX_LINES_PER_FILE: usize = 10_000;
const MAX_FILES: usize = 20;
const RING_CAP: usize = 1000;

/// Which subsystem an event came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogOrigin {
    Usage,
    Update,
    Auth,
    Accounts,
    Sessions,
    Agents,
    Setup,
    Statusline,
    Tui,
}

impl LogOrigin {
    pub const ALL: [LogOrigin; 9] = [
        LogOrigin::Usage,
        LogOrigin::Update,
        LogOrigin::Auth,
        LogOrigin::Accounts,
        LogOrigin::Sessions,
        LogOrigin::Agents,
        LogOrigin::Setup,
        LogOrigin::Statusline,
        LogOrigin::Tui,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            LogOrigin::Usage => "usage",
            LogOrigin::Update => "update",
            LogOrigin::Auth => "auth",
            LogOrigin::Accounts => "accounts",
            LogOrigin::Sessions => "sessions",
            LogOrigin::Agents => "agents",
            LogOrigin::Setup => "setup",
            LogOrigin::Statusline => "statusline",
            LogOrigin::Tui => "tui",
        }
    }
}

/// What kind of thing happened.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogKind {
    Api,
    FileRead,
    FileWrite,
    Proc,
    Info,
    Error,
}

impl LogKind {
    pub const ALL: [LogKind; 6] = [
        LogKind::Api,
        LogKind::FileRead,
        LogKind::FileWrite,
        LogKind::Proc,
        LogKind::Info,
        LogKind::Error,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            LogKind::Api => "api",
            LogKind::FileRead => "read",
            LogKind::FileWrite => "write",
            LogKind::Proc => "proc",
            LogKind::Info => "info",
            LogKind::Error => "error",
        }
    }
}

/// A single structured log event, as kept in the in-memory ring.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub ts: DateTime<Utc>,
    pub origin: LogOrigin,
    pub kind: LogKind,
    pub message: String,
}

struct Logger {
    dir: PathBuf,
    file: Option<BufWriter<File>>,
    file_idx: u64,
    line_count: usize,
}

static LOGGER: OnceLock<Option<Mutex<Logger>>> = OnceLock::new();
static RING: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
static RING_VERSION: AtomicU64 = AtomicU64::new(0);

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

fn ring() -> &'static Mutex<VecDeque<LogEntry>> {
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAP)))
}

impl Logger {
    /// Write an already-formatted line (timestamp + tags + message)
    /// verbatim, rotating first if needed.
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        if self.file.is_none() || self.line_count >= MAX_LINES_PER_FILE {
            self.rotate()?;
        }
        let writer = self
            .file
            .as_mut()
            .expect("file is Some after rotate()");
        writeln!(writer, "{line}")?;
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

/// Push an entry onto the in-memory ring, dropping the oldest once
/// full, and bump the version counter. Never fails — a poisoned
/// mutex just means the event is silently dropped from the ring.
fn push_ring(entry: LogEntry) {
    if let Ok(mut g) = ring().lock() {
        if g.len() >= RING_CAP {
            g.pop_front();
        }
        g.push_back(entry);
    }
    RING_VERSION.fetch_add(1, Ordering::Relaxed);
}

/// Append a structured event: pushed to the in-memory ring (always,
/// even when file logging is disabled) and appended to the rotating
/// file (best-effort).
pub fn log_event(origin: LogOrigin, kind: LogKind, message: &str) {
    let ts = Utc::now();

    push_ring(LogEntry {
        ts,
        origin,
        kind,
        message: message.to_string(),
    });

    let Some(m) = logger() else { return };
    if let Ok(mut g) = m.lock() {
        let line = format!(
            "{} [{}] [{}] {}",
            ts.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            origin.as_str(),
            kind.as_str(),
            message
        );
        let _ = g.write_line(&line);
    }
}

/// Legacy shim used by existing call sites in tui/mod.rs and main.rs.
/// Equivalent to `log_event(LogOrigin::Tui, LogKind::Info, line)`.
pub fn log(line: &str) {
    log_event(LogOrigin::Tui, LogKind::Info, line);
}

/// Snapshot of the ring, oldest → newest. Capped at `RING_CAP` (1000).
pub fn recent() -> Vec<LogEntry> {
    match ring().lock() {
        Ok(g) => g.iter().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

/// Monotonic counter bumped on every append — lets the TUI skip
/// re-cloning the ring when nothing new arrived.
pub fn ring_version() -> u64 {
    RING_VERSION.load(Ordering::Relaxed)
}

/// Cycle helper for the TUI's origin filter key: `None` → first
/// `LogOrigin::ALL` entry → … → last → `None`.
pub fn cycle_origin(cur: Option<LogOrigin>) -> Option<LogOrigin> {
    match cur {
        None => Some(LogOrigin::ALL[0]),
        Some(c) => {
            let idx = LogOrigin::ALL.iter().position(|o| *o == c).unwrap_or(0);
            LogOrigin::ALL.get(idx + 1).copied()
        }
    }
}

/// Cycle helper for the TUI's kind filter key: `None` → first
/// `LogKind::ALL` entry → … → last → `None`.
pub fn cycle_kind(cur: Option<LogKind>) -> Option<LogKind> {
    match cur {
        None => Some(LogKind::ALL[0]),
        Some(c) => {
            let idx = LogKind::ALL.iter().position(|k| *k == c).unwrap_or(0);
            LogKind::ALL.get(idx + 1).copied()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_as_str_values() {
        assert_eq!(LogOrigin::Usage.as_str(), "usage");
        assert_eq!(LogOrigin::Update.as_str(), "update");
        assert_eq!(LogOrigin::Auth.as_str(), "auth");
        assert_eq!(LogOrigin::Accounts.as_str(), "accounts");
        assert_eq!(LogOrigin::Sessions.as_str(), "sessions");
        assert_eq!(LogOrigin::Agents.as_str(), "agents");
        assert_eq!(LogOrigin::Setup.as_str(), "setup");
        assert_eq!(LogOrigin::Statusline.as_str(), "statusline");
        assert_eq!(LogOrigin::Tui.as_str(), "tui");
    }

    #[test]
    fn kind_as_str_values() {
        assert_eq!(LogKind::Api.as_str(), "api");
        assert_eq!(LogKind::FileRead.as_str(), "read");
        assert_eq!(LogKind::FileWrite.as_str(), "write");
        assert_eq!(LogKind::Proc.as_str(), "proc");
        assert_eq!(LogKind::Info.as_str(), "info");
        assert_eq!(LogKind::Error.as_str(), "error");
    }

    #[test]
    fn origin_all_length_and_order() {
        assert_eq!(LogOrigin::ALL.len(), 9);
        assert_eq!(
            LogOrigin::ALL,
            [
                LogOrigin::Usage,
                LogOrigin::Update,
                LogOrigin::Auth,
                LogOrigin::Accounts,
                LogOrigin::Sessions,
                LogOrigin::Agents,
                LogOrigin::Setup,
                LogOrigin::Statusline,
                LogOrigin::Tui,
            ]
        );
    }

    #[test]
    fn kind_all_length_and_order() {
        assert_eq!(LogKind::ALL.len(), 6);
        assert_eq!(
            LogKind::ALL,
            [
                LogKind::Api,
                LogKind::FileRead,
                LogKind::FileWrite,
                LogKind::Proc,
                LogKind::Info,
                LogKind::Error,
            ]
        );
    }

    #[test]
    fn cycle_origin_full_cycle_with_none_wrap() {
        let mut cur = None;
        for expected in LogOrigin::ALL {
            cur = cycle_origin(cur);
            assert_eq!(cur, Some(expected));
        }
        // one past the last entry wraps back to None
        cur = cycle_origin(cur);
        assert_eq!(cur, None);
        // and cycling from None starts over at the first entry
        cur = cycle_origin(cur);
        assert_eq!(cur, Some(LogOrigin::ALL[0]));
    }

    #[test]
    fn cycle_kind_full_cycle_with_none_wrap() {
        let mut cur = None;
        for expected in LogKind::ALL {
            cur = cycle_kind(cur);
            assert_eq!(cur, Some(expected));
        }
        cur = cycle_kind(cur);
        assert_eq!(cur, None);
        cur = cycle_kind(cur);
        assert_eq!(cur, Some(LogKind::ALL[0]));
    }

    /// The ring is process-global and tests may run concurrently, so
    /// every assertion here is scoped to entries carrying a marker
    /// unique to this test, never to the ring's raw contents.
    fn marker(tag: &str) -> String {
        format!(
            "debug_log::tests::{tag}::{:?}",
            std::thread::current().id()
        )
    }

    #[test]
    fn ring_append_and_recent_orders_oldest_to_newest() {
        let mk = marker("order");
        for i in 0..5u32 {
            log_event(LogOrigin::Agents, LogKind::Info, &format!("{mk} {i}"));
        }
        let entries = recent();
        let mine: Vec<u32> = entries
            .iter()
            .filter_map(|e| {
                e.message
                    .strip_prefix(&format!("{mk} "))
                    .and_then(|n| n.parse::<u32>().ok())
            })
            .collect();
        // Only the last few of ours are guaranteed to still be present
        // (the ring is shared with other tests), but whichever of our
        // markers survived must appear in strictly increasing order.
        assert!(!mine.is_empty(), "expected at least one surviving entry");
        for pair in mine.windows(2) {
            assert!(pair[0] < pair[1], "ring entries out of order: {mine:?}");
        }
    }

    #[test]
    fn ring_cap_enforced_and_oldest_dropped() {
        let mk = marker("cap");
        // Push one more than RING_CAP entries carrying our own unique
        // marker. Regardless of what other tests interleave into the
        // ring concurrently, the ring can only ever hold RING_CAP
        // entries total, so by the time our own (RING_CAP + 1)-th push
        // lands, our own index 0 must already have been evicted.
        for i in 0..=RING_CAP {
            log_event(LogOrigin::Agents, LogKind::Error, &format!("{mk} {i}"));
        }
        let entries = recent();
        assert!(entries.len() <= RING_CAP);

        let mine: Vec<usize> = entries
            .iter()
            .filter_map(|e| {
                e.message
                    .strip_prefix(&format!("{mk} "))
                    .and_then(|n| n.parse::<usize>().ok())
            })
            .collect();

        assert!(
            !mine.contains(&0),
            "oldest entry should have been evicted from the ring: {mine:?}"
        );
        assert!(
            mine.contains(&RING_CAP),
            "most recent entry should still be in the ring: {mine:?}"
        );
        for pair in mine.windows(2) {
            assert!(pair[0] < pair[1], "ring entries out of order: {mine:?}");
        }
    }

    #[test]
    fn ring_version_is_monotonic() {
        let before = ring_version();
        log_event(LogOrigin::Setup, LogKind::Info, &marker("version"));
        let after = ring_version();
        assert!(after > before);
    }

    #[test]
    fn log_shim_maps_to_tui_info() {
        let mk = marker("shim");
        log(&mk);
        let entries = recent();
        let found = entries
            .iter()
            .rev()
            .find(|e| e.message == mk)
            .expect("log() entry should be in the ring");
        assert_eq!(found.origin, LogOrigin::Tui);
        assert_eq!(found.kind, LogKind::Info);
    }
}
