//! Agent-activity visualizer strip — a music-visualizer-style row of
//! jumping columns at the bottom of view 5, one column per running
//! agent — sessions AND their live sub-agents, each spawning its own
//! bar right beside its parent (the flattened slice is DFS-ordered, so
//! children follow their parent) — driven by state/activity instead of
//! an audio signal. The columns stretch to fill the strip's full width —
//! five agents means five wide bars spanning the whole band, not five
//! slivers in a corner.
//!
//! Each column eases toward a target height (see [`target_height`]) with
//! a fast attack / slow release, plus a small per-column bounce so
//! columns don't move in lockstep like a single pulsing block. Bars are
//! drawn bottom-up with the [`super::palette::BARS`] eighth-block ramp,
//! coloured hotter as they climb via [`super::palette::heat_color`].

use crate::live_session::{Activity, SessionState};
use crate::tui::SessionRef;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::palette::{BARS, P_LABEL, heat_color};

/// Per-frame dt is clamped to this many seconds so a backgrounded
/// terminal that gets redrawn after a long stall doesn't spring-snap
/// every column to its target in one frame.
const MAX_DT: f64 = 0.1;

/// Rise time constant — how quickly a column jumps up to a hotter
/// target. Fast, like a VU meter's attack.
const RISE_TAU: f64 = 0.12;

/// Fall time constant — how slowly a column decays back down (e.g. to
/// the idle ember). Slower than the rise, like a VU meter's release, so
/// the strip doesn't look jittery when activity flickers.
const FALL_TAU: f64 = 0.45;

/// Ember height a column decays to when its session is idle — a single
/// flickering cell rather than nothing, so the strip stays visibly
/// alive.
const EMBER: f64 = 0.06;

/// Peak-to-peak size of the per-column bounce, as a fraction of the
/// column's own current height (so the ember doesn't bounce wildly
/// while a tall bar visibly breathes).
const BOUNCE_AMPLITUDE: f64 = 0.05;

const BOUNCE_BASE_HZ: f64 = 1.3;
const BOUNCE_PER_COL_HZ: f64 = 0.11;

/// Animation state for one column, keyed by column index.
struct ColumnAnim {
    /// Eased height fraction, 0.0..=1.0, with no bounce applied.
    current: f64,
    /// Accumulated bounce phase, in radians.
    phase: f64,
}

struct VisualizerState {
    last_tick: Instant,
    columns: Vec<ColumnAnim>,
}

static STATE: OnceLock<Mutex<VisualizerState>> = OnceLock::new();

/// Move `current` toward `target` by one exponential-easing step. `dt` is
/// clamped to [`MAX_DT`] (and never negative) before use, so a stalled
/// clock can't produce a huge alpha. The result is always a convex
/// combination of `current` and `target` — it can never overshoot past
/// `target`, regardless of how large `dt` or how small `tau` is.
fn ease(current: f64, target: f64, dt: f64, tau: f64) -> f64 {
    let dt = dt.clamp(0.0, MAX_DT);
    if tau <= 0.0 {
        return target;
    }
    let alpha = 1.0 - (-dt / tau).exp();
    current + (target - current) * alpha
}

/// Target bar-height fraction, `0.0..=1.0`, for a session's current
/// state and activity. Pure — no animation, no I/O — so the mapping is
/// unit-testable on its own.
///
/// - An idle session decays to a small ember, regardless of its last
///   recorded activity.
/// - Writing/Editing/Running (actively producing output) peg near the
///   top.
/// - Asking/Awaiting (waiting on the user, but mid-turn) sit high.
/// - Delegating/running a named tool sit upper-mid.
/// - Thinking/Reading/Searching/Fetching/Starting/Compacting sit at
///   the middle — busy, but not visibly "loud".
pub fn target_height(state: SessionState, activity: &Activity) -> f64 {
    if matches!(state, SessionState::Idle) {
        return EMBER;
    }
    match activity {
        Activity::Writing | Activity::Editing | Activity::Running => 1.0,
        Activity::Asking | Activity::Awaiting => 0.8,
        Activity::Delegating | Activity::Tool(_) => 0.7,
        Activity::Thinking
        | Activity::Reading
        | Activity::Searching
        | Activity::Fetching
        | Activity::Starting
        | Activity::Compacting => 0.5,
        Activity::Waiting => EMBER,
    }
}

