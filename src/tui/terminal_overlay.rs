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

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use vt100::Screen;

/// Markers that strongly suggest claude has popped a TUI overlay and is
/// waiting for input. Pattern-based and intentionally easy to extend —
/// false positives are recoverable with Ctrl-].
const PROMPT_MARKERS: &[&str] = &[
    "❯",          // claude's selection cursor in pickers
    "[y/N]",
    "[Y/n]",
    "(y/n)",
    "(Y/n)",
    "Continue?",
    "Press y ",
    "Use this ",  // accept-edit prompt
];

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

/// Render `screen` as an overlay inside `area`, wiping whatever was
/// drawn there first. Clips rows/cols that exceed the area; the PTY is
/// fixed at 40×120 today.
pub fn render(frame: &mut Frame, area: Rect, screen: &Screen) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" claude is asking — Ctrl-] to dismiss ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (rows, cols) = screen.size();
    let max_rows = inner.height.min(rows);
    let max_cols = inner.width.min(cols);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(max_rows as usize);
    for row in 0..max_rows {
        lines.push(row_to_line(screen, row, max_cols));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn row_to_line(screen: &Screen, row: u16, max_cols: u16) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur_style = Style::default();
    let mut started = false;
    let mut col = 0u16;
    while col < max_cols {
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
        // Skip continuation column of wide chars so we don't double-render.
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
    fn detects_picker_arrow() {
        let p = parse("\x1b[2J❯ Option 1\r\n  Option 2".as_bytes());
        assert!(prompt_visible(p.screen(), false));
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
}
