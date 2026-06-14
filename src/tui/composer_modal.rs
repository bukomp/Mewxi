//! Status-line composer modal (Config view).
//!
//! Lists every known status-line block in render order. The user can
//! reorder them, toggle each on/off, deep-edit a block's TOML in `$EDITOR`,
//! or create a brand-new block — with a **live preview** of the composed
//! line (rendered against representative sample data) at the bottom.
//!
//! Keys: ↑/↓ select · Shift+↑/↓ or J/K move · Space toggle · e edit ·
//! n new · Enter save · Esc cancel.

use super::text_input::TextInput;
use super::widgets::ansi_to_spans;
use crate::statusline::{self, Block};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as RBlock, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

/// One row: a block plus its enabled flag and whether it's a built-in.
struct Row {
    block: Block,
    enabled: bool,
    is_builtin: bool,
}

/// What the modal asks the event loop to do.
pub enum ComposerOutcome {
    /// Consumed the key; nothing for the caller to do.
    Stay,
    /// Close without saving.
    Cancel,
    /// Persist this `(id, enabled)` order via `accounts::set_status_blocks`.
    Save(Vec<(String, bool)>),
    /// Open this block's TOML in `$EDITOR` (caller suspends the terminal).
    EditExternally { id: String, is_builtin: bool },
    /// Create a new user block with this id, then open it in `$EDITOR`.
    NewBlock(String),
}

pub struct ComposerModal {
    rows: Vec<Row>,
    selected: usize,
    dirty: bool,
    status: Option<String>,
    /// Active while typing the id for a new block.
    new_input: Option<TextInput>,
}

impl ComposerModal {
    /// Build from `statusline::composer_rows` output: `(block, enabled,
    /// is_builtin)` in display order.
    pub fn new(rows: Vec<(Block, bool, bool)>) -> Self {
        Self {
            rows: rows
                .into_iter()
                .map(|(block, enabled, is_builtin)| Row {
                    block,
                    enabled,
                    is_builtin,
                })
                .collect(),
            selected: 0,
            dirty: false,
            status: None,
            new_input: None,
        }
    }

    /// Replace the rows (after an external edit / new block) while keeping
    /// the selection in range.
    pub fn reload(&mut self, rows: Vec<(Block, bool, bool)>) {
        let keep = self.selected;
        *self = ComposerModal::new(rows);
        self.selected = keep.min(self.rows.len().saturating_sub(1));
    }

    pub fn set_status(&mut self, msg: String) {
        self.status = Some(msg);
    }

    /// The current `(id, enabled)` composition, in display order.
    pub fn order(&self) -> Vec<(String, bool)> {
        self.rows
            .iter()
            .map(|r| (r.block.id.clone(), r.enabled))
            .collect()
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ComposerOutcome {
        // New-block id entry owns all keys while active.
        if let Some(input) = self.new_input.as_mut() {
            match k.code {
                KeyCode::Enter => {
                    let id = sanitize_id(input.as_str());
                    self.new_input = None;
                    if id.is_empty() {
                        self.status = Some("block id was empty".into());
                        return ComposerOutcome::Stay;
                    }
                    return ComposerOutcome::NewBlock(id);
                }
                KeyCode::Esc => {
                    self.new_input = None;
                    return ComposerOutcome::Stay;
                }
                _ => {
                    input.handle_edit_key(k);
                    return ComposerOutcome::Stay;
                }
            }
        }

        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Esc => ComposerOutcome::Cancel,
            KeyCode::Enter => ComposerOutcome::Save(self.order()),
            KeyCode::Up if shift => {
                self.move_up();
                ComposerOutcome::Stay
            }
            KeyCode::Down if shift => {
                self.move_down();
                ComposerOutcome::Stay
            }
            KeyCode::Char('K') => {
                self.move_up();
                ComposerOutcome::Stay
            }
            KeyCode::Char('J') => {
                self.move_down();
                ComposerOutcome::Stay
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                ComposerOutcome::Stay
            }
            KeyCode::Down => {
                if self.selected + 1 < self.rows.len() {
                    self.selected += 1;
                }
                ComposerOutcome::Stay
            }
            KeyCode::Char(' ') | KeyCode::Char('x') => {
                if let Some(r) = self.rows.get_mut(self.selected) {
                    r.enabled = !r.enabled;
                    self.dirty = true;
                }
                ComposerOutcome::Stay
            }
            KeyCode::Char('e') => match self.rows.get(self.selected) {
                Some(r) => ComposerOutcome::EditExternally {
                    id: r.block.id.clone(),
                    is_builtin: r.is_builtin,
                },
                None => ComposerOutcome::Stay,
            },
            KeyCode::Char('n') => {
                self.new_input = Some(TextInput::new());
                self.status = None;
                ComposerOutcome::Stay
            }
            _ => ComposerOutcome::Stay,
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 && self.selected < self.rows.len() {
            self.rows.swap(self.selected, self.selected - 1);
            self.selected -= 1;
            self.dirty = true;
        }
    }

    fn move_down(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.rows.swap(self.selected, self.selected + 1);
            self.selected += 1;
            self.dirty = true;
        }
    }

