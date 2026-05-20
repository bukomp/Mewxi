//! Read the Claude Code OAuth access token for a given account.
//!
//! Resolution order:
//!
//! 1. `MEWXI_OAUTH_TOKEN` env var — universal escape hatch (CI,
//!    remote shells, single-account scripts predating multi-account
//!    support).
//! 2. The account's [`TokenSource`][crate::accounts::TokenSource], one of:
//!    - `Env(name)` — read `$name`.
//!    - `Keychain(service)` — `security find-generic-password -s <service> -w`
//!      and parse `claudeAiOauth.accessToken` out of the JSON blob.
//!    - `File(path)` — same JSON parser, read from disk.
//!
//! If every source fails, the aggregate error lists exactly what was
//! tried so users can act on it instead of guessing.

use crate::accounts::{self, Account, TokenSource};
use anyhow::{anyhow, Result};
use std::path::Path;

pub fn read_oauth_token(account: &Account) -> Result<String> {
    let mut errs: Vec<String> = Vec::new();

    match std::env::var("MEWXI_OAUTH_TOKEN") {
        Ok(v) if !v.is_empty() => return Ok(v),
        Ok(_) => errs.push("MEWXI_OAUTH_TOKEN: set but empty".into()),
        Err(_) => errs.push("MEWXI_OAUTH_TOKEN: unset".into()),
    }

    match &account.token_source {
        TokenSource::Env { env } => match std::env::var(env) {
            Ok(v) if !v.is_empty() => return Ok(v),
            Ok(_) => errs.push(format!("env {env}: set but empty")),
            Err(_) => errs.push(format!("env {env}: unset")),
        },
        TokenSource::Keychain { keychain } => {
            #[cfg(target_os = "macos")]
            match read_token_macos_keychain(keychain) {
                Ok(t) => return Ok(t),
                Err(e) => errs.push(format!("keychain {keychain}: {e}")),
            }
            #[cfg(not(target_os = "macos"))]
            errs.push(format!(
                "keychain {keychain}: macOS keychain not available on this platform"
            ));
        }
        TokenSource::File { file } => match read_token_credentials_file(file) {
            Ok(t) => return Ok(t),
            Err(e) => errs.push(format!("file {}: {e}", file.display())),
        },
        TokenSource::Auto => {
            // Mirror Claude Code's per-CLAUDE_CONFIG_DIR keychain layout:
            // try the hashed service name (Claude Code-credentials-{8hex}),
            // then the legacy single-account "Claude Code-credentials"
            // entry, then the on-disk credentials file. This is what makes
            // multi-account discovery "just work" without any user config.
            #[cfg(target_os = "macos")]
            {
                let canonical = std::fs::canonicalize(&account.dir).unwrap_or_else(|_| account.dir.clone());
                let hashed = accounts::hashed_keychain_service(&canonical);
                match read_token_macos_keychain(&hashed) {
                    Ok(t) => return Ok(t),
                    Err(e) => errs.push(format!("keychain {hashed}: {e}")),
                }
                match read_token_macos_keychain("Claude Code-credentials") {
                    Ok(t) => return Ok(t),
                    Err(e) => errs.push(format!("keychain Claude Code-credentials: {e}")),
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = accounts::hashed_keychain_service; // suppress unused
            }
            let creds = account.dir.join(".credentials.json");
            match read_token_credentials_file(&creds) {
                Ok(t) => return Ok(t),
                Err(e) => errs.push(format!("file {}: {e}", creds.display())),
            }
        }
    }

    Err(anyhow!(
        "could not read Claude Code OAuth token for account '{}':\n  - {}\n\
         pass --no-live to silence this, or set MEWXI_OAUTH_TOKEN.",
        account.name,
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

fn read_token_credentials_file(path: &Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("{}: {e}", path.display()))?;
    parse_token_from_json(&raw)
}

#[cfg(target_os = "macos")]
fn read_token_macos_keychain(service: &str) -> Result<String> {
    use std::process::Command;
    let out = Command::new("security")
        .args(["find-generic-password", "-s", service, "-w"])
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
