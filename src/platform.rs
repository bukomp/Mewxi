//! Small cross-platform shims for the few places mewxi has to reach
//! outside the standard library — process enumeration and signalling.
//!
//! Everything here keeps the *same observable behaviour* on Windows as
//! on Unix; only the underlying mechanism differs (`ps`/`kill` vs
//! `tasklist`/`taskkill`). Keeping the platform split in one module
//! means the call sites read identically on every OS.

use std::collections::HashSet;
use std::process::Command;

/// Snapshot of every live process id on the box. Returns the empty set
/// when the platform tool is unusable — callers treat that as "can't
/// tell" (every marker is then considered stale). Mirrors the contract
/// the Unix `ps` path always had.
pub fn alive_pids() -> HashSet<u32> {
    #[cfg(windows)]
    {
        // `tasklist /NH /FO CSV` → `"image","PID","Session","#","Mem"`.
        // The PID is the second comma-separated, quote-wrapped field.
        Command::new("tasklist")
            .args(["/NH", "/FO", "CSV"])
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|line| line.split("\",\"").nth(1))
                    .filter_map(|s| s.trim_matches('"').trim().parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        Command::new("ps")
            .args(["-A", "-o", "pid="])
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Terminate the process `pid`. On Unix this is `kill` (SIGTERM); on
/// Windows it's `taskkill /F /T` (force the process tree, since console
/// children like `node` don't honour the graceful WM_CLOSE path). The
/// returned `ExitStatus` lets the caller report success/failure exactly
/// as the old inline `kill` invocation did.
pub fn terminate_pid(pid: u32) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
    }
    #[cfg(not(windows))]
    {
        Command::new("kill").arg(pid.to_string()).status()
    }
}
