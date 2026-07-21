//! View 5 — "Mewxi rave" view: full-featured accounts + sessions dashboard
//! restyled in a Y2K/arcade purple-pink palette, with an animated cat
//! mascot, a big pixel-font headline, a music-visualizer strip driven by
//! agent activity, an arcade-style streak/combo HUD, and an optional
//! screen-shake post-pass.
//!
//! This file is the module root/orchestrator: it declares the
//! `view_mewxi/*` submodules (palette, font, visualizer, fx, streaks,
//! table, accounts_panel), defines the public config surface
//! (`RaveConfig` + its enums), lays out the view adaptively for whatever
//! terminal size is available, and wires each submodule's `render` into
//! that layout. The animated cat-logo machinery (`AnimState`/`tick_anim`/
//! `render_logo`/`pick_logo` and friends) is carried over unchanged from
//! the pre-rebuild version of this file — it's now rendered as a small
//! side mascot rather than owning half the screen.

mod accounts_panel;
mod font;
mod fx;
mod marquee;
mod palette;
mod score_store;
pub(super) mod scores_modal;
mod streaks;
mod table;
mod visualizer;

use super::{
    LOGO_LARGE, LOGO_LARGE_DIMS, LOGO_MEDIUM, LOGO_MEDIUM_DIMS, LOGO_SMALL, LOGO_SMALL_DIMS,
    LOGO_TINY, LOGO_TINY_DIMS, PerAccount, SessionRef,
};
use crate::live_session::{Activity, SessionState};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, TableState};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

// ---------------------------------------------------------------------
// Public config surface
// ---------------------------------------------------------------------

/// How hard the post-render screen-shake effect hits. `Off` disables it
/// entirely (zero cost — [`fx::apply_shake`] no-ops immediately).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShakeLevel {
    Off,
    Subtle,
    Full,
}

impl ShakeLevel {
    /// Parse a config string, trimmed and case-insensitive. Unknown or
    /// absent values default to [`ShakeLevel::Subtle`].
    pub fn from_config(s: Option<&str>) -> Self {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("off") => ShakeLevel::Off,
            Some("full") => ShakeLevel::Full,
            _ => ShakeLevel::Subtle,
        }
    }
}

/// Overall intensity multiplier for the rave effects (shake amplitude,
/// idle wobble, …).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxIntensity {
    Chill,
    Rave,
    Insane,
}

impl FxIntensity {
    /// Parse a config string, trimmed and case-insensitive. Unknown or
    /// absent values default to [`FxIntensity::Rave`].
    pub fn from_config(s: Option<&str>) -> Self {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("chill") => FxIntensity::Chill,
            Some("insane") => FxIntensity::Insane,
            _ => FxIntensity::Rave,
        }
    }
}

/// Which style of headline chrome to draw at the top of the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsciiStyle {
    /// Big blocky pixel-font "MEWXI" headline + animated cat mascot.
    Y2k,
    /// A single plain styled title line — animations keep running
    /// underneath, just without the big block-letter chrome.
    Classic,
}

impl AsciiStyle {
    /// Parse a config string, trimmed and case-insensitive. Unknown or
    /// absent values default to [`AsciiStyle::Y2k`].
    pub fn from_config(s: Option<&str>) -> Self {
        match s.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("classic") => AsciiStyle::Classic,
            _ => AsciiStyle::Y2k,
        }
    }
}

/// Full configuration for view 5's rave rendering, threaded through from
/// user config by the caller.
#[derive(Clone, Copy, Debug)]
pub struct RaveConfig {
    /// Whether to draw the agent-activity visualizer strip.
    pub visualizer: bool,
    pub shake: ShakeLevel,
    /// Whether to draw the arcade combo/streak HUD line.
    pub streaks: bool,
    pub intensity: FxIntensity,
    pub ascii_style: AsciiStyle,
}

impl Default for RaveConfig {
    fn default() -> Self {
        RaveConfig {
            visualizer: true,
            shake: ShakeLevel::Subtle,
            streaks: true,
            intensity: FxIntensity::Rave,
            ascii_style: AsciiStyle::Y2k,
        }
    }
}

// ---------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------

