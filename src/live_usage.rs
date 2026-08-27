//! Fetch real-time usage from Claude Code's internal OAuth endpoint
//! (the same source that powers the in-CLI `/usage` command and status bar).
//!
//! This endpoint is undocumented. Schema below is captured from observed
//! responses and a public gist; the `anthropic-beta` header pins the
//! protocol version so a schema change will surface as a 4xx instead of
//! silently returning a different shape.

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::accounts::Account;
use crate::auth;

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA_HEADER: &str = "oauth-2025-04-20";
/// Mirror Claude Code's own UA so the server treats us the same way it
/// treats the real CLI. This *must* track Claude Code's current format —
/// the endpoint has been observed to 403 ("Request not allowed") requests
/// whose UA doesn't look like a genuine Claude Code client. The version
/// number is the one currently installed on the author's machine; only the
/// `claude-cli/X.Y.Z (external, cli)` shape appears to matter.
const USER_AGENT: &str = "claude-cli/2.1.116 (external, cli)";

/// Built-in default for the minimum age of a cached response before an
/// on-demand refetch. Override with `live_refresh_interval_secs` in
/// `~/.config/mewxi/accounts.toml` (see [`refresh_interval`]).
///
/// 120s, not 60: the endpoint sits behind a Cloudflare rate-limit rule
/// that, observed over a morning of logs, sustains roughly one request
/// per two minutes per account (with a burst allowance of ~15 after an
/// idle spell). Polling at 60s produced a steady ok/ok/429 cycle; at
/// 120s the account stays under the limit indefinitely. Claude Code
/// itself also calls this endpoint with the same token, so leave
/// headroom rather than sitting exactly on the line.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(120);
/// Built-in default for the post-429 backoff. Override with
/// `live_backoff_secs` in `accounts.toml` (see [`backoff_after_429`]).
pub const DEFAULT_BACKOFF_AFTER_429: Duration = Duration::from_secs(120);

/// Floor for both tunables — a typo'd `live_refresh_interval_secs = 1`
/// must not turn the per-keypress statusline into an HTTP hammer.
const MIN_TUNABLE_SECS: u64 = 10;

/// Minimum age of a cached response before we'll refetch on demand.
/// `live_refresh_interval_secs` in `accounts.toml`; defaults to
/// [`DEFAULT_REFRESH_INTERVAL`]. Cached per process but invalidated
/// whenever `accounts.toml`'s mtime changes (see [`tuning`]) — the
/// statusline spawns fresh per invocation so edits apply immediately
/// there, and a long-lived process such as `mewxi watch` or the TUI now
/// picks up an edited value on its next poll, without needing a restart.
pub fn refresh_interval() -> Duration {
    tuning().0
}

/// After a 429 (or 401/403), wait at least this long before trying
/// again. `live_backoff_secs` in `accounts.toml`; defaults to
/// [`DEFAULT_BACKOFF_AFTER_429`].
pub fn backoff_after_429() -> Duration {
    tuning().1
}

fn tunable_duration(configured: Option<u64>, default: Duration) -> Duration {
    Duration::from_secs(configured.unwrap_or(default.as_secs()).max(MIN_TUNABLE_SECS))
}

// Cached tunables, in seconds; 0 = not read yet (MIN_TUNABLE_SECS keeps
// real values from ever being 0). `CONFIG_STAMP` records accounts.toml's
// mtime (see `config_stamp`) as of the last time these were populated.
// `tuning()` re-stats the config on every call and re-reads it whenever
// the live stamp no longer matches, so a long-running process (e.g.
// `mewxi watch`) picks up a config edit made by *another* process —
// including the TUI's Config view — on its next poll, without a
// restart. `reload_tuning()` still exists for same-process immediacy:
// see its doc for why the stamp check alone isn't quite enough there.
static TUNED_REFRESH_SECS: AtomicU64 = AtomicU64::new(0);
static TUNED_BACKOFF_SECS: AtomicU64 = AtomicU64::new(0);
static CONFIG_STAMP: AtomicU64 = AtomicU64::new(0);

/// Sentinel stamp for "accounts.toml doesn't exist or its mtime couldn't
/// be read". Kept distinct from any real mtime-derived stamp (which for
/// a sane system clock is seconds since 1970 — nowhere near `u64::MAX`)
/// so "no config" is its own stable state rather than aliasing onto an
/// ordinary timestamp.
const MISSING_CONFIG_STAMP: u64 = u64::MAX;

/// Map an optional file mtime to a `u64` that's stable for a given mtime
/// and comparable across calls. Pure (no I/O) so the mapping is
/// unit-testable without touching the real `~/.config/mewxi/accounts.toml`;
/// the I/O lives in [`config_stamp`].
fn stamp_from_mtime(mtime: Option<std::time::SystemTime>) -> u64 {
    match mtime {
        Some(t) => t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        None => MISSING_CONFIG_STAMP,
    }
}

/// Current stamp for `accounts.toml`: one `stat` via
/// [`crate::accounts::config_mtime`], no read+parse. Cheap enough to call
/// on every [`tuning`] invocation — a missing or unreadable config must
/// not degrade into a full config read on every call, and one stat is
/// negligible next to the HTTP calls this whole module exists to pace.
fn config_stamp() -> u64 {
    stamp_from_mtime(crate::accounts::config_mtime())
}

