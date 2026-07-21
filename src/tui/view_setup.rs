//! View 4 — Config. One flat, navigable list of everything mewxi can
//! configure, grouped into sections:
//!
//! - Claude Code integration — per-account statusLine wiring + the
//!   background watcher service.
//! - Updates — self-update channel (release tags vs main branch),
//!   the automatic-check toggle + interval, the startup prompt
//!   toggle, and an on-demand check/install row.
//! - Preferences — TUI behaviour toggles.
//! - Mewxi view — the agent-activity visualizer, screen shake, streak
//!   celebrations, fx intensity, and ascii art style used by the
//!   Mewxi rave view.
//! - Status line — the block composer.
//! - Logs — a scrollable tail of recent `crate::debug_log` entries,
//!   filterable by origin/kind, rendered in its own panel at the
//!   bottom of the view (above the footer). `L` toggles it between a
//!   fixed 9-row window and an expanded view that takes the flexible
//!   share of the vertical space (shrinking the settings list to a
//!   fixed 10-row window in exchange).
//!
//! Interaction model: ↑/↓ (or Tab) moves over actionable rows, Enter
//! performs the row's single contextual action, and the hint box under
//! the list spells out what Enter will do *before* the user presses
//! it. The old single-letter keys (`s`/`w`/`t`/`i`/`a`/`R`) still work
//! as shortcuts for the same actions. `o`/`y` cycle the logs panel's
//! origin/kind filters, `L` expands/shrinks it, and PgUp/PgDn scroll it.

use super::widgets::render_footer;
use crate::setup::{SetupSnapshot, StatusLineState, WatcherState};
use crate::update::{UpdateChannel, UpdateInterval, UpdateStatus};
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
    UpdateCheck,
    UpdateInterval,
    UpdatePrompt,
    UpdateBuildDir,
    UpdateCheckNow,
    DefaultView,
    DefocusToggle,
    /// Steps (or, via inline edit, sets exactly) the minimum time
    /// between live usage-endpoint probes (`live_refresh_interval_secs`
    /// in accounts.toml).
    LivePollInterval,
    /// Steps (or, via inline edit, sets exactly) the per-file line cap
    /// for `crate::debug_log` (`log_max_lines` in accounts.toml).
    LogMaxLines,
    /// Toggles the `— Tool(arg)` suffix on sub-agent row captions
    /// (`subagent_tool_action` in accounts.toml).
    SubagentToolActionToggle,
    /// Toggles the agent-activity visualizer in the Mewxi rave view.
    MewxiVisualizerToggle,
    /// Cycles the Mewxi view's screen-shake level (off · subtle · full).
    MewxiShakeCycle,
    /// Toggles win/output streak celebrations in the Mewxi rave view.
    MewxiStreaksToggle,
    /// Cycles the Mewxi view's fx intensity (chill · rave · insane).
    MewxiFxIntensityCycle,
    /// Cycles the Mewxi view's ascii art style (y2k · classic).
    MewxiAsciiStyleCycle,
    /// Opens the status-line block composer modal.
    StatusLineComposer,
}

/// Which view the TUI opens in — the `default_view` key in
/// `accounts.toml`. Mirrors `ViewMode::from_config` in `tui::mod`, so
/// `as_str()` must only emit strings that parser accepts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DefaultView {
    All,
    Session,
    Account,
    Config,
    Mewxi,
}

impl DefaultView {
    pub fn from_config(s: Option<&str>) -> Self {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("session" | "session_detail" | "2") => DefaultView::Session,
            Some("account" | "account_detail" | "3") => DefaultView::Account,
            Some("config" | "setup" | "4") => DefaultView::Config,
            Some("mewxi" | "rave" | "5") => DefaultView::Mewxi,
            _ => DefaultView::All,
        }
    }

    pub fn cycled(self) -> Self {
        match self {
            DefaultView::All => DefaultView::Session,
            DefaultView::Session => DefaultView::Account,
            DefaultView::Account => DefaultView::Config,
            DefaultView::Config => DefaultView::Mewxi,
            DefaultView::Mewxi => DefaultView::All,
        }
    }

    /// Value written to `accounts.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            DefaultView::All => "all",
            DefaultView::Session => "session",
            DefaultView::Account => "account",
            DefaultView::Config => "config",
            DefaultView::Mewxi => "mewxi",
        }
    }

    /// Human label including the view's switch key.
    pub fn label(self) -> &'static str {
        match self {
            DefaultView::All => "all sessions (view 1)",
            DefaultView::Session => "session detail (view 2)",
            DefaultView::Account => "account detail (view 3)",
            DefaultView::Config => "config (view 4)",
            DefaultView::Mewxi => "mewxi rave (view m)",
        }
    }
}

/// Fixed step size for the "usage poll interval" row — Enter always
/// advances to the next multiple of this many seconds.
pub const LIVE_POLL_STEP_SECS: u64 = 30;
/// Floor for both the stepped cycle and hand-typed custom values.
/// Probing more often than this would hammer the usage endpoint for no
/// real benefit.
pub const LIVE_POLL_MIN_SECS: u64 = 30;
/// Ceiling for both the stepped cycle and hand-typed custom values (15
/// minutes) — anything rarer than this defeats the point of a "live"
/// view.
pub const LIVE_POLL_MAX_SECS: u64 = 900;

/// Next 30-second grid point strictly above `current`, capped at
/// `LIVE_POLL_MAX_SECS` and wrapping back to `LIVE_POLL_MIN_SECS` once
/// `current` has reached (or somehow exceeds) the ceiling. Off-grid
/// values — e.g. a hand-typed 45s — snap up to the next multiple of 30
/// rather than adding a full step on top, so repeated presses always
/// land exactly on the grid.
pub fn next_live_poll_step(current: u64) -> u64 {
    if current >= LIVE_POLL_MAX_SECS {
        return LIVE_POLL_MIN_SECS;
    }
    let next = (current / LIVE_POLL_STEP_SECS + 1) * LIVE_POLL_STEP_SECS;
    next.min(LIVE_POLL_MAX_SECS)
}

