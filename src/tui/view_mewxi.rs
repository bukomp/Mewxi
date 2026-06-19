//! View 5 — full-screen Mewxi splash with minified account + agent data.
//!
//! Left: largest Mewxi ASCII art that fits, rendered in a purple scale.
//! Right: stacked condensed panels —
//!   - Accounts: name + 5h / weekly / extra mini-bars with percentages.
//!   - Agents: per-session state, status, ctx%.

use super::{LOGO_LARGE, LOGO_LARGE_DIMS, LOGO_MEDIUM, LOGO_MEDIUM_DIMS, LOGO_SMALL,
    LOGO_SMALL_DIMS, LOGO_TINY, LOGO_TINY_DIMS, PerAccount, SessionRef};
use crate::live_session::{Activity, SessionState};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

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

/// Purple scale used across this view. From dim → bright.
/// Picked from the 256-colour palette so it renders without truecolor.
const P_DIM: Color = Color::Indexed(54);   // dark purple
const P_LOW: Color = Color::Indexed(97);   // muted purple
const P_MID: Color = Color::Indexed(135);  // medium purple
const P_HIGH: Color = Color::Indexed(171); // bright purple
const P_HOT: Color = Color::Indexed(207);  // hot pink-purple
const P_TEXT: Color = Color::Indexed(183); // light lavender (body text)
const P_LABEL: Color = Color::Indexed(141);

/// Gauge fill colour in the purple scale, hotter as utilisation climbs.
fn purple_gauge(pct: f64) -> Color {
    if pct >= 90.0 {
        P_HOT
    } else if pct >= 70.0 {
        P_HIGH
    } else if pct >= 40.0 {
        P_MID
    } else {
        P_LOW
    }
}

pub fn render(
    f: &mut Frame,
    area: Rect,
    accounts: &[&PerAccount],
    sessions: &[&SessionRef],
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(P_MID))
        .title(Span::styled(
            " Mewxi ",
            Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    let active = sessions
        .iter()
        .filter(|s| s.state == SessionState::Active)
        .count();
    render_logo(f, cols[0], active);
    render_side_panel(f, cols[1], accounts, sessions);

    // Cosmetic "under construction" hazard band. Drawn last so it sits
    // on top of the logo + panels, but it's render-only — the view
    // underneath keeps updating and stays fully interactive.
    super::under_construction::render(f, area);
}

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

fn render_side_panel(
    f: &mut Frame,
    area: Rect,
    accounts: &[&PerAccount],
    sessions: &[&SessionRef],
) {
    // Each account block: 1 header + 3 gauge rows = 4 lines. Cap so we
    // always leave room for the agents panel below.
    let acct_block_lines: u16 = 4;
    let want_acct = (acct_block_lines * accounts.len().max(1) as u16) + 2;
    let max_acct = area.height.saturating_sub(6);
    let acct_h = want_acct.min(max_acct).max(5);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(acct_h), Constraint::Min(4)])
        .split(area);

    render_accounts(f, rows[0], accounts);
    render_agents(f, rows[1], sessions);
}

fn render_accounts(f: &mut Frame, area: Rect, accounts: &[&PerAccount]) {
    let title = Span::styled(
        format!(" accounts ({}) ", accounts.len()),
        Style::default().fg(P_HIGH).add_modifier(Modifier::BOLD),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(P_LOW))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if accounts.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no accounts",
            Style::default().fg(P_DIM),
        )));
        f.render_widget(p, inner);
        return;
    }

    let constraints: Vec<Constraint> = accounts
        .iter()
        .map(|_| Constraint::Length(4))
        .collect();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, pa) in accounts.iter().enumerate() {
        if i >= chunks.len() {
            break;
        }
        render_account(f, chunks[i], pa);
    }
}

fn render_account(f: &mut Frame, area: Rect, pa: &PerAccount) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let active = pa
        .live_sessions
        .iter()
        .filter(|s| s.state == SessionState::Active)
        .count();
    let header = Line::from(vec![
        Span::styled(
            format!("[{}]", pa.account.name),
            Style::default().fg(P_HOT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{active} live"),
            Style::default().fg(if active > 0 { P_HIGH } else { P_DIM }),
        ),
        Span::raw("  "),
        Span::styled(
            format!("${:.2}", pa.agg.all.cost_usd),
            Style::default().fg(P_LABEL),
        ),
    ]);
    f.render_widget(Paragraph::new(header), rows[0]);

    let live = pa.live.as_ref();
    let five_h = live.and_then(|l| l.five_hour.as_ref()).map(|w| w.utilization);
    let weekly = live.and_then(|l| l.seven_day.as_ref()).map(|w| w.utilization);
    let extra = live
        .and_then(|l| l.extra_usage.as_ref())
        .filter(|e| e.is_enabled)
        .and_then(|e| e.utilization);

    render_mini_gauge(f, rows[1], "5h", five_h);
    render_mini_gauge(f, rows[2], "wk", weekly);
    render_mini_gauge(f, rows[3], "ex", extra);
}

