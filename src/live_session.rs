//! Detect currently-open Claude Code instances per account.
//!
//! Claude Code writes a marker at `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json`
//! for every running interactive instance:
//!
//! ```json
//! {"pid": 56869,
//!  "sessionId": "f2323e13-...",
//!  "cwd": "/Users/.../mewxi",
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
use chrono::{DateTime, TimeZone, Utc};
use serde::Serialize;
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Active,
    Idle,
}

/// What the session is doing right now — a one-word summary derived from
/// the marker (`busy`/`idle`) and the tail of the transcript. `Waiting`
/// is the only state used when the marker is idle; the rest are picked
/// from the most recent record's `content` and indicate what Claude is
/// currently spending its turn on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Waiting,
    Starting,
    Thinking,
    Writing,
    Reading,
    Editing,
    Searching,
    Fetching,
    Running,
    Delegating,
    Asking,
    /// A permission dialog is currently up. Detected via the hook-written
    /// `<session>.awaiting` sibling of the session marker — Claude Code
    /// itself emits no transcript record when the dialog appears.
    Awaiting,
    /// Claude Code is summarising the conversation to free up context.
    /// No transcript record is written during summarisation — the
    /// `/compact` invocation, the summary, and the completion stdout
    /// are all appended at once *after* compaction finishes. So we
    /// can't classify a single record as Compacting; instead [`scan`]
    /// detects the in-flight window heuristically: marker is `busy`,
    /// the tail's latest classifiable record is the previous turn's
    /// final assistant text, and the JSONL hasn't been appended to for
    /// several seconds.
    Compacting,
    Tool(String),
}

impl Activity {
    pub fn label(&self) -> String {
        match self {
            Activity::Waiting => "waiting".into(),
            Activity::Starting => "starting".into(),
            Activity::Thinking => "thinking".into(),
            Activity::Writing => "writing".into(),
            Activity::Reading => "reading".into(),
            Activity::Editing => "editing".into(),
            Activity::Searching => "searching".into(),
            Activity::Fetching => "fetching".into(),
            Activity::Running => "running".into(),
            Activity::Delegating => "delegating".into(),
            Activity::Asking => "asking".into(),
            Activity::Awaiting => "awaiting".into(),
            Activity::Compacting => "compacting".into(),
            Activity::Tool(n) => n.clone(),
        }
    }
}

