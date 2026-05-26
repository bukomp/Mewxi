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
//! Focus rotates Accounts → Recent → PathBar → EntryList (Tab forward,
//! Shift-Tab backward). The Recent pane lists directories the selected
//! account has been used in before — pulled from its `projects/`
//! subtree — so the common case ("spawn another session in a project
//! I've already worked on") doesn't require typing or browsing. `.`
//! from any focus confirms and spawns under the currently-resolved
//! directory.

use crate::accounts::{self, Account};
use crate::tui::text_input::{EditOutcome, TextInput};
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
    Recent,
    PathBar,
    EntryList,
}

/// How many "previously used" directories to surface per account.
/// Big enough to cover real workflow breadth, small enough that the
/// pane stays scannable without scrolling on a min-height modal.
const RECENT_LIMIT: usize = 20;

pub struct NewSessionModal {
    accounts: Vec<Account>,
    account_idx: usize,
    /// Editable text. The leading part up to the last `/` is the
    /// directory whose contents `entries` was built from; the trailing
    /// fragment after the last `/` is the live fuzzy filter applied
    /// over `entries`. Backed by [`TextInput`] for readline-style
    /// cursor movement and edit shortcuts.
    path_input: TextInput,
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
    /// Directories the currently-selected account has spawned sessions
    /// in before, newest first. Refreshed whenever `account_idx` moves.
    recent: Vec<PathBuf>,
    recent_idx: usize,
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
        let mut path_str = browse_cwd.to_string_lossy().into_owned();
        if !path_str.ends_with('/') {
            path_str.push('/');
        }
        let path_input = TextInput::from_str(&path_str);
        let recent = accounts
            .get(account_idx)
            .map(|a| accounts::recent_project_dirs(a, RECENT_LIMIT))
            .unwrap_or_default();
        // When the account has history, land on Recent so the most
        // common action ("re-enter a project I just used") is a single
        // Enter away. With no history, fall through to the path bar
        // where typing/browsing is the only path forward.
        let focus = if recent.is_empty() {
            ModalFocus::PathBar
        } else {
            ModalFocus::Recent
        };
        Self {
            accounts,
            account_idx,
            path_input,
            browse_cwd,
            error,
            entries,
            entry_idx: 0,
            recent,
            recent_idx: 0,
            focus,
        }
    }

    /// Repopulate the recent-projects list after the selected account
    /// changes. Resets the highlight to the top of the new list.
    fn refresh_recent(&mut self) {
        self.recent = self
            .accounts
            .get(self.account_idx)
            .map(|a| accounts::recent_project_dirs(a, RECENT_LIMIT))
            .unwrap_or_default();
        self.recent_idx = 0;
    }

    /// Directory portion of `path_input` — everything up to and
    /// including the last `/`. `~` is expanded.
    fn derived_dir(&self) -> PathBuf {
        let s = self.path_input.as_str();
        let cut = s.rfind('/').map(|i| i + 1).unwrap_or(0);
        expand_tilde(&s[..cut])
    }

    /// Fragment after the last `/` of `path_input`. This is the live
    /// fuzzy filter applied to the entries of [`Self::derived_dir`].
    fn derived_filter(&self) -> &str {
        let s = self.path_input.as_str();
        let cut = s.rfind('/').map(|i| i + 1).unwrap_or(0);
        &s[cut..]
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
        self.path_input.set(s);
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
        // Recent is skipped on accounts with no history — landing on
        // an empty pane is a dead-end the user can't act on.
        let order: &[ModalFocus] = if self.recent.is_empty() {
            &[
                ModalFocus::Accounts,
                ModalFocus::PathBar,
                ModalFocus::EntryList,
            ]
        } else {
            &[
                ModalFocus::Accounts,
                ModalFocus::Recent,
                ModalFocus::PathBar,
                ModalFocus::EntryList,
            ]
        };
        let cur = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (cur + 1) % order.len()
        } else {
            (cur + order.len() - 1) % order.len()
        };
        self.focus = order[next];
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
                        self.refresh_recent();
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.account_idx + 1 < self.accounts.len() {
                        self.account_idx += 1;
                        self.refresh_recent();
                    }
                }
                KeyCode::Enter => {
                    // After picking an account, advance to the most
                    // useful next step: Recent if there's history to
                    // pick from, otherwise the path bar to type/browse.
                    self.focus = if self.recent.is_empty() {
                        ModalFocus::PathBar
                    } else {
                        ModalFocus::Recent
                    };
                }
                KeyCode::Char('.') => return self.confirm_spawn(),
                _ => {}
            },
            ModalFocus::Recent => match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.recent_idx > 0 {
                        self.recent_idx -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.recent_idx + 1 < self.recent.len() {
                        self.recent_idx += 1;
                    }
                }
                KeyCode::Enter => {
                    // Two-step pattern (load into path bar, don't
                    // auto-spawn) keeps the user one keystroke from
                    // either: confirming with `.`, narrowing into a
                    // subdir, or aborting. Spawning immediately on
                    // Enter would be surprising in a list that scrolls
                    // and could trigger the wrong session on a mis-key.
                    if let Some(dir) = self.recent.get(self.recent_idx).cloned() {
                        if dir.is_dir() {
                            self.navigate_to(dir);
                            self.focus = ModalFocus::PathBar;
                        } else {
                            self.error = Some(format!(
                                "not a directory (moved/deleted?): {}",
                                dir.display()
                            ));
                        }
                    }
                }
                KeyCode::Char('.') => {
                    // `.` shortcut: pick + spawn in one go.
                    if let Some(dir) = self.recent.get(self.recent_idx).cloned() {
                        if dir.is_dir() {
                            self.navigate_to(dir);
                            return self.confirm_spawn();
                        }
                        self.error = Some(format!(
                            "not a directory (moved/deleted?): {}",
                            dir.display()
                        ));
                    }
                }
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
                        let typed = expand_tilde(self.path_input.as_str().trim_end_matches('/'));
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
                (KeyCode::Char('.'), m) if m.contains(KeyModifiers::CONTROL) => {
                    return self.confirm_spawn();
                }
                // All other keys (chars, Backspace, Delete, arrows,
                // Home/End, Ctrl-A/E/W/U/K/H, Alt-B/F/D) go through the
                // shared readline editor. `/` is a word boundary under
                // its alnum+`_` rule, so Ctrl-W still deletes the
                // trailing path segment.
                _ => {
                    if let EditOutcome::Consumed { changed: true } =
                        self.path_input.handle_edit_key(k)
                    {
                        self.refresh_entries_if_dir_changed();
                    }
                }
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
        let modal_area = center_rect(area, 85, 75, 78, 24);
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

        // Right side: Recent (top) + Folder (bottom). Recent is sized
        // to fit the list snugly (cap at ~8 rows + chrome) so the
        // folder browser keeps the bulk of the height even when the
        // account has dozens of historical projects.
        let recent_rows = (self.recent.len().min(8) as u16) + 2; // +borders
        let recent_h = recent_rows.max(3); // always show at least the empty-state line
        let right_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(recent_h), Constraint::Min(6)])
            .split(cols[1]);
        self.render_recent_pane(f, right_split[0]);
        self.render_folder_pane(f, right_split[1]);
    }

    fn render_recent_pane(&self, f: &mut Frame, area: Rect) {
        let title = format!(
            " Recent — {} project{} ",
            self.recent.len(),
            if self.recent.len() == 1 { "" } else { "s" }
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(focus_border(self.focus == ModalFocus::Recent));
        let inner = block.inner(area);
        f.render_widget(block, area);

        if self.recent.is_empty() {
            let msg = if self.accounts.is_empty() {
                "(no account selected)"
            } else {
                "(no previous sessions for this account)"
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    msg,
                    Style::default().fg(Color::DarkGray),
                ))),
                inner,
            );
            return;
        }

        let visible_rows = inner.height as usize;
        let focused = self.focus == ModalFocus::Recent;
        let offset = if focused {
            self.recent_idx.saturating_sub(visible_rows.saturating_sub(1))
        } else {
            0
        };
        let home = dirs::home_dir();
        let items: Vec<ListItem> = self
            .recent
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible_rows.max(1))
            .map(|(i, p)| {
                let selected = focused && i == self.recent_idx;
                let label = display_recent(p, home.as_deref());
                let mut style = Style::default().fg(Color::White);
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
        f.render_widget(List::new(items), inner);
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
                self.path_input.cursor_char(),
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
        let s = self.path_input.as_str();
        let cut = s.rfind('/').map(|i| i + 1).unwrap_or(0);
        &s[..cut]
    }
}

