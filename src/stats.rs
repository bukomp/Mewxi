//! Parse, cache, and aggregate Claude Code assistant-message usage from
//! the per-account JSONL transcripts under each `CLAUDE_CONFIG_DIR`.
//!
//! The public surface is small:
//!
//! - [`scan_all`] — walk every JSONL under a given root, parse
//!   assistant messages into [`UsageRecord`]s, dedup by `message_id`,
//!   return chronological.
//! - [`aggregate`] — fold a slice of records into an [`Aggregate`] with
//!   all-time / period totals, per-model / per-project / per-day
//!   breakdowns, plus the rolling 5-hour block.
//! - [`load_and_aggregate_for`] — one-shot `scan_all` + `aggregate`
//!   scoped to a single [`Account`].
//! - [`overage_cost_usd`] — USD cost of tokens in a 5h block that
//!   exceed a caller-supplied cap.
//! - [`current_context_from_transcript`] / [`context_cap_for`] —
//!   per-session context size and cap detection for the `ctx` segment.
//! - [`parse_file_cached`] — exposed for [`crate::live_session`]; reuses
//!   the per-account cache transparently.
//!
//! Pricing comes from [`crate::pricing`], which refreshes daily from
//! LiteLLM's public model_prices JSON and falls back to baked-in rates
//! when offline. Per-file parse
//! results are cached on disk keyed on `(mtime, size)` under
//! `$XDG_CACHE_HOME/mewxi/files-<slug>.json`, one file per
//! account so concurrent watchers don't stomp on each other.
//!
//! Claude Code transcripts are append-only in practice, so the in-memory
//! parse caches behind [`parse_file_cached`] and
//! [`current_context_from_transcript`] are incremental: when a watched
//! file grows, only the newly appended bytes are read and parsed, not
//! the whole (potentially tens-of-MB) file. A short "guard" — the last
//! few bytes before the previously-consumed offset — is re-checked on
//! every growth to detect an in-place rewrite or truncation; a mismatch
//! forces a full reparse from byte 0, so correctness never depends on
//! the append-only assumption holding.

use crate::accounts::Account;
use crate::debug_log::{LogKind, LogOrigin};
use anyhow::Result;
use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};
use walkdir::WalkDir;

use crate::pricing::price_for;

/// A single assistant message's token usage, extracted from a JSONL session file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageRecord {
    pub timestamp: DateTime<Utc>,
    pub session_id: String,
    pub project: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cost_usd: f64,
    pub message_id: String,
    /// True when the record was produced by a sub-agent (Task tool,
    /// plan-mode helper, etc.) rather than the main agent. Tokens
    /// still count toward the session totals — the user pays for
    /// sub-agent work — but the displayed model should ignore these
    /// records so a one-off Sonnet helper doesn't stick the badge to
    /// Sonnet for the rest of a Haiku session.
    #[serde(default)]
    pub is_sidechain: bool,
}

impl UsageRecord {
    pub fn total_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct UsageTotals {
    pub messages: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cost_usd: f64,
}

impl UsageTotals {
    pub fn add(&mut self, r: &UsageRecord) {
        self.messages += 1;
        self.input += r.input;
        self.output += r.output;
        self.cache_read += r.cache_read;
        self.cache_write_5m += r.cache_write_5m;
        self.cache_write_1h += r.cache_write_1h;
        self.cost_usd += r.cost_usd;
    }
    pub fn total_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write_5m + self.cache_write_1h
    }
}

#[derive(Default, Clone, Debug, Serialize)]
pub struct Aggregate {
    pub all: UsageTotals,
    pub today: UsageTotals,
    pub this_week: UsageTotals,
    pub this_month: UsageTotals,
    pub by_model: BTreeMap<String, UsageTotals>,
    pub by_project: BTreeMap<String, UsageTotals>,
    pub by_day: BTreeMap<NaiveDate, UsageTotals>,
    pub recent: Vec<UsageRecord>,
    pub sessions_count: usize,
    pub projects_count: usize,
    /// Rolling 5-hour window: messages whose timestamp is within the last 5h
    /// AND that belong to the current "session block" (no >5h gap preceding them).
    pub rolling_5h: UsageTotals,
    /// Earliest message in the current 5h window (the block start).
    pub five_h_window_start: Option<DateTime<Utc>>,
    /// Five hours after window start — when Claude's rolling limit resets.
    pub five_h_resets_at: Option<DateTime<Utc>>,
    /// Chronologically ordered records in the current 5h block (oldest first).
    /// Used to compute overage cost against a configurable cap.
    pub five_h_records: Vec<UsageRecord>,
    /// Nominal API-rate cost of all records in the trailing 7 days
    /// (now - 7d, rolling — approximates the API's weekly window).
    pub trailing_7d_cost_usd: f64,
    /// Per-session slice of `trailing_7d_cost_usd`, keyed by session_id.
    pub trailing_7d_cost_by_session: HashMap<String, f64>,
}

fn floor_to_hour(ts: DateTime<Utc>) -> DateTime<Utc> {
    let naive = ts.naive_utc();
    let floored = naive.date().and_hms_opt(naive.hour(), 0, 0).unwrap_or(naive);
    DateTime::from_naive_utc_and_offset(floored, Utc)
}

/// Per-account on-disk file-cache path: `files-<slug>.json` under the
/// shared `mewxi` cache dir. Keeping caches per-account means
/// concurrent watchers (one per account) don't clobber each other.
pub fn cache_path_for(account: &Account) -> Option<PathBuf> {
    dirs::cache_dir()
        .map(|c| c.join("mewxi").join(format!("files3-{}.json", account.slug())))
}

/// Per-account on-disk statusLine output: `status-<slug>.txt`.
pub fn status_cache_path_for(account: &Account) -> Option<PathBuf> {
    dirs::cache_dir()
        .map(|c| c.join("mewxi").join(format!("status-{}.txt", account.slug())))
}

/// Single-file mirror of the most-recently-modified account, kept so
/// existing statusLine hooks pointed at `status.txt` continue to work.
pub fn status_cache_path_mirror() -> Option<PathBuf> {
    dirs::cache_dir().map(|c| c.join("mewxi").join("status.txt"))
}