fn tuning() -> (Duration, Duration) {
    let stamp = config_stamp();
    // Acquire pairs with the Release below: a reader that sees a
    // non-zero refresh value is guaranteed to see both the backoff value
    // and the config stamp that were written before it (both stored with
    // Relaxed ordering, ahead of the Release store to
    // `TUNED_REFRESH_SECS`, in the population branch below).
    // A populated cache with a stale stamp means accounts.toml's mtime
    // moved since we cached — someone (the TUI, a hand edit, or another
    // process entirely) changed the config. Fall through and re-read it,
    // just like a fresh process or an explicit `reload_tuning()` call.
    let refresh = TUNED_REFRESH_SECS.load(Ordering::Acquire);
    if refresh != 0 && CONFIG_STAMP.load(Ordering::Relaxed) == stamp {
        return (
            Duration::from_secs(refresh),
            Duration::from_secs(TUNED_BACKOFF_SECS.load(Ordering::Relaxed)),
        );
    }
    let (refresh, backoff) = crate::accounts::live_tuning();
    let refresh = tunable_duration(refresh, DEFAULT_REFRESH_INTERVAL);
    let backoff = tunable_duration(backoff, DEFAULT_BACKOFF_AFTER_429);
    TUNED_BACKOFF_SECS.store(backoff.as_secs(), Ordering::Relaxed);
    CONFIG_STAMP.store(stamp, Ordering::Relaxed);
    TUNED_REFRESH_SECS.store(refresh.as_secs(), Ordering::Release);
    (refresh, backoff)
}

