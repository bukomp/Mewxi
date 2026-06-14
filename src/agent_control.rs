//! Drive a real interactive Claude Code session that mewxi spawns and
//! owns the PTY for.
//!
//! Spawning happens via [`portable-pty`]: mewxi allocates a pseudo-
//! terminal, starts `claude` (the regular interactive entrypoint, not
//! `--print`) attached to it, and keeps both ends in-process. Keystrokes
//! sent by the user inside mewxi are written to the PTY master; the
//! TUI bytes the child writes back are drained into a ring buffer so
//! the child never blocks, but mewxi normally does not render them —
//! we render mewxi's own UI off the JSONL transcript the child writes,
//! exactly like every other session in the dashboard.
//!
//! This only drives sessions mewxi spawned. Pre-existing interactive
//! `claude` instances running in another terminal cannot be attached;
//! Unix offers no native primitive for cross-process stdin injection.

use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::accounts::Account;

pub const PTY_ROWS: u16 = 40;
// 160 (not 120) so claude has room for the AskUserQuestion side-by-side
// preview layout — at 120 the option list and preview pane fight for
// width and the modal wraps awkwardly. mewxi's terminal_overlay re-parses
// pickers natively, so the wider PTY is purely about giving claude
// breathing room to lay out cleanly before we re-render.
pub const PTY_COLS: u16 = 160;

/// A running interactive `claude` child whose PTY mewxi owns.
pub struct PtySession {
    /// Writable half of the PTY master — keystrokes go here.
    writer: Box<dyn Write + Send>,
    /// The child process handle. Held so the child stays alive for the
    /// lifetime of the `PtySession` and can be waited on / killed.
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Last N bytes the child wrote. Useful for "did anything come back
    /// yet?" checks and ANSI-stripped error reports. Bounded; the drain
    /// thread trims to [`PTY_RING_BYTES`].
    ring: Arc<Mutex<Vec<u8>>>,
    /// vt100 parser fed in parallel with the ring. Maintains the
    /// authoritative PTY_ROWS×PTY_COLS screen grid so the TUI can render
    /// claude's terminal overlays (prompts, pickers) when needed.
    parser: Arc<Mutex<vt100::Parser>>,
}

const PTY_RING_BYTES: usize = 64 * 1024;

impl PtySession {
    /// Spawn `claude` under a fresh PTY for the given account, with
    /// `cwd` as the child's working directory. The child renders its
    /// TUI to the PTY but mewxi never displays those bytes.
    pub fn spawn(
        account: &Account,
        cwd: PathBuf,
        claude_bin: PathBuf,
        resume_session_id: Option<String>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: PTY_ROWS,
                cols: PTY_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("openpty: {e}"))?;

