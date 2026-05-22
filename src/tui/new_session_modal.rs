//! Modal overlay for picking the account + working directory before
//! mewxi spawns a new interactive `claude` session.
//!
//! The modal renders on top of whatever view is active. While open it
//! owns every keystroke — the parent run loop must route keys through
//! [`NewSessionModal::handle_key`] and not fall through to global
//! handlers, otherwise the global `q`/`Esc` would terminate mewxi from
//! inside the modal.
//!
//! Layout:
//!  - Left pane: account list.
//!  - Right pane (vertical stack): editable path bar, fuzzy filter
//!    input, then the filtered list of subdirectories of the path bar.
//!
//! Focus rotates Accounts → PathBar → Filter → EntryList (Tab forward,
//! Shift-Tab backward). Confirm with `.` from the EntryList focus to
//! spawn under the highlighted account in the path-bar directory.

use crate::accounts::Account;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, PartialEq, Eq)]
enum ModalFocus {
    Accounts,
    PathBar,
    Filter,
    EntryList,
}

pub struct NewSessionModal {
    accounts: Vec<Account>,
    account_idx: usize,
    /// Editable text mirror of the directory the entries list is
    /// reading from. Diverges from `browse_cwd` while the user types
    /// in the path bar; an Enter in the path bar reconciles them.
    path_input: String,
    /// Resolved directory whose contents `entries` was built from.
    browse_cwd: PathBuf,
    /// Inline error from a failed path-bar resolution or read_dir; one
    /// line, cleared on the next successful navigation.
    error: Option<String>,
    /// Directory entries of `browse_cwd` (just `..` plus subdirs).
    /// Dotfiles included.
    entries: Vec<PathBuf>,
    /// Fuzzy filter applied over `entries`. Index into the *filtered*
    /// list, not `entries`.
    filter: String,
    entry_idx: usize,
    focus: ModalFocus,
}

/// Result of dispatching a key into the modal.
pub enum ModalOutcome {
    /// Modal stays open.
    Stay,
    /// User pressed Esc — close without spawning.
    Cancel,
    /// User confirmed — spawn under `account` in `cwd`.
    Confirm { account: Account, cwd: PathBuf },
}

impl NewSessionModal {
    pub fn new(accounts: Vec<Account>, initial_idx: usize, initial_dir: PathBuf) -> Self {
        let account_idx = initial_idx.min(accounts.len().saturating_sub(1));
        let (browse_cwd, entries, error) = match read_dirs(&initial_dir) {
            Ok(es) => (initial_dir.clone(), es, None),
            Err(e) => {
                let fallback = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
                let entries = read_dirs(&fallback).unwrap_or_default();
                (fallback, entries, Some(format!("{}: {}", initial_dir.display(), e)))
            }
        };
        let path_input = browse_cwd.to_string_lossy().into_owned();
        Self {
            accounts,
            account_idx,
            path_input,
            browse_cwd,
            error,
            entries,
            filter: String::new(),
            entry_idx: 0,
            // Start on the path bar — the cursor is visibly there so
            // the user immediately knows where typing goes. From here
            // Tab → Filter → EntryList → Accounts → PathBar.
            focus: ModalFocus::PathBar,
        }
    }

