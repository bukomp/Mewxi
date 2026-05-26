//! Render claude's PTY screen as a ratatui overlay when an interactive
//! prompt (model-switch confirmation, multiselect, accept-edit y/N) is
//! up and would otherwise be invisible inside mewxi's JSONL-driven
//! session view.
//!
//! The PTY bytes are already being parsed into a `vt100::Screen` over in
//! [`crate::agent_control::PtySession`]; this module is presentation +
//! detection only. The event-loop side in `tui/mod.rs` decides when to
//! open the overlay (via [`prompt_visible`]) and routes keystrokes
//! straight to the PTY while it's open.
//!
//! The overlay shows only the **popup region** of claude's 40×120 grid,
//! not the whole TUI — mewxi remains the primary view. We locate the
//! popup by finding the prompt-marker row, then expanding up/down until
//! we hit a box-corner or blank row.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use vt100::Screen;

/// Substring markers that strongly suggest claude has popped a TUI
/// overlay (y/N prompts, accept-edit, continue dialog). Pattern-based
/// and intentionally easy to extend — false positives are recoverable
/// with Ctrl-]. Anything added here **must not** appear in claude's
/// normal chat input box, otherwise the overlay triggers on every
/// keystroke. Picker UIs (numbered or unnumbered options with a cursor)
/// are detected structurally by [`picker_cursor_col`] + sibling indent
/// match — never name a cursor char here, claude uses one of several
/// and reusing the input-prompt glyph would re-introduce the every-
/// keystroke false positive.
/// Specific markers that almost never appear in chat prose; trigger
/// from anywhere on the screen.
const PROMPT_MARKERS_SPECIFIC: &[&str] = &[
    "[y/N]",
    "[Y/n]",
    "(y/n)",
    "(Y/n)",
    "Esc to cancel",   // picker hint footer
    "Enter to select", // picker hint footer
];

/// Common-English markers that DO appear in claude's chat prose
/// ("Press y to confirm", "Use this approach instead"). Only trigger
/// when the match row is inside a box-drawn popup (border line within
/// 3 rows above or below) — chat output is plain text with no box
/// chars near it.
const PROMPT_MARKERS_PROSE: &[&str] = &[
    "Continue?",
    "Press y ",
    "Use this ",     // accept-edit prompt
    "Do you trust",  // startup trust dialog for unrecognised folders
    "trust the files", // alternate phrasing the trust dialog uses
];

/// Max overlay height in PTY rows. The widest popup we've seen is the
/// accept-edit preview, which is ~15 rows; cap at 20 so a runaway match
/// can't blow up over mewxi's view.
const MAX_POPUP_ROWS: u16 = 20;

/// True when claude is likely waiting for the user to answer a TUI
/// overlay. We trust the screen contents only — `awaiting_marker` is
/// kept in the signature for future use, but does NOT short-circuit
/// to true: claude's `PostToolUse` hook doesn't always fire (e.g.
/// when the user Esc-interrupts a tool-use prompt), so the marker
/// file stays set and the overlay would otherwise paint forever over
/// an empty input box. Screen markers cover y/N (`[y/N]`, `(y/n)`),
/// continue dialogs, accept-edit (`Use this `), and numbered pickers
/// (model switch, plan acceptance, plan-mode pick).
pub fn prompt_visible(screen: &Screen, _awaiting_marker: bool) -> bool {
    find_marker_row(screen).is_some()
}

/// Diagnostic helper: when the overlay is currently triggered, return
/// the matched marker (row, marker string, full row text) so callers
/// can log why detection fired. Returns `None` when no marker hit.
/// Mirrors [`find_marker_row`]'s gating rules exactly.
pub fn matched_marker(screen: &Screen) -> Option<(u16, &'static str, String)> {
    let (rows, cols) = screen.size();
    for r in (0..rows).rev() {
        let txt = row_text(screen, r, cols);
        if let Some(m) = PROMPT_MARKERS_SPECIFIC.iter().find(|m| txt.contains(*m)) {
            return Some((r, m, txt));
        }
        if let Some(m) = PROMPT_MARKERS_PROSE.iter().find(|m| txt.contains(*m)) {
            if row_near_box_border(screen, r, cols) {
                return Some((r, m, txt));
            }
        }
    }
    None
}

/// Picker row detection by **shape**, not by digit prefix. Used only
/// inside an already-anchored popup region (`parse_picker`) to walk
/// option rows — NOT as the overlay trigger. The trigger lives in
/// [`find_marker_row`] and requires a PROMPT_MARKER hit, because
/// shape-only matching produces too many false positives in claude's
/// bullet-marker scrollback.
///
/// claude's pickers come in two flavours handled uniformly:
///   - numbered: `❯ 1. Yes` / `  2. No`               (the model picker)
///   - unnumbered: `❯ Auto-accept edits` / `  Manual` (the AskUserQuestion picker)