#[derive(Serialize, Deserialize, Default)]
struct FileCache {
    files: HashMap<PathBuf, FileEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct FileEntry {
    mtime_unix: u64,
    size: u64,
    records: Vec<UsageRecord>,
}

fn load_file_cache(path: &Path) -> FileCache {
    fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_file_cache(path: &Path, cache: &FileCache) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match serde_json::to_vec(cache) {
        Ok(bytes) => {
            let tmp = path.with_extension("json.tmp");
            match fs::write(&tmp, &bytes).and_then(|_| fs::rename(&tmp, path)) {
                Ok(()) => {
                    crate::debug_log::log_event(
                        LogOrigin::Sessions,
                        LogKind::FileWrite,
                        &format!("wrote stats cache · {} files", cache.files.len()),
                    );
                }
                Err(e) => {
                    crate::debug_log::log_event(
                        LogOrigin::Sessions,
                        LogKind::Error,
                        &format!("cache write failed — {e}"),
                    );
                }
            }
        }
        Err(e) => {
            crate::debug_log::log_event(
                LogOrigin::Sessions,
                LogKind::Error,
                &format!("cache serialize failed — {e}"),
            );
        }
    }
}

/// Bytes of guard kept before an offset to detect an in-place rewrite or
/// truncation on the next incremental read (see [`read_tail`]).
const GUARD_LEN: u64 = 64;

/// Outcome of attempting to read only the bytes appended to a grown file.
enum TailRead {
    /// Verified append: the guard bytes immediately before `offset` on
    /// disk still match what was recorded last time, so everything from
    /// `offset` onward is genuinely new. `text` holds exactly the newly
    /// *complete* lines (terminated by `\n`); a trailing partial line (the
    /// writer mid-flush) is held back so it isn't parsed until it
    /// completes. `new_offset`/`new_guard` are ready to store for the
    /// next call.
    Append { text: String, new_offset: u64, new_guard: Vec<u8> },
    /// The guard didn't match (or the read was otherwise too short) —
    /// the file was rewritten or truncated in place rather than appended
    /// to. Caller must fall back to a full reparse.
    Reparse,
}

/// Read only the bytes appended to `path` since `offset`, verifying that
/// the `guard` bytes (the up-to-[`GUARD_LEN`] bytes immediately before
/// `offset` as of the last read) are still present on disk immediately
/// before `offset`. A mismatch means the file was rewritten/truncated in
/// place rather than purely appended to, and the caller must reparse from
/// scratch — this is what keeps the incremental path correct even though
/// append-only is only the *common* case, not a guarantee.
fn read_tail(path: &Path, offset: u64, guard: &[u8]) -> std::io::Result<TailRead> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let read_from = offset.saturating_sub(guard.len() as u64);
    f.seek(SeekFrom::Start(read_from))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    let gl = (offset - read_from) as usize;
    if buf.len() < gl || &buf[..gl] != guard {
        return Ok(TailRead::Reparse);
    }

    let tail = &buf[gl..];
    match tail.iter().rposition(|&b| b == b'\n') {
        None => {
            // No newly-completed line yet; nothing to parse, offset/guard unchanged.
            Ok(TailRead::Append { text: String::new(), new_offset: offset, new_guard: guard.to_vec() })
        }
        Some(last_nl) => {
            let complete_end = gl + last_nl + 1;
            let text = String::from_utf8_lossy(&tail[..=last_nl]).into_owned();
            let new_offset = offset + last_nl as u64 + 1;
            let guard_start = complete_end.saturating_sub(GUARD_LEN.min(new_offset) as usize);
            let new_guard = buf[guard_start..complete_end].to_vec();
            Ok(TailRead::Append { text, new_offset, new_guard })
        }
    }
}

/// Byte offset just past the last complete line, and the guard bytes
/// leading up to it, for a freshly fully-parsed file's content. Shared by
/// every full-reparse path so the incremental offset/guard bookkeeping is
/// computed the same way everywhere.
fn offset_and_guard_for(content: &str) -> (u64, Vec<u8>) {
    let offset = content.rfind('\n').map(|i| i as u64 + 1).unwrap_or(0);
    let bytes = content.as_bytes();
    let start = (offset as usize).saturating_sub(GUARD_LEN as usize);
    let guard = bytes[start..offset as usize].to_vec();
    (offset, guard)
}

/// One file's cached parse state: the records plus enough bookkeeping to
/// extend them incrementally on the next call. `tail_trusted` is false
/// only for entries seeded from the on-disk [`FileCache`] (see
/// [`scan_all`]) where `offset`/`guard` are assumed (offset = size, empty
/// guard) rather than read from disk — the first growth after seeding
/// must do one full reparse to establish a byte-accurate offset/guard
/// before incremental reads are safe.
struct ParsedEntry {
    mtime: u64,
    size: u64,
    offset: u64,
    guard: Vec<u8>,
    tail_trusted: bool,
    records: Vec<UsageRecord>,
}

/// Process-wide cache of per-file parse results, populated as
/// [`scan_all`] runs. Lets [`parse_file_cached`] (used by
/// [`crate::live_session::scan`]) return the freshest records without
/// re-reading the file on every UI tick. Entries carry a byte
/// offset/guard so a grown file can be extended incrementally instead of
/// re-read in full (see module docs).
fn parsed_cache() -> &'static Mutex<HashMap<PathBuf, ParsedEntry>> {
    static S: OnceLock<Mutex<HashMap<PathBuf, ParsedEntry>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Parse a transcript file, reusing the process-wide cache when possible.
