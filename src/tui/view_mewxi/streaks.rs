//! Arcade-style gamification HUD ("streaks") for view 5's rave view.
//!
//! Everything here is tuned to reward **parallel** productivity — one
//! agent grinding alone keeps the lights on, but the numbers only get
//! interesting when work fans out:
//!
//! - COMBO — how many agents are working *right now*, sub-agents
//!   included. This is the score multiplier.
//! - STREAK — how long (seconds) at least [`PARALLEL_MIN`] agents have
//!   been working in parallel, continuously. Solo time doesn't build
//!   it; a grace window absorbs brief dips (a worker finishing before
//!   the next wave spawns) so orchestration gaps don't reset it.
//! - SCORE — the current *run*'s score. Each working agent earns a
//!   point per second **times the combo**, so `n` parallel agents score
//!   `n²`/s — four agents earn 16× one agent. Active STREAK tiers add
//!   +25% each on top. A run ends after [`GRACE_SECS`] with zero agents
//!   working: the score banks into BEST and resets, arcade style.
//! - BEST — the high score: the best run score ever, persisted across
//!   restarts and sessions. Shown live — once the current run overtakes
//!   it, BEST climbs along.
//! - MILESTONE — fires the frame a STREAK tier is crossed, COMBO hits a
//!   new all-time high, or the current run first beats the high score,
//!   so the root can flash/shake the panel.
//! - HISTORY — every run that banks is recorded (score, peak combo, peak
//!   streak, end time) into a capped, cross-session list exposed via
//!   [`history`], for the score-board modal's run list.
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

/// Agents that must be working at once for the STREAK to count —
/// parallel means at least two.
const PARALLEL_MIN: usize = 2;

/// Seconds below [`PARALLEL_MIN`] tolerated before STREAK resets.
/// Orchestrated runs breathe — a wave of workers returns before the
/// manager fans out the next — so this is deliberately roomier than a
/// plain idle-gap debounce.
const GRACE_SECS: f64 = 15.0;

/// Score scale factor — combo-weighted agent-seconds × this, truncated
/// to an integer, so the number climbs at an arcade-y pace instead of
/// reading like raw seconds.
const SCORE_SCALE: f64 = 100.0;

/// Extra score multiplier per active STREAK tier (tier 2 ⇒ ×1.5).
const TIER_SCORE_BONUS: f64 = 0.25;

/// STREAK tier thresholds in seconds. `streak_tier` returns the index of
/// the highest threshold met (0 = below the first one). Parallel time is
/// scarcer than mere uptime, so the ladder starts lower than a
/// wall-clock one would.
const STREAK_TIERS: &[f64] = &[30.0, 120.0, 300.0, 900.0, 1800.0];

/// How long a milestone flash takes to decay back to 0, in seconds.
const FLASH_DECAY_SECS: f64 = 1.2;

/// Public per-frame snapshot handed to the renderer. Cheap to copy.
#[derive(Clone, Copy, Debug)]
pub struct StreakHud {
    /// Agents working right now, sub-agents included.
    pub combo: usize,
    /// Continuous parallel (≥[`PARALLEL_MIN`]-active) time, in seconds
    /// (grace-extended).
    pub streak_secs: f64,
    /// The high score — the best run score this session, tracking the
    /// current run live once it overtakes the banked best.
    pub best_score: u64,
    /// The current run's combo-weighted score, scaled for arcade
    /// flavour. Resets when a run ends (all-idle past grace).
    pub score: u64,
    /// True on the exact frame a tier-up or combo-high fired.
    pub milestone: bool,
    /// 0..1 flash intensity; peaks on a milestone frame, decays after.
    pub flash: f64,
}

/// A run the pure `advance` layer has just banked, minus its wall-clock
/// end time — `tick` stamps `ended_at` when it drains this buffer.
struct CompletedRun {
    score: u64,
    peak_combo: usize,
    peak_streak_secs: f64,
}

