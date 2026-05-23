//! View 2 — selected session detail.
//!
//! Top band: the parent account's three gauges. Middle: session token
//! breakdown. Then a formatted chat log read from the session's JSONL
//! transcript. Footer: keybinding hints.

use super::markdown;
use super::widgets::{self, fmt_tokens_compact};
use super::{PerAccount, SessionRef};
use crate::chat_log::{self, ChatEntry, EntryKind};
use serde_json::Value;
use std::path::Path;
use crate::live_session::SessionState;
use crate::stats::fmt_num;
use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Driver pane state passed in from [`super::run_loop`] when the
/// currently-pinned session is one mewxi spawned and owns the PTY for.
/// `None` means the session is just being observed; the input row is
/// not rendered in that case.
pub struct DriverPane<'a> {
    /// What the user has typed but not yet submitted.
    pub input: &'a str,
    /// True while the input row has keyboard focus. Renders a bright
    /// cursor; otherwise the row is dim with an `i to type` hint.
    pub focused: bool,
    /// True when the terminal overlay (claude's PTY screen) is up. The
    /// footer hint switches to advertise passthrough + Ctrl-] dismiss.
    pub overlay_active: bool,
}

/// Placeholder pane for a session mewxi spawned but whose JSONL
/// session marker has not yet appeared. Shown immediately after `n`
/// so the keypress feels instant — the real chat log takes over once
/// the marker promotion runs in the parent event loop.
pub struct PendingPane {
    pub account_name: String,
    pub cwd: std::path::PathBuf,
    pub elapsed: std::time::Duration,
    pub last_output: Option<String>,
}

pub fn render(
    f: &mut Frame,
    area: Rect,
    accounts: &[&PerAccount],
    session: Option<&SessionRef>,
    chat_scroll: &mut usize,
    changes_selection: &mut Option<usize>,
    last_change_count: &mut usize,
    detail_scroll: &mut usize,
    chat_rect: &mut Option<Rect>,
    actions_rect: &mut Option<Rect>,
    detail_rect: &mut Option<Rect>,
    driver: Option<&DriverPane<'_>>,
    pending: Option<&PendingPane>,
) {
    if let Some(p) = pending {
        render_pending(f, area, accounts, p);
        return;
    }
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

    let mut constraints = vec![
        Constraint::Length(3), // header
        Constraint::Length(4), // 3 gauges
        Constraint::Length(3), // compact session totals
        Constraint::Length(3), // meta
        Constraint::Min(4),    // chat log
    ];
    if driver.is_some() {
        // 3-row input pane: borders + one row of text.
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Length(1)); // keybind footer
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
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
    render_chat_log(
        f,
        rows[4],
        session,
        chat_scroll,
        changes_selection,
        last_change_count,
        detail_scroll,
        chat_rect,
        actions_rect,
        detail_rect,
    );
    let default_hint =
        "↑/↓ Tab switch · PgUp/PgDn chat · j/k actions · J/K detail · K kill (2×) · Esc back";
    let footer_hint = match driver {
        Some(d) if d.overlay_active => {
            "claude is asking — keys pass through  ·  Ctrl-] dismiss"
        }
        Some(d) if d.focused => {
            "Enter send  Shift-Tab cycle mode  Esc unfocus  Ctrl-D end  Ctrl-C cancel"
        }
        Some(_) => {
            "i type  m model  Shift-Tab cycle mode  Ctrl-D end  K kill (2×)  1 all"
        }
        None => default_hint,
    };
    if let Some(d) = driver {
        render_driver_input(f, rows[5], d);
        widgets::render_footer(f, rows[6], "2", footer_hint);
    } else {
        widgets::render_footer(f, rows[5], "2", footer_hint);
    }
}

