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
//!  - Right pane: combined editable path bar + filtered list of
//!    subdirectories. The path bar doubles as the fuzzy filter — text
//!    up to the last `/` is the directory we list, and the tail after
//!    the last `/` is the live fuzzy filter applied to that directory's
//!    entries. Pressing Enter on the path bar drills into the
//!    highlighted entry (or spawns when there is no tail to complete).
//!
//! Focus rotates Accounts → PathBar → EntryList (Tab forward,
//! Shift-Tab backward). `.` from any focus confirms and spawns under
//! the currently-resolved directory.

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
    EntryList,
}

pub struct NewSessionModal {
    accounts: Vec<Account>,
    account_idx: usize,
    /// Editable text. The leading part up to the last `/` is the
    /// directory whose contents `entries` was built from; the trailing
    /// fragment after the last `/` is the live fuzzy filter applied
    /// over `entries`.
    path_input: String,
    /// Resolved directory whose contents `entries` was built from.
    /// Stays in sync with [`Self::derived_dir`]; updated by
    /// [`Self::refresh_entries_if_dir_changed`].
    browse_cwd: PathBuf,
    /// Inline error from a failed path-bar resolution or read_dir; one
    /// line, cleared on the next successful navigation.
    error: Option<String>,
    /// Directory entries of `browse_cwd` (just `..` plus subdirs).
    /// Dotfiles included.
    entries: Vec<PathBuf>,
    /// Index into the *filtered* entry list (the live fuzzy match over
    /// `entries`).
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
        // Trailing slash on the path input keeps the dir/tail split
        // well-defined: the tail is empty (no filter), and any chars
        // the user types next become the filter for `browse_cwd`.
        let mut path_input = browse_cwd.to_string_lossy().into_owned();
        if !path_input.ends_with('/') {
            path_input.push('/');
        }
        Self {
            accounts,
            account_idx,
            path_input,
            browse_cwd,
            error,
            entries,
            entry_idx: 0,
            // Start on the path bar — the cursor is visibly there so
            // the user immediately knows where typing goes.
            focus: ModalFocus::PathBar,
        }
    }

    /// Directory portion of `path_input` — everything up to and
    /// including the last `/`. `~` is expanded.
    fn derived_dir(&self) -> PathBuf {
        let cut = self.path_input.rfind('/').map(|i| i + 1).unwrap_or(0);
        expand_tilde(&self.path_input[..cut])
    }

    /// Fragment after the last `/` of `path_input`. This is the live
    /// fuzzy filter applied to the entries of [`Self::derived_dir`].
    fn derived_filter(&self) -> &str {
        let cut = self.path_input.rfind('/').map(|i| i + 1).unwrap_or(0);
        &self.path_input[cut..]
    }

    /// Indices of `entries` that match the live fuzzy filter
    /// ([`Self::derived_filter`]). Empty filter passes every entry.
    fn filtered_indices(&self) -> Vec<usize> {
        let filter = self.derived_filter();
        if filter.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let q: Vec<char> = filter.chars().flat_map(|c| c.to_lowercase()).collect();
        let mut scored: Vec<(usize, usize)> = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            let name = entry_label(e);
            // `..` should never match a typed filter — listing the
            // parent dir while the user is narrowing on a name is
            // surprising. Skip it whenever the filter is non-empty.
            if name == ".." {
                continue;
            }
            if let Some(score) = fuzzy_score(&q, &name) {
                scored.push((i, score));
            }
        }
        scored.sort_by_key(|&(_, s)| std::cmp::Reverse(s));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    /// If the dir portion of `path_input` differs from `browse_cwd`,
    /// re-read entries and reset the highlight. Called after every
    /// path_input mutation so the entry list always reflects the
    /// currently-typed prefix.
    fn refresh_entries_if_dir_changed(&mut self) {
        let new_dir = self.derived_dir();
        if new_dir == self.browse_cwd {
            // Same dir, only the filter changed — keep entries, just
            // reset highlight so the top filtered match is selected.
            self.entry_idx = 0;
            return;
        }
        match read_dirs(&new_dir) {
            Ok(es) => {
                self.browse_cwd = new_dir;
                self.entries = es;
                self.error = None;
            }
            Err(e) => {
                // Keep the old browse_cwd/entries around so the list
                // doesn't blank out mid-typing on a not-yet-valid
                // prefix. Surface the failure only when the user
                // actively tries to navigate (Enter).
                self.error = Some(format!("read {}: {}", new_dir.display(), e));
            }
        }
        self.entry_idx = 0;
    }

    /// Replace the path_input with `dir`'s display string + trailing
    /// `/`, then refresh entries.
    fn navigate_to(&mut self, dir: PathBuf) {
        let canonical = dir.canonicalize().unwrap_or(dir);
        let mut s = canonical.to_string_lossy().into_owned();
        if !s.ends_with('/') {
            s.push('/');
        }
        self.path_input = s;
        self.refresh_entries_if_dir_changed();
    }

    /// Apply the top filtered entry to the path: drill into it. Sets
    /// path_input to `<dir>/<entry_name>/` and refreshes. No-op when
    /// the filtered list is empty.
    fn complete_to_highlighted(&mut self) -> bool {
        let filtered = self.filtered_indices();
        let Some(&real) = filtered.get(self.entry_idx) else {
            return false;
        };
        let Some(target) = self.entries.get(real).cloned() else {
            return false;
        };
        self.navigate_to(target);
        true
    }

    fn cycle_focus(&mut self, forward: bool) {
        self.focus = match (self.focus, forward) {
            (ModalFocus::Accounts, true) => ModalFocus::PathBar,
            (ModalFocus::PathBar, true) => ModalFocus::EntryList,
            (ModalFocus::EntryList, true) => ModalFocus::Accounts,
            (ModalFocus::Accounts, false) => ModalFocus::EntryList,
            (ModalFocus::PathBar, false) => ModalFocus::Accounts,
            (ModalFocus::EntryList, false) => ModalFocus::PathBar,
        };
    }

    fn confirm_spawn(&self) -> ModalOutcome {
        if let Some(account) = self.accounts.get(self.account_idx).cloned() {
            return ModalOutcome::Confirm {
                account,
                cwd: self.browse_cwd.clone(),
            };
        }
        ModalOutcome::Stay
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
                    self.focus = ModalFocus::PathBar;
                }
                KeyCode::Char('.') => return self.confirm_spawn(),
                _ => {}
            },
            ModalFocus::PathBar => match (k.code, k.modifiers) {
                (KeyCode::Enter, _) => {
                    // Two modes:
                    //  - Filter tail is non-empty → complete to the
                    //    highlighted entry (typical: typed `Wo`, Enter
                    //    completes to `Work/`).
                    //  - Filter tail is empty → user has the bare dir
                    //    selected; treat Enter as confirm/spawn so the
                    //    keypress is never wasted.
                    let filter_empty = self.derived_filter().is_empty();
                    if filter_empty {
                        // Verify the dir actually exists before
                        // spawning under a typo.
                        let typed = expand_tilde(self.path_input.trim_end_matches('/'));
                        if typed.is_dir() || self.browse_cwd.is_dir() {
                            return self.confirm_spawn();
                        }
                        self.error =
                            Some(format!("not a directory: {}", typed.display()));
                    } else if !self.complete_to_highlighted() {
                        self.error = Some(format!(
                            "no match for `{}` in {}",
                            self.derived_filter(),
                            self.browse_cwd.display()
                        ));
                    }
                }
                (KeyCode::Down, _) => {
                    let filtered = self.filtered_indices();
                    if self.entry_idx + 1 < filtered.len() {
                        self.entry_idx += 1;
                    }
                }
                (KeyCode::Up, _) => {
                    if self.entry_idx > 0 {
                        self.entry_idx -= 1;
                    }
                }
                (KeyCode::Backspace, _) => {
                    self.path_input.pop();
                    self.refresh_entries_if_dir_changed();
                }
                (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.path_input.clear();
                    self.refresh_entries_if_dir_changed();
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
                    self.refresh_entries_if_dir_changed();
                }
                (KeyCode::Char('.'), m) if m.contains(KeyModifiers::CONTROL) => {
                    return self.confirm_spawn();
                }
                (KeyCode::Char(c), m)
                    if !m.contains(KeyModifiers::CONTROL)
                        && !m.contains(KeyModifiers::ALT) =>
                {
                    self.path_input.push(c);
                    self.refresh_entries_if_dir_changed();
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
                        self.complete_to_highlighted();
                    }
                    KeyCode::Char('.') => return self.confirm_spawn(),
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
                self.focus == ModalFocus::PathBar || self.focus == ModalFocus::EntryList,
            ));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // path label
                Constraint::Length(1), // path input (doubles as filter)
                Constraint::Length(1), // error (or blank)
                Constraint::Min(2),    // entries
            ])
            .split(inner);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "path / filter:",
                Style::default().fg(Color::DarkGray),
            ))),
            rows[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(input_spans_split(
                self.dir_part(),
                self.derived_filter(),
                self.focus == ModalFocus::PathBar,
                rows[1].width as usize,
            ))),
            rows[1],
        );

        if let Some(err) = &self.error {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    err.clone(),
                    Style::default().fg(Color::Red),
                ))),
                rows[2],
            );
        }

        let filtered = self.filtered_indices();
        let list_focused =
            self.focus == ModalFocus::EntryList || self.focus == ModalFocus::PathBar;
        let visible_rows = rows[3].height as usize;
        let offset = if list_focused {
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
                let selected = list_focused && abs_i == self.entry_idx;
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
        f.render_widget(List::new(items), rows[3]);
    }

    /// String slice of the dir portion of `path_input` (everything up
    /// to and including the last `/`). Used by the renderer to colour
    /// dir and filter halves differently.
    fn dir_part(&self) -> &str {
        let cut = self.path_input.rfind('/').map(|i| i + 1).unwrap_or(0);
        &self.path_input[..cut]
    }
}

