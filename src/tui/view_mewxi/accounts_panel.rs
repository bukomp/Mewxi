//! View 5 — full per-account panels, restyled into the rave purple/pink
//! palette. Feature-adapted from view 1's `render_account_stack` /
//! `render_one_account` / `render_one_account_compact` — same layout and
//! data, different colors.

use super::palette::{purple_gauge, P_BG, P_DIM, P_HIGH, P_HOT, P_LABEL, P_PINK, P_TEXT};
use crate::live_session::SessionState;
use crate::live_usage::{refresh_interval, LiveUsage};
use crate::tui::PerAccount;
use chrono::{Local, Utc};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Frame;

/// Lines consumed by one account block: 1 header + 4 gauges + 1 spacer.
pub const ROWS_PER_ACCOUNT: u16 = 6;

/// Width of the cache-age column — right-aligned so "0s ago", "59m ago"
/// and "23h ago" all occupy the same space, keeping the $ column
/// aligned across accounts.
const CACHE_AGE_WIDTH: usize = 7;

/// Render the account stack into `area`. When `compact` the caller has
/// decided the terminal is too short — render one line per account
/// instead of the full gauge block. Guards tiny/empty areas; never
/// panics.
pub fn render(f: &mut Frame, area: Rect, accounts: &[&PerAccount], compact: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let title = format!(
        "Accounts ({} account{})",
        accounts.len(),
        if accounts.len() == 1 { "" } else { "s" }
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(P_LABEL))
        .title(Span::styled(
            title,
            Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if accounts.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no accounts…",
            Style::default().fg(P_DIM),
        )));
        f.render_widget(p, inner);
        return;
    }

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Vertically split the inner area into one slice per account.
    let rows_per = if compact { 1 } else { ROWS_PER_ACCOUNT };
    let constraints: Vec<Constraint> = accounts
        .iter()
        .map(|_| Constraint::Length(rows_per))
        .collect();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, pa) in accounts.iter().enumerate() {
        if i >= chunks.len() || chunks[i].height == 0 {
            break;
        }
        if compact {
            render_one_account_compact(f, chunks[i], pa);
        } else {
            render_one_account(f, chunks[i], pa);
        }
    }
}