/// Render the full rave view into `area`.
///
/// `sessions_rect` is an out-param: the caller uses it as the sessions
/// table's hit-area for mouse-wheel scrolling, mirroring view_all's
/// contract. `table_state` is owned by the caller across frames so
/// scroll position persists. `selected_driven` mirrors view_all: true
/// when the selected row is a mewxi-driven session, the only case
/// where the `Del kill` footer chip applies.
#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    area: Rect,
    accounts: &[&PerAccount],
    sessions: &[&SessionRef],
    selected: Option<usize>,
    sessions_rect: &mut Option<Rect>,
    table_state: &mut TableState,
    selected_driven: bool,
    cfg: &RaveConfig,
) {
    if area.width < 4 || area.height < 4 {
        return;
    }

    // The footer hint row sits at the very bottom of the screen, below
    // the bordered block — same placement as every other view. Carved
    // off first so the block (and the shake pass, which only touches
    // the block) can never overlap it.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    let block_area = outer[0];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette::P_MID))
        .title(Span::styled(
            font::deco_bracket(" MEWXI "),
            Style::default().fg(palette::P_HOT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(block_area);
    f.render_widget(block, block_area);

    // `Del kill` only when the selected row is a mewxi-driven session —
    // same gating as view 1's footer. `? help` always.
    let mut hint = String::from("↑/↓ select · Enter open · n new · s scores");
    if selected_driven {
        hint.push_str(" · Del kill");
    }
    hint.push_str(" · r refresh limits · Esc back · ? help");
    super::widgets::render_footer(f, outer[1], "m", &hint, true);

    if inner.width == 0 || inner.height == 0 {
        fx::apply_shake(f.buffer_mut(), block_area, cfg.shake, cfg.intensity, false);
        return;
    }

    // Top-level sessions only — used for the "an agent came online"
    // shake pulse below.
    let active_top = sessions
        .iter()
        .filter(|s| s.subagent.is_none() && s.state == SessionState::Active)
        .count();
    // The HUD's combo is parallelism width: every agent working right
    // now, sub-agents included — a session fanning out to N sub-agents
    // counts as 1 + N. Sub-agent rows only exist while their delegation
    // runs, so their presence *is* their activity.
    let workers =
        active_top + sessions.iter().filter(|s| s.subagent.is_some()).count();

    let hud = if cfg.streaks {
        Some(streaks::tick(workers))
    } else {
        None
    };

    // Fire shake pulses on every event worth feeling, so the screen
    // reacts to the swarm rather than only to streak milestones: an
    // agent coming online, a session or sub-agent row appearing or
    // wrapping up, and a burst of "loud" work (writing / editing /
    // running) kicking in. Milestones stay the biggest jolt.
    let row_count = sessions.len();
    let loud = sessions
        .iter()
        .filter(|s| {
            (s.subagent.is_some() || s.state == SessionState::Active)
                && matches!(
                    s.activity,
                    Activity::Writing | Activity::Editing | Activity::Running
                )
        })
        .count();
    static PULSE_MEMORY: OnceLock<Mutex<(usize, usize, usize)>> = OnceLock::new();
    {
        let cell = PULSE_MEMORY.get_or_init(|| Mutex::new((0, 0, 0)));
        let mut last = cell.lock().expect("pulse memory poisoned");
        let (last_active, last_rows, last_loud) = *last;
        if active_top > last_active {
            fx::trigger_pulse(0.8); // an agent came online
        }
        if row_count > last_rows {
            fx::trigger_pulse(0.6); // a new session/sub-agent appeared
        } else if row_count < last_rows {
            fx::trigger_pulse(0.4); // one wrapped up
        }
        if loud > last_loud {
            fx::trigger_pulse(0.5); // writing/editing/running just started
        }
        *last = (active_top, row_count, loud);
    }
    if let Some(h) = &hud {
        if h.milestone {
            fx::trigger_pulse(1.0);
        }
    }

    // --- Layout ---------------------------------------------------------
    // Two full-height columns, the pre-rebuild arrangement: chrome on
    // the left (pixel headline, marquee, streak HUD, cat mascot, and
    // the visualizer along the column's bottom edge), data on the right
    // (accounts panel over the sessions table, floor to ceiling).
    // Narrow terminals drop the chrome column entirely — the data
    // panels always win.
    let table_min: u16 = 5;

    let left_w: u16 = if inner.width >= 64 {
        (inner.width / 2).max(24)
    } else {
        0
    };

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_w), Constraint::Min(0)])
        .split(inner);

    // Headline shimmer/marquee phase is driven independently of the cat
    // mascot's activity-eased `ANIM_STATE` — that state is only ticked
    // when `render_logo` actually runs (i.e. the mascot is visible), so
    // reusing it here would either double-tick it or freeze the
    // headline whenever the mascot is hidden. Ticked even while the
    // chrome column is hidden so the animation doesn't snap when the
    // terminal widens again.
    let headline_phase_f = tick_headline_phase();
    let headline_phase = headline_phase_f as usize;
    let marquee_offset = (headline_phase_f * 4.0) as usize;

    if left_w > 0 {
        render_chrome_column(
            f,
            cols[0],
            cfg,
            hud.as_ref(),
            headline_phase,
            marquee_offset,
            workers,
            sessions,
        );
    }

    // Right column: accounts over the sessions table, exactly like the
    // pre-rebuild splash's side panel (but with the full parity table).
    let right = cols[1];
    let full_acct_want = accounts_panel::ROWS_PER_ACCOUNT * accounts.len() as u16 + 2;
    let acct_budget = right.height.saturating_sub(table_min);
    let compact_accounts = full_acct_want > acct_budget;
    let acct_want = if compact_accounts {
        accounts.len() as u16 + 2
    } else {
        full_acct_want
    };
    let acct_h = acct_want.min(acct_budget);
    let right_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(acct_h), Constraint::Min(table_min)])
        .split(right);
    accounts_panel::render(f, right_rows[0], accounts, compact_accounts);
    *sessions_rect = Some(right_rows[1]);
    table::render(f, right_rows[1], sessions, selected, table_state);

    // Screen-shake post-pass — mutates already-rendered cells within
    // the block, so it must run last. The footer below the block is
    // deliberately left steady.
    fx::apply_shake(f.buffer_mut(), block_area, cfg.shake, cfg.intensity, workers > 0);
}