/// Returns `(cursor_col, prefix)` when `line` is a picker's selected
/// row: `<prefix><cursor><space><text>`, where `prefix` is the leading
/// run of whitespace + box-drawing chars. The cursor glyph must be a
/// single non-alphanumeric, non-whitespace, non-box character (e.g.
/// `❯`, `›`, `▶`). `prefix` is captured exactly so siblings must
/// share the same leading-prefix structure — that prevents the input
/// box (`> user_text` with no border) from being mistaken as the
/// selected row of an adjacent y/N popup (`│ [y/N] │` with a border).
fn picker_cursor_info(line: &str) -> Option<(usize, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut idx = 0;
    while idx < chars.len() && (chars[idx].is_whitespace() || is_box_char(chars[idx])) {
        idx += 1;
    }
    let cursor = *chars.get(idx)?;
    if cursor.is_alphanumeric() || is_box_char(cursor) {
        return None;
    }
    if chars.get(idx + 1) != Some(&' ') {
        return None;
    }
    // Text after `<cursor> ` must be a non-empty, non-box payload.
    let has_text = chars[idx + 2..]
        .iter()
        .any(|c| !c.is_whitespace() && !is_box_char(*c));
    if !has_text {
        return None;
    }
    // Exclude claude's slash-command preview shape: `▶ /model haiku`
    // followed by a one-line description. Structurally identical to a
    // 2-option picker (cursor + 1 sibling), but claude pickers never
    // place a `/`-prefixed token in the cursor row.
    if chars.get(idx + 2) == Some(&'/') {
        return None;
    }
    let prefix: String = chars[..idx].iter().collect();
    Some((idx, prefix))
}

/// True when `line` is a sibling (unselected) option for a cursor row
/// with the given `cursor_prefix`. Sibling must:
///   1. Start with the **same exact** `cursor_prefix` as the cursor row
///      (so a bordered popup and a borderless input box can't share
///      siblings).
///   2. Have exactly 2 spaces immediately after the prefix (matching
///      the cursor + its trailing space).
///   3. Then have non-empty, non-box content.
fn is_picker_sibling_line(line: &str, cursor_prefix: &str) -> bool {
    if !line.starts_with(cursor_prefix) {
        return false;
    }
    let after: Vec<char> = line[cursor_prefix.len()..].chars().collect();
    if after.len() < 3 {
        return false;
    }
    if after[0] != ' ' || after[1] != ' ' {
        return false;
    }
    after[2..]
        .iter()
        .any(|c| !c.is_whitespace() && !is_box_char(*c))
}

fn is_box_char(c: char) -> bool {
    // Unicode Box Drawing block.
    matches!(c as u32, 0x2500..=0x257F)
}

/// Find the bottom-most row matching a PROMPT_MARKER substring. Used
/// by both [`prompt_visible`] and [`find_popup_region`] so the trigger
/// and the crop anchor agree.
///
/// Structural cursor+sibling detection is intentionally NOT used as a
/// trigger here: claude's chat scrollback uses bullet glyphs (`●`,
/// `*`, `›`) that match `picker_cursor_info`, and any indented
/// continuation line (e.g. `  Set model to Haiku 4.5 for this
/// session`) matches `is_picker_sibling_line` — so structural
/// detection fires constantly on normal chat. Real claude pickers
/// always render a `↑/↓ navigate · Esc to cancel · Enter to select`
/// hint footer; the matching substrings live in `PROMPT_MARKERS_SPECIFIC`
/// and `PROMPT_MARKERS_PROSE` (the latter additionally requires a box
/// border within 3 rows to suppress chat-prose false positives).
fn find_marker_row(screen: &Screen) -> Option<u16> {
    let (rows, cols) = screen.size();
    for r in (0..rows).rev() {
        let txt = row_text(screen, r, cols);
        if PROMPT_MARKERS_SPECIFIC.iter().any(|m| txt.contains(m)) {
            return Some(r);
        }
        if PROMPT_MARKERS_PROSE.iter().any(|m| txt.contains(m))
            && row_near_box_border(screen, r, cols)
        {
            return Some(r);
        }
    }
    None
}

/// True when row `r` is inside a box-drawn popup — i.e. any row within
/// 3 lines above OR below contains a box-drawing character. Real
/// claude prompts always render with ratatui-style borders; chat
/// prose has no box chars within several rows.
fn row_near_box_border(screen: &Screen, r: u16, cols: u16) -> bool {
    let lo = r.saturating_sub(3);
    let hi = r.saturating_add(3);
    for rr in lo..=hi {
        let txt = row_text(screen, rr, cols);
        if txt.chars().any(is_box_char) {
            return true;
        }
    }
    false
}

fn is_horizontal_separator(line: &str) -> bool {
    let t = line.trim();
    // Char count, not byte length — `─` is 3 bytes, so byte-length
    // comparison would call a 2-glyph `──` a separator.
    t.chars().count() >= 4 && t.chars().all(|c| matches!(c, '─' | '━' | '-' | '═'))
}

/// Render the popup the way mewxi prefers: if the prompt is a numbered
/// picker (Switch model? / plan-mode pick / etc.) we parse its
/// structure and re-render it as a native mewxi modal so it matches
/// the rest of the chrome. Otherwise (y/N, accept-edit with diff,
/// anything we can't structurally parse) we fall back to cropping
/// claude's rendered PTY screen and surfacing it verbatim.
///
/// Keystrokes always pass through to the PTY regardless of render
/// path; for the native picker the user's ↑↓ move claude's real
/// cursor, we re-parse next frame, and the modal stays in sync.
///
/// `plan_content` is the markdown body of the pending `ExitPlanMode`
/// tool_use for this session, sourced from the transcript JSONL by the
/// caller. When `Some`, it's spliced into the picker modal so the user
/// can see what they're approving — the plan text isn't on the PTY
/// screen, only in the JSONL. When `None`, render the picker as-is.
///
/// Tying this to the JSONL tool_use (a protocol-level signal) instead
/// of pattern-matching the picker's prose means rewordings of claude's
/// plan-acceptance dialog don't silently disable the plan view.
pub fn render(frame: &mut Frame, area: Rect, screen: &Screen, plan_content: Option<&str>) {
    if let Some(picker) = parse_picker(screen) {
        render_native_picker(frame, area, &picker, plan_content);
    } else {
        render_pty_crop(frame, area, screen);
    }
}

