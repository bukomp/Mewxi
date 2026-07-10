//! Structured debug logger with an in-memory ring buffer and a single
//! line-capped, front-trimmed log file on disk.
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
//! In parallel, each event is best-effort appended as a line to a
//! single shared file, `~/.cache/mewxi/logs/mewxi-XXXX.log`:
//! `2026-07-09T12:34:56.789Z [origin] [kind] message`. Every mewxi
//! process — including the short-lived `mewxi status` invocations
//! Claude Code's statusline spawns every few seconds — adopts the
//! newest existing log file rather than minting its own on launch
//! (leftovers from older rotating builds are deleted at init). The file
//! never rotates: once its line count hits the cap, the oldest lines
//! are trimmed in place and new lines keep appending at the end — a
//! ring buffer on disk. Disabled silently if the cache dir can't be
//! created, or if `MEWXI_LOG=0` is set.
//!
//! The cap defaults to `DEFAULT_MAX_LINES_PER_FILE` (10,000) but is
//! configurable via `log_max_lines` in
//! `~/.config/mewxi/accounts.toml` (floored at
//! `MIN_MAX_LINES_PER_FILE` = 100 so a typo can't turn every log line
//! into a trim). Every log event cheaply re-checks accounts.toml's
//! mtime and re-reads the cap when it moves (the same pattern as the
//! usage tuning cache in `live_usage`), so a cap edit — from the TUI's
//! Config row or by hand — applies to every running process without a
//! restart.
//!
//! Because several processes may append to the same file concurrently,
//! each process's in-memory `line_count` only sees the lines *it*
//! wrote, not siblings' — trim timing is therefore approximate by
//! design (a shared file may run a bit past the cap before any one
//! process notices and trims, and each trim re-syncs the trimmer's
//! count to the file's real length). This is bounded staleness, never
//! corruption: every write is a single flushed line under `O_APPEND`,
//! so concurrent appends interleave cleanly rather than clobbering
//! each other.
//!
//! On disk this bounds growth to roughly the line cap; in memory it's
//! capped at `RING_CAP` (1000) entries. Logging never panics and never
//! bubbles errors — the point of the logger is to help diagnose other
//! bugs, it must never become a bug source itself.

use chrono::{DateTime, Utc};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// Built-in per-file line cap; override with `log_max_lines` in
/// `accounts.toml`, applied via [`set_max_lines`].
pub const DEFAULT_MAX_LINES_PER_FILE: usize = 10_000;
/// Floor for the configurable cap — a typo'd `log_max_lines = 1` must
/// not turn every log line into a whole-file trim.
pub const MIN_MAX_LINES_PER_FILE: usize = 100;
const RING_CAP: usize = 1000;

/// Configured per-file line cap; 0 means "not set, use the default".
static MAX_LINES_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// accounts.toml mtime as of the last time the cap was refreshed from
/// config; 0 = never checked, `u64::MAX` = config missing — the same
/// sentinel scheme as `live_usage`'s tuning cache.
static CONFIG_STAMP: AtomicU64 = AtomicU64::new(0);

/// Re-read `log_max_lines` whenever accounts.toml's mtime moves, so a
/// cap edit made by any process (the TUI's Config row, a hand edit)
/// reaches every running process on its next log event — the same
/// mtime-stamp pattern as `live_usage::tuning()`. One `stat` per event,
/// no read+parse in the steady state.
///
/// Two ordering constraints keep this deadlock- and recursion-free:
/// it runs BEFORE `log_event` takes the logger mutex (the accounts
/// getters must never end up logging while that mutex is held), and the
/// stamp is stored before the config is read, so an accidental nested
/// `log_event` sees a matching stamp instead of recursing.
fn refresh_max_lines_from_config() {
    let stamp = match crate::accounts::config_mtime() {
        Some(t) => t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        None => u64::MAX,
    };
    if CONFIG_STAMP.load(Ordering::Relaxed) == stamp {
        return;
    }
    CONFIG_STAMP.store(stamp, Ordering::Relaxed);
    set_max_lines(crate::accounts::log_max_lines_setting());
}

/// Apply the configurable per-file line cap. `None` restores the
/// built-in default. Kept public for the TUI's Config row, which calls
/// it right after persisting: [`refresh_max_lines_from_config`] already
/// covers cross-process pickup, but mtime has only 1-second resolution,
/// so a same-process write-then-log pair inside the same second could
/// miss the stamp check — this is the belt to that braces.
pub fn set_max_lines(lines: Option<u64>) {
    let v = lines
        .map(|l| (l as usize).max(MIN_MAX_LINES_PER_FILE))
        .unwrap_or(0);
    MAX_LINES_OVERRIDE.store(v, Ordering::Relaxed);
}

fn max_lines_per_file() -> usize {
    match MAX_LINES_OVERRIDE.load(Ordering::Relaxed) {
        0 => DEFAULT_MAX_LINES_PER_FILE,
        n => n,
    }
}

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
    path: PathBuf,
    file: Option<BufWriter<File>>,
    line_count: usize,
}

static LOGGER: OnceLock<Option<Mutex<Logger>>> = OnceLock::new();
static RING: OnceLock<Mutex<VecDeque<LogEntry>>> = OnceLock::new();
static RING_VERSION: AtomicU64 = AtomicU64::new(0);

fn enabled_via_env() -> bool {
    std::env::var("MEWXI_LOG").ok().as_deref() != Some("0")
}

/// Count newline bytes in `path` without loading assumptions about
/// line endings beyond `\n` — best-effort, `None` if the file is
/// missing or unreadable (caller treats that the same as "start
/// fresh").
fn count_lines(path: &std::path::Path) -> Option<usize> {
    let bytes = fs::read(path).ok()?;
    Some(bytes.iter().filter(|&&b| b == b'\n').count())
}

