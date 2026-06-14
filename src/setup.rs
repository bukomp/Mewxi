//! Setup actions for `statusLine` wiring + background watcher service.
//!
//! Pure-Rust API: every action returns `Result<()>` and every inspect
//! returns a plain struct. The CLI subcommands (`setup`, `stop`) are
//! thin wrappers that print human-readable progress; the TUI's setup
//! view calls the same functions to keep behavior identical.
//!
//! Per-account: each `CLAUDE_CONFIG_DIR` has its own `settings.json`
//! and gets its own statusLine block. Wiring works on every account.
//!
//! Watcher service: a single user-scope unit (one on the box). On
//! macOS it's a `launchd` agent under `~/Library/LaunchAgents/`; on
//! Linux it's a `systemd` user unit; on Windows it's a per-user
//! `ONLOGON` Scheduled Task (`schtasks`).

use crate::accounts::{self, Account};
use crate::watch;
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// State inspection
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum StatusLineState {
    /// The block matches our `mewxi status` invocation.
    Wired,
    /// A `statusLine` block exists but points elsewhere.
    OtherCommand(String),
    /// No `statusLine` key (or the file is absent).
    Missing,
    /// The file exists but couldn't be parsed.
    Unreadable(String),
}

impl StatusLineState {
    pub fn is_ok(&self) -> bool {
        matches!(self, StatusLineState::Wired)
    }
}

#[derive(Clone, Debug)]
pub enum WatcherState {
    /// Service is installed *and* loaded/active.
    Running,
    /// Unit file exists but isn't currently active.
    Installed,
    /// No unit file present.
    NotInstalled,
    /// Couldn't determine (e.g. `launchctl` / `systemctl` missing).
    Unknown(String),
}

impl WatcherState {
    pub fn is_ok(&self) -> bool {
        matches!(self, WatcherState::Running)
    }
    pub fn short(&self) -> &'static str {
        match self {
            WatcherState::Running => "running",
            WatcherState::Installed => "stopped",
            WatcherState::NotInstalled => "not installed",
            WatcherState::Unknown(_) => "unknown",
        }
    }
}

#[derive(Clone, Debug)]
pub struct AccountSetupState {
    pub account_name: String,
    pub settings_path: PathBuf,
    pub statusline: StatusLineState,
    /// `true` when the user has marked this account as ignored in
    /// `accounts.toml`. Ignored accounts appear in the setup view (so
    /// they can be toggled back on) but are skipped by `apply_all`,
    /// the incomplete-setup banner, and every other view.
    pub ignored: bool,
}

#[derive(Clone, Debug)]
pub struct SetupSnapshot {
    pub binary: PathBuf,
    /// Active + ignored, in that order. Use `.is_ignored()` to filter.
    pub accounts: Vec<AccountSetupState>,
    pub watcher: WatcherState,
}

impl SetupSnapshot {
    /// True when every *active* account is wired AND the watcher is running.
    pub fn fully_ok(&self) -> bool {
        self.watcher.is_ok()
            && self
                .accounts
                .iter()
                .filter(|a| !a.ignored)
                .all(|a| a.statusline.is_ok())
    }
    /// Count of *active* accounts that still need wiring.
    pub fn unwired_count(&self) -> usize {
        self.accounts
            .iter()
            .filter(|a| !a.ignored && !a.statusline.is_ok())
            .count()
    }
}

pub fn current_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolving current executable path")?;
    Ok(strip_deleted_marker(exe))
}

/// On Linux, `current_exe` reads `/proc/self/exe`, which the kernel
/// resolves to `"<path> (deleted)"` once the running binary's file has
/// been replaced or removed — exactly what an in-place self-update does
/// to its own binary. Baking that string into a wired statusLine/hook
/// command produces a path that can never execute (and reads as "other
/// command" in the config view). Strip the marker so we wire the real
/// install path; a self-update writes the new binary to that same path,
/// so it's valid by the time anything runs the command.
fn strip_deleted_marker(exe: PathBuf) -> PathBuf {
    match exe.to_string_lossy().strip_suffix(" (deleted)") {
        Some(clean) => PathBuf::from(clean),
        None => exe,
    }
}

// `no_live` no longer affects inspection (statusLine detection is now
// path- and flag-independent), but the param is kept so callers don't churn.
pub fn inspect(_no_live: bool) -> Result<SetupSnapshot> {
    let binary = current_binary()?;
    let view = accounts::load_accounts()?;
    let accounts: Vec<AccountSetupState> = view
        .all_accounts()
        .map(|(a, ignored)| {
            let path = settings_path_for(a);
            let state = inspect_statusline(&path);
            AccountSetupState {
                account_name: a.name.clone(),
                settings_path: path,
                statusline: state,
                ignored,
            }
        })
        .collect();
    Ok(SetupSnapshot {
        binary,
        accounts,
        watcher: inspect_watcher(),
    })
}

fn settings_path_for(account: &Account) -> PathBuf {
    account.dir.join("settings.json")
}

/// Cheap config-health probe behind the statusLine "open mewxi" hint.
///
/// The statusLine runs every few seconds, so this deliberately avoids
/// spawning `systemctl`/`launchctl` (unlike [`inspect_watcher`]): wiring
/// is read straight from each account's settings.json, and watcher
/// liveness is inferred from the freshness of the status cache files the
/// daemon rewrites on its 15s heartbeat. Returns true when setup looks
/// incomplete (an active account isn't wired, or the watcher is down).
pub fn setup_incomplete() -> bool {
    let Ok(view) = accounts::load_accounts() else {
        return true; // can't even read our own config → definitely an issue
    };
    let any_unwired = view.all_accounts().any(|(a, ignored)| {
        !ignored && !inspect_statusline(&settings_path_for(a)).is_ok()
    });
    any_unwired || watcher_heartbeat_stale(&view)
}

