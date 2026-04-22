//! Read the Claude Code OAuth access token from the OS credential store.
//!
//! macOS: `security find-generic-password -s "Claude Code-credentials" -w`
//! returns a JSON blob whose `claudeAiOauth.accessToken` is the Bearer token
//! used by Claude Code to call api.anthropic.com/api/oauth/usage.
//!
//! Linux/Windows keychains are not wired up yet; callers get a clear error.

use anyhow::{anyhow, Result};

pub fn read_oauth_token() -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        read_token_macos()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(anyhow!(
            "live usage fetch is only implemented on macOS (credential store access); \
             pass --no-live to silence this"
        ))
    }
}

#[cfg(target_os = "macos")]
fn read_token_macos() -> Result<String> {
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
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| anyhow!("credential JSON parse failed: {e}"))?;
    v.get("claudeAiOauth")
        .and_then(|o| o.get("accessToken"))
        .and_then(|t| t.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("credential missing claudeAiOauth.accessToken"))
}