/// Public so the live-session detector can share work with [`scan_all`].
///
/// Three paths, cheapest first:
///  - `(mtime, size)` unchanged → clone cached records, no I/O.
///  - File grew and the cached offset/guard are trustworthy → read and
///    parse only the newly appended complete lines, append to the cached
///    records. A guard mismatch (rewrite/truncation in place) falls back
///    to the next case.
///  - Otherwise (new file, shrank, or an untrusted seeded offset) → full
///    reparse.
pub fn parse_file_cached(path: &Path) -> Result<Vec<UsageRecord>> {
    let meta = fs::metadata(path)?;
    let mtime = mtime_unix(&meta);
    let size = meta.len();
    let key = path.to_path_buf();

    if let Ok(g) = parsed_cache().lock() {
        if let Some(e) = g.get(&key) {
            if e.mtime == mtime && e.size == size {
                return Ok(e.records.clone());
            }
        }
    }

    let grown_trusted = {
        let g = parsed_cache().lock().ok();
        g.and_then(|g| {
            g.get(&key).and_then(|e| {
                if size > e.size && e.tail_trusted {
                    Some((e.offset, e.guard.clone(), e.records.clone()))
                } else {
                    None
                }
            })
        })
    };

    if let Some((offset, guard, mut records)) = grown_trusted {
        match read_tail(path, offset, &guard) {
            Ok(TailRead::Append { text, new_offset, new_guard }) => {
                let project = project_name_from_path(path);
                for line in text.lines() {
                    if let Some(r) = parse_record_line(line, &project) {
                        records.push(r);
                    }
                }
                if let Ok(mut g) = parsed_cache().lock() {
                    g.insert(
                        key,
                        ParsedEntry {
                            mtime,
                            size,
                            offset: new_offset,
                            guard: new_guard,
                            tail_trusted: true,
                            records: records.clone(),
                        },
                    );
                }
                return Ok(records);
            }
            Ok(TailRead::Reparse) | Err(_) => {
                // Fall through to full reparse below.
            }
        }
    }

    // Full reparse: read the file once, deriving both the records and the
    // byte offset/guard that future growth extends incrementally. This is
    // the only path that reads the whole file (new file, shrink, rewrite,
    // or an untrusted seeded offset); every subsequent append reads only
    // the newly appended tail.
    let content = fs::read_to_string(path).unwrap_or_default();
    let project = project_name_from_path(path);
    let recs: Vec<UsageRecord> =
        content.lines().filter_map(|l| parse_record_line(l, &project)).collect();
    let (offset, guard) = offset_and_guard_for(&content);
    if let Ok(mut g) = parsed_cache().lock() {
        g.insert(
            key,
            ParsedEntry { mtime, size, offset, guard, tail_trusted: true, records: recs.clone() },
        );
    }
    Ok(recs)
}

fn mtime_unix(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Walk every JSONL under `root` and return all usage records.
/// Deduplicates by message_id so resumed/forked sessions don't
/// double-count. `cache_path` is the per-account on-disk cache the
/// caller provides (see [`cache_path_for`]); `None` disables caching.
///
/// Uses the cache keyed on (mtime, size): unchanged files skip
/// re-parsing, so repeat scans on a large history are near-instant.
pub fn scan_all(root: &Path, cache_path: Option<&Path>) -> Result<Vec<UsageRecord>> {
    let start = Instant::now();
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if !root.exists() {
        return Ok(out);
    }

    let mut cache = cache_path
        .map(load_file_cache)
        .unwrap_or_default();
    let mut next: HashMap<PathBuf, FileEntry> = HashMap::new();
    let mut changed = false;
    let mut files_scanned = 0usize;
    let mut files_parsed = 0usize;

    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        files_scanned += 1;
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = mtime_unix(&meta);
        let size = meta.len();
        let path_buf = path.to_path_buf();

        let entry_records = match cache.files.remove(&path_buf) {
            Some(e) if e.mtime_unix == mtime && e.size == size => e.records,
            _ => {
                changed = true;
                files_parsed += 1;
                // Route through the incremental in-memory cache rather
                // than a raw full parse — an actively-written file that
                // already has a byte-accurate offset/guard entry only
                // pays for the appended tail here, not the whole file.
                match parse_file_cached(path) {
                    Ok(recs) => recs,
                    Err(e) => {
                        crate::debug_log::log_event(
                            LogOrigin::Sessions,
                            LogKind::Error,
                            &format!("{} parse failed — {e}", file_label(path)),
                        );
                        Vec::new()
                    }
                }
            }
        };
        next.insert(path_buf, FileEntry { mtime_unix: mtime, size, records: entry_records });
    }

    if !cache.files.is_empty() {
        // Some files were deleted since last run.
        changed = true;
    }

    // Iterate files in a stable order so that when the same message_id appears
    // in multiple JSONL files (session resumes/forks across project dirs), the
    // record we keep — and therefore its project attribution — is deterministic
    // run to run. HashMap iteration is randomized and would otherwise cause
    // per-project cost totals to flap between scans.
    let mut ordered: Vec<(&PathBuf, &FileEntry)> = next.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));
    // Populate the process-wide parsed cache so live_session::scan can
    // skip re-reading these files this tick. Only seed files whose
    // in-memory entry is missing or stale — a changed file already got a
    // byte-accurate, tail_trusted entry from parse_file_cached above, and
    // clobbering it here would erase its offset/guard and force the next
    // growth to do a full reparse instead of an incremental one. An
    // unchanged (cache-hit) file has no in-memory entry yet; seed it with
    // tail_trusted:false since we don't know its true on-disk offset —
    // the first future growth reparses fully to establish one, and every
    // growth after that is incremental.
    if let Ok(mut g) = parsed_cache().lock() {
        for (p, fe) in &ordered {
            let stale = match g.get(*p) {
                Some(existing) => existing.mtime != fe.mtime_unix || existing.size != fe.size,
                None => true,
            };
            if stale {
                g.insert(
                    (*p).clone(),
                    ParsedEntry {
                        mtime: fe.mtime_unix,
                        size: fe.size,
                        offset: fe.size,
                        guard: Vec::new(),
                        tail_trusted: false,
                        records: fe.records.clone(),
                    },
                );
            }
        }
    }
    for (_, fe) in ordered {
        for r in &fe.records {
            if seen.insert(r.message_id.clone()) {
                out.push(r.clone());
            }
        }
    }
    out.sort_by_key(|r| r.timestamp);

    if changed {
        if let Some(p) = cache_path {
            save_file_cache(p, &FileCache { files: next });
        }
        // Only log when the scan actually did work (a file was parsed,
        // deleted, or is new) — a pure cache-hit rescan (the common case
        // once history has been scanned once) stays silent so the hot
        // per-tick reload path in the TUI doesn't flood the log ring.
        crate::debug_log::log_event(
            LogOrigin::Sessions,
            LogKind::FileRead,
            &format!(
                "parsed {files_parsed}/{files_scanned} files · {} records · {}",
                out.len(),
                fmt_dur(start.elapsed())
            ),
        );
    }
    Ok(out)
}

/// Format an elapsed duration for a log line: `142ms` under a second,
/// `1.2s` at or above.
fn fmt_dur(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// Filename (not full path) for log messages — keeps scan/parse log
/// lines short and avoids spelling out the full account/project tree.
fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}

