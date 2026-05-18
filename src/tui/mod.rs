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

mod view_account;
mod view_all;
mod view_session;
mod view_setup;
mod widgets;

use crate::accounts::{self, Account, AccountsView};
use crate::live_session::{self, LiveSession, SessionState};
use crate::live_usage::{self, LiveUsage};
use crate::setup::{self, SetupSnapshot};
use crate::stats::{self, Aggregate, UsageTotals};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::backend::CrosstermBackend;
use ratatui::Frame;
use ratatui::Terminal;
use std::collections::HashMap;
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
    pub project: String,
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
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum ViewMode {
    AllSessions,
    SessionDetail,
    AccountDetail,
    Setup,
}

/// Stylised cat-face brand logo. The fixed 100-col left/right padding
/// in the file lets `Alignment::Center` place the glyph cleanly in
/// wider terminals without us computing offsets ourselves.
const LOGO_ASCII: &str = include_str!("../../images/muxi.ascii");
const LOGO_CONTENT_HEIGHT: u16 = 28;

/// Big "Muxi" in standard-figlet ASCII line-art, mixed case. 24 cols ×
/// 5 rows. Trailing whitespace on a line before `\n\` is preserved —
/// only the source newline + leading indent after `\` is consumed.
const MUXI_BIG: &str = " __  __               _ \n\
                        |  \\/  | _   _ __  __(_)\n\
                        | |\\/| || | | |\\ \\/ /| |\n\
                        | |  | || |_| | >  < | |\n\
                        |_|  |_| \\__,_|/_/\\_\\|_|";
