//! One-shot configuration helper for the `setup` subcommand.
//!
//! Wires Claude Code's `statusLine` to `claude-usage status`, which reads
//! Claude Code's stdin payload per render and so can include per-session
//! context. Also seeds the disk cache (consumed by `watch`) and — when
//! `--service` is passed — installs a user-scope service unit (systemd on
//! Linux, launchd on macOS) that runs `claude-usage watch` at login.
//!
//! Idempotent: if `statusLine` already matches the desired value we
//! leave it alone. If it points somewhere else we refuse to overwrite
//! unless `--force` is given.

use crate::stats;
use crate::watch;
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(install_service: bool, force: bool, no_live: bool) -> Result<()> {
    let binary = std::env::current_exe().context("resolving current executable path")?;
    let cache = stats::status_cache_path()
        .ok_or_else(|| anyhow!("no home/cache dir — cannot determine status cache path"))?;
    let settings = claude_settings_path()
        .ok_or_else(|| anyhow!("no home dir — cannot locate ~/.claude/settings.json"))?;

    println!("claude-usage setup");
    println!("  binary:   {}", binary.display());
    println!("  cache:    {}", cache.display());
    println!("  settings: {}", settings.display());
    println!();

    update_settings_json(&settings, &binary, no_live, force)?;
    seed_status_cache(&cache, no_live)?;

    if install_service {
        install_service_unit(&binary, no_live)?;
    } else {
        println!();
        println!("To keep the status fresh, run `claude-usage watch` under a supervisor,");
        println!("or re-run `claude-usage setup --service` to install a user service unit.");
    }

    println!();
    println!("Done. Reload Claude Code (or start a new session) to see the status line.");
    Ok(())
}

fn claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Merge a `statusLine` block into `~/.claude/settings.json`, preserving
/// any other keys. Idempotent when the existing block already matches.
fn update_settings_json(path: &Path, binary: &Path, no_live: bool, force: bool) -> Result<()> {
    let desired_cmd = if no_live {
        format!("{} --no-live status", shell_quote(binary))
    } else {
        format!("{} status", shell_quote(binary))
    };
    let desired = serde_json::json!({
        "type": "command",
        "command": desired_cmd,
    });

    let mut root: serde_json::Value = if path.exists() {
        let s = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        if s.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&s).with_context(|| {
                format!("parsing {} — fix any JSON errors and re-run", path.display())
            })?
        }
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        serde_json::json!({})
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;

    match obj.get("statusLine") {
        Some(v) if v == &desired => {
            println!("  settings: statusLine already wired (no change)");
            return Ok(());
        }
        Some(v) if !force => {
            println!("  settings: statusLine already set to something else:");
            println!("    {}", serde_json::to_string(v).unwrap_or_default());
            println!("    re-run with --force to overwrite");
            return Ok(());
        }
        _ => {}
    }

    obj.insert("statusLine".to_string(), desired);
    let serialized = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write(path, serialized.as_bytes())?;
    println!("  settings: wrote statusLine block");
    Ok(())
}

