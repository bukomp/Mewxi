//! Post-render "screen shake with pseudo-3D" transform for view 5.
//!
//! Applied AFTER the view has finished rendering, by mutating cells
//! directly in `f.buffer_mut()` within the view's `area`. Per-row
//! horizontal offsets follow a decaying sine wave that rolls down the
//! rows — neighbouring rows land at slightly different offsets, which
//! reads as a skew/parallax and gives a cheap pseudo-3D depth illusion —
//! plus an occasional 1-cell vertical jitter for a glitchy accent.
//!
//! Amplitude is driven by an event-driven pulse envelope (see
//! [`trigger_pulse`]) that decays exponentially over roughly 0.4–0.6s,
//! so a burst (e.g. a streak milestone) reads as a short, sharp jolt
//! rather than a constant rattle.
//!
//! No `rand` crate is used — [`lcg_next`] is a small hand-rolled linear
//! congruential generator, deterministic and dependency-free.

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Per-process shake/pulse animation state. Mirrors the `AnimState` /
/// `tick_anim` pattern used for the logo animation in the parent module:
/// a single global, advanced once per frame from a monotonically
/// accumulating `Instant`, with `dt` clamped so a stalled/backgrounded
/// terminal doesn't cause a huge jump on the next visible frame.
struct FxState {
    last_tick: Instant,
    /// Current pulse envelope amplitude, roughly 0..=1 (can be nudged
    /// slightly above 1 by concurrent triggers before the next decay
    /// tick pulls it back down via `envelope_decay`, which never grows
    /// it, so it settles quickly).
    envelope: f64,
    /// Rolling phase for the row-skew sine wave, radians, accumulated
    /// frame-to-frame so the wave keeps rolling smoothly.
    phase: f64,
    /// Rolling phase for the Insane-only idle wobble, radians.
    idle_phase: f64,
    /// Hand-rolled LCG seed, advanced once per frame to pick the
    /// (rare) vertical jitter row/direction.
    seed: u32,
}

static STATE: OnceLock<Mutex<FxState>> = OnceLock::new();

fn state_cell() -> &'static Mutex<FxState> {
    STATE.get_or_init(|| {
        Mutex::new(FxState {
            last_tick: Instant::now(),
            envelope: 0.0,
            phase: 0.0,
            idle_phase: 0.0,
            // Arbitrary non-zero seed; an LCG seeded at 0 with these
            // constants still walks away from 0 immediately since the
            // increment is non-zero, but a non-zero seed avoids the
            // degenerate first step being purely the increment.
            seed: 0x9E37_79B9,
        })
    })
}

/// Exponential envelope decay time constant, in seconds. Roughly 3×tau
/// is "fully settled" — ~0.7s, long enough that a jolt is unmistakable
/// without turning into a constant rattle.
const DECAY_TAU: f64 = 0.22;

/// Rolling phase speed for the row-skew sine wave, radians/second.
const PHASE_SPEED: f64 = 5.0;

/// Rolling phase speed for the Insane idle wobble, radians/second.
/// Slower than the pulse skew so it reads as a lazy breathing motion
/// rather than another jolt.
const IDLE_PHASE_SPEED: f64 = 1.2;

/// Baseline amplitude (pre-clamp, in "cells") of the Insane idle
/// wobble. Small on purpose — it's meant to be a subtle ambient tell
/// that agents are active, not another shake.
const IDLE_BASE_AMP: f64 = 0.35;

/// Per-row phase increment used by [`row_offset`] so the sine wave
/// rolls down the rows instead of moving every row in lockstep.
const ROW_K: f64 = 0.35;

/// Minimum combined amplitude (pre shake-level clamp) before the rare
/// vertical jitter is even considered, and only at `ShakeLevel::Full`.
const VJITTER_AMP_THRESHOLD: f64 = 0.15;

/// Probability (0..1) that a `Full`-level frame with enough amplitude
/// picks a row to jitter vertically by one cell.
const VJITTER_CHANCE: f64 = 0.12;

/// Bump the pulse envelope by `strength` (0..1-ish). Called by the root
/// on streak milestones and when the active-agent count rises.
/// Additive, saturating at 1.0. Negative strengths are ignored rather
/// than allowed to *reduce* the envelope — decay is `apply_shake`'s job.
pub fn trigger_pulse(strength: f64) {
    let cell = state_cell();
    let mut s = cell.lock().expect("fx state poisoned");
    s.envelope = (s.envelope + strength.max(0.0)).min(1.0);
}

