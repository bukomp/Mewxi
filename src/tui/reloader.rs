//! Off-UI-thread reload worker.
//!
//! The mewxi frame loop targets ~16ms so the TUI stays responsive, but a
//! reload wave — walking every JSONL under an account's `projects/` dir,
//! re-aggregating usage, scanning live sessions, and attributing prices —
//! is heavy enough to stall that budget for hundreds of milliseconds on a
//! busy account. This module moves that work onto a dedicated background
//! thread: the UI thread only sends account names that need a refresh
//! ([`Reloader::mark_dirty`]) and polls for finished results
//! ([`Reloader::try_recv`]), never blocking on the actual filesystem walk
//! or aggregation.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::accounts::Account;
use crate::live_session::{self, LiveSession};
use crate::limit_attr;
use crate::stats::{self, Aggregate};

/// The outcome of a single account's reload wave, sent from the
/// background thread back to the UI thread.
pub struct ReloadResult {
    pub account_name: String,
    pub agg: Aggregate,
    pub price_by_session: HashMap<String, f64>,
    pub live_sessions: Vec<LiveSession>,
}

/// Handle to the background reload thread.
///
/// Holds the sending half of a request channel (account names the UI
/// thread wants refreshed) and the receiving half of a result channel
/// (finished [`ReloadResult`]s). Dropping the `Reloader` drops the
/// request sender, which causes the background thread's `recv` to
/// observe a disconnect and exit cleanly.
pub struct Reloader {
    req_tx: Sender<String>,
    result_rx: Receiver<ReloadResult>,
    /// Kept only so the thread isn't detached-and-forgotten; never
    /// joined — teardown is the request channel disconnecting.
    _handle: JoinHandle<()>,
}

impl Reloader {
    /// Spawn the background reload thread.
    ///
    /// `accounts` is each account paired with its already-loaded initial
    /// live sessions, so the first background scan for that account has
    /// a `previous` slice to diff against (preserving `state_since`
    /// across the handoff from the synchronous startup load).
    pub fn spawn(accounts: Vec<(Account, Vec<LiveSession>)>) -> Self {
        let (req_tx, req_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let handle = thread::spawn(move || run(req_rx, result_tx, accounts));
        Reloader {
            req_tx,
            result_rx,
            _handle: handle,
        }
    }

    /// Mark an account as needing a reload. The background thread will
    /// pick it up on its next wave, subject to the 500ms per-account
    /// debounce (a burst of `mark_dirty` calls for the same account
    /// collapses into a single reload).
    pub fn mark_dirty(&self, account_name: String) {
        let _ = self.req_tx.send(account_name);
    }

    /// Non-blocking poll for a finished reload. Call this once per UI
    /// frame; returns `None` when nothing new is ready.
    pub fn try_recv(&self) -> Option<ReloadResult> {
        self.result_rx.try_recv().ok()
    }
}

/// Pure debounce decision: is an account due for reload?
///
/// `None` (never reloaded) is always due. Otherwise due once more than
/// `debounce` has elapsed since the last reload.
fn is_due(last_reload: Option<Instant>, now: Instant, debounce: Duration) -> bool {
    match last_reload {
        None => true,
        Some(t) => now.duration_since(t) > debounce,
    }
}

/// Body of the background reload thread. Free function (rather than a
/// method) so it can be driven directly from unit tests.
///
/// Every 5 seconds all accounts are force-reloaded regardless of dirty
/// state (`force_tick`); otherwise only accounts marked dirty via
/// `req_rx` are considered. Each account is further subject to a 500ms
/// debounce: if it was reloaded more recently than that, it's skipped
/// for this wave and stays dirty so a later wave retries it.
fn run(req_rx: Receiver<String>, result_tx: Sender<ReloadResult>, accounts: Vec<(Account, Vec<LiveSession>)>) {
    const DEBOUNCE: Duration = Duration::from_millis(500);
    const FORCE_TICK_PERIOD: Duration = Duration::from_secs(5);

    let mut account_by_name: HashMap<String, Account> = HashMap::new();
    let mut previous_live: HashMap<String, Vec<LiveSession>> = HashMap::new();
    for (account, live) in accounts {
        previous_live.insert(account.name.clone(), live);
        account_by_name.insert(account.name.clone(), account);
    }

    let mut dirty: HashSet<String> = HashSet::new();
    let mut last_reload: HashMap<String, Instant> = HashMap::new();
    let mut last_full_tick = Instant::now();

    loop {
        match req_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(name) => {
                dirty.insert(name);
                while let Ok(n) = req_rx.try_recv() {
                    dirty.insert(n);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let force_tick = last_full_tick.elapsed() > FORCE_TICK_PERIOD;
        if dirty.is_empty() && !force_tick {
            continue;
        }

        let names: Vec<String> = if force_tick {
            account_by_name.keys().cloned().collect()
        } else {
            dirty.iter().cloned().collect()
        };

        let alive = live_session::alive_pids();
        let now = Instant::now();

        for name in names {
            if !is_due(last_reload.get(&name).copied(), now, DEBOUNCE) {
                continue;
            }
            let Some(account) = account_by_name.get(&name) else {
                continue;
            };

            let (records, agg) = stats::load_records_and_aggregate_for(account).unwrap_or_default();
            let ledger = limit_attr::load_ledger(account);
            let price_by_session = limit_attr::session_prices(&records, &ledger);
            let previous = previous_live.get(&name).map(|v| v.as_slice()).unwrap_or(&[]);
            let live_sessions = live_session::scan(account, &alive, previous);
            previous_live.insert(name.clone(), live_sessions.clone());

            last_reload.insert(name.clone(), Instant::now());

            if result_tx
                .send(ReloadResult {
                    account_name: name.clone(),
                    agg,
                    price_by_session,
                    live_sessions,
                })
                .is_err()
            {
                return;
            }
            dirty.remove(&name);
        }

        if force_tick {
            last_full_tick = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_due_never_reloaded_is_due() {
        let now = Instant::now();
        assert!(is_due(None, now, Duration::from_millis(500)));
    }

    #[test]
    fn is_due_just_reloaded_is_not_due() {
        let now = Instant::now();
        assert!(!is_due(Some(now), now, Duration::from_millis(500)));
    }

    #[test]
    fn is_due_past_debounce_is_due() {
        let now = Instant::now();
        let last = now.checked_sub(Duration::from_millis(600)).unwrap();
        assert!(is_due(Some(last), now, Duration::from_millis(500)));
    }

    #[test]
    fn is_due_within_debounce_is_not_due() {
        let now = Instant::now();
        let last = now.checked_sub(Duration::from_millis(400)).unwrap();
        assert!(!is_due(Some(last), now, Duration::from_millis(500)));
    }

    #[test]
    fn run_exits_cleanly_when_request_sender_dropped() {
        let (req_tx, req_rx) = mpsc::channel();
        let (result_tx, _result_rx) = mpsc::channel();
        let handle = thread::spawn(move || run(req_rx, result_tx, Vec::new()));

        drop(req_tx);

        assert!(handle.join().is_ok());
    }
}