fn seed_status_cache(cache: &Path, no_live: bool) -> Result<()> {
    if let Some(parent) = cache.parent() {
        fs::create_dir_all(parent).ok();
    }
    let line = watch::render_status(None, None, no_live);
    atomic_write(cache, line.as_bytes())?;
    println!("  cache:    seeded");
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_service_unit(binary: &Path, no_live: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    let unit_dir = home.join(".config/systemd/user");
    fs::create_dir_all(&unit_dir).context("creating ~/.config/systemd/user")?;
    let unit_path = unit_dir.join("claude-usage-watch.service");

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
    println!("  service:  wrote {}", unit_path.display());

    let enable = (|| -> Result<()> {
        run_cmd("systemctl", &["--user", "daemon-reload"])?;
        run_cmd(
            "systemctl",
            &["--user", "enable", "--now", "claude-usage-watch.service"],
        )?;
        Ok(())
    })();
    match enable {
        Ok(()) => println!("  service:  enabled & started claude-usage-watch.service"),
        Err(e) => {
            println!("  service:  could not auto-enable ({e})");
            println!("            run manually:");
            println!("              systemctl --user daemon-reload");
            println!("              systemctl --user enable --now claude-usage-watch.service");
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_service_unit(binary: &Path, no_live: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    let agents_dir = home.join("Library/LaunchAgents");
    fs::create_dir_all(&agents_dir).context("creating ~/Library/LaunchAgents")?;
    let plist_path = agents_dir.join("com.claude-usage.watch.plist");

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
    println!("  service:  wrote {}", plist_path.display());

    let plist_str = plist_path.to_string_lossy().into_owned();
    // Best-effort unload of any prior instance, then load.
    let _ = Command::new("launchctl").args(["unload", &plist_str]).output();
    match Command::new("launchctl")
        .args(["load", "-w", &plist_str])
        .output()
    {
        Ok(o) if o.status.success() => println!("  service:  loaded launchd agent"),
        Ok(o) => {
            println!(
                "  service:  launchctl load failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            println!("            run manually: launchctl load -w {plist_str}");
        }
        Err(e) => println!("  service:  launchctl not available ({e})"),
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn install_service_unit(_binary: &Path, _no_live: bool) -> Result<()> {
    println!("  service:  --service is not supported on this OS; run `claude-usage watch` yourself");
    Ok(())
}

/// Counterpart to `setup --service`: stop the watcher service. With `disable`,
/// also prevent it from starting on the next login.
pub fn stop(disable: bool) -> Result<()> {
    println!("claude-usage stop");
    stop_service_unit(disable)
}

#[cfg(target_os = "linux")]
fn stop_service_unit(disable: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    let unit_path = home.join(".config/systemd/user/claude-usage-watch.service");
    if !unit_path.exists() {
        println!("  service:  no unit at {} — nothing to stop", unit_path.display());
        return Ok(());
    }

    match run_cmd("systemctl", &["--user", "stop", "claude-usage-watch.service"]) {
        Ok(()) => println!("  service:  stopped claude-usage-watch.service"),
        Err(e) => println!("  service:  stop failed: {e}"),
    }
    if disable {
        match run_cmd("systemctl", &["--user", "disable", "claude-usage-watch.service"]) {
            Ok(()) => println!("  service:  disabled (will not restart on login)"),
            Err(e) => println!("  service:  disable failed: {e}"),
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn stop_service_unit(disable: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    let plist_path = home.join("Library/LaunchAgents/com.claude-usage.watch.plist");
    if !plist_path.exists() {
        println!("  service:  no agent at {} — nothing to stop", plist_path.display());
        return Ok(());
    }

    // `-w` persists the stop across logins (writes a disabled flag); without
    // it, stopping is transient and launchd reloads on next login.
    let mut args: Vec<&str> = vec!["unload"];
    if disable {
        args.push("-w");
    }
    let plist_str = plist_path.to_string_lossy().into_owned();
    args.push(&plist_str);
    match Command::new("launchctl").args(&args).output() {
        Ok(o) if o.status.success() => {
            if disable {
                println!("  service:  unloaded and disabled launchd agent");
            } else {
                println!("  service:  unloaded launchd agent");
            }
        }
        Ok(o) => println!(
            "  service:  launchctl unload failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!("  service:  launchctl not available ({e})"),
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn stop_service_unit(_disable: bool) -> Result<()> {
    println!("  service:  stop is not supported on this OS; kill your `claude-usage watch` process manually");
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

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

/// Atomic file write: write to `<path>.tmp`, fsync, rename into place.
/// Preserves the original filename (unlike `with_extension`, which would
/// turn `settings.json` into `settings.tmp`).
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

/// POSIX-safe shell quoting: leave paths made of safe characters bare,
/// otherwise wrap in single quotes with embedded `'` escaped via `'\''`.
fn shell_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.' | ':' | '+' | '=' | ',');
    if !s.is_empty() && s.chars().all(safe) {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
