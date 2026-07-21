//! `?` help modal — lists the shortcuts that are actually available in
//! the current view/state. The caller (mod.rs event loop) builds a
//! [`HelpCtx`] snapshot and opens the modal with
//! `HelpModal::new(sections_for(&ctx))`; the modal itself is dumb — it
//! only renders the sections it was given and closes on `Esc`/`?`/`q`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

/// Which top-level view the modal was opened from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewKind {
    AllSessions,
    SessionDetail,
    AccountDetail,
    Setup,
    Mewxi,
}

/// Session-view flags that decide which driving shortcuts apply.
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionFlags {
    /// True when the pinned session lives in the drivers registry —
    /// i.e. mewxi spawned this claude process and can manage it.
    pub driven: bool,
    /// True while the driver input row has keyboard focus.
    pub input_focused: bool,
    /// True while claude's own picker/prompt overlay is passing keys
    /// through.
    pub overlay_active: bool,
}

/// Snapshot of everything that decides shortcut availability.
#[derive(Clone, Copy, Debug)]
pub struct HelpCtx {
    pub view: ViewKind,
    /// Any sessions listed at all (view 1 selection/Enter need one).
    pub has_sessions: bool,
    /// Views 1 and 5 (both carry the sessions table): the
    /// currently-selected row is a mewxi-driven session (enables
    /// `Del` kill there).
    pub selected_driven: bool,
    /// View 2 only: pinned-session flags. `None` when nothing pinned.
    pub session: Option<SessionFlags>,
}

/// One titled group of `(keys, action)` rows.
pub struct HelpSection {
    pub title: String,
    pub entries: Vec<(String, String)>,
}

pub enum HelpOutcome {
    Stay,
    Close,
}

