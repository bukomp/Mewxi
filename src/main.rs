//! CLI entry point for mewxi.
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
mod agent_control;
mod auth;
mod chat_log;
mod debug_log;
mod live_session;
mod live_usage;
mod mcp;
mod platform;
mod pricing;
mod setup;
mod skills;
mod stats;
mod tui;
mod update;
mod watch;

#[derive(Parser)]
#[command(name = "mewxi", version, about = "MCP server + TUI for coding-agent usage stats")]
struct Cli {
    /// Disable live usage fetch from Claude Code's OAuth endpoint. Local JSONL only.
    /// Also honored via the MEWXI_NO_LIVE env var (any non-empty value).
    #[arg(long, global = true)]
    no_live: bool,

    /// Subcommand. Omit to launch the interactive TUI (default).
    #[command(subcommand)]
    cmd: Option<Cmd>,
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
        /// Also install a user-scope service (systemd on Linux, launchd on macOS, Scheduled Task on Windows) to run the watcher at login.
        #[arg(long)]
        service: bool,
        /// Overwrite an existing non-mewxi statusLine entry in each account's settings.json.
        #[arg(long)]
        force: bool,
    },
    /// Check for a newer mewxi and rebuild from the source checkout.
    /// Channel comes from `update_channel` in accounts.toml (release
    /// tags by default, `dev` to follow main) — also editable in the
    /// TUI's Config view.
    Update {
        /// Only check and report; don't rebuild.
        #[arg(long)]
        check: bool,
    },
    /// Stop the watcher service (systemd user unit on Linux, launchd agent on macOS, Scheduled Task on Windows).
    Stop {
        /// Also disable the service so it does not start again on login.
        #[arg(long)]
        disable: bool,
    },
    /// Spawn an interactive `claude` session that mewxi owns (PTY-backed),
    /// type the given prompt into it, wait for the response, then print
    /// the resulting transcript. End-to-end smoke test for the agent-
    /// control plumbing before TUI integration.
    Drive {
        /// Account name as shown in `mewxi dump` (`claude`, `claude-priv`, …).
        /// Defaults to the configured default account.
        #[arg(long)]
        account: Option<String>,
        /// Working directory for the spawned session. Defaults to $PWD.
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
        /// Prompt text to type into the session.
        #[arg(long)]
        prompt: String,
        /// How long to wait after sending the prompt before reading the
        /// transcript and killing the child.
        #[arg(long, default_value_t = 30)]
        listen_secs: u64,
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

/// Owned form of the fields we pull from Claude Code's statusLine JSON
/// payload. Borrowed into a [`watch::SessionMeta`] at the call site.
#[derive(Default)]
struct StatusPayload {
    transcript: Option<std::path::PathBuf>,
    model_alias: Option<String>,
    model_display: Option<String>,
    thinking_enabled: bool,
    effort_level: Option<String>,
}

/// Read Claude Code's statusLine JSON payload from stdin. Stdin may be
/// empty (e.g. invoked from a terminal) — returns a default payload.
fn read_status_payload_from_stdin() -> StatusPayload {
    use std::io::Read;
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return StatusPayload::default();
    }
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return StatusPayload::default();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(buf.trim()) else {
        return StatusPayload::default();
    };
    let str_at = |path: &[&str]| -> Option<String> {
        let mut cur = &v;
        for key in path {
            cur = cur.get(key)?;
        }
        cur.as_str().map(String::from)
    };
    StatusPayload {
        transcript: str_at(&["transcript_path"]).map(std::path::PathBuf::from),
        // "model.id" is typically the alias the user configured (may
        // contain [1m]); "model.display_name" is the short label, e.g. "Opus".
        model_alias: str_at(&["model", "id"]),
        model_display: str_at(&["model", "display_name"]),
        // "thinking.enabled" (bool) + "effort.level" (low|medium|high|xhigh|max).
        thinking_enabled: v
            .get("thinking")
            .and_then(|t| t.get("enabled"))
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        effort_level: str_at(&["effort", "level"]),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let no_live = cli.no_live || std::env::var_os("MEWXI_NO_LIVE").is_some_and(|v| !v.is_empty());
    match cli.cmd.unwrap_or(Cmd::Tui) {
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
            let p = read_status_payload_from_stdin();
            let meta = watch::SessionMeta {
                model_alias: p.model_alias.as_deref(),
                model_display: p.model_display.as_deref(),
                thinking_enabled: p.thinking_enabled,
                effort_level: p.effort_level.as_deref(),
            };
            let line = watch::render_status(p.transcript.as_deref(), meta, no_live);
            print!("{line}");
            Ok(())
        }
        Cmd::Watch => watch::run_forever(no_live),
        Cmd::Update { check } => {
            let status = update::check_now()?;
            println!("mewxi update check");
            println!("  channel: {}", status.channel.label());
            println!("  current: {}", status.current);
            println!("  latest:  {} ({})", status.latest, status.detail);
            if !status.available {
                println!("  already up to date");
                return Ok(());
            }
            if check {
                println!("  update available — run `mewxi update` to install");
                return Ok(());
            }
            println!();
            let msg = update::apply_now()?;
            println!("{msg}");
            Ok(())
        }
        Cmd::Setup { service, force } => setup::run(service, force, no_live),
        Cmd::Stop { disable } => setup::stop(disable),
        Cmd::Mcp => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(mcp::run(no_live))
        }
        Cmd::Hook { action } => run_hook(action),
        Cmd::Drive {
            account,
            cwd,
            prompt,
            listen_secs,
        } => run_drive(account, cwd, prompt, listen_secs),
    }
}

