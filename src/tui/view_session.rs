//! View 2 — selected session detail.
//!
//! Top band: the parent account's three gauges. Middle: session token
//! breakdown. Then a formatted chat log read from the session's JSONL
//! transcript. Footer: keybinding hints.

use super::widgets::{self, fmt_tokens_compact};
use super::{PerAccount, SessionRef};
use crate::chat_log::{self, ChatEntry, EntryKind};
use crate::live_session::SessionState;
use crate::stats::fmt_num;
use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

pub fn render(
    f: &mut Frame,
    area: Rect,
    accounts: &[&PerAccount],
    session: Option<&SessionRef>,
    chat_scroll: &mut usize,
) {
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
            Constraint::Length(3), // header
            Constraint::Length(4), // 3 gauges
            Constraint::Length(8), // session breakdown
            Constraint::Length(3), // meta
            Constraint::Min(4),    // chat log
            Constraint::Length(1), // footer
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
    render_chat_log(f, rows[4], session, chat_scroll);
    widgets::render_footer(f, rows[5], "1 all  3 account  PgUp/PgDn scroll  End live");
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
    let ctx_span = match (s.current_context, s.context_cap) {
        (Some(cur), Some(cap)) => {
            let pct = (cur as f64 / cap as f64 * 100.0).min(999.0);
            let color = if pct >= 85.0 {
                Color::Red
            } else if pct >= 60.0 {
                Color::Yellow
            } else {
                Color::Green
            };
            Span::styled(
                format!(
                    "{pct:>5.1}%  ({}/{})",
                    fmt_tokens_compact(cur),
                    fmt_tokens_compact(cap)
                ),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        }
        _ => Span::styled("n/a", Style::default().fg(Color::DarkGray)),
    };
    let line = Line::from(vec![
        Span::styled("context ", Style::default().fg(Color::DarkGray)),
        ctx_span,
        Span::raw("    "),
        Span::styled("last active ", Style::default().fg(Color::DarkGray)),
        Span::styled(fmt_age(age), Style::default().fg(Color::Yellow)),
        Span::raw("    "),
        Span::styled("folder ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            s.cwd.to_string_lossy().into_owned(),
            Style::default().fg(Color::Cyan),
        ),
    ]);
    f.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title("Session meta")),
        area,
    );
}

const LABEL_W: usize = 9; // "you      ", "claude   ", "tool→    ", etc.

fn render_chat_log(f: &mut Frame, area: Rect, s: &SessionRef, scroll: &mut usize) {
    let entries = chat_log::read(&s.transcript_path);
    let width = area.width.saturating_sub(2) as usize;
    let body_w = width.saturating_sub(LABEL_W).max(10);
    let target_h = area.height.saturating_sub(2) as usize;

    // Build all visible-style lines so we can compute total + scroll
    // bounds. (Cheap enough; transcripts are bounded by a session.)
    let mut all: Vec<Line<'static>> = Vec::with_capacity(entries.len() * 2);
    for e in &entries {
        all.extend(entry_to_lines(e, body_w));
    }
    let total = all.len();
    let max_scroll = total.saturating_sub(target_h);
    // Clamp the caller's state so repeated PgUp at the top (or PgDn at
    // the bottom) doesn't pile up off-screen scroll the user then has to
    // unwind before anything visibly happens.
    if *scroll > max_scroll {
        *scroll = max_scroll;
    }
    let clamped = *scroll;

    let title = if clamped == 0 {
        format!("Chat log ({} entries) — tailing", entries.len())
    } else {
        format!(
            "Chat log ({} entries) — scrolled {}/{} lines back",
            entries.len(),
            clamped,
            max_scroll
        )
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if entries.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no chat content yet — waiting for transcript",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(hint, inner);
        return;
    }

    let end = total.saturating_sub(clamped);
    let start = end.saturating_sub(target_h);
    let visible: Vec<Line<'static>> = all[start..end].to_vec();
    f.render_widget(Paragraph::new(visible), inner);
}

fn entry_to_lines(e: &ChatEntry, body_w: usize) -> Vec<Line<'static>> {
    let (label, label_style, body_style) = match &e.kind {
        EntryKind::User => (
            "you      ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            Style::default().fg(Color::White),
        ),
        EntryKind::Assistant => (
            "claude   ",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            Style::default().fg(Color::White),
        ),
        EntryKind::Thinking => (
            "thinking ",
            Style::default().fg(Color::Magenta),
            Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC),
        ),
        EntryKind::ToolUse { name, input_summary } => {
            let head = if input_summary.is_empty() {
                name.clone()
            } else {
                format!("{name}  {input_summary}")
            };
            return build_lines(
                "tool→    ",
                Style::default().fg(Color::Yellow),
                Style::default().fg(Color::Yellow),
                &head,
                body_w,
            );
        }
        EntryKind::ToolResult { ok } => {
            let marker = if *ok { "tool✓    " } else { "tool✗    " };
            let color = if *ok { Color::DarkGray } else { Color::Red };
            return build_lines(
                marker,
                Style::default().fg(color),
                Style::default().fg(color),
                &e.text,
                body_w,
            );
        }
        EntryKind::System => (
            "system   ",
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::DarkGray),
        ),
    };
    build_lines(label, label_style, body_style, &e.text, body_w)
}

fn build_lines(
    label: &str,
    label_style: Style,
    body_style: Style,
    text: &str,
    body_w: usize,
) -> Vec<Line<'static>> {
    let wrapped = wrap_text(text, body_w);
    if wrapped.is_empty() {
        return vec![Line::from(Span::styled(label.to_string(), label_style))];
    }
    let indent: String = " ".repeat(LABEL_W);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(wrapped.len());
    out.push(Line::from(vec![
        Span::styled(label.to_string(), label_style),
        Span::styled(wrapped[0].clone(), body_style),
    ]));
    for cont in &wrapped[1..] {
        out.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(cont.clone(), body_style),
        ]));
    }
    out
}

/// Word-wrap honouring existing newlines. Splits long words by char
/// boundary if a single word doesn't fit.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.lines().map(|l| l.to_string()).collect();
    }
    let mut out: Vec<String> = Vec::new();
    for src_line in text.lines() {
        let trimmed = src_line.trim_end();
        if trimmed.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_w = 0usize;
        for word in trimmed.split_whitespace() {
            let wlen = word.chars().count();
            if current_w == 0 {
                if wlen > width {
                    for chunk in chunk_chars(word, width) {
                        out.push(chunk);
                    }
                } else {
                    current.push_str(word);
                    current_w = wlen;
                }
            } else if current_w + 1 + wlen <= width {
                current.push(' ');
                current.push_str(word);
                current_w += 1 + wlen;
            } else {
                out.push(std::mem::take(&mut current));
                current_w = 0;
                if wlen > width {
                    for chunk in chunk_chars(word, width) {
                        out.push(chunk);
                    }
                } else {
                    current.push_str(word);
                    current_w = wlen;
                }
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out
}

fn chunk_chars(word: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    chars
        .chunks(width)
        .map(|c| c.iter().collect::<String>())
        .collect()
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