/// Pull the human-visible text out of a user-record `content` field.
/// Claude Code writes it either as a plain string or as an array of
/// `{type:"text",text:"..."}` blocks; both forms collapse to a single
/// string for substring matching.
fn user_text(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let arr = content.as_array()?;
    let mut buf = String::new();
    for ci in arr {
        if ci.get("type").and_then(|x| x.as_str()) == Some("text") {
            if let Some(t) = ci.get("text").and_then(|x| x.as_str()) {
                buf.push_str(t);
                buf.push('\n');
            }
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

fn classify_tool(name: &str) -> Activity {
    // Strip MCP server prefix: mcp__server__tool → tool.
    let short = name.rsplit("__").next().unwrap_or(name);
    match short.to_ascii_lowercase().as_str() {
        "read" => Activity::Reading,
        "edit" | "write" | "notebookedit" => Activity::Editing,
        "bash" | "bashoutput" | "killbash" | "killshell" => Activity::Running,
        "grep" | "glob" => Activity::Searching,
        "websearch" | "toolsearch" => Activity::Searching,
        "webfetch" => Activity::Fetching,
        "agent" | "task" => Activity::Delegating,
        "askuserquestion" => Activity::Asking,
        other => Activity::Tool(other.to_string()),
    }
}

/// Inspect one JSONL record and report the activity it implies, or None
/// if it's a record kind that says nothing about what Claude is doing
/// (attachment, ai-title, file-history-snapshot, etc.).
fn classify_record(j: &serde_json::Value) -> Option<Activity> {
    let t = j.get("type")?.as_str()?;
    match t {
        "assistant" => {
            let arr = j.get("message")?.get("content")?.as_array()?;
            // Walk content items in reverse — the last meaningful item is
            // what Claude was producing when the record was written.
            for ci in arr.iter().rev() {
                let Some(ct) = ci.get("type").and_then(|x| x.as_str()) else { continue };
                match ct {
                    "tool_use" => {
                        let name = ci.get("name").and_then(|x| x.as_str()).unwrap_or("");
                        return Some(classify_tool(name));
                    }
                    "thinking" => return Some(Activity::Thinking),
                    "text" => return Some(Activity::Writing),
                    _ => continue,
                }
            }
            None
        }
        "user" => {
            let content = j.get("message")?.get("content")?;
            // The whole `/compact` record cluster (`<command-name>/compact</command-name>`,
            // its `<local-command-stdout>...Compacted...` echo, and the
            // `isCompactSummary` summary text) is written *after* compaction
            // finishes — Claude Code emits nothing to the JSONL while it is
            // summarising. So none of those records are in-flight signals.
            // Treat them as inert and let the walker keep going to the
            // pre-compaction turn record, which represents what the user was
            // last actually doing.
            if let Some(text) = user_text(content) {
                if text.contains("<local-command-stdout>")
                    && text.contains("Compacted")
                {
                    return None;
                }
                if text.contains("<command-name>/compact</command-name>") {
                    return None;
                }
            }
            // The injected post-compaction summary itself.
            if j.get("isCompactSummary").and_then(|x| x.as_bool()) == Some(true) {
                return None;
            }
            if let Some(arr) = content.as_array() {
                let has_tool_result = arr.iter().any(|ci| {
                    ci.get("type").and_then(|x| x.as_str()) == Some("tool_result")
                });
                if has_tool_result {
                    // Tool returned, Claude is processing the result before
                    // its next move.
                    return Some(Activity::Thinking);
                }
            }
            // Plain prompt — user just sent something, no response yet.
            Some(Activity::Starting)
        }
        _ => None,
    }
}

/// Read the last `max_bytes` of `path` as a string, discarding any
/// partial leading line so callers can safely split on `\n`. Returns
/// None on read errors or empty files.
fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    if start > 0 {
        if let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=nl);
        } else {
            return None;
        }
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Seconds since the transcript file was last modified. Used by the
/// `/compact` heuristic in [`scan`] to distinguish the compaction window
/// (marker busy, no JSONL writes for ~minutes) from a normal turn
/// (records stream in within a second of the marker flipping to busy).
fn transcript_age_seconds(path: &Path) -> Option<f64> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime: DateTime<Utc> = metadata.modified().ok()?.into();
    Some((Utc::now() - mtime).num_milliseconds() as f64 / 1000.0)
}

/// What the tail says about the session, with enough context for the
/// caller to decide whether the marker's `idle` flag should override.
#[derive(Debug, PartialEq, Eq)]
pub enum TailKind {
    /// Last meaningful record is an assistant `tool_use` with no
    /// matching `tool_result` yet — Claude is paused on that tool, be
    /// it executing, awaiting a permission prompt, or blocked on
    /// `AskUserQuestion`. The marker may read either `busy` or `idle`
    /// (Claude Code flips to idle while a question dialog is up) but
    /// the session is not actually waiting for a fresh prompt.
    PendingTool(Activity),
    /// Last meaningful record completed Claude's side of the turn —
    /// an assistant text/thinking block, or a user tool_result that
    /// Claude has not yet acted on. Safe to surface `Waiting` when
    /// the marker says idle.
    Completed(Activity),
}

/// Max age of a `tool_result` tail for which we still carry the
/// preceding tool's activity. Past this, Claude has had enough time
/// that the pause is a real reasoning pause, not the inter-record gap
/// in a fast-tool cascade — so we flip to `Thinking` instead.
const TOOL_RESULT_CARRY_WINDOW: chrono::Duration = chrono::Duration::milliseconds(1500);

/// Walk the tail of a transcript backwards looking for the most recent
/// record that implies an activity. Returns None if nothing meaningful
/// is found in the inspected window.
fn tail_activity(path: &Path) -> Option<TailKind> {
    let tail = read_tail(path, 256 * 1024)?;
    classify_tail(&tail, Utc::now())
}

fn classify_tail(tail: &str, now: DateTime<Utc>) -> Option<TailKind> {
    let lines: Vec<&str> = tail.lines().collect();

    for (idx, line) in lines.iter().enumerate().rev() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(a) = classify_record(&v) else { continue };
        let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");

        // Fast tools (Read/Grep/Glob) complete in <15 ms, well under the
        // TUI's 500 ms debounce, so every scan lands AFTER the tool_result
        // is written — we never observe the assistant tool_use as the tail
        // record. Falling back to `Thinking` makes the entire fast-tool
        // cascade invisible. Walk back to the matching assistant tool_use
        // and surface its activity so e.g. a Read sequence keeps reading.
        //
        // But: once the tool_result has been sitting on disk longer than
        // `TOOL_RESULT_CARRY_WINDOW`, Claude is no longer mid-cascade — it
        // is genuinely thinking before the next move. Stop carrying then,
        // so the activity column actually surfaces real reasoning pauses
        // as `thinking`.
        if t == "user" && a == Activity::Thinking && record_is_recent(&v, now) {
            if let Some(tool_a) = preceding_tool_activity(&lines[..idx]) {
                return Some(TailKind::Completed(tool_a));
            }
        }

        // An assistant record we just classified as a tool is by
        // definition unresolved — nothing came after it in the file.
        let is_pending = t == "assistant" && is_tool_activity(&a);
        return Some(if is_pending { TailKind::PendingTool(a) } else { TailKind::Completed(a) });
    }
    None
}

