//! Shared widgets and metric helpers used by every TUI view.
//!
//! Extracted from the original single-pane `tui.rs` so each view can
//! reuse the same gauges, sparklines, and formatters without copy.

use crate::live_usage::LiveUsage;
use crate::stats::{self, Aggregate, UsageRecord, UsageTotals, fmt_num};
use chrono::{Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table};

/// Token cap used for the local 5h estimate; must match watch::DEFAULT_5H_CAP_TOKENS.
pub const LOCAL_5H_CAP: u64 = 11_500_000;

pub struct Metrics {
    pub recent_burn_tok_hr: f64,
    pub recent_burn_usd_hr: f64,
    pub eta_minutes_to_local_cap: Option<i64>,
    pub daily_14d_tokens: Vec<u64>,
    pub daily_14d_peak: u64,
    pub daily_14d_avg: u64,
    pub cache_hit_ratio: f64,
    pub overage_usd: f64,
    pub block_cost_usd: f64,
    pub block_messages: u64,
}

pub fn compute_metrics(agg: &Aggregate) -> Metrics {
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
pub fn recent_burn(records: &[UsageRecord]) -> (f64, f64) {
    if records.is_empty() {
        return (0.0, 0.0);
    }
    let now = Utc::now();
    let cutoff = now - chrono::Duration::minutes(15);
    let recent: Vec<&UsageRecord> = records.iter().filter(|r| r.timestamp >= cutoff).collect();
    if recent.is_empty() {
        return (0.0, 0.0);
    }
    let first_ts = recent.first().unwrap().timestamp;
    let elapsed_hours = ((now - first_ts).num_seconds().max(60) as f64) / 3600.0;
    let tok: u64 = recent.iter().map(|r| r.total_tokens()).sum();
    let cost: f64 = recent.iter().map(|r| r.cost_usd).sum();
    (tok as f64 / elapsed_hours, cost / elapsed_hours)
}

pub fn daily_series(agg: &Aggregate, days: usize) -> Vec<u64> {
    let today = Local::now().date_naive();
    (0..days as i64)
        .rev()
        .map(|i| {
            let d = today - chrono::Duration::days(i);
            agg.by_day.get(&d).map(|t| t.total_tokens()).unwrap_or(0)
        })
        .collect()
}

pub fn gauge_color(pct: f64) -> Color {
    if pct >= 85.0 {
        Color::Red
    } else if pct >= 60.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

pub fn currency_symbol(code: Option<&str>) -> &'static str {
    match code.map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("USD") => "$",
        Some("EUR") => "€",
        Some("GBP") => "£",
        Some("JPY") => "¥",
        _ => "$",
    }
}