/// Drop the cached tunables so the next read re-parses `accounts.toml`,
/// regardless of what [`config_stamp`] reports. Called by the TUI's
/// Config view right after persisting a new value.
///
/// [`tuning`]'s own mtime check already covers *other* processes — a
/// long-running `mewxi watch` notices the edit on its next poll — but
/// mtime has only 1-second resolution on most filesystems, so a
/// same-process write-then-read pair (exactly what the Config view does)
/// can land inside the same mtime second and be missed by the stamp
/// check alone. This is the belt to that braces.
pub fn reload_tuning() {
    TUNED_REFRESH_SECS.store(0, Ordering::Release);
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowUsage {
    /// Percentage utilization, 0..=100.
    pub utilization: f64,
    /// When this window resets. Absent fields become `None`.
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
}

/// One entry from the `limits` array — a generalized replacement for the
/// old per-model top-level fields (`seven_day_opus`, `seven_day_sonnet`,
/// etc., all of which the endpoint now sends back as `null`). Each model
/// or surface with its own cap shows up here instead, e.g. `kind:
/// "weekly_scoped"` with `scope.model.display_name: "Fable"`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LimitEntry {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub percent: f64,
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub scope: Option<LimitScope>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LimitScope {
    #[serde(default)]
    pub model: Option<LimitModel>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LimitModel {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExtraUsage {
    #[serde(default)]
    pub is_enabled: bool,
    /// Server sends this as a number that may be float (e.g. 5000) — treat as f64
    /// to survive both int and fractional values. Gist docs say "cents".
    #[serde(default)]
    pub monthly_limit: Option<f64>,
    #[serde(default)]
    pub used_credits: Option<f64>,
    #[serde(default)]
    pub utilization: Option<f64>,
    #[serde(default)]
    pub currency: Option<String>,
}

/// Current cache schema. Bump whenever the meaning of a cached value
/// changes in a way old readers can't safely interpret — e.g. the
/// per-CLAUDE_CONFIG_DIR token fix, where v1 caches were written with
/// a single shared token and so contained the wrong account's numbers.
/// `load_cached` returns `None` for any cache file at a lower version.
pub const CACHE_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveUsage {
    #[serde(default)]
    pub five_hour: Option<WindowUsage>,
    #[serde(default)]
    pub seven_day: Option<WindowUsage>,
    #[serde(default)]
    pub extra_usage: Option<ExtraUsage>,
    /// Per-model/per-surface scoped caps (see [`LimitEntry`]).
    #[serde(default)]
    pub limits: Vec<LimitEntry>,
    /// When we fetched this — set by us, not the server.
    pub fetched_at: DateTime<Utc>,
    /// Set when written; absent on caches from pre-`CACHE_SCHEMA_VERSION`
    /// builds, which means they're discarded by `load_cached`.
    #[serde(default)]
    pub cache_schema_version: u32,
}

impl LiveUsage {
    /// Age of this value relative to now, in seconds (clamped >=0).
    pub fn age_seconds(&self) -> i64 {
        (Utc::now() - self.fetched_at).num_seconds().max(0)
    }
    /// "Stale" means the value is older than a full refresh+backoff cycle, i.e.
    /// we should assume a fetch has been attempted and failed (or the poller
    /// hasn't run). A value aged just a few tens of seconds is NOT stale — the
    /// status-line command is invoked per keypress and we serve cached reads
    /// within `refresh_interval()` by design.
    pub fn is_stale(&self) -> bool {
        self.age_seconds()
            > (refresh_interval().as_secs() as i64 + backoff_after_429().as_secs() as i64)
    }

    /// The scoped weekly limit entry for a given model display name
    /// (case-insensitive), e.g. `"Fable"`.
    pub fn model_weekly_limit(&self, model_name: &str) -> Option<&LimitEntry> {
        self.limits.iter().find(|l| {
            l.kind == "weekly_scoped"
                && l.scope
                    .as_ref()
                    .and_then(|s| s.model.as_ref())
                    .and_then(|m| m.display_name.as_deref())
                    .is_some_and(|n| n.eq_ignore_ascii_case(model_name))
        })
    }

    pub fn fable_limit(&self) -> Option<&LimitEntry> {
        self.model_weekly_limit("Fable")
    }
}

#[derive(Debug)]
pub enum FetchError {
    RateLimited,
    Unauthorized,
    /// 403 "Request not allowed" — an edge/permission rejection distinct
    /// from an expired token. Observed causes: a User-Agent that doesn't
    /// match Claude Code's own format, or an account without an active
    /// Claude subscription. Kept as its own variant (rather than falling
    /// into `Other`) so it gets a clean message instead of a raw JSON dump,
    /// and so callers can back off instead of hammering the endpoint.
    Forbidden,
    Other(anyhow::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (429)"),
            FetchError::Unauthorized => write!(f, "unauthorized (401) — token expired or invalid"),
            FetchError::Forbidden => write!(
                f,
                "forbidden (403) — Anthropic rejected the request; open Claude Code once to refresh credentials, or the account may lack an active Claude subscription"
            ),
            FetchError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Request timeout for the usage endpoint. Doubles as the single-flight
/// window: while an attempt marker is younger than this, a concurrent
/// fetch can still be in flight, so siblings serve the cache instead.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Format a millisecond duration the way the logs panel expects:
/// sub-second as `142ms`, otherwise `1.2s`.
fn fmt_dur(ms: u128) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Collapse a response body onto one log line: whitespace squashed,
/// truncated so a surprise HTML error page can't flood the log.
fn summarize_body(body: &str) -> String {
    const MAX: usize = 200;
    let compact: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return "(empty body)".to_string();
    }
    if compact.chars().count() <= MAX {
        return compact;
    }
    let cut: String = compact.chars().take(MAX).collect();
    format!("{cut}…")
}

/// Minutes since `expiry`, if it's already in the past. `None` for a
/// still-valid token or when the credential carries no expiry. Pure so
/// the skip-expired-token rule in [`fetch_or_cached_inner`] is testable
/// without a keychain.
fn expired_minutes_ago(expiry: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<i64> {
    let expiry = expiry?;
    if expiry < now {
        Some((now - expiry).num_minutes().max(0))
    } else {
        None
    }
}

/// Raw HTTP call. Synchronous; blocks up to the request timeout.
pub fn fetch_live(token: &str) -> Result<LiveUsage, FetchError> {
    let start = Instant::now();
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", BETA_HEADER)
        .set("User-Agent", USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .call();
    let dur_ms = start.elapsed().as_millis();

    match resp {
        Ok(r) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Api,
                &format!("limits fetched · {}", fmt_dur(dur_ms)),
            );
            let body = r
                .into_string()
                .map_err(|e| FetchError::Other(anyhow!("read body: {e}")))?;
            let parsed: RawLive = serde_json::from_str(&body).map_err(|e| {
                FetchError::Other(anyhow!("parse response ({e}); body was: {body}"))
            })?;
            Ok(LiveUsage {
                five_hour: parsed.five_hour,
                seven_day: parsed.seven_day,
                extra_usage: parsed.extra_usage,
                limits: parsed.limits,
                fetched_at: Utc::now(),
                cache_schema_version: CACHE_SCHEMA_VERSION,
            })
        }
        Err(ureq::Error::Status(429, r)) => {
            // Keep the server's side of the story. In practice this is a
            // Cloudflare edge block (`server: cloudflare`, `retry-after:
            // 0`, ~30ms round trip, generic `rate_limit_error` body) —
            // the request never reached Anthropic's application — but
            // that's exactly the kind of thing that changes without
            // notice, so record what came back rather than assuming.
            let retry_after = r.header("retry-after").unwrap_or("-").to_string();
            let cf_ray = r.header("cf-ray").unwrap_or("-").to_string();
            let server = r.header("server").unwrap_or("-").to_string();
            let body = r.into_string().unwrap_or_default();
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Error,
                &format!(
                    "429 rate limited · {} · server={server} retry-after={retry_after} cf-ray={cf_ray} · {}",
                    fmt_dur(dur_ms),
                    summarize_body(&body)
                ),
            );
            Err(FetchError::RateLimited)
        }
        Err(ureq::Error::Status(401, _)) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Error,
                &format!("401 unauthorized · {}", fmt_dur(dur_ms)),
            );
            Err(FetchError::Unauthorized)
        }
        Err(ureq::Error::Status(403, _)) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Error,
                &format!("403 forbidden · {}", fmt_dur(dur_ms)),
            );
            Err(FetchError::Forbidden)
        }
        Err(ureq::Error::Status(code, r)) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Error,
                &format!("request failed — http {code} · {}", fmt_dur(dur_ms)),
            );
            let body = r.into_string().unwrap_or_default();
            Err(FetchError::Other(anyhow!("HTTP {code}: {body}")))
        }
        Err(e) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Error,
                &format!("request failed — {e} · {}", fmt_dur(dur_ms)),
            );
            Err(FetchError::Other(anyhow!("transport: {e}")))
        }
    }
}