    /// The composed preview line (sample data), as a raw ANSI string.
    fn preview_line(&self) -> String {
        let ctx = statusline::preview_ctx();
        let blocks: Vec<Block> = self
            .rows
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.block.clone())
            .collect();
        statusline::render_blocks(&ctx, &blocks)
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        // Full-screen so the composed preview can be read at the terminal's
        // full width (and wraps to more rows when it's longer still).
        f.render_widget(Clear, area);
        let title = if self.dirty {
            " Status line composer · unsaved changes "
        } else {
            " Status line composer "
        };
        let outer = RBlock::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Magenta));
        let inner = outer.inner(area);
        f.render_widget(outer, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // block list
                Constraint::Length(5), // preview (top-bordered, wraps)
                Constraint::Length(1), // help / status / new-block input
            ])
            .split(inner);

        // --- block list (windowed to keep the selection visible) ---------
        let list_h = chunks[0].height.max(1) as usize;
        let offset = if self.selected < list_h {
            0
        } else {
            self.selected + 1 - list_h
        };
        let list: Vec<Line> = self
            .rows
            .iter()
            .enumerate()
            .skip(offset)
            .take(list_h)
            .map(|(i, r)| self.row_line(i, r))
            .collect();
        f.render_widget(Paragraph::new(list), chunks[0]);

        // --- live preview (full width, wraps onto extra rows) ------------
        let rendered = self.preview_line();
        let preview_line = if rendered.trim().is_empty() {
            Line::from(Span::styled(
                "(all blocks disabled)",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ))
        } else {
            Line::from(ansi_to_spans(&rendered))
        };
        let preview_block = RBlock::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(" preview ", Style::default().fg(Color::DarkGray)));
        f.render_widget(
            Paragraph::new(preview_line)
                .wrap(Wrap { trim: false })
                .block(preview_block),
            chunks[1],
        );

        // --- help / status / new-block input -----------------------------
        let bottom = if let Some(input) = self.new_input.as_ref() {
            Line::from(vec![
                Span::styled("new block id: ", Style::default().fg(Color::Yellow)),
                Span::raw(input.as_str().to_string()),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
                Span::styled("  Enter create · Esc cancel", Style::default().fg(Color::DarkGray)),
            ])
        } else if let Some(msg) = self.status.as_ref() {
            Line::from(Span::styled(msg.clone(), Style::default().fg(Color::Cyan)))
        } else {
            Line::from(Span::styled(
                "↑/↓ select · J/K move · space toggle · e edit · n new · Enter save · Esc cancel",
                Style::default().fg(Color::DarkGray),
            ))
        };
        f.render_widget(Paragraph::new(bottom), chunks[2]);
    }

    fn row_line(&self, idx: usize, r: &Row) -> Line<'static> {
        let is_sel = idx == self.selected;
        let arrow = if is_sel { " ▶ " } else { "   " };
        let (check, check_color) = if r.enabled {
            ("✓", Color::Green)
        } else {
            ("✗", Color::DarkGray)
        };
        let id_style = if !r.enabled {
            Style::default().fg(Color::DarkGray)
        } else if is_sel {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled(
                arrow.to_string(),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{check} "), Style::default().fg(check_color)),
            Span::styled(format!("{:<14}", r.block.id), id_style),
        ];
        let mut tag = String::new();
        if r.block.is_command() {
            tag.push_str("cmd ");
        }
        if !r.is_builtin {
            tag.push_str("custom");
        }
        if !tag.is_empty() {
            spans.push(Span::styled(
                format!("{tag:<8} "),
                Style::default().fg(Color::Blue),
            ));
        } else {
            spans.push(Span::raw(format!("{:<9}", "")));
        }
        spans.push(Span::styled(
            r.block.label.clone(),
            Style::default().fg(Color::DarkGray),
        ));
        Line::from(spans)
    }
}