/// Flush the arcade score state to the local status file — called by
/// the event loop once on shutdown so the tail of a run isn't lost.
pub(super) fn flush_scores() {
    streaks::flush_scores();
}

/// Headline shimmer/marquee phase state — a steady independent clock
/// (see the comment at its call site in [`render_rave`]).
static HEADLINE_PHASE: OnceLock<Mutex<(Instant, f64)>> = OnceLock::new();

fn tick_headline_phase() -> f64 {
    let cell = HEADLINE_PHASE.get_or_init(|| Mutex::new((Instant::now(), 0.0)));
    let mut s = cell.lock().expect("headline phase state poisoned");
    let now = Instant::now();
    let dt = (now - s.0).as_secs_f64().min(0.1);
    s.0 = now;
    s.1 = (s.1 + dt * 3.0).rem_euclid(1000.0);
    s.1
}

/// Render the left chrome column, top to bottom: the pixel-font
/// headline (a single plain title line in [`AsciiStyle::Classic`]), a
/// gradient separator and marquee ticker row when there's room, the
/// streak HUD (big pixel-font when the column is tall and wide enough,
/// else the one-liner), the animated cat mascot centred in whatever
/// height remains, and the agent visualizer along the column's bottom
/// edge at the column's full width.
#[allow(clippy::too_many_arguments)]
fn render_chrome_column(
    f: &mut Frame,
    area: Rect,
    cfg: &RaveConfig,
    hud: Option<&streaks::StreakHud>,
    phase: usize,
    marquee_offset: usize,
    active_agents: usize,
    sessions: &[&SessionRef],
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let y2k = matches!(cfg.ascii_style, AsciiStyle::Y2k);
    let headline_h: u16 = if y2k { font::HEADLINE_HEIGHT } else { 1 }.min(area.height);
    let mut used = headline_h;
    let sep_h: u16 = if y2k && area.height > used { 1 } else { 0 };
    used += sep_h;
    let marquee_h: u16 = if y2k && area.height > used { 1 } else { 0 };
    used += marquee_h;

    let hud_h: u16 = match hud {
        Some(h) if cfg.streaks => {
            let left = area.height.saturating_sub(used);
            if left >= streaks::BIG_HUD_HEIGHT && area.width >= streaks::big_hud_width(h) {
                streaks::BIG_HUD_HEIGHT
            } else if left >= 1 {
                1
            } else {
                0
            }
        }
        _ => 0,
    };
    used += hud_h;

    // Visualizer along the column's bottom: 4–6 rows, but only once the
    // mascot keeps a workable window above it.
    let rem = area.height.saturating_sub(used);
    let viz_h: u16 = if cfg.visualizer {
        if rem >= 12 {
            6
        } else if rem >= 8 {
            4
        } else {
            0
        }
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(headline_h),
            Constraint::Length(sep_h),
            Constraint::Length(marquee_h),
            Constraint::Length(hud_h),
            Constraint::Min(0),
            Constraint::Length(viz_h),
        ])
        .split(area);

    if y2k {
        render_headline_text(f, chunks[0], phase);
    } else {
        let line = Line::from(Span::styled(
            font::deco_bracket("MEWXI RAVE"),
            Style::default().fg(palette::P_HOT).add_modifier(Modifier::BOLD),
        ));
        f.render_widget(Paragraph::new(line).alignment(Alignment::Center), chunks[0]);
    }
    if sep_h > 0 {
        f.render_widget(
            Paragraph::new(font::gradient_separator(chunks[1].width as usize)),
            chunks[1],
        );
    }
    if marquee_h > 0 {
        let text = font::marquee(
            &marquee::ticker_text(sessions),
            chunks[2].width as usize,
            marquee_offset,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default().fg(palette::P_TEXT),
            ))),
            chunks[2],
        );
    }
    if hud_h > 0 {
        if let Some(h) = hud {
            streaks::render_hud(f, chunks[3], h);
        }
    }
    render_logo(f, chunks[4], active_agents);
    if viz_h > 0 {
        visualizer::render(f, chunks[5], sessions);
    }
}