#[derive(Deserialize)]
struct RawLive {
    #[serde(default)]
    five_hour: Option<WindowUsage>,
    #[serde(default)]
    seven_day: Option<WindowUsage>,
    #[serde(default)]
    extra_usage: Option<ExtraUsage>,
    #[serde(default)]
    limits: Vec<LimitEntry>,
}

pub fn cache_path(account: &Account) -> Option<PathBuf> {
    dirs::cache_dir().map(|c| {
        c.join("mewxi")
            .join(format!("live-{}.json", account.slug()))
    })
}

pub fn load_cached(account: &Account) -> Option<LiveUsage> {
    let p = cache_path(account)?;
    let file_name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let bytes = match fs::read(&p) {
        Ok(b) => b,
        Err(e) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::FileRead,
                &format!("{file_name} unreadable — {e}"),
            );
            return None;
        }
    };
    crate::debug_log::log_event(
        crate::debug_log::LogOrigin::Usage,
        crate::debug_log::LogKind::FileRead,
        &format!("read {file_name}"),
    );
    let parsed: LiveUsage = serde_json::from_slice(&bytes).ok()?;
    // Reject caches from older binaries (which may have been written with
    // the wrong account's token, before per-CLAUDE_CONFIG_DIR keychain
    // discovery existed). They'll be refetched fresh on next call.
    if parsed.cache_schema_version < CACHE_SCHEMA_VERSION {
        return None;
    }
    Some(parsed)
}

pub fn save_cached(account: &Account, u: &LiveUsage) {
    let Some(p) = cache_path(account) else { return };
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file_name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if let Ok(bytes) = serde_json::to_vec(u) {
        let tmp = p.with_extension("json.tmp");
        match fs::write(&tmp, &bytes).and_then(|_| fs::rename(&tmp, &p)) {
            Ok(()) => crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::FileWrite,
                &format!("wrote {file_name}"),
            ),
            Err(e) => crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::FileWrite,
                &format!("{file_name} write failed — {e}"),
            ),
        }
    }
}

// --- Cross-process backoff + single-flight markers ---------------------------
//
// The statusline is a fresh process per Claude Code invocation, so
// in-memory throttle state dies before it can help. Worse, a 429 leaves
// the cache stale, which used to mean every subsequent invocation across
// every concurrent session retried the endpoint immediately — being rate
// limited *increased* our request rate. Both markers therefore live on
// disk, next to the per-account cache file:
//
//  * `live-<slug>.backoff.json` — written on 429/401/403, removed on the
//    next success. While younger than `backoff_after_429()`, fetches
//    short-circuit (429 unconditionally; 401/403 unless `force`).
//  * `live-<slug>.attempt` — touched right before each HTTP call. While
//    younger than `HTTP_TIMEOUT`, sibling processes serve the cache
//    instead of piling on when the cache expires. Best-effort by design:
//    the check-then-touch race window is microseconds wide, versus the
//    seconds-wide synchronized herd it suppresses; a leftover marker
//    from a crashed process ages out after 15s.
//  * `live-<slug>.skipnote` — touched when a "fetch skipped" line is
//    logged. While younger than `refresh_interval()`, further skip lines
//    are suppressed: the TUI poller, the watcher, and every statusline
//    re-exec all probe the same backoff marker many times a second in
//    aggregate, and per-probe logging floods the log for the entire
//    backoff window even though nothing new happened.

/// On-disk shape of `live-<slug>.backoff.json`.
#[derive(Serialize, Deserialize)]
struct BackoffMarker {
    at: DateTime<Utc>,
    /// `"rate_limited"` (429) or `"denied"` (401/403). Unknown values
    /// from a future binary are treated as `rate_limited` — the more
    /// conservative read.
    kind: String,
    /// Human-readable reason recorded at rejection time, surfaced by
    /// short-circuited callers so they can report *why*, not just that
    /// a backoff is in effect. Empty for plain 429s.
    reason: String,
}

/// Why the last fetch was rejected, per the on-disk marker, plus how much
/// longer the backoff window has left.
enum Backoff {
    RateLimited { remaining: Duration },
    Denied { reason: String, remaining: Duration },
}

fn backoff_path(account: &Account) -> Option<PathBuf> {
    cache_path(account).map(|p| p.with_extension("backoff.json"))
}

fn attempt_path(account: &Account) -> Option<PathBuf> {
    cache_path(account).map(|p| p.with_extension("attempt"))
}

fn skip_note_path(account: &Account) -> Option<PathBuf> {
    cache_path(account).map(|p| p.with_extension("skipnote"))
}

