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

/// Substring markers that strongly suggest claude has popped a TUI
/// overlay (y/N prompts, accept-edit, continue dialog). Pattern-based
/// and intentionally easy to extend — false positives are recoverable
/// with Ctrl-]. Anything added here **must not** appear in claude's
/// normal chat input box, otherwise the overlay triggers on every
/// keystroke. Picker UIs (numbered options with a cursor) are detected
/// structurally by [`is_picker_option_row`] — never name a cursor char
/// here, claude uses one of several and reusing the input-prompt
/// glyph would re-introduce the every-keystroke false positive.
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
    find_marker_row(screen).is_some()
}

/// True when `row` looks like one entry in a numbered picker. We
/// deliberately match by structure (a digit-period anchor) rather than
/// by cursor character, so we don't conflate the input box's prompt
/// glyph with picker cursors. Two shapes count:
///   - unselected: `   1. Option`           (whitespace, then digit + `.`)
///   - selected:   `<cursor> 1. Option`     (one non-digit char + space + digit + `.`)
///
/// Pair with a nearby-sibling check before treating any single row as
/// a picker — a stray "1. foo" in chat shouldn't trigger.
fn is_picker_option_row(screen: &Screen, row: u16, cols: u16) -> bool {
    let line = row_text(screen, row, cols);
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let sep = if first.is_ascii_digit() {
        chars.next()
    } else {
        if chars.next() != Some(' ') {
            return false;
        }
        let Some(d) = chars.next() else {
            return false;
        };
        if !d.is_ascii_digit() {
            return false;
        }
        chars.next()
    };
    matches!(sep, Some('.') | Some(')'))
}

/// Find the bottom-most row that looks like a prompt or picker. Used by
/// both [`prompt_visible`] and [`find_popup_region`] so the trigger and
/// the crop anchor agree.
fn find_marker_row(screen: &Screen) -> Option<u16> {
    let (rows, cols) = screen.size();
    for r in (0..rows).rev() {
        let txt = row_text(screen, r, cols);
        if PROMPT_MARKERS.iter().any(|m| txt.contains(m)) {
            return Some(r);
        }
        if is_picker_option_row(screen, r, cols) {
            // Confirm by a sibling option row within ±4 rows so a stray
            // "1. foo" in chat doesn't trigger.
            for delta in [-4i32, -3, -2, -1, 1, 2, 3, 4] {
                let r2 = r as i32 + delta;
                if r2 < 0 || r2 >= rows as i32 {
                    continue;
                }
                if r2 as u16 != r && is_picker_option_row(screen, r2 as u16, cols) {
                    return Some(r);
                }
            }
        }
    }
    None
}

fn is_horizontal_separator(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 4 && t.chars().all(|c| matches!(c, '─' | '━' | '-' | '═'))
}

/// Render the popup the way mewxi prefers: if the prompt is a numbered
/// picker (Switch model? / plan-mode pick / etc.) we parse its
/// structure and re-render it as a native mewxi modal so it matches
/// the rest of the chrome. Otherwise (y/N, accept-edit with diff,
/// anything we can't structurally parse) we fall back to cropping
/// claude's rendered PTY screen and surfacing it verbatim.
///
/// Keystrokes always pass through to the PTY regardless of render
/// path; for the native picker the user's ↑↓ move claude's real
/// cursor, we re-parse next frame, and the modal stays in sync.
pub fn render(frame: &mut Frame, area: Rect, screen: &Screen) {
    if let Some(picker) = parse_picker(screen) {
        render_native_picker(frame, area, &picker);
    } else {
        render_pty_crop(frame, area, screen);
    }
}

fn render_pty_crop(frame: &mut Frame, area: Rect, screen: &Screen) {
    let region = find_popup_region(screen).unwrap_or_else(|| fallback_region(screen));
    let (top, bot, left, right) = region;
    let popup_rows = bot - top + 1;
    let popup_cols = right - left + 1;

    let h = (popup_rows + 2).min(area.height);
    let w = (popup_cols + 4).min(area.width);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h + 1);
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

