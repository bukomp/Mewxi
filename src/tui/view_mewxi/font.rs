//! Y2K / 2000s digital-game-culture typography helpers for view 5.
//!
//! Big blocky pixel-font headline lettering, `░▒▓` gradient separators,
//! deco brackets, and a scrolling marquee/ticker — the "rave" chrome text
//! primitives shared across view 5's panels. Colors come from
//! [`super::palette`] so everything stays on the purple→pink scale.

use super::palette::{P_HIGH, P_HOT, P_LOW, P_MID, P_NEON};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Height in rows of one headline glyph (same for every glyph).
pub const HEADLINE_HEIGHT: u16 = 5;

/// Pixel-font glyph rows for one char, [`HEADLINE_HEIGHT`] rows tall,
/// each row a `&'static str` of block chars '█'/spaces. All rows of one
/// glyph share a width, but widths vary per glyph — letters are 4–5
/// cols, digits 3, punctuation 1 — so number-heavy strings (the streak
/// HUD values) stay compact. Supports the letters in "MEWXI" / "RAVE" /
/// "K", the digits, ':' and '.', plus space; unknown chars render as an
/// all-space glyph.
pub fn glyph(c: char) -> [&'static str; HEADLINE_HEIGHT as usize] {
    match c.to_ascii_uppercase() {
        'M' => ["█   █", "██ ██", "█ █ █", "█   █", "█   █"],
        'E' => ["█████", "█    ", "████ ", "█    ", "█████"],
        'W' => ["█   █", "█   █", "█ █ █", "██ ██", "█   █"],
        'X' => ["█   █", " █ █ ", "  █  ", " █ █ ", "█   █"],
        'I' => ["█████", "  █  ", "  █  ", "  █  ", "█████"],
        'R' => ["████ ", "█   █", "████ ", "█  █ ", "█   █"],
        'A' => [" ███ ", "█   █", "█████", "█   █", "█   █"],
        'V' => ["█   █", "█   █", "█   █", " █ █ ", "  █  "],
        'K' => ["█  █", "█ █ ", "██  ", "█ █ ", "█  █"],
        '0' => ["███", "█ █", "█ █", "█ █", "███"],
        '1' => [" █ ", "██ ", " █ ", " █ ", "███"],
        '2' => ["███", "  █", "███", "█  ", "███"],
        '3' => ["███", "  █", "███", "  █", "███"],
        '4' => ["█ █", "█ █", "███", "  █", "  █"],
        '5' => ["███", "█  ", "███", "  █", "███"],
        '6' => ["███", "█  ", "███", "█ █", "███"],
        '7' => ["███", "  █", "  █", "  █", "  █"],
        '8' => ["███", "█ █", "███", "█ █", "███"],
        '9' => ["███", "█ █", "███", "  █", "███"],
        ':' => [" ", "█", " ", "█", " "],
        '.' => [" ", " ", " ", " ", "█"],
        ' ' => ["     ", "     ", "     ", "     ", "     "],
        _ => ["     ", "     ", "     ", "     ", "     "],
    }
}

/// Render a word as block-letter rows, returning one `String` per glyph
/// row (one space of gutter between glyphs). Unsupported characters
/// render as blank cells. Public so the streak HUD can typeset its
/// values in the same pixel font as the headline.
pub fn big_word(word: &str) -> Vec<String> {
    let height = HEADLINE_HEIGHT as usize;
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

/// Vertical gradient ramp used to color headline rows, cool → hot.
const GRADIENT_RAMP: [Color; 5] = [P_LOW, P_MID, P_HIGH, P_HOT, P_NEON];

/// Render `word` as big pixel-font [`Line`]s, styled in a purple→pink
/// vertical gradient (top rows cooler, bottom rows hotter). `phase`
/// shifts the gradient so it shimmers across frames. Returns exactly
/// [`HEADLINE_HEIGHT`] lines.
pub fn headline_lines(word: &str, phase: usize) -> Vec<Line<'static>> {
    let rows = big_word(word);
    let ramp_len = GRADIENT_RAMP.len();
    rows.into_iter()
        .enumerate()
        .map(|(i, row)| {
            let color = GRADIENT_RAMP[(i + phase) % ramp_len];
            Line::from(Span::styled(row, Style::default().fg(color)))
        })
        .collect()
}

