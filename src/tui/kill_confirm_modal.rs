use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

pub struct KillConfirmModal {
    pub acct: String,
    pub sid: String,
    pub pid: u32,
}

pub enum KillConfirmOutcome {
    Stay,
    Cancel,
    Confirm,
}

impl KillConfirmModal {
    pub fn new(acct: String, sid: String, pid: u32) -> Self {
        Self { acct, sid, pid }
    }

    pub fn handle_key(&self, k: KeyEvent) -> KillConfirmOutcome {
        match k.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => KillConfirmOutcome::Confirm,
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                KillConfirmOutcome::Cancel
            }
            _ => KillConfirmOutcome::Stay,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let modal_area = center_rect(area, 52, 7);
        f.render_widget(Clear, modal_area);

        let short = self.sid.chars().take(8).collect::<String>();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Kill session ")
            .border_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));

        let body = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  Kill "),
                Span::styled(&short, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw(format!("  (pid {})", self.pid)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  [Enter / y]",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" confirm  "),
                Span::styled("[Esc / n]", Style::default().fg(Color::DarkGray)),
                Span::raw(" cancel"),
            ]),
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