/// Returns true when the record's `timestamp` is within
/// [`TOOL_RESULT_CARRY_WINDOW`] of `now`. Records without a parseable
/// timestamp are treated as recent — keeps test stubs working and lets
/// malformed records degrade to the (safer) carry-back path rather
/// than flipping to a confident `Thinking`.
fn record_is_recent(v: &serde_json::Value, now: DateTime<Utc>) -> bool {
    let Some(ts_str) = v.get("timestamp").and_then(|x| x.as_str()) else { return true };
    match DateTime::parse_from_rfc3339(ts_str) {
        Ok(ts) => now - ts.with_timezone(&Utc) < TOOL_RESULT_CARRY_WINDOW,
        Err(_) => true,
    }
}

/// Walk `prior` lines (older-first) in reverse and return the tool
/// activity of the most recent assistant record, if that record was a
/// tool_use. If the most recent assistant record was thinking/text (no
/// tool), or there is none in window, returns None.
fn preceding_tool_activity(prior: &[&str]) -> Option<Activity> {
    for line in prior.iter().rev() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v.get("type").and_then(|x| x.as_str()) != Some("assistant") {
            continue;
        }
        let a = classify_record(&v)?;
        return is_tool_activity(&a).then_some(a);
    }
    None
}

fn is_tool_activity(a: &Activity) -> bool {
    matches!(
        a,
        Activity::Reading
            | Activity::Editing
            | Activity::Searching
            | Activity::Fetching
            | Activity::Running
            | Activity::Delegating
            | Activity::Asking
            | Activity::Tool(_)
    )
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveSession {
    pub account_name: String,
    pub session_id: String,
    /// Process id of the running `claude` instance. Surfaced so the TUI
    /// can target it for kill — both for sessions mewxi spawned itself
    /// and for ones started in another terminal.
    pub pid: u32,
    pub project: String,
    pub cwd: PathBuf,
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
    /// One-word summary of what Claude is doing right now. `Waiting`
    /// when the marker is idle; otherwise derived from the tail of the
    /// transcript (last `tool_use`, `thinking`, `text`, or `tool_result`).
    pub activity: Activity,
    /// Public Managed Agents session id (`session_…`) when the process
    /// was started with `--remote-control`. `Some` means mewxi can drive
    /// this session via the bridge API.
    pub bridge_session_id: Option<String>,
}

#[derive(Clone, Debug)]
struct SessionMarker {
    pid: u32,
    session_id: String,
    cwd: PathBuf,
    status: String,
    /// Wall-clock time the marker was last refreshed (ms since epoch).
    /// Used as the activity timestamp for brand-new sessions whose
    /// transcript JSONL hasn't been created yet (user opened a window
    /// but hasn't sent the first prompt).
    updated_at_ms: Option<i64>,
    started_at_ms: Option<i64>,
    /// Public Managed Agents session id (`session_…`) when the process
    /// was started with `--remote-control`. Present only on RC-enabled
    /// sessions; the field exposes those to mewxi's agent-control path.
    bridge_session_id: Option<String>,
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
    // Claude Code encodes a cwd into its on-disk projects/<dir> name by
    // replacing `/`, `_`, and `.` with `-` (one dash per source char), so
    // `/Users/foo/.claude/bar_baz` becomes `-Users-foo--claude-bar-baz`.
    // Mirror that here or the transcript JSONL lookup misses for any
    // path containing `_` or `.`.
    cwd.to_string_lossy()
        .chars()
        .map(|c| if matches!(c, '/' | '_' | '.') { '-' } else { c })
        .collect()
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
        let updated_at_ms = v.get("updatedAt").and_then(|x| x.as_i64());
        let started_at_ms = v.get("startedAt").and_then(|x| x.as_i64());
        let bridge_session_id = v
            .get("bridgeSessionId")
            .and_then(|s| s.as_str())
            .map(String::from);
        let (Some(pid), Some(session_id), Some(cwd)) = (pid, session_id, cwd) else {
            continue;
        };
        // PID liveness gate: a marker for a dead process is leftover
        // state from a crashed/uncleanly-exited instance.
        if !alive.contains(&pid) {
            continue;
        }
        out.push(SessionMarker {
            pid,
            session_id,
            cwd,
            status,
            updated_at_ms,
            started_at_ms,
            bridge_session_id,
        });
    }
    out
}