/// Gradient ramp used by [`gradient_separator`], low → high density.
const SEP_RAMP: [(char, Color); 7] = [
    ('░', P_LOW),
    ('▒', P_LOW),
    ('▓', P_MID),
    ('█', P_HOT),
    ('▓', P_MID),
    ('▒', P_HIGH),
    ('░', P_HIGH),
];

/// A full-width `░▒▓█▓▒░`-style gradient separator [`Line`] of the given
/// width, styled across the palette purples. Never panics on width 0.
pub fn gradient_separator(width: usize) -> Line<'static> {
    if width == 0 {
        return Line::from("");
    }
    let ramp_len = SEP_RAMP.len();
    let spans: Vec<Span<'static>> = (0..width)
        .map(|i| {
            let (ch, color) = SEP_RAMP[i % ramp_len];
            Span::styled(ch.to_string(), Style::default().fg(color))
        })
        .collect();
    Line::from(spans)
}

/// Wrap `text` in Y2K deco brackets, returning a plain String like
/// "«▓▒░ TEXT ░▒▓»".
pub fn deco_bracket(text: &str) -> String {
    format!("«▓▒░ {} ░▒▓»", text)
}

/// Ticker/marquee: tile `text` (with a trailing gap) horizontally to
/// exactly `width` chars, starting `offset` chars in so it scrolls and
/// is clipped at both edges. Never panics on width 0 or empty text.
pub fn marquee(text: &str, width: usize, offset: usize) -> String {
    if width == 0 {
        return String::new();
    }
    // Trailing gap gives a visible break between repeats of the phrase.
    let tiled = format!("{}    ", text);
    let chars: Vec<char> = tiled.chars().collect();
    if chars.is_empty() {
        return " ".repeat(width);
    }
    (0..width)
        .map(|i| chars[(i + offset) % chars.len()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marquee_exact_width() {
        assert_eq!(marquee("HELLO", 20, 0).chars().count(), 20);
        assert_eq!(marquee("HELLO", 3, 5).chars().count(), 3);
    }

    #[test]
    fn marquee_width_zero_is_empty() {
        assert_eq!(marquee("HELLO", 0, 0), "");
        assert_eq!(marquee("", 0, 3), "");
    }

    #[test]
    fn marquee_empty_text_is_spaces() {
        assert_eq!(marquee("", 6, 0), " ".repeat(6));
    }

    #[test]
    fn deco_bracket_contains_text_and_glyphs() {
        let s = deco_bracket("RAVE");
        assert!(s.contains("RAVE"));
        assert!(s.contains('«'));
        assert!(s.contains('»'));
        assert!(s.contains('▓'));
        assert!(s.contains('▒'));
        assert!(s.contains('░'));
    }

    #[test]
    fn glyph_rows_same_width() {
        for c in "MEWXIRAVK0123456789:. ?".chars() {
            let g = glyph(c);
            let w = g[0].chars().count();
            for row in g.iter() {
                assert_eq!(row.chars().count(), w, "row width mismatch for {c:?}");
            }
        }
    }

    #[test]
    fn big_word_rows_uniform_width_across_mixed_glyph_widths() {
        // Digits (3 cols), ':' (1 col) and letters (4–5 cols) in one
        // word must still produce equal-length rows.
        let rows = big_word("12:07K");
        assert_eq!(rows.len(), HEADLINE_HEIGHT as usize);
        let w = rows[0].chars().count();
        for row in &rows {
            assert_eq!(row.chars().count(), w);
        }
        assert!(w > 0);
    }

    #[test]
    fn headline_lines_mewxi_height() {
        let lines = headline_lines("MEWXI", 0);
        assert_eq!(lines.len(), HEADLINE_HEIGHT as usize);
    }

    #[test]
    fn headline_lines_phase_shifts_without_panicking() {
        for phase in 0..10 {
            let lines = headline_lines("RAVE", phase);
            assert_eq!(lines.len(), HEADLINE_HEIGHT as usize);
        }
    }

    #[test]
    fn gradient_separator_widths() {
        let line = gradient_separator(10);
        let width: usize = line
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(width, 10);

        let zero = gradient_separator(0);
        let width0: usize = zero.spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(width0, 0);
    }
}
