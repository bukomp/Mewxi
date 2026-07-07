//! View 2 — selected session detail.
//!
//! Top band: the parent account's three gauges. Middle: session token
//! breakdown. Then a formatted chat log read from the session's JSONL
//! transcript. Footer: keybinding hints.

use super::markdown;
use super::widgets::{self, fmt_tokens_compact};
use super::{PerAccount, SessionRef};
use crate::chat_log::{self, ChatEntry, EntryKind, Task, TaskStatus};
use serde_json::Value;
use std::path::{Path, PathBuf};
use crate::live_session::{Activity, SessionState};
use crate::stats::fmt_num;
use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// Mouse-drag text selection within the chat log. Coordinates are
/// terminal-screen cells (the same frame crossterm's mouse events use)
/// so highlight and copy share one frame of reference. `anchor` is
/// where the drag started; `cursor` follows the mouse. Either ordering
/// is allowed — [`rect`] normalizes to top-left → bottom-right.
#[derive(Clone, Copy, Debug)]
pub struct ChatSelection {
    pub anchor: (u16, u16),
    pub cursor: (u16, u16),
}

impl ChatSelection {
    /// Returns `(col_start, row_start, col_end_exclusive, row_end)`
    /// with start <= end on both axes. The end column is exclusive so
    /// a zero-length selection (anchor == cursor) yields an empty
    /// range and won't render a stray highlight.
    pub fn rect(&self) -> (u16, u16, u16, u16) {
        let (a_col, a_row) = self.anchor;
        let (c_col, c_row) = self.cursor;
        let (col_start, col_end) = if a_col <= c_col {
            (a_col, c_col + 1)
        } else {
            (c_col, a_col + 1)
        };
        let (row_start, row_end) = if a_row <= c_row {
            (a_row, c_row)
        } else {
            (c_row, a_row)
        };
        (col_start, row_start, col_end, row_end)
    }
}

/// A code block currently visible in the chat log, projected onto
/// terminal-screen rows so the run loop can map a mouse click to it.
/// `top`/`bottom` are inclusive screen rows (the same frame crossterm's
/// mouse events use); `source` is the verbatim, untruncated code to drop
/// on the clipboard as one chunk. Only blocks with at least one on-screen
/// row are emitted.
#[derive(Clone, Debug)]
pub struct CodeBlockRegion {
    pub top: u16,
    pub bottom: u16,
    pub source: String,
}

/// A click-to-copy span in the right-hand Detail pane, projected onto
/// terminal-screen rows so the run loop can map a click to it. Today
/// these are the individual parts of a Bash command (each pipeline /
/// list segment split at `&&`, `||`, `|`, `;`): clicking one drops just
/// that part's runnable text on the clipboard. `top`/`bottom` are
/// inclusive screen rows (the same frame crossterm's mouse events use);
/// `source` is the verbatim, unwrapped command part. Only parts with at
/// least one on-screen row are emitted.
#[derive(Clone, Debug)]
pub struct DetailCopyRegion {
    pub top: u16,
    pub bottom: u16,
    pub source: String,
}

/// Driver pane state passed in from [`super::run_loop`] when the
/// currently-pinned session is one mewxi spawned and owns the PTY for.
/// `None` means the session is just being observed; the input row is
/// not rendered in that case.
pub struct DriverPane<'a> {
    /// What the user has typed but not yet submitted.
    pub input: &'a str,
    /// Byte index of the edit cursor within `input`. Always sits on a
    /// char boundary; equals `input.len()` when the cursor is past the
    /// last character.
    pub cursor: usize,
    /// True while the input row has keyboard focus. Renders a bright
    /// cursor; otherwise the row is dim with an `i to type` hint.
    pub focused: bool,
    /// True when the terminal overlay (claude's PTY screen) is up. The
    /// footer hint switches to advertise passthrough + F10 dismiss.
    pub overlay_active: bool,
    /// Persistent horizontal scroll offset (in chars) for long inputs.
    /// Owned by the run loop so it survives across frames; render only
    /// nudges it the minimum amount needed to keep the cursor on
    /// screen. That way the caret can roam freely inside the window
    /// without the text snapping back to the start on every left edit.
    pub scroll: &'a mut usize,
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

#[allow(clippy::too_many_arguments)]
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
    chat_selection: Option<ChatSelection>,
    chat_inner_out: &mut Option<Rect>,
    chat_visible_out: &mut Vec<String>,
    chat_code_blocks_out: &mut Vec<CodeBlockRegion>,
    detail_copy_out: &mut Vec<DetailCopyRegion>,
    mouse_pos: Option<(u16, u16)>,
    driver: Option<&mut DriverPane<'_>>,
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

    // On short terminals the fixed panels above the chat (header 3 +
    // gauges 4 + totals 3 + meta 3) would crush it. Drop panels in
    // order of least loss — gauges (also visible in views 1/3), then
    // totals, then meta, then the blank spacer — until the chat keeps
    // a usable minimum height.
    const CHAT_MIN: u16 = 10;
    let driver_h: u16 = if driver.is_some() { 3 } else { 0 };
    let mut show_gauges = true;
    let mut show_totals = true;
    let mut show_meta = true;
    let mut show_blank = true;
    {
        let fixed = |g: bool, t: bool, m: bool, b: bool| -> u16 {
            3 + if g { 4 } else { 0 }
                + if t { 3 } else { 0 }
                + if m { 3 } else { 0 }
                + if b { 1 } else { 0 }
                + driver_h
                + 1 // footer
        };
        let fits = |g: bool, t: bool, m: bool, b: bool| {
            fixed(g, t, m, b) + CHAT_MIN <= area.height
        };
        if !fits(show_gauges, show_totals, show_meta, show_blank) {
            show_gauges = false;
        }
        if !fits(show_gauges, show_totals, show_meta, show_blank) {
            show_totals = false;
        }
        if !fits(show_gauges, show_totals, show_meta, show_blank) {
            show_meta = false;
        }
        if !fits(show_gauges, show_totals, show_meta, show_blank) {
            show_blank = false;
        }
    }

    let mut constraints = vec![Constraint::Length(3)]; // header
    if show_gauges {
        constraints.push(Constraint::Length(4)); // 3 gauges
    }
    if show_totals {
        constraints.push(Constraint::Length(3)); // compact session totals
    }
    if show_meta {
        constraints.push(Constraint::Length(3)); // meta
    }
    constraints.push(Constraint::Min(4)); // chat log (mode/activity now in its bottom border)
    if driver.is_some() {
        // 3-row input pane: borders + one row of text.
        constraints.push(Constraint::Length(3));
    }
    if show_blank {
        constraints.push(Constraint::Length(1)); // empty line above footer
    }
    constraints.push(Constraint::Length(1)); // keybind footer
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut next = 0usize;
    let mut row = || {
        let r = rows[next];
        next += 1;
        r
    };

    render_header(f, row(), session);

    if show_gauges {
        let gauge_area = row();
        if let Some(pa) = parent {
            let gauge_row = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(25),
                    Constraint::Percentage(25),
                    Constraint::Percentage(25),
                    Constraint::Percentage(25),
                ])
                .split(gauge_area);
            widgets::render_5h_gauge(f, gauge_row[0], &pa.agg, pa.live.as_ref());
            widgets::render_7d_gauge(f, gauge_row[1], pa.live.as_ref());
            widgets::render_extra_gauge(f, gauge_row[2], pa.live.as_ref());
            widgets::render_fable_gauge(f, gauge_row[3], pa.live.as_ref());
        }
    }

    if show_totals {
        render_session_table(f, row(), session);
    }
    if show_meta {
        render_meta_panel(f, row(), session);
    }
    let chat_area = row();
    render_chat_log(
        f,
        chat_area,
        session,
        chat_scroll,
        changes_selection,
        last_change_count,
        detail_scroll,
        chat_rect,
        actions_rect,
        detail_rect,
        chat_selection,
        chat_inner_out,
        chat_visible_out,
        chat_code_blocks_out,
        detail_copy_out,
        mouse_pos,
    );
    let default_hint =
        "↑/↓ switch · PgUp/PgDn scroll · m model · Del kill · Esc back · j/k changes · J/K detail";
    let driver_flags = driver.as_ref().map(|d| (d.overlay_active, d.focused));
    let footer_hint = match driver_flags {
        Some((true, _)) => "claude is asking — keys pass through · F10 dismiss",
        Some((false, true)) => {
            "Enter send · Ctrl-E editor · Shift-Tab mode · Esc unfocus · Ctrl-D end · Ctrl-C cancel"
        }
        Some((false, false)) => {
            "i type · m model · / skill · Shift-Tab mode · Ctrl-C cancel · Ctrl-D end · Del kill"
        }
        None => default_hint,
    };
    if let Some(d) = driver {
        render_driver_input(f, row(), d);
    }
    if show_blank {
        row(); // spacer above the footer
    }
    // Never show the `m mewxi` nav chip in the session view: `m` is
    // reserved for the model picker here (driven sessions open it,
    // observed ones get a nudge), so advertising it as the Mewxi
    // shortcut would be wrong. The splash stays reachable via `m`
    // from the other views.
    widgets::render_footer(f, row(), "2", footer_hint, false);
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
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
            ])
            .split(chunks[idx]);
        widgets::render_5h_gauge(f, gauge_row[0], &pa.agg, pa.live.as_ref());
        widgets::render_7d_gauge(f, gauge_row[1], pa.live.as_ref());
        widgets::render_extra_gauge(f, gauge_row[2], pa.live.as_ref());
        widgets::render_fable_gauge(f, gauge_row[3], pa.live.as_ref());
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
    // A pending pane is always a session mewxi spawned (driven), so `m`
    // is the model picker — drop the Mewxi nav chip here too.
    widgets::render_footer(
        f,
        chunks[idx],
        "2",
        "Esc cancel via K  ·  1 back to all sessions",
        false,
    );
}