/// Advance the global visualizer animation by one frame for `cols`
/// (already truncated to the number of columns that fit) and return the
/// displayed height fraction — eased target plus bounce — for each.
fn tick_visualizer(cols: &[&SessionRef], now: Instant) -> Vec<f64> {
    let cell = STATE.get_or_init(|| {
        Mutex::new(VisualizerState {
            last_tick: now,
            columns: Vec::new(),
        })
    });
    let mut s = cell.lock().expect("visualizer state poisoned");
    let dt = (now.saturating_duration_since(s.last_tick))
        .as_secs_f64()
        .min(MAX_DT);
    s.last_tick = now;

    let n = cols.len();
    if s.columns.len() != n {
        s.columns.resize_with(n, || ColumnAnim {
            current: EMBER,
            phase: 0.0,
        });
    }

    let two_pi = std::f64::consts::TAU;
    let mut out = Vec::with_capacity(n);
    for (i, (col, session)) in s.columns.iter_mut().zip(cols.iter()).enumerate() {
        // A sub-agent row only exists while its delegation is live, so
        // it always counts as active — its `state` field mirrors the
        // parent session's and would wrongly ember the bar.
        let state = if session.subagent.is_some() {
            SessionState::Active
        } else {
            session.state
        };
        let target = target_height(state, &session.activity);
        let tau = if target >= col.current { RISE_TAU } else { FALL_TAU };
        col.current = ease(col.current, target, dt, tau).clamp(0.0, 1.0);

        let hz = BOUNCE_BASE_HZ + BOUNCE_PER_COL_HZ * i as f64;
        col.phase = (col.phase + dt * hz * two_pi).rem_euclid(two_pi);
        let bounce = BOUNCE_AMPLITUDE * col.phase.sin() * col.current;

        out.push((col.current + bounce).clamp(0.0, 1.0));
    }
    out
}

