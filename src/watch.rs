//! Status-line renderer + background watcher.
//!
//! [`render_status`] builds the one-line ANSI-coloured string shown in
//! Claude Code's `statusLine`. It composes up to four segments:
//!
//! - 5h window (live if available, local estimate otherwise).
//! - 5h reset time.
//! - 7d window (live only).
//! - Active-extra-usage segment (promotes itself to leading position
//!   and hides the 5h % while subscription credits are being burned).
//! - Per-session context (`ctx N%`) when a transcript is in scope.
//!
//! [`run_forever`] is the `watch` subcommand: it subscribes to JSONL
//! change events, debounces to at most one write per 500 ms,
//! heartbeats every 15 s, and atomically renames
//! `status.txt.tmp` → `status.txt` under
//! `$XDG_CACHE_HOME/claude-usage/`. Point the statusLine hook at
//! `cat $XDG_CACHE_HOME/claude-usage/status.txt` for zero-latency
//! rendering.

use crate::live_usage;
use crate::stats;
use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

/// 5h cap in tokens. Overridable via env var to match your plan.
/// Defaults to Max 5× (~11.5M tokens — calibrated against Claude Code's /usage display).
/// Pro ≈ 2.3M, Max 20× ≈ 46M.
const DEFAULT_5H_CAP_TOKENS: u64 = 11_500_000;

fn five_h_cap_tokens() -> u64 {
    std::env::var("CLAUDE_USAGE_5H_CAP_TOKENS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&v: &u64| v > 0)
        .unwrap_or(DEFAULT_5H_CAP_TOKENS)
}

fn fmt_tokens_compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Render the current usage as an ANSI-colored one-liner for Claude Code's statusLine.
/// `transcript_path` is used to compute current-session context size;
/// `model_alias` is Claude Code's `model.id` from its stdin payload (may contain `[1m]`).
/// `no_live` disables the OAuth-endpoint fetch and falls back to local-JSONL estimates.
pub fn render_status(transcript_path: Option<&Path>, model_alias: Option<&str>, no_live: bool) -> String {
    let agg = stats::load_and_aggregate().unwrap_or_default();
    let live = live_usage::fetch_or_cached(no_live);

    // Extra usage is "actively billing" once we've spent any credits. At that
    // point the 5h meter is pinned at 100% and the 7d meter is noise next to
    // the hard-limit dollar spend, so we hide both and let `extra` lead the
    // line — followed by the 5h reset time so the user still sees when the
    // main meter frees up.
    let billing_extra = live
        .as_ref()
        .and_then(|l| l.extra_usage.as_ref())
        .filter(|e| e.is_enabled)
        .and_then(|e| e.used_credits)
        .is_some_and(|c| c > 0.0);

    // --- 5h window segment -------------------------------------------------
    // When billing_extra is active we drop the 5h label+pct but keep the
    // reset time (it appears after the `extra` segment below).
    let (five_h_segment, reset_segment) = if billing_extra {
        (String::new(), five_h_reset_from_live(live.as_ref()))
    } else {
        match five_h_from_live(live.as_ref()) {
            Some((seg, reset)) => (seg, reset),
            None => local_five_h_segment(&agg),
        }
    };

    // --- 7d window segment (only from live endpoint, hidden when billing) -
    let seven_d_segment = if billing_extra {
        String::new()
    } else {
        live.as_ref()
            .and_then(|l| l.seven_day.as_ref())
            .map(|w| {
                let pct = w.utilization;
                let color = pct_color(pct);
                let reset = w
                    .resets_at
                    .map(|t| format!(" (reset {})", t.with_timezone(&Local).format("%a")))
                    .unwrap_or_default();
                format!(
                    " \x1b[90m|\x1b[0m \x1b[36m7d\x1b[0m \x1b[{c}m{p:.0}%\x1b[0m\x1b[90m{reset}\x1b[0m",
                    c = color,
                    p = pct
                )
            })
            .unwrap_or_default()
    };

    // --- Extra usage segment (only when actually billing) -----------------
    // When billing_extra is active this leads the line, so we omit the
    // leading ` | ` separator.
    let extra_segment = if billing_extra {
        live.as_ref()
            .and_then(|l| l.extra_usage.as_ref())
            .map(|e| {
                let used = e.used_credits.unwrap_or(0.0) / 100.0;
                let limit = e.monthly_limit.unwrap_or(0.0) / 100.0;
                let pct = e.utilization.unwrap_or(0.0);
                let color = pct_color(pct);
                let sym = currency_symbol(e.currency.as_deref());
                format!(
                    "\x1b[36mextra\x1b[0m \x1b[{c}m{p:.0}%\x1b[0m \x1b[90m({sym}{:.2}/{sym}{:.2})\x1b[0m",
                    used,
                    limit,
                    c = color,
                    p = pct,
                )
            })
            .unwrap_or_default()
    } else {
        String::new()
    };

    // --- Context segment ---------------------------------------------------
    let ctx_segment = transcript_path
        .and_then(stats::current_context_from_transcript)
        .map(|sc| {
            let cap = stats::context_cap_for(&sc.model, sc.max_observed, model_alias);
            let pct = (sc.current as f64 / cap as f64 * 100.0).min(999.0);
            let color = if pct >= 85.0 { "31" } else if pct >= 60.0 { "33" } else { "32" };
            format!(
                " \x1b[90m|\x1b[0m \x1b[36mctx\x1b[0m \x1b[{c}m{p:.0}%\x1b[0m ({}/{})",
                fmt_tokens_compact(sc.current),
                fmt_tokens_compact(cap),
                c = color,
                p = pct
            )
        })
        .unwrap_or_default();

    if billing_extra {
        format!("{extra_segment}{reset_segment}{ctx_segment}")
    } else {
        format!("{five_h_segment}{reset_segment}{seven_d_segment}{extra_segment}{ctx_segment}")
    }
}

fn currency_symbol(code: Option<&str>) -> &'static str {
    match code.map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("USD") => "$",
        Some("EUR") => "€",
        Some("GBP") => "£",
        Some("JPY") => "¥",
        _ => "$",
    }
}