fn render_driver_input(f: &mut Frame, area: Rect, d: &mut DriverPane<'_>) {
    let border_color = if d.focused { Color::Green } else { Color::DarkGray };
    let mut spans: Vec<Span> = vec![Span::styled(
        "> ",
        Style::default().fg(if d.focused { Color::Green } else { Color::DarkGray }),
    )];
    // Multi-line buffers (from Alt-e editor) don't fit a single-line
    // composer. Render the first line + a "+N more" badge so the user
    // sees something is staged; full content is preserved in the
    // buffer and round-trips via Alt-e for editing.
    if d.input.contains('\n') {
        let extra = d.input.matches('\n').count();
        let first_line = d.input.split('\n').next().unwrap_or("");
        spans.push(Span::raw(first_line.to_string()));
        spans.push(Span::styled(
            format!("  +{} more line{}", extra, if extra == 1 { "" } else { "s" }),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ));
        if d.focused {
            spans.push(Span::styled(
                " █",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border_color)),
            ),
            area,
        );
        return;
    }
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
    } else if d.focused {
        // Split around the cursor so mid-string edits actually show the
        // caret where the next keystroke will land. Clamp the byte
        // index defensively in case the caller passes a stale value.
        let cursor = d.cursor.min(d.input.len());
        let cursor = if d.input.is_char_boundary(cursor) {
            cursor
        } else {
            d.input.len()
        };

        // Horizontal scroll: nudge the persistent scroll offset just
        // enough to keep the cursor visible. The caret roams freely
        // inside the window — the view only slides when the cursor
        // would otherwise fall off the left or right edge.
        // `area.width` includes the 2-col border; the "> " prefix above
        // eats another 2 cols. Reserve 1 more col for the trailing
        // block when the cursor sits past the last char.
        let inner = (area.width as usize).saturating_sub(2 + 2);
        let chars: Vec<(usize, char)> = d.input.char_indices().collect();
        let cursor_char = d.input[..cursor].chars().count();
        let total_chars = chars.len();
        let cursor_at_end = cursor >= d.input.len();
        let budget = if cursor_at_end { inner.saturating_sub(1) } else { inner }.max(1);

        let mut start = *d.scroll;
        // Clamp first: if the buffer shrank (e.g. Ctrl-U), the previous
        // offset may now point past the end. Pull it back so the window
        // shows the tail of what's left.
        let max_start = total_chars.saturating_sub(budget);
        if start > max_start {
            start = max_start;
        }
        // Cursor walked off the left edge → slide the window left so
        // the cursor lands on the leftmost visible column.
        if cursor_char < start {
            start = cursor_char;
        }
        // Cursor walked off the right edge → slide the window right so
        // the cursor lands on the rightmost visible column.
        if cursor_char >= start + budget {
            start = (cursor_char + 1).saturating_sub(budget);
        }
        *d.scroll = start;

        let start_char = start;
        let end_char = (start_char + budget).min(total_chars);
        let start_byte = chars.get(start_char).map(|(b, _)| *b).unwrap_or(d.input.len());
        let end_byte = chars.get(end_char).map(|(b, _)| *b).unwrap_or(d.input.len());

        let pre = &d.input[start_byte..cursor.min(end_byte).max(start_byte)];
        if !pre.is_empty() {
            spans.push(Span::raw(pre.to_string()));
        }
        if cursor < d.input.len() && cursor < end_byte {
            // Cursor sits over a real visible char: render it with
            // reversed colours so it reads as a block cursor without
            // losing the glyph underneath.
            let next = next_char_boundary(d.input, cursor);
            spans.push(Span::styled(
                d.input[cursor..next.min(end_byte)].to_string(),
                Style::default()
                    .bg(Color::Green)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ));
            let rest_end = end_byte;
            let rest_start = next.min(rest_end);
            if rest_start < rest_end {
                spans.push(Span::raw(d.input[rest_start..rest_end].to_string()));
            }
        } else {
            // Cursor past the last visible char — append the trailing
            // block. (Mid-string cursor scrolled past the right edge
            // can't happen because the scroll window keeps it in view.)
            spans.push(Span::styled(
                "█",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ));
        }
    } else {
        // Unfocused: still clamp to the available width so a long
        // staged prompt doesn't overflow the border.
        let inner = (area.width as usize).saturating_sub(2 + 2).max(1);
        let chars: Vec<(usize, char)> = d.input.char_indices().collect();
        if chars.len() <= inner {
            spans.push(Span::raw(d.input.to_string()));
        } else {
            let end_byte = chars[inner].0;
            spans.push(Span::raw(d.input[..end_byte].to_string()));
        }
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border_color)),
        ),
        area,
    );
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

