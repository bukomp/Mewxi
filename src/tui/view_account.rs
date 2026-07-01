//! View 3 — single-account detail (the original single-pane dashboard).

use super::widgets::{self, compute_metrics, Metrics};
use super::PerAccount;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Width at which the layout switches from stacked (narrow) to multi-column (wide).
const WIDE_THRESHOLD: u16 = 100;

pub fn render(f: &mut Frame, area: Rect, pa: &PerAccount) {
    let m = compute_metrics(&pa.agg);
    if area.width < WIDE_THRESHOLD {
        render_narrow(f, area, pa, &m);
    } else {
        render_wide(f, area, pa, &m);
    }
}

fn render_wide(f: &mut Frame, area: Rect, pa: &PerAccount, m: &Metrics) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Length(8),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);

    let title = format!("[{}] account detail", pa.account.name);
    widgets::render_header(f, rows[0], &title, &pa.agg, pa.live.as_ref());

    let gauge_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(rows[1]);
    widgets::render_5h_gauge(f, gauge_row[0], &pa.agg, pa.live.as_ref());
    widgets::render_7d_gauge(f, gauge_row[1], pa.live.as_ref());
    widgets::render_extra_gauge(f, gauge_row[2], pa.live.as_ref());
    widgets::render_fable_gauge(f, gauge_row[3], pa.live.as_ref());

    let stats_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
        ])
        .split(rows[2]);
    widgets::render_burn_panel(f, stats_row[0], m);
    widgets::render_sparkline_panel(f, stats_row[1], m);
    widgets::render_efficiency_panel(f, stats_row[2], &pa.agg, m);

    widgets::render_by_project(f, rows[3], &pa.agg);
    widgets::render_footer(f, rows[4], "3", "↑/↓ next account · r refresh limits · Esc back · live via api.anthropic.com/api/oauth/usage", true);
}

fn render_narrow(f: &mut Frame, area: Rect, pa: &PerAccount, m: &Metrics) {
    let h = area.height;
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(5),
    ];
    let mut keys = vec!["header", "5h", "7d", "extra", "fable", "burn"];
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
    let title = format!("[{}] account detail", pa.account.name);

    for (rect, key) in rects.iter().zip(keys.iter()) {
        match *key {
            "header" => widgets::render_header(f, *rect, &title, &pa.agg, pa.live.as_ref()),
            "5h" => widgets::render_5h_gauge(f, *rect, &pa.agg, pa.live.as_ref()),
            "7d" => widgets::render_7d_gauge(f, *rect, pa.live.as_ref()),
            "extra" => widgets::render_extra_gauge(f, *rect, pa.live.as_ref()),
            "fable" => widgets::render_fable_gauge(f, *rect, pa.live.as_ref()),
            "burn" => widgets::render_burn_and_efficiency_narrow(f, *rect, &pa.agg, m),
            "sparkline" => widgets::render_sparkline_panel(f, *rect, m),
            "projects" => widgets::render_by_project(f, *rect, &pa.agg),
            "footer" => widgets::render_footer(f, *rect, "3", "↑/↓ next · Esc back", true),
            _ => {}
        }
    }
}
