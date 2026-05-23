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
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::accounts::Account;

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
}

const PTY_RING_BYTES: usize = 64 * 1024;

impl PtySession {
    /// Spawn `claude` under a fresh PTY for the given account, with
    /// `cwd` as the child's working directory. The child renders its
    /// TUI to the PTY but mewxi never displays those bytes.
    pub fn spawn(account: &Account, cwd: PathBuf, claude_bin: PathBuf) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow!("openpty: {e}"))?;

        let mut cmd = CommandBuilder::new(&claude_bin);
        cmd.cwd(&cwd);
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
        {
            let ring = Arc::clone(&ring);
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut r = ring.lock().expect("ring poisoned");
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

        Ok(Self { writer, child, ring })
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
        self.ring.lock().expect("ring poisoned").clone()
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
    let Some(home) = std::env::var_os("HOME") else { return false };
    PathBuf::from(home).join(".claude") == dir
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
    PathBuf::from("claude")
}