// Mirrors view_all.rs's `activity_display` so the same word reads the
// same colour across views; kept in sync manually.
fn activity_badge(a: &Activity) -> (String, Color) {
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

/// Playful animated cat. Kneads + occasional blink while the agent is
/// doing anything; curls up with accumulating `ᶻ`s when waiting. Frame
/// is wall-clock derived so it advances correctly across the variable
/// redraw cadence (session view redraws at ~5fps minimum).
///
/// The 5-cell body `^•ﻌ•^` is anchored at columns 2–6 of the returned
/// span; column 1 is reserved for the left paw (a space when absent)
/// so the body doesn't jerk sideways when paws flash. Columns 0 and
/// the trailing column give one cell of breathing room on each side
/// so the cat doesn't sit flush against the bottom-border `─` chars.
/// Right-side extras (right paw or sleep ᶻ's) sit between the body
/// and the trailing breathing cell.
fn cat_indicator(activity: &Activity) -> (Span<'static>, u16) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if matches!(activity, Activity::Waiting) {
        // 0 → 1 → 2 → 3 ᶻ's so the snoring count climbs smoothly
        // instead of jumping from 1 to 3. Sleep slot is padded to a
        // fixed [`SLEEP_SLOT`] width: max-z body (10 cells: 2 leading
        // + 5 body + 3 ᶻ's) + 2 trailing breathing cells. Fixed width
        // keeps the cat's right edge from creeping outward as ᶻ's
        // accumulate.
        const SLEEP_Z: [&str; 4] = ["", "ᶻ", "ᶻᶻ", "ᶻᶻᶻ"];
        const SLEEP_SLOT: usize = 12;
        let zs = SLEEP_Z[((millis / 1200) % SLEEP_Z.len() as u128) as usize];
        // 2 leading + 5 body + ᶻ's, then pad to SLEEP_SLOT.
        let mut s = format!("  ^-ﻌ-^{zs}");
        let core_w = 7 + zs.chars().count();
        for _ in core_w..SLEEP_SLOT {
            s.push(' ');
        }
        return (
            Span::styled(s, Style::default().fg(Color::DarkGray)),
            SLEEP_SLOT as u16,
        );
    }
    // Busy: 24-beat × 400ms = 9.6s cycle. Pattern: `-B-B-B-----LLL---RRR----`
    // — three quick blinks (single beats) up front, then alternating paw
    // flashes (3 beats each). 400ms aligns with the session view's 200ms
    // poll cadence so beats land evenly.
    let beat = (millis / 400) % 24;
    let (paw_left, paw_right, eyes) = match beat {
        1 | 3 | 5 => (false, false, '-'),   // blink
        11 | 12 | 13 => (true, false, '•'), // left paw
        17 | 18 | 19 => (false, true, '•'), // right paw
        _ => (false, false, '•'),           // rest
    };
    // All busy frames render as a fixed 9-cell slot: outer breathing
    // cell + left-paw slot + 5-cell body + right-paw slot + outer
    // breathing cell. Paws light up their reserved slot in place of a
    // space, so the body (and the cat's overall right edge) stays
    // anchored across rest/blink/left-paw/right-paw transitions.
    let mut out = String::with_capacity(16);
    out.push(' '); // col 0 — outer breathing
    out.push(if paw_left { 'ฅ' } else { ' ' }); // col 1
    out.push('^'); // col 2
    out.push(eyes); // col 3
    out.push('ﻌ'); // col 4 — mouth, stable anchor
    out.push(eyes); // col 5
    out.push('^'); // col 6
    out.push(if paw_right { 'ฅ' } else { ' ' }); // col 7
    out.push(' '); // col 8 — outer breathing
    let w = 9u16;
    // Constant cat colour while busy so it reads as the same critter
    // regardless of activity — the activity badge already carries the
    // colour signal. Sleep stays dim (handled above).
    (
        Span::styled(
            out,
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        w as u16,
    )
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

/// Colour for the `/effort` badge. Picked along a "cool → warm"
/// gradient so the badge reads as a thermometer: dim grey for `auto`
/// (let claude decide), blue/green for the cheap levels, magenta/red
/// for the expensive ones. `?` for unknown future levels.
pub(crate) fn effort_color(raw: &str) -> Color {
    match raw {
        "auto" => Color::DarkGray,
        "low" => Color::Blue,
        "medium" => Color::Green,
        "high" => Color::Cyan,
        "xhigh" => Color::Magenta,
        "max" => Color::Red,
        _ => Color::DarkGray,
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
    // "default" is a placeholder meaning "whatever claude picks"; any
    // real model name agrees with it. Without this, unconfigured
    // accounts paint a perpetual `via <model>` indicator next to the
    // `[default]` badge even though there's no real divergence.
    if p == "default" {
        return true;
    }
    let a = active.to_ascii_lowercase();
    a.contains(&p) || p.contains(&a)
}

/// Model names shown in badges drop the redundant `claude-` vendor
/// prefix (`claude-sonnet-4-6` → `sonnet-4-6`) to save horizontal
/// space; short slugs (`opus`, `default`) pass through untouched.
///
/// Shared across every view that renders a model name so they all agree
/// on formatting — a per-view reimplementation previously truncated
/// unrecognized families (e.g. `claude-fable-5` → `claude-f`) instead of
/// just stripping the vendor prefix.
pub(crate) fn trim_model(m: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = m.strip_prefix("claude-").unwrap_or(m);
    // Split off a `[1m]` tier suffix so the date check below still fires
    // on `…-20251001[1m]`-shaped ids.
    let (base, has_tier) = match trimmed.find("[1m]") {
        Some(i) => (&trimmed[..i], true),
        None => (trimmed, false),
    };
    // Drop the trailing release-date stamp — model ids read from transcript
    // records carry it (`haiku-4-5-20251001`), and it's noise in a label.
    let base = base
        .rsplit_once('-')
        .filter(|(_, tail)| tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()))
        .map(|(head, _)| head)
        .unwrap_or(base);
    if !has_tier {
        return std::borrow::Cow::Borrowed(base);
    }
    // Families that are natively 1M (Fable, Opus 4.8+) drop the tier suffix
    // entirely — it's redundant. Others capitalize it (`[1m]` -> `[1M]`) so
    // it reads consistently with the statusline's `1M` label.
    if crate::stats::native_1m_context(base) {
        std::borrow::Cow::Borrowed(base)
    } else {
        std::borrow::Cow::Owned(format!("{base}[1M]"))
    }
}

fn render_header(f: &mut Frame, area: Rect, s: &SessionRef) {
    let mut spans = vec![
        Span::styled(
            format!("[{}]", s.account_name),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(s.project.clone(), Style::default().fg(Color::Cyan)),
    ];
    match &s.subagent {
        Some(tag) => {
            // Sub-agent row: label the id as an agent and point back at
            // the delegating session so it's obvious this isn't a
            // top-level session.
            spans.push(Span::raw("  agent "));
            spans.push(Span::styled(
                s.session_id.clone(),
                Style::default().fg(Color::Yellow),
            ));
            spans.push(Span::styled(
                format!("  ↳ {}", tag.agent_type.as_deref().unwrap_or("sub-agent")),
                Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!(
                    " of session {}",
                    tag.parent_session_id.chars().take(8).collect::<String>()
                ),
                Style::default().fg(Color::DarkGray),
            ));
        }
        None => {
            spans.push(Span::raw("  session "));
            spans.push(Span::styled(
                s.session_id.clone(),
                Style::default().fg(Color::Yellow),
            ));
        }
    }
    f.render_widget(
        Paragraph::new(Line::from(spans))
            .block(Block::default().borders(Borders::ALL).title("Session detail")),
        area,
    );
}

/// Spans for the ephemeral status indicators (`[mode]`, `[activity]`,
/// `[idle]`, transient `via <model>`). Rendered as the chat-log
/// block's bottom-title overlay so the live state sits visually attached
/// to the chat it describes. Returns an empty vec when nothing notable
/// is happening — callers should then skip the `title_bottom` call so
/// the chat box keeps a clean border.
fn build_status_spans(s: &SessionRef) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let push_sep = |spans: &mut Vec<Span<'static>>| {
        if !spans.is_empty() {
            // Box-drawing horizontals rendered with the default
            // foreground so they read at the same weight as the
            // chat box's own border — the badges look strung along
            // a continuous bottom edge: `[…]──[…]──[…]`.
            spans.push(Span::raw("──"));
        }
    };
    // Thinking effort sits to the left of the permission-mode badge so
    // the two `/`-commands a user controls (`/effort` and Shift-Tab's
    // permission mode) read left-to-right in pick order.
    if let Some(eff) = s.effort.as_deref() {
        push_sep(&mut spans);
        let model = if s.model.is_empty() {
            std::borrow::Cow::Borrowed("default")
        } else {
            trim_model(&s.model)
        };
        spans.push(Span::styled(
            format!("[{model}:{eff}]"),
            Style::default()
                .fg(effort_color(eff))
                .add_modifier(Modifier::BOLD),
        ));
    } else if s.subagent.is_some() && !s.model.is_empty() {
        // Sub-agent on an effort-less model (Haiku): the effort badge
        // above never fires, but the model the agent runs on still has
        // to be visible in this view — show it bare.
        push_sep(&mut spans);
        spans.push(Span::styled(
            format!("[{}]", trim_model(&s.model)),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(mode_raw) = s.permission_mode.as_deref() {
        let (label, color) = mode_badge(mode_raw);
        push_sep(&mut spans);
        spans.push(Span::styled(
            format!("[{label}]"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }
    // Hide Waiting — it's the boring default; the idle-for-Nm badge
    // already conveys "nothing happening" when applicable.
    if !matches!(s.activity, Activity::Waiting) {
        let (label, color) = activity_badge(&s.activity);
        push_sep(&mut spans);
        spans.push(Span::styled(
            format!("[{label}]"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
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
        push_sep(&mut spans);
        spans.push(Span::styled(
            format!("via {}", trim_model(&s.active_model)),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    if s.state == SessionState::Idle {
        push_sep(&mut spans);
        spans.push(Span::styled(
            "[idle]",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

/// Worst-case width (in cells) of [`build_status_spans`] given the
/// session's current *badge structure*. Per-badge content that
/// fluctuates rapidly (activity-label flaps, idle-minute increments,
/// mode label swaps) is replaced with its largest plausible value so
/// the cat anchor downstream stays stable across those changes. Only
/// structural changes — a badge appearing or disappearing — shift it.
fn build_status_reserve_width(s: &SessionRef) -> usize {
    // Widest known fixed-set labels (recompute if new variants are added).
    const MAX_MODE_LABEL: usize = 12; // "accept edits"
    const MAX_ACTIVITY_LABEL: usize = 10; // "delegating" / "compacting"
    let mut w = 0usize;
    let push_sep = |w: &mut usize| {
        if *w > 0 {
            // "──" — must match the separator emitted by
            // `build_status_spans`.
            *w += 2;
        }
    };
    if let Some(eff) = s.effort.as_deref() {
        push_sep(&mut w);
        let model_w = if s.model.is_empty() {
            "default".len()
        } else {
            trim_model(&s.model).chars().count()
        };
        // "[model:effort]"
        w += 2 + model_w + 1 + eff.chars().count();
    } else if s.subagent.is_some() && !s.model.is_empty() {
        // "[model]" — the bare sub-agent model badge.
        push_sep(&mut w);
        w += 2 + trim_model(&s.model).chars().count();
    }
    if s.permission_mode.is_some() {
        push_sep(&mut w);
        w += 2 + MAX_MODE_LABEL;
    }
    if !matches!(s.activity, Activity::Waiting) {
        push_sep(&mut w);
        let label_w = match &s.activity {
            // Tool name is set per turn and doesn't flap beat-to-beat;
            // use the current width.
            Activity::Tool(n) => n.chars().count(),
            _ => MAX_ACTIVITY_LABEL,
        };
        w += 2 + label_w;
    }
    if !s.active_model.is_empty()
        && !s.model.is_empty()
        && !models_match(&s.model, &s.active_model)
    {
        push_sep(&mut w);
        w += 4 + trim_model(&s.active_model).chars().count(); // "via <model>"
    }
    if s.state == SessionState::Idle {
        push_sep(&mut w);
        w += "[idle]".chars().count();
    }
    w
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
    let mut spans = vec![
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
    ];
    if let Some(branch) = git_branch(&s.cwd) {
        spans.push(Span::raw("    "));
        spans.push(Span::styled("branch ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            branch,
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ));
    }
    let line = Line::from(spans);
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

/// Splits each visible chat line at the selection boundary and
/// REVERSE-modifies the spans that fall inside the rectangle. Column
/// math uses `chars().count()` — accurate for ASCII / Latin text and
/// only slightly off for wide-character runs, which the chat log
/// rarely contains.
fn apply_selection_highlight(
    visible: &mut Vec<Line<'static>>,
    inner: Rect,
    sel: ChatSelection,
) {
    let (col_start, row_start, col_end, row_end) = sel.rect();
    // Clamp the selection to the inner area; nothing outside it can
    // be highlighted (or copied) by design.
    let inner_x_end = inner.x.saturating_add(inner.width);
    let inner_y_end = inner.y.saturating_add(inner.height);
    let cs = col_start.max(inner.x).min(inner_x_end);
    let ce = col_end.max(inner.x).min(inner_x_end);
    if ce <= cs {
        return;
    }
    let rs = row_start.max(inner.y).min(inner_y_end);
    let re = row_end.max(inner.y).min(inner_y_end);
    let i_start = rs.saturating_sub(inner.y) as usize;
    let i_end = re.saturating_sub(inner.y) as usize;
    let col_lo = cs.saturating_sub(inner.x) as usize;
    let col_hi = ce.saturating_sub(inner.x) as usize;
    for i in i_start..=i_end {
        if i >= visible.len() {
            break;
        }
        visible[i] = highlight_line(&visible[i], col_lo, col_hi);
    }
}

/// Paint a subtle background across visible rows `[lo, hi)` to mark the
/// code block under the mouse as clickable. Fills spans that don't
/// already carry a background (so the user-message tint and any other
/// explicit bg win), then pads each row out to `row_w` so the highlight
/// reads as a clean rectangular band rather than tracing the ragged
/// right edge of the code.
fn apply_code_hover(visible: &mut [Line<'static>], lo: usize, hi: usize, row_w: usize) {
    // A touch lighter than the default terminal background — enough to
    // read as "this block is live" without competing with the text.
    const HOVER_BG: Color = Color::Indexed(236);
    for line in visible.iter_mut().take(hi).skip(lo) {
        let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        let mut spans: Vec<Span<'static>> = std::mem::take(&mut line.spans)
            .into_iter()
            .map(|mut s| {
                if s.style.bg.is_none() {
                    s.style = s.style.bg(HOVER_BG);
                }
                s
            })
            .collect();
        if used < row_w {
            spans.push(Span::styled(
                " ".repeat(row_w - used),
                Style::default().bg(HOVER_BG),
            ));
        }
        let mut new_line = Line::from(spans);
        new_line.alignment = line.alignment;
        *line = new_line;
    }
}

fn highlight_line(line: &Line<'static>, col_lo: usize, col_hi: usize) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut col = 0usize;
    for span in &line.spans {
        let w = span.content.chars().count();
        let span_start = col;
        let span_end = col + w;
        col = span_end;
        if w == 0 {
            out.push(span.clone());
            continue;
        }
        if span_end <= col_lo || span_start >= col_hi {
            out.push(span.clone());
            continue;
        }
        let chars: Vec<char> = span.content.chars().collect();
        let mid_lo = col_lo.saturating_sub(span_start);
        let mid_hi = col_hi.saturating_sub(span_start).min(w);
        if mid_lo > 0 {
            let s: String = chars[..mid_lo].iter().collect();
            out.push(Span::styled(s, span.style));
        }
        let s_mid: String = chars[mid_lo..mid_hi].iter().collect();
        out.push(Span::styled(
            s_mid,
            span.style.add_modifier(Modifier::REVERSED),
        ));
        if mid_hi < w {
            let s: String = chars[mid_hi..].iter().collect();
            out.push(Span::styled(s, span.style));
        }
    }
    let mut new_line = Line::from(out);
    new_line.alignment = line.alignment;
    new_line
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
    chat_selection: Option<ChatSelection>,
    chat_inner_out: &mut Option<Rect>,
    chat_visible_out: &mut Vec<String>,
    chat_code_blocks_out: &mut Vec<CodeBlockRegion>,
    detail_copy_out: &mut Vec<DetailCopyRegion>,
    mouse_pos: Option<(u16, u16)>,
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

    let width = chat_area.width.saturating_sub(4) as usize;
    let body_w = width.saturating_sub(LABEL_W).max(10);
    // Reserve 2 rows for top/bottom borders plus 2 rows of persistent
    // inner padding (one at the top, one at the bottom) so messages
    // never sit flush against the chat-log border, even mid-scroll.
    let target_h = chat_area.height.saturating_sub(4) as usize;

    // Track chat-line ranges per ToolUse so the currently-selected
    // action in the right pane can be visually echoed in the chat log.
    let mut all: Vec<Line<'static>> = Vec::with_capacity(entries.len() * 2);
    let mut action_line_ranges: Vec<(usize, usize)> = Vec::new();
    // Code blocks across the whole buffer, as (start, end exclusive,
    // source) in absolute `all` indices. Projected to screen rows once
    // the visible window is known so a click can map to its source.
    let mut code_blocks_abs: Vec<(usize, usize, String)> = Vec::new();
    for e in &entries {
        let start = all.len();
        let (lines, blocks) = entry_to_lines(e, body_w, &s.cwd);
        all.extend(lines);
        for b in blocks {
            code_blocks_abs.push((start + b.start, start + b.end, b.source));
        }
        if matches!(e.kind, EntryKind::ToolUse { .. }) {
            action_line_ranges.push((start, all.len()));
        }
    }

    // Resolve the action selection up front (mirrors the logic used by
    // the Actions pane below) so we can apply the matching highlight
    // to the chat log before it's sliced for rendering. Only highlight
    // when the user has explicitly selected a row — None means "follow
    // tail" and shouldn't drag a highlight onto every new tool call.
    let row_count = entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::ToolUse { .. }))
        .count();
    if let Some(sel) = *changes_selection {
        let resolved = if row_count == 0 { 0 } else { sel.min(row_count - 1) };
        if let Some(&(rs, re)) = action_line_ranges.get(resolved) {
            for line in &mut all[rs..re] {
                for span in &mut line.spans {
                    span.style = span.style.add_modifier(Modifier::REVERSED);
                }
            }
            // Drag the chat viewport along with the highlighted action so
            // navigating j/k in the Actions pane never strands the
            // selection off-screen. `scroll` counts lines back from the
            // tail; visible window is `[total - scroll - target_h, total - scroll)`.
            let total = all.len();
            let max_scroll = total.saturating_sub(target_h);
            let cur_end = total.saturating_sub(*scroll);
            let cur_start = cur_end.saturating_sub(target_h);
            if target_h > 0 && (rs < cur_start || re > cur_end) {
                // Center the block in the viewport when it fits;
                // otherwise anchor its start at the top.
                let span = re.saturating_sub(rs);
                let desired_end = if span <= target_h {
                    let mid = rs + span / 2;
                    (mid + target_h / 2).min(total).max(re)
                } else {
                    (rs + target_h).min(total)
                };
                *scroll = total.saturating_sub(desired_end).min(max_scroll);
            }
        }
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
    // Live status (mode/activity/via/idle) overlays the chat-log's
    // bottom border so it sits visually attached to the conversation
    // it describes. The cat rides on the same border row, rendered
    // as a *separate* 1-row widget — title spans paint every cell
    // (including spaces) opaquely, so to keep the box's `─` chars
    // visible between status and cat we have to leave those cells
    // unrendered by either widget.
    //
    // `build_status_reserve_width` returns the worst-case width for
    // the current badge structure (max activity-label width, max
    // plausible idle-minute digits, max mode-label width); the cat
    // anchors to that so per-beat label flaps (`[thinking]` ↔
    // `[running]`) and per-minute idle ticks don't yank it sideways.
    // It only shifts on structural changes — a badge appearing or
    // disappearing.
    let status = build_status_spans(s);
    let actual_w: usize = status.iter().map(|s| s.content.chars().count()).sum();
    let reserve_w = build_status_reserve_width(s).max(actual_w);
    let mut block = Block::default().borders(Borders::ALL).title(title);
    if !status.is_empty() {
        block = block.title_bottom(Line::from(status));
    }
    let block_inner = block.inner(chat_area);
    f.render_widget(block, chat_area);
    // Cat: anchor at chat_area.x + 1 (skip corner) + reserve_w + GAP,
    // then nudge [`CAT_SHIFT_LEFT`] cells leftward so the cat sits
    // visually closer to the typical (sub-reserve) status. Floored at
    // `actual_w + min_gap` so it never overlaps a status that has
    // grown into the reserved slack. Skipped when the chat box is too
    // narrow to host both status and cat.
    const CAT_GAP: u16 = 2;
    const CAT_SHIFT_LEFT: u16 = 10;
    let (cat_span, cat_w) = cat_indicator(&s.activity);
    let cat_offset_base = 1u16
        .saturating_add(reserve_w as u16)
        .saturating_add(CAT_GAP);
    let cat_offset_shifted = cat_offset_base.saturating_sub(CAT_SHIFT_LEFT);
    let cat_offset_min = 1u16
        .saturating_add(actual_w as u16)
        .saturating_add(CAT_GAP);
    let cat_offset = cat_offset_shifted.max(cat_offset_min);
    let cat_x = chat_area.x.saturating_add(cat_offset);
    let cat_y = chat_area
        .y
        .saturating_add(chat_area.height.saturating_sub(1));
    let right_corner = chat_area.x.saturating_add(chat_area.width.saturating_sub(1));
    if cat_x.saturating_add(cat_w) < right_corner {
        let cat_rect = Rect {
            x: cat_x,
            y: cat_y,
            width: cat_w,
            height: 1,
        };
        f.render_widget(Paragraph::new(Line::from(cat_span)), cat_rect);
    }

    // Persistent 1-cell padding around the chat viewport (top/bottom/left/right).
    let inner = Rect {
        x: block_inner.x.saturating_add(1),
        y: block_inner.y.saturating_add(1),
        width: block_inner.width.saturating_sub(2),
        height: block_inner.height.saturating_sub(2),
    };

    *chat_inner_out = Some(inner);
    chat_visible_out.clear();
    chat_code_blocks_out.clear();
    // Cleared here (not in render_changes_detail) so the narrow layout —
    // which never renders the Detail pane — leaves no stale regions for
    // the click handler to match against.
    detail_copy_out.clear();
    if entries.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no chat content yet — waiting for transcript",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(hint, inner);
    } else {
        let end = total.saturating_sub(clamped);
        let start = end.saturating_sub(target_h);
        // Is the cursor hovering inside the chat pane's horizontal band?
        // (Used to decide whether a code block should light up.)
        let hover_in_band = mouse_pos.is_some_and(|(c, _)| {
            c >= inner.x && c < inner.x.saturating_add(inner.width)
        });
        // Visible-row range (relative to `visible`, end exclusive) of the
        // code block under the cursor, if any — gets a subtle highlight.
        let mut hover_rows: Option<(usize, usize)> = None;
        // Project each code block onto the visible window's screen rows.
        // A block straddling the top/bottom edge is clipped to whatever
        // rows are on screen — clicking any visible row still copies the
        // whole (untruncated) source. Rows outside [start, end) are not
        // clickable this frame.
        for (bs, be, source) in &code_blocks_abs {
            let vis_start = (*bs).max(start);
            let vis_end = (*be).min(end);
            if vis_end <= vis_start {
                continue;
            }
            let top = inner.y.saturating_add((vis_start - start) as u16);
            let bottom = inner
                .y
                .saturating_add((vis_end - start - 1) as u16);
            if hover_in_band {
                if let Some((_, r)) = mouse_pos {
                    if r >= top && r <= bottom {
                        hover_rows = Some((vis_start - start, vis_end - start));
                    }
                }
            }
            chat_code_blocks_out.push(CodeBlockRegion {
                top,
                bottom,
                source: source.clone(),
            });
        }
        let mut visible: Vec<Line<'static>> = all[start..end].to_vec();
        // Stash plain text for each visible row (one entry per row)
        // so the parent can extract selected text without re-walking
        // the transcript or coping with split spans.
        for line in &visible {
            let mut s = String::new();
            for span in &line.spans {
                s.push_str(&span.content);
            }
            chat_visible_out.push(s);
        }
        // Subtle hover fill on the code block under the cursor — a hint
        // that it's clickable to copy. Applied before the selection
        // highlight so an active drag-selection still reads on top.
        if let Some((lo, hi)) = hover_rows {
            apply_code_hover(&mut visible, lo, hi, inner.width as usize);
        }
        if let Some(sel) = chat_selection {
            apply_selection_highlight(&mut visible, inner, sel);
        }
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

        let tasks = chat_log::extract_tasks(&entries);
        // Reserve a bottom slice for the Tasks panel sized to the tasks
        // we actually have — bordered box (2) + one row per task, capped
        // at ~40% of the right column so it can't squeeze Actions/Detail
        // off-screen on tall layouts. A short "no tasks yet" placeholder
        // is still shown so the user knows the panel exists.
        let task_lines: u16 = tasks.len().min(64) as u16;
        let want_h: u16 = (task_lines + 2).max(4);
        let max_h: u16 = (panel_area.height * 4) / 10;
        let tasks_h: u16 = want_h.min(max_h).max(4);
        let upper_h: u16 = panel_area.height.saturating_sub(tasks_h);

        let outer_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(upper_h), Constraint::Length(tasks_h)])
            .split(panel_area);
        let panel_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(outer_rows[0]);
        *actions_rect = Some(panel_rows[0]);
        *detail_rect = Some(panel_rows[1]);
        render_changes_list(f, panel_rows[0], &rows, resolved, &s.cwd);
        render_changes_detail(
            f,
            panel_rows[1],
            &rows,
            resolved,
            detail_scroll,
            &s.cwd,
            detail_copy_out,
            mouse_pos,
        );
        render_tasks_panel(f, outer_rows[1], &tasks);
    } else {
        *last_change_count = 0;
    }
}

/// Render the reconstructed task list from the transcript. Each row is
/// `<status-glyph> <subject>`; the in-progress row also shows its
/// `activeForm` in dim text to mirror what Claude's todo UI shows the
/// user. Counts in the title (`done/total`) give a quick progress
/// readout at the box level.
fn render_tasks_panel(f: &mut Frame, area: Rect, tasks: &[Task]) {
    let done = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Completed)
        .count();
    let active = tasks
        .iter()
        .find(|t| t.status == TaskStatus::InProgress);
    let title = if tasks.is_empty() {
        "Tasks".to_string()
    } else {
        format!("Tasks ({done}/{} done)", tasks.len())
    };

    let mut block = Block::default().borders(Borders::ALL).title(title);
    if let Some(t) = active {
        // Bottom border echoes whatever Claude is *currently* doing so
        // it sits visually attached to the task list without taking up
        // an interior row.
        let af = t.active_form.as_deref().unwrap_or(&t.subject);
        block = block.title_bottom(Line::from(vec![
            Span::styled(" ▶ ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(
                truncate_chars(af, area.width.saturating_sub(6) as usize),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    let inner = block.inner(area);
    f.render_widget(block, area);

    if tasks.is_empty() {
        let hint = Paragraph::new(Line::from(Span::styled(
            "no tasks yet — claude will populate this once it calls TaskCreate",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(hint, inner);
        return;
    }

    let width = inner.width as usize;
    let target_h = inner.height as usize;
    // Anchor on the active task when one exists so it stays visible;
    // otherwise show the tail (latest creation order) like a TODO list.
    let anchor = tasks
        .iter()
        .position(|t| t.status == TaskStatus::InProgress)
        .unwrap_or(tasks.len().saturating_sub(1));
    let max_start = tasks.len().saturating_sub(target_h);
    let half = target_h / 2;
    let start = anchor.saturating_sub(half).min(max_start);
    let end = (start + target_h).min(tasks.len());

    let lines: Vec<Line<'static>> = tasks[start..end]
        .iter()
        .map(|t| {
            let (glyph, glyph_color) = match t.status {
                TaskStatus::Pending => ("○", Color::DarkGray),
                TaskStatus::InProgress => ("▶", Color::Yellow),
                TaskStatus::Completed => ("✓", Color::Green),
                TaskStatus::Cancelled => ("✗", Color::Red),
                TaskStatus::Other => ("?", Color::DarkGray),
            };
            let subj_style = match t.status {
                TaskStatus::Completed => Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT),
                TaskStatus::InProgress => Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
                _ => Style::default().fg(Color::Gray),
            };
            let prefix_w = 2 + 1 + t.id.chars().count() + 2;
            let body_w = width.saturating_sub(prefix_w).max(4);
            let subj = truncate_chars(&t.subject, body_w);
            Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(glyph_color).add_modifier(Modifier::BOLD)),
                Span::styled(format!("#{} ", t.id), Style::default().fg(Color::DarkGray)),
                Span::styled(subj, subj_style),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines), inner);
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

#[allow(clippy::too_many_arguments)]
fn render_changes_detail(
    f: &mut Frame,
    area: Rect,
    rows: &[ChangeRow],
    selection: usize,
    detail_scroll: &mut usize,
    cwd: &Path,
    detail_copy_out: &mut Vec<DetailCopyRegion>,
    mouse_pos: Option<(u16, u16)>,
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
    // `parts` are click-to-copy command segments, in `lines` row indices
    // (only the head of the buffer — the output appended below never
    // contains parts).
    let (mut lines, parts) = format_tool_detail(&row.name, &row.input, width, cwd);

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
    // Advertise click-to-copy only when there are parts to copy (Bash
    // rows); other tools just get the scroll hint.
    let mut hint_spans = vec![Span::raw(" ")];
    if !parts.is_empty() {
        hint_spans.push(Span::styled("click", Style::default().fg(Color::Yellow)));
        hint_spans.push(Span::styled(" copy · ", Style::default().fg(Color::DarkGray)));
    }
    hint_spans.push(Span::styled("J/K", Style::default().fg(Color::Yellow)));
    hint_spans.push(Span::styled(" scroll ", Style::default().fg(Color::DarkGray)));
    let hint = Line::from(hint_spans).alignment(Alignment::Right);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title(hint);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Project each command part onto the visible window's screen rows so
    // the run loop can map a click back to its source. A part straddling
    // a scroll edge is clipped to whatever rows are on screen — clicking
    // any visible row still copies the whole (unwrapped) part. Parts that
    // scrolled fully out of view (e.g. into the output section) emit no
    // region this frame.
    let hover_in_band = mouse_pos
        .is_some_and(|(c, _)| c >= inner.x && c < inner.x.saturating_add(inner.width));
    let mut hover_rows: Option<(usize, usize)> = None;
    for (ps, pe, source) in &parts {
        let vis_start = (*ps).max(start);
        let vis_end = (*pe).min(end);
        if vis_end <= vis_start {
            continue;
        }
        let top = inner.y.saturating_add((vis_start - start) as u16);
        let bottom = inner.y.saturating_add((vis_end - start - 1) as u16);
        if hover_in_band {
            if let Some((_, r)) = mouse_pos {
                if r >= top && r <= bottom {
                    hover_rows = Some((vis_start - start, vis_end - start));
                }
            }
        }
        detail_copy_out.push(DetailCopyRegion {
            top,
            bottom,
            source: source.clone(),
        });
    }

    let mut visible: Vec<Line<'static>> = lines.drain(start..end).collect();
    // Subtle hover fill on the command part under the cursor — a hint
    // that it's clickable to copy, mirroring the chat-log code blocks.
    if let Some((lo, hi)) = hover_rows {
        apply_code_hover(&mut visible, lo, hi, inner.width as usize);
    }
    f.render_widget(Paragraph::new(visible), inner);
}

/// Render a tool's `input` to styled detail lines. Also returns the
/// click-to-copy parts the lines contain (in `out`'s row indices) —
/// currently the segments of a Bash `command`; empty for every other
/// tool.
fn format_tool_detail(
    name: &str,
    input: &Value,
    width: usize,
    cwd: &Path,
) -> (Vec<Line<'static>>, Vec<(usize, usize, String)>) {
    let lower = name.to_ascii_lowercase();
    let body_w = width.max(10);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut parts: Vec<(usize, usize, String)> = Vec::new();
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
                parts = push_bash_command(&mut out, &cmd, body_w);
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
    (out, parts)
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
///
/// Returns one `(start, end_exclusive, source)` per command part, with
/// the row range (in `out`'s indices, spanning every wrapped sub-line
/// of the part) and the verbatim *unwrapped* part text (no leading
/// separator), so the caller can make each part individually
/// click-to-copy.
fn push_bash_command(
    out: &mut Vec<Line<'static>>,
    cmd: &str,
    width: usize,
) -> Vec<(usize, usize, String)> {
    let exec_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let sep_style = Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::White);

    let mut parts: Vec<(usize, usize, String)> = Vec::new();
    for raw in cmd.split('\n') {
        if raw.is_empty() {
            out.push(Line::raw(""));
            continue;
        }
        // Visual sugar: break the command at top-level shell
        // separators (`&&`, `||`, `|`, `;`) so each action sits on
        // its own line with the separator leading the next line.
        for (idx, (sep, body)) in split_bash_by_separators(raw).into_iter().enumerate() {
            // The copyable source is the bare part body (already
            // trimmed by the splitter), without the leading separator —
            // so pasting it yields a runnable command on its own.
            let source = body.clone();
            let line_text = match (idx, sep) {
                (0, _) => body,
                (_, Some(s)) => format!(" {s} {body}"),
                (_, None) => body,
            };
            let part_start = out.len();
            let wrapped = wrap_text(&line_text, width);
            if wrapped.is_empty() {
                out.push(Line::raw(""));
                continue;
            }
            // First wrapped sub-line: scan for executable tokens and
            // shell separators. Subsequent sub-lines render plain so
            // the highlight stays anchored to the real start of
            // commands.
            out.push(Line::from(bash_spans(&wrapped[0], exec_style, sep_style, normal)));
            for cont in &wrapped[1..] {
                out.push(Line::from(Span::styled(cont.clone(), normal)));
            }
            if !source.is_empty() {
                parts.push((part_start, out.len(), source));
            }
        }
    }
    parts
}

/// Split a one-line bash command into segments at top-level shell
/// separators. Returns `(separator_before, body)` pairs; the first
/// entry's separator is `None`. Best-effort and quote-unaware, matching
/// the highlighter's existing fidelity.
fn split_bash_by_separators(line: &str) -> Vec<(Option<&'static str>, String)> {
    const SEPS: &[&str] = &["&&", "||", "|", ";"];
    let mut segments: Vec<(Option<&'static str>, String)> = Vec::new();
    let mut current_sep: Option<&'static str> = None;
    let mut buf = String::new();
    let mut i = 0;
    while i < line.len() {
        let rest = &line[i..];
        let matched = SEPS.iter().find(|s| rest.starts_with(**s)).copied();
        if let Some(s) = matched {
            segments.push((current_sep, buf.trim().to_string()));
            current_sep = Some(s);
            buf.clear();
            i += s.len();
            continue;
        }
        let ch = line[i..].chars().next().unwrap();
        let ch_len = ch.len_utf8();
        buf.push(ch);
        i += ch_len;
    }
    segments.push((current_sep, buf.trim().to_string()));
    segments
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

/// Render one chat entry to styled lines, plus any code blocks it
/// contains with their ranges *relative to the returned `Vec<Line>`*
/// (end exclusive). The caller offsets these by the entry's start in
/// the full buffer. Only User/Assistant/Thinking bodies pass through
/// the markdown renderer, so only those can carry code blocks.
fn entry_to_lines(
    e: &ChatEntry,
    body_w: usize,
    _cwd: &Path,
) -> (Vec<Line<'static>>, Vec<markdown::CodeSpan>) {
    match &e.kind {
        EntryKind::User => {
            let bg = Color::Indexed(237);
            let (md, blocks) = markdown::render_with_blocks(
                &e.text,
                body_w,
                Style::default().fg(Color::White).bg(bg),
            );
            let lines = build_lines_md(
                "you      ",
                Style::default()
                    .fg(Color::Cyan)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
                md,
            );
            // `highlight_user_lines` bookends the block with one blank
            // spacer row at the top, shifting every code-block index by
            // one. `build_lines_md` is 1:1 (only mutates spans), so no
            // shift before that.
            let lines = highlight_user_lines(lines, body_w + LABEL_W, bg);
            let blocks = shift_blocks(blocks, 1);
            (lines, blocks)
        }
        EntryKind::Assistant => {
            let (md, blocks) = markdown::render_with_blocks(
                &e.text,
                body_w,
                Style::default().fg(Color::White),
            );
            let lines = build_lines_md(
                "claude   ",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                md,
            );
            (lines, blocks)
        }
        EntryKind::Thinking => {
            let (md, blocks) = markdown::render_with_blocks(
                &e.text,
                body_w,
                Style::default().fg(Color::Gray).add_modifier(Modifier::ITALIC),
            );
            let lines = build_lines_md(
                "thinking ",
                Style::default().fg(Color::Magenta),
                md,
            );
            (lines, blocks)
        }
        EntryKind::ToolUse { name, .. } => {
            // Heavy dim — readable marginalia next to user/assistant
            // turns. Full input/diff/output lives in the Actions pane.
            let style = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM);
            (
                vec![Line::from(vec![
                    Span::styled("· ".to_string(), style),
                    Span::styled(name.clone(), style),
                ])],
                Vec::new(),
            )
        }
        EntryKind::ToolResult { ok } => {
            let (text, color) = if *ok {
                ("↳ ok", Color::DarkGray)
            } else {
                ("↳ error", Color::Red)
            };
            let style = Style::default().fg(color).add_modifier(Modifier::DIM);
            (
                vec![Line::from(vec![
                    Span::raw("  "),
                    Span::styled(text.to_string(), style),
                ])],
                Vec::new(),
            )
        }
        EntryKind::System => (
            build_lines(
                "system   ",
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::DarkGray),
                &e.text,
                body_w,
            ),
            Vec::new(),
        ),
    }
}

/// Offset every code-block span by `by` rows. Used when a line-buffer
/// transform (e.g. the user-message highlight's leading spacer) inserts
/// rows ahead of the markdown body.
fn shift_blocks(blocks: Vec<markdown::CodeSpan>, by: usize) -> Vec<markdown::CodeSpan> {
    blocks
        .into_iter()
        .map(|mut b| {
            b.start += by;
            b.end += by;
            b
        })
        .collect()
}

/// Like `build_lines`, but the body is a pre-rendered markdown block
/// (vec of styled lines). The first line gets the label prefix; the
/// rest are left-padded by `LABEL_W` spaces to align with it.
/// Wrap user-message lines in a light background highlight, padding
/// each line out to `row_w` so the bg color fills the full chat row,
/// and bookending the block with blank highlighted spacer lines.
fn highlight_user_lines(
    lines: Vec<Line<'static>>,
    row_w: usize,
    bg: Color,
) -> Vec<Line<'static>> {
    let pad_style = Style::default().bg(bg);
    let blank = || Line::from(Span::styled(" ".repeat(row_w), pad_style));
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len() + 2);
    out.push(blank());
    for line in lines {
        let used: usize = line
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        let mut spans: Vec<Span<'static>> = line
            .spans
            .into_iter()
            .map(|mut s| {
                if s.style.bg.is_none() {
                    s.style = s.style.bg(bg);
                }
                s
            })
            .collect();
        if used < row_w {
            spans.push(Span::styled(" ".repeat(row_w - used), pad_style));
        }
        out.push(Line::from(spans));
    }
    out.push(blank());
    out
}

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

/// Best-effort current branch for a working directory. Walks up from
/// `cwd` looking for `.git`; reads `HEAD` directly so we don't shell
/// out per render. Returns the branch name for a normal HEAD, a short
/// commit hash for a detached HEAD, or `None` if the directory isn't
/// inside a git repo (or `.git` looks malformed).
fn git_branch(cwd: &Path) -> Option<String> {
    let mut dir: &Path = cwd;
    loop {
        let dot_git = dir.join(".git");
        let head_path: Option<PathBuf> = match std::fs::metadata(&dot_git) {
            Ok(m) if m.is_dir() => Some(dot_git.join("HEAD")),
            Ok(_) => std::fs::read_to_string(&dot_git)
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find_map(|l| l.strip_prefix("gitdir: ").map(|p| PathBuf::from(p.trim()).join("HEAD")))
                }),
            Err(_) => None,
        };
        if let Some(p) = head_path {
            if let Ok(contents) = std::fs::read_to_string(&p) {
                let trimmed = contents.trim();
                if let Some(rest) = trimmed.strip_prefix("ref: refs/heads/") {
                    return Some(rest.to_string());
                }
                return Some(trimmed.chars().take(7).collect());
            }
        }
        dir = dir.parent()?;
    }
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
    use super::{models_match, trim_model};

    #[test]
    fn trim_model_drops_vendor_prefix_and_date_stamp() {
        assert_eq!(trim_model("claude-sonnet-4-6"), "sonnet-4-6");
        assert_eq!(trim_model("claude-haiku-4-5-20251001"), "haiku-4-5");
        assert_eq!(trim_model("opus"), "opus");
        assert_eq!(trim_model("default"), "default");
        // Only an 8-digit trailing segment is a date stamp — version
        // segments stay.
        assert_eq!(trim_model("claude-fable-5"), "fable-5");
        // Opus 4.8 is natively 1M, so the tier suffix is dropped entirely;
        // a 200K family on the opt-in tier keeps the capitalized badge —
        // with the date stamp gone in both cases.
        assert_eq!(trim_model("claude-opus-4-8[1m]"), "opus-4-8");
        assert_eq!(trim_model("claude-sonnet-4-6[1m]"), "sonnet-4-6[1M]");
        assert_eq!(trim_model("claude-haiku-4-5-20251001[1m]"), "haiku-4-5[1M]");
    }

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

    // --- Detail-pane click-to-copy command parts ---
    //
    // The Detail pane makes each Bash command segment individually
    // copyable; these lock the `(start, end, source)` ranges the click
    // handler maps a click to. `source` must be the bare, runnable part
    // (no leading separator); the range must cover every rendered line
    // of the part (including wrapped continuations) and stay in bounds.

    #[test]
    fn bash_parts_split_at_separators() {
        let mut out = Vec::new();
        // Wide width → no wrapping, so one line per segment.
        let parts = super::push_bash_command(&mut out, "cd /foo && npm test | tee log", 200);
        let sources: Vec<&str> = parts.iter().map(|(_, _, s)| s.as_str()).collect();
        assert_eq!(sources, vec!["cd /foo", "npm test", "tee log"]);
        // Ranges are contiguous, non-empty, and within the rendered buffer.
        assert_eq!(parts.first().map(|p| p.0), Some(0));
        for (start, end, _) in &parts {
            assert!(end > start, "part range must be non-empty");
            assert!(*end <= out.len(), "part range must stay in bounds");
        }
    }

    #[test]
    fn bash_single_command_is_one_part() {
        let mut out = Vec::new();
        let parts = super::push_bash_command(&mut out, "git status", 200);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].2, "git status");
        assert_eq!(parts[0].0, 0);
        assert_eq!(parts[0].1, out.len());
    }

    #[test]
    fn bash_part_range_spans_wrapped_lines() {
        let mut out = Vec::new();
        // Narrow width forces this single (separator-free) command to
        // wrap across several lines; the part must span all of them and
        // still copy the full unwrapped text.
        let cmd = "echo aaaa bbbb cccc dddd";
        let parts = super::push_bash_command(&mut out, cmd, 10);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].2, cmd);
        assert!(parts[0].1 - parts[0].0 > 1, "wrapped part should span >1 line");
        assert_eq!(parts[0].1, out.len());
    }
}
