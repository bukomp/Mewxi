//! Reusable "under construction" hazard banner.
//!
//! A cosmetic, render-only overlay: a diagonal yellow/black caution-tape
//! band across the middle of any view, carrying a big repeating
//! "UNDER CONSTRUCTION" marquee plus a smaller repeating exit hint. It
//! consumes no input and reads none of the view's data — call
//! [`render`] as the *last* draw step for a view so it sits on top while
//! the view underneath keeps updating and stays fully interactive.
//!
//! ```ignore
//! view_foo::render(f, area, ...);
//! under_construction::render(f, area); // banner on top, "press esc to return"
//! ```

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

/// Hazard-tape colours: high-vis yellow and near-black, mirroring the
/// caution stripes used on real construction barriers.
const HAZARD_YELLOW: Color = Color::Indexed(220);
const HAZARD_BLACK: Color = Color::Indexed(16);
/// Width of each diagonal band, in cells.
const STRIPE_W: usize = 3;
/// Fraction of the view height the band occupies, as a percentage.
const BAND_PCT: u32 = 30;
/// Default exit hint shown in the smaller repeating marquee.
const DEFAULT_HINT: &str = "press esc to return";

/// Paint the hazard banner over `view` with the default "press esc to
/// return" hint. See [`render_with_hint`] for a custom hint.
pub fn render(f: &mut Frame, view: Rect) {
    render_with_hint(f, view, DEFAULT_HINT);
}

/// Paint a diagonal yellow/black "UNDER CONSTRUCTION" hazard band over
/// `view`: full width, ~30% of the height, vertically centred. `hint` is
/// the smaller repeating line shown above and below the title (e.g. how
/// to leave the view). Purely cosmetic — nothing here consumes input or
/// alters the data shown underneath.
pub fn render_with_hint(f: &mut Frame, view: Rect, hint: &str) {
    if view.width == 0 || view.height == 0 {
        return;
    }

    // ~30% of the view height, vertically centred, spanning full width.
    let band_h = ((view.height as u32 * BAND_PCT) / 100).max(1) as u16;
    let band = Rect {
        x: view.x,
        y: view.y + view.height.saturating_sub(band_h) / 2,
        width: view.width,
        height: band_h,
    };

    // Diagonal stripes: offsetting the band boundary by the row index
    // slants the otherwise-vertical bands into diagonals. Adjacent cells
    // in the same band are merged into one span to keep the line cheap.
    let w = band.width as usize;
    let rows: Vec<Line> = (0..band.height as usize)
        .map(|r| {
            let mut spans: Vec<Span> = Vec::new();
            let mut x = 0usize;
            while x < w {
                let yellow = (((x + r) / STRIPE_W) % 2) == 0;
                let run = (STRIPE_W - ((x + r) % STRIPE_W)).min(w - x);
                let bg = if yellow { HAZARD_YELLOW } else { HAZARD_BLACK };
                spans.push(Span::styled(
                    " ".repeat(run),
                    Style::default().bg(bg),
                ));
                x += run;
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Clear, band);
    f.render_widget(Paragraph::new(rows), band);

    // Repeating "UNDER CONSTRUCTION" marquee in big block letters,
    // spanning the full width and clipped at both edges so it reads as
    // caution tape running off-screen. Sits on a solid black strip
    // through the vertical centre of the band; stripes show above/below.
    let sign_style = Style::default()
        .fg(HAZARD_YELLOW)
        .bg(HAZARD_BLACK)
        .add_modifier(Modifier::BOLD);

    if band.height >= 5 {
        // Trailing spaces give a visible gap between repeats of each phrase.
        let unit = big_word("UNDER CONSTRUCTION    ");
        // Smaller, dimmer single-row hint that repeats the same way.
        let hint = format!("{}     ", hint.trim());
        let hint_style = Style::default().fg(Color::Indexed(178)).bg(HAZARD_BLACK);

        // Layout: hint row, 2 separators, the big block word, 2
        // separators, then the hint row again. Capped to band, centred.
        let line_count = unit.len() as u16 + 6;
        let strip_h = line_count.min(band.height);
        let strip = Rect {
            x: band.x,
            y: band.y + band.height.saturating_sub(strip_h) / 2,
            width: band.width,
            height: strip_h,
        };

        let w = strip.width as usize;
        // Start part-way through each phrase so the left edge is cut too.
        let big_offset = unit.first().map_or(0, |r| r.chars().count() / 3);
        let blank = Line::from(Span::styled(" ".repeat(w), sign_style));
        let hint_line = Line::from(Span::styled(
            tile_to(&hint, w, hint.chars().count() / 2),
            hint_style,
        ));

        let mut lines: Vec<Line> = vec![hint_line.clone(), blank.clone(), blank.clone()];
        for row in &unit {
            lines.push(Line::from(Span::styled(tile_to(row, w, big_offset), sign_style)));
        }
        lines.push(blank.clone());
        lines.push(blank);
        lines.push(hint_line);

        f.render_widget(Clear, strip);
        f.render_widget(Paragraph::new(lines).style(sign_style), strip);
    } else {
        // Too short for big text — fall back to a centred one-line label.
        let label = " ⚠ UNDER CONSTRUCTION ⚠ ";
        let lw = (label.chars().count() as u16).min(band.width);
        let strip = Rect {
            x: band.x + band.width.saturating_sub(lw) / 2,
            y: band.y + band.height / 2,
            width: lw,
            height: 1,
        };
        f.render_widget(Clear, strip);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(label, sign_style))),
            strip,
        );
    }
}