/// Advance the envelope one frame and apply the shake to `buf` within
/// `area`. `agents_active` gates the Insane idle wobble. No-op for
/// `ShakeLevel::Off` or an empty area. Runs in O(area cells) and never
/// panics, even on 0×0 or 1×1 areas.
pub fn apply_shake(
    buf: &mut Buffer,
    area: Rect,
    level: super::ShakeLevel,
    intensity: super::FxIntensity,
    agents_active: bool,
) {
    if matches!(level, super::ShakeLevel::Off) {
        return;
    }
    if area.width == 0 || area.height == 0 {
        return;
    }

    let max_cells: i32 = match level {
        super::ShakeLevel::Off => return,
        super::ShakeLevel::Subtle => 1,
        super::ShakeLevel::Full => 3,
    };

    let cell = state_cell();
    let mut s = cell.lock().expect("fx state poisoned");

    let now = Instant::now();
    let dt = (now - s.last_tick).as_secs_f64().min(0.1).max(0.0);
    s.last_tick = now;

    s.envelope = envelope_decay(s.envelope, dt, DECAY_TAU);
    s.phase = (s.phase + dt * PHASE_SPEED).rem_euclid(std::f64::consts::TAU);

    let idle_amp = if matches!(intensity, super::FxIntensity::Insane) && agents_active {
        s.idle_phase = (s.idle_phase + dt * IDLE_PHASE_SPEED).rem_euclid(std::f64::consts::TAU);
        // Rectified so it's a continuous non-negative "wobble" amount
        // rather than something that cancels the envelope out.
        IDLE_BASE_AMP * (0.5 + 0.5 * s.idle_phase.sin())
    } else {
        0.0
    };

    let intensity_mult = match intensity {
        super::FxIntensity::Chill => 0.6,
        super::FxIntensity::Rave => 1.0,
        super::FxIntensity::Insane => 1.2,
    };

    let combined_amplitude = (s.envelope + idle_amp) * intensity_mult;
    // Scale the normalized envelope up to the level's cell cap — a
    // full-strength pulse should actually use `Full`'s ±3-cell range,
    // not round down to the same ±1 cell `Subtle` produces.
    let amp_cells = combined_amplitude * max_cells as f64;

    // Advance the LCG once per frame and use it to (rarely) pick a row
    // and direction for a 1-cell vertical jitter. Only at Full level
    // and only once amplitude is meaningful, so Subtle stays a pure
    // horizontal skew and idle-only frames don't jitter.
    s.seed = lcg_next(s.seed);
    let height = area.height;
    let vjitter: Option<(u16, i32)> = if matches!(level, super::ShakeLevel::Full)
        && combined_amplitude > VJITTER_AMP_THRESHOLD
        && height > 0
    {
        let roll = (s.seed >> 8) as f64 / (u32::MAX >> 8) as f64;
        if roll < VJITTER_CHANCE {
            let row_pick = (s.seed % height as u32) as u16;
            let dir = if s.seed & 1 == 0 { 1 } else { -1 };
            Some((row_pick, dir))
        } else {
            None
        }
    } else {
        None
    };

    let phase = s.phase;
    // Done mutating the shared state; drop the lock before touching
    // the buffer so a panic in rendering (there shouldn't be one, but
    // just in case) can't leave the mutex poisoned mid-shake.
    drop(s);

    let width = area.width;
    let mut row_buf: Vec<Cell> = Vec::with_capacity(width as usize);

    for ry in 0..height {
        let y = area.top() + ry;

        row_buf.clear();
        for dx in 0..width {
            let x = area.left() + dx;
            row_buf.push(buf.cell((x, y)).cloned().unwrap_or(Cell::EMPTY));
        }

        let hoff = row_offset(amp_cells, ry, phase, max_cells);
        write_row_shifted(buf, area, y, &row_buf, hoff);

        if let Some((row_pick, dir)) = vjitter {
            if ry == row_pick {
                let dst_y_i32 = y as i32 + dir;
                if dst_y_i32 >= area.top() as i32 && dst_y_i32 < area.top() as i32 + height as i32
                {
                    write_row_shifted(buf, area, dst_y_i32 as u16, &row_buf, hoff);
                }
            }
        }
    }
}

/// Write `snapshot` (a row of `area.width` cells) into row `y` of
/// `buf`, shifted horizontally by `offset` cells within `area`.
/// Cells shifted in from off-edge become blank. Bounds-checked via
/// `cell_mut`'s `Option`, so this never panics regardless of `offset`.
fn write_row_shifted(buf: &mut Buffer, area: Rect, y: u16, snapshot: &[Cell], offset: i32) {
    let width = area.width as i32;
    for dx in 0..width {
        let src_idx = dx - offset;
        let value = if src_idx >= 0 && (src_idx as usize) < snapshot.len() {
            snapshot[src_idx as usize].clone()
        } else {
            Cell::EMPTY
        };
        let x = area.left() as i32 + dx;
        if x < 0 || x > u16::MAX as i32 {
            continue;
        }
        if let Some(c) = buf.cell_mut((x as u16, y)) {
            *c = value;
        }
    }
}