/// Parse one JSONL line into a [`UsageRecord`], or `None` if the line
/// isn't a priced assistant message (wrong `type`, missing usage, or all
/// token counts are zero — e.g. a stop event). Shared by the full-file
/// parse ([`parse_file`]) and the incremental tail parse in
/// [`parse_file_cached`] so there is exactly one implementation of the
/// per-line extraction logic.
fn parse_record_line(line: &str, project: &str) -> Option<UsageRecord> {
    if line.is_empty() {
        return None;
    }
    let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;

    let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let output = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cache_read = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);

    // cache_creation has ephemeral_5m / ephemeral_1h; fall back to cache_creation_input_tokens as 5m
    let (cw5, cw1h) = if let Some(cc) = usage.get("cache_creation") {
        (
            cc.get("ephemeral_5m_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            cc.get("ephemeral_1h_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        )
    } else {
        let c = usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
        (c, 0)
    };

    // Skip rows with no tokens at all (stop events)
    if input + output + cache_read + cw5 + cw1h == 0 {
        return None;
    }

    let model = msg.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
    let timestamp = v
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let session_id = v.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let message_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let message_id = if message_id.is_empty() {
        // Fallback: uuid from outer envelope
        v.get("uuid").and_then(|v| v.as_str()).unwrap_or("").to_string()
    } else {
        message_id
    };
    let is_sidechain = v.get("isSidechain").and_then(|v| v.as_bool()).unwrap_or(false);

    let p = price_for(&model);
    let cost_usd = (input as f64 * p.input
        + output as f64 * p.output
        + cache_read as f64 * p.cache_read
        + cw5 as f64 * p.cache_write_5m
        + cw1h as f64 * p.cache_write_1h)
        / 1_000_000.0;

    Some(UsageRecord {
        timestamp,
        session_id,
        project: project.to_string(),
        model,
        input,
        output,
        cache_read,
        cache_write_5m: cw5,
        cache_write_1h: cw1h,
        cost_usd,
        message_id,
        is_sidechain,
    })
}

/// Full-file parse into records. `parse_file_cached` inlines this same
/// `parse_record_line` fold on its full-reparse path (it needs the raw
/// content anyway, to derive the incremental offset/guard) so this is now
/// only a cold-parse reference for the tests.
#[cfg(test)]
fn parse_file(path: &Path) -> Result<Vec<UsageRecord>> {
    let content = std::fs::read_to_string(path)?;
    let project = project_name_from_path(path);
    Ok(content.lines().filter_map(|l| parse_record_line(l, &project)).collect())
}

fn project_name_from_path(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(decode_project_slug)
        .unwrap_or_else(|| "unknown".to_string())
}

/// Claude Code encodes project paths by replacing '/' with '-'.
/// We can't fully reverse it (dashes in real project names collide with
/// the path separator), so just show the last segment — the basename of
/// the cwd, which is what users think of as the project name.
fn decode_project_slug(slug: &str) -> String {
    let s = slug.trim_start_matches('-');
    s.rsplit('-').next().unwrap_or(s).to_string()
}

/// Fold a chronologically-sorted slice of records into the
/// summary shape rendered by the TUI and MCP layer.
///
/// Computes:
/// - `all` / `today` / `this_week` / `this_month` totals (by local
///   calendar day / ISO week / calendar month).
/// - `by_model`, `by_project`, `by_day` breakdowns (BTreeMap for
///   stable iteration).
/// - `recent` — last 20 records, newest first.
/// - `rolling_5h` + `five_h_records` + `five_h_window_start` +
///   `five_h_resets_at` — the current 5-hour block (see module
///   docs for the matching rules).
pub fn aggregate(records: &[UsageRecord]) -> Aggregate {
    let now = Local::now();
    let today = now.date_naive();
    let iso_week = today.iso_week();
    let (year, month) = (today.year(), today.month());

    let mut agg = Aggregate::default();
    let mut sessions: HashSet<String> = HashSet::new();
    let mut projects: HashSet<String> = HashSet::new();
    let mut by_model: HashMap<String, UsageTotals> = HashMap::new();
    let mut by_project: HashMap<String, UsageTotals> = HashMap::new();
    let mut by_day: HashMap<NaiveDate, UsageTotals> = HashMap::new();
    let trailing_7d_cutoff = Utc::now() - chrono::Duration::days(7);

    for r in records {
        agg.all.add(r);
        sessions.insert(r.session_id.clone());
        projects.insert(r.project.clone());

        let local_date = r.timestamp.with_timezone(&Local).date_naive();
        if local_date == today {
            agg.today.add(r);
        }
        let r_iso = local_date.iso_week();
        if r_iso.year() == iso_week.year() && r_iso.week() == iso_week.week() {
            agg.this_week.add(r);
        }
        if local_date.year() == year && local_date.month() == month {
            agg.this_month.add(r);
        }
        if r.timestamp >= trailing_7d_cutoff {
            agg.trailing_7d_cost_usd += r.cost_usd;
            *agg.trailing_7d_cost_by_session.entry(r.session_id.clone()).or_insert(0.0) += r.cost_usd;
        }
        by_model.entry(r.model.clone()).or_default().add(r);
        by_project.entry(r.project.clone()).or_default().add(r);
        by_day.entry(local_date).or_default().add(r);
    }

    agg.by_model = by_model.into_iter().collect();
    agg.by_project = by_project.into_iter().collect();
    agg.by_day = by_day.into_iter().collect();
    agg.sessions_count = sessions.len();
    agg.projects_count = projects.len();

    // last 20 records, newest first
    agg.recent = records.iter().rev().take(20).cloned().collect();

    // Rolling 5h session block, matching Anthropic's own accounting:
    //  - A block starts at the clock hour of its oldest message (timestamp floored to the hour).
    //  - The block lasts exactly 5 hours from that hour. Messages whose timestamp falls
    //    within [block_start, block_start + 5h) count toward this block.
    //  - A gap of ≥5h between messages ends the block; the next message starts a new one.
    //  - When a block's 5h ends mid-activity, subsequent messages start a new block at
    //    floor_to_hour(their own timestamp) — so walking back we must also stop once an
    //    older message's floor_to_hour would push block_start such that the newest message
    //    no longer fits inside [block_start, block_start + 5h). Without this check,
    //    continuous-activity histories spanning >5h collapse block_start to a past hour,
    //    defeating the post-loop "current block" test and zeroing the whole window.
    //  - A block is "current" only if now < block_start + 5h.
    let now_utc = Utc::now();
    let five_h = chrono::Duration::hours(5);
    let newest_ts = records.last().map(|r| r.timestamp);
    let mut block_start: Option<DateTime<Utc>> = None;
    let mut prev_ts: Option<DateTime<Utc>> = None;
    let mut block_records: Vec<UsageRecord> = Vec::new();
    for r in records.iter().rev() {
        if let Some(prev) = prev_ts {
            if prev - r.timestamp >= five_h {
                break;
            }
        }
        let candidate_start = floor_to_hour(r.timestamp);
        if let Some(n) = newest_ts {
            if candidate_start + five_h <= n {
                break;
            }
        }
        block_start = Some(candidate_start);
        prev_ts = Some(r.timestamp);
        agg.rolling_5h.add(r);
        block_records.push(r.clone());
    }
    if let Some(start) = block_start {
        if now_utc >= start + five_h {
            agg.rolling_5h = UsageTotals::default();
            block_start = None;
            block_records.clear();
        }
    }
    block_records.reverse(); // chronological
    agg.five_h_records = block_records;
    agg.five_h_window_start = block_start;
    agg.five_h_resets_at = block_start.map(|s| s + five_h);

    agg
}