pub fn fmt_tokens_compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub fn fmt_duration_minutes(mins: i64) -> String {
    if mins >= 60 {
        format!("{}h {}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    }
}

pub fn kv_line(key: impl Into<String>, value: impl Into<String>, color: Color) -> Line<'static> {
    let k = key.into();
    Line::from(vec![
        Span::styled(format!("{k:<13}"), Style::default().fg(Color::DarkGray)),
        Span::styled(value.into(), Style::default().fg(color).add_modifier(Modifier::BOLD)),
    ])
}

// --- Render functions ------------------------------------------------------

pub fn render_header(
    f: &mut Frame,
    area: Rect,
    title: &str,
    agg: &Aggregate,
    live: Option<&LiveUsage>,
) {
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
        Span::styled(title.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
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

pub fn render_5h_gauge(f: &mut Frame, area: Rect, agg: &Aggregate, live: Option<&LiveUsage>) {
    let (pct, suffix, source_tag) = match live.and_then(|l| l.five_hour.as_ref()) {
        Some(w) => {
            let reset = w
                .resets_at
                .map(|t| {
                    let remaining = (t - Utc::now()).num_minutes().max(0);
                    format!("  reset {} ({:>3}m)", t.with_timezone(&Local).format("%H:%M"), remaining)
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
                    format!("  reset {} ({:>3}m)", t.with_timezone(&Local).format("%H:%M"), remaining)
                })
                .unwrap_or_default();
            (p, reset, "5h (estimate)".to_string())
        }
    };
    let ratio = (pct / 100.0).clamp(0.0, 1.0);
    let label = format!("{:.1}%{}", pct, suffix);
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(source_tag))
        .gauge_style(Style::default().fg(gauge_color(pct)).bg(Color::Black))
        .ratio(ratio)
        .label(label);
    f.render_widget(gauge, area);
}

pub fn render_7d_gauge(f: &mut Frame, area: Rect, live: Option<&LiveUsage>) {
    match live.and_then(|l| l.seven_day.as_ref()) {
        Some(w) => {
            let reset = w
                .resets_at
                .map(|t| format!("  reset {}", t.with_timezone(&Local).format("%a %H:%M")))
                .unwrap_or_default();
            let ratio = (w.utilization / 100.0).clamp(0.0, 1.0);
            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL).title("Weekly"))
                .gauge_style(Style::default().fg(gauge_color(w.utilization)).bg(Color::Black))
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

pub fn render_extra_gauge(f: &mut Frame, area: Rect, live: Option<&LiveUsage>) {
    let extra = live.and_then(|l| l.extra_usage.as_ref());
    match extra {
        Some(e) if e.is_enabled => {
            let pct = e.utilization.unwrap_or(0.0);
            let sym = currency_symbol(e.currency.as_deref());
            let used = e.used_credits.unwrap_or(0.0) / 100.0;
            let limit = e.monthly_limit.unwrap_or(0.0) / 100.0;
            let ratio = (pct / 100.0).clamp(0.0, 1.0);
            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL).title("Extra usage"))
                .gauge_style(Style::default().fg(gauge_color(pct)).bg(Color::Black))
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

pub fn render_burn_panel(f: &mut Frame, area: Rect, m: &Metrics) {
    let lines = vec![
        kv_line("tok/hr", fmt_tokens_compact(m.recent_burn_tok_hr as u64), Color::Cyan),
        kv_line("$/hr", format!("${:.2}", m.recent_burn_usd_hr), Color::Green),
        kv_line(
            "ETA to cap",
            match m.eta_minutes_to_local_cap {
                Some(mins) if mins > 0 => fmt_duration_minutes(mins),
                Some(_) => "—".into(),
                None => "idle".into(),
            },
            Color::Yellow,
        ),
        kv_line("block cost", format!("${:.2}", m.block_cost_usd), Color::Green),
        kv_line("block msgs", fmt_num(m.block_messages), Color::White),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Burn rate (last 15m)"));
    f.render_widget(p, area);
}

pub fn render_sparkline_panel(f: &mut Frame, area: Rect, m: &Metrics) {
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

pub fn render_efficiency_panel(f: &mut Frame, area: Rect, agg: &Aggregate, m: &Metrics) {
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
        kv_line("5h overage", format!("${:.2}", m.overage_usd),
            if m.overage_usd > 0.0 { Color::Red } else { Color::Green }),
        kv_line("$/msg avg", format!("${:.4}", avg_cost_per_msg), Color::White),
        kv_line("all-time cost", format!("${:.2}", agg.all.cost_usd), Color::Green),
    ];
    let p = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Efficiency"));
    f.render_widget(p, area);
}

pub fn render_burn_and_efficiency_narrow(f: &mut Frame, area: Rect, agg: &Aggregate, m: &Metrics) {
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

pub fn render_by_project(f: &mut Frame, area: Rect, agg: &Aggregate) {
    let mut rows: Vec<(&String, &UsageTotals)> = agg.by_project.iter().collect();
    rows.sort_by(|a, b| b.1.cost_usd.partial_cmp(&a.1.cost_usd).unwrap_or(std::cmp::Ordering::Equal));
    let max_cost = rows.first().map(|(_, t)| t.cost_usd).unwrap_or(0.0).max(0.01);
    let height = area.height.saturating_sub(3) as usize;

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

/// Render the footer key-hint bar. `active` is one of "1" / "2" / "3"
/// / "4" / "m" — the matching nav chip gets a highlighted style so the
/// user can see which view they're in. `hint` carries view-specific
/// extra keys appended at the end (already styled dim).
///
/// Width-aware: on narrow terminals the per-chip labels collapse,
/// then the hint truncates with `…`, so the bar stays useful at any
/// size instead of clipping mid-word.
pub fn render_footer(f: &mut Frame, area: Rect, active: &str, hint: &str) {
    let inactive = Style::default().fg(Color::Black).bg(Color::Gray);
    let active_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let mewxi_inactive = Style::default().fg(Color::Black).bg(Color::Magenta);
    let chip_style = |key: &str| -> Style {
        if key == active {
            active_style
        } else if key == "m" {
            mewxi_inactive
        } else {
            inactive
        }
    };
    let chips: [(&str, &str); 7] = [
        ("1", "all"),
        ("2", "session"),
        ("3", "account"),
        ("4", "setup"),
        ("m", "mewxi"),
        ("r", "reload"),
        ("q", "quit"),
    ];

    let total_w = area.width as usize;
    // Width with labels: sum of `" K " + " label  "` per chip.
    let labeled_w: usize = chips
        .iter()
        .map(|(_, l)| 3 + 1 + l.chars().count() + 2)
        .sum();

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(chips.len() * 2 + 2);
    let labels_fit = labeled_w + 2 <= total_w; // need at least 2 cols slack for hint
    let mut used: usize = 0;
    for (k, l) in chips {
        spans.push(Span::styled(format!(" {} ", k), chip_style(k)));
        used += 3;
        if labels_fit {
            let seg = format!(" {}  ", l);
            used += seg.chars().count();
            spans.push(Span::raw(seg));
        } else {
            spans.push(Span::raw(" "));
            used += 1;
        }
    }
    // Truncate hint to whatever width is left.
    let remaining = total_w.saturating_sub(used);
    let hint_str: String = if hint.chars().count() <= remaining {
        hint.to_string()
    } else if remaining == 0 {
        String::new()
    } else {
        let take = remaining.saturating_sub(1);
        let mut s: String = hint.chars().take(take).collect();
        s.push('…');
        s
    };
    if !hint_str.is_empty() {
        spans.push(Span::styled(hint_str, Style::default().fg(Color::DarkGray)));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

