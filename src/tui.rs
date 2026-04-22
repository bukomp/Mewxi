//! The interactive ratatui dashboard.
//!
//! The main loop ([`run_loop`]) owns three inputs:
//!
//! 1. A [`notify`] watcher on `~/.claude/projects/` that marks the
//!    aggregate as dirty when any JSONL changes.
//! 2. A background poller thread ([`spawn_live_poller`]) that fetches
//!    the OAuth `/usage` endpoint on its own cadence and sends results
//!    back over an `mpsc` channel.
//! 3. Keyboard events (`q`/`Esc` to quit, `r` to force reload + live
//!    refetch).
//!
//! The aggregate is recomputed at most once per 500 ms when the
//! watcher has fired, and unconditionally at least every 5 s as a
//! safety net. Every frame draws from the last successful
//! [`stats::Aggregate`] + [`LiveUsage`].
//!
//! Derived metrics (burn rate, ETA to cap, overage, cache-hit ratio,
//! daily sparkline) are computed once per frame in [`compute_metrics`].
//! Layout switches between multi-column and stacked at
//! [`WIDE_THRESHOLD`] = 100 columns.

use crate::live_usage::{self, LiveUsage};
use crate::stats::{self, Aggregate, UsageRecord, UsageTotals, fmt_num};
use anyhow::Result;
use chrono::{Local, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table};
use ratatui::Frame;
use ratatui::Terminal;
use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// Width at which the layout switches from stacked (narrow) to multi-column (wide).
const WIDE_THRESHOLD: u16 = 100;
/// Token cap used for the local 5h estimate; must match watch::DEFAULT_5H_CAP_TOKENS.
const LOCAL_5H_CAP: u64 = 11_500_000;

/// Entry point for the `tui` subcommand. Enters the alternate screen,
/// runs the event loop until `q`/`Esc`, and always restores the
/// terminal on exit — even if the inner loop errored.
pub fn run(no_live: bool) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, no_live);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

enum LiveMsg {
    Update(Option<LiveUsage>),
}
enum LiveCmd {
    Refresh,
    Stop,
}

