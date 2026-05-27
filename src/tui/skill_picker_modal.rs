//! Modal overlay for picking a skill (or slash command) to invoke in a
//! mewxi-driven `claude` session. The pick is sent to the PTY by the
//! caller as `/<name>\r`; this module is presentation-only.
//!
//! Discovery happens once at modal-open time via [`crate::skills::discover`]
//! against the active account's `CLAUDE_CONFIG_DIR` and the session's cwd
//! — same locations claude itself scans, so the list matches what the
//! `Skill` tool offers inside the embedded session.
//!
//! Layout: single column. Top line is an inline filter input that
//! narrows the list by substring on both the name and description. Each
//! row shows `<arrow> <name>` on one line and `<indent> <description>`
//! wrapped beneath it. The footer shows the origin tag and source path
//! of the highlighted entry — useful for "wait, where did this skill
//! actually come from?" moments.

use crate::skills::{Skill, SkillOrigin};
use crate::tui::text_input::{EditOutcome, TextInput};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use std::cell::RefCell;

pub enum SkillOutcome {
    Stay,
    Cancel,
    /// User picked a skill. Caller sends `/<name>\r` to the PTY.
    Confirm { name: String },
}

pub struct SkillPickerModal {
    skills: Vec<Skill>,
    filter: TextInput,
    /// Index into the *filtered* view, not `skills`.
    selected: usize,
    /// Held across renders so ratatui keeps the selected row in view.
    list_state: RefCell<ListState>,
}

impl SkillPickerModal {
    pub fn new(skills: Vec<Skill>) -> Self {
        Self {
            skills,
            filter: TextInput::new(),
            selected: 0,
            list_state: RefCell::new(ListState::default()),
        }
    }