/// True when it's time to log another "fetch skipped" line for this
/// account — at most one per `refresh_interval()` across all processes.
/// In-memory throttling can't work here: the statusline is a fresh
/// process per invocation, so the note lives on disk like the other
/// markers. Touches the note when it returns true; the check-then-touch
/// race is as benign as the attempt marker's (worst case one duplicate
/// line). A future mtime (clock skew) reads as "log it" — the safe
/// failure mode is a noisy log, never a permanently silent one.
fn should_log_skip(account: &Account) -> bool {
    let Some(p) = skip_note_path(account) else { return true };
    let noted_recently = fs::metadata(&p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
        .map(|age| age < refresh_interval())
        .unwrap_or(false);
    if noted_recently {
        return false;
    }
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&p, b"");
    true
}

fn write_backoff(account: &Account, kind: &str, reason: &str) {
    let Some(p) = backoff_path(account) else { return };
    write_backoff_marker(&p, kind, reason);
}

fn clear_backoff(account: &Account) {
    // A stale skip note must not swallow the first skip line of the *next*
    // backoff window, so it goes when the marker goes. Silent: unlike the
    // marker, the note carries no state worth an audit trail.
    if let Some(np) = skip_note_path(account) {
        let _ = fs::remove_file(&np);
    }
    if let Some(p) = backoff_path(account) {
        let file_name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match fs::remove_file(&p) {
            Ok(()) => crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::FileWrite,
                &format!("cleared {file_name}"),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::FileWrite,
                &format!("{file_name} clear failed — {e}"),
            ),
        }
    }
}

/// The account's backoff state, if a marker exists and is still within
/// the `backoff_after_429()` window. Unreadable or aged-out markers
/// read as "no backoff" — polling must never wedge on a corrupt file.
fn recent_backoff(account: &Account) -> Option<Backoff> {
    let p = backoff_path(account)?;
    read_backoff_marker(&p, backoff_after_429())
}

fn write_backoff_marker(p: &Path, kind: &str, reason: &str) {
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let marker = BackoffMarker {
        at: Utc::now(),
        kind: kind.to_string(),
        reason: reason.to_string(),
    };
    let file_name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if let Ok(bytes) = serde_json::to_vec(&marker) {
        let tmp = p.with_extension("json.tmp");
        match fs::write(&tmp, &bytes).and_then(|_| fs::rename(&tmp, p)) {
            Ok(()) => crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::FileWrite,
                &format!("wrote {file_name}"),
            ),
            Err(e) => crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::FileWrite,
                &format!("{file_name} write failed — {e}"),
            ),
        }
    }
}

fn read_backoff_marker(p: &Path, max_age: Duration) -> Option<Backoff> {
    // Not logged in either direction: a missing marker is the expected
    // steady state, and while a backoff is active every poller probes the
    // marker several times a second in aggregate — the throttled "fetch
    // skipped" line is the record that a marker was seen.
    let bytes = fs::read(p).ok()?;
    let marker: BackoffMarker = serde_json::from_slice(&bytes).ok()?;
    let delta = chrono::Duration::from_std(max_age).ok()?;
    let elapsed = Utc::now() - marker.at;
    if elapsed >= delta {
        return None;
    }
    let remaining = Duration::from_secs((delta - elapsed).num_seconds().max(0) as u64);
    Some(match marker.kind.as_str() {
        "denied" => Backoff::Denied {
            reason: marker.reason,
            remaining,
        },
        _ => Backoff::RateLimited { remaining },
    })
}

/// True while a sibling process's HTTP attempt may still be in flight.
/// A future mtime (clock skew) reads as *not* attempted — the safe
/// failure mode is one extra fetch, never a blocked poller.
fn fetch_recently_attempted(account: &Account) -> bool {
    let Some(p) = attempt_path(account) else { return false };
    let Ok(meta) = fs::metadata(&p) else { return false };
    let Ok(mtime) = meta.modified() else { return false };
    mtime
        .elapsed()
        .map(|age| age < HTTP_TIMEOUT)
        .unwrap_or(false)
}

fn mark_fetch_attempt(account: &Account) {
    let Some(p) = attempt_path(account) else { return };
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file_name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    match fs::write(&p, b"") {
        Ok(()) => crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Usage,
            crate::debug_log::LogKind::FileWrite,
            &format!("wrote {file_name}"),
        ),
        Err(e) => crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Usage,
            crate::debug_log::LogKind::FileWrite,
            &format!("{file_name} write failed — {e}"),
        ),
    }
}

/// What a fetch attempt actually did. Carries the usage value to display
/// (fresh or a cached fallback) plus enough detail for the TUI to tell the
/// user whether a manual refresh genuinely reached the web.
pub enum FetchOutcome {
    /// Fresh data pulled from the endpoint.
    Fetched(LiveUsage),
    /// A cached value was returned without hitting the network — either it
    /// was still fresh (`fetch_or_cached`) or `no_live` is set.
    Cached(Option<LiveUsage>),
    /// Another process touched the attempt marker within the last
    /// `HTTP_TIMEOUT`, so its fetch may still be on the wire; the cached
    /// value is served instead of doubling up. Applies to `force` too.
    InFlight(Option<LiveUsage>),
    /// The endpoint asked us to back off (429), or a prior 429 backoff is
    /// still in effect. The carried value is the cached fallback, if any.
    RateLimited(Option<LiveUsage>),
    /// The fetch failed (no token, transport error, bad status). Carries the
    /// human-readable reason and the cached fallback, if any.
    Failed {
        reason: String,
        cached: Option<LiveUsage>,
    },
}

