//! Persist and read the live per-agent status labels leaked by Claude
//! Code's undocumented `subagentStatusLine` setting.
//!
//! Shaped like the documented `statusLine`, it runs a configured command
//! roughly every 5s while agents/tasks are active and pipes it a JSON
//! payload on stdin — the same payload Claude Code's own agent panel
//! renders from, including a `tasks[].label` field carrying the live
//! LLM-generated summary of what each sub-agent is doing right now (e.g.
//! "Reading errorHandler.ts response mapping"). A mewxi hook subcommand
//! is wired up as that command; it forwards the raw payload to
//! [`write_feed`] here. The sub-agent scanner then calls [`read_feed`] to
//! stamp a live label onto each sub-agent row it already found by other
//! means.
//!
//! On disk:
//!
//! ```text
//! <CLAUDE_CONFIG_DIR>/sessions/<sessionId>.agent-status.json   ← latest raw payload for a session
//! <CLAUDE_CONFIG_DIR>/sessions/any.agent-status.json           ← used when the payload carries no session id
//! ```
//!
//! The payload's session-identifying keys are unconfirmed (recovered
//! from the binary, not documented), so both this writer and the reader
//! treat everything about the payload shape defensively: unknown or
//! missing keys degrade to the `any` bucket rather than erroring, and a
//! non-object/non-JSON payload is silently dropped. The hook this feeds
//! is on Claude Code's critical path — it must never fail or block on a
//! malformed payload.
//!
//! Task `id` is believed to be the sub-agent's `agentId` (the
//! `agent-<id>.jsonl` filename stem) but that's not confirmed either, so
//! [`AgentStatusFeed`] indexes labels by both `id` and launch
//! `description` — the scanner tries `id` first and falls back to
//! matching on description when it doesn't hit.
//!
//! A task's `label` falls back to its `description` inside Claude Code
//! itself when no summary has been generated yet; `label == description`
//! therefore carries no live signal and is skipped rather than indexed.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Freshness bound for the feed file. The feed writes every ~5s while
/// agents run; a file older than this is a dead feed (session closed,
/// panel gone) and must not override the scanner's own heuristics.
pub const FRESH_WINDOW: Duration = Duration::from_secs(45);

/// Live per-agent labels parsed from one feed payload.
pub struct AgentStatusFeed {
    /// task id → label. Task ids are believed to equal sub-agent agentIds.
    pub labels_by_id: HashMap<String, String>,
    /// launch description → label. Fallback index for the id mismatch case.
    pub labels_by_description: HashMap<String, String>,
}

/// Persist one raw subagentStatusLine payload for the TUI to read.
/// `dir` is the account's CLAUDE_CONFIG_DIR. Best-effort and fast —
/// called from a hook Claude Code waits on.
pub fn write_feed(dir: &Path, payload: &str) -> Result<()> {
    // Reject anything that isn't a JSON object outright: never let a
    // malformed payload fail the hook, and never create files for it.
    let value: Value = match serde_json::from_str::<Value>(payload) {
        Ok(v) if v.is_object() => v,
        _ => return Ok(()),
    };

    let session_id = value
        .get("session_id")
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)
        .filter(|id| is_safe_filename(id))
        .unwrap_or("any");

    let sessions_dir = dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    let path = feed_path(&sessions_dir, session_id);
    let tmp_path = path.with_extension("json.tmp");
    // Atomic write: the TUI polls this file on a 500ms tick and must
    // never observe a partial write.
    std::fs::write(&tmp_path, payload)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// The fresh feed for `session_id`, or None when the file is missing,