/// Core mutable state advanced once per frame. Kept separate from
/// `StreakHud` so the pure logic never has to know about `Instant` —
/// `advance` takes an already-computed `dt`.
struct StreakCore {
    /// Agents working as of the last [`advance`] call, sub-agents
    /// included. Tracked on the core (not just the returned `StreakHud`)
    /// so a non-mutating [`snapshot`] can read the live combo too.
    combo: usize,
    streak_secs: f64,
    /// The current run's raw combo-weighted score;
    /// `score = (score_acc * SCORE_SCALE) as u64`.
    score_acc: f64,
    /// Best banked run score so far (same raw unit as `score_acc`).
    best_score_acc: f64,
    /// Seconds of below-parallel grace remaining; resets to `GRACE_SECS`
    /// whenever `active >= PARALLEL_MIN`, counts down to 0 otherwise.
    grace_left: f64,
    /// Seconds of all-idle grace remaining before the run ends and the
    /// score banks. Separate from `grace_left`: the streak cares about
    /// dips below parallel, the run only about everyone going quiet.
    idle_grace_left: f64,
    /// True once this run has beaten the banked high score, so the
    /// crossing only fires one milestone per run.
    beat_high_this_run: bool,
    best_combo: usize,
    /// Highest `streak_tier` value already fired this run, so climbing
    /// through several thresholds in one frame only counts once (and
    /// dropping back down doesn't refire the same tier).
    last_milestone_tier: usize,
    flash: f64,
    /// This run's highest combo seen so far, reset when the run banks.
    run_peak_combo: usize,
    /// This run's longest streak seen so far, reset when the run banks.
    run_peak_streak_secs: f64,
    /// Runs banked since the last drain — `tick` drains and stamps these
    /// into `StreakState::history`. Kept here (not returned from
    /// `advance`) so `advance` can stay a plain `bool`-returning fn while
    /// still being fully testable: after a bank, this holds the run.
    completed: Vec<CompletedRun>,
}

impl StreakCore {
    fn new() -> Self {
        StreakCore {
            combo: 0,
            streak_secs: 0.0,
            score_acc: 0.0,
            best_score_acc: 0.0,
            grace_left: 0.0,
            idle_grace_left: 0.0,
            beat_high_this_run: false,
            best_combo: 0,
            last_milestone_tier: 0,
            flash: 0.0,
            run_peak_combo: 0,
            run_peak_streak_secs: 0.0,
            completed: Vec::new(),
        }
    }
}

/// Wall-clock wrapper state — just the last-tick `Instant` plus the core.
/// Split out so `advance` (the part under test) never touches `Instant`.
struct StreakState {
    core: StreakCore,
    last_tick: Instant,
    /// Last time the scores file was written, for debouncing.
    last_write: Instant,
    /// The `(score, combo, best_score)` triple as of the last write, so
    /// the debounce can tell whether there's anything new to save.
    saved: SavedMarks,
    /// Completed runs, newest first, capped at [`HISTORY_CAP`]. Seeded
    /// from the loaded scores file at `STATE` init, appended to as runs
    /// bank in [`tick`].
    history: Vec<RunEntry>,
}

/// The subset of `StreakCore` state that determines whether a save is
/// worth doing — cheap to compare each tick.
#[derive(Clone, Copy, PartialEq)]
struct SavedMarks {
    score: u64,
    combo: u64,
    best_score: u64,
}

/// Snapshot the fields `SavedMarks` compares, from the current core.
fn marks_of(core: &StreakCore) -> SavedMarks {
    SavedMarks {
        score: (core.score_acc * SCORE_SCALE) as u64,
        combo: core.combo as u64,
        best_score: (core.best_score_acc.max(core.score_acc) * SCORE_SCALE) as u64,
    }
}

/// Build the on-disk scores file from the live core and its history.
/// `updated_at` is left empty — `score_store::save` stamps it at write
/// time.
fn scores_file_of(core: &StreakCore, history: &[RunEntry]) -> super::score_store::ScoresFile {
    super::score_store::ScoresFile {
        best_score: (core.best_score_acc.max(core.score_acc) * SCORE_SCALE) as u64,
        best_combo: core.best_combo as u64,
        current_score: (core.score_acc * SCORE_SCALE) as u64,
        current_combo: core.combo as u64,
        current_streak_secs: core.streak_secs,
        updated_at: String::new(),
        history: history.iter().map(record_from_entry).collect(),
    }
}

