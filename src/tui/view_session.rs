//! View 2 — selected session detail.
//!
//! Top band: the parent account's three gauges. Middle: session token
//! breakdown and recent burn. Footer: keybinding hints.

use super::widgets::{self, fmt_tokens_compact};
use super::{PerAccount, SessionRef};
use crate::live_session::SessionState;
use crate::stats::fmt_num;
use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

pub fn render(f: &mut Frame, area: Rect, accounts: &[&PerAccount], session: Option<&SessionRef>) {
    let Some(session) = session else {
        let p = Paragraph::new(Line::from(Span::styled(
            "no session selected — switch to view 1 (press 1) and pick one with ↑/↓ then Enter",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::default().borders(Borders::ALL).title("Session detail"));
        f.render_widget(p, area);
        return;
    };
    let parent = accounts.iter().find(|a| a.account.name == session.account_name);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Length(4),  // 3 gauges
            Constraint::Length(8),  // session breakdown
            Constraint::Min(4),     // (room for future recent-messages list)
            Constraint::Length(1),  // footer
        ])
        .split(area);

    render_header(f, rows[0], session);

    if let Some(pa) = parent {
        let gauge_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(rows[1]);
        widgets::render_5h_gauge(f, gauge_row[0], &pa.agg, pa.live.as_ref());
        widgets::render_7d_gauge(f, gauge_row[1], pa.live.as_ref());
        widgets::render_extra_gauge(f, gauge_row[2], pa.live.as_ref());
    }

    render_session_table(f, rows[2], session);
    render_meta_panel(f, rows[3], session);
    widgets::render_footer(f, rows[4], "1 all  3 account");
}

fn render_header(f: &mut Frame, area: Rect, s: &SessionRef) {
    let mut spans = vec![
        Span::styled(
            format!("[{}]", s.account_name),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(s.project.clone(), Style::default().fg(Color::Cyan)),
        Span::raw("  session "),
        Span::styled(s.session_id.clone(), Style::default().fg(Color::Yellow)),
        Span::raw("  "),
        Span::styled(s.model.clone(), Style::default().fg(Color::Green)),
    ];
    if s.state == SessionState::Idle {
        let mins = (Utc::now() - s.state_since).num_minutes().max(0);
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("[idle for {mins}m]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans))
            .block(Block::default().borders(Borders::ALL).title("Session detail")),
        area,
    );
}

fn render_session_table(f: &mut Frame, area: Rect, s: &SessionRef) {
    let totals = &s.totals;
    let rows = vec![
        kv_row("messages", fmt_num(totals.messages)),
        kv_row("input tokens", fmt_num(totals.input)),
        kv_row("output tokens", fmt_num(totals.output)),
        kv_row("cache read", fmt_num(totals.cache_read)),
        kv_row("cache write 5m", fmt_num(totals.cache_write_5m)),
        kv_row("cache write 1h", fmt_num(totals.cache_write_1h)),
        kv_row("cost", format!("${:.4}", totals.cost_usd)),
    ];
    let table = Table::new(
        rows,
        [Constraint::Length(18), Constraint::Min(10)],
    )
    .block(Block::default().borders(Borders::ALL).title("Tokens this session"));
    f.render_widget(table, area);
}

fn render_meta_panel(f: &mut Frame, area: Rect, s: &SessionRef) {
    let now = Utc::now();
    let age = (now - s.last_activity).num_seconds().max(0);
    let ctx_line = match (s.current_context, s.context_cap) {
        (Some(cur), Some(cap)) => {
            let pct = (cur as f64 / cap as f64 * 100.0).min(999.0);
            let color = if pct >= 85.0 { Color::Red } else if pct >= 60.0 { Color::Yellow } else { Color::Green };
            Line::from(vec![
                Span::styled("context     ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{pct:>5.1}%  ({}/{})", fmt_tokens_compact(cur), fmt_tokens_compact(cap)),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ])
        }
        _ => Line::from(vec![
            Span::styled("context     ", Style::default().fg(Color::DarkGray)),
            Span::styled("n/a", Style::default().fg(Color::DarkGray)),
        ]),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("transcript  ", Style::default().fg(Color::DarkGray)),
            Span::styled(s.transcript_path.display().to_string(), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("last active ", Style::default().fg(Color::DarkGray)),
            Span::styled(fmt_age(age), Style::default().fg(Color::Yellow)),
        ]),
        ctx_line,
    ];
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Session meta")),
        area,
    );
}

fn kv_row(k: &str, v: impl Into<String>) -> Row<'static> {
    Row::new(vec![
        Cell::from(k.to_string()).style(Style::default().fg(Color::DarkGray)),
        Cell::from(v.into()).style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
    ])
}

fn fmt_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}
