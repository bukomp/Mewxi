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
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// Built-in default for the post-429 backoff. Override with
/// `live_backoff_secs` in `accounts.toml` (see [`backoff_after_429`]).
pub const DEFAULT_BACKOFF_AFTER_429: Duration = Duration::from_secs(120);

/// Floor for both tunables — a typo'd `live_refresh_interval_secs = 1`
/// must not turn the per-keypress statusline into an HTTP hammer.
const MIN_TUNABLE_SECS: u64 = 10;

/// Minimum age of a cached response before we'll refetch on demand.
/// `live_refresh_interval_secs` in `accounts.toml`; defaults to
/// [`DEFAULT_REFRESH_INTERVAL`]. Read once per process — the statusline
/// spawns fresh per invocation so edits apply immediately there; the
/// long-lived TUI picks them up on restart.
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
// real values from ever being 0). Plain atomics rather than OnceLock so
// the Config view can invalidate them after writing a new value —
// otherwise a running TUI would keep the old cadence until restart.
static TUNED_REFRESH_SECS: AtomicU64 = AtomicU64::new(0);
static TUNED_BACKOFF_SECS: AtomicU64 = AtomicU64::new(0);

fn tuning() -> (Duration, Duration) {
    // Acquire pairs with the Release below: a reader that sees a
    // non-zero refresh value is guaranteed to see the backoff written
    // before it.
    let refresh = TUNED_REFRESH_SECS.load(Ordering::Acquire);
    if refresh != 0 {
        return (
            Duration::from_secs(refresh),
            Duration::from_secs(TUNED_BACKOFF_SECS.load(Ordering::Relaxed)),
        );
    }
    let (refresh, backoff) = crate::accounts::live_tuning();
    let refresh = tunable_duration(refresh, DEFAULT_REFRESH_INTERVAL);
    let backoff = tunable_duration(backoff, DEFAULT_BACKOFF_AFTER_429);
    TUNED_BACKOFF_SECS.store(backoff.as_secs(), Ordering::Relaxed);
    TUNED_REFRESH_SECS.store(refresh.as_secs(), Ordering::Release);
    (refresh, backoff)
}

/// Drop the cached tunables so the next read re-parses `accounts.toml`.
/// Called by the TUI's Config view after persisting a new value.
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
        Err(ureq::Error::Status(429, _)) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Error,
                &format!("429 rate limited · {}", fmt_dur(dur_ms)),
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

fn write_backoff(account: &Account, kind: &str, reason: &str) {
    let Some(p) = backoff_path(account) else { return };
    write_backoff_marker(&p, kind, reason);
}

fn clear_backoff(account: &Account) {
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
    // A missing marker is the common, expected steady state (no backoff in
    // effect) — not logged to avoid noise; only a successful read (an
    // active or recently-expired marker) is worth recording.
    let bytes = fs::read(p).ok()?;
    let file_name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    crate::debug_log::log_event(
        crate::debug_log::LogOrigin::Usage,
        crate::debug_log::LogKind::FileRead,
        &format!("read {file_name}"),
    );
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
/// short-circuit and always attempt one HTTP call. Used by the TUI's
/// initial poller bootstrap so a cold open never trusts a poisoned
/// cache from a stale background daemon. 429 backoff is still honored.
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
        // to hammer the endpoint after it told us to back off. This is the
        // only place a manual `r` refresh during backoff produces any
        // visible trace: no HTTP call happens, so `fetch_live`'s own
        // logging never fires (and the original 429 may even have been
        // observed by a different process, e.g. the watcher).
        Some(Backoff::RateLimited { remaining }) => {
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Error,
                &format!(
                    "fetch skipped — 429 backoff · {}s left · {}",
                    remaining.as_secs(),
                    account.name
                ),
            );
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
            crate::debug_log::log_event(
                crate::debug_log::LogOrigin::Usage,
                crate::debug_log::LogKind::Error,
                &format!(
                    "fetch skipped — {code} backoff · {}s left · {}",
                    remaining.as_secs(),
                    account.name
                ),
            );
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
        // Single-flight: when the cache expires, every concurrent session's
        // statusline sees "stale" in the same instant. Let the first one
        // fetch; siblings serve the (marginally stale) cache instead of
        // firing a synchronized burst at the endpoint.
        if fetch_recently_attempted(account) {
            return FetchOutcome::Cached(cached);
        }
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

    mark_fetch_attempt(account);
    match fetch_live(&token) {
        Ok(fresh) => {
            save_cached(account, &fresh);
            clear_backoff(account);
            clear_error_entry(&account.name);
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