/// stale (older than FRESH_WINDOW), or unparseable.
pub fn read_feed(dir: &Path, session_id: &str) -> Option<AgentStatusFeed> {
    let sessions_dir = dir.join("sessions");
    let primary = is_safe_filename(session_id).then(|| feed_path(&sessions_dir, session_id));
    let path = match primary {
        Some(p) if p.exists() => p,
        _ => feed_path(&sessions_dir, "any"),
    };
    if !is_fresh(&path, FRESH_WINDOW) {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    parse_feed(&raw)
}

fn feed_path(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(format!("{session_id}.agent-status.json"))
}

/// Filename-safety gate for session ids pulled out of an untrusted
/// payload (or handed in by a caller) — keeps them from escaping the
/// `sessions/` dir or colliding with the `any` bucket's name.
fn is_safe_filename(id: &str) -> bool {
    !id.is_empty() && id != "any" && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Split out so the freshness rule (currently [`FRESH_WINDOW`]) can be
/// unit-tested against an arbitrary window without touching the const.
fn is_fresh(path: &Path, window: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .is_some_and(|age| age <= window)
}

fn parse_feed(raw: &str) -> Option<AgentStatusFeed> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let obj = value.as_object()?;
    let tasks = obj.get("tasks").and_then(Value::as_array);

    let mut labels_by_id = HashMap::new();
    let mut labels_by_description = HashMap::new();
    for task in tasks.into_iter().flatten() {
        let label = task.get("label").and_then(Value::as_str).unwrap_or("");
        let description = task
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        // Empty, or equal to description: Claude Code's own fallback for
        // "no live summary generated yet" — carries no signal.
        if label.is_empty() || label == description {
            continue;
        }
        if let Some(id) = task.get("id").and_then(Value::as_str) {
            labels_by_id.insert(id.to_string(), label.to_string());
        }
        if !description.is_empty() {
            labels_by_description.insert(description.to_string(), label.to_string());
        }
    }

    Some(AgentStatusFeed {
        labels_by_id,
        labels_by_description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_roundtrips_only_real_labels() {
        let dir = tempfile::tempdir().unwrap();
        let payload = r#"{
            "session_id": "sess-123",
            "columns": 160,
            "tasks": [
                {"id": "a1", "description": "Reading files", "label": "Reading errorHandler.ts response mapping"},
                {"id": "a2", "description": "Writing tests", "label": "Writing tests"}
            ]
        }"#;
        write_feed(dir.path(), payload).unwrap();

        let expected_path = dir
            .path()
            .join("sessions")
            .join("sess-123.agent-status.json");
        assert!(expected_path.exists());

        let feed = read_feed(dir.path(), "sess-123").expect("fresh feed should parse");
        assert_eq!(
            feed.labels_by_id.get("a1").map(String::as_str),
            Some("Reading errorHandler.ts response mapping")
        );
        assert_eq!(feed.labels_by_id.get("a2"), None, "label == description");
        assert_eq!(
            feed.labels_by_description.get("Reading files").map(String::as_str),
            Some("Reading errorHandler.ts response mapping")
        );
        assert_eq!(feed.labels_by_description.get("Writing tests"), None);
    }

    #[test]
    fn payload_without_session_id_lands_in_any_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let payload = r#"{"tasks": [{"id": "a1", "description": "d", "label": "live label"}]}"#;
        write_feed(dir.path(), payload).unwrap();

        let expected_path = dir.path().join("sessions").join("any.agent-status.json");
        assert!(expected_path.exists());

        // A totally unrelated session id falls back to the `any` bucket.
        let feed = read_feed(dir.path(), "some-other-session").expect("falls back to any");
        assert_eq!(
            feed.labels_by_id.get("a1").map(String::as_str),
            Some("live label")
        );
    }

    #[test]
    fn is_fresh_gates_on_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        std::fs::write(&path, "{}").unwrap();

        assert!(is_fresh(&path, Duration::from_secs(60)));
        // A zero window means "must have been modified after now", which
        // a file written moments ago never satisfies.
        assert!(!is_fresh(&path, Duration::from_secs(0)));
    }

    #[test]
    fn garbage_payload_writes_nothing_and_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        write_feed(dir.path(), "not json").unwrap();
        write_feed(dir.path(), "[1, 2, 3]").unwrap();
        assert!(!dir.path().join("sessions").exists());
    }
}
