//! The interactive ratatui dashboard, multi-account edition.
//!
//! Three views, switched with the `1`/`2`/`3` keys:
//!
//! - View 1: every account's 5h/weekly/extra bars plus a single table
//!   of every live session across every account.
//! - View 2: drill-down on the selected session (shows the parent
//!   account's bars + session-scoped token breakdown).
//! - View 3: the original single-pane dashboard, scoped to the
//!   selected account.
//!
//! Data flow:
//!  - One `notify` watcher per account `projects/` dir, fanned into a
//!    single mpsc; every JSONL change marks the owning account dirty.
//!  - One live poller thread per account, staggered so we don't fire
//!    every OAuth request on the same second.
//!  - On every event-loop tick we drain channels, debounce dirty
//!    reloads to ≥500ms per account, and rescan live sessions.

mod composer_modal;
mod kill_confirm_modal;
mod markdown;
mod model_picker_modal;
mod new_session_modal;
mod skill_picker_modal;
mod terminal_overlay;
mod text_input;
mod toast;
mod under_construction;
mod update_prompt_modal;
mod view_account;
mod view_all;
mod view_mewxi;
mod view_session;
mod view_setup;
mod widgets;

use composer_modal::{ComposerModal, ComposerOutcome};
use kill_confirm_modal::{KillConfirmModal, KillConfirmOutcome};
use model_picker_modal::{ModelOutcome, ModelPickerModal};
use new_session_modal::{ModalOutcome, NewSessionModal};
use skill_picker_modal::{SkillOutcome, SkillPickerModal};
use update_prompt_modal::{UpdatePromptModal, UpdatePromptOutcome};

use crate::accounts::{self, Account, AccountsView};
use crate::agent_control::{self, PtySession};
use crate::live_session::{self, LiveSession, SessionState};
use crate::live_usage::{self, LiveUsage};
use crate::setup::{self, SetupSnapshot};
use crate::stats::{self, Aggregate, UsageTotals};
use crate::update::{self, UpdateStatus};
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Frame;
use ratatui::Terminal;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

/// One account's runtime state — aggregate + live + currently-running sessions.
pub struct PerAccount {
    pub account: Account,
    pub agg: Aggregate,
    pub live: Option<LiveUsage>,
    pub live_sessions: Vec<LiveSession>,
}

/// Flat session reference used by view 1's table and view 2's drill-down.
pub struct SessionRef {
    pub account_name: String,
    pub session_id: String,
    pub pid: u32,
    pub project: String,
    pub cwd: PathBuf,
    pub transcript_path: PathBuf,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub state_since: chrono::DateTime<chrono::Utc>,
    pub model: String,
    /// Latest assistant model from the transcript, INCLUDING sub-agents
    /// and plan-mode helpers. Empty when no assistant record yet. Used
    /// for the dim "via …" indicator next to the main model badge when
    /// claude has internally diverged from the user's pick.
    pub active_model: String,
    pub tokens: u64,
    pub cost_usd: f64,
    pub totals: UsageTotals,
    pub current_context: Option<u64>,
    pub context_cap: Option<u64>,
    pub state: SessionState,
    pub activity: crate::live_session::Activity,
    /// Latest permission mode from the transcript (`default`, `auto`,
    /// `acceptEdits`, `plan`). `None` until a record exposes one.
    pub permission_mode: Option<String>,
    /// Current `/effort` level (`auto`, `low`, `medium`, `high`,
    /// `xhigh`, `max`). Sourced from the optimistic pick first, then the
    /// per-session level claude reported through `mewxi status`, then the
    /// account's `settings.json` `effortLevel` as a last resort. `None`
    /// when the model has no effort support (Haiku) or nothing's
    /// configured yet.
    pub effort: Option<String>,
    /// True once the user has asked mewxi to close this agent. The row
    /// renders a red `killing` status with every other column dashed
    /// out, and is kept alive as a synthetic placeholder for a short
    /// linger ([`KILLING_TTL`]) after the session marker disappears from
    /// the scan, so the feedback doesn't blink out the instant the
    /// process dies. See [`apply_killing_overlay`].
    pub killing: bool,
    /// `Some` when this row is a sub-agent a session is running right
    /// now, not a top-level session. Sub-agent rows are full members of
    /// the flattened list — selectable, inspectable in view 2 (their
    /// `transcript_path` points at the agent's own JSONL) — and are kept
    /// glued directly under their parent by [`regroup_sessions`]. For
    /// these rows `session_id` holds the agent id and `pid` mirrors the
    /// parent's (sort key only — a sub-agent has no process of its own,
    /// which is why the kill path refuses them).
    pub subagent: Option<SubAgentTag>,
}

/// Identity + label data for a sub-agent row. See [`SessionRef::subagent`].
#[derive(Clone, PartialEq, Eq)]
pub struct SubAgentTag {
    /// `session_id` of the session that delegated to this agent.
    pub parent_session_id: String,
    /// Agent kind from the sidecar (`Explore`, `general-purpose`, …).
    pub agent_type: Option<String>,
    /// Short task label for the row.
    pub description: String,
    /// Name of the Workflow run this agent was spawned from, `None` for
    /// a plain Agent/Task delegation. Rendered as a `⚙ <name> ›` prefix.
    pub workflow: Option<String>,
}

/// Optimistic per-driver state for things mewxi just commanded but
/// hasn't seen reflected in the transcript yet. The badge would
/// otherwise lag by however long claude takes to flush its next
/// permission-mode / assistant record — for Shift-Tab cycles that can
/// be never (no record gets written until the next user prompt), and
/// for `/model X` it's at least one round-trip.
///
/// We snapshot the transcript's value at the moment we set the optimistic
/// guess in `*_baseline`. Each scan compares the latest transcript value
/// against that baseline: as soon as they differ, claude has caught up
/// and we drop the optimistic guess so the transcript is authoritative
/// again. If they match (transcript hasn't moved), we keep showing what
/// the user just commanded.
#[derive(Default, Clone)]
struct DriverOptimistic {
    mode: Option<String>,
    mode_baseline: Option<String>,
    /// Mode we believed was current when we last sent Shift-Tab. Paired
    /// with `cycle_auto` so the reconcile path can record
    /// `cycle_prev → actual_next` into the learned cycle map once the
    /// transcript catches up. Cleared on observation.
    cycle_prev: Option<String>,
    cycle_auto: Option<bool>,
    model: Option<String>,
    model_baseline: Option<String>,
    /// Effort the user just picked via the model picker. There's no
    /// transcript record for effort, so this stays "optimistic" for the
    /// life of the driver — we have no reconcile path back to claude's
    /// view, but we know `/effort X` was acknowledged the moment the
    /// PTY accepted the bytes. Cleared on driver respawn.
    effort: Option<String>,
}

/// Hardcoded fallback cycle order — used only on the very first
/// Shift-Tab from a given (mode, auto) state, before [`ModeCycle`] has
/// observed claude's actual transition.
///
/// Verified against claude 2.1.150's `dRH` (telemetry: `tengu_mode_cycle`)
/// which encodes:
///   default → acceptEdits → plan → {bypassPermissions|auto|default}
/// When auto is available and bypass is not, the cycle becomes
/// `default → acceptEdits → plan → auto → default`. Without auto it
/// collapses to the 3-cycle.
///
/// `auto_available` requires both:
///   * the account opted into auto-mode-by-default via
///     `skipAutoPermissionPrompt`, and
///   * the current model is one claude itself allows auto on (Opus and
///     Sonnet; Haiku is rejected with "auto mode unavailable").
///
/// Don't add knowledge to this function — extend the hardcoded list
/// reluctantly. The forward-compatible mechanism is [`ModeCycle`]: it
/// observes the actual next mode that lands in the transcript and from
/// then on predicts from observation, not from this fallback. So if a
/// future claude shuffles the order, mewxi self-corrects after one
/// cycle per affected source mode.
fn cycle_mode(current: &str, auto_available: bool) -> &'static str {
    let cycle: &[&str] = if auto_available {
        &["default", "acceptEdits", "plan", "auto"]
    } else {
        &["default", "acceptEdits", "plan"]
    };
    let idx = cycle.iter().position(|m| *m == current).unwrap_or(0);
    cycle[(idx + 1) % cycle.len()]
}

/// Learned next-mode map keyed by `(current_mode, auto_available)`.
/// Populated from real claude transitions observed in the transcript
/// after a Shift-Tab; queried before [`cycle_mode`] so a fallback that
/// goes stale (because claude shuffled its cycle) costs at most one
/// wrong prediction per source mode per process lifetime.
///
/// Not persisted across restarts — claude rewrites the permission-mode
/// record on every Shift-Tab, so a fresh process re-learns within a
/// keystroke or two of the user touching the feature.
#[derive(Default)]
struct ModeCycle {
    next: HashMap<(String, bool), String>,
}

impl ModeCycle {
    /// Best guess for the mode claude lands on after Shift-Tab from
    /// `current`. Prefers a learned transition, falls back to the
    /// hardcoded [`cycle_mode`] when we've never seen one.
    fn predict(&self, current: &str, auto_available: bool) -> String {
        if let Some(next) = self.next.get(&(current.to_string(), auto_available)) {
            return next.clone();
        }
        cycle_mode(current, auto_available).to_string()
    }

    /// Record an observed `prev → actual` transition for future
    /// predictions. No-op when either side is empty, when they match
    /// (no transition happened), or when `actual` is a transient
    /// pseudo-mode we don't model. `auto_available` reflects the state
    /// at the time of the keystroke, not now — pass through what the
    /// cycle handler saw.
    fn observe(&mut self, prev: &str, actual: &str, auto_available: bool) {
        if prev.is_empty() || actual.is_empty() || prev == actual {
            return;
        }
        self.next
            .insert((prev.to_string(), auto_available), actual.to_string());
    }
}

/// True when claude would accept auto mode on this model. Mirrors
/// claude's own gate ("auto mode unavailable for this model"): only
/// Haiku is rejected. Everything else — including unknown/future model
/// families — is treated as supported. An opus/sonnet allowlist here
/// silently dropped `auto` from the predicted cycle on fable sessions,
/// leaving the badge stuck on a wrong mode; with the denylist a wrong
/// guess costs one mispredicted frame instead.
fn model_supports_auto(model: &str) -> bool {
    !model.trim().to_ascii_lowercase().contains("haiku")
}

/// Human label shown in the status banner — mirrors the rendered badge.
fn mode_label(raw: &str) -> &'static str {
    match raw {
        "default" => "manual",
        "auto" => "auto",
        "acceptEdits" => "accept edits",
        "plan" => "plan",
        _ => "?",
    }
}

/// Send Shift-Tab to the driven PTY and immediately cycle our local
/// optimistic mode so the badge reacts in the same frame. Reconcile runs
/// after the next transcript scan and snaps to claude's reality if our
/// prediction was off.
fn cycle_mode_via_pty(
    pty: &mut PtySession,
    key: &(String, String),
    per_account: &[PerAccount],
    optimistic: &mut HashMap<(String, String), DriverOptimistic>,
    mode_cycle: &ModeCycle,
    status: &mut Option<(String, Instant)>,
) {
    if pty.send_keys(b"\x1b[Z").is_err() {
        *status = Some(("mode cycle FAILED — pty write error".into(), Instant::now()));
        return;
    }
    let pa = per_account.iter().find(|p| p.account.name == key.0);
    let account_opt_in = pa
        .map(|p| p.account.default_permission_mode() == "auto")
        .unwrap_or(false);
    let ls = pa.and_then(|p| p.live_sessions.iter().find(|s| s.session_id == key.1));
    let transcript_mode = ls.and_then(|s| s.permission_mode.clone());
    let opt = optimistic.entry(key.clone()).or_default();
    let effective_model = opt
        .model
        .clone()
        .or_else(|| ls.map(|s| s.model.clone()))
        .unwrap_or_default();
    let auto_available = account_opt_in && model_supports_auto(&effective_model);
    let current = opt
        .mode
        .clone()
        .or_else(|| transcript_mode.clone())
        .unwrap_or_else(|| "default".into());
    let next = mode_cycle.predict(&current, auto_available);
    opt.mode_baseline = transcript_mode;
    opt.mode = Some(next.clone());
    // Remember what we *thought* was current and which cycle variant
    // applied, so the reconcile pass can teach [`ModeCycle`] the actual
    // next mode claude lands on. Pinning `auto_available` here matters:
    // if the user switches models between the keystroke and the
    // transcript update, the gate may flip but the observation still
    // belongs to the cycle variant that was live at keystroke time.
    opt.cycle_prev = Some(current);
    opt.cycle_auto = Some(auto_available);
    *status = Some((format!("mode → {}", mode_label(&next)), Instant::now()));
}

/// A `claude` child mewxi spawned whose session marker hasn't appeared
/// yet. Each frame we diff that account's live sessions against the
/// snapshot taken at spawn time; the first new session_id becomes the
/// driver's identity, and the [`PtySession`] graduates to the live
/// `drivers` registry keyed by `(account_name, session_id)`.
struct PendingSpawn {
    account_name: String,
    snapshot_session_ids: HashSet<String>,
    pty: PtySession,
    started_at: Instant,
    /// Where the child was spawned. Shown in the "starting…" placeholder.
    cwd: PathBuf,
    /// Synthetic `(account, "__pending:<id>")` key under which this
    /// spawn is pinned in `pinned_session` *before* its real session_id
    /// appears. The promotion step matches on this to swap pins to the
    /// real key without changing view mode.
    placeholder_key: (String, String),
}

/// Marker prefix used to distinguish a synthetic placeholder
/// session_id from a real UUID. Real session ids are UUIDs, so this
/// prefix cannot collide.
const PLACEHOLDER_PREFIX: &str = "__pending:";

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum ViewMode {
    AllSessions,
    SessionDetail,
    AccountDetail,
    Setup,
    Mewxi,
}

impl ViewMode {
    /// Parse the `default_view` config value. Accepts the documented
    /// view names and their `1`–`4` switch keys; anything else
    /// (including `None`) falls back to the overview.
    fn from_config(s: Option<&str>) -> Self {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("session" | "session_detail" | "2") => ViewMode::SessionDetail,
            Some("account" | "account_detail" | "3") => ViewMode::AccountDetail,
            Some("config" | "setup" | "4") => ViewMode::Setup,
            _ => ViewMode::AllSessions,
        }
    }
}

/// Stylised cat-face brand logos at four sizes. We pick the biggest one
/// that fits the splash area at runtime; the smaller variants are used
/// on narrower terminals so the cat stays recognisable instead of being
/// truncated. `_DIMS` tuples are `(rendered_height, max_line_width)`
/// after stripping blank padding rows.
const LOGO_LARGE: &str = include_str!("../../images/mewxi.ascii");
const LOGO_MEDIUM: &str = include_str!("../../images/mewxi-medium.ascii");
const LOGO_SMALL: &str = include_str!("../../images/mewxi-small.ascii");
const LOGO_TINY: &str = include_str!("../../images/mewxi-tiny.ascii");
const LOGO_LARGE_DIMS: (u16, u16) = (35, 84);
const LOGO_MEDIUM_DIMS: (u16, u16) = (26, 64);
const LOGO_SMALL_DIMS: (u16, u16) = (18, 44);
const LOGO_TINY_DIMS: (u16, u16) = (12, 32);

/// Big "Mewxi" in standard-figlet ASCII line-art, mixed case. 24 cols ×
/// 5 rows. Trailing whitespace on a line before `\n\` is preserved —
/// only the source newline + leading indent after `\` is consumed.
const MEWXI_BIG: &str = " __  __                         _ \n\
                        |  \\/  |  ___  __      ____  __(_)\n\
                        | |\\/| | / _ \\ \\ \\ /\\ / /\\ \\/ /| |\n\
                        | |  | ||  __/  \\ V  V /  >  < | |\n\
                        |_|  |_| \\___|   \\_/\\_/  /_/\\_\\|_|";
const MEWXI_BIG_HEIGHT: u16 = 5;
const MEWXI_BIG_WIDTH: u16 = 34;

/// Hold the splash for this long unless the user dismisses with a key.
/// Long enough to register the brand, short enough that returning users
/// don't feel held hostage.
const SPLASH_DURATION: Duration = Duration::from_millis(1400);

pub fn run(no_live: bool) -> Result<()> {
    // Silence stderr-bound logging from background fetch threads — any
    // `eprintln!` into the alternate screen would visibly corrupt rows.
    // Errors are still captured in `live_usage::most_recent_error()` so
    // the TUI can render them in its own bordered footer.
    live_usage::set_tui_mode(true);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Opt into the kitty keyboard protocol so crossterm parses
    // Shift+Tab (and other modified keys) correctly on terminals like
    // ghostty / kitty / foot that have it always-on. Without this,
    // such terminals deliver Shift+Tab as the raw byte sequence
    // `\x1b[9;2u` and crossterm reports it as a string of unrelated
    // keypresses, so our Shift-Tab handler never fires. Terminals
    // that don't support the protocol silently ignore the push.
    // DISAMBIGUATE_ESCAPE_CODES is the minimum flag needed for
    // unambiguous BackTab; the matching Pop on exit restores the
    // terminal's prior state.
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    enable_hover_tracking();

    // Kick the self-update check the moment the splash mascot appears,
    // so the network round-trip overlaps the splash hold and the
    // initial account/session load instead of starting after them —
    // by the time the main view is up the answer is usually in.
    let startup_update = start_update_check();
    let _ = show_splash(&mut terminal, SPLASH_DURATION);
    let result = run_loop(&mut terminal, no_live, startup_update);

    let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    disable_hover_tracking();
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    live_usage::set_tui_mode(false);
    match result? {
        LoopExit::Quit => Ok(()),
        LoopExit::RestartAfterUpdate => {
            // The loop already tore down driven sessions and pollers —
            // same path as 'q' — and the terminal is restored, so the
            // new binary can take over this process wholesale.
            println!("mewxi updated — restarting …");
            let err = update::restart_process();
            Err(anyhow::anyhow!(
                "failed to restart after update: {err} — restart mewxi manually"
            ))
        }
    }
}

/// How [`run_loop`] ended: a normal quit, or a successful self-update
/// that should be followed by exec'ing the new binary.
enum LoopExit {
    Quit,
    RestartAfterUpdate,
}

/// Tear down the alt-screen / raw-mode / mouse-capture stack so an
/// external interactive program (vim, nano, ...) can take over the
/// controlling terminal. Pair with [`resume_terminal`] after the child
/// exits — always call resume, even on error, or the shell is left
/// wedged in raw mode.
fn suspend_terminal<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
) -> io::Result<()> {
    let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    disable_hover_tracking();
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Re-arm the same crossterm flags `run()` set up on startup. Mirrors
/// the prologue at the top of [`run`]; keep the two in sync.
fn resume_terminal<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
) -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let _ = execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    );
    terminal.clear()?;
    enable_hover_tracking();
    Ok(())
}

/// Suspend the TUI, run the self-update (git fast-forward + cargo
/// rebuild, streaming output to the real terminal so the user sees
/// progress), then resume. Returns a one-line outcome message for the
/// status banner plus whether the update actually installed — callers
/// use the flag to exit the loop and restart into the new binary.
fn run_update_apply<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
) -> (String, bool) {
    if let Err(e) = suspend_terminal(terminal) {
        return (format!("update aborted — failed to suspend terminal: {e}"), false);
    }
    let res = update::apply_now();
    let resume = resume_terminal(terminal);
    match (res, resume) {
        (Ok(msg), Ok(())) => (msg, true),
        (Ok(msg), Err(e)) => (format!("{msg} (terminal resume error: {e})"), true),
        (Err(e), _) => (format!("update FAILED: {e}"), false),
    }
}

/// Enable xterm "any-event" mouse tracking (mode 1003) on top of
/// crossterm's button-only capture, so the chat view receives
/// `MouseEventKind::Moved` events with no button held — needed to
/// highlight the code block under the cursor on hover. crossterm has no
/// dedicated command for this private mode, so we write the escape
/// directly. Best-effort: terminals lacking it ignore the sequence.
fn enable_hover_tracking() {
    use std::io::Write;
    let mut out = io::stdout();
    let _ = out.write_all(b"\x1b[?1003h");
    let _ = out.flush();
}

/// Undo [`enable_hover_tracking`]. Paired with every place that drops
/// mouse capture so an external program (or the restored shell) isn't
/// left emitting motion reports.
fn disable_hover_tracking() {
    use std::io::Write;
    let mut out = io::stdout();
    let _ = out.write_all(b"\x1b[?1003l");
    let _ = out.flush();
}

