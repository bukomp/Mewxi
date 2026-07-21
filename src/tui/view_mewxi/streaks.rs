//! Arcade-style gamification HUD ("streaks") for view 5's rave view.
//!
//! - COMBO — the current count of active top-level agent sessions.
//! - STREAK — how long (seconds) at least one agent has been continuously
//!   active. A short grace window absorbs brief all-idle gaps so a
//!   flicker between sessions doesn't reset the counter.
//! - BEST — the longest STREAK reached this run.
//! - SCORE — accumulated active-agent-seconds, scaled up to read as an
//!   arcade score. Monotonic non-decreasing.
//! - MILESTONE — fires the frame a STREAK tier is crossed, or COMBO hits
//!   a new all-time high, so the root can flash/shake the panel.
//!
//! All the transition math lives in pure free functions (`advance`,
//! `streak_tier`) so it's testable without a `Frame` or wall-clock time.
//! The `OnceLock<Mutex<..>>` + `Instant`-tick wrapper mirrors the
//! `AnimState`/`tick_anim` pattern in `super::super` (`view_mewxi.rs`).

use super::font;
use super::palette::{P_DIM, P_HOT, P_LABEL, P_NEON, P_TEXT, heat_color, purple_gauge};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Seconds of continuous zero-active idle tolerated before STREAK resets.
/// Bridges brief gaps between one agent finishing and the next starting.
const GRACE_SECS: f64 = 5.0;

/// Score scale factor — active-agent-seconds × this, truncated to an
/// integer, so the number climbs at an arcade-y pace instead of reading
/// like raw seconds.
const SCORE_SCALE: f64 = 100.0;

/// STREAK tier thresholds in seconds. `streak_tier` returns the index of
/// the highest threshold met (0 = below the first one).
const STREAK_TIERS: &[f64] = &[60.0, 300.0, 900.0, 1800.0];

/// How long a milestone flash takes to decay back to 0, in seconds.
const FLASH_DECAY_SECS: f64 = 1.2;

/// Public per-frame snapshot handed to the renderer. Cheap to copy.
#[derive(Clone, Copy, Debug)]
pub struct StreakHud {
    /// Current active agent count.
    pub combo: usize,
    /// Continuous ≥1-active time, in seconds (grace-extended).
    pub streak_secs: f64,
    /// Longest `streak_secs` reached this run.
    pub best_secs: f64,
    /// Accumulated active-agent-seconds, scaled for arcade flavour.
    pub score: u64,
    /// True on the exact frame a tier-up or combo-high fired.
    pub milestone: bool,
    /// 0..1 flash intensity; peaks on a milestone frame, decays after.
    pub flash: f64,
}

/// Core mutable state advanced once per frame. Kept separate from
/// `StreakHud` so the pure logic never has to know about `Instant` —
/// `advance` takes an already-computed `dt`.
struct StreakCore {
    streak_secs: f64,
    best_secs: f64,
    /// Raw active-agent-seconds; `score = (score_acc * SCORE_SCALE) as u64`.
    score_acc: f64,
    /// Seconds of zero-active grace remaining; resets to `GRACE_SECS`
    /// whenever `active > 0`, counts down to 0 while `active == 0`.
    grace_left: f64,
    best_combo: usize,
    /// Highest `streak_tier` value already fired this run, so climbing
    /// through several thresholds in one frame only counts once (and
    /// dropping back down doesn't refire the same tier).
    last_milestone_tier: usize,
    flash: f64,
}

impl StreakCore {
    fn new() -> Self {
        StreakCore {
            streak_secs: 0.0,
            best_secs: 0.0,
            score_acc: 0.0,
            grace_left: 0.0,
            best_combo: 0,
            last_milestone_tier: 0,
            flash: 0.0,
        }
    }
}

/// Wall-clock wrapper state — just the last-tick `Instant` plus the core.
/// Split out so `advance` (the part under test) never touches `Instant`.
struct StreakState {
    core: StreakCore,
    last_tick: Instant,
}

static STATE: OnceLock<Mutex<StreakState>> = OnceLock::new();

/// Highest tier index whose threshold `secs` has reached, `0` if below
/// the first threshold. `59 → 0`, `60 → 1`, `300 → 2`, `900 → 3`,
/// `1800 → 4`.
fn streak_tier(secs: f64) -> usize {
    let mut tier = 0;
    for &threshold in STREAK_TIERS {
        if secs >= threshold {
            tier += 1;
        } else {
            break;
        }
    }
    tier
}