/// Seed a fresh [`StreakCore`] from a previously loaded scores file —
/// pure and testable, unlike the `tick`-layer load itself.
fn seeded_core(file: &super::score_store::ScoresFile) -> StreakCore {
    let mut core = StreakCore::new();
    core.best_score_acc = file.best_score as f64 / SCORE_SCALE;
    core.best_combo = file.best_combo as usize;
    core
}

/// How long to wait between non-milestone score-file writes, so a busy
/// session doesn't hammer disk every frame.
const SCORE_WRITE_DEBOUNCE_SECS: f64 = 5.0;

static STATE: OnceLock<Mutex<StreakState>> = OnceLock::new();

/// Highest tier index whose threshold `secs` has reached, `0` if below
/// the first threshold. `29 → 0`, `30 → 1`, `120 → 2`, `300 → 3`,
/// `900 → 4`, `1800 → 5`.
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

/// Advance `core` by `dt` seconds given this frame's working-agent count
/// (sub-agents included). Pure and deterministic — no wall-clock reads —
/// so it's fully unit testable. Returns `true` iff a milestone (streak
/// tier-up or new combo high) fired this step.
///
/// `dt` is expected to already be clamped by the caller (the `tick`
/// wrapper clamps to 0.1s, matching the crate's `tick_anim` pattern).
fn advance(core: &mut StreakCore, active: usize, dt: f64) -> bool {
    let dt = dt.max(0.0);
    core.combo = active;

    core.run_peak_combo = core.run_peak_combo.max(core.combo);

    if active >= PARALLEL_MIN {
        core.grace_left = GRACE_SECS;
        core.streak_secs += dt;
    } else if core.grace_left > 0.0 {
        // Below parallel but inside the grace window: streak keeps
        // counting — a wave of workers just returned and the next one
        // could fan out any moment.
        core.grace_left = (core.grace_left - dt).max(0.0);
        core.streak_secs += dt;
    } else {
        core.streak_secs = 0.0;
    }

    core.run_peak_streak_secs = core.run_peak_streak_secs.max(core.streak_secs);

    // Every working agent earns a point per second times the combo, so
    // `n` parallel agents accrue `n²`/s — the quadratic is the point:
    // parallel work is what scores. Active streak tiers sweeten it
    // further. Solo work still earns its 1/s. A run ends once everyone
    // has been idle past grace: the score banks into the high score and
    // resets, arcade style.
    let mut beat_high = false;
    if active > 0 {
        core.idle_grace_left = GRACE_SECS;
        let tier_bonus = 1.0 + TIER_SCORE_BONUS * streak_tier(core.streak_secs) as f64;
        core.score_acc += (active * active) as f64 * tier_bonus * dt;
        if !core.beat_high_this_run
            && core.best_score_acc > 0.0
            && core.score_acc > core.best_score_acc
        {
            core.beat_high_this_run = true;
            beat_high = true;
        }
    } else if core.idle_grace_left > 0.0 {
        core.idle_grace_left = (core.idle_grace_left - dt).max(0.0);
    } else if core.score_acc > 0.0 {
        core.best_score_acc = core.best_score_acc.max(core.score_acc);
        core.completed.push(CompletedRun {
            score: (core.score_acc * SCORE_SCALE) as u64,
            peak_combo: core.run_peak_combo,
            peak_streak_secs: core.run_peak_streak_secs,
        });
        core.score_acc = 0.0;
        core.beat_high_this_run = false;
        core.run_peak_combo = 0;
        core.run_peak_streak_secs = 0.0;
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

    let milestone = tier_up || combo_high || beat_high;
    if milestone {
        core.flash = 1.0;
    } else if core.flash > 0.0 {
        let decay = if FLASH_DECAY_SECS > 0.0 { dt / FLASH_DECAY_SECS } else { 1.0 };
        core.flash = (core.flash - decay).max(0.0);
    }

    milestone
}

/// Advance the global streak state for this frame's working-agent count
/// (sub-agents included) and return the HUD snapshot. `dt` is derived
/// from an internal
/// `Instant`, clamped to 0.1s so a backgrounded terminal can't cause a
/// giant score/streak jump on the next visible frame — mirrors
/// `tick_anim` in `view_mewxi.rs`.
pub fn tick(active: usize) -> StreakHud {
    let cell = STATE.get_or_init(|| {
        let file = super::score_store::load();
        let core = seeded_core(&file);
        let saved = marks_of(&core);
        let history = history_from_records(&file.history);
        Mutex::new(StreakState {
            core,
            last_tick: Instant::now(),
            last_write: Instant::now(),
            saved,
            history,
        })
    });
    let mut s = cell.lock().expect("streak state poisoned");
    let now = Instant::now();
    let dt = (now - s.last_tick).as_secs_f64().min(0.1);
    s.last_tick = now;

    let milestone = advance(&mut s.core, active, dt);

    let hud = StreakHud {
        combo: active,
        streak_secs: s.core.streak_secs,
        best_score: (s.core.best_score_acc.max(s.core.score_acc) * SCORE_SCALE) as u64,
        score: (s.core.score_acc * SCORE_SCALE) as u64,
        milestone,
        flash: s.core.flash,
    };

    // Drain any runs `advance` just banked, stamping the wall-clock end
    // time here (the pure core never touches it), newest first. Any
    // bank — best-beating or not — forces an immediate save below, so
    // a history entry never sits only in memory for the debounce window.
    let finished = std::mem::take(&mut s.core.completed);
    let banked = !finished.is_empty();
    if banked {
        let ended_at = chrono::Utc::now().to_rfc3339();
        for run in finished {
            s.history.insert(
                0,
                RunEntry {
                    score: run.score,
                    peak_combo: run.peak_combo,
                    peak_streak_secs: run.peak_streak_secs,
                    ended_at: ended_at.clone(),
                },
            );
        }
        s.history.truncate(HISTORY_CAP);
    }

    let marks = marks_of(&s.core);
    let due = (now - s.last_write).as_secs_f64() >= SCORE_WRITE_DEBOUNCE_SECS;
    if milestone || banked || (marks != s.saved && due) {
        let file = scores_file_of(&s.core, &s.history);
        super::score_store::save(&file);
        s.last_write = now;
        s.saved = marks;
    }

    hud
}

/// Read-only snapshot of the arcade score state, for the `s` score
/// modal and the status-file writer. Unlike [`tick`], this never
/// advances the clock or mutates anything — safe to call any number
/// of times per frame, from any view.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScoreSnapshot {
    /// Agents working as of the last [`tick`].
    pub combo: usize,
    /// Continuous parallel time as of the last [`tick`], in seconds.
    pub streak_secs: f64,
    /// The current run's score.
    pub score: u64,
    /// The high score — the best run score, persisted across sessions.
    pub best_score: u64,
    /// The best combo ever seen, persisted across sessions.
    pub best_combo: usize,
}

