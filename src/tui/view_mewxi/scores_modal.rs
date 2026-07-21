//! `s` score-board modal for the mewxi rave view — shows the live
//! arcade stats ([`super::streaks::snapshot`]) plus the persisted
//! all-time bests, and where the scores status file lives.

use super::palette::{P_DIM, P_HOT, P_LABEL, P_MID, P_TEXT};
use super::streaks;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

pub struct ScoresModal;

pub enum ScoresOutcome {
    Stay,
    Close,
}

impl ScoresModal {
    pub fn new() -> Self {
        Self
    }

    /// `Esc`, `q` and `s` close; everything else is swallowed so no
    /// shortcut fires underneath the modal.
    pub fn handle_key(&mut self, k: KeyEvent) -> ScoresOutcome {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('s') => ScoresOutcome::Close,
            _ => ScoresOutcome::Stay,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let snap = streaks::snapshot();
        let history = streaks::history();

        const MODAL_WIDTH: u16 = 64;
        // Fixed chrome, in lines, that always surrounds the history rows:
        // 2 border lines + blank + 5 stat rows + blank + footer + blank +
        // heading = 12.
        const FIXED_CHROME: u16 = 12;

        let combo_str = format!("x{}", snap.combo);
        let best_combo_str = format!("x{}", snap.best_combo);
        let streak_str = streaks::fmt_mmss(snap.streak_secs);
        let footer = match streaks::scores_file_path() {
            Some(path) => format!("saved to {}", path.display()),
            None => "saved to (unavailable)".to_string(),
        };

        let label_style = Style::default().fg(P_LABEL);
        let value_style = Style::default().fg(P_TEXT).add_modifier(Modifier::BOLD);
        let heading_style = Style::default().fg(P_LABEL);
        let dim_style = Style::default().fg(P_DIM);

        let stat_line = |label: &'static str, value: String| {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{label:<12}"), label_style),
                Span::styled(value, value_style),
            ])
        };

        // How many history rows fit given the available terminal height,
        // after the fixed chrome above is accounted for. When none fit
        // (or there's no history), the placeholder line below covers it.
        let max_rows = area.height.saturating_sub(FIXED_CHROME) as usize;
        let shown = history.len().min(max_rows);

        let modal_height = FIXED_CHROME + if shown == 0 { 1 } else { shown as u16 };

        let modal_area = center_rect(area, MODAL_WIDTH, modal_height);
        f.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " HIGH SCORE ",
                Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(P_MID));

        let mut body = vec![
            Line::from(""),
            stat_line("SCORE", snap.score.to_string()),
            stat_line("BEST", snap.best_score.to_string()),
            stat_line("COMBO", combo_str),
            stat_line("BEST COMBO", best_combo_str),
            stat_line("STREAK", streak_str),
            Line::from(""),
            Line::from(Span::styled(format!("  {footer}"), Style::default().fg(P_DIM))),
            Line::from(""),
            Line::from(Span::styled("  RECENT RUNS", heading_style)),
        ];

        if shown == 0 {
            body.push(Line::from(Span::styled(
                "  no finished runs yet",
                dim_style,
            )));
        } else {
            for (idx, entry) in history.iter().take(shown).enumerate() {
                body.push(Line::from(Span::styled(
                    format!("  {}", history_row(idx + 1, entry)),
                    Style::default().fg(P_TEXT),
                )));
            }
        }

        f.render_widget(Paragraph::new(body).block(block), modal_area);
    }
}

/// Format one history row (1-based `idx`) into a fixed-width, aligned
/// string: index, score, peak combo, peak streak and the completion
/// timestamp (rendered in UTC, `%Y-%m-%d %H:%M`). Pure — no global
/// state — so it's unit-testable with hand-built [`streaks::RunEntry`]
/// values. Never panics: a missing/unparsable `ended_at` renders as a
/// dash rather than erroring.
fn history_row(idx: usize, e: &streaks::RunEntry) -> String {
    let combo_str = format!("x{}", e.peak_combo);
    let streak_str = streaks::fmt_mmss(e.peak_streak_secs);
    let when = if e.ended_at.is_empty() {
        "-".to_string()
    } else {
        match chrono::DateTime::parse_from_rfc3339(&e.ended_at) {
            Ok(dt) => dt
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            Err(_) => "-".to_string(),
        }
    };

    format!(
        "{idx:>2}  {score:>8}  {combo:<6}  {streak:>6}  {when}",
        idx = idx,
        score = e.score,
        combo = combo_str,
        streak = streak_str,
        when = when,
    )
}

fn center_rect(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn render_does_not_panic_at_any_size_and_shows_labels() {
        for (w, h) in [(120u16, 30u16), (10, 4), (1, 1)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| ScoresModal::new().render(f, f.area()))
                .unwrap();

            if w == 120 && h == 30 {
                let buf = terminal.backend().buffer();
                let mut text = String::new();
                for y in 0..buf.area.height {
                    for x in 0..buf.area.width {
                        text.push_str(buf[(x, y)].symbol());
                    }
                    text.push('\n');
                }
                assert!(text.contains("SCORE"), "missing SCORE label:\n{text}");
                assert!(text.contains("BEST"), "missing BEST label:\n{text}");
                assert!(
                    text.contains("RECENT RUNS"),
                    "missing RECENT RUNS heading:\n{text}"
                );
                assert!(
                    text.contains("no finished runs yet"),
                    "missing empty-history placeholder:\n{text}"
                );
            }
        }
    }

    #[test]
    fn history_row_formats_valid_entry() {
        let entry = streaks::RunEntry {
            score: 12_345,
            peak_combo: 7,
            peak_streak_secs: 125.0,
            ended_at: "2026-07-21T09:05:00Z".to_string(),
        };
        let row = history_row(3, &entry);
        assert!(row.contains("2026-07-21 09:05"), "row: {row}");
        assert!(row.contains("x7"), "row: {row}");
        assert!(row.contains("2:05"), "row: {row}");
        assert!(row.contains("12345"), "row: {row}");
    }

    #[test]
    fn history_row_handles_missing_ended_at_without_panicking() {
        let entry = streaks::RunEntry {
            score: 0,
            peak_combo: 0,
            peak_streak_secs: 0.0,
            ended_at: String::new(),
        };
        let row = history_row(1, &entry);
        assert!(row.contains('-'), "row should fall back to a dash: {row}");
    }
}