impl FetchOutcome {
    /// The usage value to display, regardless of how it was obtained.
    pub fn into_usage(self) -> Option<LiveUsage> {
        match self {
            FetchOutcome::Fetched(u) => Some(u),
            FetchOutcome::Cached(c)
            | FetchOutcome::InFlight(c)
            | FetchOutcome::RateLimited(c)
            | FetchOutcome::Failed { cached: c, .. } => c,
        }
    }
}

/// High-level helper: return cached value if fresh enough; otherwise fetch.
/// If a fetch fails, return the stale cached value (if any) rather than nothing.
///
/// `no_live=true` short-circuits to cache only and never hits the network.
pub fn fetch_or_cached(account: &Account, no_live: bool) -> Option<LiveUsage> {
    fetch_or_cached_inner(account, no_live, false).into_usage()
}

/// Like [`fetch_or_cached`] but bypass the `REFRESH_INTERVAL` freshness
/// short-circuit. Used by the TUI's initial poller bootstrap so a cold
/// open never trusts a poisoned cache from a stale background daemon.
/// Still honored: 429 backoff, the cross-process single-flight marker
/// (a sibling's in-flight fetch is served from cache rather than
/// duplicated), and the local token-expiry check.
pub fn fetch_force(account: &Account, no_live: bool) -> Option<LiveUsage> {
    fetch_or_cached_inner(account, no_live, true).into_usage()
}

/// Like [`fetch_force`] but reports the [`FetchOutcome`] instead of
/// collapsing it to an `Option`. Used by the TUI's manual `r` refresh so it
/// can tell the user whether the web fetch actually succeeded.
pub fn fetch_force_outcome(account: &Account, no_live: bool) -> FetchOutcome {
    fetch_or_cached_inner(account, no_live, true)
}