fn render_pending(f: &mut Frame, area: Rect, accounts: &[&PerAccount], p: &PendingPane) {
    let parent = accounts.iter().find(|a| a.account.name == p.account_name);
    let mut rows = vec![
        Constraint::Length(3), // header
        Constraint::Length(4), // gauges (optional, shown if parent found)
        Constraint::Min(4),    // pending box
        Constraint::Length(1), // footer
    ];
    if parent.is_none() {
        rows.remove(1);
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(rows.clone())
        .split(area);

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            format!("[{}]", p.account_name),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            "starting claude…",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL).title("Session detail"));
    f.render_widget(header, chunks[0]);

    let mut idx = 1;
    if let Some(pa) = parent {
        let gauge_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(33),
                Constraint::Percentage(33),
            ])
            .split(chunks[idx]);
        widgets::render_5h_gauge(f, gauge_row[0], &pa.agg, pa.live.as_ref());
        widgets::render_7d_gauge(f, gauge_row[1], pa.live.as_ref());
        widgets::render_extra_gauge(f, gauge_row[2], pa.live.as_ref());
        idx += 1;
    }

    let pending_area = chunks[idx];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Spawning ")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(pending_area);
    f.render_widget(block, pending_area);

    let secs = p.elapsed.as_secs();
    // Animated trailing dots: 0..=3, ticked roughly twice per second.
    let dots_count = ((p.elapsed.as_millis() / 500) % 4) as usize;
    let dots: String = ".".repeat(dots_count);

    // Pick the largest mascot that fits with room for a 4-line caption
    // (blank + meowing + blank + folder line) below it.
    let caption_h: u16 = 4;
    let (mascot, mascot_h, mascot_w) = pick_mascot(inner.width, inner.height, caption_h);

    let vrows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(mascot_h),
            Constraint::Length(caption_h),
            Constraint::Min(0),
        ])
        .split(inner);

    if !mascot.is_empty() && mascot_w <= vrows[0].width {
        let pad = vrows[0].width.saturating_sub(mascot_w) / 2;
        let mascot_area = Rect {
            x: vrows[0].x + pad,
            y: vrows[0].y,
            width: mascot_w,
            height: mascot_h.min(vrows[0].height),
        };
        f.render_widget(
            Paragraph::new(mascot).style(Style::default().fg(Color::Magenta)),
            mascot_area,
        );
    }

    let cwd_display = p.cwd.to_string_lossy().into_owned();
    let caption_lines: Vec<Line<'static>> = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!("meowing new agent{:<3}", dots),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({}s)", secs),
                Style::default().fg(Color::DarkGray),
            ),
        ])
        .alignment(ratatui::layout::Alignment::Center),
        Line::from(""),
        Line::from(vec![
            Span::styled("under ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                p.account_name.clone(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ·  ", Style::default().fg(Color::DarkGray)),
            Span::styled(cwd_display, Style::default().fg(Color::Cyan)),
        ])
        .alignment(ratatui::layout::Alignment::Center),
    ];
    f.render_widget(Paragraph::new(caption_lines), vrows[1]);
    let _ = p.last_output;
    idx += 1;
    widgets::render_footer(
        f,
        chunks[idx],
        "2",
        "Esc cancel via K  ·  1 back to all sessions",
    );
}

fn render_driver_input(f: &mut Frame, area: Rect, d: &DriverPane<'_>) {
    let (border_color, title) = if d.focused {
        (Color::Green, " Drive (focused) ")
    } else {
        (Color::DarkGray, " Drive ")
    };
    let mut spans: Vec<Span> = vec![Span::styled(
        "> ",
        Style::default().fg(if d.focused { Color::Green } else { Color::DarkGray }),
    )];
    if d.input.is_empty() {
        if d.focused {
            spans.push(Span::styled(
                "█",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                "(press i to type a prompt and Enter to send)",
                Style::default().fg(Color::DarkGray),
            ));
        }
    } else {
        spans.push(Span::raw(d.input.to_string()));
        if d.focused {
            spans.push(Span::styled(
                "█",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ));
        }
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(border_color)),
        ),
        area,
    );
}

/// Map a raw permission-mode string from the transcript to the badge
/// label + colour shown in the header. `default` reads as "manual"
/// because that's how Claude Code presents it to users.
fn mode_badge(raw: &str) -> (&'static str, Color) {
    match raw {
        "default" => ("manual", Color::DarkGray),
        "auto" => ("auto", Color::Yellow),
        "acceptEdits" => ("accept edits", Color::Cyan),
        "plan" => ("plan", Color::Magenta),
        _ => ("?", Color::DarkGray),
    }
}

