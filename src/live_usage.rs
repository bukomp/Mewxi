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
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::auth;

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA_HEADER: &str = "oauth-2025-04-20";
/// Mirror Claude Code's UA so the server treats us the same. The version is
/// the one currently installed on the author's machine; any `claude-code/X.Y.Z`
/// string appears to be accepted.
const USER_AGENT: &str = "claude-code/2.1.116";

/// Minimum age of a cached response before we'll refetch on demand.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// After a 429, wait at least this long before trying again.
pub const BACKOFF_AFTER_429: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowUsage {
    /// Percentage utilization, 0..=100.
    pub utilization: f64,
    /// When this window resets. Absent fields become `None`.
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LiveUsage {
    #[serde(default)]
    pub five_hour: Option<WindowUsage>,
    #[serde(default)]
    pub seven_day: Option<WindowUsage>,
    #[serde(default)]
    pub extra_usage: Option<ExtraUsage>,
    /// When we fetched this — set by us, not the server.
    pub fetched_at: DateTime<Utc>,
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
    /// within REFRESH_INTERVAL by design.
    pub fn is_stale(&self) -> bool {
        self.age_seconds() > (REFRESH_INTERVAL.as_secs() as i64 + BACKOFF_AFTER_429.as_secs() as i64)
    }
}

#[derive(Debug)]
pub enum FetchError {
    RateLimited,
    Unauthorized,
    Other(anyhow::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (429)"),
            FetchError::Unauthorized => write!(f, "unauthorized (401) — token expired or invalid"),
            FetchError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Raw HTTP call. Synchronous; blocks up to the request timeout.
pub fn fetch_live(token: &str) -> Result<LiveUsage, FetchError> {
    let resp = ureq::get(ENDPOINT)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", BETA_HEADER)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(15))
        .call();

    match resp {
        Ok(r) => {
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
                fetched_at: Utc::now(),
            })
        }
        Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
        Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            Err(FetchError::Other(anyhow!("HTTP {code}: {body}")))
        }
        Err(e) => Err(FetchError::Other(anyhow!("transport: {e}"))),
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
}

pub fn cache_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|c| c.join("claude-usage").join("live.json"))
}

pub fn load_cached() -> Option<LiveUsage> {
    let p = cache_path()?;
    let bytes = fs::read(&p).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn save_cached(u: &LiveUsage) {
    let Some(p) = cache_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(u) {
        let tmp = p.with_extension("json.tmp");
        if fs::write(&tmp, &bytes).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
    }
}

/// High-level helper: return cached value if fresh enough; otherwise fetch.
/// If a fetch fails, return the stale cached value (if any) rather than nothing.
///
/// `no_live=true` short-circuits to cache only and never hits the network.
pub fn fetch_or_cached(no_live: bool) -> Option<LiveUsage> {
    let cached = load_cached();

    if no_live {
        return cached;
    }

    let now = Utc::now();
    if let Some(ref c) = cached {
        // Hold off on refetch after a 429 for longer.
        let min_age = if is_recent_429() {
            BACKOFF_AFTER_429
        } else {
            REFRESH_INTERVAL
        };
        if let Ok(delta) = chrono::Duration::from_std(min_age) {
            if now - c.fetched_at < delta {
                return Some(c.clone());
            }
        }
    }

    let token = match auth::read_oauth_token() {
        Ok(t) => t,
        Err(e) => {
            log_once(format!("claude-usage: oauth token unavailable: {e}"));
            return cached;
        }
    };

    match fetch_live(&token) {
        Ok(fresh) => {
            save_cached(&fresh);
            clear_429_flag();
            Some(fresh)
        }
        Err(FetchError::RateLimited) => {
            mark_429();
            cached
        }
        Err(e) => {
            log_once(format!("claude-usage: live fetch failed: {e}"));
            cached
        }
    }
}

// --- Minimal in-process state for 429 back-off + log dedupe -----------------

use std::sync::Mutex;
use std::sync::OnceLock;

fn last_429_ts() -> &'static Mutex<Option<DateTime<Utc>>> {
    static S: OnceLock<Mutex<Option<DateTime<Utc>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}
fn logged_once_set() -> &'static Mutex<std::collections::HashSet<String>> {
    static S: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn mark_429() {
    if let Ok(mut g) = last_429_ts().lock() {
        *g = Some(Utc::now());
    }
}
fn clear_429_flag() {
    if let Ok(mut g) = last_429_ts().lock() {
        *g = None;
    }
}
fn is_recent_429() -> bool {
    let Ok(g) = last_429_ts().lock() else { return false };
    let Some(ts) = *g else { return false };
    let Ok(delta) = chrono::Duration::from_std(BACKOFF_AFTER_429) else { return false };
    Utc::now() - ts < delta
}

fn log_once(msg: String) {
    if let Ok(mut s) = logged_once_set().lock() {
        if s.insert(msg.clone()) {
            eprintln!("{msg}");
        }
    }
}
