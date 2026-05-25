//! View 1 — all accounts + all currently-running sessions.
//!
//! Top: one block per account with a header line (name · live count ·
//! cost) and three aligned gauges (5h / weekly / extra) showing
//! `[bar] pct  meta`. Bottom: one flat table of every live session
//! across every account, sorted by most-recent activity.

use super::widgets::{fmt_tokens_compact, gauge_color, render_footer};
use super::{PerAccount, SessionRef};
use crate::live_session::{Activity, SessionState};
use crate::live_usage::{LiveUsage, REFRESH_INTERVAL};
use chrono::{Local, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
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
    selected: Option<usize>,
) {
    // Reserve enough rows for every account block, capped so the
    // sessions table always gets at least 5 rows.
    let want = (ROWS_PER_ACCOUNT * accounts.len() as u16) + 2; // +2 for borders
    let max_for_accounts = area.height.saturating_sub(6);
    let acct_height = want.min(max_for_accounts).max(5);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(acct_height), // per-account gauges
            Constraint::Min(5),               // live sessions table
            Constraint::Length(1),            // footer
        ])
        .split(area);

    render_account_stack(f, rows[0], accounts);
    render_sessions_table(f, rows[1], sessions, selected);
    render_footer(f, rows[2], "1 here · 2 session · 3 account · 4 setup · m mewxi");
}

fn render_account_stack(f: &mut Frame, area: Rect, accounts: &[&PerAccount]) {
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

fn render_sessions_table(f: &mut Frame, area: Rect, sessions: &[&SessionRef], selected: Option<usize>) {
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
            "no sessions touched in the last 2h — start a session or press `r` to rescan",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(p, inner);
        return;
    }

    // Responsive columns — added in priority order as the screen widens.
    // Base columns need ~57 chars (6 columns + 5 spacers + 2 borders);
    // each extra column adds (length + 1 spacer). Thresholds include a
    // small buffer so columns don't appear right at the edge of fitting.
    let w = area.width;
    let show_ctx = w >= 68;
    let show_status = w >= 80;
    let show_io = w >= 94;
    let show_cache = w >= 103;

    // Sort by project (alphabetical, case-insensitive), then within each
    // project keep the existing order (active first, then idle, newest
    // first). Project becomes a header row above each group, so the
    // per-row project column goes away.
    let mut ordered: Vec<&SessionRef> = sessions.to_vec();
    ordered.sort_by(|a, b| {
        let pa = a.project.to_ascii_lowercase();
        let pb = b.project.to_ascii_lowercase();
        let rank = |s: SessionState| match s {
            SessionState::Active => 0,
            SessionState::Idle => 1,
        };
        pa.cmp(&pb)
            .then_with(|| rank(a.state).cmp(&rank(b.state)))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });

    // Selection comes in as an index into the original `sessions` slice.
    // Resolve it to a pointer so we can match the right row after sorting.
    let selected_ptr: Option<*const SessionRef> = selected
        .and_then(|i| sessions.get(i).copied())
        .map(|s| s as *const SessionRef);

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
    let mut group_start = 0usize;
    let mut first_group = true;
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

        // Blank spacer row between groups (not before the first).
        if !first_group {
            let mut blank: Vec<Cell> = Vec::with_capacity(col_count);
            for _ in 0..col_count { blank.push(Cell::from("")); }
            rows.push(Row::new(blank));
        }
        first_group = false;

        let mut header_cells: Vec<Cell> = Vec::with_capacity(col_count);
        header_cells.push(Cell::from(Line::from(vec![
            Span::styled(
                format!("▾ {label}"),
                Style::default().fg(project_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(label_pad + 2)),
            Span::styled(count_text, Style::default().fg(Color::DarkGray)),
        ])));
        for _ in 1..col_count {
            header_cells.push(Cell::from(""));
        }
        rows.push(Row::new(header_cells));

        for s in group {
            let age_secs = (now - s.state_since).num_seconds().max(0);
            let is_selected = selected_ptr == Some(*s as *const SessionRef);
            let arrow = if is_selected { "▶ " } else { "  " };
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
            cells.push(Cell::from(short_model(&s.model)));
            cells.push(Cell::from(Span::styled(state_label, Style::default().fg(state_color))));
            rows.push(Row::new(cells).style(base_style));
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
    constraints.push(Constraint::Length(8));
    header_labels.push("state");
    constraints.push(Constraint::Length(7));

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
    f.render_widget(table, area);
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
