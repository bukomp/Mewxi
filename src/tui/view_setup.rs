//! View 4 — Config. One flat, navigable list of everything mewxi can
//! configure, grouped into sections:
//!
//! - Claude Code integration — per-account statusLine wiring + the
//!   background watcher service.
//! - Updates — self-update channel (release tags vs main branch),
//!   the startup prompt toggle, and an on-demand check/install row.
//! - Preferences — TUI behaviour toggles.
//!
//! Interaction model: ↑/↓ (or Tab) moves over actionable rows, Enter
//! performs the row's single contextual action, and the hint box under
//! the list spells out what Enter will do *before* the user presses
//! it. The old single-letter keys (`s`/`w`/`t`/`i`/`a`/`R`) still work
//! as shortcuts for the same actions.

use super::widgets::render_footer;
use crate::setup::{SetupSnapshot, StatusLineState, WatcherState};
use crate::update::{UpdateChannel, UpdateStatus};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

/// One actionable row in the Config list. `items()` defines the
/// canonical order; the key handler in `tui::mod` and the renderer
/// here both build the same list so selection indices always agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigItem {
    /// Index into `SetupSnapshot::accounts`.
    Account(usize),
    Watcher,
    UpdateChannel,
    UpdatePrompt,
    UpdateCheckNow,
    DefocusToggle,
}

/// Actionable rows, in display order. Accounts come first so the
/// existing account-oriented shortcuts (`s`, `i`) keep indexing
/// naturally.
pub fn items(snap: Option<&SetupSnapshot>) -> Vec<ConfigItem> {
    let n = snap.map(|s| s.accounts.len()).unwrap_or(0);
    let mut v: Vec<ConfigItem> = (0..n).map(ConfigItem::Account).collect();
    v.push(ConfigItem::Watcher);
    v.push(ConfigItem::UpdateChannel);
    v.push(ConfigItem::UpdatePrompt);
    v.push(ConfigItem::UpdateCheckNow);
    v.push(ConfigItem::DefocusToggle);
    v
}

/// Self-update state the renderer needs, owned by the TUI event loop.
pub struct UpdateUi<'a> {
    pub channel: UpdateChannel,
    pub prompt_enabled: bool,
    /// A background check is in flight right now.
    pub checking: bool,
    /// Most recent successful check this TUI run (or from cache).
    pub status: Option<&'a UpdateStatus>,
    /// Most recent check failure, if any.
    pub error: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    area: Rect,
    snap: Option<&SetupSnapshot>,
    selected: usize,
    last_message: Option<&str>,
    defocus_input_after_send: bool,
    update: &UpdateUi,
    setup_rect: &mut Option<Rect>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header summary
            Constraint::Min(8),    // settings list
            Constraint::Length(4), // action hint + last message
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_header(f, rows[0], snap, update);
    *setup_rect = Some(rows[1]);
    render_list(f, rows[1], snap, selected, defocus_input_after_send, update);
    render_info(f, rows[2], snap, selected, defocus_input_after_send, update, last_message);
    render_footer(
        f,
        rows[3],
        "4",
        "↑/↓ select · Enter action · a fix all · i ignore account · R rescan · Esc back",
    );
}

