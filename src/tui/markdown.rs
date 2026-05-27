//! Render a markdown blob (assistant / user message body) into a flat
//! `Vec<Line<'static>>` ready to drop into a ratatui `Paragraph`.
//!
//! We walk `pulldown-cmark` events, maintain a tiny inline-style stack
//! (bold/italic/strike/underline) and a block-prefix stack (blockquote
//! gutters, list indents + markers), and word-wrap text to the given
//! column width. Code blocks are preserved verbatim and truncated
//! (rather than wrapped) so they stay readable.
//!
//! This is intentionally a narrow renderer: just what assistant turns
//! actually use in practice. GFM tables are rendered as box-drawn
//! grids; footnotes pass through as best we can.
//!
//! ASCII glyphs are avoided for prefix decoration — we use box-drawing
//! characters (▏ for blockquote gutter, ┃ for code-block gutter, • for
//! bullets) because they read as quiet structural marks instead of
//! syntax.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Render `text` (markdown source) to ratatui lines, wrapping inline
/// content to `width` columns. `base_style` applies to plain prose;
/// inline emphasis and block constructs layer on top of it.
pub fn render(text: &str, width: usize, base_style: Style) -> Vec<Line<'static>> {
    if width == 0 || text.trim().is_empty() {
        return vec![Line::from(Span::styled(text.to_string(), base_style))];
    }
    let mut r = Renderer::new(width, base_style);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    for ev in Parser::new_ext(text, opts) {
        r.event(ev);
    }
    r.finish()
}

struct PrefixFrame {
    /// Applied at the start of every new line inside this block.
    lead: String,
    /// One-shot marker applied to the next new line only (e.g. the
    /// `• ` or `1. ` of a list item). Cleared after first use.
    first: Option<String>,
}

struct Renderer {
    out: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    cur_w: usize,
    width: usize,
    base: Style,

    // Inline emphasis depth counters — we add the modifier whenever
    // any depth is non-zero so nested `**_bold italic_**` works.
    bold: u32,
    italic: u32,
    strike: u32,
    underline: u32,
    heading: u32,

    prefix_stack: Vec<PrefixFrame>,
    list_stack: Vec<Option<u64>>, // None = bullet, Some(n) = next ordered counter
    code_block: Option<String>,
    code_buf: String,

    body_started: bool,

    // Table state: while inside a table we capture cell text into
    // `table_cell_buf`, then push completed rows. On End(Table) we
    // render the whole grid in one shot.
    in_table: bool,
    in_table_cell: bool,
    table_in_head: bool,
    table_alignments: Vec<Alignment>,
    table_head: Vec<String>,
    table_rows: Vec<Vec<String>>,
    table_current_row: Vec<String>,
    table_cell_buf: String,
}

impl Renderer {
    fn new(width: usize, base: Style) -> Self {
        Self {
            out: Vec::new(),
            current: Vec::new(),
            cur_w: 0,
            width,
            base,
            bold: 0,
            italic: 0,
            strike: 0,
            underline: 0,
            heading: 0,
            prefix_stack: Vec::new(),
            list_stack: Vec::new(),
            code_block: None,
            code_buf: String::new(),
            body_started: false,
            in_table: false,
            in_table_cell: false,
            table_in_head: false,
            table_alignments: Vec::new(),
            table_head: Vec::new(),
            table_rows: Vec::new(),
            table_current_row: Vec::new(),
            table_cell_buf: String::new(),
        }
    }

    fn inline_style(&self) -> Style {
        let mut s = if self.heading > 0 {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            self.base
        };
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.underline > 0 {
            s = s.add_modifier(Modifier::UNDERLINED);
        }
        s
    }

    fn prefix_width(&self) -> usize {
        self.prefix_stack
            .iter()
            .map(|f| f.lead.chars().count())
            .sum()
    }

    fn ensure_prefix(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        if self.prefix_stack.is_empty() {
            return;
        }
        // Build the per-line prefix by concatenating all frame leads,
        // then if the topmost frame has a one-shot marker, replace its
        // own lead's slice in the prefix with the marker.
        let mut prefix = String::new();
        let last_idx = self.prefix_stack.len() - 1;
        for (i, frame) in self.prefix_stack.iter_mut().enumerate() {
            if i == last_idx {
                if let Some(first) = frame.first.take() {
                    prefix.push_str(&first);
                } else {
                    prefix.push_str(&frame.lead);
                }
            } else {
                prefix.push_str(&frame.lead);
            }
        }
        if prefix.is_empty() {
            return;
        }
        let plen = prefix.chars().count();
        self.current.push(Span::styled(
            prefix,
            Style::default().fg(Color::DarkGray),
        ));
        self.cur_w = plen;
    }