/// Open the user's preferred editor on a tempfile seeded with
/// `initial`, then return the saved content. `Ok(None)` means the user
/// quit without changes (or the file is empty and was unchanged) — the
/// caller should leave the existing composer buffer alone.
///
/// Always restores the terminal before returning, even when the editor
/// fails to spawn or exits non-zero, so a misbehaving editor can't
/// strand the TUI.
fn open_editor_for_input<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    account: &Account,
    initial: &str,
) -> Result<Option<String>> {
    use std::io::Write;

    let cmd = account.editor_command();
    let mut parts = cmd.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("editor command is empty"))?
        .to_string();
    let args: Vec<String> = parts.map(|s| s.to_string()).collect();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "mewxi-compose-{}-{}.md",
        std::process::id(),
        ts
    ));
    {
        let mut f = std::fs::File::create(&path)?;
        f.write_all(initial.as_bytes())?;
    }

    suspend_terminal(terminal)?;
    let status_res = std::process::Command::new(&program)
        .args(&args)
        .arg(&path)
        .status();
    // Always resume — even if the editor failed to spawn, the terminal
    // state may have been touched.
    let resume_res = resume_terminal(terminal);

    let status = status_res?;
    resume_res?;

    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);

    if !status.success() {
        return Err(anyhow::anyhow!(
            "{} exited with status {}",
            program,
            status
        ));
    }

    // Editors typically append a trailing newline on save; strip one to
    // avoid sending a blank "Enter" at the tail. Preserve interior
    // newlines (multi-line prompts).
    let trimmed = content.strip_suffix('\n').unwrap_or(&content);
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);

    if trimmed == initial {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Open `path` in the user's editor (no tempfile, no content returned).
/// Like [`open_editor_for_input`] but for an existing on-disk file —
/// used by the status-line composer to deep-edit a block's TOML. Always
/// restores the terminal, even on failure.
fn open_editor_for_path<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    account: &Account,
    path: &std::path::Path,
) -> Result<()> {
    let cmd = account.editor_command();
    let mut parts = cmd.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("editor command is empty"))?
        .to_string();
    let args: Vec<String> = parts.map(|s| s.to_string()).collect();

    suspend_terminal(terminal)?;
    let status_res = std::process::Command::new(&program)
        .args(&args)
        .arg(path)
        .status();
    let resume_res = resume_terminal(terminal);

    let status = status_res?;
    resume_res?;
    if !status.success() {
        return Err(anyhow::anyhow!("{} exited with status {}", program, status));
    }
    Ok(())
}

/// Resolve `<dir>/<id>.toml`, creating `dir` and seeding the file when it
/// doesn't exist yet — from the embedded default for a built-in block, or
/// a commented skeleton for a new/custom one. An **existing** file is left
/// untouched, so editing a default block creates an editable override copy
/// on the first edit and preserves the user's edits on every edit after.
/// Returns the path to open.
fn ensure_block_file(
    dir: &std::path::Path,
    id: &str,
    is_builtin: bool,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{id}.toml"));
    if !path.exists() {
        let seed = if is_builtin {
            crate::statusline::default_block_source(id).map(str::to_string)
        } else {
            Some(crate::statusline::new_block_skeleton(id))
        };
        if let Some(contents) = seed {
            std::fs::write(&path, contents)?;
        }
    }
    Ok(path)
}

/// Resolve the `<id>.toml` path under the user's status-blocks dir,
/// seeding it (from the embedded default for a built-in, or a commented
/// skeleton for a new/custom block) if it doesn't exist yet, then open it
/// in `$EDITOR`. Afterward, reloads `composer` from a fresh config so the
/// edited content shows in the list + preview.
fn edit_status_block_in_editor<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    view: &AccountsView,
    id: &str,
    is_builtin: bool,
    composer: &mut Option<ComposerModal>,
) {
    let Some(account) = view.pick(None).cloned() else {
        if let Some(m) = composer.as_mut() {
            m.set_status("no account available for $EDITOR".into());
        }
        return;
    };
    let Some(dir) = view
        .status_blocks_dir
        .clone()
        .or_else(accounts::default_status_blocks_dir)
    else {
        if let Some(m) = composer.as_mut() {
            m.set_status("no status-blocks dir configured".into());
        }
        return;
    };

    let result = (|| -> Result<()> {
        let path = ensure_block_file(&dir, id, is_builtin)?;
        open_editor_for_path(terminal, &account, &path)
    })();

    // Reload from a fresh view so on-disk edits surface immediately.
    if let Ok(fresh) = accounts::load_accounts() {
        if let Some(m) = composer.as_mut() {
            m.reload(crate::statusline::composer_rows(&fresh));
        }
    }
    if let Some(m) = composer.as_mut() {
        m.set_status(match result {
            Ok(()) => format!("edited {id}.toml"),
            Err(e) => format!("editor: {e}"),
        });
    }
}

#[cfg(test)]
mod block_file_tests {
    use super::ensure_block_file;

    #[test]
    fn editing_a_default_creates_a_copy_seeded_from_the_embedded_block() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = ensure_block_file(tmp.path(), "ctx", true).unwrap();
        assert!(path.exists());
        assert_eq!(path, tmp.path().join("ctx.toml"));
        let written = std::fs::read_to_string(&path).unwrap();
        let embedded = crate::statusline::default_block_source("ctx").unwrap();
        assert_eq!(
            written, embedded,
            "the override copy must start as an exact copy of the embedded default"
        );
    }

    #[test]
    fn a_later_edit_preserves_user_changes_and_does_not_reseed() {
        let tmp = tempfile::TempDir::new().unwrap();
        // First edit seeds the override copy from the default.
        let path = ensure_block_file(tmp.path(), "ctx", true).unwrap();
        // User edits + saves it.
        std::fs::write(&path, "template = \"EDITED\"\n").unwrap();
        // Editing again must reopen the SAME file untouched, not re-seed it.
        let again = ensure_block_file(tmp.path(), "ctx", true).unwrap();
        assert_eq!(path, again);
        assert_eq!(
            std::fs::read_to_string(&again).unwrap(),
            "template = \"EDITED\"\n",
            "an existing override must be left as the user saved it"
        );
    }

    #[test]
    fn a_new_block_is_seeded_with_a_skeleton_naming_the_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = ensure_block_file(tmp.path(), "mine", false).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("label = \"mine\""), "skeleton:\n{written}");
        assert!(written.contains("template ="), "skeleton:\n{written}");
    }

    #[test]
    fn the_blocks_dir_is_created_if_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("does/not/exist/blocks");
        let path = ensure_block_file(&nested, "model", true).unwrap();
        assert!(nested.is_dir());
        assert!(path.exists());
    }
}

fn show_splash<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    duration: Duration,
) -> Result<()> {
    let start = Instant::now();
    loop {
        terminal.draw(|f| render_splash(f, f.area()))?;
        let remaining = duration.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        let poll_for = remaining.min(Duration::from_millis(80));
        if event::poll(poll_for)? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn render_splash(f: &mut Frame, area: ratatui::layout::Rect) {
    use ratatui::layout::{Alignment, Constraint, Direction, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Clear, Paragraph};

    f.render_widget(Clear, area);

    let gap_h: u16 = 1;
    let tagline_h: u16 = 1;
    let plain_mewxi_h: u16 = 1;

    let mewxi_style = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let tagline_style = Style::default().fg(Color::DarkGray);

    // Pick the biggest cat that fits together with the figlet + a
    // one-row gap between every element (logo / mewxi / tagline). The
    // tiny cat falls back to plain-text "Mewxi" instead of the figlet,
    // so the brand label is always present.
    let figlet_block = MEWXI_BIG_HEIGHT + gap_h + gap_h + tagline_h;
    let plain_block = plain_mewxi_h + gap_h + gap_h + tagline_h;
    let cat: Option<(&'static str, u16, bool)> = if area.height
        >= LOGO_LARGE_DIMS.0 + figlet_block
        && area.width >= LOGO_LARGE_DIMS.1.max(MEWXI_BIG_WIDTH)
    {
        Some((LOGO_LARGE, LOGO_LARGE_DIMS.0, true))
    } else if area.height >= LOGO_MEDIUM_DIMS.0 + figlet_block
        && area.width >= LOGO_MEDIUM_DIMS.1.max(MEWXI_BIG_WIDTH)
    {
        Some((LOGO_MEDIUM, LOGO_MEDIUM_DIMS.0, true))
    } else if area.height >= LOGO_SMALL_DIMS.0 + figlet_block
        && area.width >= LOGO_SMALL_DIMS.1.max(MEWXI_BIG_WIDTH)
    {
        Some((LOGO_SMALL, LOGO_SMALL_DIMS.0, true))
    } else if area.height >= LOGO_TINY_DIMS.0 + plain_block
        && area.width >= LOGO_TINY_DIMS.1
    {
        Some((LOGO_TINY, LOGO_TINY_DIMS.0, false))
    } else {
        None
    };

    let mewxi_line = || Line::from(Span::styled("Mewxi", mewxi_style));
    let tagline_line = || {
        Line::from(Span::styled(
            "multi-agent CLI usage tracker",
            tagline_style,
        ))
    };

    // No cat fits: plain-text "Mewxi" centred, with tagline below if
    // there's room. Add the one-row gap when the screen can afford it.
    let Some((src, logo_h, show_figlet)) = cat else {
        if area.height < plain_mewxi_h + tagline_h {
            f.render_widget(
                Paragraph::new(mewxi_line()).alignment(Alignment::Center),
                area,
            );
            return;
        }
        let with_gap = area.height >= plain_mewxi_h + gap_h + tagline_h;
        let total_h = plain_mewxi_h + if with_gap { gap_h } else { 0 } + tagline_h;
        let top_pad = area.height.saturating_sub(total_h) / 2;
        let mut constraints = vec![
            Constraint::Length(top_pad),
            Constraint::Length(plain_mewxi_h),
        ];
        if with_gap {
            constraints.push(Constraint::Length(gap_h));
        }
        constraints.push(Constraint::Length(tagline_h));
        constraints.push(Constraint::Min(0));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        f.render_widget(
            Paragraph::new(mewxi_line()).alignment(Alignment::Center),
            chunks[1],
        );
        let tagline_idx = if with_gap { 3 } else { 2 };
        f.render_widget(
            Paragraph::new(tagline_line()).alignment(Alignment::Center),
            chunks[tagline_idx],
        );
        return;
    };

    let mewxi_h = if show_figlet { MEWXI_BIG_HEIGHT } else { plain_mewxi_h };
    let total_h = logo_h + gap_h + mewxi_h + gap_h + tagline_h;
    let top_pad = area.height.saturating_sub(total_h) / 2;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_pad),
            Constraint::Length(logo_h),
            Constraint::Length(gap_h),
            Constraint::Length(mewxi_h),
            Constraint::Length(gap_h),
            Constraint::Length(tagline_h),
            Constraint::Min(0),
        ])
        .split(area);

    // Strip the file's blank padding rows so the rendered height
    // matches the LOGO_*_DIMS.0 we sized the slot to.
    let logo_lines: Vec<Line> = src
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(Color::Magenta))))
        .collect();
    f.render_widget(
        Paragraph::new(logo_lines).alignment(Alignment::Center),
        chunks[1],
    );

    if show_figlet {
        let mewxi_lines: Vec<Line> = MEWXI_BIG
            .lines()
            .map(|l| Line::from(Span::styled(l.to_string(), mewxi_style)))
            .collect();
        f.render_widget(
            Paragraph::new(mewxi_lines).alignment(Alignment::Center),
            chunks[3],
        );
    } else {
        f.render_widget(
            Paragraph::new(mewxi_line()).alignment(Alignment::Center),
            chunks[3],
        );
    }

    f.render_widget(
        Paragraph::new(tagline_line()).alignment(Alignment::Center),
        chunks[5],
    );
}

enum LiveMsg {
    Update {
        account_name: String,
        live: Option<LiveUsage>,
    },
    /// Result of a user-initiated `LiveCmd::Refresh`. `ok` is true only when
    /// fresh data was actually pulled from the web; `detail` carries the
    /// failure reason otherwise. Sent alongside the matching `Update`.
    RefreshResult {
        account_name: String,
        ok: bool,
        detail: String,
    },
}
enum LiveCmd {
    Stop,
    /// Force an immediate HTTP fetch, bypassing `REFRESH_INTERVAL`.
    /// Triggered by the user pressing `r` to manually refresh limits.
    Refresh,
}

/// Tracks an in-flight manual refresh (the `r` key) so a single aggregate
/// toast can report the result once every account's poller has reported.
struct RefreshTally {
    /// How many poller `RefreshResult`s we still expect.
    pending: usize,
    /// How many accounts were asked to refresh (for the "X/N" progress text).
    total: usize,
    /// Accounts whose web fetch succeeded.
    ok: usize,
    /// `(account_name, reason)` for accounts whose fetch failed.
    failures: Vec<(String, String)>,
    /// When the refresh was kicked off, used to give up if a poller never
    /// answers (dead thread) so the "refreshing…" toast can't hang forever.
    started: Instant,
}

/// How long to wait for all pollers to answer a manual refresh before
/// reporting whatever results arrived. Comfortably above the 15s HTTP
/// timeout in `live_usage::fetch_live`.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(20);

/// Tag for the manual-refresh toast so its progress updates and final result
/// replace one box in place instead of stacking.
const REFRESH_TOAST_TAG: &str = "refresh";

/// Poll cadence for the in-TUI live updater. Much shorter than the
/// underlying `REFRESH_INTERVAL` because `fetch_or_cached` is cheap on
/// hits — it just re-reads the cache file from disk and returns. The
/// actual HTTP rate is still capped by `REFRESH_INTERVAL`; this tick
/// just makes us pick up writes from the background watcher daemon (or
/// from another TUI instance) within a few seconds, so the limits
/// gauges keep ticking for both the active *and* the idle session
/// without anyone having to interact with Claude Code.
const POLLER_TICK: Duration = Duration::from_secs(5);

/// How long view 1's session selection stays highlighted after the last
/// navigation key. Long enough that the user can pick a row, glance at
/// it, and decide whether to drill in; short enough that an unattended
/// dashboard doesn't leave a stale highlight pinned to an arbitrary row.
const SELECTION_VISIBLE: Duration = Duration::from_secs(5);

/// How long a live-fetch error stays in the footer before it auto-hides.
/// The user can also dismiss it earlier with `x`.
const ERROR_VISIBLE: Duration = Duration::from_secs(10);

