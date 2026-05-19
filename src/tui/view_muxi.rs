//! View 5 — full-screen Muxi splash with minified account + agent data.
//!
//! Left: largest Muxi ASCII art that fits, rendered in a purple scale.
//! Right: stacked condensed panels —
//!   - Accounts: name + 5h / weekly / extra mini-bars with percentages.
//!   - Agents: per-session state, status, ctx%.

use super::{LOGO_LARGE, LOGO_LARGE_DIMS, LOGO_MEDIUM, LOGO_MEDIUM_DIMS, LOGO_SMALL,
    LOGO_SMALL_DIMS, LOGO_TINY, LOGO_TINY_DIMS, PerAccount, SessionRef};
use crate::live_session::{Activity, SessionState};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};

/// Purple scale used across this view. From dim → bright.
/// Picked from the 256-colour palette so it renders without truecolor.
const P_DIM: Color = Color::Indexed(54);   // dark purple
const P_LOW: Color = Color::Indexed(97);   // muted purple
const P_MID: Color = Color::Indexed(135);  // medium purple
const P_HIGH: Color = Color::Indexed(171); // bright purple
const P_HOT: Color = Color::Indexed(207);  // hot pink-purple
const P_TEXT: Color = Color::Indexed(183); // light lavender (body text)
const P_LABEL: Color = Color::Indexed(141);

/// Gauge fill colour in the purple scale, hotter as utilisation climbs.
fn purple_gauge(pct: f64) -> Color {
    if pct >= 90.0 {
        P_HOT
    } else if pct >= 70.0 {
        P_HIGH
    } else if pct >= 40.0 {
        P_MID
    } else {
        P_LOW
    }
}

pub fn render(
    f: &mut Frame,
    area: Rect,
    accounts: &[&PerAccount],
    sessions: &[&SessionRef],
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(P_MID))
        .title(Span::styled(
            " Muxi ",
            Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    render_logo(f, cols[0]);
    render_side_panel(f, cols[1], accounts, sessions);
}

fn render_logo(f: &mut Frame, area: Rect) {
    let (src, logo_h) = pick_logo(area);
    let lines: Vec<Line> = src
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            // Subtle vertical gradient: top dim, middle bright, bottom dim.
            let h = logo_h.max(1) as usize;
            let pos = (i as f64 / h as f64 * 100.0).clamp(0.0, 100.0);
            let shade = if pos < 20.0 || pos > 80.0 {
                P_DIM
            } else if pos < 40.0 || pos > 60.0 {
                P_MID
            } else {
                P_HIGH
            };
            Line::from(Span::styled(l.to_string(), Style::default().fg(shade)))
        })
        .collect();

    let top_pad = area.height.saturating_sub(logo_h) / 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_pad),
            Constraint::Length(logo_h),
            Constraint::Min(0),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        chunks[1],
    );
}

fn pick_logo(area: Rect) -> (&'static str, u16) {
    if area.height >= LOGO_LARGE_DIMS.0 && area.width >= LOGO_LARGE_DIMS.1 {
        (LOGO_LARGE, LOGO_LARGE_DIMS.0)
    } else if area.height >= LOGO_MEDIUM_DIMS.0 && area.width >= LOGO_MEDIUM_DIMS.1 {
        (LOGO_MEDIUM, LOGO_MEDIUM_DIMS.0)
    } else if area.height >= LOGO_SMALL_DIMS.0 && area.width >= LOGO_SMALL_DIMS.1 {
        (LOGO_SMALL, LOGO_SMALL_DIMS.0)
    } else {
        (LOGO_TINY, LOGO_TINY_DIMS.0)
    }
}

fn render_side_panel(
    f: &mut Frame,
    area: Rect,
    accounts: &[&PerAccount],
    sessions: &[&SessionRef],
) {
    // Each account block: 1 header + 3 gauge rows = 4 lines. Cap so we
    // always leave room for the agents panel below.
    let acct_block_lines: u16 = 4;
    let want_acct = (acct_block_lines * accounts.len().max(1) as u16) + 2;
    let max_acct = area.height.saturating_sub(6);
    let acct_h = want_acct.min(max_acct).max(5);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(acct_h), Constraint::Min(4)])
        .split(area);

    render_accounts(f, rows[0], accounts);
    render_agents(f, rows[1], sessions);
}

fn render_accounts(f: &mut Frame, area: Rect, accounts: &[&PerAccount]) {
    let title = Span::styled(
        format!(" accounts ({}) ", accounts.len()),
        Style::default().fg(P_HIGH).add_modifier(Modifier::BOLD),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(P_LOW))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if accounts.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no accounts",
            Style::default().fg(P_DIM),
        )));
        f.render_widget(p, inner);
        return;
    }

    let constraints: Vec<Constraint> = accounts
        .iter()
        .map(|_| Constraint::Length(4))
        .collect();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, pa) in accounts.iter().enumerate() {
        if i >= chunks.len() {
            break;
        }
        render_account(f, chunks[i], pa);
    }
}