fn render_native_picker(frame: &mut Frame, area: Rect, picker: &PickerContent) {
    let title_text = picker
        .title
        .clone()
        .unwrap_or_else(|| "claude is asking".to_string());
    let title = format!(
        " {} — ↑↓ navigate · Enter confirm · Ctrl-] cancel ",
        title_text
    );

    // 1-row padding top + bottom inside the borders, blank row between
    // body and options (if any body), and blank row above the hint.
    let blank_between_body_and_options: usize = if picker.body.is_empty() { 0 } else { 1 };
    let inner_h = picker.body.len() + blank_between_body_and_options + picker.options.len() + 2;
    let inner_w = std::iter::once(title.chars().count())
        .chain(picker.body.iter().map(|s| s.chars().count()))
        .chain(picker.options.iter().map(|o| o.chars().count() + 4))
        .max()
        .unwrap_or(40)
        + 2;

    let h = (inner_h as u16 + 2).min(area.height);
    let w = (inner_w as u16 + 2).min(area.width);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let outer = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, outer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner_h);
    lines.push(Line::raw("")); // top padding
    for line in &picker.body {
        lines.push(Line::from(Span::styled(
            format!(" {}", line),
            Style::default().fg(Color::Gray),
        )));
    }
    if !picker.body.is_empty() {
        lines.push(Line::raw(""));
    }
    for (i, opt) in picker.options.iter().enumerate() {
        let selected = i == picker.selected;
        let (cursor, style) = if selected {
            (
                " ▶ ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("   ", Style::default().fg(Color::White))
        };
        lines.push(Line::from(vec![
            Span::styled(cursor, style),
            Span::styled(opt.clone(), style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Structured view of a numbered picker pulled off the PTY screen.
/// Used to re-render the prompt as a native mewxi modal.
struct PickerContent {
    /// First non-blank header line (e.g. "Switch model?"). Goes into
    /// the modal's title bar.
    title: Option<String>,
    /// Remaining header lines (description, context paragraphs).
    body: Vec<String>,
    /// Option text with the "N. " prefix and cursor stripped.
    options: Vec<String>,
    /// 0-based index of the option claude has its cursor on.
    selected: usize,
}

/// Parse a numbered picker from `screen` if one is up. Returns None
/// for y/N prompts, accept-edit dialogs, and anything else that isn't
/// shaped like `<header> ... <cursor> 1. opt / 2. opt / ...`.
fn parse_picker(screen: &Screen) -> Option<PickerContent> {
    let (top, bot, _left, _right) = find_popup_region(screen)?;
    let (_, cols) = screen.size();
    let lines: Vec<String> = (top..=bot).map(|r| row_text(screen, r, cols)).collect();

    let option_idx: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| if is_picker_option_line(line) { Some(i) } else { None })
        .collect();
    if option_idx.len() < 2 {
        return None;
    }
    let first_opt = option_idx[0];

    let header: Vec<String> = lines[..first_opt]
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let (title, body) = match header.split_first() {
        Some((t, rest)) => (Some(t.clone()), rest.to_vec()),
        None => (None, Vec::new()),
    };

    let mut options: Vec<String> = Vec::new();
    let mut selected: usize = 0;
    for (slot, &row_in_region) in option_idx.iter().enumerate() {
        let (is_selected, text) = parse_option_line(&lines[row_in_region])?;
        options.push(text);
        if is_selected {
            selected = slot;
        }
    }

    Some(PickerContent {
        title,
        body,
        options,
        selected,
    })
}

/// String-side mirror of [`is_picker_option_row`] — works on a row's
/// extracted text so parsing doesn't have to re-walk the vt100 cells.
fn is_picker_option_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let sep = if first.is_ascii_digit() {
        // consume any extra digits (handle "10.", "11.", etc.)
        let mut peek = chars.clone();
        while peek.clone().next().is_some_and(|c| c.is_ascii_digit()) {
            peek.next();
        }
        peek.next()
    } else {
        if chars.next() != Some(' ') {
            return false;
        }
        let Some(d) = chars.next() else {
            return false;
        };
        if !d.is_ascii_digit() {
            return false;
        }
        let mut peek = chars.clone();
        while peek.clone().next().is_some_and(|c| c.is_ascii_digit()) {
            peek.next();
        }
        peek.next()
    };
    matches!(sep, Some('.') | Some(')'))
}

/// Pull the option text out of a picker row. Returns `(is_selected,
/// text)` with the cursor + number + separator + leading space stripped.
fn parse_option_line(line: &str) -> Option<(bool, String)> {
    let trimmed = line.trim_start();
    let chars: Vec<char> = trimmed.chars().collect();
    let (is_selected, mut idx) = if chars.first()?.is_ascii_digit() {
        (false, 0usize)
    } else {
        if chars.get(1)? != &' ' {
            return None;
        }
        if !chars.get(2)?.is_ascii_digit() {
            return None;
        }
        (true, 2usize)
    };
    while chars.get(idx).is_some_and(|c| c.is_ascii_digit()) {
        idx += 1;
    }
    if !matches!(chars.get(idx)?, '.' | ')') {
        return None;
    }
    idx += 1;
    if chars.get(idx) == Some(&' ') {
        idx += 1;
    }
    let text: String = chars[idx..].iter().collect();
    Some((is_selected, text.trim_end().to_string()))
}

/// Locate the popup's bounding box on the screen. Returns
/// `(row_top, row_bot, col_left, col_right)` inclusive. None when no
/// prompt marker is present (caller falls back to a generic region).
///
/// Expansion tolerates a single blank row so we keep the header +
/// description of un-boxed pickers (e.g. claude's "Switch model?"
/// dialog separates header from options with a blank line). Stops at
/// 2 consecutive blanks, a horizontal separator, a box corner, or the
/// height cap.
fn find_popup_region(screen: &Screen) -> Option<(u16, u16, u16, u16)> {
    let (rows, cols) = screen.size();
    let marker_row = find_marker_row(screen)?;

    let mut top = marker_row;
    let mut blanks_up = 0u16;
    while top > 0 && marker_row.saturating_sub(top) < MAX_POPUP_ROWS {
        let prev = top - 1;
        let txt = row_text(screen, prev, cols);
        if is_horizontal_separator(&txt) {
            break;
        }
        if txt.trim().is_empty() {
            blanks_up += 1;
            if blanks_up >= 2 {
                break;
            }
        } else {
            blanks_up = 0;
        }
        top = prev;
        if txt.chars().any(is_box_top_corner) {
            break;
        }
    }
    // Trim leading blank rows.
    while top < marker_row && row_text(screen, top, cols).trim().is_empty() {
        top += 1;
    }

    let mut bot = marker_row;
    let mut blanks_dn = 0u16;
    while bot + 1 < rows && bot.saturating_sub(marker_row) < MAX_POPUP_ROWS {
        let next = bot + 1;
        let txt = row_text(screen, next, cols);
        if is_horizontal_separator(&txt) {
            break;
        }
        // Stop *before* the next UI element's top border (input box).
        if txt.chars().any(is_box_top_corner) && !txt.chars().any(is_box_bottom_corner) {
            break;
        }
        if txt.trim().is_empty() {
            blanks_dn += 1;
            if blanks_dn >= 2 {
                break;
            }
        } else {
            blanks_dn = 0;
        }
        bot = next;
        if txt.chars().any(is_box_bottom_corner) {
            break;
        }
    }
    while bot > marker_row && row_text(screen, bot, cols).trim().is_empty() {
        bot -= 1;
    }

    // Hard cap height: anchor on marker so the user sees the prompt.
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
    fn detects_numbered_picker_with_chevron_cursor() {
        // The model-switch picker: a question + numbered options with
        // a chevron on the selected row.
        let p = parse("\x1b[2JSwitch model?\r\n\r\n❯ 1. Yes, switch\r\n  2. No, go back".as_bytes());
        assert!(prompt_visible(p.screen(), false));
    }

    #[test]
    fn detects_numbered_picker_with_alternate_cursor() {
        // Some terminal fonts render the cursor as `›` instead of `❯`.
        let p = parse("\x1b[2J› 1. Yes\r\n  2. No".as_bytes());
        assert!(prompt_visible(p.screen(), false));
    }

    #[test]
    fn ignores_lone_numbered_line_without_sibling() {
        // A stray "1. foo" in chat output should NOT trigger — pickers
        // always have at least 2 sibling option rows.
        let p = parse("\x1b[2JHere's an item: 1. apple\r\n\r\nnext paragraph".as_bytes());
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn picker_parser_extracts_model_switch_shape() {
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JSwitch model?\r\n");
        bytes.push_str("Your next response will be slower\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("This conversation is cached.\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("❯ 1. Yes, switch to Haiku 4.5\r\n");
        bytes.push_str("  2. No, go back\r\n");
        let p = parse(bytes.as_bytes());
        let picker = parse_picker(p.screen()).expect("picker parsed");
        assert_eq!(picker.title.as_deref(), Some("Switch model?"));
        assert_eq!(picker.options.len(), 2);
        assert_eq!(picker.options[0], "Yes, switch to Haiku 4.5");
        assert_eq!(picker.options[1], "No, go back");
        assert_eq!(picker.selected, 0);
        // Body should at minimum mention the description content.
        assert!(picker.body.iter().any(|b| b.contains("slower")));
    }

    #[test]
    fn picker_parser_tracks_selection_on_second_option() {
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JPick one?\r\n");
        bytes.push_str("  1. Apple\r\n");
        bytes.push_str("› 2. Banana\r\n");
        let p = parse(bytes.as_bytes());
        let picker = parse_picker(p.screen()).expect("picker parsed");
        assert_eq!(picker.selected, 1);
        assert_eq!(picker.options, vec!["Apple", "Banana"]);
    }

    #[test]
    fn picker_parser_returns_none_for_y_n_prompt() {
        // y/N prompts have specific text markers but no numbered
        // options — they should fall through to the cropped vt100
        // render, not the native picker.
        let p = parse(b"\x1b[2JEdit foo.rs? [y/N]");
        assert!(parse_picker(p.screen()).is_none());
    }

    #[test]
    fn picker_parser_handles_multi_digit_numbers() {
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JChoose?\r\n");
        for n in 1..=12 {
            let cursor = if n == 11 { "❯" } else { " " };
            bytes.push_str(&format!("{} {}. Option {}\r\n", cursor, n, n));
        }
        let p = parse(bytes.as_bytes());
        let picker = parse_picker(p.screen()).expect("picker parsed");
        assert_eq!(picker.options.len(), 12);
        assert_eq!(picker.selected, 10); // 0-indexed
        assert_eq!(picker.options[10], "Option 11");
    }

    #[test]
    fn picker_region_includes_header_across_blank_row() {
        // Header + blank + chevron+digit options — region should span
        // header to the last option.
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JSwitch model?\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("❯ 1. Yes\r\n");
        bytes.push_str("  2. No\r\n");
        let p = parse(bytes.as_bytes());
        let (top, bot, _, _) = find_popup_region(p.screen()).expect("picker found");
        let txt: Vec<String> = (top..=bot).map(|r| row_text(p.screen(), r, p.screen().size().1)).collect();
        assert!(txt.iter().any(|l| l.contains("Switch model?")), "expected header in region: {:?}", txt);
        assert!(txt.iter().any(|l| l.contains("❯ 1. Yes")), "expected option in region: {:?}", txt);
        assert!(txt.iter().any(|l| l.contains("2. No")), "expected sibling option in region: {:?}", txt);
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