    fn flush_line(&mut self) {
        if self.current.is_empty() && !self.body_started {
            // Nothing on this line — push a true blank.
            self.out.push(Line::default());
        } else {
            self.out
                .push(Line::from(std::mem::take(&mut self.current)));
        }
        self.cur_w = 0;
        self.body_started = false;
    }

    fn blank_line(&mut self) {
        // Coalesce runs of blanks; never start with a blank.
        let last_blank = self
            .out
            .last()
            .map(|l| l.spans.is_empty())
            .unwrap_or(true);
        if !last_blank {
            self.out.push(Line::default());
        }
    }

    fn push_text(&mut self, text: &str, style: Style) {
        for word in text.split_whitespace() {
            let wlen = word.chars().count();
            let avail_full = self.width.saturating_sub(self.prefix_width());
            if wlen > avail_full && avail_full > 1 {
                // Word is too long for a single line — hard-chunk it.
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(avail_full) {
                    let s: String = chunk.iter().collect();
                    self.push_word(&s, style);
                    if self.cur_w >= self.width {
                        self.flush_line();
                    }
                }
            } else {
                self.push_word(word, style);
            }
        }
    }

    fn push_word(&mut self, word: &str, style: Style) {
        if word.is_empty() {
            return;
        }
        self.ensure_prefix();
        let wlen = word.chars().count();
        let need_space = self.body_started;
        let cost = if need_space { wlen + 1 } else { wlen };
        if self.cur_w + cost > self.width && self.body_started {
            self.flush_line();
            self.ensure_prefix();
            self.current.push(Span::styled(word.to_string(), style));
            self.cur_w += wlen;
        } else {
            if need_space {
                self.current
                    .push(Span::styled(" ".to_string(), self.base));
                self.cur_w += 1;
            }
            self.current.push(Span::styled(word.to_string(), style));
            self.cur_w += wlen;
        }
        self.body_started = true;
    }