/// True when the freshest `status-<slug>.txt` is missing or older than
/// 90s — i.e. the watcher daemon (15s heartbeat) isn't running.
fn watcher_heartbeat_stale(view: &accounts::AccountsView) -> bool {
    use std::time::Duration;
    let freshest = view
        .accounts
        .iter()
        .filter_map(crate::stats::status_cache_path_for)
        .filter_map(|p| fs::metadata(p).ok())
        .filter_map(|m| m.modified().ok())
        .max();
    match freshest {
        Some(t) => t.elapsed().map(|e| e > Duration::from_secs(90)).unwrap_or(false),
        None => true,
    }
}

fn desired_command(binary: &Path, no_live: bool) -> String {
    if no_live {
        format!("{} --no-live status", shell_quote(binary))
    } else {
        format!("{} status", shell_quote(binary))
    }
}

fn inspect_statusline(path: &Path) -> StatusLineState {
    if !path.exists() {
        return StatusLineState::Missing;
    }
    let Ok(raw) = fs::read_to_string(path) else {
        return StatusLineState::Unreadable("read failed".into());
    };
    if raw.trim().is_empty() {
        return StatusLineState::Missing;
    }
    let Ok(v): std::result::Result<serde_json::Value, _> = serde_json::from_str(&raw) else {
        return StatusLineState::Unreadable("parse failed".into());
    };
    let Some(sl) = v.get("statusLine") else {
        return StatusLineState::Missing;
    };
    let cmd = sl.get("command").and_then(|c| c.as_str()).unwrap_or("");
    // Match the subcommand signature, not the exact wired path: an update
    // that moves the binary (e.g. into ~/.cargo/bin) leaves the old path in
    // settings, and a path-sensitive check would mislabel it "other command".
    // Mirrors `is_mewxi_hook_command`.
    if is_mewxi_statusline_command(cmd) {
        StatusLineState::Wired
    } else if cmd.is_empty() {
        StatusLineState::Missing
    } else {
        StatusLineState::OtherCommand(cmd.to_string())
    }
}

/// True when `cmd` is our statusLine invocation — `<binary> status` or
/// `<binary> --no-live status` — regardless of where the binary lived when
/// it was wired. Keys on the subcommand signature plus a `mewxi` binary
/// basename so an update that relocates the binary still reads as "wired",
/// while a third-party `… status` command (e.g. `git status`) never matches.
fn is_mewxi_statusline_command(cmd: &str) -> bool {
    let Some(prefix) = cmd.trim().strip_suffix(" status") else {
        return false;
    };
    let prefix = prefix.strip_suffix(" --no-live").unwrap_or(prefix);
    binary_token_is_mewxi(prefix)
}

/// The leading program token of a wired command refers to the mewxi binary —
/// basename `mewxi` or `mewxi.exe`, with POSIX/Windows shell quoting stripped.
fn binary_token_is_mewxi(token: &str) -> bool {
    let token = token.trim();
    let unquoted = token
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| token.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(token);
    // A self-update can bake a `<path> (deleted)` marker into the wired
    // path (see `strip_deleted_marker`). Tolerate it so the entry is still
    // recognized as ours and gets rewired to the live path in place rather
    // than stranded as an un-upgradeable "other command".
    let cleaned = unquoted.strip_suffix(" (deleted)").unwrap_or(unquoted);
    let base = cleaned.rsplit(['/', '\\']).next().unwrap_or(cleaned);
    base == "mewxi" || base == "mewxi.exe"
}

// ---------------------------------------------------------------------------
// statusLine actions
// ---------------------------------------------------------------------------

/// Re-execute the statusLine command every N **seconds** in addition to
/// event-driven updates. Claude Code v2.1.97+ honors this; older clients
/// ignore unknown keys harmlessly. 5s matches the TUI poller cadence and
/// keeps idle terminals from showing stale numbers. NOTE: the field is in
/// seconds despite "interval" suggesting otherwise — passing milliseconds
/// here silently turns into many-minute refreshes.
const STATUSLINE_REFRESH_INTERVAL_SECS: u64 = 5;

/// Hook events we wire into each account's `settings.json` so the TUI
/// can show `awaiting` when a permission dialog is up. Claude Code
/// emits nothing to the transcript during the dialog, so without these
/// hooks we'd have no signal. See `Cmd::Hook` in `main.rs` for the
/// handler — it just touches/removes a sibling marker file.
///
/// `PermissionRequest` sets the marker; `PostToolUse` + `PostToolUseFailure`
/// both clear it (a denied/failed tool fires PostToolUseFailure, an
/// approved one fires PostToolUse, and we want the marker gone in both
/// cases). `Stop` clears too, as a belt-and-suspenders for any edge
/// case where neither post-tool event fires.
const HOOK_SET_EVENT: &str = "PermissionRequest";
const HOOK_CLEAR_EVENTS: &[&str] = &["PostToolUse", "PostToolUseFailure", "Stop"];

fn hook_command_set(binary: &Path, account_dir: &Path) -> String {
    format!(
        "{} hook awaiting-set --dir {}",
        shell_quote(binary),
        shell_quote(account_dir)
    )
}

fn hook_command_clear(binary: &Path, account_dir: &Path) -> String {
    format!(
        "{} hook awaiting-clear --dir {}",
        shell_quote(binary),
        shell_quote(account_dir)
    )
}