/// True when the primary model badge already refers to the same model
/// as the latest assistant record. `primary` is often a short slug
/// (`haiku`, `sonnet`, `opus`, or the `default` placeholder) while
/// `active` is the full transcript name (`claude-sonnet-4-6`); a
/// case-insensitive substring match either way handles both.
fn models_match(primary: &str, active: &str) -> bool {
    if primary == active {
        return true;
    }
    let p = primary.to_ascii_lowercase();
    let a = active.to_ascii_lowercase();
    a.contains(&p) || p.contains(&a)
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
    ];
    if !s.model.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            s.model.clone(),
            Style::default().fg(Color::Green),
        ));
    }
    // Transient "via …" indicator when claude's latest assistant
    // record (sub-agent or plan-mode helper) is a different model
    // than the user's pick. Snaps back to invisible on the next
    // main-agent response in the user's chosen model.
    if !s.active_model.is_empty()
        && !s.model.is_empty()
        && !models_match(&s.model, &s.active_model)
    {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("via {}", s.active_model),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    if let Some(mode_raw) = s.permission_mode.as_deref() {
        let (label, color) = mode_badge(mode_raw);
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("[mode: {label}]"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }
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
    let t = &s.totals;
    let label = |k: &'static str| Span::styled(k, Style::default().fg(Color::DarkGray));
    let val = |v: String| {
        Span::styled(
            v,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )
    };
    let sep = || Span::styled("  ·  ", Style::default().fg(Color::DarkGray));
    let line = Line::from(vec![
        label("msgs "),
        val(fmt_num(t.messages)),
        sep(),
        label("in "),
        val(fmt_tokens_compact(t.input)),
        sep(),
        label("out "),
        val(fmt_tokens_compact(t.output)),
        sep(),
        label("cache r/w "),
        val(format!(
            "{}/{}",
            fmt_tokens_compact(t.cache_read),
            fmt_tokens_compact(t.cache_write_5m + t.cache_write_1h)
        )),
        sep(),
        label("cost "),
        val(format!("${:.4}", t.cost_usd)),
    ]);
    f.render_widget(
        Paragraph::new(line)
            .block(Block::default().borders(Borders::ALL).title("Tokens this session")),
        area,
    );
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

const WIDE_BREAKPOINT: u16 = 130;

struct ChangeRow {
    ok: Option<bool>,
    name: String,
    input: Value,
    result: Option<String>,
}

fn collect_change_rows(entries: &[ChatEntry]) -> Vec<ChangeRow> {
    let mut rows: Vec<ChangeRow> = Vec::new();
    for e in entries {
        match &e.kind {
            EntryKind::ToolUse { name, input } => {
                rows.push(ChangeRow {
                    ok: None,
                    name: name.clone(),
                    input: input.clone(),
                    result: None,
                });
            }
            EntryKind::ToolResult { ok } => {
                if let Some(r) = rows.iter_mut().rev().find(|r| r.ok.is_none()) {
                    r.ok = Some(*ok);
                    r.result = Some(e.text.clone());
                }
            }
            _ => {}
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn render_chat_log(
    f: &mut Frame,
    area: Rect,
    s: &SessionRef,
    scroll: &mut usize,
    changes_selection: &mut Option<usize>,
    last_change_count: &mut usize,
    detail_scroll: &mut usize,
    chat_rect: &mut Option<Rect>,
    actions_rect: &mut Option<Rect>,
    detail_rect: &mut Option<Rect>,
) {
    let entries = chat_log::read(&s.transcript_path);

    let wide = area.width >= WIDE_BREAKPOINT;
    let (chat_area, changes_area) = if wide {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };
    *chat_rect = Some(chat_area);

    let width = chat_area.width.saturating_sub(2) as usize;
    let body_w = width.saturating_sub(LABEL_W).max(10);
    let target_h = chat_area.height.saturating_sub(2) as usize;

    let mut all: Vec<Line<'static>> = Vec::with_capacity(entries.len() * 2);
    for e in &entries {
        all.extend(entry_to_lines(e, body_w, &s.cwd));
    }
    let total = all.len();
    let max_scroll = total.saturating_sub(target_h);
    if *scroll > max_scroll {
        *scroll = max_scroll;
    }
    let clamped = *scroll;

    let mut title = if clamped == 0 {
        format!("Chat log ({} entries) — tailing", entries.len())
    } else {
        format!(
            "Chat log ({} entries) — scrolled {}/{} lines back",
            entries.len(),
            clamped,
            max_scroll
        )
    };
    if wide {
        title.push_str(" · actions →");
    } else {
        let has_tool = entries
            .iter()
            .any(|e| matches!(e.kind, EntryKind::ToolUse { .. }));
        if has_tool {
            title.push_str(" · actions hidden (resize ≥130 cols for Actions pane)");
        }
    }
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(chat_area);
    f.render_widget(block, chat_area);

    if entries.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no chat content yet — waiting for transcript",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(hint, inner);
    } else {
        let end = total.saturating_sub(clamped);
        let start = end.saturating_sub(target_h);
        let visible: Vec<Line<'static>> = all[start..end].to_vec();
        f.render_widget(Paragraph::new(visible), inner);
    }

    if let Some(panel_area) = changes_area {
        let rows = collect_change_rows(&entries);
        *last_change_count = rows.len();
        // Resolve "follow tail" (None) and clamp explicit indices that
        // outran the current row count (e.g. transcript was reloaded).
        let resolved = if rows.is_empty() {
            0
        } else {
            match *changes_selection {
                None => rows.len() - 1,
                Some(i) if i >= rows.len() => rows.len() - 1,
                Some(i) => i,
            }
        };

        let panel_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(panel_area);
        *actions_rect = Some(panel_rows[0]);
        *detail_rect = Some(panel_rows[1]);
        render_changes_list(f, panel_rows[0], &rows, resolved, &s.cwd);
        render_changes_detail(f, panel_rows[1], &rows, resolved, detail_scroll, &s.cwd);
    } else {
        *last_change_count = 0;
    }
}

fn render_changes_list(
    f: &mut Frame,
    area: Rect,
    rows: &[ChangeRow],
    selection: usize,
    cwd: &Path,
) {
    let title = if rows.is_empty() {
        "Actions".to_string()
    } else {
        format!("Actions ({}/{})", selection + 1, rows.len())
    };
    let hint = Line::from(vec![
        Span::raw(" "),
        Span::styled("j/k", Style::default().fg(Color::Yellow)),
        Span::styled(" move ", Style::default().fg(Color::DarkGray)),
        Span::styled("g/G", Style::default().fg(Color::Yellow)),
        Span::styled(" top/tail ", Style::default().fg(Color::DarkGray)),
    ])
    .alignment(Alignment::Right);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title(hint);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if rows.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no tool activity yet",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(hint, inner);
        return;
    }

    let width = inner.width as usize;
    let target_h = inner.height as usize;

    // Middle-anchor the cursor so the highlighted row visibly moves
    // when j/k is pressed (instead of sticking to the bottom edge).
    // Near the top/bottom of the list the window pins naturally via
    // the clamps.
    let max_start = rows.len().saturating_sub(target_h);
    let half = target_h / 2;
    let start = selection.saturating_sub(half).min(max_start);
    let end = (start + target_h).min(rows.len());
    let visible = &rows[start..end];

    let lines: Vec<Line<'static>> = visible
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let abs_idx = start + i;
            let selected = abs_idx == selection;
            let (glyph, glyph_style) = match r.ok {
                Some(true) => ("✓", Style::default().fg(Color::Green)),
                Some(false) => ("✗", Style::default().fg(Color::Red)),
                None => ("·", Style::default().fg(Color::DarkGray)),
            };
            let name_style = tool_name_style(&r.name);
            let summary = shorten_summary_paths(
                &chat_log::tool_input_summary(&r.name, Some(&r.input)),
                cwd,
            );
            let prefix_w = 2 + r.name.chars().count() + 1;
            let body_w = width.saturating_sub(prefix_w).max(4);
            let summary = truncate_chars(&summary, body_w);
            let mut spans = vec![
                Span::styled(format!("{glyph} "), glyph_style),
                Span::styled(r.name.clone(), name_style),
                Span::raw(" "),
            ];
            if r.name.eq_ignore_ascii_case("bash") {
                // Highlight executable(s) and shell separators in the
                // bash one-liner so commands like `grep ...` or
                // `find ... | xargs ...` are scannable at a glance.
                let normal = Style::default().fg(Color::Gray);
                let exec_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
                let sep_style = Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD);
                spans.extend(bash_spans(&summary, exec_style, sep_style, normal));
            } else {
                spans.push(Span::styled(summary, Style::default().fg(Color::Gray)));
            }
            if selected {
                for s in &mut spans {
                    s.style = s.style.add_modifier(Modifier::REVERSED);
                }
            }
            Line::from(spans)
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
}

fn render_changes_detail(
    f: &mut Frame,
    area: Rect,
    rows: &[ChangeRow],
    selection: usize,
    detail_scroll: &mut usize,
    cwd: &Path,
) {
    let Some(row) = rows.get(selection) else {
        let block = Block::default().borders(Borders::ALL).title("Detail");
        f.render_widget(block, area);
        return;
    };
    let status = match row.ok {
        Some(true) => " ✓",
        Some(false) => " ✗",
        None => " ·",
    };
    // Title gets filled in once we know the clamped scroll position.
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    let width = inner_w;
    let mut lines = format_tool_detail(&row.name, &row.input, width, cwd);

    // Append the actual command/tool output below the input. For a
    // Bash row this is stdout/stderr; for Read it's the file content
    // claude saw; for Edit it's the success / error message.
    let separator_style = Style::default().fg(Color::DarkGray);
    let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let body_style = match row.ok {
        Some(false) => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::Gray),
    };

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "─".repeat(width.max(1)),
        separator_style,
    )));
    let header = match row.ok {
        Some(true) => "output",
        Some(false) => "error",
        None => "output (pending…)",
    };
    lines.push(Line::from(Span::styled(header, header_style)));
    lines.push(Line::raw(""));

    match &row.result {
        Some(text) if !text.is_empty() => {
            push_plain(&mut lines, text, width, body_style);
        }
        Some(_) => {
            lines.push(Line::from(Span::styled(
                "(empty)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        None => {
            lines.push(Line::from(Span::styled(
                "(no result yet)",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let target_h = inner_h;
    let total = lines.len();
    let max_scroll = total.saturating_sub(target_h);
    if *detail_scroll > max_scroll {
        *detail_scroll = max_scroll;
    }
    let start = *detail_scroll;
    let end = (start + target_h).min(total);

    let title = if max_scroll > 0 {
        format!(
            "Detail · {}{}  [scroll {}/{}]",
            row.name, status, start, max_scroll
        )
    } else {
        format!("Detail · {}{}", row.name, status)
    };
    let hint = Line::from(vec![
        Span::raw(" "),
        Span::styled("J/K", Style::default().fg(Color::Yellow)),
        Span::styled(" scroll ", Style::default().fg(Color::DarkGray)),
    ])
    .alignment(Alignment::Right);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title(hint);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let visible: Vec<Line<'static>> = lines.drain(start..end).collect();
    f.render_widget(Paragraph::new(visible), inner);
}

fn format_tool_detail(
    name: &str,
    input: &Value,
    width: usize,
    cwd: &Path,
) -> Vec<Line<'static>> {
    let lower = name.to_ascii_lowercase();
    let body_w = width.max(10);
    let mut out: Vec<Line<'static>> = Vec::new();
    let str_field = |v: &Value, key: &str| -> Option<String> {
        v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
    };
    let path_field = |v: &Value, key: &str| -> Option<String> {
        str_field(v, key).map(|p| shorten_path(&p, cwd))
    };

    match lower.as_str() {
        "bash" => {
            if let Some(desc) = str_field(input, "description") {
                for w in wrap_text(&desc, body_w) {
                    out.push(Line::from(Span::styled(
                        w,
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                    )));
                }
                out.push(Line::raw(""));
            }
            if let Some(cmd) = str_field(input, "command") {
                push_bash_command(&mut out, &cmd, body_w);
            } else {
                push_pretty_json(&mut out, input, body_w);
            }
        }
        "edit" => {
            if let Some(fp) = path_field(input, "file_path") {
                out.push(file_header(&fp));
                out.push(Line::raw(""));
            }
            let old = str_field(input, "old_string").unwrap_or_default();
            let new = str_field(input, "new_string").unwrap_or_default();
            push_diff(&mut out, &old, &new, body_w);
        }
        "multiedit" => {
            if let Some(fp) = path_field(input, "file_path") {
                out.push(file_header(&fp));
            }
            if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
                for (i, e) in edits.iter().enumerate() {
                    out.push(Line::raw(""));
                    out.push(Line::from(Span::styled(
                        format!("@@ edit {} @@", i + 1),
                        Style::default().fg(Color::DarkGray),
                    )));
                    let old = str_field(e, "old_string").unwrap_or_default();
                    let new = str_field(e, "new_string").unwrap_or_default();
                    push_diff(&mut out, &old, &new, body_w);
                }
            }
        }
        "write" => {
            if let Some(fp) = path_field(input, "file_path") {
                out.push(file_header(&fp));
                out.push(Line::raw(""));
            }
            let content = str_field(input, "content").unwrap_or_default();
            push_plain(&mut out, &content, body_w, Style::default().fg(Color::White));
        }
        "notebookedit" => {
            for key in ["notebook_path", "cell_id", "edit_mode", "cell_type"] {
                let val = if key == "notebook_path" {
                    path_field(input, key)
                } else {
                    str_field(input, key)
                };
                if let Some(v) = val {
                    out.push(Line::from(vec![
                        Span::styled(
                            format!("{key}: "),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(v, Style::default().fg(Color::Cyan)),
                    ]));
                }
            }
            if let Some(src) = str_field(input, "new_source") {
                out.push(Line::raw(""));
                push_plain(&mut out, &src, body_w, Style::default().fg(Color::White));
            }
        }
        _ => {
            push_pretty_json(&mut out, input, body_w);
        }
    }
    out
}

/// If `p` is an absolute path inside `cwd`, return the path relative
/// to `cwd`; otherwise return `p` unchanged. Returns the input as-is
/// for non-absolute paths so we don't mangle things like `node_modules`
/// or bare filenames coming back from tools.
fn shorten_path(p: &str, cwd: &Path) -> String {
    let path = Path::new(p);
    if !path.is_absolute() {
        return p.to_string();
    }
    match path.strip_prefix(cwd) {
        Ok(rel) => {
            let s = rel.display().to_string();
            if s.is_empty() { ".".to_string() } else { s }
        }
        Err(_) => p.to_string(),
    }
}

/// `tool_input_summary` returns a single field's value; when that field
/// is a file path (Edit/Write/etc.), shorten it relative to the
/// session's cwd. Detected by leading `/`.
fn shorten_summary_paths(summary: &str, cwd: &Path) -> String {
    if summary.starts_with('/') {
        shorten_path(summary, cwd)
    } else {
        summary.to_string()
    }
}

fn file_header(path: &str) -> Line<'static> {
    Line::from(Span::styled(
        path.to_string(),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))
}

fn push_plain(out: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for src in text.split('\n') {
        if src.is_empty() {
            out.push(Line::raw(""));
            continue;
        }
        let wrapped = wrap_text(src, width);
        if wrapped.is_empty() {
            out.push(Line::raw(""));
        }
        for w in wrapped {
            out.push(Line::from(Span::styled(w, style)));
        }
    }
}

/// Render a bash `command` field with the executable name(s)
/// highlighted. Each `\n`-separated source line and each pipeline /
/// list segment (split by `|`, `&&`, `||`, `;`) gets its first
/// non-whitespace word colored bold cyan; the separator itself is
/// rendered in magenta. Wrapping is honored on the first wrapped
/// sub-line only — continuation lines stay plain so the highlight
/// always reflects the actual start of a command.
fn push_bash_command(out: &mut Vec<Line<'static>>, cmd: &str, width: usize) {
    let exec_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let sep_style = Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::White);

    for raw in cmd.split('\n') {
        if raw.is_empty() {
            out.push(Line::raw(""));
            continue;
        }
        let wrapped = wrap_text(raw, width);
        if wrapped.is_empty() {
            out.push(Line::raw(""));
            continue;
        }
        // First wrapped sub-line: scan for executable tokens and
        // shell separators. Subsequent sub-lines render plain so the
        // highlight stays anchored to the real start of commands.
        out.push(Line::from(bash_spans(&wrapped[0], exec_style, sep_style, normal)));
        for cont in &wrapped[1..] {
            out.push(Line::from(Span::styled(cont.clone(), normal)));
        }
    }
}

fn bash_spans(
    line: &str,
    exec_style: Style,
    sep_style: Style,
    normal: Style,
) -> Vec<Span<'static>> {
    const SEPS: &[&str] = &["&&", "||", "|", ";"];
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = line;
    let mut expect_exec = true;

    while !rest.is_empty() {
        // Consume any leading whitespace into a normal span so the
        // executable still gets its color.
        let trimmed = rest.trim_start();
        if trimmed.len() < rest.len() {
            let ws_len = rest.len() - trimmed.len();
            spans.push(Span::styled(rest[..ws_len].to_string(), normal));
            rest = &rest[ws_len..];
            if rest.is_empty() {
                break;
            }
        }
        // Match a separator at the current position.
        let mut matched_sep: Option<&str> = None;
        for s in SEPS {
            if rest.starts_with(s) {
                matched_sep = Some(s);
                break;
            }
        }
        if let Some(s) = matched_sep {
            spans.push(Span::styled(s.to_string(), sep_style));
            rest = &rest[s.len()..];
            expect_exec = true;
            continue;
        }
        // Take the next whitespace-or-separator-delimited token.
        let mut tok_end = rest.len();
        for (i, ch) in rest.char_indices() {
            if ch.is_whitespace() {
                tok_end = i;
                break;
            }
            // Don't split mid-token if a separator starts here — but
            // *do* end the token if a separator begins immediately.
            if i > 0 && SEPS.iter().any(|s| rest[i..].starts_with(s)) {
                tok_end = i;
                break;
            }
        }
        let token = &rest[..tok_end];
        let style = if expect_exec { exec_style } else { normal };
        spans.push(Span::styled(token.to_string(), style));
        rest = &rest[tok_end..];
        expect_exec = false;
    }
    spans
}

fn push_diff(out: &mut Vec<Line<'static>>, old: &str, new: &str, width: usize) {
    let minus = Style::default().fg(Color::Red);
    let plus = Style::default().fg(Color::Green);
    let body_w = width.saturating_sub(2).max(8);
    for src in old.split('\n') {
        for w in wrap_or_empty(src, body_w) {
            out.push(Line::from(vec![
                Span::styled("- ", minus),
                Span::styled(w, minus),
            ]));
        }
    }
    for src in new.split('\n') {
        for w in wrap_or_empty(src, body_w) {
            out.push(Line::from(vec![
                Span::styled("+ ", plus),
                Span::styled(w, plus),
            ]));
        }
    }
}

fn wrap_or_empty(s: &str, width: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    let w = wrap_text(s, width);
    if w.is_empty() { vec![String::new()] } else { w }
}

fn push_pretty_json(out: &mut Vec<Line<'static>>, v: &Value, width: usize) {
    let pretty = serde_json::to_string_pretty(v).unwrap_or_default();
    let style = Style::default().fg(Color::Gray);
    for src in pretty.split('\n') {
        for w in wrap_or_empty(src, width) {
            out.push(Line::from(Span::styled(w, style)));
        }
    }
}

fn tool_name_style(name: &str) -> Style {
    let lower = name.to_ascii_lowercase();
    let color = match lower.as_str() {
        "edit" | "write" | "multiedit" | "notebookedit" => Color::Yellow,
        "bash" => Color::Magenta,
        "read" | "grep" | "glob" => Color::Cyan,
        _ => Color::Gray,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(1);
    let mut out: String = chars.into_iter().take(take).collect();
    out.push('…');
    out
}

fn entry_to_lines(e: &ChatEntry, body_w: usize, _cwd: &Path) -> Vec<Line<'static>> {
    match &e.kind {
        EntryKind::User => build_lines_md(
            "you      ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            markdown::render(
                &e.text,
                body_w,
                Style::default().fg(Color::White),
            ),
        ),
        EntryKind::Assistant => build_lines_md(
            "claude   ",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            markdown::render(
                &e.text,
                body_w,
                Style::default().fg(Color::White),
            ),
        ),
        EntryKind::Thinking => build_lines_md(
            "thinking ",
            Style::default().fg(Color::Magenta),
            markdown::render(
                &e.text,
                body_w,
                Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC),
            ),
        ),
        EntryKind::ToolUse { name, .. } => {
            // Heavy dim — readable marginalia next to user/assistant
            // turns. Full input/diff/output lives in the Actions pane.
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM);
            vec![Line::from(vec![
                Span::styled("· ".to_string(), style),
                Span::styled(name.clone(), style),
            ])]
        }
        EntryKind::ToolResult { ok } => {
            let (text, color) = if *ok {
                ("↳ ok", Color::DarkGray)
            } else {
                ("↳ error", Color::Red)
            };
            let style = Style::default().fg(color).add_modifier(Modifier::DIM);
            vec![Line::from(vec![
                Span::raw("  "),
                Span::styled(text.to_string(), style),
            ])]
        }
        EntryKind::System => build_lines(
            "system   ",
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::DarkGray),
            &e.text,
            body_w,
        ),
    }
}