fn ms_to_utc(ms: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms).single()
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
        let transcript_exists = transcript.exists();

        let records = if transcript_exists {
            stats::parse_file_cached(&transcript).unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut totals = UsageTotals::default();
        let mut last_activity = DateTime::<Utc>::MIN_UTC;
        let mut model = String::new();
        for r in &records {
            if r.session_id != marker.session_id {
                continue;
            }
            totals.add(r);
            if r.timestamp > last_activity {
                last_activity = r.timestamp;
                model = r.model.clone();
            }
        }
        // Project label = the raw cwd basename from the marker. The
        // record's `project` field is run through stats::decode_project_slug,
        // which only keeps the last dash segment (Claude Code's projects/<dir>
        // encoding flattens `/`, `_`, and `.` all to `-`, so it can't be
        // reversed unambiguously). The marker's cwd preserves the real name.
        let project = marker
            .cwd
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if last_activity == DateTime::<Utc>::MIN_UTC {
            // No usage records yet — either the transcript JSONL hasn't
            // been created (brand-new window, no first prompt sent) or
            // it exists but has no records for this sessionId yet.
            // Prefer marker timestamps so the row sorts correctly even
            // without a transcript file; fall back to JSONL mtime.
            last_activity = marker
                .updated_at_ms
                .and_then(ms_to_utc)
                .or_else(|| marker.started_at_ms.and_then(ms_to_utc))
                .or_else(|| {
                    std::fs::metadata(&transcript)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(DateTime::<Utc>::from)
                })
                .unwrap_or_else(Utc::now);
        }
        let (current_context, context_cap) = if transcript_exists {
            match stats::current_context_from_transcript(&transcript) {
                Some(SessionContext { current, max_observed, model: m }) => {
                    let cap = stats::context_cap_for(
                        &m,
                        max_observed,
                        None,
                        account,
                        Some(&marker.session_id),
                    );
                    (Some(current), Some(cap))
                }
                None => (None, None),
            }
        } else {
            (None, None)
        };

        let state = if marker.status == "busy" {
            SessionState::Active
        } else {
            SessionState::Idle
        };

        // Sidechannel marker written by the `mewxi hook awaiting-set`
        // command we install into each account's `settings.json` —
        // present means a permission dialog is currently up. Claude Code
        // itself emits nothing to the transcript while the dialog is
        // displayed (the `tool_use` record only lands after the user
        // responds), so the hook is the only reliable signal.
        let awaiting_marker = account
            .dir
            .join("sessions")
            .join(format!("{}.awaiting", marker.session_id));

        // Decide activity:
        //  1. `.awaiting` file present → permission dialog up, hands down.
        //  2. Otherwise an unresolved tool_use in the transcript →
        //     surface that tool's activity (covers `AskUserQuestion`,
        //     where Claude Code flips the marker to `idle` but Claude
        //     is genuinely paused on the user).
        //  3. Otherwise honor the marker: `idle` → Waiting, `busy` →
        //     last completed tail activity (Thinking/Writing) or
        //     Starting if there's nothing yet.
        //  4. /compact heuristic: marker is `busy` but the tail's latest
        //     classifiable record is the *previous* turn's final text and
        //     the JSONL hasn't been touched for several seconds. Claude
        //     Code emits no records to the transcript while it is
        //     summarising; this signature distinguishes that window from
        //     a fresh prompt (which would land its user record almost
        //     immediately and break the "stale + ends in assistant text"
        //     match).
        let activity = if awaiting_marker.exists() {
            Activity::Awaiting
        } else {
            let tail = transcript_exists
                .then(|| tail_activity(&transcript))
                .flatten();
            match (state, tail) {
                (_, Some(TailKind::PendingTool(a))) => a,
                (SessionState::Idle, _) => Activity::Waiting,
                (SessionState::Active, Some(TailKind::Completed(Activity::Writing)))
                    if transcript_age_seconds(&transcript).is_some_and(|s| s > 5.0) =>
                {
                    Activity::Compacting
                }
                (SessionState::Active, Some(TailKind::Completed(a))) => a,
                (SessionState::Active, None) => Activity::Starting,
            }
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
            pid: marker.pid,
            project,
            cwd: marker.cwd.clone(),
            transcript_path: transcript,
            last_activity,
            state_since,
            model,
            session_tokens: totals,
            current_context,
            context_cap,
            state,
            activity,
            bridge_session_id: marker.bridge_session_id.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        // Fixed reference point so timestamped fixtures have a stable "now".
        Utc.with_ymd_and_hms(2026, 5, 18, 18, 0, 0).unwrap()
    }

    fn assistant_thinking() -> &'static str {
        r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#
    }
    fn assistant_tool_use(name: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"{name}"}}]}}}}"#
        )
    }
    fn user_tool_result() -> &'static str {
        r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#
    }
    fn user_tool_result_at(ts: DateTime<Utc>) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{}","message":{{"content":[{{"type":"tool_result"}}]}}}}"#,
            ts.to_rfc3339()
        )
    }
    fn assistant_text() -> &'static str {
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#
    }

    #[test]
    fn pending_tool_when_tool_use_is_tail() {
        // Slow tool still running: assistant tool_use is the latest record,
        // nothing after it yet. Must surface as Pending so the marker's
        // `idle` (e.g. AskUserQuestion) can't downgrade us to Waiting.
        let tail = format!("{}\n{}\n", assistant_thinking(), assistant_tool_use("Read"));
        assert_eq!(classify_tail(&tail, now()), Some(TailKind::PendingTool(Activity::Reading)));
    }

    #[test]
    fn carry_tool_activity_past_tool_result() {
        // The regression: fast Read completes in <15ms, so scans always
        // land after the tool_result is written. Without the walk-back,
        // this returned Completed(Thinking) and made the whole Read
        // cascade invisible. With it, we surface Completed(Reading).
        // No timestamp → treated as recent → carry-back applies.
        let tail = format!(
            "{}\n{}\n{}\n",
            assistant_thinking(),
            assistant_tool_use("Read"),
            user_tool_result()
        );
        assert_eq!(classify_tail(&tail, now()), Some(TailKind::Completed(Activity::Reading)));
    }

    #[test]
    fn carry_works_for_bash_and_edit_too() {
        let bash_tail = format!("{}\n{}\n", assistant_tool_use("Bash"), user_tool_result());
        assert_eq!(classify_tail(&bash_tail, now()), Some(TailKind::Completed(Activity::Running)));
        let edit_tail = format!("{}\n{}\n", assistant_tool_use("Edit"), user_tool_result());
        assert_eq!(classify_tail(&edit_tail, now()), Some(TailKind::Completed(Activity::Editing)));
    }

    #[test]
    fn fresh_tool_result_carries_back() {
        // tool_result is 200ms old — well within the carry window. The
        // user is mid-cascade; surface the tool's activity, not Thinking.
        let recent = now() - chrono::Duration::milliseconds(200);
        let tail = format!(
            "{}\n{}\n",
            assistant_tool_use("Read"),
            user_tool_result_at(recent)
        );
        assert_eq!(classify_tail(&tail, now()), Some(TailKind::Completed(Activity::Reading)));
    }

    #[test]
    fn stale_tool_result_flips_to_thinking() {
        // tool_result is 5s old — Claude is past the cascade window and
        // is genuinely reasoning before the next action. Show Thinking
        // rather than continuing to claim the tool is the activity.
        let stale = now() - chrono::Duration::seconds(5);
        let tail = format!(
            "{}\n{}\n",
            assistant_tool_use("Read"),
            user_tool_result_at(stale)
        );
        assert_eq!(classify_tail(&tail, now()), Some(TailKind::Completed(Activity::Thinking)));
    }

    #[test]
    fn thinking_only_tail_stays_thinking() {
        // Claude has emitted a thinking block but not yet the tool_use.
        // No prior tool to carry; must read as Completed(Thinking) so the
        // activity column actually says "thinking" during this window.
        let tail = format!("{}\n", assistant_thinking());
        assert_eq!(classify_tail(&tail, now()), Some(TailKind::Completed(Activity::Thinking)));
    }

    #[test]
    fn tool_result_with_no_preceding_tool_falls_back_to_thinking() {
        // Defensive: malformed tail with a tool_result but no assistant
        // tool_use anywhere in window. Carry-back finds nothing → fall
        // through to the original Completed(Thinking) classification.
        let tail = format!("{}\n", user_tool_result());
        assert_eq!(classify_tail(&tail, now()), Some(TailKind::Completed(Activity::Thinking)));
    }

    #[test]
    fn final_text_classifies_as_writing() {
        // End of turn: assistant ends with a text block, no tool to carry.
        let tail = format!("{}\n", assistant_text());
        assert_eq!(classify_tail(&tail, now()), Some(TailKind::Completed(Activity::Writing)));
    }

    #[test]
    fn post_compaction_cluster_is_inert() {
        // Claude Code writes the entire `/compact` cluster — the boundary
        // system record, the `isCompactSummary` summary text, the
        // re-emitted `<command-name>/compact</command-name>` user record,
        // and the `<local-command-stdout>...Compacted...` echo — only
        // *after* compaction finishes. Treating any of them as an
        // in-flight signal left the activity stuck on `compacting` long
        // after the agent was actually idle. They must all be skipped so
        // the walker falls through to the real pre-compaction last
        // activity (here: Writing from the trailing assistant text).
        let tail = format!(
            "{}\n{}\n{}\n{}\n",
            assistant_text(),
            r#"{"type":"user","isCompactSummary":true,"message":{"content":[{"type":"text","text":"This session is being continued..."}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args></command-args>"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"<local-command-stdout>Compacted (ctrl+o to see full summary)</local-command-stdout>"}]}}"#
        );
        assert_eq!(
            classify_tail(&tail, now()),
            Some(TailKind::Completed(Activity::Writing))
        );
    }

    #[test]
    fn skips_non_meaningful_records() {
        // Records like ai-title / file-history-snapshot don't classify;
        // the walk-back must skip them and use the real last activity.
        let tail = format!(
            "{}\n{}\n{}\n{}\n",
            assistant_tool_use("Bash"),
            user_tool_result(),
            r#"{"type":"ai-title"}"#,
            r#"{"type":"file-history-snapshot"}"#
        );
        assert_eq!(classify_tail(&tail, now()), Some(TailKind::Completed(Activity::Running)));
    }
}