/// Write a `statusLine` block into `<settings_path>`, preserving any other
/// keys. Creates parent dirs if missing. Idempotent when already at the
/// desired shape; returns Ok(false) in that case. If the existing block
/// already points at our binary (by `command`) we always overwrite it —
/// that's how we upgrade pre-`refreshInterval` wirings without forcing the
/// user through the `force=true` path.
pub fn wire_statusline(settings_path: &Path, binary: &Path, no_live: bool, force: bool) -> Result<bool> {
    let desired_cmd = desired_command(binary, no_live);
    let desired = serde_json::json!({
        "type": "command",
        "command": desired_cmd,
        "refreshInterval": STATUSLINE_REFRESH_INTERVAL_SECS,
    });

    let mut root: serde_json::Value = if settings_path.exists() {
        let s = fs::read_to_string(settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?;
        if s.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&s).with_context(|| format!("parsing {}", settings_path.display()))?
        }
    } else {
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        serde_json::json!({})
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", settings_path.display()))?;

    match obj.get("statusLine") {
        Some(v) if v == &desired => return Ok(false),
        Some(v) if existing_points_at_us(v) => {
            // Same command, different/missing fields → safe to overwrite
            // (this is the "add refreshInterval to an older wiring" path).
        }
        Some(_) if !force => {
            return Err(anyhow!(
                "{} has a non-mewxi statusLine; pass force=true to overwrite",
                settings_path.display()
            ));
        }
        _ => {}
    }

    obj.insert("statusLine".to_string(), desired);
    let serialized = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write(settings_path, serialized.as_bytes())?;
    Ok(true)
}

/// True when the existing `statusLine` block already invokes our binary —
/// tolerates either spelling of the `--no-live` flag so toggling that flag
/// doesn't get classified as a third-party statusLine.
fn existing_points_at_us(sl: &serde_json::Value) -> bool {
    sl.get("command")
        .and_then(|c| c.as_str())
        .is_some_and(is_mewxi_statusline_command)
}

/// True when `cmd` is one of our hook invocations — `<binary> hook
/// awaiting-set --dir <dir>` / `<binary> hook awaiting-clear --dir
/// <dir>` — no matter where the binary lived when it was wired. Keys
/// on the `hook awaiting-*` subcommand signature, which is unique to
/// mewxi, so third-party hooks can never match. This is what lets a
/// re-run of setup clean up hooks left by an installation at a
/// different path (e.g. after moving the binary into ~/.cargo/bin).
fn is_mewxi_hook_command(cmd: &str) -> bool {
    ["hook awaiting-set", "hook awaiting-clear"].iter().any(|sub| {
        cmd.split_once(sub)
            .is_some_and(|(pre, post)| {
                pre.ends_with(' ') && (post.is_empty() || post.starts_with(' '))
            })
    })
}

/// Install (or refresh) the awaiting-permission hooks in
/// `<settings_path>`. Returns Ok(true) if anything changed. Preserves
/// any pre-existing hook entries for the same events — we add our
/// `command` to the existing array rather than replacing it. Hooks
/// from a previous install (any binary path, detected by the
/// mewxi-specific subcommand signature) get overwritten in place so a
/// binary-path change doesn't leave stale entries behind.
pub fn wire_awaiting_hooks(settings_path: &Path, binary: &Path, account_dir: &Path) -> Result<bool> {
    let mut root: serde_json::Value = if settings_path.exists() {
        let s = fs::read_to_string(settings_path)
            .with_context(|| format!("reading {}", settings_path.display()))?;
        if s.trim().is_empty() { serde_json::json!({}) } else {
            serde_json::from_str(&s).with_context(|| format!("parsing {}", settings_path.display()))?
        }
    } else {
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent).ok();
        }
        serde_json::json!({})
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", settings_path.display()))?;
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("`hooks` is not an object"))?;

    let set_cmd = hook_command_set(binary, account_dir);
    let clear_cmd = hook_command_clear(binary, account_dir);

    let mut changed = false;
    changed |= upsert_hook(hooks, HOOK_SET_EVENT, None, &set_cmd);
    for ev in HOOK_CLEAR_EVENTS {
        // Tools-matchers like "*" only apply to events that key by tool
        // (PostToolUse, PostToolUseFailure). `Stop` has no matcher.
        let matcher = if *ev == "Stop" { None } else { Some("*") };
        changed |= upsert_hook(hooks, ev, matcher, &clear_cmd);
    }
    if !changed {
        return Ok(false);
    }
    let serialized = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write(settings_path, serialized.as_bytes())?;
    Ok(true)
}

/// Make sure `hooks[event]` contains a group with the given matcher
/// whose inner `hooks` array carries `{type: command, command: cmd}`.
/// Drops any older mewxi hook entry (recognized by subcommand, not
/// binary path — see `is_mewxi_hook_command`) so re-installing with a
/// different account_dir or binary path doesn't pile up duplicates.
/// Returns true if the structure was mutated.
fn upsert_hook(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    matcher: Option<&str>,
    cmd: &str,
) -> bool {
    let arr = hooks
        .entry(event.to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()))
        .as_array_mut();
    let Some(arr) = arr else { return false };

    let want = serde_json::json!({"type": "command", "command": cmd});

    // First pass: drop any stale mewxi entry — from any past binary
    // location — so we refresh in place rather than leak duplicates.
    let mut changed = false;
    for group in arr.iter_mut() {
        let Some(grp) = group.as_object_mut() else { continue };
        let Some(inner) = grp.get_mut("hooks").and_then(|h| h.as_array_mut()) else { continue };
        let before = inner.len();
        inner.retain(|h| {
            let c = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
            !(is_mewxi_hook_command(c) && c != cmd)
        });
        if inner.len() != before {
            changed = true;
        }
    }
    // Drop groups the stale pass emptied out, so they don't linger as
    // dead weight in settings.json. Groups without an inner `hooks`
    // array are left alone — they're not ours to judge.
    if changed {
        arr.retain(|g| {
            g.get("hooks")
                .and_then(|h| h.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(true)
        });
    }

    // Already wired? Scan for an exact match with the right matcher.
    let already = arr.iter().any(|g| {
        let matcher_ok = match matcher {
            Some(m) => g.get("matcher").and_then(|x| x.as_str()) == Some(m),
            None => g.get("matcher").is_none(),
        };
        matcher_ok
            && g.get("hooks")
                .and_then(|h| h.as_array())
                .map(|a| a.iter().any(|h| h == &want))
                .unwrap_or(false)
    });
    if already {
        return changed;
    }

    // Try to append to an existing group with the matching matcher
    // (don't fragment unnecessarily) before adding a brand-new group.
    for group in arr.iter_mut() {
        let Some(grp) = group.as_object_mut() else { continue };
        let matcher_ok = match matcher {
            Some(m) => grp.get("matcher").and_then(|x| x.as_str()) == Some(m),
            None => !grp.contains_key("matcher"),
        };
        if !matcher_ok { continue; }
        let inner = grp
            .entry("hooks".to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()))
            .as_array_mut();
        if let Some(inner) = inner {
            inner.push(want);
            return true;
        }
    }

    let mut new_group = serde_json::Map::new();
    if let Some(m) = matcher {
        new_group.insert("matcher".into(), serde_json::Value::String(m.into()));
    }
    new_group.insert("hooks".into(), serde_json::Value::Array(vec![want]));
    arr.push(serde_json::Value::Object(new_group));
    true
}