/// Cycles the Mewxi view's screen-shake level: off → subtle → full →
/// off. Matching is trimmed + case-insensitive; unknown input is
/// treated as the default (`"subtle"`), so the function returns the
/// default's successor (`"full"`).
pub fn next_shake_level(cur: &str) -> &'static str {
    match cur.trim().to_ascii_lowercase().as_str() {
        "off" => "subtle",
        "full" => "off",
        _ => "full", // "subtle" and anything unrecognized (default: subtle)
    }
}

/// Cycles the Mewxi view's fx intensity: chill → rave → insane → chill.
/// Matching is trimmed + case-insensitive; unknown input is treated as
/// the default (`"rave"`), so the function returns the default's
/// successor (`"insane"`).
pub fn next_fx_intensity(cur: &str) -> &'static str {
    match cur.trim().to_ascii_lowercase().as_str() {
        "chill" => "rave",
        "insane" => "chill",
        _ => "insane", // "rave" and anything unrecognized (default: rave)
    }
}

/// Cycles the Mewxi view's ascii art style: y2k → classic → y2k.
/// Matching is trimmed + case-insensitive; unknown input is treated as
/// the default (`"y2k"`), so the function returns the default's
/// successor (`"classic"`).
pub fn next_ascii_style(cur: &str) -> &'static str {
    match cur.trim().to_ascii_lowercase().as_str() {
        "classic" => "y2k",
        _ => "classic", // "y2k" and anything unrecognized (default: y2k)
    }
}

/// Parses `lower` (already trimmed + lowercased) as `"90"`, `"90s"`,
/// `"2m"`, or `"1m30s"` into a whole number of seconds. `None` for
/// anything that doesn't match one of those shapes.
fn parse_duration_secs(lower: &str) -> Option<u64> {
    if lower.is_empty() {
        return None;
    }
    if let Ok(v) = lower.parse::<u64>() {
        return Some(v);
    }
    let (min_str, sec_str) = match lower.split_once('m') {
        Some((m, rest)) => (Some(m), rest),
        None => (None, lower),
    };
    // The part after the (optional) minutes is either empty — no
    // seconds — or must end in 's'; anything else is garbage.
    let sec_str = if sec_str.is_empty() {
        None
    } else {
        Some(sec_str.strip_suffix('s')?)
    };
    // A bare "s" strips down to nothing — with no minutes part either,
    // there isn't a single digit in the input; that's garbage, not 0s.
    if min_str.is_none() && sec_str.is_none_or(str::is_empty) {
        return None;
    }
    let mins: u64 = match min_str {
        Some(m) => m.parse().ok()?,
        None => 0,
    };
    let secs: u64 = match sec_str {
        Some(s) if !s.is_empty() => s.parse().ok()?,
        _ => 0,
    };
    Some(mins * 60 + secs)
}

/// Parses a user-typed poll interval from the inline edit box. Accepts
/// (trimmed, case-insensitive) a plain integer of seconds (`"90"`), or
/// `"90s"`, `"2m"`, `"1m30s"` shapes. Rejects anything outside
/// `[LIVE_POLL_MIN_SECS, LIVE_POLL_MAX_SECS]` with a message the row
/// can show verbatim as the edit hint.
pub fn parse_poll_input(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    let lower = trimmed.to_ascii_lowercase();
    let secs = parse_duration_secs(&lower)
        .ok_or_else(|| format!("can't parse '{s}' — try 90s, 2m or 1m30s"))?;
    if !(LIVE_POLL_MIN_SECS..=LIVE_POLL_MAX_SECS).contains(&secs) {
        return Err("interval must be between 30s and 15m".to_string());
    }
    Ok(secs)
}

/// `45` → `"45s"`, `120` → `"2m"`, `90` → `"1m30s"` — whole minutes
/// collapse, a leftover remainder of seconds is appended so
/// hand-edited/custom values always render exactly.
pub fn fmt_poll_secs(secs: u64) -> String {
    let mins = secs / 60;
    let rem = secs % 60;
    if mins == 0 {
        format!("{rem}s")
    } else if rem == 0 {
        format!("{mins}m")
    } else {
        format!("{mins}m{rem}s")
    }
}

/// Fixed step size for the "log file max lines" row — Enter always
/// advances to the next multiple of this many lines.
pub const LOG_LINES_STEP: u64 = 1_000;
/// Floor for both the stepped cycle and hand-typed custom values.
pub const LOG_LINES_MIN: u64 = 500;
/// Ceiling for both the stepped cycle and hand-typed custom values.
pub const LOG_LINES_MAX: u64 = 100_000;

/// Next 1,000-line grid point strictly above `current`, capped at
/// `LOG_LINES_MAX` and wrapping back to `LOG_LINES_MIN` once `current`
/// has reached (or somehow exceeds) the ceiling. Off-grid values snap up
/// to the next multiple of 1,000 rather than adding a full step on top,
/// so repeated presses always land exactly on the grid.
pub fn next_log_lines_step(current: u64) -> u64 {
    if current >= LOG_LINES_MAX {
        return LOG_LINES_MIN;
    }
    let next = (current / LOG_LINES_STEP + 1) * LOG_LINES_STEP;
    next.min(LOG_LINES_MAX)
}

/// Parses a user-typed line cap from the inline edit box. Accepts
/// (trimmed, case-insensitive) a plain integer (`"5000"`) or a
/// thousands suffix (`"5k"`). Rejects anything outside
/// `[LOG_LINES_MIN, LOG_LINES_MAX]` with a message the row can show
/// verbatim as the edit hint.
pub fn parse_log_lines_input(s: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    let lower = trimmed.to_ascii_lowercase();
    let lines = if let Some(k) = lower.strip_suffix('k') {
        k.parse::<u64>().ok().and_then(|v| v.checked_mul(1_000))
    } else {
        lower.parse::<u64>().ok()
    }
    .ok_or_else(|| format!("can't parse '{s}' — try 5000 or 5k"))?;
    if !(LOG_LINES_MIN..=LOG_LINES_MAX).contains(&lines) {
        return Err(format!(
            "line cap must be between {LOG_LINES_MIN} and {LOG_LINES_MAX}"
        ));
    }
    Ok(lines)
}