/// One-shot scan + aggregate, also returning the raw deduped
/// chronological records for callers that need per-record attribution
/// (see `limit_attr::session_prices`).
pub fn load_records_and_aggregate_for(account: &Account) -> Result<(Vec<UsageRecord>, Aggregate)> {
    let root = account.projects_dir();
    let cache = cache_path_for(account);
    let records = scan_all(&root, cache.as_deref())?;
    let agg = aggregate(&records);
    Ok((records, agg))
}

pub fn load_and_aggregate_for(account: &Account) -> Result<Aggregate> {
    Ok(load_records_and_aggregate_for(account)?.1)
}

pub struct SessionContext {
    pub current: u64,
    pub max_observed: u64,
    pub model: String,
}

/// `(ctx_tokens, model)` for a non-sidechain assistant message whose
/// context size is nonzero, or `None` if the line doesn't qualify
/// (wrong type, sidechain, missing usage, or ctx == 0). Mirrors
/// [`parse_record_line`]'s shape — one implementation shared by the
/// full-file scan and the incremental tail scan in
/// [`current_context_from_transcript`].
fn ctx_from_line(line: &str) -> Option<(u64, String)> {
    let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    // Sub-agents (Task tool, plan-mode helpers) use their own
    // model + context window. Skip them so the cap stays anchored
    // to the main agent — otherwise a one-off Sonnet helper would
    // briefly inflate the displayed cap to Sonnet's 1M.
    if v.get("isSidechain").and_then(|x| x.as_bool()).unwrap_or(false) {
        return None;
    }
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;
    let input = usage.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let cread = usage.get("cache_read_input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let (c5, c1h) = if let Some(cc) = usage.get("cache_creation") {
        (
            cc.get("ephemeral_5m_input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
            cc.get("ephemeral_1h_input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
        )
    } else {
        (usage.get("cache_creation_input_tokens").and_then(|x| x.as_u64()).unwrap_or(0), 0)
    };
    let ctx = input + cread + c5 + c1h;
    if ctx == 0 {
        return None;
    }
    let model = msg.get("model").and_then(|x| x.as_str()).unwrap_or("unknown").to_string();
    Some((ctx, model))
}

/// One transcript's cached context-tracking state, mirroring
/// [`ParsedEntry`]'s offset/guard incremental scheme but tracking the
/// rolling `(max_observed, current)` fold instead of a record list —
/// see [`current_context_from_transcript`].
struct CtxEntry {
    mtime: u64,
    size: u64,
    offset: u64,
    guard: Vec<u8>,
    tail_trusted: bool,
    max_observed: u64,
    current: Option<(u64, String)>,
}

/// Process-wide cache for [`current_context_from_transcript`], separate
/// from [`parsed_cache`] since it tracks a different fold (rolling
/// max/current context, not a record list) over the same files.
fn ctx_cache() -> &'static Mutex<HashMap<PathBuf, CtxEntry>> {
    static S: OnceLock<Mutex<HashMap<PathBuf, CtxEntry>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Parse a transcript and return:
///  - current context size (last non-sidechain assistant message's
///    input + cache tokens)
///  - max context size ever observed in this session (to detect 1M tier)
///  - model id of the most recent non-sidechain assistant message
///
/// Incremental like [`parse_file_cached`]: an append only re-folds the
/// newly-completed lines into the cached `(max_observed, current)` state;
/// a guard mismatch (rewrite/truncation) forces a full reparse.
pub fn current_context_from_transcript(path: &Path) -> Option<SessionContext> {
    let meta = fs::metadata(path).ok()?;
    let mtime = mtime_unix(&meta);
    let size = meta.len();
    let key = path.to_path_buf();

    if let Ok(g) = ctx_cache().lock() {
        if let Some(e) = g.get(&key) {
            if e.mtime == mtime && e.size == size {
                let (cur, model) = e.current.clone()?;
                return Some(SessionContext { current: cur, max_observed: e.max_observed, model });
            }
        }
    }

    let grown_trusted = {
        let g = ctx_cache().lock().ok();
        g.and_then(|g| {
            g.get(&key).and_then(|e| {
                if size > e.size && e.tail_trusted {
                    Some((e.offset, e.guard.clone(), e.max_observed, e.current.clone()))
                } else {
                    None
                }
            })
        })
    };

    if let Some((offset, guard, mut max_observed, mut current)) = grown_trusted {
        if let Ok(TailRead::Append { text, new_offset, new_guard }) = read_tail(path, offset, &guard) {
            for line in text.lines() {
                if let Some((ctx, model)) = ctx_from_line(line) {
                    max_observed = max_observed.max(ctx);
                    current = Some((ctx, model));
                }
            }
            if let Ok(mut g) = ctx_cache().lock() {
                g.insert(
                    key,
                    CtxEntry {
                        mtime,
                        size,
                        offset: new_offset,
                        guard: new_guard,
                        tail_trusted: true,
                        max_observed,
                        current: current.clone(),
                    },
                );
            }
            let (cur, model) = current?;
            return Some(SessionContext { current: cur, max_observed, model });
        }
        // Guard mismatch or I/O error — fall through to full reparse.
    }

    let content = std::fs::read_to_string(path).ok()?;
    let mut max_observed: u64 = 0;
    let mut current: Option<(u64, String)> = None;
    for line in content.lines() {
        if let Some((ctx, model)) = ctx_from_line(line) {
            max_observed = max_observed.max(ctx);
            current = Some((ctx, model));
        }
    }
    let (offset, guard) = offset_and_guard_for(&content);
    if let Ok(mut g) = ctx_cache().lock() {
        g.insert(
            key,
            CtxEntry { mtime, size, offset, guard, tail_trusted: true, max_observed, current: current.clone() },
        );
    }
    let (cur, model) = current?;
    Some(SessionContext { current: cur, max_observed, model })
}

/// Read `"model"` from an account's settings and return true if it
/// requests an extended-context variant (e.g. `opus[1m]`, `sonnet[1m]`).
pub fn extended_context_from_settings(account: &Account) -> bool {
    for path in account.settings_paths() {
        let Ok(s) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else { continue };
        let model_str: Option<String> = match v.get("model") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Object(o)) => o.get("id").and_then(|x| x.as_str()).map(String::from),
            _ => None,
        };
        if let Some(s) = model_str {
            if s.contains("[1m]") {
                return true;
            }
        }
    }
    false
}