    /// Indices into `self.skills` that pass the current filter.
    fn filtered_indices(&self) -> Vec<usize> {
        let needle = self.filter.as_str().trim().to_ascii_lowercase();
        if needle.is_empty() {
            return (0..self.skills.len()).collect();
        }
        self.skills
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                let hay = format!("{} {}", s.name, s.description).to_ascii_lowercase();
                hay.contains(&needle).then_some(i)
            })
            .collect()
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> SkillOutcome {
        match k.code {
            KeyCode::Esc => return SkillOutcome::Cancel,
            KeyCode::Enter => {
                let visible = self.filtered_indices();
                let Some(&idx) = visible.get(self.selected) else {
                    return SkillOutcome::Stay;
                };
                return SkillOutcome::Confirm {
                    name: self.skills[idx].name.clone(),
                };
            }
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                return SkillOutcome::Stay;
            }
            KeyCode::Down => {
                let len = self.filtered_indices().len();
                if self.selected + 1 < len {
                    self.selected += 1;
                }
                return SkillOutcome::Stay;
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(8);
                return SkillOutcome::Stay;
            }
            KeyCode::PageDown => {
                let len = self.filtered_indices().len();
                if len > 0 {
                    self.selected = (self.selected + 8).min(len - 1);
                }
                return SkillOutcome::Stay;
            }
            _ => {}
        }
        // Everything else goes to the filter input. Reset the cursor to
        // the top whenever the filter changes so the first hit is what
        // Enter picks.
        if let EditOutcome::Consumed { changed: true } = self.filter.handle_edit_key(k) {
            self.selected = 0;
        }
        SkillOutcome::Stay
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let modal_area = center_rect(area, 80, 24);
        f.render_widget(Clear, modal_area);

        let title = format!(
            " Skills — type to filter · ↑↓ move · Enter run · Esc cancel  ({} total) ",
            self.skills.len()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Magenta));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // filter line
                Constraint::Min(1),    // list
                Constraint::Length(1), // footer (origin + path)
            ])
            .split(inner);

        self.render_filter(f, chunks[0]);
        let visible = self.filtered_indices();
        self.render_list(f, chunks[1], &visible);
        self.render_footer(f, chunks[2], &visible);
    }

    fn render_filter(&self, f: &mut Frame, area: Rect) {
        let buf = self.filter.as_str();
        let cursor = self.filter.cursor_char();
        // Render as `/{filter}|` with a block cursor on the active char,
        // matching the rest of mewxi's input fields. The leading `/`
        // hints that the picker is about to send a slash-command.
        let mut spans = vec![Span::styled("/", Style::default().fg(Color::DarkGray))];
        for (i, ch) in buf.chars().enumerate() {
            let style = if i == cursor {
                Style::default()
                    .bg(Color::White)
                    .fg(Color::Black)
            } else {
                Style::default().fg(Color::White)
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        if cursor >= buf.chars().count() {
            spans.push(Span::styled(
                " ",
                Style::default().bg(Color::White).fg(Color::Black),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_list(&self, f: &mut Frame, area: Rect, visible: &[usize]) {
        if visible.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "  no matches",
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                ))),
                area,
            );
            return;
        }
        let items: Vec<ListItem> = visible
            .iter()
            .enumerate()
            .map(|(row, &idx)| {
                let s = &self.skills[idx];
                let selected = row == self.selected;
                let arrow = if selected { "▶ " } else { "  " };
                let name_style = if selected {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let origin_tag = match &s.origin {
                    SkillOrigin::User => "[user]".to_string(),
                    SkillOrigin::Project => "[project]".to_string(),
                    SkillOrigin::Plugin(_) => "[plugin]".to_string(),
                    SkillOrigin::BuiltIn => "[built-in]".to_string(),
                };
                let origin_style = match s.origin {
                    SkillOrigin::User => Style::default().fg(Color::Cyan),
                    SkillOrigin::Project => Style::default().fg(Color::Green),
                    SkillOrigin::Plugin(_) => Style::default().fg(Color::Magenta),
                    SkillOrigin::BuiltIn => Style::default().fg(Color::Blue),
                };
                let desc = if s.description.is_empty() {
                    "(no description)".to_string()
                } else {
                    truncate_to(&s.description, area.width.saturating_sub(6) as usize)
                };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(arrow, name_style),
                        Span::styled(s.name.clone(), name_style),
                        Span::raw("  "),
                        Span::styled(origin_tag, origin_style),
                    ]),
                    Line::from(Span::styled(
                        format!("    {desc}"),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
            })
            .collect();
        let mut state = self.list_state.borrow_mut();
        state.select(Some(self.selected));
        f.render_stateful_widget(List::new(items), area, &mut state);
    }

    fn render_footer(&self, f: &mut Frame, area: Rect, visible: &[usize]) {
        let line = match visible.get(self.selected) {
            Some(&idx) => {
                let s = &self.skills[idx];
                let plugin_suffix = match &s.origin {
                    SkillOrigin::Plugin(p) => format!(" · {p}"),
                    _ => String::new(),
                };
                format!(
                    " {}{}  ·  {}",
                    s.origin.label(),
                    plugin_suffix,
                    truncate_to(&s.source_path.display().to_string(), area.width as usize)
                )
            }
            None => String::new(),
        };
        f.render_widget(
            Paragraph::new(Span::styled(line, Style::default().fg(Color::DarkGray))),
            area,
        );
    }
}

fn truncate_to(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
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
    use crossterm::event::KeyModifiers;
    use std::path::PathBuf;

    fn skill(name: &str, desc: &str, origin: SkillOrigin) -> Skill {
        Skill {
            name: name.into(),
            description: desc.into(),
            origin,
            source_path: PathBuf::from("/tmp/SKILL.md"),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn char(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn enter_confirms_top_match() {
        let mut m = SkillPickerModal::new(vec![
            skill("alpha", "first", SkillOrigin::User),
            skill("beta", "second", SkillOrigin::User),
        ]);
        match m.handle_key(key(KeyCode::Enter)) {
            SkillOutcome::Confirm { name } => assert_eq!(name, "alpha"),
            _ => panic!("expected Confirm"),
        }
    }

    #[test]
    fn filter_narrows_then_enter_picks_filtered() {
        let mut m = SkillPickerModal::new(vec![
            skill("alpha", "first", SkillOrigin::User),
            skill("beta", "second", SkillOrigin::User),
            skill("gamma-beta", "third", SkillOrigin::User),
        ]);
        // Type "beta" — should narrow to two entries.
        for c in "beta".chars() {
            assert!(matches!(m.handle_key(char(c)), SkillOutcome::Stay));
        }
        assert_eq!(m.filtered_indices(), vec![1, 2]);
        // Enter picks the top filtered result.
        match m.handle_key(key(KeyCode::Enter)) {
            SkillOutcome::Confirm { name } => assert_eq!(name, "beta"),
            _ => panic!("expected Confirm"),
        }
    }

    #[test]
    fn down_arrow_moves_selection() {
        let mut m = SkillPickerModal::new(vec![
            skill("a", "", SkillOrigin::User),
            skill("b", "", SkillOrigin::User),
            skill("c", "", SkillOrigin::User),
        ]);
        assert!(matches!(m.handle_key(key(KeyCode::Down)), SkillOutcome::Stay));
        assert!(matches!(m.handle_key(key(KeyCode::Down)), SkillOutcome::Stay));
        match m.handle_key(key(KeyCode::Enter)) {
            SkillOutcome::Confirm { name } => assert_eq!(name, "c"),
            _ => panic!("expected Confirm"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut m = SkillPickerModal::new(vec![skill("a", "", SkillOrigin::User)]);
        assert!(matches!(m.handle_key(key(KeyCode::Esc)), SkillOutcome::Cancel));
    }

    #[test]
    fn empty_list_enter_is_noop() {
        let mut m = SkillPickerModal::new(vec![]);
        assert!(matches!(m.handle_key(key(KeyCode::Enter)), SkillOutcome::Stay));
    }

    #[test]
    fn filter_matches_on_description() {
        let mut m = SkillPickerModal::new(vec![
            skill("xyz", "review the diff", SkillOrigin::User),
            skill("abc", "another thing", SkillOrigin::User),
        ]);
        for c in "review".chars() {
            m.handle_key(char(c));
        }
        let visible = m.filtered_indices();
        assert_eq!(visible, vec![0]);
    }
}