/// `10_000` → `"10k"`, `5_500` → `"5500"` — round thousands collapse to
/// the short form, anything else renders as the raw number so
/// hand-edited/custom values always render exactly.
pub fn fmt_log_lines(n: u64) -> String {
    if n != 0 && n.is_multiple_of(1_000) {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// Actionable rows, in display order. Accounts come first so the
/// existing account-oriented shortcuts (`s`, `i`) keep indexing
/// naturally.
pub fn items(snap: Option<&SetupSnapshot>) -> Vec<ConfigItem> {
    let n = snap.map(|s| s.accounts.len()).unwrap_or(0);
    let mut v: Vec<ConfigItem> = (0..n).map(ConfigItem::Account).collect();
    v.push(ConfigItem::Watcher);
    v.push(ConfigItem::UpdateChannel);
    v.push(ConfigItem::UpdateCheck);
    v.push(ConfigItem::UpdateInterval);
    v.push(ConfigItem::UpdatePrompt);
    v.push(ConfigItem::UpdateBuildDir);
    v.push(ConfigItem::UpdateCheckNow);
    v.push(ConfigItem::DefaultView);
    v.push(ConfigItem::DefocusToggle);
    v.push(ConfigItem::LivePollInterval);
    v.push(ConfigItem::LogMaxLines);
    v.push(ConfigItem::SubagentToolActionToggle);
    v.push(ConfigItem::MewxiVisualizerToggle);
    v.push(ConfigItem::MewxiShakeCycle);
    v.push(ConfigItem::MewxiStreaksToggle);
    v.push(ConfigItem::MewxiFxIntensityCycle);
    v.push(ConfigItem::MewxiAsciiStyleCycle);
    v.push(ConfigItem::StatusLineComposer);
    v
}

/// Self-update state the renderer needs, owned by the TUI event loop.
pub struct UpdateUi<'a> {
    pub channel: UpdateChannel,
    /// Automatic checks (startup + watcher) are enabled.
    pub check_enabled: bool,
    /// Minimum time between automatic checks.
    pub interval: UpdateInterval,
    pub prompt_enabled: bool,
    /// Configured build dir for updates; `None` = OS temp dir.
    pub build_dir: Option<&'a str>,
    /// When the build-dir row is being edited, the in-progress text
    /// (with a trailing cursor marker rendered by this view).
    pub build_dir_edit: Option<&'a str>,
    /// A background check is in flight right now.
    pub checking: bool,
    /// Most recent successful check this TUI run (or from cache).
    pub status: Option<&'a UpdateStatus>,
    /// Most recent check failure, if any.
    pub error: Option<&'a str>,
}

/// Poll-interval state for the "usage poll interval" Config row, built
/// each frame by the event loop. Mirrors the `UpdateUi::build_dir_edit`
/// pattern used for the build-dir row: `edit` is `Some` only while the
/// user is mid-typing a custom value and holds the in-progress buffer
/// (this view appends the trailing cursor marker itself).
#[derive(Clone, Copy)]
pub struct LivePollUi<'a> {
    pub secs: u64,
    pub edit: Option<&'a str>,
}

/// Line-cap state for the "log file max lines" Config row, built each
/// frame by the event loop. Mirrors `LivePollUi`.
#[derive(Clone, Copy)]
pub struct LogMaxLinesUi<'a> {
    pub lines: u64,
    pub edit: Option<&'a str>,
}

/// Current Mewxi-view settings the Config list renders, borrowed from
/// the event loop each frame. Mirrors `LivePollUi`.
#[derive(Clone, Copy)]
pub struct MewxiRowsUi<'a> {
    pub visualizer: bool,
    pub shake: &'a str,
    pub streaks: bool,
    pub fx_intensity: &'a str,
    pub ascii_style: &'a str,
}
impl MewxiRowsUi<'static> {
    /// The settings' documented defaults: visualizer on, shake
    /// "subtle", streaks on, fx "rave", ascii "y2k".
    #[cfg(test)]
    pub fn fallback() -> Self {
        MewxiRowsUi { visualizer: true, shake: "subtle", streaks: true, fx_intensity: "rave", ascii_style: "y2k" }
    }
}

/// Logs-panel state owned by the TUI event loop.
pub struct LogsUi<'a> {
    /// Full recent ring snapshot, oldest → newest (filtering happens here in the view).
    pub entries: &'a [crate::debug_log::LogEntry],
    /// None = show all origins.
    pub origin_filter: Option<crate::debug_log::LogOrigin>,
    /// None = show all kinds.
    pub kind_filter: Option<crate::debug_log::LogKind>,
    /// Lines scrolled up from the tail; 0 = pinned to newest.
    pub scroll: usize,
    /// When true the logs panel takes the flexible share of the vertical
    /// space and the settings list shrinks to a fixed window.
    pub expanded: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    area: Rect,
    snap: Option<&SetupSnapshot>,
    selected: usize,
    scroll: &mut usize,
    last_message: Option<&str>,
    defocus_input_after_send: bool,
    subagent_tool_action: bool,
    default_view: DefaultView,
    live_poll: LivePollUi,
    log_max_lines: LogMaxLinesUi,
    mewxi: &MewxiRowsUi,
    update: &UpdateUi,
    logs: &LogsUi,
    setup_rect: &mut Option<Rect>,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if logs.expanded {
            [
                Constraint::Length(3),  // header summary
                Constraint::Length(10), // settings list (fixed while logs expand)
                Constraint::Length(4),  // action hint + last message
                Constraint::Min(9),     // logs panel (flexible)
                Constraint::Length(1),  // footer
            ]
        } else {
            [
                Constraint::Length(3), // header summary
                Constraint::Min(8),    // settings list
                Constraint::Length(4), // action hint + last message
                Constraint::Length(9), // logs panel
                Constraint::Length(1), // footer
            ]
        })
        .split(area);

    render_header(f, rows[0], snap, update);
    *setup_rect = Some(rows[1]);
    render_list(f, rows[1], snap, selected, scroll, defocus_input_after_send, subagent_tool_action, default_view, live_poll, log_max_lines, mewxi, update);
    render_info(f, rows[2], snap, selected, defocus_input_after_send, subagent_tool_action, default_view, live_poll, log_max_lines, mewxi, update, last_message);
    render_logs(f, rows[3], logs);
    render_footer(
        f,
        rows[4],
        "4",
        "↑/↓ select · Enter action · a fix all · i ignore account · R rescan · Esc back · o/y/L logs · PgUp/PgDn scroll logs · ? help",
        true,
    );
}

