//! Modal overlay for picking the model + thinking-effort of a
//! mewxi-driven `claude` session. The pick is sent as `/model <slug>\r`
//! and (when an effort is chosen) `/effort <level>\r` to the PTY by the
//! caller — this module is presentation-only.
//!
//! While open, the modal owns every keystroke (Esc to cancel, Up/Down
//! to move, Tab to switch pane, Enter to confirm, `d` to confirm and
//! also persist the effort as this account's startup default). The
//! parent must dispatch keys through [`ModelPickerModal::handle_key`]
//! *before* the global handlers, otherwise the underlying view's
//! keybinds will leak through.
//!
//! Enter alone is session-scoped: claude's own `/effort` is "this
//! session only" — it does not update `settings.json`. `d` adds the
//! settings-file write so a brand-new session opens at the chosen
//! level.
//!
//! ## Per-model effort matrix
//!
//! Mirrors Claude Code 2.1.150's gating (functions `SW`/`TgH`/`XM$` in
//! the binary): `xhigh` requires opus-4-7, `max` requires opus-4-6/4-7
//! or sonnet-4-6, and haiku-4-5 has no effort support at all. When
//! claude rolls out a new tier we'll add it here; the safest default
//! when the model is unknown is the broadest universally-supported set
//! (auto + low/medium/high + max) — claude itself silently downgrades
//! unsupported picks.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

/// One row in the model column — a `/model` slug plus a short label and
/// a help line explaining when to pick it.
pub struct ModelChoice {
    pub slug: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

/// Fixed list of models we offer. Slugs are what `/model` accepts as
/// aliases. `default` clears any session-local override and falls back
/// to whatever Claude Code was started with.
const MODELS: &[ModelChoice] = &[
    ModelChoice { slug: "opus",    label: "Opus",    hint: "most capable, slow + expensive" },
    ModelChoice { slug: "sonnet",  label: "Sonnet",  hint: "balanced default" },
    ModelChoice { slug: "haiku",   label: "Haiku",   hint: "fastest + cheapest" },
    ModelChoice { slug: "default", label: "Default", hint: "clear override; use Claude Code's default" },
];

/// Return the `/effort` levels we expose for a given model. Returns an
/// empty slice when the model doesn't support effort (e.g. Haiku 4.5).
///
/// `model` may be a short slug (`opus`, `sonnet`, `haiku`, `default`)
/// from the picker, or the long transcript form (`claude-opus-4-7`,
/// `claude-sonnet-4-6`, `claude-haiku-4-5`). Case-insensitive substring
/// matching handles both — same trick the rest of the TUI uses.
pub fn effort_levels_for(model: &str) -> &'static [&'static str] {
    let m = model.to_ascii_lowercase();
    // No effort support — claude rejects `/effort` outright.
    if m.contains("haiku") {
        return &[];
    }
    // opus-4-7 is the only model with `xhigh`.
    if m.contains("opus-4-7") || m == "opus" {
        return &["auto", "low", "medium", "high", "xhigh", "max"];
    }
    // opus-4-6 and sonnet-4-6 support `max` but not `xhigh`.
    if m.contains("opus-4-6") || m.contains("sonnet-4-6") || m == "sonnet" {
        return &["auto", "low", "medium", "high", "max"];
    }
    // Default / unknown: assume claude's default model (sonnet/opus) and
    // expose the broadest safe set. claude silent-downgrades anything it
    // can't honour, so this errs toward "let the user try".
    &["auto", "low", "medium", "high", "max"]
}

/// Hint shown to the right of an effort level. Pure label-decoration —
/// the user already knows what `low`/`high` mean conceptually, this
/// just clarifies the `auto` and `max` extremes.
fn effort_hint(level: &str) -> &'static str {
    match level {
        "auto" => "let claude pick per turn",
        "low" => "minimal thinking",
        "medium" => "moderate thinking",
        "high" => "deep thinking",
        "xhigh" => "extra-deep (opus-4-7 only)",
        "max" => "maximum thinking budget",
        _ => "",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Model,
    Effort,
}

pub struct ModelPickerModal {
    model_idx: usize,
    /// Selection within the *current model's* effort list. Reset to a
    /// best-effort match whenever the model selection changes so the
    /// pre-existing pick survives a model switch when the new model
    /// still offers it.
    effort_idx: usize,
    focus: Pane,
    /// User's effort at modal-open time, used to seed `effort_idx` for
    /// each model the user navigates over. Without it, switching from
    /// Opus → Sonnet would lose the original highlight.
    initial_effort: Option<String>,
}

pub enum ModelOutcome {
    Stay,
    Cancel,
    /// Send `/model <slug>\r` (and optionally `/effort <level>\r`) to
    /// the driven PTY. `effort` is `None` either because the selected
    /// model has no effort support or because nothing was changed worth
    /// re-sending — the caller treats both the same.
    Confirm {
        slug: String,
        effort: Option<String>,
    },
    /// Same as [`Confirm`] but also asks the caller to persist `effort`
    /// to the account's `settings.json` (`effortLevel`) so future
    /// sessions start there. Only emitted when the current model
    /// actually supports effort — `effort` is therefore always `Some`.
    ConfirmAsDefault {
        slug: String,
        effort: String,
    },
}