/// Remove our awaiting-permission hooks from `<settings_path>` —
/// matched by the mewxi-specific subcommand signature, so hooks wired
/// by an installation at any past binary path are removed too. Non-
/// mewxi hooks are never touched. No-op if absent. Returns Ok(true)
/// if a change was made.
#[allow(dead_code)]
pub fn unwire_awaiting_hooks(settings_path: &Path) -> Result<bool> {
    if !settings_path.exists() { return Ok(false); }
    let s = fs::read_to_string(settings_path)?;
    if s.trim().is_empty() { return Ok(false); }
    let mut root: serde_json::Value = serde_json::from_str(&s)
        .with_context(|| format!("parsing {}", settings_path.display()))?;
    let Some(obj) = root.as_object_mut() else { return Ok(false) };
    let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(false);
    };
    let mut changed = false;
    for events in std::iter::once(HOOK_SET_EVENT).chain(HOOK_CLEAR_EVENTS.iter().copied()) {
        let Some(arr) = hooks.get_mut(events).and_then(|v| v.as_array_mut()) else { continue };
        for group in arr.iter_mut() {
            let Some(grp) = group.as_object_mut() else { continue };
            let Some(inner) = grp.get_mut("hooks").and_then(|h| h.as_array_mut()) else { continue };
            let before = inner.len();
            inner.retain(|h| {
                let c = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                !is_mewxi_hook_command(c)
            });
            if inner.len() != before { changed = true; }
        }
        arr.retain(|g| {
            g.get("hooks").and_then(|h| h.as_array()).map(|a| !a.is_empty()).unwrap_or(true)
        });
        if arr.is_empty() {
            hooks.remove(events);
            changed = true;
        }
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    if changed {
        let serialized = serde_json::to_string_pretty(&root)? + "\n";
        atomic_write(settings_path, serialized.as_bytes())?;
    }
    Ok(changed)
}

/// Remove the `statusLine` block from `<settings_path>`. No-op if the
/// file or key doesn't exist. Returns Ok(true) if a change was made.
pub fn unwire_statusline(settings_path: &Path) -> Result<bool> {
    if !settings_path.exists() {
        return Ok(false);
    }
    let s = fs::read_to_string(settings_path)?;
    if s.trim().is_empty() {
        return Ok(false);
    }
    let mut root: serde_json::Value = serde_json::from_str(&s)
        .with_context(|| format!("parsing {}", settings_path.display()))?;
    let Some(obj) = root.as_object_mut() else { return Ok(false) };
    if obj.remove("statusLine").is_none() {
        return Ok(false);
    }
    let serialized = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write(settings_path, serialized.as_bytes())?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Watcher service actions
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn watcher_plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents/com.mewxi.watch.plist"))
}

#[cfg(target_os = "linux")]
fn watcher_unit_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config/systemd/user/mewxi-watch.service"))
}

#[cfg(target_os = "macos")]
fn inspect_watcher() -> WatcherState {
    let Some(plist) = watcher_plist_path() else {
        return WatcherState::Unknown("no home dir".into());
    };
    if !plist.exists() {
        return WatcherState::NotInstalled;
    }
    // `launchctl list <label>` exits 0 when the agent is loaded.
    match Command::new("launchctl")
        .args(["list", "com.mewxi.watch"])
        .output()
    {
        Ok(o) if o.status.success() => WatcherState::Running,
        Ok(_) => WatcherState::Installed,
        Err(e) => WatcherState::Unknown(format!("launchctl: {e}")),
    }
}

#[cfg(target_os = "linux")]
fn inspect_watcher() -> WatcherState {
    let Some(unit) = watcher_unit_path() else {
        return WatcherState::Unknown("no home dir".into());
    };
    if !unit.exists() {
        return WatcherState::NotInstalled;
    }
    match Command::new("systemctl")
        .args(["--user", "is-active", "mewxi-watch.service"])
        .output()
    {
        Ok(o) if o.status.success() => WatcherState::Running,
        Ok(_) => WatcherState::Installed,
        Err(e) => WatcherState::Unknown(format!("systemctl: {e}")),
    }
}

/// Windows: the watcher runs as a per-user Scheduled Task triggered
/// `ONLOGON` (the closest analogue to a launchd agent / systemd user
/// unit — survives reboots, no admin rights needed).
#[cfg(windows)]
const WATCH_TASK_NAME: &str = "mewxi-watch";