    /// Indices of `entries` that match the fuzzy `filter`. When `filter`
    /// is empty, every entry passes through.
    fn filtered_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let q: Vec<char> = self.filter.chars().flat_map(|c| c.to_lowercase()).collect();
        let mut scored: Vec<(usize, usize)> = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            let name = entry_label(e);
            if let Some(score) = fuzzy_score(&q, &name) {
                scored.push((i, score));
            }
        }
        scored.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    fn reload_entries(&mut self) {
        match read_dirs(&self.browse_cwd) {
            Ok(es) => {
                self.entries = es;
                self.error = None;
            }
            Err(e) => {
                self.entries = Vec::new();
                self.error = Some(format!("read {}: {}", self.browse_cwd.display(), e));
            }
        }
        self.filter.clear();
        self.entry_idx = 0;
    }

    fn navigate_to(&mut self, path: PathBuf) {
        self.browse_cwd = path;
        self.path_input = self.browse_cwd.to_string_lossy().into_owned();
        self.reload_entries();
    }

    fn cycle_focus(&mut self, forward: bool) {
        self.focus = match (self.focus, forward) {
            (ModalFocus::Accounts, true) => ModalFocus::PathBar,
            (ModalFocus::PathBar, true) => ModalFocus::Filter,
            (ModalFocus::Filter, true) => ModalFocus::EntryList,
            (ModalFocus::EntryList, true) => ModalFocus::Accounts,
            (ModalFocus::Accounts, false) => ModalFocus::EntryList,
            (ModalFocus::PathBar, false) => ModalFocus::Accounts,
            (ModalFocus::Filter, false) => ModalFocus::PathBar,
            (ModalFocus::EntryList, false) => ModalFocus::Filter,
        };
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModalOutcome {
        // Global modal keys first.
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _) => return ModalOutcome::Cancel,
            (KeyCode::Tab, m) if !m.contains(KeyModifiers::SHIFT) => {
                self.cycle_focus(true);
                return ModalOutcome::Stay;
            }
            (KeyCode::BackTab, _) => {
                self.cycle_focus(false);
                return ModalOutcome::Stay;
            }
            _ => {}
        }

        match self.focus {
            ModalFocus::Accounts => match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.account_idx > 0 {
                        self.account_idx -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.account_idx + 1 < self.accounts.len() {
                        self.account_idx += 1;
                    }
                }
                KeyCode::Enter => {
                    self.focus = ModalFocus::EntryList;
                }
                _ => {}
            },
            ModalFocus::PathBar => match (k.code, k.modifiers) {
                (KeyCode::Enter, _) => {
                    let typed = expand_tilde(&self.path_input);
                    if typed.is_dir() {
                        self.navigate_to(typed);
                        self.focus = ModalFocus::EntryList;
                    } else {
                        self.error = Some(format!("not a directory: {}", typed.display()));
                    }
                }
                (KeyCode::Backspace, _) => {
                    self.path_input.pop();
                }
                (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.path_input.clear();
                }
                (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                    // Delete trailing path segment, like shell readline.
                    while matches!(self.path_input.chars().last(), Some('/')) {
                        self.path_input.pop();
                    }
                    while let Some(c) = self.path_input.chars().last() {
                        if c == '/' {
                            break;
                        }
                        self.path_input.pop();
                    }
                }
                (KeyCode::Char(c), m)
                    if !m.contains(KeyModifiers::CONTROL)
                        && !m.contains(KeyModifiers::ALT) =>
                {
                    self.path_input.push(c);
                }
                _ => {}
            },
            ModalFocus::Filter => match (k.code, k.modifiers) {
                (KeyCode::Backspace, _) => {
                    self.filter.pop();
                    self.entry_idx = 0;
                }
                (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.filter.clear();
                    self.entry_idx = 0;
                }
                (KeyCode::Char(c), m)
                    if !m.contains(KeyModifiers::CONTROL)
                        && !m.contains(KeyModifiers::ALT) =>
                {
                    self.filter.push(c);
                    self.entry_idx = 0;
                }
                (KeyCode::Enter, _) => {
                    self.focus = ModalFocus::EntryList;
                }
                _ => {}
            },
            ModalFocus::EntryList => {
                let filtered = self.filtered_indices();
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        if self.entry_idx > 0 {
                            self.entry_idx -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if self.entry_idx + 1 < filtered.len() {
                            self.entry_idx += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(&real) = filtered.get(self.entry_idx) {
                            if let Some(target) = self.entries.get(real).cloned() {
                                self.navigate_to(target);
                            }
                        }
                    }
                    KeyCode::Char('.') => {
                        if let Some(account) = self.accounts.get(self.account_idx).cloned() {
                            return ModalOutcome::Confirm {
                                account,
                                cwd: self.browse_cwd.clone(),
                            };
                        }
                    }
                    _ => {}
                }
            }
        }
        ModalOutcome::Stay
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let modal_area = center_rect(area, 80, 70, 70, 22);
        f.render_widget(Clear, modal_area);

        let outer = Block::default()
            .borders(Borders::ALL)
            .title(" New session — Tab cycles focus · Esc cancel · . spawn ")
            .border_style(Style::default().fg(Color::Magenta));
        let inner = outer.inner(modal_area);
        f.render_widget(outer, modal_area);

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(inner);

        self.render_accounts(f, cols[0]);
        self.render_folder_pane(f, cols[1]);
    }

    fn render_accounts(&self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Account ")
            .border_style(focus_border(self.focus == ModalFocus::Accounts));
        let items: Vec<ListItem> = self
            .accounts
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let selected = i == self.account_idx;
                let name_style = if selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let arrow = if selected { "▶ " } else { "  " };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(arrow, name_style),
                        Span::styled(a.name.clone(), name_style),
                    ]),
                    Line::from(Span::styled(
                        format!("    {}", a.dir.display()),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
            })
            .collect();
        f.render_widget(List::new(items).block(block), area);
    }

    fn render_folder_pane(&self, f: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Folder ")
            .border_style(focus_border(
                self.focus == ModalFocus::PathBar
                    || self.focus == ModalFocus::Filter
                    || self.focus == ModalFocus::EntryList,
            ));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // path label
                Constraint::Length(1), // path input
                Constraint::Length(1), // filter label
                Constraint::Length(1), // filter input
                Constraint::Length(1), // error (or blank)
                Constraint::Min(2),    // entries
            ])
            .split(inner);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "path:",
                Style::default().fg(Color::DarkGray),
            ))),
            rows[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(input_spans(
                &self.path_input,
                self.focus == ModalFocus::PathBar,
                rows[1].width as usize,
            ))),
            rows[1],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "filter:",
                Style::default().fg(Color::DarkGray),
            ))),
            rows[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(input_spans(
                &self.filter,
                self.focus == ModalFocus::Filter,
                rows[3].width as usize,
            ))),
            rows[3],
        );

        if let Some(err) = &self.error {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    err.clone(),
                    Style::default().fg(Color::Red),
                ))),
                rows[4],
            );
        }

        let filtered = self.filtered_indices();
        let focused_list = self.focus == ModalFocus::EntryList;
        let visible_rows = rows[5].height as usize;
        let offset = if focused_list {
            self.entry_idx.saturating_sub(visible_rows.saturating_sub(1))
        } else {
            0
        };
        let items: Vec<ListItem> = filtered
            .iter()
            .skip(offset)
            .take(visible_rows.max(1))
            .enumerate()
            .map(|(visible_i, &real)| {
                let abs_i = visible_i + offset;
                let selected = focused_list && abs_i == self.entry_idx;
                let p = &self.entries[real];
                let label = entry_label(p);
                let is_parent = label == "..";
                let mut style = Style::default().fg(if is_parent {
                    Color::Cyan
                } else {
                    Color::White
                });
                if selected {
                    style = style.add_modifier(Modifier::BOLD).bg(Color::DarkGray);
                }
                let arrow = if selected { "▶ " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(arrow, style),
                    Span::styled(label, style),
                ]))
            })
            .collect();
        f.render_widget(List::new(items), rows[5]);
    }
}