/// Advance `core` by `dt` seconds given this frame's active-agent count.
/// Pure and deterministic — no wall-clock reads — so it's fully unit
/// testable. Returns `true` iff a milestone (streak tier-up or new combo
/// high) fired this step.
///
/// `dt` is expected to already be clamped by the caller (the `tick`
/// wrapper clamps to 0.1s, matching the crate's `tick_anim` pattern).
fn advance(core: &mut StreakCore, active: usize, dt: f64) -> bool {
    let dt = dt.max(0.0);

    if active > 0 {
        core.grace_left = GRACE_SECS;
        core.streak_secs += dt;
        core.score_acc += active as f64 * dt;
    } else if core.grace_left > 0.0 {
        // Still inside the post-activity grace window: streak keeps
        // counting (agents could resume any moment) but doesn't accrue
        // score, since nothing is actually active.
        core.grace_left = (core.grace_left - dt).max(0.0);
        core.streak_secs += dt;
    } else {
        core.streak_secs = 0.0;
    }

    if core.streak_secs > core.best_secs {
        core.best_secs = core.streak_secs;
    }

    let new_tier = streak_tier(core.streak_secs);
    let tier_up = new_tier > core.last_milestone_tier;
    if tier_up {
        core.last_milestone_tier = new_tier;
    }
    // A streak reset (idle past grace) drops the tier watermark back to
    // whatever the now-zero streak implies, so climbing again from
    // scratch can refire the same milestones.
    if core.streak_secs <= 0.0 {
        core.last_milestone_tier = 0;
    }

    let combo_high = active > 0 && active > core.best_combo;
    if combo_high {
        core.best_combo = active;
    }

    let milestone = tier_up || combo_high;
    if milestone {
        core.flash = 1.0;
    } else if core.flash > 0.0 {
        let decay = if FLASH_DECAY_SECS > 0.0 { dt / FLASH_DECAY_SECS } else { 1.0 };
        core.flash = (core.flash - decay).max(0.0);
    }

    milestone
}

/// Advance the global streak state for this frame's active-agent count
/// and return the HUD snapshot. `dt` is derived from an internal
/// `Instant`, clamped to 0.1s so a backgrounded terminal can't cause a
/// giant score/streak jump on the next visible frame — mirrors
/// `tick_anim` in `view_mewxi.rs`.
pub fn tick(active: usize) -> StreakHud {
    let cell = STATE.get_or_init(|| {
        Mutex::new(StreakState {
            core: StreakCore::new(),
            last_tick: Instant::now(),
        })
    });
    let mut s = cell.lock().expect("streak state poisoned");
    let now = Instant::now();
    let dt = (now - s.last_tick).as_secs_f64().min(0.1);
    s.last_tick = now;

    let milestone = advance(&mut s.core, active, dt);

    StreakHud {
        combo: active,
        streak_secs: s.core.streak_secs,
        best_secs: s.core.best_secs,
        score: (s.core.score_acc * SCORE_SCALE) as u64,
        milestone,
        flash: s.core.flash,
    }
}