fn pct_color(pct: f64) -> &'static str {
    if pct >= 85.0 { "31" } else if pct >= 60.0 { "33" } else { "32" }
}

/// Build (segment, reset_segment) from the live endpoint's 5h window, or
/// return None if the live value is missing.
fn five_h_from_live(live: Option<&live_usage::LiveUsage>) -> Option<(String, String)> {
    let l = live?;
    let w = l.five_hour.as_ref()?;
    let pct = w.utilization;
    let color = pct_color(pct);
    // Only label as stale when the value is genuinely old (poller failed);
    // a freshly-disk-cached value a few seconds old is still "live".
    let tag = if l.is_stale() {
        format!(" \x1b[90m(stale {}m)\x1b[0m", l.age_seconds() / 60)
    } else {
        " \x1b[90m(live)\x1b[0m".to_string()
    };
    let seg = format!(
        "\x1b[36m5h\x1b[0m \x1b[{c}m{p:.0}%\x1b[0m{tag}",
        c = color,
        p = pct,
        tag = tag,
    );
    Some((seg, format_reset(w.resets_at)))
}

/// 5h reset-time segment only — used when the 5h label is hidden but we
/// still want to surface when the main meter frees up.
fn five_h_reset_from_live(live: Option<&live_usage::LiveUsage>) -> String {
    live.and_then(|l| l.five_hour.as_ref())
        .map(|w| format_reset(w.resets_at))
        .unwrap_or_default()
}

fn format_reset(resets_at: Option<DateTime<Utc>>) -> String {
    resets_at
        .map(|t| {
            let local = t.with_timezone(&Local);
            let remaining = (t - Utc::now()).num_minutes().max(0);
            format!(
                " \x1b[90m→\x1b[0m reset \x1b[33m{}\x1b[0m \x1b[90m({}m)\x1b[0m",
                local.format("%H:%M"),
                remaining
            )
        })
        .unwrap_or_default()
}

/// Local-JSONL fallback: compute 5h usage from transcripts + a token cap.
fn local_five_h_segment(agg: &stats::Aggregate) -> (String, String) {
    let five_h_tokens = agg.rolling_5h.total_tokens();
    let cap = five_h_cap_tokens();
    let pct = (five_h_tokens as f64 / cap as f64 * 100.0).min(999.0);
    let color = pct_color(pct);
    let over_suffix = if five_h_tokens > cap {
        let extra = stats::overage_cost_usd(&agg.five_h_records, cap);
        format!(" \x1b[31m+${:.2} extra\x1b[0m", extra)
    } else {
        String::new()
    };
    let seg = format!(
        "\x1b[36m5h\x1b[0m \x1b[{c}m{p:.0}%\x1b[0m ({}/{}) \x1b[90m(est)\x1b[0m{over_suffix}",
        fmt_tokens_compact(five_h_tokens),
        fmt_tokens_compact(cap),
        c = color,
        p = pct
    );
    (seg, format_reset(agg.five_h_resets_at))
}

fn write_status_cache(line: &str) -> Result<()> {
    let Some(path) = stats::status_cache_path() else { return Ok(()) };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("txt.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(line.as_bytes())?;
        f.sync_data().ok();
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Run forever: watch ~/.claude/projects and re-render the status cache on every JSONL change.
/// Writes at most once per 500ms (debounced) and at least once every 15s as a heartbeat.
pub fn run_forever(no_live: bool) -> Result<()> {
    let dir = stats::claude_projects_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    fs::create_dir_all(&dir).ok();

    // Seed the cache immediately so statusLine has something to show.
    let _ = write_status_cache(&render_status(None, None, no_live));

    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(tx)?;
    watcher.watch(&dir, RecursiveMode::Recursive)?;

    let mut dirty = false;
    let mut last_write = Instant::now() - Duration::from_secs(60);

    loop {
        // Wait up to 1s for any event so we stay responsive without busy-looping.
        let got = rx.recv_timeout(Duration::from_secs(1));
        match got {
            Ok(Ok(ev)) => {
                if ev.paths.iter().any(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl")) {
                    dirty = true;
                }
            }
            Ok(Err(_)) => {}
            Err(_) => {} // timeout; fall through to heartbeat check
        }
        // Drain any remaining events to coalesce bursts.
        while let Ok(more) = rx.try_recv() {
            if let Ok(ev) = more {
                if ev.paths.iter().any(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl")) {
                    dirty = true;
                }
            }
        }

        let since = last_write.elapsed();
        let should_write = (dirty && since > Duration::from_millis(500)) || since > Duration::from_secs(15);
        if should_write {
            let _ = write_status_cache(&render_status(None, None, no_live));
            last_write = Instant::now();
            dirty = false;
        }

        // If the cache file was manually deleted, recreate it on next tick.
        if let Some(path) = stats::status_cache_path() {
            if !path.exists() {
                let _ = write_status_cache(&render_status(None, None, no_live));
                last_write = Instant::now();
            }
        }
    }
}