fn render_account(f: &mut Frame, area: Rect, pa: &PerAccount) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let active = pa
        .live_sessions
        .iter()
        .filter(|s| s.state == SessionState::Active)
        .count();
    let header = Line::from(vec![
        Span::styled(
            format!("[{}]", pa.account.name),
            Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{active} live"),
            Style::default().fg(if active > 0 { P_HIGH } else { P_DIM }),
        ),
        Span::raw("  "),
        Span::styled(
            format!("${:.2}", pa.agg.all.cost_usd),
            Style::default().fg(P_LABEL),
        ),
    ]);
    f.render_widget(Paragraph::new(header), rows[0]);

    let live = pa.live.as_ref();
    let five_h = live.and_then(|l| l.five_hour.as_ref()).map(|w| w.utilization);
    let weekly = live.and_then(|l| l.seven_day.as_ref()).map(|w| w.utilization);
    let extra = live
        .and_then(|l| l.extra_usage.as_ref())
        .filter(|e| e.is_enabled)
        .and_then(|e| e.utilization);

    render_mini_gauge(f, rows[1], "5h", five_h);
    render_mini_gauge(f, rows[2], "wk", weekly);
    render_mini_gauge(f, rows[3], "ex", extra);
}

fn render_mini_gauge(f: &mut Frame, area: Rect, label: &str, pct: Option<f64>) {
    const LABEL_W: u16 = 5;
    const PCT_W: u16 = 7;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(LABEL_W),
            Constraint::Min(4),
            Constraint::Length(PCT_W),
        ])
        .split(area);

    let label_p = Paragraph::new(Line::from(Span::styled(
        format!(" {label}"),
        Style::default().fg(P_LABEL),
    )));
    f.render_widget(label_p, cols[0]);

    match pct {
        Some(p) => {
            let ratio = (p / 100.0).clamp(0.0, 1.0);
            let color = purple_gauge(p);
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(color).bg(Color::Indexed(53)))
                .ratio(ratio)
                .label("");
            f.render_widget(gauge, cols[1]);
            let pct_p = Paragraph::new(Line::from(Span::styled(
                format!("{p:>5.1}% "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
            f.render_widget(pct_p, cols[2]);
        }
        None => {
            let dim = Paragraph::new(Line::from(Span::styled(
                "─".repeat(cols[1].width as usize),
                Style::default().fg(P_DIM),
            )));
            f.render_widget(dim, cols[1]);
            let pct_p = Paragraph::new(Line::from(Span::styled(
                "  n/a  ",
                Style::default().fg(P_DIM),
            )));
            f.render_widget(pct_p, cols[2]);
        }
    }
}

fn render_agents(f: &mut Frame, area: Rect, sessions: &[&SessionRef]) {
    let active = sessions
        .iter()
        .filter(|s| s.state == SessionState::Active)
        .count();
    let idle = sessions.len() - active;
    let title = Span::styled(
        format!(" agents · {active} active · {idle} idle "),
        Style::default().fg(P_HIGH).add_modifier(Modifier::BOLD),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(P_LOW))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if sessions.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no agents running",
            Style::default().fg(P_DIM),
        )));
        f.render_widget(p, inner);
        return;
    }

    let visible = (inner.height as usize).min(sessions.len());
    let lines: Vec<Line> = sessions
        .iter()
        .take(visible)
        .map(|s| agent_line(s))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn agent_line(s: &SessionRef) -> Line<'static> {
    let state_label = match s.state {
        SessionState::Active => "active",
        SessionState::Idle => "idle",
    };
    let state_color = match s.state {
        SessionState::Active => P_HOT,
        SessionState::Idle => P_DIM,
    };
    let (status, status_color) = activity_purple(&s.activity);
    let ctx = fmt_ctx(s.current_context, s.context_cap);
    let ctx_color = ctx_color(s.current_context, s.context_cap);

    Line::from(vec![
        Span::styled(
            format!(" {:<10} ", trim_to(&s.account_name, 10)),
            Style::default().fg(P_LABEL),
        ),
        Span::styled(
            format!("{:<7}", state_label),
            Style::default().fg(state_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<11}", status),
            Style::default().fg(status_color),
        ),
        Span::styled(
            format!("ctx {:>4}", ctx),
            Style::default().fg(ctx_color),
        ),
        Span::raw("  "),
        Span::styled(trim_to(&s.project, 18), Style::default().fg(P_TEXT)),
    ])
}

fn activity_purple(a: &Activity) -> (String, Color) {
    let color = match a {
        Activity::Waiting => P_DIM,
        Activity::Awaiting => P_HOT,
        Activity::Asking => P_HOT,
        Activity::Thinking | Activity::Starting => P_MID,
        Activity::Writing | Activity::Editing => P_HIGH,
        Activity::Reading | Activity::Searching | Activity::Fetching => P_LOW,
        Activity::Running | Activity::Delegating => P_HIGH,
        Activity::Tool(_) => P_TEXT,
    };
    (a.label(), color)
}

fn fmt_ctx(current: Option<u64>, cap: Option<u64>) -> String {
    match (current, cap) {
        (Some(c), Some(cap)) if cap > 0 => {
            let pct = (c as f64 / cap as f64 * 100.0).round() as u32;
            format!("{pct}%")
        }
        _ => "—".into(),
    }
}

fn ctx_color(current: Option<u64>, cap: Option<u64>) -> Color {
    match (current, cap) {
        (Some(c), Some(cap)) if cap > 0 => {
            let pct = c as f64 / cap as f64 * 100.0;
            purple_gauge(pct)
        }
        _ => P_DIM,
    }
}

fn trim_to(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