    fn event(&mut self, ev: Event) {
        // Table body: capture cell contents, render the grid on End(Table).
        if self.in_table {
            match ev {
                Event::Start(Tag::TableHead) => {
                    self.table_in_head = true;
                    self.table_current_row.clear();
                }
                Event::End(TagEnd::TableHead) => {
                    self.table_head =
                        std::mem::take(&mut self.table_current_row);
                    self.table_in_head = false;
                }
                Event::Start(Tag::TableRow) => {
                    self.table_current_row.clear();
                }
                Event::End(TagEnd::TableRow) => {
                    self.table_rows
                        .push(std::mem::take(&mut self.table_current_row));
                }
                Event::Start(Tag::TableCell) => {
                    self.in_table_cell = true;
                    self.table_cell_buf.clear();
                }
                Event::End(TagEnd::TableCell) => {
                    self.in_table_cell = false;
                    self.table_current_row
                        .push(std::mem::take(&mut self.table_cell_buf));
                }
                Event::End(TagEnd::Table) => {
                    self.in_table = false;
                    let aligns = std::mem::take(&mut self.table_alignments);
                    let head = std::mem::take(&mut self.table_head);
                    let rows = std::mem::take(&mut self.table_rows);
                    self.emit_table(&aligns, &head, &rows);
                }
                Event::Text(s) if self.in_table_cell => {
                    self.table_cell_buf.push_str(&s);
                }
                Event::Code(s) if self.in_table_cell => {
                    self.table_cell_buf.push_str(&s);
                }
                Event::SoftBreak | Event::HardBreak
                    if self.in_table_cell =>
                {
                    self.table_cell_buf.push(' ');
                }
                _ => {}
            }
            return;
        }

        // Code-block body: capture verbatim, emit on End.
        if self.code_block.is_some() {
            match ev {
                Event::Text(s) => self.code_buf.push_str(&s),
                Event::End(TagEnd::CodeBlock) => {
                    let lang = self.code_block.take().unwrap_or_default();
                    let body = std::mem::take(&mut self.code_buf);
                    self.emit_code_block(&lang, &body);
                }
                _ => {}
            }
            return;
        }

        match ev {
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if self.body_started {
                    self.flush_line();
                }
                self.blank_line();
            }

            Event::Start(Tag::Heading { level, .. }) => {
                if self.body_started {
                    self.flush_line();
                }
                self.blank_line();
                self.heading += 1;
                let hashes = "#".repeat(level as usize);
                let style = self.inline_style();
                self.ensure_prefix();
                self.current.push(Span::styled(format!("{hashes} "), style));
                self.cur_w += hashes.chars().count() + 1;
                self.body_started = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                self.heading = self.heading.saturating_sub(1);
                self.flush_line();
                self.blank_line();
            }

            Event::Start(Tag::Emphasis) => self.italic += 1,
            Event::End(TagEnd::Emphasis) => {
                self.italic = self.italic.saturating_sub(1)
            }
            Event::Start(Tag::Strong) => self.bold += 1,
            Event::End(TagEnd::Strong) => self.bold = self.bold.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => self.strike += 1,
            Event::End(TagEnd::Strikethrough) => {
                self.strike = self.strike.saturating_sub(1)
            }
            Event::Start(Tag::Link { .. }) => self.underline += 1,
            Event::End(TagEnd::Link) => {
                self.underline = self.underline.saturating_sub(1)
            }

            Event::Start(Tag::BlockQuote(_)) => {
                if self.body_started {
                    self.flush_line();
                }
                self.prefix_stack.push(PrefixFrame {
                    lead: "▏ ".to_string(),
                    first: None,
                });
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                if self.body_started {
                    self.flush_line();
                }
                self.prefix_stack.pop();
                self.blank_line();
            }

            Event::Start(Tag::List(start)) => {
                if self.body_started {
                    self.flush_line();
                }
                self.list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.blank_line();
                }
            }
            Event::Start(Tag::Item) => {
                if self.body_started {
                    self.flush_line();
                }
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    Some(None) => "• ".to_string(),
                    None => String::new(),
                };
                let mw = marker.chars().count();
                let continuation = format!("{indent}{}", " ".repeat(mw));
                let first = format!("{indent}{marker}");
                self.prefix_stack.push(PrefixFrame {
                    lead: continuation,
                    first: Some(first),
                });
            }
            Event::End(TagEnd::Item) => {
                if self.body_started {
                    self.flush_line();
                }
                self.prefix_stack.pop();
            }

            Event::Start(Tag::Table(aligns)) => {
                if self.body_started {
                    self.flush_line();
                }
                self.blank_line();
                self.in_table = true;
                self.table_alignments = aligns;
                self.table_head.clear();
                self.table_rows.clear();
                self.table_current_row.clear();
                self.table_cell_buf.clear();
            }

            Event::Start(Tag::CodeBlock(kind)) => {
                if self.body_started {
                    self.flush_line();
                }
                let lang = match kind {
                    CodeBlockKind::Fenced(s) => s.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code_block = Some(lang);
                self.code_buf.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                // Defensive: should be handled in the early-return arm.
                self.code_block = None;
                self.code_buf.clear();
            }

            Event::Text(s) => {
                let style = self.inline_style();
                self.push_text(&s, style);
            }
            Event::Code(s) => {
                let style = Style::default().fg(Color::LightYellow);
                self.push_text(&s, style);
            }
            Event::SoftBreak => {
                if self.body_started && self.cur_w < self.width {
                    self.current
                        .push(Span::styled(" ".to_string(), self.base));
                    self.cur_w += 1;
                }
            }
            Event::HardBreak => {
                self.flush_line();
            }
            Event::Rule => {
                if self.body_started {
                    self.flush_line();
                }
                self.blank_line();
                self.out.push(Line::from(Span::styled(
                    "─".repeat(self.width.max(1)),
                    Style::default().fg(Color::DarkGray),
                )));
                self.blank_line();
            }
            Event::TaskListMarker(checked) => {
                let mark = if checked { "[x] " } else { "[ ] " };
                self.ensure_prefix();
                self.current
                    .push(Span::styled(mark.to_string(), self.base));
                self.cur_w += 4;
                self.body_started = true;
            }
            _ => {}
        }
    }

    fn emit_code_block(&mut self, lang: &str, body: &str) {
        let rule_style = Style::default().fg(Color::DarkGray);
        let gutter_text = "┃ ";
        let gutter_w = 2;
        let avail = self.width.saturating_sub(gutter_w);
        let header = if lang.is_empty() {
            "─".repeat(self.width.max(1))
        } else {
            let head = format!("─ {lang} ");
            let hw = head.chars().count();
            let pad = self.width.saturating_sub(hw);
            format!("{head}{}", "─".repeat(pad))
        };
        self.out
            .push(Line::from(Span::styled(header, rule_style)));
        // pulldown emits a trailing newline; lines() handles that correctly.
        for raw in body.lines() {
            let line_text = truncate_to(raw, avail);
            self.out.push(Line::from(vec![
                Span::styled(gutter_text.to_string(), rule_style),
                Span::styled(line_text, Style::default().fg(Color::White)),
            ]));
        }
        self.out.push(Line::from(Span::styled(
            "─".repeat(self.width.max(1)),
            rule_style,
        )));
        self.blank_line();
    }

    fn emit_table(
        &mut self,
        aligns: &[Alignment],
        head: &[String],
        rows: &[Vec<String>],
    ) {
        let rule_style = Style::default().fg(Color::DarkGray);
        let head_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let cell_style = self.base;

        let ncols = head
            .len()
            .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }

        // Normalize all rows to ncols and trim/collapse whitespace per cell.
        let norm = |s: &str| -> String {
            s.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let head_n: Vec<String> = (0..ncols)
            .map(|i| head.get(i).map(|s| norm(s)).unwrap_or_default())
            .collect();
        let rows_n: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                (0..ncols)
                    .map(|i| r.get(i).map(|s| norm(s)).unwrap_or_default())
                    .collect()
            })
            .collect();

        // Natural width per column = max content width.
        let mut col_w: Vec<usize> = (0..ncols)
            .map(|i| {
                let h = head_n[i].chars().count();
                rows_n
                    .iter()
                    .map(|r| r[i].chars().count())
                    .max()
                    .unwrap_or(0)
                    .max(h)
                    .max(1)
            })
            .collect();

        // Each cell renders as " {content} ", separated by '│'.
        // Total width: sum(col_w + 2) + (ncols + 1) for borders.
        let border_overhead = ncols + 1; // outer + separators
        let padding = ncols * 2; // one space on each side
        let target = self.width.max(border_overhead + padding + ncols);
        let mut total = col_w.iter().sum::<usize>() + border_overhead + padding;
        // Shrink widest column until it fits.
        while total > target {
            let (i, _) = col_w
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| **w)
                .unwrap();
            if col_w[i] <= 1 {
                break;
            }
            col_w[i] -= 1;
            total -= 1;
        }

        let pad_cell = |s: &str, w: usize, align: Alignment| -> String {
            let chars: Vec<char> = s.chars().collect();
            let truncated: String = if chars.len() > w {
                if w <= 1 {
                    chars.into_iter().take(w).collect()
                } else {
                    let mut t: String =
                        chars.into_iter().take(w - 1).collect();
                    t.push('…');
                    t
                }
            } else {
                s.to_string()
            };
            let len = truncated.chars().count();
            let pad = w.saturating_sub(len);
            match align {
                Alignment::Right => {
                    format!("{}{}", " ".repeat(pad), truncated)
                }
                Alignment::Center => {
                    let l = pad / 2;
                    let r = pad - l;
                    format!("{}{}{}", " ".repeat(l), truncated, " ".repeat(r))
                }
                _ => format!("{}{}", truncated, " ".repeat(pad)),
            }
        };

        let make_rule = |left: &str, mid: &str, right: &str| -> String {
            let mut s = String::from(left);
            for (i, w) in col_w.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2));
                if i + 1 < col_w.len() {
                    s.push_str(mid);
                }
            }
            s.push_str(right);
            s
        };

        // Top rule.
        self.out
            .push(Line::from(Span::styled(make_rule("┌", "┬", "┐"), rule_style)));

        // Header row.
        let mut spans: Vec<Span<'static>> = Vec::new();
        spans.push(Span::styled("│".to_string(), rule_style));
        for (i, cell) in head_n.iter().enumerate() {
            let align = aligns.get(i).copied().unwrap_or(Alignment::None);
            let text = format!(" {} ", pad_cell(cell, col_w[i], align));
            spans.push(Span::styled(text, head_style));
            spans.push(Span::styled("│".to_string(), rule_style));
        }
        self.out.push(Line::from(spans));

        // Header/body separator.
        self.out
            .push(Line::from(Span::styled(make_rule("├", "┼", "┤"), rule_style)));

        // Body rows.
        for row in &rows_n {
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled("│".to_string(), rule_style));
            for (i, cell) in row.iter().enumerate() {
                let align = aligns.get(i).copied().unwrap_or(Alignment::None);
                let text = format!(" {} ", pad_cell(cell, col_w[i], align));
                spans.push(Span::styled(text, cell_style));
                spans.push(Span::styled("│".to_string(), rule_style));
            }
            self.out.push(Line::from(spans));
        }

        // Bottom rule.
        self.out
            .push(Line::from(Span::styled(make_rule("└", "┴", "┘"), rule_style)));
        self.blank_line();
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if self.body_started || !self.current.is_empty() {
            self.flush_line();
        }
        while self
            .out
            .last()
            .map(|l| l.spans.is_empty())
            .unwrap_or(false)
        {
            self.out.pop();
        }
        while self
            .out
            .first()
            .map(|l| l.spans.is_empty())
            .unwrap_or(false)
        {
            self.out.remove(0);
        }
        if self.out.is_empty() {
            self.out.push(Line::default());
        }
        self.out
    }
}