/// Exponential envelope decay: new amplitude after `dt` seconds given
/// time constant `tau`. Returns `current * exp(-dt/tau)`, with `dt`
/// clamped to the same `[0, 0.1]` window every per-frame tick in this
/// crate uses, so a stalled/huge `dt` still decays by at most one
/// frame's worth and can never produce a negative or non-finite
/// result.
pub fn envelope_decay(current: f64, dt: f64, tau: f64) -> f64 {
    let dt = dt.clamp(0.0, 0.1);
    let tau = tau.max(1e-6);
    let decayed = current * (-dt / tau).exp();
    if decayed.is_finite() {
        decayed.max(0.0)
    } else {
        0.0
    }
}

/// Deterministic LCG step (Numerical Recipes constants). Returns the
/// next state given `seed`. Pure and dependency-free — no `rand`.
pub fn lcg_next(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

/// Horizontal cell offset for `row` given the current `amplitude`, a
/// rolling `phase`, and a per-level `max_cells` cap. A decaying sine
/// that rolls down the rows: `round(amplitude * sin(phase + row * k))`,
/// clamped to `[-max_cells, max_cells]`. `max_cells` is treated as a
/// magnitude (its sign is ignored) so a caller can never trigger a
/// `clamp` panic by passing a negative cap.
pub fn row_offset(amplitude: f64, row: u16, phase: f64, max_cells: i32) -> i32 {
    let max_cells = max_cells.abs();
    if max_cells == 0 || amplitude == 0.0 {
        return 0;
    }
    let raw = amplitude * (phase + row as f64 * ROW_K).sin();
    if !raw.is_finite() {
        return 0;
    }
    (raw.round() as i32).clamp(-max_cells, max_cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_decay_decays_toward_zero() {
        let next = envelope_decay(1.0, 0.05, 0.16);
        assert!(next < 1.0);
        assert!(next >= 0.0);
    }

    #[test]
    fn envelope_decay_clamps_huge_dt() {
        let next = envelope_decay(1.0, 1_000_000.0, 0.16);
        assert!(next.is_finite());
        assert!(next >= 0.0);
        assert!(next < 1.0);
    }

    #[test]
    fn envelope_decay_zero_stays_zero() {
        assert_eq!(envelope_decay(0.0, 0.05, 0.16), 0.0);
    }

    #[test]
    fn envelope_decay_negative_dt_is_clamped_not_growth() {
        // A negative dt should behave like dt=0 (no growth), never
        // amplify the envelope.
        let next = envelope_decay(0.5, -1.0, 0.16);
        assert!((next - 0.5).abs() < 1e-9);
    }

    #[test]
    fn lcg_next_is_deterministic() {
        let a = lcg_next(12345);
        let b = lcg_next(12345);
        assert_eq!(a, b);
    }

    #[test]
    fn lcg_next_changes_value() {
        let seed = 12345u32;
        assert_ne!(lcg_next(seed), seed);
    }

    #[test]
    fn lcg_next_sequence_varies() {
        let a = lcg_next(1);
        let b = lcg_next(a);
        let c = lcg_next(b);
        assert!(a != b || b != c);
    }

    #[test]
    fn row_offset_never_exceeds_cap() {
        for row in [0u16, 1, 5, 17, 200, u16::MAX] {
            for phase_i in 0..20 {
                let phase = phase_i as f64 * 0.7;
                let off = row_offset(3.0, row, phase, 2);
                assert!(off.abs() <= 2, "offset {off} exceeded cap for row {row}");
            }
        }
    }

    #[test]
    fn row_offset_zero_cap_is_zero() {
        assert_eq!(row_offset(5.0, 3, 1.0, 0), 0);
    }

    #[test]
    fn row_offset_zero_amplitude_is_zero() {
        assert_eq!(row_offset(0.0, 3, 1.0, 3), 0);
    }

    #[test]
    fn row_offset_negative_cap_does_not_panic() {
        // max_cells is documented as a magnitude; a negative value
        // must not cause `clamp` to panic (min > max).
        let off = row_offset(5.0, 3, 1.0, -2);
        assert!(off.abs() <= 2);
    }

    #[test]
    fn apply_shake_off_is_noop() {
        let area = Rect::new(0, 0, 4, 4);
        let mut buf = Buffer::empty(area);
        if let Some(c) = buf.cell_mut((1, 1)) {
            c.set_symbol("X");
        }
        let before = buf.clone();

        apply_shake(
            &mut buf,
            area,
            super::super::ShakeLevel::Off,
            super::super::FxIntensity::Rave,
            true,
        );

        assert_eq!(buf, before);
    }
}