/// True for model versions whose native context window is already 1M,
/// which makes an explicit `[1M]` tier badge redundant. Accepts either an
/// api id (`claude-opus-4-8`) or a display name (`Opus 4.8 (1M context)`)
/// — version digits are read regardless of the separator.
///
/// Looks the exact version up in the LiteLLM price/context table (see
/// [`crate::pricing::context_window_for`]) instead of hard-coding which
/// family/version crossed the 1M threshold — that table refreshes on its
/// own, so a newly released model gets the right answer here without a
/// mewxi code change. An unrecognized family or a version not yet in the
/// table conservatively reports not-native (200K).
pub fn native_1m_context(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let family = if lower.contains("fable") {
        "fable"
    } else if lower.contains("opus") {
        "opus"
    } else if lower.contains("sonnet") {
        "sonnet"
    } else if lower.contains("haiku") {
        "haiku"
    } else {
        return false;
    };
    let (major, minor) = leading_version(&lower);
    crate::pricing::context_window_for(family, major, minor).is_some_and(|ctx| ctx > 200_000)
}

/// First two digit runs of a string as `(major, minor)` — tolerant of any
/// separator so it reads `claude-opus-4-8`, `Opus 4.8 (…)`, and
/// `opus-4-8[1m]` alike. Missing components default to 0.
fn leading_version(lower: &str) -> (u32, u32) {
    let mut nums = lower
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse::<u32>().ok());
    (nums.next().unwrap_or(0), nums.next().unwrap_or(0))
}

/// Decide a model's context cap. The heuristics, in order of confidence:
///  1. The model's native context window is already 1M ([`native_1m_context`]) → 1M
///  2. stdin alias from Claude Code containing `[1m]` → 1M
///  3. A prior statusline call for this session saw `[1m]` (marker file) → 1M
///  4. Any message in this session had >200K context → 1M
///  5. The account's `settings.json` model is `…[1m]` → 1M
///  6. Otherwise 200K (default for all current Claude models)
///
/// Natively-1M models (e.g. Fable 5) never get a `[1m]` alias suffix from
/// Claude Code, so without heuristic 1 they'd show a 200K cap until a
/// single message exceeded 200K tokens.
///
/// `session_id` is optional and used to consult the persisted [1m] marker
/// written by the statusline. Without it the TUI can't tell that the user
/// is on [1m] until a single message exceeds 200K tokens, which leaves
/// the ctx column showing wildly inflated percentages until then.
pub fn context_cap_for(
    api_model: &str,
    max_observed: u64,
    stdin_alias: Option<&str>,
    account: &Account,
    session_id: Option<&str>,
) -> u64 {
    let one_m = native_1m_context(api_model)
        || stdin_alias.is_some_and(|s| s.contains("[1m]"))
        || session_id.is_some_and(|sid| extended_context_marked(account, sid))
        || max_observed > 200_000
        || extended_context_from_settings(account);
    if one_m { 1_000_000 } else { 200_000 }
}

fn extended_context_marker_path(account: &Account, session_id: &str) -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|c| {
        c.join("mewxi")
            .join("ext-ctx")
            .join(format!("{}-{}.flag", account.slug(), session_id))
    })
}

/// Record that this session has been seen using `[1m]` (1M context tier).
/// Idempotent — writing an existing file is a no-op for our purposes.
pub fn mark_extended_context(account: &Account, session_id: &str) {
    let Some(p) = extended_context_marker_path(account, session_id) else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, b"");
}

/// True iff [`mark_extended_context`] was previously called for this session.
pub fn extended_context_marked(account: &Account, session_id: &str) -> bool {
    extended_context_marker_path(account, session_id)
        .is_some_and(|p| p.exists())
}

fn session_effort_path(account: &Account, session_id: &str) -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|c| {
        c.join("mewxi")
            .join("effort")
            .join(format!("{}-{}.txt", account.slug(), session_id))
    })
}

/// Record the reasoning effort Claude Code reported for this session via
/// the statusline payload. The TUI never sees that stdin payload, so
/// without this its all-sessions table falls back to the account-global
/// `effortLevel` and shows every session the same level. Overwritten on
/// each `mewxi status` refresh so it tracks the live value.
pub fn mark_session_effort(account: &Account, session_id: &str, effort: &str) {
    let Some(p) = session_effort_path(account, session_id) else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, effort.as_bytes());
}