fn render_headline_text(f: &mut Frame, area: Rect, phase: usize) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = font::headline_lines("MEWXI", phase);
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

// ---------------------------------------------------------------------
// Animated cat mascot — carried over unchanged from the pre-rebuild
// view. No longer owns half the screen; rendered as a side mascot in
// the headline band when there's spare width/height for it.
// ---------------------------------------------------------------------

/// Per-process animation state. Updated once per frame from
/// `tick_anim` — accumulating phases (rather than recomputing from
/// elapsed × freq each frame) lets us change frequency without tearing,
/// and lets `eased_active` ramp smoothly between values when agents
/// start/stop instead of snapping.
struct AnimState {
    last_tick: Instant,
    /// Exponentially-smoothed count of active agents. Lags the real
    /// count by ~`EASE_TAU` seconds, so transitions ease in and out.
    eased_active: f64,
    /// Sin-wave bob phase, in radians, accumulated frame-to-frame.
    bob_phase: f64,
    /// Colour-wave phase, in palette steps, accumulated frame-to-frame.
    wave_phase: f64,
}

static ANIM_STATE: OnceLock<Mutex<AnimState>> = OnceLock::new();

/// Exponential ease time constant. ~63% of the way to the new target
/// after this many seconds; effectively settled after ~3× this. 0.8s
/// is long enough to read as a fade, short enough not to feel laggy.
const EASE_TAU: f64 = 0.8;

/// Advance the global animation state by one frame for the given
/// active-agent count and return `(eased_active, bob_phase, wave_phase)`.
fn tick_anim(active: usize) -> (f64, f64, f64) {
    let cell = ANIM_STATE.get_or_init(|| {
        Mutex::new(AnimState {
            last_tick: Instant::now(),
            eased_active: 0.0,
            bob_phase: 0.0,
            wave_phase: 0.0,
        })
    });
    let mut s = cell.lock().expect("anim state poisoned");
    let now = Instant::now();
    // Clamp dt so a long stall (terminal backgrounded) doesn't cause a
    // big spring-snap on the next visible frame.
    let dt = (now - s.last_tick).as_secs_f64().min(0.1);
    s.last_tick = now;

    let target = active as f64;
    let alpha = 1.0 - (-dt / EASE_TAU).exp();
    s.eased_active += (target - s.eased_active) * alpha;

    // Intensity is the eased active count, clamped to [0, 1]. It
    // gates BOTH the colour wave and the bob, so when no agents are
    // working they both ease to a stop, and when activity resumes they
    // ease back up. The base rates never apply at zero intensity.
    let intensity = s.eased_active.clamp(0.0, 1.0);
    let two_pi = std::f64::consts::TAU;
    let bob_hz = (BOB_BASE_HZ + BOB_PER_AGENT_HZ * s.eased_active) * intensity;
    s.bob_phase = (s.bob_phase + dt * bob_hz * two_pi).rem_euclid(two_pi);

    let wave_hz = (WAVE_BASE_HZ + WAVE_PER_AGENT_HZ * s.eased_active) * intensity;
    s.wave_phase = (s.wave_phase + dt * wave_hz).rem_euclid(WAVE.len() as f64);

    (s.eased_active, s.bob_phase, s.wave_phase)
}

