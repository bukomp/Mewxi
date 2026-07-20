//! Attribute account-level limits and pay-per-use spend down to the
//! individual session that caused them.
//!
//! The Anthropic live-usage endpoint only reports account-wide numbers:
//! "you've used X% of your rolling 5h limit", "you've used Y% of your
//! weekly limit", and (when pay-per-use "extra usage" is active) a real
//! dollar amount billed this month. It has no notion of per-session
//! attribution. Locally, `crate::stats` computes a *nominal* API-rate
//! cost per record from published per-token pricing — nominal because a
//! Pro/Max plan isn't billed per-token at all while within its limits.
//!
//! This module bridges the two:
//!
//! - **Limit share**: a session's estimated share of the account's
//!   rolling 5h and weekly usage limits, calibrated as `(window
//!   utilization % reported by Anthropic's live usage endpoint) ×
//!   (this session's slice of the locally-computed nominal window cost
//!   ÷ the account's total nominal window cost)`. It's an estimate —
//!   local nominal API-rate costs are used only as *proportions*, then
//!   scaled by the real utilization the server reports.
//!
//! - **Price**: a session's real price is $0 while the account is
//!   within plan limits. When pay-per-use "extra usage" is active, mewxi
//!   records each observed increase in the account's billed credits
//!   (polled ~60s) into an on-disk delta ledger tagged with its
//!   observation interval; `session_prices` then attributes each delta
//!   only to the sessions whose records fall inside that interval,
//!   proportional to their nominal cost. Tokens produced entirely within
//!   plan limits are never priced.

use crate::accounts::Account;
use crate::debug_log::{LogKind, LogOrigin};
use crate::live_usage::{refresh_interval, LiveUsage};
use crate::stats::Aggregate;
use crate::stats::UsageRecord;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A session's estimated share of the account's rolling 5h and weekly
/// usage limits. See the module doc for the estimation model.
pub struct LimitShare {
    /// Estimated % of the account's 5h limit consumed by this session's
    /// activity in the current block (0..=100 scale, can exceed session's
    /// literal share only via rounding). None when live data is missing.
    pub five_h_pct: Option<f64>,
    /// Same for the weekly limit over the trailing 7 days.
    pub weekly_pct: Option<f64>,
}

/// Estimate `session_id`'s share of the account's 5h and weekly limits.
/// See the module doc for the estimation model.
pub fn session_limit_share(agg: &Aggregate, live: Option<&LiveUsage>, session_id: &str) -> LimitShare {
    let five_h_pct = (|| {
        let u = live?.five_hour.as_ref()?.utilization;
        let sess: f64 = agg
            .five_h_records
            .iter()
            .filter(|r| r.session_id == session_id)
            .map(|r| r.cost_usd)
            .sum();
        let acct = agg.rolling_5h.cost_usd;
        if acct > 1e-6 {
            Some(u * sess / acct)
        } else {
            None
        }
    })();

    let weekly_pct = (|| {
        let u = live?.seven_day.as_ref()?.utilization;
        let sess = agg
            .trailing_7d_cost_by_session
            .get(session_id)
            .copied()
            .unwrap_or(0.0);
        let acct = agg.trailing_7d_cost_usd;
        if acct > 1e-6 {
            Some(u * sess / acct)
        } else {
            None
        }
    })();

    LimitShare { five_h_pct, weekly_pct }
}

/// One observed increase in the account's billed "extra usage" credits,
/// tagged with the interval over which it was observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Start of the observation interval — the previous successful
    /// poll's fetched_at (best effort: `at - refresh_interval()` when
    /// no previous cache existed).
    pub window_start: DateTime<Utc>,
    /// When the increase was observed (the fresh fetch's fetched_at).
    pub at: DateTime<Utc>,
    /// Cumulative used_credits (cents) before/after — delta = to - from.
    pub from_cents: f64,
    pub to_cents: f64,
    /// Currency code reported by the endpoint (e.g. "EUR").
    pub currency: Option<String>,
}

/// `<cache>/mewxi/extra-ledger-<slug>.jsonl` (mirrors live_usage::cache_path).
pub fn ledger_path(account: &Account) -> Option<PathBuf> {
    dirs::cache_dir().map(|c| c.join("mewxi").join(format!("extra-ledger-{}.jsonl", account.slug())))
}