/// Single-line account summary used when the terminal is too short for
/// the full gauge block: name · live · age · $ followed by the four
/// utilizations inline, colored like the gauges they replace.
fn render_one_account_compact(f: &mut Frame, area: Rect, pa: &PerAccount) {
    let active_count = pa
        .live_sessions
        .iter()
        .filter(|s| s.state == SessionState::Active)
        .count();
    let live_color = if active_count == 0 { P_DIM } else { P_HIGH };
    let live = pa.live.as_ref();
    let pct_span = |label: &'static str, pct: Option<f64>| -> Vec<Span<'static>> {
        let mut v = vec![Span::styled(
            format!("  {label} "),
            Style::default().fg(P_LABEL),
        )];
        match pct {
            Some(p) => v.push(Span::styled(
                format!("{p:>5.1}%"),
                Style::default()
                    .fg(purple_gauge(p))
                    .add_modifier(Modifier::BOLD),
            )),
            None => v.push(Span::styled("  n/a ", Style::default().fg(P_DIM))),
        }
        v
    };
    let mut spans = vec![
        Span::styled(
            format!("{:8}", format!("[{}]", pa.account.name)),
            Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>3} live", active_count),
            Style::default().fg(live_color),
        ),
        Span::raw(" · "),
        cache_age_span(live),
        Span::raw("  "),
        Span::styled(
            format!("${:>9.2}", pa.agg.all.cost_usd),
            Style::default().fg(P_TEXT),
        ),
    ];
    spans.extend(pct_span(
        "5h",
        live.and_then(|l| l.five_hour.as_ref())
            .map(|w| w.utilization),
    ));
    spans.extend(pct_span(
        "wk",
        live.and_then(|l| l.seven_day.as_ref())
            .map(|w| w.utilization),
    ));
    spans.extend(pct_span(
        "ex",
        live.and_then(|l| l.extra_usage.as_ref())
            .filter(|e| e.is_enabled)
            .and_then(|e| e.utilization),
    ));
    spans.extend(pct_span(
        "fb",
        live.and_then(|l| l.fable_limit()).map(|w| w.percent),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_one_account(f: &mut Frame, area: Rect, pa: &PerAccount) {
    // Row 0: name · N live · cache age
    // Row 1: 5h gauge
    // Row 2: weekly gauge
    // Row 3: extra gauge
    // Row 4: fable gauge
    // Row 5: blank spacer
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    render_account_header(f, rows[0], pa);

    let live = pa.live.as_ref();
    let five_h_pct = live
        .and_then(|l| l.five_hour.as_ref())
        .map(|w| w.utilization);
    let seven_d_pct = live
        .and_then(|l| l.seven_day.as_ref())
        .map(|w| w.utilization);
    let extra_pct = live
        .and_then(|l| l.extra_usage.as_ref())
        .filter(|e| e.is_enabled)
        .and_then(|e| e.utilization);
    let fable_pct = live.and_then(|l| l.fable_limit()).map(|w| w.percent);

    let five_h_meta = live
        .and_then(|l| l.five_hour.as_ref())
        .and_then(|w| w.resets_at)
        .map(|t| {
            let local = t.with_timezone(&Local);
            let mins = (t - Utc::now()).num_minutes().max(0);
            format!("reset {} ({:>3}m)", local.format("%H:%M"), mins)
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
            let sym = crate::tui::widgets::currency_symbol(e.currency.as_deref());
            format!("{sym}{used:.2} / {sym}{limit:.2}")
        })
        .unwrap_or_default();
    let fable_meta = live
        .and_then(|l| l.fable_limit())
        .and_then(|w| w.resets_at)
        .map(|t| format!("reset {}", t.with_timezone(&Local).format("%a %H:%M")))
        .unwrap_or_default();

    // Fix the meta column width across all four rows so the percent
    // column lines up vertically.
    let meta_col_width = [&five_h_meta, &seven_d_meta, &extra_meta, &fable_meta]
        .iter()
        .map(|m| m.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .min(28);

    render_gauge_row(f, rows[1], "5h", five_h_pct, &five_h_meta, meta_col_width);
    render_gauge_row(
        f,
        rows[2],
        "weekly",
        seven_d_pct,
        &seven_d_meta,
        meta_col_width,
    );
    render_gauge_row(f, rows[3], "extra", extra_pct, &extra_meta, meta_col_width);
    render_gauge_row(f, rows[4], "fable", fable_pct, &fable_meta, meta_col_width);
}

fn render_account_header(f: &mut Frame, area: Rect, pa: &PerAccount) {
    let active_count = pa
        .live_sessions
        .iter()
        .filter(|s| s.state == SessionState::Active)
        .count();
    let live_color = if active_count == 0 { P_DIM } else { P_HIGH };
    let mut spans = vec![
        Span::styled(
            format!("{:8}", format!("[{}]", pa.account.name)),
            Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>3} live", active_count),
            Style::default().fg(live_color),
        ),
    ];
    // Cache-age indicator — always reserved as a fixed-width slot so
    // headers line up across accounts even when live data is absent.
    spans.push(Span::raw(" · "));
    spans.push(cache_age_span(pa.live.as_ref()));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn cache_age_span(live: Option<&LiveUsage>) -> Span<'static> {
    let Some(lu) = live else {
        // Pad so absent values don't collapse the column.
        return Span::raw(" ".repeat(CACHE_AGE_WIDTH));
    };
    let age = lu.age_seconds();
    let refresh = refresh_interval().as_secs() as i64;
    let color = if lu.is_stale() {
        P_HOT
    } else if age > 2 * refresh {
        P_PINK
    } else {
        P_DIM
    };
    let raw = fmt_cache_age(age);
    Span::styled(
        format!("{raw:>CACHE_AGE_WIDTH$}"),
        Style::default().fg(color),
    )
}

/// Format a cache age in seconds as "Ns ago" / "Nm ago" / "Nh ago".
/// Pure helper, extracted so its boundaries can be unit-tested.
fn fmt_cache_age(age: i64) -> String {
    if age < 60 {
        format!("{age}s ago")
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else {
        format!("{}h ago", age / 3600)
    }
}

/// Lay out a single gauge row as:
///   `  LABEL  [============bar============]  PCT   meta…`
/// with fixed-width label / pct / meta columns so multiple rows line up.
/// `meta_col_chars` is the max meta width across rows in this account block.
fn render_gauge_row(
    f: &mut Frame,
    area: Rect,
    label: &str,
    pct_opt: Option<f64>,
    meta: &str,
    meta_col_chars: u16,
) {
    const LABEL_WIDTH: u16 = 10; // "  weekly  " — fits longest label "weekly"
    const PCT_WIDTH: u16 = 8; //  "  100.0% "
    let meta_width = if meta_col_chars == 0 {
        1
    } else {
        meta_col_chars + 2
    };

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(LABEL_WIDTH),
            Constraint::Min(8),
            Constraint::Length(PCT_WIDTH),
            Constraint::Length(meta_width),
        ])
        .split(area);

    let label_p = Paragraph::new(Line::from(Span::styled(
        format!("  {label}"),
        Style::default().fg(P_LABEL),
    )));
    f.render_widget(label_p, cols[0]);

    match pct_opt {
        Some(p) => {
            let ratio = (p / 100.0).clamp(0.0, 1.0);
            let color = purple_gauge(p);
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(color).bg(P_BG))
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
                Style::default().fg(P_DIM),
            )));
            f.render_widget(dim, cols[1]);
            let pct_p = Paragraph::new(Line::from(Span::styled(
                "  n/a ",
                Style::default().fg(P_DIM),
            )));
            f.render_widget(pct_p, cols[2]);
        }
    }

    if !meta.is_empty() {
        let meta_p = Paragraph::new(Line::from(Span::styled(
            format!(" {meta}"),
            Style::default().fg(P_DIM),
        )));
        f.render_widget(meta_p, cols[3]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_cache_age_boundaries() {
        assert_eq!(fmt_cache_age(0), "0s ago");
        assert_eq!(fmt_cache_age(59), "59s ago");
        assert_eq!(fmt_cache_age(60), "1m ago");
        assert_eq!(fmt_cache_age(3599), "59m ago");
        assert_eq!(fmt_cache_age(3600), "1h ago");
        assert_eq!(fmt_cache_age(86399), "23h ago");
    }
}