fn spawn_live_poller(
    account: Account,
    no_live: bool,
    stagger: Duration,
) -> (Receiver<LiveMsg>, Sender<LiveCmd>) {
    let (out_tx, out_rx) = channel::<LiveMsg>();
    let (in_tx, in_rx) = channel::<LiveCmd>();
    thread::spawn(move || {
        thread::sleep(stagger);
        // Paint cached value ASAP for instant first frame…
        let _ = out_tx.send(LiveMsg::Update {
            account_name: account.name.clone(),
            live: live_usage::load_cached(&account),
        });
        // …then force one HTTP fetch even if cache is "fresh enough".
        // A stale background daemon running an older binary can keep
        // overwriting our cache with wrong-account data; bypassing the
        // REFRESH_INTERVAL short-circuit on bootstrap guarantees the
        // TUI's first real frame shows correct numbers.
        let _ = out_tx.send(LiveMsg::Update {
            account_name: account.name.clone(),
            live: live_usage::fetch_force(&account, no_live),
        });
        loop {
            match in_rx.recv_timeout(POLLER_TICK) {
                Ok(LiveCmd::Stop) => break,
                // A manual refresh bypasses `REFRESH_INTERVAL` so the user
                // gets fresh numbers from the web on demand; the periodic
                // tick below stays on the cheap cached path. We report the
                // outcome back so the TUI can notify success/failure.
                Ok(LiveCmd::Refresh) => {
                    let outcome = live_usage::fetch_force_outcome(&account, no_live);
                    let (ok, detail) = match &outcome {
                        live_usage::FetchOutcome::Fetched(_) => (true, String::new()),
                        live_usage::FetchOutcome::RateLimited(_) => {
                            (false, "rate limited (429)".to_string())
                        }
                        live_usage::FetchOutcome::Failed { reason, .. } => (false, reason.clone()),
                        live_usage::FetchOutcome::Cached(_) => {
                            (false, "live disabled".to_string())
                        }
                    };
                    let live = outcome.into_usage();
                    let update_ok = out_tx
                        .send(LiveMsg::Update {
                            account_name: account.name.clone(),
                            live,
                        })
                        .is_ok();
                    let result_ok = out_tx
                        .send(LiveMsg::RefreshResult {
                            account_name: account.name.clone(),
                            ok,
                            detail,
                        })
                        .is_ok();
                    if !(update_ok && result_ok) {
                        break;
                    }
                }
                Err(_) => {
                    let live = live_usage::fetch_or_cached(&account, no_live);
                    if out_tx
                        .send(LiveMsg::Update {
                            account_name: account.name.clone(),
                            live,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    (out_rx, in_tx)
}

/// In-flight startup self-update check, started in [`run`] the moment
/// the splash mascot appears and handed to [`run_loop`], which drains
/// `rx` on every tick. `cached` is the still-fresh on-disk check (when
/// one exists) so the UI can show a verdict instantly while the live
/// check confirms it against origin.
struct StartupUpdateCheck {
    tx: Sender<std::result::Result<UpdateStatus, String>>,
    rx: Receiver<std::result::Result<UpdateStatus, String>>,
    cached: Option<UpdateStatus>,
    checking: bool,
}

/// Start the startup update check. Unlike the periodic cache refresh,
/// this always hits origin (one background git fetch) so every launch
/// knows whether the running binary is outdated — a fresh cache only
/// pre-seeds the answer, it doesn't suppress the check. The only
/// opt-out is `update_check = false` in `accounts.toml`.
fn start_update_check() -> StartupUpdateCheck {
    let (tx, rx) = channel::<std::result::Result<UpdateStatus, String>>();
    let view = accounts::load_accounts().ok();
    let enabled = view.as_ref().map(|v| v.update_check).unwrap_or(true);
    let mut cached = None;
    let mut checking = false;
    if enabled {
        if let Some(view) = view.as_ref() {
            cached = update::fresh_cached_status(
                update::channel_from_view(view),
                update::interval_from_view(view),
            );
        }
        update::spawn_check(tx.clone());
        checking = true;
    }
    StartupUpdateCheck { tx, rx, cached, checking }
}

fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    no_live: bool,
    startup_update: StartupUpdateCheck,
) -> Result<LoopExit> {
    crate::debug_log::log(&format!(
        "tui.start no_live={no_live} pid={}",
        std::process::id()
    ));
    let view: AccountsView = accounts::load_accounts()?;
    if view.accounts.is_empty() {
        return Err(anyhow::anyhow!("no accounts discovered"));
    }
    // Seed per-account state.
    let alive = live_session::alive_pids();
    let mut per_account: Vec<PerAccount> = view
        .accounts
        .iter()
        .map(|a| PerAccount {
            account: a.clone(),
            agg: stats::load_and_aggregate_for(a).unwrap_or_default(),
            live: live_usage::load_cached(a),
            live_sessions: live_session::scan(a, &alive, &[]),
        })
        .collect();

    // Filesystem watchers — one per account, fan into one channel.
    let (dirty_tx, dirty_rx) = channel::<String>();
    let mut watchers: Vec<RecommendedWatcher> = Vec::new();
    for account in &view.accounts {
        let dir = account.projects_dir();
        if !dir.exists() {
            continue;
        }
        let acct_name = account.name.clone();
        let tx = dirty_tx.clone();
        let mut w: RecommendedWatcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res {
                    if ev
                        .paths
                        .iter()
                        .any(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                    {
                        let _ = tx.send(acct_name.clone());
                    }
                }
            })?;
        w.watch(&dir, RecursiveMode::Recursive)?;
        watchers.push(w);
    }
    drop(dirty_tx);

    // Live poller per account, staggered so we don't burst the OAuth endpoint.
    let mut live_pollers: Vec<(Receiver<LiveMsg>, Sender<LiveCmd>)> = Vec::new();
    for (i, account) in view.accounts.iter().enumerate() {
        let stagger = Duration::from_millis(250 * i as u64);
        live_pollers.push(spawn_live_poller(account.clone(), no_live, stagger));
    }

    let mut mode = ViewMode::from_config(view.default_view.as_deref());
    let mut selected_session: usize = 0;
    let mut last_selected_session: usize = usize::MAX;
    // Stable identity of the session currently being inspected in
    // SessionDetail. The flattened list is re-sorted every frame by
    // last_activity, so a raw index would silently jump to a different
    // session whenever another session's activity moves it ahead.
    let mut pinned_session: Option<(String, String)> = None;
    // Sessions the user has just asked mewxi to close, keyed by
    // (account, session_id). Drives the red `killing` overlay in view 1
    // and keeps a placeholder row alive after the process marker
    // disappears. Pruned by `apply_killing_overlay` after `KILLING_TTL`.
    let mut killing_sessions: HashMap<(String, String), KillingEntry> = HashMap::new();
    // Last frame's `pinned_session`. Used to detect view-change so we can
    // drop a stale dismiss-signature (see `overlay_dismissed_sig`) when
    // the user navigates back to a session whose modal they dismissed —
    // the "don't re-pop" intent only applies while the user is still
    // viewing that session.
    let mut last_pinned_session: Option<(String, String)> = None;
    let mut chat_scroll: usize = 0;
    // Active mouse-drag text selection over the chat log. Coordinates
    // are in terminal cells; cleared whenever the chat content shifts
    // (scroll, view-change) so the highlight never points at a row
    // that's already moved off-screen. `dragging` distinguishes a
    // still-in-progress drag from a finished one that's been copied
    // and is being held visible until the user clicks again.
    let mut chat_selection: Option<view_session::ChatSelection> = None;
    let mut chat_selection_dragging: bool = false;
    // Last known mouse position (terminal cells), updated on any mouse
    // event including hover (motion with no button). The chat view uses
    // it to highlight the code block under the cursor.
    let mut mouse_pos: Option<(u16, u16)> = None;
    // `None` means follow tail (selection tracks the latest change row);
    // `Some(i)` pins to a concrete index. j/k transitions out of follow
    // mode; G / End re-enters it. The render function writes the row
    // count back here so key handlers can resolve "one before tail"
    // without needing to load the transcript themselves.
    let mut changes_selection: Option<usize> = None;
    let mut last_change_count: usize = 0;
    // Lines scrolled down within the currently selected change's
    // detail pane. Reset to 0 whenever the selection changes so
    // each row starts at the top of its input.
    let mut detail_scroll: usize = 0;
    let mut selected_account: usize = 0;
    let mut selected_setup: usize = 0;
    // Persistent windowed offset for the config list so it scrolls only
    // when the cursor reaches the top or bottom edge of the visible area.
    let mut setup_scroll: usize = 0;
    // View 1's session selection highlight fades out after a short
    // idle period so the table doesn't stay visually pinned to a row
    // the user picked once and forgot. The selection *index* still
    // drives view 2's drill-down — only the row chrome (arrow / yellow
    // bold) is suppressed once stale.
    let mut last_session_select: Instant = Instant::now();
    // Scroll state for view 1's sessions table — persists across frames
    // so the table follows the selection instead of clipping it once the
    // cursor moves past the visible window.
    let mut all_table_state = ratatui::widgets::TableState::default();
    let mut last_reload: HashMap<String, Instant> = HashMap::new();
    let mut dirty: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_full_tick = Instant::now();
    let mut setup_snapshot: Option<SetupSnapshot> = setup::inspect(no_live).ok();
    let mut setup_message: Option<String> = None;
    // Error footer auto-hide: track the currently-displayed error so we
    // can time it out after 10s and let the user dismiss it with `x`.
    // Resets whenever a new (different) error appears.
    let mut error_shown: Option<(String, Instant)> = None;
    let mut error_dismissed = false;

    // Agent-control: sessions mewxi itself spawned and owns the PTY
    // for, plus the in-flight spawns waiting for a session-marker file
    // to appear so we can pin them by session_id.
    let mut drivers: HashMap<(String, String), PtySession> = HashMap::new();
    let mut pending_spawns: Vec<PendingSpawn> = Vec::new();
    let mut driver_input = text_input::TextInput::new();
    let mut driver_input_focused: bool = false;
    // Persistent horizontal scroll offset (in chars) for the driver
    // input row. The render function nudges it just enough to keep the
    // cursor inside the visible window — it does NOT snap back to 0
    // when the cursor moves left, so the caret can roam freely until it
    // hits an edge.
    let mut driver_input_scroll: usize = 0;
    let mut defocus_input_after_send: bool = view.defocus_input_after_send;
    let mut default_view_pref =
        view_setup::DefaultView::from_config(view.default_view.as_deref());
    let mut driver_status: Option<(String, Instant)> = None;
    // View-agnostic toast notifications, drawn over the whole frame after the
    // active view renders so feedback shows in every view (unlike
    // `driver_status`, which only surfaces in the Setup view).
    let mut toasts = toast::Toasts::default();
    // Set when the user presses `r`; cleared once every poller has reported
    // its outcome (or the timeout elapses), at which point a success/failure
    // notification replaces the "refreshing…" toast.
    let mut refresh_tally: Option<RefreshTally> = None;
    let mut driver_optimistic: HashMap<(String, String), DriverOptimistic> = HashMap::new();
    // Self-learning Shift-Tab cycle. Seeded empty; the reconcile pass
    // populates it the first time each `(prev_mode, auto_available)`
    // pair is observed transitioning. From then on predictions use
    // claude's actual behaviour instead of [`cycle_mode`]'s fallback,
    // so a future claude shuffling its cycle costs at most one
    // mispredicted keystroke per source mode.
    let mut mode_cycle = ModeCycle::default();

    // Terminal overlay: when claude pops a TUI prompt (model-switch
    // continue, multiselect, accept-edit y/N), surface its rendered PTY
    // screen on top of the chat-log view and route keystrokes straight
    // to the PTY. `overlay_dismissed_sig` records the content hash of
    // a popup the user dismissed with F10 so re-detection stays
    // suppressed for as long as the popup stays on screen unchanged.
    // The entry is dropped when the popup vanishes (counted as
    // "answered") or when its content hash changes (a different
    // popup), so the next genuine new prompt re-pops the overlay.
    let mut overlay_open: HashSet<(String, String)> = HashSet::new();
    let mut overlay_dismissed_sig: HashMap<(String, String), u64> = HashMap::new();
    // After a slash command is sent (e.g. /clear, /compact), claude
    // performs an internal session reset and may briefly drain stdin.
    // Hold the next send until this deadline to give the reset time to
    // settle and the session_id rotation to be observed by the
    // re-pin pass above.
    let mut driver_send_grace_until: HashMap<(String, String), Instant> = HashMap::new();

    // New-session modal state. Some(_) while it's open and intercepting
    // every keystroke; None otherwise. `next_spawn_id` produces the
    // synthetic session_id under which a pending spawn is pinned before
    // its real session_id becomes known.
    let mut new_session_modal: Option<NewSessionModal> = None;
    let mut next_spawn_id: u64 = 0;

    // Model-picker modal state. Owns every keystroke while open. The
    // chosen slug is sent to the driven session's PTY as `/model <slug>\r`
    // when the user confirms. Opened with `m` in driven-session scope.
    let mut model_picker: Option<ModelPickerModal> = None;

    // Skill-picker modal state. Lists skills + commands discovered from
    // the account's CLAUDE_CONFIG_DIR (user + plugin scope) and from
    // .claude/ in the session cwd (project scope), so the user can
    // browse and inject one as `/<name>\r`. Opened with `k` in
    // driven-session scope.
    let mut skill_picker: Option<SkillPickerModal> = None;

    let mut kill_confirm_modal: Option<KillConfirmModal> = None;

    // Status-line composer modal (opened from the Config view). Owns
    // every keystroke while open.
    let mut composer_modal: Option<ComposerModal> = None;

    // Self-update: the background check was kicked in `run` the moment
    // the splash mascot appeared (see [`start_update_check`]), so by
    // now its result is often already waiting on `update_rx` — the
    // drain in the main loop picks it up on the first tick. A fresh
    // cached check pre-seeds the status (and the startup prompt) so
    // the verdict shows instantly even before the live result lands.
    // When something newer exists AND the startup prompt is enabled,
    // the modal asks the user once per run. The Config view reads the
    // same state.
    let StartupUpdateCheck {
        tx: update_tx,
        rx: update_rx,
        cached: update_cached,
        checking,
    } = startup_update;
    let mut update_channel = update::channel_from_view(&view);
    let mut update_check_enabled = view.update_check;
    let mut update_interval = update::interval_from_view(&view);
    let mut update_prompt_enabled = view.update_prompt;
    let mut update_status: Option<UpdateStatus> = None;
    let mut update_error: Option<String> = None;
    let mut update_checking = checking;
    let mut update_prompt_modal: Option<UpdatePromptModal> = None;
    let mut update_build_dir: Option<String> = view
        .update_build_dir
        .as_ref()
        .map(|p| p.display().to_string());
    // While Some, the Config view's build-dir row is in text-edit mode
    // and keys go to this input instead of navigation.
    let mut update_build_dir_edit: Option<text_input::TextInput> = None;
    let mut update_prompted = false;
    if let Some(cached) = update_cached {
        if cached.available && update_prompt_enabled {
            update_prompt_modal = Some(UpdatePromptModal::new(cached.clone()));
            update_prompted = true;
        }
        update_status = Some(cached);
    }

    // Refresh the launchd watcher if it's still running the previous
    // binary in memory — otherwise it will keep overwriting our cache
    // files with stale-account data. install_watcher does unload+load
    // so this is effectively a restart.
    if let Some(snap) = setup_snapshot.as_ref() {
        if let Some(msg) = setup::restart_watcher_if_stale(&snap.binary, no_live) {
            setup_message = Some(msg);
            setup_snapshot = setup::inspect(no_live).ok();
        }
    }

    // First-run UX: if anything isn't set up, drop the user straight
    // into the setup view so they discover it without reading docs.
    if setup_snapshot.as_ref().is_some_and(|s| !s.fully_ok()) {
        mode = ViewMode::Setup;
    }

    // Set when a self-update installed successfully: exit the loop
    // through the normal teardown below, then let `run` exec the new
    // binary instead of returning to the shell.
    let mut restart_after_update = false;

    loop {
        let mut sessions = flatten_sessions(&per_account, &driver_optimistic);
        apply_killing_overlay(&mut sessions, &mut killing_sessions);
        if selected_session >= sessions.len() && !sessions.is_empty() {
            selected_session = sessions.len() - 1;
        }
        if selected_account >= per_account.len() && !per_account.is_empty() {
            selected_account = per_account.len() - 1;
        }
        let setup_items_len = view_setup::items(setup_snapshot.as_ref()).len();
        if selected_setup >= setup_items_len && setup_items_len > 0 {
            selected_setup = setup_items_len - 1;
        }

        // Filter out accounts the user has marked ignored in the
        // setup view. We use the live setup_snapshot as the source of
        // truth so toggling `i` takes effect on the next frame without
        // a restart. Watcher/poller threads for ignored accounts keep
        // running in the background but their updates land on nothing
        // visible.
        let ignored: std::collections::HashSet<String> = setup_snapshot
            .as_ref()
            .map(|s| {
                s.accounts
                    .iter()
                    .filter(|a| a.ignored)
                    .map(|a| a.account_name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let visible_accounts: Vec<&PerAccount> = per_account
            .iter()
            .filter(|p| !ignored.contains(&p.account.name))
            .collect();
        let visible_sessions: Vec<&SessionRef> = sessions
            .iter()
            .filter(|s| !ignored.contains(&s.account_name))
            .collect();
        if selected_session >= visible_sessions.len() && !visible_sessions.is_empty() {
            selected_session = visible_sessions.len() - 1;
        }
        if selected_account >= visible_accounts.len() && !visible_accounts.is_empty() {
            selected_account = visible_accounts.len() - 1;
        }

        // Re-resolve selected_session from the pinned identity so a
        // re-sort of visible_sessions (e.g. another session's activity
        // bumped it to the top) doesn't make the detail view jump.
        if mode == ViewMode::SessionDetail {
            if let Some((acct, sid)) = &pinned_session {
                if let Some(idx) = visible_sessions
                    .iter()
                    .position(|s| s.account_name == *acct && s.session_id == *sid)
                {
                    selected_session = idx;
                } else if !sid.starts_with(PLACEHOLDER_PREFIX) {
                    // Pinned session is gone (e.g. just killed). Re-pin
                    // to whatever selected_session was clamped to so the
                    // driver_pane lookup below sees the same key the
                    // chat log is rendering — otherwise the input row
                    // disappears until the user round-trips through
                    // view 1.
                    //
                    // Skip for `__pending:` placeholders — they're not
                    // in visible_sessions by design (no JSONL marker
                    // yet), and stomping the pin here cancels the
                    // loading pane before promotion can swap it for
                    // the real session_id.
                    pinned_session = visible_sessions
                        .get(selected_session)
                        .map(|s| (s.account_name.clone(), s.session_id.clone()));
                }
            }
        }

        let visible_selection = if last_session_select.elapsed() < SELECTION_VISIBLE {
            Some(selected_session)
        } else {
            None
        };

        let raw_error = live_usage::most_recent_error()
            .map(|(acct, msg)| format!("[{acct}] {msg}"));
        let live_error = match &raw_error {
            None => {
                error_shown = None;
                error_dismissed = false;
                None
            }
            Some(msg) => {
                let is_new = error_shown.as_ref().is_none_or(|(m, _)| m != msg);
                if is_new {
                    error_shown = Some((msg.clone(), Instant::now()));
                    error_dismissed = false;
                }
                let age = error_shown.as_ref().map(|(_, t)| t.elapsed()).unwrap_or_default();
                if error_dismissed || age >= ERROR_VISIBLE {
                    None
                } else {
                    Some(msg.as_str())
                }
            }
        };

        if selected_session != last_selected_session {
            chat_scroll = 0;
            changes_selection = None;
            detail_scroll = 0;
            last_selected_session = selected_session;
        }

        // Per-frame pane rectangles. Declared inside the loop so the
        // previous frame's values don't leak when the user switches
        // views; render() repopulates only what applies to the current
        // view. Read by handle_scroll later in the same iteration.
        let mut chat_rect: Option<Rect> = None;
        let mut actions_rect: Option<Rect> = None;
        let mut detail_rect: Option<Rect> = None;
        let mut sessions_rect: Option<Rect> = None;
        let mut setup_rect: Option<Rect> = None;
        // Inner rect (border-stripped) of the chat-log pane and the
        // plaintext of each visible row, both written by the renderer
        // so the mouse-drag selection handler can map screen cells to
        // text without re-loading the transcript.
        let mut chat_inner: Option<Rect> = None;
        let mut chat_visible: Vec<String> = Vec::new();
        // Code blocks visible in the chat-log this frame, in screen-row
        // coordinates, so a click can map to the block under it and copy
        // its untruncated source. Repopulated every frame by the renderer.
        let mut chat_code_blocks: Vec<view_session::CodeBlockRegion> = Vec::new();
        // Click-to-copy command parts visible in the Detail pane this
        // frame, in screen-row coordinates, so a click can map to the
        // part under it and copy that segment. Repopulated every frame.
        let mut detail_copy_blocks: Vec<view_session::DetailCopyRegion> = Vec::new();

        // Promote any pending spawn whose session marker has appeared
        // since the last frame. Identify the new session by diffing the
        // account's current `live_sessions` against the snapshot taken
        // at spawn time. Sessions whose marker hasn't appeared after a
        // generous timeout are abandoned (child probably crashed).
        let mut promotions: Vec<(usize, String)> = Vec::new();
        // Track session_ids already bound to an earlier pending spawn this
        // frame so two concurrent spawns under the same account don't both
        // claim the first new session_id (which would make drivers.insert
        // overwrite — and Drop-kill — the earlier PTY).
        let mut claimed_in_frame: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for (i, ps) in pending_spawns.iter().enumerate() {
            if let Some(pa) = per_account.iter().find(|p| p.account.name == ps.account_name) {
                let new = pa.live_sessions.iter().find(|s| {
                    !ps.snapshot_session_ids.contains(&s.session_id)
                        && !claimed_in_frame
                            .contains(&(ps.account_name.clone(), s.session_id.clone()))
                });
                if let Some(s) = new {
                    claimed_in_frame.insert((ps.account_name.clone(), s.session_id.clone()));
                    promotions.push((i, s.session_id.clone()));
                }
            }
        }
        // Apply promotions in reverse index order so swap_remove indices
        // remain valid.
        for (i, sid) in promotions.into_iter().rev() {
            let ps = pending_spawns.swap_remove(i);
            let key = (ps.account_name.clone(), sid.clone());
            crate::debug_log::log(&format!(
                "driver.promote placeholder={:?} → key={key:?}",
                ps.placeholder_key
            ));
            drivers.insert(key.clone(), ps.pty);
            // If this spawn's placeholder is currently pinned, swap
            // the pin to the real key without changing view mode — the
            // user is already looking at the "starting…" pane.
            // Otherwise (e.g. user navigated away), still auto-pin so
            // the freshly-spawned session is what they see when they
            // come back.
            if let Some(opt) = driver_optimistic.remove(&ps.placeholder_key) {
                driver_optimistic.insert(key.clone(), opt);
            }
            // Seed opt.model from settings.json's `model` field so the
            // user's configured default acts as the persistent intent
            // even when they didn't use mewxi's `m` picker. The model
            // badge then reflects this slug regardless of what claude
            // internally switches to (sub-agents, plan-mode helpers).
            if let Some(pa) = per_account
                .iter()
                .find(|p| p.account.name == key.0)
            {
                if let Some(default_model) = pa.account.default_model() {
                    let opt = driver_optimistic.entry(key.clone()).or_default();
                    if opt.model.is_none() {
                        opt.model = Some(default_model);
                        // No baseline — this seed is the user's
                        // standing preference, not a transient pick to
                        // reconcile against.
                        opt.model_baseline = None;
                    }
                }
            }
            if pinned_session.as_ref() == Some(&ps.placeholder_key) {
                pinned_session = Some(key);
            } else {
                mode = ViewMode::SessionDetail;
                pinned_session = Some(key);
            }
            driver_status = Some((
                format!("driving new session {} ({})", short_sid(&sid), ps.account_name),
                Instant::now(),
            ));
        }
        // Drop pending spawns that took too long to register a marker
        // (most likely the child crashed before writing one).
        let now = Instant::now();
        let mut expired_placeholders: Vec<(String, String)> = Vec::new();
        pending_spawns.retain(|ps| {
            let too_old = now.duration_since(ps.started_at) > Duration::from_secs(15);
            if too_old {
                crate::debug_log::log(&format!(
                    "driver.spawn-timeout placeholder={:?} account={}",
                    ps.placeholder_key, ps.account_name
                ));
                expired_placeholders.push(ps.placeholder_key.clone());
                driver_status = Some((
                    format!(
                        "drive: no session marker appeared under {} within 15s — child crashed?",
                        ps.account_name
                    ),
                    Instant::now(),
                ));
            }
            !too_old
        });
        // Bounce the user back to the all-sessions view if the
        // placeholder they were watching just expired.
        for k in &expired_placeholders {
            driver_optimistic.remove(k);
            overlay_open.remove(k);
            overlay_dismissed_sig.remove(k);
            if pinned_session.as_ref() == Some(k) {
                pinned_session = None;
                mode = ViewMode::AllSessions;
            }
        }

        // Reap drivers whose claude child exited (user hit Ctrl-D, or
        // session ended). Drop the entry so the input row disappears.
        let exited: Vec<(String, String)> = drivers
            .iter_mut()
            .filter_map(|(k, pty)| match pty.try_wait().ok().flatten() {
                Some(_) => Some(k.clone()),
                None => None,
            })
            .collect();
        for k in &exited {
            crate::debug_log::log(&format!("driver.reap key={k:?} reason=child-exit"));
            drivers.remove(k);
            driver_optimistic.remove(k);
            overlay_open.remove(k);
            overlay_dismissed_sig.remove(k);
            driver_send_grace_until.remove(k);
            driver_status = Some((
                format!("driven session {} ended", short_sid(&k.1)),
                Instant::now(),
            ));
            if pinned_session.as_ref() == Some(k) {
                driver_input.clear();
                driver_input_focused = false;
            }
        }

        // Re-pin drivers whose claude child rotated its session_id
        // (happens on `/clear`, `/compact`, etc — same PID, new id).
        // For each driver, match its child PID against per_account's
        // live_sessions; if the matching live_session's id differs from
        // the driver's pinned key, swap keys across every state map and
        // bump pinned_session so the UI follows the new transcript.
        let rotations: Vec<((String, String), String)> = drivers
            .iter()
            .filter_map(|(key, pty)| {
                let pid = pty.child_pid()?;
                let pa = per_account.iter().find(|p| p.account.name == key.0)?;
                let ls = pa.live_sessions.iter().find(|s| s.pid == pid)?;
                if ls.session_id == key.1 {
                    return None;
                }
                Some((key.clone(), ls.session_id.clone()))
            })
            .collect();
        for (old_key, new_sid) in rotations {
            let new_key = (old_key.0.clone(), new_sid.clone());
            crate::debug_log::log(&format!(
                "driver.rotate-session-id old={old_key:?} → new={new_key:?}"
            ));
            if let Some(pty) = drivers.remove(&old_key) {
                drivers.insert(new_key.clone(), pty);
            }
            if let Some(opt) = driver_optimistic.remove(&old_key) {
                driver_optimistic.insert(new_key.clone(), opt);
            }
            if overlay_open.remove(&old_key) {
                overlay_open.insert(new_key.clone());
            }
            if let Some(t) = overlay_dismissed_sig.remove(&old_key) {
                overlay_dismissed_sig.insert(new_key.clone(), t);
            }
            if let Some(t) = driver_send_grace_until.remove(&old_key) {
                driver_send_grace_until.insert(new_key.clone(), t);
            }
            if pinned_session.as_ref() == Some(&old_key) {
                pinned_session = Some(new_key);
            }
        }

        // View-change reset: when the user navigates away from a session
        // (pinned_session changes), drop any dismiss signatures. The
        // "don't re-pop" intent only holds while the user is still
        // looking at the same view — once they leave and come back, a
        // still-up prompt should re-surface its modal.
        if pinned_session != last_pinned_session {
            overlay_dismissed_sig.clear();
            // Different session in the chat pane → previous selection
            // points at unrelated text. Drop it.
            chat_selection = None;
            chat_selection_dragging = false;
            last_pinned_session = pinned_session.clone();
        }
        // Selection only exists in view 2; if the user navigated away,
        // discard it so coming back doesn't restore a stale highlight.
        if mode != ViewMode::SessionDetail && chat_selection.is_some() {
            chat_selection = None;
            chat_selection_dragging = false;
        }

        // Terminal-overlay detection. Sweep every driven session's vt100
        // screen looking for prompt markers; auto-open the overlay when
        // claude is asking for input, auto-close when the prompt clears.
        // A dismissed signature suppresses re-opening for as long as
        // the popup's content hash stays the same — so the same popup
        // never re-pops on its own, but a genuinely different popup
        // (different text, different cursor row, new question) does.
        for (key, pty) in drivers.iter() {
            let awaiting = per_account
                .iter()
                .find(|p| p.account.name == key.0)
                .and_then(|p| p.live_sessions.iter().find(|s| s.session_id == key.1))
                .is_some_and(|s| matches!(s.activity, live_session::Activity::Awaiting));
            let screen = pty.screen_snapshot();
            let visible = terminal_overlay::prompt_visible(&screen, awaiting);
            let was_open = overlay_open.contains(key);
            if visible {
                let current_sig = terminal_overlay::popup_signature(&screen);
                let dismissed = overlay_dismissed_sig.get(key).copied();
                let suppressed = matches!((dismissed, current_sig), (Some(d), Some(c)) if d == c);
                if suppressed {
                    overlay_open.remove(key);
                } else {
                    // Popup content changed since last dismiss → stale
                    // signature, clear it so a future re-dismiss of the
                    // new popup overrides the old hash.
                    if dismissed.is_some() {
                        overlay_dismissed_sig.remove(key);
                    }
                    overlay_open.insert(key.clone());
                    if !was_open {
                        if let Some((row, marker, snippet)) =
                            terminal_overlay::matched_marker(&screen)
                        {
                            crate::debug_log::log(&format!(
                                "overlay.open key={key:?} row={row} marker={marker:?} awaiting={awaiting} snippet={snippet:?}"
                            ));
                        } else {
                            crate::debug_log::log(&format!(
                                "overlay.open key={key:?} awaiting={awaiting} marker=<unknown>"
                            ));
                        }
                    }
                }
            } else {
                overlay_open.remove(key);
                // Popup gone → treat as answered. Drop the stored
                // signature so the next popup (even if identical bytes)
                // gets surfaced.
                overlay_dismissed_sig.remove(key);
                if was_open {
                    crate::debug_log::log(&format!("overlay.close key={key:?} reason=marker-gone"));
                }
            }
        }

        // Compute the driver pane state to hand to view_session.
        let is_driven = mode == ViewMode::SessionDetail
            && pinned_session
                .as_ref()
                .is_some_and(|k| drivers.contains_key(k));
        // Snapshot of the overlay screen for this frame's render, if the
        // pinned session has an active overlay. Computed here so the
        // render closure can borrow it without re-locking the parser.
        let overlay_screen: Option<vt100::Screen> = if is_driven {
            pinned_session
                .as_ref()
                .filter(|k| overlay_open.contains(*k))
                .and_then(|k| drivers.get(k))
                .map(|pty| pty.screen_snapshot())
        } else {
            None
        };
        // Plan content for the active overlay session. Driven by the
        // session's JSONL — specifically, the `input.plan` of the most
        // recent `ExitPlanMode` tool_use that hasn't been resolved by a
        // matching tool_result. This is a protocol-level signal (tool
        // names are part of the API, not UI prose) so it doesn't depend
        // on how claude words the acceptance picker.
        let overlay_plan_content: Option<String> = overlay_screen.as_ref().and_then(|_| {
            let k = pinned_session.as_ref()?;
            let pa = per_account.iter().find(|p| p.account.name == k.0)?;
            let ls = pa.live_sessions.iter().find(|s| s.session_id == k.1)?;
            crate::chat_log::pending_plan_content(&ls.transcript_path)
        });
        let overlay_active_here = pinned_session
            .as_ref()
            .is_some_and(|k| overlay_open.contains(k));
        let mut driver_pane = if is_driven && !overlay_active_here {
            Some(view_session::DriverPane {
                input: driver_input.as_str(),
                cursor: driver_input.cursor_byte(),
                focused: driver_input_focused,
                overlay_active: false,
                scroll: &mut driver_input_scroll,
            })
        } else {
            // Either not driven, or the overlay is up and stealing
            // every keystroke — in both cases the input row would just
            // mislead the user, so hide it.
            if driver_input_focused {
                crate::debug_log::log(&format!(
                    "driver_input.defocus reason={} pinned={:?} overlay_active={}",
                    if !is_driven { "not-driven" } else { "overlay-active" },
                    pinned_session,
                    overlay_active_here,
                ));
            }
            driver_input_focused = false;
            None
        };

        // If the pinned session is a `__pending:` placeholder, look up
        // its `PendingSpawn` so view_session can render a "starting…"
        // pane instead of the usual chat log. The last few bytes of
        // ring output are useful when claude errors out before writing
        // a session marker.
        let pending_pane: Option<view_session::PendingPane> = pinned_session
            .as_ref()
            .filter(|k| k.1.starts_with(PLACEHOLDER_PREFIX))
            .and_then(|k| {
                pending_spawns
                    .iter_mut()
                    .find(|ps| ps.placeholder_key == *k)
                    .map(|ps| {
                        let snapshot = ps.pty.ring_snapshot();
                        let tail = ansi_strip_tail(&snapshot, 400);
                        view_session::PendingPane {
                            account_name: ps.account_name.clone(),
                            cwd: ps.cwd.clone(),
                            elapsed: ps.started_at.elapsed(),
                            last_output: tail,
                        }
                    })
            });

        let combined_message: Option<String> = match (&driver_status, &setup_message) {
            (Some((m, t)), _) if t.elapsed() < Duration::from_secs(8) => Some(m.clone()),
            (_, Some(m)) => Some(m.clone()),
            _ => None,
        };

        // Self-update state for the Config view, rebuilt each frame
        // from the event-loop's owned fields.
        let update_ui = view_setup::UpdateUi {
            channel: update_channel,
            check_enabled: update_check_enabled,
            interval: update_interval,
            prompt_enabled: update_prompt_enabled,
            build_dir: update_build_dir.as_deref(),
            build_dir_edit: update_build_dir_edit.as_ref().map(|i| i.as_str()),
            checking: update_checking,
            status: update_status.as_ref(),
            error: update_error.as_deref(),
        };

        terminal.draw(|f| {
            render(
                f,
                mode,
                &visible_accounts,
                &visible_sessions,
                selected_session,
                &mut chat_scroll,
                &mut changes_selection,
                &mut last_change_count,
                &mut detail_scroll,
                &mut chat_rect,
                &mut actions_rect,
                &mut detail_rect,
                &mut sessions_rect,
                &mut all_table_state,
                &mut setup_rect,
                chat_selection,
                &mut chat_inner,
                &mut chat_visible,
                &mut chat_code_blocks,
                &mut detail_copy_blocks,
                mouse_pos,
                visible_selection,
                selected_account,
                selected_setup,
                &mut setup_scroll,
                setup_snapshot.as_ref(),
                combined_message.as_deref(),
                defocus_input_after_send,
                default_view_pref,
                &update_ui,
                live_error,
                driver_pane.as_mut(),
                pending_pane.as_ref(),
            );
            // Terminal overlay (claude's PTY screen) renders before the
            // mewxi modals so an open modal still wins, but after the
            // base view so it visibly sits on top of the chat-log. The
            // render fn auto-sizes a small box around just the popup
            // region — it does not take over the full mewxi view.
            if let Some(screen) = overlay_screen.as_ref() {
                terminal_overlay::render(f, f.area(), screen, overlay_plan_content.as_deref());
            }
            // Modal overlays everything else when open. Render last so
            // it sits on top with Clear + its own border.
            if let Some(modal) = new_session_modal.as_ref() {
                modal.render(f, f.area());
            }
            if let Some(modal) = model_picker.as_ref() {
                modal.render(f, f.area());
            }
            if let Some(modal) = skill_picker.as_ref() {
                modal.render(f, f.area());
            }
            if let Some(modal) = kill_confirm_modal.as_ref() {
                modal.render(f, f.area());
            }
            if let Some(modal) = composer_modal.as_ref() {
                modal.render(f, f.area());
            }
            if let Some(modal) = update_prompt_modal.as_ref() {
                modal.render(f, f.area());
            }
            // Toasts sit on top of everything (including modals) so transient
            // feedback is always visible in the top-right, whatever the view.
            toasts.prune();
            toasts.render(f, f.area());
        })?;

        // Capture the spawn inputs we'd need on `n` into owned values
        // here, *before* we release the visible_accounts borrow. The
        // data-reload block below needs `per_account.iter_mut()`, which
        // the borrow checker won't allow while visible_accounts (which
        // borrows from per_account) is still alive in the key handler.
        let spawn_candidate_account: Option<Account> = visible_accounts
            .get(selected_account)
            .or_else(|| visible_accounts.first())
            .map(|pa| pa.account.clone());
        drop(visible_accounts);
        // `visible_sessions` borrows from `sessions` (owned, local to
        // this frame), not from `per_account` — so we keep it alive for
        // the keyboard handler's navigation logic.

        // Drain live updates from every poller.
        for (rx, _) in &live_pollers {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    LiveMsg::Update { account_name, live } => {
                        if let Some(pa) =
                            per_account.iter_mut().find(|p| p.account.name == account_name)
                        {
                            pa.live = live;
                        }
                    }
                    LiveMsg::RefreshResult {
                        account_name,
                        ok,
                        detail,
                    } => {
                        if let Some(t) = refresh_tally.as_mut() {
                            t.pending = t.pending.saturating_sub(1);
                            if ok {
                                t.ok += 1;
                            } else {
                                t.failures.push((account_name, detail));
                            }
                            // Live progress: slow accounts (up to the 15s HTTP
                            // timeout) no longer leave the toast frozen — the
                            // user watches "done X/N" climb as each lands.
                            if t.pending > 0 {
                                let done = t.total - t.pending;
                                toasts.push_tagged(
                                    REFRESH_TOAST_TAG,
                                    toast::ToastKind::Info,
                                    format!("refreshing limits… {done}/{} done", t.total),
                                    REFRESH_TIMEOUT + Duration::from_secs(3),
                                );
                            }
                        }
                    }
                }
            }
        }

        // Finalize a manual `r` refresh once every poller has reported (or it
        // timed out waiting on a stuck one), turning the progress toast into a
        // concrete success/failure result.
        if let Some(t) = refresh_tally.as_ref() {
            if t.pending == 0 || t.started.elapsed() >= REFRESH_TIMEOUT {
                let t = refresh_tally.take().expect("checked Some above");
                let failed = t.failures.len();
                let (kind, msg) = if failed == 0 {
                    (
                        toast::ToastKind::Success,
                        format!("refreshed limits for {} account(s)", t.ok),
                    )
                } else if t.ok == 0 {
                    // Reasons are usually identical across accounts; show the
                    // first so the toast stays one line.
                    let reason = &t.failures[0].1;
                    (
                        toast::ToastKind::Error,
                        format!("refresh failed for {failed} account(s): {reason}"),
                    )
                } else {
                    (
                        toast::ToastKind::Error,
                        format!("refreshed {} account(s), {failed} failed", t.ok),
                    )
                };
                toasts.push_tagged(REFRESH_TOAST_TAG, kind, msg, Duration::from_millis(6000));
            }
        }

        // Drain filesystem events.
        while let Ok(name) = dirty_rx.try_recv() {
            dirty.insert(name);
        }

        // Drain the self-update check result (at most one per spawn).
        while let Ok(res) = update_rx.try_recv() {
            update_checking = false;
            match res {
                Ok(s) => {
                    update_error = None;
                    if s.available
                        && update_prompt_enabled
                        && !update_prompted
                        && update_prompt_modal.is_none()
                    {
                        update_prompt_modal = Some(UpdatePromptModal::new(s.clone()));
                        update_prompted = true;
                    }
                    update_status = Some(s);
                }
                Err(e) => update_error = Some(e),
            }
        }

        // Per-account debounced reload, plus a periodic safety-net refresh
        // for live_sessions (mtime age changes purely by clock).
        let force_tick = last_full_tick.elapsed() > Duration::from_secs(5);
        if !dirty.is_empty() || force_tick {
            let names: Vec<String> = if force_tick {
                per_account.iter().map(|p| p.account.name.clone()).collect()
            } else {
                dirty.iter().cloned().collect()
            };
            // Snapshot live PIDs once per debounce wave so the marker
            // liveness gate doesn't shell out per account.
            let alive = live_session::alive_pids();
            for name in names {
                let stale = last_reload
                    .get(&name)
                    .map(|t| t.elapsed() > Duration::from_millis(500))
                    .unwrap_or(true);
                if !stale {
                    continue;
                }
                if let Some(pa) = per_account.iter_mut().find(|p| p.account.name == name) {
                    pa.agg = stats::load_and_aggregate_for(&pa.account).unwrap_or_default();
                    pa.live_sessions = live_session::scan(&pa.account, &alive, &pa.live_sessions);
                    last_reload.insert(name.clone(), Instant::now());
                }
                dirty.remove(&name);
            }
            if force_tick {
                last_full_tick = Instant::now();
            }
            // Reconcile optimistic mode state: once the transcript moves
            // past the baseline snapshotted at command time, claude has
            // caught up and the optimistic guess is obsolete.
            //
            // opt.model is NOT auto-cleared anymore. claude internally
            // switches model for plan-mode helpers, sub-agents, etc.,
            // and the user's mewxi pick (or settings.json default) is
            // the source of truth for what the badge should show. The
            // "via <model>" indicator (LiveSession::active_model)
            // surfaces internal model use without overwriting the
            // user's choice.
            driver_optimistic.retain(|key, opt| {
                let ls = per_account
                    .iter()
                    .find(|p| p.account.name == key.0)
                    .and_then(|p| p.live_sessions.iter().find(|s| s.session_id == key.1));
                if let Some(ls) = ls {
                    if opt.mode.is_some()
                        && opt.mode_baseline.as_deref() != ls.permission_mode.as_deref()
                    {
                        // Teach the learner what claude actually did
                        // before we forget the `prev` snapshot. The
                        // transcript's new value is authoritative; our
                        // optimistic `opt.mode` may have been wrong,
                        // which is exactly the case learning fixes.
                        if let (Some(prev), Some(auto), Some(actual)) = (
                            opt.cycle_prev.as_deref(),
                            opt.cycle_auto,
                            ls.permission_mode.as_deref(),
                        ) {
                            mode_cycle.observe(prev, actual, auto);
                        }
                        opt.mode = None;
                        opt.mode_baseline = None;
                        opt.cycle_prev = None;
                        opt.cycle_auto = None;
                    }
                }
                opt.mode.is_some() || opt.model.is_some()
            });
        }

        // Keyboard. Mewxi view animates the logo every frame, so it
        // polls at ~60 fps for buttery-smooth gradient + bob sampling;
        // every other view keeps the original 200ms budget — no point
        // burning CPU on a static screen.
        let poll_timeout = if mode == ViewMode::Mewxi {
            Duration::from_millis(16)
        } else if !overlay_open.is_empty() {
            // While an overlay is up the user is actively driving claude
            // through the PTY; redraw fast so arrow-key feedback feels
            // immediate, otherwise the perceived input lag piles up
            // (200ms poll + claude's redraw + next render).
            Duration::from_millis(33)
        } else {
            Duration::from_millis(200)
        };
        if event::poll(poll_timeout)? {
            let evt = event::read()?;
            if let Event::Mouse(m) = &evt {
                // Track the cursor for hover highlighting. Scroll events
                // shift the rows under the cursor, so drop the remembered
                // position then (the highlight recomputes next hover).
                mouse_pos = match m.kind {
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => None,
                    _ => Some((m.column, m.row)),
                };
                let dir: i32 = match m.kind {
                    MouseEventKind::ScrollUp => -1,
                    MouseEventKind::ScrollDown => 1,
                    _ => 0,
                };
                if dir != 0 {
                    // Any scroll invalidates the highlight — its
                    // anchor is a screen cell, and the row underneath
                    // is about to change.
                    chat_selection = None;
                    chat_selection_dragging = false;
                    handle_scroll(
                        dir,
                        m.column,
                        m.row,
                        mode,
                        &mut chat_scroll,
                        &mut changes_selection,
                        last_change_count,
                        &mut detail_scroll,
                        chat_rect,
                        actions_rect,
                        detail_rect,
                        sessions_rect,
                        setup_rect,
                        &mut selected_session,
                        &mut selected_setup,
                        &mut last_session_select,
                        &visible_sessions,
                        sessions.len(),
                        view_setup::items(setup_snapshot.as_ref()).len(),
                        &mut pinned_session,
                    );
                }
                // Drag-select inside the chat-log pane. Only fires in
                // view 2; clamped to the inner (border-stripped) rect
                // so the selection never bleeds onto adjacent panes.
                if mode == ViewMode::SessionDetail {
                    if let Some(inner) = chat_inner {
                        let in_chat = m.column >= inner.x
                            && m.column < inner.x + inner.width
                            && m.row >= inner.y
                            && m.row < inner.y + inner.height;
                        let clamp = |col: u16, row: u16| -> (u16, u16) {
                            let c = col
                                .clamp(inner.x, inner.x + inner.width.saturating_sub(1));
                            let r = row
                                .clamp(inner.y, inner.y + inner.height.saturating_sub(1));
                            (c, r)
                        };
                        match m.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                // A click landing on a code block copies
                                // the whole block's untruncated source as
                                // one chunk (ready to paste into a shell)
                                // instead of starting a text drag.
                                let hit = if in_chat {
                                    chat_code_blocks
                                        .iter()
                                        .find(|b| m.row >= b.top && m.row <= b.bottom)
                                } else {
                                    None
                                };
                                if let Some(block) = hit {
                                    chat_selection = None;
                                    chat_selection_dragging = false;
                                    let src = block.source.clone();
                                    let lines = src.lines().count().max(1);
                                    match arboard::Clipboard::new()
                                        .and_then(|mut c| c.set_text(src))
                                    {
                                        Ok(()) => {
                                            toasts.success(format!(
                                                "copied code block ({lines} line{})",
                                                if lines == 1 { "" } else { "s" }
                                            ));
                                        }
                                        Err(e) => {
                                            toasts.error(format!("clipboard error: {e}"));
                                        }
                                    }
                                } else if in_chat {
                                    let p = clamp(m.column, m.row);
                                    chat_selection = Some(
                                        view_session::ChatSelection {
                                            anchor: p,
                                            cursor: p,
                                        },
                                    );
                                    chat_selection_dragging = true;
                                } else {
                                    chat_selection = None;
                                    chat_selection_dragging = false;
                                }
                            }
                            MouseEventKind::Drag(MouseButton::Left) => {
                                if chat_selection_dragging {
                                    if let Some(sel) = chat_selection.as_mut() {
                                        sel.cursor = clamp(m.column, m.row);
                                    }
                                }
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                if chat_selection_dragging {
                                    chat_selection_dragging = false;
                                    if let Some(sel) = chat_selection {
                                        let text = extract_chat_selection_text(
                                            &chat_visible,
                                            inner,
                                            sel,
                                        );
                                        if !text.is_empty() {
                                            match arboard::Clipboard::new()
                                                .and_then(|mut c| c.set_text(text.clone()))
                                            {
                                                Ok(()) => {
                                                    toasts.success(format!(
                                                        "copied {} chars",
                                                        text.chars().count()
                                                    ));
                                                }
                                                Err(e) => {
                                                    toasts.error(format!(
                                                        "clipboard error: {e}"
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    // Click-to-copy a single Bash command part in the
                    // Detail pane. Each rendered segment is a clickable
                    // region (projected to screen rows by the renderer);
                    // clicking one drops just that part's runnable text on
                    // the clipboard. Gated on `detail_rect` so a same-row
                    // click in the chat pane to the left can't match it.
                    if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                        if hit(detail_rect, m.column, m.row) {
                            if let Some(region) = detail_copy_blocks
                                .iter()
                                .find(|b| m.row >= b.top && m.row <= b.bottom)
                            {
                                let src = region.source.clone();
                                let chars = src.chars().count();
                                match arboard::Clipboard::new()
                                    .and_then(|mut c| c.set_text(src))
                                {
                                    Ok(()) => {
                                        toasts.success(format!(
                                            "copied command part ({chars} chars)"
                                        ));
                                    }
                                    Err(e) => {
                                        toasts.error(format!("clipboard error: {e}"));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Event::Key(k) = evt {
                if k.kind == KeyEventKind::Press {
                    crate::debug_log::log(&format!(
                        "key code={:?} mods={:?} mode={:?} pinned={:?} overlay_open_n={} focused={} modals: picker={} new={}",
                        k.code,
                        k.modifiers,
                        mode,
                        pinned_session,
                        overlay_open.len(),
                        driver_input_focused,
                        model_picker.is_some(),
                        new_session_modal.is_some(),
                    ));
                    // Terminal overlay: when an overlay is open for the
                    // currently-pinned driven session, every keystroke
                    // goes verbatim to the PTY so the user can answer
                    // claude's prompt naturally (y/n, arrows, Enter).
                    // The only mewxi-reserved key is F10, which
                    // dismisses the overlay and records the popup's
                    // content signature — re-detection stays suppressed
                    // until the popup changes or vanishes.
                    if mode == ViewMode::SessionDetail {
                        if let Some(key) = pinned_session
                            .as_ref()
                            .filter(|k| overlay_open.contains(*k))
                            .cloned()
                        {
                            let is_dismiss = matches!(k.code, KeyCode::F(10));
                            if is_dismiss {
                                crate::debug_log::log(&format!(
                                    "overlay.close key={key:?} reason=f10"
                                ));
                                overlay_open.remove(&key);
                                if let Some(sig) = drivers
                                    .get(&key)
                                    .and_then(|pty| terminal_overlay::popup_signature(&pty.screen_snapshot()))
                                {
                                    overlay_dismissed_sig.insert(key, sig);
                                }
                            } else if let Some(pty) = drivers.get_mut(&key) {
                                if let Err(e) = pty.send_key_event(k) {
                                    driver_status =
                                        Some((format!("overlay send failed: {e}"), Instant::now()));
                                }
                            }
                            continue;
                        }
                    }
                    // Update prompt modal owns every keystroke while open.
                    if let Some(modal) = update_prompt_modal.as_ref() {
                        match modal.handle_key(k) {
                            UpdatePromptOutcome::Stay => {}
                            UpdatePromptOutcome::NotNow => {
                                update_prompt_modal = None;
                            }
                            UpdatePromptOutcome::DisableStartupPrompt => {
                                update_prompt_modal = None;
                                match accounts::set_update_prompt(false) {
                                    Ok(()) => {
                                        update_prompt_enabled = false;
                                        setup_message = Some(
                                            "startup update prompt disabled — re-enable it in Config (4)"
                                                .into(),
                                        );
                                    }
                                    Err(e) => {
                                        setup_message =
                                            Some(format!("failed to save preference: {e}"));
                                    }
                                }
                            }
                            UpdatePromptOutcome::UpdateNow => {
                                update_prompt_modal = None;
                                let (msg, updated) = run_update_apply(terminal);
                                // The cache was rewritten by apply; drop
                                // the stale in-memory status so the
                                // Config view doesn't keep advertising
                                // the update we just installed.
                                update_status = None;
                                update_error = None;
                                setup_message = Some(msg);
                                if updated {
                                    for (_, cmd_tx) in &live_pollers {
                                        let _ = cmd_tx.send(LiveCmd::Stop);
                                    }
                                    restart_after_update = true;
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                    // Build-dir edit on the Config view owns every
                    // keystroke while active: Enter saves, Esc cancels,
                    // everything else edits the path.
                    if let Some(input) = update_build_dir_edit.as_mut() {
                        match k.code {
                            KeyCode::Enter => {
                                let dir = input.as_str().trim().to_string();
                                update_build_dir_edit = None;
                                match accounts::set_update_build_dir(&dir) {
                                    Ok(()) => {
                                        update_build_dir =
                                            if dir.is_empty() { None } else { Some(dir.clone()) };
                                        setup_message = Some(match update_build_dir.as_deref() {
                                            Some(d) => format!("update build dir → {d}"),
                                            None => format!(
                                                "update build dir → system temp ({})",
                                                std::env::temp_dir().display()
                                            ),
                                        });
                                    }
                                    Err(e) => {
                                        setup_message =
                                            Some(format!("failed to save build dir: {e}"));
                                    }
                                }
                            }
                            KeyCode::Esc => {
                                update_build_dir_edit = None;
                                setup_message = Some("build dir edit cancelled".into());
                            }
                            _ => {
                                let _ = input.handle_edit_key(k);
                            }
                        }
                        continue;
                    }
                    // Kill confirm modal owns every keystroke while open.
                    if let Some(modal) = kill_confirm_modal.as_ref() {
                        match modal.handle_key(k) {
                            KillConfirmOutcome::Cancel => {
                                kill_confirm_modal = None;
                            }
                            KillConfirmOutcome::Confirm => {
                                let acct = modal.acct.clone();
                                let sid = modal.sid.clone();
                                let pid = modal.pid;
                                kill_confirm_modal = None;
                                let key = (acct.clone(), sid.clone());
                                let msg = if let Some(mut pty) = drivers.remove(&key) {
                                    let _ = pty.kill();
                                    format!(
                                        "killed driven session {} (pid {})",
                                        short_sid(&sid),
                                        pid
                                    )
                                } else {
                                    match crate::platform::terminate_pid(pid) {
                                        Ok(s) if s.success() => format!(
                                            "terminated {} (pid {})",
                                            short_sid(&sid),
                                            pid
                                        ),
                                        Ok(s) => format!(
                                            "kill {pid} exited {}",
                                            s.code()
                                                .map(|c| c.to_string())
                                                .unwrap_or_else(|| "signal".into())
                                        ),
                                        Err(e) => format!("kill {pid} failed: {e}"),
                                    }
                                };
                                driver_status = Some((msg, Instant::now()));
                                mark_killing(
                                    &mut killing_sessions,
                                    &sessions,
                                    &acct,
                                    &sid,
                                    pid,
                                );
                                // Leave the session view if it was showing
                                // the agent we just closed — its pane is
                                // about to go dead.
                                if mode == ViewMode::SessionDetail
                                    && pinned_session.as_ref() == Some(&key)
                                {
                                    mode = ViewMode::AllSessions;
                                    pinned_session = None;
                                    last_session_select = Instant::now();
                                }
                            }
                            KillConfirmOutcome::Stay => {}
                        }
                        continue;
                    }
                    // Status-line composer owns every keystroke while open.
                    if let Some(modal) = composer_modal.as_mut() {
                        let outcome = modal.handle_key(k);
                        // `modal` is not used past this point, so the
                        // borrow ends here and we can mutate composer_modal
                        // (mirrors the model-picker pattern below).
                        match outcome {
                            ComposerOutcome::Stay => {}
                            ComposerOutcome::Cancel => composer_modal = None,
                            ComposerOutcome::Save(order) => {
                                setup_message = Some(match accounts::set_status_blocks(&order) {
                                    Ok(()) => "status line blocks saved".to_string(),
                                    Err(e) => format!("failed to save status blocks: {e}"),
                                });
                                composer_modal = None;
                            }
                            ComposerOutcome::EditExternally { id, is_builtin } => {
                                edit_status_block_in_editor(
                                    terminal,
                                    &view,
                                    &id,
                                    is_builtin,
                                    &mut composer_modal,
                                );
                            }
                            ComposerOutcome::NewBlock(id) => {
                                // New block: always a fresh user file.
                                edit_status_block_in_editor(
                                    terminal,
                                    &view,
                                    &id,
                                    false,
                                    &mut composer_modal,
                                );
                            }
                        }
                        continue;
                    }
                    // Model picker owns every keystroke while open.
                    // Dispatched ahead of the new-session modal,
                    // driver input, and globals — the two modals are
                    // mutually exclusive but this ordering documents
                    // the precedence.
                    if let Some(modal) = model_picker.as_mut() {
                        // Unify Confirm and ConfirmAsDefault into one
                        // (slug, effort_opt, persist_default) tuple so
                        // the PTY-send body below isn't duplicated.
                        // ConfirmAsDefault adds a settings.json write
                        // *after* the in-session sends, so the live
                        // session reflects the change even if the
                        // settings write later fails.
                        let action: Option<(String, Option<String>, bool)> =
                            match modal.handle_key(k) {
                                ModelOutcome::Stay => None,
                                ModelOutcome::Cancel => {
                                    model_picker = None;
                                    None
                                }
                                ModelOutcome::Confirm { slug, effort } => {
                                    model_picker = None;
                                    Some((slug, effort, false))
                                }
                                ModelOutcome::ConfirmAsDefault { slug, effort } => {
                                    model_picker = None;
                                    Some((slug, Some(effort), true))
                                }
                            };
                        if let Some((slug, effort, persist_default)) = action {
                            if let Some(key) = pinned_session.clone() {
                                if let Some(pty) = drivers.get_mut(&key) {
                                    let mut bytes = format!("/model {slug}").into_bytes();
                                    bytes.push(b'\r');
                                    match pty.send_keys(&bytes) {
                                        Ok(_) => {
                                            let ls = per_account
                                                .iter()
                                                .find(|p| p.account.name == key.0)
                                                .and_then(|p| {
                                                    p.live_sessions
                                                        .iter()
                                                        .find(|s| s.session_id == key.1)
                                                });
                                            let model_baseline = ls
                                                .map(|s| s.model.clone())
                                                .unwrap_or_default();
                                            let transcript_mode =
                                                ls.and_then(|s| s.permission_mode.clone());
                                            let opt = driver_optimistic
                                                .entry(key.clone())
                                                .or_default();
                                            let prior_mode = opt
                                                .mode
                                                .clone()
                                                .or_else(|| transcript_mode.clone());
                                            opt.model = Some(slug.clone());
                                            opt.model_baseline = Some(model_baseline);
                                            // Mirror claude's
                                            // auto-downgrade so a
                                            // stale "auto" doesn't
                                            // resurface when the
                                            // user later switches
                                            // back to a model that
                                            // does support it.
                                            if prior_mode.as_deref() == Some("auto")
                                                && !model_supports_auto(&slug)
                                            {
                                                opt.mode = Some("default".into());
                                                opt.mode_baseline = transcript_mode;
                                            }
                                            // Send `/effort X\r` after
                                            // the model change so the
                                            // second command lands
                                            // once claude's already
                                            // processed the model
                                            // switch. `/effort` itself
                                            // is session-only — the
                                            // settings.json write
                                            // below (when
                                            // `persist_default`) is
                                            // what makes it stick.
                                            let mut status_msg = format!("model → {slug}");
                                            if let Some(eff) = effort.as_deref() {
                                                let mut eb =
                                                    format!("/effort {eff}").into_bytes();
                                                eb.push(b'\r');
                                                match pty.send_keys(&eb) {
                                                    Ok(_) => {
                                                        opt.effort = Some(eff.to_string());
                                                        status_msg = format!(
                                                            "model → {slug}  ·  effort → {eff}"
                                                        );
                                                    }
                                                    Err(e) => {
                                                        status_msg = format!(
                                                            "model → {slug}  ·  effort send failed: {e}"
                                                        );
                                                    }
                                                }
                                            }
                                            // Persist BOTH the model
                                            // and effort as the
                                            // account's startup
                                            // defaults. This is the
                                            // whole point of `d`: a
                                            // brand-new session reads
                                            // `model` + `effortLevel`
                                            // from settings.json, so
                                            // saving only one half left
                                            // new sessions on the stale
                                            // other half (e.g. the
                                            // account stuck at
                                            // `opus:low`). We persist
                                            // regardless of whether the
                                            // live `/effort` send
                                            // landed — the on-disk
                                            // default is independent of
                                            // this session's state.
                                            if persist_default {
                                                if let Some(pa) = per_account
                                                    .iter()
                                                    .find(|p| p.account.name == key.0)
                                                {
                                                    let model_res =
                                                        crate::accounts::set_default_model(
                                                            &pa.account,
                                                            &slug,
                                                        );
                                                    let effort_res = effort
                                                        .as_deref()
                                                        .map(|eff| {
                                                            crate::accounts::set_default_effort(
                                                                &pa.account,
                                                                eff,
                                                            )
                                                        });
                                                    let eff_label = effort
                                                        .as_deref()
                                                        .unwrap_or("default");
                                                    match (model_res, effort_res) {
                                                        (Ok(()), None | Some(Ok(()))) => {
                                                            status_msg = format!(
                                                                "model → {slug}  ·  effort → {eff_label} (saved as default)"
                                                            );
                                                        }
                                                        (Err(e), _) | (_, Some(Err(e))) => {
                                                            status_msg = format!(
                                                                "model → {slug}  ·  effort → {eff_label}  ·  default save failed: {e}"
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                            driver_status =
                                                Some((status_msg, Instant::now()));
                                        }
                                        Err(e) => {
                                            driver_status = Some((
                                                format!("model send failed: {e}"),
                                                Instant::now(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    // Skill picker owns every keystroke while open. On
                    // confirm we inject `/<name>\r` into the driven PTY
                    // exactly the way the user would type it themselves;
                    // claude resolves the rest (slash-command lookup,
                    // skill expansion).
                    if let Some(modal) = skill_picker.as_mut() {
                        match modal.handle_key(k) {
                            SkillOutcome::Stay => {}
                            SkillOutcome::Cancel => {
                                skill_picker = None;
                            }
                            SkillOutcome::Confirm { name } => {
                                skill_picker = None;
                                if let Some(key) = pinned_session.clone() {
                                    if let Some(pty) = drivers.get_mut(&key) {
                                        let mut bytes =
                                            format!("/{name}").into_bytes();
                                        bytes.push(b'\r');
                                        match pty.send_keys(&bytes) {
                                            Ok(_) => {
                                                // Same slash-command grace
                                                // window we arm after a
                                                // manual /command send —
                                                // claude flushes stdin
                                                // briefly on these.
                                                driver_send_grace_until.insert(
                                                    key.clone(),
                                                    Instant::now()
                                                        + Duration::from_millis(1500),
                                                );
                                                driver_status = Some((
                                                    format!("/{name} sent"),
                                                    Instant::now(),
                                                ));
                                            }
                                            Err(e) => {
                                                driver_status = Some((
                                                    format!("skill send failed: {e}"),
                                                    Instant::now(),
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    // New-session modal owns every keystroke while open.
                    // We dispatch first, before driver input or global
                    // shortcuts, so that the modal's own Esc/Tab/typing
                    // can never fall through and quit mewxi or move
                    // navigation in the underlying view.
                    if let Some(modal) = new_session_modal.as_mut() {
                        match modal.handle_key(k) {
                            ModalOutcome::Stay => {}
                            ModalOutcome::Cancel => {
                                new_session_modal = None;
                            }
                            ModalOutcome::Confirm { account, cwd, resume_session_id } => {
                                new_session_modal = None;
                                let snapshot: HashSet<String> = per_account
                                    .iter()
                                    .find(|p| p.account.name == account.name)
                                    .map(|p| {
                                        p.live_sessions
                                            .iter()
                                            .map(|s| s.session_id.clone())
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                let bin = agent_control::resolve_claude_bin(&account);
                                let resume_for_status = resume_session_id.clone();
                                match PtySession::spawn(&account, cwd.clone(), bin, resume_session_id) {
                                    Ok(pty) => {
                                        let spawn_id = next_spawn_id;
                                        next_spawn_id += 1;
                                        crate::debug_log::log(&format!(
                                            "driver.spawn account={} cwd={:?} spawn_id={spawn_id} snapshot_n={}",
                                            account.name,
                                            cwd,
                                            snapshot.len()
                                        ));
                                        let placeholder_key = (
                                            account.name.clone(),
                                            format!("{}{}", PLACEHOLDER_PREFIX, spawn_id),
                                        );
                                        // Instant pin: switch to the
                                        // session detail view with a
                                        // placeholder pinned. The
                                        // promotion loop swaps it for
                                        // the real session_id once the
                                        // marker file appears.
                                        mode = ViewMode::SessionDetail;
                                        pinned_session = Some(placeholder_key.clone());
                                        let verb = if resume_for_status.is_some() {
                                            "resuming"
                                        } else {
                                            "spawning"
                                        };
                                        driver_status = Some((
                                            format!(
                                                "{verb} claude under {} in {}",
                                                account.name,
                                                cwd.display()
                                            ),
                                            Instant::now(),
                                        ));
                                        pending_spawns.push(PendingSpawn {
                                            account_name: account.name.clone(),
                                            snapshot_session_ids: snapshot,
                                            pty,
                                            started_at: Instant::now(),
                                            cwd,
                                            placeholder_key,
                                        });
                                    }
                                    Err(e) => {
                                        driver_status = Some((
                                            format!("spawn failed: {e}"),
                                            Instant::now(),
                                        ));
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    // Driver input mode owns the keyboard exclusively: chars
                    // become PTY keystrokes, Enter submits, Esc unfocuses,
                    // Ctrl-D ends the session, Ctrl-C interrupts claude
                    // (and discards any composed input).
                    if driver_input_focused {
                        if let Some(key) = pinned_session.clone() {
                            if let Some(pty) = drivers.get_mut(&key) {
                                match (k.code, k.modifiers) {
                                    (KeyCode::Esc, _) => {
                                        driver_input_focused = false;
                                    }
                                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                                        // ESC byte → claude treats it as
                                        // "interrupt the current request"
                                        // (the same key the standalone CLI
                                        // uses). Clear the input too so a
                                        // single keystroke aborts both the
                                        // in-flight run and the message
                                        // the user was composing.
                                        driver_input.clear();
                                        match pty.cancel_execution() {
                                            Ok(_) => {
                                                driver_status = Some((
                                                    "cancel sent — claude should interrupt".into(),
                                                    Instant::now(),
                                                ));
                                            }
                                            Err(e) => {
                                                driver_status = Some((
                                                    format!("cancel failed: {e}"),
                                                    Instant::now(),
                                                ));
                                            }
                                        }
                                    }
                                    (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                                        let _ = pty.kill();
                                        driver_input.clear();
                                        driver_input_focused = false;
                                    }
                                    (KeyCode::Enter, _) => {
                                        if driver_input.is_empty() {
                                            crate::debug_log::log(&format!(
                                                "driver_input.enter-empty key={key:?}"
                                            ));
                                        } else {
                                            let screen = pty.screen_snapshot();
                                            // Slash-command grace gate: if a
                                            // previous send armed a deadline,
                                            // hold until it passes. Avoids
                                            // racing claude's post-/clear
                                            // internal reset where stdin is
                                            // briefly drained-and-discarded.
                                            let grace_blocks = driver_send_grace_until
                                                .get(&key)
                                                .is_some_and(|t| Instant::now() < *t);
                                            if grace_blocks {
                                                crate::debug_log::log(&format!(
                                                    "driver_input.send-rejected key={key:?} reason=slash-grace"
                                                ));
                                                driver_status = Some((
                                                    "wait — claude is still settling after the last slash command".into(),
                                                    Instant::now(),
                                                ));
                                                continue;
                                            }
                                            // Render the screen to plain rows
                                            // for diagnostic logging. Shows
                                            // what claude is actually
                                            // *displaying* (popup overlays,
                                            // slash-command suggestions etc.),
                                            // unlike the ring which is raw
                                            // ANSI noise.
                                            let (rows, cols) = screen.size();
                                            let mut screen_dump = String::new();
                                            for r in 0..rows {
                                                let mut row_str = String::with_capacity(cols as usize);
                                                for c in 0..cols {
                                                    match screen.cell(r, c) {
                                                        Some(cell) if cell.has_contents() => {
                                                            row_str.push_str(cell.contents())
                                                        }
                                                        _ => row_str.push(' '),
                                                    }
                                                }
                                                screen_dump.push_str(row_str.trim_end());
                                                screen_dump.push('\n');
                                            }
                                            crate::debug_log::log(&format!(
                                                "driver_input.screen-at-send key={key:?}\n----SCREEN----\n{}----END----",
                                                screen_dump
                                            ));
                                            let text_bytes =
                                                driver_input.as_str().as_bytes().to_vec();
                                            let preview: String =
                                                driver_input.as_str().chars().take(60).collect();
                                            crate::debug_log::log(&format!(
                                                "driver_input.send key={key:?} bytes_len={} preview={:?}",
                                                text_bytes.len() + 1,
                                                preview
                                            ));
                                            let ring_before = pty.ring_snapshot();
                                            let tail_before: String =
                                                String::from_utf8_lossy(
                                                    &ring_before[ring_before
                                                        .len()
                                                        .saturating_sub(160)..],
                                                )
                                                .chars()
                                                .filter(|c| !c.is_control() || *c == '\n')
                                                .collect();
                                            crate::debug_log::log(&format!(
                                                "driver_input.ring-tail-before key={key:?} tail={tail_before:?}"
                                            ));
                                            let was_slash_cmd =
                                                driver_input.as_str().starts_with('/');
                                            // Split text + \r into TWO
                                            // writes with a tiny pause —
                                            // claude's input handler
                                            // treats a multi-byte chunk
                                            // as a paste and inserts the
                                            // trailing \r as a literal
                                            // newline (multi-line input)
                                            // instead of submitting.
                                            // Separating the \r makes it
                                            // arrive in a distinct read()
                                            // and register as a discrete
                                            // Enter keystroke that
                                            // triggers submit.
                                            let send_result = pty
                                                .send_keys(&text_bytes)
                                                .and_then(|_| {
                                                    std::thread::sleep(
                                                        Duration::from_millis(30),
                                                    );
                                                    pty.send_keys(b"\r")
                                                });
                                            match send_result {
                                                Ok(_) => {
                                                    crate::debug_log::log(&format!(
                                                        "driver_input.send-ok key={key:?} slash={was_slash_cmd}"
                                                    ));
                                                    driver_input.clear();
                                                    if defocus_input_after_send {
                                                        driver_input_focused = false;
                                                    }
                                                    // Arm a grace window
                                                    // after a slash command
                                                    // so the NEXT send
                                                    // (rejected via the
                                                    // gate above) doesn't
                                                    // race claude's
                                                    // internal reset.
                                                    if was_slash_cmd {
                                                        driver_send_grace_until.insert(
                                                            key.clone(),
                                                            Instant::now()
                                                                + Duration::from_millis(
                                                                    1500,
                                                                ),
                                                        );
                                                    }
                                                    driver_status = Some((
                                                        "prompt sent".into(),
                                                        Instant::now(),
                                                    ));
                                                }
                                                Err(e) => {
                                                    crate::debug_log::log(&format!(
                                                        "driver_input.send-err key={key:?} err={e}"
                                                    ));
                                                    driver_status = Some((
                                                        format!("send failed: {e}"),
                                                        Instant::now(),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    // Ctrl-E — pop into the user's
                                    // external editor (config `editor`
                                    // field, then $VISUAL/$EDITOR/vim)
                                    // pre-seeded with the current
                                    // composer text. On save the
                                    // content (incl. newlines) lands
                                    // back in the buffer; the user
                                    // hits Enter to send. This takes
                                    // priority over text_input's
                                    // Ctrl-E=move-to-end binding (use
                                    // End key for that in the driver
                                    // composer).
                                    (KeyCode::Char('e'), m) if m.contains(KeyModifiers::CONTROL) => {
                                        let initial = driver_input.as_str().to_owned();
                                        let account_opt = per_account
                                            .iter()
                                            .find(|p| p.account.name == key.0)
                                            .map(|p| p.account.clone());
                                        let Some(account) = account_opt else {
                                            driver_status = Some((
                                                "editor: account not found".into(),
                                                Instant::now(),
                                            ));
                                            continue;
                                        };
                                        match open_editor_for_input(
                                            terminal,
                                            &account,
                                            &initial,
                                        ) {
                                            Ok(Some(new)) => {
                                                driver_input.set(new);
                                                driver_status = Some((
                                                    "loaded from editor — Enter to send"
                                                        .into(),
                                                    Instant::now(),
                                                ));
                                            }
                                            Ok(None) => {}
                                            Err(e) => {
                                                crate::debug_log::log(&format!(
                                                    "driver_input.editor-err key={key:?} err={e}"
                                                ));
                                                driver_status = Some((
                                                    format!("editor failed: {e}"),
                                                    Instant::now(),
                                                ));
                                            }
                                        }
                                    }
                                    // Shift-Tab. crossterm reports it
                                    // as either `BackTab` (legacy
                                    // terminals) or `(Tab, SHIFT)`
                                    // (terminals that have negotiated
                                    // a disambiguating keyboard
                                    // protocol). Both forms cycle
                                    // claude's permission mode by
                                    // forwarding ANSI CSI Z to the
                                    // PTY. The next transcript scan
                                    // picks the new mode up.
                                    (KeyCode::BackTab, _) => {
                                        cycle_mode_via_pty(
                                            pty,
                                            &key,
                                            &per_account,
                                            &mut driver_optimistic,
                                            &mode_cycle,
                                            &mut driver_status,
                                        );
                                    }
                                    (KeyCode::Tab, m) if m.contains(KeyModifiers::SHIFT) => {
                                        cycle_mode_via_pty(
                                            pty,
                                            &key,
                                            &per_account,
                                            &mut driver_optimistic,
                                            &mode_cycle,
                                            &mut driver_status,
                                        );
                                    }
                                    // Everything else routes through the
                                    // shared readline-style editor:
                                    // char insert, Backspace/Delete,
                                    // arrows + Ctrl/Alt arrows for word
                                    // jumps, Home/End, Ctrl-A/E/W/U/K,
                                    // Alt-B/F/D, Ctrl-H. Unrecognised
                                    // Ctrl/Alt chords return Passthrough
                                    // and are dropped here, so e.g.
                                    // Ctrl-L doesn't append `l` to the
                                    // buffer.
                                    _ => {
                                        let _ = driver_input.handle_edit_key(k);
                                    }
                                }
                                continue;
                            }
                        }
                        // Pinned session was reaped while focused — drop focus
                        // and consume this keystroke so the global handler
                        // doesn't misinterpret it (e.g. `q` would quit
                        // mewxi, Esc would close the pin) when the user
                        // was mid-type into the driver input.
                        driver_input_focused = false;
                        continue;
                    }
                    // Shift-Tab handled here regardless of `k.code`
                    // form because terminals disagree: some send
                    // `BackTab`, others `(Tab, SHIFT)`. We unify
                    // before falling into the `match k.code` arms
                    // below, which would otherwise route a
                    // `(Tab, SHIFT)` into the forward-cycle Tab arm
                    // and do the opposite of what the user wanted.
                    if matches!(k.code, KeyCode::BackTab)
                        || (matches!(k.code, KeyCode::Tab)
                            && k.modifiers.contains(KeyModifiers::SHIFT))
                    {
                        let driven = mode == ViewMode::SessionDetail
                            && pinned_session
                                .as_ref()
                                .is_some_and(|k| drivers.contains_key(k));
                        if driven {
                            if let Some(key) = pinned_session.clone() {
                                if let Some(pty) = drivers.get_mut(&key) {
                                    cycle_mode_via_pty(
                                        pty,
                                        &key,
                                        &per_account,
                                        &mut driver_optimistic,
                                        &mode_cycle,
                                        &mut driver_status,
                                    );
                                    continue;
                                }
                            }
                        }
                        // Not driven — when the key arrived as
                        // `(Tab, SHIFT)` the `match k.code` below would
                        // route to the forward-cycle `Tab` arm instead
                        // of `BackTab`, so handle the same per-mode
                        // back-cycle the `BackTab` arm does and skip
                        // the match. The legacy `BackTab` shape still
                        // falls through naturally.
                        if matches!(k.code, KeyCode::Tab) {
                            match mode {
                                ViewMode::AllSessions | ViewMode::SessionDetail => {
                                    if !sessions.is_empty() {
                                        selected_session = (selected_session + sessions.len() - 1)
                                            % sessions.len();
                                        last_session_select = Instant::now();
                                        if mode == ViewMode::SessionDetail {
                                            pinned_session = visible_sessions.get(selected_session).map(
                                                |s| (s.account_name.clone(), s.session_id.clone()),
                                            );
                                        }
                                    }
                                }
                                ViewMode::AccountDetail => {
                                    if !per_account.is_empty() {
                                        selected_account = (selected_account + per_account.len()
                                            - 1)
                                            % per_account.len();
                                    }
                                }
                                ViewMode::Setup => {
                                    let len = view_setup::items(setup_snapshot.as_ref()).len();
                                    if len > 0 {
                                        selected_setup = (selected_setup + len - 1) % len;
                                    }
                                }
                                ViewMode::Mewxi => {}
                            }
                            continue;
                        }
                    }
                    match k.code {
                        KeyCode::Char('q') => {
                            for (_, cmd_tx) in &live_pollers {
                                let _ = cmd_tx.send(LiveCmd::Stop);
                            }
                            break;
                        }
                        KeyCode::Esc => match mode {
                            ViewMode::AllSessions => {}
                            ViewMode::SessionDetail
                            | ViewMode::AccountDetail
                            | ViewMode::Setup
                            | ViewMode::Mewxi => {
                                mode = ViewMode::AllSessions;
                                last_session_select = Instant::now();
                                pinned_session = None;
                            }
                        },
                        KeyCode::Char('x') | KeyCode::Char('X') => {
                            if error_shown.is_some() {
                                error_dismissed = true;
                            }
                        }
                        KeyCode::Char('1') => {
                            mode = ViewMode::AllSessions;
                            last_session_select = Instant::now();
                            pinned_session = None;
                        }
                        KeyCode::Char('2') => {
                            mode = ViewMode::SessionDetail;
                            pinned_session = visible_sessions
                                .get(selected_session)
                                .map(|s| (s.account_name.clone(), s.session_id.clone()));
                        }
                        KeyCode::Char('3') => {
                            mode = ViewMode::AccountDetail;
                            pinned_session = None;
                        }
                        KeyCode::Char('4') => {
                            setup_snapshot = setup::inspect(no_live).ok();
                            mode = ViewMode::Setup;
                            pinned_session = None;
                        }
                        KeyCode::Char('m') | KeyCode::Char('M') => {
                            // In the session view `m` is reserved for the
                            // model picker; only the other views use it as
                            // the shortcut to the Mewxi splash. A driven
                            // session opens the picker; an observed one
                            // can't change model, so nudge toward `n`.
                            let driven = mode == ViewMode::SessionDetail
                                && pinned_session
                                    .as_ref()
                                    .is_some_and(|k| drivers.contains_key(k));
                            if driven {
                                // Prefer the user's last optimistic pick
                                // over the raw transcript value — re-opening
                                // the picker before the transcript catches
                                // up should pre-highlight the pick the user
                                // just made, otherwise Enter on the default
                                // highlight silently reverts it.
                                let current = pinned_session.as_ref().and_then(|k| {
                                    driver_optimistic
                                        .get(k)
                                        .and_then(|o| o.model.clone())
                                        .or_else(|| {
                                            per_account
                                                .iter()
                                                .find(|p| p.account.name == k.0)
                                                .and_then(|p| {
                                                    p.live_sessions
                                                        .iter()
                                                        .find(|s| s.session_id == k.1)
                                                        .map(|s| s.model.clone())
                                                })
                                        })
                                });
                                // Effort: same precedence as the model
                                // badge — optimistic pick first, then
                                // the account's persisted `effortLevel`
                                // (claude stores it globally rather
                                // than in the transcript).
                                let current_effort = pinned_session.as_ref().and_then(|k| {
                                    driver_optimistic
                                        .get(k)
                                        .and_then(|o| o.effort.clone())
                                        .or_else(|| {
                                            per_account
                                                .iter()
                                                .find(|p| p.account.name == k.0)
                                                .and_then(|p| p.account.default_effort())
                                        })
                                });
                                model_picker = Some(ModelPickerModal::new(
                                    current.as_deref(),
                                    current_effort.as_deref(),
                                ));
                            } else if mode == ViewMode::SessionDetail {
                                // Observed (un-driven) session: mewxi can
                                // only change the model of a session it
                                // drives, so point the user at `n` rather
                                // than silently doing nothing.
                                driver_status = Some((
                                    "drive this session (n) to change its model".into(),
                                    Instant::now(),
                                ));
                            } else {
                                mode = ViewMode::Mewxi;
                                pinned_session = None;
                            }
                        }
                        KeyCode::Char('/') => {
                            // `/` opens the skill picker in a driven
                            // session — same key that prefixes every
                            // slash command so the muscle memory is
                            // already there. Discovery runs at
                            // open time (cheap — a few directory reads),
                            // so newly-installed skills appear without
                            // a TUI restart.
                            let driven = mode == ViewMode::SessionDetail
                                && pinned_session
                                    .as_ref()
                                    .is_some_and(|k| drivers.contains_key(k));
                            if driven {
                                if let Some(key) = pinned_session.clone() {
                                    let account = per_account
                                        .iter()
                                        .find(|p| p.account.name == key.0)
                                        .map(|p| p.account.clone());
                                    let cwd = per_account
                                        .iter()
                                        .find(|p| p.account.name == key.0)
                                        .and_then(|p| {
                                            p.live_sessions
                                                .iter()
                                                .find(|s| s.session_id == key.1)
                                                .map(|s| s.cwd.clone())
                                        })
                                        .unwrap_or_else(|| {
                                            std::env::current_dir()
                                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                                        });
                                    if let Some(acct) = account {
                                        let bin = agent_control::resolve_claude_bin(&acct);
                                        let skills = crate::skills::discover(
                                            &acct.dir,
                                            &cwd,
                                            Some(&bin),
                                        );
                                        skill_picker = Some(SkillPickerModal::new(skills));
                                    }
                                }
                            }
                        }
                        KeyCode::Char('R') if mode == ViewMode::Setup => {
                            setup_snapshot = setup::inspect(no_live).ok();
                            setup_message = Some("rescanned setup state".to_string());
                        }
                        // `r` force-fetches usage limits from the web for
                        // every linked account, bypassing the per-account
                        // REFRESH_INTERVAL. The HTTP work happens on the
                        // poller threads (via LiveCmd::Refresh) so the UI
                        // stays responsive; results flow back through the
                        // same LiveMsg::Update channel the periodic tick uses.
                        KeyCode::Char('r') => {
                            let mut refreshed = 0usize;
                            for (_, cmd_tx) in &live_pollers {
                                if cmd_tx.send(LiveCmd::Refresh).is_ok() {
                                    refreshed += 1;
                                }
                            }
                            if refreshed == 0 {
                                refresh_tally = None;
                                toasts.push(
                                    toast::ToastKind::Info,
                                    "no accounts to refresh",
                                    toast::DEFAULT_TTL,
                                );
                            } else {
                                // Arm the tally; the drain loop updates the
                                // progress toast and finalizes it once every
                                // poller has answered.
                                refresh_tally = Some(RefreshTally {
                                    pending: refreshed,
                                    total: refreshed,
                                    ok: 0,
                                    failures: Vec::new(),
                                    started: Instant::now(),
                                });
                                // Tagged so the per-account progress updates
                                // and the final result replace it in place
                                // rather than stacking new boxes. TTL outlives
                                // the worst-case wait so it never vanishes
                                // before the result lands.
                                toasts.push_tagged(
                                    REFRESH_TOAST_TAG,
                                    toast::ToastKind::Info,
                                    format!("refreshing limits for {refreshed} account(s)…"),
                                    REFRESH_TIMEOUT + Duration::from_secs(3),
                                );
                            }
                        }
                        KeyCode::Char('s') if mode == ViewMode::Setup => {
                            if let Some(view_setup::ConfigItem::Account(i)) =
                                view_setup::items(setup_snapshot.as_ref()).get(selected_setup)
                            {
                                setup_message = toggle_statusline_for_selected(
                                    &mut setup_snapshot,
                                    *i,
                                    no_live,
                                );
                                setup_snapshot = setup::inspect(no_live).ok();
                            }
                        }
                        KeyCode::Char('i') if mode == ViewMode::Setup => {
                            if let Some(view_setup::ConfigItem::Account(i)) =
                                view_setup::items(setup_snapshot.as_ref()).get(selected_setup)
                            {
                                setup_message = toggle_ignore_for_selected(&setup_snapshot, *i);
                                setup_snapshot = setup::inspect(no_live).ok();
                            }
                        }
                        KeyCode::Char('w') if mode == ViewMode::Setup => {
                            setup_message = toggle_watcher(&mut setup_snapshot, no_live);
                            setup_snapshot = setup::inspect(no_live).ok();
                        }
                        KeyCode::Char('a') if mode == ViewMode::Setup => {
                            setup_message = Some(apply_all_action(no_live));
                            setup_snapshot = setup::inspect(no_live).ok();
                        }
                        KeyCode::Char('t') if mode == ViewMode::Setup => {
                            let next = !defocus_input_after_send;
                            match accounts::set_defocus_input_after_send(next) {
                                Ok(()) => {
                                    defocus_input_after_send = next;
                                    setup_message = Some(format!(
                                        "defocus input after send: {}",
                                        if next { "on" } else { "off" }
                                    ));
                                }
                                Err(e) => {
                                    setup_message =
                                        Some(format!("failed to save preference: {e}"));
                                }
                            }
                        }
                        KeyCode::Tab => match mode {
                            ViewMode::AllSessions | ViewMode::SessionDetail => {
                                if !sessions.is_empty() {
                                    selected_session = (selected_session + 1) % sessions.len();
                                    last_session_select = Instant::now();
                                    if mode == ViewMode::SessionDetail {
                                        pinned_session = visible_sessions
                                            .get(selected_session)
                                            .map(|s| (s.account_name.clone(), s.session_id.clone()));
                                    }
                                }
                            }
                            ViewMode::AccountDetail => {
                                if !per_account.is_empty() {
                                    selected_account = (selected_account + 1) % per_account.len();
                                }
                            }
                            ViewMode::Setup => {
                                let len = view_setup::items(setup_snapshot.as_ref()).len();
                                if len > 0 {
                                    selected_setup = (selected_setup + 1) % len;
                                }
                            }
                            ViewMode::Mewxi => {}
                        },
                        KeyCode::BackTab => match mode {
                            // Driven SessionDetail Shift-Tab is
                            // intercepted ahead of this match (forwards
                            // to claude's PTY). This arm only sees the
                            // observe-only case, so prev-session cycle
                            // is the right behaviour.
                            ViewMode::AllSessions | ViewMode::SessionDetail => {
                                if !sessions.is_empty() {
                                    selected_session = (selected_session + sessions.len() - 1)
                                        % sessions.len();
                                    last_session_select = Instant::now();
                                    if mode == ViewMode::SessionDetail {
                                        pinned_session = visible_sessions
                                            .get(selected_session)
                                            .map(|s| (s.account_name.clone(), s.session_id.clone()));
                                    }
                                }
                            }
                            ViewMode::AccountDetail => {
                                if !per_account.is_empty() {
                                    selected_account = (selected_account + per_account.len() - 1)
                                        % per_account.len();
                                }
                            }
                            ViewMode::Setup => {
                                let len = view_setup::items(setup_snapshot.as_ref()).len();
                                if len > 0 {
                                    selected_setup = (selected_setup + len - 1) % len;
                                }
                            }
                            ViewMode::Mewxi => {}
                        },
                        KeyCode::Down => match mode {
                            ViewMode::AllSessions | ViewMode::SessionDetail => {
                                if !sessions.is_empty() {
                                    selected_session = (selected_session + 1).min(sessions.len() - 1);
                                    last_session_select = Instant::now();
                                    if mode == ViewMode::SessionDetail {
                                        pinned_session = visible_sessions
                                            .get(selected_session)
                                            .map(|s| (s.account_name.clone(), s.session_id.clone()));
                                    }
                                }
                            }
                            ViewMode::AccountDetail => {
                                if !per_account.is_empty() {
                                    selected_account =
                                        (selected_account + 1).min(per_account.len() - 1);
                                }
                            }
                            ViewMode::Setup => {
                                let len = view_setup::items(setup_snapshot.as_ref()).len();
                                if len > 0 {
                                    selected_setup = (selected_setup + 1).min(len - 1);
                                }
                            }
                            ViewMode::Mewxi => {}
                        },
                        KeyCode::Up => match mode {
                            ViewMode::AllSessions | ViewMode::SessionDetail => {
                                if selected_session > 0 {
                                    selected_session -= 1;
                                    last_session_select = Instant::now();
                                    if mode == ViewMode::SessionDetail {
                                        pinned_session = visible_sessions
                                            .get(selected_session)
                                            .map(|s| (s.account_name.clone(), s.session_id.clone()));
                                    }
                                }
                            }
                            ViewMode::AccountDetail => {
                                if selected_account > 0 {
                                    selected_account -= 1;
                                }
                            }
                            ViewMode::Setup => {
                                if selected_setup > 0 {
                                    selected_setup -= 1;
                                }
                            }
                            ViewMode::Mewxi => {}
                        },
                        KeyCode::Enter => {
                            if mode == ViewMode::AllSessions && !sessions.is_empty() {
                                mode = ViewMode::SessionDetail;
                                pinned_session = visible_sessions
                                    .get(selected_session)
                                    .map(|s| (s.account_name.clone(), s.session_id.clone()));
                            } else if mode == ViewMode::Setup {
                                // One contextual action per row — the
                                // hint box above the footer spells out
                                // what this does for the selected row.
                                let item = view_setup::items(setup_snapshot.as_ref())
                                    .get(selected_setup)
                                    .cloned();
                                match item {
                                    Some(view_setup::ConfigItem::Account(i)) => {
                                        let is_ignored = setup_snapshot
                                            .as_ref()
                                            .and_then(|s| s.accounts.get(i))
                                            .is_some_and(|a| a.ignored);
                                        setup_message = if is_ignored {
                                            toggle_ignore_for_selected(&setup_snapshot, i)
                                        } else {
                                            toggle_statusline_for_selected(
                                                &mut setup_snapshot,
                                                i,
                                                no_live,
                                            )
                                        };
                                        setup_snapshot = setup::inspect(no_live).ok();
                                    }
                                    Some(view_setup::ConfigItem::Watcher) => {
                                        setup_message =
                                            toggle_watcher(&mut setup_snapshot, no_live);
                                        setup_snapshot = setup::inspect(no_live).ok();
                                    }
                                    Some(view_setup::ConfigItem::UpdateChannel) => {
                                        let next = update_channel.toggled();
                                        match accounts::set_update_channel(next.as_str()) {
                                            Ok(()) => {
                                                update_channel = next;
                                                setup_message = Some(format!(
                                                    "update channel → {}",
                                                    next.label()
                                                ));
                                                // Re-check against the new channel.
                                                update_status = None;
                                                update_error = None;
                                                update_checking = true;
                                                update::spawn_check(update_tx.clone());
                                            }
                                            Err(e) => {
                                                setup_message = Some(format!(
                                                    "failed to save channel: {e}"
                                                ));
                                            }
                                        }
                                    }
                                    Some(view_setup::ConfigItem::UpdateCheck) => {
                                        let next = !update_check_enabled;
                                        match accounts::set_update_check(next) {
                                            Ok(()) => {
                                                update_check_enabled = next;
                                                setup_message = Some(format!(
                                                    "automatic update checks: {}",
                                                    if next { "on" } else { "off" }
                                                ));
                                                // Turning them back on: check
                                                // right away instead of waiting
                                                // for the next TUI start.
                                                if next && !update_checking {
                                                    update_error = None;
                                                    update_checking = true;
                                                    update::spawn_check(update_tx.clone());
                                                }
                                            }
                                            Err(e) => {
                                                setup_message = Some(format!(
                                                    "failed to save preference: {e}"
                                                ));
                                            }
                                        }
                                    }
                                    Some(view_setup::ConfigItem::UpdateInterval) => {
                                        let next = update_interval.cycled();
                                        match accounts::set_update_interval(next.as_str()) {
                                            Ok(()) => {
                                                update_interval = next;
                                                setup_message = Some(format!(
                                                    "check for updates {}",
                                                    next.label()
                                                ));
                                            }
                                            Err(e) => {
                                                setup_message = Some(format!(
                                                    "failed to save preference: {e}"
                                                ));
                                            }
                                        }
                                    }
                                    Some(view_setup::ConfigItem::UpdatePrompt) => {
                                        let next = !update_prompt_enabled;
                                        match accounts::set_update_prompt(next) {
                                            Ok(()) => {
                                                update_prompt_enabled = next;
                                                setup_message = Some(format!(
                                                    "ask about updates on startup: {}",
                                                    if next { "on" } else { "off" }
                                                ));
                                            }
                                            Err(e) => {
                                                setup_message = Some(format!(
                                                    "failed to save preference: {e}"
                                                ));
                                            }
                                        }
                                    }
                                    Some(view_setup::ConfigItem::UpdateBuildDir) => {
                                        update_build_dir_edit =
                                            Some(text_input::TextInput::from_str(
                                                update_build_dir.as_deref().unwrap_or(""),
                                            ));
                                        setup_message = None;
                                    }
                                    Some(view_setup::ConfigItem::UpdateCheckNow) => {
                                        if update_checking {
                                            // A check is already in flight.
                                        } else if update_status
                                            .as_ref()
                                            .is_some_and(|s| s.available)
                                        {
                                            let (msg, updated) =
                                                run_update_apply(terminal);
                                            update_status = None;
                                            update_error = None;
                                            setup_message = Some(msg);
                                            if updated {
                                                for (_, cmd_tx) in &live_pollers {
                                                    let _ = cmd_tx.send(LiveCmd::Stop);
                                                }
                                                restart_after_update = true;
                                                break;
                                            }
                                        } else {
                                            update_error = None;
                                            update_checking = true;
                                            update::spawn_check(update_tx.clone());
                                            setup_message =
                                                Some("checking origin for updates…".into());
                                        }
                                    }
                                    Some(view_setup::ConfigItem::DefaultView) => {
                                        let next = default_view_pref.cycled();
                                        match accounts::set_default_view(next.as_str()) {
                                            Ok(()) => {
                                                default_view_pref = next;
                                                setup_message = Some(format!(
                                                    "start in {}",
                                                    next.label()
                                                ));
                                            }
                                            Err(e) => {
                                                setup_message = Some(format!(
                                                    "failed to save preference: {e}"
                                                ));
                                            }
                                        }
                                    }
                                    Some(view_setup::ConfigItem::DefocusToggle) => {
                                        let next = !defocus_input_after_send;
                                        match accounts::set_defocus_input_after_send(next) {
                                            Ok(()) => {
                                                defocus_input_after_send = next;
                                                setup_message = Some(format!(
                                                    "defocus input after send: {}",
                                                    if next { "on" } else { "off" }
                                                ));
                                            }
                                            Err(e) => {
                                                setup_message = Some(format!(
                                                    "failed to save preference: {e}"
                                                ));
                                            }
                                        }
                                    }
                                    Some(view_setup::ConfigItem::StatusLineComposer) => {
                                        composer_modal = Some(ComposerModal::new(
                                            crate::statusline::composer_rows(&view),
                                        ));
                                        setup_message = None;
                                    }
                                    None => {}
                                }
                            }
                        }
                        KeyCode::PageUp if mode == ViewMode::SessionDetail => {
                            chat_scroll = chat_scroll.saturating_add(10);
                            chat_selection = None;
                            chat_selection_dragging = false;
                        }
                        KeyCode::PageDown if mode == ViewMode::SessionDetail => {
                            chat_scroll = chat_scroll.saturating_sub(10);
                            chat_selection = None;
                            chat_selection_dragging = false;
                        }
                        KeyCode::Home if mode == ViewMode::SessionDetail => {
                            // Jump to oldest — cap value gets clamped in view.
                            chat_scroll = usize::MAX / 2;
                            chat_selection = None;
                            chat_selection_dragging = false;
                        }
                        KeyCode::End if mode == ViewMode::SessionDetail => {
                            chat_scroll = 0;
                            // Resume tailing the latest change row too.
                            changes_selection = None;
                            detail_scroll = 0;
                            chat_selection = None;
                            chat_selection_dragging = false;
                        }
                        KeyCode::Char('k') if mode == ViewMode::SessionDetail => {
                            let tail = last_change_count.saturating_sub(1);
                            let cur = changes_selection.unwrap_or(tail);
                            changes_selection = Some(cur.saturating_sub(1));
                            detail_scroll = 0;
                        }
                        KeyCode::Char('j') if mode == ViewMode::SessionDetail => {
                            let tail = last_change_count.saturating_sub(1);
                            let cur = changes_selection.unwrap_or(tail);
                            let next = (cur + 1).min(tail);
                            changes_selection = Some(next);
                            detail_scroll = 0;
                        }
                        KeyCode::Char('g') if mode == ViewMode::SessionDetail => {
                            changes_selection = Some(0);
                            detail_scroll = 0;
                        }
                        KeyCode::Char('G') if mode == ViewMode::SessionDetail => {
                            changes_selection = None;
                            detail_scroll = 0;
                        }
                        KeyCode::Char('J') if mode == ViewMode::SessionDetail => {
                            detail_scroll = detail_scroll.saturating_add(1);
                        }
                        KeyCode::Char('K') if mode == ViewMode::SessionDetail => {
                            detail_scroll = detail_scroll.saturating_sub(1);
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            // Open the picker. The actual spawn happens
                            // on `ModalOutcome::Confirm` above; from
                            // there the instant-pin path takes over so
                            // the user sees the session detail view
                            // before the marker has even been written.
                            let accounts_snapshot: Vec<Account> = per_account
                                .iter()
                                .filter(|p| !ignored.contains(&p.account.name))
                                .map(|p| p.account.clone())
                                .collect();
                            if accounts_snapshot.is_empty() {
                                driver_status = Some((
                                    "no accounts available to spawn under".into(),
                                    Instant::now(),
                                ));
                            } else {
                                let initial_idx = accounts_snapshot
                                    .iter()
                                    .position(|a| {
                                        spawn_candidate_account
                                            .as_ref()
                                            .is_some_and(|c| c.name == a.name)
                                    })
                                    .unwrap_or(0);
                                let initial_dir =
                                    accounts::resolve_default_new_session_dir(&view);
                                new_session_modal = Some(NewSessionModal::new(
                                    accounts_snapshot,
                                    initial_idx,
                                    initial_dir,
                                ));
                            }
                        }
                        KeyCode::Char('i')
                            if mode == ViewMode::SessionDetail
                                && pinned_session
                                    .as_ref()
                                    .is_some_and(|k| drivers.contains_key(k)) =>
                        {
                            let key = pinned_session
                                .as_ref()
                                .expect("driven session ⇒ pinned set");
                            crate::debug_log::log(&format!(
                                "driver_input.focus key={key:?}"
                            ));
                            driver_input_focused = true;
                        }
                        KeyCode::Char('d')
                            if k.modifiers.contains(KeyModifiers::CONTROL)
                                && mode == ViewMode::SessionDetail =>
                        {
                            if let Some(key) = pinned_session.clone() {
                                if let Some(pty) = drivers.get_mut(&key) {
                                    let _ = pty.kill();
                                    driver_status = Some((
                                        format!("ending driven session {}", short_sid(&key.1)),
                                        Instant::now(),
                                    ));
                                    let pid = sessions
                                        .iter()
                                        .find(|s| {
                                            s.account_name == key.0 && s.session_id == key.1
                                        })
                                        .map(|s| s.pid)
                                        .unwrap_or(0);
                                    mark_killing(
                                        &mut killing_sessions,
                                        &sessions,
                                        &key.0,
                                        &key.1,
                                        pid,
                                    );
                                    // Drop out of the now-dead session's
                                    // view back to the overview.
                                    mode = ViewMode::AllSessions;
                                    pinned_session = None;
                                    last_session_select = Instant::now();
                                }
                            }
                        }
                        // Cancel claude's in-flight execution. Esc is
                        // mewxi's "back to view 1" key, so Ctrl-C takes
                        // the role Esc plays inside a standalone claude.
                        KeyCode::Char('c')
                            if k.modifiers.contains(KeyModifiers::CONTROL)
                                && mode == ViewMode::SessionDetail =>
                        {
                            if let Some(key) = pinned_session.clone() {
                                if let Some(pty) = drivers.get_mut(&key) {
                                    match pty.cancel_execution() {
                                        Ok(_) => {
                                            driver_status = Some((
                                                "cancel sent — claude should interrupt".into(),
                                                Instant::now(),
                                            ));
                                        }
                                        Err(e) => {
                                            driver_status = Some((
                                                format!("cancel failed: {e}"),
                                                Instant::now(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Delete => {
                            let target: Option<&&SessionRef> = match mode {
                                ViewMode::SessionDetail => pinned_session
                                    .as_ref()
                                    .and_then(|(acct, sid)| {
                                        visible_sessions
                                            .iter()
                                            .find(|s| s.account_name == *acct && s.session_id == *sid)
                                    }),
                                ViewMode::AllSessions => visible_sessions
                                    .get(selected_session),
                                _ => None,
                            };
                            match target {
                                // A sub-agent has no process of its own —
                                // its `pid` mirrors the parent's, so the
                                // kill would take down the whole session.
                                Some(s) if s.subagent.is_some() => {
                                    driver_status = Some((
                                        "sub-agents can't be killed — kill the parent session".into(),
                                        Instant::now(),
                                    ));
                                }
                                Some(s) => {
                                    kill_confirm_modal = Some(KillConfirmModal::new(
                                        s.account_name.clone(),
                                        s.session_id.clone(),
                                        s.pid,
                                    ));
                                }
                                None => {
                                    driver_status = Some((
                                        "no session selected to kill".into(),
                                        Instant::now(),
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Tear down every driven session before we surrender the TUI. Kill
    // pending spawns too — their child may have rendered but never made
    // it onto the registry. PtySession::Drop also kills, but explicit
    // here so a noisy log is easier to chase.
    for (_, mut pty) in drivers.drain() {
        let _ = pty.kill();
    }
    for mut ps in pending_spawns.drain(..) {
        let _ = ps.pty.kill();
    }
    Ok(if restart_after_update {
        LoopExit::RestartAfterUpdate
    } else {
        LoopExit::Quit
    })
}

/// Short, table-friendly form of a session-id UUID (first 8 chars).
/// Extract plain text inside the chat-log selection rectangle.
/// `visible` holds the chat's currently-rendered rows (one per inner
/// screen row); `inner` is that pane's rect. Rows outside the
/// rendered range are skipped silently — the renderer clamps the
/// selection to `inner` before storing it, so out-of-range only
/// happens if the chat shrank between render and copy.
fn extract_chat_selection_text(
    visible: &[String],
    inner: Rect,
    sel: view_session::ChatSelection,
) -> String {
    let (col_start, row_start, col_end, row_end) = sel.rect();
    if col_end <= col_start {
        return String::new();
    }
    let i_start = row_start.saturating_sub(inner.y) as usize;
    let i_end = row_end.saturating_sub(inner.y) as usize;
    let col_lo = col_start.saturating_sub(inner.x) as usize;
    let col_hi = col_end.saturating_sub(inner.x) as usize;
    let mut out = String::new();
    for i in i_start..=i_end {
        let Some(row) = visible.get(i) else { break };
        let chars: Vec<char> = row.chars().collect();
        let lo = col_lo.min(chars.len());
        let hi = col_hi.min(chars.len());
        if i > i_start {
            out.push('\n');
        }
        // Trim trailing spaces so single-line selections don't drag
        // padding from the right edge of the pane into the clipboard.
        let slice: String = chars[lo..hi].iter().collect();
        out.push_str(slice.trim_end());
    }
    out
}

fn short_sid(sid: &str) -> String {
    sid.chars().take(8).collect()
}

fn hit(rect: Option<Rect>, col: u16, row: u16) -> bool {
    match rect {
        Some(r) => col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height,
        None => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_scroll(
    dir: i32,
    col: u16,
    row: u16,
    mode: ViewMode,
    chat_scroll: &mut usize,
    changes_selection: &mut Option<usize>,
    last_change_count: usize,
    detail_scroll: &mut usize,
    chat_rect: Option<Rect>,
    actions_rect: Option<Rect>,
    detail_rect: Option<Rect>,
    sessions_rect: Option<Rect>,
    setup_rect: Option<Rect>,
    selected_session: &mut usize,
    selected_setup: &mut usize,
    last_session_select: &mut Instant,
    visible_sessions: &[&SessionRef],
    sessions_len: usize,
    setup_len: usize,
    pinned_session: &mut Option<(String, String)>,
) {
    match mode {
        ViewMode::SessionDetail => {
            if hit(detail_rect, col, row) {
                if dir < 0 {
                    *detail_scroll = detail_scroll.saturating_sub(1);
                } else {
                    *detail_scroll = detail_scroll.saturating_add(1);
                }
            } else if hit(actions_rect, col, row) {
                let tail = last_change_count.saturating_sub(1);
                let cur = changes_selection.unwrap_or(tail);
                let next = if dir < 0 {
                    cur.saturating_sub(1)
                } else {
                    (cur + 1).min(tail)
                };
                *changes_selection = Some(next);
                *detail_scroll = 0;
            } else if hit(chat_rect, col, row) {
                // Wheel-up reveals older content; chat_scroll counts
                // lines back from tail, so wheel-up increases it.
                if dir < 0 {
                    *chat_scroll = chat_scroll.saturating_add(3);
                } else {
                    *chat_scroll = chat_scroll.saturating_sub(3);
                }
            }
        }
        ViewMode::AllSessions => {
            if hit(sessions_rect, col, row) && sessions_len > 0 {
                if dir < 0 {
                    *selected_session = selected_session.saturating_sub(1);
                } else {
                    *selected_session = (*selected_session + 1).min(sessions_len - 1);
                }
                *last_session_select = Instant::now();
                *pinned_session = visible_sessions
                    .get(*selected_session)
                    .map(|s| (s.account_name.clone(), s.session_id.clone()));
            }
        }
        ViewMode::Setup => {
            if hit(setup_rect, col, row) && setup_len > 0 {
                if dir < 0 {
                    *selected_setup = selected_setup.saturating_sub(1);
                } else {
                    *selected_setup = (*selected_setup + 1).min(setup_len - 1);
                }
            }
        }
        ViewMode::AccountDetail | ViewMode::Mewxi => {}
    }
}

/// Cheap ANSI strip — drops CSI escape sequences and most C0 control
/// codes (keeping `\n`). Used to surface the last bytes of PTY output
/// in the "starting…" placeholder when a child errors out before
/// writing a session marker. Returns the last `max_bytes` characters
/// of the cleaned text, or `None` if nothing is left.
fn ansi_strip_tail(bytes: &[u8], max_bytes: usize) -> Option<String> {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip CSI / OSC / single-char escapes. Eat until we hit a
            // terminating letter (0x40..0x7e for CSI) or a BEL/ST for OSC.
            if let Some(&next) = chars.peek() {
                if next == '[' {
                    chars.next();
                    for nc in chars.by_ref() {
                        if nc.is_ascii_alphabetic() || nc == '~' {
                            break;
                        }
                    }
                    continue;
                }
                if next == ']' {
                    chars.next();
                    for nc in chars.by_ref() {
                        if nc == '\x07' {
                            break;
                        }
                    }
                    continue;
                }
                // Two-char escape like ESC ( B; skip one char.
                chars.next();
                continue;
            }
        }
        if c == '\n' || c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    let trimmed = out.trim_end_matches(['\n', ' ']).to_string();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= max_bytes {
        return Some(trimmed);
    }
    let start = trimmed.chars().count() - max_bytes;
    Some(trimmed.chars().skip(start).collect())
}

#[allow(clippy::too_many_arguments)]
fn render(
    f: &mut Frame,
    mode: ViewMode,
    accounts: &[&PerAccount],
    sessions: &[&SessionRef],
    selected_session: usize,
    chat_scroll: &mut usize,
    changes_selection: &mut Option<usize>,
    last_change_count: &mut usize,
    detail_scroll: &mut usize,
    chat_rect: &mut Option<Rect>,
    actions_rect: &mut Option<Rect>,
    detail_rect: &mut Option<Rect>,
    sessions_rect: &mut Option<Rect>,
    all_table_state: &mut ratatui::widgets::TableState,
    setup_rect: &mut Option<Rect>,
    chat_selection: Option<view_session::ChatSelection>,
    chat_inner: &mut Option<Rect>,
    chat_visible: &mut Vec<String>,
    chat_code_blocks: &mut Vec<view_session::CodeBlockRegion>,
    detail_copy_blocks: &mut Vec<view_session::DetailCopyRegion>,
    mouse_pos: Option<(u16, u16)>,
    visible_session_selection: Option<usize>,
    selected_account: usize,
    selected_setup: usize,
    setup_scroll: &mut usize,
    setup: Option<&SetupSnapshot>,
    setup_message: Option<&str>,
    defocus_input_after_send: bool,
    default_view: view_setup::DefaultView,
    update_ui: &view_setup::UpdateUi,
    live_error: Option<&str>,
    driver: Option<&mut view_session::DriverPane<'_>>,
    pending: Option<&view_session::PendingPane>,
) {
    let area = f.area();
    let needs_setup = setup.is_some_and(|s| !s.fully_ok());
    // Reserve 4 rows (2 borders + 2 content lines, wrapped) when there's
    // a live-fetch error to surface. Skip the reservation entirely when
    // there's no error so the view gets the full screen.
    let error_height: u16 = if live_error.is_some() { 4 } else { 0 };

    let mut constraints = vec![ratatui::layout::Constraint::Length(1)]; // top header
    if needs_setup && mode != ViewMode::Setup {
        constraints.push(ratatui::layout::Constraint::Length(1)); // setup banner
    }
    constraints.push(ratatui::layout::Constraint::Min(0)); // view area
    if error_height > 0 {
        constraints.push(ratatui::layout::Constraint::Length(error_height));
    }
    let chunks = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let header_area = chunks[0];
    let (banner_area, view_area, error_area) = match (needs_setup && mode != ViewMode::Setup, error_height > 0) {
        (true, true) => (Some(chunks[1]), chunks[2], Some(chunks[3])),
        (true, false) => (Some(chunks[1]), chunks[2], None),
        (false, true) => (None, chunks[1], Some(chunks[2])),
        (false, false) => (None, chunks[1], None),
    };

    render_top_header(f, header_area);
    if let (Some(area), Some(snap)) = (banner_area, setup) {
        render_setup_banner(f, area, snap);
    }
    if let (Some(area), Some(msg)) = (error_area, live_error) {
        render_error_footer(f, area, msg);
    }

    match mode {
        ViewMode::AllSessions => view_all::render(
            f,
            view_area,
            accounts,
            sessions,
            visible_session_selection,
            sessions_rect,
            all_table_state,
        ),
        ViewMode::SessionDetail => view_session::render(
            f,
            view_area,
            accounts,
            sessions.get(selected_session).copied(),
            chat_scroll,
            changes_selection,
            last_change_count,
            detail_scroll,
            chat_rect,
            actions_rect,
            detail_rect,
            chat_selection,
            chat_inner,
            chat_visible,
            chat_code_blocks,
            detail_copy_blocks,
            mouse_pos,
            driver,
            pending,
        ),
        ViewMode::AccountDetail => {
            if let Some(pa) = accounts.get(selected_account) {
                view_account::render(f, view_area, pa);
            }
        }
        ViewMode::Setup => view_setup::render(
            f,
            view_area,
            setup,
            selected_setup,
            setup_scroll,
            setup_message,
            defocus_input_after_send,
            default_view,
            update_ui,
            setup_rect,
        ),
        ViewMode::Mewxi => view_mewxi::render(f, view_area, accounts, sessions),
    }
}

fn render_top_header(f: &mut Frame, area: ratatui::layout::Rect) {
    use ratatui::layout::Alignment;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    let line = Line::from(vec![Span::styled(
        "Mewxi",
        Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
    )]);
    f.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

fn render_error_footer(f: &mut Frame, area: ratatui::layout::Rect, msg: &str) {
    use ratatui::layout::Alignment;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
    let title = Line::from(vec![
        Span::styled(
            " live fetch error ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    ]);
    // Right-aligned dismiss hint on the top border. Bracketed `x` so the
    // key to press is unmistakable; the parenthetical reminds the user
    // the banner also auto-hides.
    let hint = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "[x]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        Span::styled(
            " hide (auto-hides in 10s) ",
            Style::default().fg(Color::Yellow),
        ),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(title)
        .title(hint.alignment(Alignment::Right));
    let p = Paragraph::new(Line::from(Span::styled(
        msg.to_string(),
        Style::default().fg(Color::Red),
    )))
    .block(block)
    .wrap(Wrap { trim: true });
    f.render_widget(p, area);
}

fn render_setup_banner(f: &mut Frame, area: ratatui::layout::Rect, snap: &SetupSnapshot) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    let unwired = snap.unwired_count();
    let watcher = snap.watcher.short();
    let msg = if unwired > 0 && !snap.watcher.is_ok() {
        format!("⚠ setup incomplete — {unwired} account(s) unwired · watcher {watcher} · press 4 to fix")
    } else if unwired > 0 {
        format!("⚠ setup incomplete — {unwired} account(s) unwired · press 4 to fix")
    } else {
        format!("⚠ setup incomplete — watcher {watcher} · press 4 to fix")
    };
    let line = Line::from(vec![Span::styled(
        msg,
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )]);
    f.render_widget(Paragraph::new(line), area);
}

fn toggle_ignore_for_selected(
    snap: &Option<SetupSnapshot>,
    idx: usize,
) -> Option<String> {
    let acct = snap.as_ref()?.accounts.get(idx)?;
    let name = acct.account_name.clone();
    match accounts::toggle_ignored(&name) {
        Ok(true) => Some(format!(
            "[{name}] now ignored — restart `mewxi tui` to drop from other views"
        )),
        Ok(false) => Some(format!(
            "[{name}] un-ignored — restart `mewxi tui` to see in other views"
        )),
        Err(e) => Some(format!("[{name}] toggle ignore FAILED: {e}")),
    }
}

fn toggle_statusline_for_selected(
    snap: &mut Option<SetupSnapshot>,
    idx: usize,
    no_live: bool,
) -> Option<String> {
    let acct = snap.as_ref()?.accounts.get(idx)?.clone();
    let binary = snap.as_ref()?.binary.clone();
    match &acct.statusline {
        setup::StatusLineState::Wired => match setup::unwire_statusline(&acct.settings_path) {
            Ok(true) => Some(format!("[{}] unwired", acct.account_name)),
            Ok(false) => Some(format!("[{}] nothing to remove", acct.account_name)),
            Err(e) => Some(format!("[{}] FAILED: {e}", acct.account_name)),
        },
        setup::StatusLineState::OtherCommand(_) => {
            // Force-overwrite an existing third-party statusLine.
            match setup::wire_statusline(&acct.settings_path, &binary, no_live, true) {
                Ok(true) => Some(format!("[{}] overwrote existing statusLine", acct.account_name)),
                Ok(false) => Some(format!("[{}] already wired", acct.account_name)),
                Err(e) => Some(format!("[{}] FAILED: {e}", acct.account_name)),
            }
        }
        _ => match setup::wire_statusline(&acct.settings_path, &binary, no_live, false) {
            Ok(true) => Some(format!("[{}] wired statusLine → {}", acct.account_name, acct.settings_path.display())),
            Ok(false) => Some(format!("[{}] already wired", acct.account_name)),
            Err(e) => Some(format!("[{}] FAILED: {e}", acct.account_name)),
        },
    }
}

fn toggle_watcher(snap: &mut Option<SetupSnapshot>, no_live: bool) -> Option<String> {
    let s = snap.as_ref()?;
    let binary = s.binary.clone();
    match &s.watcher {
        setup::WatcherState::Running => match setup::uninstall_watcher() {
            Ok(()) => Some("watcher stopped and uninstalled".into()),
            Err(e) => Some(format!("watcher uninstall FAILED: {e}")),
        },
        setup::WatcherState::Installed | setup::WatcherState::NotInstalled => {
            match setup::install_watcher(&binary, no_live) {
                Ok(()) => Some("watcher installed and started".into()),
                Err(e) => Some(format!("watcher install FAILED: {e}")),
            }
        }
        setup::WatcherState::Unknown(why) => Some(format!("watcher state unknown: {why}")),
    }
}

fn apply_all_action(no_live: bool) -> String {
    match setup::apply_all(false, no_live) {
        Ok(outcome) => {
            let mut parts: Vec<String> = Vec::new();
            if !outcome.wired_accounts.is_empty() {
                parts.push(format!("wired: {}", outcome.wired_accounts.join(", ")));
            }
            if outcome.watcher_installed {
                parts.push("watcher installed".into());
            }
            for s in &outcome.skipped {
                parts.push(format!("skipped {s}"));
            }
            for e in &outcome.errors {
                parts.push(format!("err {e}"));
            }
            if parts.is_empty() {
                "nothing to do — already fully set up".into()
            } else {
                parts.join("  ·  ")
            }
        }
        Err(e) => format!("apply_all FAILED: {e}"),
    }
}

/// How long a `killing` row lingers after the user asks mewxi to close
/// the agent. The session marker disappears from the very next scan once
/// the process dies, so without a linger the red `killing` feedback would
/// blink out within a refresh tick. After it elapses the synthetic
/// placeholder is dropped and the (now dead) session is gone for good.
const KILLING_TTL: Duration = Duration::from_secs(4);

/// A session the user just asked mewxi to close. Snapshots enough to keep
/// rendering a placeholder row after the process marker vanishes from the
/// scan — see [`apply_killing_overlay`].
struct KillingEntry {
    since: Instant,
    account_name: String,
    project: String,
    pid: u32,
}

/// Register a session as being closed by mewxi so view 1 shows the red
/// `killing` overlay. Captures the project label from the current scan
/// (while the row is still present) so a placeholder groups correctly
/// after the marker disappears.
fn mark_killing(
    killing: &mut HashMap<(String, String), KillingEntry>,
    sessions: &[SessionRef],
    account: &str,
    session_id: &str,
    pid: u32,
) {
    let project = sessions
        .iter()
        .find(|s| s.account_name == account && s.session_id == session_id)
        .map(|s| s.project.clone())
        .unwrap_or_default();
    killing.insert(
        (account.to_string(), session_id.to_string()),
        KillingEntry {
            since: Instant::now(),
            account_name: account.to_string(),
            project,
            pid,
        },
    );
}

/// Mark every session the user just asked mewxi to close: flag rows still
/// present in the scan, and inject a synthetic placeholder for any whose
/// marker has already disappeared so the row doesn't vanish mid-kill.
/// Entries older than [`KILLING_TTL`] are pruned first.
fn apply_killing_overlay(
    sessions: &mut Vec<SessionRef>,
    killing: &mut HashMap<(String, String), KillingEntry>,
) {
    killing.retain(|_, e| e.since.elapsed() < KILLING_TTL);
    if killing.is_empty() {
        return;
    }
    for s in sessions.iter_mut() {
        if killing.contains_key(&(s.account_name.clone(), s.session_id.clone())) {
            s.killing = true;
        }
    }
    // Drop sub-agent rows under a killing session — the whole tree goes
    // away with the parent process, so live-looking child rows beneath
    // the red `killing` placeholder would be a lie (and were hidden
    // before sub-agents became selectable rows).
    sessions.retain(|s| {
        s.subagent.as_ref().is_none_or(|t| {
            !killing.contains_key(&(s.account_name.clone(), t.parent_session_id.clone()))
        })
    });
    let mut injected = false;
    for (key, entry) in killing.iter() {
        let present = sessions
            .iter()
            .any(|s| s.account_name == key.0 && s.session_id == key.1);
        if !present {
            sessions.push(killing_placeholder(key, entry));
            injected = true;
        }
    }
    if injected {
        // Re-sort so injected placeholders land in their project group —
        // mirrors flatten_sessions' ordering so view 1's grouping holds.
        regroup_sessions(sessions);
    }
}

/// A bare [`SessionRef`] standing in for a killed session whose marker is
/// already gone. Only the identifying fields carry real values; the rest
/// are inert because the row renders as dashes anyway (`killing == true`).
fn killing_placeholder(key: &(String, String), entry: &KillingEntry) -> SessionRef {
    SessionRef {
        account_name: entry.account_name.clone(),
        session_id: key.1.clone(),
        pid: entry.pid,
        project: entry.project.clone(),
        cwd: PathBuf::new(),
        transcript_path: PathBuf::new(),
        last_activity: chrono::Utc::now(),
        state_since: chrono::Utc::now(),
        model: String::new(),
        active_model: String::new(),
        tokens: 0,
        cost_usd: 0.0,
        totals: UsageTotals::default(),
        current_context: None,
        context_cap: None,
        state: SessionState::Idle,
        activity: crate::live_session::Activity::Waiting,
        permission_mode: None,
        effort: None,
        killing: true,
        subagent: None,
    }
}

fn flatten_sessions(
    accounts: &[PerAccount],
    optimistic: &HashMap<(String, String), DriverOptimistic>,
) -> Vec<SessionRef> {
    let mut out: Vec<SessionRef> = accounts
        .iter()
        .flat_map(|pa| {
            pa.live_sessions.iter().flat_map(move |ls| {
                let key = (ls.account_name.clone(), ls.session_id.clone());
                let opt = optimistic.get(&key);
                let model = opt
                    .and_then(|o| o.model.clone())
                    .unwrap_or_else(|| ls.model.clone());
                // Brand-new session with no transcript records yet:
                // surface the account's settings-level model override
                // so the badge isn't blank during the spawn → first
                // assistant response window. Falls back to the literal
                // `default` placeholder when nothing is configured,
                // matching the picker's "Default (recommended)" label.
                let model = if model.is_empty() {
                    pa.account
                        .default_model()
                        .unwrap_or_else(|| "default".to_string())
                } else {
                    model
                };
                let permission_mode = opt
                    .and_then(|o| o.mode.clone())
                    .or_else(|| ls.permission_mode.clone());
                // Claude itself disables auto when the model can't run
                // it (Haiku rejects with "auto mode unavailable for this
                // model"), but the transcript only catches up on the
                // next prompt. Downgrade `auto` → `default` here so the
                // badge reflects what claude's internal state already
                // is the moment the user picks an unsupported model.
                let permission_mode = match permission_mode.as_deref() {
                    Some("auto") if !model_supports_auto(&model) => {
                        Some("default".into())
                    }
                    _ => permission_mode,
                };
                // Effort: user's in-session pick wins; otherwise the
                // per-session level claude reported through `mewxi status`
                // (kept fresh on every statusline refresh); otherwise the
                // account's persisted `effortLevel`. Suppress when the
                // resolved model has no effort support so the badge doesn't
                // claim a level claude is silently ignoring.
                let effort = opt
                    .and_then(|o| o.effort.clone())
                    .or_else(|| stats::session_effort(&pa.account, &ls.session_id))
                    .or_else(|| pa.account.default_effort());
                let effort = effort.filter(|_| {
                    !model_picker_modal::effort_levels_for(&model).is_empty()
                });
                let parent = SessionRef {
                    account_name: ls.account_name.clone(),
                    session_id: ls.session_id.clone(),
                    pid: ls.pid,
                    project: ls.project.clone(),
                    cwd: ls.cwd.clone(),
                    transcript_path: ls.transcript_path.clone(),
                    last_activity: ls.last_activity,
                    state_since: ls.state_since,
                    model,
                    active_model: ls.active_model.clone(),
                    tokens: ls.session_tokens.total_tokens(),
                    cost_usd: ls.session_tokens.cost_usd,
                    totals: ls.session_tokens.clone(),
                    current_context: ls.current_context,
                    context_cap: ls.context_cap,
                    state: ls.state,
                    activity: ls.activity.clone(),
                    permission_mode,
                    effort,
                    killing: false,
                    subagent: None,
                };
                // Sub-agents follow their parent as first-class rows so
                // they're selectable and inspectable like sessions. They
                // borrow the parent's project/pid (grouping + sort keys)
                // and carry their own transcript, tokens and context.
                let mut rows = Vec::with_capacity(1 + ls.subagents.len());
                rows.push(parent);
                rows.extend(ls.subagents.iter().map(|sub| SessionRef {
                    account_name: ls.account_name.clone(),
                    session_id: sub.agent_id.clone(),
                    pid: ls.pid,
                    project: ls.project.clone(),
                    cwd: ls.cwd.clone(),
                    transcript_path: sub.transcript_path.clone(),
                    last_activity: sub.last_activity,
                    state_since: sub.started_at,
                    model: sub.model.clone(),
                    active_model: sub.model.clone(),
                    tokens: sub.tokens,
                    // Cost is rolled into the account aggregate via the
                    // parent's project; view 1 dashes it out on these rows.
                    cost_usd: sub.totals.cost_usd,
                    totals: sub.totals.clone(),
                    current_context: sub.current_context,
                    context_cap: sub.context_cap,
                    state: SessionState::Active,
                    activity: sub.activity.clone(),
                    permission_mode: None,
                    effort: None,
                    killing: false,
                    subagent: Some(SubAgentTag {
                        parent_session_id: ls.session_id.clone(),
                        agent_type: sub.agent_type.clone(),
                        description: sub.description.clone(),
                        workflow: sub.workflow.clone(),
                    }),
                }));
                rows
            })
        })
        .collect();
    regroup_sessions(&mut out);
    out
}

/// Order the flat row list for view 1: parents grouped by project
/// (alphabetical, case-insensitive) and pid ascending within each group —
/// stable for the lifetime of a session, so rows don't shuffle when a
/// session's state flips or its last_activity updates; session_id breaks
/// the unlikely pid tie. Each parent's sub-agent rows are then spliced
/// directly beneath it, preserving their scan order (launch time). j/k
/// navigation walks this same order so the selection cursor tracks the
/// visible row order rather than jumping around.
fn regroup_sessions(sessions: &mut Vec<SessionRef>) {
    let mut parents: Vec<SessionRef> = Vec::new();
    let mut children: Vec<SessionRef> = Vec::new();
    for s in sessions.drain(..) {
        if s.subagent.is_some() {
            children.push(s);
        } else {
            parents.push(s);
        }
    }
    parents.sort_by(|a, b| {
        a.project
            .to_ascii_lowercase()
            .cmp(&b.project.to_ascii_lowercase())
            .then_with(|| a.pid.cmp(&b.pid))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    let mut out = Vec::with_capacity(parents.len() + children.len());
    for p in parents {
        let key = (p.account_name.clone(), p.session_id.clone());
        out.push(p);
        let mut i = 0;
        while i < children.len() {
            let mine = children[i].account_name == key.0
                && children[i]
                    .subagent
                    .as_ref()
                    .is_some_and(|t| t.parent_session_id == key.1);
            if mine {
                out.push(children.remove(i));
            } else {
                i += 1;
            }
        }
    }
    // Orphans (parent vanished between scans) — keep them visible at the
    // tail rather than dropping rows the selection index may sit on.
    out.append(&mut children);
    *sessions = out;
}

#[cfg(test)]
mod tests {
    use super::{
        cycle_mode, model_supports_auto, regroup_sessions, ModeCycle, SessionRef, SubAgentTag,
        ViewMode,
    };

    /// Minimal row for regroup tests — only the sort/grouping keys carry
    /// meaning.
    fn row(project: &str, pid: u32, sid: &str, parent: Option<&str>) -> SessionRef {
        SessionRef {
            account_name: "acct".into(),
            session_id: sid.into(),
            pid,
            project: project.into(),
            cwd: std::path::PathBuf::new(),
            transcript_path: std::path::PathBuf::new(),
            last_activity: chrono::Utc::now(),
            state_since: chrono::Utc::now(),
            model: String::new(),
            active_model: String::new(),
            tokens: 0,
            cost_usd: 0.0,
            totals: crate::stats::UsageTotals::default(),
            current_context: None,
            context_cap: None,
            state: crate::live_session::SessionState::Active,
            activity: crate::live_session::Activity::Waiting,
            permission_mode: None,
            effort: None,
            killing: false,
            subagent: parent.map(|p| SubAgentTag {
                parent_session_id: p.into(),
                agent_type: None,
                description: String::new(),
                workflow: None,
            }),
        }
    }

    #[test]
    fn regroup_sorts_parents_and_splices_children_beneath_them() {
        // Deliberately scrambled: children first, parents out of order.
        let mut sessions = vec![
            row("beta", 2, "agent-2", Some("s-beta")),
            row("alpha", 1, "agent-1", Some("s-alpha")),
            row("beta", 2, "s-beta", None),
            row("alpha", 1, "s-alpha", None),
            row("beta", 2, "agent-3", Some("s-beta")),
        ];
        regroup_sessions(&mut sessions);
        let order: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(
            order,
            vec!["s-alpha", "agent-1", "s-beta", "agent-2", "agent-3"],
            "parents sorted by project, children glued beneath their parent in original order"
        );
    }

    #[test]
    fn regroup_keeps_orphan_children_at_the_tail() {
        let mut sessions = vec![
            row("alpha", 1, "agent-x", Some("gone")),
            row("alpha", 1, "s-alpha", None),
        ];
        regroup_sessions(&mut sessions);
        let order: Vec<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(order, vec!["s-alpha", "agent-x"]);
    }

    #[test]
    fn view_mode_from_config_accepts_names_and_keys() {
        assert_eq!(ViewMode::from_config(Some("overview")), ViewMode::AllSessions);
        assert_eq!(ViewMode::from_config(Some("session")), ViewMode::SessionDetail);
        assert_eq!(ViewMode::from_config(Some("account")), ViewMode::AccountDetail);
        assert_eq!(ViewMode::from_config(Some("config")), ViewMode::Setup);
        assert_eq!(ViewMode::from_config(Some("setup")), ViewMode::Setup);
        assert_eq!(ViewMode::from_config(Some("2")), ViewMode::SessionDetail);
        assert_eq!(ViewMode::from_config(Some(" Config ")), ViewMode::Setup);
    }

    #[test]
    fn view_mode_from_config_falls_back_to_overview() {
        assert_eq!(ViewMode::from_config(None), ViewMode::AllSessions);
        assert_eq!(ViewMode::from_config(Some("")), ViewMode::AllSessions);
        assert_eq!(ViewMode::from_config(Some("garbage")), ViewMode::AllSessions);
    }

    #[test]
    fn cycle_mode_three_step_without_auto() {
        assert_eq!(cycle_mode("default", false), "acceptEdits");
        assert_eq!(cycle_mode("acceptEdits", false), "plan");
        assert_eq!(cycle_mode("plan", false), "default");
    }

    #[test]
    fn cycle_mode_four_step_with_auto_matches_claude_2_1_150() {
        // Mirrors claude's `dRH` switch: default → acceptEdits → plan
        // → auto → default. Auto is reached *after* plan, not after
        // default — getting this wrong was the bug that motivated
        // [`ModeCycle`].
        assert_eq!(cycle_mode("default", true), "acceptEdits");
        assert_eq!(cycle_mode("acceptEdits", true), "plan");
        assert_eq!(cycle_mode("plan", true), "auto");
        assert_eq!(cycle_mode("auto", true), "default");
    }

    #[test]
    fn cycle_mode_unknown_starts_from_default() {
        assert_eq!(cycle_mode("garbage", false), "acceptEdits");
        assert_eq!(cycle_mode("", true), "acceptEdits");
    }

    #[test]
    fn mode_cycle_falls_back_when_unlearned() {
        let cycle = ModeCycle::default();
        assert_eq!(cycle.predict("default", false), "acceptEdits");
        assert_eq!(cycle.predict("plan", true), "auto");
    }

    #[test]
    fn mode_cycle_learned_beats_fallback() {
        // Simulate a future claude shuffle: `default` no longer goes
        // to `acceptEdits`. After one observation the learner wins
        // and the wrong fallback never fires again for that source.
        let mut cycle = ModeCycle::default();
        cycle.observe("default", "plan", false);
        assert_eq!(cycle.predict("default", false), "plan");
        // Untaught source still uses the fallback.
        assert_eq!(cycle.predict("acceptEdits", false), "plan");
    }

    #[test]
    fn mode_cycle_keys_by_auto_availability() {
        // The cycle variant changes when auto is unlocked, so
        // observations from one variant must not leak into the other.
        let mut cycle = ModeCycle::default();
        cycle.observe("plan", "default", false);
        cycle.observe("plan", "auto", true);
        assert_eq!(cycle.predict("plan", false), "default");
        assert_eq!(cycle.predict("plan", true), "auto");
    }

    #[test]
    fn mode_cycle_ignores_noop_and_empty_observations() {
        let mut cycle = ModeCycle::default();
        cycle.observe("plan", "plan", false); // no transition
        cycle.observe("", "plan", false); // empty prev
        cycle.observe("plan", "", false); // empty actual
        // Nothing learned → falls back.
        assert_eq!(cycle.predict("plan", false), "default");
    }

    #[test]
    fn auto_supported_on_opus_and_sonnet() {
        assert!(model_supports_auto("opus"));
        assert!(model_supports_auto("sonnet"));
        assert!(model_supports_auto("claude-opus-4-7"));
        assert!(model_supports_auto("claude-sonnet-4-6"));
        assert!(model_supports_auto("Claude-Opus-4-7"));
    }

    #[test]
    fn auto_blocked_on_haiku() {
        assert!(!model_supports_auto("haiku"));
        assert!(!model_supports_auto("claude-haiku-4-5"));
        assert!(!model_supports_auto("CLAUDE-HAIKU-4-5-20251001"));
    }

    #[test]
    fn auto_assumed_on_unknown_or_default() {
        assert!(model_supports_auto(""));
        assert!(model_supports_auto("default"));
        assert!(model_supports_auto("  default  "));
    }
}