/// Build a [`ScoreSnapshot`] from a core — pure, so it's shared between
/// the mutating [`tick`] path (indirectly, via `snapshot`) and any future
/// caller that already holds a core.
fn snapshot_of(core: &StreakCore) -> ScoreSnapshot {
    ScoreSnapshot {
        combo: core.combo,
        streak_secs: core.streak_secs,
        score: (core.score_acc * SCORE_SCALE) as u64,
        best_score: (core.best_score_acc.max(core.score_acc) * SCORE_SCALE) as u64,
        best_combo: core.best_combo,
    }
}

/// Non-mutating read of the global score state. Returns zeros if
/// [`tick`] has never run.
pub fn snapshot() -> ScoreSnapshot {
    let Some(cell) = STATE.get() else { return ScoreSnapshot::default() };
    let s = cell.lock().expect("streak state poisoned");
    snapshot_of(&s.core)
}

/// Persist the score state to the status file immediately, bypassing
/// any debounce. The event loop calls this once on shutdown.
pub fn flush_scores() {
    let Some(cell) = STATE.get() else { return };
    let mut s = cell.lock().expect("streak state poisoned");
    let file = scores_file_of(&s.core, &s.history);
    super::score_store::save(&file);
    let now = Instant::now();
    s.last_write = now;
    s.saved = marks_of(&s.core);
}