/// Keep only id-safe characters (alnum, `-`, `_`); lowercase the rest out.
fn sanitize_id(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statusline::engine::Condition;
    use crate::statusline::BlockKind;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn tpl(id: &str) -> Block {
        Block {
            id: id.into(),
            label: id.into(),
            kind: BlockKind::Template {
                when: Condition::Always,
                template: format!("{{{id}}}"),
            },
        }
    }

    fn modal() -> ComposerModal {
        ComposerModal::new(vec![
            (tpl("a"), true, true),
            (tpl("b"), true, true),
            (tpl("c"), false, false),
        ])
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn toggle_flips_enabled() {
        let mut m = modal();
        // select row 0 then toggle off.
        match m.handle_key(key(KeyCode::Char(' '))) {
            ComposerOutcome::Stay => {}
            _ => panic!("expected stay"),
        }
        let order = m.order();
        assert_eq!(order[0], ("a".to_string(), false));
    }

    #[test]
    fn shift_down_reorders() {
        let mut m = modal();
        m.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)); // move 'a' down
        let order = m.order();
        assert_eq!(order[0].0, "b");
        assert_eq!(order[1].0, "a");
    }

    #[test]
    fn enter_saves_full_order_with_flags() {
        let mut m = modal();
        match m.handle_key(key(KeyCode::Enter)) {
            ComposerOutcome::Save(order) => {
                assert_eq!(
                    order,
                    vec![
                        ("a".into(), true),
                        ("b".into(), true),
                        ("c".into(), false),
                    ]
                );
            }
            _ => panic!("expected save"),
        }
    }

    #[test]
    fn edit_bubbles_id_and_builtin() {
        let mut m = modal();
        m.handle_key(key(KeyCode::Down)); // select 'b'
        match m.handle_key(key(KeyCode::Char('e'))) {
            ComposerOutcome::EditExternally { id, is_builtin } => {
                assert_eq!(id, "b");
                assert!(is_builtin);
            }
            _ => panic!("expected edit"),
        }
    }

    #[test]
    fn new_block_collects_sanitized_id() {
        let mut m = modal();
        m.handle_key(key(KeyCode::Char('n')));
        for c in "git branch!".chars() {
            m.handle_key(key(KeyCode::Char(c)));
        }
        match m.handle_key(key(KeyCode::Enter)) {
            ComposerOutcome::NewBlock(id) => assert_eq!(id, "gitbranch"),
            _ => panic!("expected new block"),
        }
    }

    fn render_text(m: &ComposerModal, w: u16, h: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| m.render(f, f.area())).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn renders_without_panic() {
        let text = render_text(&modal(), 90, 20);
        assert!(text.contains("Status line composer"), "missing title:\n{text}");
        assert!(text.contains("preview"), "missing preview:\n{text}");
    }

    #[test]
    fn renders_in_tiny_area_without_panic() {
        // Windowing + center_rect must survive a cramped terminal.
        let _ = render_text(&modal(), 12, 5);
        let _ = render_text(&modal(), 1, 1);
    }
}