fn spawn_live_poller(no_live: bool) -> (Receiver<LiveMsg>, Sender<LiveCmd>) {
    let (out_tx, out_rx) = channel::<LiveMsg>();
    let (in_tx, in_rx) = channel::<LiveCmd>();
    thread::spawn(move || {
        let _ = out_tx.send(LiveMsg::Update(live_usage::load_cached()));
        let _ = out_tx.send(LiveMsg::Update(live_usage::fetch_or_cached(no_live)));
        loop {
            match in_rx.recv_timeout(live_usage::REFRESH_INTERVAL) {
                Ok(LiveCmd::Stop) => break,
                Ok(LiveCmd::Refresh) | Err(_) => {
                    let live = live_usage::fetch_or_cached(no_live);
                    if out_tx.send(LiveMsg::Update(live)).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (out_rx, in_tx)
}

fn run_loop<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, no_live: bool) -> Result<()> {
    let projects_dir = stats::claude_projects_dir().ok_or_else(|| anyhow::anyhow!("no home"))?;

    let (tx, rx): (_, Receiver<notify::Result<notify::Event>>) = channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(tx)?;
    if projects_dir.exists() {
        watcher.watch(&projects_dir, RecursiveMode::Recursive)?;
    }

    let (live_rx, live_cmd_tx) = spawn_live_poller(no_live);

    let mut agg = stats::load_and_aggregate().unwrap_or_default();
    let mut live: Option<LiveUsage> = None;
    let mut last_reload = Instant::now();
    let mut dirty = false;

    loop {
        terminal.draw(|f| render(f, &agg, live.as_ref()))?;

        while let Ok(LiveMsg::Update(l)) = live_rx.try_recv() {
            live = l;
        }
        while let Ok(ev) = rx.try_recv() {
            if let Ok(ev) = ev {
                if ev.paths.iter().any(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl")) {
                    dirty = true;
                }
            }
        }

        if (dirty && last_reload.elapsed() > Duration::from_millis(500))
            || last_reload.elapsed() > Duration::from_secs(5)
        {
            if let Ok(a) = stats::load_and_aggregate() {
                agg = a;
            }
            last_reload = Instant::now();
            dirty = false;
        }

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            let _ = live_cmd_tx.send(LiveCmd::Stop);
                            break;
                        }
                        KeyCode::Char('r') => {
                            if let Ok(a) = stats::load_and_aggregate() {
                                agg = a;
                            }
                            last_reload = Instant::now();
                            let _ = live_cmd_tx.send(LiveCmd::Refresh);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Derived metrics
// ---------------------------------------------------------------------------

struct Metrics {
    /// Tokens per hour over the most recent 15 minutes of activity (or the
    /// whole current block if less). Zero if nothing recent.
    recent_burn_tok_hr: f64,
    /// Cost per hour on the same window.
    recent_burn_usd_hr: f64,
    /// Minutes until the local 5h cap is hit at `recent_burn_tok_hr`.
    /// None when idle or already over the cap.
    eta_minutes_to_local_cap: Option<i64>,
    /// Last 14 calendar days of total tokens, oldest → newest.
    daily_14d_tokens: Vec<u64>,
    /// Peak single-day tokens across the 14d window (for sparkline caption).
    daily_14d_peak: u64,
    /// Average per active day across the 14d window.
    daily_14d_avg: u64,
    /// All-time cache_read as a fraction of all input-side tokens (0..1).
    cache_hit_ratio: f64,
    /// Estimated USD overage beyond the local 5h cap.
    overage_usd: f64,
    /// Sum of cost for the current 5h block.
    block_cost_usd: f64,
    /// Messages in the current 5h block.
    block_messages: u64,
}

fn compute_metrics(agg: &Aggregate) -> Metrics {
    let (recent_burn_tok_hr, recent_burn_usd_hr) = recent_burn(&agg.five_h_records);

    let eta_minutes_to_local_cap = if recent_burn_tok_hr > 0.0 {
        let current = agg.rolling_5h.total_tokens() as f64;
        if current >= LOCAL_5H_CAP as f64 {
            None
        } else {
            let hours = (LOCAL_5H_CAP as f64 - current) / recent_burn_tok_hr;
            let mut m = (hours * 60.0) as i64;
            if let Some(reset) = agg.five_h_resets_at {
                let window_left = (reset - Utc::now()).num_minutes().max(0);
                m = m.min(window_left);
            }
            Some(m)
        }
    } else {
        None
    };

    let daily_14d_tokens = daily_series(agg, 14);
    let daily_14d_peak = daily_14d_tokens.iter().copied().max().unwrap_or(0);
    let active_days: Vec<u64> = daily_14d_tokens.iter().copied().filter(|n| *n > 0).collect();
    let daily_14d_avg = if active_days.is_empty() {
        0
    } else {
        active_days.iter().sum::<u64>() / active_days.len() as u64
    };

    let t = &agg.all;
    let input_like = t.input + t.cache_read + t.cache_write_5m + t.cache_write_1h;
    let cache_hit_ratio = if input_like > 0 {
        t.cache_read as f64 / input_like as f64
    } else {
        0.0
    };

    let overage_usd = stats::overage_cost_usd(&agg.five_h_records, LOCAL_5H_CAP);

    Metrics {
        recent_burn_tok_hr,
        recent_burn_usd_hr,
        eta_minutes_to_local_cap,
        daily_14d_tokens,
        daily_14d_peak,
        daily_14d_avg,
        cache_hit_ratio,
        overage_usd,
        block_cost_usd: agg.rolling_5h.cost_usd,
        block_messages: agg.rolling_5h.messages,
    }
}

/// (tokens/hr, usd/hr) averaged over the last 15 min of the block.
/// Uses the whole block when less than 15 min of activity exists.
fn recent_burn(records: &[UsageRecord]) -> (f64, f64) {
    if records.is_empty() {
        return (0.0, 0.0);
    }
    let now = Utc::now();
    let cutoff = now - chrono::Duration::minutes(15);
    let recent: Vec<&UsageRecord> = records.iter().filter(|r| r.timestamp >= cutoff).collect();
    let slice: &[&UsageRecord] = if recent.is_empty() {
        // No messages in last 15 min: don't show a bogus sustained burn rate.
        return (0.0, 0.0);
    } else {
        &recent
    };
    let first_ts = slice.first().unwrap().timestamp;
    // Clamp elapsed to at least 1 minute to avoid divide-by-tiny spikes.
    let elapsed_hours = ((now - first_ts).num_seconds().max(60) as f64) / 3600.0;
    let tok: u64 = slice.iter().map(|r| r.total_tokens()).sum();
    let cost: f64 = slice.iter().map(|r| r.cost_usd).sum();
    (tok as f64 / elapsed_hours, cost / elapsed_hours)
}

fn daily_series(agg: &Aggregate, days: usize) -> Vec<u64> {
    let today = Local::now().date_naive();
    (0..days as i64)
        .rev()
        .map(|i| {
            let d = today - chrono::Duration::days(i);
            agg.by_day.get(&d).map(|t| t.total_tokens()).unwrap_or(0)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, agg: &Aggregate, live: Option<&LiveUsage>) {
    let area = f.area();
    let m = compute_metrics(agg);
    if area.width < WIDE_THRESHOLD {
        render_narrow(f, area, agg, live, &m);
    } else {
        render_wide(f, area, agg, live, &m);
    }
}

fn render_wide(
    f: &mut Frame,
    area: Rect,
    agg: &Aggregate,
    live: Option<&LiveUsage>,
    m: &Metrics,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(4), // three gauges
            Constraint::Length(8), // burn / sparkline / cache
            Constraint::Min(6),    // by project
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_header(f, rows[0], agg, live);

    let gauge_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(rows[1]);
    render_5h_gauge(f, gauge_row[0], agg, live);
    render_7d_gauge(f, gauge_row[1], live);
    render_extra_gauge(f, gauge_row[2], live);

    let stats_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
        ])
        .split(rows[2]);
    render_burn_panel(f, stats_row[0], m);
    render_sparkline_panel(f, stats_row[1], m);
    render_efficiency_panel(f, stats_row[2], agg, m);

    render_by_project(f, rows[3], agg);
    render_footer(f, rows[4]);
}

fn render_narrow(
    f: &mut Frame,
    area: Rect,
    agg: &Aggregate,
    live: Option<&LiveUsage>,
    m: &Metrics,
) {
    // Decide what fits. Height budget:
    //   header 3 + 3 gauges × 3 + burn 5 + sparkline 4 + projects min 5 + footer 1 = 27
    // If shorter, drop sparkline then projects.
    let h = area.height;
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(3), // header
        Constraint::Length(3), // 5h gauge
        Constraint::Length(3), // 7d gauge
        Constraint::Length(3), // extra gauge
        Constraint::Length(5), // burn + efficiency merged
    ];
    let mut keys = vec!["header", "5h", "7d", "extra", "burn"];
    if h >= 24 {
        constraints.push(Constraint::Length(4));
        keys.push("sparkline");
    }
    if h >= 22 {
        constraints.push(Constraint::Min(4));
        keys.push("projects");
    }
    constraints.push(Constraint::Length(1));
    keys.push("footer");

    let rects = Layout::default().direction(Direction::Vertical).constraints(constraints).split(area);

    for (rect, key) in rects.iter().zip(keys.iter()) {
        match *key {
            "header" => render_header(f, *rect, agg, live),
            "5h" => render_5h_gauge(f, *rect, agg, live),
            "7d" => render_7d_gauge(f, *rect, live),
            "extra" => render_extra_gauge(f, *rect, live),
            "burn" => render_burn_and_efficiency_narrow(f, *rect, agg, m),
            "sparkline" => render_sparkline_panel(f, *rect, m),
            "projects" => render_by_project(f, *rect, agg),
            "footer" => render_footer(f, *rect),
            _ => {}
        }
    }
}

fn render_header(f: &mut Frame, area: Rect, agg: &Aggregate, live: Option<&LiveUsage>) {
    let source: String = match live {
        Some(l) if l.is_stale() => format!("• live: stale {}m", l.age_seconds() / 60),
        Some(l) => {
            let age = l.age_seconds();
            if age < 90 {
                "• live: fresh".to_string()
            } else {
                format!("• live: cached {}m", age / 60)
            }
        }
        None => "• live: off".to_string(),
    };
    let line = Line::from(vec![
        Span::styled("Claude Usage", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("   sessions "),
        Span::styled(agg.sessions_count.to_string(), Style::default().fg(Color::Yellow)),
        Span::raw("   projects "),
        Span::styled(agg.projects_count.to_string(), Style::default().fg(Color::Yellow)),
        Span::raw("   total "),
        Span::styled(format!("${:.2}", agg.all.cost_usd), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(source, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line).block(Block::default().borders(Borders::ALL)), area);
}

fn gauge_color(pct: f64) -> Color {
    if pct >= 85.0 {
        Color::Red
    } else if pct >= 60.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn render_5h_gauge(f: &mut Frame, area: Rect, agg: &Aggregate, live: Option<&LiveUsage>) {
    let (pct, suffix, source_tag) = match live.and_then(|l| l.five_hour.as_ref()) {
        Some(w) => {
            let reset = w
                .resets_at
                .map(|t| {
                    let remaining = (t - Utc::now()).num_minutes().max(0);
                    format!("  reset {} ({}m)", t.with_timezone(&Local).format("%H:%M"), remaining)
                })
                .unwrap_or_default();
            let tag: String = match live {
                Some(l) if l.is_stale() => format!("5h (stale {}m)", l.age_seconds() / 60),
                _ => "5h (live)".to_string(),
            };
            (w.utilization, reset, tag)
        }
        None => {
            let p = (agg.rolling_5h.total_tokens() as f64 / LOCAL_5H_CAP as f64 * 100.0).min(999.0);
            let reset = agg
                .five_h_resets_at
                .map(|t| {
                    let remaining = (t - Utc::now()).num_minutes().max(0);
                    format!("  reset {} ({}m)", t.with_timezone(&Local).format("%H:%M"), remaining)
                })
                .unwrap_or_default();
            (p, reset, "5h (estimate)".to_string())
        }
    };
    let ratio = (pct / 100.0).clamp(0.0, 1.0);
    let label = format!("{:.1}%{}", pct, suffix);
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(source_tag))
        .gauge_style(Style::default().fg(gauge_color(pct)))
        .ratio(ratio)
        .label(label);
    f.render_widget(gauge, area);
}

fn render_7d_gauge(f: &mut Frame, area: Rect, live: Option<&LiveUsage>) {
    match live.and_then(|l| l.seven_day.as_ref()) {
        Some(w) => {
            let reset = w
                .resets_at
                .map(|t| format!("  reset {}", t.with_timezone(&Local).format("%a %H:%M")))
                .unwrap_or_default();
            let ratio = (w.utilization / 100.0).clamp(0.0, 1.0);
            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL).title("Weekly"))
                .gauge_style(Style::default().fg(gauge_color(w.utilization)))
                .ratio(ratio)
                .label(format!("{:.1}%{}", w.utilization, reset));
            f.render_widget(gauge, area);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "no live data",
                Style::default().fg(Color::DarkGray),
            )))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("Weekly"));
            f.render_widget(p, area);
        }
    }
}

fn render_extra_gauge(f: &mut Frame, area: Rect, live: Option<&LiveUsage>) {
    let extra = live.and_then(|l| l.extra_usage.as_ref());
    match extra {
        Some(e) if e.is_enabled => {
            let pct = e.utilization.unwrap_or(0.0);
            let sym = currency_symbol(e.currency.as_deref());
            // Server reports cents.
            let used = e.used_credits.unwrap_or(0.0) / 100.0;
            let limit = e.monthly_limit.unwrap_or(0.0) / 100.0;
            let ratio = (pct / 100.0).clamp(0.0, 1.0);
            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL).title("Extra usage"))
                .gauge_style(Style::default().fg(gauge_color(pct)))
                .ratio(ratio)
                .label(format!("{:.1}%  {sym}{:.2} / {sym}{:.2}", pct, used, limit));
            f.render_widget(gauge, area);
        }
        _ => {
            let msg = if extra.is_some() { "disabled" } else { "no live data" };
            let p = Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(Color::DarkGray),
            )))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("Extra usage"));
            f.render_widget(p, area);
        }
    }
}