fn truncate_to(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut t: String = chars.into_iter().take(max - 1).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn heading_then_paragraph() {
        let lines = render("# Hello\n\nworld", 80, Style::default());
        assert!(lines.iter().any(|l| {
            joined(l).contains("Hello")
                && l.spans
                    .iter()
                    .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        }));
        assert!(lines.iter().any(|l| joined(l).contains("world")));
    }

    #[test]
    fn fenced_code_block_gutter() {
        let lines = render("```rust\nfn main() {}\n```", 80, Style::default());
        assert!(lines.iter().any(|l| {
            l.spans
                .first()
                .map(|s| s.content.as_ref() == "┃ ")
                .unwrap_or(false)
        }));
        assert!(lines.iter().any(|l| joined(l).contains("fn main() {}")));
    }

    #[test]
    fn nested_list_bullets() {
        let lines = render("- one\n- two\n  - nested", 80, Style::default());
        let texts: Vec<String> = lines.iter().map(joined).collect();
        assert!(texts.iter().any(|t| t.contains("• one")));
        assert!(texts.iter().any(|t| t.contains("• nested")));
    }

    #[test]
    fn inline_code_is_yellow() {
        let lines = render("use `foo` here", 80, Style::default());
        let has = lines.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.as_ref() == "foo"
                    && s.style.fg == Some(Color::LightYellow)
            })
        });
        assert!(has);
    }

    #[test]
    fn blockquote_prefix_on_each_line() {
        let lines = render("> hello\n> world", 80, Style::default());
        let count = lines
            .iter()
            .filter(|l| {
                l.spans
                    .first()
                    .map(|s| s.content.as_ref().starts_with("▏"))
                    .unwrap_or(false)
            })
            .count();
        assert!(count >= 1);
    }

    #[test]
    fn gfm_table_renders_grid() {
        let src = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
        let lines = render(src, 80, Style::default());
        let texts: Vec<String> = lines.iter().map(joined).collect();
        assert!(
            texts.iter().any(|t| t.starts_with("┌") && t.contains("┬")),
            "expected top rule with junctions, got: {texts:#?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("│") && t.contains("A")),
            "expected header row with A, got: {texts:#?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("│") && t.contains("1") && t.contains("2")),
            "expected body row with 1 and 2"
        );
        assert!(
            texts.iter().any(|t| t.starts_with("└") && t.contains("┴")),
            "expected bottom rule"
        );
    }

    #[test]
    fn strong_and_emphasis_modifiers() {
        let lines = render("**bold** and *italic*", 80, Style::default());
        let mut saw_bold = false;
        let mut saw_italic = false;
        for l in &lines {
            for s in &l.spans {
                if s.content.as_ref() == "bold" {
                    saw_bold = s.style.add_modifier.contains(Modifier::BOLD);
                }
                if s.content.as_ref() == "italic" {
                    saw_italic = s.style.add_modifier.contains(Modifier::ITALIC);
                }
            }
        }
        assert!(saw_bold, "bold token should carry Modifier::BOLD");
        assert!(saw_italic, "italic token should carry Modifier::ITALIC");
    }
}
