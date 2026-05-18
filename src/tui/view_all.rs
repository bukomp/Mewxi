//! View 1 — all accounts + all currently-running sessions.
//!
//! Top: one block per account with a header line (name · live count ·
//! cost) and three aligned gauges (5h / weekly / extra) showing
//! `[bar] pct  meta`. Bottom: one flat table of every live session
//! across every account, sorted by most-recent activity.

use super::widgets::{fmt_tokens_compact, gauge_color, render_footer};
use super::{PerAccount, SessionRef};
use crate::live_session::SessionState;
use crate::live_usage::{LiveUsage, REFRESH_INTERVAL};
use chrono::{Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};

/// Lines consumed by one account block: 1 header + 3 gauges + 1 spacer.
const ROWS_PER_ACCOUNT: u16 = 5;

pub fn render(
    f: &mut Frame,
    area: Rect,
    accounts: &[&PerAccount],
    sessions: &[&SessionRef],
    selected: usize,
) {
    let title = format!(
        "Claude Usage — {} account{}",
        accounts.len(),
        if accounts.len() == 1 { "" } else { "s" }
    );

    // Reserve enough rows for every account block, capped so the
    // sessions table always gets at least 5 rows.
    let want = (ROWS_PER_ACCOUNT * accounts.len() as u16) + 2; // +2 for borders
    let max_for_accounts = area.height.saturating_sub(8);
    let acct_height = want.min(max_for_accounts).max(5);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),           // header
            Constraint::Length(acct_height), // per-account gauges
            Constraint::Min(5),              // live sessions table
            Constraint::Length(1),           // footer
        ])
        .split(area);

    render_header(f, rows[0], &title);
    render_account_stack(f, rows[1], accounts);
    render_sessions_table(f, rows[2], sessions, selected);
    render_footer(f, rows[3], "1 here · 2 session · 3 account · 4 setup");
}

fn render_header(f: &mut Frame, area: Rect, title: &str) {
    let line = Line::from(vec![Span::styled(
        title.to_string(),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )]);
    f.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_account_stack(f: &mut Frame, area: Rect, accounts: &[&PerAccount]) {
    let block = Block::default().borders(Borders::ALL).title("Accounts");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if accounts.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no accounts (all ignored?)",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(p, inner);
        return;
    }

    // Vertically split the inner area into one slice per account.
    let constraints: Vec<Constraint> = accounts
        .iter()
        .map(|_| Constraint::Length(ROWS_PER_ACCOUNT))
        .collect();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, pa) in accounts.iter().enumerate() {
        if i >= chunks.len() {
            break;
        }
        render_one_account(f, chunks[i], pa);
    }
}

fn render_one_account(f: &mut Frame, area: Rect, pa: &PerAccount) {
    // Row 0: name · N live · $total
    // Row 1: 5h gauge
    // Row 2: weekly gauge
    // Row 3: extra gauge
    // Row 4: blank spacer
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_account_header(f, rows[0], pa);

    let live = pa.live.as_ref();
    let five_h_pct = live.and_then(|l| l.five_hour.as_ref()).map(|w| w.utilization);
    let seven_d_pct = live.and_then(|l| l.seven_day.as_ref()).map(|w| w.utilization);
    let extra_pct = live
        .and_then(|l| l.extra_usage.as_ref())
        .filter(|e| e.is_enabled)
        .and_then(|e| e.utilization);

    let five_h_meta = live
        .and_then(|l| l.five_hour.as_ref())
        .and_then(|w| w.resets_at)
        .map(|t| {
            let local = t.with_timezone(&Local);
            let mins = (t - Utc::now()).num_minutes().max(0);
            format!("reset {} ({}m)", local.format("%H:%M"), mins)
        })
        .unwrap_or_default();
    let seven_d_meta = live
        .and_then(|l| l.seven_day.as_ref())
        .and_then(|w| w.resets_at)
        .map(|t| format!("reset {}", t.with_timezone(&Local).format("%a %H:%M")))
        .unwrap_or_default();
    let extra_meta = live
        .and_then(|l| l.extra_usage.as_ref())
        .filter(|e| e.is_enabled)
        .map(|e| {
            let used = e.used_credits.unwrap_or(0.0) / 100.0;
            let limit = e.monthly_limit.unwrap_or(0.0) / 100.0;
            let sym = currency_symbol(e.currency.as_deref());
            format!("{sym}{used:.2} / {sym}{limit:.2}")
        })
        .unwrap_or_default();

    render_gauge_row(f, rows[1], "5h",     five_h_pct,  &five_h_meta);
    render_gauge_row(f, rows[2], "weekly", seven_d_pct, &seven_d_meta);
    render_gauge_row(f, rows[3], "extra",  extra_pct,   &extra_meta);
}