fn render_mini_gauge(f: &mut Frame, area: Rect, label: &str, pct: Option<f64>) {
    const LABEL_W: u16 = 5;
    const PCT_W: u16 = 7;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(LABEL_W),
            Constraint::Min(4),
            Constraint::Length(PCT_W),
        ])
        .split(area);

    let label_p = Paragraph::new(Line::from(Span::styled(
        format!(" {label}"),
        Style::default().fg(P_LABEL),
    )));
    f.render_widget(label_p, cols[0]);

    match pct {
        Some(p) => {
            let ratio = (p / 100.0).clamp(0.0, 1.0);
            let color = purple_gauge(p);
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(color).bg(Color::Indexed(53)))
                .ratio(ratio)
                .label("");
            f.render_widget(gauge, cols[1]);
            let pct_p = Paragraph::new(Line::from(Span::styled(
                format!("{p:>5.1}% "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
            f.render_widget(pct_p, cols[2]);
        }
        None => {
            let dim = Paragraph::new(Line::from(Span::styled(
                "─".repeat(cols[1].width as usize),
                Style::default().fg(P_DIM),
            )));
            f.render_widget(dim, cols[1]);
            let pct_p = Paragraph::new(Line::from(Span::styled(
                "  n/a  ",
                Style::default().fg(P_DIM),
            )));
            f.render_widget(pct_p, cols[2]);
        }
    }
}

fn render_agents(f: &mut Frame, area: Rect, sessions: &[&SessionRef]) {
    let active = sessions
        .iter()
        .filter(|s| s.state == SessionState::Active)
        .count();
    let idle = sessions.len() - active;
    let title = Span::styled(
        format!(" agents · {active} active · {idle} idle "),
        Style::default().fg(P_HIGH).add_modifier(Modifier::BOLD),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(P_LOW))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if sessions.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no agents running",
            Style::default().fg(P_DIM),
        )));
        f.render_widget(p, inner);
        return;
    }

    let visible = (inner.height as usize).min(sessions.len());
    let lines: Vec<Line> = sessions
        .iter()
        .take(visible)
        .map(|s| agent_line(s))
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn agent_line(s: &SessionRef) -> Line<'static> {
    let state_label = match s.state {
        SessionState::Active => "active",
        SessionState::Idle => "idle",
    };
    let state_color = match s.state {
        SessionState::Active => P_HOT,
        SessionState::Idle => P_DIM,
    };
    let (status, status_color) = activity_purple(&s.activity);
    let ctx = fmt_ctx(s.current_context, s.context_cap);
    let ctx_color = ctx_color(s.current_context, s.context_cap);

    Line::from(vec![
        Span::styled(
            format!(" {:<10} ", trim_to(&s.account_name, 10)),
            Style::default().fg(P_LABEL),
        ),
        Span::styled(
            format!("{:<7}", state_label),
            Style::default().fg(state_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<11}", status),
            Style::default().fg(status_color),
        ),
        Span::styled(
            format!("ctx {:>4}", ctx),
            Style::default().fg(ctx_color),
        ),
        Span::raw("  "),
        Span::styled(trim_to(&s.project, 18), Style::default().fg(P_TEXT)),
    ])
}

fn activity_purple(a: &Activity) -> (String, Color) {
    let color = match a {
        Activity::Waiting => P_DIM,
        Activity::Awaiting => P_HOT,
        Activity::Asking => P_HOT,
        Activity::Thinking | Activity::Starting => P_MID,
        Activity::Writing | Activity::Editing => P_HIGH,
        Activity::Reading | Activity::Searching | Activity::Fetching => P_LOW,
        Activity::Running | Activity::Delegating => P_HIGH,
        Activity::Compacting => P_HIGH,
        Activity::Tool(_) => P_TEXT,
    };
    (a.label(), color)
}

fn fmt_ctx(current: Option<u64>, cap: Option<u64>) -> String {
    match (current, cap) {
        (Some(c), Some(cap)) if cap > 0 => {
            let pct = (c as f64 / cap as f64 * 100.0).round() as u32;
            format!("{pct}%")
        }
        _ => "—".into(),
    }
}

fn ctx_color(current: Option<u64>, cap: Option<u64>) -> Color {
    match (current, cap) {
        (Some(c), Some(cap)) if cap > 0 => {
            let pct = c as f64 / cap as f64 * 100.0;
            purple_gauge(pct)
        }
        _ => P_DIM,
    }
}

fn trim_to(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
