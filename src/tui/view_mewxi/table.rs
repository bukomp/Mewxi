//! View 5 sessions table — full feature parity with view 1's
//! [`super::super::view_all`] table (grouping, sub-agent rows, responsive
//! columns, scrolling, off-screen counter), restyled onto the rave
//! purple/pink palette.
//!
//! Adapted from `render_sessions_table` and its helpers in `view_all.rs`
//! (kept byte-for-byte in behavior; only `Style`/`Color` choices differ).

use super::palette::{activity_color, P_DIM, P_HIGH, P_HOT, P_LABEL, P_PINK, P_TEXT};
use crate::live_session::{Activity, SessionState};
use crate::tui::widgets::{currency_symbol, fmt_tokens_compact};
use crate::tui::SessionRef;
use chrono::Utc;
use ratatui::layout::{Constraint, Flex, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

/// Render the parity sessions table into `area`. Same semantics as
/// view_all's table: `sessions` is the flattened grouped slice (incl.
/// sub-agents), `selected` indexes into it. Restyled to the rave palette.
/// Guards tiny areas (never panic).
pub fn render(
    f: &mut Frame,
    area: Rect,
    sessions: &[&SessionRef],
    selected: Option<usize>,
    table_state: &mut TableState,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Sub-agent rows are excluded from the session tallies — the title
    // and group headers keep counting sessions, exactly as before they
    // became selectable rows.
    let parent_count = sessions.iter().filter(|s| s.subagent.is_none()).count();
    let active_count = sessions
        .iter()
        .filter(|s| s.subagent.is_none() && s.state == SessionState::Active)
        .count();
    let idle_count = parent_count - active_count;
    let title = if idle_count == 0 {
        format!("Sessions — {active_count} active")
    } else {
        format!("Sessions — {active_count} active · {idle_count} idle")
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(P_HIGH))
        .title(Span::styled(title, Style::default().fg(P_HIGH).add_modifier(Modifier::BOLD)));

    if sessions.is_empty() {
        *table_state = TableState::default();
        let inner = block.inner(area);
        f.render_widget(block, area);
        let p = Paragraph::new(Line::from(Span::styled(
            "no sessions touched in the last 2h — start a session or press `r` to rescan",
            Style::default().fg(P_DIM),
        )));
        f.render_widget(p, inner);
        return;
    }

    // Responsive columns — added in priority order as the screen widens.
    // Mirrors view_all's thresholds exactly.
    let w = area.width;
    let show_ctx = w >= 72;
    let show_status = w >= 84;
    let show_limits = w >= 98;
    let show_io = w >= 112;
    let show_cache = w >= 121;
    // Price is data-gated rather than width-gated: show it only once at
    // least one row (session or sub-agent) carries a real price.
    let show_price = sessions.iter().any(|s| s.price > 0.005);

    // `sessions` is already grouped by project (alphabetical), with pid
    // ascending within each group — sort lives upstream so selection
    // indexes match visible row order.
    let ordered: &[&SessionRef] = sessions;

    let now = Utc::now();
    // Compute column count up front so header rows pad correctly.
    // Base: account, age, tokens, model, state = 5.
    let mut col_count = 5;
    if show_status {
        col_count += 1;
    }
    if show_ctx {
        col_count += 1;
    }
    if show_io {
        col_count += 1;
    }
    if show_cache {
        col_count += 1;
    }
    if show_price {
        col_count += 1;
    }
    if show_limits {
        col_count += 2;
    }

    // Pad project names to the widest one so the "x/y active" count
    // lines up vertically across all group headers regardless of name
    // length or digit count.
    let max_project_len = ordered
        .iter()
        .map(|s| {
            if s.project.is_empty() {
                "(unknown)".len()
            } else {
                s.project.chars().count()
            }
        })
        .max()
        .unwrap_or(0);

    let mut rows: Vec<Row> = Vec::with_capacity(ordered.len() + 8);
    // Visual row index of the selected session, plus the row scrolling up
    // should stop at — the group's blank+header rows when the cursor
    // sits on the first session of a group, so the "▾ project" line
    // scrolls into view together with the selection.
    let mut selected_row: Option<usize> = None;
    let mut selected_anchor: Option<usize> = None;
    // Visual row index of every session row (blank/header rows excluded)
    // so the off-screen counter counts agents, not table rows.
    let mut session_rows: Vec<usize> = Vec::with_capacity(ordered.len());
    // Project-header rows carry no column data, so they're repainted as
    // one full-width line after the table renders (see overlay below).
    let mut project_headers: Vec<(usize, Line)> = Vec::new();
    let mut group_start = 0usize;
    while group_start < ordered.len() {
        let project = &ordered[group_start].project;
        let mut group_end = group_start + 1;
        while group_end < ordered.len() && ordered[group_end].project == *project {
            group_end += 1;
        }
        let group = &ordered[group_start..group_end];
        let active_in_group = group
            .iter()
            .filter(|s| s.subagent.is_none() && s.state == SessionState::Active)
            .count();
        let total_in_group = group.iter().filter(|s| s.subagent.is_none()).count();
        let label = if project.is_empty() { "(unknown)" } else { project.as_str() };
        let label_pad = max_project_len.saturating_sub(label.chars().count());
        let count_text = format!("{active_in_group}/{total_in_group} active");
        let any_active = active_in_group > 0;
        let project_color = if any_active { P_HIGH } else { P_DIM };

        // Blank spacer row before every group.
        let mut blank: Vec<Cell> = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            blank.push(Cell::from(""));
        }
        rows.push(Row::new(blank));

        let header_line = Line::from(vec![
            Span::styled(
                format!("▾ {label}"),
                Style::default().fg(project_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(label_pad + 2)),
            Span::styled(count_text, Style::default().fg(P_DIM)),
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
            // Sub-agent child row.
            if s.subagent.is_some() {
                session_rows.push(rows.len());
                rows.push(subagent_row(
                    s, now, is_selected, show_status, show_ctx, show_io, show_cache, show_price,
                    show_limits,
                ));
                continue;
            }
            // A session mewxi just asked to close.
            if s.killing {
                let dash = || Cell::from(Span::styled("—", Style::default().fg(P_DIM)));
                let mut cells: Vec<Cell> = vec![Cell::from(format!("  {arrow}—")), dash()];
                if show_status {
                    cells.push(Cell::from(Span::styled(
                        "killing",
                        Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
                    )));
                }
                if show_ctx {
                    cells.push(dash());
                }
                cells.push(dash()); // tokens
                if show_io {
                    cells.push(dash());
                }
                if show_cache {
                    cells.push(dash());
                }
                if show_price {
                    cells.push(dash());
                }
                if show_limits {
                    cells.push(dash()); // 5h%
                    cells.push(dash()); // wk%
                }
                cells.push(dash()); // model
                cells.push(dash()); // state
                let style = if is_selected {
                    Style::default().fg(P_HOT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(P_DIM)
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
                SessionState::Active => P_HIGH,
                SessionState::Idle => P_DIM,
            };
            let base_style = if is_selected {
                Style::default().fg(P_HOT).add_modifier(Modifier::BOLD)
            } else if s.state == SessionState::Idle {
                Style::default().fg(P_DIM)
            } else {
                Style::default().fg(P_TEXT)
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
            if show_price {
                let sym = currency_symbol(s.price_currency.as_deref());
                if s.price > 0.005 {
                    let text = format!("~{sym}{:.2}", s.price);
                    cells.push(if is_selected {
                        Cell::from(text)
                    } else {
                        Cell::from(Span::styled(text, Style::default().fg(P_HIGH)))
                    });
                } else {
                    let text = format!("{sym}0.00");
                    cells.push(if is_selected {
                        Cell::from(text)
                    } else {
                        Cell::from(Span::styled(text, Style::default().fg(P_DIM)))
                    });
                }
            }
            if show_limits {
                let limit_cell = |v: Option<f64>| -> Cell<'static> {
                    match v {
                        None => Cell::from(Span::styled("—", Style::default().fg(P_DIM))),
                        Some(v) => {
                            let text = if v >= 9.95 { format!("{v:.0}%") } else { format!("{v:.1}%") };
                            if v < 0.05 {
                                Cell::from(Span::styled(text, Style::default().fg(P_DIM)))
                            } else {
                                Cell::from(text)
                            }
                        }
                    }
                };
                cells.push(limit_cell(s.limit_5h_pct));
                cells.push(limit_cell(s.limit_wk_pct));
            }
            // Model + thinking budget, e.g. `opus:xhigh` / `sonnet-4-6[1M]:medium`.
            let model_label = crate::tui::view_session::trim_model(&s.model).into_owned();
            let model_cell = match s.effort.as_deref() {
                Some(eff) => Cell::from(Line::from(vec![
                    Span::raw(model_label),
                    Span::styled(
                        format!(":{eff}"),
                        Style::default().fg(crate::tui::view_session::effort_color(eff)),
                    ),
                ])),
                None => Cell::from(model_label),
            };
            cells.push(model_cell);
            cells.push(Cell::from(Span::styled(state_label, Style::default().fg(state_color))));
            session_rows.push(rows.len());
            rows.push(Row::new(cells).style(base_style));
        }

        group_start = group_end;
    }

    // Account names are indented by 4 chars under project headers ("  ▶ "
    // or "    "), so the column label needs the same lead-in to line up.
    let mut header_labels: Vec<&'static str> = vec!["    account", "age"];
    let mut constraints: Vec<Constraint> = vec![Constraint::Min(14), Constraint::Length(6)];
    if show_status {
        header_labels.push("status");
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
        constraints.push(Constraint::Length(11));
    }
    if show_cache {
        header_labels.push("cache");
        constraints.push(Constraint::Length(7));
    }
    if show_price {
        header_labels.push("price");
        constraints.push(Constraint::Length(9));
    }
    if show_limits {
        header_labels.push("5h%");
        constraints.push(Constraint::Length(6));
        header_labels.push("wk%");
        constraints.push(Constraint::Length(6));
    }
    header_labels.push("model");
    constraints.push(Constraint::Length(21));
    header_labels.push("state");
    constraints.push(Constraint::Length(7));

    let row_count = rows.len();

    // Stateful render so a selection past the visible window scrolls the
    // table. The anchor pulls the offset back up to the group header
    // when scrolling upward.
    table_state.select(selected_row);
    if let Some(anchor) = selected_anchor {
        if table_state.offset() > anchor {
            *table_state.offset_mut() = anchor;
        }
    }
    if table_state.offset() >= row_count {
        *table_state.offset_mut() = row_count.saturating_sub(1);
    }

    // Off-screen agent counter on the bottom border. Replicate ratatui's
    // offset adjustment (all rows are height 1) to know which window
    // will actually be drawn.
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
        block = block.title(Line::from(Span::styled(
            format!("· {} off-screen", parts.join(" · ")),
            Style::default().fg(P_DIM),
        )));
    }

    let table = Table::new(rows, constraints)
        .header(
            Row::new(header_labels)
                .style(Style::default().fg(P_LABEL).add_modifier(Modifier::BOLD)),
        )
        // ratatui defaults Table to Flex::Start, so Constraint::Min no
        // longer absorbs leftover width — Legacy flex restores the "fill
        // remaining space" behavior.
        .flex(Flex::Legacy)
        .block(block);

    f.render_stateful_widget(table, area, table_state);

    // Repaint each visible project header as one full-width line — the
    // table clips it to the account column since header rows have no
    // column data.
    let inner_x = area.x + 1;
    let inner_y = area.y + 1;
    let inner_w = area.width.saturating_sub(2);
    let buf = f.buffer_mut();
    for (hr, line) in &project_headers {
        if *hr < final_offset || *hr >= final_offset + viewport {
            continue;
        }
        let y = inner_y + 1 + (*hr - final_offset) as u16;
        if y >= area.y + area.height {
            continue;
        }
        buf.set_line(inner_x, y, line, inner_w);
    }
}

/// One indented child row beneath a session for a sub-agent it is
/// currently running. Adapted from view_all's `subagent_row` — same
/// precedence rules for the live caption (status_label → narration →
/// description), same truncation and depth-based indent, restyled onto
/// the rave palette.
fn subagent_row(
    s: &SessionRef,
    now: chrono::DateTime<chrono::Utc>,
    is_selected: bool,
    show_status: bool,
    show_ctx: bool,
    show_io: bool,
    show_cache: bool,
    show_price: bool,
    show_limits: bool,
) -> Row<'static> {
    let (dim, text) = if is_selected {
        let sel = Style::default().fg(P_HOT).add_modifier(Modifier::BOLD);
        (sel, sel)
    } else {
        (Style::default().fg(P_DIM), Style::default().fg(P_TEXT))
    };
    // Indent two further than the session's "  ▶ name" so the tree
    // nests, then two more per level of nesting beyond depth 1.
    let depth = s.subagent.as_ref().map(|t| t.depth).unwrap_or(1).max(1);
    let indent = "  ".repeat((depth - 1) as usize);
    let lead = if is_selected {
        format!("{indent}    ▶ ↳ ")
    } else {
        format!("{indent}      ↳ ")
    };
    let lead_style = if is_selected { text } else { Style::default().fg(P_HIGH) };
    let mut label = vec![Span::styled(lead, lead_style)];
    let tag = s.subagent.as_ref();
    if let Some(name) = tag.and_then(|t| t.workflow.as_ref()) {
        label.push(Span::styled(
            format!("⚙ {name} › "),
            Style::default().fg(P_PINK),
        ));
    }
    let agent_type = tag.and_then(|t| t.agent_type.clone());
    let action = tag.and_then(|t| t.current_action.clone());
    // Precedence is fidelity order: Claude Code's own panel label beats
    // the agent's own narration, which beats the static launch
    // description — each is a fallback for the one before it going
    // stale or never having arrived.
    let description = tag
        .and_then(|t| t.status_label.clone())
        .or_else(|| tag.and_then(|t| t.narration.clone()))
        .or_else(|| tag.map(|t| t.description.clone()))
        .unwrap_or_default();
    // A live action competes with the caption for row width, so shrink
    // the caption to make room; with no action it's shown in full.
    let description = if action.is_some() {
        let mut truncated: String = description.chars().take(32).collect();
        if description.chars().count() > 32 {
            truncated.push('…');
        }
        truncated
    } else {
        description
    };
    match agent_type {
        Some(t) => {
            label.push(Span::styled(
                t,
                Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
            ));
            label.push(Span::styled(format!(" · {description}"), text));
        }
        None => label.push(Span::styled(description, text)),
    }
    // Appended after the match so it lands in both branches: the live
    // caption of whatever tool call the agent is currently on.
    if let Some(action) = action {
        let action_style = if is_selected { text } else { Style::default().fg(P_HIGH) };
        label.push(Span::styled(format!(" — {action}"), action_style));
    }
    let age = (now - s.last_activity).num_seconds().max(0);
    let mut cells: Vec<Cell> = vec![
        Cell::from(Line::from(label)),
        Cell::from(Span::styled(fmt_age(age), dim)),
    ];
    if show_status {
        let (lbl, color) = activity_display(&s.activity);
        cells.push(Cell::from(Span::styled(lbl, Style::default().fg(color))));
    }
    if show_ctx {
        cells.push(Cell::from(Span::styled(
            fmt_ctx(s.current_context, s.context_cap),
            text,
        )));
    }
    cells.push(Cell::from(Span::styled(fmt_tokens_compact(s.tokens), text)));
    if show_io {
        cells.push(Cell::from(Span::styled(
            format!(
                "{}/{}",
                fmt_tokens_compact(s.totals.input),
                fmt_tokens_compact(s.totals.output),
            ),
            text,
        )));
    }
    if show_cache {
        cells.push(Cell::from(Span::styled(
            fmt_tokens_compact(s.totals.cache_read),
            text,
        )));
    }
    if show_price {
        let sym = currency_symbol(s.price_currency.as_deref());
        if s.price > 0.005 {
            cells.push(Cell::from(Span::styled(format!("~{sym}{:.2}", s.price), text)));
        } else {
            cells.push(Cell::from(Span::styled(format!("{sym}0.00"), dim)));
        }
    }
    if show_limits {
        cells.push(Cell::from(Span::styled("—", dim)));
        cells.push(Cell::from(Span::styled("—", dim)));
    }
    let model_label = crate::tui::view_session::trim_model(&s.model).into_owned();
    let model_cell = match s.effort.as_deref() {
        Some(eff) => Cell::from(Line::from(vec![
            Span::styled(model_label, text),
            Span::styled(
                format!(":{eff}"),
                Style::default().fg(crate::tui::view_session::effort_color(eff)),
            ),
        ])),
        None => Cell::from(Span::styled(model_label, text)),
    };
    cells.push(model_cell);
    cells.push(Cell::from(Span::styled("active", Style::default().fg(P_HIGH))));
    Row::new(cells)
}

fn activity_display(a: &Activity) -> (String, Color) {
    (a.label(), activity_color(a))
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

/// Age column is 6 chars wide. Always include the next-finer unit so the
/// value ticks visibly every second/minute rather than freezing between
/// rollovers. Max widths: "59m59s", "23h59m", "99d23h" — all exactly 6
/// chars.
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
    use super::*;

    #[test]
    fn fmt_age_boundaries() {
        assert_eq!(fmt_age(59), "59s");
        assert_eq!(fmt_age(60), "1m0s");
        assert_eq!(fmt_age(3600), "1h0m");
        assert_eq!(fmt_age(86400), "1d0h");
    }

    #[test]
    fn fmt_ctx_variants() {
        assert_eq!(fmt_ctx(Some(50), Some(100)), "50%");
        assert_eq!(fmt_ctx(None, Some(100)), "—");
        assert_eq!(fmt_ctx(Some(50), None), "—");
        assert_eq!(fmt_ctx(Some(50), Some(0)), "—");
    }
}