fn render_burn_panel(f: &mut Frame, area: Rect, m: &Metrics) {
    let lines = vec![
        kv_line("tok/hr", &fmt_tokens_compact(m.recent_burn_tok_hr as u64), Color::Cyan),
        kv_line("$/hr", &format!("${:.2}", m.recent_burn_usd_hr), Color::Green),
        kv_line(
            "ETA to cap",
            &match m.eta_minutes_to_local_cap {
                Some(mins) if mins > 0 => fmt_duration_minutes(mins),
                Some(_) => "—".into(),
                None => "idle".into(),
            },
            Color::Yellow,
        ),
        kv_line("block cost", &format!("${:.2}", m.block_cost_usd), Color::Green),
        kv_line("block msgs", &fmt_num(m.block_messages), Color::White),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Burn rate (last 15m)"));
    f.render_widget(p, area);
}

fn render_sparkline_panel(f: &mut Frame, area: Rect, m: &Metrics) {
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let spark = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Daily tokens (14d)"),
        )
        .data(&m.daily_14d_tokens)
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(spark, split[0]);
    let caption = Line::from(vec![
        Span::raw(" peak "),
        Span::styled(fmt_tokens_compact(m.daily_14d_peak), Style::default().fg(Color::Yellow)),
        Span::raw("   avg "),
        Span::styled(fmt_tokens_compact(m.daily_14d_avg), Style::default().fg(Color::Green)),
        Span::raw("   today "),
        Span::styled(
            fmt_tokens_compact(*m.daily_14d_tokens.last().unwrap_or(&0)),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    f.render_widget(Paragraph::new(caption), split[1]);
}

fn render_efficiency_panel(f: &mut Frame, area: Rect, agg: &Aggregate, m: &Metrics) {
    let hit_pct = m.cache_hit_ratio * 100.0;
    let hit_color = if hit_pct >= 70.0 {
        Color::Green
    } else if hit_pct >= 40.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    let avg_cost_per_msg = if agg.all.messages > 0 {
        agg.all.cost_usd / agg.all.messages as f64
    } else {
        0.0
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("cache hit ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{hit_pct:>5.1}%"), Style::default().fg(hit_color).add_modifier(Modifier::BOLD)),
        ]),
        kv_line("5h overage", &format!("${:.2}", m.overage_usd),
            if m.overage_usd > 0.0 { Color::Red } else { Color::Green }),
        kv_line("$/msg avg", &format!("${:.4}", avg_cost_per_msg), Color::White),
        kv_line("all-time cost", &format!("${:.2}", agg.all.cost_usd), Color::Green),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Efficiency"));
    f.render_widget(p, area);
}

fn render_burn_and_efficiency_narrow(f: &mut Frame, area: Rect, agg: &Aggregate, m: &Metrics) {
    let hit_pct = m.cache_hit_ratio * 100.0;
    let eta = match m.eta_minutes_to_local_cap {
        Some(mins) if mins > 0 => fmt_duration_minutes(mins),
        Some(_) => "—".into(),
        None => "idle".into(),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("burn ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                fmt_tokens_compact(m.recent_burn_tok_hr as u64),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw("/hr  "),
            Span::styled(
                format!("${:.2}/hr", m.recent_burn_usd_hr),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::from(vec![
            Span::styled("ETA to cap ", Style::default().fg(Color::DarkGray)),
            Span::styled(eta, Style::default().fg(Color::Yellow)),
            Span::raw("    overage "),
            Span::styled(
                format!("${:.2}", m.overage_usd),
                Style::default().fg(if m.overage_usd > 0.0 { Color::Red } else { Color::Green }),
            ),
        ]),
        Line::from(vec![
            Span::styled("cache hit ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{hit_pct:>5.1}%"),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("    total "),
            Span::styled(
                format!("${:.2}", agg.all.cost_usd),
                Style::default().fg(Color::Green),
            ),
        ]),
    ];
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Usage")),
        area,
    );
}

fn render_by_project(f: &mut Frame, area: Rect, agg: &Aggregate) {
    let mut rows: Vec<(&String, &UsageTotals)> = agg.by_project.iter().collect();
    rows.sort_by(|a, b| b.1.cost_usd.partial_cmp(&a.1.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
    let max_cost = rows.first().map(|(_, t)| t.cost_usd).unwrap_or(0.0).max(0.01);
    let height = area.height.saturating_sub(3) as usize;

    // Tiny inline bar made of block chars — scales to the top row.
    fn bar(cost: f64, max: f64, width: usize) -> String {
        let ratio = (cost / max).clamp(0.0, 1.0);
        let filled = (ratio * width as f64).round() as usize;
        "█".repeat(filled) + &"░".repeat(width.saturating_sub(filled))
    }

    let rs: Vec<Row> = rows
        .into_iter()
        .take(height.max(1))
        .map(|(p, t)| {
            Row::new(vec![
                Cell::from(p.clone()),
                Cell::from(fmt_num(t.total_tokens())),
                Cell::from(bar(t.cost_usd, max_cost, 16)).style(Style::default().fg(Color::Cyan)),
                Cell::from(format!("${:.2}", t.cost_usd)).style(Style::default().fg(Color::Green)),
            ])
        })
        .collect();
    let table = Table::new(
        rs,
        [
            Constraint::Min(14),
            Constraint::Length(12),
            Constraint::Length(18),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(vec!["project", "tokens", "", "cost"])
            .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title("By project"));
    f.render_widget(table, area);
}

fn render_footer(f: &mut Frame, area: Rect) {
    let p = Paragraph::new(Line::from(vec![
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" quit  "),
        Span::styled(" r ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" reload + refresh  "),
        Span::styled(
            "live via api.anthropic.com/api/oauth/usage",
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    f.render_widget(p, area);
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn kv_line(key: impl Into<String>, value: impl Into<String>, color: Color) -> Line<'static> {
    let k = key.into();
    Line::from(vec![
        Span::styled(format!("{k:<13}"), Style::default().fg(Color::DarkGray)),
        Span::styled(value.into(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ])
}

fn fmt_tokens_compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_duration_minutes(mins: i64) -> String {
    if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
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