fn focus_border(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

/// Render the combined path/filter input as two colour bands (dim
/// dir + bright filter) with the edit cursor drawn at its actual
/// position. `cursor_char` is a character index into `dir + filter`.
/// Long inputs scroll horizontally so the cursor stays on-screen.
fn input_spans_split(
    dir: &str,
    filter: &str,
    cursor_char: usize,
    focused: bool,
    max_width: usize,
) -> Vec<Span<'static>> {
    let dir_chars: Vec<char> = dir.chars().collect();
    let filter_chars: Vec<char> = filter.chars().collect();
    let total_len = dir_chars.len() + filter_chars.len();
    let dir_len = dir_chars.len();

    // Reserve one column for the trailing block when focused + cursor
    // at end. Mid-string cursor overlays a char so no reservation.
    let cursor_at_end = cursor_char >= total_len;
    let budget = if focused && cursor_at_end {
        max_width.saturating_sub(1)
    } else {
        max_width
    };
    let budget = budget.max(1);

    // Scroll window: keep the cursor inside [start, start+budget).
    let start = if total_len <= budget {
        0
    } else if cursor_char >= budget {
        // Bias so the cursor sits a couple columns from the right edge
        // when scrolled — leaves room to see what comes next on
        // backward edits without snapping the view.
        (cursor_char + 1).saturating_sub(budget)
    } else {
        0
    };

    let dir_color = if focused { Color::Gray } else { Color::DarkGray };
    let filter_color = if focused { Color::Yellow } else { Color::Gray };
    let dir_style = Style::default().fg(dir_color);
    let filter_style = Style::default().fg(filter_color).add_modifier(Modifier::BOLD);

    // Helper to style a char by its absolute index (dir vs filter).
    let style_at = |abs: usize| if abs < dir_len { dir_style } else { filter_style };
    // Build a visible char list with original absolute indices preserved
    // so we can split around the cursor and keep dir/filter coloring.
    let visible: Vec<(usize, char)> = dir_chars
        .iter()
        .chain(filter_chars.iter())
        .copied()
        .enumerate()
        .skip(start)
        .take(budget)
        .collect();

    let mut spans: Vec<Span<'static>> = Vec::new();
    let cursor_style = Style::default()
        .bg(Color::Yellow)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);

    // Group consecutive visible chars by (style, is_cursor) into runs
    // so the rendered line stays compact.
    let mut run: String = String::new();
    let mut run_style: Option<Style> = None;
    let flush = |s: &mut String, style: &mut Option<Style>, out: &mut Vec<Span<'static>>| {
        if !s.is_empty() {
            if let Some(st) = *style {
                out.push(Span::styled(std::mem::take(s), st));
            }
            *style = None;
        }
    };

    for (abs, c) in &visible {
        let style = if focused && *abs == cursor_char {
            cursor_style
        } else {
            style_at(*abs)
        };
        if run_style.is_some_and(|s| s == style) {
            run.push(*c);
        } else {
            flush(&mut run, &mut run_style, &mut spans);
            run.push(*c);
            run_style = Some(style);
        }
    }
    flush(&mut run, &mut run_style, &mut spans);

    if focused && cursor_at_end {
        spans.push(Span::styled("█", filter_style));
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

/// Compact display for a recent-project path: collapse the user's
/// home dir to `~` so the typical `/home/user/Work/foo` doesn't
/// dominate the column.
fn display_recent(p: &Path, home: Option<&Path>) -> String {
    if let Some(h) = home {
        if let Ok(rel) = p.strip_prefix(h) {
            return format!("~/{}", rel.display());
        }
    }
    p.display().to_string()
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