/// Purple wave palette cycled through the logo rows. Palindromic so
/// the gradient flows smoothly in both directions as it scrolls. Many
/// closely-spaced steps so neighbouring rows differ by one perceptual
/// notch — that's what makes the wave look continuous rather than
/// banded. Stored as raw 256-colour indices so they can be RGB-cube
/// blended with `REST_INDEX` per frame.
const WAVE: &[u8] = &[
    53, 54, 55, 56, 91, 92, 97, 98, 99,
    134, 135, 140, 141, 170, 171, 176, 177,
    206, 207, 213, 219,
    213, 207, 206, 177, 176, 171, 170,
    141, 140, 135, 134, 99, 98, 97, 92, 91, 56, 55, 54,
];

/// Solid colour the logo settles on when no agents are working.
/// `Color::Indexed(135)` — a calm medium purple. All animated rows
/// crossfade to this as `eased_active → 0`.
const REST_INDEX: u8 = 135;

/// Decompose a 6×6×6 cube colour index (16..=231) into (r, g, b),
/// each in 0..=5. Indices outside the cube fall back to the rest
/// colour's coordinates — none of our palette values hit that path.
fn cube_decompose(idx: u8) -> (u8, u8, u8) {
    let n = idx.saturating_sub(16);
    (n / 36, (n / 6) % 6, n % 6)
}

fn cube_compose(r: u8, g: u8, b: u8) -> u8 {
    16 + 36 * r.min(5) + 6 * g.min(5) + b.min(5)
}

/// Lerp between two 256-colour cube indices in RGB space. `t=0 → a`,
/// `t=1 → b`. Used to smoothly bleed each row's animated colour into
/// `REST_INDEX` as the eased intensity drops.
fn blend_cube(a: u8, b: u8, t: f64) -> u8 {
    let t = t.clamp(0.0, 1.0);
    let (ar, ag, ab) = cube_decompose(a);
    let (br, bg, bb) = cube_decompose(b);
    let lerp = |x: u8, y: u8| -> u8 {
        (x as f64 * (1.0 - t) + y as f64 * t).round() as u8
    };
    cube_compose(lerp(ar, br), lerp(ag, bg), lerp(ab, bb))
}

/// Base wave speed in palette steps per second when no agent is
/// active. Each active agent multiplies this — the more work in
/// flight, the faster the colour ripples down the logo.
const WAVE_BASE_HZ: f64 = 5.5;
const WAVE_PER_AGENT_HZ: f64 = 1.5;

/// Sin-wave bob, only enabled when ≥1 agent is active. Frequency is
/// kept low so the motion reads as breathing rather than bouncing.
/// Amplitude scales with panel height between `BOB_AMP_MIN_ROWS` (on
/// small terminals) and `BOB_AMP_MAX_ROWS` (on large ones), and is
/// also capped by the slack between the logo and the panel edge.
const BOB_BASE_HZ: f64 = 0.18;
const BOB_PER_AGENT_HZ: f64 = 0.05;
const BOB_AMP_MIN_ROWS: f64 = 3.0;
const BOB_AMP_MAX_ROWS: f64 = 7.0;
/// Panel heights at which amplitude hits the min and max. Linearly
/// interpolated in between, clamped at the ends.
const BOB_AMP_MIN_AT_HEIGHT: f64 = 24.0;
const BOB_AMP_MAX_AT_HEIGHT: f64 = 60.0;

fn render_logo(f: &mut Frame, area: Rect, active_agents: usize) {
    let (src, logo_h) = pick_logo(area);

    // Advance the shared, eased animation state. `eased_active` ramps
    // smoothly toward `active_agents`, so amplitude and frequency
    // transitions don't snap when agents start or stop.
    let (eased_active, bob_phase, wave_phase) = tick_anim(active_agents);
    let phase = wave_phase as usize;

    // `rest_blend` is the fraction of the way from animated → rest the
    // colour should be on this frame. At full activity it's 0 (pure
    // animated rainbow); at idle it's 1 (every row collapses onto
    // `REST_INDEX`). The lerp happens in the 256-colour cube so the
    // transition looks like a real colour fade, not a palette skip.
    let intensity = eased_active.clamp(0.0, 1.0);
    let rest_blend = 1.0 - intensity;

    let lines: Vec<Line> = src
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            let animated = WAVE[(i + phase) % WAVE.len()];
            let idx = blend_cube(animated, REST_INDEX, rest_blend);
            Line::from(Span::styled(
                l.to_string(),
                Style::default()
                    .fg(Color::Indexed(idx))
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .collect();

    // Vertical sin-wave bob — amplitude is multiplied by an eased
    // intensity (0 → 1 as agents come online, 1 → 0 as they all idle),
    // so the cat glides to and from rest instead of snapping. Target
    // amplitude scales linearly with panel height (small screens get
    // ~5 rows, big screens up to ~10), then is clipped to the slack
    // between logo and panel edge so it never pushes past the border.
    let base_pad = area.height.saturating_sub(logo_h) / 2;
    let bob_offset_rows: i32 = if base_pad > 0 && intensity > 0.001 {
        let t = ((area.height as f64 - BOB_AMP_MIN_AT_HEIGHT)
            / (BOB_AMP_MAX_AT_HEIGHT - BOB_AMP_MIN_AT_HEIGHT))
            .clamp(0.0, 1.0);
        let target_amp = BOB_AMP_MIN_ROWS + t * (BOB_AMP_MAX_ROWS - BOB_AMP_MIN_ROWS);
        let slack = (base_pad as f64 - 1.0).max(0.0);
        let amp = target_amp.min(slack) * intensity;
        (bob_phase.sin() * amp).round() as i32
    } else {
        0
    };
    let max_pad = area.height.saturating_sub(logo_h);
    let top_pad = (base_pad as i32 + bob_offset_rows)
        .clamp(0, max_pad as i32) as u16;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_pad),
            Constraint::Length(logo_h),
            Constraint::Min(0),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        chunks[1],
    );
}