impl ModelPickerModal {
    /// Open the picker. `current_model` is the model string currently
    /// shown in the session header (e.g. `claude-sonnet-4-6`); if it
    /// contains one of our slugs we pre-select that row so Enter is
    /// idempotent. `current_effort` is the same idea but for the right
    /// pane — typically read from the account's `settings.json`
    /// `effortLevel` field.
    pub fn new(current_model: Option<&str>, current_effort: Option<&str>) -> Self {
        let model_idx = current_model
            .and_then(|c| {
                let c = c.to_ascii_lowercase();
                MODELS.iter().position(|m| c.contains(m.slug))
            })
            .unwrap_or(0);
        let initial_effort = current_effort.map(|s| s.to_ascii_lowercase());
        let effort_idx = Self::seed_effort_idx(MODELS[model_idx].slug, initial_effort.as_deref());
        Self {
            model_idx,
            effort_idx,
            focus: Pane::Model,
            initial_effort,
        }
    }

    /// Find the index of `target` in the given model's effort list,
    /// defaulting to 0 (`auto`) when not found or when the model has no
    /// effort support.
    fn seed_effort_idx(model_slug: &str, target: Option<&str>) -> usize {
        let levels = effort_levels_for(model_slug);
        if levels.is_empty() {
            return 0;
        }
        target
            .and_then(|t| levels.iter().position(|l| l.eq_ignore_ascii_case(t)))
            .unwrap_or(0)
    }

