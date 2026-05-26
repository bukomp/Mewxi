//! View 4 — Setup. Per-account statusLine wiring + background watcher
//! service, all from inside the TUI so the user never has to touch a
//! shell or the `setup` subcommand.

use super::widgets::render_footer;
use crate::setup::{SetupSnapshot, StatusLineState, WatcherState};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

pub fn render(
    f: &mut Frame,
    area: Rect,
    snap: Option<&SetupSnapshot>,
    selected: usize,
    last_message: Option<&str>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(6),     // accounts table
            Constraint::Length(3),  // watcher row
            Constraint::Length(3),  // last-action message
            Constraint::Length(1),  // footer
        ])
        .split(area);

    render_header(f, rows[0], snap);
    match snap {
        Some(s) => {
            render_accounts(f, rows[1], s, selected);
            render_watcher(f, rows[2], s);
        }
        None => {
            let p = Paragraph::new(Line::from(Span::styled(
                "loading setup state…",
                Style::default().fg(Color::DarkGray),
            )))
            .block(Block::default().borders(Borders::ALL));
            f.render_widget(p, rows[1]);
            f.render_widget(
                Paragraph::new("").block(Block::default().borders(Borders::ALL)),
                rows[2],
            );
        }
    }
    render_message(f, rows[3], last_message);
    render_footer(f, rows[4], "4", "↑/↓ select · s wire/unwire · i ignore · w toggle watcher · a apply all · R recheck · Esc back");
}

fn render_header(f: &mut Frame, area: Rect, snap: Option<&SetupSnapshot>) {
    let banner = match snap {
        None => Line::from(vec![Span::styled(
            "Setup",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )]),
        Some(s) if s.fully_ok() => Line::from(vec![
            Span::styled(
                "Setup",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                "✓ everything wired and watcher running",
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
        ]),
        Some(s) => {
            let unwired = s.unwired_count();
            let watcher_ok = s.watcher.is_ok();
            let msg = match (unwired, watcher_ok) {
                (0, true) => "✓ all set".to_string(),
                (n, true) => format!("{n} account(s) need wiring"),
                (0, false) => format!("watcher {}", s.watcher.short()),
                (n, false) => {
                    format!("{n} account(s) need wiring · watcher {}", s.watcher.short())
                }
            };
            Line::from(vec![
                Span::styled(
                    "Setup",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(msg, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::raw("   "),
                Span::styled(
                    "(press `a` to apply everything)",
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        }
    };
    f.render_widget(
        Paragraph::new(banner).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_accounts(f: &mut Frame, area: Rect, snap: &SetupSnapshot, selected: usize) {
    if snap.accounts.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no accounts",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::default().borders(Borders::ALL).title("Accounts (statusLine)"));
        f.render_widget(p, area);
        return;
    }

    let rows: Vec<Row> = snap
        .accounts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let arrow = if i == selected { "▶ " } else { "  " };
            let (status_text, status_color) = if a.ignored {
                ("— ignored —".to_string(), Color::DarkGray)
            } else {
                match &a.statusline {
                    StatusLineState::Wired => ("✓ wired".to_string(), Color::Green),
                    StatusLineState::OtherCommand(_) => ("other command".to_string(), Color::Yellow),
                    StatusLineState::Missing => ("missing".to_string(), Color::Red),
                    StatusLineState::Unreadable(why) => (format!("error: {why}"), Color::Red),
                }
            };
            let name_color = if a.ignored { Color::DarkGray } else { Color::Magenta };
            let row_style = if i == selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else if a.ignored {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(format!("{arrow}{}", a.account_name))
                    .style(Style::default().fg(name_color).add_modifier(Modifier::BOLD)),
                Cell::from(status_text).style(Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
                Cell::from(a.settings_path.display().to_string())
                    .style(Style::default().fg(Color::DarkGray)),
            ])
            .style(row_style)
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(20),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec!["account", "statusLine", "settings.json"])
            .style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title("Accounts (s wire/unwire · i ignore/un-ignore)"));
    f.render_widget(table, area);
}

fn render_watcher(f: &mut Frame, area: Rect, snap: &SetupSnapshot) {
    let (state_text, state_color) = match &snap.watcher {
        WatcherState::Running => ("✓ running".to_string(), Color::Green),
        WatcherState::Installed => ("installed but stopped".to_string(), Color::Yellow),
        WatcherState::NotInstalled => ("not installed".to_string(), Color::Red),
        WatcherState::Unknown(why) => (format!("unknown ({why})"), Color::DarkGray),
    };
    let action_hint = match &snap.watcher {
        WatcherState::Running => "press w to stop + uninstall",
        WatcherState::Installed | WatcherState::NotInstalled => "press w to install + start",
        WatcherState::Unknown(_) => "(no action available)",
    };
    let line = Line::from(vec![
        Span::styled(
            "Watcher  ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(state_text, Style::default().fg(state_color).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled(action_hint, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title("Background watcher (user service)")),
        area,
    );
}

fn render_message(f: &mut Frame, area: Rect, msg: Option<&str>) {
    let line = match msg {
        Some(m) => Line::from(Span::styled(m.to_string(), Style::default().fg(Color::Cyan))),
        None => Line::from(Span::styled(
            "no actions taken yet — try `a` to wire everything that's missing",
            Style::default().fg(Color::DarkGray),
        )),
    };
    f.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title("Last action")),
        area,
    );
}
