//! Read the Claude Code OAuth access token from whichever store the user's
//! Claude Code install actually uses.
//!
//! Lookup order, first success wins:
//!
//! 1. `CLAUDE_USAGE_OAUTH_TOKEN` env var — universal escape hatch (CI,
//!    remote shells without keychain access, platforms we don't natively
//!    support).
//! 2. macOS keychain (Mac only):
//!    `security find-generic-password -s "Claude Code-credentials" -w`
//!    returns a JSON blob whose `claudeAiOauth.accessToken` is the Bearer.
//! 3. `~/.claude/.credentials.json` — the plaintext file (mode 0600) that
//!    Claude Code writes on Linux; same JSON shape as the keychain blob,
//!    so we reuse the parser. Also acts as a Mac fallback for sandboxed
//!    runs where `security` is unavailable.
//!
//! If every source fails, the aggregate error lists exactly what was tried
//! so users can act on it instead of guessing.
//!
//! Windows has no native-store integration yet; users there should rely on
//! the env var or drop the credentials file into `%USERPROFILE%\.claude\`.

use anyhow::{anyhow, Result};
use std::path::PathBuf;

pub fn read_oauth_token() -> Result<String> {
    let mut errs: Vec<String> = Vec::new();

    match std::env::var("CLAUDE_USAGE_OAUTH_TOKEN") {
        Ok(v) if !v.is_empty() => return Ok(v),
        Ok(_) => errs.push("CLAUDE_USAGE_OAUTH_TOKEN: set but empty".into()),
        Err(_) => errs.push("CLAUDE_USAGE_OAUTH_TOKEN: unset".into()),
    }

    #[cfg(target_os = "macos")]
    match read_token_macos_keychain() {
        Ok(t) => return Ok(t),
        Err(e) => errs.push(format!("macOS keychain: {e}")),
    }

    match read_token_credentials_file() {
        Ok(t) => return Ok(t),
        Err(e) => errs.push(format!("credentials file: {e}")),
    }

    Err(anyhow!(
        "could not read Claude Code OAuth token:\n  - {}\n\
         pass --no-live to silence this, or set CLAUDE_USAGE_OAUTH_TOKEN.",
        errs.join("\n  - ")
    ))
}

/// Parse `claudeAiOauth.accessToken` out of the JSON blob shared by the
/// keychain and the on-disk credentials file.
fn parse_token_from_json(raw: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow!("credential JSON parse failed: {e}"))?;
    v.get("claudeAiOauth")
        .and_then(|o| o.get("accessToken"))
        .and_then(|t| t.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("credential missing claudeAiOauth.accessToken"))
}

fn credentials_file_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    Ok(home.join(".claude").join(".credentials.json"))
}

fn read_token_credentials_file() -> Result<String> {
    let path = credentials_file_path()?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("{}: {e}", path.display()))?;
    parse_token_from_json(&raw)
}

#[cfg(target_os = "macos")]
fn read_token_macos_keychain() -> Result<String> {
    use std::process::Command;
    let out = Command::new("security")
        .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
        .output()
        .map_err(|e| anyhow!("failed to invoke `security`: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "`security` exited {}: {}",
            out.status,
            stderr.trim()
        ));
    }
    let raw = String::from_utf8(out.stdout)
        .map_err(|e| anyhow!("credential is not valid UTF-8: {e}"))?;
    parse_token_from_json(&raw)
}
