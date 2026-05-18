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

pub fn run(no_live: bool) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, no_live);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
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

        terminal.draw(|f| {
            render(
                f,
                mode,
                &visible_accounts,
                &visible_sessions,
                selected_session,
                selected_account,
                selected_setup,
                setup_snapshot.as_ref(),
                setup_message.as_deref(),
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
                        KeyCode::Char('1') => mode = ViewMode::AllSessions,
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
    selected_account: usize,
    selected_setup: usize,
    setup: Option<&SetupSnapshot>,
    setup_message: Option<&str>,
) {
    let area = f.area();
    // "Setup incomplete" banner — shown on every non-setup view when wiring
    // is missing, so the user discovers the fix without reading docs.
    let needs_setup = setup.is_some_and(|s| !s.fully_ok());
    let (banner_area, view_area) = if needs_setup && mode != ViewMode::Setup {
        let chunks = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Length(1),
                ratatui::layout::Constraint::Min(0),
            ])
            .split(area);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, area)
    };
    if let (Some(area), Some(snap)) = (banner_area, setup) {
        render_setup_banner(f, area, snap);
    }

    match mode {
        ViewMode::AllSessions => view_all::render(f, view_area, accounts, sessions, selected_session),
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
            "[{name}] now ignored — restart `claude-usage tui` to drop from other views"
        )),
        Ok(false) => Some(format!(
            "[{name}] un-ignored — restart `claude-usage tui` to see in other views"
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