/// Render a word as block-letter ASCII art, returning one `String` per
/// glyph row. Unsupported characters render as blank cells.
fn big_word(word: &str) -> Vec<String> {
    let height = glyph(' ').len();
    let mut rows = vec![String::new(); height];
    for (i, c) in word.chars().enumerate() {
        let g = glyph(c);
        for (r, row) in rows.iter_mut().enumerate() {
            if i > 0 {
                row.push(' ');
            }
            row.push_str(g[r]);
        }
    }
    rows
}

/// Tile `row` horizontally to exactly `width` chars, starting `offset`
/// chars in, so the pattern repeats and is clipped at both ends.
fn tile_to(row: &str, width: usize, offset: usize) -> String {
    let chars: Vec<char> = row.chars().collect();
    if chars.is_empty() || width == 0 {
        return " ".repeat(width);
    }
    (0..width)
        .map(|i| chars[(i + offset) % chars.len()])
        .collect()
}

/// 7-row block-letter glyphs for the characters used by the banner.
fn glyph(c: char) -> [&'static str; 7] {
    match c.to_ascii_uppercase() {
        'U' => ["█   █", "█   █", "█   █", "█   █", "█   █", "█   █", "█████"],
        'N' => ["█   █", "██  █", "██  █", "█ █ █", "█  ██", "█  ██", "█   █"],
        'D' => ["████ ", "█   █", "█   █", "█   █", "█   █", "█   █", "████ "],
        'E' => ["█████", "█    ", "█    ", "████ ", "█    ", "█    ", "█████"],
        'R' => ["████ ", "█   █", "█   █", "████ ", "█ █  ", "█  █ ", "█   █"],
        'C' => ["█████", "█    ", "█    ", "█    ", "█    ", "█    ", "█████"],
        'O' => ["█████", "█   █", "█   █", "█   █", "█   █", "█   █", "█████"],
        'S' => ["█████", "█    ", "█    ", "█████", "    █", "    █", "█████"],
        'T' => ["█████", "  █  ", "  █  ", "  █  ", "  █  ", "  █  ", "  █  "],
        'I' => ["█████", "  █  ", "  █  ", "  █  ", "  █  ", "  █  ", "█████"],
        _ => ["     ", "     ", "     ", "     ", "     ", "     ", "     "],
    }
}