/// Pure: compute the sections the modal should show for a context.
/// Only shortcuts that actually work in that state may appear — this is
/// the whole point of the feature, so treat every branch below as load
/// bearing rather than cosmetic.
pub fn sections_for(ctx: &HelpCtx) -> Vec<HelpSection> {
    // The overlay/input-focus states swallow almost every key (claude's
    // own picker or the input row owns the keyboard), so the truthful
    // shortcut list there is just the one or two keys that still work —
    // return that alone rather than pretending the rest of the app's
    // bindings are reachable.
    if ctx.view == ViewKind::SessionDetail {
        if let Some(session) = ctx.session {
            if session.overlay_active {
                return vec![HelpSection {
                    title: "Session".into(),
                    entries: vec![("F10".into(), "dismiss overlay".into())],
                }];
            }
            if session.input_focused {
                return vec![HelpSection {
                    title: "Session".into(),
                    entries: vec![
                        ("Esc".into(), "unfocus input".into()),
                        ("Enter".into(), "send".into()),
                    ],
                }];
            }
        }
    }

    let mut sections = Vec::new();

    match ctx.view {
        ViewKind::AllSessions => {
            let mut entries = Vec::new();
            if ctx.has_sessions {
                entries.push(("↑/↓".into(), "select session".into()));
                entries.push(("Tab / Shift-Tab".into(), "cycle sessions".into()));
                entries.push(("Enter".into(), "open session".into()));
            }
            if ctx.selected_driven {
                entries.push(("Del".into(), "kill selected session".into()));
            }
            // Nothing selectable and nothing killable — omit the section
            // rather than show an empty titled box.
            if !entries.is_empty() {
                sections.push(HelpSection { title: "Sessions".into(), entries });
            }
        }
        ViewKind::SessionDetail => {
            // Common nav — available whenever a session view is showing,
            // pinned or not, driven or observed.
            sections.push(HelpSection {
                title: "Session".into(),
                entries: vec![
                    ("↑/↓ / Tab".into(), "switch session".into()),
                    ("PgUp/PgDn".into(), "scroll chat".into()),
                    ("Home".into(), "oldest".into()),
                    ("End".into(), "latest".into()),
                    ("j / k".into(), "walk file-change rows".into()),
                    ("J / K".into(), "scroll detail pane".into()),
                    ("Esc".into(), "back".into()),
                ],
            });

            // Management keys only exist for a mewxi-driven session that
            // isn't mid-overlay/input-focus (those short-circuited
            // above). Observed sessions (session is None, or driven is
            // false) must not see Del/Ctrl-D/i//.
            let driven = ctx.session.map(|s| s.driven).unwrap_or(false);
            if driven {
                sections.push(HelpSection {
                    title: "Driven session".into(),
                    entries: vec![
                        ("i".into(), "type prompt".into()),
                        ("m".into(), "model / effort picker".into()),
                        ("/".into(), "run skill".into()),
                        ("Shift-Tab".into(), "cycle permission mode".into()),
                        ("Ctrl-C".into(), "cancel execution".into()),
                        ("Ctrl-D".into(), "end session".into()),
                        ("Del".into(), "kill (with confirm)".into()),
                    ],
                });
            }
        }
        ViewKind::AccountDetail => {
            sections.push(HelpSection {
                title: "Account".into(),
                entries: vec![("↑/↓ / Tab".into(), "next account".into())],
            });
        }
        ViewKind::Setup => {
            sections.push(HelpSection {
                title: "Config".into(),
                entries: vec![
                    ("Tab / ↑/↓".into(), "select row".into()),
                    ("Enter".into(), "contextual action".into()),
                    ("R".into(), "rescan setup".into()),
                    ("s".into(), "toggle statusline".into()),
                    ("i".into(), "ignore account".into()),
                    ("w".into(), "toggle watcher".into()),
                    ("e".into(), "edit value".into()),
                    ("a".into(), "apply all".into()),
                    ("t".into(), "defocus-after-send toggle".into()),
                    ("o / y".into(), "cycle log filters".into()),
                    ("L".into(), "expand logs".into()),
                    ("PgUp/PgDn".into(), "scroll logs".into()),
                ],
            });
        }
        ViewKind::Mewxi => {
            // The rave view carries the same sessions table as view 1,
            // so it advertises the same selection keys.
            let mut entries = Vec::new();
            if ctx.has_sessions {
                entries.push(("↑/↓".into(), "select session".into()));
                entries.push(("Tab / Shift-Tab".into(), "cycle sessions".into()));
                entries.push(("Enter".into(), "open session".into()));
            }
            if ctx.selected_driven {
                entries.push(("Del".into(), "kill selected session".into()));
            }
            if !entries.is_empty() {
                sections.push(HelpSection { title: "Sessions".into(), entries });
            }

            sections.push(HelpSection {
                title: "Rave".into(),
                entries: vec![("s".into(), "score board".into())],
            });
        }
    }

    // Global keys — present in every context, appended last so the
    // view-specific keys the user actually came here for read first.
    let mut global = vec![
        ("q".into(), "quit".into()),
        ("1".into(), "all sessions".into()),
        ("2".into(), "session detail".into()),
        ("3".into(), "account detail".into()),
        ("4".into(), "config".into()),
        ("r".into(), "refresh usage limits".into()),
        ("n".into(), "new session".into()),
    ];
    // `m` opens the mewxi splash — except in SessionDetail, where `m` is
    // already claimed by the model/effort picker, and on the splash
    // itself, where it's a no-op.
    if ctx.view != ViewKind::SessionDetail && ctx.view != ViewKind::Mewxi {
        global.push(("m".into(), "open mewxi splash".into()));
    }
    // Esc backs out to AllSessions from everywhere else; on AllSessions
    // itself there's nowhere further back to go.
    if ctx.view != ViewKind::AllSessions {
        global.push(("Esc".into(), "back to all sessions".into()));
    }
    sections.push(HelpSection { title: "Global".into(), entries: global });

    sections
}

pub struct HelpModal {
    sections: Vec<HelpSection>,
    scroll: usize,
}

impl HelpModal {
    pub fn new(sections: Vec<HelpSection>) -> Self {
        Self { sections, scroll: 0 }
    }

    /// `Esc`, `?` and `q` close; `↑/↓`/`PgUp/PgDn` scroll; everything
    /// else is swallowed so no shortcut fires underneath the modal.
    pub fn handle_key(&mut self, k: KeyEvent) -> HelpOutcome {
        match k.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => HelpOutcome::Close,
            KeyCode::Up | KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(1);
                HelpOutcome::Stay
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(1);
                HelpOutcome::Stay
            }
            _ => HelpOutcome::Stay,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let lines = flatten(&self.sections);