#[cfg(windows)]
fn inspect_watcher() -> WatcherState {
    match Command::new("schtasks")
        .args(["/Query", "/TN", WATCH_TASK_NAME, "/FO", "LIST", "/V"])
        .output()
    {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            // The verbose LIST view carries a `Status: Running/Ready` line;
            // a long-lived `mewxi watch` process keeps it at Running.
            let running = text.lines().any(|l| {
                let l = l.trim();
                l.starts_with("Status:") && l.contains("Running")
            });
            if running {
                WatcherState::Running
            } else {
                WatcherState::Installed
            }
        }
        // schtasks exits non-zero when the task doesn't exist.
        Ok(_) => WatcherState::NotInstalled,
        Err(e) => WatcherState::Unknown(format!("schtasks: {e}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn inspect_watcher() -> WatcherState {
    WatcherState::Unknown("unsupported platform".into())
}

#[cfg(target_os = "macos")]
pub fn install_watcher(binary: &Path, no_live: bool) -> Result<()> {
    let plist_path = watcher_plist_path().ok_or_else(|| anyhow!("no home dir"))?;
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent).context("creating ~/Library/LaunchAgents")?;
    }

    let mut args_xml = format!(
        "        <string>{}</string>\n",
        xml_escape(&binary.display().to_string())
    );
    if no_live {
        args_xml.push_str("        <string>--no-live</string>\n");
    }
    args_xml.push_str("        <string>watch</string>\n");

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.mewxi.watch</string>
    <key>ProgramArguments</key>
    <array>
{args_xml}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
</dict>
</plist>
"#
    );
    atomic_write(&plist_path, plist.as_bytes())?;

    let plist_str = plist_path.to_string_lossy().into_owned();
    // Reload to pick up any binary-path change.
    let _ = Command::new("launchctl").args(["unload", &plist_str]).output();
    let out = Command::new("launchctl")
        .args(["load", "-w", &plist_str])
        .output()
        .map_err(|e| anyhow!("launchctl: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "launchctl load failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn install_watcher(binary: &Path, no_live: bool) -> Result<()> {
    let unit_path = watcher_unit_path().ok_or_else(|| anyhow!("no home dir"))?;
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent).context("creating ~/.config/systemd/user")?;
    }

    let exec_start = if no_live {
        format!("{} --no-live watch", binary.display())
    } else {
        format!("{} watch", binary.display())
    };
    let unit = format!(
        "[Unit]\n\
         Description=mewxi status watcher\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    );
    atomic_write(&unit_path, unit.as_bytes())?;
    run_cmd("systemctl", &["--user", "daemon-reload"])?;
    run_cmd(
        "systemctl",
        &["--user", "enable", "--now", "mewxi-watch.service"],
    )?;
    Ok(())
}

#[cfg(windows)]
pub fn install_watcher(binary: &Path, no_live: bool) -> Result<()> {
    let tr = if no_live {
        format!("\"{}\" --no-live watch", binary.display())
    } else {
        format!("\"{}\" watch", binary.display())
    };
    // `/F` recreates the task so a binary-path change is picked up.
    // `/RL LIMITED` keeps it in the user's normal token (no elevation).
    run_cmd(
        "schtasks",
        &[
            "/Create", "/TN", WATCH_TASK_NAME, "/SC", "ONLOGON", "/TR", &tr, "/F", "/RL", "LIMITED",
        ],
    )?;
    // Start it now too, so setup doesn't have to wait for the next logon.
    // Best-effort: ignore failure (e.g. an instance is already running).
    let _ = Command::new("schtasks")
        .args(["/Run", "/TN", WATCH_TASK_NAME])
        .output();
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn install_watcher(_binary: &Path, _no_live: bool) -> Result<()> {
    Err(anyhow!("watcher service install is not supported on this OS"))
}

#[cfg(target_os = "macos")]
pub fn stop_watcher_now() -> Result<()> {
    let plist_path = watcher_plist_path().ok_or_else(|| anyhow!("no home dir"))?;
    if !plist_path.exists() {
        return Ok(());
    }
    let plist_str = plist_path.to_string_lossy().into_owned();
    let out = Command::new("launchctl")
        .args(["unload", &plist_str])
        .output()
        .map_err(|e| anyhow!("launchctl: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "launchctl unload failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn stop_watcher_now() -> Result<()> {
    let unit_path = watcher_unit_path().ok_or_else(|| anyhow!("no home dir"))?;
    if !unit_path.exists() {
        return Ok(());
    }
    run_cmd("systemctl", &["--user", "stop", "mewxi-watch.service"])
}

#[cfg(windows)]
pub fn stop_watcher_now() -> Result<()> {
    // `/End` stops the running instance; the task stays registered so it
    // comes back on next logon. Idempotent — no-op when not installed.
    if matches!(inspect_watcher(), WatcherState::NotInstalled) {
        return Ok(());
    }
    let _ = Command::new("schtasks")
        .args(["/End", "/TN", WATCH_TASK_NAME])
        .output();
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn stop_watcher_now() -> Result<()> {
    Err(anyhow!("watcher service stop is not supported on this OS"))
}

#[cfg(target_os = "macos")]
pub fn uninstall_watcher() -> Result<()> {
    let plist_path = watcher_plist_path().ok_or_else(|| anyhow!("no home dir"))?;
    if !plist_path.exists() {
        return Ok(());
    }
    let plist_str = plist_path.to_string_lossy().into_owned();
    // `-w` persists the stop so launchd won't re-enable on next login.
    let _ = Command::new("launchctl").args(["unload", "-w", &plist_str]).output();
    fs::remove_file(&plist_path).ok();
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall_watcher() -> Result<()> {
    let unit_path = watcher_unit_path().ok_or_else(|| anyhow!("no home dir"))?;
    if !unit_path.exists() {
        return Ok(());
    }
    let _ = run_cmd("systemctl", &["--user", "disable", "--now", "mewxi-watch.service"]);
    fs::remove_file(&unit_path).ok();
    let _ = run_cmd("systemctl", &["--user", "daemon-reload"]);
    Ok(())
}

#[cfg(windows)]
pub fn uninstall_watcher() -> Result<()> {
    if matches!(inspect_watcher(), WatcherState::NotInstalled) {
        return Ok(());
    }
    // Stop the live instance first, then drop the task registration.
    let _ = Command::new("schtasks")
        .args(["/End", "/TN", WATCH_TASK_NAME])
        .output();
    run_cmd("schtasks", &["/Delete", "/TN", WATCH_TASK_NAME, "/F"])
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn uninstall_watcher() -> Result<()> {
    Err(anyhow!("watcher service uninstall is not supported on this OS"))
}

// ---------------------------------------------------------------------------
// Stale-binary detection + auto-restart
//
// `launchctl` (and `systemd`) only respawn a watcher process when it
// exits. If the user upgrades the binary on disk, the running watcher
// stays loaded with the *previous* binary in memory — and keeps writing
// stale cache files that the TUI then reads. We detect this on every
// TUI launch and silently reload the unit so the next read is fresh.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn running_watcher_pid() -> Option<u32> {
    // `launchctl list` output is whitespace-separated:  PID  STATUS  LABEL
    // For an unloaded/exited unit the PID column is "-".
    let out = Command::new("launchctl").arg("list").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find(|l| l.ends_with("com.mewxi.watch"))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|s| s.parse().ok())
}

#[cfg(target_os = "macos")]
fn process_start_epoch(pid: u32) -> Option<i64> {
    // `ps -o lstart=` prints e.g. "Mon May 18 14:30:15 2026".
    // BSD `date -j -f` parses that back to a Unix timestamp.
    let ps = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !ps.status.success() {
        return None;
    }
    let lstart = String::from_utf8_lossy(&ps.stdout).trim().to_string();
    if lstart.is_empty() {
        return None;
    }
    let date = Command::new("date")
        .args(["-j", "-f", "%a %b %e %H:%M:%S %Y", &lstart, "+%s"])
        .output()
        .ok()?;
    if !date.status.success() {
        return None;
    }
    String::from_utf8_lossy(&date.stdout).trim().parse().ok()
}

#[cfg(target_os = "macos")]
fn plist_program_path(plist_text: &str) -> Option<PathBuf> {
    // ProgramArguments holds <string>BINARY</string> followed by zero or
    // more flag strings. We want the first one.
    let after = plist_text.split_once("<key>ProgramArguments</key>")?.1;
    let open = after.find("<string>")? + "<string>".len();
    let close_rel = after[open..].find("</string>")?;
    Some(PathBuf::from(&after[open..open + close_rel]))
}

#[cfg(target_os = "macos")]
fn binary_mtime_epoch(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    Some(mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// True when the launchd-managed watcher is currently running a binary
/// older than the one on disk — i.e. the user upgraded the binary but
/// the daemon is still executing the previous image. Best-effort:
/// returns `false` on any inability to determine state (so we never
/// "auto-restart" something we don't understand).
#[cfg(target_os = "macos")]
pub fn watcher_binary_is_stale() -> bool {
    let Some(pid) = running_watcher_pid() else { return false };
    let Some(start_epoch) = process_start_epoch(pid) else { return false };
    let Some(plist) = watcher_plist_path() else { return false };
    let Some(plist_text) = fs::read_to_string(&plist).ok() else { return false };
    let Some(bin_path) = plist_program_path(&plist_text) else { return false };
    let Some(bin_epoch) = binary_mtime_epoch(&bin_path) else { return false };
    // 2-second slop in case of fs timestamp rounding.
    bin_epoch > start_epoch + 2
}

#[cfg(not(target_os = "macos"))]
pub fn watcher_binary_is_stale() -> bool {
    false
}

/// If the watcher is running an out-of-date binary, refresh it by
/// reinstalling the launch unit (which unloads-then-loads on macOS).
/// Returns a one-line message for the setup banner, or `None` when
/// nothing was done.
pub fn restart_watcher_if_stale(binary: &Path, no_live: bool) -> Option<String> {
    if !watcher_binary_is_stale() {
        return None;
    }
    match install_watcher(binary, no_live) {
        Ok(()) => Some("watcher binary refreshed (was stale)".into()),
        Err(e) => Some(format!("watcher refresh FAILED: {e}")),
    }
}

// ---------------------------------------------------------------------------
// "Apply everything that's missing" helper used by the TUI's auto-setup
// ---------------------------------------------------------------------------

pub struct ApplyOutcome {
    pub wired_accounts: Vec<String>,
    pub watcher_installed: bool,
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

pub fn apply_all(force: bool, no_live: bool) -> Result<ApplyOutcome> {
    let mut out = ApplyOutcome {
        wired_accounts: Vec::new(),
        watcher_installed: false,
        skipped: Vec::new(),
        errors: Vec::new(),
    };
    let snap = inspect(no_live)?;
    let view = accounts::load_accounts()?;
    for acct in snap.accounts.iter().filter(|a| !a.ignored) {
        match &acct.statusline {
            StatusLineState::OtherCommand(cmd) if !force => {
                out.skipped.push(format!(
                    "{}: keeping existing statusLine ({})",
                    acct.account_name, cmd
                ));
            }
            // Wired blocks fall through so wire_statusline can upgrade them
            // (e.g. to add refreshInterval to an older wiring). The call is
            // idempotent — returns Ok(false) when nothing changed.
            _ => match wire_statusline(&acct.settings_path, &snap.binary, no_live, force) {
                Ok(true) => out.wired_accounts.push(acct.account_name.clone()),
                Ok(false) => {}
                Err(e) => out.errors.push(format!("{}: {e}", acct.account_name)),
            },
        }
        // Hooks live alongside the statusLine — quietly wire them in
        // even when statusLine wasn't touched, so the TUI's `awaiting`
        // status starts working after a single re-run of setup.
        if let Some(acct_full) = view.all_accounts().find(|(a, _)| a.name == acct.account_name).map(|(a, _)| a) {
            if let Err(e) = wire_awaiting_hooks(&acct.settings_path, &snap.binary, &acct_full.dir) {
                out.errors.push(format!("{} hooks: {e}", acct.account_name));
            }
        }
    }
    if !snap.watcher.is_ok() {
        match install_watcher(&snap.binary, no_live) {
            Ok(()) => out.watcher_installed = true,
            Err(e) => out.errors.push(format!("watcher: {e}")),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// CLI entrypoints (`setup` / `stop`) — thin wrappers that print progress
// ---------------------------------------------------------------------------

pub fn run(install_service: bool, force: bool, no_live: bool) -> Result<()> {
    let snap = inspect(no_live)?;
    let view = accounts::load_accounts()?;
    println!("mewxi setup");
    println!("  binary:   {}", snap.binary.display());
    println!();
    for acct in snap.accounts.iter().filter(|a| !a.ignored) {
        match &acct.statusline {
            StatusLineState::OtherCommand(cmd) if !force => {
                println!("  [{}] statusLine set to something else:", acct.account_name);
                println!("        {cmd}");
                println!("        re-run with --force to overwrite");
            }
            // Wired falls through so wire_statusline can upgrade in place
            // (e.g. fix a refreshInterval that was written with the wrong
            // unit by an older binary). It returns Ok(false) when nothing
            // actually changed.
            _ => match wire_statusline(&acct.settings_path, &snap.binary, no_live, force) {
                Ok(true) => println!("  [{}] wrote statusLine to {}", acct.account_name, acct.settings_path.display()),
                Ok(false) => println!("  [{}] statusLine already wired ({})", acct.account_name, acct.settings_path.display()),
                Err(e) => println!("  [{}] FAILED: {e}", acct.account_name),
            },
        }
        if let Some(acct_full) = view.all_accounts().find(|(a, _)| a.name == acct.account_name).map(|(a, _)| a) {
            match wire_awaiting_hooks(&acct.settings_path, &snap.binary, &acct_full.dir) {
                Ok(true) => println!("  [{}] installed awaiting-permission hooks", acct.account_name),
                Ok(false) => {}
                Err(e) => println!("  [{}] hooks FAILED: {e}", acct.account_name),
            }
        }
    }

    // Seed cache so statusLine has something even before watcher runs.
    if let Some(cache) = crate::stats::status_cache_path_mirror() {
        if let Some(parent) = cache.parent() {
            fs::create_dir_all(parent).ok();
        }
        let _ = atomic_write(
            &cache,
            watch::render_status(None, watch::SessionMeta::default(), no_live).as_bytes(),
        );
        println!();
        println!("  cache:    seeded {}", cache.display());
    }

    if install_service {
        println!();
        match install_watcher(&snap.binary, no_live) {
            Ok(()) => println!("  service:  installed and started"),
            Err(e) => println!("  service:  FAILED: {e}"),
        }
    } else {
        println!();
        println!("To keep the status fresh, re-run with --service to install a user service unit,");
        println!("or run `mewxi watch` yourself.");
    }
    println!();
    println!("Done. Reload Claude Code (or start a new session) to see the status line.");
    Ok(())
}

pub fn stop(disable: bool) -> Result<()> {
    println!("mewxi stop");
    if disable {
        match uninstall_watcher() {
            Ok(()) => println!("  service:  uninstalled"),
            Err(e) => println!("  service:  FAILED: {e}"),
        }
    } else {
        match stop_watcher_now() {
            Ok(()) => println!("  service:  stopped"),
            Err(e) => println!("  service:  FAILED: {e}"),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", windows))]
fn run_cmd(bin: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("running `{bin} {}`", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "`{bin} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_data().ok();
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Quote a path for embedding in the `statusLine`/hook command strings
/// that Claude Code later runs through the platform shell. POSIX uses
/// single-quote escaping; Windows (`cmd.exe`) uses double quotes and
/// treats the backslash separator as an ordinary character.
#[cfg(not(windows))]
fn shell_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    let safe = |c: char| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | ':' | '+' | '=' | ',')
    };
    if !s.is_empty() && s.chars().all(safe) {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(windows)]
fn shell_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    // Backslash and colon are normal in Windows paths, so they're left
    // unquoted; anything outside the safe set (notably spaces) forces a
    // double-quoted form, which is what cmd.exe understands.
    let safe = |c: char| {
        c.is_ascii_alphanumeric()
            || matches!(c, '/' | '\\' | '_' | '-' | '.' | ':' | '+' | '=' | ',')
    };
    if !s.is_empty() && s.chars().all(safe) {
        s.into_owned()
    } else {
        format!("\"{}\"", s.replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mewxi_hook_command_detection() {
        // Any binary path counts, including quoted ones.
        assert!(is_mewxi_hook_command(
            "/old/place/mewxi hook awaiting-set --dir /home/u/.claude"
        ));
        assert!(is_mewxi_hook_command(
            "'/path with spaces/mewxi' hook awaiting-clear --dir '/d ir'"
        ));
        assert!(is_mewxi_hook_command(
            "/Users/u/.cargo/bin/mewxi hook awaiting-clear --dir /x"
        ));
        // Third-party hooks must never match.
        assert!(!is_mewxi_hook_command("notify-send 'tool done'"));
        assert!(!is_mewxi_hook_command("/usr/bin/other-tool hook something"));
        assert!(!is_mewxi_hook_command("echo hook awaiting-setup")); // not our subcommand
        assert!(!is_mewxi_hook_command("hook awaiting-set --dir /x")); // no binary part
    }

    #[test]
    fn mewxi_statusline_command_detection() {
        // Any binary path counts (so an update that moves the binary still
        // reads as "wired"), with or without --no-live, quoted or not.
        assert!(is_mewxi_statusline_command("/old/place/mewxi status"));
        assert!(is_mewxi_statusline_command("/home/u/.cargo/bin/mewxi status"));
        assert!(is_mewxi_statusline_command("/old/place/mewxi --no-live status"));
        assert!(is_mewxi_statusline_command("'/path with spaces/mewxi' status"));
        assert!(is_mewxi_statusline_command(r#""C:\Program Files\mewxi.exe" status"#));
        assert!(is_mewxi_statusline_command(r"C:\tools\mewxi.exe --no-live status"));
        // A self-update can leave a `(deleted)` marker on the path; still
        // recognized as ours so it heals to the live path in place.
        assert!(is_mewxi_statusline_command(
            "'/home/u/.cargo/bin/mewxi (deleted)' status"
        ));
        assert_eq!(
            strip_deleted_marker(PathBuf::from("/home/u/.cargo/bin/mewxi (deleted)")),
            PathBuf::from("/home/u/.cargo/bin/mewxi")
        );
        assert_eq!(
            strip_deleted_marker(PathBuf::from("/home/u/.cargo/bin/mewxi")),
            PathBuf::from("/home/u/.cargo/bin/mewxi")
        );
        // Third-party `… status` commands must never match.
        assert!(!is_mewxi_statusline_command("git status"));
        assert!(!is_mewxi_statusline_command("/usr/bin/other-tool status"));
        assert!(!is_mewxi_statusline_command("mewxi tui")); // wrong subcommand
        assert!(!is_mewxi_statusline_command("")); // empty
    }

    fn settings_with(hooks: serde_json::Value) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = serde_json::json!({"hooks": hooks});
        fs::write(
            dir.path().join("settings.json"),
            serde_json::to_string_pretty(&root).unwrap(),
        )
        .unwrap();
        dir
    }

    fn read_hooks(dir: &tempfile::TempDir) -> serde_json::Value {
        let s = fs::read_to_string(dir.path().join("settings.json")).unwrap();
        serde_json::from_str::<serde_json::Value>(&s).unwrap()["hooks"].clone()
    }

    #[test]
    fn rewire_replaces_old_binary_path_hooks() {
        let dir = settings_with(serde_json::json!({
            "PermissionRequest": [
                {"hooks": [{"type": "command", "command": "/old/mewxi hook awaiting-set --dir /acct"}]}
            ],
            "PostToolUse": [
                {"matcher": "*", "hooks": [
                    {"type": "command", "command": "/old/mewxi hook awaiting-clear --dir /acct"},
                    {"type": "command", "command": "third-party-formatter"}
                ]}
            ]
        }));
        let settings = dir.path().join("settings.json");
        let changed =
            wire_awaiting_hooks(&settings, Path::new("/new/mewxi"), Path::new("/acct")).unwrap();
        assert!(changed);
        let hooks = read_hooks(&dir);
        let as_cmds = |ev: &str| -> Vec<String> {
            hooks[ev]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                .map(|h| h["command"].as_str().unwrap().to_string())
                .collect()
        };
        // Old-path mewxi hooks are gone, new ones present exactly once.
        assert_eq!(as_cmds("PermissionRequest"), vec!["/new/mewxi hook awaiting-set --dir /acct"]);
        // Non-mewxi entry untouched, old mewxi entry replaced.
        let post = as_cmds("PostToolUse");
        assert!(post.contains(&"third-party-formatter".to_string()));
        assert!(post.contains(&"/new/mewxi hook awaiting-clear --dir /acct".to_string()));
        assert!(!post.iter().any(|c| c.starts_with("/old/")));
    }

    #[test]
    fn rewire_prunes_groups_emptied_by_stale_drop() {
        // Old install used a different matcher, so the stale pass empties
        // that group entirely — it should be removed, not left as `[]`.
        let dir = settings_with(serde_json::json!({
            "PostToolUse": [
                {"matcher": "Bash", "hooks": [
                    {"type": "command", "command": "/old/mewxi hook awaiting-clear --dir /acct"}
                ]}
            ]
        }));
        let settings = dir.path().join("settings.json");
        wire_awaiting_hooks(&settings, Path::new("/new/mewxi"), Path::new("/acct")).unwrap();
        let groups = read_hooks(&dir)["PostToolUse"].as_array().unwrap().clone();
        assert!(groups.iter().all(|g| !g["hooks"].as_array().unwrap().is_empty()));
        assert!(!groups
            .iter()
            .any(|g| g.get("matcher").and_then(|m| m.as_str()) == Some("Bash")));
    }

    #[test]
    fn unwire_removes_any_path_keeps_foreign() {
        let dir = settings_with(serde_json::json!({
            "PermissionRequest": [
                {"hooks": [{"type": "command", "command": "/somewhere/else/mewxi hook awaiting-set --dir /acct"}]}
            ],
            "Stop": [
                {"hooks": [
                    {"type": "command", "command": "/old/mewxi hook awaiting-clear --dir /acct"},
                    {"type": "command", "command": "say done"}
                ]}
            ],
            "SessionStart": [
                {"hooks": [{"type": "command", "command": "foreign-init"}]}
            ]
        }));
        let settings = dir.path().join("settings.json");
        assert!(unwire_awaiting_hooks(&settings).unwrap());
        let hooks = read_hooks(&dir);
        assert!(hooks.get("PermissionRequest").is_none());
        let stop_cmds: Vec<&str> = hooks["Stop"].as_array().unwrap().iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter())
            .map(|h| h["command"].as_str().unwrap())
            .collect();
        assert_eq!(stop_cmds, vec!["say done"]);
        // Events we don't own are untouched.
        assert_eq!(
            hooks["SessionStart"][0]["hooks"][0]["command"],
            "foreign-init"
        );
    }
}