fn focus_border(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Render the combined path/filter input as two colour bands:
/// the dim dir portion followed by the bright tail (the live filter).
/// Long inputs are tail-trimmed so the cursor stays on screen.
fn input_spans_split(
    dir: &str,
    filter: &str,
    focused: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let budget = if focused {
        max_width.saturating_sub(1)
    } else {
        max_width
    };
    let combined: String = format!("{dir}{filter}");
    let chars: Vec<char> = combined.chars().collect();
    let (start, visible_dir_len) = if chars.len() <= budget {
        (0, dir.chars().count())
    } else {
        // Trim from the head so the typed tail stays visible. Recompute
        // how much of the dir portion survives the trim.
        let start = chars.len() - budget;
        let dir_chars = dir.chars().count();
        let visible_dir = dir_chars.saturating_sub(start);
        (start, visible_dir)
    };
    let visible: String = chars.iter().skip(start).copied().collect();
    let visible_chars: Vec<char> = visible.chars().collect();
    let dir_seg: String = visible_chars.iter().take(visible_dir_len).collect();
    let filter_seg: String = visible_chars.iter().skip(visible_dir_len).collect();

    let dir_color = if focused { Color::Gray } else { Color::DarkGray };
    let filter_color = if focused { Color::Yellow } else { Color::Gray };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(dir_seg, Style::default().fg(dir_color)),
        Span::styled(
            filter_seg,
            Style::default().fg(filter_color).add_modifier(Modifier::BOLD),
        ),
    ];
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