/// Format a seconds count as `m:ss`.
fn fmt_mmss(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Compact arcade score: `9 999` as-is, `817 000` → `817K`,
/// `1 234 567` → `1.2M`. Bounds the pixel-font rendering to a handful
/// of glyphs no matter how long the session runs.
fn fmt_score(score: u64) -> String {
    if score < 10_000 {
        score.to_string()
    } else if score < 1_000_000 {
        format!("{}K", score / 1_000)
    } else {
        format!("{:.1}M", score as f64 / 1_000_000.0)
    }
}

/// Rows one big-HUD pixel band occupies: a normal-text label row plus
/// the pixel-font value rows.
pub const BIG_HUD_HEIGHT: u16 = font::HEADLINE_HEIGHT + 1;

/// Columns between HUD segments in the big rendering.
const BIG_SEG_GAP: usize = 3;

/// The four HUD stats as `(label, value)` pairs, in display order —
/// shared by the big renderer and [`big_hud_width`] so the width gate
/// can never disagree with what actually gets drawn.
fn segment_values(hud: &StreakHud) -> [(&'static str, String); 4] {
    [
        ("COMBO", format!("X{}", hud.combo)),
        ("STREAK", fmt_mmss(hud.streak_secs)),
        ("BEST", fmt_mmss(hud.best_secs)),
        ("SCORE", fmt_score(hud.score)),
    ]
}

/// Width in columns the big pixel-font HUD needs for `hud`'s current
/// values (1 leading space + per-segment max(label, pixel value) +
/// gaps). The root uses this to decide whether the tall band is worth
/// allocating; [`render_hud`] uses it again to pick big vs one-line.
pub fn big_hud_width(hud: &StreakHud) -> u16 {
    let segs = segment_values(hud);
    let mut w = 1; // leading space, mirrors the one-liner's indent
    for (i, (label, value)) in segs.iter().enumerate() {
        if i > 0 {
            w += BIG_SEG_GAP;
        }
        let pixel_w = font::big_word(value)[0].chars().count();
        w += label.chars().count().max(pixel_w);
    }
    w.min(u16::MAX as usize) as u16
}

/// Render the arcade HUD into `area`: big pixel-font stat bands when the
/// area is tall and wide enough ([`BIG_HUD_HEIGHT`] rows, see
/// [`big_hud_width`]), otherwise the compact one-liner. Guards against
/// tiny areas — never panics on `area.height == 0` or a very narrow
/// width.
pub fn render_hud(f: &mut Frame, area: Rect, hud: &StreakHud) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    if area.height >= BIG_HUD_HEIGHT && area.width >= big_hud_width(hud) {
        render_hud_big(f, area, hud);
    } else {
        render_hud_line(f, area, hud);
    }
}

/// The big rendering: one label row (normal text) over
/// [`font::HEADLINE_HEIGHT`] pixel-font rows, each stat a column
/// segment. Colors keep the one-liner's semantics — combo on the
/// utilization gauge scale, streak/score in body text, best dimmed —
/// and a milestone flashes the combo neon on the flash background.
fn render_hud_big(f: &mut Frame, area: Rect, hud: &StreakHud) {
    let hot = hud.flash > 0.0 || hud.milestone;
    let combo_color = purple_gauge((hud.combo.min(10) as f64 / 10.0) * 100.0);
    let value_styles = [
        if hot {
            Style::default().fg(P_NEON).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(combo_color)
        },
        Style::default().fg(P_TEXT),
        Style::default().fg(P_DIM),
        Style::default().fg(P_TEXT),
    ];

    let segs = segment_values(hud);
    let gap = " ".repeat(BIG_SEG_GAP);

    let mut label_spans: Vec<Span> = vec![Span::raw(" ")];
    let mut pixel_spans: Vec<Vec<Span>> =
        (0..font::HEADLINE_HEIGHT).map(|_| vec![Span::raw(" ")]).collect();

    for (i, (label, value)) in segs.iter().enumerate() {
        if i > 0 {
            label_spans.push(Span::raw(gap.clone()));
            for row in pixel_spans.iter_mut() {
                row.push(Span::raw(gap.clone()));
            }
        }
        let rows = font::big_word(value);
        let pixel_w = rows[0].chars().count();
        let seg_w = label.chars().count().max(pixel_w);

        let label_pad = seg_w - label.chars().count();
        label_spans.push(Span::styled(
            format!("{label}{}", " ".repeat(label_pad)),
            Style::default().fg(P_LABEL),
        ));

        let value_pad = seg_w - pixel_w;
        for (r, row) in rows.into_iter().enumerate() {
            pixel_spans[r].push(Span::styled(
                format!("{row}{}", " ".repeat(value_pad)),
                value_styles[i],
            ));
        }
    }

    if hot {
        label_spans.push(Span::raw(gap));
        label_spans.push(Span::styled(
            "★ MILESTONE ★",
            Style::default()
                .fg(P_NEON)
                .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
        ));
    }

    let mut lines = vec![Line::from(label_spans)];
    lines.extend(pixel_spans.into_iter().map(Line::from));

    let style = if hot {
        Style::default().bg(Color::Indexed(89))
    } else {
        Style::default()
    };
    f.render_widget(Paragraph::new(lines).style(style), area);
}

/// The compact single-line rendering, used when the area can't fit the
/// pixel-font bands.
fn render_hud_line(f: &mut Frame, area: Rect, hud: &StreakHud) {
    let hot = hud.flash > 0.0 || hud.milestone;
    let accent = if hot { heat_color(hud.flash.max(0.6)) } else { P_HOT };
    let combo_style = if hot {
        Style::default().fg(P_NEON).add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    };

    let combo_color = purple_gauge((hud.combo.min(10) as f64 / 10.0) * 100.0);

    let mut spans = vec![
        Span::styled(" COMBO ", Style::default().fg(P_LABEL)),
        Span::styled(format!("x{}", hud.combo), combo_style.fg(combo_color)),
        Span::raw("  "),
        Span::styled("STREAK ", Style::default().fg(P_LABEL)),
        Span::styled(
            fmt_mmss(hud.streak_secs),
            Style::default().fg(P_TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("BEST ", Style::default().fg(P_LABEL)),
        Span::styled(fmt_mmss(hud.best_secs), Style::default().fg(P_DIM)),
        Span::raw("  "),
        Span::styled("SCORE ", Style::default().fg(P_LABEL)),
        Span::styled(
            fmt_score(hud.score),
            Style::default().fg(P_TEXT).add_modifier(Modifier::BOLD),
        ),
    ];

    if hot {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "★ MILESTONE ★",
            Style::default()
                .fg(P_NEON)
                .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
        ));
    }

    let style = if hot {
        Style::default().bg(Color::Indexed(89))
    } else {
        Style::default()
    };

    let p = Paragraph::new(Line::from(spans)).style(style);
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_with_active_streak(secs: f64) -> StreakCore {
        let mut core = StreakCore::new();
        let mut remaining = secs;
        while remaining > 0.0 {
            let step = remaining.min(0.05);
            advance(&mut core, 1, step);
            remaining -= step;
        }
        core
    }

    #[test]
    fn fmt_score_compacts_large_values() {
        assert_eq!(fmt_score(0), "0");
        assert_eq!(fmt_score(9_999), "9999");
        assert_eq!(fmt_score(10_000), "10K");
        assert_eq!(fmt_score(817_400), "817K");
        assert_eq!(fmt_score(1_234_567), "1.2M");
    }

    #[test]
    fn big_hud_width_matches_growing_values() {
        let small = StreakHud {
            combo: 1,
            streak_secs: 0.0,
            best_secs: 0.0,
            score: 0,
            milestone: false,
            flash: 0.0,
        };
        let big = StreakHud {
            combo: 12,
            streak_secs: 3600.0,
            best_secs: 7200.0,
            score: 1_234_567,
            milestone: false,
            flash: 0.0,
        };
        let w_small = big_hud_width(&small);
        let w_big = big_hud_width(&big);
        assert!(w_small > 0);
        assert!(w_big > w_small, "wider values must need more columns");
    }

    #[test]
    fn render_hud_big_and_line_paths_do_not_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let hud = StreakHud {
            combo: 3,
            streak_secs: 127.0,
            best_secs: 260.0,
            score: 45_600,
            milestone: true,
            flash: 1.0,
        };
        // Tall + wide → big path; 1-row and narrow → one-liner; 0-size
        // guards.
        for (w, h) in [(120u16, BIG_HUD_HEIGHT), (120, 1), (10, BIG_HUD_HEIGHT), (1, 1)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|f| render_hud(f, f.area(), &hud))
                .unwrap();
        }
    }

    #[test]
    fn big_hud_renders_pixel_blocks_when_it_fits() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let hud = StreakHud {
            combo: 2,
            streak_secs: 61.0,
            best_secs: 61.0,
            score: 100,
            milestone: false,
            flash: 0.0,
        };
        let backend = TestBackend::new(big_hud_width(&hud) + 4, BIG_HUD_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_hud(f, f.area(), &hud))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("COMBO"), "label row missing:\n{text}");
        assert!(text.contains("SCORE"), "label row missing:\n{text}");
        assert!(text.contains('█'), "pixel-font rows missing:\n{text}");
    }

    #[test]
    fn streak_tier_boundaries() {
        assert_eq!(streak_tier(0.0), 0);
        assert_eq!(streak_tier(59.0), 0);
        assert_eq!(streak_tier(60.0), 1);
        assert_eq!(streak_tier(299.999), 1);
        assert_eq!(streak_tier(300.0), 2);
        assert_eq!(streak_tier(899.999), 2);
        assert_eq!(streak_tier(900.0), 3);
        assert_eq!(streak_tier(1800.0), 4);
    }

    #[test]
    fn grace_keeps_streak_alive_under_five_seconds_idle() {
        let mut core = StreakCore::new();
        // 1 active agent for 2s.
        for _ in 0..20 {
            advance(&mut core, 1, 0.1);
        }
        let streak_before_idle = core.streak_secs;
        assert!((streak_before_idle - 2.0).abs() < 1e-9);

        // Drop to 0 active for 3s (< GRACE_SECS) — streak should keep
        // climbing through the grace window, not reset.
        for _ in 0..30 {
            advance(&mut core, 0, 0.1);
        }
        assert!(
            core.streak_secs > streak_before_idle,
            "streak should keep counting during grace: {}",
            core.streak_secs
        );

        // Come back within grace — no reset happened.
        advance(&mut core, 1, 0.1);
        assert!(core.streak_secs > 4.9);
    }

    #[test]
    fn streak_resets_after_grace_expires() {
        let mut core = StreakCore::new();
        for _ in 0..20 {
            advance(&mut core, 1, 0.1); // 2s active
        }
        assert!(core.streak_secs > 0.0);

        // Idle for longer than GRACE_SECS (5s) continuously.
        for _ in 0..70 {
            advance(&mut core, 0, 0.1); // 7s idle total
        }
        assert_eq!(core.streak_secs, 0.0);

        // One more idle step confirms it stays at zero, doesn't go
        // negative or bounce.
        advance(&mut core, 0, 0.1);
        assert_eq!(core.streak_secs, 0.0);
    }

    #[test]
    fn milestone_fires_once_at_60s_not_again_until_300s() {
        let mut core = StreakCore::new();

        // The very first active step is itself a milestone: combo goes
        // from 0 (no agents ever seen) to a new all-time high of 1.
        // Consume that one separately so it doesn't get confused with
        // the streak-tier milestones this test is about.
        assert!(advance(&mut core, 1, 0.1), "first-ever combo should fire a new-high milestone");
        let mut elapsed = 0.1;

        let mut fired_at_60 = false;
        let mut fire_count_before_300 = 0;

        // Step in small increments from just after start to just past
        // 60s. Combo stays pinned at 1 the whole time, so no further
        // combo-high milestones can fire here — only the streak tier-up.
        while elapsed < 61.0 {
            let fired = advance(&mut core, 1, 0.1);
            elapsed += 0.1;
            if fired && core.streak_secs >= 60.0 && core.streak_secs < 60.2 {
                fired_at_60 = true;
            }
            if fired {
                fire_count_before_300 += 1;
            }
        }
        assert!(fired_at_60, "expected a milestone right at the 60s crossing");
        // Only the 60s tier-up should have fired in this window.
        assert_eq!(fire_count_before_300, 1);

        // Continue up to 300s — expect exactly one more tier-up fire.
        let mut fire_count_to_300 = 0;
        while elapsed < 300.5 {
            let fired = advance(&mut core, 1, 0.1);
            elapsed += 0.1;
            if fired {
                fire_count_to_300 += 1;
            }
        }
        assert_eq!(fire_count_to_300, 1, "expected exactly one fire crossing 300s");
    }

    #[test]
    fn new_combo_high_fires_milestone_same_or_lower_does_not() {
        let mut core = StreakCore::new();

        // First activity at combo=2 sets an initial high (best_combo was 0).
        let fired = advance(&mut core, 2, 0.1);
        assert!(fired, "first nonzero combo should set a new high");
        assert_eq!(core.best_combo, 2);

        // Same combo again — no milestone (streak tier hasn't changed
        // either, we're at ~0.2s).
        let fired_same = advance(&mut core, 2, 0.1);
        assert!(!fired_same, "same combo should not refire a milestone");

        // Lower combo — no milestone.
        let fired_lower = advance(&mut core, 1, 0.1);
        assert!(!fired_lower, "lower combo should not fire a milestone");

        // New high combo — fires again.
        let fired_higher = advance(&mut core, 3, 0.1);
        assert!(fired_higher, "new combo high should fire a milestone");
        assert_eq!(core.best_combo, 3);
    }

    #[test]
    fn score_is_monotonic_and_grows_while_active() {
        let mut core = StreakCore::new();
        let mut last_score = 0.0;
        for i in 0..50 {
            let active = if i % 3 == 0 { 0 } else { 2 };
            advance(&mut core, active, 0.1);
            let score = core.score_acc * SCORE_SCALE;
            assert!(score >= last_score, "score must never decrease");
            last_score = score;
        }
        assert!(last_score > 0.0, "score should have grown while active");
    }

    #[test]
    fn best_secs_tracks_the_longest_streak_seen() {
        let core = core_with_active_streak(3.0);
        assert!((core.best_secs - 3.0).abs() < 1e-6);

        let mut core2 = StreakCore::new();
        for _ in 0..50 {
            advance(&mut core2, 1, 0.1); // 5s streak
        }
        for _ in 0..70 {
            advance(&mut core2, 0, 0.1); // idle past grace, resets streak
        }
        assert_eq!(core2.streak_secs, 0.0);
        // best_secs should retain the earlier peak even after reset.
        assert!(core2.best_secs >= 4.9);
    }
}