    fn current_levels(&self) -> &'static [&'static str] {
        effort_levels_for(MODELS[self.model_idx].slug)
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> ModelOutcome {
        match (self.focus, k.code) {
            (_, KeyCode::Esc) => ModelOutcome::Cancel,
            // Tab cycles focus only when the right pane has something to
            // focus on. Haiku → no effort list → Tab is a no-op.
            (_, KeyCode::Tab) => {
                if !self.current_levels().is_empty() {
                    self.focus = match self.focus {
                        Pane::Model => Pane::Effort,
                        Pane::Effort => Pane::Model,
                    };
                }
                ModelOutcome::Stay
            }
            (_, KeyCode::BackTab) => {
                if !self.current_levels().is_empty() {
                    self.focus = match self.focus {
                        Pane::Model => Pane::Effort,
                        Pane::Effort => Pane::Model,
                    };
                }
                ModelOutcome::Stay
            }
            // Left/Right also swap panes — feels natural in a 2-column
            // layout and h/j/k/l vim users get this for free.
            (_, KeyCode::Left) | (_, KeyCode::Char('h')) => {
                self.focus = Pane::Model;
                ModelOutcome::Stay
            }
            (_, KeyCode::Right) | (_, KeyCode::Char('l')) => {
                if !self.current_levels().is_empty() {
                    self.focus = Pane::Effort;
                }
                ModelOutcome::Stay
            }
            (Pane::Model, KeyCode::Up) | (Pane::Model, KeyCode::Char('k')) => {
                if self.model_idx > 0 {
                    self.model_idx -= 1;
                    self.effort_idx = Self::seed_effort_idx(
                        MODELS[self.model_idx].slug,
                        self.initial_effort.as_deref(),
                    );
                    // Snap focus back to the right pane if we lost it
                    // by landing on a model with no effort support.
                    if self.current_levels().is_empty() {
                        self.focus = Pane::Model;
                    }
                }
                ModelOutcome::Stay
            }
            (Pane::Model, KeyCode::Down) | (Pane::Model, KeyCode::Char('j')) => {
                if self.model_idx + 1 < MODELS.len() {
                    self.model_idx += 1;
                    self.effort_idx = Self::seed_effort_idx(
                        MODELS[self.model_idx].slug,
                        self.initial_effort.as_deref(),
                    );
                    if self.current_levels().is_empty() {
                        self.focus = Pane::Model;
                    }
                }
                ModelOutcome::Stay
            }
            (Pane::Effort, KeyCode::Up) | (Pane::Effort, KeyCode::Char('k')) => {
                if self.effort_idx > 0 {
                    self.effort_idx -= 1;
                }
                ModelOutcome::Stay
            }
            (Pane::Effort, KeyCode::Down) | (Pane::Effort, KeyCode::Char('j')) => {
                let levels = self.current_levels();
                if !levels.is_empty() && self.effort_idx + 1 < levels.len() {
                    self.effort_idx += 1;
                }
                ModelOutcome::Stay
            }
            (_, KeyCode::Enter) => {
                let slug = MODELS[self.model_idx].slug.to_string();
                let levels = self.current_levels();
                let effort = levels.get(self.effort_idx).map(|s| (*s).to_string());
                ModelOutcome::Confirm { slug, effort }
            }
            // `d` confirms *and* persists the effort to settings.json.
            // No-op when the current model has no effort to persist
            // (Haiku) — falling through to Stay leaves the modal open
            // so the user can switch models and try again.
            (_, KeyCode::Char('d')) | (_, KeyCode::Char('D')) => {
                let levels = self.current_levels();
                match levels.get(self.effort_idx) {
                    Some(eff) => ModelOutcome::ConfirmAsDefault {
                        slug: MODELS[self.model_idx].slug.to_string(),
                        effort: (*eff).to_string(),
                    },
                    None => ModelOutcome::Stay,
                }
            }
            _ => ModelOutcome::Stay,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        // Fixed-size centered box. Wider than the old picker so two
        // columns fit comfortably without truncating the hint lines.
        let modal_area = center_rect(area, 64, 14);
        f.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Model & Thinking — Tab swap · Enter session · d default · Esc cancel ")
            .border_style(Style::default().fg(Color::Magenta));

        // Carve inside the block: two columns side-by-side.
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(inner);

        self.render_model_column(f, cols[0]);
        self.render_effort_column(f, cols[1]);
    }

    fn render_model_column(&self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Pane::Model;
        let title_style = if focused {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let items: Vec<ListItem> = MODELS
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let selected = i == self.model_idx;
                let arrow = if selected && focused {
                    "▶ "
                } else if selected {
                    "• "
                } else {
                    "  "
                };
                let name_style = if selected && focused {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else if selected {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(arrow, name_style),
                        Span::styled(m.label, name_style),
                        Span::raw("  "),
                        Span::styled(
                            format!("({})", m.slug),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]),
                    Line::from(Span::styled(
                        format!("    {}", m.hint),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
            })
            .collect();

        let block = Block::default()
            .borders(Borders::RIGHT)
            .title(Span::styled(" Model ", title_style));
        f.render_widget(List::new(items).block(block), area);
    }

    fn render_effort_column(&self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Pane::Effort;
        let title_style = if focused {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .borders(Borders::NONE)
            .title(Span::styled(" Thinking ", title_style));
        let levels = self.current_levels();
        if levels.is_empty() {
            // Haiku and the rest of the no-effort club: explain why the
            // pane is empty instead of leaving an unexplained void.
            let msg = Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  effort not supported",
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                )),
                Line::from(Span::styled(
                    "  for this model",
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                )),
            ])
            .block(block);
            f.render_widget(msg, area);
            return;
        }
        let items: Vec<ListItem> = levels
            .iter()
            .enumerate()
            .map(|(i, lvl)| {
                let selected = i == self.effort_idx;
                let arrow = if selected && focused {
                    "▶ "
                } else if selected {
                    "• "
                } else {
                    "  "
                };
                let name_style = if selected && focused {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else if selected {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(arrow, name_style),
                    Span::styled(*lvl, name_style),
                    Span::raw("  "),
                    Span::styled(
                        effort_hint(lvl),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect();
        f.render_widget(List::new(items).block(block), area);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_has_xhigh() {
        let opts = effort_levels_for("opus");
        assert!(opts.contains(&"xhigh"));
        assert!(opts.contains(&"max"));
    }

    #[test]
    fn sonnet_has_max_but_not_xhigh() {
        let opts = effort_levels_for("sonnet");
        assert!(opts.contains(&"max"));
        assert!(!opts.contains(&"xhigh"));
    }

    #[test]
    fn haiku_has_no_effort() {
        assert!(effort_levels_for("haiku").is_empty());
        assert!(effort_levels_for("claude-haiku-4-5").is_empty());
    }

    #[test]
    fn long_form_matches() {
        let o = effort_levels_for("claude-opus-4-7");
        assert!(o.contains(&"xhigh"));
        let s = effort_levels_for("claude-sonnet-4-6");
        assert!(s.contains(&"max"));
        assert!(!s.contains(&"xhigh"));
        let o6 = effort_levels_for("claude-opus-4-6");
        assert!(o6.contains(&"max"));
        assert!(!o6.contains(&"xhigh"));
    }

    #[test]
    fn unknown_model_gets_broad_set() {
        let opts = effort_levels_for("default");
        assert!(opts.contains(&"auto"));
        assert!(opts.contains(&"max"));
        assert!(!opts.contains(&"xhigh"));
    }

    fn key(c: char) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(
            KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        )
    }

    #[test]
    fn d_emits_confirm_as_default_with_current_effort() {
        // Opens on opus + max (the level we'll then persist).
        let mut m = ModelPickerModal::new(Some("opus"), Some("max"));
        match m.handle_key(key('d')) {
            ModelOutcome::ConfirmAsDefault { slug, effort } => {
                assert_eq!(slug, "opus");
                assert_eq!(effort, "max");
            }
            _ => panic!("expected ConfirmAsDefault"),
        }
    }

    #[test]
    fn d_is_noop_when_model_has_no_effort() {
        // Haiku has no effort levels — `d` has nothing to persist and
        // must not silently fall through to Confirm or Cancel.
        let mut m = ModelPickerModal::new(Some("haiku"), None);
        assert!(matches!(m.handle_key(key('d')), ModelOutcome::Stay));
    }
}