        // Size to content, then clamp to ~80% of the frame so a
        // long section list on a small terminal never claims the
        // whole screen.
        let content_width = lines.iter().map(line_width).max().unwrap_or(24);
        let content_height = lines.len() + 1; // +1 for the bottom help line.

        let max_w = ((area.width as u32 * 8) / 10) as u16;
        let max_h = ((area.height as u32 * 8) / 10) as u16;

        let w = ((content_width as u16).saturating_add(4)).min(max_w).min(area.width).max(1);
        let h = ((content_height as u16).saturating_add(2)).min(max_h).min(area.height).max(1);

        let modal_area = center_rect(area, w, h);
        f.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Shortcuts ")
            .border_style(Style::default().fg(Color::Cyan));

        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        // Bottom row is always the dim help line; everything above it
        // is the (possibly windowed) content.
        let body_h = inner.height.saturating_sub(1);
        let total_rows = lines.len();
        let overflow = total_rows > body_h as usize;
        let (top_ind_h, bot_ind_h): (u16, u16) = if overflow { (1, 1) } else { (0, 0) };
        let visible_h = (body_h.saturating_sub(top_ind_h + bot_ind_h)).max(1) as usize;

        // Clamp the stored scroll offset against actual overflow here,
        // at render time, rather than in handle_key.
        let max_offset = total_rows.saturating_sub(visible_h);
        let offset = self.scroll.min(max_offset);

        let mut y = inner.y;

        if overflow {
            let text = if offset > 0 { "↑ more" } else { "" };
            let p = Paragraph::new(Line::from(Span::styled(text, Style::default().fg(Color::DarkGray))));
            f.render_widget(p, Rect { x: inner.x, y, width: inner.width, height: 1 });
            y = y.saturating_add(1);
        }

        let visible: Vec<Line> = lines.iter().skip(offset).take(visible_h).cloned().collect();
        let visible_rect = Rect { x: inner.x, y, width: inner.width, height: visible_h as u16 };
        f.render_widget(Paragraph::new(visible), visible_rect);
        y = y.saturating_add(visible_h as u16);

        if overflow {
            let text = if offset < max_offset { "↓ more" } else { "" };
            let p = Paragraph::new(Line::from(Span::styled(text, Style::default().fg(Color::DarkGray))));
            f.render_widget(p, Rect { x: inner.x, y, width: inner.width, height: 1 });
            y = y.saturating_add(1);
        }

        let help = Paragraph::new(Line::from(Span::styled(
            "↑/↓ scroll · Esc close",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(help, Rect { x: inner.x, y, width: inner.width, height: 1 });
    }
}

/// Flatten sections into pre-styled lines once: a bold title row per
/// section, its `key  action` rows aligned on a shared key column, and
/// a blank separator between sections. Rendering then just windows this
/// Vec by the scroll offset, so title and entry rows always scroll
/// together.
fn flatten(sections: &[HelpSection]) -> Vec<Line<'static>> {
    let key_width = sections
        .iter()
        .flat_map(|s| s.entries.iter())
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);

