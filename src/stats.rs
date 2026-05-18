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
//! `$XDG_CACHE_HOME/muxi/files-<slug>.json`, one file per
//! account so concurrent watchers don't stomp on each other.

use crate::accounts::Account;
use anyhow::Result;
use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;
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
}

fn floor_to_hour(ts: DateTime<Utc>) -> DateTime<Utc> {
    let naive = ts.naive_utc();
    let floored = naive.date().and_hms_opt(naive.hour(), 0, 0).unwrap_or(naive);
    DateTime::from_naive_utc_and_offset(floored, Utc)
}

/// Per-account on-disk file-cache path: `files-<slug>.json` under the
/// shared `muxi` cache dir. Keeping caches per-account means
/// concurrent watchers (one per account) don't clobber each other.
pub fn cache_path_for(account: &Account) -> Option<PathBuf> {
    dirs::cache_dir()
        .map(|c| c.join("muxi").join(format!("files2-{}.json", account.slug())))
}

/// Per-account on-disk statusLine output: `status-<slug>.txt`.
pub fn status_cache_path_for(account: &Account) -> Option<PathBuf> {
    dirs::cache_dir()
        .map(|c| c.join("muxi").join(format!("status-{}.txt", account.slug())))
}

/// Single-file mirror of the most-recently-modified account, kept so
/// existing statusLine hooks pointed at `status.txt` continue to work.
pub fn status_cache_path_mirror() -> Option<PathBuf> {
    dirs::cache_dir().map(|c| c.join("muxi").join("status.txt"))
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
    if let Ok(bytes) = serde_json::to_vec(cache) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, &bytes).is_ok() {
            let _ = fs::rename(&tmp, path);
        }
    }
}

/// Process-wide cache of per-file parse results, populated as
/// [`scan_all`] runs. Lets [`parse_file_cached`] (used by
/// [`crate::live_session::scan`]) return the freshest records without
/// re-reading the file on every UI tick.
fn parsed_cache() -> &'static Mutex<HashMap<PathBuf, (u64, u64, Vec<UsageRecord>)>> {
    static S: OnceLock<Mutex<HashMap<PathBuf, (u64, u64, Vec<UsageRecord>)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Parse a transcript file, reusing the process-wide cache when the
/// file's `(mtime, size)` matches the cached entry. Public so the
/// live-session detector can share work with [`scan_all`].
pub fn parse_file_cached(path: &Path) -> Result<Vec<UsageRecord>> {
    let meta = fs::metadata(path)?;
    let mtime = mtime_unix(&meta);
    let size = meta.len();
    let key = path.to_path_buf();
    if let Ok(g) = parsed_cache().lock() {
        if let Some((m, s, recs)) = g.get(&key) {
            if *m == mtime && *s == size {
                return Ok(recs.clone());
            }
        }
    }
    let recs = parse_file(path).unwrap_or_default();
    if let Ok(mut g) = parsed_cache().lock() {
        g.insert(key, (mtime, size, recs.clone()));
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

    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = mtime_unix(&meta);
        let size = meta.len();
        let path_buf = path.to_path_buf();

        let entry_records = match cache.files.remove(&path_buf) {
            Some(e) if e.mtime_unix == mtime && e.size == size => e.records,
            _ => {
                changed = true;
                parse_file(path).unwrap_or_default()
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
    // skip re-reading these files this tick.
    if let Ok(mut g) = parsed_cache().lock() {
        for (p, fe) in &ordered {
            g.insert((*p).clone(), (fe.mtime_unix, fe.size, fe.records.clone()));
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
    }
    Ok(out)
}

fn parse_file(path: &Path) -> Result<Vec<UsageRecord>> {
    let content = std::fs::read_to_string(path)?;
    let project = project_name_from_path(path);
    let mut out = Vec::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                continue;
            }
            let Some(msg) = v.get("message") else { continue };
            let Some(usage) = msg.get("usage") else { continue };

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
                continue;
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

            let p = price_for(&model);
            let cost_usd = (input as f64 * p.input
                + output as f64 * p.output
                + cache_read as f64 * p.cache_read
                + cw5 as f64 * p.cache_write_5m
                + cw1h as f64 * p.cache_write_1h)
                / 1_000_000.0;

            out.push(UsageRecord {
                timestamp,
                session_id,
                project: project.clone(),
                model,
                input,
                output,
                cache_read,
                cache_write_5m: cw5,
                cache_write_1h: cw1h,
                cost_usd,
                message_id,
            });
        }
    }
    Ok(out)
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

pub fn load_and_aggregate_for(account: &Account) -> Result<Aggregate> {
    let root = account.projects_dir();
    let cache = cache_path_for(account);
    let records = scan_all(&root, cache.as_deref())?;
    Ok(aggregate(&records))
}

pub struct SessionContext {
    pub current: u64,
    pub max_observed: u64,
    pub model: String,
}

/// Parse a transcript and return:
///  - current context size (last assistant message's input + cache tokens)
///  - max context size ever observed in this session (to detect 1M tier)
///  - model id of the most recent assistant message
pub fn current_context_from_transcript(path: &Path) -> Option<SessionContext> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut current: Option<(u64, String)> = None;
    let mut max_observed: u64 = 0;
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        let Some(usage) = msg.get("usage") else { continue };
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
            continue;
        }
        if ctx > max_observed {
            max_observed = ctx;
        }
        let model = msg.get("model").and_then(|x| x.as_str()).unwrap_or("unknown").to_string();
        current = Some((ctx, model));
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

/// Decide a model's context cap. The heuristics, in order of confidence:
///  1. stdin alias from Claude Code containing `[1m]` → 1M
///  2. A prior statusline call for this session saw `[1m]` (marker file) → 1M
///  3. Any message in this session had >200K context → 1M
///  4. The account's `settings.json` model is `…[1m]` → 1M
///  5. Otherwise 200K (default for all current Claude models)
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
    let _ = api_model;
    let one_m = stdin_alias.is_some_and(|s| s.contains("[1m]"))
        || session_id.is_some_and(|sid| extended_context_marked(account, sid))
        || max_observed > 200_000
        || extended_context_from_settings(account);
    if one_m { 1_000_000 } else { 200_000 }
}

fn extended_context_marker_path(account: &Account, session_id: &str) -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|c| {
        c.join("muxi")
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
