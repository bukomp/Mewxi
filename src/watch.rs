//! Status-line renderer + background watcher (multi-account).
//!
//! [`render_status`] builds the one-line ANSI-coloured string shown in
//! Claude Code's `statusLine`, scoped to whichever account owns the
//! active transcript. It composes up to three segments:
//!
//! - 5h window (live if available, local estimate otherwise) + reset time.
//! - Active-extra-usage segment (promotes itself to leading position
//!   and hides the 5h % once the current 5h window is at its cap).
//! - Per-session context (`ctx N%`) when a transcript is in scope.
//!
//! When more than one account is configured the line is prefixed with
//! the account name in brackets, e.g. `[priv] 5h …`.
//!
//! [`run_forever`] is the `watch` subcommand: it subscribes to JSONL
//! change events under every account's `projects/` dir, debounces to
//! at most one write per 500 ms, heartbeats every 15 s, and atomically
//! renames `status-<slug>.txt.tmp` → `status-<slug>.txt`. A single
//! `status.txt` mirror of the most-recently-modified account is kept
//! for back-compat with existing statusLine hooks.

use crate::accounts::{self, Account};
use crate::live_usage;
use crate::stats;
use crate::update;
use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

/// 5h cap in tokens. Overridable via env var to match your plan.
/// Defaults to Max 5× (~11.5M tokens — calibrated against Claude Code's /usage display).
/// Pro ≈ 2.3M, Max 20× ≈ 46M.
const DEFAULT_5H_CAP_TOKENS: u64 = 11_500_000;

fn five_h_cap_tokens() -> u64 {
    std::env::var("MEWXI_5H_CAP_TOKENS")
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

/// Compact a model's `display_name` for the statusline. Claude Code now
/// sends verbose labels like `Opus 4.8 (1M context)`; we shorten the
/// extended-context parenthetical to a bare `1M` so the segment stays
/// narrow (`Opus 4.8 1M`).
fn compact_model_name(name: &str) -> String {
    if let Some(idx) = name.find(" (1M context)") {
        format!("{} 1M{}", &name[..idx], &name[idx + " (1M context)".len()..])
    } else {
        name.to_string()
    }
}

/// Optional per-session metadata Claude Code passes to `mewxi status`
/// on stdin (the "Status" hook payload). Every field is absent when the
/// line is rendered by the watcher (no stdin) or seeded during setup.
#[derive(Default, Clone, Copy)]
pub struct SessionMeta<'a> {
    /// `model.id` — the configured alias (may contain `[1m]`). Drives
    /// the context-window cap heuristic.
    pub model_alias: Option<&'a str>,
    /// `model.display_name` — short human label, e.g. "Opus".
    pub model_display: Option<&'a str>,
    /// `thinking.enabled` — whether extended thinking is on this turn.
    pub thinking_enabled: bool,
    /// `effort.level` — reasoning effort: low|medium|high|xhigh|max.
    pub effort_level: Option<&'a str>,
}

/// Render the current usage as an ANSI-colored one-liner for Claude
/// Code's statusLine. Picks the account that owns `transcript_path`;
/// falls back to the configured default. When more than one account is
/// known, the line is prefixed with `[name]`.
pub fn render_status(
    transcript_path: Option<&Path>,
    meta: SessionMeta<'_>,
    no_live: bool,
) -> String {
    let view = match accounts::load_accounts() {
        Ok(v) => v,
        Err(e) => return format!("\x1b[31mmewxi: {e}\x1b[0m"),
    };

    let account = match transcript_path
        .and_then(|p| accounts::account_for_transcript(&view.accounts, p))
    {
        Some(a) => a,
        None => match view.pick(None) {
            Some(a) => a,
            None => return String::new(),
        },
    };

    render_status_for_account(account, view.accounts.len() > 1, transcript_path, meta, no_live)
}