        let mut cmd = build_claude_command(&claude_bin);
        cmd.cwd(&cwd);
        // Honour the account's auto-mode opt-in by starting claude in
        // auto. Claude itself always launches in `default` regardless of
        // `skipAutoPermissionPrompt`; that flag only suppresses the
        // confirmation prompt the first time the user toggles into
        // auto. Without `--permission-mode auto` here, mewxi's badge
        // (which uses the opt-in as a pre-transcript fallback) would
        // briefly show `auto` and then snap back to `manual` once
        // claude's first `permission-mode` record lands — and the
        // Shift-Tab cycle would start from the wrong baseline.
        if account.default_permission_mode() == "auto" {
            cmd.arg("--permission-mode");
            cmd.arg("auto");
        }
        // Resuming an existing session: claude expects `--resume <id>`
        // where <id> is the JSONL file stem from
        // `<config_dir>/projects/<encoded>/<id>.jsonl`. The child reads
        // the transcript and continues; the in-process session id may
        // rotate but the mewxi driver follows it via the marker file.
        if let Some(id) = resume_session_id.as_deref() {
            cmd.arg("--resume");
            cmd.arg(id);
        }
        // Only override CLAUDE_CONFIG_DIR for non-default accounts. For
        // the default `~/.claude`, claude already discovers its own
        // config dir, and setting the env forces it to look for
        // `.claude.json` *inside* `~/.claude` — but the user's real
        // auth/theme config lives at `$HOME/.claude.json`. Setting the
        // env on the default account would surface the empty stub at
        // `~/.claude/.claude.json` and trigger the first-run welcome
        // flow (theme picker + login method), which never resolves
        // because mewxi hides the PTY.
        if !is_default_claude_dir(&account.dir) {
            cmd.env("CLAUDE_CONFIG_DIR", &account.dir);
        }
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow!("spawn {}: {e}", claude_bin.display()))?;
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("take_writer: {e}"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("try_clone_reader: {e}"))?;

        let ring: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::with_capacity(PTY_RING_BYTES)));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(PTY_ROWS, PTY_COLS, 0)));
        {
            let ring = Arc::clone(&ring);
            let parser = Arc::clone(&parser);
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            // Recover from a poisoned lock — vt100 has
                            // its own assert paths and we'd rather keep
                            // serving stale screen state than crash the
                            // whole TUI (which would leave the terminal
                            // in raw mode).
                            parser
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .process(&buf[..n]);
                            let mut r = ring.lock().unwrap_or_else(|e| e.into_inner());
                            r.extend_from_slice(&buf[..n]);
                            if r.len() > PTY_RING_BYTES {
                                let cut = r.len() - PTY_RING_BYTES;
                                r.drain(..cut);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        Ok(Self {
            writer,
            child,
            ring,
            parser,
        })
    }

    /// Write raw bytes to the PTY master. Used for keystrokes (a final
    /// `\r` submits an input box).
    pub fn send_keys(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .map_err(|e| anyhow!("write to pty: {e}"))?;
        self.writer.flush().ok();
        Ok(())
    }

    /// Snapshot of the child's recent output (raw, ANSI-laden).
    pub fn ring_snapshot(&self) -> Vec<u8> {
        self.ring.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Snapshot of the vt100 screen state. Cloning is cheap (flat cell
    /// array + a few counters) so callers can take a snapshot per render
    /// tick without holding the parser lock across the render.
    pub fn screen_snapshot(&self) -> vt100::Screen {
        self.parser
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .screen()
            .clone()
    }

    /// Interrupt claude's in-flight execution by sending a bare ESC
    /// byte to the PTY. Claude's input handler treats `0x1b` as the
    /// cancel signal — same behaviour the user would get by pressing
    /// Esc inside a standalone `claude` session.
    pub fn cancel_execution(&mut self) -> Result<()> {
        self.send_keys(b"\x1b")
    }

    /// Forward a crossterm KeyEvent to the PTY as the byte sequence
    /// claude expects. Centralises key→bytes conversion so the overlay
    /// passthrough and the existing driver-input path share one
    /// implementation.
    pub fn send_key_event(&mut self, key: KeyEvent) -> Result<()> {
        let bytes = key_event_to_bytes(key);
        if bytes.is_empty() {
            return Ok(());
        }
        self.send_keys(&bytes)
    }

    /// OS process id of the spawned claude child, when available. Used
    /// by the TUI to detect when claude rotates its `sessionId` on the
    /// same process (e.g. after `/clear`, `/compact`) and re-pin the
    /// driver to the new id without re-spawning.
    pub fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Has the child exited?
    pub fn try_wait(&mut self) -> Result<Option<i32>> {
        match self.child.try_wait() {
            Ok(None) => Ok(None),
            Ok(Some(status)) => Ok(Some(status.exit_code() as i32)),
            Err(e) => Err(anyhow!("try_wait: {e}")),
        }
    }

    /// Send SIGHUP / TerminateProcess and wait briefly for the child to
    /// exit. Idempotent.
    pub fn kill(&mut self) -> Result<()> {
        let _ = self.child.kill();
        for _ in 0..20 {
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// True when `dir` is the user's default Claude config directory
/// (`$HOME/.claude`). Used to decide whether to forward
/// `CLAUDE_CONFIG_DIR` to the child — see [`PtySession::spawn`].
fn is_default_claude_dir(dir: &std::path::Path) -> bool {
    let Some(home) = dirs::home_dir() else { return false };
    home.join(".claude") == dir
}

/// Convert a crossterm `KeyEvent` to the byte sequence claude (and most
/// xterm-like terminals) expect on stdin. Returns an empty Vec for keys
/// that have no meaningful PTY representation (function keys we don't
/// map, modifier-only events, etc.).
pub fn key_event_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let mut out: Vec<u8> = Vec::with_capacity(8);
    if alt {
        out.push(0x1b);
    }
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let upper = c.to_ascii_uppercase() as u32;
                if (b'@' as u32..=b'_' as u32).contains(&upper) {
                    out.push((upper - b'@' as u32) as u8);
                } else if c == ' ' {
                    out.push(0);
                } else if c == '?' {
                    out.push(0x7f);
                } else {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Up => out.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => out.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => out.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => out.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        _ => {
            if alt {
                out.pop();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ck(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn plain_char_is_utf8() {
        assert_eq!(key_event_to_bytes(k(KeyCode::Char('y'))), b"y".to_vec());
    }

    #[test]
    fn enter_is_carriage_return() {
        assert_eq!(key_event_to_bytes(k(KeyCode::Enter)), b"\r".to_vec());
    }

    #[test]
    fn back_tab_is_csi_z() {
        assert_eq!(
            key_event_to_bytes(k(KeyCode::BackTab)),
            b"\x1b[Z".to_vec()
        );
    }

    #[test]
    fn arrows_are_csi_letters() {
        assert_eq!(key_event_to_bytes(k(KeyCode::Up)), b"\x1b[A".to_vec());
        assert_eq!(key_event_to_bytes(k(KeyCode::Down)), b"\x1b[B".to_vec());
        assert_eq!(key_event_to_bytes(k(KeyCode::Right)), b"\x1b[C".to_vec());
        assert_eq!(key_event_to_bytes(k(KeyCode::Left)), b"\x1b[D".to_vec());
    }

    #[test]
    fn ctrl_letter_is_control_byte() {
        assert_eq!(
            key_event_to_bytes(ck(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            vec![0x01]
        );
        // Ctrl-] = 0x1d. No longer reserved by mewxi (overlay dismiss
        // is F10 now); kept here to lock in the byte-encoding contract.
        assert_eq!(
            key_event_to_bytes(ck(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            vec![0x1d]
        );
    }

    #[test]
    fn backspace_is_del_byte() {
        assert_eq!(key_event_to_bytes(k(KeyCode::Backspace)), vec![0x7f]);
    }
}

/// Resolve the `claude` binary to spawn. Honours `MEWXI_CLAUDE_BIN`
/// override, then the per-account directory's sibling launcher (e.g.
/// `claude-priv` for `~/.claude-priv`), then plain `claude` on PATH.
pub fn resolve_claude_bin(_account: &Account) -> PathBuf {
    if let Ok(p) = std::env::var("MEWXI_CLAUDE_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // Heuristic: account dir `~/.claude-foo` often pairs with a shell
    // alias `claude-foo`. We set `CLAUDE_CONFIG_DIR` directly so any
    // generic `claude` binary on PATH works — no need to hunt for the
    // alias.
    #[cfg(windows)]
    {
        // npm's installer drops a `claude.cmd` shim (no `.exe`), which
        // `CreateProcess` won't discover from the bare name. Resolve the
        // real file off PATH; `spawn` wraps `.cmd`/`.bat` in `cmd /c`.
        if let Some(found) = find_on_path("claude", &["exe", "cmd", "bat"]) {
            return found;
        }
    }
    PathBuf::from("claude")
}

/// Search `PATH` for `<stem>.<ext>` over the given extensions, in order.
/// Returns the first existing file. Windows-only — Unix resolves bare
/// names through the usual `execvp` lookup.
#[cfg(windows)]
fn find_on_path(stem: &str, exts: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for ext in exts {
            let cand = dir.join(format!("{stem}.{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Build the `CommandBuilder` that launches `claude`. On Windows a
/// `.cmd`/`.bat` shim (what npm installs) can't be executed directly by
/// `CreateProcess`, so it's run through `cmd /c`; a real `.exe` and every
/// Unix binary launch directly.
fn build_claude_command(bin: &std::path::Path) -> CommandBuilder {
    #[cfg(windows)]
    {
        let is_batch = bin
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
            .unwrap_or(false);
        if is_batch {
            let mut c = CommandBuilder::new("cmd");
            c.arg("/c");
            c.arg(bin);
            return c;
        }
    }
    CommandBuilder::new(bin)
}
