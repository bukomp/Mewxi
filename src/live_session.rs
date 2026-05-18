//! Detect currently-open Claude Code instances per account.
//!
//! Claude Code writes a marker at `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json`
//! for every running interactive instance:
//!
//! ```json
//! {"pid": 56869,
//!  "sessionId": "f2323e13-...",
//!  "cwd": "/Users/.../claude-usage",
//!  "status": "busy" | "idle",
//!  ...}
//! ```
//!
//! That marker is the ground truth for "is there an open Claude Code
//! window with this session." Using it solves two problems the previous
//! mtime-based detection had:
//!
//! - **Subagent / one-shot transcripts no longer pollute the list.** A
//!   subagent JSONL lives at `<project>/<sessionId>/subagents/agent-*.jsonl`
//!   — i.e. one level deeper than the canonical `<project>/<sessionId>.jsonl`.
//!   We only consider the canonical path derived from the marker, so
//!   the subagent rows are filtered out for free.
//! - **The row never disappears during a thinking turn.** The marker
//!   stays put as long as the process is alive; we no longer race file
//!   mtime against pauses in the JSONL append stream.
//!
//! State assignment: `marker.status == "busy"` → [`SessionState::Active`],
//! anything else → [`SessionState::Idle`].

use crate::accounts::Account;
use crate::stats::{self, SessionContext, UsageTotals};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Active,
    Idle,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveSession {
    pub account_name: String,
    pub session_id: String,
    pub project: String,
    pub transcript_path: PathBuf,
    pub last_activity: DateTime<Utc>,
    /// When the session entered its current [`SessionState`]. Carries
    /// forward across scans while the state is unchanged so the TUI's
    /// "age" column shows time-in-current-state rather than time-since-
    /// last-record. On a transition (same session seen previously with a
    /// different state) it snaps to `Utc::now()` — the moment of the
    /// observed flip — rather than `last_activity`, because the marker
    /// can flip before the new turn's first JSONL record hits disk. For
    /// a never-before-seen session, falls back to `last_activity` so the
    /// first frame reflects real elapsed time.
    pub state_since: DateTime<Utc>,
    pub model: String,
    pub session_tokens: UsageTotals,
    pub current_context: Option<u64>,
    pub context_cap: Option<u64>,
    pub state: SessionState,
}

#[derive(Clone, Debug)]
struct SessionMarker {
    #[allow(dead_code)]
    pid: u32,
    session_id: String,
    cwd: PathBuf,
    status: String,
}

/// Snapshot the currently-alive PIDs once per scan wave so we don't
/// shell out per marker. Returns the empty set if `ps` is unusable —
/// callers that get an empty set should treat every marker as stale.
pub fn alive_pids() -> HashSet<u32> {
    use std::process::Command;
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

/// Reverse of Claude Code's project-dir flattening: every `/` in the
/// process cwd becomes `-`. Leading slash → leading dash.
fn cwd_to_project_dir(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
}

fn read_markers(account: &Account, alive: &HashSet<u32>) -> Vec<SessionMarker> {
    let dir = account.dir.join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(v): serde_json::Result<serde_json::Value> = serde_json::from_str(&raw) else {
            continue;
        };
        let pid = v.get("pid").and_then(|p| p.as_u64()).map(|n| n as u32);
        let session_id = v.get("sessionId").and_then(|s| s.as_str()).map(String::from);
        let cwd = v.get("cwd").and_then(|s| s.as_str()).map(PathBuf::from);
        let status = v
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("idle")
            .to_string();
        let (Some(pid), Some(session_id), Some(cwd)) = (pid, session_id, cwd) else {
            continue;
        };
        // PID liveness gate: a marker for a dead process is leftover
        // state from a crashed/uncleanly-exited instance.
        if !alive.contains(&pid) {
            continue;
        }
        out.push(SessionMarker { pid, session_id, cwd, status });
    }
    out
}