const MUXI_BIG_HEIGHT: u16 = 5;
const MUXI_BIG_WIDTH: u16 = 24;

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
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let _ = show_splash(&mut terminal, SPLASH_DURATION);
    let result = run_loop(&mut terminal, no_live);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
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
    let want_full = area.height >= LOGO_CONTENT_HEIGHT + gap_h + MUXI_BIG_HEIGHT + tagline_h
        && area.width >= MUXI_BIG_WIDTH + 4;
    let want_big = area.height >= MUXI_BIG_HEIGHT + tagline_h
        && area.width >= MUXI_BIG_WIDTH + 4;

    if !want_big {
        let line = Line::from(Span::styled(
            "Muxi",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
        f.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
        return;
    }

    let mut constraints: Vec<Constraint> = Vec::new();
    let total_h = if want_full {
        LOGO_CONTENT_HEIGHT + gap_h + MUXI_BIG_HEIGHT + tagline_h
    } else {
        MUXI_BIG_HEIGHT + tagline_h
    };
    let top_pad = area.height.saturating_sub(total_h) / 2;
    constraints.push(Constraint::Length(top_pad));
    if want_full {
        constraints.push(Constraint::Length(LOGO_CONTENT_HEIGHT));
        constraints.push(Constraint::Length(gap_h));
    }
    constraints.push(Constraint::Length(MUXI_BIG_HEIGHT));
    constraints.push(Constraint::Length(tagline_h));
    constraints.push(Constraint::Min(0));

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut idx = 1; // skip top_pad

    if want_full {
        // Strip the file's blank padding rows so LOGO_CONTENT_HEIGHT is
        // the actual rendered height.
        let logo_lines: Vec<Line> = LOGO_ASCII
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(Color::Magenta),
                ))
            })
            .collect();
        f.render_widget(
            Paragraph::new(logo_lines).alignment(Alignment::Center),
            chunks[idx],
        );
        idx += 2; // skip logo + gap
    }

    let muxi_lines: Vec<Line> = MUXI_BIG
        .lines()
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    f.render_widget(
        Paragraph::new(muxi_lines).alignment(Alignment::Center),
        chunks[idx],
    );
    idx += 1;

    let tagline = Line::from(Span::styled(
        "multi-agent CLI usage tracker",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(
        Paragraph::new(tagline).alignment(Alignment::Center),
        chunks[idx],
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

        let visible_selection = if last_session_select.elapsed() < SELECTION_VISIBLE {
            Some(selected_session)
        } else {
            None
        };

        let live_error = live_usage::most_recent_error()
            .map(|(acct, msg)| format!("[{acct}] {msg}"));

        terminal.draw(|f| {
            render(
                f,
                mode,
                &visible_accounts,
                &visible_sessions,
                selected_session,
                visible_selection,
                selected_account,
                selected_setup,
                setup_snapshot.as_ref(),
                setup_message.as_deref(),
                live_error.as_deref(),
            )
        })?;

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

        // Keyboard.
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            for (_, cmd_tx) in &live_pollers {
                                let _ = cmd_tx.send(LiveCmd::Stop);
                            }
                            break;
                        }
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
                        KeyCode::Char('1') => {
                            mode = ViewMode::AllSessions;
                            last_session_select = Instant::now();
                        }
                        KeyCode::Char('2') => mode = ViewMode::SessionDetail,
                        KeyCode::Char('3') => mode = ViewMode::AccountDetail,
                        KeyCode::Char('4') => {
                            setup_snapshot = setup::inspect(no_live).ok();
                            mode = ViewMode::Setup;
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
                        },
                        KeyCode::BackTab => match mode {
                            ViewMode::AllSessions | ViewMode::SessionDetail => {
                                if !sessions.is_empty() {
                                    selected_session = (selected_session + sessions.len() - 1)
                                        % sessions.len();
                                    last_session_select = Instant::now();
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
                        },
                        KeyCode::Down => match mode {
                            ViewMode::AllSessions | ViewMode::SessionDetail => {
                                if !sessions.is_empty() {
                                    selected_session = (selected_session + 1).min(sessions.len() - 1);
                                    last_session_select = Instant::now();
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
                        },
                        KeyCode::Up => match mode {
                            ViewMode::AllSessions | ViewMode::SessionDetail => {
                                if selected_session > 0 {
                                    selected_session -= 1;
                                    last_session_select = Instant::now();
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
                        },
                        KeyCode::Enter => {
                            if mode == ViewMode::AllSessions && !sessions.is_empty() {
                                mode = ViewMode::SessionDetail;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render(
    f: &mut Frame,
    mode: ViewMode,
    accounts: &[&PerAccount],
    sessions: &[&SessionRef],
    selected_session: usize,
    visible_session_selection: Option<usize>,
    selected_account: usize,
    selected_setup: usize,
    setup: Option<&SetupSnapshot>,
    setup_message: Option<&str>,
    live_error: Option<&str>,
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
        ViewMode::AllSessions => {
            view_all::render(f, view_area, accounts, sessions, visible_session_selection)
        }
        ViewMode::SessionDetail => {
            view_session::render(f, view_area, accounts, sessions.get(selected_session).copied())
        }
        ViewMode::AccountDetail => {
            if let Some(pa) = accounts.get(selected_account) {
                view_account::render(f, view_area, pa);
            }
        }
        ViewMode::Setup => view_setup::render(f, view_area, setup, selected_setup, setup_message),
    }
}

fn render_top_header(f: &mut Frame, area: ratatui::layout::Rect) {
    use ratatui::layout::Alignment;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    let line = Line::from(vec![Span::styled(
        "Muxi",
        Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
    )]);
    f.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);
}

fn render_error_footer(f: &mut Frame, area: ratatui::layout::Rect, msg: &str) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red))
        .title(Span::styled(
            " live fetch error ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
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
            "[{name}] now ignored — restart `muxi tui` to drop from other views"
        )),
        Ok(false) => Some(format!(
            "[{name}] un-ignored — restart `muxi tui` to see in other views"
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
                project: ls.project.clone(),
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
            })
        })
        .collect();
    // Active sessions first (newest first), then idle (newest first) —
    // mirrors the per-account scan order so view 1's table looks the
    // same whether you have one account or many.
    out.sort_by(|a, b| {
        let rank = |s: SessionState| match s {
            SessionState::Active => 0,
            SessionState::Idle => 1,
        };
        rank(a.state)
            .cmp(&rank(b.state))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });
    out
}