fn fetch_or_cached_inner(account: &Account, no_live: bool, force: bool) -> FetchOutcome {
    let cached = load_cached(account);

    if no_live {
        return FetchOutcome::Cached(cached);
    }

    let now = Utc::now();

    // On-disk backoff marker, written on 429/401/403 and removed on the
    // next success (see the marker section below). Checked before anything
    // else — including the cache, which after a rejection is stale or
    // absent, precisely when the backoff matters most.
    match recent_backoff(account) {
        // 429 backoff is non-negotiable even with `force` — we don't want
        // to hammer the endpoint after it told us to back off. No HTTP
        // call happens, so `fetch_live`'s own logging never fires (and the
        // original 429 may even have been observed by a different process,
        // e.g. the watcher); the skip line below is the only log trace,
        // throttled because every poller in every process lands here for
        // the whole backoff window. A manual `r` refresh still gets its
        // feedback through the returned `FetchOutcome::RateLimited`.
        Some(Backoff::RateLimited { remaining }) => {
            if should_log_skip(account) {
                crate::debug_log::log_event(
                    crate::debug_log::LogOrigin::Usage,
                    crate::debug_log::LogKind::Error,
                    &format!(
                        "fetch skipped — 429 backoff · {}s left · {}",
                        remaining.as_secs(),
                        account.name
                    ),
                );
            }
            return FetchOutcome::RateLimited(cached);
        }
        // 401/403 won't be reconsidered by the endpoint until something
        // changes locally, so they back off like a 429 — but `force` (the
        // TUI's manual refresh) bypasses, so the user can retry immediately
        // after fixing their credentials.
        Some(Backoff::Denied { reason, remaining }) if !force => {
            let code = if reason.contains("401") {
                "401"
            } else if reason.contains("403") {
                "403"
            } else {
                "denied"
            };
            if should_log_skip(account) {
                crate::debug_log::log_event(
                    crate::debug_log::LogOrigin::Usage,
                    crate::debug_log::LogKind::Error,
                    &format!(
                        "fetch skipped — {code} backoff · {}s left · {}",
                        remaining.as_secs(),
                        account.name
                    ),
                );
            }
            return FetchOutcome::Failed { reason, cached };
        }
        _ => {}
    }

    if !force {
        if let Some(ref c) = cached {
            if let Ok(delta) = chrono::Duration::from_std(refresh_interval()) {
                if now - c.fetched_at < delta {
                    return FetchOutcome::Cached(cached);
                }
            }
        }
    }
    // Single-flight: when the cache expires, every concurrent session's
    // statusline sees "stale" in the same instant. Let the first one
    // fetch; siblings serve the (marginally stale) cache instead of
    // firing a synchronized burst at the endpoint.
    //
    // Deliberately *not* inside `if !force`: `force` exists to bypass
    // the freshness window, not to double up on a request that's already
    // on the wire. The TUI's bootstrap fetch landing two seconds after
    // the watch daemon's regular poll was a reliable way to earn a 429
    // — and with it a two-minute backoff for every process on the box.
    if fetch_recently_attempted(account) {
        return FetchOutcome::InFlight(cached);
    }

    let (token, expiry) = match auth::read_oauth_token_with_expiry(account) {
        Ok(t) => t,
        Err(e) => {
            log_once(
                &account.name,
                format!("mewxi: oauth token unavailable for '{}': {e}", account.name),
            );
            return FetchOutcome::Failed {
                reason: format!("token unavailable: {e}"),
                cached,
            };
        }
    };

    // Never send a token we already know is expired. mewxi only *reads*
    // Claude Code's credential; it can't refresh it, so the request can't
    // succeed — and it does real harm: the origin answers 401, and the
    // Cloudflare edge in front of it treats repeated 401s as abuse and
    // blocks the token outright with 429s. Retrying every backoff window
    // kept that block armed for hours on one account. Record a "denied"
    // marker so the per-keypress statusline doesn't re-read the keychain
    // on every invocation, and say plainly what fixes it. `force` doesn't
    // bypass this: the check is local and the answer won't change until
    // Claude Code writes a new token.
    if let Some(stale_for) = expired_minutes_ago(expiry, now) {
        let reason = format!(
            "local token expired {stale_for}m ago — open Claude Code for '{}' to refresh it (fetch not attempted)",
            account.name
        );
        write_backoff(account, "denied", &reason);
        crate::debug_log::log_event(
            crate::debug_log::LogOrigin::Usage,
            crate::debug_log::LogKind::Error,
            &format!("fetch skipped — {reason}"),
        );
        log_once(
            &account.name,
            format!("mewxi: live fetch skipped for '{}': {reason}", account.name),
        );
        return FetchOutcome::Failed { reason, cached };
    }

    mark_fetch_attempt(account);
    match fetch_live(&token) {
        Ok(fresh) => {
            save_cached(account, &fresh);
            clear_backoff(account);
            clear_error_entry(&account.name);
            // Record any increase in pay-per-use "extra usage" credits
            // into the on-disk delta ledger, tagged with this poll's
            // observation interval, so pricing can causally attribute the
            // spend to the sessions active during it. `cached` is the
            // pre-fetch value (the previous successful poll). Best-effort:
            // never blocks or fails the fetch path.
            crate::limit_attr::record_extra_delta(account, cached.as_ref(), &fresh);
            FetchOutcome::Fetched(fresh)
        }
        Err(FetchError::RateLimited) => {
            write_backoff(account, "rate_limited", "");
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Error,
                &format!(
                    "backing off {}s after 429 · {}",
                    backoff_after_429().as_secs(),
                    account.name
                ),
            );
            FetchOutcome::RateLimited(cached)
        }
        Err(e @ (FetchError::Unauthorized | FetchError::Forbidden)) => {
            // Both are permission rejections the endpoint won't reconsider
            // until something changes locally (a refreshed token, or a
            // subscription change) — back off like a 429 rather than
            // retrying every keypress. If we know the local credential's
            // expiry and it's already passed, say so: that's the most
            // actionable read for a user staring at a 401/403 they didn't
            // cause (e.g. Claude Code hasn't refreshed the token on this
            // machine in a while).
            let mut reason = e.to_string();
            if let Some(expiry) = expiry {
                if expiry < now {
                    let stale_for = (now - expiry).num_minutes().max(0);
                    reason.push_str(&format!(
                        " (local token expired {stale_for}m ago — open Claude Code on this machine to refresh it)"
                    ));
                }
            }
            write_backoff(account, "denied", &reason);
            log_once(
                &account.name,
                format!("mewxi: live fetch failed for '{}': {reason}", account.name),
            );
            FetchOutcome::Failed { reason, cached }
        }
        Err(e) => {
            log_once(
                &account.name,
                format!("mewxi: live fetch failed for '{}': {e}", account.name),
            );
            FetchOutcome::Failed {
                reason: e.to_string(),
                cached,
            }
        }
    }
}

// --- Minimal in-process state for error display + log dedupe ----------------
//
// (429/401/403 backoff used to live here too, but in-memory flags die
// with the one-shot statusline process — it's on disk now, see the
// marker section above.)

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

/// Set to `true` by the TUI before it enters the alternate screen. Once
/// on, [`log_once`] no longer writes to stderr (which would corrupt the
/// TUI's rendering) — it records into the in-memory registry so the TUI
/// can surface errors in its own bordered footer instead.
static TUI_MODE: AtomicBool = AtomicBool::new(false);

pub fn set_tui_mode(on: bool) {
    TUI_MODE.store(on, Ordering::Relaxed);
}

fn error_registry() -> &'static Mutex<HashMap<String, (String, Instant)>> {
    static S: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_error(account: &str, msg: &str) {
    if let Ok(mut m) = error_registry().lock() {
        m.insert(account.to_string(), (msg.to_string(), Instant::now()));
    }
}

fn clear_error_entry(account: &str) {
    if let Ok(mut m) = error_registry().lock() {
        m.remove(account);
    }
}

