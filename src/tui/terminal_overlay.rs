//! Render claude's PTY screen as a ratatui overlay when an interactive
//! prompt (model-switch confirmation, multiselect, accept-edit y/N) is
//! up and would otherwise be invisible inside mewxi's JSONL-driven
//! session view.
//!
//! The PTY bytes are already being parsed into a `vt100::Screen` over in
//! [`crate::agent_control::PtySession`]; this module is presentation +
//! detection only. The event-loop side in `tui/mod.rs` decides when to
//! open the overlay (via [`prompt_visible`]) and routes keystrokes
//! straight to the PTY while it's open.
//!
//! The overlay shows only the **popup region** of claude's 40×120 grid,
//! not the whole TUI — mewxi remains the primary view. We locate the
//! popup by finding the prompt-marker row, then expanding up/down until
//! we hit a box-corner or blank row.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use vt100::Screen;

/// Markers that strongly suggest claude has popped a TUI overlay and is
/// waiting for input. Pattern-based and intentionally easy to extend —
/// false positives are recoverable with Ctrl-]. Anything added here
/// **must not** appear in claude's normal chat input box, otherwise the
/// overlay triggers on every keystroke. Notably `❯` is *not* included:
/// claude uses it as its input-prompt char, so it matches always.
/// Picker UIs (where `❯` is the option cursor) need a stricter
/// row-context check — to be added when a real picker case shows up.
const PROMPT_MARKERS: &[&str] = &[
    "[y/N]",
    "[Y/n]",
    "(y/n)",
    "(Y/n)",
    "Continue?",
    "Press y ",
    "Use this ",  // accept-edit prompt
];

/// Max overlay height in PTY rows. The widest popup we've seen is the
/// accept-edit preview, which is ~15 rows; cap at 20 so a runaway match
/// can't blow up over mewxi's view.
const MAX_POPUP_ROWS: u16 = 20;

/// True when claude is likely waiting for the user to answer a TUI
/// overlay. `awaiting_marker` short-circuits to true and lets the
/// existing permission-prompt signal trigger the overlay even if the
/// screen-content heuristic would miss the prompt text.
pub fn prompt_visible(screen: &Screen, awaiting_marker: bool) -> bool {
    if awaiting_marker {
        return true;
    }
    let contents = screen.contents();
    PROMPT_MARKERS.iter().any(|m| contents.contains(m))
}

