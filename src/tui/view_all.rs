//! View 1 — all accounts + all currently-running sessions.
//!
//! Top: one block per account with a header line (name · live count ·
//! cost) and three aligned gauges (5h / weekly / extra) showing
//! `[bar] pct  meta`. Bottom: one flat table of every live session
//! across every account, grouped by project (alphabetical) with
//! sessions ordered by pid within each group so rows stay put as
//! sessions toggle active/idle.

use super::widgets::{fmt_tokens_compact, gauge_color, render_footer};
use super::{PerAccount, SessionRef};
use crate::live_session::{Activity, SessionState};
use crate::live_usage::{LiveUsage, REFRESH_INTERVAL};
use chrono::{Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, TableState};

/// Lines consumed by one account block: 1 header + 3 gauges + 1 spacer.
const ROWS_PER_ACCOUNT: u16 = 5;

pub fn render(
    f: &mut Frame,
    area: Rect,
    accounts: &[&PerAccount],
    sessions: &[&SessionRef],
    selected: Option<usize>,
    sessions_rect: &mut Option<Rect>,
    table_state: &mut TableState,
) {
    // Reserve enough rows for every account block, capped so the
    // sessions table always gets at least 5 rows. On short terminals
    // the full 5-row gauge blocks don't fit — fall back to one compact
    // line per account so every account stays visible and the sessions
    // table keeps its space.
    let full_want = (ROWS_PER_ACCOUNT * accounts.len() as u16) + 2; // +2 for borders
    let max_for_accounts = area.height.saturating_sub(8);
    let compact = full_want > max_for_accounts;
    let want = if compact {
        accounts.len() as u16 + 2
    } else {
        full_want
    };
    let acct_height = want.min(max_for_accounts).max(3);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(acct_height), // per-account gauges
            Constraint::Min(5),               // live sessions table
            Constraint::Length(1),            // footer
        ])
        .split(area);

    render_account_stack(f, rows[0], accounts, compact);
    *sessions_rect = Some(rows[1]);
    render_sessions_table(f, rows[1], sessions, selected, table_state);
    render_footer(
        f,
        rows[2],
        "1",
        "↑/↓ select · Enter open · n new · Del kill",
        true,
    );
}

