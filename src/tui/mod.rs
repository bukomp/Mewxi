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

mod markdown;
mod model_picker_modal;
mod new_session_modal;
mod view_account;
mod view_all;
mod view_mewxi;
mod view_session;
mod view_setup;
mod widgets;

use model_picker_modal::{ModelOutcome, ModelPickerModal};
use new_session_modal::{ModalOutcome, NewSessionModal};

use crate::accounts::{self, Account, AccountsView};
use crate::agent_control::{self, PtySession};
use crate::live_session::{self, LiveSession, SessionState};
use crate::live_usage::{self, LiveUsage};
use crate::setup::{self, SetupSnapshot};
use crate::stats::{self, Aggregate, UsageTotals};
use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
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

#[derive(Copy, Clone, PartialEq, Eq)]
enum ViewMode {
    AllSessions,
    SessionDetail,
    AccountDetail,
    Setup,
    Mewxi,
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

    let _ = show_splash(&mut terminal, SPLASH_DURATION);
    let result = run_loop(&mut terminal, no_live);

    let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    live_usage::set_tui_mode(false);
    result
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
}
enum LiveCmd {
    Refresh,
    Stop,
}

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

/// How long the user has between the first `K` (arms the confirmation
/// banner) and the second `K` (executes the kill). Long enough to read
/// the banner, short enough that the armed state doesn't outlive the
/// user's attention.
const KILL_CONFIRM_WINDOW: Duration = Duration::from_secs(3);

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
                Ok(LiveCmd::Refresh) | Err(_) => {
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

fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    no_live: bool,
) -> Result<()> {
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

    let mut mode = ViewMode::AllSessions;
    let mut selected_session: usize = 0;
    let mut last_selected_session: usize = usize::MAX;
    // Stable identity of the session currently being inspected in
    // SessionDetail. The flattened list is re-sorted every frame by
    // last_activity, so a raw index would silently jump to a different
    // session whenever another session's activity moves it ahead.
    let mut pinned_session: Option<(String, String)> = None;
    let mut chat_scroll: usize = 0;
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
    // View 1's session selection highlight fades out after a short
    // idle period so the table doesn't stay visually pinned to a row
    // the user picked once and forgot. The selection *index* still
    // drives view 2's drill-down — only the row chrome (arrow / yellow
    // bold) is suppressed once stale.
    let mut last_session_select: Instant = Instant::now();
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
    let mut driver_input: String = String::new();
    let mut driver_input_focused: bool = false;
    let mut driver_status: Option<(String, Instant)> = None;

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

    // Two-press confirmation for `K`-to-kill. First press captures the
    // target; second press within KILL_CONFIRM_WINDOW executes. Killing
    // someone else's claude is destructive (loses any unsaved /compact
    // work, prompts in progress) so we don't do it on a single keypress.
    let mut pending_kill: Option<(String, String, u32, Instant)> = None;

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

    loop {
        let sessions = flatten_sessions(&per_account);
        if selected_session >= sessions.len() && !sessions.is_empty() {
            selected_session = sessions.len() - 1;
        }
        if selected_account >= per_account.len() && !per_account.is_empty() {
            selected_account = per_account.len() - 1;
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

        // Promote any pending spawn whose session marker has appeared
        // since the last frame. Identify the new session by diffing the
        // account's current `live_sessions` against the snapshot taken
        // at spawn time. Sessions whose marker hasn't appeared after a
        // generous timeout are abandoned (child probably crashed).
        let mut promotions: Vec<(usize, String)> = Vec::new();
        for (i, ps) in pending_spawns.iter().enumerate() {
            if let Some(pa) = per_account.iter().find(|p| p.account.name == ps.account_name) {
                let new = pa
                    .live_sessions
                    .iter()
                    .find(|s| !ps.snapshot_session_ids.contains(&s.session_id));
                if let Some(s) = new {
                    promotions.push((i, s.session_id.clone()));
                }
            }
        }
        // Apply promotions in reverse index order so swap_remove indices
        // remain valid.
        for (i, sid) in promotions.into_iter().rev() {
            let ps = pending_spawns.swap_remove(i);
            let key = (ps.account_name.clone(), sid.clone());
            drivers.insert(key.clone(), ps.pty);
            // If this spawn's placeholder is currently pinned, swap
            // the pin to the real key without changing view mode — the
            // user is already looking at the "starting…" pane.
            // Otherwise (e.g. user navigated away), still auto-pin so
            // the freshly-spawned session is what they see when they
            // come back.
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
            drivers.remove(k);
            driver_status = Some((
                format!("driven session {} ended", short_sid(&k.1)),
                Instant::now(),
            ));
            if pinned_session.as_ref() == Some(k) {
                driver_input.clear();
                driver_input_focused = false;
            }
        }

        // Compute the driver pane state to hand to view_session.
        let is_driven = mode == ViewMode::SessionDetail
            && pinned_session
                .as_ref()
                .is_some_and(|k| drivers.contains_key(k));
        let driver_pane = if is_driven {
            Some(view_session::DriverPane {
                input: driver_input.as_str(),
                focused: driver_input_focused,
            })
        } else {
            // Unfocus if the pinned session is no longer driven.
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

        // Expire pending-kill confirmation if the user didn't press K
        // again in time. Keeping it armed past its window would be
        // surprising — a stray K minutes later shouldn't kill a session.
        if let Some((_, _, _, armed_at)) = &pending_kill {
            if armed_at.elapsed() > KILL_CONFIRM_WINDOW {
                pending_kill = None;
            }
        }

        // Build the transient banner. Pending-kill takes priority since
        // it's actionable; then driver status; then setup message.
        let combined_message: Option<String> = if let Some((_, sid, pid, armed_at)) = &pending_kill {
            let remaining = KILL_CONFIRM_WINDOW
                .saturating_sub(armed_at.elapsed())
                .as_secs()
                + 1;
            Some(format!(
                "kill claude pid {pid} (session {})? press K again within {remaining}s to confirm",
                short_sid(sid),
            ))
        } else {
            match (&driver_status, &setup_message) {
                (Some((m, t)), _) if t.elapsed() < Duration::from_secs(8) => Some(m.clone()),
                (_, Some(m)) => Some(m.clone()),
                _ => None,
            }
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
                &mut setup_rect,
                visible_selection,
                selected_account,
                selected_setup,
                setup_snapshot.as_ref(),
                combined_message.as_deref(),
                live_error,
                driver_pane.as_ref(),
                pending_pane.as_ref(),
            );
            // Modal overlays everything else when open. Render last so
            // it sits on top with Clear + its own border.
            if let Some(modal) = new_session_modal.as_ref() {
                modal.render(f, f.area());
            }
            if let Some(modal) = model_picker.as_ref() {
                modal.render(f, f.area());
            }
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
            while let Ok(LiveMsg::Update { account_name, live }) = rx.try_recv() {
                if let Some(pa) = per_account.iter_mut().find(|p| p.account.name == account_name) {
                    pa.live = live;
                }
            }
        }

        // Drain filesystem events.
        while let Ok(name) = dirty_rx.try_recv() {
            dirty.insert(name);
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
        }

        // Keyboard. Mewxi view animates the logo every frame, so it
        // polls at ~60 fps for buttery-smooth gradient + bob sampling;
        // every other view keeps the original 200ms budget — no point
        // burning CPU on a static screen.
        let poll_timeout = if mode == ViewMode::Mewxi {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(200)
        };
        if event::poll(poll_timeout)? {
            let evt = event::read()?;
            if let Event::Mouse(m) = &evt {
                let dir: i32 = match m.kind {
                    MouseEventKind::ScrollUp => -1,
                    MouseEventKind::ScrollDown => 1,
                    _ => 0,
                };
                if dir != 0 {
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
                        setup_snapshot.as_ref().map(|s| s.accounts.len()).unwrap_or(0),
                        &mut pinned_session,
                    );
                }
            }
            if let Event::Key(k) = evt {
                if k.kind == KeyEventKind::Press {
                    if std::env::var("MEWXI_KEY_LOG").is_ok() {
                        use std::io::Write as _;
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("/tmp/mewxi-keys.log")
                        {
                            let _ = writeln!(
                                f,
                                "[{:?}] code={:?} mods={:?} focused={} driven={}",
                                std::time::SystemTime::now(),
                                k.code,
                                k.modifiers,
                                driver_input_focused,
                                pinned_session
                                    .as_ref()
                                    .is_some_and(|x| drivers.contains_key(x))
                            );
                        }
                    }
                    // Model picker owns every keystroke while open.
                    // Dispatched ahead of the new-session modal,
                    // driver input, and globals — the two modals are
                    // mutually exclusive but this ordering documents
                    // the precedence.
                    if let Some(modal) = model_picker.as_mut() {
                        match modal.handle_key(k) {
                            ModelOutcome::Stay => {}
                            ModelOutcome::Cancel => {
                                model_picker = None;
                            }
                            ModelOutcome::Confirm(slug) => {
                                model_picker = None;
                                if let Some(key) = pinned_session.clone() {
                                    if let Some(pty) = drivers.get_mut(&key) {
                                        let mut bytes = format!("/model {slug}").into_bytes();
                                        bytes.push(b'\r');
                                        match pty.send_keys(&bytes) {
                                            Ok(_) => {
                                                driver_status = Some((
                                                    format!("sent /model {slug}"),
                                                    Instant::now(),
                                                ));
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
                            ModalOutcome::Confirm { account, cwd } => {
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
                                match PtySession::spawn(&account, cwd.clone(), bin) {
                                    Ok(pty) => {
                                        let spawn_id = next_spawn_id;
                                        next_spawn_id += 1;
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
                                        driver_status = Some((
                                            format!(
                                                "spawning claude under {} in {}",
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
                    // Ctrl-D ends the session, Ctrl-C clears the buffer.
                    if driver_input_focused {
                        if let Some(key) = pinned_session.clone() {
                            if let Some(pty) = drivers.get_mut(&key) {
                                match (k.code, k.modifiers) {
                                    (KeyCode::Esc, _) => {
                                        driver_input_focused = false;
                                    }
                                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                                        driver_input.clear();
                                    }
                                    (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                                        let _ = pty.kill();
                                        driver_input.clear();
                                        driver_input_focused = false;
                                    }
                                    (KeyCode::Enter, _) => {
                                        if !driver_input.is_empty() {
                                            let mut bytes = driver_input.as_bytes().to_vec();
                                            bytes.push(b'\r');
                                            match pty.send_keys(&bytes) {
                                                Ok(_) => {
                                                    driver_input.clear();
                                                    driver_status = Some((
                                                        "prompt sent".into(),
                                                        Instant::now(),
                                                    ));
                                                }
                                                Err(e) => {
                                                    driver_status = Some((
                                                        format!("send failed: {e}"),
                                                        Instant::now(),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    (KeyCode::Backspace, _) => {
                                        driver_input.pop();
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
                                        let r = pty.send_keys(b"\x1b[Z");
                                        driver_status = Some((
                                            format!("sent \\x1b[Z to pty ({:?})", r.is_ok()),
                                            Instant::now(),
                                        ));
                                    }
                                    (KeyCode::Tab, m) if m.contains(KeyModifiers::SHIFT) => {
                                        let r = pty.send_keys(b"\x1b[Z");
                                        driver_status = Some((
                                            format!("sent \\x1b[Z to pty ({:?})", r.is_ok()),
                                            Instant::now(),
                                        ));
                                    }
                                    (KeyCode::Char(c), _) => {
                                        driver_input.push(c);
                                    }
                                    _ => {}
                                }
                                continue;
                            }
                        }
                        // Pinned session was reaped while focused — drop focus
                        // so the next keypress hits the global handler.
                        driver_input_focused = false;
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
                                    let r = pty.send_keys(b"\x1b[Z");
                                    if std::env::var("MEWXI_KEY_LOG").is_ok() {
                                        use std::io::Write as _;
                                        if let Ok(mut f) = std::fs::OpenOptions::new()
                                            .create(true).append(true)
                                            .open("/tmp/mewxi-keys.log")
                                        {
                                            let _ = writeln!(
                                                f,
                                                "  -> [unfocused] pty.send_keys(\\x1b[Z) = {:?}",
                                                r
                                            );
                                        }
                                    }
                                    driver_status = Some((
                                        format!("sent \\x1b[Z to pty ({:?})", r.is_ok()),
                                        Instant::now(),
                                    ));
                                    continue;
                                }
                            }
                        }
                        // Not driven — fall through to the existing
                        // BackTab arm in the match below (prev-session
                        // cycle). Rewrite the code so the match sees
                        // the canonical `BackTab` form rather than
                        // having to handle both there too.
                        // (We can't mutate `k`; the BackTab arm
                        // already runs for both shapes thanks to the
                        // existing match arm — for `(Tab, SHIFT)` we
                        // call into the same logic directly here.)
                        if matches!(k.code, KeyCode::Tab) && !sessions.is_empty() {
                            selected_session =
                                (selected_session + sessions.len() - 1) % sessions.len();
                            last_session_select = Instant::now();
                            if mode == ViewMode::SessionDetail {
                                pinned_session = visible_sessions
                                    .get(selected_session)
                                    .map(|s| (s.account_name.clone(), s.session_id.clone()));
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
                        KeyCode::Char('r') => {
                            let alive = live_session::alive_pids();
                            for pa in per_account.iter_mut() {
                                pa.agg = stats::load_and_aggregate_for(&pa.account).unwrap_or_default();
                                pa.live_sessions = live_session::scan(&pa.account, &alive, &pa.live_sessions);
                                last_reload.insert(pa.account.name.clone(), Instant::now());
                            }
                            for (_, cmd_tx) in &live_pollers {
                                let _ = cmd_tx.send(LiveCmd::Refresh);
                            }
                        }
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
                            // In a driven session, `m` opens the model
                            // picker. Elsewhere it stays the
                            // shortcut to the Mewxi splash view.
                            let driven = mode == ViewMode::SessionDetail
                                && pinned_session
                                    .as_ref()
                                    .is_some_and(|k| drivers.contains_key(k));
                            if driven {
                                let current = pinned_session
                                    .as_ref()
                                    .and_then(|k| {
                                        per_account
                                            .iter()
                                            .find(|p| p.account.name == k.0)
                                            .and_then(|p| {
                                                p.live_sessions
                                                    .iter()
                                                    .find(|s| s.session_id == k.1)
                                                    .map(|s| s.model.clone())
                                            })
                                    });
                                model_picker = Some(ModelPickerModal::new(
                                    current.as_deref(),
                                ));
                            } else {
                                mode = ViewMode::Mewxi;
                                pinned_session = None;
                            }
                        }
                        KeyCode::Char('R') if mode == ViewMode::Setup => {
                            setup_snapshot = setup::inspect(no_live).ok();
                            setup_message = Some("rescanned setup state".to_string());
                        }
                        KeyCode::Char('s') if mode == ViewMode::Setup => {
                            setup_message = toggle_statusline_for_selected(
                                &mut setup_snapshot,
                                selected_setup,
                                no_live,
                            );
                            setup_snapshot = setup::inspect(no_live).ok();
                        }
                        KeyCode::Char('i') if mode == ViewMode::Setup => {
                            setup_message = toggle_ignore_for_selected(
                                &setup_snapshot,
                                selected_setup,
                            );
                            setup_snapshot = setup::inspect(no_live).ok();
                        }
                        KeyCode::Char('w') if mode == ViewMode::Setup => {
                            setup_message = toggle_watcher(&mut setup_snapshot, no_live);
                            setup_snapshot = setup::inspect(no_live).ok();
                        }
                        KeyCode::Char('a') if mode == ViewMode::Setup => {
                            setup_message = Some(apply_all_action(no_live));
                            setup_snapshot = setup::inspect(no_live).ok();
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
                                let len = setup_snapshot.as_ref().map(|s| s.accounts.len()).unwrap_or(0);
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
                                let len = setup_snapshot.as_ref().map(|s| s.accounts.len()).unwrap_or(0);
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
                                let len = setup_snapshot.as_ref().map(|s| s.accounts.len()).unwrap_or(0);
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
                            }
                        }
                        KeyCode::PageUp if mode == ViewMode::SessionDetail => {
                            chat_scroll = chat_scroll.saturating_add(10);
                        }
                        KeyCode::PageDown if mode == ViewMode::SessionDetail => {
                            chat_scroll = chat_scroll.saturating_sub(10);
                        }
                        KeyCode::Home if mode == ViewMode::SessionDetail => {
                            // Jump to oldest — cap value gets clamped in view.
                            chat_scroll = usize::MAX / 2;
                        }
                        KeyCode::End if mode == ViewMode::SessionDetail => {
                            chat_scroll = 0;
                            // Resume tailing the latest change row too.
                            changes_selection = None;
                            detail_scroll = 0;
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
                        KeyCode::Char('K') if mode == ViewMode::SessionDetail => {
                            detail_scroll = detail_scroll.saturating_sub(1);
                        }
                        KeyCode::Char('J') if mode == ViewMode::SessionDetail => {
                            detail_scroll = detail_scroll.saturating_add(1);
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
                                }
                            }
                        }
                        // Capital K only — lowercase k is reserved for
                        // future navigation use, and case sensitivity
                        // matches the muscle-memory "destructive action
                        // wants Shift" rule (Vim's D vs d, etc.).
                        KeyCode::Char('K') => {
                            // Resolve the target: in view 2 it's the
                            // pinned session; in view 1 the highlighted
                            // row. Other views: no target.
                            let target: Option<(String, String, u32)> = match mode {
                                ViewMode::SessionDetail => pinned_session
                                    .as_ref()
                                    .and_then(|(acct, sid)| {
                                        visible_sessions
                                            .iter()
                                            .find(|s| s.account_name == *acct && s.session_id == *sid)
                                            .map(|s| (s.account_name.clone(), s.session_id.clone(), s.pid))
                                    }),
                                ViewMode::AllSessions => visible_sessions
                                    .get(selected_session)
                                    .map(|s| (s.account_name.clone(), s.session_id.clone(), s.pid)),
                                _ => None,
                            };
                            let Some((acct, sid, pid)) = target else {
                                driver_status = Some((
                                    "no session selected to kill".into(),
                                    Instant::now(),
                                ));
                                continue;
                            };
                            match &pending_kill {
                                // Second press on the same session within
                                // the window: do the kill.
                                Some((p_acct, p_sid, p_pid, armed_at))
                                    if p_acct == &acct
                                        && p_sid == &sid
                                        && armed_at.elapsed() <= KILL_CONFIRM_WINDOW =>
                                {
                                    let key = (acct.clone(), sid.clone());
                                    // If mewxi owns the PTY, kill through
                                    // PtySession so the registry stays
                                    // consistent. Otherwise SIGTERM the
                                    // pid directly.
                                    let msg = if let Some(mut pty) = drivers.remove(&key) {
                                        let _ = pty.kill();
                                        format!(
                                            "killed driven session {} (pid {})",
                                            short_sid(&sid),
                                            p_pid
                                        )
                                    } else {
                                        match std::process::Command::new("kill")
                                            .arg(p_pid.to_string())
                                            .status()
                                        {
                                            Ok(s) if s.success() => format!(
                                                "sent SIGTERM to {} (pid {})",
                                                short_sid(&sid),
                                                p_pid
                                            ),
                                            Ok(s) => format!(
                                                "kill {p_pid} exited {}",
                                                s.code()
                                                    .map(|c| c.to_string())
                                                    .unwrap_or_else(|| "signal".into())
                                            ),
                                            Err(e) => format!("kill {p_pid} failed: {e}"),
                                        }
                                    };
                                    driver_status = Some((msg, Instant::now()));
                                    pending_kill = None;
                                }
                                // First press, or armed on a different
                                // session: arm the confirmation.
                                _ => {
                                    pending_kill = Some((acct, sid, pid, Instant::now()));
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
    Ok(())
}

/// Short, table-friendly form of a session-id UUID (first 8 chars).
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
    setup_rect: &mut Option<Rect>,
    visible_session_selection: Option<usize>,
    selected_account: usize,
    selected_setup: usize,
    setup: Option<&SetupSnapshot>,
    setup_message: Option<&str>,
    live_error: Option<&str>,
    driver: Option<&view_session::DriverPane<'_>>,
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
            setup_message,
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

fn flatten_sessions(accounts: &[PerAccount]) -> Vec<SessionRef> {
    let mut out: Vec<SessionRef> = accounts
        .iter()
        .flat_map(|pa| {
            pa.live_sessions.iter().map(move |ls| SessionRef {
                account_name: ls.account_name.clone(),
                session_id: ls.session_id.clone(),
                pid: ls.pid,
                project: ls.project.clone(),
                cwd: ls.cwd.clone(),
                transcript_path: ls.transcript_path.clone(),
                last_activity: ls.last_activity,
                state_since: ls.state_since,
                model: ls.model.clone(),
                tokens: ls.session_tokens.total_tokens(),
                cost_usd: ls.session_tokens.cost_usd,
                totals: ls.session_tokens.clone(),
                current_context: ls.current_context,
                context_cap: ls.context_cap,
                state: ls.state,
                activity: ls.activity.clone(),
                permission_mode: ls.permission_mode.clone(),
            })
        })
        .collect();
    // Group by project (alphabetical, case-insensitive); within each
    // project, active sessions first (newest first) then idle (newest
    // first). View 1 renders project headers above each group, and j/k
    // navigation walks this same order so the selection cursor tracks
    // the visible row order rather than jumping around.
    out.sort_by(|a, b| {
        let rank = |s: SessionState| match s {
            SessionState::Active => 0,
            SessionState::Idle => 1,
        };
        a.project
            .to_ascii_lowercase()
            .cmp(&b.project.to_ascii_lowercase())
            .then_with(|| rank(a.state).cmp(&rank(b.state)))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });
    out
}