/// Renders the logs panel: a bordered box showing the newest lines of
/// the (origin/kind-)filtered log tail, scrolled by `logs.scroll`
/// lines up from the newest entry.
fn render_logs(f: &mut Frame, area: Rect, logs: &LogsUi) {
    use crate::debug_log::LogKind;

    let filtered: Vec<&crate::debug_log::LogEntry> = logs
        .entries
        .iter()
        .filter(|e| match logs.origin_filter {
            Some(o) => e.origin == o,
            None => true,
        })
        .filter(|e| match logs.kind_filter {
            Some(k) => e.kind == k,
            None => true,
        })
        .collect();

    let visible_h = area.height.saturating_sub(2) as usize; // borders
    let max_scroll = filtered.len().saturating_sub(visible_h);
    let effective_scroll = logs.scroll.min(max_scroll);

    let end = filtered.len().saturating_sub(effective_scroll);
    let start = end.saturating_sub(visible_h);
    let window = &filtered[start..end];

    let origin_label = logs
        .origin_filter
        .map(|o| o.as_str().to_string())
        .unwrap_or_else(|| "all".to_string());
    let kind_label = logs
        .kind_filter
        .map(|k| k.as_str().to_string())
        .unwrap_or_else(|| "all".to_string());
    let mut title = format!(" Logs · origin: {origin_label} · type: {kind_label} · {}", filtered.len());
    if effective_scroll > 0 {
        title.push_str(&format!(" · ↑{effective_scroll}"));
    }
    title.push_str(if logs.expanded { " · L shrink" } else { " · L expand" });
    title.push(' ');

    let lines: Vec<Line> = if window.is_empty() {
        let text = if logs.entries.is_empty() {
            "no log entries"
        } else {
            "no entries match filters"
        };
        vec![Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        window
            .iter()
            .map(|entry| {
                let ts = entry
                    .ts
                    .with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string();
                let kind_color = match entry.kind {
                    LogKind::Api => Color::Magenta,
                    LogKind::FileRead => Color::Blue,
                    LogKind::FileWrite => Color::Yellow,
                    LogKind::Proc => Color::Cyan,
                    LogKind::Info => Color::DarkGray,
                    LogKind::Error => Color::Red,
                };
                let msg_style = if matches!(entry.kind, LogKind::Error) {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                };
                Line::from(vec![
                    Span::styled(format!("{ts} "), Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("[{}] ", entry.origin.as_str()),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!("[{}] ", entry.kind.as_str()),
                        Style::default().fg(kind_color),
                    ),
                    Span::styled(entry.message.clone(), msg_style),
                ])
            })
            .collect()
    };

    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
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
#[allow(clippy::too_many_arguments)]
fn build_lines(
    snap: Option<&SetupSnapshot>,
    selected: usize,
    defocus: bool,
    subagent_tool_action: bool,
    default_view: DefaultView,
    live_poll: LivePollUi,
    log_max_lines: LogMaxLinesUi,
    mewxi: &MewxiRowsUi,
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
            ConfigItem::UpdateCheck => {
                let (txt, color) = if update.check_enabled {
                    ("✓ on", Color::Green)
                } else {
                    ("off", Color::Yellow)
                };
                lines.push(row(
                    i,
                    "automatic checks".to_string(),
                    bold(format!("{txt:<16}"), color),
                    "check origin in the background (startup + watcher)".to_string(),
                ));
                owners.push(Some(i));
            }
            ConfigItem::UpdateInterval => {
                lines.push(row(
                    i,
                    "check interval".to_string(),
                    bold(format!("{:<16}", update.interval.as_str()), Color::Magenta),
                    format!("check at most {}", update.interval.label()),
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
            ConfigItem::UpdateBuildDir => {
                let (txt, color, extra) = if let Some(edit) = update.build_dir_edit {
                    (
                        format!("{edit}▏"),
                        Color::Yellow,
                        "Enter save · Esc cancel · empty = system temp".to_string(),
                    )
                } else {
                    match update.build_dir {
                        Some(d) => (
                            d.to_string(),
                            Color::Magenta,
                            "where updates clone + build".to_string(),
                        ),
                        None => (
                            "system temp".to_string(),
                            Color::Magenta,
                            std::env::temp_dir().display().to_string(),
                        ),
                    }
                };
                lines.push(row(i, "update build dir".to_string(), bold(format!("{txt:<16}"), color), extra));
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
            ConfigItem::DefaultView => {
                lines.push(row(
                    i,
                    "default view".to_string(),
                    bold(format!("{:<16}", default_view.as_str()), Color::Magenta),
                    format!("open {} when mewxi starts", default_view.label()),
                ));
                owners.push(Some(i));
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
            ConfigItem::LivePollInterval => {
                let (txt, color, extra) = if let Some(edit) = live_poll.edit {
                    (
                        format!("{edit}▏"),
                        Color::Yellow,
                        "Enter save · Esc cancel · e.g. 90s, 2m, 1m30s".to_string(),
                    )
                } else {
                    (
                        fmt_poll_secs(live_poll.secs),
                        Color::Magenta,
                        "min time between usage-endpoint probes (per account)".to_string(),
                    )
                };
                lines.push(row(i, "usage poll interval".to_string(), bold(format!("{txt:<16}"), color), extra));
                owners.push(Some(i));
            }
            ConfigItem::LogMaxLines => {
                let (txt, color, extra) = if let Some(edit) = log_max_lines.edit {
                    (
                        format!("{edit}▏"),
                        Color::Yellow,
                        "Enter save · Esc cancel · e.g. 5000, 5k".to_string(),
                    )
                } else {
                    (
                        fmt_log_lines(log_max_lines.lines),
                        Color::Magenta,
                        "line cap for the debug-log file — oldest trimmed (default 10k)".to_string(),
                    )
                };
                lines.push(row(i, "log file max lines".to_string(), bold(format!("{txt:<16}"), color), extra));
                owners.push(Some(i));
            }
            ConfigItem::SubagentToolActionToggle => {
                let (txt, color) = if subagent_tool_action {
                    ("✓ on", Color::Green)
                } else {
                    ("off", Color::Yellow)
                };
                lines.push(row(
                    i,
                    "sub-agent tool call".to_string(),
                    bold(format!("{txt:<16}"), color),
                    "suffix agent captions with the in-flight tool".to_string(),
                ));
                owners.push(Some(i));

                push_header(&mut lines, &mut owners, "Mewxi view", false);
            }
            ConfigItem::MewxiVisualizerToggle => {
                let (txt, color) = if mewxi.visualizer {
                    ("✓ on", Color::Green)
                } else {
                    ("off", Color::Yellow)
                };
                lines.push(row(
                    i,
                    "agent visualizer".to_string(),
                    bold(format!("{txt:<16}"), color),
                    "animate live agent activity in the rave view".to_string(),
                ));
                owners.push(Some(i));
            }
            ConfigItem::MewxiShakeCycle => {
                lines.push(row(
                    i,
                    "screen shake".to_string(),
                    bold(format!("{:<16}", mewxi.shake), Color::Magenta),
                    "off · subtle · full".to_string(),
                ));
                owners.push(Some(i));
            }
            ConfigItem::MewxiStreaksToggle => {
                let (txt, color) = if mewxi.streaks {
                    ("✓ on", Color::Green)
                } else {
                    ("off", Color::Yellow)
                };
                lines.push(row(
                    i,
                    "streaks".to_string(),
                    bold(format!("{txt:<16}"), color),
                    "celebrate win/output streaks".to_string(),
                ));
                owners.push(Some(i));
            }
            ConfigItem::MewxiFxIntensityCycle => {
                lines.push(row(
                    i,
                    "fx intensity".to_string(),
                    bold(format!("{:<16}", mewxi.fx_intensity), Color::Magenta),
                    "chill · rave · insane".to_string(),
                ));
                owners.push(Some(i));
            }
            ConfigItem::MewxiAsciiStyleCycle => {
                lines.push(row(
                    i,
                    "ascii style".to_string(),
                    bold(format!("{:<16}", mewxi.ascii_style), Color::Magenta),
                    "y2k · classic".to_string(),
                ));
                owners.push(Some(i));

                push_header(&mut lines, &mut owners, "Status line", false);
            }
            ConfigItem::StatusLineComposer => {
                lines.push(row(
                    i,
                    "status line blocks".to_string(),
                    bold(format!("{:<16}", "open ↵"), Color::Magenta),
                    "reorder · toggle · add blocks (live preview)".to_string(),
                ));
                owners.push(Some(i));
            }
        }
    }

    (lines, owners)
}

#[allow(clippy::too_many_arguments)]
fn render_list(
    f: &mut Frame,
    area: Rect,
    snap: Option<&SetupSnapshot>,
    selected: usize,
    scroll: &mut usize,
    defocus: bool,
    subagent_tool_action: bool,
    default_view: DefaultView,
    live_poll: LivePollUi,
    log_max_lines: LogMaxLinesUi,
    mewxi: &MewxiRowsUi,
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

    let (lines, owners) = build_lines(
        snap,
        selected,
        defocus,
        subagent_tool_action,
        default_view,
        live_poll,
        log_max_lines,
        mewxi,
        update,
    );

    // Edge-triggered scrolling: the cursor moves freely inside the visible
    // window and the list only scrolls once the cursor reaches the top or
    // bottom edge. `scroll` persists the windowed offset across renders.
    let visible_h = area.height.saturating_sub(2) as usize; // borders
    let sel_line = owners
        .iter()
        .position(|o| *o == Some(selected))
        .unwrap_or(0);
    let max_offset = lines.len().saturating_sub(visible_h);
    let mut offset = (*scroll).min(max_offset);
    if visible_h > 0 {
        if sel_line < offset {
            // cursor pushed past the top edge — scroll up to meet it
            offset = sel_line;
        } else if sel_line >= offset + visible_h {
            // cursor pushed past the bottom edge — scroll down to meet it
            offset = sel_line + 1 - visible_h;
        }
    }
    *scroll = offset;
    let windowed: Vec<Line> = lines.into_iter().skip(offset).take(visible_h.max(1)).collect();

    f.render_widget(Paragraph::new(windowed).block(block), area);
}

/// What Enter will do on the selected row — shown before the user
/// presses it so no action is a surprise.
#[allow(clippy::too_many_arguments)]
fn action_hint(
    snap: Option<&SetupSnapshot>,
    selected: usize,
    defocus: bool,
    subagent_tool_action: bool,
    default_view: DefaultView,
    live_poll: LivePollUi,
    log_max_lines: LogMaxLinesUi,
    mewxi: &MewxiRowsUi,
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
        ConfigItem::UpdateCheck => if update.check_enabled {
            "Enter: stop checking for updates automatically (manual checks still work)"
                .to_string()
        } else {
            "Enter: check for updates automatically on startup and from the watcher".to_string()
        },
        ConfigItem::UpdateInterval => format!(
            "Enter: check {} instead (currently {})",
            update.interval.cycled().label(),
            update.interval.label()
        ),
        ConfigItem::UpdatePrompt => if update.prompt_enabled {
            "Enter: stop asking about updates when the TUI starts".to_string()
        } else {
            "Enter: ask about available updates when the TUI starts".to_string()
        },
        ConfigItem::UpdateBuildDir => {
            if update.build_dir_edit.is_some() {
                "type the build directory — Enter: save · Esc: cancel · empty resets to the system temp dir"
                    .to_string()
            } else {
                "Enter: edit where updates clone + build (default: the OS temp dir)".to_string()
            }
        }
        ConfigItem::UpdateCheckNow => {
            if update.checking {
                "checking origin — hold on…".to_string()
            } else if update.status.is_some_and(|s| s.available) {
                "Enter: install the update now (git + cargo rebuild, takes a minute)".to_string()
            } else {
                "Enter: check origin for a newer mewxi now".to_string()
            }
        }
        ConfigItem::DefaultView => format!(
            "Enter: start in {} instead (currently {})",
            default_view.cycled().label(),
            default_view.label()
        ),
        ConfigItem::DefocusToggle => if defocus {
            "Enter: keep the prompt box focused after sending (type follow-ups immediately)"
                .to_string()
        } else {
            "Enter: unfocus the prompt box after sending (keys go back to navigation)".to_string()
        },
        ConfigItem::LivePollInterval => {
            if live_poll.edit.is_some() {
                "type an interval (30s–15m) — Enter: save · Esc: cancel".to_string()
            } else {
                format!(
                    "Enter: probe every {} instead (currently {}) · e: type a custom interval (30s–15m)",
                    fmt_poll_secs(next_live_poll_step(live_poll.secs)),
                    fmt_poll_secs(live_poll.secs)
                )
            }
        }
        ConfigItem::LogMaxLines => {
            if log_max_lines.edit.is_some() {
                format!(
                    "type a line cap ({LOG_LINES_MIN}–{LOG_LINES_MAX}) — Enter: save · Esc: cancel"
                )
            } else {
                format!(
                    "Enter: cap at {} lines instead (currently {}) · e: type a custom cap ({LOG_LINES_MIN}–{LOG_LINES_MAX}) — caps the debug-log file's line count, trimming the oldest lines to make room for new ones (default 10k); every running mewxi process picks it up on its next log write",
                    fmt_log_lines(next_log_lines_step(log_max_lines.lines)),
                    fmt_log_lines(log_max_lines.lines)
                )
            }
        }
        ConfigItem::SubagentToolActionToggle => if subagent_tool_action {
            "Enter: hide the — Tool(arg) suffix on sub-agent captions".to_string()
        } else {
            "Enter: show the agent's in-flight tool call after its caption — e.g. — Bash(cargo test)"
                .to_string()
        },
        ConfigItem::MewxiVisualizerToggle => if mewxi.visualizer {
            "Enter: turn the agent-activity visualizer off".to_string()
        } else {
            "Enter: turn the agent-activity visualizer on".to_string()
        },
        ConfigItem::MewxiShakeCycle => format!(
            "Enter: screen shake {} (currently {})",
            next_shake_level(mewxi.shake),
            mewxi.shake
        ),
        ConfigItem::MewxiStreaksToggle => if mewxi.streaks {
            "Enter: stop celebrating streaks".to_string()
        } else {
            "Enter: celebrate win/output streaks".to_string()
        },
        ConfigItem::MewxiFxIntensityCycle => format!(
            "Enter: fx intensity {} instead (currently {})",
            next_fx_intensity(mewxi.fx_intensity),
            mewxi.fx_intensity
        ),
        ConfigItem::MewxiAsciiStyleCycle => format!(
            "Enter: ascii style {} instead (currently {})",
            next_ascii_style(mewxi.ascii_style),
            mewxi.ascii_style
        ),
        ConfigItem::StatusLineComposer => {
            "Enter: open the status-line composer — reorder / toggle / add / edit blocks with a live preview"
                .to_string()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_info(
    f: &mut Frame,
    area: Rect,
    snap: Option<&SetupSnapshot>,
    selected: usize,
    defocus: bool,
    subagent_tool_action: bool,
    default_view: DefaultView,
    live_poll: LivePollUi,
    log_max_lines: LogMaxLinesUi,
    mewxi: &MewxiRowsUi,
    update: &UpdateUi,
    last_message: Option<&str>,
) {
    let hint = action_hint(
        snap,
        selected,
        defocus,
        subagent_tool_action,
        default_view,
        live_poll,
        log_max_lines,
        mewxi,
        update,
    );
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
            check_enabled: true,
            interval: UpdateInterval::Hour6,
            prompt_enabled: true,
            build_dir: None,
            build_dir_edit: None,
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
        assert_eq!(list[4], ConfigItem::UpdateCheck);
        assert_eq!(list[5], ConfigItem::UpdateInterval);
        assert_eq!(list[6], ConfigItem::UpdatePrompt);
        assert_eq!(list[7], ConfigItem::UpdateBuildDir);
        assert_eq!(list[8], ConfigItem::UpdateCheckNow);
        assert_eq!(list[9], ConfigItem::DefaultView);
        assert_eq!(list[10], ConfigItem::DefocusToggle);
        assert_eq!(list[11], ConfigItem::LivePollInterval);
        assert_eq!(list[12], ConfigItem::LogMaxLines);
        assert_eq!(list[13], ConfigItem::SubagentToolActionToggle);
        assert_eq!(list[14], ConfigItem::MewxiVisualizerToggle);
        assert_eq!(list[15], ConfigItem::MewxiShakeCycle);
        assert_eq!(list[16], ConfigItem::MewxiStreaksToggle);
        assert_eq!(list[17], ConfigItem::MewxiFxIntensityCycle);
        assert_eq!(list[18], ConfigItem::MewxiAsciiStyleCycle);
        assert_eq!(list[19], ConfigItem::StatusLineComposer);
        // No snapshot yet → only the fixed rows.
        assert_eq!(items(None).len(), 18);
    }

    #[test]
    fn fmt_poll_secs_formats_minutes_and_seconds() {
        assert_eq!(fmt_poll_secs(30), "30s");
        assert_eq!(fmt_poll_secs(45), "45s");
        assert_eq!(fmt_poll_secs(90), "1m30s");
        assert_eq!(fmt_poll_secs(120), "2m");
        assert_eq!(fmt_poll_secs(630), "10m30s");
        assert_eq!(fmt_poll_secs(900), "15m");
    }

    #[test]
    fn next_live_poll_step_advances_by_30s_and_wraps() {
        assert_eq!(next_live_poll_step(30), 60);
        // Off-grid values snap up to the next multiple of 30.
        assert_eq!(next_live_poll_step(45), 60);
        assert_eq!(next_live_poll_step(870), 900);
        assert_eq!(next_live_poll_step(890), 900);
        // Wraps past the ceiling back to the floor.
        assert_eq!(next_live_poll_step(900), 30);
        assert_eq!(next_live_poll_step(10), 30);
    }

    #[test]
    fn parse_poll_input_accepts_seconds_and_minute_shapes() {
        assert_eq!(parse_poll_input("90"), Ok(90));
        assert_eq!(parse_poll_input("90s"), Ok(90));
        assert_eq!(parse_poll_input("2m"), Ok(120));
        assert_eq!(parse_poll_input("1m30s"), Ok(90));
        assert_eq!(parse_poll_input(" 15M "), Ok(900));
    }

    #[test]
    fn parse_poll_input_rejects_out_of_range_and_garbage() {
        assert_eq!(
            parse_poll_input("29"),
            Err("interval must be between 30s and 15m".to_string())
        );
        assert_eq!(
            parse_poll_input("16m"),
            Err("interval must be between 30s and 15m".to_string())
        );
        assert_eq!(
            parse_poll_input("abc"),
            Err("can't parse 'abc' — try 90s, 2m or 1m30s".to_string())
        );
        // No digits at all — a parse failure, not a 0s range failure.
        assert_eq!(
            parse_poll_input("s"),
            Err("can't parse 's' — try 90s, 2m or 1m30s".to_string())
        );
        assert_eq!(
            parse_poll_input(""),
            Err("can't parse '' — try 90s, 2m or 1m30s".to_string())
        );
    }

    #[test]
    fn fmt_log_lines_formats_thousands_and_raw() {
        assert_eq!(fmt_log_lines(500), "500");
        assert_eq!(fmt_log_lines(1_000), "1k");
        assert_eq!(fmt_log_lines(5_500), "5500");
        assert_eq!(fmt_log_lines(10_000), "10k");
        assert_eq!(fmt_log_lines(100_000), "100k");
    }

    #[test]
    fn next_log_lines_step_advances_by_1000_and_wraps() {
        assert_eq!(next_log_lines_step(1_000), 2_000);
        // Off-grid values snap up to the next multiple of 1,000.
        assert_eq!(next_log_lines_step(1_500), 2_000);
        assert_eq!(next_log_lines_step(99_000), 100_000);
        assert_eq!(next_log_lines_step(99_500), 100_000);
        // Wraps past the ceiling back to the floor.
        assert_eq!(next_log_lines_step(100_000), 500);
        // Below the floor, still snaps up to the next 1,000 grid point
        // (the floor only kicks in once the ceiling wraps around).
        assert_eq!(next_log_lines_step(200), 1_000);
    }

    #[test]
    fn parse_log_lines_input_accepts_plain_and_k_suffix() {
        assert_eq!(parse_log_lines_input("5000"), Ok(5000));
        assert_eq!(parse_log_lines_input("5k"), Ok(5000));
        assert_eq!(parse_log_lines_input(" 10K "), Ok(10_000));
        assert_eq!(parse_log_lines_input("500"), Ok(500));
    }

    #[test]
    fn parse_log_lines_input_rejects_out_of_range_and_garbage() {
        assert_eq!(
            parse_log_lines_input("499"),
            Err("line cap must be between 500 and 100000".to_string())
        );
        assert_eq!(
            parse_log_lines_input("100001"),
            Err("line cap must be between 500 and 100000".to_string())
        );
        assert_eq!(
            parse_log_lines_input("abc"),
            Err("can't parse 'abc' — try 5000 or 5k".to_string())
        );
        assert_eq!(
            parse_log_lines_input(""),
            Err("can't parse '' — try 5000 or 5k".to_string())
        );
    }

    fn render_to_text(selected: usize, status: Option<UpdateStatus>) -> String {
        render_to_text_full(selected, status, &[], None, None, 0, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_to_text_full(
        selected: usize,
        status: Option<UpdateStatus>,
        entries: &[crate::debug_log::LogEntry],
        origin_filter: Option<crate::debug_log::LogOrigin>,
        kind_filter: Option<crate::debug_log::LogKind>,
        log_scroll: usize,
        expanded: bool,
    ) -> String {
        let snap = snapshot();
        // Tall enough that the full settings list (~28 lines across all
        // sections, including the new "Mewxi view" section) stays
        // on-screen alongside the logs panel (3 header + list + 4 info +
        // 9 logs + 1 footer = 17 fixed rows; the list gets the flexible
        // `Min(8)` share, so a total height of 52 gives it ~35 rows).
        let backend = TestBackend::new(100, 52);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let mut rect = None;
                let mut scroll = 0usize;
                let logs = LogsUi {
                    entries,
                    origin_filter,
                    kind_filter,
                    scroll: log_scroll,
                    expanded,
                };
                render(
                    f,
                    f.area(),
                    Some(&snap),
                    selected,
                    &mut scroll,
                    Some("did a thing"),
                    true,
                    false,
                    DefaultView::All,
                    LivePollUi { secs: 60, edit: None },
                    LogMaxLinesUi { lines: 10_000, edit: None },
                    &MewxiRowsUi::fallback(),
                    &update_ui(status.as_ref()),
                    &logs,
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
            "automatic checks",
            "check interval",
            "ask on startup",
            "update build dir",
            "check for updates",
            "Preferences",
            "defocus input after send",
            "usage poll interval",
            "log file max lines",
            "Mewxi view",
            "agent visualizer",
            "screen shake",
            "streaks",
            "fx intensity",
            "ascii style",
            "subtle",
            "rave",
            "y2k",
            "Status line",
            "status line blocks",
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
        // Select the check-now row (index 8 with two accounts).
        let text = render_to_text(8, Some(status));
        assert!(text.contains("⬆ v0.2.0 available"), "header notice missing:\n{text}");
        assert!(text.contains("install the update now"), "hint missing:\n{text}");
    }

    #[test]
    fn logs_panel_renders_entries() {
        use crate::debug_log::{LogEntry, LogKind, LogOrigin};
        let now = chrono::Utc::now();
        let entries = vec![
            LogEntry {
                ts: now,
                origin: LogOrigin::Usage,
                kind: LogKind::Api,
                message: "GET /usage".into(),
            },
            LogEntry {
                ts: now,
                origin: LogOrigin::Setup,
                kind: LogKind::FileWrite,
                message: "wrote settings.json".into(),
            },
            LogEntry {
                ts: now,
                origin: LogOrigin::Tui,
                kind: LogKind::Error,
                message: "boom".into(),
            },
        ];
        let text = render_to_text_full(0, None, &entries, None, None, 0, false);
        for needle in [
            "GET /usage",
            "[usage]",
            "[api]",
            "wrote settings.json",
            "[setup]",
            "[write]",
            "boom",
            "[tui]",
            "[error]",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(text.contains("origin: all"), "title missing origin:all in:\n{text}");
        assert!(text.contains("type: all · 3"), "title missing count in:\n{text}");
    }

    #[test]
    fn logs_panel_filters_by_origin_and_kind() {
        use crate::debug_log::{LogEntry, LogKind, LogOrigin};
        let now = chrono::Utc::now();
        let entries = vec![
            LogEntry {
                ts: now,
                origin: LogOrigin::Usage,
                kind: LogKind::Api,
                message: "usage message".into(),
            },
            LogEntry {
                ts: now,
                origin: LogOrigin::Setup,
                kind: LogKind::Info,
                message: "setup message".into(),
            },
        ];
        let text = render_to_text_full(0, None, &entries, Some(LogOrigin::Usage), None, 0, false);
        assert!(text.contains("usage message"), "kept entry missing:\n{text}");
        assert!(!text.contains("setup message"), "filtered entry leaked:\n{text}");
        assert!(text.contains("origin: usage"), "title missing filter in:\n{text}");
        assert!(text.contains("type: all · 1"), "title missing filtered count in:\n{text}");
    }

    #[test]
    fn logs_panel_empty_state() {
        let text = render_to_text_full(0, None, &[], None, None, 0, false);
        assert!(text.contains("no log entries"), "empty-state text missing:\n{text}");

        use crate::debug_log::{LogEntry, LogKind, LogOrigin};
        let entries = vec![LogEntry {
            ts: chrono::Utc::now(),
            origin: LogOrigin::Setup,
            kind: LogKind::Info,
            message: "setup message".into(),
        }];
        let filtered_out = render_to_text_full(0, None, &entries, Some(LogOrigin::Usage), None, 0, false);
        assert!(
            filtered_out.contains("no entries match filters"),
            "filtered-empty-state text missing:\n{filtered_out}"
        );
    }

    #[test]
    fn logs_panel_expanded_shows_more_and_flips_title_hint() {
        use crate::debug_log::{LogEntry, LogKind, LogOrigin};
        let now = chrono::Utc::now();
        let entries: Vec<LogEntry> = (0..20)
            .map(|i| LogEntry {
                ts: now,
                origin: LogOrigin::Setup,
                kind: LogKind::Info,
                message: format!("entry {i}"),
            })
            .collect();

        // Collapsed (9-row) panel only has room for the last 7 lines, so
        // an early entry is scrolled off the top.
        let collapsed = render_to_text_full(0, None, &entries, None, None, 0, false);
        assert!(collapsed.contains("L expand"), "collapsed title missing hint:\n{collapsed}");
        assert!(
            !collapsed.contains("entry 5"),
            "entry 5 should be cut off in the collapsed panel:\n{collapsed}"
        );

        // Expanded, the logs panel takes the flexible share of the
        // layout and has room for all 20 entries.
        let expanded = render_to_text_full(0, None, &entries, None, None, 0, true);
        assert!(expanded.contains("L shrink"), "expanded title missing hint:\n{expanded}");
        assert!(
            expanded.contains("entry 5"),
            "entry 5 should be visible once the logs panel expands:\n{expanded}"
        );
    }

    #[test]
    fn mewxi_cycle_helpers() {
        // Happy-path progression through the full cycle.
        assert_eq!(next_shake_level("off"), "subtle");
        assert_eq!(next_shake_level("subtle"), "full");
        assert_eq!(next_shake_level("full"), "off");

        assert_eq!(next_fx_intensity("chill"), "rave");
        assert_eq!(next_fx_intensity("rave"), "insane");
        assert_eq!(next_fx_intensity("insane"), "chill");

        assert_eq!(next_ascii_style("y2k"), "classic");
        assert_eq!(next_ascii_style("classic"), "y2k");

        // Case-insensitivity (and surrounding whitespace is trimmed).
        assert_eq!(next_shake_level("SUBTLE"), "full");
        assert_eq!(next_shake_level(" Off "), "subtle");

        // Unknown input ⇒ treated as the default, returns its successor.
        assert_eq!(next_shake_level("nope"), "full"); // default "subtle" → "full"
        assert_eq!(next_fx_intensity(""), "insane"); // default "rave" → "insane"
        assert_eq!(next_ascii_style("zzz"), "classic"); // default "y2k" → "classic"
    }

    #[test]
    fn default_view_mewxi() {
        assert_eq!(DefaultView::from_config(Some("mewxi")), DefaultView::Mewxi);
        assert_eq!(DefaultView::from_config(Some("rave")), DefaultView::Mewxi);
        assert_eq!(DefaultView::from_config(Some("5")), DefaultView::Mewxi);
        assert_eq!(DefaultView::Config.cycled(), DefaultView::Mewxi);
        assert_eq!(DefaultView::Mewxi.cycled(), DefaultView::All);
        assert_eq!(DefaultView::Mewxi.as_str(), "mewxi");
    }

    fn render_to_text_mewxi(selected: usize, mewxi: &MewxiRowsUi) -> String {
        let snap = snapshot();
        // Same generous height as `render_to_text_full` so the whole
        // settings list (including the Mewxi view section) fits without
        // windowing.
        let backend = TestBackend::new(100, 52);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let mut rect = None;
                let mut scroll = 0usize;
                let logs = LogsUi {
                    entries: &[],
                    origin_filter: None,
                    kind_filter: None,
                    scroll: 0,
                    expanded: false,
                };
                render(
                    f,
                    f.area(),
                    Some(&snap),
                    selected,
                    &mut scroll,
                    Some("did a thing"),
                    true,
                    false,
                    DefaultView::All,
                    LivePollUi { secs: 60, edit: None },
                    LogMaxLinesUi { lines: 10_000, edit: None },
                    mewxi,
                    &update_ui(None),
                    &logs,
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
    fn mewxi_section_renders() {
        let mewxi = MewxiRowsUi {
            visualizer: false,
            shake: "full",
            streaks: false,
            fx_intensity: "insane",
            ascii_style: "classic",
        };
        let text = render_to_text_mewxi(0, &mewxi);
        assert!(text.contains("Mewxi view"), "section header missing:\n{text}");
        assert!(text.contains("full"), "shake value missing:\n{text}");
        assert!(text.contains("insane"), "fx intensity value missing:\n{text}");
        assert!(text.contains("classic"), "ascii style value missing:\n{text}");
        assert!(text.contains("off"), "visualizer-off row missing:\n{text}");
    }
}