fn render_pty_crop(frame: &mut Frame, area: Rect, screen: &Screen) {
    let region = find_popup_region(screen).unwrap_or_else(|| fallback_region(screen));
    let (top, bot, left, right) = region;
    let popup_rows = bot - top + 1;
    let popup_cols = right - left + 1;

    let h = (popup_rows + 2).min(area.height);
    let w = (popup_cols + 4).min(area.width);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h + 1);
    let outer = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, outer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" claude is asking — Ctrl-] to dismiss ");
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let max_rows = inner.height.min(popup_rows);
    let max_cols = inner.width.min(popup_cols);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(max_rows as usize);
    for r in 0..max_rows {
        lines.push(row_to_line(screen, top + r, left, max_cols));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_native_picker(
    frame: &mut Frame,
    area: Rect,
    picker: &PickerContent,
    plan_content: Option<&str>,
) {
    // Border title stays short + stable so it never overflows on
    // narrow screens; the actual question is rendered prominently
    // inside the modal body.
    let border_title = " claude is asking — Ctrl-] dismiss ";

    // Modal size: cap at ~90 % of the available area so we keep a
    // visual margin from the surrounding mewxi UI, and clamp width to
    // 96 cols max so very wide screens don't get one giant overlay.
    let max_w = (((area.width.saturating_sub(4)) as u32 * 9) / 10).min(96) as u16;
    let max_h = (((area.height.saturating_sub(2)) as u32 * 9) / 10) as u16;

    let longest_line = picker
        .title
        .iter()
        .map(|s| s.chars().count())
        .chain(picker.body.iter().map(|s| s.chars().count()))
        .chain(picker.options.iter().map(|o| o.chars().count() + 4))
        .max()
        .unwrap_or(40);
    let w = ((longest_line as u16) + 6)
        .clamp(40, max_w.max(40))
        .min(area.width);

    let mut content_lines: Vec<Line<'static>> = Vec::new();
    if let Some(q) = picker.title.as_deref() {
        content_lines.push(Line::from(Span::styled(
            q.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
    }
    for body in &picker.body {
        content_lines.push(Line::from(Span::styled(
            body.clone(),
            Style::default().fg(Color::Gray),
        )));
    }
    if let Some(plan) = plan_content {
        if !content_lines.is_empty() {
            content_lines.push(Line::raw(""));
        }
        content_lines.push(Line::from(Span::styled(
            "── plan ──".to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for plan_line in plan.lines() {
            content_lines.push(Line::from(Span::styled(
                plan_line.to_string(),
                Style::default().fg(Color::Gray),
            )));
        }
    }

    let option_lines: Vec<Line<'static>> = picker
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let selected = i == picker.selected;
            let (cursor, style) = if selected {
                (
                    " ▶ ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("   ", Style::default().fg(Color::White))
            };
            Line::from(vec![
                Span::styled(cursor, style),
                Span::styled(opt.clone(), style),
            ])
        })
        .collect();

    // Estimate content height: each line wraps to ceil(len / inner_w).
    let inner_w = w.saturating_sub(2).max(1) as usize;
    let est_content_h: usize = content_lines
        .iter()
        .map(|line| {
            let len = line
                .spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
                .max(1);
            len.div_ceil(inner_w).max(1)
        })
        .sum();
    let options_h = option_lines.len() as u16;
    let want_h = est_content_h as u16 + options_h + 2; // +2 borders
    let min_h = options_h + 4;
    let h = want_h.clamp(min_h, max_h.max(min_h)).min(area.height);

    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let outer = Rect { x, y, width: w, height: h };

    frame.render_widget(Clear, outer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
        .title(Span::styled(
            border_title,
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    // Split inner: content (wrapping) on top, options fixed at bottom.
    let content_h = inner.height.saturating_sub(options_h);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(content_h), Constraint::Length(options_h)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(content_lines).wrap(Wrap { trim: false }),
        chunks[0],
    );
    frame.render_widget(Paragraph::new(option_lines), chunks[1]);
}

/// Structured view of a numbered picker pulled off the PTY screen.
/// Used to re-render the prompt as a native mewxi modal.
struct PickerContent {
    /// First non-blank header line (e.g. "Switch model?"). Goes into
    /// the modal's title bar.
    title: Option<String>,
    /// Remaining header lines (description, context paragraphs).
    body: Vec<String>,
    /// Option text with the "N. " prefix and cursor stripped.
    options: Vec<String>,
    /// 0-based index of the option claude has its cursor on.
    selected: usize,
}

/// Parse a numbered picker from `screen` if one is up. Returns None
/// for y/N prompts, accept-edit dialogs, and anything else that isn't
/// shaped like `<header> ... <cursor> 1. opt / 2. opt / ...`.
fn parse_picker(screen: &Screen) -> Option<PickerContent> {
    let (top, bot, _left, _right) = find_popup_region(screen)?;
    let (_, cols) = screen.size();
    let lines: Vec<String> = (top..=bot).map(|r| row_text(screen, r, cols)).collect();

    // Find the cursor row inside the region (there's exactly one).
    let (cursor_idx, cursor_col, cursor_prefix) = lines
        .iter()
        .enumerate()
        .find_map(|(i, l)| picker_cursor_info(l).map(|(c, p)| (i, c, p)))?;

    // Walk up and down from the cursor collecting contiguous sibling
    // rows. A non-sibling row (or end of region) terminates the option
    // list — that way header text above and hint text below stay out
    // of the options.
    let mut option_lines: Vec<(usize, bool)> = Vec::new();
    let mut i = cursor_idx;
    loop {
        if i == 0 {
            break;
        }
        let prev = i - 1;
        if is_picker_sibling_line(&lines[prev], &cursor_prefix) {
            option_lines.insert(0, (prev, false));
            i = prev;
        } else {
            break;
        }
    }
    option_lines.push((cursor_idx, true));
    for j in (cursor_idx + 1)..lines.len() {
        if is_picker_sibling_line(&lines[j], &cursor_prefix) {
            option_lines.push((j, false));
        } else {
            break;
        }
    }

    if option_lines.len() < 2 {
        return None;
    }

    let first_opt_idx = option_lines[0].0;
    let header: Vec<String> = lines[..first_opt_idx]
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let (title, body) = match header.split_first() {
        Some((t, rest)) => (Some(t.clone()), rest.to_vec()),
        None => (None, Vec::new()),
    };

    let mut options = Vec::with_capacity(option_lines.len());
    let mut selected = 0;
    for (slot, (line_idx, is_selected)) in option_lines.iter().enumerate() {
        options.push(extract_option_text(&lines[*line_idx], cursor_col, *is_selected));
        if *is_selected {
            selected = slot;
        }
    }

    Some(PickerContent {
        title,
        body,
        options,
        selected,
    })
}

/// Strip the cursor + indent prefix, the leading `N. ` numbering if
/// present, and trailing box border / whitespace from a picker row,
/// leaving just the option label.
fn extract_option_text(line: &str, cursor_col: usize, _is_selected: bool) -> String {
    let chars: Vec<char> = line.chars().collect();
    // Both selected and sibling rows consume the same `cursor_col + 2`
    // leading columns: selected = `<prefix><cursor><space>`, sibling
    // = `<prefix><indent of cursor + 2>`.
    let strip = cursor_col + 2;
    let mut text: String = chars.iter().skip(strip).collect();
    // Drop trailing box border + whitespace (popup might wrap in `│ … │`).
    while text
        .chars()
        .next_back()
        .is_some_and(|c| c.is_whitespace() || is_box_char(c))
    {
        text.pop();
    }
    // Strip leading `<digits><.|)> ` so numbered options render
    // cleanly under mewxi's own cursor indicator (otherwise we'd show
    // `▶ 1. Yes, switch …`).
    text = strip_leading_number(&text);
    text
}

fn strip_leading_number(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut idx = 0;
    while chars.get(idx).is_some_and(|c| c.is_ascii_digit()) {
        idx += 1;
    }
    if idx == 0 || !matches!(chars.get(idx), Some('.') | Some(')')) {
        return s.to_string();
    }
    idx += 1;
    if chars.get(idx) == Some(&' ') {
        idx += 1;
    }
    chars[idx..].iter().collect()
}

/// Locate the popup's bounding box on the screen. Returns
/// `(row_top, row_bot, col_left, col_right)` inclusive. None when no
/// prompt marker is present (caller falls back to a generic region).
///
/// Anchors the upward walk on the **picker cursor row** when one is
/// found above the marker — Claude's AskUserQuestion picker leaves
/// ≥2 blank rows between the options block and the footer hint, so a
/// marker-anchored walk hits its blank-row stop before reaching the
/// cursor / options / header. Non-picker prompts (y/N, accept-edit,
/// trust dialog) have no cursor row and fall back to anchoring on the
/// marker. Expansion still stops at a horizontal separator, a box
/// corner, two consecutive blanks, or the height cap.
fn find_popup_region(screen: &Screen) -> Option<(u16, u16, u16, u16)> {
    let (rows, cols) = screen.size();
    let marker_row = find_marker_row(screen)?;
    let anchor_top = scan_cursor_row_above(screen, marker_row, cols).unwrap_or(marker_row);

    let mut top = anchor_top;
    let mut blanks_up = 0u16;
    while top > 0 && anchor_top.saturating_sub(top) < MAX_POPUP_ROWS {
        let prev = top - 1;
        let txt = row_text(screen, prev, cols);
        if is_horizontal_separator(&txt) {
            break;
        }
        if txt.trim().is_empty() {
            blanks_up += 1;
            if blanks_up >= 2 {
                break;
            }
        } else {
            blanks_up = 0;
        }
        top = prev;
        if txt.chars().any(is_box_top_corner) {
            break;
        }
    }
    // Trim leading blank rows.
    while top < anchor_top && row_text(screen, top, cols).trim().is_empty() {
        top += 1;
    }

    let mut bot = marker_row;
    let mut blanks_dn = 0u16;
    while bot + 1 < rows && bot.saturating_sub(marker_row) < MAX_POPUP_ROWS {
        let next = bot + 1;
        let txt = row_text(screen, next, cols);
        if is_horizontal_separator(&txt) {
            break;
        }
        // Stop *before* the next UI element's top border (input box).
        if txt.chars().any(is_box_top_corner) && !txt.chars().any(is_box_bottom_corner) {
            break;
        }
        if txt.trim().is_empty() {
            blanks_dn += 1;
            if blanks_dn >= 2 {
                break;
            }
        } else {
            blanks_dn = 0;
        }
        bot = next;
        if txt.chars().any(is_box_bottom_corner) {
            break;
        }
    }
    while bot > marker_row && row_text(screen, bot, cols).trim().is_empty() {
        bot -= 1;
    }

    // Hard cap height: anchor on the cursor (the action point of a
    // picker) when present, else the marker. Centering on `anchor_top`
    // keeps the most relevant rows visible in pathological cases where
    // the cursor and marker are far apart.
    if bot - top + 1 > MAX_POPUP_ROWS {
        let half = MAX_POPUP_ROWS.saturating_sub(4);
        top = anchor_top.saturating_sub(half);
        bot = (top + MAX_POPUP_ROWS - 1).min(rows - 1);
    }

    let (left, right) = column_bounds(screen, top, bot, cols)?;
    Some((top, bot, left, right))
}

/// Scan upward from `marker_row` (capped at `MAX_POPUP_ROWS`) for the
/// closest picker cursor row — i.e. a row [`picker_cursor_info`] hits
/// **and** that has at least one matching sibling immediately above or
/// below. Used by [`find_popup_region`] so the upward expansion can
/// anchor on the picker's structural action row, not just the footer
/// hint that triggered the overlay.
///
/// The sibling requirement is the false-positive guard: chat
/// scrollback bullets (`●`, `›`) match the cursor shape on their own,
/// so a lone bullet far above the marker would otherwise hijack the
/// anchor and drag the region into chat scrollback. Real pickers
/// always have ≥2 option rows, so we always have a sibling next to the
/// cursor.
fn scan_cursor_row_above(screen: &Screen, marker_row: u16, cols: u16) -> Option<u16> {
    let lo = marker_row.saturating_sub(MAX_POPUP_ROWS);
    for r in (lo..marker_row).rev() {
        let txt = row_text(screen, r, cols);
        let Some((_, prefix)) = picker_cursor_info(&txt) else { continue };
        let has_sib_above = r > 0
            && is_picker_sibling_line(&row_text(screen, r - 1, cols), &prefix);
        let has_sib_below = r + 1 < marker_row
            && is_picker_sibling_line(&row_text(screen, r + 1, cols), &prefix);
        if has_sib_above || has_sib_below {
            return Some(r);
        }
    }
    None
}

/// Fallback: bottom-most contiguous block of non-blank rows. Used when
/// the awaiting marker fires but no in-screen prompt text is visible
/// (e.g. claude's still painting), so something rendered is better than
/// a blank box.
fn fallback_region(screen: &Screen) -> (u16, u16, u16, u16) {
    let (rows, cols) = screen.size();
    let mut bot = rows.saturating_sub(1);
    while bot > 0 && row_text(screen, bot, cols).trim().is_empty() {
        bot -= 1;
    }
    let mut top = bot;
    let mut count: u16 = 1;
    while top > 0 && count < MAX_POPUP_ROWS {
        let prev = top - 1;
        if row_text(screen, prev, cols).trim().is_empty() {
            break;
        }
        top = prev;
        count += 1;
    }
    let (left, right) = column_bounds(screen, top, bot, cols).unwrap_or((0, cols - 1));
    (top, bot, left, right)
}

fn column_bounds(screen: &Screen, top: u16, bot: u16, cols: u16) -> Option<(u16, u16)> {
    let mut left = cols;
    let mut right = 0u16;
    let mut any = false;
    for r in top..=bot {
        for c in 0..cols {
            let has = screen
                .cell(r, c)
                .map(|x| x.has_contents() && !x.contents().chars().all(char::is_whitespace))
                .unwrap_or(false);
            if has {
                if c < left {
                    left = c;
                }
                if c > right {
                    right = c;
                }
                any = true;
            }
        }
    }
    if !any {
        None
    } else {
        Some((left, right))
    }
}

fn row_text(screen: &Screen, row: u16, cols: u16) -> String {
    let mut s = String::with_capacity(cols as usize);
    for c in 0..cols {
        match screen.cell(row, c) {
            Some(cell) if cell.has_contents() => s.push_str(cell.contents()),
            _ => s.push(' '),
        }
    }
    s
}

fn is_box_top_corner(c: char) -> bool {
    matches!(c, '╭' | '┌' | '╔' | '┏' | '┎' | '┍')
}

fn is_box_bottom_corner(c: char) -> bool {
    matches!(c, '╰' | '└' | '╚' | '┗' | '┖' | '┕')
}

fn row_to_line(screen: &Screen, row: u16, col_start: u16, width: u16) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur_style = Style::default();
    let mut started = false;
    let end = col_start.saturating_add(width);
    let mut col = col_start;
    while col < end {
        let cell = screen.cell(row, col);
        let (text, style) = match cell {
            Some(c) if c.has_contents() => (c.contents().to_string(), cell_style(c)),
            _ => (" ".to_string(), Style::default()),
        };
        if !started {
            cur_style = style;
            started = true;
        }
        if style == cur_style {
            buf.push_str(&text);
        } else {
            spans.push(Span::styled(std::mem::take(&mut buf), cur_style));
            buf.push_str(&text);
            cur_style = style;
        }
        let advance = cell.map(|c| if c.is_wide() { 2 } else { 1 }).unwrap_or(1);
        col = col.saturating_add(advance);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur_style));
    }
    Line::from(spans)
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut s = Style::default()
        .fg(map_color(cell.fgcolor()))
        .bg(map_color(cell.bgcolor()));
    let mut m = Modifier::empty();
    if cell.bold() {
        m |= Modifier::BOLD;
    }
    if cell.dim() {
        m |= Modifier::DIM;
    }
    if cell.italic() {
        m |= Modifier::ITALIC;
    }
    if cell.underline() {
        m |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        m |= Modifier::REVERSED;
    }
    if !m.is_empty() {
        s = s.add_modifier(m);
    }
    s
}

fn map_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_control::{PTY_COLS, PTY_ROWS};

    fn parse(bytes: &[u8]) -> vt100::Parser {
        let mut p = vt100::Parser::new(PTY_ROWS, PTY_COLS, 0);
        p.process(bytes);
        p
    }

    #[test]
    fn detects_y_slash_n_prompt() {
        let p = parse(b"Edit foo.rs? [y/N]");
        assert!(prompt_visible(p.screen(), false));
    }

    #[test]
    fn ignores_chevron_in_input_box() {
        // claude's input prompt is `❯ user_text`. The overlay must NOT
        // open on this — it would trigger on every keystroke.
        let p = parse("\x1b[2J❯ hello world".as_bytes());
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn detects_numbered_picker_with_chevron_cursor() {
        // The model-switch picker: a question + numbered options with
        // a chevron on the selected row + the hint footer that real
        // claude renders below every picker.
        let p = parse("\x1b[2JSwitch model?\r\n\r\n❯ 1. Yes, switch\r\n  2. No, go back\r\n\r\n↑/↓ navigate · Esc to cancel · Enter to select".as_bytes());
        assert!(prompt_visible(p.screen(), false));
    }

    #[test]
    fn detects_numbered_picker_with_alternate_cursor() {
        // Some terminal fonts render the cursor as `›` instead of `❯`.
        let p = parse("\x1b[2J› 1. Yes\r\n  2. No\r\n\r\nEsc to cancel".as_bytes());
        assert!(prompt_visible(p.screen(), false));
    }

    #[test]
    fn ignores_lone_numbered_line_without_sibling() {
        // A stray "1. foo" in chat output should NOT trigger — pickers
        // always have at least 2 sibling option rows.
        let p = parse("\x1b[2JHere's an item: 1. apple\r\n\r\nnext paragraph".as_bytes());
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn picker_parser_extracts_model_switch_shape() {
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JSwitch model?\r\n");
        bytes.push_str("Your next response will be slower\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("This conversation is cached.\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("❯ 1. Yes, switch to Haiku 4.5\r\n");
        bytes.push_str("  2. No, go back\r\n");
        bytes.push_str("\r\nEsc to cancel\r\n");
        let p = parse(bytes.as_bytes());
        let picker = parse_picker(p.screen()).expect("picker parsed");
        assert_eq!(picker.title.as_deref(), Some("Switch model?"));
        assert_eq!(picker.options.len(), 2);
        assert_eq!(picker.options[0], "Yes, switch to Haiku 4.5");
        assert_eq!(picker.options[1], "No, go back");
        assert_eq!(picker.selected, 0);
        // Body should at minimum mention the description content.
        assert!(picker.body.iter().any(|b| b.contains("slower")));
    }

    #[test]
    fn picker_parser_tracks_selection_on_second_option() {
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JPick one?\r\n");
        bytes.push_str("  1. Apple\r\n");
        bytes.push_str("› 2. Banana\r\n");
        bytes.push_str("\r\nEsc to cancel\r\n");
        let p = parse(bytes.as_bytes());
        let picker = parse_picker(p.screen()).expect("picker parsed");
        assert_eq!(picker.selected, 1);
        assert_eq!(picker.options, vec!["Apple", "Banana"]);
    }

    #[test]
    fn picker_parser_returns_none_for_y_n_prompt() {
        // y/N prompts have specific text markers but no numbered
        // options — they should fall through to the cropped vt100
        // render, not the native picker.
        let p = parse(b"\x1b[2JEdit foo.rs? [y/N]");
        assert!(parse_picker(p.screen()).is_none());
    }

    #[test]
    fn detects_unnumbered_picker_with_chevron_cursor() {
        // AskUserQuestion-style picker: a cursor row + indented
        // siblings, no numeric prefixes.
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JWhat scope?\r\n");
        bytes.push_str("❯ Quick fix\r\n");
        bytes.push_str("  Full refactor\r\n");
        bytes.push_str("  Skip for now\r\n");
        bytes.push_str("\r\nEsc to cancel\r\n");
        let p = parse(bytes.as_bytes());
        assert!(prompt_visible(p.screen(), false));
        let picker = parse_picker(p.screen()).expect("picker parsed");
        assert_eq!(picker.options, vec!["Quick fix", "Full refactor", "Skip for now"]);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.title.as_deref(), Some("What scope?"));
    }

    #[test]
    fn picker_with_only_last_option_numbered() {
        // The exact AskUserQuestion shape the user reported broken:
        // 4 unnumbered options + a numbered "Chat about this" footer.
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JTask scope?\r\n");
        bytes.push_str("  Create a simple script\r\n");
        bytes.push_str("  Make a more complete tool\r\n");
        bytes.push_str("  Just answer the question\r\n");
        bytes.push_str("❯ Refactor existing code\r\n");
        bytes.push_str("  5. Chat about this\r\n");
        bytes.push_str("\r\nEsc to cancel\r\n");
        let p = parse(bytes.as_bytes());
        let picker = parse_picker(p.screen()).expect("picker parsed");
        assert_eq!(picker.options.len(), 5, "got {:?}", picker.options);
        assert_eq!(picker.options[0], "Create a simple script");
        assert_eq!(picker.options[3], "Refactor existing code");
        assert_eq!(picker.options[4], "Chat about this"); // leading "5. " stripped
        assert_eq!(picker.selected, 3);
    }

    #[test]
    fn chat_scrollback_with_bullet_markers_does_not_trigger() {
        // Image-13 regression: claude's normal chat scrollback uses
        // bullet glyphs (`›`, `●`, `*`) that all match the structural
        // cursor shape, and continuation lines (`  Set model to …`)
        // match the sibling shape. Pre-fix, this fired the overlay
        // over normal chat after every `/model` command.
        let mut bytes = String::new();
        bytes.push_str("\x1b[2J› /model haiku\r\n");
        bytes.push_str("  Set model to Haiku 4.5 for this session\r\n");
        bytes.push_str("› say hi\r\n");
        bytes.push_str("● Hi!\r\n");
        bytes.push_str("* Cogitated for 2s\r\n");
        let p = parse(bytes.as_bytes());
        assert!(!prompt_visible(p.screen(), false));
        assert!(parse_picker(p.screen()).is_none());
    }

    #[test]
    fn slash_command_preview_does_not_trigger() {
        // claude renders typed slash commands as `▶ /model haiku` +
        // a one-line description sibling. The overlay must NOT open
        // on this — structurally it matches a 2-option picker.
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JWelcome back!\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("▶ /model haiku\r\n");
        bytes.push_str("  Set model to Haiku 4.5 for this session\r\n");
        let p = parse(bytes.as_bytes());
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn input_box_chevron_with_no_siblings_does_not_trigger() {
        // Regression guard: a single `❯ user_text` line with nothing
        // sibling-shaped above or below must NOT open the overlay.
        let p = parse("\x1b[2J\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n\r\n❯ hello world".as_bytes());
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn picker_parser_handles_multi_digit_numbers() {
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JChoose?\r\n");
        for n in 1..=12 {
            let cursor = if n == 11 { "❯" } else { " " };
            bytes.push_str(&format!("{} {}. Option {}\r\n", cursor, n, n));
        }
        bytes.push_str("\r\nEsc to cancel\r\n");
        let p = parse(bytes.as_bytes());
        let picker = parse_picker(p.screen()).expect("picker parsed");
        assert_eq!(picker.options.len(), 12);
        assert_eq!(picker.selected, 10); // 0-indexed
        assert_eq!(picker.options[10], "Option 11");
    }

    #[test]
    fn picker_region_includes_header_across_blank_row() {
        // Header + blank + chevron+digit options — region should span
        // header to the last option.
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JSwitch model?\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("❯ 1. Yes\r\n");
        bytes.push_str("  2. No\r\n");
        bytes.push_str("\r\nEsc to cancel\r\n");
        let p = parse(bytes.as_bytes());
        let (top, bot, _, _) = find_popup_region(p.screen()).expect("picker found");
        let txt: Vec<String> = (top..=bot).map(|r| row_text(p.screen(), r, p.screen().size().1)).collect();
        assert!(txt.iter().any(|l| l.contains("Switch model?")), "expected header in region: {:?}", txt);
        assert!(txt.iter().any(|l| l.contains("❯ 1. Yes")), "expected option in region: {:?}", txt);
        assert!(txt.iter().any(|l| l.contains("2. No")), "expected sibling option in region: {:?}", txt);
    }

    #[test]
    fn ignores_plain_chat_screen() {
        let p = parse(b"hello world\r\n> ");
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn awaiting_marker_alone_is_not_enough() {
        // Stale awaiting markers (e.g. after Esc-interrupting a
        // tool-use prompt) must NOT keep the overlay open when the
        // screen has no prompt content — otherwise we paint an empty
        // input box overlay that blocks every keystroke until Ctrl-].
        let p = parse(b"");
        assert!(!prompt_visible(p.screen(), true));
    }

    #[test]
    fn empty_input_box_after_interrupt_does_not_trigger() {
        // After `[Request interrupted by user for tool use]`, claude
        // collapses back to just its input box. The overlay must
        // close.
        let p = parse("\x1b[2J❯ ".as_bytes());
        assert!(!prompt_visible(p.screen(), true));
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn detects_continue_prompt_inside_box() {
        // Real claude prompts always render inside a ratatui box; the
        // prose-prone marker only triggers when a box border is within
        // a few rows.
        let mut bytes = String::new();
        bytes.push_str("╭──────────────╮\r\n");
        bytes.push_str("│ Continue?    │\r\n");
        bytes.push_str("╰──────────────╯\r\n");
        let p = parse(bytes.as_bytes());
        assert!(prompt_visible(p.screen(), false));
    }

    #[test]
    fn continue_prompt_in_chat_prose_does_not_trigger() {
        // Regression: a chat message containing "Continue?" (no box
        // border anywhere near) must NOT pop the overlay — otherwise
        // every keystroke goes to the PTY and the user has to find
        // Ctrl-] to recover.
        let p = parse(b"chat line\r\nWould you like me to Continue? Probably yes.\r\nchat line\r\n");
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn use_this_in_chat_prose_does_not_trigger() {
        // Same gating for the accept-edit marker.
        let p = parse(b"chat\r\nUse this approach instead of the other one.\r\nchat\r\n");
        assert!(!prompt_visible(p.screen(), false));
    }

    #[test]
    fn trust_dialog_detected_when_in_box() {
        // claude's startup trust dialog renders inside a box.
        let mut bytes = String::new();
        bytes.push_str("╭───────────────────────────────────╮\r\n");
        bytes.push_str("│ Do you trust the files in this    │\r\n");
        bytes.push_str("│ folder?                           │\r\n");
        bytes.push_str("│ 1. Yes, proceed                   │\r\n");
        bytes.push_str("│ 2. No, exit                       │\r\n");
        bytes.push_str("╰───────────────────────────────────╯\r\n");
        let p = parse(bytes.as_bytes());
        assert!(prompt_visible(p.screen(), false));
    }

    #[test]
    fn popup_region_crops_tightly_to_box() {
        // Simulated screen: lots of chat above, then a small popup box,
        // then a blank input line. We want the region to bound the box.
        let mut bytes = String::new();
        for i in 0..15 {
            bytes.push_str(&format!("chat line {i}\r\n"));
        }
        bytes.push_str("╭──────────────╮\r\n");
        bytes.push_str("│ Continue?    │\r\n");
        bytes.push_str("│ [y/N]        │\r\n");
        bytes.push_str("╰──────────────╯\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("> input box\r\n");
        let p = parse(bytes.as_bytes());
        let (top, bot, left, right) = find_popup_region(p.screen()).expect("popup found");
        // Should cover 4 rows (╭, Continue?, [y/N], ╰).
        assert_eq!(bot - top + 1, 4, "rows {top}..={bot}");
        // Width = 16 cols of the box.
        assert_eq!(right - left + 1, 16, "cols {left}..={right}");
    }

    #[test]
    fn popup_region_handles_no_box_around_marker() {
        // Marker without a surrounding box — region should still pick
        // up the marker row and tightly clip horizontally. Uses a
        // specific marker (`[y/N]`) since prose-prone markers like
        // `Continue?` now require a box border to trigger.
        let p = parse(b"[y/N]  ");
        let (top, bot, left, right) = find_popup_region(p.screen()).expect("popup found");
        assert_eq!(top, bot);
        // "[y/N]" is 5 chars wide.
        assert_eq!(right - left + 1, 5);
    }

    #[test]
    fn popup_region_spans_blanks_between_options_and_footer() {
        // Regression: AskUserQuestion picker leaves blank rows between
        // its option block and the footer hint. Pre-fix, the upward
        // walk halted at those blanks and only captured the footer,
        // so parse_picker fell through and render_pty_crop emitted a
        // 2-row sliver. Anchoring on the cursor row pulls the header
        // and all options back into the region.
        let mut bytes = String::new();
        bytes.push_str("\x1b[2JWhat scope?\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("  Create a simple script\r\n");
        bytes.push_str("  Make a more complete tool\r\n");
        bytes.push_str("❯ Refactor existing code\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("Enter to select · ↑/↓ to navigate · n to add notes · Esc to cancel\r\n");
        let p = parse(bytes.as_bytes());
        let (top, bot, _, _) = find_popup_region(p.screen()).expect("popup found");
        let txt: Vec<String> = (top..=bot)
            .map(|r| row_text(p.screen(), r, p.screen().size().1))
            .collect();
        assert!(
            txt.iter().any(|l| l.contains("What scope?")),
            "expected header in region: {txt:?}"
        );
        assert!(
            txt.iter().any(|l| l.contains("Create a simple script")),
            "expected first option in region: {txt:?}"
        );
        assert!(
            txt.iter().any(|l| l.contains("Refactor existing code")),
            "expected cursor option in region: {txt:?}"
        );
        // parse_picker now succeeds (was None pre-fix because the
        // cursor row wasn't in the region).
        let picker = parse_picker(p.screen()).expect("picker parsed");
        assert_eq!(picker.options.len(), 3);
        assert_eq!(picker.selected, 2);
    }

    #[test]
    fn cursor_anchor_ignores_lone_bullet_in_scrollback() {
        // A stray `●` in chat scrollback with no sibling must not be
        // chosen as the picker anchor — otherwise the region would be
        // dragged into chat scrollback far above any real popup.
        let mut bytes = String::new();
        bytes.push_str("\x1b[2J● Some chat line about something\r\n");
        bytes.push_str("More chat content here\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("\r\n");
        bytes.push_str("[y/N]\r\n");
        let p = parse(bytes.as_bytes());
        let (_, cols) = p.screen().size();
        let marker_row = find_marker_row(p.screen()).expect("marker found");
        assert!(scan_cursor_row_above(p.screen(), marker_row, cols).is_none());
    }

    #[test]
    fn popup_region_caps_height_at_max() {
        // Build a screen where everything is non-blank box content;
        // ensure we cap at MAX_POPUP_ROWS.
        let mut bytes = String::new();
        bytes.push_str("╭──────────╮\r\n");
        for _ in 0..40 {
            bytes.push_str("│ x        │\r\n");
        }
        bytes.push_str("│ [y/N]    │\r\n");
        let p = parse(bytes.as_bytes());
        let (top, bot, _, _) = find_popup_region(p.screen()).expect("popup found");
        assert!(bot - top + 1 <= MAX_POPUP_ROWS, "rows {top}..={bot}");
    }
}