/// Like `build_lines`, but the body is a pre-rendered markdown block
/// (vec of styled lines). The first line gets the label prefix; the
/// rest are left-padded by `LABEL_W` spaces to align with it.
fn build_lines_md(
    label: &str,
    label_style: Style,
    md_lines: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    if md_lines.is_empty() {
        return vec![Line::from(Span::styled(label.to_string(), label_style))];
    }
    let indent: String = " ".repeat(LABEL_W);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(md_lines.len());
    let mut iter = md_lines.into_iter();
    let first = iter.next().unwrap();
    let mut first_spans = vec![Span::styled(label.to_string(), label_style)];
    first_spans.extend(first.spans);
    out.push(Line::from(first_spans));
    for line in iter {
        let mut spans = vec![Span::raw(indent.clone())];
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    out
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

fn fmt_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

const MASCOT_SMALL: &str = include_str!("../../images/mewxi-small.ascii");
const MASCOT_TINY: &str = include_str!("../../images/mewxi-tiny.ascii");

/// Pick the largest mewxi mascot whose dimensions fit inside the given
/// area together with `caption_h` rows reserved below it. Returns the
/// raw mascot text plus its (height, width) — empty string when nothing
/// fits.
fn pick_mascot(area_w: u16, area_h: u16, caption_h: u16) -> (&'static str, u16, u16) {
    let avail_h = area_h.saturating_sub(caption_h);
    let candidates: [(&str, u16, u16); 2] = [
        (MASCOT_SMALL, 22, 44),
        (MASCOT_TINY, 16, 32),
    ];
    for (art, h, w) in candidates {
        if h <= avail_h && w <= area_w {
            return (art, h, w);
        }
    }
    ("", 0, 0)
}

#[cfg(test)]
mod tests {
    use super::models_match;

    #[test]
    fn slug_matches_full_name() {
        assert!(models_match("haiku", "claude-haiku-4-5"));
        assert!(models_match("sonnet", "claude-sonnet-4-6"));
        assert!(models_match("opus", "claude-opus-4-7"));
    }

    #[test]
    fn full_name_matches_full_name() {
        assert!(models_match("claude-haiku-4-5", "claude-haiku-4-5"));
    }

    #[test]
    fn different_models_do_not_match() {
        assert!(!models_match("haiku", "claude-sonnet-4-6"));
        assert!(!models_match("sonnet", "claude-haiku-4-5"));
    }

    #[test]
    fn case_insensitive() {
        assert!(models_match("Haiku", "claude-haiku-4-5"));
        assert!(models_match("haiku", "CLAUDE-HAIKU-4-5"));
    }
}