/// Render the popup region of `screen` as a small overlay inside
/// `area`. The overlay is sized to the popup's content (capped) and
/// positioned at the bottom of `area` so it reads as a modal sitting
/// on top of mewxi's chat-log view rather than a full takeover.
///
/// If we can't locate a popup region, falls back to the bottom-most
/// non-blank rows of the screen (still tightly cropped).
pub fn render(frame: &mut Frame, area: Rect, screen: &Screen) {
    let region = find_popup_region(screen).unwrap_or_else(|| fallback_region(screen));
    let (top, bot, left, right) = region;
    let popup_rows = bot - top + 1;
    let popup_cols = right - left + 1;

    // Frame + 1-col horizontal padding so the popup doesn't touch the
    // border. +2 rows for top/bottom border, +2 cols for left/right
    // border + the 1-col pad on each side.
    let h = (popup_rows + 2).min(area.height);
    let w = (popup_cols + 4).min(area.width);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h + 1); // 1-row gap to footer
    let outer = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, outer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" claude is asking — Ctrl-] to dismiss ");
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let max_rows = inner.height.min(popup_rows);
    let max_cols = inner.width.min(popup_cols);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(max_rows as usize);
    for r in 0..max_rows {
        lines.push(row_to_line(screen, top + r, left, max_cols));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Locate the popup's bounding box on the screen. Returns
/// `(row_top, row_bot, col_left, col_right)` inclusive. None when no
/// prompt marker is present (caller falls back to a generic region).
fn find_popup_region(screen: &Screen) -> Option<(u16, u16, u16, u16)> {
    let (rows, cols) = screen.size();
    // Prefer the bottom-most marker — claude anchors popups near the
    // input box, so the lowest match is the one the user is waiting on.
    let marker_row = (0..rows).rev().find(|r| {
        let txt = row_text(screen, *r, cols);
        PROMPT_MARKERS.iter().any(|m| txt.contains(m))
    })?;

    let mut top = marker_row;
    while top > 0 {
        let prev = top - 1;
        let txt = row_text(screen, prev, cols);
        if txt.trim().is_empty() {
            break;
        }
        top = prev;
        // Stop *after* including a row that contains a box top-corner —
        // that's the popup's upper border.
        if txt.chars().any(is_box_top_corner) {
            break;
        }
    }
    let mut bot = marker_row;
    while bot + 1 < rows {
        let next = bot + 1;
        let txt = row_text(screen, next, cols);
        if txt.trim().is_empty() {
            break;
        }
        bot = next;
        if txt.chars().any(is_box_bottom_corner) {
            break;
        }
    }
    // Cap height: anchor on marker_row so the user sees what they're
    // answering, not a long preamble that got pulled in.
    if bot - top + 1 > MAX_POPUP_ROWS {
        let half = MAX_POPUP_ROWS.saturating_sub(4);
        top = marker_row.saturating_sub(half);
        bot = (top + MAX_POPUP_ROWS - 1).min(rows - 1);
    }

    let (left, right) = column_bounds(screen, top, bot, cols)?;
    Some((top, bot, left, right))
}

/// Fallback: bottom-most contiguous block of non-blank rows. Used when
/// the awaiting marker fires but no in-screen prompt text is visible
/// (e.g. claude's still painting), so something rendered is better than
/// a blank box.
fn fallback_region(screen: &Screen) -> (u16, u16, u16, u16) {
    let (rows, cols) = screen.size();
    let mut bot = rows.saturating_sub(1);
    while bot > 0 && row_text(screen, bot, cols).trim().is_empty() {
        bot -= 1;
    }
    let mut top = bot;
    let mut count: u16 = 1;
    while top > 0 && count < MAX_POPUP_ROWS {
        let prev = top - 1;
        if row_text(screen, prev, cols).trim().is_empty() {
            break;
        }
        top = prev;
        count += 1;
    }
    let (left, right) = column_bounds(screen, top, bot, cols).unwrap_or((0, cols - 1));
    (top, bot, left, right)
}

fn column_bounds(screen: &Screen, top: u16, bot: u16, cols: u16) -> Option<(u16, u16)> {
    let mut left = cols;
    let mut right = 0u16;
    let mut any = false;
    for r in top..=bot {
        for c in 0..cols {
            let has = screen
                .cell(r, c)
                .map(|x| x.has_contents() && !x.contents().chars().all(char::is_whitespace))
                .unwrap_or(false);
            if has {
                if c < left {
                    left = c;
                }
                if c > right {
                    right = c;
                }
                any = true;
            }
        }
    }
    if !any {
        None
    } else {
        Some((left, right))
    }
}

fn row_text(screen: &Screen, row: u16, cols: u16) -> String {
    let mut s = String::with_capacity(cols as usize);
    for c in 0..cols {
        match screen.cell(row, c) {
            Some(cell) if cell.has_contents() => s.push_str(cell.contents()),
            _ => s.push(' '),
        }
    }
    s
}

fn is_box_top_corner(c: char) -> bool {
    matches!(c, '╭' | '┌' | '╔' | '┏' | '┎' | '┍')
}

fn is_box_bottom_corner(c: char) -> bool {
    matches!(c, '╰' | '└' | '╚' | '┗' | '┖' | '┕')
}

fn row_to_line(screen: &Screen, row: u16, col_start: u16, width: u16) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur_style = Style::default();
    let mut started = false;
    let end = col_start.saturating_add(width);
    let mut col = col_start;
    while col < end {
        let cell = screen.cell(row, col);
        let (text, style) = match cell {
            Some(c) if c.has_contents() => (c.contents().to_string(), cell_style(c)),
            _ => (" ".to_string(), Style::default()),
        };
        if !started {
            cur_style = style;
            started = true;
        }
        if style == cur_style {
            buf.push_str(&text);
        } else {
            spans.push(Span::styled(std::mem::take(&mut buf), cur_style));
            buf.push_str(&text);
            cur_style = style;
        }
        let advance = cell.map(|c| if c.is_wide() { 2 } else { 1 }).unwrap_or(1);
        col = col.saturating_add(advance);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur_style));
    }
    Line::from(spans)
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut s = Style::default()
        .fg(map_color(cell.fgcolor()))
        .bg(map_color(cell.bgcolor()));
    let mut m = Modifier::empty();
    if cell.bold() {
        m |= Modifier::BOLD;
    }
    if cell.dim() {
        m |= Modifier::DIM;
    }
    if cell.italic() {
        m |= Modifier::ITALIC;
    }
    if cell.underline() {
        m |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        m |= Modifier::REVERSED;
    }
    if !m.is_empty() {
        s = s.add_modifier(m);
    }
    s
}