fn run_drive(
    account_name: Option<String>,
    cwd_arg: Option<std::path::PathBuf>,
    prompt: String,
    listen_secs: u64,
) -> Result<()> {
    use std::time::{Duration, Instant};

    let view = accounts::load_accounts()?;
    let account = view
        .pick(account_name.as_deref())
        .ok_or_else(|| anyhow::anyhow!("no matching account; check `mewxi dump`"))?;

    let cwd = match cwd_arg {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let claude_bin = agent_control::resolve_claude_bin(account);

    // Snapshot the live-session set BEFORE spawning so we can identify
    // the new session marker by diffing it.
    let alive_before = live_session::alive_pids();
    let before: std::collections::HashSet<String> =
        live_session::scan(account, &alive_before, &[])
            .into_iter()
            .map(|s| s.session_id)
            .collect();

    eprintln!(
        "→ spawning `{}` under PTY (account={}, cwd={})",
        claude_bin.display(),
        account.name,
        cwd.display()
    );
    let mut session = agent_control::PtySession::spawn(account, cwd.clone(), claude_bin, None)?;

    // Give the TUI time to render its welcome screen and arm the
    // input box. 1.5s is enough on this machine; if the child exits
    // early we bail out with whatever it wrote.
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(50));
        if let Some(code) = session.try_wait()? {
            return Err(anyhow::anyhow!(
                "claude exited before prompt was sent ({}). Tail of pty:\n{}",
                code,
                String::from_utf8_lossy(&session.ring_snapshot())
            ));
        }
    }

    eprintln!("→ typing prompt: {prompt}");
    let mut keys = prompt.into_bytes();
    keys.push(b'\r');
    session.send_keys(&keys)?;

    // Locate the new session marker (and from it, the JSONL transcript).
    eprintln!("→ waiting for new session marker …");
    let deadline = Instant::now() + Duration::from_secs(listen_secs);
    let mut new_session_id: Option<String> = None;
    while Instant::now() < deadline {
        let alive = live_session::alive_pids();
        for s in live_session::scan(account, &alive, &[]) {
            if !before.contains(&s.session_id) {
                eprintln!("  session_id={} transcript={}", s.session_id, s.transcript_path.display());
                new_session_id = Some(s.session_id);
                break;
            }
        }
        if new_session_id.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let Some(_sid) = new_session_id.clone() else {
        let _ = session.kill();
        return Err(anyhow::anyhow!(
            "no new session marker appeared under {} within {}s. PTY tail:\n{}",
            account.dir.join("sessions").display(),
            listen_secs,
            String::from_utf8_lossy(&session.ring_snapshot())
        ));
    };

    // Hold the session alive while it works through the prompt.
    eprintln!("→ holding session alive until deadline ({listen_secs}s) …");
    while Instant::now() < deadline {
        if session.try_wait()?.is_some() {
            eprintln!("  child exited early");
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Re-scan once to refresh the transcript path; the marker is the
    // canonical pointer to the JSONL.
    let alive = live_session::alive_pids();
    let transcript = live_session::scan(account, &alive, &[])
        .into_iter()
        .find(|s| Some(&s.session_id) == new_session_id.as_ref())
        .map(|s| s.transcript_path);

    // Tear down the child before reading — claude flushes some records
    // on exit (TUI shutdown writes a final "system" line).
    let _ = session.kill();

    if let Some(path) = transcript {
        eprintln!("→ transcript at {}", path.display());
        for entry in chat_log::read(&path) {
            let tag = match entry.kind {
                chat_log::EntryKind::User => "user",
                chat_log::EntryKind::Assistant => "assistant",
                chat_log::EntryKind::Thinking => "thinking",
                chat_log::EntryKind::ToolUse { .. } => "tool_use",
                chat_log::EntryKind::ToolResult { .. } => "tool_result",
                chat_log::EntryKind::System => "system",
            };
            println!("[{tag}] {}", entry.text);
        }
    } else {
        eprintln!("(no transcript found; child wrote nothing)");
    }
    Ok(())
}

/// Read Claude Code's hook payload from stdin and pull `session_id`
/// plus the current `permission_mode`. The payload shape is
/// `{"session_id": "<uuid>", "permission_mode": "default", ...}` —
/// every hook event includes both (older Claude Code spelled the mode
/// `permissionMode`, accept either). Best-effort: returns None if
/// stdin is empty, not JSON, or missing session_id.
fn read_hook_payload_from_stdin() -> Option<(String, Option<String>)> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    let v: serde_json::Value = serde_json::from_str(buf.trim()).ok()?;
    let session_id = v.get("session_id").and_then(|s| s.as_str())?.to_string();
    let mode = v
        .get("permission_mode")
        .or_else(|| v.get("permissionMode"))
        .and_then(|s| s.as_str())
        .map(String::from);
    Some((session_id, mode))
}

fn run_hook(action: HookAction) -> Result<()> {
    // Hooks must never fail Claude Code — silently no-op on bad input.
    // Claude Code waits for the hook to exit; we want it back fast.
    let (dir, do_set) = match action {
        HookAction::AwaitingSet { dir } => (dir, true),
        HookAction::AwaitingClear { dir } => (dir, false),
    };
    let Some((session_id, mode)) = read_hook_payload_from_stdin() else {
        return Ok(());
    };
    let sessions = dir.join("sessions");
    let marker = sessions.join(format!("{session_id}.awaiting"));
    if do_set {
        let _ = std::fs::create_dir_all(&sessions);
        let _ = std::fs::File::create(&marker);
    } else {
        let _ = std::fs::remove_file(&marker);
    }
    // Persist the live permission mode alongside the marker. The
    // transcript only records the mode on typed user prompts (Claude
    // Code ≥2.1.x no longer writes dedicated `permission-mode`
    // records), so Shift-Tab cycles inside a session would otherwise
    // be invisible until the next prompt. Hooks fire on every tool
    // use and turn end and always carry the current mode — the
    // freshest signal available. scan() prefers this file over the
    // transcript tail.
    if let Some(mode) = mode {
        let _ = std::fs::create_dir_all(&sessions);
        let _ = std::fs::write(sessions.join(format!("{session_id}.mode")), mode);
    }
    Ok(())
}
