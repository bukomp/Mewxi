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
use std::path::PathBuf;
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
    /// within REFRESH_INTERVAL by design.
    pub fn is_stale(&self) -> bool {
        self.age_seconds() > (REFRESH_INTERVAL.as_secs() as i64 + BACKOFF_AFTER_429.as_secs() as i64)
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
                limits: parsed.limits,
                fetched_at: Utc::now(),
                cache_schema_version: CACHE_SCHEMA_VERSION,
            })
        }
        Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
        Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
        Err(ureq::Error::Status(403, _)) => Err(FetchError::Forbidden),
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
    let bytes = fs::read(&p).ok()?;
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
    if let Ok(bytes) = serde_json::to_vec(u) {
        let tmp = p.with_extension("json.tmp");
        if fs::write(&tmp, &bytes).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
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

    // 401/403 get the same treatment as 429: the statusline is invoked per
    // keypress, so re-hitting an endpoint that just told us "no" on every
    // render would hammer it for nothing. Unlike the 429 branch below, this
    // must not depend on having a (fresh) cache — a persistent 403 means the
    // cache is old or absent, precisely when the backoff matters most.
    // `force` (the TUI's manual refresh) bypasses it so the user can retry
    // immediately after fixing their credentials.
    if !force {
        if let Some(reason) = recent_denied(&account.name) {
            return FetchOutcome::Failed { reason, cached };
        }
    }

    if let Some(ref c) = cached {
        // 429 backoff is non-negotiable even with `force` — we don't want
        // to hammer the endpoint after it told us to back off.
        if is_recent_429(&account.name) {
            if let Ok(delta) = chrono::Duration::from_std(BACKOFF_AFTER_429) {
                if now - c.fetched_at < delta {
                    return FetchOutcome::RateLimited(cached);
                }
            }
        } else if !force {
            if let Ok(delta) = chrono::Duration::from_std(REFRESH_INTERVAL) {
                if now - c.fetched_at < delta {
                    return FetchOutcome::Cached(cached);
                }
            }
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

    match fetch_live(&token) {
        Ok(fresh) => {
            save_cached(account, &fresh);
            clear_429_flag(&account.name);
            clear_denied_flag(&account.name);
            clear_error_entry(&account.name);
            FetchOutcome::Fetched(fresh)
        }
        Err(FetchError::RateLimited) => {
            mark_429(&account.name);
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
            mark_denied(&account.name, reason.clone());
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

// --- Minimal in-process state for 429 back-off + log dedupe -----------------

use std::sync::atomic::{AtomicBool, Ordering};
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

fn last_429_ts() -> &'static Mutex<HashMap<String, DateTime<Utc>>> {
    static S: OnceLock<Mutex<HashMap<String, DateTime<Utc>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Parallel to `last_429_ts`, but for 401/403 (permission rejections rather
/// than rate limiting). Kept as a distinct map — rather than reusing the 429
/// one — so a short-circuited call during the backoff window can still
/// report `FetchOutcome::Failed` with the *actual* reason (forbidden /
/// unauthorized, possibly with the stale-token note) instead of being
/// mislabeled as `RateLimited`.
fn last_denied() -> &'static Mutex<HashMap<String, (DateTime<Utc>, String)>> {
    static S: OnceLock<Mutex<HashMap<String, (DateTime<Utc>, String)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}
fn logged_once_set() -> &'static Mutex<std::collections::HashSet<String>> {
    static S: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn mark_429(account: &str) {
    if let Ok(mut g) = last_429_ts().lock() {
        g.insert(account.to_string(), Utc::now());
    }
}
fn clear_429_flag(account: &str) {
    if let Ok(mut g) = last_429_ts().lock() {
        g.remove(account);
    }
}
fn is_recent_429(account: &str) -> bool {
    let Ok(g) = last_429_ts().lock() else { return false };
    let Some(ts) = g.get(account).copied() else { return false };
    let Ok(delta) = chrono::Duration::from_std(BACKOFF_AFTER_429) else { return false };
    Utc::now() - ts < delta
}

fn mark_denied(account: &str, reason: String) {
    if let Ok(mut g) = last_denied().lock() {
        g.insert(account.to_string(), (Utc::now(), reason));
    }
}
fn clear_denied_flag(account: &str) {
    if let Ok(mut g) = last_denied().lock() {
        g.remove(account);
    }
}
/// If the account was denied (401/403) within the backoff window, return
/// the reason string that was recorded at the time — so a short-circuited
/// caller can still surface *why*, not just that it's backing off.
fn recent_denied(account: &str) -> Option<String> {
    let g = last_denied().lock().ok()?;
    let (ts, reason) = g.get(account)?;
    let delta = chrono::Duration::from_std(BACKOFF_AFTER_429).ok()?;
    if Utc::now() - *ts < delta {
        Some(reason.clone())
    } else {
        None
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