pub(crate) fn render_status_for_account(
    account: &Account,
    prefix_name: bool,
    transcript_path: Option<&Path>,
    meta: SessionMeta<'_>,
    no_live: bool,
) -> String {
    let agg = stats::load_and_aggregate_for(account).unwrap_or_default();
    let live = live_usage::fetch_or_cached(account, no_live);

    // Promote `extra` to the lead only once the *current* 5h window is at its
    // cap. `extra_usage.used_credits` accumulates over the billing period, so
    // testing it alone hides the 5h meter for the rest of the month after the
    // first extra credit is ever spent — even when the current 5h window is
    // fresh. Require both signals: credits actively burning AND 5h at cap.
    // When promoted, the 5h label+pct is dropped but the reset time stays so
    // the user still sees when the main meter frees up.
    let five_h_at_cap = live
        .as_ref()
        .and_then(|l| l.five_hour.as_ref())
        .is_some_and(|w| w.utilization >= 100.0);

    let billing_extra = five_h_at_cap
        && live
            .as_ref()
            .and_then(|l| l.extra_usage.as_ref())
            .filter(|e| e.is_enabled)
            .and_then(|e| e.used_credits)
            .is_some_and(|c| c > 0.0);

    // --- 5h window segment -------------------------------------------------
    let (five_h_segment, reset_segment) = if billing_extra {
        (String::new(), five_h_reset_from_live(live.as_ref()))
    } else {
        match five_h_from_live(live.as_ref()) {
            Some((seg, reset)) => (seg, reset),
            None => local_five_h_segment(&agg),
        }
    };

    // --- Extra usage segment (only when actually billing) -----------------
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
                    "\x1b[36mextra\x1b[0m \x1b[{c}m{p:.1}%\x1b[0m \x1b[90m({sym}{:.2}/{sym}{:.2})\x1b[0m",
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
    let session_id = transcript_path
        .and_then(|p| p.file_stem())
        .and_then(|s| s.to_str())
        .map(str::to_string);
    // If Claude Code told us this session is on `[1m]`, persist that fact
    // so the TUI (which doesn't see stdin) renders the same cap. Without
    // this, ctx% in the all-sessions table can read ~5x higher than the
    // statusline until any single message crosses 200K tokens.
    if let (Some(alias), Some(sid)) = (meta.model_alias, session_id.as_deref()) {
        if alias.contains("[1m]") {
            stats::mark_extended_context(account, sid);
        }
    }
    // Persist the reported reasoning effort for the same reason: the TUI
    // never sees this stdin payload, so without a per-session record its
    // all-sessions table shows every session the account-global default.
    if let (Some(eff), Some(sid)) = (
        meta.effort_level.filter(|s| !s.is_empty()),
        session_id.as_deref(),
    ) {
        stats::mark_session_effort(account, sid, eff);
    }
    let ctx_segment = transcript_path
        .and_then(stats::current_context_from_transcript)
        .map(|sc| {
            let cap = stats::context_cap_for(
                &sc.model,
                sc.max_observed,
                meta.model_alias,
                account,
                session_id.as_deref(),
            );
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

    let prefix = if prefix_name {
        format!("\x1b[35m[{}]\x1b[0m ", account.name)
    } else {
        String::new()
    };

    // --- Model + thinking segment -----------------------------------------
    // Only present on the live `mewxi status` path (the watcher has no
    // stdin). Shows e.g. `Opus · think:high |` or just `Opus |`.
    let model_segment = match meta.model_display {
        Some(name) if !name.is_empty() => {
            let name = compact_model_name(name);
            let think = if meta.thinking_enabled {
                let lvl = meta.effort_level.filter(|s| !s.is_empty()).unwrap_or("on");
                format!(" \x1b[90m·\x1b[0m \x1b[35mthink:{lvl}\x1b[0m")
            } else {
                String::new()
            };
            format!("\x1b[36m{name}\x1b[0m{think} \x1b[90m|\x1b[0m ")
        }
        _ => String::new(),
    };

    // Small "mewxi has an update" notice, fed from the cached update
    // check — surfaces inside every Claude Code session's statusline.
    // Rendered as a leading segment (right after any setup-incomplete
    // hint) so the update nudge survives narrow-terminal truncation.
    let update_segment = crate::update::statusline_segment().unwrap_or_default();

    // Nudge to open the TUI when setup looks incomplete (an account
    // isn't wired, or the watcher daemon's heartbeat has gone stale).
    // The probe is filesystem-only — safe to run on every refresh.
    // Rendered as the leading segment so it stays visible even when the
    // statusline is truncated on narrow terminals.
    let hint_segment = if crate::setup::setup_incomplete() {
        "\x1b[33m⚠ mewxi: setup incomplete — open mewxi\x1b[0m \x1b[90m|\x1b[0m ".to_string()
    } else {
        String::new()
    };

    if billing_extra {
        format!("{hint_segment}{update_segment}{prefix}{model_segment}{extra_segment}{reset_segment}{ctx_segment}")
    } else {
        format!("{hint_segment}{update_segment}{prefix}{model_segment}{five_h_segment}{reset_segment}{ctx_segment}")
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

fn five_h_from_live(live: Option<&live_usage::LiveUsage>) -> Option<(String, String)> {
    let l = live?;
    let w = l.five_hour.as_ref()?;
    let pct = w.utilization;
    let color = pct_color(pct);
    let tag = if l.is_stale() {
        format!(" \x1b[90m(stale {}m)\x1b[0m", l.age_seconds() / 60)
    } else {
        " \x1b[90m(live)\x1b[0m".to_string()
    };
    let seg = format!(
        "\x1b[36m5h\x1b[0m \x1b[{c}m{p:.1}%\x1b[0m{tag}",
        c = color,
        p = pct,
        tag = tag,
    );
    Some((seg, format_reset(w.resets_at)))
}

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

fn write_status_cache(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("txt.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(line.as_bytes())?;
        f.sync_data().ok();
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Render and persist `status-<slug>.txt` for `account`, plus update the
/// `status.txt` mirror so single-statusLine deployments still get the
/// most-recently-modified account.
fn write_account_status(account: &Account, prefix_name: bool, no_live: bool) {
    let line = render_status_for_account(account, prefix_name, None, SessionMeta::default(), no_live);
    if let Some(p) = stats::status_cache_path_for(account) {
        let _ = write_status_cache(&p, &line);
    }
    if let Some(mirror) = stats::status_cache_path_mirror() {
        let _ = write_status_cache(&mirror, &line);
    }
}

/// Run forever: watch every discovered account's `projects/` and
/// rewrite its status cache on every JSONL change. One watcher thread
/// per account; the main thread heartbeats every 15 s.
pub fn run_forever(no_live: bool) -> Result<()> {
    let view = accounts::load_accounts()?;
    let prefix_name = view.accounts.len() > 1;

    // Seed once so statusLine has something to show immediately.
    for account in &view.accounts {
        fs::create_dir_all(account.projects_dir()).ok();
        write_account_status(account, prefix_name, no_live);
    }

    // mpsc channel for cross-thread "this account is dirty" pings.
    let (dirty_tx, dirty_rx) = channel::<String>();
    let mut watchers: Vec<RecommendedWatcher> = Vec::new();
    for account in &view.accounts {
        let dir = account.projects_dir();
        if !dir.exists() {
            continue;
        }
        let acct_name = account.name.clone();
        let tx: Sender<String> = dirty_tx.clone();
        let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if ev.paths.iter().any(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl")) {
                    let _ = tx.send(acct_name.clone());
                }
            }
        })?;
        watcher.watch(&dir, RecursiveMode::Recursive)?;
        watchers.push(watcher);
    }
    drop(dirty_tx);

    let mut dirty: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_write_by_account: std::collections::HashMap<String, Instant> =
        std::collections::HashMap::new();
    let mut last_heartbeat = Instant::now();
    // Keep the update-check cache warm so the statusLine notice stays
    // honest even when the TUI is never opened. refresh_cache_async is
    // a no-op while the cache is fresh; the Instant gate just stops us
    // re-spawning a checker thread every 6h-heartbeat when the check
    // itself keeps failing (offline, ssh agent absent in launchd, …).
    update::refresh_cache_async();
    let mut last_update_refresh = Instant::now();

    loop {
        match dirty_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(name) => {
                dirty.insert(name);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Drain remaining pings.
        while let Ok(name) = dirty_rx.try_recv() {
            dirty.insert(name);
        }

        // Per-account debounce: write only if >500ms since the last write
        // for that account.
        let mut wrote_any = false;
        if !dirty.is_empty() {
            let drained: Vec<String> = dirty.drain().collect();
            for name in drained {
                let Some(account) = view.accounts.iter().find(|a| a.name == name) else {
                    continue;
                };
                let recent = last_write_by_account
                    .get(&name)
                    .map(|t| t.elapsed() < Duration::from_millis(500))
                    .unwrap_or(false);
                if recent {
                    dirty.insert(name);
                    continue;
                }
                write_account_status(account, prefix_name, no_live);
                last_write_by_account.insert(name, Instant::now());
                wrote_any = true;
            }
        }

        // Heartbeat: at least one full pass every 15s so stale caches and
        // reset-time labels stay current even when no JSONL events fire.
        if last_heartbeat.elapsed() > Duration::from_secs(15) {
            for account in &view.accounts {
                write_account_status(account, prefix_name, no_live);
                last_write_by_account.insert(account.name.clone(), Instant::now());
            }
            last_heartbeat = Instant::now();
            wrote_any = true;
        }

        // Nudge the update cache once a minute; refresh_cache_async
        // itself enforces the configured `update_interval` (15m–24h)
        // via cache freshness, so this only bounds how often the
        // config + cache files get re-read.
        if last_update_refresh.elapsed() > Duration::from_secs(60) {
            update::refresh_cache_async();
            last_update_refresh = Instant::now();
        }

        // Restore caches that were manually deleted.
        if !wrote_any {
            let mut missing: Vec<&Account> = Vec::new();
            for account in &view.accounts {
                let Some(p) = stats::status_cache_path_for(account) else { continue };
                if !p.exists() {
                    missing.push(account);
                }
            }
            for account in missing {
                write_account_status(account, prefix_name, no_live);
                last_write_by_account.insert(account.name.clone(), Instant::now());
            }
            if let Some(mirror) = stats::status_cache_path_mirror() {
                if !mirror.exists() {
                    if let Some(first) = view.accounts.first() {
                        write_account_status(first, prefix_name, no_live);
                    }
                }
            }
        }
    }
    Ok(())
}

