//! Modal overlay for picking the model of a mewxi-driven `claude`
//! session. The pick is sent as `/model <slug>\r` to the PTY by the
//! caller — this module is presentation-only.
//!
//! While open, the modal owns every keystroke (Esc to cancel, Up/Down
//! to move, Enter to confirm). The parent must dispatch keys through
//! [`ModelPickerModal::handle_key`] *before* the global handlers,
//! otherwise the underlying view's keybinds will leak through.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem};

/// One row in the picker — a `/model` slug plus a short label and a
/// help line explaining when to pick it.
pub struct ModelChoice {
    pub slug: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

/// Fixed list of models we offer. Slugs are what `/model` accepts as
/// aliases. `default` clears any session-local override and falls back
/// to whatever Claude Code was started with.
const MODELS: &[ModelChoice] = &[
    ModelChoice { slug: "opus",    label: "Opus",    hint: "most capable, slow + expensive" },
    ModelChoice { slug: "sonnet",  label: "Sonnet",  hint: "balanced default" },
    ModelChoice { slug: "haiku",   label: "Haiku",   hint: "fastest + cheapest" },
    ModelChoice { slug: "default", label: "Default", hint: "clear override; use Claude Code's default" },
];

pub struct ModelPickerModal {
    idx: usize,
}

pub enum ModelOutcome {
    Stay,
    Cancel,
    /// Send `/model <slug>\r` to the driven PTY.
    Confirm(String),
}

impl ModelPickerModal {
    /// Open the picker. `current` is the model string currently shown
    /// in the session header (e.g. `claude-sonnet-4-6`); if it contains
    /// one of our slugs we pre-select that row so Enter is idempotent.
    pub fn new(current: Option<&str>) -> Self {
        let idx = current
            .and_then(|c| {
                let c = c.to_ascii_lowercase();
                MODELS.iter().position(|m| c.contains(m.slug))
            })
            .unwrap_or(0);
        Self { idx }
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModelOutcome {
        match k.code {
            KeyCode::Esc => ModelOutcome::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.idx > 0 {
                    self.idx -= 1;
                }
                ModelOutcome::Stay
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.idx + 1 < MODELS.len() {
                    self.idx += 1;
                }
                ModelOutcome::Stay
            }
            KeyCode::Enter => {
                let slug = MODELS[self.idx].slug.to_string();
                ModelOutcome::Confirm(slug)
            }
            _ => ModelOutcome::Stay,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        // Fixed-size centered box — the list is short and known.
        let modal_area = center_rect(area, 40, 12);
        f.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Model — Enter pick · Esc cancel ")
            .border_style(Style::default().fg(Color::Magenta));

        let items: Vec<ListItem> = MODELS
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let selected = i == self.idx;
                let arrow = if selected { "▶ " } else { "  " };
                let name_style = if selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(arrow, name_style),
                        Span::styled(m.label, name_style),
                        Span::raw("  "),
                        Span::styled(
                            format!("({})", m.slug),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]),
                    Line::from(Span::styled(
                        format!("    {}", m.hint),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
            })
            .collect();

        f.render_widget(List::new(items).block(block), modal_area);
    }
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