fn load_ledger_from(path: &Path) -> Vec<LedgerEntry> {
    let bytes = match fs::read_to_string(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    bytes
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<LedgerEntry>(l).ok())
        .collect()
}

/// JSONL: one LedgerEntry per line; unparseable lines skipped; missing file = empty.
pub fn load_ledger(account: &Account) -> Vec<LedgerEntry> {
    ledger_path(account).map(|p| load_ledger_from(&p)).unwrap_or_default()
}

/// Best-effort: observe an increase in extra-usage credits and append a
/// ledger entry. Never panics, never blocks the fetch path — any IO
/// failure logs and returns.
///
/// Rules:
/// 1. If `fresh` has no enabled extra-usage with a positive `used_credits`,
///    there is nothing to record — return.
/// 2. `from` is the greater of the previous fetch's `used_credits` (if
///    enabled) and the last ledger entry's `to_cents` — the max is a
///    cross-process dedupe: a sibling process that already recorded this
///    increase leaves `last_to` at the new level, so this process's delta
///    collapses to zero instead of double-counting.
/// 3. If `fresh_used` fell below `from` (monthly counter reset, or
///    endpoint jitter), append a zero-delta "rebaseline" entry so future
///    deltas measure from the new baseline, attribute nothing this round,
///    and return.
/// 4. Otherwise append `LedgerEntry { window_start: <previous fetch's
///    fetched_at, or now - refresh_interval()>, at: <fresh's fetched_at>,
///    from_cents: from, to_cents: fresh_used, currency }`.
/// 5. Entries older than 45 days are pruned on every write.
pub fn record_extra_delta(account: &Account, prev: Option<&LiveUsage>, fresh: &LiveUsage) {
    let Some(path) = ledger_path(account) else { return };
    record_extra_delta_at(&path, prev, fresh, refresh_interval());
}

fn write_ledger(path: &Path, mut entries: Vec<LedgerEntry>, new_entry: LedgerEntry) {
    let cutoff = Utc::now() - chrono::Duration::days(45);
    entries.retain(|e| e.at >= cutoff);
    entries.push(new_entry);

    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            crate::debug_log::log_event(
                LogOrigin::Usage,
                LogKind::Error,
                &format!("extra-ledger: create_dir_all failed — {e}"),
            );
            return;
        }
    }

    let mut lines = Vec::with_capacity(entries.len());
    for entry in &entries {
        match serde_json::to_string(entry) {
            Ok(s) => lines.push(s),
            Err(e) => {
                crate::debug_log::log_event(
                    LogOrigin::Usage,
                    LogKind::Error,
                    &format!("extra-ledger: serialize failed — {e}"),
                );
                return;
            }
        }
    }
    let mut content = lines.join("\n");
    content.push('\n');

    let tmp = path.with_extension("jsonl.tmp");
    if let Err(e) = fs::write(&tmp, &content).and_then(|_| fs::rename(&tmp, path)) {
        crate::debug_log::log_event(
            LogOrigin::Usage,
            LogKind::Error,
            &format!("extra-ledger: write failed — {e}"),
        );
    }
}

fn record_extra_delta_at(path: &Path, prev: Option<&LiveUsage>, fresh: &LiveUsage, refresh: Duration) {
    let fresh_used = match fresh.extra_usage.as_ref() {
        Some(e) if e.is_enabled => match e.used_credits {
            Some(v) if v > 0.0 => v,
            _ => return,
        },
        _ => return,
    };

    let existing = load_ledger_from(path);
    let last_to = existing.last().map(|e| e.to_cents);
    let prev_used = prev.and_then(|p| p.extra_usage.as_ref()).and_then(|e| if e.is_enabled { e.used_credits } else { None });

    let from = prev_used.unwrap_or(0.0).max(last_to.unwrap_or(0.0));
    let at = fresh.fetched_at;
    let currency = fresh.extra_usage.as_ref().and_then(|e| e.currency.clone());

    if fresh_used < from {
        let rebaseline = LedgerEntry {
            window_start: at,
            at,
            from_cents: fresh_used,
            to_cents: fresh_used,
            currency,
        };
        write_ledger(path, existing, rebaseline);
        return;
    }

    let delta = fresh_used - from;
    if delta <= 0.0 {
        return;
    }

    let window_start = prev
        .map(|p| p.fetched_at)
        .unwrap_or(at - chrono::Duration::from_std(refresh).unwrap_or_else(|_| chrono::Duration::seconds(60)));

    let entry = LedgerEntry { window_start, at, from_cents: from, to_cents: fresh_used, currency: currency.clone() };
    write_ledger(path, existing, entry);

    crate::debug_log::log_event(
        LogOrigin::Usage,
        LogKind::FileWrite,
        &format!("extra usage +{:.2} {} recorded", delta / 100.0, currency.as_deref().unwrap_or("")),
    );
}