/// One completed (banked) run, for the score-board modal's history
/// list and the status file.
#[derive(Clone, Debug)]
pub struct RunEntry {
    /// The run's final banked score.
    pub score: u64,
    /// Highest combo reached during the run.
    pub peak_combo: usize,
    /// Longest parallel streak reached during the run, in seconds.
    pub peak_streak_secs: f64,
    /// When the run banked — RFC3339 UTC; empty when unknown (entries
    /// from files written before history existed).
    pub ended_at: String,
}

/// Convert a persisted [`super::score_store::RunRecord`] into a
/// [`RunEntry`]. Pure and the inverse of [`record_from_entry`].
fn entry_from_record(r: &super::score_store::RunRecord) -> RunEntry {
    RunEntry {
        score: r.score,
        peak_combo: r.peak_combo as usize,
        peak_streak_secs: r.peak_streak_secs,
        ended_at: r.ended_at.clone(),
    }
}

/// Convert a [`RunEntry`] into the persisted
/// [`super::score_store::RunRecord`] shape. Pure and the inverse of
/// [`entry_from_record`].
fn record_from_entry(e: &RunEntry) -> super::score_store::RunRecord {
    super::score_store::RunRecord {
        score: e.score,
        peak_combo: e.peak_combo as u64,
        peak_streak_secs: e.peak_streak_secs,
        ended_at: e.ended_at.clone(),
    }
}

/// Convert a loaded file's `history` (newest-first `RunRecord`s) into
/// in-memory `RunEntry`s, defensively capped at [`HISTORY_CAP`] in case
/// an on-disk file was ever hand-edited or written by a future version
/// with a looser cap.
fn history_from_records(records: &[super::score_store::RunRecord]) -> Vec<RunEntry> {
    let mut entries: Vec<RunEntry> = records.iter().map(entry_from_record).collect();
    entries.truncate(HISTORY_CAP);
    entries
}

/// Completed runs, newest first, capped at [`HISTORY_CAP`]. Spans
/// sessions — persisted entries load with the rest of the score state.
/// Non-mutating and IO-free per call: it reads the in-memory list seeded
/// from disk at [`tick`]'s first call, not the file itself.
pub fn history() -> Vec<RunEntry> {
    let Some(cell) = STATE.get() else { return Vec::new() };
    let s = cell.lock().expect("streak state poisoned");
    s.history.clone()
}

/// Most completed runs kept in memory and in the status file.
pub const HISTORY_CAP: usize = 50;

/// Location of the scores status file:
/// `$XDG_STATE_HOME/mewxi/scores.json`, defaulting to
/// `~/.local/state/mewxi/scores.json`. XDG-style on every platform,
/// mirroring the `accounts.toml` rationale in `accounts::config_path`.
pub fn scores_file_path() -> Option<std::path::PathBuf> {
    super::score_store::scores_file_path()
}