/// The last per-session effort recorded by [`mark_session_effort`], if any.
pub fn session_effort(account: &Account, session_id: &str) -> Option<String> {
    let p = session_effort_path(account, session_id)?;
    let raw = std::fs::read_to_string(&p).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Given a chronological list of 5h-block records and the plan's token cap,
/// return the USD cost of tokens that exceeded the cap. The message that
/// crosses the threshold is billed proportionally by the fraction of its
/// tokens that landed beyond the cap.
pub fn overage_cost_usd(block_records: &[UsageRecord], cap_tokens: u64) -> f64 {
    let mut cum: u64 = 0;
    let mut extra: f64 = 0.0;
    for r in block_records {
        let before = cum;
        let r_tokens = r.total_tokens();
        let after = before.saturating_add(r_tokens);
        if after <= cap_tokens {
            cum = after;
            continue;
        }
        if before >= cap_tokens {
            extra += r.cost_usd;
        } else {
            let over = after - cap_tokens;
            let frac = over as f64 / r_tokens.max(1) as f64;
            extra += r.cost_usd * frac;
        }
        cum = after;
    }
    extra
}

/// Format a token count with thousands separators.
pub fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod native_1m_tests {
    use super::native_1m_context;

    // These read the live LiteLLM table (same network dependency as
    // pricing::tests::end_to_end_fetch_and_lookup), so the exact set of
    // "native 1M" versions is whatever LiteLLM currently reports — that's
    // the point: nothing here is a version threshold we maintain by hand.

    #[test]
    fn versions_litellm_reports_over_200k_are_native_1m() {
        assert!(native_1m_context("claude-fable-5"));
        assert!(native_1m_context("Fable 5 (1M context)"));
        assert!(native_1m_context("claude-opus-4-8[1m]"));
        assert!(native_1m_context("Opus 4.8 (1M context)"));
    }

    #[test]
    fn versions_still_on_200k_default_are_not() {
        assert!(!native_1m_context("claude-opus-4-5"));
        assert!(!native_1m_context("claude-sonnet-4-5"));
        assert!(!native_1m_context("claude-haiku-4-5-20251001"));
    }

    #[test]
    fn version_not_yet_in_the_table_defaults_to_not_native() {
        assert!(!native_1m_context("claude-opus-99-9"));
    }
}

#[cfg(test)]
mod context_cap_tests {
    use super::context_cap_for;
    use crate::accounts::{Account, TokenSource};

    fn account(dir: &std::path::Path) -> Account {
        Account {
            name: "test".into(),
            dir: dir.to_path_buf(),
            token_source: TokenSource::Auto,
        }
    }

    #[test]
    fn natively_1m_model_gets_1m_cap_without_any_1m_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let cap = context_cap_for("claude-fable-5", 49_000, Some("Fable 5"), &account(tmp.path()), None);
        assert_eq!(cap, 1_000_000);
    }

    #[test]
    fn model_on_200k_default_keeps_200k_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let cap = context_cap_for("claude-opus-4-5", 49_000, Some("Opus 4.5"), &account(tmp.path()), None);
        assert_eq!(cap, 200_000);
    }
}

#[cfg(test)]
mod trailing_tests {
    use super::*;

    fn record(session_id: &str, message_id: &str, cost_usd: f64, timestamp: DateTime<Utc>) -> UsageRecord {
        UsageRecord {
            timestamp,
            session_id: session_id.to_string(),
            project: "proj".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            input: 100,
            output: 50,
            cache_read: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cost_usd,
            message_id: message_id.to_string(),
            is_sidechain: false,
        }
    }

    #[test]
    fn trailing_7d_cost_by_session() {
        let now = Utc::now();
        let one_day_ago = now - chrono::Duration::days(1);
        let ten_days_ago = now - chrono::Duration::days(10);

        let records = vec![
            record("session-a", "m1", 1.5, now),
            record("session-a", "m2", 2.5, one_day_ago),
            record("session-b", "m3", 4.0, one_day_ago),
            // Outside the trailing 7d window, but may or may not fall in the
            // current calendar month depending on when the test runs.
            record("session-b", "m4", 9.0, ten_days_ago),
        ];

        let agg = aggregate(&records);

        // trailing_7d_cost_usd sums only records within the last 7 days,
        // excluding the 10-days-ago record.
        assert!((agg.trailing_7d_cost_usd - (1.5 + 2.5 + 4.0)).abs() < 1e-9);

        // Per-session split of the trailing window.
        assert!((agg.trailing_7d_cost_by_session["session-a"] - 4.0).abs() < 1e-9);
        assert!((agg.trailing_7d_cost_by_session["session-b"] - 4.0).abs() < 1e-9);
        assert!(!agg.trailing_7d_cost_by_session.contains_key("nonexistent-session"));
    }
}

#[cfg(test)]
mod incremental_tests {
    use super::*;
    use std::fs;
    use std::fs::OpenOptions;
    use std::io::Write;

    /// One assistant-message JSONL line with real (nonzero) token counts,
    /// so the resulting record isn't priced/zeroed out. `cache_read` and
    /// `input` are caller-controlled so tests can build predictable
    /// context-token sums; `ephemeral_5m` is fixed at 10 for simplicity.
    fn assistant_line(
        id: &str,
        session: &str,
        ts: &str,
        model: &str,
        input: u64,
        cache_read: u64,
        sidechain: bool,
    ) -> String {
        let side = if sidechain { r#","isSidechain":true"# } else { "" };
        format!(
            r#"{{"type":"assistant","sessionId":"{session}","timestamp":"{ts}"{side},"message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{input},"output_tokens":50,"cache_read_input_tokens":{cache_read},"cache_creation":{{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":0}}}}}}}}"#
        )
    }

    #[test]
    fn append_only_growth_matches_full_reparse() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t1.jsonl");

        let lines: Vec<String> = (0..5)
            .map(|i| assistant_line(&format!("m{i}"), "s1", "2026-07-21T10:00:00Z", "claude-sonnet-4-5", 1000 + i, 2000, false))
            .collect();
        fs::write(&path, lines.join("\n") + "\n").unwrap();

        let a = parse_file_cached(&path).unwrap();
        assert_eq!(a.len(), 5);

        let more: Vec<String> = (5..8)
            .map(|i| assistant_line(&format!("m{i}"), "s1", "2026-07-21T10:05:00Z", "claude-opus-4-5", 1000 + i, 2000, false))
            .collect();
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            for l in &more {
                writeln!(f, "{l}").unwrap();
            }
        }

        let b = parse_file_cached(&path).unwrap();
        assert_eq!(b.len(), 8);

        // Cold full parse of the final bytes, from a brand-new path so the
        // in-memory cache can't short-circuit it.
        let cold_path = tmp.path().join("t1_cold.jsonl");
        fs::copy(&path, &cold_path).unwrap();
        let cold = parse_file(&cold_path).unwrap();

        assert_eq!(b.len(), cold.len());
        for (x, y) in b.iter().zip(cold.iter()) {
            assert_eq!(x.message_id, y.message_id);
            assert_eq!(x.model, y.model);
            assert_eq!(x.total_tokens(), y.total_tokens());
        }
        assert_eq!(
            b.iter().map(|r| r.message_id.clone()).collect::<Vec<_>>(),
            (0..8).map(|i| format!("m{i}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn partial_trailing_line_not_consumed_until_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t2.jsonl");

        let complete = assistant_line("m0", "s1", "2026-07-21T10:00:00Z", "claude-sonnet-4-5", 1000, 2000, false);
        let full_line1 = assistant_line("m1", "s1", "2026-07-21T10:01:00Z", "claude-sonnet-4-5", 1500, 2000, false);
        let split_at = full_line1.len() / 2;
        let (frag_prefix, frag_rest) = full_line1.split_at(split_at);

        // Complete line, then a trailing fragment with NO newline.
        fs::write(&path, format!("{complete}\n{frag_prefix}")).unwrap();

        let a = parse_file_cached(&path).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].message_id, "m0");