    let mut lines = Vec::new();
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            section.title.clone(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        for (key, action) in &section.entries {
            let padded = format!("{key:<key_width$}");
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(padded, Style::default().fg(Color::Yellow)),
                Span::raw("  "),
                Span::styled(action.clone(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    lines
}

fn line_width(line: &Line) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
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

    fn all_pairs(ctx: &HelpCtx) -> Vec<(String, String)> {
        sections_for(ctx).into_iter().flat_map(|s| s.entries).collect()
    }

    fn all_keys(ctx: &HelpCtx) -> Vec<String> {
        all_pairs(ctx).into_iter().map(|(k, _)| k).collect()
    }

    fn session_detail_ctx(session: Option<SessionFlags>) -> HelpCtx {
        HelpCtx {
            view: ViewKind::SessionDetail,
            has_sessions: true,
            selected_driven: false,
            session,
        }
    }

    const MANAGEMENT_KEYS: [&str; 4] = ["Del", "Ctrl-D", "i", "/"];

    #[test]
    fn observed_session_has_no_management_keys() {
        let ctx = session_detail_ctx(Some(SessionFlags { driven: false, ..Default::default() }));
        let keys = all_keys(&ctx);
        for forbidden in MANAGEMENT_KEYS {
            assert!(!keys.iter().any(|k| k == forbidden), "unexpected `{forbidden}` for observed session");
        }
    }

    #[test]
    fn unpinned_session_has_no_management_keys() {
        let ctx = session_detail_ctx(None);
        let keys = all_keys(&ctx);
        for forbidden in MANAGEMENT_KEYS {
            assert!(!keys.iter().any(|k| k == forbidden), "unexpected `{forbidden}` for unpinned session");
        }
    }

    #[test]
    fn driven_unfocused_session_has_management_keys() {
        let ctx = session_detail_ctx(Some(SessionFlags {
            driven: true,
            input_focused: false,
            overlay_active: false,
        }));
        let keys = all_keys(&ctx);
        for required in MANAGEMENT_KEYS {
            assert!(keys.iter().any(|k| k == required), "missing `{required}` for driven session");
        }
    }

    #[test]
    fn all_sessions_del_depends_on_selected_driven() {
        let mut ctx = HelpCtx {
            view: ViewKind::AllSessions,
            has_sessions: true,
            selected_driven: false,
            session: None,
        };
        assert!(!all_keys(&ctx).iter().any(|k| k == "Del"));
        ctx.selected_driven = true;
        assert!(all_keys(&ctx).iter().any(|k| k == "Del"));
    }

    #[test]
    fn all_sessions_without_sessions_has_no_selection_keys() {
        let ctx = HelpCtx {
            view: ViewKind::AllSessions,
            has_sessions: false,
            selected_driven: false,
            session: None,
        };
        let keys = all_keys(&ctx);
        assert!(!keys.iter().any(|k| k == "Enter"));
        assert!(!keys.iter().any(|k| k == "↑/↓"));
    }

    #[test]
    fn all_sessions_with_sessions_has_selection_keys() {
        let ctx = HelpCtx {
            view: ViewKind::AllSessions,
            has_sessions: true,
            selected_driven: false,
            session: None,
        };
        let keys = all_keys(&ctx);
        assert!(keys.iter().any(|k| k == "Enter"));
        assert!(keys.iter().any(|k| k == "↑/↓"));
    }

    #[test]
    fn every_view_yields_nonempty_sections() {
        let views = [
            ViewKind::AllSessions,
            ViewKind::SessionDetail,
            ViewKind::AccountDetail,
            ViewKind::Setup,
            ViewKind::Mewxi,
        ];
        for view in views {
            let ctx = HelpCtx {
                view,
                has_sessions: true,
                selected_driven: true,
                session: Some(SessionFlags { driven: true, ..Default::default() }),
            };
            let sections = sections_for(&ctx);
            assert!(!sections.is_empty(), "{view:?} produced no sections");
            let total_entries: usize = sections.iter().map(|s| s.entries.len()).sum();
            assert!(total_entries > 0, "{view:?} produced sections with no entries");
        }
    }

    #[test]
    fn mewxi_advertises_score_board_key() {
        let ctx = HelpCtx {
            view: ViewKind::Mewxi,
            has_sessions: false,
            selected_driven: false,
            session: None,
        };
        assert!(
            all_pairs(&ctx).iter().any(|(k, a)| k == "s" && a == "score board"),
            "Mewxi view must advertise the `s` score-board key even with no sessions"
        );
    }

    #[test]
    fn mewxi_splash_hint_absent_in_session_detail_present_elsewhere() {
        let session_ctx = session_detail_ctx(Some(SessionFlags { driven: true, ..Default::default() }));
        assert!(
            !all_pairs(&session_ctx).iter().any(|(k, a)| k == "m" && a == "open mewxi splash"),
            "SessionDetail must not advertise the mewxi-splash `m`"
        );

        let all_sessions_ctx = HelpCtx {
            view: ViewKind::AllSessions,
            has_sessions: true,
            selected_driven: false,
            session: None,
        };
        assert!(
            all_pairs(&all_sessions_ctx).iter().any(|(k, a)| k == "m" && a == "open mewxi splash"),
            "AllSessions should advertise the mewxi-splash `m`"
        );
    }
}
