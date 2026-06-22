//! Lightweight, view-agnostic toast notifications.
//!
//! A toast is a small floating box that appears in the top-right corner of
//! the screen for a few seconds and then fades out. Unlike `driver_status`
//! (which only surfaces inside the Setup view) toasts are drawn over the
//! *whole* frame after every view has rendered, so feedback is visible no
//! matter which view the user is in.
//!
//! Usage from the event loop:
//! ```ignore
//! let mut toasts = Toasts::default();
//! toasts.success("copied to clipboard");          // transient, default TTL
//! toasts.push_tagged("refresh", ToastKind::Info, "refreshing…", ttl); // updatable
//! // each frame:
//! toasts.prune();
//! toasts.render(f, f.area());
//! ```
//!
//! A `tag` lets a later push *replace* an earlier toast in place — handy for
//! turning a "working…" toast into its "done"/"failed" result without
//! stacking two boxes.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

/// Default time a toast stays on screen before it auto-hides.
pub const DEFAULT_TTL: Duration = Duration::from_millis(3500);

/// Severity of a toast — drives its icon and colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastKind {
    Success,
    Error,
    Info,
}

impl ToastKind {
    fn color(self) -> Color {
        match self {
            ToastKind::Success => Color::Green,
            ToastKind::Error => Color::Red,
            ToastKind::Info => Color::Cyan,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            ToastKind::Success => "✓",
            ToastKind::Error => "✗",
            ToastKind::Info => "•",
        }
    }
}

struct Toast {
    /// Optional identity. A `push_tagged` with the same tag replaces this one
    /// in place instead of stacking a second box.
    tag: Option<&'static str>,
    message: String,
    kind: ToastKind,
    created: Instant,
    ttl: Duration,
}

impl Toast {
    fn expired(&self) -> bool {
        self.created.elapsed() >= self.ttl
    }
}

/// A small stack of active toasts, newest at the top-right.
#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    /// Push a transient toast that cannot be addressed/replaced later.
    pub fn push(&mut self, kind: ToastKind, message: impl Into<String>, ttl: Duration) {
        self.items.push(Toast {
            tag: None,
            message: message.into(),
            kind,
            created: Instant::now(),
            ttl,
        });
    }

    /// Push (or replace) a toast addressed by `tag`. A second call with the
    /// same tag updates the existing toast's text/kind/TTL in place — use it
    /// for progress→result transitions ("refreshing…" → "refreshed").
    pub fn push_tagged(
        &mut self,
        tag: &'static str,
        kind: ToastKind,
        message: impl Into<String>,
        ttl: Duration,
    ) {
        let toast = Toast {
            tag: Some(tag),
            message: message.into(),
            kind,
            created: Instant::now(),
            ttl,
        };
        if let Some(slot) = self.items.iter_mut().find(|t| t.tag == Some(tag)) {
            *slot = toast;
        } else {
            self.items.push(toast);
        }
    }

    /// Convenience: a green success toast with the default TTL.
    pub fn success(&mut self, message: impl Into<String>) {
        self.push(ToastKind::Success, message, DEFAULT_TTL);
    }

    /// Convenience: a red error toast, shown a little longer than the
    /// default so the user has time to read the failure.
    pub fn error(&mut self, message: impl Into<String>) {
        self.push(ToastKind::Error, message, Duration::from_millis(6000));
    }

    /// Drop toasts whose TTL has elapsed. Call once per frame.
    pub fn prune(&mut self) {
        self.items.retain(|t| !t.expired());
    }

    /// Draw the active toasts stacked down from the top-right of `area`.
    /// Each box clears its own cells first so it reads cleanly over whatever
    /// view (or modal) rendered underneath it.
    pub fn render(&self, f: &mut Frame, area: Rect) {
        if self.items.is_empty() || area.width < 8 || area.height < 3 {
            return;
        }
        let box_h: u16 = 3;
        let mut y = area.y;
        for toast in &self.items {
            // Stop once we'd run past the bottom of the screen.
            if y.saturating_add(box_h) > area.y.saturating_add(area.height) {
                break;
            }
            let label = format!("{} {}", toast.kind.icon(), toast.message);
            let label_w = label.chars().count() as u16;
            // content + 2 borders + 2 padding, clamped to the frame width.
            let box_w = (label_w + 4).min(area.width.max(1));
            let x = area
                .x
                .saturating_add(area.width.saturating_sub(box_w).saturating_sub(1));
            let rect = Rect {
                x,
                y,
                width: box_w,
                height: box_h,
            };
            let color = toast.kind.color();
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color));
            let para = Paragraph::new(Line::from(Span::styled(
                format!(" {label} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )))
            .block(block);
            f.render_widget(Clear, rect);
            f.render_widget(para, rect);
            y = y.saturating_add(box_h);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untagged_pushes_stack() {
        let mut t = Toasts::default();
        t.success("a");
        t.success("b");
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn same_tag_replaces_in_place() {
        let mut t = Toasts::default();
        t.push_tagged("refresh", ToastKind::Info, "working…", DEFAULT_TTL);
        t.push_tagged("refresh", ToastKind::Info, "1/2 done", DEFAULT_TTL);
        t.push_tagged("refresh", ToastKind::Success, "done", DEFAULT_TTL);
        assert_eq!(t.len(), 1, "a tagged toast must update in place, not stack");
        assert_eq!(t.items[0].message, "done");
        assert_eq!(t.items[0].kind, ToastKind::Success);
    }

    #[test]
    fn distinct_tags_coexist() {
        let mut t = Toasts::default();
        t.push_tagged("refresh", ToastKind::Info, "x", DEFAULT_TTL);
        t.push_tagged("update", ToastKind::Info, "y", DEFAULT_TTL);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn prune_drops_expired_only() {
        let mut t = Toasts::default();
        t.push(ToastKind::Info, "gone", Duration::ZERO);
        t.push(ToastKind::Info, "stays", DEFAULT_TTL);
        t.prune();
        assert_eq!(t.len(), 1);
        assert_eq!(t.items[0].message, "stays");
    }
}