        // Complete the fragment, add a newline, then one more full line.
        let next_line = assistant_line("m2", "s1", "2026-07-21T10:02:00Z", "claude-sonnet-4-5", 1200, 2000, false);
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            write!(f, "{frag_rest}\n{next_line}\n").unwrap();
        }

        let b = parse_file_cached(&path).unwrap();
        assert_eq!(
            b.iter().map(|r| r.message_id.clone()).collect::<Vec<_>>(),
            vec!["m0".to_string(), "m1".to_string(), "m2".to_string()]
        );

        let cold_path = tmp.path().join("t2_cold.jsonl");
        fs::copy(&path, &cold_path).unwrap();
        let cold = parse_file(&cold_path).unwrap();
        assert_eq!(b.len(), cold.len());
        for (x, y) in b.iter().zip(cold.iter()) {
            assert_eq!(x.message_id, y.message_id);
            assert_eq!(x.total_tokens(), y.total_tokens());
        }
    }

    #[test]
    fn rewrite_or_truncation_forces_full_reparse() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t3.jsonl");

        let lines: Vec<String> = (0..4)
            .map(|i| assistant_line(&format!("m{i}"), "s1", "2026-07-21T10:00:00Z", "claude-sonnet-4-5", 1000 + i, 2000, false))
            .collect();
        fs::write(&path, lines.join("\n") + "\n").unwrap();

        let a = parse_file_cached(&path).unwrap();
        assert_eq!(a.len(), 4);

        // Rewrite in place with different, shorter content — size shrinks,
        // so the incremental path must be discarded in favor of a full
        // reparse rather than treating this as a truncated-but-consistent
        // append.
        let new_line = assistant_line("z0", "s2", "2026-07-21T11:00:00Z", "claude-opus-4-5", 500, 100, false);
        fs::write(&path, format!("{new_line}\n")).unwrap();

        let b = parse_file_cached(&path).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].message_id, "z0");

        let cold_path = tmp.path().join("t3_cold.jsonl");
        fs::copy(&path, &cold_path).unwrap();
        let cold = parse_file(&cold_path).unwrap();
        assert_eq!(b.len(), cold.len());
        assert_eq!(b[0].message_id, cold[0].message_id);
        assert_eq!(b[0].total_tokens(), cold[0].total_tokens());
    }

    #[test]
    fn grown_but_rewritten_tail_guard_mismatch_forces_full_reparse() {
        // A file that grows in total size but whose *earlier* bytes were
        // also rewritten (not a pure append) must still be caught: the
        // guard bytes immediately before the old offset won't match, so
        // read_tail must report Reparse rather than silently splicing
        // stale + new content together.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t3b.jsonl");

        let original: Vec<String> = (0..3)
            .map(|i| assistant_line(&format!("orig{i}"), "s1", "2026-07-21T10:00:00Z", "claude-sonnet-4-5", 1000 + i, 2000, false))
            .collect();
        fs::write(&path, original.join("\n") + "\n").unwrap();
        let a = parse_file_cached(&path).unwrap();
        assert_eq!(a.len(), 3);

        // Replace wholesale with a longer, entirely different set of lines.
        let replaced: Vec<String> = (0..6)
            .map(|i| assistant_line(&format!("new{i}"), "s2", "2026-07-21T12:00:00Z", "claude-opus-4-5", 2000 + i, 3000, false))
            .collect();
        fs::write(&path, replaced.join("\n") + "\n").unwrap();

        let b = parse_file_cached(&path).unwrap();
        let cold_path = tmp.path().join("t3b_cold.jsonl");
        fs::copy(&path, &cold_path).unwrap();
        let cold = parse_file(&cold_path).unwrap();

        assert_eq!(b.len(), cold.len());
        for (x, y) in b.iter().zip(cold.iter()) {
            assert_eq!(x.message_id, y.message_id);
        }
        assert!(b.iter().all(|r| r.message_id.starts_with("new")));
    }

    #[test]
    fn context_tracking_incremental_matches_cold_reparse() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t4.jsonl");

        // Early high-context line (simulates approaching the 1M cap).
        let l0 = assistant_line("m0", "s1", "2026-07-21T09:00:00Z", "claude-opus-4-5", 900_000, 0, false);
        // Later, lower-context lines.
        let l1 = assistant_line("m1", "s1", "2026-07-21T09:05:00Z", "claude-sonnet-4-5", 1000, 500, false);
        // Sidechain line with a huge ctx that must be ignored entirely.
        let l2 = assistant_line("side0", "s1", "2026-07-21T09:06:00Z", "claude-haiku-4-5", 999_000, 0, true);
        fs::write(&path, format!("{l0}\n{l1}\n{l2}\n")).unwrap();

        let ctx1 = current_context_from_transcript(&path).unwrap();
        assert_eq!(ctx1.max_observed, 900_010); // 900_000 input + 10 ephemeral_5m
        assert_eq!(ctx1.model, "claude-sonnet-4-5"); // last non-sidechain line

        // Append more lower-context lines, plus another sidechain line.
        let l3 = assistant_line("m3", "s1", "2026-07-21T09:10:00Z", "claude-sonnet-4-5", 2000, 500, false);
        let side1 = assistant_line("side1", "s1", "2026-07-21T09:10:30Z", "claude-haiku-4-5", 500_000, 0, true);
        let l4 = assistant_line("m4", "s1", "2026-07-21T09:11:00Z", "claude-opus-4-5", 3000, 500, false);
        {
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "{l3}").unwrap();
            writeln!(f, "{side1}").unwrap();
            writeln!(f, "{l4}").unwrap();
        }

        let ctx2 = current_context_from_transcript(&path).unwrap();

        let cold_path = tmp.path().join("t4_cold.jsonl");
        fs::copy(&path, &cold_path).unwrap();
        let cold = current_context_from_transcript(&cold_path).unwrap();

        assert_eq!(ctx2.current, cold.current);
        assert_eq!(ctx2.max_observed, cold.max_observed);
        assert_eq!(ctx2.model, cold.model);

        // The historical high from m0 is retained even though later
        // messages have much smaller context…
        assert_eq!(ctx2.max_observed, 900_010);
        // …while `current` tracks the last non-sidechain line (m4), and
        // the sidechain lines never influence either field.
        assert_eq!(ctx2.model, "claude-opus-4-5");
        assert_eq!(ctx2.current, 3000 + 500 + 10);
    }
}