/// Render the agent-activity visualizer strip into `area`. `sessions` is
/// the flattened slice — every row gets a column, sub-agents included,
/// so a session's children bounce right beside it. Early-returns without
/// drawing anything when `area` is too short/narrow or there is nothing
/// to show.
pub fn render(f: &mut Frame, area: Rect, sessions: &[&SessionRef]) {
    if area.height < 3 || area.width == 0 {
        return;
    }

    let total = sessions.len();
    if total == 0 {
        return;
    }

    let avail = area.width as usize;
    // Columns are drawn as `col gap col gap … col` (no trailing gap), so
    // `cols` fit in `cols*2 - 1` cells.
    let max_cols_fit = (avail + 1) / 2;

    let mut cols_to_show = total.min(max_cols_fit).max(0);
    let mut indicator = String::new();
    if total > cols_to_show {
        // Shrink the column count until the trailing overflow indicator
        // fits alongside the bars, giving up (indicator-only / nothing)
        // if the area is too narrow for even one column.
        loop {
            let overflow_n = total - cols_to_show;
            let candidate = format!("»{overflow_n}");
            let candidate_w = candidate.chars().count();
            let bars_w = cols_to_show.saturating_mul(2).saturating_sub(if cols_to_show > 0 { 1 } else { 0 });
            let gap = if cols_to_show > 0 { 1 } else { 0 };
            let needed = bars_w + gap + candidate_w;
            if needed <= avail || cols_to_show == 0 {
                indicator = candidate;
                break;
            }
            cols_to_show -= 1;
        }
    }

    let now = Instant::now();
    let heights = tick_visualizer(&sessions[..cols_to_show], now);

    // Stretch the columns across the strip's whole width: the overflow
    // indicator (plus its gap) is carved off the right edge, the rest is
    // slotted equally among the bars.
    let indicator_w = if indicator.is_empty() {
        0
    } else {
        indicator.chars().count() + 1
    };
    let strip_w = avail.saturating_sub(indicator_w);
    let slots = slot_layout(strip_w, heights.len());

    let total_rows = area.height as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(total_rows);
    for y in 0..total_rows {
        let row_from_bottom = total_rows - 1 - y;
        let mut spans: Vec<Span> = Vec::with_capacity(heights.len() * 2 + 2);
        for (i, &frac) in heights.iter().enumerate() {
            let (bar_w, gap_w) = slots[i];
            if bar_w == 0 {
                continue;
            }
            let total_eighths = ((frac * total_rows as f64 * 8.0).round().max(0.0)) as usize;
            let full_cells = total_eighths / 8;
            let remainder = total_eighths % 8;
            let glyph = if row_from_bottom < full_cells {
                Some(BARS[7])
            } else if row_from_bottom == full_cells && remainder > 0 {
                Some(BARS[remainder - 1])
            } else {
                None
            };
            match glyph {
                Some(g) => spans.push(Span::styled(
                    g.to_string().repeat(bar_w),
                    Style::default().fg(heat_color(frac)),
                )),
                None => spans.push(Span::raw(" ".repeat(bar_w))),
            }
            if gap_w > 0 {
                spans.push(Span::raw(" ".repeat(gap_w)));
            }
        }
        if y == total_rows - 1 && !indicator.is_empty() {
            if !heights.is_empty() {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(indicator.clone(), Style::default().fg(P_LABEL)));
        }
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// Split `strip_w` cells into `n` equal-as-possible column slots,
/// returning `(bar_w, gap_w)` per column. A single gap column separates
/// adjacent bars (none after the last), slot widths never differ by more
/// than one cell, and the pieces always sum to exactly `strip_w`.
fn slot_layout(strip_w: usize, n: usize) -> Vec<(usize, usize)> {
    (0..n)
        .map(|i| {
            let start = i * strip_w / n;
            let end = (i + 1) * strip_w / n;
            let slot = end - start;
            let gap = if i + 1 < n && slot > 1 { 1 } else { 0 };
            (slot - gap, gap)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_decays_to_a_small_ember() {
        let h = target_height(SessionState::Idle, &Activity::Waiting);
        assert!(h > 0.0 && h <= 0.1, "expected small ember, got {h}");

        // Idle overrides any stale activity.
        let h2 = target_height(SessionState::Idle, &Activity::Writing);
        assert!(h2 > 0.0 && h2 <= 0.1, "idle must override activity, got {h2}");
    }

    #[test]
    fn writing_is_near_the_top() {
        let h = target_height(SessionState::Active, &Activity::Writing);
        assert!(h >= 0.95, "expected near-max height for Writing, got {h}");
    }

    #[test]
    fn monotonic_sanity_writing_thinking_idle() {
        let writing = target_height(SessionState::Active, &Activity::Writing);
        let thinking = target_height(SessionState::Active, &Activity::Thinking);
        let idle = target_height(SessionState::Idle, &Activity::Waiting);
        assert!(writing >= thinking, "{writing} should be >= {thinking}");
        assert!(thinking >= idle, "{thinking} should be >= {idle}");
    }

    #[test]
    fn all_variants_within_unit_range() {
        let activities = [
            Activity::Waiting,
            Activity::Starting,
            Activity::Thinking,
            Activity::Writing,
            Activity::Reading,
            Activity::Editing,
            Activity::Searching,
            Activity::Fetching,
            Activity::Running,
            Activity::Delegating,
            Activity::Asking,
            Activity::Awaiting,
            Activity::Compacting,
            Activity::Tool("Bash".into()),
        ];
        for state in [SessionState::Active, SessionState::Idle] {
            for a in &activities {
                let h = target_height(state, a);
                assert!((0.0..=1.0).contains(&h), "{state:?}/{a:?} -> {h} out of range");
            }
        }
    }

    #[test]
    fn ease_moves_toward_target_without_overshoot() {
        let mut cur = 0.0;
        for _ in 0..50 {
            cur = ease(cur, 1.0, 0.05, 0.12);
            assert!((0.0..=1.0).contains(&cur), "overshot: {cur}");
        }
        assert!(cur > 0.9, "expected convergence near target, got {cur}");
    }

    #[test]
    fn ease_never_overshoots_for_large_dt() {
        // Even a huge dt should land exactly on target, never past it,
        // because the result is a convex combination of current/target.
        let up = ease(0.2, 0.9, 1_000.0, 0.12);
        assert!((0.19999..=0.9 + 1e-9).contains(&up), "up overshoot: {up}");
        let down = ease(0.9, 0.2, 1_000.0, 0.12);
        assert!((0.2 - 1e-9..=0.90001).contains(&down), "down overshoot: {down}");
    }

    #[test]
    fn slot_layout_fills_the_full_width() {
        for (w, n) in [(88usize, 5usize), (40, 5), (10, 3), (7, 3), (60, 1), (5, 5)] {
            let slots = slot_layout(w, n);
            assert_eq!(slots.len(), n);
            let total: usize = slots.iter().map(|(b, g)| b + g).sum();
            assert_eq!(total, w, "slots must sum to the strip width ({w}, {n})");
            let bars: Vec<usize> = slots.iter().map(|(b, _)| *b).collect();
            let min = bars.iter().min().unwrap();
            let max = bars.iter().max().unwrap();
            assert!(max - min <= 2, "bars should stay near-equal ({w}, {n}): {bars:?}");
            // No trailing gap — the last slot ends flush at the edge.
            assert_eq!(slots.last().unwrap().1, 0);
        }
    }

    #[test]
    fn slot_layout_zero_columns_is_empty() {
        assert!(slot_layout(50, 0).is_empty());
    }

    #[test]
    fn ease_clamps_negative_dt_to_zero() {
        // A negative dt (clock skew) must not move current away from
        // target or blow up the exponent.
        let cur = ease(0.5, 1.0, -5.0, 0.12);
        assert_eq!(cur, 0.5);
    }
}