/// Attribute ledger deltas to the sessions whose records fall in each
/// observation interval. Returned values are in the account's
/// extra-usage currency units (e.g. EUR, not cents). Pure — no IO.
///
/// Each entry's interval is `(window_start, at]`. Records with a
/// timestamp in that range have their nominal `cost_usd` summed per
/// session; the entry's delta is then split across those sessions
/// proportional to their share of that interval's total nominal cost.
/// Zero-delta entries (rebaselines) and intervals with no matching
/// records (or zero total nominal cost) contribute nothing — sessions
/// wholly within plan limits are simply absent from the returned map.
pub fn session_prices(records: &[UsageRecord], ledger: &[LedgerEntry]) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = HashMap::new();

    for entry in ledger {
        if entry.to_cents <= entry.from_cents {
            continue;
        }

        let matching: Vec<&UsageRecord> = records
            .iter()
            .filter(|r| entry.window_start < r.timestamp && r.timestamp <= entry.at)
            .collect();

        let total: f64 = matching.iter().map(|r| r.cost_usd).sum();
        if total <= 0.0 {
            continue;
        }

        let amount = (entry.to_cents - entry.from_cents) / 100.0;

        let mut cost_by_session: HashMap<&str, f64> = HashMap::new();
        for r in &matching {
            *cost_by_session.entry(r.session_id.as_str()).or_insert(0.0) += r.cost_usd;
        }

        for (session_id, sess_cost) in cost_by_session {
            *out.entry(session_id.to_string()).or_insert(0.0) += amount * sess_cost / total;
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_usage::{ExtraUsage, LiveUsage, WindowUsage};
    use crate::stats::UsageRecord;
    use chrono::Duration as ChronoDuration;
    use chrono::Utc;
    use std::collections::HashMap;
    use std::time::Duration;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "expected {b}, got {a}");
    }

    fn live(five_hour: Option<WindowUsage>, seven_day: Option<WindowUsage>, extra_usage: Option<ExtraUsage>) -> LiveUsage {
        LiveUsage {
            five_hour,
            seven_day,
            extra_usage,
            limits: Vec::new(),
            fetched_at: Utc::now(),
            cache_schema_version: 0,
        }
    }

    fn live_extra(used_credits: Option<f64>, is_enabled: bool, fetched_at: DateTime<Utc>) -> LiveUsage {
        LiveUsage {
            five_hour: None,
            seven_day: None,
            extra_usage: Some(ExtraUsage {
                is_enabled,
                monthly_limit: None,
                used_credits,
                utilization: None,
                currency: Some("EUR".to_string()),
            }),
            limits: Vec::new(),
            fetched_at,
            cache_schema_version: 0,
        }
    }

    fn record(session_id: &str, cost_usd: f64, message_id: &str) -> UsageRecord {
        UsageRecord {
            timestamp: Utc::now(),
            session_id: session_id.to_string(),
            project: "proj".to_string(),
            model: "claude".to_string(),
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cost_usd,
            message_id: message_id.to_string(),
            is_sidechain: false,
        }
    }

    fn record_at(session_id: &str, cost_usd: f64, ts: DateTime<Utc>) -> UsageRecord {
        let mut r = record(session_id, cost_usd, "m");
        r.timestamp = ts;
        r
    }

    #[test]
    fn five_h_share_with_block_activity() {
        let mut agg = Aggregate { ..Default::default() };
        agg.five_h_records = vec![record("sess-a", 3.0, "m1"), record("sess-b", 1.0, "m2")];
        agg.rolling_5h.cost_usd = 4.0;

        let l = live(Some(WindowUsage { utilization: 50.0, resets_at: None }), None, None);
        let share = session_limit_share(&agg, Some(&l), "sess-a");
        approx(share.five_h_pct.expect("expected Some"), 50.0 * 3.0 / 4.0);
    }

    #[test]
    fn missing_live_data_yields_none() {
        let agg = Aggregate { ..Default::default() };
        let share = session_limit_share(&agg, None, "sess-a");
        assert_eq!(share.five_h_pct, None);
        assert_eq!(share.weekly_pct, None);
    }

    #[test]
    fn zero_account_window_cost_yields_none() {
        let agg = Aggregate { ..Default::default() };
        let l = live(Some(WindowUsage { utilization: 50.0, resets_at: None }), None, None);
        let share = session_limit_share(&agg, Some(&l), "sess-a");
        assert_eq!(share.five_h_pct, None);
    }

    #[test]
    fn session_with_no_block_activity_yields_some_zero() {
        let mut agg = Aggregate { ..Default::default() };
        agg.five_h_records = vec![record("sess-b", 4.0, "m1")];
        agg.rolling_5h.cost_usd = 4.0;

        let l = live(Some(WindowUsage { utilization: 50.0, resets_at: None }), None, None);
        let share = session_limit_share(&agg, Some(&l), "sess-a");
        approx(share.five_h_pct.expect("expected Some"), 0.0);
    }

    #[test]
    fn weekly_share_with_activity() {
        let mut agg = Aggregate { ..Default::default() };
        let mut by_session = HashMap::new();
        by_session.insert("sess-a".to_string(), 6.0);
        agg.trailing_7d_cost_by_session = by_session;
        agg.trailing_7d_cost_usd = 8.0;

        let l = live(None, Some(WindowUsage { utilization: 20.0, resets_at: None }), None);
        let share = session_limit_share(&agg, Some(&l), "sess-a");
        approx(share.weekly_pct.expect("expected Some"), 20.0 * 6.0 / 8.0);
    }

    #[test]
    fn session_prices_splits_delta_proportionally() {
        let t0 = Utc::now();
        let t1 = t0 + ChronoDuration::seconds(60);

        let ledger = vec![LedgerEntry {
            window_start: t0,
            at: t1,
            from_cents: 0.0,
            to_cents: 300.0,
            currency: Some("EUR".to_string()),
        }];

        let records = vec![
            record_at("sess-a", 3.0, t0 + ChronoDuration::seconds(10)),
            record_at("sess-b", 1.0, t0 + ChronoDuration::seconds(20)),
        ];

        let out = session_prices(&records, &ledger);
        approx(*out.get("sess-a").expect("sess-a present"), 2.25);
        approx(*out.get("sess-b").expect("sess-b present"), 0.75);
    }

    #[test]
    fn session_prices_ignores_records_outside_intervals() {
        let t0 = Utc::now();
        let t1 = t0 + ChronoDuration::seconds(60);

        let ledger = vec![LedgerEntry {
            window_start: t0,
            at: t1,
            from_cents: 0.0,
            to_cents: 100.0,
            currency: Some("EUR".to_string()),
        }];

        // Before window_start (not > window_start) and after `at`.
        let records = vec![
            record_at("sess-before", 5.0, t0 - ChronoDuration::seconds(1)),
            record_at("sess-after", 5.0, t1 + ChronoDuration::seconds(1)),
        ];

        let out = session_prices(&records, &ledger);
        assert!(!out.contains_key("sess-before"));
        assert!(!out.contains_key("sess-after"));
        assert!(out.is_empty());
    }

    #[test]
    fn session_prices_accumulates_multiple_entries() {
        let t0 = Utc::now();
        let t1 = t0 + ChronoDuration::seconds(60);
        let t2 = t1 + ChronoDuration::seconds(60);
        let t3 = t2 + ChronoDuration::seconds(60);

        let ledger = vec![
            LedgerEntry { window_start: t0, at: t1, from_cents: 0.0, to_cents: 100.0, currency: Some("EUR".to_string()) },
            LedgerEntry { window_start: t2, at: t3, from_cents: 100.0, to_cents: 200.0, currency: Some("EUR".to_string()) },
        ];

        let records = vec![
            record_at("sess-a", 1.0, t0 + ChronoDuration::seconds(10)),
            record_at("sess-a", 1.0, t2 + ChronoDuration::seconds(10)),
        ];

        let out = session_prices(&records, &ledger);
        // First interval: sess-a is sole session → gets full 1.00 EUR.
        // Second interval: sess-a is sole session → gets full 1.00 EUR.
        approx(*out.get("sess-a").expect("sess-a present"), 2.0);
    }

    #[test]
    fn session_prices_ignores_zero_delta_entries() {
        let t0 = Utc::now();
        let t1 = t0 + ChronoDuration::seconds(60);

        let ledger = vec![LedgerEntry {
            window_start: t0,
            at: t1,
            from_cents: 100.0,
            to_cents: 100.0,
            currency: Some("EUR".to_string()),
        }];

        let records = vec![record_at("sess-a", 3.0, t0 + ChronoDuration::seconds(10))];

        let out = session_prices(&records, &ledger);
        assert!(out.is_empty());
    }

    #[test]
    fn record_extra_delta_first_increase_appends_one_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("extra-ledger.jsonl");
        let t1 = Utc::now();
        let refresh = Duration::from_secs(60);

        record_extra_delta_at(&path, None, &live_extra(Some(500.0), true, t1), refresh);

        let entries = load_ledger_from(&path);
        assert_eq!(entries.len(), 1);
        approx(entries[0].from_cents, 0.0);
        approx(entries[0].to_cents, 500.0);
        assert_eq!(entries[0].window_start, t1 - ChronoDuration::seconds(60));
    }

    #[test]
    fn record_extra_delta_same_fresh_twice_dedupes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("extra-ledger.jsonl");
        let t1 = Utc::now();
        let refresh = Duration::from_secs(60);
        let fresh = live_extra(Some(500.0), true, t1);

        record_extra_delta_at(&path, None, &fresh, refresh);
        record_extra_delta_at(&path, None, &fresh, refresh);

        let entries = load_ledger_from(&path);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn record_extra_delta_decrease_rebaselines_then_measures_from_new_baseline() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("extra-ledger.jsonl");
        let refresh = Duration::from_secs(60);
        let t1 = Utc::now();
        let t2 = t1 + ChronoDuration::seconds(60);
        let t3 = t2 + ChronoDuration::seconds(60);

        record_extra_delta_at(&path, None, &live_extra(Some(500.0), true, t1), refresh);
        assert_eq!(load_ledger_from(&path).len(), 1);

        // Decrease (monthly reset) → rebaseline, nothing attributed.
        record_extra_delta_at(&path, None, &live_extra(Some(100.0), true, t2), refresh);
        let entries = load_ledger_from(&path);
        assert_eq!(entries.len(), 2);
        approx(entries[1].from_cents, 100.0);
        approx(entries[1].to_cents, 100.0);

        // Subsequent increase measures from the new baseline (100), not the old (500).
        record_extra_delta_at(&path, None, &live_extra(Some(250.0), true, t3), refresh);
        let entries = load_ledger_from(&path);
        assert_eq!(entries.len(), 3);
        approx(entries[2].from_cents, 100.0);
        approx(entries[2].to_cents, 250.0);
    }

    #[test]
    fn record_extra_delta_prunes_entries_older_than_45d() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("extra-ledger.jsonl");
        let old_at = Utc::now() - ChronoDuration::days(50);

        let old_entry = LedgerEntry {
            window_start: old_at - ChronoDuration::seconds(60),
            at: old_at,
            from_cents: 0.0,
            to_cents: 100.0,
            currency: Some("EUR".to_string()),
        };
        fs::write(&path, format!("{}\n", serde_json::to_string(&old_entry).unwrap())).unwrap();

        let refresh = Duration::from_secs(60);
        let now = Utc::now();
        record_extra_delta_at(&path, None, &live_extra(Some(200.0), true, now), refresh);

        let entries = load_ledger_from(&path);
        assert_eq!(entries.len(), 1);
        approx(entries[0].from_cents, 100.0);
        approx(entries[0].to_cents, 200.0);
    }
}