fn map_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_control::{PTY_COLS, PTY_ROWS};

    fn parse(bytes: &[u8]) -> vt100::Parser {
        let mut p = vt100::Parser::new(PTY_ROWS, PTY_COLS, 0);
        p.process(bytes);
        p
    }

    #[test]
    fn detects_y_slash_n_prompt() {
        let p = parse(b"Edit foo.rs? [y/N]");
        assert!(prompt_visible(p.screen(), false));
    }

    #[test]
    fn ignores_chevron_in_input_box() {
        // claude's input prompt is `❯ user_text`. The overlay must NOT
        // open on this — it would trigger on every keystroke.
        let p = parse("\x1b[2J❯ hello world".as_bytes());
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn ignores_plain_chat_screen() {
        let p = parse(b"hello world\r\n> ");
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn awaiting_marker_short_circuits() {
        let p = parse(b"");
        assert!(prompt_visible(p.screen(), true));
    }

    #[test]
    fn detects_continue_prompt() {
        let p = parse(b"Continue conversation? Continue?");
        assert!(prompt_visible(p.screen(), false));
    }

    #[test]
    fn popup_region_crops_tightly_to_box() {
        // Simulated screen: lots of chat above, then a small popup box,
        // then a blank input line. We want the region to bound the box.
        let mut bytes = String::new();
        for i in 0..15 {
            bytes.push_str(&format!("chat line {i}\r\n"));
        }
        bytes.push_str("╭──────────────╮\r\n");
        bytes.push_str("│ Continue?    │\r\n");
        bytes.push_str("│ [y/N]        │\r\n");
        bytes.push_str("╰──────────────╯\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("> input box\r\n");
        let p = parse(bytes.as_bytes());
        let (top, bot, left, right) = find_popup_region(p.screen()).expect("popup found");
        // Should cover 4 rows (╭, Continue?, [y/N], ╰).
        assert_eq!(bot - top + 1, 4, "rows {top}..={bot}");
        // Width = 16 cols of the box.
        assert_eq!(right - left + 1, 16, "cols {left}..={right}");
    }

    #[test]
    fn popup_region_handles_no_box_around_marker() {
        // Marker without a surrounding box — region should still pick
        // up the marker row and tightly clip horizontally.
        let p = parse(b"Continue?  ");
        let (top, bot, left, right) = find_popup_region(p.screen()).expect("popup found");
        assert_eq!(top, bot);
        // "Continue?" is 9 chars wide.
        assert_eq!(right - left + 1, 9);
    }

    #[test]
    fn popup_region_caps_height_at_max() {
        // Build a screen where everything is non-blank box content;
        // ensure we cap at MAX_POPUP_ROWS.
        let mut bytes = String::new();
        bytes.push_str("╭──────────╮\r\n");
        for _ in 0..40 {
            bytes.push_str("│ x        │\r\n");
        }
        bytes.push_str("│ [y/N]    │\r\n");
        let p = parse(bytes.as_bytes());
        let (top, bot, _, _) = find_popup_region(p.screen()).expect("popup found");
        assert!(bot - top + 1 <= MAX_POPUP_ROWS, "rows {top}..={bot}");
    }
}