/// Return one [`LiveSession`] per currently-open Claude Code instance
/// belonging to `account`. Pass [`alive_pids`] for `alive`; callers
/// that iterate multiple accounts should compute it once and share.
///
/// `previous` is the result of the most recent scan for this account (or
/// `&[]` on a cold start / one-shot caller). It's used solely to preserve
/// `state_since` across scans when the session's state hasn't flipped.
pub fn scan(
    account: &Account,
    alive: &HashSet<u32>,
    previous: &[LiveSession],
) -> Vec<LiveSession> {
    let projects = account.projects_dir();
    if !projects.exists() {
        return Vec::new();
    }
    let markers = read_markers(account, alive);

    let mut out = Vec::new();
    for marker in &markers {
        let proj_dir = cwd_to_project_dir(&marker.cwd);
        let transcript = projects
            .join(&proj_dir)
            .join(format!("{}.jsonl", marker.session_id));
        if !transcript.exists() {
            // Marker exists but transcript hasn't materialized yet —
            // brand-new session. Skip; it'll show up on next scan.
            continue;
        }

        let records = stats::parse_file_cached(&transcript).unwrap_or_default();
        let mut totals = UsageTotals::default();
        let mut last_activity = DateTime::<Utc>::MIN_UTC;
        let mut model = String::new();
        let mut project = String::new();
        for r in &records {
            if r.session_id != marker.session_id {
                continue;
            }
            totals.add(r);
            if r.timestamp > last_activity {
                last_activity = r.timestamp;
                model = r.model.clone();
                project = r.project.clone();
            }
        }
        if last_activity == DateTime::<Utc>::MIN_UTC {
            // No usage records yet (fresh session, user hasn't sent
            // the first message). Use the JSONL mtime as a reasonable
            // proxy so the row sorts correctly.
            if let Ok(meta) = std::fs::metadata(&transcript) {
                if let Ok(mtime) = meta.modified() {
                    last_activity = DateTime::<Utc>::from(mtime);
                }
            }
        }
        if project.is_empty() {
            project = marker
                .cwd
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
        }

        let (current_context, context_cap) = match stats::current_context_from_transcript(&transcript) {
            Some(SessionContext { current, max_observed, model: m }) => {
                let cap = stats::context_cap_for(&m, max_observed, None, account);
                (Some(current), Some(cap))
            }
            None => (None, None),
        };

        let state = if marker.status == "busy" {
            SessionState::Active
        } else {
            SessionState::Idle
        };

        // Carry `state_since` forward while the state hasn't flipped so
        // the displayed age keeps growing through repeated scans (and
        // through every appended record while Active). For a transition
        // (same session seen previously with a different state) snap to
        // `Utc::now()` — the moment we observed the flip. We can't use
        // `last_activity` here because the marker can flip before the
        // first JSONL record of the new turn hits disk; using the stale
        // record timestamp would leave `state_since` equal to its old
        // value and the age column would look unchanged. For a session
        // we've truly never seen, fall back to `last_activity` so the
        // first frame shows a realistic age instead of pretending the
        // state just began.
        let prior = previous.iter().find(|p| p.session_id == marker.session_id);
        let state_since = match prior {
            Some(p) if p.state == state => p.state_since,
            Some(_) => Utc::now(),
            None => last_activity,
        };

        out.push(LiveSession {
            account_name: account.name.clone(),
            session_id: marker.session_id.clone(),
            project,
            transcript_path: transcript,
            last_activity,
            state_since,
            model,
            session_tokens: totals,
            current_context,
            context_cap,
            state,
        });
    }

    out.sort_by(|a, b| {
        let rank = |s: SessionState| match s {
            SessionState::Active => 0,
            SessionState::Idle => 1,
        };
        rank(a.state)
            .cmp(&rank(b.state))
            .then_with(|| b.last_activity.cmp(&a.last_activity))
    });
    out
}