fn init_logger() -> Option<Mutex<Logger>> {
    if !enabled_via_env() {
        return None;
    }
    let dir = dirs::cache_dir()?.join("mewxi").join("logs");
    fs::create_dir_all(&dir).ok()?;
    // Adopt the newest existing file — left by a prior run, an earlier
    // rotating build, or a concurrently-running sibling process — so
    // every process shares one log instead of minting its own per
    // launch (short-lived `mewxi status` invocations used to churn the
    // dir with thousands of near-empty files). An empty dir starts at
    // `mewxi-0001.log`.
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
        .unwrap_or(1);
    let path = dir.join(format!("mewxi-{max_idx:04}.log"));
    let line_count = count_lines(&path).unwrap_or(0);
    prune_legacy(&dir, &path);
    Some(Mutex::new(Logger {
        path,
        file: None,
        line_count,
    }))
}

/// The single front-trimmed file is the whole on-disk story now, but
/// earlier rotating builds left up to `20` numbered siblings behind (and
/// their per-process churn could leave thousands). Best-effort delete of
/// every `mewxi-*.log` other than the adopted one, once per init.
fn prune_legacy(dir: &std::path::Path, keep: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let is_log = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("mewxi-") && n.ends_with(".log"));
        if is_log && p != *keep {
            let _ = fs::remove_file(&p);
        }
    }
}

fn logger() -> Option<&'static Mutex<Logger>> {
    LOGGER.get_or_init(init_logger).as_ref()
}

fn ring() -> &'static Mutex<VecDeque<LogEntry>> {
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAP)))
}

impl Logger {
    /// Write an already-formatted line (timestamp + tags + message)
    /// verbatim, front-trimming the file first if it's at the cap.
    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        if self.file.is_none() {
            // `append`, never `write + truncate`: several processes
            // share this file, and O_APPEND makes each flushed line
            // land at the current end instead of clobbering siblings'.
            let f = OpenOptions::new().create(true).append(true).open(&self.path)?;
            self.file = Some(BufWriter::new(f));
        }
        if self.line_count >= max_lines_per_file() {
            self.trim();
        }
        let writer = self.file.as_mut().expect("file opened above");
        writeln!(writer, "{line}")?;
        writer.flush()?;
        self.line_count += 1;
        Ok(())
    }

    /// Drop the file's oldest lines in place so the newest ~90% of the
    /// cap remain, then keep appending — the file behaves as a ring, no
    /// rotation. Trimming to 90% rather than cap-1 amortizes the
    /// read+rewrite over ~cap/10 appends instead of running per line.
    ///
    /// Best-effort: on any error the file and count are left as-is, so
    /// the next write simply retries. The in-place `fs::write` keeps
    /// the inode, so our own and siblings' O_APPEND handles stay valid
    /// and append at the new end; a sibling line landing inside the
    /// trim window can end up out of order but never torn, since every
    /// append is a single flushed write.
    fn trim(&mut self) {
        let cap = max_lines_per_file();
        let Ok(bytes) = fs::read(&self.path) else { return };
        let total = bytes.iter().filter(|&&b| b == b'\n').count();
        if total < cap {
            // Our per-process count drifted past the file's real length
            // — a sibling already trimmed, or the file was removed and
            // recreated. Adopt the real count and skip the rewrite.
            self.line_count = total;
            return;
        }
        let keep = cap - (cap / 10).max(1);
        let start = tail_offset(&bytes, total - keep);
        if fs::write(&self.path, &bytes[start..]).is_ok() {
            self.line_count = keep;
        }
    }
}

/// Byte offset where the content after the first `drop_lines` lines of
/// `bytes` starts — `bytes.len()` when asked to drop more lines than
/// exist. Pure so the trim boundary is unit-testable.
fn tail_offset(bytes: &[u8], drop_lines: usize) -> usize {
    if drop_lines == 0 {
        return 0;
    }
    let mut seen = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            seen += 1;
            if seen == drop_lines {
                return i + 1;
            }
        }
    }
    bytes.len()
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
/// even when file logging is disabled) and appended to the shared
/// front-trimmed file (best-effort).
pub fn log_event(origin: LogOrigin, kind: LogKind, message: &str) {
    refresh_max_lines_from_config();
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

    /// Unique path under `std::env::temp_dir()` for a single test, so
    /// concurrent test runs don't collide.
    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mewxi-debug-log-test-{tag}-{:?}-{}.log",
            std::thread::current().id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ))
    }

    #[test]
    fn count_lines_counts_newlines_in_a_written_file() {
        let path = temp_path("count-n");
        fs::write(&path, "line1\nline2\nline3\n").unwrap();
        assert_eq!(count_lines(&path), Some(3));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn count_lines_of_empty_file_is_zero() {
        let path = temp_path("count-empty");
        fs::write(&path, "").unwrap();
        assert_eq!(count_lines(&path), Some(0));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn count_lines_missing_file_is_none() {
        let path = temp_path("count-missing");
        // Deliberately never created.
        assert_eq!(count_lines(&path), None);
    }

    #[test]
    fn tail_offset_drops_requested_lines() {
        let bytes = b"a\nbb\nccc\ndddd\n";
        assert_eq!(tail_offset(bytes, 0), 0);
        assert_eq!(tail_offset(bytes, 1), 2);
        assert_eq!(tail_offset(bytes, 2), 5);
        assert_eq!(&bytes[tail_offset(bytes, 3)..], b"dddd\n");
        assert_eq!(tail_offset(bytes, 4), bytes.len());
        // Asking to drop more lines than exist empties the file rather
        // than panicking or wrapping.
        assert_eq!(tail_offset(bytes, 9), bytes.len());
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
