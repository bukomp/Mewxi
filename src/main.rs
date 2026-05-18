//! CLI entry point for muxi.
//!
//! Five subcommands, one binary:
//!
//! - `tui`    — interactive ratatui dashboard (see [`crate::tui`]).
//! - `status` — one-line statusLine string for Claude Code. Reads the
//!   JSON payload Claude Code writes to stdin to pick up the active
//!   session's transcript path and model alias.
//! - `watch`  — background daemon that keeps the status string cached
//!   on disk so the statusLine hook can `cat` it cheaply.
//! - `dump`   — JSON dump of the aggregate + live payload.
//! - `mcp`    — JSON-RPC MCP server over stdio.
//!
//! See `README.md` for the end-user view.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod accounts;
mod auth;
mod chat_log;
mod live_session;
mod live_usage;
mod mcp;
mod pricing;
mod setup;
mod stats;
mod tui;
mod watch;

#[derive(Parser)]
#[command(name = "muxi", version, about = "MCP server + TUI for Claude Code usage stats")]
struct Cli {
    /// Disable live usage fetch from Claude Code's OAuth endpoint. Local JSONL only.
    /// Also honored via the MUXI_NO_LIVE env var (any non-empty value).
    #[arg(long, global = true)]
    no_live: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run as an MCP server over stdio (wire this into Claude Code's MCP config).
    Mcp,
    /// Launch the interactive TUI with live-updating stats.
    Tui,
    /// Print the current aggregate as JSON and exit (useful for scripts / debugging).
    Dump,
    /// Print a one-line usage summary — designed for Claude Code's statusLine.
    Status,
    /// Run a background watcher that keeps the status cache hot as session files change.
    Watch,
    /// Wire Claude Code's statusLine into every discovered account and (optionally) install the watcher service. Same actions are available inside the TUI under view 4 (no CLI run required).
    Setup {
        /// Also install a user-scope service (systemd on Linux, launchd on macOS) to run the watcher at login.
        #[arg(long)]
        service: bool,
        /// Overwrite an existing non-muxi statusLine entry in each account's settings.json.
        #[arg(long)]
        force: bool,
    },
    /// Stop the watcher service (systemd user unit on Linux, launchd agent on macOS).
    Stop {
        /// Also disable the service so it does not start again on login.
        #[arg(long)]
        disable: bool,
    },
    /// Internal: hook handler invoked by Claude Code's settings.json hooks.
    /// Reads the hook payload JSON from stdin, extracts session_id, and
    /// touches or removes `<dir>/sessions/<session_id>.awaiting` so the
    /// TUI knows a permission dialog is up.
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
}

#[derive(Subcommand)]
enum HookAction {
    /// Mark the calling session as awaiting permission (creates the marker).
    AwaitingSet {
        /// `CLAUDE_CONFIG_DIR` of the account that installed the hook.
        #[arg(long)]
        dir: std::path::PathBuf,
    },
    /// Clear the awaiting-permission marker for the calling session.
    AwaitingClear {
        /// `CLAUDE_CONFIG_DIR` of the account that installed the hook.
        #[arg(long)]
        dir: std::path::PathBuf,
    },
}

/// Read Claude Code's statusLine JSON payload from stdin and extract
/// (transcript_path, model_alias). Stdin may be empty — returns (None, None).
fn read_status_payload_from_stdin() -> (Option<std::path::PathBuf>, Option<String>) {
    use std::io::Read;
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return (None, None);
    }
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return (None, None);
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(buf.trim()) else {
        return (None, None);
    };
    let transcript = v
        .get("transcript_path")
        .and_then(|x| x.as_str())
        .map(std::path::PathBuf::from);
    // Claude Code payload shape: {"model": {"id": "...", "display_name": "..."}}
    // "id" is typically the alias the user configured (may contain [1m]).
    let model_alias = v
        .get("model")
        .and_then(|m| m.get("id"))
        .and_then(|x| x.as_str())
        .map(String::from);
    (transcript, model_alias)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let no_live = cli.no_live || std::env::var_os("MUXI_NO_LIVE").is_some_and(|v| !v.is_empty());
    match cli.cmd {
        Cmd::Tui => tui::run(no_live),
        Cmd::Dump => {
            let view = accounts::load_accounts()?;
            let mut out_accounts = Vec::with_capacity(view.accounts.len());
            let alive = live_session::alive_pids();
            for account in &view.accounts {
                let agg = stats::load_and_aggregate_for(account).unwrap_or_default();
                let live = live_usage::fetch_or_cached(account, no_live);
                let live_sessions = live_session::scan(account, &alive, &[]);
                out_accounts.push(serde_json::json!({
                    "name": account.name,
                    "dir": account.dir,
                    "aggregate": agg,
                    "live": live,
                    "live_sessions": live_sessions,
                }));
            }
            let out = serde_json::json!({
                "generated_at": chrono::Utc::now(),
                "default_account": view.default_account,
                "accounts": out_accounts,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
        Cmd::Status => {
            // Claude Code writes a JSON payload to stdin containing the active session's
            // transcript path and model. We use both to render context with the right cap.
            let (transcript, model_alias) = read_status_payload_from_stdin();
            let line = watch::render_status(transcript.as_deref(), model_alias.as_deref(), no_live);
            print!("{line}");
            Ok(())
        }
        Cmd::Watch => watch::run_forever(no_live),
        Cmd::Setup { service, force } => setup::run(service, force, no_live),
        Cmd::Stop { disable } => setup::stop(disable),
        Cmd::Mcp => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(mcp::run(no_live))
        }
        Cmd::Hook { action } => run_hook(action),
    }
}

/// Read Claude Code's hook payload from stdin and pull `session_id`.
/// The payload shape is `{"session_id": "<uuid>", ...}` — every hook
/// event includes it. Best-effort: returns None if stdin is empty,
/// not JSON, or missing the field.
fn read_session_id_from_stdin() -> Option<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    let v: serde_json::Value = serde_json::from_str(buf.trim()).ok()?;
    v.get("session_id").and_then(|s| s.as_str()).map(String::from)
}

fn run_hook(action: HookAction) -> Result<()> {
    // Hooks must never fail Claude Code — silently no-op on bad input.
    // Claude Code waits for the hook to exit; we want it back fast.
    let (dir, do_set) = match action {
        HookAction::AwaitingSet { dir } => (dir, true),
        HookAction::AwaitingClear { dir } => (dir, false),
    };
    let Some(session_id) = read_session_id_from_stdin() else {
        return Ok(());
    };
    let marker = dir.join("sessions").join(format!("{session_id}.awaiting"));
    if do_set {
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::File::create(&marker);
    } else {
        let _ = std::fs::remove_file(&marker);
    }
    Ok(())
}