fn focus_border(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn input_spans(text: &str, focused: bool, max_width: usize) -> Vec<Span<'static>> {
    let body_color = if focused { Color::White } else { Color::Gray };
    // Reserve one cell for the cursor block so the caret stays inside
    // the row even when the text exactly fills the available width.
    let budget = if focused {
        max_width.saturating_sub(1)
    } else {
        max_width
    };
    // Show the *tail* of the string so freshly-typed characters
    // remain visible. Without this, long paths overflow off the right
    // edge and the user's keystrokes appear to do nothing.
    let chars: Vec<char> = text.chars().collect();
    let visible: String = if chars.len() <= budget {
        chars.iter().collect()
    } else {
        chars
            .iter()
            .skip(chars.len() - budget)
            .copied()
            .collect()
    };
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        visible,
        Style::default().fg(body_color),
    )];
    if focused {
        spans.push(Span::styled(
            "█",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

fn center_rect(
    area: Rect,
    width_pct: u16,
    height_pct: u16,
    min_w: u16,
    min_h: u16,
) -> Rect {
    let w = ((area.width as u32 * width_pct as u32) / 100) as u16;
    let h = ((area.height as u32 * height_pct as u32) / 100) as u16;
    let w = w.max(min_w).min(area.width);
    let h = h.max(min_h).min(area.height);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn entry_label(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// Read `dir`, return `..` (when a parent exists) followed by the
/// directory entries sorted case-insensitively. Includes dotdirs.
/// Non-dir entries are skipped.
fn read_dirs(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut subs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let e = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let p = e.path();
        let is_dir = match e.file_type() {
            Ok(t) => t.is_dir() || (t.is_symlink() && p.is_dir()),
            Err(_) => false,
        };
        if is_dir {
            subs.push(p);
        }
    }
    subs.sort_by(|a, b| {
        let an = entry_label(a).to_lowercase();
        let bn = entry_label(b).to_lowercase();
        an.cmp(&bn)
    });
    let mut out: Vec<PathBuf> = Vec::with_capacity(subs.len() + 1);
    if dir.parent().is_some() {
        let mut parent = dir.to_path_buf();
        parent.push("..");
        out.push(parent);
    }
    out.extend(subs);
    Ok(out)
}

fn expand_tilde(s: &str) -> PathBuf {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}

/// Case-insensitive subsequence match. Returns `None` if `needle` is
/// not a subsequence of `haystack`. Higher score = better match;
/// rewards contiguous runs and early starts.
fn fuzzy_score(needle: &[char], haystack: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = haystack.chars().flat_map(|c| c.to_lowercase()).collect();
    let mut hi = 0usize;
    let mut score: usize = 0;
    let mut last_match: Option<usize> = None;
    let mut first_match: Option<usize> = None;
    for &n in needle {
        while hi < hay.len() && hay[hi] != n {
            hi += 1;
        }
        if hi >= hay.len() {
            return None;
        }
        if first_match.is_none() {
            first_match = Some(hi);
        }
        if let Some(prev) = last_match {
            if hi == prev + 1 {
                score += 10;
            }
        }
        last_match = Some(hi);
        hi += 1;
    }
    let early_bonus = 5usize.saturating_sub(first_match.unwrap_or(0));
    Some(score + early_bonus + needle.len())
}