fn render_header(f: &mut Frame, area: Rect, snap: Option<&SetupSnapshot>, update: &UpdateUi) {
    let mut spans: Vec<Span> = vec![Span::styled(
        "Config",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    )];

    match snap {
        None => spans.push(Span::styled(
            "   loading…",
            Style::default().fg(Color::DarkGray),
        )),
        Some(s) => {
            let unwired = s.unwired_count();
            if unwired == 0 && s.watcher.is_ok() {
                spans.push(Span::styled(
                    "   ✓ all wired · watcher running",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ));
            } else {
                let mut parts: Vec<String> = Vec::new();
                if unwired > 0 {
                    parts.push(format!("{unwired} account(s) need wiring"));
                }
                if !s.watcher.is_ok() {
                    parts.push(format!("watcher {}", s.watcher.short()));
                }
                spans.push(Span::styled(
                    format!("   ⚠ {}", parts.join(" · ")),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    "  — press a to fix everything",
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
    }

    if update.checking {
        spans.push(Span::styled(
            "   · checking for updates…",
            Style::default().fg(Color::DarkGray),
        ));
    } else if let Some(st) = update.status.filter(|s| s.available) {
        spans.push(Span::styled(
            format!("   · ⬆ {} available", st.latest),
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ));
    }

    f.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

/// Build the list body: section headers (non-selectable) interleaved
/// with actionable rows. Returns one Line per screen row plus, in
/// parallel, the item index each line belongs to (None for headers /
/// spacers) so the renderer can place the selection arrow and keep the
/// selected row scrolled into view.
fn build_lines(
    snap: Option<&SetupSnapshot>,
    selected: usize,
    defocus: bool,
    update: &UpdateUi,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let mut lines: Vec<Line> = Vec::new();
    let mut owners: Vec<Option<usize>> = Vec::new();
    let list = items(snap);

    let header = |text: &str| {
        Line::from(Span::styled(
            format!(" {text}"),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
    };
    let push_header = |lines: &mut Vec<Line<'static>>, owners: &mut Vec<Option<usize>>, text: &str, first: bool| {
        if !first {
            lines.push(Line::from(""));
            owners.push(None);
        }
        lines.push(header(text));
        owners.push(None);
    };

    // Generic row: " ▶ label  state  extra" with the arrow + bold on
    // the selected one.
    let row = |idx: usize, label: String, state: Span<'static>, extra: String| -> Line<'static> {
        let is_sel = idx == selected;
        let arrow = if is_sel { " ▶ " } else { "   " };
        let mut spans = vec![
            Span::styled(
                arrow.to_string(),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{label:<24}"),
                if is_sel {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            ),
            state,
        ];
        if !extra.is_empty() {
            spans.push(Span::styled(
                format!("   {extra}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if is_sel {
            Line::from(spans).style(Style::default().add_modifier(Modifier::BOLD))
        } else {
            Line::from(spans)
        }
    };

    let bold = |text: String, color: Color| {
        Span::styled(text, Style::default().fg(color).add_modifier(Modifier::BOLD))
    };

    push_header(&mut lines, &mut owners, "Claude Code integration", true);
    for (i, item) in list.iter().enumerate() {
        match item {
            ConfigItem::Account(ai) => {
                let Some(a) = snap.and_then(|s| s.accounts.get(*ai)) else { continue };
                let (state_text, color) = if a.ignored {
                    ("ignored".to_string(), Color::DarkGray)
                } else {
                    match &a.statusline {
                        StatusLineState::Wired => ("✓ wired".to_string(), Color::Green),
                        StatusLineState::OtherCommand(_) => ("other command".to_string(), Color::Yellow),
                        StatusLineState::Missing => ("not wired".to_string(), Color::Red),
                        StatusLineState::Unreadable(why) => (format!("error: {why}"), Color::Red),
                    }
                };
                lines.push(row(
                    i,
                    format!("account · {}", a.account_name),
                    bold(format!("{state_text:<16}"), color),
                    a.settings_path.display().to_string(),
                ));
                owners.push(Some(i));
            }
            ConfigItem::Watcher => {
                let (state_text, color) = match snap.map(|s| &s.watcher) {
                    Some(WatcherState::Running) => ("✓ running".to_string(), Color::Green),
                    Some(WatcherState::Installed) => ("stopped".to_string(), Color::Yellow),
                    Some(WatcherState::NotInstalled) => ("not installed".to_string(), Color::Red),
                    Some(WatcherState::Unknown(why)) => (format!("unknown ({why})"), Color::DarkGray),
                    None => ("…".to_string(), Color::DarkGray),
                };
                lines.push(row(
                    i,
                    "background watcher".to_string(),
                    bold(format!("{state_text:<16}"), color),
                    "keeps the statusline fresh between sessions".to_string(),
                ));
                owners.push(Some(i));

                push_header(&mut lines, &mut owners, "Updates", false);
            }
            ConfigItem::UpdateChannel => {
                lines.push(row(
                    i,
                    "channel".to_string(),
                    bold(format!("{:<16}", update.channel.as_str()), Color::Magenta),
                    update.channel.label().to_string(),
                ));
                owners.push(Some(i));
            }
            ConfigItem::UpdatePrompt => {
                let (txt, color) = if update.prompt_enabled {
                    ("✓ on", Color::Green)
                } else {
                    ("off", Color::Yellow)
                };
                lines.push(row(
                    i,
                    "ask on startup".to_string(),
                    bold(format!("{txt:<16}"), color),
                    "offer available updates when the TUI opens".to_string(),
                ));
                owners.push(Some(i));
            }
            ConfigItem::UpdateCheckNow => {
                let (txt, color, extra) = if update.checking {
                    ("checking…".to_string(), Color::DarkGray, String::new())
                } else if let Some(e) = update.error {
                    ("check failed".to_string(), Color::Red, e.to_string())
                } else if let Some(st) = update.status {
                    if st.available {
                        (
                            format!("⬆ {} available", st.latest),
                            Color::Magenta,
                            st.detail.clone(),
                        )
                    } else {
                        ("✓ up to date".to_string(), Color::Green, st.detail.clone())
                    }
                } else {
                    ("not checked yet".to_string(), Color::DarkGray, String::new())
                };
                lines.push(row(i, "check for updates".to_string(), bold(format!("{txt:<16}"), color), extra));
                owners.push(Some(i));

                push_header(&mut lines, &mut owners, "Preferences", false);
            }
            ConfigItem::DefocusToggle => {
                let (txt, color) = if defocus { ("✓ on", Color::Green) } else { ("off", Color::Yellow) };
                lines.push(row(
                    i,
                    "defocus input after send".to_string(),
                    bold(format!("{txt:<16}"), color),
                    "unfocus the prompt box after sending".to_string(),
                ));
                owners.push(Some(i));
            }
        }
    }

    (lines, owners)
}

fn render_list(
    f: &mut Frame,
    area: Rect,
    snap: Option<&SetupSnapshot>,
    selected: usize,
    defocus: bool,
    update: &UpdateUi,
) {
    let block = Block::default().borders(Borders::ALL).title("Settings");
    if snap.is_none() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "loading setup state…",
                Style::default().fg(Color::DarkGray),
            )))
            .block(block),
            area,
        );
        return;
    }

    let (lines, owners) = build_lines(snap, selected, defocus, update);

    // Window the lines so the selected row is always visible.
    let visible_h = area.height.saturating_sub(2) as usize; // borders
    let sel_line = owners
        .iter()
        .position(|o| *o == Some(selected))
        .unwrap_or(0);
    let offset = if visible_h == 0 || sel_line < visible_h {
        0
    } else {
        sel_line + 1 - visible_h
    };
    let windowed: Vec<Line> = lines.into_iter().skip(offset).take(visible_h.max(1)).collect();

    f.render_widget(Paragraph::new(windowed).block(block), area);
}

/// What Enter will do on the selected row — shown before the user
/// presses it so no action is a surprise.
fn action_hint(
    snap: Option<&SetupSnapshot>,
    selected: usize,
    defocus: bool,
    update: &UpdateUi,
) -> String {
    let list = items(snap);
    let Some(item) = list.get(selected) else {
        return String::new();
    };
    match item {
        ConfigItem::Account(ai) => {
            let Some(a) = snap.and_then(|s| s.accounts.get(*ai)) else {
                return String::new();
            };
            if a.ignored {
                return format!(
                    "Enter / i: un-ignore {} — it is currently hidden from every view",
                    a.account_name
                );
            }
            match &a.statusline {
                StatusLineState::Wired => format!(
                    "Enter: remove mewxi's statusLine from {} · i: ignore this account",
                    a.settings_path.display()
                ),
                StatusLineState::OtherCommand(cmd) => format!(
                    "Enter: overwrite the existing statusLine ({cmd})"
                ),
                _ => format!(
                    "Enter: wire mewxi's statusLine into {}",
                    a.settings_path.display()
                ),
            }
        }
        ConfigItem::Watcher => match snap.map(|s| &s.watcher) {
            Some(WatcherState::Running) => {
                "Enter: stop + uninstall the background watcher service".to_string()
            }
            Some(WatcherState::Unknown(_)) | None => "no watcher action available".to_string(),
            _ => "Enter: install + start the background watcher (runs at login)".to_string(),
        },
        ConfigItem::UpdateChannel => format!(
            "Enter: switch to {} — release follows tagged versions, dev follows the main branch",
            update.channel.toggled().as_str()
        ),
        ConfigItem::UpdatePrompt => if update.prompt_enabled {
            "Enter: stop asking about updates when the TUI starts".to_string()
        } else {
            "Enter: ask about available updates when the TUI starts".to_string()
        },
        ConfigItem::UpdateCheckNow => {
            if update.checking {
                "checking origin — hold on…".to_string()
            } else if update.status.is_some_and(|s| s.available) {
                "Enter: install the update now (git + cargo rebuild, takes a minute)".to_string()
            } else {
                "Enter: check origin for a newer mewxi now".to_string()
            }
        }
        ConfigItem::DefocusToggle => if defocus {
            "Enter: keep the prompt box focused after sending (type follow-ups immediately)"
                .to_string()
        } else {
            "Enter: unfocus the prompt box after sending (keys go back to navigation)".to_string()
        },
    }
}

fn render_info(
    f: &mut Frame,
    area: Rect,
    snap: Option<&SetupSnapshot>,
    selected: usize,
    defocus: bool,
    update: &UpdateUi,
    last_message: Option<&str>,
) {
    let hint = action_hint(snap, selected, defocus, update);
    let msg_line = match last_message {
        Some(m) => Line::from(Span::styled(m.to_string(), Style::default().fg(Color::Cyan))),
        None => Line::from(Span::styled(
            "no actions taken yet",
            Style::default().fg(Color::DarkGray),
        )),
    };
    let body = vec![
        Line::from(Span::styled(hint, Style::default().fg(Color::Yellow))),
        msg_line,
    ];
    f.render_widget(
        Paragraph::new(body).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::AccountSetupState;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::path::PathBuf;

    fn snapshot() -> SetupSnapshot {
        SetupSnapshot {
            binary: PathBuf::from("/usr/local/bin/mewxi"),
            accounts: vec![
                AccountSetupState {
                    account_name: "default".into(),
                    settings_path: PathBuf::from("/home/u/.claude/settings.json"),
                    statusline: StatusLineState::Wired,
                    ignored: false,
                },
                AccountSetupState {
                    account_name: "priv".into(),
                    settings_path: PathBuf::from("/home/u/.claude-priv/settings.json"),
                    statusline: StatusLineState::Missing,
                    ignored: false,
                },
            ],
            watcher: WatcherState::Running,
        }
    }

    fn update_ui<'a>(status: Option<&'a UpdateStatus>) -> UpdateUi<'a> {
        UpdateUi {
            channel: UpdateChannel::Release,
            prompt_enabled: true,
            checking: false,
            status,
            error: None,
        }
    }

    #[test]
    fn items_order_accounts_first_then_fixed_rows() {
        let snap = snapshot();
        let list = items(Some(&snap));
        assert_eq!(list[0], ConfigItem::Account(0));
        assert_eq!(list[1], ConfigItem::Account(1));
        assert_eq!(list[2], ConfigItem::Watcher);
        assert_eq!(list[3], ConfigItem::UpdateChannel);
        assert_eq!(list[4], ConfigItem::UpdatePrompt);
        assert_eq!(list[5], ConfigItem::UpdateCheckNow);
        assert_eq!(list[6], ConfigItem::DefocusToggle);
        // No snapshot yet → only the fixed rows.
        assert_eq!(items(None).len(), 5);
    }

    fn render_to_text(selected: usize, status: Option<UpdateStatus>) -> String {
        let snap = snapshot();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let mut rect = None;
                render(
                    f,
                    f.area(),
                    Some(&snap),
                    selected,
                    Some("did a thing"),
                    true,
                    &update_ui(status.as_ref()),
                    &mut rect,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_all_sections_and_rows() {
        let text = render_to_text(0, None);
        for needle in [
            "Claude Code integration",
            "account · default",
            "account · priv",
            "background watcher",
            "Updates",
            "channel",
            "ask on startup",
            "check for updates",
            "Preferences",
            "defocus input after send",
            "did a thing",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        // Selected row 0 (wired account) hints at unwiring.
        assert!(text.contains("remove mewxi's statusLine"), "hint missing:\n{text}");
    }

    #[test]
    fn available_update_surfaces_in_header_and_hint() {
        let status = UpdateStatus {
            channel: UpdateChannel::Release,
            available: true,
            current: "v0.1.0".into(),
            latest: "v0.2.0".into(),
            detail: "tag v0.2.0 is newer than v0.1.0".into(),
        };
        // Select the check-now row (index 5 with two accounts).
        let text = render_to_text(5, Some(status));
        assert!(text.contains("⬆ v0.2.0 available"), "header notice missing:\n{text}");
        assert!(text.contains("install the update now"), "hint missing:\n{text}");
    }
}