/// Most recently observed live-fetch error across all accounts, if any.
/// Returns (account, message) for the TUI to render in its error footer.
pub fn most_recent_error() -> Option<(String, String)> {
    let m = error_registry().lock().ok()?;
    m.iter()
        .max_by_key(|(_, (_, t))| *t)
        .map(|(acct, (msg, _))| (acct.clone(), msg.clone()))
}

fn logged_once_set() -> &'static Mutex<std::collections::HashSet<String>> {
    static S: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunable_duration_defaults_clamps_and_passes_through() {
        // Unset → built-in default.
        assert_eq!(
            tunable_duration(None, DEFAULT_REFRESH_INTERVAL),
            DEFAULT_REFRESH_INTERVAL
        );
        // Configured value wins.
        assert_eq!(
            tunable_duration(Some(300), DEFAULT_REFRESH_INTERVAL),
            Duration::from_secs(300)
        );
        // Absurdly low values are floored, not honored.
        assert_eq!(
            tunable_duration(Some(1), DEFAULT_REFRESH_INTERVAL),
            Duration::from_secs(MIN_TUNABLE_SECS)
        );
    }

    #[test]
    fn stamp_from_mtime_distinguishes_missing_and_tracks_changes() {
        use std::time::{Duration as StdDuration, UNIX_EPOCH};

        // Missing config gets the dedicated sentinel, not an aliasable
        // real timestamp.
        assert_eq!(stamp_from_mtime(None), MISSING_CONFIG_STAMP);

        let t1 = UNIX_EPOCH + StdDuration::from_secs(1_700_000_000);
        let t2 = UNIX_EPOCH + StdDuration::from_secs(1_700_000_001);

        // Same mtime → same stamp (the no-op path `tuning()` relies on
        // to skip re-reading the config).
        assert_eq!(stamp_from_mtime(Some(t1)), stamp_from_mtime(Some(t1)));
        // Different mtime → different stamp (the edit-detection path).
        assert_ne!(stamp_from_mtime(Some(t1)), stamp_from_mtime(Some(t2)));
        // Neither collides with the "missing" sentinel.
        assert_ne!(stamp_from_mtime(Some(t1)), MISSING_CONFIG_STAMP);
    }

    #[test]
    fn backoff_marker_roundtrips_and_ages_out() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("live-test.backoff.json");

        // No file → no backoff.
        assert!(read_backoff_marker(&p, Duration::from_secs(120)).is_none());

        write_backoff_marker(&p, "denied", "forbidden (403)");
        match read_backoff_marker(&p, Duration::from_secs(120)) {
            Some(Backoff::Denied { reason, .. }) => assert_eq!(reason, "forbidden (403)"),
            _ => panic!("expected Denied within the window"),
        }

        write_backoff_marker(&p, "rate_limited", "");
        assert!(matches!(
            read_backoff_marker(&p, Duration::from_secs(120)),
            Some(Backoff::RateLimited { .. })
        ));

        // Unknown kinds from a future binary read as the conservative
        // rate-limited state, not as "no backoff".
        write_backoff_marker(&p, "some-future-kind", "");
        assert!(matches!(
            read_backoff_marker(&p, Duration::from_secs(120)),
            Some(Backoff::RateLimited { .. })
        ));

        // A marker older than the window reads as "no backoff". Simulate
        // age by shrinking the window instead of sleeping.
        assert!(read_backoff_marker(&p, Duration::ZERO).is_none());

        // Corrupt markers must not wedge polling.
        std::fs::write(&p, b"not json").unwrap();
        assert!(read_backoff_marker(&p, Duration::from_secs(120)).is_none());
    }

    #[test]
    fn expired_minutes_ago_only_fires_for_past_expiry() {
        let now = DateTime::parse_from_rfc3339("2026-08-27T07:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let earlier = now - chrono::Duration::minutes(150);
        let later = now + chrono::Duration::minutes(5);

        // No expiry on the credential (env/MEWXI_OAUTH_TOKEN paths) →
        // nothing to check, send as before.
        assert_eq!(expired_minutes_ago(None, now), None);
        // Still valid → send.
        assert_eq!(expired_minutes_ago(Some(later), now), None);
        // Exactly at expiry is not yet "past".
        assert_eq!(expired_minutes_ago(Some(now), now), None);
        // Past → skip, reporting how stale (whole minutes).
        assert_eq!(expired_minutes_ago(Some(earlier), now), Some(150));
    }

    #[test]
    fn summarize_body_squashes_and_truncates() {
        assert_eq!(summarize_body(""), "(empty body)");
        assert_eq!(summarize_body("   \n\t "), "(empty body)");
        assert_eq!(
            summarize_body("{\n  \"error\": {\n    \"type\": \"rate_limit_error\"\n  }\n}"),
            "{ \"error\": { \"type\": \"rate_limit_error\" } }"
        );
        let long = "x".repeat(500);
        let s = summarize_body(&long);
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), 201);
    }
}

fn log_once(account: &str, msg: String) {
    record_error(account, &msg);
    // In TUI mode, never write to stderr — it bleeds onto the alternate
    // screen and corrupts the UI. The TUI reads the registry instead.
    if TUI_MODE.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut s) = logged_once_set().lock() {
        if s.insert(msg.clone()) {
            eprintln!("{msg}");
        }
    }
}