fn render_account_header(f: &mut Frame, area: Rect, pa: &PerAccount) {
    let active_count = pa
        .live_sessions
        .iter()
        .filter(|s| s.state == SessionState::Active)
        .count();
    let live_color = if active_count == 0 {
        Color::DarkGray
    } else {
        Color::Green
    };
    let mut spans = vec![
        Span::styled(
            format!("{:8}", format!("[{}]", pa.account.name)),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>3} live", active_count),
            Style::default().fg(live_color),
        ),
    ];
    // Cache-age indicator — makes it obvious when the bars below are
    // out of date because the daemon stopped or the endpoint backed off.
    if let Some(age_span) = cache_age_span(pa.live.as_ref()) {
        spans.push(Span::raw(" · "));
        spans.push(age_span);
    }
    spans.push(Span::raw("   "));
    spans.push(Span::styled(
        format!("${:>9.2} total", pa.agg.all.cost_usd),
        Style::default().fg(Color::Green),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn cache_age_span(live: Option<&LiveUsage>) -> Option<Span<'static>> {
    let lu = live?;
    let age = lu.age_seconds();
    let refresh = REFRESH_INTERVAL.as_secs() as i64;
    let color = if lu.is_stale() {
        Color::Red
    } else if age > 2 * refresh {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let text = if age < 60 {
        format!("{age}s ago")
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else {
        format!("{}h ago", age / 3600)
    };
    Some(Span::styled(text, Style::default().fg(color)))
}

/// Lay out a single gauge row as:
///   `  LABEL  [============bar============]  PCT   meta…`
/// with fixed-width label / pct columns so multiple rows line up.
fn render_gauge_row(f: &mut Frame, area: Rect, label: &str, pct_opt: Option<f64>, meta: &str) {
    const LABEL_WIDTH: u16 = 10;   // "  weekly  " — fits longest label "weekly"
    const PCT_WIDTH: u16 = 8;      //  "  100.0% "
    let meta_width = if meta.is_empty() { 0 } else { (meta.chars().count() as u16).min(28) + 2 };

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(LABEL_WIDTH),
            Constraint::Min(8),
            Constraint::Length(PCT_WIDTH),
            Constraint::Length(meta_width.max(1)),
        ])
        .split(area);

    let label_p = Paragraph::new(Line::from(Span::styled(
        format!("  {label}"),
        Style::default().fg(Color::Cyan),
    )));
    f.render_widget(label_p, cols[0]);

    match pct_opt {
        Some(p) => {
            let ratio = (p / 100.0).clamp(0.0, 1.0);
            let color = gauge_color(p);
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(color))
                .ratio(ratio)
                .label(""); // we render our own percentage column next to it
            f.render_widget(gauge, cols[1]);
            let pct_p = Paragraph::new(Line::from(Span::styled(
                format!("{p:>5.1}% "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
            f.render_widget(pct_p, cols[2]);
        }
        None => {
            let dim = Paragraph::new(Line::from(Span::styled(
                "—".repeat(cols[1].width as usize),
                Style::default().fg(Color::DarkGray),
            )));
            f.render_widget(dim, cols[1]);
            let pct_p = Paragraph::new(Line::from(Span::styled(
                "  n/a ",
                Style::default().fg(Color::DarkGray),
            )));
            f.render_widget(pct_p, cols[2]);
        }
    }

    if !meta.is_empty() {
        let meta_p = Paragraph::new(Line::from(Span::styled(
            format!(" {meta}"),
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(meta_p, cols[3]);
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

fn render_sessions_table(f: &mut Frame, area: Rect, sessions: &[&SessionRef], selected: usize) {
    let active_count = sessions
        .iter()
        .filter(|s| s.state == SessionState::Active)
        .count();
    let idle_count = sessions.len() - active_count;
    let title = if idle_count == 0 {
        format!("Sessions — {active_count} active")
    } else {
        format!("Sessions — {active_count} active · {idle_count} idle")
    };
    let block = Block::default().borders(Borders::ALL).title(title);

    if sessions.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let p = Paragraph::new(Line::from(Span::styled(
            "no sessions touched in the last 2h — start a claude conversation or press `r` to rescan",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(p, inner);
        return;
    }

    let now = Utc::now();
    let rows: Vec<Row> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let age_secs = (now - s.last_activity).num_seconds().max(0);
            let arrow = if i == selected { "▶ " } else { "  " };
            let state_label = match s.state {
                SessionState::Active => "active",
                SessionState::Idle => "idle",
            };
            let state_color = match s.state {
                SessionState::Active => Color::Green,
                SessionState::Idle => Color::DarkGray,
            };
            let base_style = if i == selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else if s.state == SessionState::Idle {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(format!("{arrow}{}", s.account_name)),
                Cell::from(s.project.clone()),
                Cell::from(short_id(&s.session_id)),
                Cell::from(fmt_age(age_secs)),
                Cell::from(fmt_tokens_compact(s.tokens)),
                Cell::from(format!("${:.2}", s.cost_usd)),
                Cell::from(short_model(&s.model)),
                Cell::from(Span::styled(state_label, Style::default().fg(state_color))),
            ])
            .style(base_style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Min(14),
            Constraint::Length(11),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Length(7),
        ],
    )
    .header(
        Row::new(vec!["account", "project", "session", "age", "tokens", "cost", "model", "state"])
            .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
    )
    .block(block);
    f.render_widget(table, area);
}

fn short_id(s: &str) -> String {
    let n = 8.min(s.len());
    s.chars().take(n).collect::<String>() + if s.len() > n { "…" } else { "" }
}

fn short_model(m: &str) -> String {
    let lower = m.to_ascii_lowercase();
    if lower.contains("opus") {
        "opus".into()
    } else if lower.contains("sonnet") {
        "sonnet".into()
    } else if lower.contains("haiku") {
        "haiku".into()
    } else {
        m.chars().take(8).collect()
    }
}

fn fmt_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}