fn render_account_stack(f: &mut Frame, area: Rect, accounts: &[&PerAccount], compact: bool) {
    let title = format!(
        "Accounts ({} account{})",
        accounts.len(),
        if accounts.len() == 1 { "" } else { "s" }
    );
    let block = Block::default().borders(Borders::ALL).title(title);
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
/// the full gauge block: name · live · age · $ followed by the three
/// utilizations inline, colored like the gauges they replace.
fn render_one_account_compact(f: &mut Frame, area: Rect, pa: &PerAccount) {
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
    let live = pa.live.as_ref();
    let pct_span = |label: &'static str, pct: Option<f64>| -> Vec<Span<'static>> {
        let mut v = vec![Span::styled(
            format!("  {label} "),
            Style::default().fg(Color::Cyan),
        )];
        match pct {
            Some(p) => v.push(Span::styled(
                format!("{p:>5.1}%"),
                Style::default().fg(gauge_color(p)).add_modifier(Modifier::BOLD),
            )),
            None => v.push(Span::styled("  n/a ", Style::default().fg(Color::DarkGray))),
        }
        v
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
        Span::raw(" · "),
        cache_age_span(live),
        Span::raw("  "),
        Span::styled(
            format!("${:>9.2}", pa.agg.all.cost_usd),
            Style::default().fg(Color::Green),
        ),
    ];
    spans.extend(pct_span(
        "5h",
        live.and_then(|l| l.five_hour.as_ref()).map(|w| w.utilization),
    ));
    spans.extend(pct_span(
        "wk",
        live.and_then(|l| l.seven_day.as_ref()).map(|w| w.utilization),
    ));
    spans.extend(pct_span(
        "ex",
        live.and_then(|l| l.extra_usage.as_ref())
            .filter(|e| e.is_enabled)
            .and_then(|e| e.utilization),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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
            let sym = currency_symbol(e.currency.as_deref());
            format!("{sym}{used:.2} / {sym}{limit:.2}")
        })
        .unwrap_or_default();

    // Fix the meta column width across all three rows so the percent
    // column lines up vertically — otherwise per-row meta length makes
    // the gauge column eat different widths and the percentages drift.
    let meta_col_width = [&five_h_meta, &seven_d_meta, &extra_meta]
        .iter()
        .map(|m| m.chars().count() as u16)
        .max()
        .unwrap_or(0)
        .min(28);

    render_gauge_row(f, rows[1], "5h",     five_h_pct,  &five_h_meta,  meta_col_width);
    render_gauge_row(f, rows[2], "weekly", seven_d_pct, &seven_d_meta, meta_col_width);
    render_gauge_row(f, rows[3], "extra",  extra_pct,   &extra_meta,   meta_col_width);
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
    // Always reserved as a fixed-width slot so the $ column lines up
    // across accounts regardless of whether one is "0s ago" and another
    // is "59s ago"; absent live data renders as blank padding.
    spans.push(Span::raw(" · "));
    spans.push(cache_age_span(pa.live.as_ref()));
    spans.push(Span::raw("   "));
    spans.push(Span::styled(
        format!("${:>9.2} total", pa.agg.all.cost_usd),
        Style::default().fg(Color::Green),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Width of the cache-age column — right-aligned so "0s ago", "59m ago"
/// and "23h ago" all occupy the same space. Without this, varying widths
/// across accounts shift the $-column on the right.
const CACHE_AGE_WIDTH: usize = 7;

fn cache_age_span(live: Option<&LiveUsage>) -> Span<'static> {
    let Some(lu) = live else {
        // Pad so absent values don't collapse the column.
        return Span::raw(" ".repeat(CACHE_AGE_WIDTH));
    };
    let age = lu.age_seconds();
    let refresh = REFRESH_INTERVAL.as_secs() as i64;
    let color = if lu.is_stale() {
        Color::Red
    } else if age > 2 * refresh {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let raw = if age < 60 {
        format!("{age}s ago")
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else {
        format!("{}h ago", age / 3600)
    };
    Span::styled(format!("{raw:>CACHE_AGE_WIDTH$}"), Style::default().fg(color))
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
    const LABEL_WIDTH: u16 = 10;   // "  weekly  " — fits longest label "weekly"
    const PCT_WIDTH: u16 = 8;      //  "  100.0% "
    let meta_width = if meta_col_chars == 0 { 1 } else { meta_col_chars + 2 };

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

fn render_sessions_table(
    f: &mut Frame,
    area: Rect,
    sessions: &[&SessionRef],
    selected: Option<usize>,
    table_state: &mut TableState,
) {
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
    let mut block = Block::default().borders(Borders::ALL).title(title);

    if sessions.is_empty() {
        *table_state = TableState::default();
        let inner = block.inner(area);
        f.render_widget(block, area);
        let p = Paragraph::new(Line::from(Span::styled(
            "no sessions touched in the last 2h — start a session or press `r` to rescan",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(p, inner);
        return;
    }

    // Responsive columns — added in priority order as the screen widens.
    // Base columns need ~61 chars (6 columns + 5 spacers + 2 borders, with
    // the model column at 21); each extra column adds (length + 1 spacer).
    // Thresholds include a small buffer so columns don't appear right at
    // the edge of fitting and crop the rightmost `state` column.
    let w = area.width;
    let show_ctx = w >= 72;
    let show_status = w >= 84;
    let show_io = w >= 98;
    let show_cache = w >= 107;

    // `sessions` is already grouped by project (alphabetical), with
    // pid ascending within each group — sort lives in flatten_sessions
    // so selection indexes match visible row order.
    let ordered: &[&SessionRef] = sessions;

    let now = Utc::now();
    // Compute column count up front so header rows pad correctly.
    // Base: account, age, tokens, cost, model, state = 6.
    let mut col_count = 6;
    if show_status { col_count += 1; }
    if show_ctx { col_count += 1; }
    if show_io { col_count += 1; }
    if show_cache { col_count += 1; }

    // Pad project names to the widest one so the "x/y active" count
    // lines up vertically across all group headers regardless of name
    // length or digit count.
    let max_project_len = ordered
        .iter()
        .map(|s| if s.project.is_empty() { "(unknown)".len() } else { s.project.chars().count() })
        .max()
        .unwrap_or(0);

    let mut rows: Vec<Row> = Vec::with_capacity(ordered.len() + 8);
    // Visual row index of the selected session, plus the row scrolling
    // up should stop at — the group's blank+header rows when the cursor
    // sits on the first session of a group, so the "▾ project" line
    // scrolls into view together with the selection.
    let mut selected_row: Option<usize> = None;
    let mut selected_anchor: Option<usize> = None;
    // Visual row index of every session row (blank/header rows excluded)
    // so the off-screen counter counts agents, not table rows.
    let mut session_rows: Vec<usize> = Vec::with_capacity(ordered.len());
    // Project-header rows carry no column data, so they're repainted as
    // one full-width line after the table renders (see overlay below).
    // ratatui would otherwise clip the "▾ project  x/y active" text to the
    // account column. Collect (row index, line) for each header here.
    let mut project_headers: Vec<(usize, Line)> = Vec::new();
    let mut group_start = 0usize;
    while group_start < ordered.len() {
        let project = &ordered[group_start].project;
        let mut group_end = group_start + 1;
        while group_end < ordered.len() && ordered[group_end].project == *project {
            group_end += 1;
        }
        let group = &ordered[group_start..group_end];
        let active_in_group = group.iter().filter(|s| s.state == SessionState::Active).count();
        let total_in_group = group.len();
        let label = if project.is_empty() { "(unknown)" } else { project.as_str() };
        let label_pad = max_project_len.saturating_sub(label.chars().count());
        let count_text = format!("{active_in_group}/{total_in_group} active");
        let any_active = active_in_group > 0;
        let project_color = if any_active { Color::Cyan } else { Color::DarkGray };

        // Blank spacer row before every group — separates groups from
        // each other and gives the first group breathing room under
        // the column-header row.
        let mut blank: Vec<Cell> = Vec::with_capacity(col_count);
        for _ in 0..col_count { blank.push(Cell::from("")); }
        rows.push(Row::new(blank));

        let header_line = Line::from(vec![
            Span::styled(
                format!("▾ {label}"),
                Style::default().fg(project_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(label_pad + 2)),
            Span::styled(count_text, Style::default().fg(Color::DarkGray)),
        ]);
        project_headers.push((rows.len(), header_line.clone()));
        let mut header_cells: Vec<Cell> = Vec::with_capacity(col_count);
        header_cells.push(Cell::from(header_line));
        for _ in 1..col_count {
            header_cells.push(Cell::from(""));
        }
        rows.push(Row::new(header_cells));

        for (offset, s) in group.iter().enumerate() {
            let age_secs = (now - s.state_since).num_seconds().max(0);
            let is_selected = selected == Some(group_start + offset);
            if is_selected {
                selected_row = Some(rows.len());
                selected_anchor = Some(if offset == 0 {
                    rows.len().saturating_sub(2)
                } else {
                    rows.len()
                });
            }
            let arrow = if is_selected { "▶ " } else { "  " };
            // A session mewxi just asked to close: every column dashes
            // out and the status reads a red `killing` so it's instantly
            // clear the agent is going away. Built before the normal row
            // so the dead session's stale token/model/state values never
            // flash.
            if s.killing {
                let dash = || Cell::from(Span::styled("—", Style::default().fg(Color::DarkGray)));
                let mut cells: Vec<Cell> = vec![
                    Cell::from(format!("  {arrow}—")),
                    dash(),
                ];
                if show_status {
                    cells.push(Cell::from(Span::styled(
                        "killing",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )));
                }
                if show_ctx { cells.push(dash()); }
                cells.push(dash()); // tokens
                if show_io { cells.push(dash()); }
                if show_cache { cells.push(dash()); }
                cells.push(dash()); // cost
                cells.push(dash()); // model
                cells.push(dash()); // state
                let style = if is_selected {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                session_rows.push(rows.len());
                rows.push(Row::new(cells).style(style));
                continue;
            }
            let state_label = match s.state {
                SessionState::Active => "active",
                SessionState::Idle => "idle",
            };
            let state_color = match s.state {
                SessionState::Active => Color::Green,
                SessionState::Idle => Color::DarkGray,
            };
            let base_style = if is_selected {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else if s.state == SessionState::Idle {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let mut cells: Vec<Cell> = vec![
                Cell::from(format!("  {arrow}{}", s.account_name)),
                Cell::from(fmt_age(age_secs)),
            ];
            if show_status {
                let (lbl, color) = activity_display(&s.activity);
                cells.push(Cell::from(Span::styled(lbl, Style::default().fg(color))));
            }
            if show_ctx {
                cells.push(Cell::from(fmt_ctx(s.current_context, s.context_cap)));
            }
            cells.push(Cell::from(fmt_tokens_compact(s.tokens)));
            if show_io {
                cells.push(Cell::from(format!(
                    "{}/{}",
                    fmt_tokens_compact(s.totals.input),
                    fmt_tokens_compact(s.totals.output),
                )));
            }
            if show_cache {
                cells.push(Cell::from(fmt_tokens_compact(s.totals.cache_read)));
            }
            cells.push(Cell::from(format!("${:.2}", s.cost_usd)));
            // Model + thinking budget, e.g. `opus:xhigh` / `sonnet[1M]:medium`.
            // The effort suffix is omitted when the model has no effort
            // support (Haiku) or nothing is configured, and coloured on the
            // same thermometer gradient as the session-detail badge.
            let model_label = short_model(&s.model, s.context_cap == Some(1_000_000));
            let model_cell = match s.effort.as_deref() {
                Some(eff) => Cell::from(Line::from(vec![
                    Span::raw(model_label),
                    Span::styled(
                        format!(":{eff}"),
                        Style::default().fg(super::view_session::effort_color(eff)),
                    ),
                ])),
                None => Cell::from(model_label),
            };
            cells.push(model_cell);
            cells.push(Cell::from(Span::styled(state_label, Style::default().fg(state_color))));
            session_rows.push(rows.len());
            rows.push(Row::new(cells).style(base_style));

            // Indented child rows for the sub-agents this session is
            // running right now. They are display-only: excluded from
            // `session_rows` so the off-screen counter keeps counting
            // sessions, and never assigned a selection index so ↑/↓
            // navigation continues to step session-to-session.
            for sub in &s.subagents {
                rows.push(subagent_row(
                    sub, now, show_status, show_ctx, show_io, show_cache,
                ));
            }
        }

        group_start = group_end;
    }

    // Account names are indented by 4 chars under project headers ("  ▶ "
    // or "    "), so the column label needs the same lead-in to line up.
    let mut header_labels: Vec<&'static str> = vec!["    account", "age"];
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Min(14),
        Constraint::Length(6),
    ];
    if show_status {
        header_labels.push("status");
        // Longest label is "delegating" (10 chars); pad to 11 so the
        // header "status" + value never bump the next column.
        constraints.push(Constraint::Length(11));
    }
    if show_ctx {
        header_labels.push("ctx");
        constraints.push(Constraint::Length(5));
    }
    header_labels.push("tokens");
    constraints.push(Constraint::Length(8));
    if show_io {
        header_labels.push("in/out");
        // "999k/999k" = 9 chars, "1.00M/1.00M" worst case = 11. Header
        // "in/out" is 6 chars. Give it 11 so both values stay legible.
        constraints.push(Constraint::Length(11));
    }
    if show_cache {
        header_labels.push("cache");
        constraints.push(Constraint::Length(7));
    }
    header_labels.push("cost");
    constraints.push(Constraint::Length(9));
    header_labels.push("model");
    // Worst case is `sonnet-4.6[1M]:medium` (21 chars); size to fit so the
    // thinking-budget suffix never gets clipped.
    constraints.push(Constraint::Length(21));
    header_labels.push("state");
    constraints.push(Constraint::Length(7));

    let row_count = rows.len();

    // Stateful render so a selection past the visible window scrolls the
    // table. The offset persists across frames (owned by the run loop);
    // ratatui only ever scrolls the minimum needed to keep the selected
    // row visible, so the list stays put while the cursor moves within
    // the window and follows it once it hits an edge. The anchor pulls
    // the offset back up to the group header when scrolling upward.
    table_state.select(selected_row);
    if let Some(anchor) = selected_anchor {
        if table_state.offset() > anchor {
            *table_state.offset_mut() = anchor;
        }
    }
    if table_state.offset() >= row_count {
        *table_state.offset_mut() = row_count.saturating_sub(1);
    }

    // Off-screen agent counter on the bottom border. ratatui adjusts the
    // offset during render to keep the selection visible, but the border
    // title must be baked in beforehand — so replicate that adjustment
    // (all rows are height 1) to know which window will actually be drawn.
    let viewport = area.height.saturating_sub(3) as usize; // 2 borders + header row
    let mut final_offset = table_state.offset();
    if let Some(sel) = selected_row {
        if sel < final_offset {
            final_offset = sel;
        } else if viewport > 0 && sel >= final_offset + viewport {
            final_offset = sel + 1 - viewport;
        }
    }
    let above = session_rows.iter().filter(|&&r| r < final_offset).count();
    let below = session_rows
        .iter()
        .filter(|&&r| r >= final_offset + viewport)
        .count();
    let mut parts: Vec<String> = Vec::new();
    if above > 0 {
        parts.push(format!("↑ {above}"));
    }
    if below > 0 {
        parts.push(format!("↓ {below}"));
    }
    if !parts.is_empty() {
        // Successive left-aligned titles render one space apart, so this
        // lands right after "Sessions — N active · N idle" on the top
        // border.
        block = block.title(Line::from(Span::styled(
            format!("· {} off-screen", parts.join(" · ")),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let table = Table::new(rows, constraints)
        .header(
            Row::new(header_labels)
                .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
        )
        // ratatui 0.28 defaults Table to Flex::Start, so Constraint::Min
        // no longer absorbs leftover width — the project column would sit
        // at 14 chars and leave a gap after the rightmost column. Legacy
        // flex restores the "fill remaining space" behavior.
        .flex(Flex::Legacy)
        .block(block);

    f.render_stateful_widget(table, area, table_state);

    // Repaint each visible project header as one full-width line. The table
    // clipped it to the account column (header rows have no column data), so
    // draw the full "▾ project  x/y active" line over the top, spanning the
    // inner width. `final_offset`/`viewport` mirror the window the table
    // actually drew — the same values the off-screen counter relies on — so
    // the y positions line up. Header text is a prefix of the full line, so
    // overwriting from the same x fully covers the clipped remnant.
    let inner_x = area.x + 1;
    let inner_y = area.y + 1;
    let inner_w = area.width.saturating_sub(2);
    let buf = f.buffer_mut();
    for (hr, line) in &project_headers {
        if *hr < final_offset || *hr >= final_offset + viewport {
            continue;
        }
        let y = inner_y + 1 + (*hr - final_offset) as u16;
        buf.set_line(inner_x, y, line, inner_w);
    }
}

/// One indented child row beneath a session for a sub-agent it is
/// currently running. These rows only exist while the delegation is live
/// (it's dropped the moment the parent records its result), so the row is
/// styled to read as *active* — readable text, a coloured live-activity
/// column, and a green `active` state — rather than reusing the dim style
/// idle sessions wear. Cost and the in/out/cache columns dash out: a
/// sub-agent's tokens already roll up into the parent session's totals, so
/// re-billing them here would double-count.
fn subagent_row(
    sub: &crate::subagents::SubAgent,
    now: chrono::DateTime<chrono::Utc>,
    show_status: bool,
    show_ctx: bool,
    show_io: bool,
    show_cache: bool,
) -> Row<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let text = Style::default().fg(Color::Gray);
    // Indent two further than the session's "  ▶ name" so the tree nests:
    //   ▾ project
    //     ▶ account        (session)
    //       ↳ Explore · …  (sub-agent)
    let mut label = vec![Span::styled("      ↳ ", Style::default().fg(Color::Cyan))];
    match &sub.agent_type {
        Some(t) => {
            label.push(Span::styled(
                t.clone(),
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ));
            label.push(Span::styled(format!(" · {}", sub.description), text));
        }
        None => label.push(Span::styled(sub.description.clone(), text)),
    }
    let age = (now - sub.last_activity).num_seconds().max(0);
    let mut cells: Vec<Cell> = vec![
        Cell::from(Line::from(label)),
        Cell::from(Span::styled(fmt_age(age), dim)),
    ];
    if show_status {
        let (lbl, color) = activity_display(&sub.activity);
        cells.push(Cell::from(Span::styled(lbl, Style::default().fg(color))));
    }
    if show_ctx {
        cells.push(Cell::from(Span::styled("—", dim)));
    }
    cells.push(Cell::from(Span::styled(fmt_tokens_compact(sub.tokens), text)));
    // in/out and cache mirror a session row so the cache-heavy split is
    // visible (a sub-agent's tokens are mostly cache reads). The figures
    // come from the sub-agent's own transcript and don't overlap the
    // parent session's totals.
    if show_io {
        cells.push(Cell::from(Span::styled(
            format!(
                "{}/{}",
                fmt_tokens_compact(sub.totals.input),
                fmt_tokens_compact(sub.totals.output),
            ),
            text,
        )));
    }
    if show_cache {
        cells.push(Cell::from(Span::styled(
            fmt_tokens_compact(sub.totals.cache_read),
            text,
        )));
    }
    cells.push(Cell::from(Span::styled("—", dim))); // cost — rolled into the account aggregate
    cells.push(Cell::from(Span::styled(short_model(&sub.model, false), text)));
    cells.push(Cell::from(Span::styled("active", Style::default().fg(Color::Green))));
    Row::new(cells)
}

fn activity_display(a: &Activity) -> (String, Color) {
    let color = match a {
        Activity::Waiting => Color::DarkGray,
        Activity::Starting => Color::Cyan,
        Activity::Thinking => Color::Cyan,
        Activity::Writing => Color::Green,
        Activity::Reading => Color::Blue,
        Activity::Editing => Color::Yellow,
        Activity::Searching => Color::Blue,
        Activity::Fetching => Color::Blue,
        Activity::Running => Color::Magenta,
        Activity::Delegating => Color::Magenta,
        Activity::Asking => Color::Yellow,
        Activity::Awaiting => Color::Red,
        Activity::Compacting => Color::LightBlue,
        Activity::Tool(_) => Color::White,
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

/// Short model slug for the table, with the version (`opus-4.8`) and the
/// extended-context marker appended when the session is on the 1M tier so
/// it reads `sonnet-4.6[1M]` rather than masquerading as the 200K variant
/// (mirrors the statusline).
fn short_model(m: &str, extended_ctx: bool) -> String {
    let lower = m.to_ascii_lowercase();
    let base = if lower.contains("opus") {
        "opus"
    } else if lower.contains("sonnet") {
        "sonnet"
    } else if lower.contains("haiku") {
        "haiku"
    } else {
        return m.chars().take(8).collect();
    };
    let mut label = match model_version(&lower) {
        Some(ver) => format!("{base}-{ver}"),
        None => base.to_string(),
    };
    // Skip the badge for families that are natively 1M (Fable, Opus 4.8+) —
    // it only carries meaning for the 200K models on the opt-in 1M tier.
    if extended_ctx && !crate::stats::native_1m_context(&lower) {
        label.push_str("[1M]");
    }
    label
}

/// Extract the `major.minor` version from a model id (`claude-opus-4-8[1m]`
/// → `4.8`). Joins the first two short numeric segments after the family
/// name with a dot; the trailing date stamp (`…-4-5-20251001`) and the
/// `[1m]` tier suffix are ignored. Returns `None` when no version is found.
fn model_version(lower: &str) -> Option<String> {
    let cleaned = lower.replace("[1m]", "");
    let mut nums = cleaned
        .split('-')
        .filter(|seg| seg.len() <= 2 && seg.chars().all(|c| c.is_ascii_digit()) && !seg.is_empty());
    let major = nums.next()?;
    match nums.next() {
        Some(minor) => Some(format!("{major}.{minor}")),
        None => Some(major.to_string()),
    }
}

/// Age column is 6 chars wide. Always include the next-finer unit so the
/// value ticks visibly every second/minute rather than freezing between
/// rollovers — when the marker flips the column has to *look* like it
/// reset, which is invisible if "5m" sits there for 60s before becoming
/// "6m". Max widths: "59m59s", "23h59m", "99d23h" — all exactly 6 chars.
fn fmt_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::short_model;

    #[test]
    fn short_model_includes_version() {
        // Opus 4.8 is natively 1M, so the badge is suppressed even on the
        // extended tier; Sonnet keeps it.
        assert_eq!(short_model("claude-opus-4-8[1m]", true), "opus-4.8");
        assert_eq!(short_model("claude-sonnet-4-6", true), "sonnet-4.6[1M]");
        assert_eq!(short_model("claude-sonnet-4-6", false), "sonnet-4.6");
        assert_eq!(short_model("claude-haiku-4-5-20251001", false), "haiku-4.5");
    }

    #[test]
    fn short_model_unknown_family_truncates() {
        assert_eq!(short_model("gpt-9-mega", false), "gpt-9-me");
    }
}
