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
//! Linux it's a `systemd` user unit.

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
    /// The block matches our `claude-usage status` invocation.
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
    std::env::current_exe().context("resolving current executable path")
}

pub fn inspect(no_live: bool) -> Result<SetupSnapshot> {
    let binary = current_binary()?;
    let view = accounts::load_accounts()?;
    let accounts: Vec<AccountSetupState> = view
        .all_accounts()
        .map(|(a, ignored)| {
            let path = settings_path_for(a);
            let state = inspect_statusline(&path, &binary, no_live);
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

fn desired_command(binary: &Path, no_live: bool) -> String {
    if no_live {
        format!("{} --no-live status", shell_quote(binary))
    } else {
        format!("{} status", shell_quote(binary))
    }
}

fn inspect_statusline(path: &Path, binary: &Path, no_live: bool) -> StatusLineState {
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
    let desired = desired_command(binary, no_live);
    // Tolerate either spelling (with/without --no-live) as equivalent to "wired".
    let desired_other = if no_live {
        format!("{} status", shell_quote(binary))
    } else {
        format!("{} --no-live status", shell_quote(binary))
    };
    if cmd == desired || cmd == desired_other {
        StatusLineState::Wired
    } else if cmd.is_empty() {
        StatusLineState::Missing
    } else {
        StatusLineState::OtherCommand(cmd.to_string())
    }
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
        Some(v) if existing_points_at_us(v, binary, no_live) => {
            // Same command, different/missing fields → safe to overwrite
            // (this is the "add refreshInterval to an older wiring" path).
        }
        Some(_) if !force => {
            return Err(anyhow!(
                "{} has a non-claude-usage statusLine; pass force=true to overwrite",
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
fn existing_points_at_us(sl: &serde_json::Value, binary: &Path, no_live: bool) -> bool {
    let Some(cmd) = sl.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    let a = desired_command(binary, no_live);
    let b = if no_live {
        format!("{} status", shell_quote(binary))
    } else {
        format!("{} --no-live status", shell_quote(binary))
    };
    cmd == a || cmd == b
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
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents/com.claude-usage.watch.plist"))
}

#[cfg(target_os = "linux")]
fn watcher_unit_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config/systemd/user/claude-usage-watch.service"))
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
        .args(["list", "com.claude-usage.watch"])
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
        .args(["--user", "is-active", "claude-usage-watch.service"])
        .output()
    {
        Ok(o) if o.status.success() => WatcherState::Running,
        Ok(_) => WatcherState::Installed,
        Err(e) => WatcherState::Unknown(format!("systemctl: {e}")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
    <string>com.claude-usage.watch</string>
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
         Description=claude-usage status watcher\n\
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
        &["--user", "enable", "--now", "claude-usage-watch.service"],
    )?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
    run_cmd("systemctl", &["--user", "stop", "claude-usage-watch.service"])
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
    let _ = run_cmd("systemctl", &["--user", "disable", "--now", "claude-usage-watch.service"]);
    fs::remove_file(&unit_path).ok();
    let _ = run_cmd("systemctl", &["--user", "daemon-reload"]);
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
        .find(|l| l.ends_with("com.claude-usage.watch"))
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
    println!("claude-usage setup");
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
    }

    // Seed cache so statusLine has something even before watcher runs.
    if let Some(cache) = crate::stats::status_cache_path_mirror() {
        if let Some(parent) = cache.parent() {
            fs::create_dir_all(parent).ok();
        }
        let _ = atomic_write(&cache, watch::render_status(None, None, no_live).as_bytes());
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
        println!("or run `claude-usage watch` yourself.");
    }
    println!();
    println!("Done. Reload Claude Code (or start a new session) to see the status line.");
    Ok(())
}

pub fn stop(disable: bool) -> Result<()> {
    println!("claude-usage stop");
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

#[cfg(target_os = "linux")]
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
