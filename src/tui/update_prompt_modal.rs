//! Startup "mewxi has an update" prompt. Pops once per TUI run when
//! the background update check (see [`crate::update`]) reports that
//! the configured channel has something newer. The user can install
//! now, postpone, or turn the startup question off for good (the
//! Config view can re-enable it).

use crate::update::UpdateStatus;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

pub struct UpdatePromptModal {
    pub status: UpdateStatus,
}

pub enum UpdatePromptOutcome {
    Stay,
    /// Close without updating — ask again next startup.
    NotNow,
    /// Suspend the TUI, run git + cargo, resume.
    UpdateNow,
    /// Close AND persist `update_prompt = false`.
    DisableStartupPrompt,
}

impl UpdatePromptModal {
    pub fn new(status: UpdateStatus) -> Self {
        Self { status }
    }

    pub fn handle_key(&self, k: KeyEvent) -> UpdatePromptOutcome {
        match k.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                UpdatePromptOutcome::UpdateNow
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                UpdatePromptOutcome::NotNow
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                UpdatePromptOutcome::DisableStartupPrompt
            }
            _ => UpdatePromptOutcome::Stay,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let modal_area = center_rect(area, 62, 9);
        f.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Update available ")
            .border_style(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD));

        let s = &self.status;
        let body = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  mewxi "),
                Span::styled(&s.current, Style::default().fg(Color::DarkGray)),
                Span::raw(" → "),
                Span::styled(
                    &s.latest,
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(s.channel.label(), Style::default().fg(Color::Cyan)),
                Span::styled(format!(" · {}", s.detail), Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  [Enter/y]",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" update now   "),
                Span::styled("[Esc/n]", Style::default().fg(Color::DarkGray)),
                Span::raw(" later   "),
                Span::styled("[d]", Style::default().fg(Color::DarkGray)),
                Span::raw(" don't ask on startup"),
            ]),
            Line::from(vec![Span::styled(
                "  updates the source checkout, then rebuilds via cargo",
                Style::default().fg(Color::DarkGray),
            )]),
        ];

        f.render_widget(Paragraph::new(body).block(block), modal_area);
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