/// Format a seconds count as `m:ss`. Shared with the score-board modal
/// so the two never drift on formatting.
pub(super) fn fmt_mmss(secs: f64) -> String {
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
        ("BEST", fmt_score(hud.best_score)),
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
        Span::styled(fmt_score(hud.best_score), Style::default().fg(P_DIM)),
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

    fn core_with_parallel_streak(secs: f64) -> StreakCore {
        let mut core = StreakCore::new();
        let mut remaining = secs;
        while remaining > 0.0 {
            let step = remaining.min(0.05);
            advance(&mut core, PARALLEL_MIN, step);
            remaining -= step;
        }
        core
    }

    #[test]
    fn fmt_mmss_formats_minutes_and_seconds() {
        assert_eq!(fmt_mmss(0.0), "0:00");
        assert_eq!(fmt_mmss(59.4), "0:59");
        assert_eq!(fmt_mmss(65.0), "1:05");
        assert_eq!(fmt_mmss(-3.0), "0:00");
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
            best_score: 0,
            score: 0,
            milestone: false,
            flash: 0.0,
        };
        let big = StreakHud {
            combo: 12,
            streak_secs: 3600.0,
            best_score: 9_876_543,
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
            best_score: 260_000,
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
            best_score: 100,
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
        assert_eq!(streak_tier(29.0), 0);
        assert_eq!(streak_tier(30.0), 1);
        assert_eq!(streak_tier(119.999), 1);
        assert_eq!(streak_tier(120.0), 2);
        assert_eq!(streak_tier(299.999), 2);
        assert_eq!(streak_tier(300.0), 3);
        assert_eq!(streak_tier(900.0), 4);
        assert_eq!(streak_tier(1800.0), 5);
    }

    #[test]
    fn solo_work_earns_score_but_builds_no_streak() {
        let mut core = StreakCore::new();
        for _ in 0..100 {
            advance(&mut core, 1, 0.1); // 10s of one lone agent
        }
        assert_eq!(core.streak_secs, 0.0, "solo time must not build the parallel streak");
        assert!(core.score_acc > 0.0, "solo time still earns base score");
    }

    #[test]
    fn grace_bridges_a_dip_below_parallel() {
        let mut core = StreakCore::new();
        // 2 agents in parallel for 2s.
        for _ in 0..20 {
            advance(&mut core, PARALLEL_MIN, 0.1);
        }
        let streak_before_dip = core.streak_secs;
        assert!((streak_before_dip - 2.0).abs() < 1e-9);

        // Dip to a single agent for 10s (< GRACE_SECS) — the wave gap.
        // Streak should keep climbing through the grace window.
        for _ in 0..100 {
            advance(&mut core, 1, 0.1);
        }
        assert!(
            core.streak_secs > streak_before_dip,
            "streak should keep counting during grace: {}",
            core.streak_secs
        );

        // Fan out again within grace — no reset happened.
        advance(&mut core, PARALLEL_MIN, 0.1);
        assert!(core.streak_secs > 11.9);
    }

    #[test]
    fn streak_resets_after_grace_expires() {
        let mut core = StreakCore::new();
        for _ in 0..20 {
            advance(&mut core, PARALLEL_MIN, 0.1); // 2s parallel
        }
        assert!(core.streak_secs > 0.0);

        // Solo for longer than GRACE_SECS (15s) continuously — below
        // parallel counts the grace down even though one agent still
        // works.
        for _ in 0..170 {
            advance(&mut core, 1, 0.1); // 17s solo total
        }
        assert_eq!(core.streak_secs, 0.0);

        // One more solo step confirms it stays at zero, doesn't go
        // negative or bounce.
        advance(&mut core, 1, 0.1);
        assert_eq!(core.streak_secs, 0.0);
    }

    #[test]
    fn milestone_fires_once_at_30s_not_again_until_120s() {
        let mut core = StreakCore::new();

        // The very first parallel step is itself a milestone: combo goes
        // from 0 (no agents ever seen) to a new all-time high.
        // Consume that one separately so it doesn't get confused with
        // the streak-tier milestones this test is about.
        assert!(
            advance(&mut core, PARALLEL_MIN, 0.1),
            "first-ever combo should fire a new-high milestone"
        );
        let mut elapsed = 0.1;

        let mut fired_at_30 = false;
        let mut fire_count_before_120 = 0;

        // Step in small increments from just after start to just past
        // 30s. Combo stays pinned the whole time, so no further
        // combo-high milestones can fire here — only the streak tier-up.
        while elapsed < 31.0 {
            let fired = advance(&mut core, PARALLEL_MIN, 0.1);
            elapsed += 0.1;
            if fired && core.streak_secs >= 30.0 && core.streak_secs < 30.2 {
                fired_at_30 = true;
            }
            if fired {
                fire_count_before_120 += 1;
            }
        }
        assert!(fired_at_30, "expected a milestone right at the 30s crossing");
        // Only the 30s tier-up should have fired in this window.
        assert_eq!(fire_count_before_120, 1);

        // Continue up to 120s — expect exactly one more tier-up fire.
        let mut fire_count_to_120 = 0;
        while elapsed < 120.5 {
            let fired = advance(&mut core, PARALLEL_MIN, 0.1);
            elapsed += 0.1;
            if fired {
                fire_count_to_120 += 1;
            }
        }
        assert_eq!(fire_count_to_120, 1, "expected exactly one fire crossing 120s");
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
    fn score_is_monotonic_within_a_run() {
        let mut core = StreakCore::new();
        let mut last_score = 0.0;
        // Idle flickers stay well inside the idle grace, so this is all
        // one run — the score may only climb.
        for i in 0..50 {
            let active = if i % 3 == 0 { 0 } else { 2 };
            advance(&mut core, active, 0.1);
            let score = core.score_acc * SCORE_SCALE;
            assert!(score >= last_score, "score must never decrease within a run");
            last_score = score;
        }
        assert!(last_score > 0.0, "score should have grown while active");
    }

    #[test]
    fn parallel_work_scores_quadratically() {
        let mut duo = StreakCore::new();
        let mut squad = StreakCore::new();
        // Same wall-clock time, 10s each — under the first streak tier
        // so no tier bonus muddies the ratio.
        for _ in 0..100 {
            advance(&mut duo, 2, 0.1);
            advance(&mut squad, 4, 0.1);
        }
        // 4 agents ⇒ 16/s vs 2 agents ⇒ 4/s: twice the agents, four
        // times the score.
        let ratio = squad.score_acc / duo.score_acc;
        assert!((ratio - 4.0).abs() < 1e-6, "expected 4x score ratio, got {ratio}");
    }

    #[test]
    fn streak_tier_boosts_score_rate() {
        // Build a tier-1 streak (≥30s), then compare one further second
        // of accrual against a fresh tier-0 core at the same combo.
        let mut hot = core_with_parallel_streak(31.0);
        let mut cold = StreakCore::new();
        let hot_before = hot.score_acc;
        for _ in 0..10 {
            advance(&mut hot, 2, 0.1);
            advance(&mut cold, 2, 0.1);
        }
        let hot_gain = hot.score_acc - hot_before;
        let cold_gain = cold.score_acc;
        assert!(
            hot_gain > cold_gain * 1.2,
            "tier bonus should outpace tier 0: {hot_gain} vs {cold_gain}"
        );
    }

    #[test]
    fn score_banks_into_best_when_the_run_ends() {
        let mut core = StreakCore::new();
        for _ in 0..50 {
            advance(&mut core, 2, 0.1); // 5s parallel run
        }
        let run_score = core.score_acc;
        assert!(run_score > 0.0);

        // Everyone idle past grace: the run is over — score resets, the
        // high score keeps it.
        for _ in 0..170 {
            advance(&mut core, 0, 0.1); // 17s all-idle
        }
        assert_eq!(core.score_acc, 0.0, "score should reset when the run ends");
        assert!(
            (core.best_score_acc - run_score).abs() < 1e-9,
            "the run's score should bank into best"
        );

        // A weaker second run must not lower the banked best.
        for _ in 0..10 {
            advance(&mut core, 2, 0.1); // 1s only
        }
        for _ in 0..170 {
            advance(&mut core, 0, 0.1);
        }
        assert!((core.best_score_acc - run_score).abs() < 1e-9);
    }

    #[test]
    fn beating_the_high_score_fires_one_milestone() {
        let mut core = StreakCore::new();
        // Run 1: the first step's combo high is consumed here; 5s at
        // 2-wide banks a score of 2² × 5 = 20.
        for _ in 0..50 {
            advance(&mut core, 2, 0.1);
        }
        for _ in 0..170 {
            advance(&mut core, 0, 0.1); // run ends, score banks
        }
        assert!(core.best_score_acc > 0.0);

        // Run 2 at the same width: no combo high, no streak tier before
        // 30s — the only milestone in the first 10s is the moment the
        // score overtakes the banked best (just past the 5s mark), and
        // it fires exactly once.
        let mut fires = 0;
        for _ in 0..100 {
            if advance(&mut core, 2, 0.1) {
                fires += 1;
            }
        }
        assert_eq!(fires, 1, "expected exactly one high-score milestone");
        assert!(core.score_acc > core.best_score_acc);
    }

    #[test]
    fn seeded_core_restores_best_from_file() {
        let file = super::super::score_store::ScoresFile {
            best_score: 5000,
            best_combo: 7,
            ..Default::default()
        };
        let core = seeded_core(&file);
        assert!((core.best_score_acc - 50.0).abs() < 1e-9);
        assert_eq!(core.best_combo, 7);
    }

    #[test]
    fn snapshot_reflects_live_combo_after_advance() {
        let mut core = StreakCore::new();
        advance(&mut core, 4, 0.1);
        assert_eq!(snapshot_of(&core).combo, 4);
    }

    #[test]
    fn banking_records_a_completed_run() {
        let mut core = StreakCore::new();
        for _ in 0..50 {
            advance(&mut core, 2, 0.1); // 5s parallel run
        }
        let run_score = (core.score_acc * SCORE_SCALE) as u64;
        assert!(run_score > 0);

        for _ in 0..170 {
            advance(&mut core, 0, 0.1); // 17s all-idle: run ends, banks
        }

        assert_eq!(core.completed.len(), 1);
        let run = &core.completed[0];
        assert_eq!(run.score, run_score);
        assert_eq!(run.peak_combo, 2);
        assert!(run.peak_streak_secs >= 0.0);
    }

    #[test]
    fn peaks_reset_for_the_next_run() {
        let mut core = StreakCore::new();
        // Run 1: 2-wide.
        for _ in 0..50 {
            advance(&mut core, 2, 0.1);
        }
        for _ in 0..170 {
            advance(&mut core, 0, 0.1);
        }
        assert_eq!(core.completed.len(), 1);
        assert_eq!(core.completed[0].peak_combo, 2);

        // Run 2: wider, 3-wide — peaks must not carry over from run 1.
        for _ in 0..50 {
            advance(&mut core, 3, 0.1);
        }
        for _ in 0..170 {
            advance(&mut core, 0, 0.1);
        }
        assert_eq!(core.completed.len(), 2);
        assert_eq!(core.completed[1].peak_combo, 3);
    }

    #[test]
    fn peak_combo_tracks_the_max_within_a_run() {
        let mut core = StreakCore::new();
        for _ in 0..20 {
            advance(&mut core, 2, 0.1);
        }
        for _ in 0..20 {
            advance(&mut core, 4, 0.1);
        }
        for _ in 0..20 {
            advance(&mut core, 1, 0.1);
        }
        // Bank without ever dropping to 0 combo mid-run.
        for _ in 0..170 {
            advance(&mut core, 0, 0.1);
        }
        assert_eq!(core.completed.len(), 1);
        assert_eq!(core.completed[0].peak_combo, 4);
    }

    #[test]
    fn run_entry_record_round_trip_is_exact() {
        let entry = RunEntry {
            score: 12_345,
            peak_combo: 7,
            peak_streak_secs: 123.5,
            ended_at: "2026-07-21T00:00:00+00:00".to_string(),
        };
        let record = record_from_entry(&entry);
        let back = entry_from_record(&record);
        assert_eq!(back.score, entry.score);
        assert_eq!(back.peak_combo, entry.peak_combo);
        assert_eq!(back.peak_streak_secs, entry.peak_streak_secs);
        assert_eq!(back.ended_at, entry.ended_at);
    }

    #[test]
    fn history_from_records_caps_at_history_cap_newest_first() {
        let records: Vec<super::super::score_store::RunRecord> = (0..60)
            .map(|i| super::super::score_store::RunRecord {
                score: i as u64,
                peak_combo: 2,
                peak_streak_secs: 10.0,
                ended_at: format!("run-{i}"),
            })
            .collect();
        let entries = history_from_records(&records);
        assert_eq!(entries.len(), HISTORY_CAP);
        // Newest-first ordering is preserved (input order kept, just
        // truncated), so the first entry is still the file's first one.
        assert_eq!(entries[0].ended_at, "run-0");
        assert_eq!(entries[HISTORY_CAP - 1].ended_at, format!("run-{}", HISTORY_CAP - 1));
    }
}