fn pick_logo(area: Rect) -> (&'static str, u16) {
    if area.height >= LOGO_LARGE_DIMS.0 && area.width >= LOGO_LARGE_DIMS.1 {
        (LOGO_LARGE, LOGO_LARGE_DIMS.0)
    } else if area.height >= LOGO_MEDIUM_DIMS.0 && area.width >= LOGO_MEDIUM_DIMS.1 {
        (LOGO_MEDIUM, LOGO_MEDIUM_DIMS.0)
    } else if area.height >= LOGO_SMALL_DIMS.0 && area.width >= LOGO_SMALL_DIMS.1 {
        (LOGO_SMALL, LOGO_SMALL_DIMS.0)
    } else {
        (LOGO_TINY, LOGO_TINY_DIMS.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shake_level_from_config() {
        assert_eq!(ShakeLevel::from_config(Some("off")), ShakeLevel::Off);
        assert_eq!(ShakeLevel::from_config(Some("subtle")), ShakeLevel::Subtle);
        assert_eq!(ShakeLevel::from_config(Some("full")), ShakeLevel::Full);
        assert_eq!(ShakeLevel::from_config(Some("  FULL ")), ShakeLevel::Full);
        assert_eq!(ShakeLevel::from_config(None), ShakeLevel::Subtle);
        assert_eq!(ShakeLevel::from_config(Some("garbage")), ShakeLevel::Subtle);
    }

    #[test]
    fn fx_intensity_from_config() {
        assert_eq!(FxIntensity::from_config(Some("chill")), FxIntensity::Chill);
        assert_eq!(FxIntensity::from_config(Some("rave")), FxIntensity::Rave);
        assert_eq!(FxIntensity::from_config(Some("insane")), FxIntensity::Insane);
        assert_eq!(FxIntensity::from_config(Some("  Insane ")), FxIntensity::Insane);
        assert_eq!(FxIntensity::from_config(Some("CHILL")), FxIntensity::Chill);
        assert_eq!(FxIntensity::from_config(None), FxIntensity::Rave);
        assert_eq!(FxIntensity::from_config(Some("garbage")), FxIntensity::Rave);
    }

    #[test]
    fn ascii_style_from_config() {
        assert_eq!(AsciiStyle::from_config(Some("y2k")), AsciiStyle::Y2k);
        assert_eq!(AsciiStyle::from_config(Some("classic")), AsciiStyle::Classic);
        assert_eq!(AsciiStyle::from_config(Some("  Classic ")), AsciiStyle::Classic);
        assert_eq!(AsciiStyle::from_config(Some("Y2K")), AsciiStyle::Y2k);
        assert_eq!(AsciiStyle::from_config(None), AsciiStyle::Y2k);
        assert_eq!(AsciiStyle::from_config(Some("garbage")), AsciiStyle::Y2k);
    }

    #[test]
    fn rave_config_defaults() {
        let cfg = RaveConfig::default();
        assert!(cfg.visualizer);
        assert_eq!(cfg.shake, ShakeLevel::Subtle);
        assert!(cfg.streaks);
        assert_eq!(cfg.intensity, FxIntensity::Rave);
        assert_eq!(cfg.ascii_style, AsciiStyle::Y2k);
    }
}
